//! Embedded Whisper speech-to-text powered by `whisper.cpp` (`whisper-rs`).
//!
//! Ships Whisper directly inside CutPilot with zero external Python/CLI
//! dependencies:
//! - Extracts 16 kHz mono f32 PCM via `ffmpeg` (cancellable).
//! - Auto-downloads the chosen GGML model from Hugging Face into the local
//!   user cache on first use (atomic `.part` -> rename, progress reported).
//! - Runs `whisper.cpp` in-process with progress and abort callbacks while
//!   silencing C/GGML logs so MCP stdout JSON-RPC is never polluted.
//! - Returns tick-exact [`TranscriptSegment`]s (`1 centisecond = 36,000 ticks`).

use crate::MediaError;
use cutpilot_core::{TranscriptSegment, Ticks};
use std::ffi::{c_int, c_void};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};

/// Default GGML model name: fast, compact (~148 MB), and accurate for English.
pub const DEFAULT_MODEL: &str = "base.en";

/// Supported named GGML models auto-downloadable from `ggerganov/whisper.cpp`.
pub const SUPPORTED_MODELS: &[&str] = &[
    "tiny.en",
    "tiny",
    "base.en",
    "base",
    "small.en",
    "small",
    "medium.en",
    "medium",
    "large-v3-turbo",
    "large-v3",
];

/// One whisper.cpp timestamp unit is 10 ms (1 centisecond) = 36,000 ticks.
pub const TICKS_PER_CENTISEC: Ticks = cutpilot_core::TICKS_PER_SEC / 100;

static LOG_HOOK_INIT: Once = Once::new();

fn ensure_logging_silenced() {
    LOG_HOOK_INIT.call_once(|| {
        whisper_rs::install_logging_hooks();
    });
}

/// Capabilities reported by `system_capabilities`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WhisperCapabilities {
    pub available: bool,
    pub backend: String,
    pub whisper_cpp_version: String,
    pub default_model: String,
    pub models_dir: String,
    pub downloaded_models: Vec<String>,
    pub supported_models: Vec<String>,
}

/// Options for [`transcribe_file`].
#[derive(Debug, Clone)]
pub struct TranscribeOptions {
    /// Named model (e.g. `"base.en"`, `"small"`, `"large-v3-turbo"`) or path
    /// to a local GGML `.bin` file.
    pub model: String,
    /// ISO-639-1 language code (e.g. `"en"`, `"es"`) or `None` / `"auto"`.
    pub language: Option<String>,
    /// Translate non-English speech to English.
    pub translate: bool,
}

impl Default for TranscribeOptions {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            language: None,
            translate: false,
        }
    }
}

/// Result of transcribing a media file.
#[derive(Debug, Clone)]
pub struct TranscribeOutput {
    pub model: String,
    pub language: String,
    pub segments: Vec<TranscriptSegment>,
}

/// Directory where downloaded GGML models are cached.
pub fn models_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CUTPILOT_WHISPER_MODELS_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("cutpilot").join("whisper");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed)
                .join(".cache")
                .join("cutpilot")
                .join("whisper");
        }
    }
    std::env::temp_dir().join("cutpilot-whisper")
}

/// Canonical filename for a named Whisper GGML model.
pub fn model_filename(model: &str) -> Result<String, MediaError> {
    let normalized = model.trim();
    if SUPPORTED_MODELS.contains(&normalized) {
        Ok(format!("ggml-{normalized}.bin"))
    } else {
        Err(MediaError::BadData(format!(
            "unsupported Whisper model `{model}`; choose one of: {} (or pass a local .bin file path)",
            SUPPORTED_MODELS.join(", ")
        )))
    }
}

/// Hugging Face download URL for a named GGML model.
pub fn model_download_url(model: &str) -> Result<String, MediaError> {
    let fname = model_filename(model)?;
    Ok(format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{fname}"
    ))
}

