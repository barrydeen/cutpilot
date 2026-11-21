//! Real-time A/V playback engine + latest-wins coalesced scrubbing decoder.
//!
//! Design goals:
//! 1. **Zero backlog on scrub**: a single latest-wins slot (`Mutex + Condvar`)
//!    replaces unbounded channels so scrubbing across 4K footage drops stale
//!    positions immediately and always decodes the current playhead.
//! 2. **Frame-quantized LRU cache**: scrubbing back and forth hits RAM in 0 ms.
//! 3. **Single reused GPU texture**: updates in-place via `TextureHandle::set`
//!    instead of allocating/destroying GPU textures every frame.
//! 4. **True real-time playback**: spawns one persistent streaming `ffmpeg`
//!    video decoder + one streaming audio decoder feeding `cpal`, synchronized
//!    to wall clock, dropping late frames if needed so A/V stays locked.

use eframe::egui;
use cutpilot_core::{AssetKind, Project, Ticks, TrackKind, TICKS_PER_SEC};
use cutpilot_media::frames::{self, AudioStreamDecoder, RgbaFrame, VideoStreamDecoder};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PREVIEW_WIDTH: u32 = 960;
const FRAME_CACHE_LIMIT: usize = 64;
const AUDIO_SAMPLE_RATE: u32 = 48_000;

#[derive(Clone)]
struct LayerSpec {
    path: PathBuf,
    seek_secs: f64,
    vf: String,
    scale_w: u32,
    scale_h: u32,
    paste_x: i64,
    paste_y: i64,
    opacity: f32,
    hwaccel: bool,
}

struct ScrubRequest {
    key: u64,
    canvas_w: u32,
    canvas_h: u32,
    layers: Vec<LayerSpec>,
    fast_seek: bool,
}

enum DecodeResult {
    ScrubFrame { key: u64, exact: bool, frame: RgbaFrame },
}

/// Active streaming playback state.
struct ActivePlayback {
    stop: Arc<AtomicBool>,
    /// Latest decoded frame pushed by the streaming playback thread.
    latest_frame: Arc<Mutex<Option<(Ticks, RgbaFrame)>>>,
    /// Wall clock anchor for smooth playhead advancement.
    started_at: Instant,
    start_ticks: Ticks,
    end_ticks: Ticks,
}

pub struct PlaybackEngine {
    // --- scrubbing (latest-wins slot) ---
    scrub_slot: Arc<(Mutex<Option<ScrubRequest>>, Condvar)>,
    scrub_rx: mpsc::Receiver<DecodeResult>,
    /// Key + whether exact decode was requested for the last submitted scrub.
    last_submitted: Option<(u64, bool)>,
    /// key -> (frame, is_exact)
    frame_cache: HashMap<u64, (RgbaFrame, bool)>,
    cache_order: VecDeque<u64>,

    // --- single reused GPU texture ---
    texture: Option<egui::TextureHandle>,
    displayed_key: Option<u64>,
    showing_black: bool,

    // --- real-time streaming playback ---
    active: Option<ActivePlayback>,
    pub muted: bool,
    pub master_volume: f32,
}

