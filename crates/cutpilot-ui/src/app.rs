//! The CutPilot native editor app.
//!
//! Architecture highlights:
//! - Non-blocking UI thread: uses a cached `Project` snapshot refreshed via
//!   `try_lock()` so background imports, silence detection, and MCP agent
//!   calls never freeze frame rate.
//! - Real-time A/V playback + latest-wins coalesced scrubbing via `playback.rs`.
//! - Background GPU edit proxies, waveforms, and thumbnails via `media_cache.rs`.
//! - Live MCP IPC bridge (`cutpilot_engine::IpcServer`) so Claude Code / Cursor
//!   / Codex can edit the open timeline in real time (`mcp_panel.rs`).
//! - All semantic edits route through `Engine::execute` (no UI-only actions).

use crate::filepicker::{self, FolderPickerOutcome, PickerOutcome};
use crate::mcp_panel::McpPanelState;
use crate::media_cache::{MediaCache, ProxyState};
use crate::playback::PlaybackEngine;
use crate::timeline::{self, TimelineState, TimelineTool};
use eframe::egui;
use cutpilot_core::{AssetKind, Command, Project, Selection, Ticks, TrackKind, TICKS_PER_SEC};
use cutpilot_engine::{Engine, IpcServer, IpcStats, JobRegistry};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ExportResolution {
    #[default]
    MatchProject,
    Uhd4K,
    Qhd1440,
    Fhd1080,
    Hd720,
    Sd480,
    Vertical1080,
    Square1080,
    Custom,
}