/// List models already cached on disk in [`models_dir`].
pub fn downloaded_models() -> Vec<String> {
    let dir = models_dir();
    let mut found = Vec::new();
    for &name in SUPPORTED_MODELS {
        let p = dir.join(format!("ggml-{name}.bin"));
        if p.metadata().map(|m| m.len() > 1_000_000).unwrap_or(false) {
            found.push(name.to_string());
        }
    }
    found
}

/// Report built-in Whisper capabilities and cached models.
pub fn capabilities() -> WhisperCapabilities {
    let backend = if cfg!(feature = "whisper-cuda") {
        "whisper.cpp (embedded, CUDA)"
    } else if cfg!(feature = "whisper-vulkan") {
        "whisper.cpp (embedded, Vulkan)"
    } else {
        "whisper.cpp (embedded, CPU)"
    };
    WhisperCapabilities {
        available: true,
        backend: backend.to_string(),
        whisper_cpp_version: whisper_rs::WHISPER_CPP_VERSION.to_string(),
        default_model: DEFAULT_MODEL.to_string(),
        models_dir: models_dir().display().to_string(),
        downloaded_models: downloaded_models(),
        supported_models: SUPPORTED_MODELS.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Resolve a model name (downloading it into the local cache if missing) or
/// accept an existing local `.bin` path directly.
pub fn resolve_or_download_model(
    model: &str,
    progress: Option<&Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: &Arc<AtomicBool>,
) -> Result<PathBuf, MediaError> {
    let trimmed = model.trim();
    let direct = Path::new(trimmed);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }

    let fname = model_filename(trimmed)?;
    let dir = models_dir();
    let dest = dir.join(&fname);
    if dest.metadata().map(|m| m.len() > 1_000_000).unwrap_or(false) {
        return Ok(dest);
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(MediaError::Cancelled);
    }

    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let url = model_download_url(trimmed)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(60))
        .redirects(5)
        .build();
    let resp = agent
        .get(&url)
        .call()
        .map_err(|e| MediaError::BadData(format!("failed to download Whisper model `{trimmed}` from {url}: {e}")))?;
    if !resp.get_url().starts_with("https://") {
        return Err(MediaError::BadData(format!(
            "refusing non-HTTPS redirect while downloading Whisper model `{trimmed}`"
        )));
    }

    const MAX_MODEL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
    let total_bytes: Option<u64> = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0);
    if total_bytes.is_some_and(|n| n > MAX_MODEL_BYTES) {
        return Err(MediaError::BadData(format!(
            "Whisper model `{trimmed}` exceeds maximum allowed download size"
        )));
    }

    let tmp_path = dir.join(format!("{fname}.{}.part", uuid::Uuid::new_v4()));
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&tmp_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(MediaError::Cancelled);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > MAX_MODEL_BYTES {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(MediaError::BadData(format!(
                "downloaded Whisper model `{trimmed}` exceeded maximum allowed size"
            )));
        }
        file.write_all(&buf[..n])?;
        if let (Some(cb), Some(total)) = (progress, total_bytes) {
            let frac = (written as f64 / total as f64).clamp(0.0, 1.0);
            cb(frac * 0.15);
        }
    }
    file.flush()?;
    drop(file);

    if written < 1_000_000 {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(MediaError::BadData(format!(
            "downloaded Whisper model `{trimmed}` was unexpectedly small ({written} bytes)"
        )));
    }
    std::fs::rename(&tmp_path, &dest)?;
    Ok(dest)
}

