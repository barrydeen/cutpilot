//! CutPilot media layer: ffprobe, silence analysis, frame extraction,
//! waveform peaks, caption serialization, NVENC-aware export, and the
//! HyperFrames sidecar.
//!
//! Everything here shells out to the system `ffmpeg`/`ffprobe` (n9-era) with
//! JSON/rawvideo pipes. The export plan builder is pure and unit-tested;
//! the runner adds progress, cancellation, and NVENC fallback.

pub mod ass;
pub mod export;
pub mod ffprobe;
pub mod frames;
pub mod gpu;
pub mod hyperframes;
pub mod process;
pub mod proxy;
pub mod silence;
pub mod srt;
pub mod waveform;
pub mod whisper;

use thiserror::Error;

pub use cutpilot_core::ticks::format_seconds as FMT;

/// Errors from the media layer.
#[derive(Debug, Error)]
pub enum MediaError {
    #[error("ffprobe failed for `{path}`: {message}")]
    Probe { path: String, message: String },
    #[error("ffmpeg failed: {message}")]
    Ffmpeg { message: String },
    #[error("process was cancelled")]
    Cancelled,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad media data: {0}")]
    BadData(String),
}

/// Whether the required tools are on PATH.
pub fn tool_status() -> (bool, bool) {
    let has_ffmpeg = which("ffmpeg");
    let has_ffprobe = which("ffprobe");
    (has_ffmpeg, has_ffprobe)
}

fn which(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(tool);
                p.is_file()
            })
        })
        .unwrap_or(false)
}

/// Format seconds for ffmpeg `-ss` / `-t` arguments.
pub fn fmt_secs(secs: f64) -> String {
    if secs.abs() < 1e-6 {
        "0".to_string()
    } else {
        format!("{secs:.6}")
    }
}

/// FFmpeg/ffprobe `-protocol_whitelist` restricting media inputs to local files
/// only (blocks `http://`, `https://`, `concat:`, `subfile:`, `crypto:`, and
/// remote playlist/HLS fetches).
pub const LOCAL_FILE_PROTOCOL_WHITELIST: &str = "file";

/// Validate that `path` is a local filesystem path (not a network or protocol
/// URI such as `http://`, `concat:`, `subfile:`, `pipe:`, etc.) and return a
/// safe argument string for `ffmpeg`/`ffprobe -i` that cannot be misinterpreted
/// as a CLI option.
pub fn safe_local_input(path: &std::path::Path) -> Result<String, MediaError> {
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return Err(MediaError::BadData("media path cannot be empty".into()));
    }
    if raw.contains('\0') {
        return Err(MediaError::BadData("media path contains NUL byte".into()));
    }
    if let Some(colon) = raw.find(':') {
        let is_windows_drive = colon == 1
            && raw.as_bytes()[0].is_ascii_alphabetic()
            && matches!(raw.as_bytes().get(2), Some(b'\\' | b'/'));
        // FFmpeg's URL parser treats anything before the first `:` containing no
        // path separator (including `subfile,,start,0,,:`) as a protocol spec.
        let prefix = &raw[..colon];
        let has_path_sep = prefix.contains('/') || prefix.contains('\\');
        if !is_windows_drive && !has_path_sep {
            return Err(MediaError::BadData(format!(
                "protocol URI `{raw}` is not allowed; only local filesystem paths are supported"
            )));
        }
    }
    if raw.starts_with('-') {
        Ok(format!("./{raw}"))
    } else {
        Ok(raw.into_owned())
    }
}

/// Convert `#RRGGBB` to ffmpeg's `0xRRGGBB`.
pub fn hex_to_ffmpeg_color(hex: &str) -> Result<String, MediaError> {
    let ok = hex.len() == 7 && hex.starts_with('#') && hex[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return Err(MediaError::BadData(format!(
            "invalid color `{hex}`: expected #RRGGBB"
        )));
    }
    Ok(format!("0x{}", &hex[1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn safe_local_input_rejects_protocols_and_guards_leading_dash() {
        assert_eq!(
            safe_local_input(Path::new("/tmp/video.mp4")).unwrap(),
            "/tmp/video.mp4"
        );
        assert_eq!(
            safe_local_input(Path::new("/tmp/dir:name/video.mp4")).unwrap(),
            "/tmp/dir:name/video.mp4"
        );
        assert_eq!(
            safe_local_input(Path::new("-leading.mp4")).unwrap(),
            "./-leading.mp4"
        );
        assert_eq!(
            safe_local_input(Path::new("C:\\Videos\\clip.mp4")).unwrap(),
            "C:\\Videos\\clip.mp4"
        );
        for bad in [
            "http://example.com/a.mp4",
            "https://169.254.169.254/latest",
            "concat:/etc/passwd|/tmp/a.mp4",
            "subfile,,start,0,end,10,,:/etc/passwd",
            "hls+http://evil.invalid/list.m3u8",
            "pipe:0",
            "",
        ] {
            assert!(
                safe_local_input(Path::new(bad)).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }
}