impl ExportResolution {
    const ALL: &'static [Self] = &[
        Self::MatchProject,
        Self::Uhd4K,
        Self::Qhd1440,
        Self::Fhd1080,
        Self::Hd720,
        Self::Sd480,
        Self::Vertical1080,
        Self::Square1080,
        Self::Custom,
    ];

    fn label(self, project: &Project) -> String {
        match self {
            Self::MatchProject => format!(
                "Match Project ({}×{})",
                project.settings.width, project.settings.height
            ),
            Self::Uhd4K => "4K UHD (3840×2160)".into(),
            Self::Qhd1440 => "1440p QHD (2560×1440)".into(),
            Self::Fhd1080 => "1080p Full HD (1920×1080)".into(),
            Self::Hd720 => "720p HD (1280×720)".into(),
            Self::Sd480 => "480p SD (854×480)".into(),
            Self::Vertical1080 => "Vertical 9:16 (1080×1920)".into(),
            Self::Square1080 => "Square 1:1 (1080×1080)".into(),
            Self::Custom => "Custom…".into(),
        }
    }

    fn resolve(self, project: &Project, custom_w: u32, custom_h: u32) -> (u32, u32) {
        let even = |v: u32| v.clamp(16, 7680) & !1;
        match self {
            Self::MatchProject => (project.settings.width, project.settings.height),
            Self::Uhd4K => (3840, 2160),
            Self::Qhd1440 => (2560, 1440),
            Self::Fhd1080 => (1920, 1080),
            Self::Hd720 => (1280, 720),
            Self::Sd480 => (854, 480),
            Self::Vertical1080 => (1080, 1920),
            Self::Square1080 => (1080, 1080),
            Self::Custom => (even(custom_w), even(custom_h)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderPickerPurpose {
    OpenProject,
    NewProjectLocation,
    SaveProjectAs,
}

const NEW_PROJECT_FPS_OPTIONS: &[(&str, u32, u32)] = &[
    ("24 fps", 24, 1),
    ("29.97 fps (NTSC)", 30_000, 1_001),
    ("30 fps", 30, 1),
    ("60 fps", 60, 1),
];

struct NewProjectDialog {
    parent_dir: PathBuf,
    name: String,
    resolution: ExportResolution,
    fps_idx: usize,
    focus_name: bool,
}

pub struct CutPilotApp {
    engine: Arc<Mutex<Engine>>,
    /// Direct handle to the engine's background job registry so the UI can
    /// read active renders (e.g. HyperFrames from MCP) without ever blocking on
    /// `engine.lock()`.
    jobs: Arc<Mutex<JobRegistry>>,
    /// Keeps the live Unix-socket IPC bridge alive for MCP agents.
    _ipc: IpcServer,
    ipc_stats: Arc<Mutex<IpcStats>>,
    mcp_panel: McpPanelState,
    was_hyperframes_busy: bool,

    /// Cached snapshot refreshed via `try_lock()` so the UI never blocks.
    cached_project: Option<Project>,
    cached_project_dir: Option<PathBuf>,
    last_revision: u64,

    playback: PlaybackEngine,
    media_cache: MediaCache,
    timeline_state: TimelineState,

    status: String,
    status_cell: Arc<Mutex<StatusLine>>,
    status_seq: u64,

    media_path_field: String,
    picker_rx: Option<std::sync::mpsc::Receiver<PickerOutcome>>,
    folder_picker_rx: Option<(
        FolderPickerPurpose,
        std::sync::mpsc::Receiver<FolderPickerOutcome>,
    )>,
    new_project_dialog: Option<NewProjectDialog>,
    drop_list_len: usize,
    /// Asset currently being dragged from the Media Bin onto the timeline.
    dragging_asset: Option<Uuid>,

    // Inline caption editor buffer (synced when caption selection changes).
    caption_edit_id: Option<Uuid>,
    caption_edit_text: String,

    // Whisper transcription controls.
    whisper_model: String,
    transcript_search: String,

    // Export controls.
    export_bitrate: f64,
    use_nvenc: bool,
    export_codec: String,
    export_preset: String,
    export_resolution: ExportResolution,
    export_custom_width: u32,
    export_custom_height: u32,
}

impl CutPilotApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_nle_theme(&cc.egui_ctx);

        let engine_inner = Engine::new();
        let jobs = engine_inner.jobs();
        {
            let wake_ctx = cc.egui_ctx.clone();
            if let Ok(mut reg) = jobs.lock() {
                reg.set_on_change(Arc::new(move || {
                    wake_ctx.request_repaint();
                }));
            }
        }
        let engine = Arc::new(Mutex::new(engine_inner));
        let repaint_ctx = cc.egui_ctx.clone();
        let ipc = IpcServer::start(
            engine.clone(),
            Some(Arc::new(move || {
                repaint_ctx.request_repaint();
            })),
        );
        let ipc_stats = ipc.stats();

        // Warm GPU capability detection off the UI thread at launch so the
        // Export panel and preview know NVENC/CUDA status immediately.
        let gpu_ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            let _ = cutpilot_media::gpu::capabilities();
            gpu_ctx.request_repaint();
        });

        let initial_status =
            "Import video or audio (WAV/MP3) or drag & drop — or click “Connect MCP Agent” to edit with AI."
                .to_string();

        Self {
            engine,
            jobs,
            _ipc: ipc,
            ipc_stats,
            mcp_panel: McpPanelState::default(),
            was_hyperframes_busy: false,
            cached_project: None,
            cached_project_dir: None,
            last_revision: 0,
            playback: PlaybackEngine::new(cc.egui_ctx.clone()),
            media_cache: MediaCache::new(),
            timeline_state: TimelineState::default(),
            status: initial_status.clone(),
            status_cell: Arc::new(Mutex::new(StatusLine {
                seq: 1,
                text: initial_status,
            })),
            status_seq: 1,
            media_path_field: String::new(),
            picker_rx: None,
            folder_picker_rx: None,
            new_project_dialog: None,
            drop_list_len: 0,
            dragging_asset: None,
            caption_edit_id: None,
            caption_edit_text: String::new(),
            whisper_model: cutpilot_media::whisper::DEFAULT_MODEL.into(),
            transcript_search: String::new(),
            export_bitrate: 0.0,
            use_nvenc: true,
            export_codec: "h264".into(),
            export_preset: "balanced".into(),
            export_resolution: ExportResolution::MatchProject,
            export_custom_width: 1920,
            export_custom_height: 1080,
        }
    }

    // ------------------------------------------------------------------
    // Engine plumbing (non-blocking snapshot + command dispatch)
    // ------------------------------------------------------------------

    fn report(&self, text: impl Into<String>) {
        report_to(&self.status_cell, text);
    }

    fn refresh_snapshot(&mut self) {
        if let Ok(mut engine) = self.engine.try_lock() {
            let _ = engine.poll_jobs();
            let dir = engine.project_dir().map(std::path::Path::to_path_buf);
            let proj = engine.project().cloned();
            if let Some(p) = &proj {
                if p.revision != self.last_revision {
                    self.last_revision = p.revision;
                    // If an edit landed while playing (e.g. from MCP), stop playback so
                    // the stream restarts cleanly on the updated timeline.
                    if self.playback.is_playing() {
                        self.playback.stop_playback();
                    }
                }
            }
            self.cached_project = proj;
            self.cached_project_dir = dir;
        }
    }

    fn exec(&mut self, cmd: Command) {
        let mutates = !cmd.is_editor_state();
        if mutates && self.playback.is_playing() {
            self.playback.stop_playback();
        }
        let Ok(mut engine) = self.engine.lock() else {
            return;
        };
        match engine.execute(cmd) {
            Ok(outcome) => {
                if mutates {
                    self.report(outcome.message);
                }
                self.cached_project = engine.project().cloned();
                self.cached_project_dir = engine.project_dir().map(std::path::Path::to_path_buf);
                if let Some(p) = &self.cached_project {
                    self.last_revision = p.revision;
                }
            }
            Err(e) => {
                self.report(format!("error: {e}"));
            }
        }
    }

    fn seek_to(&mut self, ticks: Ticks) {
        let clamped = ticks.max(0);
        if self.playback.is_playing() {
            self.playback.stop_playback();
        }
        self.exec(Command::SetPlayhead { ticks: clamped });
    }

    fn toggle_play(&mut self, ctx: &egui::Context) {
        if self.playback.is_playing() {
            self.playback.stop_playback();
        } else if let Some(project) = &self.cached_project {
            let proxies = self.media_cache.ready_proxy_map();
            self.playback.start_playback(project, &proxies, ctx);
        }
    }

    /// Import media files off-thread without holding the engine lock during probing.
    fn import_files(&self, paths: Vec<PathBuf>, ctx: egui::Context) {
        if paths.is_empty() {
            return;
        }
        let engine = self.engine.clone();
        let status = self.status_cell.clone();
        std::thread::spawn(move || {
            let needs_project = engine.lock().map(|g| !g.is_open()).unwrap_or(false);
            if needs_project {
                if let Err(msg) = auto_create_project_for(&engine, &status, &paths) {
                    report_to(&status, msg);
                    ctx.request_repaint();
                    return;
                }
            }
            for path in paths {
                if !path.is_file() {
                    report_to(&status, format!("skipped (not a file): {}", path.display()));
                    ctx.request_repaint();
                    continue;
                }
                let already = engine
                    .lock()
                    .ok()
                    .and_then(|g| {
                        g.project()
                            .map(|p| p.assets.iter().any(|a| a.path == path))
                    })
                    .unwrap_or(false);
                if already {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    report_to(&status, format!("already in media bin: {name}"));
                    ctx.request_repaint();
                    continue;
                }
                let res = {
                    let Ok(mut guard) = engine.lock() else {
                        return;
                    };
                    guard.execute(Command::ImportMedia {
                        path: path.clone(),
                        auto_place: Some(true),
                    })
                };
                match res {
                    Ok(outcome) => {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string());
                        let kind = outcome.data["kind"].as_str().unwrap_or("video");
                        let msg = match kind {
                            "audio" => format!("Imported audio {name} onto audio track"),
                            "image" => format!("Imported image {name}"),
                            _ => format!(
                                "Imported {name} (building GPU edit proxy in background…)"
                            ),
                        };
                        report_to(&status, msg);
                    }
                    Err(e) => report_to(&status, format!("import failed ({}): {e}", path.display())),
                }
                ctx.request_repaint();
            }
        });
    }

    fn open_file_picker(&mut self) {
        if self.picker_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = filepicker::pick_files_blocking("Import media into CutPilot");
            let _ = tx.send(outcome);
        });
        self.picker_rx = Some(rx);
        self.report("Opening file picker…");
    }

    fn open_folder_picker(&mut self, purpose: FolderPickerPurpose) {
        if self.folder_picker_rx.is_some() {
            return;
        }
        let title = match purpose {
            FolderPickerPurpose::OpenProject => "Open CutPilot Project Folder",
            FolderPickerPurpose::NewProjectLocation => "Choose Folder for New CutPilot Project",
            FolderPickerPurpose::SaveProjectAs => "Choose Folder to Save Project Into",
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = filepicker::pick_folder_blocking(title);
            let _ = tx.send(outcome);
        });
        self.folder_picker_rx = Some((purpose, rx));
        self.report("Opening folder browser…");
    }

    /// Run silence detection completely OFF the engine lock, then lock briefly
    /// to commit the atomic `CommandsBatch`.
    fn cut_silence_for_asset(&mut self, asset_id: Uuid, ctx: egui::Context) {
        let Some(project) = self.cached_project.clone() else {
            return;
        };
        let Some(asset) = project.asset(&asset_id).cloned() else {
            return;
        };
        let engine = self.engine.clone();
        let status = self.status_cell.clone();
        self.report(format!("Analyzing silence in {}…", asset.name));

        std::thread::spawn(move || {
            let opts = cutpilot_media::silence::SilenceOptions::default();
            let duration = asset.editable_duration();
            let ranges = match cutpilot_media::silence::detect_silence(&asset.path, duration, &opts)
            {
                Ok(r) => r,
                Err(e) => {
                    report_to(&status, format!("silence detection failed: {e}"));
                    ctx.request_repaint();
                    return;
                }
            };
            if ranges.is_empty() {
                report_to(&status, "No dead space found to cut.");
                ctx.request_repaint();
                return;
            }
            let Ok(mut guard) = engine.lock() else {
                return;
            };
            let Some(live_project) = guard.project().cloned() else {
                return;
            };
            let mut commands: Vec<Command> = Vec::new();
            for range in ranges {
                for clip in &live_project.clips {
                    if clip.asset_id != asset_id {
                        continue;
                    }
                    let tl_start = clip.timeline_start + (range.start - clip.source_in).max(0);
                    let tl_end = clip.timeline_start + (range.end - clip.source_in).max(0);
                    if tl_end > tl_start && tl_start < clip.timeline_end() {
                        commands.push(Command::RemoveRanges {
                            start: tl_start,
                            end: tl_end.min(clip.timeline_end()),
                            ripple: Some(true),
                            tracks: None,
                        });
                        break;
                    }
                }
            }
            commands.reverse();
            if commands.is_empty() {
                report_to(&status, "No matching timeline silence found.");
                ctx.request_repaint();
                return;
            }
            let count = commands.len();
            match guard.execute(Command::CommandsBatch {
                commands,
                expected_revision: None,
                label: Some("cut detected silence".into()),
            }) {
                Ok(_) => report_to(&status, format!("Cut {count} silent range(s) with ripple.")),
                Err(e) => report_to(&status, format!("silence cut failed: {e}")),
            }
            ctx.request_repaint();
        });
    }

    fn split_at_playhead(&mut self) {
        let Some(project) = self.cached_project.clone() else {
            return;
        };
        let playhead = project.editor.playhead_ticks;
        // Prefer selected clip if playhead is inside it; otherwise split the clip
        // under the playhead on the base video track (or any unlocked track).
        let selected_under = project.editor.selection.iter().find_map(|s| match s {
            Selection::Clip(id) => project
                .clip(id)
                .filter(|c| playhead > c.timeline_start && playhead < c.timeline_end())
                .map(|c| c.id),
            _ => None,
        });
        let target = selected_under.or_else(|| {
            project
                .base_video_track()
                .and_then(|t| project.clip_at(&t.id, playhead))
                .filter(|c| playhead > c.timeline_start && playhead < c.timeline_end())
                .map(|c| c.id)
        }).or_else(|| {
            project
                .clips
                .iter()
                .find(|c| playhead > c.timeline_start && playhead < c.timeline_end())
                .map(|c| c.id)
        });

        if let Some(clip_id) = target {
            self.exec(Command::SplitClip {
                clip_id,
                at: playhead,
            });
        } else {
            self.report("Move playhead inside a clip to split.");
        }
    }

    fn ripple_delete_clip(&mut self, clip_id: Uuid) {
        let Some(project) = self.cached_project.clone() else {
            return;
        };
        let Some(clip) = project.clip(&clip_id) else {
            return;
        };
        self.exec(Command::RemoveRanges {
            start: clip.timeline_start,
            end: clip.timeline_end(),
            ripple: Some(true),
            tracks: None,
        });
    }

    fn fit_timeline_zoom(&mut self, viewport_width: f32) {
        let Some(project) = &self.cached_project else {
            return;
        };
        let dur_secs = cutpilot_core::secs_from_ticks(project.timeline_end().max(TICKS_PER_SEC * 2));
        let usable = (viewport_width - timeline::HEADER_WIDTH - 48.0).max(200.0);
        let pps = ((usable as f64 / (dur_secs * 1.02 + 1.0)) as f32)
            .clamp(timeline::MIN_ZOOM, timeline::MAX_ZOOM);
        self.exec(Command::SetZoom {
            pixels_per_second: pps,
        });
    }

    fn transcribe_asset(&mut self, asset_id: Uuid) {
        self.exec(Command::TranscribeMedia {
            asset_id,
            model: Some(self.whisper_model.clone()),
            language: None,
            translate: Some(false),
            add_captions: Some(false),
            wait: Some(false),
        });
    }

    // ------------------------------------------------------------------
    // Keyboard shortcuts
    // ------------------------------------------------------------------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.new_project_dialog.is_some() {
            return;
        }

        // File-level shortcuts work even before a project is open.
        let (cmd_mod, shift, key_n, key_o, key_i, key_s) = ctx.input(|i| {
            (
                i.modifiers.command || i.modifiers.ctrl,
                i.modifiers.shift,
                i.key_pressed(egui::Key::N),
                i.key_pressed(egui::Key::O),
                i.key_pressed(egui::Key::I),
                i.key_pressed(egui::Key::S),
            )
        });
        if cmd_mod && key_n {
            self.open_folder_picker(FolderPickerPurpose::NewProjectLocation);
            return;
        }
        if cmd_mod && key_o {
            self.open_folder_picker(FolderPickerPurpose::OpenProject);
            return;
        }
        if cmd_mod && key_i {
            self.open_file_picker();
            return;
        }
        if cmd_mod && shift && key_s && self.cached_project.is_some() {
            self.open_folder_picker(FolderPickerPurpose::SaveProjectAs);
            return;
        }

        let Some(project) = self.cached_project.clone() else {
            return;
        };
        let frame = project.settings.fps.frame_duration_ticks();
        let playhead = project.editor.playhead_ticks;
        let end = project.timeline_end();

        let (
            space,
            key_v,
            key_c,
            key_s,
            key_d,
            key_m,
            key_i,
            key_o,
            key_x,
            key_f,
            key_j,
            key_k,
            key_l,
            left,
            right,
            home,
            end_k,
            delete,
            shift,
            cmd_mod,
            key_z,
            key_y,
            plus,
            minus,
        ) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::V),
                i.key_pressed(egui::Key::C) || i.key_pressed(egui::Key::B),
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::D),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::I),
                i.key_pressed(egui::Key::O),
                i.key_pressed(egui::Key::X),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::J),
                i.key_pressed(egui::Key::K),
                i.key_pressed(egui::Key::L),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Home),
                i.key_pressed(egui::Key::End),
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.modifiers.shift,
                i.modifiers.command || i.modifiers.ctrl,
                i.key_pressed(egui::Key::Z),
                i.key_pressed(egui::Key::Y),
                i.key_pressed(egui::Key::Equals) || i.key_pressed(egui::Key::Plus),
                i.key_pressed(egui::Key::Minus),
            )
        });

        if cmd_mod && key_z && shift {
            self.exec(Command::Redo {});
            return;
        }
        if cmd_mod && key_z {
            self.exec(Command::Undo {});
            return;
        }
        if cmd_mod && key_y {
            self.exec(Command::Redo {});
            return;
        }
        if cmd_mod && key_s {
            self.exec(Command::SaveProject { path: None });
            return;
        }
        if cmd_mod && key_d {
            let sel = project.editor.selection.iter().find_map(|s| match s {
                Selection::Clip(id) => Some(*id),
                _ => None,
            });
            if let Some(cid) = sel {
                self.exec(Command::DuplicateClip {
                    clip_id: cid,
                    timeline_start: None,
                });
            }
            return;
        }

        if space {
            self.toggle_play(ctx);
        } else if key_k {
            self.playback.stop_playback();
        } else if key_l {
            if !self.playback.is_playing() {
                self.toggle_play(ctx);
            }
        } else if key_j {
            self.seek_to((playhead - TICKS_PER_SEC * 2).max(0));
        } else if key_v {
            self.timeline_state.tool = TimelineTool::Select;
        } else if key_c {
            self.timeline_state.tool = TimelineTool::Razor;
        } else if key_s {
            self.split_at_playhead();
        } else if key_m {
            let label = format!("Marker {}", project.markers.len() + 1);
            self.exec(Command::AddMarker {
                at: playhead,
                label,
                color: None,
            });
        } else if key_i {
            self.timeline_state.in_point = Some(playhead);
        } else if key_o {
            self.timeline_state.out_point = Some(playhead);
        } else if key_x {
            if let Some((s, e)) = self.timeline_state.active_range() {
                self.exec(Command::RemoveRanges {
                    start: s,
                    end: e,
                    ripple: Some(true),
                    tracks: None,
                });
                self.timeline_state.in_point = None;
                self.timeline_state.out_point = None;
            }
        } else if key_f {
            let w = ctx.content_rect().width();
            self.fit_timeline_zoom(w);
        } else if left {
            let step = if shift { TICKS_PER_SEC } else { frame };
            self.seek_to((playhead - step).max(0));
        } else if right {
            let step = if shift { TICKS_PER_SEC } else { frame };
            self.seek_to((playhead + step).min(end));
        } else if home {
            self.seek_to(0);
        } else if end_k {
            self.seek_to(end);
        } else if delete {
            let sel = project.editor.selection.iter().find_map(|s| match s {
                Selection::Clip(id) => Some(*id),
                _ => None,
            });
            if let Some(cid) = sel {
                if shift {
                    self.ripple_delete_clip(cid);
                } else {
                    self.exec(Command::RemoveClip { clip_id: cid });
                }
            }
        } else if plus {
            let new_z = (project.editor.zoom_pixels_per_second * 1.35)
                .clamp(timeline::MIN_ZOOM, timeline::MAX_ZOOM);
            self.exec(Command::SetZoom {
                pixels_per_second: new_z,
            });
        } else if minus {
            let new_z = (project.editor.zoom_pixels_per_second / 1.35)
                .clamp(timeline::MIN_ZOOM, timeline::MAX_ZOOM);
            self.exec(Command::SetZoom {
                pixels_per_second: new_z,
            });
        }
    }

    // ------------------------------------------------------------------
    // Panels
    // ------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let has_project = self.cached_project.is_some();
        let selected_clip = self.cached_project.as_ref().and_then(|p| {
            p.editor.selection.iter().find_map(|s| match s {
                Selection::Clip(id) => Some(*id),
                _ => None,
            })
        });

        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui
                    .add(egui::Button::new("New Project…").shortcut_text("Ctrl+N"))
                    .clicked()
                {
                    self.open_folder_picker(FolderPickerPurpose::NewProjectLocation);
                    ui.close();
                }
                if ui
                    .add(egui::Button::new("Open Project…").shortcut_text("Ctrl+O"))
                    .clicked()
                {
                    self.open_folder_picker(FolderPickerPurpose::OpenProject);
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Save").shortcut_text("Ctrl+S"),
                    )
                    .clicked()
                {
                    self.exec(Command::SaveProject { path: None });
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S"),
                    )
                    .clicked()
                {
                    self.open_folder_picker(FolderPickerPurpose::SaveProjectAs);
                    ui.close();
                }
                if ui
                    .add_enabled(has_project, egui::Button::new("Close Project"))
                    .clicked()
                {
                    self.exec(Command::CloseProject {});
                    self.playback.invalidate_cache();
                    ui.close();
                }
                ui.separator();
                if ui
                    .add(egui::Button::new("Import Media…").shortcut_text("Ctrl+I"))
                    .clicked()
                {
                    self.open_file_picker();
                    ui.close();
                }
            });

            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Undo").shortcut_text("Ctrl+Z"),
                    )
                    .clicked()
                {
                    self.exec(Command::Undo {});
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Redo").shortcut_text("Ctrl+Shift+Z"),
                    )
                    .clicked()
                {
                    self.exec(Command::Redo {});
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Split at Playhead").shortcut_text("S"),
                    )
                    .clicked()
                {
                    self.split_at_playhead();
                    ui.close();
                }
                if ui
                    .add_enabled(
                        selected_clip.is_some(),
                        egui::Button::new("Duplicate Clip").shortcut_text("Ctrl+D"),
                    )
                    .clicked()
                {
                    if let Some(cid) = selected_clip {
                        self.exec(Command::DuplicateClip {
                            clip_id: cid,
                            timeline_start: None,
                        });
                    }
                    ui.close();
                }
                if ui
                    .add_enabled(
                        selected_clip.is_some(),
                        egui::Button::new("Unlink Audio / Video"),
                    )
                    .clicked()
                {
                    if let Some(cid) = selected_clip {
                        self.exec(Command::UnlinkClip { clip_id: cid });
                    }
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Add Marker at Playhead").shortcut_text("M"),
                    )
                    .clicked()
                {
                    if let Some(p) = &self.cached_project {
                        let at = p.editor.playhead_ticks;
                        let label = format!("Marker {}", p.markers.len() + 1);
                        self.exec(Command::AddMarker {
                            at,
                            label,
                            color: None,
                        });
                    }
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        selected_clip.is_some(),
                        egui::Button::new("Delete Selected Clip").shortcut_text("Del"),
                    )
                    .clicked()
                {
                    if let Some(cid) = selected_clip {
                        self.exec(Command::RemoveClip { clip_id: cid });
                    }
                    ui.close();
                }
                if ui
                    .add_enabled(
                        selected_clip.is_some(),
                        egui::Button::new("Ripple Delete Clip").shortcut_text("Shift+Del"),
                    )
                    .clicked()
                {
                    if let Some(cid) = selected_clip {
                        self.ripple_delete_clip(cid);
                    }
                    ui.close();
                }
            });

            ui.menu_button("View", |ui| {
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Fit Timeline to View").shortcut_text("F"),
                    )
                    .clicked()
                {
                    let w = ui.ctx().content_rect().width();
                    self.fit_timeline_zoom(w);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Zoom In").shortcut_text("+"),
                    )
                    .clicked()
                {
                    if let Some(p) = &self.cached_project {
                        let new_z = (p.editor.zoom_pixels_per_second * 1.35)
                            .clamp(timeline::MIN_ZOOM, timeline::MAX_ZOOM);
                        self.exec(Command::SetZoom {
                            pixels_per_second: new_z,
                        });
                    }
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_project,
                        egui::Button::new("Zoom Out").shortcut_text("−"),
                    )
                    .clicked()
                {
                    if let Some(p) = &self.cached_project {
                        let new_z = (p.editor.zoom_pixels_per_second / 1.35)
                            .clamp(timeline::MIN_ZOOM, timeline::MAX_ZOOM);
                        self.exec(Command::SetZoom {
                            pixels_per_second: new_z,
                        });
                    }
                    ui.close();
                }
                ui.separator();
                ui.checkbox(&mut self.timeline_state.snap, "Snap to Edges & Playhead");
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(p) = &self.cached_project {
                    let tip = self
                        .cached_project_dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default();
                    ui.label(
                        egui::RichText::new(format!(
                            "📁 {}  ({}×{} @ {}fps)",
                            p.name,
                            p.settings.width,
                            p.settings.height,
                            p.settings.fps.display(),
                        ))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    )
                    .on_hover_text(tip);
                }
            });
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.label(
                egui::RichText::new("CutPilot")
                    .strong()
                    .size(14.5)
                    .color(egui::Color32::from_rgb(130, 185, 255)),
            );
            ui.separator();

            if ui
                .button(egui::RichText::new("＋ Import Media…").strong())
                .on_hover_text("Pick videos, audio (WAV/MP3), or images (Ctrl+I, or drag & drop anywhere)")
                .clicked()
            {
                self.open_file_picker();
            }

            ui.separator();

            let has_project = self.cached_project.is_some();
            if ui
                .add_enabled(has_project, egui::Button::new("↶ Undo"))
                .on_hover_text("Undo (Ctrl+Z)")
                .clicked()
            {
                self.exec(Command::Undo {});
            }
            if ui
                .add_enabled(has_project, egui::Button::new("↷ Redo"))
                .on_hover_text("Redo (Ctrl+Shift+Z)")
                .clicked()
            {
                self.exec(Command::Redo {});
            }

            ui.separator();

            // Prominent MCP Agent button!
            self.mcp_panel.top_bar_button(ui, &self.ipc_stats);

            // Right side: proxy indicator + compact status.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(p) = self.media_cache.any_proxy_building() {
                    ui.label(
                        egui::RichText::new(format!("⚡ GPU Proxy {:.0}%", p * 100.0))
                            .small()
                            .color(egui::Color32::from_rgb(120, 215, 255)),
                    );
                    ui.separator();
                }
                ui.label(
                    egui::RichText::new(&self.status)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });

        // Drag-and-drop import.
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
            self.report(format!("Importing {n} dropped file(s)…"));
            self.import_files(new_paths, ui.ctx().clone());
        }
    }

    fn show_new_project_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.new_project_dialog.take() else {
            return;
        };

        let mut keep_open = true;
        let mut create_now = false;
        let mut change_folder = false;

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            keep_open = false;
        }

        let dummy_project = Project::new("tmp", cutpilot_core::ProjectSettings::default());
        let (w, h) = dialog.resolution.resolve(&dummy_project, 1920, 1080);
        let stem = filepicker::sanitize_project_folder_stem(&dialog.name);
        let target_dir = dialog.parent_dir.join(format!("{stem}.cutpilot"));

        egui::Window::new("New Project")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .fixed_size(egui::vec2(440.0, 0.0))
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Project Name").strong());
                let name_edit = ui.add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .hint_text("e.g. Summer Vlog")
                        .desired_width(ui.available_width()),
                );
                if dialog.focus_name {
                    name_edit.request_focus();
                    dialog.focus_name = false;
                }
                if name_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create_now = true;
                }

                ui.add_space(10.0);
                ui.label(egui::RichText::new("Location").strong());
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(dialog.parent_dir.display().to_string())
                            .monospace()
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Browse…").clicked() {
                            change_folder = true;
                        }
                    });
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Resolution");
                    let res_choices = [
                        ExportResolution::Fhd1080,
                        ExportResolution::Uhd4K,
                        ExportResolution::Qhd1440,
                        ExportResolution::Hd720,
                        ExportResolution::Vertical1080,
                        ExportResolution::Square1080,
                    ];
                    let current_label = dialog.resolution.label(&dummy_project);
                    egui::ComboBox::from_id_salt("new_proj_res")
                        .selected_text(current_label)
                        .show_ui(ui, |ui| {
                            for preset in res_choices {
                                let label = preset.label(&dummy_project);
                                ui.selectable_value(&mut dialog.resolution, preset, label);
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("Frame Rate");
                    let fps_label = NEW_PROJECT_FPS_OPTIONS
                        .get(dialog.fps_idx)
                        .map(|f| f.0)
                        .unwrap_or("30 fps");
                    egui::ComboBox::from_id_salt("new_proj_fps")
                        .selected_text(fps_label)
                        .show_ui(ui, |ui| {
                            for (idx, (label, _, _)) in NEW_PROJECT_FPS_OPTIONS.iter().enumerate() {
                                ui.selectable_value(&mut dialog.fps_idx, idx, *label);
                            }
                        });
                });

                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("Will create: {}", target_dir.display()))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );

                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_create = !dialog.name.trim().is_empty();
                    let create_btn = egui::Button::new(
                        egui::RichText::new("Create Project")
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(46, 118, 220));
                    if ui.add_enabled(can_create, create_btn).clicked() {
                        create_now = true;
                    }
                    if ui.button("Cancel").clicked() {
                        keep_open = false;
                    }
                });
            });

        if change_folder {
            self.new_project_dialog = Some(dialog);
            self.open_folder_picker(FolderPickerPurpose::NewProjectLocation);
            return;
        }

        if create_now && !dialog.name.trim().is_empty() {
            let (_, fps_num, fps_den) = NEW_PROJECT_FPS_OPTIONS
                .get(dialog.fps_idx)
                .copied()
                .unwrap_or(("30 fps", 30, 1));
            self.exec(Command::CreateProject {
                name: dialog.name.trim().to_string(),
                path: target_dir,
                width: Some(w),
                height: Some(h),
                fps_num: Some(fps_num),
                fps_den: Some(fps_den),
            });
            self.playback.invalidate_cache();
            return;
        }

        if keep_open {
            self.new_project_dialog = Some(dialog);
        }
    }

    fn media_bin(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Media Bin");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("＋ Browse…").clicked() {
                    self.open_file_picker();
                }
            });
        });

        let row_w = ui.available_width();
        ui.allocate_ui_with_layout(
            egui::vec2(row_w, 22.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                if ui.small_button("Add").clicked() {
                    let text = self.media_path_field.trim().to_string();
                    if !text.is_empty() {
                        let path = PathBuf::from(text);
                        if path.is_file() {
                            self.media_path_field.clear();
                            self.report(format!("Importing {}…", path.display()));
                            self.import_files(vec![path], ui.ctx().clone());
                        } else {
                            self.report(format!("Not a file: {}", path.display()));
                        }
                    }
                }
                let field = &mut self.media_path_field;
                ui.add(
                    egui::TextEdit::singleline(field)
                        .hint_text("paste media path…")
                        .desired_width((ui.available_width() - 1.0).max(40.0)),
                );
            },
        );

        ui.separator();

        let Some(project) = self.cached_project.clone() else {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(
                    "Drop video, audio (WAV/MP3), or image files anywhere or press “Import Media…” — CutPilot auto-creates a project automatically.",
                )
                .small()
                .color(ui.visuals().weak_text_color()),
            );
            return;
        };

        if project.assets.is_empty() {
            ui.label(
                egui::RichText::new("No media imported yet.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        }

        let ctx = ui.ctx().clone();
        let audio_tracks: Vec<(Uuid, String, bool)> = project
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .map(|t| (t.id, t.name.clone(), t.locked))
            .collect();
        let playhead = project.editor.playhead_ticks;

        for asset in &project.assets {
            let selected = project
                .editor
                .selection
                .iter()
                .any(|s| matches!(s, Selection::Asset(id) if *id == asset.id));

            egui::Frame::new()
                .fill(if selected {
                    egui::Color32::from_rgb(42, 56, 78)
                } else {
                    egui::Color32::from_rgb(28, 31, 38)
                })
                .corner_radius(5.0)
                .inner_margin(egui::Margin::symmetric(6, 6))
                .stroke(egui::Stroke::new(
                    1.0,
                    if selected {
                        egui::Color32::from_rgb(100, 165, 255)
                    } else {
                        egui::Color32::from_gray(46)
                    },
                ))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // Visual preview tile (video/image thumbnail or live audio mini-waveform),
                        // draggable directly onto any compatible timeline track.
                        let tile_resp = if asset.kind == AssetKind::Audio {
                            let (tile_rect, resp) = ui.allocate_exact_size(
                                egui::vec2(64.0, 36.0),
                                egui::Sense::click_and_drag(),
                            );
                            draw_audio_bin_tile(
                                ui.painter(),
                                tile_rect,
                                asset,
                                self.media_cache.waveforms().get(&asset.id).map(|v| v.as_slice()),
                            );
                            Some(resp)
                        } else if let Some(thumb) = self.media_cache.thumbnail(&asset.id) {
                            let (tile_rect, resp) = ui.allocate_exact_size(
                                egui::vec2(64.0, 36.0),
                                egui::Sense::click_and_drag(),
                            );
                            ui.painter().image(
                                thumb.id(),
                                tile_rect,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                egui::Color32::WHITE,
                            );
                            Some(resp)
                        } else {
                            None
                        };
                        if let Some(resp) = tile_resp {
                            if resp.drag_started() {
                                self.dragging_asset = Some(asset.id);
                            }
                            if resp.clicked() {
                                self.exec(Command::SetSelection {
                                    selection: vec![Selection::Asset(asset.id)],
                                });
                            }
                            resp.on_hover_text(
                                "Drag onto a timeline track to insert",
                            );
                        }

                        ui.vertical(|ui| {
                            let name_resp = ui
                                .add(
                                    egui::Button::selectable(
                                        selected,
                                        egui::RichText::new(&asset.name).strong().small(),
                                    )
                                    .sense(egui::Sense::click_and_drag()),
                                )
                                .on_hover_text("Click to select · drag onto a timeline track");
                            if name_resp.drag_started() {
                                self.dragging_asset = Some(asset.id);
                            }
                            if name_resp.clicked() {
                                self.exec(Command::SetSelection {
                                    selection: vec![Selection::Asset(asset.id)],
                                });
                            }
                            let meta = format_asset_meta(asset);
                            ui.label(
                                egui::RichText::new(meta)
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        });
                    });

                    ui.horizontal(|ui| {
                        // Proxy status badge.
                        match self.media_cache.proxy_state(&asset.id) {
                            Some(ProxyState::Ready(_)) => {
                                ui.label(
                                    egui::RichText::new("⚡ Proxy")
                                        .small()
                                        .color(egui::Color32::from_rgb(95, 215, 145)),
                                );
                            }
                            Some(ProxyState::Building(p)) => {
                                ui.label(
                                    egui::RichText::new(format!("⏳ {:.0}%", p * 100.0))
                                        .small()
                                        .color(egui::Color32::from_rgb(120, 200, 255)),
                                );
                            }
                            _ => {}
                        }
                        if project.transcript(&asset.id).is_some() {
                            ui.label(
                                egui::RichText::new("🎙 Text")
                                    .small()
                                    .color(egui::Color32::from_rgb(245, 210, 110)),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let has_audio = asset.kind == AssetKind::Audio
                                || asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false);
                            if has_audio
                                && ui
                                    .small_button("🎙")
                                    .on_hover_text("Transcribe audio with built-in Whisper")
                                    .clicked()
                            {
                                self.transcribe_asset(asset.id);
                            }
                            if has_audio
                                && ui
                                    .small_button("Cut Silence")
                                    .on_hover_text("Auto-detect and ripple-cut silent gaps")
                                    .clicked()
                            {
                                self.cut_silence_for_asset(asset.id, ctx.clone());
                            }

                            if asset.kind == AssetKind::Audio {
                                ui.menu_button("▾", |ui| {
                                    for (tid, tname, locked) in &audio_tracks {
                                        if ui
                                            .add_enabled(
                                                !*locked,
                                                egui::Button::new(format!(
                                                    "Add to {tname} at Playhead"
                                                )),
                                            )
                                            .clicked()
                                        {
                                            self.insert_audio_on_track(asset, *tid, playhead);
                                            ui.close();
                                        }
                                        let end = track_end_ticks(&project, tid);
                                        if ui
                                            .add_enabled(
                                                !*locked,
                                                egui::Button::new(format!("Append to {tname}")),
                                            )
                                            .clicked()
                                        {
                                            self.insert_audio_on_track(asset, *tid, end);
                                            ui.close();
                                        }
                                    }
                                    ui.separator();
                                    if ui.button("＋ New Audio Track at Playhead").clicked() {
                                        self.insert_audio_on_new_track(asset, playhead);
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button("🗑 Remove from Media Bin").clicked() {
                                        self.exec(Command::RemoveAsset { asset_id: asset.id });
                                        ui.close();
                                    }
                                });
                                if ui
                                    .small_button("＋ Audio")
                                    .on_hover_text(
                                        "Add audio clip to an audio track at the playhead (creates a new audio track if busy)",
                                    )
                                    .clicked()
                                {
                                    self.place_audio_at_playhead(&project, asset);
                                }
                            } else {
                                ui.menu_button("▾", |ui| {
                                    if ui.button("Append to Timeline").clicked() {
                                        self.place_asset_on_timeline(&project, asset);
                                        ui.close();
                                    }
                                    if ui.button("🗑 Remove from Media Bin").clicked() {
                                        self.exec(Command::RemoveAsset { asset_id: asset.id });
                                        ui.close();
                                    }
                                });
                                if ui
                                    .small_button("＋ Place")
                                    .on_hover_text("Append asset to timeline")
                                    .clicked()
                                {
                                    self.place_asset_on_timeline(&project, asset);
                                }
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
    }

    fn insert_audio_on_track(
        &mut self,
        asset: &cutpilot_core::Asset,
        track_id: Uuid,
        timeline_start: Ticks,
    ) {
        let duration = asset.editable_duration();
        if duration <= 0 {
            self.report(format!("error: `{}` has zero duration", asset.name));
            return;
        }
        self.exec(Command::InsertClip {
            asset_id: asset.id,
            track_id,
            timeline_start: timeline_start.max(0),
            source_in: 0,
            source_out: duration,
            link_group: None,
        });
    }

    fn insert_audio_on_new_track(&mut self, asset: &cutpilot_core::Asset, timeline_start: Ticks) {
        let duration = asset.editable_duration();
        if duration <= 0 {
            self.report(format!("error: `{}` has zero duration", asset.name));
            return;
        }
        if self.playback.is_playing() {
            self.playback.stop_playback();
        }
        let Ok(mut engine) = self.engine.lock() else {
            return;
        };
        let track_id = match engine.execute(Command::AddTrack {
            track_kind: "audio".into(),
            name: None,
        }) {
            Ok(outcome) => match outcome.data["track_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                Some(id) => id,
                None => {
                    self.report("error: failed to create audio track");
                    return;
                }
            },
            Err(e) => {
                self.report(format!("error: {e}"));
                return;
            }
        };
        match engine.execute(Command::InsertClip {
            asset_id: asset.id,
            track_id,
            timeline_start: timeline_start.max(0),
            source_in: 0,
            source_out: duration,
            link_group: None,
        }) {
            Ok(outcome) => {
                self.report(outcome.message);
                self.cached_project = engine.project().cloned();
                self.cached_project_dir = engine.project_dir().map(std::path::Path::to_path_buf);
                if let Some(p) = &self.cached_project {
                    self.last_revision = p.revision;
                }
            }
            Err(e) => self.report(format!("error: {e}")),
        }
    }

    /// Place an audio asset at the current playhead onto the first unlocked
    /// audio track without an overlap; if all unlocked audio tracks are busy
    /// at the playhead (or none exist), creates a fresh audio track (`A2`, …).
    fn place_audio_at_playhead(&mut self, project: &Project, asset: &cutpilot_core::Asset) {
        let duration = asset.editable_duration();
        if duration <= 0 {
            self.report(format!("error: `{}` has zero duration", asset.name));
            return;
        }
        let start = project.editor.playhead_ticks.max(0);
        let end = start + duration;
        let free_track = project
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio && !t.locked)
            .find(|t| {
                !project
                    .clips
                    .iter()
                    .any(|c| c.track_id == t.id && start < c.timeline_end() && c.timeline_start < end)
            })
            .map(|t| t.id);

        if let Some(tid) = free_track {
            self.insert_audio_on_track(asset, tid, start);
        } else {
            self.insert_audio_on_new_track(asset, start);
        }
    }

    fn place_asset_on_timeline(&mut self, project: &Project, asset: &cutpilot_core::Asset) {
        let duration = asset.editable_duration();
        let end = project.timeline_end();
        if asset.kind == AssetKind::Audio {
            self.place_audio_at_playhead(project, asset);
            return;
        }
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
        if let Some(v_track) = base {
            let group = if asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false)
                && asset.kind == AssetKind::Video
            {
                Some(Uuid::new_v4())
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

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let Some(project) = self.cached_project.clone() else {
            ui.heading("Inspector");
            ui.label(
                egui::RichText::new("No project open.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        };

        // ---- Clip Inspector ----
        ui.heading("Inspector");
        let selected_clip = project.editor.selection.iter().find_map(|s| match s {
            Selection::Clip(id) => project.clip(id).cloned(),
            _ => None,
        });

        match selected_clip {
            None => {
                ui.label(
                    egui::RichText::new("Click a clip on the timeline to inspect or trim it.")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
            Some(clip) => {
                let asset_name = project
                    .asset(&clip.asset_id)
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                let is_overlay = project
                    .base_video_track()
                    .map(|b| b.id != clip.track_id)
                    .unwrap_or(false)
                    && project
                        .track(&clip.track_id)
                        .map(|t| t.kind == TrackKind::Video)
                        .unwrap_or(false);

                ui.label(egui::RichText::new(asset_name).strong());
                let dur_label = if (clip.effective_speed() - 1.0).abs() > 0.01 {
                    format!(
                        "Duration: {:.2}s ({:.2}× speed)",
                        cutpilot_core::secs_from_ticks(clip.timeline_duration()),
                        clip.effective_speed()
                    )
                } else {
                    format!(
                        "Duration: {:.2}s",
                        cutpilot_core::secs_from_ticks(clip.timeline_duration())
                    )
                };
                ui.label(
                    egui::RichText::new(dur_label)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(4.0);

                if let Some(new_start) =
                    timeline::seconds_field(ui, "Timeline start", clip.timeline_start)
                {
                    self.exec(Command::MoveClip {
                        clip_id: clip.id,
                        timeline_start: new_start,
                        track_id: None,
                    });
                }
                if let Some(new_in) = timeline::seconds_field(ui, "Source in", clip.source_in) {
                    if new_in < clip.source_out {
                        self.exec(Command::TrimClip {
                            clip_id: clip.id,
                            timeline_start: None,
                            source_in: Some(new_in),
                            source_out: None,
                        });
                    }
                }
                if let Some(new_out) = timeline::seconds_field(ui, "Source out", clip.source_out) {
                    if new_out > clip.source_in {
                        self.exec(Command::TrimClip {
                            clip_id: clip.id,
                            timeline_start: None,
                            source_in: None,
                            source_out: Some(new_out),
                        });
                    }
                }

                ui.add_space(4.0);
                let playhead = project.editor.playhead_ticks;
                ui.horizontal_wrapped(|ui| {
                    let can_split =
                        playhead > clip.timeline_start && playhead < clip.timeline_end();
                    if ui
                        .add_enabled(can_split, egui::Button::new("✂ Split"))
                        .clicked()
                    {
                        self.exec(Command::SplitClip {
                            clip_id: clip.id,
                            at: playhead,
                        });
                    }
                    if ui
                        .button("⧉ Duplicate")
                        .on_hover_text("Duplicate clip (Ctrl+D)")
                        .clicked()
                    {
                        self.exec(Command::DuplicateClip {
                            clip_id: clip.id,
                            timeline_start: None,
                        });
                    }
                    if clip.link_group.is_some()
                        && ui
                            .button("⛓ Unlink")
                            .on_hover_text("Detach audio & video to edit independently")
                            .clicked()
                    {
                        self.exec(Command::UnlinkClip { clip_id: clip.id });
                    }
                    if ui.button("🗑 Delete").clicked() {
                        self.exec(Command::RemoveClip { clip_id: clip.id });
                    }
                    if ui.button("⏪ Ripple Del").clicked() {
                        self.ripple_delete_clip(clip.id);
                    }
                });

                let is_audio_clip = project
                    .track(&clip.track_id)
                    .map(|t| t.kind == TrackKind::Audio)
                    .unwrap_or(false);

                ui.horizontal_wrapped(|ui| {
                    if is_audio_clip {
                        let fade_dur = (TICKS_PER_SEC / 2).min(clip.timeline_duration());
                        if ui.small_button("Fade In 0.5s").clicked() {
                            self.exec(Command::UpdateClip {
                                clip_id: clip.id,
                                volume: None,
                                opacity: None,
                                scale: None,
                                x_offset: None,
                                y_offset: None,
                                fade_in_ticks: Some(fade_dur),
                                fade_out_ticks: None,
                                rotation_deg: None,
                                flip_h: None,
                                flip_v: None,
                                crop: None,
                            });
                        }
                        if ui.small_button("Fade Out 0.5s").clicked() {
                            self.exec(Command::UpdateClip {
                                clip_id: clip.id,
                                volume: None,
                                opacity: None,
                                scale: None,
                                x_offset: None,
                                y_offset: None,
                                fade_in_ticks: None,
                                fade_out_ticks: Some(fade_dur),
                                rotation_deg: None,
                                flip_h: None,
                                flip_v: None,
                                crop: None,
                            });
                        }
                    } else {
                        if ui.small_button("Fade In 0.5s").clicked() {
                            self.exec(Command::AddTransition {
                                at: clip.timeline_start,
                                duration: TICKS_PER_SEC / 2,
                                transition_kind: None,
                                color: Some("#000000".into()),
                            });
                        }
                        if ui.small_button("Fade Out 0.5s").clicked() {
                            self.exec(Command::AddTransition {
                                at: clip.timeline_end(),
                                duration: TICKS_PER_SEC / 2,
                                transition_kind: None,
                                color: Some("#000000".into()),
                            });
                        }
                    }
                    if ui.small_button("Cut Silence").clicked() {
                        self.cut_silence_for_asset(clip.asset_id, ui.ctx().clone());
                    }
                    if ui
                        .small_button("🎙 Transcribe")
                        .on_hover_text("Extract speech transcript with built-in Whisper")
                        .clicked()
                    {
                        self.transcribe_asset(clip.asset_id);
                    }
                });

                // ---- Speed ----
                ui.add_space(4.0);
                let mut speed = clip.effective_speed();
                ui.horizontal(|ui| {
                    ui.label("Speed");
                    if ui
                        .add(egui::Slider::new(&mut speed, 0.25..=4.0).suffix("×"))
                        .changed()
                    {
                        self.exec(Command::SetClipSpeed {
                            clip_id: clip.id,
                            speed,
                        });
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for (lbl, val) in [("0.5×", 0.5_f32), ("1×", 1.0), ("1.5×", 1.5), ("2×", 2.0)]
                    {
                        let active = (clip.effective_speed() - val).abs() < 0.02;
                        if ui.add(egui::Button::new(lbl).small().selected(active)).clicked() {
                            self.exec(Command::SetClipSpeed {
                                clip_id: clip.id,
                                speed: val,
                            });
                        }
                    }
                });

                if is_audio_clip {
                    let audio_tracks: Vec<(Uuid, String)> = project
                        .tracks
                        .iter()
                        .filter(|t| t.kind == TrackKind::Audio && !t.locked)
                        .map(|t| (t.id, t.name.clone()))
                        .collect();
                    if audio_tracks.len() > 1 {
                        let cur_name = project
                            .track(&clip.track_id)
                            .map(|t| t.name.clone())
                            .unwrap_or_else(|| "Audio".into());
                        let mut target_track = clip.track_id;
                        ui.horizontal(|ui| {
                            ui.label("Audio Track");
                            egui::ComboBox::from_id_salt("clip_audio_track")
                                .selected_text(cur_name)
                                .show_ui(ui, |ui| {
                                    for (tid, tname) in &audio_tracks {
                                        ui.selectable_value(&mut target_track, *tid, tname);
                                    }
                                });
                        });
                        if target_track != clip.track_id {
                            self.exec(Command::MoveClip {
                                clip_id: clip.id,
                                timeline_start: clip.timeline_start,
                                track_id: Some(target_track),
                            });
                        }
                    }
                    if let Some(new_fi) =
                        timeline::seconds_field(ui, "Fade in", clip.fade_in_ticks)
                    {
                        let clamped = new_fi.clamp(0, clip.timeline_duration());
                        self.exec(Command::UpdateClip {
                            clip_id: clip.id,
                            volume: None,
                            opacity: None,
                            scale: None,
                            x_offset: None,
                            y_offset: None,
                            fade_in_ticks: Some(clamped),
                            fade_out_ticks: None,
                            rotation_deg: None,
                            flip_h: None,
                            flip_v: None,
                            crop: None,
                        });
                    }
                    if let Some(new_fo) =
                        timeline::seconds_field(ui, "Fade out", clip.fade_out_ticks)
                    {
                        let clamped = new_fo.clamp(0, clip.timeline_duration());
                        self.exec(Command::UpdateClip {
                            clip_id: clip.id,
                            volume: None,
                            opacity: None,
                            scale: None,
                            x_offset: None,
                            y_offset: None,
                            fade_in_ticks: None,
                            fade_out_ticks: Some(clamped),
                            rotation_deg: None,
                            flip_h: None,
                            flip_v: None,
                            crop: None,
                        });
                    }
                }

                ui.add_space(4.0);
                let mut volume = clip.volume;
                ui.horizontal(|ui| {
                    ui.label("Volume");
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
                            rotation_deg: None,
                            flip_h: None,
                            flip_v: None,
                            crop: None,
                        });
                    }
                });

                if !is_audio_clip {
                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("Transform (Zoom / Pan / Crop)")
                        .default_open(true)
                        .show(ui, |ui| {
                            if is_overlay {
                                let mut opacity = clip.opacity;
                                ui.horizontal(|ui| {
                                    ui.label("Opacity");
                                    if ui.add(egui::Slider::new(&mut opacity, 0.0..=1.0)).changed()
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
                                            rotation_deg: None,
                                            flip_h: None,
                                            flip_v: None,
                                            crop: None,
                                        });
                                    }
                                });
                            }
                            let mut scale = clip.scale;
                            ui.horizontal(|ui| {
                                ui.label("Zoom");
                                if ui
                                    .add(egui::Slider::new(&mut scale, 0.1..=4.0).suffix("×"))
                                    .changed()
                                {
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: None,
                                        scale: Some(scale),
                                        x_offset: None,
                                        y_offset: None,
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: None,
                                        flip_h: None,
                                        flip_v: None,
                                        crop: None,
                                    });
                                }
                            });
                            let mut x_off = clip.x_offset;
                            let mut y_off = clip.y_offset;
                            let half_w = project.settings.width as f32;
                            let half_h = project.settings.height as f32;
                            ui.horizontal(|ui| {
                                ui.label("Pan X");
                                if ui
                                    .add(egui::Slider::new(&mut x_off, -half_w..=half_w).suffix("px"))
                                    .changed()
                                {
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: None,
                                        scale: None,
                                        x_offset: Some(x_off),
                                        y_offset: None,
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: None,
                                        flip_h: None,
                                        flip_v: None,
                                        crop: None,
                                    });
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("Pan Y");
                                if ui
                                    .add(egui::Slider::new(&mut y_off, -half_h..=half_h).suffix("px"))
                                    .changed()
                                {
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: None,
                                        scale: None,
                                        x_offset: None,
                                        y_offset: Some(y_off),
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: None,
                                        flip_h: None,
                                        flip_v: None,
                                        crop: None,
                                    });
                                }
                            });
                            let mut rot = clip.rotation_deg;
                            ui.horizontal(|ui| {
                                ui.label("Rotate");
                                if ui
                                    .add(egui::Slider::new(&mut rot, -180.0..=180.0).suffix("°"))
                                    .changed()
                                {
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: None,
                                        scale: None,
                                        x_offset: None,
                                        y_offset: None,
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: Some(rot),
                                        flip_h: None,
                                        flip_v: None,
                                        crop: None,
                                    });
                                }
                            });
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add(egui::Button::new("⇄ Flip H").small().selected(clip.flip_h))
                                    .clicked()
                                {
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: None,
                                        scale: None,
                                        x_offset: None,
                                        y_offset: None,
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: None,
                                        flip_h: Some(!clip.flip_h),
                                        flip_v: None,
                                        crop: None,
                                    });
                                }
                                if ui
                                    .add(egui::Button::new("⇅ Flip V").small().selected(clip.flip_v))
                                    .clicked()
                                {
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: None,
                                        scale: None,
                                        x_offset: None,
                                        y_offset: None,
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: None,
                                        flip_h: None,
                                        flip_v: Some(!clip.flip_v),
                                        crop: None,
                                    });
                                }
                                if ui.small_button("↻ +90°").clicked() {
                                    let mut next = clip.rotation_deg + 90.0;
                                    if next > 180.0 {
                                        next -= 360.0;
                                    }
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: None,
                                        scale: None,
                                        x_offset: None,
                                        y_offset: None,
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: Some(next),
                                        flip_h: None,
                                        flip_v: None,
                                        crop: None,
                                    });
                                }
                                if ui.small_button("Reset").clicked() {
                                    self.exec(Command::UpdateClip {
                                        clip_id: clip.id,
                                        volume: None,
                                        opacity: Some(1.0),
                                        scale: Some(1.0),
                                        x_offset: Some(0.0),
                                        y_offset: Some(0.0),
                                        fade_in_ticks: None,
                                        fade_out_ticks: None,
                                        rotation_deg: Some(0.0),
                                        flip_h: Some(false),
                                        flip_v: Some(false),
                                        crop: Some(cutpilot_core::CropRect::default()),
                                    });
                                }
                            });
                            let mut crop = clip.crop;
                            let mut crop_changed = false;
                            ui.horizontal(|ui| {
                                ui.label("Crop");
                                ui.label("L");
                                crop_changed |= ui
                                    .add(
                                        egui::DragValue::new(&mut crop.left)
                                            .speed(0.005)
                                            .range(0.0..=0.45),
                                    )
                                    .changed();
                                ui.label("R");
                                crop_changed |= ui
                                    .add(
                                        egui::DragValue::new(&mut crop.right)
                                            .speed(0.005)
                                            .range(0.0..=0.45),
                                    )
                                    .changed();
                                ui.label("T");
                                crop_changed |= ui
                                    .add(
                                        egui::DragValue::new(&mut crop.top)
                                            .speed(0.005)
                                            .range(0.0..=0.45),
                                    )
                                    .changed();
                                ui.label("B");
                                crop_changed |= ui
                                    .add(
                                        egui::DragValue::new(&mut crop.bottom)
                                            .speed(0.005)
                                            .range(0.0..=0.45),
                                    )
                                    .changed();
                            });
                            if crop_changed {
                                self.exec(Command::UpdateClip {
                                    clip_id: clip.id,
                                    volume: None,
                                    opacity: None,
                                    scale: None,
                                    x_offset: None,
                                    y_offset: None,
                                    fade_in_ticks: None,
                                    fade_out_ticks: None,
                                    rotation_deg: None,
                                    flip_h: None,
                                    flip_v: None,
                                    crop: Some(crop),
                                });
                            }
                        });

                    egui::CollapsingHeader::new("🔍 Zoom & Pan (Ken Burns)")
                        .default_open(clip.motion.is_some())
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                for (lbl, preset) in [
                                    ("Off", "none"),
                                    ("Zoom In", "zoom_in"),
                                    ("Zoom Out", "zoom_out"),
                                    ("Punch In", "punch_in"),
                                    ("Pan →", "pan_right"),
                                    ("Pan ←", "pan_left"),
                                ] {
                                    let is_off = preset == "none" && clip.motion.is_none();
                                    if ui
                                        .add(egui::Button::new(lbl).small().selected(is_off))
                                        .clicked()
                                    {
                                        if let Ok(m) = cutpilot_core::ClipMotion::preset(preset) {
                                            self.exec(Command::SetClipMotion {
                                                clip_id: clip.id,
                                                motion: m,
                                            });
                                        }
                                    }
                                }
                            });
                            if let Some(mut m) = clip.motion.clone() {
                                let mut changed = false;
                                ui.horizontal(|ui| {
                                    ui.label("Start Zoom");
                                    changed |= ui
                                        .add(
                                            egui::Slider::new(&mut m.start_scale, 1.0..=4.0)
                                                .suffix("×"),
                                        )
                                        .changed();
                                });
                                ui.horizontal(|ui| {
                                    ui.label("End Zoom");
                                    changed |= ui
                                        .add(
                                            egui::Slider::new(&mut m.end_scale, 1.0..=4.0)
                                                .suffix("×"),
                                        )
                                        .changed();
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Start Pan");
                                    ui.label("X");
                                    changed |= ui
                                        .add(
                                            egui::DragValue::new(&mut m.start_pan_x)
                                                .speed(0.02)
                                                .range(-1.0..=1.0),
                                        )
                                        .changed();
                                    ui.label("Y");
                                    changed |= ui
                                        .add(
                                            egui::DragValue::new(&mut m.start_pan_y)
                                                .speed(0.02)
                                                .range(-1.0..=1.0),
                                        )
                                        .changed();
                                });
                                ui.horizontal(|ui| {
                                    ui.label("End Pan");
                                    ui.label("X");
                                    changed |= ui
                                        .add(
                                            egui::DragValue::new(&mut m.end_pan_x)
                                                .speed(0.02)
                                                .range(-1.0..=1.0),
                                        )
                                        .changed();
                                    ui.label("Y");
                                    changed |= ui
                                        .add(
                                            egui::DragValue::new(&mut m.end_pan_y)
                                                .speed(0.02)
                                                .range(-1.0..=1.0),
                                        )
                                        .changed();
                                });
                                let prev_easing = m.easing;
                                ui.horizontal(|ui| {
                                    ui.label("Easing");
                                    egui::ComboBox::from_id_salt("clip_motion_easing")
                                        .selected_text(m.easing.label())
                                        .show_ui(ui, |ui| {
                                            for e in [
                                                cutpilot_core::Easing::EaseInOut,
                                                cutpilot_core::Easing::Linear,
                                                cutpilot_core::Easing::EaseIn,
                                                cutpilot_core::Easing::EaseOut,
                                            ] {
                                                ui.selectable_value(&mut m.easing, e, e.label());
                                            }
                                        });
                                });
                                if m.easing != prev_easing {
                                    changed = true;
                                }
                                if changed {
                                    self.exec(Command::SetClipMotion {
                                        clip_id: clip.id,
                                        motion: Some(m),
                                    });
                                }
                            }
                        });

                    egui::CollapsingHeader::new("🎨 Color & Filters")
                        .default_open(!clip.color.is_neutral())
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                if ui.small_button("Reset").clicked() {
                                    self.exec(Command::SetClipColor {
                                        clip_id: clip.id,
                                        color: cutpilot_core::ColorGrade::default(),
                                    });
                                }
                                if ui.small_button("B&W").clicked() {
                                    let mut c = clip.color;
                                    c.saturation = 0.0;
                                    c.contrast = 1.15;
                                    self.exec(Command::SetClipColor {
                                        clip_id: clip.id,
                                        color: c,
                                    });
                                }
                                if ui.small_button("Vivid").clicked() {
                                    let mut c = clip.color;
                                    c.contrast = 1.15;
                                    c.saturation = 1.3;
                                    self.exec(Command::SetClipColor {
                                        clip_id: clip.id,
                                        color: c,
                                    });
                                }
                                if ui.small_button("Vignette").clicked() {
                                    let mut c = clip.color;
                                    c.vignette = 0.5;
                                    self.exec(Command::SetClipColor {
                                        clip_id: clip.id,
                                        color: c,
                                    });
                                }
                            });
                            let mut c = clip.color;
                            let mut changed = false;
                            ui.horizontal(|ui| {
                                ui.label("Bright");
                                changed |= ui
                                    .add(egui::Slider::new(&mut c.brightness, -0.5..=0.5))
                                    .changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("Contrast");
                                changed |= ui
                                    .add(egui::Slider::new(&mut c.contrast, 0.2..=2.5))
                                    .changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("Sat");
                                changed |= ui
                                    .add(egui::Slider::new(&mut c.saturation, 0.0..=2.5))
                                    .changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("Gamma");
                                changed |= ui
                                    .add(egui::Slider::new(&mut c.gamma, 0.2..=2.5))
                                    .changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("Blur");
                                changed |= ui
                                    .add(egui::Slider::new(&mut c.blur, 0.0..=15.0))
                                    .changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("Vignette");
                                changed |= ui
                                    .add(egui::Slider::new(&mut c.vignette, 0.0..=1.0))
                                    .changed();
                            });
                            if changed {
                                self.exec(Command::SetClipColor {
                                    clip_id: clip.id,
                                    color: c,
                                });
                            }
                        });
                }
            }
        }

        // ---- Timeline Markers Section ----
        if !project.markers.is_empty() {
            ui.separator();
            ui.horizontal(|ui| {
                ui.heading("Markers");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("＋ At Playhead").clicked() {
                        let label = format!("Marker {}", project.markers.len() + 1);
                        self.exec(Command::AddMarker {
                            at: project.editor.playhead_ticks,
                            label,
                            color: None,
                        });
                    }
                });
            });
            for m in &project.markers {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            false,
                            format!(
                                "🚩 {}  {}",
                                cutpilot_core::ticks::format_seconds(m.at),
                                m.label
                            ),
                        )
                        .clicked()
                    {
                        self.seek_to(m.at);
                    }
                    if ui.small_button("✕").clicked() {
                        self.exec(Command::RemoveMarker { marker_id: m.id });
                    }
                });
            }
        }

        // ---- Whisper Transcript Section ----
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Transcript (Whisper)");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let target_asset = project
                    .editor
                    .selection
                    .iter()
                    .find_map(|s| match s {
                        Selection::Clip(cid) => project.clip(cid).map(|c| c.asset_id),
                        Selection::Asset(aid) => Some(*aid),
                        _ => None,
                    })
                    .or_else(|| {
                        project
                            .assets
                            .iter()
                            .find(|a| {
                                a.kind == AssetKind::Audio
                                    || a.probe.as_ref().map(|p| p.has_audio).unwrap_or(false)
                            })
                            .map(|a| a.id)
                    });
                if ui
                    .add_enabled(target_asset.is_some(), egui::Button::new("🎙 Transcribe").small())
                    .on_hover_text("Run built-in Whisper speech-to-text (auto-downloads model on first use)")
                    .clicked()
                {
                    if let Some(aid) = target_asset {
                        self.transcribe_asset(aid);
                    }
                }
            });
        });

        ui.horizontal(|ui| {
            ui.label("Model");
            egui::ComboBox::from_id_salt("whisper_model")
                .selected_text(&self.whisper_model)
                .show_ui(ui, |ui| {
                    for &m in cutpilot_media::whisper::SUPPORTED_MODELS {
                        ui.selectable_value(&mut self.whisper_model, m.to_string(), m);
                    }
                });
            if !project.transcripts.is_empty()
                && ui
                    .small_button("💬 → Captions")
                    .on_hover_text("Generate timed timeline captions from transcript")
                    .clicked()
            {
                self.exec(Command::TranscriptToCaptions {
                    asset_id: None,
                    replace: Some(false),
                });
            }
        });

        let timeline_segs = project.timeline_transcript_segments(None);
        if !timeline_segs.is_empty() {
            ui.add(
                egui::TextEdit::singleline(&mut self.transcript_search)
                    .hint_text("search transcript…")
                    .desired_width(ui.available_width()),
            );
            let q = self.transcript_search.trim().to_ascii_lowercase();
            egui::ScrollArea::vertical()
                .id_salt("transcript_scroll")
                .max_height(160.0)
                .show(ui, |ui| {
                    for seg in &timeline_segs {
                        if !q.is_empty() && !seg.text.to_ascii_lowercase().contains(&q) {
                            continue;
                        }
                        let active = project.editor.playhead_ticks >= seg.timeline_start
                            && project.editor.playhead_ticks < seg.timeline_end;
                        let label = format!(
                            "[{}] {}",
                            cutpilot_core::ticks::format_seconds(seg.timeline_start),
                            seg.text
                        );
                        if ui.selectable_label(active, label).clicked() {
                            self.seek_to(seg.timeline_start);
                        }
                    }
                });
        } else if project.transcripts.is_empty() {
            ui.label(
                egui::RichText::new("Click “🎙 Transcribe” to extract a searchable transcript (also exposed to your MCP agent).")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        }

        // ---- Captions Section ----
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Captions");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let playhead = project.editor.playhead_ticks;
                if ui.small_button("＋ At Playhead").clicked() {
                    self.exec(Command::AddCaption {
                        start: playhead,
                        end: playhead + TICKS_PER_SEC * 2,
                        text: "New caption".into(),
                    });
                }
            });
        });

        let selected_caption = project.editor.selection.iter().find_map(|s| match s {
            Selection::Caption(id) => project.captions.iter().find(|c| c.id == *id).cloned(),
            _ => None,
        });
        if let Some(cap) = selected_caption {
            if self.caption_edit_id != Some(cap.id) {
                self.caption_edit_id = Some(cap.id);
                self.caption_edit_text = cap.text.clone();
            }
            let row_w = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(row_w, 22.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if ui.small_button("Apply").clicked()
                        && !self.caption_edit_text.trim().is_empty()
                    {
                        self.exec(Command::UpdateCaption {
                            caption_id: cap.id,
                            start: None,
                            end: None,
                            text: Some(self.caption_edit_text.clone()),
                        });
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.caption_edit_text)
                            .desired_width((ui.available_width() - 1.0).max(40.0)),
                    );
                },
            );
            if let Some(ns) = timeline::seconds_field(ui, "Start", cap.start) {
                if ns < cap.end {
                    self.exec(Command::UpdateCaption {
                        caption_id: cap.id,
                        start: Some(ns),
                        end: None,
                        text: None,
                    });
                }
            }
            if let Some(ne) = timeline::seconds_field(ui, "End", cap.end) {
                if ne > cap.start {
                    self.exec(Command::UpdateCaption {
                        caption_id: cap.id,
                        start: None,
                        end: Some(ne),
                        text: None,
                    });
                }
            }
        }

        for caption in &project.captions {
            let selected = project
                .editor
                .selection
                .iter()
                .any(|s| matches!(s, Selection::Caption(id) if *id == caption.id));
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(
                        selected,
                        format!(
                            "{}–{}  {}",
                            cutpilot_core::ticks::format_seconds(caption.start),
                            cutpilot_core::ticks::format_seconds(caption.end),
                            caption.text
                        ),
                    )
                    .clicked()
                {
                    self.exec(Command::SetSelection {
                        selection: vec![Selection::Caption(caption.id)],
                    });
                    self.seek_to(caption.start);
                }
                if ui.small_button("✕").clicked() {
                    self.exec(Command::RemoveCaption {
                        caption_id: caption.id,
                    });
                }
            });
        }

        // ---- GPU Export Section ----
        ui.separator();
        ui.heading("Export MP4");

        let gpu = cutpilot_media::gpu::capabilities();
        let gpu_badge = if let Some(name) = &gpu.gpu_name {
            if gpu.cuda_filters && gpu.nvenc_h264 {
                (
                    egui::Color32::from_rgb(80, 220, 130),
                    format!("⚡ {name} · Zero-Copy CUDA + NVENC"),
                )
            } else if gpu.nvenc_h264 {
                (
                    egui::Color32::from_rgb(80, 220, 130),
                    format!("⚡ {name} · NVENC Ready"),
                )
            } else {
                (egui::Color32::from_gray(165), format!("{name} (CPU encode)"))
            }
        } else {
            (egui::Color32::from_gray(165), "Software CPU encoder".into())
        };
        ui.label(egui::RichText::new(gpu_badge.1).small().color(gpu_badge.0));

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.use_nvenc, "NVIDIA GPU (NVENC)");
        });

        ui.horizontal(|ui| {
            ui.label("Resolution");
            egui::ComboBox::from_id_salt("export_resolution")
                .selected_text(self.export_resolution.label(&project))
                .show_ui(ui, |ui| {
                    for &preset in ExportResolution::ALL {
                        let label = preset.label(&project);
                        ui.selectable_value(&mut self.export_resolution, preset, label);
                    }
                });
        });

        if self.export_resolution == ExportResolution::Custom {
            ui.horizontal(|ui| {
                ui.label("Size");
                let mut w = self.export_custom_width;
                let mut h = self.export_custom_height;
                ui.add(
                    egui::DragValue::new(&mut w)
                        .speed(2)
                        .range(16..=7680)
                        .suffix(" px"),
                );
                ui.label("×");
                ui.add(
                    egui::DragValue::new(&mut h)
                        .speed(2)
                        .range(16..=7680)
                        .suffix(" px"),
                );
                self.export_custom_width = (w.clamp(16, 7680)) & !1;
                self.export_custom_height = (h.clamp(16, 7680)) & !1;
            });
        }

        let (export_w, export_h) = self.export_resolution.resolve(
            &project,
            self.export_custom_width,
            self.export_custom_height,
        );

        ui.horizontal(|ui| {
            ui.label("Codec");
            egui::ComboBox::from_id_salt("export_codec")
                .selected_text(match self.export_codec.as_str() {
                    "hevc" => "HEVC (H.265)",
                    "av1" => "AV1",
                    _ => "H.264 (MP4)",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.export_codec, "h264".into(), "H.264 (MP4)");
                    let hevc_label = if gpu.nvenc_hevc {
                        "HEVC (H.265 · NVENC)"
                    } else {
                        "HEVC (H.265)"
                    };
                    ui.selectable_value(&mut self.export_codec, "hevc".into(), hevc_label);
                    if gpu.nvenc_av1 {
                        ui.selectable_value(&mut self.export_codec, "av1".into(), "AV1 (NVENC)");
                    }
                });
        });

        ui.horizontal(|ui| {
            ui.label("Preset");
            egui::ComboBox::from_id_salt("export_preset")
                .selected_text(match self.export_preset.as_str() {
                    "fast" => "Fast (Max GPU Speed)",
                    "quality" => "High Quality",
                    _ => "Balanced",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.export_preset,
                        "fast".into(),
                        "Fast (Max GPU Speed)",
                    );
                    ui.selectable_value(&mut self.export_preset, "balanced".into(), "Balanced");
                    ui.selectable_value(&mut self.export_preset, "quality".into(), "High Quality");
                });
        });

        ui.horizontal(|ui| {
            ui.label("Bitrate");
            let mut bitrate = self.export_bitrate;
            ui.add(
                egui::DragValue::new(&mut bitrate)
                    .speed(1.0)
                    .range(0.0..=120.0)
                    .suffix(" Mbps"),
            );
            self.export_bitrate = bitrate;
            if self.export_bitrate == 0.0 {
                ui.label(
                    egui::RichText::new(format!(
                        "(Auto: {:.0}M)",
                        cutpilot_media::export::default_bitrate_mbps_for(
                            export_w,
                            export_h,
                            project.settings.fps,
                        )
                    ))
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
            }
        });

        ui.add_space(4.0);
        let export_btn = egui::Button::new(
            egui::RichText::new(format!("🚀 Export Video ({export_w}×{export_h})"))
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(egui::Color32::from_rgb(46, 118, 220))
        .min_size(egui::vec2(ui.available_width(), 30.0));
        if ui.add(export_btn).clicked() {
            let (width, height) = if self.export_resolution == ExportResolution::MatchProject {
                (None, None)
            } else {
                (Some(export_w), Some(export_h))
            };
            self.exec(Command::StartRender {
                output_path: None,
                video_bitrate_mbps: if self.export_bitrate > 0.0 {
                    Some(self.export_bitrate)
                } else {
                    None
                },
                use_nvenc: Some(self.use_nvenc),
                codec: Some(self.export_codec.clone()),
                preset: Some(self.export_preset.clone()),
                width,
                height,
            });
        }

        // ---- Active / Recent Jobs with Progress Bar & Cancel ----
        let jobs_json = self.jobs.try_lock().ok().map(|reg| reg.list_json(5));
        if let Some(jobs) = jobs_json {
            if let Some(list) = jobs["jobs"].as_array() {
                if !list.is_empty() {
                    ui.separator();
                    ui.heading("Jobs");
                    for job in list {
                        let job_id_str = job["job_id"].as_str().unwrap_or("");
                        let state = &job["state"];
                        let status = state["status"].as_str().unwrap_or("?");
                        let label = job["label"].as_str().unwrap_or("");
                        match status {
                            "running" => {
                                let p = (state["progress"].as_f64().unwrap_or(0.0) as f32)
                                    .clamp(0.0, 1.0);
                                let bar_text = if p >= 0.995 {
                                    "Finalizing…".to_string()
                                } else {
                                    format!("{:.0}%", p * 100.0)
                                };
                                // Allocate a fixed-width single-line row and place Cancel first
                                // right-to-left so the ProgressBar takes only the remaining space
                                // and can never cause an egui sidebar expansion feedback loop.
                                let row_w = ui.available_width();
                                ui.allocate_ui_with_layout(
                                    egui::vec2(row_w, 20.0),
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("Cancel").clicked() {
                                            if let Ok(id) = Uuid::parse_str(job_id_str) {
                                                self.exec(Command::CancelJob { job_id: id });
                                            }
                                        }
                                        ui.add(
                                            egui::ProgressBar::new(p)
                                                .text(bar_text)
                                                .desired_width((ui.available_width() - 1.0).max(40.0)),
                                        );
                                    },
                                );
                                ui.add(egui::Label::new(egui::RichText::new(label).small()).wrap());
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                            }
                            "done" => {
                                let msg = state["message"].as_str().unwrap_or(label);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("✓ {msg}"))
                                            .small()
                                            .color(egui::Color32::from_rgb(95, 215, 145)),
                                    )
                                    .wrap(),
                                );
                            }
                            "failed" => {
                                let err = state["error"].as_str().unwrap_or("?");
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("✗ {label}: {err}"))
                                            .small()
                                            .color(egui::Color32::from_rgb(245, 110, 110)),
                                    )
                                    .wrap(),
                                );
                            }
                            _ => {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(format!("– {label}")).small())
                                        .wrap(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui, project: &Project) {
        let playhead = project.editor.playhead_ticks;
        let end = project.timeline_end().max(TICKS_PER_SEC);
        let ctx = ui.ctx().clone();

        // Update scrub frame when paused.
        let proxies = self.media_cache.ready_proxy_map();
        self.playback.update_scrub_preview(
            project,
            &proxies,
            self.timeline_state.is_scrubbing(),
            &ctx,
        );

        // ---- Video Viewport (properly aspect-fitted in BOTH width and height!) ----
        let transport_reserve = 68.0;
        let avail = ui.available_size();
        let viewport_size = egui::vec2(avail.x, (avail.y - transport_reserve).max(140.0));

        let (vp_rect, vp_resp) = ui.allocate_exact_size(viewport_size, egui::Sense::click());
        if vp_resp.clicked() {
            self.toggle_play(&ctx);
        }

        let painter = ui.painter_at(vp_rect);
        painter.rect_filled(vp_rect, 6.0, egui::Color32::from_rgb(12, 13, 17));

        let mut image_rect = vp_rect;
        if let Some(tex) = self.playback.texture() {
            let tex_sz = tex.size_vec2();
            let scale = (vp_rect.width() / tex_sz.x.max(1.0))
                .min(vp_rect.height() / tex_sz.y.max(1.0));
            let fit_size = tex_sz * scale;
            image_rect = egui::Rect::from_center_size(vp_rect.center(), fit_size);
            painter.image(
                tex.id(),
                image_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            painter.text(
                vp_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Press Space to Play · Scrub timeline to preview",
                egui::FontId::proportional(15.0),
                ui.visuals().weak_text_color(),
            );
        }

        // Resolution / playback badge in top-right corner of viewport.
        let badge = format!(
            "{}×{} · {} fps{}",
            project.settings.width,
            project.settings.height,
            project.settings.fps.display(),
            if self.playback.is_playing() { " · ▶ PLAYING" } else { "" }
        );
        painter.text(
            vp_rect.right_top() + egui::vec2(-10.0, 8.0),
            egui::Align2::RIGHT_TOP,
            badge,
            egui::FontId::monospace(11.0),
            egui::Color32::from_rgba_unmultiplied(220, 225, 240, 180),
        );

        // Live Caption Overlay (scaled cleanly to preview rect).
        let active_caps = project.captions_at(playhead);
        if !active_caps.is_empty() {
            let cap_painter = ui.painter_at(image_rect);
            let scale_y = image_rect.height() / project.settings.height.max(1) as f32;
            let font_px = (project.caption_style.font_size as f32 * scale_y).clamp(13.0, 56.0);
            let margin_px = (project.caption_style.margin_v as f32 * scale_y).clamp(14.0, 100.0);
            let font = egui::FontId::proportional(font_px);
            for caption in active_caps {
                let y = match project.caption_style.alignment {
                    7..=9 => image_rect.top() + margin_px,
                    4..=6 => image_rect.center().y,
                    _ => image_rect.bottom() - margin_px,
                };
                let pos = egui::pos2(image_rect.center().x, y);
                for d in [
                    egui::vec2(-1.5, 0.0),
                    egui::vec2(1.5, 0.0),
                    egui::vec2(0.0, -1.5),
                    egui::vec2(0.0, 1.5),
                ] {
                    cap_painter.text(
                        pos + d,
                        egui::Align2::CENTER_CENTER,
                        &caption.text,
                        font.clone(),
                        egui::Color32::BLACK,
                    );
                }
                cap_painter.text(
                    pos,
                    egui::Align2::CENTER_CENTER,
                    &caption.text,
                    font.clone(),
                    egui::Color32::WHITE,
                );
            }
        }

        ui.add_space(6.0);

        // ---- Scrub Slider ----
        let mut playhead_secs = cutpilot_core::secs_from_ticks(playhead) as f32;
        let end_secs = cutpilot_core::secs_from_ticks(end) as f32;
        let slider = egui::Slider::new(&mut playhead_secs, 0.0..=end_secs.max(0.1))
            .show_value(false);
        ui.spacing_mut().slider_width = (ui.available_width() - 12.0).max(120.0);
        if ui.add(slider).changed() {
            self.seek_to(cutpilot_core::ticks_from_secs(playhead_secs as f64));
        }

        // ---- Transport Bar ----
        ui.horizontal(|ui| {
            let frame = project.settings.fps.frame_duration_ticks();
            if ui.button("⏮").on_hover_text("Jump to start (Home)").clicked() {
                self.seek_to(0);
            }
            if ui.button("◀ 1f").on_hover_text("Previous frame (Left)").clicked() {
                self.seek_to((playhead - frame).max(0));
            }

            let playing = self.playback.is_playing();
            let play_label = if playing { "⏸ Pause" } else { "▶ Play" };
            let play_btn = egui::Button::new(
                egui::RichText::new(play_label)
                    .strong()
                    .color(egui::Color32::WHITE),
            )
            .fill(if playing {
                egui::Color32::from_rgb(205, 80, 65)
            } else {
                egui::Color32::from_rgb(46, 130, 225)
            })
            .min_size(egui::vec2(82.0, 24.0));
            if ui.add(play_btn).on_hover_text("Play / Pause (Space)").clicked() {
                self.toggle_play(&ctx);
            }

            if ui.button("1f ▶").on_hover_text("Next frame (Right)").clicked() {
                self.seek_to((playhead + frame).min(end));
            }
            if ui.button("⏭").on_hover_text("Jump to end (End)").clicked() {
                self.seek_to(end);
            }

            ui.separator();

            ui.label(
                egui::RichText::new(format!(
                    "{}  /  {}",
                    timeline::format_timecode(playhead, project.settings.fps),
                    timeline::format_timecode(end, project.settings.fps),
                ))
                .monospace()
                .strong(),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().slider_width = 72.0;
                ui.add(
                    egui::Slider::new(&mut self.playback.master_volume, 0.0..=1.5)
                        .show_value(false),
                )
                .on_hover_text("Preview master volume");
                let mute_icon = if self.playback.muted { "🔇" } else { "🔊" };
                if ui
                    .button(mute_icon)
                    .on_hover_text("Mute / unmute preview audio")
                    .clicked()
                {
                    self.playback.muted = !self.playback.muted;
                }
            });
        });
    }
}

