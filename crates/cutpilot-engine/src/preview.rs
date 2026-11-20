//! Composite preview frames (RGBA + JPEG) from the project at a tick.
//! CPU-side compositing for v1; the GPU zero-copy pipeline is a roadmap
//! item and consumers cache these frames upstream.

use crate::error::EngineError;
use cutpilot_core::{AssetKind, Project, Ticks};
use cutpilot_media::frames::{self, RgbaFrame};

/// Render the project at `ticks` into an RGBA frame scaled so its width is
/// `max_width` (keeping the project aspect). Overlays composite with alpha;
/// captions are returned separately so callers can draw them natively.
pub fn composite_frame(
    project: &Project,
    ticks: Ticks,
    max_width: u32,
) -> Result<RgbaFrame, EngineError> {
    let max_width = max_width.clamp(64, 3840);
    let pw = max_width;
    let ph = (((pw as f64) * (project.settings.height as f64))
        / (project.settings.width.max(1) as f64))
        .round() as u32;
    let ph = ph.max(1) & !1;

    let layers = project.video_layers_at(ticks);
    let sx = pw as f64 / project.settings.width.max(1) as f64;
    let sy = ph as f64 / project.settings.height.max(1) as f64;
    let mut base: Option<RgbaFrame> = None;
    for clip in layers {
        let asset = match project.asset(&clip.asset_id) {
            Some(a) => a,
            None => continue,
        };
        let tl_delta = (ticks - clip.timeline_start).max(0);
        let src_ticks = clip.source_in + clip.source_offset_for_timeline_delta(tl_delta);
        let local = cutpilot_core::secs_from_ticks(src_ticks);
        // Images have a single frame; seek 0 always.
        let seek = if asset.kind == AssetKind::Image {
            0.0
        } else {
            local
        };
        if base.is_none() {
            let vf = frames::build_clip_preview_vf(clip, pw, ph, sx, sy, ticks, true);
            let frame = frames::extract_frame_with_vf(
                &asset.path,
                seek,
                &vf,
                pw,
                ph,
                false,
                asset.kind == AssetKind::Video,
            )?;
            base = Some(frame);
            continue;
        }
        // Overlay: scale to preview pixels (applying crop/flip/rotate/motion/color), then alpha-paste.
        let (aw, ah) = asset
            .probe
            .as_ref()
            .map(|p| p.oriented_size())
            .unwrap_or((project.settings.width, project.settings.height));
        let scale = clip.scale.clamp(0.05, 20.0) as f64;
        let ow = ((aw as f64 * scale * sx).round() as u32).clamp(2, 3840) & !1;
        let oh = ((ah as f64 * scale * sy).round() as u32).clamp(2, 2160) & !1;
        let vf = frames::build_clip_preview_vf(clip, ow, oh, sx, sy, ticks, false);
        let over = frames::extract_frame_with_vf(&asset.path, seek, &vf, ow, oh, false, false)?;
        let px = ((pw as f64 - ow as f64) / 2.0 + clip.x_offset as f64 * sx).round() as i64;
        let py = ((ph as f64 - oh as f64) / 2.0 + clip.y_offset as f64 * sy).round() as i64;
        if let Some(b) = base.as_mut() {
            frames::paste_rgba(b, &over, px, py, clip.opacity);
        }
    }

    let frame = base.unwrap_or_else(|| {
        let mut bytes = Vec::with_capacity((pw * ph * 4) as usize);
        for _ in 0..(pw * ph) {
            bytes.extend_from_slice(&[16, 16, 16, 255]);
        }
        RgbaFrame {
            width: pw,
            height: ph,
            bytes,
        }
    });
    Ok(frame)
}

/// Composite + JPEG encode in one step (MCP `preview_frame`).
pub fn composite_jpeg(
    project: &Project,
    ticks: Ticks,
    max_width: u32,
) -> Result<(Vec<u8>, u32, u32), EngineError> {
    let frame = composite_frame(project, ticks, max_width)?;
    let (w, h) = (frame.width, frame.height);
    let jpeg = frames::encode_jpeg(&frame)?;
    Ok((jpeg, w, h))
}
