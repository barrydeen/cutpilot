//! The OpenPrem native editor app.

use crate::filepicker::{self, PickerOutcome};
use crate::timeline;
use eframe::egui;
use openprem_core::{
    AssetKind, Command, Selection, TrackKind, TICKS_PER_SEC,
};
use openprem_engine::Engine;
use openprem_media::frames::{self, RgbaFrame};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const PREVIEW_WIDTH: u32 = 960;
const PREVIEW_CACHE_LIMIT: usize = 48;

/// One overlay/base layer request for the decode worker.
#[derive(Clone)]
struct LayerSpec {
    path: PathBuf,
    seek_secs: f64,
    fit: bool,
    scale_w: u32,
    scale_h: u32,
    paste_x: i64,
    paste_y: i64,
    opacity: f32,
}

struct PreviewRequest {
    key: u64,
    canvas_w: u32,
    canvas_h: u32,
    layers: Vec<LayerSpec>,
}

enum PreviewMsg {
    Frame(u64, RgbaFrame),
}

/// Shared status line: async threads report into it with a sequence number,
/// and the UI adopts newer text every frame. Fixes the old behavior where
/// background failures vanished into stderr.
#[derive(Default)]
struct StatusLine {
    seq: u64,
    text: String,
}

fn report_to(cell: &Arc<Mutex<StatusLine>>, text: impl Into<String>) {
    if let Ok(mut line) = cell.lock() {
        line.seq = line.seq.wrapping_add(1);
        line.text = text.into();
    }
}

pub struct OpenPremApp {
    engine: Arc<Mutex<Engine>>,
    /// Requests to the decode worker.
    preview_tx: std::sync::mpsc::Sender<PreviewRequest>,
    /// Completed frames from the worker.
    preview_rx: std::sync::mpsc::Receiver<PreviewMsg>,
    worker_key: Arc<AtomicU64>,
    /// key -> decoded frame (LRU).
    frame_cache: HashMap<u64, (RgbaFrame, Instant)>,
    cache_order: VecDeque<u64>,
    /// key -> uploaded texture.
    textures: HashMap<u64, egui::TextureHandle>,
    drag: timeline::DragState,
    /// Displayed status (adopted from `status_cell` each frame).
    status: String,
    status_cell: Arc<Mutex<StatusLine>>,
    status_seq: u64,
    path_field: String,
    /// Manual media path fallback field in the Media panel.
    media_path_field: String,
    /// Active file-picker worker, if any.
    picker_rx: Option<std::sync::mpsc::Receiver<PickerOutcome>>,
    /// Number of entries seen in the last frame's dropped-file list, so
    /// re-processing only happens when the list actually grows.
    drop_list_len: usize,
    export_bitrate: f64,
    use_nvenc: bool,
}

