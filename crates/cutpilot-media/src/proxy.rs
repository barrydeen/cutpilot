//! Fast GPU-accelerated edit proxy generation.
//!
//! 4K phone/camera footage (long-GOP H.264/HEVC) is expensive to seek randomly.
//! Generating a 720p short-GOP (`-g 15 -bf 0`) H.264 edit proxy on NVENC runs
//! at 300–600+ fps and makes timeline scrubbing and real-time playback instant.

use crate::{gpu, process, MediaError};
use cutpilot_core::{Asset, AssetKind, TICKS_PER_SEC};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Maximum width/height for edit proxies.
pub const PROXY_MAX_DIM: u32 = 1280;

/// True when an asset benefits from a low-latency edit proxy.
pub fn needs_proxy(asset: &Asset) -> bool {
    if asset.kind != AssetKind::Video {
        return false;
    }
    // Preserve alpha overlays (e.g., HyperFrames ProRes 4444) natively.
    if asset
        .generated
        .as_ref()
        .map(|g| g.transparent)
        .unwrap_or(false)
    {
        return false;
    }
    let Some(probe) = &asset.probe else {
        return false;
    };
    let (w, h) = probe.oriented_size();
    let high_res = w > PROXY_MAX_DIM || h > PROXY_MAX_DIM;
    let non_h264 = probe.video_codec.as_deref() != Some("h264");
    let long_enough = probe.duration_ticks >= TICKS_PER_SEC * 3;
    high_res || (non_h264 && long_enough) || probe.duration_ticks >= TICKS_PER_SEC * 15
}

/// Canonical proxy path inside a project directory.
pub fn proxy_path(project_dir: &Path, asset_id: uuid::Uuid) -> PathBuf {
    project_dir
        .join("cache")
        .join("proxies")
        .join(format!("{asset_id}.mp4"))
}

/// Build an edit proxy for `source` into `dest`, atomically via a `.tmp.mp4`
/// file. Uses NVDEC + `h264_nvenc` when available and falls back to `libx264`.
pub fn generate_proxy(
    source: &Path,
    dest: &Path,
    duration_secs: f64,
    codec: Option<&str>,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<PathBuf, MediaError> {
    if dest.is_file() {
        return Ok(dest.to_path_buf());
    }
    let _ = crate::safe_local_input(source)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("tmp.mp4");
    let caps = gpu::capabilities();
    let use_nvenc = caps.nvenc_h264;
    let use_nvdec = use_nvenc && caps.nvdec && gpu::nvdec_supported_codec(codec);

    let nvenc_args = build_proxy_args(source, &tmp, use_nvenc, use_nvdec);
    let run_result = process::run_with_progress(
        "ffmpeg",
        &nvenc_args,
        duration_secs.max(0.1),
        progress.clone(),
        cancel.clone(),
    );

    let final_res = match run_result {
        Ok(_) => Ok(()),
        Err(MediaError::Cancelled) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(MediaError::Cancelled);
        }
        Err(first_err) => {
            let _ = std::fs::remove_file(&tmp);
            if use_nvenc || use_nvdec {
                // Retry with software decode + software ultrafast x264.
                let sw_args = build_proxy_args(source, &tmp, false, false);
                process::run_with_progress(
                    "ffmpeg",
                    &sw_args,
                    duration_secs.max(0.1),
                    progress,
                    cancel,
                )
                .map(|_| ())
                .map_err(|_| first_err)
            } else {
                Err(first_err)
            }
        }
    };

    if let Err(e) = final_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    std::fs::rename(&tmp, dest)?;
    Ok(dest.to_path_buf())
}

pub fn build_proxy_args(
    source: &Path,
    dest: &Path,
    use_nvenc: bool,
    use_nvdec: bool,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-loglevel".into(),
        "error".into(),
        "-progress".into(),
        "pipe:1".into(),
    ];
    if use_nvdec {
        args.extend(["-hwaccel".into(), "cuda".into()]);
    }
    let safe_source = crate::safe_local_input(source).unwrap_or_else(|_| source.display().to_string());
    args.extend([
        "-protocol_whitelist".into(),
        crate::LOCAL_FILE_PROTOCOL_WHITELIST.into(),
        "-i".into(),
        safe_source,
    ]);
    // Scale longest side to PROXY_MAX_DIM, keep even dimensions, auto-rotate handled by ffmpeg.
    let vf = format!(
        "scale='if(gte(iw,ih),min({m},iw),-2)':'if(gte(iw,ih),-2,min({m},ih))',format=yuv420p",
        m = PROXY_MAX_DIM
    );
    args.extend(["-vf".into(), vf]);
    if use_nvenc {
        args.extend(
            [
                "-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ull", "-rc", "vbr", "-cq", "26",
                "-g", "15", "-bf", "0",
            ]
            .map(String::from),
        );
    } else {
        args.extend(
            [
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-crf",
                "26",
                "-g",
                "15",
                "-bf",
                "0",
            ]
            .map(String::from),
        );
    }
    // Keep audio lightweight for instant audio scrub/playback from the proxy too.
    args.extend(
        [
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-ar",
            "48000",
            "-map_metadata",
            "-1",
            "-movflags",
            "+faststart",
            "-y",
        ]
        .map(String::from),
    );
    args.push(dest.display().to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_args_use_short_gop_and_no_bframes() {
        let nv = build_proxy_args(Path::new("/in.mp4"), Path::new("/out.mp4"), true, true);
        let s = nv.join(" ");
        assert!(s.contains("-hwaccel cuda"));
        assert!(s.contains("-protocol_whitelist file -i /in.mp4"));
        assert!(s.contains("-map_metadata -1"));
        assert!(s.contains("h264_nvenc"));
        assert!(s.contains("-g 15 -bf 0"));

        let sw = build_proxy_args(Path::new("/in.mp4"), Path::new("/out.mp4"), false, false);
        let s2 = sw.join(" ");
        assert!(!s2.contains("cuda"));
        assert!(s2.contains("-protocol_whitelist file -i /in.mp4"));
        assert!(s2.contains("-map_metadata -1"));
        assert!(s2.contains("libx264"));
        assert!(s2.contains("-g 15 -bf 0"));
    }
}
