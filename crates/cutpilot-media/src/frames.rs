//! Frame extraction, persistent streaming video/audio decoders for real-time
//! playback, and CPU-side RGBA compositing for preview and MCP frame images.

use crate::{fmt_secs, MediaError};
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

/// A decoded RGBA frame.
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// Decode one frame at `time_secs`, letterboxed into `out_w x out_h`.
pub fn extract_frame_fit(
    path: &Path,
    time_secs: f64,
    out_w: u32,
    out_h: u32,
) -> Result<RgbaFrame, MediaError> {
    extract_frame_fast(path, time_secs, out_w, out_h, true, false, true)
}

/// Decode one frame at `time_secs`, scaled exactly to `out_w x out_h`
/// (aspect distortion is the caller's problem). Alpha is preserved.
pub fn extract_frame_scale(
    path: &Path,
    time_secs: f64,
    out_w: u32,
    out_h: u32,
) -> Result<RgbaFrame, MediaError> {
    extract_frame_fast(path, time_secs, out_w, out_h, false, false, false)
}

/// Fast single-frame decode with optional keyframe-instant seek (`fast_seek`)
/// and optional NVDEC hardware acceleration (`use_hwaccel`, with automatic
/// software fallback on codecs NVDEC does not handle).
pub fn extract_frame_fast(
    path: &Path,
    time_secs: f64,
    out_w: u32,
    out_h: u32,
    fit: bool,
    fast_seek: bool,
    use_hwaccel: bool,
) -> Result<RgbaFrame, MediaError> {
    let vf = if fit {
        format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=fast_bilinear,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1",
            w = out_w,
            h = out_h
        )
    } else {
        format!("scale={w}:{h}:flags=fast_bilinear,setsar=1", w = out_w, h = out_h)
    };
    let hw = use_hwaccel && crate::gpu::capabilities().nvdec;
    if hw {
        if let Ok(frame) =
            extract_frame_with_filter(path, time_secs, &vf, out_w, out_h, fast_seek, true)
        {
            return Ok(frame);
        }
    }
    extract_frame_with_filter(path, time_secs, &vf, out_w, out_h, fast_seek, false)
}

/// Fast timeline filmstrip thumbnail decode: keyframe-seeks to `time_secs` and
/// scales to fill `out_w x out_h` cleanly (center-cropped cover so thumbnails
/// never show distracting black bars on the timeline).
pub fn extract_filmstrip_frame(
    path: &Path,
    time_secs: f64,
    out_w: u32,
    out_h: u32,
) -> Result<RgbaFrame, MediaError> {
    let w = out_w.max(8);
    let h = out_h.max(8);
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=increase:flags=fast_bilinear,\
         crop={w}:{h},setsar=1"
    );
    let hw = crate::gpu::capabilities().nvdec;
    if hw {
        if let Ok(frame) = extract_frame_with_filter(path, time_secs, &vf, w, h, true, true) {
            return Ok(frame);
        }
    }
    extract_frame_with_filter(path, time_secs, &vf, w, h, true, false)
}

/// Decode the natural-size frame at `time_secs` (no scaling), alpha preserved.
pub fn extract_frame_native(path: &Path, time_secs: f64) -> Result<RgbaFrame, MediaError> {
    let probe = crate::ffprobe::probe(path)?;
    let (w, h) = probe.probe.oriented_size();
    if w == 0 || h == 0 {
        return Err(MediaError::BadData(format!(
            "`{}` has no decodable video stream",
            path.display()
        )));
    }
    extract_frame_with_filter(path, time_secs, "setsar=1", w, h, false, false)
}