impl OpenPremApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, worker_rx) = std::sync::mpsc::channel::<PreviewRequest>();
        let (result_tx, preview_rx) = std::sync::mpsc::channel::<PreviewMsg>();
        let worker_key = Arc::new(AtomicU64::new(0));
        let key_clone = worker_key.clone();
        std::thread::spawn(move || {
            while let Ok(req) = worker_rx.recv() {
                let frame = decode_layers(&req);
                key_clone.store(req.key, Ordering::Relaxed);
                if result_tx.send(PreviewMsg::Frame(req.key, frame)).is_err() {
                    break;
                }
            }
        });
        let initial_status = "Press “Import media…” or drop a video — with no \
                             project open, OpenPrem creates one beside your media."
            .to_string();
        Self {
            engine: Arc::new(Mutex::new(Engine::new())),
            preview_tx: tx,
            preview_rx,
            worker_key,
            frame_cache: HashMap::new(),
            cache_order: VecDeque::new(),
            textures: HashMap::new(),
            drag: timeline::DragState::default(),
            status: initial_status.clone(),
            status_cell: Arc::new(Mutex::new(StatusLine {
                seq: 1,
                text: initial_status,
            })),
            status_seq: 1,
            path_field: String::new(),
            media_path_field: String::new(),
            picker_rx: None,
            drop_list_len: 0,
            export_bitrate: 0.0, // 0 = auto
            use_nvenc: true,
        }
    }

    // ------------------------------------------------------------------
    // engine plumbing
    // ------------------------------------------------------------------

    /// Report a status line for the user (visible immediately).
    fn report(&self, text: impl Into<String>) {
        report_to(&self.status_cell, text);
    }

    fn exec(&mut self, cmd: Command) {
        let mut engine = self.engine.lock().expect("engine lock");
        match engine.execute(cmd) {
            Ok(outcome) => {
                self.report(outcome.message);
            }
            Err(e) => {
                self.report(format!("error: {e}"));
            }
        }
    }

    /// Import media files off-thread. When no project is open, one is
    /// created automatically beside the first file (canvas matched to the
    /// footage, rotation-aware) so a fresh drop can never be a no-op.
    fn import_files(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let engine = self.engine.clone();
        let status = self.status_cell.clone();
        std::thread::spawn(move || {
            let needs_project = engine
                .lock()
                .map(|g| !g.is_open())
                .unwrap_or(false);
            if needs_project {
                if let Err(msg) = auto_create_project_for(&engine, &status, &paths) {
                    report_to(&status, msg);
                    return;
                }
            }
            let Ok(mut guard) = engine.lock() else { return };
            for path in paths {
                if !path.is_file() {
                    report_to(
                        &status,
                        format!("skipped (not a file): {}", path.display()),
                    );
                    continue;
                }
                let already = guard
                    .project()
                    .map(|p| p.assets.iter().any(|a| a.path == path))
                    .unwrap_or(false);
                if already {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    report_to(&status, format!("already in the bin: {name}"));
                    continue;
                }
                match guard.execute(Command::ImportMedia {
                    path: path.clone(),
                    auto_place: Some(true),
                }) {
                    Ok(_) => report_to(
                        &status,
                        format!("imported and placed: {}", path.display()),
                    ),
                    Err(e) => report_to(
                        &status,
                        format!("import failed ({}): {e}", path.display()),
                    ),
                }
            }
        });
    }

    /// Open the system file picker on a worker thread; the result comes back
    /// through `picker_rx` and is polled in `ui`.
    fn open_file_picker(&mut self) {
        if self.picker_rx.is_some() {
            return; // a picker is already open
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome =
                filepicker::pick_files_blocking("Import media into OpenPrem");
            let _ = tx.send(outcome);
        });
        self.picker_rx = Some(rx);
        self.report("opening the file picker…");
    }

    fn project_snapshot(&self) -> Option<openprem_core::Project> {
        let engine = self.engine.lock().expect("engine lock");
        engine.project().cloned()
    }

    // ------------------------------------------------------------------
    // preview pipeline
    // ------------------------------------------------------------------

    fn request_preview(&mut self, project: &openprem_core::Project) {
        let ticks = project.editor.playhead_ticks;
        let aspect = project.settings.height as f64 / project.settings.width.max(1) as f64;
        let pw = PREVIEW_WIDTH;
        let ph = ((pw as f64 * aspect).round() as u32).max(2) & !1;

        let mut layers: Vec<LayerSpec> = Vec::new();
        let mut layer_ids: Vec<(u64, i64)> = Vec::new();
        for clip in project.video_layers_at(ticks) {
            let Some(asset) = project.asset(&clip.asset_id) else {
                continue;
            };
            let local = openprem_core::secs_from_ticks(
                clip.source_in + (ticks - clip.timeline_start).max(0),
            );
            let seek = if asset.kind == AssetKind::Image { 0.0 } else { local };
            let is_base = layers.is_empty();
            let (aw, ah) = asset
                .probe
                .as_ref()
                .map(|p| p.oriented_size())
                .unwrap_or((project.settings.width, project.settings.height));
            let sx = pw as f64 / project.settings.width.max(1) as f64;
            let sy = ph as f64 / project.settings.height.max(1) as f64;
            let spec = if is_base {
                LayerSpec {
                    path: asset.path.clone(),
                    seek_secs: seek,
                    fit: true,
                    scale_w: pw,
                    scale_h: ph,
                    paste_x: 0,
                    paste_y: 0,
                    opacity: 1.0,
                }
            } else {
                let ow = ((aw as f64 * clip.scale as f64 * sx).round() as u32).max(2) & !1;
                let oh = ((ah as f64 * clip.scale as f64 * sy).round() as u32).max(2) & !1;
                LayerSpec {
                    path: asset.path.clone(),
                    seek_secs: seek,
                    fit: false,
                    scale_w: ow,
                    scale_h: oh,
                    paste_x: ((pw as f64 - ow as f64) / 2.0 + clip.x_offset as f64 * sx)
                        .round() as i64,
                    paste_y: ((ph as f64 - oh as f64) / 2.0 + clip.y_offset as f64 * sy)
                        .round() as i64,
                    opacity: clip.opacity,
                }
            };
            layer_ids.push((clip.id.as_u64_pair().0, clip.timeline_start));
            layers.push(spec);
        }
        if layers.is_empty() {
            return;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&ticks, &mut hasher);
        std::hash::Hash::hash(&layer_ids, &mut hasher);
        for layer in &layers {
            std::hash::Hash::hash(&layer.path, &mut hasher);
            std::hash::Hash::hash(&layer.seek_secs.to_bits(), &mut hasher);
            std::hash::Hash::hash(&layer.scale_w, &mut hasher);
            std::hash::Hash::hash(&layer.scale_h, &mut hasher);
        }
        use std::hash::Hasher;
        let key = hasher.finish();

        if self.frame_cache.contains_key(&key) {
            return;
        }
        if self.worker_key.load(Ordering::Relaxed) == key {
            return; // already decoding this exact request
        }
        let _ = self.preview_tx.send(PreviewRequest {
            key,
            canvas_w: pw,
            canvas_h: ph,
            layers,
        });
    }

    fn drain_preview_results(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.preview_rx.try_recv() {
            let PreviewMsg::Frame(key, frame) = msg;
            self.frame_cache.insert(key, (frame, Instant::now()));
            self.cache_order.push_back(key);
            while self.cache_order.len() > PREVIEW_CACHE_LIMIT {
                if let Some(oldest) = self.cache_order.pop_front() {
                    self.frame_cache.remove(&oldest);
                    self.textures.remove(&oldest);
                }
            }
            ctx.request_repaint();
        }
    }

    // ------------------------------------------------------------------
    // UI
    // ------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("OpenPrem");
            ui.separator();
            if ui.button("Import media…").clicked() {
                self.open_file_picker();
            }
            ui.separator();
            let path_field = &mut self.path_field;
            ui.add(egui::TextEdit::singleline(path_field).hint_text("project directory").desired_width(240.0));
            if ui.button("New").clicked() {
                let dir = PathBuf::from(self.path_field.trim());
                let name = dir
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "My Project".into());
                if dir.as_os_str().is_empty() {
                    self.report("enter a project directory first");
                } else {
                    self.exec(Command::CreateProject {
                        name,
                        path: dir,
                        width: None,
                        height: None,
                        fps_num: None,
                        fps_den: None,
                    });
                }
            }
            if ui.button("Open").clicked() {
                let dir = PathBuf::from(self.path_field.trim());
                if dir.as_os_str().is_empty() {
                    self.report("enter a project directory first");
                } else {
                    self.exec(Command::OpenProject { path: dir });
                }
            }
            if ui.button("Save").clicked() {
                self.exec(Command::SaveProject { path: None });
            }
            ui.separator();
            if ui.button("Undo").clicked() {
                self.exec(Command::Undo {});
            }
            if ui.button("Redo").clicked() {
                self.exec(Command::Redo {});
            }
            ui.separator();
            ui.label(
                egui::RichText::new(self.status.clone())
                    .small()
                    .color(ui.visuals().text_color()),
            );
        });

        // Keep the path field in sync with an auto-created project so Save
        // and the field agree on where the project lives.
        if self.path_field.trim().is_empty() {
            let dir = self
                .engine
                .lock()
                .expect("engine lock")
                .project_dir()
                .map(std::path::Path::to_path_buf);
            if let Some(dir) = dir {
                self.path_field = dir.display().to_string();
            }
        }

        // Drag-and-drop import (kept alongside the browse button). egui's
        // dropped-file list persists after a drop, so only entries the list
        // actually gained since the last frame are processed — no import
        // spam, and a re-drop of the same file still works.
        let prev_len = self.drop_list_len;
        let (raw_len, new_paths): (usize, Vec<PathBuf>) = ui.input(|i| {
            let len = i.raw.dropped_files.len();
            let start = prev_len.min(len);
            let tail: Vec<PathBuf> = i.raw.dropped_files[start..]
                .iter()
                .filter_map(|d| {
                    let p = d.path();
                    (!p.as_os_str().is_empty()).then(|| p.to_path_buf())
                })
                .collect();
            (len, tail)
        });
        self.drop_list_len = raw_len;
        if raw_len > prev_len && !new_paths.is_empty() {
            let n = new_paths.len();
            self.report(format!("importing {n} dropped file(s)…"));
            self.import_files(new_paths);
        }
    }

    fn media_bin(&mut self, ui: &mut egui::Ui) {
        ui.heading("Media");
        ui.horizontal(|ui| {
            if ui.button("Browse files…").clicked() {
                self.open_file_picker();
            }
            let field = &mut self.media_path_field;
            ui.add(
                egui::TextEdit::singleline(field)
                    .hint_text("or paste a file path")
                    .desired_width(120.0),
            );
            if ui.button("Import").clicked() {
                let text = self.media_path_field.trim().to_string();
                if text.is_empty() {
                    self.report("enter a media file path first");
                } else {
                    let path = PathBuf::from(text);
                    if !path.is_file() {
                        self.report(format!("not a file: {}", path.display()));
                    } else {
                        self.media_path_field.clear();
                        self.report(format!("importing {}", path.display()));
                        self.import_files(vec![path]);
                    }
                }
            }
        });
        let Some(project) = self.project_snapshot() else {
            ui.label(
                egui::RichText::new(
                    "No project open — Browse or drop a video and one is \
                     created automatically beside your media.",
                )
                .small(),
            );
            return;
        };
        if project.assets.is_empty() {
            ui.label(
                egui::RichText::new(
                    "Press Browse files… or drop video onto the window; agents \
                     can import via MCP `media_import`.",
                )
                .small(),
            );
        }
        for asset in &project.assets {
            let selected = project
                .editor
                .selection
                .iter()
                .any(|s| matches!(s, Selection::Asset(id) if id == &asset.id));
            let label = format!(
                "{} [{}] {:.1}s",
                asset.name,
                match asset.kind {
                    AssetKind::Video => "video",
                    AssetKind::Audio => "audio",
                    AssetKind::Image => "image",
                },
                openprem_core::secs_from_ticks(asset.editable_duration()),
            );
            if ui
                .selectable_label(selected, label)
                .clicked()
            {
                self.exec(Command::SetSelection {
                    selection: vec![Selection::Asset(asset.id)],
                });
            }
            ui.horizontal(|ui| {
                if ui.small_button("place").clicked() {
                    // Insert the whole asset at the end of the base track,
                    // with linked audio when available.
                    let base = project
                        .tracks
                        .iter()
                        .find(|t| t.kind == TrackKind::Video)
                        .map(|t| t.id);
                    let audio = project
                        .tracks
                        .iter()
                        .find(|t| t.kind == TrackKind::Audio)
                        .map(|t| t.id);
                    let duration = asset.editable_duration();
                    let end = project
                        .clips
                        .iter()
                        .map(|c| c.timeline_end())
                        .max()
                        .unwrap_or(0);
                    if let Some(v_track) = base {
                        let group = if asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false)
                            && asset.kind == AssetKind::Video
                        {
                            Some(uuid::Uuid::new_v4())
                        } else {
                            None
                        };
                        let mut commands = vec![Command::InsertClip {
                            asset_id: asset.id,
                            track_id: v_track,
                            timeline_start: end,
                            source_in: 0,
                            source_out: duration,
                            link_group: group,
                        }];
                        if let (Some(g), Some(a_track)) = (group, audio) {
                            commands.push(Command::InsertClip {
                                asset_id: asset.id,
                                track_id: a_track,
                                timeline_start: end,
                                source_in: 0,
                                source_out: duration,
                                link_group: Some(g),
                            });
                        }
                        self.exec(Command::CommandsBatch {
                            commands,
                            expected_revision: None,
                            label: Some("place asset".into()),
                        });
                    }
                }
                if asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false)
                    && ui.small_button("cut silence").clicked()
                {
                    self.cut_silence_for_asset(asset.id);
                }
            });
            ui.end_row();
        }
    }

    fn cut_silence_for_asset(&mut self, asset_id: uuid::Uuid) {
        // Run detection on a worker; apply the cut on the engine after.
        let engine = self.engine.clone();
        let status = self.status_cell.clone();
        self.report("detecting silence…");
        std::thread::spawn(move || {
            let Ok(mut guard) = engine.lock() else { return };
            let detect = Command::DetectSilence {
                asset_id,
                threshold_db: Some(-35.0),
                min_duration_secs: Some(0.6),
                padding_secs: Some(0.15),
            };
            let Ok(outcome) = guard.execute(detect) else { return };
            let Some(ranges) = outcome.data["ranges"].as_array() else { return };
            let project = match guard.project() {
                Some(p) => p.clone(),
                None => return,
            };
            // Find the timeline clips that use this asset so detected source
            // ranges can be mapped onto timeline positions.
            let mut commands: Vec<Command> = Vec::new();
            for range in ranges {
                let start = range["start"].as_i64().unwrap_or(0);
                let end = range["end"].as_i64().unwrap_or(0);
                for clip in &project.clips {
                    if clip.asset_id != asset_id {
                        continue;
                    }
                    let tl_start = clip.timeline_start + (start - clip.source_in).max(0);
                    let tl_end = clip.timeline_start + (end - clip.source_in).max(0);
                    if tl_end > tl_start {
                        commands.push(Command::RemoveRanges {
                            start: tl_start,
                            end: tl_end,
                            ripple: Some(true),
                            tracks: None,
                        });
                        break; // linked partner covers the same span
                    }
                }
            }
            // Descending order keeps earlier ranges valid.
            commands.reverse();
            if commands.is_empty() {
                report_to(&status, "no silence worth cutting was found");
                return;
            }
            let count = commands.len();
            match guard.execute(Command::CommandsBatch {
                commands,
                expected_revision: None,
                label: Some("cut detected silence".into()),
            }) {
                Ok(_) => report_to(&status, format!("cut {count} silence range(s)")),
                Err(e) => report_to(&status, format!("silence cut failed: {e}")),
            }
        });
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let Some(project) = self.project_snapshot() else {
            ui.label("No project open.");
            return;
        };
        ui.heading("Inspector");
        let selected_clip = project.editor.selection.iter().find_map(|s| match s {
            Selection::Clip(id) => project.clip(id).cloned(),
            _ => None,
        });
        match selected_clip {
            None => {
                ui.label("select a clip on the timeline");
            }
            Some(clip) => {
                let asset_name = project
                    .asset(&clip.asset_id)
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                ui.label(format!("Clip: {asset_name}"));
                ui.separator();
                if let Some(new_start) =
                    timeline::seconds_field(ui, "start", clip.timeline_start)
                {
                    self.exec(Command::MoveClip {
                        clip_id: clip.id,
                        timeline_start: new_start,
                        track_id: None,
                    });
                }
                if let Some(new_out) =
                    timeline::seconds_field(ui, "source out", clip.source_out)
                {
                    self.exec(Command::TrimClip {
                        clip_id: clip.id,
                        timeline_start: None,
                        source_in: None,
                        source_out: Some(new_out),
                    });
                }
                let playhead = project.editor.playhead_ticks;
                ui.horizontal(|ui| {
                    if ui.button("split at playhead").clicked() {
                        self.exec(Command::SplitClip {
                            clip_id: clip.id,
                            at: playhead,
                        });
                    }
                    if ui.button("remove").clicked() {
                        self.exec(Command::RemoveClip { clip_id: clip.id });
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("fade in at start").clicked() {
                        self.exec(Command::AddTransition {
                            at: clip.timeline_start,
                            duration: TICKS_PER_SEC / 2,
                            transition_kind: None,
                            color: Some("#000000".into()),
                        });
                    }
                    if ui.button("fade out at end").clicked() {
                        self.exec(Command::AddTransition {
                            at: clip.timeline_end(),
                            duration: TICKS_PER_SEC / 2,
                            transition_kind: None,
                            color: Some("#000000".into()),
                        });
                    }
                });
                // volume / opacity
                let mut volume = clip.volume;
                ui.horizontal(|ui| {
                    ui.label("volume");
                    if ui.add(egui::Slider::new(&mut volume, 0.0..=2.0)).changed() {
                        self.exec(Command::UpdateClip {
                            clip_id: clip.id,
                            volume: Some(volume),
                            opacity: None,
                            scale: None,
                            x_offset: None,
                            y_offset: None,
                            fade_in_ticks: None,
                            fade_out_ticks: None,
                        });
                    }
                });
                let mut opacity = clip.opacity;
                ui.horizontal(|ui| {
                    ui.label("opacity");
                    if ui
                        .add(egui::Slider::new(&mut opacity, 0.0..=1.0))
                        .changed()
                    {
                        self.exec(Command::UpdateClip {
                            clip_id: clip.id,
                            volume: None,
                            opacity: Some(opacity),
                            scale: None,
                            x_offset: None,
                            y_offset: None,
                            fade_in_ticks: None,
                            fade_out_ticks: None,
                        });
                    }
                });
                if ui.button("cut silence (this clip)").clicked() {
                    self.cut_silence_for_asset(clip.asset_id);
                }
            }
        }
        ui.separator();
        ui.heading("Captions");
        let playhead = project.editor.playhead_ticks;
        if ui.button("+ caption at playhead").clicked() {
            self.exec(Command::AddCaption {
                start: playhead,
                end: playhead + TICKS_PER_SEC * 2,
                text: "New caption".into(),
            });
        }
        let captions = project.captions.clone();
        for caption in &captions {
            let selected = project
                .editor
                .selection
                .iter()
                .any(|s| matches!(s, Selection::Caption(id) if id == &caption.id));
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(
                        selected,
                        format!(
                            "{}–{}",
                            openprem_core::ticks::format_seconds(caption.start),
                            openprem_core::ticks::format_seconds(caption.end)
                        ),
                    )
                    .on_hover_text(&caption.text)
                    .clicked()
                {
                    self.exec(Command::SetSelection {
                        selection: vec![Selection::Caption(caption.id)],
                    });
                }
                if ui.small_button("✕").clicked() {
                    self.exec(Command::RemoveCaption { caption_id: caption.id });
                }
            });
        }

        ui.separator();
        ui.heading("Export");
        ui.horizontal(|ui| {
            ui.label("Mbps");
            let mut bitrate = self.export_bitrate;
            ui.add(
                egui::DragValue::new(&mut bitrate)
                    .speed(1.0)
                    .range(0.0..=120.0)
                    .suffix(" M"),
            );
            self.export_bitrate = bitrate;
            ui.checkbox(&mut self.use_nvenc, "NVENC");
        });
        if ui.button("Export MP4").clicked() {
            let cmd = Command::StartRender {
                output_path: None,
                video_bitrate_mbps: if self.export_bitrate > 0.0 {
                    Some(self.export_bitrate)
                } else {
                    None
                },
                use_nvenc: Some(self.use_nvenc),
            };
            self.exec(cmd);
        }
        // Jobs panel.
        if let Ok(mut engine) = self.engine.try_lock() {
            let _ = engine.poll_jobs();
            let jobs = {
                let reg = engine.jobs();
                let reg = reg.lock().expect("jobs lock");
                reg.list_json(5)
            };
            if let Some(list) = jobs["jobs"].as_array() {
                ui.separator();
                ui.heading("Jobs");
                for job in list {
                    let state = &job["state"];
                    let status = state["status"].as_str().unwrap_or("?");
                    let label = job["label"].as_str().unwrap_or("");
                    let line = match status {
                        "running" => format!(
                            "▶ {label} {:.0}%",
                            (state["progress"].as_f64().unwrap_or(0.0) * 100.0)
                        ),
                        "done" => format!("✓ {label}"),
                        "failed" => format!(
                            "✗ {label}: {}",
                            state["error"].as_str().unwrap_or("?")
                        ),
                        _ => format!("– {label}"),
                    };
                    ui.label(egui::RichText::new(line).small());
                }
            }
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui, project: &openprem_core::Project) {
        // Ask the worker for the frame at the playhead.
        self.request_preview(project);
        let playhead = project.editor.playhead_ticks;
        let end = project.timeline_end().max(TICKS_PER_SEC);

        // Transport.
        ui.horizontal(|ui| {
            let frame = project.settings.fps.frame_duration_ticks();
            if ui.button("⏮").clicked() {
                self.exec(Command::SetPlayhead { ticks: 0 });
            }
            if ui.button("◀ frame").clicked() {
                self.exec(Command::SetPlayhead {
                    ticks: (playhead - frame).max(0),
                });
            }
            if ui.button("frame ▶").clicked() {
                self.exec(Command::SetPlayhead {
                    ticks: (playhead + frame).min(end),
                });
            }
            if ui.button("⏭").clicked() {
                self.exec(Command::SetPlayhead { ticks: end });
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} / {}",
                    openprem_core::ticks::format_seconds(playhead),
                    openprem_core::ticks::format_seconds(end)
                ))
                .monospace(),
            );
        });
        let mut playhead_secs = openprem_core::secs_from_ticks(playhead) as f32;
        let slider = egui::Slider::new(&mut playhead_secs, 0.0..=openprem_core::secs_from_ticks(end) as f32);
        if ui.add(slider).changed() {
            self.exec(Command::SetPlayhead {
                ticks: openprem_core::ticks_from_secs(playhead_secs as f64),
            });
        }

        // The decoded frame, if ready.
        let key = latest_key_for(&self.frame_cache);
        let mut image_rect: Option<egui::Rect> = None;
        if let Some(k) = key {
            if let Some(handle) = self.textures.get(&k) {
                let avail = ui.available_size();
                let scale = (avail.x / handle.size_vec2().x.max(1.0)).min(2.0);
                let size = handle.size_vec2() * scale;
                let response =
                    ui.add(egui::Image::new(handle).max_size(size));
                image_rect = Some(response.rect);
            } else if let Some((frame, _)) = self.frame_cache.get(&k) {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.bytes,
                );
                let handle = ui.ctx().load_texture(
                    format!("preview-{k}"),
                    image,
                    egui::TextureOptions::default(),
                );
                self.textures.insert(k, handle);
                ui.ctx().request_repaint();
            }
        } else {
            let avail = ui.available_size();
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(avail.x, (avail.y - 40.0).max(80.0)),
                egui::Sense::hover(),
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "seek the timeline to preview",
                egui::FontId::proportional(16.0),
                ui.visuals().weak_text_color(),
            );
            image_rect = Some(rect);
        }

        // Caption overlay (approximation of burn-in).
        if let Some(rect) = image_rect {
            let painter = ui.painter_at(rect);
            for caption in project.captions_at(playhead) {
                let pos = egui::pos2(rect.center().x, rect.bottom() - 28.0);
                let font = egui::FontId::proportional(18.0);
                // Cheap outline: black text in four offsets under the white.
                for d in [
                    egui::vec2(-1.0, 0.0),
                    egui::vec2(1.0, 0.0),
                    egui::vec2(0.0, -1.0),
                    egui::vec2(0.0, 1.0),
                ] {
                    painter.text(pos + d, egui::Align2::CENTER_CENTER, &caption.text, font.clone(), egui::Color32::BLACK);
                }
                painter.text(pos, egui::Align2::CENTER_CENTER, &caption.text, font, egui::Color32::WHITE);
            }
        }
    }
}

