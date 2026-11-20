//! The engine: owns the open project, executes every registry command,
//! keeps snapshot history for undo, and manages background jobs.

use crate::{
    error::EngineError,
    jobs::{progress_sink, JobRegistry, JobState, PendingFinalize},
    preview, project_io,
};
use openprem_core::{
    edit, Asset, AssetKind, Clip, Command, Outcome, Project, ProjectSettings, Ticks, Track,
    TrackKind, SCHEMA_VERSION,
};
use openprem_media::{export, ffprobe, frames, hyperframes, silence, srt, waveform};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const HISTORY_LIMIT: usize = 100;
const JOBS_LIST_LIMIT: usize = 50;

struct Snapshot {
    project: Project,
    label: String,
}

struct ActiveProject {
    project: Project,
    /// Project directory (owns `project.json`).
    dir: PathBuf,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

/// The single-writer engine. Wrap in `Arc<Mutex<..>>` for thread access.
pub struct Engine {
    active: Option<ActiveProject>,
    jobs: Arc<Mutex<JobRegistry>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            active: None,
            jobs: Arc::new(Mutex::new(JobRegistry::new())),
        }
    }

    // ------------------------------------------------------------------
    // public API
    // ------------------------------------------------------------------

    /// True when a project is open.
    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    /// Current revision, or 0 with no project.
    pub fn revision(&self) -> u64 {
        self.active.as_ref().map(|a| a.project.revision).unwrap_or(0)
    }

    /// Execute one command against the engine.
    pub fn execute(&mut self, cmd: Command) -> Result<Outcome, EngineError> {
        // Finalize any completed HyperFrames jobs first so their inserts
        // land before reads/edits observe the project.
        self.poll_jobs()?;
        self.execute_inner(cmd)
    }

    /// Execute with optimistic concurrency: the caller states the revision
    /// it last saw; mismatched revisions fail without mutating.
    pub fn execute_checked(
        &mut self,
        cmd: Command,
        expected_revision: Option<u64>,
    ) -> Result<Outcome, EngineError> {
        if let Some(expected) = expected_revision {
            let actual = self.revision();
            if expected != actual {
                return Err(EngineError::Core(openprem_core::CoreError::RevisionMismatch {
                    expected,
                    actual,
                }));
            }
        }
        self.execute(cmd)
    }

    /// Apply finished HyperFrames jobs (import asset + insert clip).
    /// Called automatically by `execute`; the UI also calls it on a timer.
    pub fn poll_jobs(&mut self) -> Result<(), EngineError> {
        let pending: Vec<(Uuid, PendingFinalize)> = {
            let reg = self.jobs.lock().expect("job registry lock");
            reg.unfinished_finalizes()
        };
        for (job_id, action) in pending {
            if let PendingFinalize::ImportHyperFrames {
                asset_id,
                overlay,
                insert_at_ticks,
                duration_ticks,
                transparent,
            } = action
            {
                self.finalize_hyperframes(
                    job_id,
                    asset_id,
                    overlay,
                    insert_at_ticks,
                    duration_ticks,
                    transparent,
                )?;
            }
        }
        Ok(())
    }

    /// Read-only view for the UI without dispatching a command.
    pub fn project(&self) -> Option<&Project> {
        self.active.as_ref().map(|a| &a.project)
    }

    pub fn project_mut(&mut self) -> Option<&mut Project> {
        self.active.as_mut().map(|a| &mut a.project)
    }

    pub fn project_dir(&self) -> Option<&Path> {
        self.active.as_ref().map(|a| a.dir.as_path())
    }

    pub fn jobs(&self) -> Arc<Mutex<JobRegistry>> {
        self.jobs.clone()
    }

    /// Compute a preview frame (RGBA) at the playhead or a given tick.
    pub fn preview_rgba(
        &self,
        at_ticks: Option<Ticks>,
        max_width: u32,
    ) -> Result<frames::RgbaFrame, EngineError> {
        let active = self.active.as_ref().ok_or(EngineError::Core(
            openprem_core::CoreError::NoProject,
        ))?;
        let t = at_ticks.unwrap_or(active.project.editor.playhead_ticks);
        preview::composite_frame(&active.project, t, max_width)
    }

    // ------------------------------------------------------------------
    // dispatch
    // ------------------------------------------------------------------

    fn execute_inner(&mut self, cmd: Command) -> Result<Outcome, EngineError> {
        let kind = cmd.kind().to_string();
        let outcome = match &cmd {
            // ----- lifecycle -----
            Command::CreateProject {
                name,
                path,
                width,
                height,
                fps_num,
                fps_den,
            } => {
                let mut settings = ProjectSettings::default();
                if let Some(w) = width {
                    settings.width = *w;
                }
                if let Some(h) = height {
                    settings.height = *h;
                }
                if let (Some(n), Some(d)) = (fps_num, fps_den) {
                    settings.fps = openprem_core::commands::fps_from_parts(*n, *d)
                        .map_err(EngineError::Validation)?;
                } else {
                    settings.fps = openprem_core::Fps::from_int(30);
                }
                settings.fps.validate().map_err(EngineError::Validation)?;
                if width.unwrap_or(3840) % 2 != 0 || height.unwrap_or(2160) % 2 != 0 {
                    return Err(EngineError::Validation(
                        "width and height must be even numbers".into(),
                    ));
                }
                let project = Project::new(name.clone(), settings);
                let dir = project_io::create_new(path, &project)?;
                self.active = Some(ActiveProject {
                    project,
                    dir: dir.clone(),
                    undo: Vec::new(),
                    redo: Vec::new(),
                });
                let data = json!({
                    "project_dir": dir.display().to_string(),
                    "revision": self.revision(),
                    "settings": {
                        "width": self.active.as_ref().unwrap().project.settings.width,
                        "height": self.active.as_ref().unwrap().project.settings.height,
                        "fps": self.active.as_ref().unwrap().project.settings.fps.display(),
                    }
                });
                Ok(Outcome::new(self.revision(), &kind, data, "created project"))
            }
            Command::OpenProject { path } => {
                let project = project_io::load(path)?;
                let dir = project_io::project_dir(path);
                self.active = Some(ActiveProject {
                    project,
                    dir,
                    undo: Vec::new(),
                    redo: Vec::new(),
                });
                let p = &self.active.as_ref().unwrap().project;
                let data = json!({
                    "name": p.name,
                    "revision": p.revision,
                    "assets": p.assets.len(),
                    "clips": p.clips.len(),
                });
                Ok(Outcome::new(self.revision(), &kind, data, "opened project"))
            }
            Command::SaveProject { path } => {
                let active = self.require_active_mut()?;
                let dir = match path {
                    Some(new_dir) => {
                        let dir = project_io::project_dir(new_dir);
                        std::fs::create_dir_all(&dir)?;
                        active.dir = dir.clone();
                        dir
                    }
                    None => active.dir.clone(),
                };
                let file = project_io::project_file(&dir);
                project_io::write_atomic(&file, &active.project)?;
                let data = json!({"path": file.display().to_string()});
                Ok(Outcome::new(
                    self.revision(),
                    &kind,
                    data,
                    "saved project",
                ))
            }
            Command::CloseProject {} => {
                self.active = None;
                Ok(Outcome::new(
                    0,
                    &kind,
                    json!({"closed": true}),
                    "closed project",
                ))
            }

            // ----- pure edits (snapshot + apply + autosave) -----
            Command::UpdateProjectSettings { .. }
            | Command::AddTrack { .. }
            | Command::UpdateTrack { .. }
            | Command::MoveTrack { .. }
            | Command::RemoveTrack { .. }
            | Command::InsertClip { .. }
            | Command::MoveClip { .. }
            | Command::TrimClip { .. }
            | Command::SplitClip { .. }
            | Command::RemoveClip { .. }
            | Command::UpdateClip { .. }
            | Command::RemoveRanges { .. }
            | Command::AddTransition { .. }
            | Command::RemoveTransition { .. }
            | Command::SetCaptionStyle { .. }
            | Command::AddCaption { .. }
            | Command::UpdateCaption { .. }
            | Command::RemoveCaption { .. } => {
                let data = self.mutate(&cmd, &kind)?;
                Ok(Outcome::new(self.revision(), &kind, data, kind.clone()))
            }

            // ----- editor state (no undo snapshot) -----
            Command::SetPlayhead { .. }
            | Command::SetSelection { .. }
            | Command::SetZoom { .. } => {
                let active = self.require_active_mut()?;
                let data = edit::apply_editor_state(&mut active.project, &cmd)?;
                Ok(Outcome::new(self.revision(), &kind, data, kind.clone()))
            }

            // ----- reads -----
            Command::GetProject {} => {
                let active = self.require_active()?;
                let data = json!({
                    "project_dir": active.dir.display().to_string(),
                    "project": serde_json::to_value(&active.project)
                        .map_err(|e| EngineError::Validation(e.to_string()))?,
                });
                Ok(Outcome::new(self.revision(), &kind, data, "project document"))
            }
            Command::GetTimeline {} => {
                let active = self.require_active()?;
                let data = timeline_json(&active.project);
                Ok(Outcome::new(self.revision(), &kind, data, "timeline"))
            }
            Command::GetEditorState {} => {
                let active = self.require_active()?;
                let data = json!({
                    "playhead_ticks": active.project.editor.playhead_ticks,
                    "selection": active.project.editor.selection,
                    "zoom_pixels_per_second": active.project.editor.zoom_pixels_per_second,
                });
                Ok(Outcome::new(self.revision(), &kind, data, "editor state"))
            }
            Command::HistoryList {} => {
                let active = self.require_active()?;
                let undo: Vec<String> = active.undo.iter().map(|s| s.label.clone()).collect();
                let redo: Vec<String> = active.redo.iter().map(|s| s.label.clone()).collect();
                let data = json!({"undo": undo, "redo": redo});
                Ok(Outcome::new(self.revision(), &kind, data, "history"))
            }

            // ----- history -----
            Command::Undo {} => {
                let active = self.require_active_mut()?;
                let snapshot = active.undo.pop().ok_or_else(|| {
                    EngineError::Validation("nothing to undo".into())
                })?;
                let current = active.project.revision;
                active.redo.push(Snapshot {
                    project: active.project.clone(),
                    label: format!("redo:{}", snapshot.label),
                });
                active.project = snapshot.project;
                // Revisions are monotonic: undo itself is a new state.
                active.project.revision = current.saturating_add(1);
                Self::autosave(active)?;
                Ok(Outcome::new(
                    self.revision(),
                    &kind,
                    json!({"undone": true}),
                    "undid one step",
                ))
            }
            Command::Redo {} => {
                let active = self.require_active_mut()?;
                let snapshot = active.redo.pop().ok_or_else(|| {
                    EngineError::Validation("nothing to redo".into())
                })?;
                let current = active.project.revision;
                let label = snapshot.label.trim_start_matches("redo:").to_string();
                active.undo.push(Snapshot {
                    project: active.project.clone(),
                    label,
                });
                active.project = snapshot.project;
                active.project.revision = current.saturating_add(1);
                Self::autosave(active)?;
                Ok(Outcome::new(
                    self.revision(),
                    &kind,
                    json!({"redone": true}),
                    "redid one step",
                ))
            }

            // ----- media -----
            Command::ImportMedia { path, auto_place } => {
                let data = self.import_media(path, auto_place.unwrap_or(false))?;
                Ok(Outcome::new(self.revision(), &kind, data, "imported media"))
            }
            Command::RemoveAsset { asset_id } => {
                let active = self.require_active_mut()?;
                let used = active.project.clips.iter().any(|c| &c.asset_id == asset_id);
                if used {
                    return Err(EngineError::Validation(
                        "asset is used by timeline clips; remove those clips first".into(),
                    ));
                }
                let data = self.mutate(&cmd, &kind)?;
                Ok(Outcome::new(self.revision(), &kind, data, "removed asset"))
            }
            Command::MediaList {} => {
                let active = self.require_active()?;
                let assets: Vec<Value> = active
                    .project
                    .assets
                    .iter()
                    .map(asset_json)
                    .collect();
                let data = json!({"assets": assets});
                Ok(Outcome::new(self.revision(), &kind, data, "media bin"))
            }
            Command::ProbeMedia { asset_id } => {
                let active = self.require_active()?;
                let asset = require_asset(&active.project, asset_id)?;
                let probed = ffprobe::probe(&asset.path)?;
                let data = json!({
                    "asset_id": asset_id,
                    "probe": serde_json::to_value(&probed.probe)
                        .map_err(|e| EngineError::Validation(e.to_string()))?,
                });
                Ok(Outcome::new(self.revision(), &kind, data, "probed asset"))
            }
            Command::DetectSilence {
                asset_id,
                threshold_db,
                min_duration_secs,
                padding_secs,
            } => {
                let active = self.require_active()?;
                let asset = require_asset(&active.project, asset_id)?;
                let opts = silence::SilenceOptions {
                    threshold_db: threshold_db.unwrap_or(-35.0),
                    min_duration_secs: min_duration_secs.unwrap_or(0.6),
                    padding_secs: padding_secs.unwrap_or(0.15),
                };
                let duration = asset.editable_duration();
                let ranges = silence::detect_silence(&asset.path, duration, &opts)
                    .map_err(EngineError::Validation)?;
                let data = json!({
                    "asset_id": asset_id,
                    "threshold_db": opts.threshold_db,
                    "min_duration_secs": opts.min_duration_secs,
                    "padding_secs": opts.padding_secs,
                    "ranges": ranges.iter().map(|r| json!({
                        "start": r.start, "end": r.end,
                        "start_secs": openprem_core::secs_from_ticks(r.start),
                        "end_secs": openprem_core::secs_from_ticks(r.end),
                    })).collect::<Vec<_>>(),
                });
                Ok(Outcome::new(
                    self.revision(),
                    &kind,
                    data,
                    format!("found {} silent ranges", ranges.len()),
                ))
            }
            Command::MediaThumbnail {
                asset_id,
                at_secs,
                max_width,
            } => {
                let active = self.require_active()?;
                let asset = require_asset(&active.project, asset_id)?;
                let at = at_secs.unwrap_or(0.0);
                let jpeg = frames::thumbnail_jpeg(&asset.path, at, max_width.unwrap_or(640))?;
                Ok(Outcome::new(self.revision(), &kind, json!({}), "thumbnail")
                    .with_binary("image/jpeg", jpeg))
            }
            Command::ContactSheet { asset_id, count } => {
                let active = self.require_active()?;
                let asset = require_asset(&active.project, asset_id)?;
                let jpeg = frames::contact_sheet_jpeg(&asset.path, count.unwrap_or(8))?;
                Ok(Outcome::new(self.revision(), &kind, json!({}), "contact sheet")
                    .with_binary("image/jpeg", jpeg))
            }
            Command::Waveform { asset_id, buckets } => {
                let active = self.require_active()?;
                let asset = require_asset(&active.project, asset_id)?;
                let peaks = waveform::waveform_peaks(&asset.path, buckets.unwrap_or(512))
                    .map_err(EngineError::Validation)?;
                let data = json!({
                    "asset_id": asset_id,
                    "buckets": peaks.len(),
                    "peaks": peaks.iter().map(|p| json!({"min": p.min, "max": p.max})).collect::<Vec<_>>(),
                });
                Ok(Outcome::new(self.revision(), &kind, data, "waveform"))
            }

            // ----- captions I/O -----
            Command::ImportCaptions { path, offset_ticks } => {
                let text = std::fs::read_to_string(path)?;
                let captions = srt::parse_srt(&text, offset_ticks.unwrap_or(0))
                    .map_err(EngineError::Validation)?;
                if captions.is_empty() {
                    return Err(EngineError::Validation(
                        "caption file contained no cues".into(),
                    ));
                }
                let imported = captions.len();
                let mut data_captions = Vec::new();
                {
                    let active = self.require_active_mut()?;
                    let snapshot = take_snapshot(active, &kind);
                    active.undo.push(snapshot);
                    active.redo.clear();
                    for caption in captions {
                        let id = caption.id;
                        active.project.captions.push(caption);
                        data_captions.push(json!({"caption_id": id}));
                    }
                    active.project.captions.sort_by_key(|c| c.start);
                    active.project.revision += 1;
                    Self::autosave(active)?;
                }
                let data = json!({"imported": imported, "captions": data_captions});
                Ok(Outcome::new(self.revision(), &kind, data, "imported captions"))
            }
            Command::ExportCaptions { path } => {
                let active = self.require_active()?;
                let doc = srt::to_srt(active.project.captions.clone());
                std::fs::write(path, doc)?;
                let data = json!({"path": path.display().to_string(), "count": active.project.captions.len()});
                Ok(Outcome::new(self.revision(), &kind, data, "exported captions"))
            }

            // ----- HyperFrames -----
            Command::CreateHyperFrames {
                name,
                html,
                duration_ticks,
                transparent,
                overlay,
                insert_at_ticks,
                width,
                height,
                wait,
            } => {
                let data = self.create_hyperframes(
                    name,
                    html,
                    *duration_ticks,
                    transparent.unwrap_or(true),
                    overlay.unwrap_or(true),
                    *insert_at_ticks,
                    *width,
                    *height,
                    wait.unwrap_or(false),
                )?;
                Ok(Outcome::new(self.revision(), &kind, data, "created HyperFrames clip"))
            }
            Command::UpdateHyperFramesSource { asset_id, html } => {
                let active = self.require_active_mut()?;
                let asset = active
                    .project
                    .assets
                    .iter_mut()
                    .find(|a| &a.id == asset_id)
                    .ok_or_else(|| EngineError::Validation(format!("unknown asset {asset_id}")))?;
                let generated = asset
                    .generated
                    .as_mut()
                    .ok_or_else(|| EngineError::Validation("asset is not a HyperFrames composition".into()))?;
                generated.html = html.clone();
                std::fs::write(generated.source_dir.join("index.html"), html)?;
                let data = json!({"asset_id": asset_id, "source_updated": true});
                Ok(Outcome::new(self.revision(), &kind, data, "updated composition source"))
            }
            Command::RenderHyperFrames { asset_id, wait } => {
                let (transparent, source_dir, duration, overlay) = {
                    let active = self.require_active()?;
                    let asset = require_asset(&active.project, asset_id)?;
                    let generated = asset.generated.as_ref().ok_or_else(|| {
                        EngineError::Validation("asset is not a HyperFrames composition".into())
                    })?;
                    (
                        generated.transparent,
                        generated.source_dir.clone(),
                        generated.duration_ticks,
                        overlay_for_asset(&active.project, asset_id),
                    )
                };
                let result = self.spawn_hyperframes_render(
                    *asset_id,
                    transparent,
                    overlay,
                    None,
                    duration,
                    source_dir,
                    wait.unwrap_or(false),
                );
                let mut data = result?;
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("asset_id".into(), json!(asset_id));
                }
                Ok(Outcome::new(
                    self.revision(),
                    &kind,
                    data,
                    "re-render started",
                ))
            }

            // ----- preview -----
            Command::PreviewFrame { ticks: at, max_width } => {
                let active = self.require_active()?;
                let t = at.unwrap_or(active.project.editor.playhead_ticks);
                let (jpeg, w, h) = preview::composite_jpeg(
                    &active.project,
                    t,
                    max_width.unwrap_or(960),
                )?;
                let captions: Vec<&str> = active
                    .project
                    .captions_at(t)
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect();
                let data = json!({
                    "ticks": t,
                    "seconds": openprem_core::secs_from_ticks(t),
                    "width": w,
                    "height": h,
                    "captions": captions,
                });
                Ok(Outcome::new(self.revision(), &kind, data, "preview frame")
                    .with_binary("image/jpeg", jpeg))
            }

            // ----- jobs & render -----
            Command::StartRender {
                output_path,
                video_bitrate_mbps,
                use_nvenc,
            } => {
                let active = self.require_active()?;
                let out = match output_path {
                    Some(p) => p.clone(),
                    None => default_export_path(&active.dir),
                };
                let project = active.project.clone();
                let opts = export::ExportOptions {
                    use_nvenc: *use_nvenc,
                    hwaccel: None,
                    video_bitrate_mbps: *video_bitrate_mbps,
                    audio_bitrate_kbps: 256,
                };
                let (job_id, cancel) = {
                    let mut reg = self.jobs.lock().expect("job registry lock");
                    reg.start("render", &format!("export {}", out.display()))
                };
                let progress = progress_sink(&self.jobs, job_id);
                let registry = self.jobs.clone();
                let out_for_thread = out.clone();
                std::thread::spawn(move || {
                    let result = export::export_project(
                        &project,
                        &out_for_thread,
                        &opts,
                        Some(progress),
                        cancel,
                    );
                    let mut reg = registry.lock().expect("job registry lock");
                    match result {
                        Ok(stats) => {
                            reg.finish(
                                &job_id,
                                JobState::Done {
                                    message: format!(
                                        "exported {} ({})",
                                        stats.output_path.display(),
                                        stats.video_codec
                                    ),
                                    data: serde_json::to_value(&stats).unwrap_or_default(),
                                },
                            );
                        }
                        Err(openprem_media::MediaError::Cancelled) => {
                            reg.finish(&job_id, JobState::Cancelled);
                        }
                        Err(e) => {
                            reg.finish(&job_id, JobState::Failed { error: e.to_string() });
                        }
                    }
                });
                let data = json!({
                    "job_id": job_id,
                    "output_path": out.display().to_string(),
                    "auto_detected_nvenc": export::detect_nvenc(),
                });
                Ok(Outcome::new(
                    self.revision(),
                    &kind,
                    data,
                    "export started",
                ))
            }
            Command::CancelJob { job_id } => {
                let mut reg = self.jobs.lock().expect("job registry lock");
                let ok = reg.cancel(job_id);
                let data = json!({"job_id": job_id, "cancel_requested": ok});
                if ok {
                    Ok(Outcome::new(self.revision(), &kind, data, "cancellation requested"))
                } else {
                    Err(EngineError::Validation(format!(
                        "job {job_id} is not running"
                    )))
                }
            }
            Command::GetJob { job_id } => {
                self.poll_jobs()?;
                let reg = self.jobs.lock().expect("job registry lock");
                let job = reg.get(job_id).ok_or_else(|| {
                    EngineError::Validation(format!("unknown job {job_id}"))
                })?;
                Ok(Outcome::new(self.revision(), &kind, job.to_json(), "job status"))
            }
            Command::ListJobs {} => {
                self.poll_jobs()?;
                let reg = self.jobs.lock().expect("job registry lock");
                let data = reg.list_json(JOBS_LIST_LIMIT);
                Ok(Outcome::new(self.revision(), &kind, data, "jobs"))
            }

            // ----- system -----
            Command::SystemCapabilities {} => {
                let (has_ffmpeg, has_ffprobe) = openprem_media::tool_status();
                let data = json!({
                    "openprem": env!("CARGO_PKG_VERSION"),
                    "schema_version": SCHEMA_VERSION,
                    "ffmpeg": has_ffmpeg,
                    "ffprobe": has_ffprobe,
                    "nvenc_h264": export::detect_nvenc(),
                    "npx_available": hyperframes::sidecar_available(),
                    "gpu": gpu_name().unwrap_or_else(|| "unknown".into()),
                    "commands": openprem_core::KINDS,
                });
                Ok(Outcome::new(self.revision(), &kind, data, "capabilities"))
            }

            // ----- meta -----
            Command::CommandsBatch {
                commands,
                expected_revision,
                label,
            } => {
                if let Some(expected) = expected_revision {
                    let expected = *expected;
                    let actual = self.revision();
                    if expected != actual {
                        return Err(EngineError::Core(
                            openprem_core::CoreError::RevisionMismatch { expected, actual },
                        ));
                    }
                }
                self.execute_batch(commands, label.clone().unwrap_or_else(|| "batch".into()))
            }
        }?;
        Ok(outcome)
    }

    // ------------------------------------------------------------------
    // helpers
    // ------------------------------------------------------------------

    fn require_active(&self) -> Result<&ActiveProject, EngineError> {
        self.active
            .as_ref()
            .ok_or(EngineError::Core(openprem_core::CoreError::NoProject))
    }

    fn require_active_mut(&mut self) -> Result<&mut ActiveProject, EngineError> {
        self.active
            .as_mut()
            .ok_or(EngineError::Core(openprem_core::CoreError::NoProject))
    }

    /// Snapshot, apply a pure edit, bump revision, autosave.
    fn mutate(&mut self, cmd: &Command, kind: &str) -> Result<Value, EngineError> {
        let active = self.require_active_mut()?;
        let snapshot = take_snapshot(active, kind);
        let result = edit::apply_edit(&mut active.project, cmd);
        match result {
            Ok(data) => {
                active.undo.push(snapshot);
                if active.undo.len() > HISTORY_LIMIT {
                    active.undo.remove(0);
                }
                active.redo.clear();
                active.project.revision += 1;
                Self::autosave(active)?;
                Ok(data)
            }
            Err(e) => {
                // Roll back to the pre-command document.
                active.project = snapshot.project;
                Err(e.into())
            }
        }
    }

    /// Atomic batch: apply every command to a clone, then swap it in once.
    fn execute_batch(
        &mut self,
        commands: &[Command],
        label: String,
    ) -> Result<Outcome, EngineError> {
        let active = self.require_active_mut()?;
        for cmd in commands {
            let allowed = cmd.is_pure_edit() || cmd.is_editor_state();
            if !allowed {
                return Err(EngineError::Batch {
                    kind: cmd.kind().to_string(),
                    reason: "only pure project edits and editor state are batchable"
                        .into(),
                });
            }
            if matches!(cmd, Command::CommandsBatch { .. }) {
                return Err(EngineError::Batch {
                    kind: cmd.kind().to_string(),
                    reason: "nested batches are not allowed".into(),
                });
            }
        }
        let snapshot = take_snapshot(active, &label);
        let mut working = active.project.clone();
        let mut results: Vec<Value> = Vec::new();
        for cmd in commands {
            if cmd.is_editor_state() {
                let data = edit::apply_editor_state(&mut working, cmd)?;
                results.push(data);
            } else {
                let data = edit::apply_edit(&mut working, cmd)?;
                results.push(data);
            }
        }
        // Swap in the fully applied clone.
        let active = self.active.as_mut().unwrap();
        active.project = working;
        active.undo.push(snapshot);
        if active.undo.len() > HISTORY_LIMIT {
            active.undo.remove(0);
        }
        active.redo.clear();
        active.project.revision += 1;
        Self::autosave(active)?;
        let data = json!({
            "applied": commands.len(),
            "revision": active.project.revision,
            "results": results,
        });
        Ok(Outcome::new(
            active.project.revision,
            "commands_batch",
            data,
            format!("applied {} commands atomically", commands.len()),
        ))
    }

    /// Import + optional auto-place. One undo step.
    fn import_media(&mut self, path: &Path, auto_place: bool) -> Result<Value, EngineError> {
        if !path.exists() {
            return Err(EngineError::Validation(format!(
                "`{}` does not exist",
                path.display()
            )));
        }
        let probed = ffprobe::probe(path)?;
        let asset = Asset {
            id: Uuid::new_v4(),
            kind: probed.kind,
            name: path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "media".into()),
            path: path.to_path_buf(),
            probe: Some(probed.probe.clone()),
            generated: None,
        };
        let asset_id = asset.id;
        let kind = asset.kind;

        let mut placed: Vec<Value> = Vec::new();
        {
            let active = self.require_active_mut()?;
            let snapshot = take_snapshot(active, "import_media");
            active.undo.push(snapshot);
            active.redo.clear();
            active.project.assets.push(asset);

            if auto_place {
                let duration = active.project.assets.iter().find(|a| a.id == asset_id).map(|a| a.editable_duration()).unwrap_or(0);
                match kind {
                    AssetKind::Audio => {
                        let track_id = ensure_audio_track(&mut active.project);
                        let start = track_end(&active.project, &track_id);
                        let clip = Clip {
                            id: Uuid::new_v4(),
                            asset_id,
                            track_id,
                            timeline_start: start,
                            source_in: 0,
                            source_out: duration,
                            link_group: None,
                            opacity: 1.0,
                            scale: 1.0,
                            x_offset: 0.0,
                            y_offset: 0.0,
                            volume: 1.0,
                            fade_in_ticks: 0,
                            fade_out_ticks: 0,
                        };
                        placed.push(json!({"clip_id": clip.id, "track_id": clip.track_id, "timeline_start": clip.timeline_start}));
                        active.project.clips.push(clip);
                    }
                    AssetKind::Video | AssetKind::Image => {
                        let v_track = active
                            .project
                            .base_video_track()
                            .map(|t| t.id)
                            .ok_or_else(|| EngineError::Validation("no video track".into()))?;
                        let start = track_end(&active.project, &v_track);
                        let group = if probed.probe.has_audio && kind == AssetKind::Video {
                            Some(Uuid::new_v4())
                        } else {
                            None
                        };
                        let clip = Clip {
                            id: Uuid::new_v4(),
                            asset_id,
                            track_id: v_track,
                            timeline_start: start,
                            source_in: 0,
                            source_out: duration,
                            link_group: group,
                            opacity: 1.0,
                            scale: 1.0,
                            x_offset: 0.0,
                            y_offset: 0.0,
                            volume: 1.0,
                            fade_in_ticks: 0,
                            fade_out_ticks: 0,
                        };
                        placed.push(json!({"clip_id": clip.id, "track_id": clip.track_id, "timeline_start": clip.timeline_start}));
                        active.project.clips.push(clip);
                        // Linked audio for phone-style footage.
                        if let Some(g) = group {
                            let a_track = ensure_audio_track(&mut active.project);
                            let a_clip = Clip {
                                id: Uuid::new_v4(),
                                asset_id,
                                track_id: a_track,
                                timeline_start: start,
                                source_in: 0,
                                source_out: duration,
                                link_group: Some(g),
                                opacity: 1.0,
                                scale: 1.0,
                                x_offset: 0.0,
                                y_offset: 0.0,
                                volume: 1.0,
                                fade_in_ticks: 0,
                                fade_out_ticks: 0,
                            };
                            placed.push(json!({"clip_id": a_clip.id, "track_id": a_clip.track_id, "timeline_start": a_clip.timeline_start}));
                            active.project.clips.push(a_clip);
                        }
                    }
                }
            }
            active.project.revision += 1;
            Self::autosave(active)?;
        }
        Ok(json!({
            "asset_id": asset_id,
            "kind": kind_str(kind),
            "probe": serde_json::to_value(&probed.probe)
                .map_err(|e| EngineError::Validation(e.to_string()))?,
            "placed_clips": placed,
        }))
    }

    // ----- HyperFrames -----

    #[allow(clippy::too_many_arguments)]
    fn create_hyperframes(
        &mut self,
        name: &str,
        html: &str,
        duration_ticks: Ticks,
        transparent: bool,
        overlay: bool,
        insert_at_ticks: Option<Ticks>,
        width: Option<u32>,
        height: Option<u32>,
        wait: bool,
    ) -> Result<Value, EngineError> {
        if duration_ticks <= 0 {
            return Err(EngineError::Validation("duration must be > 0".into()));
        }
        let active = self.require_active()?;
        let w = width.unwrap_or(active.project.settings.width);
        let h = height.unwrap_or(active.project.settings.height);
        if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(EngineError::Validation(
                "composition width/height must be even".into(),
            ));
        }
        if !hyperframes::sidecar_available() {
            return Err(EngineError::Validation(
                "Node.js `npx` is not available; install Node 22+ to render HyperFrames".into(),
            ));
        }
        let dir = hyperframes::scaffold_composition(&active.dir, name, html);
        // Placeholder asset; probe fills in when the render lands.
        let asset = Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Video,
            name: name.to_string(),
            path: hyperframes::output_file(&dir, transparent),
            probe: None,
            generated: Some(openprem_core::GeneratedAsset {
                source_dir: dir.clone(),
                format: if transparent { "mov".into() } else { "mp4".into() },
                transparent,
                width: w,
                height: h,
                fps: active.project.settings.fps,
                duration_ticks,
                html: html.to_string(),
            }),
        };
        let asset_id = asset.id;
        {
            let active = self.require_active_mut()?;
            let snapshot = take_snapshot(active, "create_hyperframes");
            active.undo.push(snapshot);
            active.redo.clear();
            active.project.assets.push(asset);
            active.project.revision += 1;
            Self::autosave(active)?;
        }
        let outcome = self.spawn_hyperframes_render(
            asset_id,
            transparent,
            overlay,
            insert_at_ticks,
            duration_ticks,
            dir.clone(),
            wait,
        )?;
        let mut data = json!({
            "asset_id": asset_id,
            "transparent": transparent,
            "overlay": overlay,
            "source_dir": dir.display().to_string(),
        });
        if let Some(extra) = outcome.as_object() {
            for (k, v) in extra {
                data[k] = v.clone();
            }
        }
        Ok(data)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_hyperframes_render(
        &mut self,
        asset_id: Uuid,
        transparent: bool,
        overlay: bool,
        insert_at_ticks: Option<Ticks>,
        duration_ticks: Ticks,
        source_dir: PathBuf,
        wait: bool,
    ) -> Result<Value, EngineError> {
        let (job_id, cancel) = {
            let mut reg = self.jobs.lock().expect("job registry lock");
            reg.start(
                "hyperframes",
                &format!("render composition {asset_id}"),
            )
        };
        {
            let mut reg = self.jobs.lock().expect("job registry lock");
            reg.set_pending(
                &job_id,
                PendingFinalize::ImportHyperFrames {
                    asset_id,
                    overlay,
                    insert_at_ticks,
                    duration_ticks,
                    transparent,
                },
            );
        }
        let registry = self.jobs.clone();
        let dir = source_dir.clone();
        std::thread::spawn(move || {
            let result = hyperframes::render(&dir, transparent, cancel);
            let mut reg = registry.lock().expect("job registry lock");
            match result {
                Ok(path) => {
                    reg.finish(
                        &job_id,
                        JobState::Done {
                            message: format!("rendered {}", path.display()),
                            data: json!({"path": path.display().to_string(), "transparent": transparent}),
                        },
                    );
                }
                Err(openprem_media::MediaError::Cancelled) => {
                    reg.finish(&job_id, JobState::Cancelled);
                }
                Err(e) => {
                    reg.finish(&job_id, JobState::Failed { error: e.to_string() });
                }
            }
        });
        if wait {
            // Block until the registry reports a terminal state.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
            loop {
                {
                    let reg = self.jobs.lock().expect("job registry lock");
                    if let Some(job) = reg.get(&job_id) {
                        match &job.state {
                            JobState::Failed { error } => {
                                return Err(EngineError::Validation(error.clone()));
                            }
                            JobState::Cancelled => {
                                return Err(EngineError::Validation("render cancelled".into()));
                            }
                            JobState::Done { .. } => break,
                            JobState::Running { .. } => {}
                        }
                    }
                }
                if std::time::Instant::now() > deadline {
                    return Err(EngineError::Validation(
                        "HyperFrames render timed out after 600s".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            self.poll_jobs()?;
            return Ok(json!({"job_id": job_id, "waited": true}));
        }
        Ok(json!({"job_id": job_id, "waited": false}))
    }

    fn finalize_hyperframes(
        &mut self,
        job_id: Uuid,
        asset_id: Uuid,
        overlay: bool,
        insert_at_ticks: Option<Ticks>,
        duration_ticks: Ticks,
        transparent: bool,
    ) -> Result<(), EngineError> {
        let render_path: PathBuf = {
            let reg = self.jobs.lock().expect("job registry lock");
            let job = reg.get(&job_id).ok_or_else(|| {
                EngineError::Validation(format!("unknown job {job_id}"))
            })?;
            match &job.state {
                JobState::Done { data, .. } => data["path"]
                    .as_str()
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        EngineError::Validation("render job finished without an output path".into())
                    })?,
                _ => return Ok(()), // not actually finished yet
            }
        };
        if !render_path.is_file() {
            return Err(EngineError::Validation(format!(
                "rendered file `{}` is missing",
                render_path.display()
            )));
        }
        let probed = ffprobe::probe(&render_path)?;
        let active = self.require_active_mut()?;
        let snapshot = take_snapshot(active, "finalize_hyperframes");
        let asset = active
            .project
            .assets
            .iter_mut()
            .find(|a| a.id == asset_id)
            .ok_or_else(|| EngineError::Validation(format!("unknown asset {asset_id}")))?;
        asset.probe = Some(probed.probe.clone());
        asset.path = render_path.clone();
        let mut inserted = None;
        if let Some(at) = insert_at_ticks {
            let track_id = if overlay {
                ensure_overlay_track(&mut active.project)
            } else {
                active
                    .project
                    .base_video_track()
                    .map(|t| t.id)
                    .ok_or_else(|| EngineError::Validation("no video track".into()))?
            };
            let source_out = duration_ticks.min(probed.probe.duration_ticks);
            let edit_cmd = Command::InsertClip {
                asset_id,
                track_id,
                timeline_start: at,
                source_in: 0,
                source_out,
                link_group: None,
            };
            match edit::apply_edit(&mut active.project, &edit_cmd) {
                Ok(data) => inserted = Some(data),
                Err(e) => {
                    // Roll the asset update back with the document.
                    active.project = snapshot.project;
                    return Err(e.into());
                }
            }
        }
        active.undo.push(snapshot);
        if active.undo.len() > HISTORY_LIMIT {
            active.undo.remove(0);
        }
        active.redo.clear();
        active.project.revision += 1;
        Self::autosave(active)?;
        let mut reg = self.jobs.lock().expect("job registry lock");
        reg.mark_finalized(&job_id);
        let _ = (inserted, transparent);
        Ok(())
    }

    fn autosave(active: &ActiveProject) -> Result<(), EngineError> {
        let file = project_io::project_file(&active.dir);
        project_io::write_atomic(&file, &active.project)
    }
}

// ---------------------------------------------------------------------------
// free helpers
// ---------------------------------------------------------------------------

fn take_snapshot(active: &ActiveProject, label: &str) -> Snapshot {
    Snapshot {
        project: active.project.clone(),
        label: label.to_string(),
    }
}

fn require_asset<'a>(project: &'a Project, id: &Uuid) -> Result<&'a Asset, EngineError> {
    project
        .asset(id)
        .ok_or_else(|| EngineError::Validation(format!("unknown asset {id}")))
}