/// Build a single-frame preview `-vf` filter chain matching export effects
/// (crop, flip, fit/scale, rotate, static zoom/pan, animated `ClipMotion`
/// evaluated at `at_ticks`, and `ColorGrade`).
pub fn build_clip_preview_vf(
    clip: &cutpilot_core::Clip,
    out_w: u32,
    out_h: u32,
    sx: f64,
    sy: f64,
    at_ticks: cutpilot_core::Ticks,
    is_base: bool,
) -> String {
    let mut chain = String::new();
    crate::export::append_crop_flip(&mut chain, clip);
    if is_base {
        chain.push_str(&format!(
            ",scale={w}:{h}:force_original_aspect_ratio=decrease:flags=fast_bilinear,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1",
            w = out_w,
            h = out_h,
        ));
        if clip.rotation_deg.abs() > 0.01 {
            let rad = (clip.rotation_deg as f64).to_radians();
            chain.push_str(&format!(",rotate={rad:.6}:fillcolor=black"));
        }
        crate::export::append_static_zoom_pan(&mut chain, clip, out_w, out_h, sx, sy);
    } else {
        chain.push_str(&format!(
            ",scale={w}:{h}:flags=fast_bilinear,setsar=1",
            w = out_w,
            h = out_h,
        ));
        if clip.rotation_deg.abs() > 0.01 {
            let rad = (clip.rotation_deg as f64).to_radians();
            chain.push_str(&format!(",format=rgba,rotate={rad:.6}:fillcolor=0x00000000"));
        }
    }
    if let Some(motion) = &clip.motion {
        let dur = clip.timeline_duration().max(1) as f64;
        let u = ((at_ticks - clip.timeline_start) as f64 / dur).clamp(0.0, 1.0) as f32;
        let (z, px, py) = motion.evaluate(u);
        if z > 1.0005 || px.abs() > 0.0005 || py.abs() > 0.0005 {
            let cw = ((out_w as f64 / z as f64).round() as u32).clamp(2, out_w);
            let ch = ((out_h as f64 / z as f64).round() as u32).clamp(2, out_h);
            let cx = ((out_w - cw) as f64 * (0.5 + 0.5 * px as f64))
                .round()
                .clamp(0.0, (out_w - cw) as f64) as u32;
            let cy = ((out_h - ch) as f64 * (0.5 + 0.5 * py as f64))
                .round()
                .clamp(0.0, (out_h - ch) as f64) as u32;
            chain.push_str(&format!(
                ",crop={cw}:{ch}:{cx}:{cy},scale={out_w}:{out_h}:flags=fast_bilinear,setsar=1"
            ));
        }
    }
    crate::export::append_color_grade(&mut chain, clip);
    if !is_base {
        chain.push_str(",format=rgba");
    }
    chain.trim_start_matches(',').to_string()
}

/// Decode one frame using a custom `-vf` chain (with optional NVDEC + automatic software fallback).
#[allow(clippy::too_many_arguments)]
pub fn extract_frame_with_vf(
    path: &Path,
    time_secs: f64,
    vf: &str,
    out_w: u32,
    out_h: u32,
    fast_seek: bool,
    use_hwaccel: bool,
) -> Result<RgbaFrame, MediaError> {
    let hw = use_hwaccel && crate::gpu::capabilities().nvdec;
    if hw {
        if let Ok(frame) =
            extract_frame_with_filter(path, time_secs, vf, out_w, out_h, fast_seek, true)
        {
            return Ok(frame);
        }
    }
    extract_frame_with_filter(path, time_secs, vf, out_w, out_h, fast_seek, false)
}

