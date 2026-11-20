//! Asynchronous media cache for smooth editing:
//! - GPU-accelerated short-GOP edit proxies (`cache/proxies/<asset_id>.mp4`)
//! - High-resolution audio waveform peaks + RMS (`cache/waveforms/<asset_id>.wf2`)
//! - Media bin thumbnails (RGBA decoded once off-thread)
//! - Two-tier timeline video filmstrips:
//!   - Tier A: disk-cached overview sprite atlas per video asset (`cache/filmstrips/<asset_id>.fs1`)
//!   - Tier B: on-demand quantized viewport tile LRU for crisp zoomed-in scrubbing
//!
//! All heavy work runs off the UI thread and never acquires the Engine lock.

use eframe::egui;
use cutpilot_core::{Asset, AssetKind, Project};
use cutpilot_media::frames::{self, RgbaFrame};
use cutpilot_media::proxy;
use cutpilot_media::waveform::{self, Peak};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use uuid::Uuid;

const FILMSTRIP_TILE_H: u32 = 48;
const FILMSTRIP_ATLAS_COLS: usize = 8;
const MAX_ONDEMAND_TILES: usize = 192;
const FS_MAGIC: &[u8; 8] = b"CPFS1\0\0\0";

#[derive(Debug, Clone, PartialEq)]
pub enum ProxyState {
    /// Original file is already lightweight or not a video.
    Direct,
    /// Background proxy transcode in progress (0.0..=1.0).
    Building(f64),
    /// Proxy ready on disk at `PathBuf`.
    Ready(PathBuf),
    /// Proxy build failed; falling back to source.
    Failed,
}

/// Request from timeline painting for a higher-detail zoomed filmstrip frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FilmstripTileRequest {
    pub asset_id: Uuid,
    /// Quantized source timestamp in milliseconds.
    pub bucket_ms: i64,
    pub width: u32,
    pub height: u32,
}

/// CPU-side overview filmstrip frames ready to pack into a single GPU atlas.
struct OverviewStripData {
    tile_w: u32,
    tile_h: u32,
    frames: Vec<(f64, Vec<u8>)>,
}

/// GPU-side sprite atlas of overview frames for one video asset.
pub struct FilmstripAtlas {
    texture: egui::TextureHandle,
    cols: usize,
    rows: usize,
    times_secs: Vec<f64>,
    avg_spacing_secs: f64,
}

impl FilmstripAtlas {
    fn nearest(&self, time_secs: f64) -> Option<(egui::TextureId, egui::Rect, f64)> {
        if self.times_secs.is_empty() {
            return None;
        }
        let idx = match self
            .times_secs
            .binary_search_by(|t| t.total_cmp(&time_secs))
        {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) if i >= self.times_secs.len() => self.times_secs.len() - 1,
            Err(i) => {
                let d_prev = (time_secs - self.times_secs[i - 1]).abs();
                let d_next = (self.times_secs[i] - time_secs).abs();
                if d_prev <= d_next {
                    i - 1
                } else {
                    i
                }
            }
        };
        let col = idx % self.cols;
        let row = idx / self.cols;
        let u0 = col as f32 / self.cols as f32;
        let u1 = (col + 1) as f32 / self.cols as f32;
        let v0 = row as f32 / self.rows as f32;
        let v1 = (row + 1) as f32 / self.rows as f32;
        let uv = egui::Rect::from_min_max(egui::pos2(u0, v0), egui::pos2(u1, v1));
        let err = (self.times_secs[idx] - time_secs).abs();
        Some((self.texture.id(), uv, err))
    }
}

struct CachedTile {
    texture: egui::TextureHandle,
    last_used: u64,
}

/// Read-only snapshot of available filmstrip textures passed to `draw_timeline`.
pub struct FilmstripView<'a> {
    atlases: &'a HashMap<Uuid, FilmstripAtlas>,
    tiles: &'a HashMap<(Uuid, i64), CachedTile>,
    thumbs: &'a HashMap<Uuid, egui::TextureHandle>,
}