/// Extract 16 kHz mono 32-bit float PCM from any media file using `ffmpeg`.
pub fn extract_pcm_16k_mono(path: &Path, cancel: &Arc<AtomicBool>) -> Result<Vec<f32>, MediaError> {
    // Cap decoded PCM at 6 hours @ 16 kHz (~1.38 GiB) to prevent OOM on
    // malformed or infinite media streams.
    const MAX_PCM_SAMPLES: usize = 16_000 * 60 * 60 * 6;
    let safe_path = crate::safe_local_input(path)?;
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-protocol_whitelist",
            crate::LOCAL_FILE_PROTOCOL_WHITELIST,
            "-i",
        ])
        .arg(&safe_path)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut samples: Vec<f32> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut rem = Vec::with_capacity(4);

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(MediaError::Cancelled);
        }
        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut slice = &buf[..n];
        if !rem.is_empty() {
            let need = 4 - rem.len();
            let take = need.min(slice.len());
            rem.extend_from_slice(&slice[..take]);
            slice = &slice[take..];
            if rem.len() == 4 {
                samples.push(f32::from_le_bytes([rem[0], rem[1], rem[2], rem[3]]));
                rem.clear();
            }
        }
        let (chunks, tail) = slice.as_chunks::<4>();
        if samples.len().saturating_add(chunks.len()) > MAX_PCM_SAMPLES {
            let _ = child.kill();
            let _ = child.wait();
            return Err(MediaError::BadData(
                "audio stream exceeds maximum supported length for in-memory Whisper transcription"
                    .into(),
            ));
        }
        samples.reserve(chunks.len());
        for &c in chunks {
            samples.push(f32::from_le_bytes(c));
        }
        if !tail.is_empty() {
            rem.extend_from_slice(tail);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        let mut msg = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut msg);
        }
        return Err(MediaError::Ffmpeg {
            message: format!(
                "audio extraction for Whisper failed (`{}`): {}",
                path.display(),
                msg.trim()
            ),
        });
    }
    Ok(samples)
}

/// Convert whisper.cpp centiseconds (10 ms units) to CutPilot ticks.
pub fn centisecs_to_ticks(cs: i64) -> Ticks {
    cs.max(0).saturating_mul(TICKS_PER_CENTISEC)
}