impl PlaybackEngine {
    pub fn new(ctx: egui::Context) -> Self {
        let scrub_slot: Arc<(Mutex<Option<ScrubRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let (tx, rx) = mpsc::channel::<DecodeResult>();

        let slot_worker = scrub_slot.clone();
        let repaint_ctx = ctx;
        std::thread::spawn(move || {
            loop {
                let req = {
                    let (lock, cvar) = &*slot_worker;
                    let mut guard = match lock.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    while guard.is_none() {
                        guard = match cvar.wait(guard) {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                    }
                    guard.take().unwrap()
                };
                let frame = decode_layers(&req);
                if tx
                    .send(DecodeResult::ScrubFrame {
                        key: req.key,
                        exact: !req.fast_seek,
                        frame,
                    })
                    .is_err()
                {
                    break;
                }
                repaint_ctx.request_repaint();
            }
        });

        Self {
            scrub_slot,
            scrub_rx: rx,
            last_submitted: None,
            frame_cache: HashMap::new(),
            cache_order: VecDeque::new(),
            texture: None,
            displayed_key: None,
            showing_black: false,
            active: None,
            muted: false,
            master_volume: 1.0,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.active.is_some()
    }

    /// Clear cached scrub frames (e.g. when a proxy finishes building so we
    /// immediately decode crisp frames from the proxy, or when switching projects).
    pub fn invalidate_cache(&mut self) {
        self.frame_cache.clear();
        self.cache_order.clear();
        self.last_submitted = None;
        self.displayed_key = None;
    }

    /// Start real-time A/V playback from `project.editor.playhead_ticks`.
    pub fn start_playback(
        &mut self,
        project: &Project,
        proxies: &HashMap<Uuid, PathBuf>,
        ctx: &egui::Context,
    ) {
        self.stop_playback();

        let end_ticks = project.timeline_end();
        if end_ticks <= 0 {
            return;
        }
        let mut start_ticks = project.editor.playhead_ticks.clamp(0, end_ticks);
        if start_ticks >= end_ticks - project.settings.fps.frame_duration_ticks() {
            start_ticks = 0; // restart from beginning when at the very end
        }

        let stop = Arc::new(AtomicBool::new(false));
        let latest_frame: Arc<Mutex<Option<(Ticks, RgbaFrame)>>> = Arc::new(Mutex::new(None));
        let (pw, ph) = preview_dims(project);
        let fps = project.settings.fps;

        // Build playback schedules (base video + overlay video tracks + multi-track audio mixer).
        let schedule = build_video_schedule(project, proxies, pw, ph);
        let overlay_tracks = build_overlay_schedules(project, pw, ph);
        let audio_tracks = build_audio_tracks(project, proxies, self.muted, self.master_volume);

        // Start audio playback if there is audio and not muted.
        if !self.muted && !audio_tracks.is_empty() {
            let stop_a = stop.clone();
            std::thread::spawn(move || {
                audio_playback_loop(audio_tracks, start_ticks, end_ticks, stop_a);
            });
        }

        // Start persistent streaming video decoder thread (composites base + all overlay tracks).
        let stop_v = stop.clone();
        let frame_slot = latest_frame.clone();
        let repaint_ctx = ctx.clone();
        let started_at = Instant::now();
        std::thread::spawn(move || {
            video_playback_loop(
                schedule,
                overlay_tracks,
                start_ticks,
                end_ticks,
                started_at,
                pw,
                ph,
                fps.num,
                fps.den,
                stop_v,
                frame_slot,
                repaint_ctx,
            );
        });

        self.active = Some(ActivePlayback {
            stop,
            latest_frame,
            started_at,
            start_ticks,
            end_ticks,
        });
        ctx.request_repaint();
    }

    pub fn stop_playback(&mut self) {
        if let Some(active) = self.active.take() {
            active.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Tick real-time playback: returns the new playhead position if playing,
    /// and uploads the latest streamed frame to the single GPU texture.
    pub fn tick_playback(&mut self, ctx: &egui::Context) -> Option<Ticks> {
        let active = self.active.as_mut()?;
        let elapsed_secs = active.started_at.elapsed().as_secs_f64();
        let now_ticks = active.start_ticks + cutpilot_core::ticks_from_secs(elapsed_secs);

        // Upload newest decoded playback frame (if any).
        if let Ok(mut slot) = active.latest_frame.try_lock() {
            if let Some((_t, frame)) = slot.take() {
                upload_texture(ctx, &mut self.texture, &frame);
                self.displayed_key = None;
                self.showing_black = false;
            }
        }

        if now_ticks >= active.end_ticks {
            let final_t = active.end_ticks;
            self.stop_playback();
            return Some(final_t);
        }

        ctx.request_repaint();
        Some(now_ticks)
    }

    /// Drain completed scrub frames into the LRU cache.
    pub fn poll_scrub(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.scrub_rx.try_recv() {
            let DecodeResult::ScrubFrame { key, exact, frame } = msg;
            self.frame_cache.insert(key, (frame, exact));
            self.cache_order.retain(|k| *k != key);
            self.cache_order.push_back(key);
            while self.cache_order.len() > FRAME_CACHE_LIMIT {
                if let Some(oldest) = self.cache_order.pop_front() {
                    self.frame_cache.remove(&oldest);
                }
            }
            ctx.request_repaint();
        }
    }

    /// Ensure the preview texture reflects the current playhead while paused/scrubbing.
    pub fn update_scrub_preview(
        &mut self,
        project: &Project,
        proxies: &HashMap<Uuid, PathBuf>,
        actively_scrubbing: bool,
        ctx: &egui::Context,
    ) {
        if self.is_playing() {
            return;
        }

        let ticks = project.editor.playhead_ticks;
        let frame_idx = project.settings.fps.frame_at(ticks);
        let quantized_ticks = project.settings.fps.ticks_at_frame(frame_idx);
        let (pw, ph) = preview_dims(project);

        let mut layers = Vec::new();
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        pw.hash(&mut hasher);
        ph.hash(&mut hasher);

        let base_track_id = project.base_video_track().map(|t| t.id);
        for clip in project.video_layers_at(ticks) {
            let Some(asset) = project.asset(&clip.asset_id) else {
                continue;
            };
            let is_base = base_track_id == Some(clip.track_id);
            // Use proxy for base video layer (never for transparent overlays).
            let path = if is_base {
                proxies
                    .get(&asset.id)
                    .filter(|p| p.is_file())
                    .cloned()
                    .unwrap_or_else(|| asset.path.clone())
            } else {
                asset.path.clone()
            };
            let tl_delta = (quantized_ticks - clip.timeline_start).max(0);
            let local_ticks =
                clip.source_in + clip.source_offset_for_timeline_delta(tl_delta);
            let seek_secs = if asset.kind == AssetKind::Image {
                0.0
            } else {
                cutpilot_core::secs_from_ticks(local_ticks)
            };
            let (aw, ah) = asset
                .probe
                .as_ref()
                .map(|p| p.oriented_size())
                .unwrap_or((project.settings.width, project.settings.height));
            let sx = pw as f64 / project.settings.width.max(1) as f64;
            let sy = ph as f64 / project.settings.height.max(1) as f64;

            let (sw, sh, px, py, opacity, hwaccel) = if is_base {
                (
                    pw,
                    ph,
                    0,
                    0,
                    1.0,
                    asset.kind == AssetKind::Video,
                )
            } else {
                let ow = ((aw as f64 * clip.scale as f64 * sx).round() as u32).max(2) & !1;
                let oh = ((ah as f64 * clip.scale as f64 * sy).round() as u32).max(2) & !1;
                (
                    ow,
                    oh,
                    ((pw as f64 - ow as f64) / 2.0 + clip.x_offset as f64 * sx).round() as i64,
                    ((ph as f64 - oh as f64) / 2.0 + clip.y_offset as f64 * sy).round() as i64,
                    clip.opacity,
                    false,
                )
            };
            let vf = frames::build_clip_preview_vf(
                clip,
                sw,
                sh,
                sx,
                sy,
                quantized_ticks,
                is_base,
            );
            clip.id.hash(&mut hasher);
            path.hash(&mut hasher);
            local_ticks.hash(&mut hasher);
            vf.hash(&mut hasher);
            sw.hash(&mut hasher);
            sh.hash(&mut hasher);
            px.hash(&mut hasher);
            py.hash(&mut hasher);
            opacity.to_bits().hash(&mut hasher);
            layers.push(LayerSpec {
                path,
                seek_secs,
                vf,
                scale_w: sw,
                scale_h: sh,
                paste_x: px,
                paste_y: py,
                opacity,
                hwaccel,
            });
        }

        // Playhead over a gap -> show clean dark frame immediately.
        if layers.is_empty() {
            if !self.showing_black {
                let black = solid_frame(pw, ph, [14, 15, 19, 255]);
                upload_texture(ctx, &mut self.texture, &black);
                self.displayed_key = None;
                self.showing_black = true;
            }
            return;
        }

        let key = hasher.finish();

        // Check LRU cache first.
        let need_exact = !actively_scrubbing;
        if let Some((frame, is_exact)) = self.frame_cache.get(&key) {
            if self.displayed_key != Some(key) || self.showing_black {
                upload_texture(ctx, &mut self.texture, frame);
                self.displayed_key = Some(key);
                self.showing_black = false;
            }
            if *is_exact || !need_exact {
                return;
            }
        }

        // Coalesced latest-wins submission (never piles up stale requests!).
        if self.last_submitted == Some((key, need_exact)) {
            return;
        }
        self.last_submitted = Some((key, need_exact));

        let (lock, cvar) = &*self.scrub_slot;
        if let Ok(mut guard) = lock.lock() {
            *guard = Some(ScrubRequest {
                key,
                canvas_w: pw,
                canvas_h: ph,
                layers,
                fast_seek: actively_scrubbing,
            });
            cvar.notify_one();
        }
    }

    pub fn texture(&self) -> Option<&egui::TextureHandle> {
        self.texture.as_ref()
    }
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        self.stop_playback();
    }
}

fn preview_dims(project: &Project) -> (u32, u32) {
    let w = project.settings.width.max(2) as f64;
    let h = project.settings.height.max(2) as f64;
    if w >= h {
        let pw = PREVIEW_WIDTH;
        let ph = ((pw as f64 * (h / w)).round() as u32).max(2) & !1;
        (pw, ph)
    } else {
        // Portrait video: cap height to 720 so it fits cleanly and decodes fast.
        let ph = 720u32;
        let pw = ((ph as f64 * (w / h)).round() as u32).max(2) & !1;
        (pw, ph)
    }
}

fn upload_texture(
    ctx: &egui::Context,
    slot: &mut Option<egui::TextureHandle>,
    frame: &RgbaFrame,
) {
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [frame.width as usize, frame.height as usize],
        &frame.bytes,
    );
    match slot {
        Some(handle) if handle.size() == image.size => {
            handle.set(image, egui::TextureOptions::LINEAR);
        }
        _ => {
            *slot = Some(ctx.load_texture(
                "cutpilot-preview",
                image,
                egui::TextureOptions::LINEAR,
            ));
        }
    }
}

fn solid_frame(w: u32, h: u32, rgba: [u8; 4]) -> RgbaFrame {
    let count = (w * h) as usize;
    let mut bytes = Vec::with_capacity(count * 4);
    for _ in 0..count {
        bytes.extend_from_slice(&rgba);
    }
    RgbaFrame {
        width: w,
        height: h,
        bytes,
    }
}

fn decode_layers(req: &ScrubRequest) -> RgbaFrame {
    let mut canvas = solid_frame(req.canvas_w, req.canvas_h, [14, 15, 19, 255]);
    for layer in &req.layers {
        let Ok(over) = frames::extract_frame_with_vf(
            &layer.path,
            layer.seek_secs,
            &layer.vf,
            layer.scale_w,
            layer.scale_h,
            req.fast_seek,
            layer.hwaccel,
        ) else {
            continue;
        };
        frames::paste_rgba(&mut canvas, &over, layer.paste_x, layer.paste_y, layer.opacity);
    }
    canvas
}

// ---------------------------------------------------------------------------
// Real-time streaming video playback thread
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct VideoSegment {
    timeline_start: Ticks,
    timeline_end: Ticks,
    source_in: Ticks,
    speed: f32,
    static_vf: String,
    path: PathBuf,
    is_image: bool,
    using_proxy: bool,
}

fn build_video_schedule(
    project: &Project,
    proxies: &HashMap<Uuid, PathBuf>,
    pw: u32,
    ph: u32,
) -> Vec<VideoSegment> {
    let Some(base) = project.base_video_track() else {
        return Vec::new();
    };
    if base.hidden {
        return Vec::new();
    }
    let sx = pw as f64 / project.settings.width.max(1) as f64;
    let sy = ph as f64 / project.settings.height.max(1) as f64;
    let mut segs = Vec::new();
    for clip in project.clips_on_track(&base.id) {
        let Some(asset) = project.asset(&clip.asset_id) else {
            continue;
        };
        let (path, using_proxy) = match proxies.get(&asset.id) {
            Some(p) if p.is_file() => (p.clone(), true),
            _ => (asset.path.clone(), false),
        };
        // Precompute static video effects (crop, flip, fit, rotate, static zoom/pan, color).
        let mut static_clip = clip.clone();
        static_clip.motion = None;
        let static_vf =
            frames::build_clip_preview_vf(&static_clip, pw, ph, sx, sy, clip.timeline_start, true);
        segs.push(VideoSegment {
            timeline_start: clip.timeline_start,
            timeline_end: clip.timeline_end(),
            source_in: clip.source_in,
            speed: clip.effective_speed(),
            static_vf,
            path,
            is_image: asset.kind == AssetKind::Image,
            using_proxy,
        });
    }
    segs
}

#[derive(Clone)]
struct OverlaySegment {
    timeline_start: Ticks,
    timeline_end: Ticks,
    source_in: Ticks,
    path: PathBuf,
    is_image: bool,
    scale_w: u32,
    scale_h: u32,
    paste_x: i64,
    paste_y: i64,
    opacity: f32,
}

/// Build per-track overlay schedules for every visible video track above the
/// base video track, in back-to-front compositing order (ascending track index).
fn build_overlay_schedules(project: &Project, pw: u32, ph: u32) -> Vec<Vec<OverlaySegment>> {
    let base_id = project.base_video_track().map(|t| t.id);
    let sx = pw as f64 / project.settings.width.max(1) as f64;
    let sy = ph as f64 / project.settings.height.max(1) as f64;
    let mut tracks_out = Vec::new();

    for track in project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video && Some(t.id) != base_id && !t.hidden)
    {
        let mut segs = Vec::new();
        for clip in project.clips_on_track(&track.id) {
            let Some(asset) = project.asset(&clip.asset_id) else {
                continue;
            };
            if clip.opacity <= 0.001 {
                continue;
            }
            let (aw, ah) = asset
                .probe
                .as_ref()
                .map(|p| p.oriented_size())
                .unwrap_or((project.settings.width, project.settings.height));
            let ow = ((aw as f64 * clip.scale as f64 * sx).round() as u32).max(2) & !1;
            let oh = ((ah as f64 * clip.scale as f64 * sy).round() as u32).max(2) & !1;
            let px = ((pw as f64 - ow as f64) / 2.0 + clip.x_offset as f64 * sx).round() as i64;
            let py = ((ph as f64 - oh as f64) / 2.0 + clip.y_offset as f64 * sy).round() as i64;
            segs.push(OverlaySegment {
                timeline_start: clip.timeline_start,
                timeline_end: clip.timeline_end(),
                source_in: clip.source_in,
                path: asset.path.clone(),
                is_image: asset.kind == AssetKind::Image,
                scale_w: ow,
                scale_h: oh,
                paste_x: px,
                paste_y: py,
                opacity: clip.opacity,
            });
        }
        if !segs.is_empty() {
            tracks_out.push(segs);
        }
    }
    tracks_out
}

struct OverlayTrackState {
    segments: Vec<OverlaySegment>,
    active_idx: Option<usize>,
    decoder: Option<VideoStreamDecoder>,
    cached_image: Option<RgbaFrame>,
    last_frame: Option<RgbaFrame>,
    decoded_ticks: Ticks,
}

impl OverlayTrackState {
    fn new(segments: Vec<OverlaySegment>) -> Self {
        Self {
            segments,
            active_idx: None,
            decoder: None,
            cached_image: None,
            last_frame: None,
            decoded_ticks: 0,
        }
    }

    fn composite_at(
        &mut self,
        canvas: &mut RgbaFrame,
        at_ticks: Ticks,
        frame_ticks: Ticks,
        fps_num: u32,
        fps_den: u32,
    ) {
        let seg_idx = self
            .segments
            .iter()
            .position(|s| s.timeline_start <= at_ticks && at_ticks < s.timeline_end);

        let Some(idx) = seg_idx else {
            if self.active_idx.is_some() {
                self.active_idx = None;
                self.decoder = None;
                self.cached_image = None;
                self.last_frame = None;
            }
            return;
        };

        let seg = &self.segments[idx];
        if self.active_idx != Some(idx) {
            self.active_idx = Some(idx);
            self.decoder = None;
            self.cached_image = None;
            self.last_frame = None;

            if seg.is_image {
                self.cached_image = frames::extract_frame_fast(
                    &seg.path,
                    0.0,
                    seg.scale_w,
                    seg.scale_h,
                    false,
                    false,
                    false,
                )
                .ok();
            } else {
                let offset = (at_ticks - seg.timeline_start).max(0);
                let src_secs = cutpilot_core::secs_from_ticks(seg.source_in + offset);
                let rem_secs =
                    cutpilot_core::secs_from_ticks((seg.timeline_end - at_ticks).max(frame_ticks));
                self.decoder = VideoStreamDecoder::open_overlay(
                    &seg.path,
                    src_secs,
                    Some(rem_secs),
                    seg.scale_w,
                    seg.scale_h,
                    fps_num,
                    fps_den,
                )
                .ok();
                self.decoded_ticks = at_ticks - frame_ticks;
            }
        }

        if seg.is_image {
            if let Some(img) = &self.cached_image {
                frames::paste_rgba(canvas, img, seg.paste_x, seg.paste_y, seg.opacity);
            }
            return;
        }

        if let Some(dec) = self.decoder.as_mut() {
            while self.decoded_ticks < at_ticks {
                match dec.next_frame() {
                    Some(f) => {
                        self.last_frame = Some(f);
                        self.decoded_ticks += frame_ticks;
                    }
                    None => {
                        self.decoder = None;
                        break;
                    }
                }
            }
        }
        if let Some(ov) = &self.last_frame {
            frames::paste_rgba(canvas, ov, seg.paste_x, seg.paste_y, seg.opacity);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn video_playback_loop(
    schedule: Vec<VideoSegment>,
    overlay_schedules: Vec<Vec<OverlaySegment>>,
    start_ticks: Ticks,
    end_ticks: Ticks,
    started_at: Instant,
    pw: u32,
    ph: u32,
    fps_num: u32,
    fps_den: u32,
    stop: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<(Ticks, RgbaFrame)>>>,
    ctx: egui::Context,
) {
    let fps = (fps_num as f64 / fps_den.max(1) as f64).clamp(1.0, 120.0);
    let frame_ticks = (TICKS_PER_SEC as f64 / fps).round() as Ticks;

    let mut cur_seg_idx: Option<usize> = None;
    let mut decoder: Option<VideoStreamDecoder> = None;
    let mut decoded_ticks: Ticks = start_ticks;
    let mut overlays: Vec<OverlayTrackState> = overlay_schedules
        .into_iter()
        .map(OverlayTrackState::new)
        .collect();

    while !stop.load(Ordering::Relaxed) {
        let now_ticks =
            start_ticks + cutpilot_core::ticks_from_secs(started_at.elapsed().as_secs_f64());
        if now_ticks >= end_ticks {
            break;
        }

        // Find active segment at `now_ticks`.
        let seg_idx = schedule
            .iter()
            .position(|s| s.timeline_start <= now_ticks && now_ticks < s.timeline_end);

        match seg_idx {
            None => {
                // In a gap on V1: drop base decoder, still composite any active overlay tracks!
                if cur_seg_idx.is_some() {
                    decoder = None;
                    cur_seg_idx = None;
                }
                let mut canvas = solid_frame(pw, ph, [14, 15, 19, 255]);
                for ov in &mut overlays {
                    ov.composite_at(&mut canvas, now_ticks, frame_ticks, fps_num, fps_den);
                }
                if let Ok(mut slot) = latest_frame.lock() {
                    *slot = Some((now_ticks, canvas));
                }
                ctx.request_repaint();
                std::thread::sleep(Duration::from_millis(16));
            }
            Some(idx) => {
                let seg = &schedule[idx];
                if cur_seg_idx != Some(idx) || decoder.is_none() {
                    cur_seg_idx = Some(idx);
                    if seg.is_image {
                        decoder = None;
                        if let Ok(mut img) =
                            frames::extract_frame_fast(&seg.path, 0.0, pw, ph, true, false, false)
                        {
                            for ov in &mut overlays {
                                ov.composite_at(&mut img, now_ticks, frame_ticks, fps_num, fps_den);
                            }
                            if let Ok(mut slot) = latest_frame.lock() {
                                *slot = Some((now_ticks, img));
                            }
                            ctx.request_repaint();
                        }
                        std::thread::sleep(Duration::from_millis(33));
                        continue;
                    }
                    let tl_offset = (now_ticks - seg.timeline_start).max(0);
                    let src_offset = (tl_offset as f64 * seg.speed as f64).round() as Ticks;
                    let src_secs = cutpilot_core::secs_from_ticks(seg.source_in + src_offset);
                    let rem_src_secs = cutpilot_core::secs_from_ticks(
                        (seg.timeline_end - now_ticks).max(frame_ticks),
                    ) * seg.speed as f64;
                    let mut stream_vf = if (seg.speed - 1.0).abs() > 0.001 {
                        format!("setpts=(PTS-STARTPTS)/{:.6},{}", seg.speed, seg.static_vf)
                    } else {
                        seg.static_vf.clone()
                    };
                    stream_vf.push_str(&format!(",fps={}/{}", fps_num.max(1), fps_den.max(1)));
                    // Proxies are 720p H.264 — decode super fast on CPU or NVDEC;
                    // use NVDEC for full-res originals.
                    let use_hw = !seg.using_proxy;
                    decoder = VideoStreamDecoder::open_with_vf(
                        &seg.path,
                        src_secs,
                        Some(rem_src_secs),
                        pw,
                        ph,
                        &stream_vf,
                        use_hw,
                    )
                    .or_else(|_| {
                        VideoStreamDecoder::open_with_vf(
                            &seg.path,
                            src_secs,
                            Some(rem_src_secs),
                            pw,
                            ph,
                            &stream_vf,
                            false,
                        )
                    })
                    .ok();
                    decoded_ticks = now_ticks;
                }

                if seg.is_image {
                    if let Ok(mut img) = frames::extract_frame_with_vf(
                        &seg.path,
                        0.0,
                        &seg.static_vf,
                        pw,
                        ph,
                        false,
                        false,
                    ) {
                        for ov in &mut overlays {
                            ov.composite_at(&mut img, now_ticks, frame_ticks, fps_num, fps_den);
                        }
                        if let Ok(mut slot) = latest_frame.lock() {
                            *slot = Some((now_ticks, img));
                        }
                        ctx.request_repaint();
                    }
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }

                let Some(dec) = decoder.as_mut() else {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                };

                // Pace frames to wall clock; drop late frames if behind by > 2 frames.
                let mut frame_opt = dec.next_frame();
                decoded_ticks += frame_ticks;
                let mut current_now = start_ticks
                    + cutpilot_core::ticks_from_secs(started_at.elapsed().as_secs_f64());

                while decoded_ticks + frame_ticks * 2 < current_now {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    match dec.next_frame() {
                        Some(f) => {
                            frame_opt = Some(f);
                            decoded_ticks += frame_ticks;
                            current_now = start_ticks
                                + cutpilot_core::ticks_from_secs(started_at.elapsed().as_secs_f64());
                        }
                        None => {
                            frame_opt = None;
                            break;
                        }
                    }
                }

                if let Some(mut frame) = frame_opt {
                    for ov in &mut overlays {
                        ov.composite_at(&mut frame, decoded_ticks, frame_ticks, fps_num, fps_den);
                    }
                    if let Ok(mut slot) = latest_frame.lock() {
                        *slot = Some((decoded_ticks, frame));
                    }
                    ctx.request_repaint();
                } else {
                    // End of segment stream; wait for wall clock to move past it.
                    decoder = None;
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                // Sleep until it is time for the next frame.
                let ahead_ticks = decoded_ticks - current_now;
                if ahead_ticks > 0 {
                    let sleep_secs = cutpilot_core::secs_from_ticks(ahead_ticks).min(0.05);
                    std::thread::sleep(Duration::from_secs_f64(sleep_secs));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Real-time multi-track streaming audio playback & mixer
// ---------------------------------------------------------------------------

/// `TICKS_PER_SEC (3,600,000) / AUDIO_SAMPLE_RATE (48,000)` is an exact integer.
const TICKS_PER_AUDIO_FRAME: Ticks = TICKS_PER_SEC / AUDIO_SAMPLE_RATE as Ticks;
const AUDIO_MIX_BLOCK_FRAMES: usize = 1024;

#[derive(Debug, Clone)]
struct AudioSegment {
    timeline_start: Ticks,
    timeline_end: Ticks,
    source_in: Ticks,
    speed: f32,
    path: PathBuf,
    volume: f32,
    fade_in_ticks: Ticks,
    fade_out_ticks: Ticks,
}

/// Build per-track audio schedules matching `export.rs` semantics:
/// - Every unmuted Audio track (`A1`, `A2`, …) gets its own mixed track schedule.
/// - Any unlinked embedded audio on the base video track is mixed in as well
///   (linked pairs use their Audio track clip as authoritative so audio is never doubled).
fn build_audio_tracks(
    project: &Project,
    proxies: &HashMap<Uuid, PathBuf>,
    muted: bool,
    master_volume: f32,
) -> Vec<Vec<AudioSegment>> {
    if muted || master_volume <= 0.001 {
        return Vec::new();
    }

    let mut tracks_out: Vec<Vec<AudioSegment>> = Vec::new();
    let audio_track_ids: std::collections::HashSet<Uuid> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .map(|t| t.id)
        .collect();
    let audio_track_clips: Vec<&cutpilot_core::Clip> = project
        .clips
        .iter()
        .filter(|c| audio_track_ids.contains(&c.track_id))
        .collect();
    let linked_audio_groups: std::collections::HashSet<Uuid> = audio_track_clips
        .iter()
        .filter_map(|c| c.link_group)
        .collect();

    for track in project.tracks.iter().filter(|t| t.kind == TrackKind::Audio) {
        if track.muted || track.volume <= 0.001 {
            continue;
        }
        let mut segs = Vec::new();
        for clip in project.clips_on_track(&track.id) {
            let Some(asset) = project.asset(&clip.asset_id) else {
                continue;
            };
            let vol = (clip.volume * track.volume * master_volume).clamp(0.0, 4.0);
            if vol <= 0.001 {
                continue;
            }
            let path = proxies
                .get(&asset.id)
                .filter(|p| p.is_file())
                .cloned()
                .unwrap_or_else(|| asset.path.clone());
            segs.push(AudioSegment {
                timeline_start: clip.timeline_start,
                timeline_end: clip.timeline_end(),
                source_in: clip.source_in,
                speed: clip.effective_speed(),
                path,
                volume: vol,
                fade_in_ticks: clip.fade_in_ticks,
                fade_out_ticks: clip.fade_out_ticks,
            });
        }
        if !segs.is_empty() {
            segs.sort_by_key(|s| s.timeline_start);
            tracks_out.push(segs);
        }
    }

    // Include unlinked embedded audio from base video clips so unlinked video
    // clips still play sound alongside standalone audio tracks (e.g. WAV on A1/A2).
    if let Some(base) = project.base_video_track() {
        if !base.muted && base.volume > 0.001 {
            let mut base_segs = Vec::new();
            for clip in project.clips_on_track(&base.id) {
                let has_linked = clip
                    .link_group
                    .map(|g| linked_audio_groups.contains(&g))
                    .unwrap_or(false);
                let covered_unlinked = audio_track_clips.iter().any(|ac| {
                    ac.asset_id == clip.asset_id
                        && ac.timeline_start < clip.timeline_end()
                        && clip.timeline_start < ac.timeline_end()
                });
                if has_linked || covered_unlinked {
                    continue;
                }
                let Some(asset) = project.asset(&clip.asset_id) else {
                    continue;
                };
                if asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false) {
                    let vol = (clip.volume * base.volume * master_volume).clamp(0.0, 4.0);
                    if vol <= 0.001 {
                        continue;
                    }
                    let path = proxies
                        .get(&asset.id)
                        .filter(|p| p.is_file())
                        .cloned()
                        .unwrap_or_else(|| asset.path.clone());
                    base_segs.push(AudioSegment {
                        timeline_start: clip.timeline_start,
                        timeline_end: clip.timeline_end(),
                        source_in: clip.source_in,
                        speed: clip.effective_speed(),
                        path,
                        volume: vol,
                        fade_in_ticks: clip.fade_in_ticks,
                        fade_out_ticks: clip.fade_out_ticks,
                    });
                }
            }
            if !base_segs.is_empty() {
                base_segs.sort_by_key(|s| s.timeline_start);
                tracks_out.push(base_segs);
            }
        }
    }

    tracks_out
}

fn segment_fade_gain(tick: Ticks, seg: &AudioSegment) -> f32 {
    let mut gain = 1.0_f32;
    if seg.fade_in_ticks > 0 {
        let elapsed = tick - seg.timeline_start;
        if elapsed < seg.fade_in_ticks {
            gain *= (elapsed.max(0) as f32 / seg.fade_in_ticks as f32).clamp(0.0, 1.0);
        }
    }
    if seg.fade_out_ticks > 0 {
        let remaining = seg.timeline_end - tick;
        if remaining < seg.fade_out_ticks {
            gain *= (remaining.max(0) as f32 / seg.fade_out_ticks as f32).clamp(0.0, 1.0);
        }
    }
    gain
}

fn spawn_audio_sink() -> Option<Child> {
    // Prefer PipeWire / PulseAudio low-latency raw PCM sink, falling back to ALSA / ffplay.
    let candidates: &[(&str, &[&str])] = &[
        (
            "paplay",
            &[
                "--raw",
                "--format=float32le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=40",
            ],
        ),
        (
            "pw-play",
            &[
                "--raw",
                "--format",
                "f32",
                "--rate",
                "48000",
                "--channels",
                "2",
                "-",
            ],
        ),
        ("aplay", &["-q", "-f", "FLOAT_LE", "-r", "48000", "-c", "2", "-"]),
        (
            "ffplay",
            &[
                "-nodisp",
                "-autoexit",
                "-loglevel",
                "quiet",
                "-f",
                "f32le",
                "-ar",
                "48000",
                "-ch_layout",
                "stereo",
                "-i",
                "pipe:0",
            ],
        ),
    ];
    for (bin, args) in candidates {
        if let Ok(child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            return Some(child);
        }
    }
    None
}

struct TrackMixerState {
    segments: Vec<AudioSegment>,
    seg_idx: usize,
    attempted_seg: Option<usize>,
    decoder: Option<AudioStreamDecoder>,
}

fn audio_playback_loop(
    tracks: Vec<Vec<AudioSegment>>,
    start_ticks: Ticks,
    end_ticks: Ticks,
    stop: Arc<AtomicBool>,
) {
    let Some(mut sink) = spawn_audio_sink() else {
        return;
    };
    let Some(mut stdin) = sink.stdin.take() else {
        let _ = sink.kill();
        let _ = sink.wait();
        return;
    };

    let sr = AUDIO_SAMPLE_RATE;
    let mut states: Vec<TrackMixerState> = tracks
        .into_iter()
        .map(|segments| TrackMixerState {
            segments,
            seg_idx: 0,
            attempted_seg: None,
            decoder: None,
        })
        .collect();

    let mut mix_buf = vec![0.0_f32; AUDIO_MIX_BLOCK_FRAMES * 2];
    let mut temp_buf = vec![0.0_f32; AUDIO_MIX_BLOCK_FRAMES * 2];
    let mut byte_buf = vec![0_u8; AUDIO_MIX_BLOCK_FRAMES * 2 * 4];
    let mut frame_cursor: i64 = 0;

    while !stop.load(Ordering::Relaxed) {
        let block_start = start_ticks + frame_cursor * TICKS_PER_AUDIO_FRAME;
        if block_start >= end_ticks {
            break;
        }
        let frames_this_block = (((end_ticks - block_start) + TICKS_PER_AUDIO_FRAME - 1)
            / TICKS_PER_AUDIO_FRAME)
            .clamp(1, AUDIO_MIX_BLOCK_FRAMES as i64) as usize;
        let block_end = block_start + frames_this_block as i64 * TICKS_PER_AUDIO_FRAME;

        mix_buf[..frames_this_block * 2].fill(0.0);

        for track in &mut states {
            loop {
                while track.seg_idx < track.segments.len()
                    && track.segments[track.seg_idx].timeline_end <= block_start
                {
                    track.decoder = None;
                    track.attempted_seg = None;
                    track.seg_idx += 1;
                }
                let Some(seg) = track.segments.get(track.seg_idx) else {
                    break;
                };
                if seg.timeline_start >= block_end {
                    break;
                }

                let ov_start = block_start.max(seg.timeline_start);
                let ov_end = block_end.min(seg.timeline_end);
                let f0 = ((ov_start - block_start) / TICKS_PER_AUDIO_FRAME)
                    .clamp(0, frames_this_block as i64) as usize;
                let f1 = (((ov_end - block_start) + TICKS_PER_AUDIO_FRAME - 1)
                    / TICKS_PER_AUDIO_FRAME)
                    .clamp(f0 as i64, frames_this_block as i64) as usize;

                if f1 > f0 {
                    if track.decoder.is_none() && track.attempted_seg != Some(track.seg_idx) {
                        track.attempted_seg = Some(track.seg_idx);
                        let offset = (ov_start - seg.timeline_start).max(0);
                        let src_offset = (offset as f64 * seg.speed as f64).round() as Ticks;
                        let src_secs = cutpilot_core::secs_from_ticks(seg.source_in + src_offset);
                        let dur_secs = cutpilot_core::secs_from_ticks(
                            (seg.timeline_end - ov_start).max(TICKS_PER_AUDIO_FRAME),
                        ) * seg.speed as f64;
                        track.decoder = AudioStreamDecoder::open_with_speed(
                            &seg.path,
                            src_secs,
                            Some(dur_secs),
                            sr,
                            seg.volume,
                            seg.speed,
                        )
                        .ok();
                    }
                    if let Some(dec) = track.decoder.as_mut() {
                        let needed = (f1 - f0) * 2;
                        let read = dec.read_samples(&mut temp_buf[..needed]);
                        let read_frames = read / 2;
                        let has_fades = seg.fade_in_ticks > 0 || seg.fade_out_ticks > 0;
                        for j in 0..read_frames {
                            let gain = if has_fades {
                                let t_frame =
                                    block_start + (f0 + j) as i64 * TICKS_PER_AUDIO_FRAME;
                                segment_fade_gain(t_frame, seg)
                            } else {
                                1.0
                            };
                            let dst = (f0 + j) * 2;
                            mix_buf[dst] += temp_buf[j * 2] * gain;
                            mix_buf[dst + 1] += temp_buf[j * 2 + 1] * gain;
                        }
                    }
                }

                if seg.timeline_end <= block_end {
                    track.decoder = None;
                    track.attempted_seg = None;
                    track.seg_idx += 1;
                } else {
                    break;
                }
            }
        }

        let sample_count = frames_this_block * 2;
        for (i, &s) in mix_buf[..sample_count].iter().enumerate() {
            let clamped = s.clamp(-1.0, 1.0);
            let b = clamped.to_le_bytes();
            let off = i * 4;
            byte_buf[off..off + 4].copy_from_slice(&b);
        }
        if stdin.write_all(&byte_buf[..sample_count * 4]).is_err() {
            break;
        }
        frame_cursor += frames_this_block as i64;
    }

    drop(states);
    drop(stdin);
    let _ = sink.kill();
    let _ = sink.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutpilot_core::{Asset, Clip, MediaProbe, ProjectSettings, Track};

    #[test]
    fn multi_track_audio_schedule_includes_wav_alongside_linked_audio() {
        let mut p = Project::new("mix", ProjectSettings::default());
        let vid = Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Video,
            name: "cam".into(),
            path: PathBuf::from("/tmp/cam.mp4"),
            probe: Some(MediaProbe {
                duration_ticks: TICKS_PER_SEC * 10,
                width: 1920,
                height: 1080,
                fps: None,
                has_audio: true,
                rotation_deg: 0,
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                sample_rate: Some(48_000),
            }),
            generated: None,
        };
        let wav = Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Audio,
            name: "music".into(),
            path: PathBuf::from("/tmp/music.wav"),
            probe: Some(MediaProbe {
                duration_ticks: TICKS_PER_SEC * 8,
                width: 0,
                height: 0,
                fps: None,
                has_audio: true,
                rotation_deg: 0,
                video_codec: None,
                audio_codec: Some("pcm_s16le".into()),
                sample_rate: Some(44_100),
            }),
            generated: None,
        };
        let vid_id = vid.id;
        let wav_id = wav.id;
        p.assets.extend([vid, wav]);

        let a2 = Track {
            id: Uuid::new_v4(),
            kind: TrackKind::Audio,
            name: "A2".into(),
            muted: false,
            hidden: false,
            locked: false,
            volume: 0.8,
        };
        let a2_id = a2.id;
        p.tracks.push(a2);

        let group = Uuid::new_v4();
        p.clips.push(Clip {
            id: Uuid::new_v4(),
            asset_id: vid_id,
            track_id: p.tracks[0].id,
            timeline_start: 0,
            source_in: 0,
            source_out: TICKS_PER_SEC * 10,
            link_group: Some(group),
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: Default::default(),
            motion: None,
            color: Default::default(),
        });
        p.clips.push(Clip {
            id: Uuid::new_v4(),
            asset_id: vid_id,
            track_id: p.tracks[1].id,
            timeline_start: 0,
            source_in: 0,
            source_out: TICKS_PER_SEC * 10,
            link_group: Some(group),
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: Default::default(),
            motion: None,
            color: Default::default(),
        });
        p.clips.push(Clip {
            id: Uuid::new_v4(),
            asset_id: wav_id,
            track_id: a2_id,
            timeline_start: TICKS_PER_SEC,
            source_in: 0,
            source_out: TICKS_PER_SEC * 5,
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 0.5,
            fade_in_ticks: TICKS_PER_SEC / 2,
            fade_out_ticks: TICKS_PER_SEC / 2,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: Default::default(),
            motion: None,
            color: Default::default(),
        });

        let tracks = build_audio_tracks(&p, &HashMap::new(), false, 1.0);
        // Should contain A1 (linked video audio) AND A2 (WAV), without doubling V1.
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0][0].path, PathBuf::from("/tmp/cam.mp4"));
        assert_eq!(tracks[1][0].path, PathBuf::from("/tmp/music.wav"));
        assert!((tracks[1][0].volume - 0.4).abs() < 1e-4);

        // Verify fade envelope math.
        let seg = &tracks[1][0];
        assert!((segment_fade_gain(TICKS_PER_SEC, seg) - 0.0).abs() < 1e-4);
        assert!((segment_fade_gain(TICKS_PER_SEC + TICKS_PER_SEC / 4, seg) - 0.5).abs() < 1e-4);
        assert!((segment_fade_gain(TICKS_PER_SEC * 3, seg) - 1.0).abs() < 1e-4);
        assert!((segment_fade_gain(TICKS_PER_SEC * 6 - TICKS_PER_SEC / 4, seg) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn unlinked_video_covered_by_audio_track_is_not_double_mixed_in_preview() {
        let mut p = Project::new("unlinked", ProjectSettings::default());
        let vid = Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Video,
            name: "talking".into(),
            path: PathBuf::from("/tmp/talking.mp4"),
            probe: Some(MediaProbe {
                duration_ticks: TICKS_PER_SEC * 10,
                width: 1920,
                height: 1080,
                fps: None,
                has_audio: true,
                rotation_deg: 0,
                video_codec: Some("hevc".into()),
                audio_codec: Some("aac".into()),
                sample_rate: Some(48_000),
            }),
            generated: None,
        };
        let vid_id = vid.id;
        p.assets.push(vid);

        // Video on V1 and unlinked audio of same asset on A1 at overlapping range.
        p.clips.push(Clip {
            id: Uuid::new_v4(),
            asset_id: vid_id,
            track_id: p.tracks[0].id,
            timeline_start: 0,
            source_in: 0,
            source_out: TICKS_PER_SEC * 10,
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: Default::default(),
            motion: None,
            color: Default::default(),
        });
        p.clips.push(Clip {
            id: Uuid::new_v4(),
            asset_id: vid_id,
            track_id: p.tracks[1].id,
            timeline_start: 0,
            source_in: 0,
            source_out: TICKS_PER_SEC * 10,
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 1.5,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: Default::default(),
            motion: None,
            color: Default::default(),
        });

        let tracks = build_audio_tracks(&p, &HashMap::new(), false, 1.0);
        // Only the A1 audio clip should play — V1 must NOT double-mix it!
        assert_eq!(tracks.len(), 1);
        assert!((tracks[0][0].volume - 1.5).abs() < 1e-4);
    }

    #[test]
    fn overlay_schedules_follow_back_to_front_track_order() {
        let mut p = Project::new("layers", ProjectSettings::default());
        let v2 = Track {
            id: Uuid::new_v4(),
            kind: TrackKind::Video,
            name: "V2".into(),
            muted: false,
            hidden: false,
            locked: false,
            volume: 1.0,
        };
        let v3 = Track {
            id: Uuid::new_v4(),
            kind: TrackKind::Video,
            name: "V3".into(),
            muted: false,
            hidden: false,
            locked: false,
            volume: 1.0,
        };
        let v2_id = v2.id;
        let v3_id = v3.id;
        p.tracks.extend([v2, v3]);

        let a_back = Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Video,
            name: "hf-back".into(),
            path: PathBuf::from("/tmp/hf-back.mov"),
            probe: None,
            generated: None,
        };
        let a_front = Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Video,
            name: "hf-front".into(),
            path: PathBuf::from("/tmp/hf-front.mov"),
            probe: None,
            generated: None,
        };
        let id_back = a_back.id;
        let id_front = a_front.id;
        p.assets.extend([a_back, a_front]);

        for (aid, tid) in [(id_back, v2_id), (id_front, v3_id)] {
            p.clips.push(Clip {
                id: Uuid::new_v4(),
                asset_id: aid,
                track_id: tid,
                timeline_start: TICKS_PER_SEC * 2,
                source_in: 0,
                source_out: TICKS_PER_SEC * 3,
                link_group: None,
                opacity: 1.0,
                scale: 1.0,
                x_offset: 0.0,
                y_offset: 0.0,
                volume: 1.0,
                fade_in_ticks: 0,
                fade_out_ticks: 0,
                speed: 1.0,
                rotation_deg: 0.0,
                flip_h: false,
                flip_v: false,
                crop: Default::default(),
                motion: None,
                color: Default::default(),
            });
        }

        let schedules = build_overlay_schedules(&p, 960, 540);
        assert_eq!(schedules.len(), 2);
        assert_eq!(schedules[0][0].path, PathBuf::from("/tmp/hf-back.mov"));
        assert_eq!(schedules[1][0].path, PathBuf::from("/tmp/hf-front.mov"));
    }
}