impl<'a> FilmstripView<'a> {
    /// Look up the best available texture + UV rect for `(asset_id, time_secs)`.
    /// If the overview atlas is too coarse for the current zoom level and no
    /// on-demand tile exists yet, pushes a [`FilmstripTileRequest`] into
    /// `requests` while still returning the best fallback immediately.
    #[allow(clippy::too_many_arguments)]
    pub fn lookup_or_request(
        &self,
        asset_id: Uuid,
        is_image: bool,
        time_secs: f64,
        tile_dur_secs: f64,
        tile_w: u32,
        tile_h: u32,
        requests: &mut Vec<FilmstripTileRequest>,
    ) -> Option<(egui::TextureId, egui::Rect)> {
        let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        if is_image {
            return self.thumbs.get(&asset_id).map(|t| (t.id(), full_uv));
        }

        // Quantize requested timestamp onto a stable power-of-2 grid so
        // scrolling/panning reuses the exact same cached tile keys.
        let grid_secs = quantize_step_secs(tile_dur_secs);
        let bucket_ms = ((time_secs / grid_secs).floor() * grid_secs * 1000.0).round() as i64;
        let bucket_ms = bucket_ms.max(0);

        if let Some(tile) = self.tiles.get(&(asset_id, bucket_ms)) {
            return Some((tile.texture.id(), full_uv));
        }

        // Check overview atlas.
        let atlas_hit = self.atlases.get(&asset_id).and_then(|a| {
            let hit = a.nearest(time_secs)?;
            Some((hit, a.avg_spacing_secs))
        });

        let need_detail = match &atlas_hit {
            Some(((_, _, err), spacing)) => {
                *spacing > tile_dur_secs * 1.4 && *err > tile_dur_secs * 0.45
            }
            None => true,
        };
        if need_detail && requests.len() < 32 {
            requests.push(FilmstripTileRequest {
                asset_id,
                bucket_ms,
                width: tile_w.clamp(28, 96) & !1,
                height: tile_h.clamp(24, 64) & !1,
            });
        }

        if let Some(((tex_id, uv, _), _)) = atlas_hit {
            return Some((tex_id, uv));
        }
        self.thumbs.get(&asset_id).map(|t| (t.id(), full_uv))
    }
}

fn quantize_step_secs(tile_dur_secs: f64) -> f64 {
    let target = (tile_dur_secs * 0.85).max(0.25);
    let mut step = 0.25_f64;
    while step * 2.0 <= target && step < 1024.0 {
        step *= 2.0;
    }
    step
}

struct TileWorkerQueue {
    pending: VecDeque<(FilmstripTileRequest, PathBuf)>,
    ctx: Option<egui::Context>,
    shutdown: bool,
}

enum CacheMsg {
    ProxyProgress(Uuid, f64),
    ProxyReady(Uuid, PathBuf),
    ProxyFailed(Uuid),
    WaveformReady(Uuid, Vec<Peak>),
    ThumbnailReady(Uuid, RgbaFrame),
    OverviewStripReady(Uuid, OverviewStripData),
    TileReady(Uuid, i64, RgbaFrame),
    TileFailed(Uuid, i64),
}

pub struct MediaCache {
    project_dir: Option<PathBuf>,
    seen_assets: HashSet<Uuid>,
    proxies: HashMap<Uuid, ProxyState>,
    waveforms: HashMap<Uuid, Vec<Peak>>,
    thumb_frames: HashMap<Uuid, RgbaFrame>,
    thumb_textures: HashMap<Uuid, egui::TextureHandle>,
    overview_pending: HashMap<Uuid, OverviewStripData>,
    overview_atlases: HashMap<Uuid, FilmstripAtlas>,
    tile_frames: HashMap<(Uuid, i64), RgbaFrame>,
    tile_textures: HashMap<(Uuid, i64), CachedTile>,
    tile_inflight: HashSet<(Uuid, i64)>,
    tile_clock: u64,
    tile_queue: Arc<(Mutex<TileWorkerQueue>, Condvar)>,
    tx: mpsc::Sender<CacheMsg>,
    rx: mpsc::Receiver<CacheMsg>,
    cancel: Arc<AtomicBool>,
    /// Limit concurrent proxy transcodes so we don't starve preview decoding.
    proxy_lock: Arc<Mutex<()>>,
    /// Serialize overview filmstrip extraction so imports stay snappy.
    strip_lock: Arc<Mutex<()>>,
}