fn extract_frame_with_filter(
    path: &Path,
    time_secs: f64,
    vf: &str,
    out_w: u32,
    out_h: u32,
    fast_seek: bool,
    hwaccel_cuda: bool,
) -> Result<RgbaFrame, MediaError> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if hwaccel_cuda {
        cmd.args(["-hwaccel", "cuda"]);
    }
    if fast_seek {
        cmd.arg("-noaccurate_seek");
    }
    cmd.arg("-ss")
        .arg(fmt_secs(time_secs.max(0.0)))
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf"])
        .arg(vf)
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn()?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(MediaError::Ffmpeg {
            message: format!(
                "frame decode failed for `{}`: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let expected = out_w as usize * out_h as usize * 4;
    if output.stdout.len() != expected {
        return Err(MediaError::BadData(format!(
            "frame decode produced {} bytes, expected {expected}",
            output.stdout.len()
        )));
    }
    Ok(RgbaFrame {
        width: out_w,
        height: out_h,
        bytes: output.stdout,
    })
}

/// Persistent streaming video decoder: runs one `ffmpeg` process and yields
/// continuous RGBA frames at `fps` for smooth real-time playback without
/// per-frame process spawning.
pub struct VideoStreamDecoder {
    child: Child,
    stdout: ChildStdout,
    width: u32,
    height: u32,
    frame_bytes: usize,
}

impl VideoStreamDecoder {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        path: &Path,
        start_secs: f64,
        duration_secs: Option<f64>,
        out_w: u32,
        out_h: u32,
        fps_num: u32,
        fps_den: u32,
        use_hwaccel: bool,
    ) -> Result<Self, MediaError> {
        let vf = format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=fast_bilinear,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,fps={fn_}/{fd}",
            w = out_w,
            h = out_h,
            fn_ = fps_num.max(1),
            fd = fps_den.max(1),
        );
        Self::open_with_vf(
            path,
            start_secs,
            duration_secs,
            out_w,
            out_h,
            &vf,
            use_hwaccel,
        )
    }

    /// Open a streaming RGBA video decoder with a custom `-vf` chain.
    #[allow(clippy::too_many_arguments)]
    pub fn open_with_vf(
        path: &Path,
        start_secs: f64,
        duration_secs: Option<f64>,
        out_w: u32,
        out_h: u32,
        vf: &str,
        use_hwaccel: bool,
    ) -> Result<Self, MediaError> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-v", "error"]);
        if use_hwaccel && crate::gpu::capabilities().nvdec {
            cmd.args(["-hwaccel", "cuda"]);
        }
        cmd.arg("-ss").arg(fmt_secs(start_secs.max(0.0)));
        if let Some(dur) = duration_secs {
            if dur > 0.0 {
                cmd.arg("-t").arg(fmt_secs(dur));
            }
        }
        cmd.arg("-i")
            .arg(path)
            .args(["-an", "-sn", "-dn", "-vf"])
            .arg(vf)
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| MediaError::Ffmpeg {
            message: "failed to capture streaming decoder stdout".into(),
        })?;
        Ok(Self {
            child,
            stdout,
            width: out_w,
            height: out_h,
            frame_bytes: out_w as usize * out_h as usize * 4,
        })
    }

    /// Open a streaming RGBA video decoder for an overlay track (e.g. transparent
    /// ProRes 4444 HyperFrames animations): scales to `out_w x out_h` without opaque
    /// padding or NVDEC so the alpha channel is preserved cleanly.
    #[allow(clippy::too_many_arguments)]
    pub fn open_overlay(
        path: &Path,
        start_secs: f64,
        duration_secs: Option<f64>,
        out_w: u32,
        out_h: u32,
        fps_num: u32,
        fps_den: u32,
    ) -> Result<Self, MediaError> {
        let vf = format!(
            "scale={w}:{h}:flags=fast_bilinear,setsar=1,format=rgba,fps={fn_}/{fd}",
            w = out_w,
            h = out_h,
            fn_ = fps_num.max(1),
            fd = fps_den.max(1),
        );
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-v", "error", "-ss"])
            .arg(fmt_secs(start_secs.max(0.0)));
        if let Some(dur) = duration_secs {
            if dur > 0.0 {
                cmd.arg("-t").arg(fmt_secs(dur));
            }
        }
        cmd.arg("-i")
            .arg(path)
            .args(["-an", "-sn", "-dn", "-vf"])
            .arg(vf)
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| MediaError::Ffmpeg {
            message: "failed to capture streaming overlay decoder stdout".into(),
        })?;
        Ok(Self {
            child,
            stdout,
            width: out_w,
            height: out_h,
            frame_bytes: out_w as usize * out_h as usize * 4,
        })
    }

    /// Read the next sequential frame from the stream, or `None` at EOF.
    pub fn next_frame(&mut self) -> Option<RgbaFrame> {
        let mut bytes = vec![0u8; self.frame_bytes];
        let mut read = 0usize;
        while read < self.frame_bytes {
            match self.stdout.read(&mut bytes[read..]) {
                Ok(0) => return None,
                Ok(n) => read += n,
                Err(_) => return None,
            }
        }
        Some(RgbaFrame {
            width: self.width,
            height: self.height,
            bytes,
        })
    }
}

impl Drop for VideoStreamDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Persistent streaming audio decoder: yields interleaved stereo `f32` PCM at
/// `sample_rate` Hz (default 48,000) for real-time audio playback.
pub struct AudioStreamDecoder {
    child: Child,
    stdout: ChildStdout,
}

impl AudioStreamDecoder {
    pub fn open(
        path: &Path,
        start_secs: f64,
        duration_secs: Option<f64>,
        sample_rate: u32,
        volume: f32,
    ) -> Result<Self, MediaError> {
        Self::open_with_speed(path, start_secs, duration_secs, sample_rate, volume, 1.0)
    }

    pub fn open_with_speed(
        path: &Path,
        start_secs: f64,
        duration_secs: Option<f64>,
        sample_rate: u32,
        volume: f32,
        speed: f32,
    ) -> Result<Self, MediaError> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-v", "error", "-ss"])
            .arg(fmt_secs(start_secs.max(0.0)));
        if let Some(dur) = duration_secs {
            if dur > 0.0 {
                cmd.arg("-t").arg(fmt_secs(dur));
            }
        }
        cmd.arg("-i")
            .arg(path)
            .args(["-vn", "-sn", "-dn"]);
        let mut af_parts = Vec::new();
        let atempo = crate::export::atempo_chain(speed);
        if !atempo.is_empty() {
            af_parts.push(atempo);
        }
        if (volume - 1.0).abs() > 0.001 {
            af_parts.push(format!("volume={:.4}", volume.clamp(0.0, 4.0)));
        }
        if !af_parts.is_empty() {
            cmd.args(["-af", &af_parts.join(",")]);
        }
        cmd.args([
            "-ac",
            "2",
            "-ar",
            &sample_rate.to_string(),
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| MediaError::Ffmpeg {
            message: "failed to capture audio decoder stdout".into(),
        })?;
        Ok(Self { child, stdout })
    }