/// Transcribe `path` in-process using embedded `whisper.cpp`.
pub fn transcribe_file(
    path: &Path,
    opts: &TranscribeOptions,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<TranscribeOutput, MediaError> {
    ensure_logging_silenced();

    if cancel.load(Ordering::Relaxed) {
        return Err(MediaError::Cancelled);
    }

    let model_name = if opts.model.trim().is_empty() {
        DEFAULT_MODEL.to_string()
    } else {
        opts.model.trim().to_string()
    };

    let model_path = resolve_or_download_model(&model_name, progress.as_ref(), &cancel)?;
    if let Some(cb) = &progress {
        cb(0.16);
    }

    let pcm = extract_pcm_16k_mono(path, &cancel)?;
    if let Some(cb) = &progress {
        cb(0.22);
    }

    let resolved_lang = effective_language(&model_name, opts.language.as_deref());
    if pcm.is_empty() {
        if let Some(cb) = &progress {
            cb(1.0);
        }
        return Ok(TranscribeOutput {
            model: model_name,
            language: resolved_lang.unwrap_or_else(|| "auto".into()),
            segments: Vec::new(),
        });
    }

    let ctx_params = whisper_rs::WhisperContextParameters::default();
    let ctx = whisper_rs::WhisperContext::new_with_params(&model_path, ctx_params).map_err(|e| {
        MediaError::BadData(format!(
            "failed to load Whisper model `{}`: {e}",
            model_path.display()
        ))
    })?;
    let mut state = ctx
        .create_state()
        .map_err(|e| MediaError::BadData(format!("failed to create Whisper state: {e}")))?;

    let mut params = whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8) as c_int)
        .unwrap_or(4);
    params.set_n_threads(threads);
    params.set_translate(opts.translate);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);

    let lang_holder = resolved_lang.clone();
    params.set_language(lang_holder.as_deref());

    // Wire sound, leak-free stack-borrowed abort & progress callbacks for the
    // synchronous duration of `state.full(...)`.
    struct ProgressCarrier<'a> {
        cb: &'a (dyn Fn(f64) + Send + Sync),
    }

    unsafe extern "C" fn abort_trampoline(user_data: *mut c_void) -> bool {
        if user_data.is_null() {
            return false;
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let flag = unsafe { &*(user_data as *const AtomicBool) };
            flag.load(Ordering::Relaxed)
        }))
        .unwrap_or(true)
    }

    unsafe extern "C" fn progress_trampoline(
        _: *mut whisper_rs::WhisperSysContext,
        _: *mut whisper_rs::WhisperSysState,
        pct: c_int,
        user_data: *mut c_void,
    ) {
        if user_data.is_null() {
            return;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let carrier = unsafe { &*(user_data as *const ProgressCarrier<'_>) };
            let frac = (pct.clamp(0, 100) as f64) / 100.0;
            (carrier.cb)(0.22 + frac * 0.77);
        }));
    }

    let cancel_ptr: *const AtomicBool = Arc::as_ptr(&cancel);
    let progress_carrier = progress
        .as_ref()
        .map(|cb| ProgressCarrier { cb: cb.as_ref() });

    unsafe {
        params.set_abort_callback(Some(abort_trampoline));
        params.set_abort_callback_user_data(cancel_ptr as *mut c_void);
        if let Some(ref carrier) = progress_carrier {
            params.set_progress_callback(Some(progress_trampoline));
            params.set_progress_callback_user_data(carrier as *const ProgressCarrier<'_> as *mut c_void);
        }
    }

    let run_res = state.full(params, &pcm);
    let _ = &progress_carrier;

    if cancel.load(Ordering::Relaxed) {
        return Err(MediaError::Cancelled);
    }
    run_res.map_err(|e| MediaError::BadData(format!("Whisper transcription failed: {e}")))?;

    let mut segments = Vec::new();
    for seg in state.as_iter() {
        let text = seg
            .to_str_lossy()
            .map(|c| c.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        let start = centisecs_to_ticks(seg.start_timestamp());
        let end = centisecs_to_ticks(seg.end_timestamp()).max(start + TICKS_PER_CENTISEC);
        segments.push(TranscriptSegment { start, end, text });
    }

    if let Some(cb) = &progress {
        cb(1.0);
    }

    Ok(TranscribeOutput {
        model: model_name,
        language: resolved_lang.unwrap_or_else(|| "auto".into()),
        segments,
    })
}

fn effective_language(model: &str, requested: Option<&str>) -> Option<String> {
    if let Some(lang) = requested {
        let l = lang.trim().to_ascii_lowercase();
        if l.is_empty() || l == "auto" {
            return None;
        }
        return Some(l);
    }
    if model.ends_with(".en") {
        Some("en".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_and_urls_validate() {
        assert_eq!(model_filename("base.en").unwrap(), "ggml-base.en.bin");
        assert!(model_download_url("large-v3-turbo")
            .unwrap()
            .ends_with("/ggml-large-v3-turbo.bin"));
        assert!(model_filename("nonexistent-model").is_err());
    }

    #[test]
    fn centisecs_convert_exact_ticks() {
        assert_eq!(centisecs_to_ticks(100), cutpilot_core::TICKS_PER_SEC);
        assert_eq!(centisecs_to_ticks(250), cutpilot_core::TICKS_PER_SEC * 5 / 2);
        assert_eq!(centisecs_to_ticks(-5), 0);
    }

    #[test]
    fn extract_pcm_16k_mono_decodes_synthesized_audio() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("tone.wav");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "1.5",
            ])
            .arg(&wav)
            .status()
            .expect("ffmpeg synthesize tone");
        assert!(status.success());

        let cancel = Arc::new(AtomicBool::new(false));
        let pcm = extract_pcm_16k_mono(&wav, &cancel).unwrap();
        // ~1.5s @ 16kHz = ~24,000 samples
        assert!(
            (pcm.len() as i64 - 24_000).abs() < 1_000,
            "got {} samples",
            pcm.len()
        );
    }
}