fn kind_str(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Video => "video",
        AssetKind::Audio => "audio",
        AssetKind::Image => "image",
    }
}

fn ensure_audio_track(project: &mut Project) -> Uuid {
    if let Some(track) = project.tracks.iter().find(|t| t.kind == TrackKind::Audio) {
        return track.id;
    }
    let id = Uuid::new_v4();
    project.tracks.push(Track {
        id,
        kind: TrackKind::Audio,
        name: format!("A{}", project.tracks.iter().filter(|t| t.kind == TrackKind::Audio).count() + 1),
        muted: false,
        hidden: false,
        locked: false,
        volume: 1.0,
    });
    id
}

fn ensure_overlay_track(project: &mut Project) -> Uuid {
    let video_tracks: Vec<&Track> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .collect();
    if video_tracks.len() >= 2 {
        return video_tracks[1].id;
    }
    let id = Uuid::new_v4();
    project.tracks.push(Track {
        id,
        kind: TrackKind::Video,
        name: "V2".into(),
        muted: false,
        hidden: false,
        locked: false,
        volume: 1.0,
    });
    id
}

fn overlay_for_asset(project: &Project, asset_id: &Uuid) -> bool {
    project
        .asset(asset_id)
        .and_then(|a| a.generated.as_ref())
        .map(|g| g.transparent)
        .unwrap_or(true)
}