impl eframe::App for CutPilotApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // 1. Adopt status messages from background threads.
        if let Ok(cell) = self.status_cell.try_lock() {
            if cell.seq != self.status_seq {
                self.status_seq = cell.seq;
                self.status = cell.text.clone();
            }
        }

        // 2. Refresh non-blocking project snapshot from the Engine.
        self.refresh_snapshot();

        // 3. Poll file picker worker.
        let picked = self.picker_rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(PickerOutcome::Cancelled),
        });
        if let Some(outcome) = picked {
            self.picker_rx = None;
            match outcome {
                PickerOutcome::Files(paths) => {
                    let n = paths.len();
                    self.report(format!("Importing {n} file(s)…"));
                    self.import_files(paths, ctx.clone());
                }
                PickerOutcome::Cancelled => {}
                PickerOutcome::Failed(msg) => self.report(msg),
            }
        }

        // 3b. Poll folder picker worker (File -> Open / New / Save As).
        let picked_folder = self.folder_picker_rx.as_ref().and_then(|(purpose, rx)| {
            match rx.try_recv() {
                Ok(outcome) => Some((*purpose, outcome)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some((*purpose, FolderPickerOutcome::Cancelled))
                }
            }
        });
        if let Some((purpose, outcome)) = picked_folder {
            self.folder_picker_rx = None;
            match outcome {
                FolderPickerOutcome::Folder(dir) => match purpose {
                    FolderPickerPurpose::OpenProject => {
                        self.exec(Command::OpenProject { path: dir });
                        self.playback.invalidate_cache();
                    }
                    FolderPickerPurpose::NewProjectLocation => {
                        if let Some(existing) = &mut self.new_project_dialog {
                            existing.parent_dir = dir;
                            existing.focus_name = true;
                        } else {
                            self.new_project_dialog = Some(NewProjectDialog {
                                parent_dir: dir,
                                name: String::new(),
                                resolution: ExportResolution::Fhd1080,
                                fps_idx: 2, // 30 fps
                                focus_name: true,
                            });
                        }
                    }
                    FolderPickerPurpose::SaveProjectAs => {
                        self.exec(Command::SaveProject { path: Some(dir) });
                    }
                },
                FolderPickerOutcome::Cancelled => {
                    self.report("Folder selection cancelled.");
                }
                FolderPickerOutcome::Failed(msg) => self.report(msg),
            }
        }

        // 4. Sync media cache (GPU proxies, waveforms, thumbnails) + drain completions.
        if let Some(project) = &self.cached_project {
            self.media_cache
                .sync_project(project, self.cached_project_dir.as_deref(), &ctx);
        }
        if self.media_cache.poll(&ctx) {
            // When a new proxy completes, allow scrub cache to upgrade to it smoothly.
            self.playback.invalidate_cache();
        }

        // 5. Tick real-time A/V playback or drain coalesced scrub frames.
        if let Some(new_ticks) = self.playback.tick_playback(&ctx) {
            // Update playhead smoothly without blocking.
            if let Ok(mut eng) = self.engine.try_lock() {
                let _ = eng.execute(Command::SetPlayhead { ticks: new_ticks });
                if let Some(p) = &mut self.cached_project {
                    p.editor.playhead_ticks = new_ticks;
                }
            }
        } else {
            self.playback.poll_scrub(&ctx);
        }

        // 5b. Check whether a HyperFrames render (e.g. triggered by an MCP agent)
        //     is active. Reads `self.jobs` directly so it never blocks on `engine.lock()`.
        let active_hf_jobs = self
            .jobs
            .lock()
            .map(|reg| reg.active_jobs_of_kind("hyperframes"))
            .unwrap_or_default();
        let hyperframes_busy = !active_hf_jobs.is_empty();
        if hyperframes_busy {
            if self.playback.is_playing() {
                self.playback.stop_playback();
            }
            self.dragging_asset = None;
            self.was_hyperframes_busy = true;
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        } else if self.was_hyperframes_busy {
            self.was_hyperframes_busy = false;
            self.refresh_snapshot();
            self.playback.invalidate_cache();
        }

        // 6. Global keyboard shortcuts (locked while HyperFrames renders).
        if !hyperframes_busy {
            self.handle_shortcuts(&ctx);
        }

        // 7a. Standard Menu Bar (File / Edit / View).
        egui::Panel::top("menu_bar")
            .exact_size(26.0)
            .resizable(false)
            .show(ui, |ui| self.menu_bar(ui));

        // 7b. Compact Top Toolbar.
        egui::Panel::top("top_bar")
            .exact_size(38.0)
            .resizable(false)
            .show(ui, |ui| self.top_bar(ui));

        // 8. Left Media Bin.
        egui::Panel::left("media_bin")
            .default_size(260.0)
            .min_size(200.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.media_bin(ui));
            });

        // 9. Right Inspector + Export.
        egui::Panel::right("inspector")
            .default_size(310.0)
            .min_size(240.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.inspector(ui));
            });

        // 10. Bottom Timeline Panel.
        egui::Panel::bottom("timeline_panel")
            .default_size(290.0)
            .min_size(190.0)
            .show(ui, |ui| {
                let Some(project) = self.cached_project.clone() else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(
                                "Timeline empty — press “Import Media…” or connect your MCP agent to begin.",
                            )
                            .color(ui.visuals().weak_text_color()),
                        );
                    });
                    return;
                };
                let panel_w = ui.available_width();
                let action = {
                    let filmstrips = self.media_cache.filmstrip_view();
                    timeline::draw_timeline(
                        ui,
                        &project,
                        &mut self.timeline_state,
                        self.media_cache.waveforms(),
                        &filmstrips,
                        self.dragging_asset,
                    )
                };
                if let Some((asset_id, track_id, at_ticks)) = action.drop_asset {
                    self.dragging_asset = None;
                    if let Some(asset) = project.asset(&asset_id) {
                        let dur = asset.editable_duration();
                        if dur > 0 {
                            self.exec(Command::InsertClip {
                                asset_id,
                                track_id,
                                timeline_start: at_ticks.max(0),
                                source_in: 0,
                                source_out: dur,
                                link_group: None,
                            });
                        }
                    }
                }
                if !action.filmstrip_requests.is_empty() {
                    self.media_cache.request_filmstrip_tiles(
                        &project,
                        action.filmstrip_requests,
                        &ctx,
                    );
                }
                if let Some(t) = action.seek {
                    self.seek_to(t);
                }
                if let Some(clip_id) = action.select_clip {
                    self.exec(Command::SetSelection {
                        selection: vec![Selection::Clip(clip_id)],
                    });
                }
                if let Some(cap_id) = action.select_caption {
                    self.exec(Command::SetSelection {
                        selection: vec![Selection::Caption(cap_id)],
                    });
                }
                if let Some((clip_id, new_start, track_id)) = action.move_clip {
                    self.exec(Command::MoveClip {
                        clip_id,
                        timeline_start: new_start,
                        track_id,
                    });
                }
                if let Some(trim) = action.trim_clip {
                    self.exec(Command::TrimClip {
                        clip_id: trim.clip_id,
                        timeline_start: trim.timeline_start,
                        source_in: trim.source_in,
                        source_out: trim.source_out,
                    });
                }
                if let Some((clip_id, at)) = action.split_clip {
                    self.exec(Command::SplitClip { clip_id, at });
                }
                if action.split_at_playhead {
                    self.split_at_playhead();
                }
                if let Some(cid) = action.remove_clip {
                    self.exec(Command::RemoveClip { clip_id: cid });
                }
                if let Some(cid) = action.ripple_delete_clip {
                    self.ripple_delete_clip(cid);
                }
                if let Some(cid) = action.duplicate_clip {
                    self.exec(Command::DuplicateClip {
                        clip_id: cid,
                        timeline_start: None,
                    });
                }
                if let Some(cid) = action.unlink_clip {
                    self.exec(Command::UnlinkClip { clip_id: cid });
                }
                if let Some(at) = action.add_marker_at {
                    let label = format!("Marker {}", project.markers.len() + 1);
                    self.exec(Command::AddMarker {
                        at,
                        label,
                        color: None,
                    });
                }
                if let Some(mid) = action.remove_marker {
                    self.exec(Command::RemoveMarker { marker_id: mid });
                }
                if let Some((s, e)) = action.ripple_cut_range {
                    self.exec(Command::RemoveRanges {
                        start: s,
                        end: e,
                        ripple: Some(true),
                        tracks: None,
                    });
                }
                if let Some(at) = action.add_fade_in {
                    self.exec(Command::AddTransition {
                        at,
                        duration: TICKS_PER_SEC / 2,
                        transition_kind: None,
                        color: Some("#000000".into()),
                    });
                }
                if let Some(at) = action.add_fade_out {
                    self.exec(Command::AddTransition {
                        at,
                        duration: TICKS_PER_SEC / 2,
                        transition_kind: None,
                        color: Some("#000000".into()),
                    });
                }
                if let Some((cid, dur)) = action.set_audio_fade_in {
                    self.exec(Command::UpdateClip {
                        clip_id: cid,
                        volume: None,
                        opacity: None,
                        scale: None,
                        x_offset: None,
                        y_offset: None,
                        fade_in_ticks: Some(dur),
                        fade_out_ticks: None,
                        rotation_deg: None,
                        flip_h: None,
                        flip_v: None,
                        crop: None,
                    });
                }
                if let Some((cid, dur)) = action.set_audio_fade_out {
                    self.exec(Command::UpdateClip {
                        clip_id: cid,
                        volume: None,
                        opacity: None,
                        scale: None,
                        x_offset: None,
                        y_offset: None,
                        fade_in_ticks: None,
                        fade_out_ticks: Some(dur),
                        rotation_deg: None,
                        flip_h: None,
                        flip_v: None,
                        crop: None,
                    });
                }
                if let Some(aid) = action.cut_silence_asset {
                    self.cut_silence_for_asset(aid, ctx.clone());
                }
                if let Some(aid) = action.transcribe_asset {
                    self.transcribe_asset(aid);
                }
                if let Some(z) = action.set_zoom {
                    self.exec(Command::SetZoom {
                        pixels_per_second: z,
                    });
                }
                if action.fit_timeline {
                    self.fit_timeline_zoom(panel_w);
                }
                if let Some(kind) = action.add_track {
                    self.exec(Command::AddTrack {
                        track_kind: match kind {
                            TrackKind::Video => "video".into(),
                            TrackKind::Audio => "audio".into(),
                        },
                        name: None,
                    });
                }
                if let Some((tid, to_index)) = action.move_track {
                    self.exec(Command::MoveTrack {
                        track_id: tid,
                        to_index,
                    });
                }
                if let Some((tid, muted)) = action.toggle_mute {
                    self.exec(Command::UpdateTrack {
                        track_id: tid,
                        name: None,
                        muted: Some(muted),
                        hidden: None,
                        locked: None,
                        volume: None,
                    });
                }
                if let Some((tid, hidden)) = action.toggle_hidden {
                    self.exec(Command::UpdateTrack {
                        track_id: tid,
                        name: None,
                        muted: None,
                        hidden: Some(hidden),
                        locked: None,
                        volume: None,
                    });
                }
                if let Some((tid, locked)) = action.toggle_locked {
                    self.exec(Command::UpdateTrack {
                        track_id: tid,
                        name: None,
                        muted: None,
                        hidden: None,
                        locked: Some(locked),
                        volume: None,
                    });
                }

                // Ctrl+Wheel zoom on timeline (unrestricted zoom-out down to MIN_ZOOM).
                let zoom_delta = ui.input(|i| i.zoom_delta());
                if (zoom_delta - 1.0).abs() > f32::EPSILON {
                    let z = project.editor.zoom_pixels_per_second;
                    let new_z = (z * zoom_delta).clamp(timeline::MIN_ZOOM, timeline::MAX_ZOOM);
                    if (new_z - z).abs() > (z * 1e-4).max(1e-7) {
                        self.exec(Command::SetZoom {
                            pixels_per_second: new_z,
                        });
                    }
                }
            });

        // 11. Central Video Preview Panel.
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(project) = self.cached_project.clone() else {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(
                            egui::RichText::new("CutPilot")
                                .size(30.0)
                                .strong()
                                .color(egui::Color32::from_rgb(130, 185, 255)),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(
                                "Fast GPU-Accelerated, MCP-First Video Editor",
                            )
                            .size(15.0)
                            .color(egui::Color32::from_gray(215)),
                        );
                        ui.add_space(18.0);
                        ui.horizontal(|ui| {
                            let avail = ui.available_width();
                            ui.add_space(((avail - 580.0) * 0.5).max(0.0));
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("＋ Import Media…")
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(egui::Color32::from_rgb(46, 118, 220))
                                    .min_size(egui::vec2(145.0, 36.0)),
                                )
                                .clicked()
                            {
                                self.open_file_picker();
                            }
                            ui.add_space(8.0);
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("📂 Open Project…").strong(),
                                    )
                                    .min_size(egui::vec2(130.0, 36.0)),
                                )
                                .on_hover_text("Browse for an existing CutPilot project folder (Ctrl+O)")
                                .clicked()
                            {
                                self.open_folder_picker(FolderPickerPurpose::OpenProject);
                            }
                            ui.add_space(8.0);
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("✨ New Project…").strong(),
                                    )
                                    .min_size(egui::vec2(125.0, 36.0)),
                                )
                                .on_hover_text("Pick a folder and name a new project (Ctrl+N)")
                                .clicked()
                            {
                                self.open_folder_picker(FolderPickerPurpose::NewProjectLocation);
                            }
                            ui.add_space(8.0);
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("● Connect MCP Agent").strong(),
                                    )
                                    .min_size(egui::vec2(156.0, 36.0)),
                                )
                                .clicked()
                            {
                                self.mcp_panel.open = true;
                            }
                        });
                        ui.add_space(16.0);
                        ui.label(
                            egui::RichText::new(
                                "Or drag & drop any video, audio (WAV/MP3), or image file into this window — a project is created automatically beside your media.",
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

        // 12. Floating badge when dragging an asset from the Media Bin onto the timeline.
        if let Some(asset_id) = self.dragging_asset {
            if let (Some(project), Some(pos)) = (
                &self.cached_project,
                ctx.input(|i| i.pointer.hover_pos().or(i.pointer.latest_pos())),
            ) {
                if let Some(asset) = project.asset(&asset_id) {
                    let fg = ctx.layer_painter(egui::LayerId::new(
                        egui::Order::Tooltip,
                        egui::Id::unique("media_bin_drag_badge"),
                    ));
                    let icon = match asset.kind {
                        AssetKind::Audio => "🎵",
                        AssetKind::Video => "🎬",
                        AssetKind::Image => "🖼",
                    };
                    let text = format!("{icon} {}", asset.name);
                    let galley = fg.layout_no_wrap(
                        text,
                        egui::FontId::proportional(12.0),
                        egui::Color32::WHITE,
                    );
                    let pill = egui::Rect::from_min_size(
                        pos + egui::vec2(12.0, 10.0),
                        galley.size() + egui::vec2(14.0, 8.0),
                    );
                    fg.rect_filled(pill, 5.0, egui::Color32::from_rgba_unmultiplied(24, 34, 48, 235));
                    fg.rect_stroke(
                        pill,
                        5.0,
                        egui::Stroke::new(
                            1.2,
                            if asset.kind == AssetKind::Audio {
                                egui::Color32::from_rgb(90, 220, 150)
                            } else {
                                egui::Color32::from_rgb(100, 175, 255)
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                    fg.galley(pill.min + egui::vec2(7.0, 4.0), galley, egui::Color32::WHITE);
                }
            }
            if ctx.input(|i| !i.pointer.primary_down()) {
                self.dragging_asset = None;
            }
        }

        // 13a. New Project modal dialog (shown after picking a folder).
        self.show_new_project_dialog(&ctx);

        // 13b. Connect MCP Agent modal window.
        self.mcp_panel
            .show(&ctx, &self.ipc_stats, self.cached_project_dir.as_deref());

        // 14. Modal spinner overlay locking the UI while HyperFrames renders.
        if hyperframes_busy {
            self.draw_hyperframes_lock_overlay(&ctx, &active_hf_jobs);
        }
    }
}

impl CutPilotApp {
    fn draw_hyperframes_lock_overlay(
        &mut self,
        ctx: &egui::Context,
        active_jobs: &[cutpilot_engine::ActiveJobSnapshot],
    ) {
        let screen = ctx.content_rect();
        egui::Area::new(egui::Id::unique("hyperframes_render_lock"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .interactable(true)
            .show(ctx, |ui| {
                // Full-screen translucent scrim that absorbs all pointer input
                // so the underlying editor stays cleanly locked during render.
                let (bg_rect, _blocker) =
                    ui.allocate_exact_size(screen.size(), egui::Sense::click_and_drag());
                ui.painter().rect_filled(
                    bg_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(8, 10, 15, 195),
                );

                let card_w = 430.0_f32.min((screen.width() - 32.0).max(260.0));
                let card_rect = egui::Rect::from_center_size(
                    bg_rect.center(),
                    egui::vec2(card_w, 220.0),
                );
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(card_rect)
                        .layout(egui::Layout::top_down(egui::Align::Center)),
                );
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(24, 28, 37))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(24, 22))
                    .stroke(egui::Stroke::new(
                        1.5,
                        egui::Color32::from_rgb(88, 148, 240),
                    ))
                    .show(&mut child, |ui| {
                        ui.set_width(card_w - 48.0);
                        ui.vertical_centered(|ui| {
                            ui.add(egui::Spinner::new().size(34.0));
                            ui.add_space(12.0);
                            let all_finalizing = active_jobs.iter().all(|j| j.finalizing);
                            let heading = if all_finalizing {
                                "Finalizing HyperFrames…"
                            } else {
                                "Rendering HyperFrames…"
                            };
                            ui.label(
                                egui::RichText::new(heading)
                                    .size(18.0)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                            ui.add_space(6.0);
                            for job in active_jobs {
                                let secs = job.elapsed_secs.round() as u64;
                                let mins = secs / 60;
                                let rem = secs % 60;
                                let state_txt = if job.finalizing {
                                    "inserting onto timeline"
                                } else {
                                    "rendering frames"
                                };
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}  ({mins:02}:{rem:02} · {state_txt})",
                                        job.label
                                    ))
                                    .small()
                                    .color(egui::Color32::from_rgb(160, 205, 255)),
                                );
                            }
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new(
                                    "The interface is locked while your MCP agent renders this HyperFrames animation and will unlock automatically when done.",
                                )
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                            if !all_finalizing {
                                ui.add_space(14.0);
                                if ui
                                    .button(
                                        egui::RichText::new("Cancel Render")
                                            .color(egui::Color32::from_rgb(245, 130, 130)),
                                    )
                                    .clicked()
                                {
                                    if let Ok(mut reg) = self.jobs.lock() {
                                        for job in active_jobs {
                                            if !job.finalizing {
                                                reg.cancel(&job.id);
                                            }
                                        }
                                    }
                                    self.report("Cancelling HyperFrames render…");
                                }
                            }
                        });
                    });
            });
    }
}