    /// Read interleaved stereo f32 samples into `out`. Returns sample count read.
    pub fn read_samples(&mut self, out: &mut [f32]) -> usize {
        let byte_len = out.len() * 4;
        let mut buf = vec![0u8; byte_len];
        let mut total = 0usize;
        while total < byte_len {
            match self.stdout.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => break,
            }
        }
        let samples = total / 4;
        for (i, slot) in out.iter_mut().take(samples).enumerate() {
            let b = i * 4;
            *slot = f32::from_le_bytes([buf[b], buf[b + 1], buf[b + 2], buf[b + 3]]);
        }
        samples
    }
}

impl Drop for AudioStreamDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Encode RGBA bytes to JPEG via ffmpeg stdin (quality 4, ~90%).
pub fn encode_jpeg(frame: &RgbaFrame) -> Result<Vec<u8>, MediaError> {
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("-s")
        .arg(format!("{}x{}", frame.width, frame.height))
        .args(["-i", "pipe:0", "-frames:v", "1", "-q:v", "4"])
        .args(["-f", "image2pipe", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin piped");
        stdin.write_all(&frame.bytes)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(MediaError::Ffmpeg {
            message: format!(
                "jpeg encode failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(output.stdout)
}

/// Alpha-composite `over` onto `base` at integer offset `(x, y)` with a
/// global opacity multiplier. In place, no allocation.
pub fn paste_rgba(
    base: &mut RgbaFrame,
    over: &RgbaFrame,
    x: i64,
    y: i64,
    opacity: f32,
) {
    let opacity = opacity.clamp(0.0, 1.0);
    let bw = base.width as i64;
    let bh = base.height as i64;
    for oy in 0..over.height as i64 {
        let by = y + oy;
        if by < 0 || by >= bh {
            continue;
        }
        let ox_row = oy as usize * over.width as usize * 4;
        let bx_row = by as usize * bw as usize * 4;
        for ox in 0..over.width as i64 {
            let bx = x + ox;
            if bx < 0 || bx >= bw {
                continue;
            }
            let si = ox_row + ox as usize * 4;
            let di = bx_row + bx as usize * 4;
            let sa = over.bytes[si + 3] as f32 / 255.0 * opacity;
            if sa <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let s = over.bytes[si + c] as f32;
                let d = base.bytes[di + c] as f32;
                let out = d * (1.0 - sa) + s * sa;
                base.bytes[di + c] = out.round().clamp(0.0, 255.0) as u8;
            }
            let da = base.bytes[di + 3] as f32 / 255.0;
            base.bytes[di + 3] = ((da * (1.0 - sa) + sa) * 255.0).round() as u8;
        }
    }
}

/// JPEG thumbnail of a single frame.
pub fn thumbnail_jpeg(path: &Path, time_secs: f64, max_width: u32) -> Result<Vec<u8>, MediaError> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-ss"])
        .arg(fmt_secs(time_secs.max(0.0)))
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!("scale='min({m},iw)':-2", m = max_width))
        .args(["-q:v", "4", "-f", "image2pipe", "pipe:1"])
        .output()?;
    if !out.status.success() {
        return Err(MediaError::Ffmpeg {
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out.stdout)
}

/// Contact sheet: `count` evenly spaced frames in a JPEG grid.
pub fn contact_sheet_jpeg(path: &Path, count: u32) -> Result<Vec<u8>, MediaError> {
    let probe = crate::ffprobe::probe(path)?;
    let duration = cutpilot_core::secs_from_ticks(probe.probe.duration_ticks);
    let count = count.clamp(2, 24);
    let cols = ((count as f64).sqrt().ceil() as u32).max(2);
    let rows = count.div_ceil(cols);
    let fps_expr = if duration > 0.0 {
        format!("fps={}/{}", count, duration)
    } else {
        "fps=1".to_string()
    };
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .arg("-vf")
        .arg(format!(
            "select='isnan(prev_selected_t)+gte(t-prev_selected_t,{dur}/{n})',{fps},scale=480:-2,tile={cols}x{rows}",
            dur = if duration > 0.0 { duration } else { 1.0 },
            n = count,
            fps = fps_expr,
            cols = cols,
            rows = rows
        ))
        .args(["-frames:v", "1", "-q:v", "5", "-f", "image2pipe", "pipe:1"])
        .output()?;
    if !out.status.success() {
        return Err(MediaError::Ffmpeg {
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_composites_alpha() {
        let mut base = RgbaFrame {
            width: 2,
            height: 1,
            bytes: vec![0, 0, 0, 255, 0, 0, 0, 255],
        };
        let over = RgbaFrame {
            width: 1,
            height: 1,
            bytes: vec![255, 255, 255, 128],
        };
        paste_rgba(&mut base, &over, 0, 0, 1.0);
        assert!((base.bytes[0] as i32 - 128).abs() <= 1, "{:?}", base.bytes);
        assert_eq!(base.bytes[3], 255);
    }
}