fn track_end(project: &Project, track_id: &Uuid) -> Ticks {
    project
        .clips
        .iter()
        .filter(|c| &c.track_id == track_id)
        .map(|c| c.timeline_end())
        .max()
        .unwrap_or(0)
}

fn default_export_path(dir: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("export-{stamp}.mp4"))
}

fn gpu_name() -> Option<String> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn asset_json(asset: &Asset) -> Value {
    json!({
        "asset_id": asset.id,
        "name": asset.name,
        "kind": kind_str(asset.kind),
        "path": asset.path.display().to_string(),
        "duration_ticks": asset.editable_duration(),
        "duration_secs": openprem_core::secs_from_ticks(asset.editable_duration()),
        "probe": asset.probe.as_ref().map(|p| serde_json::to_value(p).unwrap_or_default()),
        "generated": asset.generated.is_some(),
    })
}

fn clip_json(project: &Project, clip: &Clip) -> Value {
    let asset_name = project.asset(&clip.asset_id).map(|a| a.name.clone());
    json!({
        "clip_id": clip.id,
        "asset_id": clip.asset_id,
        "asset_name": asset_name,
        "track_id": clip.track_id,
        "timeline_start": clip.timeline_start,
        "timeline_end": clip.timeline_end(),
        "source_in": clip.source_in,
        "source_out": clip.source_out,
        "link_group": clip.link_group,
        "volume": clip.volume,
        "opacity": clip.opacity,
        "scale": clip.scale,
        "x_offset": clip.x_offset,
        "y_offset": clip.y_offset,
        "fade_in_ticks": clip.fade_in_ticks,
        "fade_out_ticks": clip.fade_out_ticks,
    })
}