/// Apply a sleek, high-contrast dark NLE color palette.
fn apply_nle_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = egui::Color32::from_rgb(22, 24, 30);
    visuals.panel_fill = egui::Color32::from_rgb(20, 22, 28);
    visuals.extreme_bg_color = egui::Color32::from_rgb(13, 15, 19);
    visuals.faint_bg_color = egui::Color32::from_rgb(28, 31, 38);
    visuals.selection.bg_fill = egui::Color32::from_rgb(46, 110, 200);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(34, 38, 48);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(46, 52, 66);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(58, 72, 98);
    ctx.set_visuals(visuals);
}

fn track_end_ticks(project: &Project, track_id: &Uuid) -> Ticks {
    project
        .clips
        .iter()
        .filter(|c| &c.track_id == track_id)
        .map(|c| c.timeline_end())
        .max()
        .unwrap_or(0)
}

fn format_asset_meta(asset: &cutpilot_core::Asset) -> String {
    let dur = cutpilot_core::secs_from_ticks(asset.editable_duration());
    match (&asset.probe, asset.kind) {
        (Some(p), AssetKind::Video) => {
            let (w, h) = p.oriented_size();
            let fps = p
                .fps
                .map(|f| format!("{}fps", f.display()))
                .unwrap_or_default();
            format!("{w}×{h} {fps} · {dur:.1}s")
        }
        (Some(p), AssetKind::Audio) => {
            let ext = asset
                .path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_uppercase())
                .filter(|s| !s.is_empty());
            let fmt = ext
                .or_else(|| p.audio_codec.as_ref().map(|c| c.to_ascii_uppercase()))
                .unwrap_or_else(|| "AUDIO".into());
            if let Some(sr) = p.sample_rate {
                let khz = sr as f64 / 1000.0;
                format!("{fmt} · {khz:.1} kHz · {dur:.1}s")
            } else {
                format!("{fmt} · {dur:.1}s")
            }
        }
        (_, AssetKind::Video) => format!("video · {dur:.1}s"),
        (_, AssetKind::Audio) => format!("audio · {dur:.1}s"),
        (_, AssetKind::Image) => format!("image · {dur:.1}s"),
    }
}