fn latest_key_for(cache: &HashMap<u64, (RgbaFrame, Instant)>) -> Option<u64> {
    cache
        .iter()
        .max_by_key(|(_, (_, at))| *at)
        .map(|(k, _)| *k)
}

/// Create a project sized to the first media file (rotation-aware), placed
/// beside that file. Called on import threads when no project is open, so
/// dropping/picking media into an empty editor is never a dead end.
fn auto_create_project_for(
    engine: &Arc<Mutex<Engine>>,
    status: &Arc<Mutex<StatusLine>>,
    paths: &[PathBuf],
) -> Result<(), String> {
    let first = paths
        .iter()
        .find(|p| p.is_file())
        .ok_or("none of the files exist")?;
    let probe = openprem_media::ffprobe::probe(first).ok();
    let (w, h) = probe
        .as_ref()
        .map(|p| p.probe.oriented_size())
        .unwrap_or((1920, 1080));
    let even = |v: u32| v.clamp(2, 7680) & !1;
    let (w, h) = (even(w), even(h));
    let fps = probe
        .as_ref()
        .and_then(|p| p.probe.fps)
        .filter(|f| (1.0..=240.0).contains(&f.as_f64()));
    let (fps_num, fps_den) = fps.map(|f| (f.num, f.den)).unwrap_or((30, 1));
    let parent = first
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        });
    let stem = first
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "OpenPrem Project".into());
    let dir = unique_project_dir(&parent, &stem);
    let mut guard = engine.lock().map_err(|e| format!("engine lock: {e}"))?;
    guard
        .execute(Command::CreateProject {
            name: stem.clone(),
            path: dir.clone(),
            width: Some(w),
            height: Some(h),
            fps_num: Some(fps_num),
            fps_den: Some(fps_den),
        })
        .map_err(|e| format!("could not create project: {e}"))?;
    let fps_display = openprem_core::Fps::new(fps_num, fps_den).display();
    report_to(
        status,
        format!(
            "created project {} ({}×{} @ {}fps)",
            dir.display(),
            w,
            h,
            fps_display
        ),
    );
    Ok(())
}

