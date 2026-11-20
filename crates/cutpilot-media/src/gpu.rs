//! Cached hardware GPU / NVIDIA NVENC / NVDEC / CUDA filter detection.
//!
//! Unlike checking `ffmpeg -encoders` (which lists `h264_nvenc` whenever
//! FFmpeg was *compiled* with NVENC headers, even on machines with no GPU or
//! an older Ampere GPU that lacks AV1 NVENC), this module verifies actual
//! encoder/filter execution once per process and caches the result.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuCapabilities {
    pub gpu_name: Option<String>,
    pub nvenc_h264: bool,
    pub nvenc_hevc: bool,
    pub nvenc_av1: bool,
    pub cuda_filters: bool,
    pub nvdec: bool,
}

static CAPS: OnceLock<GpuCapabilities> = OnceLock::new();

/// Return cached GPU capabilities (probed once per process).
pub fn capabilities() -> &'static GpuCapabilities {
    CAPS.get_or_init(probe_capabilities)
}

/// True when a codec is well-supported by NVIDIA NVDEC hardware decoding.
pub fn nvdec_supported_codec(codec: Option<&str>) -> bool {
    matches!(
        codec,
        Some("h264" | "hevc" | "vp9" | "vp8" | "av1" | "mpeg2video" | "mpeg4")
    )
}

fn probe_capabilities() -> GpuCapabilities {
    let gpu_name = detect_gpu_name();
    let encoders = ffmpeg_stdout(&["-hide_banner", "-encoders"]).unwrap_or_default();
    let filters = ffmpeg_stdout(&["-hide_banner", "-filters"]).unwrap_or_default();

    let listed_h264 = encoders.contains("h264_nvenc");
    let listed_hevc = encoders.contains("hevc_nvenc");
    let listed_av1 = encoders.contains("av1_nvenc");
    let listed_cuda_filters = filters.contains("scale_cuda") && filters.contains("pad_cuda");

    // Verify actual hardware encoders (fast 1-frame null test, ~60ms each).
    // Only test HEVC/AV1/CUDA filters if h264_nvenc works so CPU-only hosts
    // pay at most one tiny probe.
    let nvenc_h264 = listed_h264 && test_nvenc_encoder("h264_nvenc");
    let nvenc_hevc = nvenc_h264 && listed_hevc && test_nvenc_encoder("hevc_nvenc");
    let nvenc_av1 = nvenc_h264 && listed_av1 && test_nvenc_encoder("av1_nvenc");
    let cuda_filters = nvenc_h264 && listed_cuda_filters && test_cuda_filters();
    let nvdec = nvenc_h264 || cuda_filters;

    GpuCapabilities {
        gpu_name,
        nvenc_h264,
        nvenc_hevc,
        nvenc_av1,
        cuda_filters,
        nvdec,
    }
}

fn detect_gpu_name() -> Option<String> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

fn ffmpeg_stdout(args: &[&str]) -> Option<String> {
    let output = Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn test_nvenc_encoder(encoder: &str) -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:r=30:d=0.05",
            "-frames:v",
            "1",
            "-c:v",
            encoder,
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn test_cuda_filters() -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-init_hw_device",
            "cuda=cu:0",
            "-filter_hw_device",
            "cu",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:r=30:d=0.05",
            "-frames:v",
            "1",
            "-vf",
            "format=nv12,hwupload_cuda,scale_cuda=128:128:format=nv12,pad_cuda=128:128:0:0",
            "-c:v",
            "h264_nvenc",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvdec_codec_allowlist() {
        assert!(nvdec_supported_codec(Some("h264")));
        assert!(nvdec_supported_codec(Some("hevc")));
        assert!(!nvdec_supported_codec(Some("prores")));
        assert!(!nvdec_supported_codec(None));
    }
}