fn draw_audio_bin_tile(
    painter: &egui::Painter,
    rect: egui::Rect,
    asset: &cutpilot_core::Asset,
    peaks: Option<&[cutpilot_media::waveform::Peak]>,
) {
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(20, 44, 34));
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(56, 128, 92)),
        egui::StrokeKind::Inside,
    );
    if let Some(peaks) = peaks.filter(|p| !p.is_empty()) {
        let mid_y = rect.center().y;
        let half_h = (rect.height() - 6.0) * 0.48;
        let inner_w = (rect.width() - 6.0).max(1.0);
        let bars = (inner_w / 2.0).floor().max(4.0) as usize;
        for b in 0..bars {
            let i0 = (b * peaks.len()) / bars;
            let i1 = (((b + 1) * peaks.len()) / bars).max(i0 + 1).min(peaks.len());
            let mut amp = 0.0_f32;
            for p in &peaks[i0..i1] {
                amp = amp.max(p.max.abs().max(p.min.abs()));
            }
            let h = (amp.clamp(0.0, 1.0).powf(0.7) * half_h).max(1.0);
            let x = rect.left() + 3.0 + b as f32 * 2.0 + 0.5;
            painter.line_segment(
                [egui::pos2(x, mid_y - h), egui::pos2(x, mid_y + h)],
                egui::Stroke::new(1.4, egui::Color32::from_rgb(100, 225, 165)),
            );
        }
    } else {
        let ext = asset
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "WAV".into());
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("♪ {ext}"),
            egui::FontId::monospace(10.5),
            egui::Color32::from_rgb(120, 235, 175),
        );
    }
}