fn timeline_json(project: &Project) -> Value {    let tracks: Vec<Value> = project
        .tracks
        .iter()
        .map(|t| {
            json!({
                "track_id": t.id,
                "kind": match t.kind { TrackKind::Video => "video", TrackKind::Audio => "audio" },
                "name": t.name,
                "muted": t.muted,
                "hidden": t.hidden,
                "locked": t.locked,
                "volume": t.volume,
                "clips": project.clips_on_track(&t.id)
                    .iter()
                    .map(|c| clip_json(project, c))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "duration_ticks": project.timeline_end(),
        "duration_secs": openprem_core::secs_from_ticks(project.timeline_end()),
        "settings": {
            "width": project.settings.width,
            "height": project.settings.height,
            "fps": project.settings.fps.display(),
            "audio_sample_rate": project.settings.audio_sample_rate,
        },
        "tracks": tracks,
        "transitions": project.transitions.iter().map(|t| json!({
            "transition_id": t.id,
            "at": t.at,
            "duration": t.duration,
            "kind": t.kind.display(),
            "color": t.color,
        })).collect::<Vec<_>>(),
        "captions": project.captions.iter().map(|c| json!({
            "caption_id": c.id,
            "start": c.start,
            "end": c.end,
            "text": c.text,
        })).collect::<Vec<_>>(),
        "caption_style": serde_json::to_value(&project.caption_style).unwrap_or_default(),
        "editor": {
            "playhead_ticks": project.editor.playhead_ticks,
            "selection": project.editor.selection,
        },
    })
}
