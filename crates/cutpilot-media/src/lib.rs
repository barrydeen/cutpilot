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
pub mod record;
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