/// Resolve canvas size and fps for auto-creating a project from imported files.
/// Audio-only files (e.g. `.wav`) report `(0, 0)` video dimensions and fall back
/// to a standard 1920×1080 @ 30fps canvas instead of clamping to 2×2.
fn resolve_auto_project_video_params(
    probes: &[cutpilot_media::ffprobe::ProbeResult],
) -> (u32, u32, u32, u32) {
    let visual = probes.iter().find(|p| {
        let (w, h) = p.probe.oriented_size();
        w > 0 && h > 0
    });
    let (w, h) = visual
        .map(|p| p.probe.oriented_size())
        .unwrap_or((1920, 1080));
    let even = |v: u32| v.clamp(2, 7680) & !1;
    let fps = visual
        .and_then(|p| p.probe.fps)
        .filter(|f| (1.0..=240.0).contains(&f.as_f64()));
    let (fps_num, fps_den) = fps.map(|f| (f.num, f.den)).unwrap_or((30, 1));
    (even(w), even(h), fps_num, fps_den)
}

fn auto_create_project_for(
    engine: &Arc<Mutex<Engine>>,
    status: &Arc<Mutex<StatusLine>>,
    paths: &[PathBuf],
) -> Result<(), String> {
    let first = paths
        .iter()
        .find(|p| p.is_file())
        .ok_or("none of the files exist")?;
    let probes: Vec<cutpilot_media::ffprobe::ProbeResult> = paths
        .iter()
        .filter(|p| p.is_file())
        .filter_map(|p| cutpilot_media::ffprobe::probe(p).ok())
        .collect();
    let (w, h, fps_num, fps_den) = resolve_auto_project_video_params(&probes);
    let parent = first
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let stem = first
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "CutPilot Project".into());
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
    let fps_display = cutpilot_core::Fps::new(fps_num, fps_den).display();
    report_to(
        status,
        format!(
            "Created project {} ({}×{} @ {}fps)",
            dir.display(),
            w,
            h,
            fps_display
        ),
    );
    Ok(())
}