impl MediaCache {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let tile_queue = Arc::new((
            Mutex::new(TileWorkerQueue {
                pending: VecDeque::new(),
                ctx: None,
                shutdown: false,
            }),
            Condvar::new(),
        ));
        spawn_tile_worker(tile_queue.clone(), tx.clone());
        Self {
            project_dir: None,
            seen_assets: HashSet::new(),
            proxies: HashMap::new(),
            waveforms: HashMap::new(),
            thumb_frames: HashMap::new(),
            thumb_textures: HashMap::new(),
            overview_pending: HashMap::new(),
            overview_atlases: HashMap::new(),
            tile_frames: HashMap::new(),
            tile_textures: HashMap::new(),
            tile_inflight: HashSet::new(),
            tile_clock: 0,
            tile_queue,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            proxy_lock: Arc::new(Mutex::new(())),
            strip_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Reset cache when switching to a different project.
    pub fn reset(&mut self, new_project_dir: Option<PathBuf>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.project_dir = new_project_dir;
        self.seen_assets.clear();
        self.proxies.clear();
        self.waveforms.clear();
        self.thumb_frames.clear();
        self.thumb_textures.clear();
        self.overview_pending.clear();
        self.overview_atlases.clear();
        self.tile_frames.clear();
        self.tile_textures.clear();
        self.tile_inflight.clear();
        if let Ok(mut q) = self.tile_queue.0.lock() {
            q.pending.clear();
        }
    }

    /// Inspect the current project and kick off background proxy/waveform/thumb/filmstrip
    /// jobs for any newly imported assets.
    pub fn sync_project(&mut self, project: &Project, project_dir: Option<&Path>, ctx: &egui::Context) {
        let dir_buf = project_dir.map(Path::to_path_buf);
        if self.project_dir != dir_buf {
            self.reset(dir_buf);
        }
        let Some(pdir) = self.project_dir.clone() else {
            return;
        };

        for asset in &project.assets {
            if !self.seen_assets.insert(asset.id) {
                continue;
            }
            if !asset.path.is_file() {
                continue;
            }
            self.enqueue_asset(asset, &pdir, ctx);
        }
    }

    fn enqueue_asset(&mut self, asset: &Asset, project_dir: &Path, ctx: &egui::Context) {
        let id = asset.id;
        let path = asset.path.clone();
        let dur_secs = cutpilot_core::secs_from_ticks(asset.editable_duration());

        // 1. Proxy (for high-res or long-GOP videos).
        if proxy::needs_proxy(asset) {
            let dest = proxy::proxy_path(project_dir, id);
            if dest.is_file() {
                self.proxies.insert(id, ProxyState::Ready(dest));
            } else {
                self.proxies.insert(id, ProxyState::Building(0.0));
                let tx = self.tx.clone();
                let cancel = self.cancel.clone();
                let proxy_lock = self.proxy_lock.clone();
                let codec = asset.probe.as_ref().and_then(|p| p.video_codec.clone());
                let src = path.clone();
                let repaint_ctx = ctx.clone();
                std::thread::spawn(move || {
                    let _guard = proxy_lock.lock().ok();
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let tx_prog = tx.clone();
                    let ctx_prog = repaint_ctx.clone();
                    let progress: Arc<dyn Fn(f64) + Send + Sync> = Arc::new(move |p| {
                        let _ = tx_prog.send(CacheMsg::ProxyProgress(id, p));
                        ctx_prog.request_repaint();
                    });
                    match proxy::generate_proxy(
                        &src,
                        &dest,
                        dur_secs,
                        codec.as_deref(),
                        Some(progress),
                        cancel,
                    ) {
                        Ok(out) => {
                            let _ = tx.send(CacheMsg::ProxyReady(id, out));
                        }
                        Err(_) => {
                            let _ = tx.send(CacheMsg::ProxyFailed(id));
                        }
                    }
                    repaint_ctx.request_repaint();
                });
            }
        } else {
            self.proxies.insert(id, ProxyState::Direct);
        }

        // 2. High-resolution waveform peaks + RMS (binary cache for instant reopen).
        let has_audio = asset.kind == AssetKind::Audio
            || asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false);
        if has_audio {
            let wf_path = project_dir
                .join("cache")
                .join("waveforms")
                .join(format!("{id}.wf2"));
            let buckets = waveform::recommended_buckets(dur_secs);
            let tx = self.tx.clone();
            let src = path.clone();
            let cancel = self.cancel.clone();
            let repaint_ctx = ctx.clone();
            std::thread::spawn(move || {
                if let Some(peaks) = waveform::load_peaks_bin(&wf_path) {
                    let _ = tx.send(CacheMsg::WaveformReady(id, peaks));
                    repaint_ctx.request_repaint();
                    return;
                }
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(peaks) = waveform::waveform_peaks(&src, buckets) {
                    let _ = waveform::save_peaks_bin(&wf_path, &peaks);
                    let _ = tx.send(CacheMsg::WaveformReady(id, peaks));
                    repaint_ctx.request_repaint();
                }
            });
        }

        // 3. Asset thumbnail (128x72 RGBA for Media Bin).
        if matches!(asset.kind, AssetKind::Video | AssetKind::Image) {
            let tx = self.tx.clone();
            let src = path.clone();
            let seek = if asset.kind == AssetKind::Image {
                0.0
            } else {
                (dur_secs * 0.1).min(2.0)
            };
            let repaint_ctx = ctx.clone();
            std::thread::spawn(move || {
                if let Ok(frame) = frames::extract_frame_fast(&src, seek, 128, 72, true, true, true)
                {
                    let _ = tx.send(CacheMsg::ThumbnailReady(id, frame));
                    repaint_ctx.request_repaint();
                }
            });
        }

        // 4. Overview timeline filmstrip atlas for video assets (progressive + disk-cached).
        if asset.kind == AssetKind::Video && dur_secs > 0.05 {
            let aspect = asset
                .probe
                .as_ref()
                .map(|p| {
                    let (w, h) = p.oriented_size();
                    if h > 0 {
                        w as f32 / h as f32
                    } else {
                        16.0 / 9.0
                    }
                })
                .unwrap_or(16.0 / 9.0)
                .clamp(0.55, 2.2);
            let tile_h = FILMSTRIP_TILE_H;
            let tile_w = (((tile_h as f32 * aspect).round() as u32).clamp(28, 96)) & !1;
            let count = ((dur_secs / 1.5).ceil() as usize).clamp(8, 48);
            let fs_path = project_dir
                .join("cache")
                .join("filmstrips")
                .join(format!("{id}.fs1"));
            let proxy_file = proxy::proxy_path(project_dir, id);
            let tx = self.tx.clone();
            let src = path;
            let cancel = self.cancel.clone();
            let strip_lock = self.strip_lock.clone();
            let repaint_ctx = ctx.clone();

            std::thread::spawn(move || {
                if let Some(cached) = load_filmstrip_bin(&fs_path) {
                    let _ = tx.send(CacheMsg::OverviewStripReady(id, cached));
                    repaint_ctx.request_repaint();
                    return;
                }
                let _guard = strip_lock.lock().ok();
                if cancel.load(Ordering::Relaxed) {
                    return;
                }

                // Build evenly spaced sample times across the asset.
                let times: Vec<f64> = (0..count)
                    .map(|i| {
                        let frac = (i as f64 + 0.5) / count as f64;
                        (frac * dur_secs).clamp(0.0, (dur_secs - 0.05).max(0.0))
                    })
                    .collect();

                // Progressive multi-pass extraction: first decode ~8 coarse
                // frames so the timeline populates almost instantly, then fill
                // in the remaining frames.
                let mut slots: Vec<Option<Vec<u8>>> = vec![None; count];
                let stride = (count / 8).max(1);
                let mut order: Vec<usize> = (0..count).step_by(stride).collect();
                for i in 0..count {
                    if i % stride != 0 {
                        order.push(i);
                    }
                }
                let first_pass_len = (0..count).step_by(stride).count();

                for (done_idx, &slot_idx) in order.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let decode_path = if proxy_file.is_file() {
                        &proxy_file
                    } else {
                        &src
                    };
                    if let Ok(frame) =
                        frames::extract_filmstrip_frame(decode_path, times[slot_idx], tile_w, tile_h)
                    {
                        slots[slot_idx] = Some(frame.bytes);
                    }
                    // Emit early preview once first coarse pass finishes.
                    if done_idx + 1 == first_pass_len && first_pass_len < count {
                        let partial: Vec<(f64, Vec<u8>)> = times
                            .iter()
                            .zip(slots.iter())
                            .filter_map(|(&t, b)| b.as_ref().map(|bytes| (t, bytes.clone())))
                            .collect();
                        if !partial.is_empty() {
                            let _ = tx.send(CacheMsg::OverviewStripReady(
                                id,
                                OverviewStripData {
                                    tile_w,
                                    tile_h,
                                    frames: partial,
                                },
                            ));
                            repaint_ctx.request_repaint();
                        }
                    }
                }

                let complete: Vec<(f64, Vec<u8>)> = times
                    .into_iter()
                    .zip(slots)
                    .filter_map(|(t, b)| b.map(|bytes| (t, bytes)))
                    .collect();
                if !complete.is_empty() {
                    let strip = OverviewStripData {
                        tile_w,
                        tile_h,
                        frames: complete,
                    };
                    let _ = save_filmstrip_bin(&fs_path, &strip);
                    let _ = tx.send(CacheMsg::OverviewStripReady(id, strip));
                    repaint_ctx.request_repaint();
                }
            });
        }
    }

    /// Enqueue missing zoomed-in filmstrip tiles discovered during timeline painting.
    pub fn request_filmstrip_tiles(
        &mut self,
        project: &Project,
        requests: Vec<FilmstripTileRequest>,
        ctx: &egui::Context,
    ) {
        if requests.is_empty() {
            return;
        }
        let mut to_push = Vec::new();
        for req in requests {
            let key = (req.asset_id, req.bucket_ms);
            if self.tile_textures.contains_key(&key) || !self.tile_inflight.insert(key) {
                continue;
            }
            // Prefer ready proxy for blazing-fast random seeks; fall back to source.
            let src = match self.proxies.get(&req.asset_id) {
                Some(ProxyState::Ready(p)) if p.is_file() => p.clone(),
                _ => match project.asset(&req.asset_id) {
                    Some(a) if a.path.is_file() => a.path.clone(),
                    _ => {
                        self.tile_inflight.remove(&key);
                        continue;
                    }
                },
            };
            to_push.push((req, src));
        }
        if to_push.is_empty() {
            return;
        }
        let (lock, cv) = &*self.tile_queue;
        if let Ok(mut q) = lock.lock() {
            q.ctx = Some(ctx.clone());
            // Keep queue bounded to latest viewport requests so rapid zooming stays crisp.
            while q.pending.len() + to_push.len() > 48 {
                if let Some((dropped, _)) = q.pending.pop_front() {
                    self.tile_inflight.remove(&(dropped.asset_id, dropped.bucket_ms));
                } else {
                    break;
                }
            }
            for item in to_push {
                q.pending.push_back(item);
            }
            cv.notify_one();
        }
    }

    /// Drain finished cache results and upload any ready textures to the GPU.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                CacheMsg::ProxyProgress(id, p) => {
                    self.proxies.insert(id, ProxyState::Building(p));
                }
                CacheMsg::ProxyReady(id, path) => {
                    self.proxies.insert(id, ProxyState::Ready(path));
                }
                CacheMsg::ProxyFailed(id) => {
                    self.proxies.insert(id, ProxyState::Failed);
                }
                CacheMsg::WaveformReady(id, peaks) => {
                    self.waveforms.insert(id, peaks);
                }
                CacheMsg::ThumbnailReady(id, frame) => {
                    self.thumb_frames.insert(id, frame);
                }
                CacheMsg::OverviewStripReady(id, strip) => {
                    self.overview_pending.insert(id, strip);
                }
                CacheMsg::TileReady(id, bucket_ms, frame) => {
                    self.tile_inflight.remove(&(id, bucket_ms));
                    self.tile_frames.insert((id, bucket_ms), frame);
                }
                CacheMsg::TileFailed(id, bucket_ms) => {
                    self.tile_inflight.remove(&(id, bucket_ms));
                }
            }
        }

        // Upload bin thumbnails.
        let pending_thumbs: Vec<Uuid> = self.thumb_frames.keys().copied().collect();
        for id in pending_thumbs {
            if let Some(frame) = self.thumb_frames.remove(&id) {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.bytes,
                );
                let handle = ctx.load_texture(
                    format!("asset-thumb-{id}"),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.thumb_textures.insert(id, handle);
            }
        }

        // Pack & upload overview filmstrip atlases.
        let pending_strips: Vec<Uuid> = self.overview_pending.keys().copied().collect();
        for id in pending_strips {
            if let Some(strip) = self.overview_pending.remove(&id) {
                if let Some(atlas) = build_filmstrip_atlas(ctx, id, strip) {
                    self.overview_atlases.insert(id, atlas);
                }
            }
        }

        // Upload on-demand zoomed tiles with LRU eviction.
        let pending_tiles: Vec<(Uuid, i64)> = self.tile_frames.keys().copied().collect();
        for key in pending_tiles {
            if let Some(frame) = self.tile_frames.remove(&key) {
                self.tile_clock = self.tile_clock.wrapping_add(1);
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.bytes,
                );
                let handle = ctx.load_texture(
                    format!("fs-tile-{}-{}", key.0, key.1),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.tile_textures.insert(
                    key,
                    CachedTile {
                        texture: handle,
                        last_used: self.tile_clock,
                    },
                );
            }
        }
        while self.tile_textures.len() > MAX_ONDEMAND_TILES {
            if let Some(oldest_key) = self
                .tile_textures
                .iter()
                .min_by_key(|(_, v)| v.last_used)
                .map(|(k, _)| *k)
            {
                self.tile_textures.remove(&oldest_key);
            } else {
                break;
            }
        }

        changed
    }

    pub fn filmstrip_view(&self) -> FilmstripView<'_> {
        FilmstripView {
            atlases: &self.overview_atlases,
            tiles: &self.tile_textures,
            thumbs: &self.thumb_textures,
        }
    }

    /// Map of asset_id -> proxy path for all ready proxies (passed to playback worker).
    pub fn ready_proxy_map(&self) -> HashMap<Uuid, PathBuf> {
        self.proxies
            .iter()
            .filter_map(|(id, st)| match st {
                ProxyState::Ready(p) if p.is_file() => Some((*id, p.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn proxy_state(&self, asset_id: &Uuid) -> Option<&ProxyState> {
        self.proxies.get(asset_id)
    }

    pub fn any_proxy_building(&self) -> Option<f64> {
        let mut total = 0.0;
        let mut count = 0usize;
        for st in self.proxies.values() {
            if let ProxyState::Building(p) = st {
                total += *p;
                count += 1;
            }
        }
        (count > 0).then(|| total / count as f64)
    }

    pub fn waveforms(&self) -> &HashMap<Uuid, Vec<Peak>> {
        &self.waveforms
    }

    pub fn thumbnail(&self, asset_id: &Uuid) -> Option<&egui::TextureHandle> {
        self.thumb_textures.get(asset_id)
    }
}

impl Drop for MediaCache {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let (lock, cv) = &*self.tile_queue;
        if let Ok(mut q) = lock.lock() {
            q.shutdown = true;
            q.pending.clear();
            cv.notify_all();
        }
    }
}

fn spawn_tile_worker(queue: Arc<(Mutex<TileWorkerQueue>, Condvar)>, tx: mpsc::Sender<CacheMsg>) {
    std::thread::spawn(move || loop {
        let (req, src, ctx) = {
            let (lock, cv) = &*queue;
            let mut q = match lock.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            while q.pending.is_empty() && !q.shutdown {
                q = match cv.wait(q) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
            if q.shutdown {
                return;
            }
            let Some((req, src)) = q.pending.pop_front() else {
                continue;
            };
            (req, src, q.ctx.clone())
        };

        let secs = req.bucket_ms as f64 / 1000.0;
        match frames::extract_filmstrip_frame(&src, secs, req.width, req.height) {
            Ok(frame) => {
                let _ = tx.send(CacheMsg::TileReady(req.asset_id, req.bucket_ms, frame));
            }
            Err(_) => {
                let _ = tx.send(CacheMsg::TileFailed(req.asset_id, req.bucket_ms));
            }
        }
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    });
}

fn build_filmstrip_atlas(
    ctx: &egui::Context,
    asset_id: Uuid,
    strip: OverviewStripData,
) -> Option<FilmstripAtlas> {
    let count = strip.frames.len();
    if count == 0 || strip.tile_w == 0 || strip.tile_h == 0 {
        return None;
    }
    let cols = count.min(FILMSTRIP_ATLAS_COLS);
    let rows = count.div_ceil(cols);
    let tw = strip.tile_w as usize;
    let th = strip.tile_h as usize;
    let atlas_w = cols * tw;
    let atlas_h = rows * th;
    let mut rgba = vec![0u8; atlas_w * atlas_h * 4];
    let mut times_secs = Vec::with_capacity(count);

    for (idx, (t, frame_bytes)) in strip.frames.into_iter().enumerate() {
        times_secs.push(t);
        if frame_bytes.len() != tw * th * 4 {
            continue;
        }
        let col = idx % cols;
        let row = idx / cols;
        let dst_x = col * tw;
        let dst_y = row * th;
        for y in 0..th {
            let src_off = y * tw * 4;
            let dst_off = ((dst_y + y) * atlas_w + dst_x) * 4;
            rgba[dst_off..dst_off + tw * 4].copy_from_slice(&frame_bytes[src_off..src_off + tw * 4]);
        }
    }

    let avg_spacing_secs = if times_secs.len() >= 2 {
        (times_secs.last().unwrap() - times_secs[0]) / (times_secs.len() - 1) as f64
    } else {
        1.0
    };

    let image = egui::ColorImage::from_rgba_unmultiplied([atlas_w, atlas_h], &rgba);
    let texture = ctx.load_texture(
        format!("asset-filmstrip-{asset_id}"),
        image,
        egui::TextureOptions::LINEAR,
    );
    Some(FilmstripAtlas {
        texture,
        cols,
        rows,
        times_secs,
        avg_spacing_secs: avg_spacing_secs.max(0.05),
    })
}

fn save_filmstrip_bin(path: &Path, strip: &OverviewStripData) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let frame_len = strip.tile_w as usize * strip.tile_h as usize * 4;
    let mut buf = Vec::with_capacity(20 + strip.frames.len() * (8 + frame_len));
    buf.extend_from_slice(FS_MAGIC);
    buf.extend_from_slice(&strip.tile_w.to_le_bytes());
    buf.extend_from_slice(&strip.tile_h.to_le_bytes());
    buf.extend_from_slice(&(strip.frames.len() as u32).to_le_bytes());
    for (t, bytes) in &strip.frames {
        if bytes.len() != frame_len {
            continue;
        }
        buf.extend_from_slice(&t.to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    let tmp = path.with_extension("fs1.tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn load_filmstrip_bin(path: &Path) -> Option<OverviewStripData> {
    const MAX_TILE_DIM: u32 = 256;
    const MAX_FILMSTRIP_FRAMES: usize = 256;
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 20 || &bytes[0..8] != FS_MAGIC {
        return None;
    }
    let tile_w = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let tile_h = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let count = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    if tile_w == 0
        || tile_h == 0
        || tile_w > MAX_TILE_DIM
        || tile_h > MAX_TILE_DIM
        || count == 0
        || count > MAX_FILMSTRIP_FRAMES
    {
        return None;
    }
    let frame_len = (tile_w as usize)
        .checked_mul(tile_h as usize)?
        .checked_mul(4)?;
    let entry_len = frame_len.checked_add(8)?;
    let expected = count.checked_mul(entry_len)?.checked_add(20)?;
    if bytes.len() != expected {
        return None;
    }
    let mut frames = Vec::with_capacity(count);
    let mut off = 20;
    for _ in 0..count {
        let t = f64::from_le_bytes([
            bytes[off],
            bytes[off + 1],
            bytes[off + 2],
            bytes[off + 3],
            bytes[off + 4],
            bytes[off + 5],
            bytes[off + 6],
            bytes[off + 7],
        ]);
        if !t.is_finite() || t < 0.0 {
            return None;
        }
        off += 8;
        let data = bytes[off..off + frame_len].to_vec();
        off += frame_len;
        frames.push((t, data));
    }
    Some(OverviewStripData {
        tile_w,
        tile_h,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_filmstrip_bin_rejects_overflow_and_corrupt_headers() {
        let tmp = std::env::temp_dir().join(format!("cutpilot-fs-sec-{}.fs1", Uuid::new_v4()));
        let mut raw = Vec::new();
        raw.extend_from_slice(FS_MAGIC);
        raw.extend_from_slice(&u32::MAX.to_le_bytes());
        raw.extend_from_slice(&u32::MAX.to_le_bytes());
        raw.extend_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&tmp, &raw).unwrap();
        assert!(load_filmstrip_bin(&tmp).is_none());
        let _ = std::fs::remove_file(&tmp);
    }
}
