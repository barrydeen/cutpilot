//! Frame extraction and CPU-side RGBA compositing for preview and MCP
//! frame images. Playback-grade zero-copy GPU compositing is a roadmap item;
//! these helpers keep browsing smooth by caching their results upstream.

use crate::{fmt_secs, MediaError};
use std::path::Path;
use std::process::{Command, Stdio};

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
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1",
        w = out_w,
        h = out_h
    );
    extract_frame_with_filter(path, time_secs, &vf, out_w, out_h)
}

/// Decode one frame at `time_secs`, scaled exactly to `out_w x out_h`
/// (aspect distortion is the caller's problem). Alpha is preserved.
pub fn extract_frame_scale(
    path: &Path,
    time_secs: f64,
    out_w: u32,
    out_h: u32,
) -> Result<RgbaFrame, MediaError> {
    let vf = format!("scale={w}:{h},setsar=1", w = out_w, h = out_h);
    extract_frame_with_filter(path, time_secs, &vf, out_w, out_h)
}

/// Decode the natural-size frame at `time_secs` (no scaling), alpha preserved.
pub fn extract_frame_native(path: &Path, time_secs: f64) -> Result<RgbaFrame, MediaError> {
    // Two-pass: probe dims first so we know the rawvideo size.
    let probe = crate::ffprobe::probe(path)?;
    let (w, h) = probe.probe.oriented_size();
    if w == 0 || h == 0 {
        return Err(MediaError::BadData(format!(
            "`{}` has no decodable video stream",
            path.display()
        )));
    }
    let vf = "setsar=1";
    extract_frame_with_filter(path, time_secs, vf, w, h)
}

fn extract_frame_with_filter(
    path: &Path,
    time_secs: f64,
    vf: &str,
    out_w: u32,
    out_h: u32,
) -> Result<RgbaFrame, MediaError> {
    let child = Command::new("ffmpeg")
        .args(["-v", "error", "-ss"])
        .arg(fmt_secs(time_secs.max(0.0)))
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf"])
        .arg(vf)
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
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
            // Strengthen destination alpha where the overlay is opaque.
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
    let duration = openprem_core::secs_from_ticks(probe.probe.duration_ticks);
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
        // 50% white over black
        assert!((base.bytes[0] as i32 - 128).abs() <= 1, "{:?}", base.bytes);
        assert_eq!(base.bytes[3], 255);
    }
}