fn unique_project_dir(parent: &std::path::Path, stem: &str) -> PathBuf {
    let base = parent.join(format!("{stem}.cutpilot"));
    if !base.exists() {
        return base;
    }
    for i in 2..100 {
        let candidate = parent.join(format!("{stem}-{i}.cutpilot"));
        if !candidate.exists() {
            return candidate;
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    parent.join(format!("{stem}-{stamp}.cutpilot"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutpilot_core::{Fps, MediaProbe};
    use cutpilot_media::ffprobe::ProbeResult;

    #[test]
    fn audio_only_wav_auto_project_defaults_to_1080p30() {
        let wav_probe = ProbeResult {
            kind: AssetKind::Audio,
            probe: MediaProbe {
                duration_ticks: TICKS_PER_SEC * 5,
                width: 0,
                height: 0,
                fps: None,
                has_audio: true,
                rotation_deg: 0,
                video_codec: None,
                audio_codec: Some("pcm_s16le".into()),
                sample_rate: Some(44_100),
            },
        };
        assert_eq!(
            resolve_auto_project_video_params(&[wav_probe]),
            (1920, 1080, 30, 1)
        );
    }

    #[test]
    fn mixed_wav_and_video_auto_project_uses_video_dimensions() {
        let wav_probe = ProbeResult {
            kind: AssetKind::Audio,
            probe: MediaProbe {
                duration_ticks: TICKS_PER_SEC * 5,
                width: 0,
                height: 0,
                fps: None,
                has_audio: true,
                rotation_deg: 0,
                video_codec: None,
                audio_codec: Some("pcm_s16le".into()),
                sample_rate: Some(48_000),
            },
        };
        let vid_probe = ProbeResult {
            kind: AssetKind::Video,
            probe: MediaProbe {
                duration_ticks: TICKS_PER_SEC * 10,
                width: 3840,
                height: 2160,
                fps: Some(Fps::from_int(60)),
                has_audio: true,
                rotation_deg: 0,
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                sample_rate: Some(48_000),
            },
        };
        assert_eq!(
            resolve_auto_project_video_params(&[wav_probe, vid_probe]),
            (3840, 2160, 60, 1)
        );
    }
}