/// `<stem>.openprem`, then `<stem>-2.openprem`, ... — never overwrites.
fn unique_project_dir(parent: &std::path::Path, stem: &str) -> PathBuf {
    let base = parent.join(format!("{stem}.openprem"));
    if !base.exists() {
        return base;
    }
    for i in 2..100 {
        let candidate = parent.join(format!("{stem}-{i}.openprem"));
        if !candidate.exists() {
            return candidate;
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    parent.join(format!("{stem}-{stamp}.openprem"))
}

fn decode_layers(req: &PreviewRequest) -> RgbaFrame {
    let mut bytes = Vec::with_capacity((req.canvas_w * req.canvas_h * 4) as usize);
    for _ in 0..(req.canvas_w * req.canvas_h) {
        bytes.extend_from_slice(&[16, 16, 16, 255]);
    }
    let mut canvas = RgbaFrame {
        width: req.canvas_w,
        height: req.canvas_h,
        bytes,
    };
    for layer in &req.layers {
        let frame = if layer.fit {
            frames::extract_frame_fit(&layer.path, layer.seek_secs, layer.scale_w, layer.scale_h)
        } else {
            frames::extract_frame_scale(
                &layer.path,
                layer.seek_secs,
                layer.scale_w,
                layer.scale_h,
            )
        };
        let Ok(over) = frame else { continue };
        frames::paste_rgba(&mut canvas, &over, layer.paste_x, layer.paste_y, layer.opacity);
    }
    canvas
}

impl eframe::App for OpenPremApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Adopt newer status text reported by background threads.
        {
            let cell = self.status_cell.lock().expect("status lock");
            if cell.seq != self.status_seq {
                self.status_seq = cell.seq;
                self.status = cell.text.clone();
            }
        }

        // Poll the file picker worker.
        let picked: Option<PickerOutcome> =
            self.picker_rx.as_ref().and_then(|rx| match rx.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(PickerOutcome::Cancelled)
                }
            });
        if let Some(outcome) = picked {
            self.picker_rx = None;
            match outcome {
                PickerOutcome::Files(paths) => {
                    let n = paths.len();
                    self.report(format!("importing {n} file(s)…"));
                    self.import_files(paths);
                }
                PickerOutcome::Cancelled => {}
                PickerOutcome::Failed(msg) => self.report(msg),
            }
        }

        // Drain completed preview frames.
        self.drain_preview_results(ui.ctx());

        egui::Panel::top("top_bar")
            .exact_size(40.0)
            .resizable(false)
            .show(ui, |ui| self.top_bar(ui));

        egui::Panel::left("media_bin")
            .default_size(230.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.media_bin(ui));
            });

        egui::Panel::right("inspector")
            .default_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.inspector(ui));
            });

        egui::Panel::bottom("timeline_panel")
            .default_size(260.0)
            .show(ui, |ui| {
                let Some(project) = self.project_snapshot() else {
                    ui.label("No project open — create one above or drive me with MCP.");
                    return;
                };
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} · {}x{} @ {}fps · revision {}",
                            project.name,
                            project.settings.width,
                            project.settings.height,
                            project.settings.fps.display(),
                            project.revision
                        ))
                        .small(),
                    );
                });
                let action = timeline::draw_timeline(ui, &project, &mut self.drag);
                if let Some(t) = action.seek {
                    self.exec(Command::SetPlayhead { ticks: t });
                }
                if let Some(clip_id) = action.select_clip {
                    self.exec(Command::SetSelection {
                        selection: vec![Selection::Clip(clip_id)],
                    });
                }
                if let Some((clip_id, new_start)) = action.move_clip {
                    self.exec(Command::MoveClip {
                        clip_id,
                        timeline_start: new_start,
                        track_id: None,
                    });
                }
                // Zoom with the wheel while hovering the timeline.
                let zoom_delta = ui.input(|i| i.zoom_delta());
                if (zoom_delta - 1.0).abs() > f32::EPSILON {
                    let project = self.project_snapshot();
                    if let Some(p) = project {
                        let z = p.editor.zoom_pixels_per_second;
                        let new_z = (z * zoom_delta).clamp(1.0, 5000.0);
                        if new_z != z {
                            self.exec(Command::SetZoom {
                                pixels_per_second: new_z,
                            });
                        }
                    }
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            let Some(project) = self.project_snapshot() else {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("OpenPrem");
                        ui.label(
                            egui::RichText::new(
                                "Press “Import media…” or drop a video file anywhere.",
                            )
                            .color(ui.visuals().text_color()),
                        );
                        ui.label(
                            egui::RichText::new(
                                "With no project open, one is created beside your media — \
                                 sized to match your footage.",
                            )
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        );
                    });
                });
                return;
            };
            self.preview_panel(ui, &project);
        });

        // Keep the UI reactive while background work lands.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
}
