//! The engine: owns the open project, executes every registry command,
//! keeps snapshot history for undo, and manages background jobs.

use crate::{
    error::EngineError,
    jobs::{progress_sink, JobRegistry, JobState, PendingFinalize},
    preview, project_io,
};
use cutpilot_core::{
    edit, Asset, AssetKind, Clip, Command, Outcome, Project, ProjectSettings, Ticks, Track,
    TrackKind, Transcript, SCHEMA_VERSION,
};
use cutpilot_media::{export, ffprobe, frames, hyperframes, silence, srt, waveform, whisper};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const HISTORY_LIMIT: usize = 100;
const JOBS_LIST_LIMIT: usize = 50;
const MAX_CONCURRENT_JOBS: usize = 4;

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
                return Err(EngineError::Core(cutpilot_core::CoreError::RevisionMismatch {
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
            match action {
                PendingFinalize::ImportHyperFrames {
                    asset_id,
                    overlay,
                    insert_at_ticks,
                    duration_ticks,
                    transparent,
                } => {
                    self.finalize_hyperframes(
                        job_id,
                        asset_id,
                        overlay,
                        insert_at_ticks,
                        duration_ticks,
                        transparent,
                    )?;
                }
                PendingFinalize::StoreTranscript {
                    asset_id,
                    add_captions,
                } => {
                    self.finalize_transcript(job_id, asset_id, add_captions)?;
                }
                PendingFinalize::None => {}
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
            cutpilot_core::CoreError::NoProject,
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
                    settings.fps = cutpilot_core::commands::fps_from_parts(*n, *d)
                        .map_err(EngineError::Validation)?;
                } else {
                    settings.fps = cutpilot_core::Fps::from_int(30);
                }
                settings.fps.validate().map_err(EngineError::Validation)?;
                if !settings.width.is_multiple_of(2)
                    || !(2..=7680).contains(&settings.width)
                    || !settings.height.is_multiple_of(2)
                    || !(2..=7680).contains(&settings.height)
                {
                    return Err(EngineError::Validation(
                        "width and height must be even numbers between 2 and 7680".into(),
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
            | Command::SetClipMotion { .. }
            | Command::SetClipSpeed { .. }
            | Command::SetClipColor { .. }
            | Command::DuplicateClip { .. }
            | Command::UnlinkClip { .. }
            | Command::AddMarker { .. }
            | Command::RemoveMarker { .. }
            | Command::RemoveRanges { .. }
            | Command::AddTransition { .. }
            | Command::RemoveTransition { .. }
            | Command::SetCaptionStyle { .. }
            | Command::AddCaption { .. }
            | Command::UpdateCaption { .. }
            | Command::RemoveCaption { .. }
            | Command::TranscriptToCaptions { .. } => {
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
                let project_dir = {
                    let active = self.require_active()?;
                    let used = active.project.clips.iter().any(|c| &c.asset_id == asset_id);
                    if used {
                        return Err(EngineError::Validation(
                            "asset is used by timeline clips; remove those clips first".into(),
                        ));
                    }
                    active.dir.clone()
                };
                let data = self.mutate(&cmd, &kind)?;
                cleanup_asset_caches(&project_dir, asset_id);
                Ok(Outcome::new(self.revision(), &kind, data, "removed asset"))
            }
            Command::MediaList {} => {
                let active = self.require_active()?;
                let assets: Vec<Value> = active
                    .project
                    .assets
                    .iter()
                    .map(|a| asset_json(&active.project, a))
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
                        "start_secs": cutpilot_core::secs_from_ticks(r.start),
                        "end_secs": cutpilot_core::secs_from_ticks(r.end),
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
                    "peaks": peaks.iter().map(|p| json!({"min": p.min, "max": p.max, "rms": p.rms})).collect::<Vec<_>>(),
                });
                Ok(Outcome::new(self.revision(), &kind, data, "waveform"))
            }
            Command::TranscribeMedia {
                asset_id,
                model,
                language,
                translate,
                add_captions,
                wait,
            } => {
                let data = self.transcribe_media(
                    *asset_id,
                    model.clone(),
                    language.clone(),
                    translate.unwrap_or(false),
                    add_captions.unwrap_or(false),
                    wait.unwrap_or(false),
                )?;
                let msg = if wait.unwrap_or(false) {
                    "transcribed media"
                } else {
                    "transcription started"
                };
                Ok(Outcome::new(self.revision(), &kind, data, msg))
            }
            Command::GetTranscript { asset_id } => {
                let active = self.require_active()?;
                let data = transcript_json(&active.project, *asset_id)?;
                Ok(Outcome::new(self.revision(), &kind, data, "transcript"))
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
                let project_dir = active.dir.clone();
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
                let validated_dir =
                    hyperframes::validate_composition_dir(&project_dir, &generated.source_dir)?;
                let index_path = validated_dir.join("index.html");
                if let Ok(meta) = std::fs::symlink_metadata(&index_path) {
                    if meta.file_type().is_symlink() {
                        return Err(EngineError::Validation(
                            "refusing to write HyperFrames index.html through a symlink".into(),
                        ));
                    }
                }
                generated.source_dir = validated_dir;
                generated.html = html.clone();
                std::fs::write(&index_path, html)?;
                let data = json!({"asset_id": asset_id, "source_updated": true});
                Ok(Outcome::new(self.revision(), &kind, data, "updated composition source"))
            }
            Command::RenderHyperFrames { asset_id, wait } => {
                let (name, transparent, source_dir, duration, overlay) = {
                    let active = self.require_active()?;
                    let asset = require_asset(&active.project, asset_id)?;
                    let generated = asset.generated.as_ref().ok_or_else(|| {
                        EngineError::Validation("asset is not a HyperFrames composition".into())
                    })?;
                    let validated_dir = hyperframes::validate_composition_dir(
                        &active.dir,
                        &generated.source_dir,
                    )?;
                    (
                        asset.name.clone(),
                        generated.transparent,
                        validated_dir,
                        generated.duration_ticks,
                        overlay_for_asset(&active.project, asset_id),
                    )
                };
                let result = self.spawn_hyperframes_render(
                    *asset_id,
                    &name,
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
                    "seconds": cutpilot_core::secs_from_ticks(t),
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
                codec,
                preset,
                width,
                height,
            } => {
                let active = self.require_active()?;
                let out = match output_path {
                    Some(p) => p.clone(),
                    None => default_export_path(&active.dir),
                };
                let parsed_codec = match codec.as_deref() {
                    Some(c) => export::VideoCodec::parse(c).map_err(EngineError::Validation)?,
                    None => export::VideoCodec::H264,
                };
                let parsed_preset = match preset.as_deref() {
                    Some(p) => export::ExportPreset::parse(p).map_err(EngineError::Validation)?,
                    None => export::ExportPreset::Balanced,
                };
                let (out_w, out_h) =
                    export::resolve_export_dimensions(&active.project, *width, *height)
                        .map_err(EngineError::Validation)?;
                let project = active.project.clone();
                let opts = export::ExportOptions {
                    use_nvenc: *use_nvenc,
                    hwaccel: None,
                    gpu_resident: None,
                    video_bitrate_mbps: *video_bitrate_mbps,
                    audio_bitrate_kbps: 256,
                    codec: parsed_codec,
                    preset: parsed_preset,
                    width: Some(out_w),
                    height: Some(out_h),
                };
                self.ensure_job_capacity()?;
                let (job_id, cancel) = {
                    let mut reg = self.jobs.lock().expect("job registry lock");
                    reg.start("render", &format!("export {out_w}×{out_h} {}", out.display()))
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
                            let mode = if stats.gpu_resident {
                                "GPU zero-copy"
                            } else if stats.video_codec.ends_with("_nvenc") {
                                "NVENC"
                            } else {
                                "CPU"
                            };
                            reg.finish(
                                &job_id,
                                JobState::Done {
                                    message: format!(
                                        "exported {} ({}×{}, {}, {mode})",
                                        stats.output_path.display(),
                                        stats.width,
                                        stats.height,
                                        stats.video_codec
                                    ),
                                    data: serde_json::to_value(&stats).unwrap_or_default(),
                                },
                            );
                        }
                        Err(cutpilot_media::MediaError::Cancelled) => {
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
                    "width": out_w,
                    "height": out_h,
                    "auto_detected_nvenc": export::detect_nvenc(),
                    "codec": parsed_codec.label(),
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
                let (has_ffmpeg, has_ffprobe) = cutpilot_media::tool_status();
                let gpu = cutpilot_media::gpu::capabilities();
                let wh = whisper::capabilities();
                let data = json!({
                    "cutpilot": env!("CARGO_PKG_VERSION"),
                    "schema_version": SCHEMA_VERSION,
                    "ffmpeg": has_ffmpeg,
                    "ffprobe": has_ffprobe,
                    "nvenc_h264": gpu.nvenc_h264,
                    "nvenc_hevc": gpu.nvenc_hevc,
                    "nvenc_av1": gpu.nvenc_av1,
                    "cuda_filters": gpu.cuda_filters,
                    "nvdec": gpu.nvdec,
                    "npx_available": hyperframes::sidecar_available(),
                    "gpu": gpu.gpu_name.clone().unwrap_or_else(|| "unknown".into()),
                    "whisper": wh.available,
                    "whisper_backend": wh.backend,
                    "whisper_cpp_version": wh.whisper_cpp_version,
                    "whisper_default_model": wh.default_model,
                    "whisper_models_dir": wh.models_dir,
                    "whisper_downloaded_models": wh.downloaded_models,
                    "whisper_supported_models": wh.supported_models,
                    "commands": cutpilot_core::KINDS,
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
                            cutpilot_core::CoreError::RevisionMismatch { expected, actual },
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
            .ok_or(EngineError::Core(cutpilot_core::CoreError::NoProject))
    }

    fn require_active_mut(&mut self) -> Result<&mut ActiveProject, EngineError> {
        self.active
            .as_mut()
            .ok_or(EngineError::Core(cutpilot_core::CoreError::NoProject))
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
                let duration = active
                    .project
                    .assets
                    .iter()
                    .find(|a| a.id == asset_id)
                    .map(|a| a.editable_duration())
                    .unwrap_or(0);
                if duration > 0 {
                    match kind {
                        AssetKind::Audio => {
                            let (track_id, start) =
                                choose_audio_track_for_auto_place(&mut active.project);
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
                                speed: 1.0,
                                rotation_deg: 0.0,
                                flip_h: false,
                                flip_v: false,
                                crop: Default::default(),
                                motion: None,
                                color: Default::default(),
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
                                speed: 1.0,
                                rotation_deg: 0.0,
                                flip_h: false,
                                flip_v: false,
                                crop: Default::default(),
                                motion: None,
                                color: Default::default(),
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
                                    speed: 1.0,
                                    rotation_deg: 0.0,
                                    flip_h: false,
                                    flip_v: false,
                                    crop: Default::default(),
                                    motion: None,
                                    color: Default::default(),
                                };
                                placed.push(json!({"clip_id": a_clip.id, "track_id": a_clip.track_id, "timeline_start": a_clip.timeline_start}));
                                active.project.clips.push(a_clip);
                            }
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
                "Node.js `npx` or `hyperframes` CLI is not available; install Node 22+ to render HyperFrames".into(),
            ));
        }
        self.ensure_job_capacity()?;
        let dir = hyperframes::scaffold_composition(&active.dir, name, html)?;
        // Placeholder asset; probe fills in when the render lands.
        let asset = Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Video,
            name: name.to_string(),
            path: hyperframes::output_file(&dir, transparent),
            probe: None,
            generated: Some(cutpilot_core::GeneratedAsset {
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
            name,
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
        name: &str,
        transparent: bool,
        overlay: bool,
        insert_at_ticks: Option<Ticks>,
        duration_ticks: Ticks,
        source_dir: PathBuf,
        wait: bool,
    ) -> Result<Value, EngineError> {
        self.ensure_job_capacity()?;
        let (job_id, cancel) = {
            let mut reg = self.jobs.lock().expect("job registry lock");
            reg.start("hyperframes", &format!("Rendering HyperFrames: {name}"))
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
                Err(cutpilot_media::MediaError::Cancelled) => {
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
        let res = self.finalize_hyperframes_inner(
            &render_path,
            asset_id,
            overlay,
            insert_at_ticks,
            duration_ticks,
            transparent,
        );
        let mut reg = self.jobs.lock().expect("job registry lock");
        if let Err(ref e) = res {
            reg.finish(
                &job_id,
                JobState::Failed {
                    error: e.to_string(),
                },
            );
        }
        reg.mark_finalized(&job_id);
        res
    }

    fn finalize_hyperframes_inner(
        &mut self,
        render_path: &Path,
        asset_id: Uuid,
        overlay: bool,
        insert_at_ticks: Option<Ticks>,
        duration_ticks: Ticks,
        transparent: bool,
    ) -> Result<(), EngineError> {
        if !render_path.is_file() {
            return Err(EngineError::Validation(format!(
                "rendered file `{}` is missing",
                render_path.display()
            )));
        }
        let probed = ffprobe::probe(render_path)?;
        let active = self.require_active_mut()?;
        let snapshot = take_snapshot(active, "finalize_hyperframes");
        let asset = active
            .project
            .assets
            .iter_mut()
            .find(|a| a.id == asset_id)
            .ok_or_else(|| EngineError::Validation(format!("unknown asset {asset_id}")))?;
        asset.probe = Some(probed.probe.clone());
        asset.path = render_path.to_path_buf();
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
        let _ = (inserted, transparent);
        Ok(())
    }

    // ----- Whisper transcription -----

    fn transcribe_media(
        &mut self,
        asset_id: Uuid,
        model: Option<String>,
        language: Option<String>,
        translate: bool,
        add_captions: bool,
        wait: bool,
    ) -> Result<Value, EngineError> {
        let (asset_name, asset_path, has_audio) = {
            let active = self.require_active()?;
            let asset = require_asset(&active.project, &asset_id)?;
            let has_audio = asset.kind == AssetKind::Audio
                || asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false);
            (asset.name.clone(), asset.path.clone(), has_audio)
        };
        if !has_audio {
            return Err(EngineError::Validation(format!(
                "asset `{asset_name}` has no audio stream to transcribe"
            )));
        }
        let model_name = model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| whisper::DEFAULT_MODEL.to_string());
        validate_whisper_model_arg(&model_name)?;
        self.ensure_job_capacity()?;

        let (job_id, cancel) = {
            let mut reg = self.jobs.lock().expect("job registry lock");
            reg.start(
                "transcribe",
                &format!("transcribe {asset_name} ({model_name})"),
            )
        };
        {
            let mut reg = self.jobs.lock().expect("job registry lock");
            reg.set_pending(
                &job_id,
                PendingFinalize::StoreTranscript {
                    asset_id,
                    add_captions,
                },
            );
        }

        let progress = progress_sink(&self.jobs, job_id);
        let registry = self.jobs.clone();
        let opts = whisper::TranscribeOptions {
            model: model_name.clone(),
            language,
            translate,
        };
        std::thread::spawn(move || {
            let result = whisper::transcribe_file(&asset_path, &opts, Some(progress), cancel);
            let mut reg = registry.lock().expect("job registry lock");
            match result {
                Ok(out) => {
                    let seg_count = out.segments.len();
                    let full_text = out
                        .segments
                        .iter()
                        .map(|s| s.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let transcript = Transcript {
                        asset_id,
                        model: out.model,
                        language: out.language,
                        segments: out.segments,
                    };
                    reg.finish(
                        &job_id,
                        JobState::Done {
                            message: format!(
                                "transcribed {asset_name} ({seg_count} segment{})",
                                if seg_count == 1 { "" } else { "s" }
                            ),
                            data: json!({
                                "asset_id": asset_id,
                                "segment_count": seg_count,
                                "full_text": full_text,
                                "transcript": transcript,
                            }),
                        },
                    );
                }
                Err(cutpilot_media::MediaError::Cancelled) => {
                    reg.finish(&job_id, JobState::Cancelled);
                }
                Err(e) => {
                    reg.finish(
                        &job_id,
                        JobState::Failed {
                            error: e.to_string(),
                        },
                    );
                }
            }
        });

        if wait {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1_800);
            loop {
                {
                    let reg = self.jobs.lock().expect("job registry lock");
                    if let Some(job) = reg.get(&job_id) {
                        match &job.state {
                            JobState::Failed { error } => {
                                return Err(EngineError::Validation(error.clone()));
                            }
                            JobState::Cancelled => {
                                return Err(EngineError::Validation(
                                    "transcription cancelled".into(),
                                ));
                            }
                            JobState::Done { .. } => break,
                            JobState::Running { .. } => {}
                        }
                    }
                }
                if std::time::Instant::now() > deadline {
                    return Err(EngineError::Validation(
                        "Whisper transcription timed out".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            self.poll_jobs()?;
            let active = self.require_active()?;
            let mut data = transcript_json(&active.project, Some(asset_id))?;
            if let Some(obj) = data.as_object_mut() {
                obj.insert("job_id".into(), json!(job_id));
                obj.insert("waited".into(), json!(true));
            }
            return Ok(data);
        }

        Ok(json!({
            "job_id": job_id,
            "asset_id": asset_id,
            "model": model_name,
            "waited": false,
        }))
    }

    fn finalize_transcript(
        &mut self,
        job_id: Uuid,
        asset_id: Uuid,
        add_captions: bool,
    ) -> Result<(), EngineError> {
        let transcript: Transcript = {
            let reg = self.jobs.lock().expect("job registry lock");
            let job = reg.get(&job_id).ok_or_else(|| {
                EngineError::Validation(format!("unknown job {job_id}"))
            })?;
            match &job.state {
                JobState::Done { data, .. } => {
                    serde_json::from_value(data["transcript"].clone()).map_err(|e| {
                        EngineError::Validation(format!("corrupt transcript payload: {e}"))
                    })?
                }
                _ => return Ok(()),
            }
        };
        let active = self.require_active_mut()?;
        if active.project.asset(&asset_id).is_none() {
            // Asset was removed while job was running; mark finalized cleanly.
            let mut reg = self.jobs.lock().expect("job registry lock");
            reg.mark_finalized(&job_id);
            return Ok(());
        }
        let snapshot = take_snapshot(active, "transcribe_media");
        active.project.upsert_transcript(transcript);
        if add_captions {
            let _ = edit::apply_edit(
                &mut active.project,
                &Command::TranscriptToCaptions {
                    asset_id: Some(asset_id),
                    replace: Some(false),
                },
            );
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
        Ok(())
    }

    fn ensure_job_capacity(&self) -> Result<(), EngineError> {
        let reg = self.jobs.lock().expect("job registry lock");
        let active_count = reg.active_jobs_of_kind("render").len()
            + reg.active_jobs_of_kind("hyperframes").len()
            + reg.active_jobs_of_kind("transcribe").len();
        if active_count >= MAX_CONCURRENT_JOBS {
            return Err(EngineError::Validation(format!(
                "too many concurrent background jobs ({active_count}/{MAX_CONCURRENT_JOBS}); wait for or cancel an active job first"
            )));
        }
        Ok(())
    }

    fn autosave(active: &ActiveProject) -> Result<(), EngineError> {
        let file = project_io::project_file(&active.dir);
        project_io::write_atomic(&file, &active.project)
    }
}

/// Remove cached proxy, waveform, and filmstrip derivatives when an asset is
/// removed from the project so deleted media does not remain on disk.
fn cleanup_asset_caches(project_dir: &Path, asset_id: &Uuid) {
    let cache = project_dir.join("cache");
    let candidates = [
        cache.join("proxies").join(format!("{asset_id}.mp4")),
        cache.join("waveforms").join(format!("{asset_id}.wf2")),
        cache.join("filmstrips").join(format!("{asset_id}.fs1")),
    ];
    for p in candidates {
        let _ = std::fs::remove_file(p);
    }
}

/// Allow only named Whisper GGML models from `SUPPORTED_MODELS` or `.bin` files
/// residing strictly inside `whisper::models_dir()`. Prevents arbitrary local
/// file existence probing and loading untrusted system files into whisper.cpp.
fn validate_whisper_model_arg(model_name: &str) -> Result<(), EngineError> {
    if whisper::SUPPORTED_MODELS.contains(&model_name) {
        return Ok(());
    }
    let models_root = whisper::models_dir();
    if let (Ok(root_canon), Ok(cand_canon)) = (
        std::fs::canonicalize(&models_root),
        std::fs::canonicalize(Path::new(model_name)),
    ) {
        let is_bin = cand_canon
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("bin"));
        if cand_canon.starts_with(&root_canon) && cand_canon.is_file() && is_bin {
            return Ok(());
        }
    }
    Err(EngineError::Validation(format!(
        "unsupported Whisper model `{model_name}`; choose one of: {} (or a .bin file inside {})",
        whisper::SUPPORTED_MODELS.join(", "),
        models_root.display()
    )))
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
    if let Some(track) = project
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Audio && !t.locked)
    {
        return track.id;
    }
    create_audio_track(project)
}

/// Choose the best audio track and start tick when auto-placing a standalone
/// audio file (e.g. `.wav`, music, voiceover):
/// 1. First unlocked empty audio track at `0`.
/// 2. First unlocked audio track containing only standalone audio clips (no
///    linked video audio) appended at its end.
/// 3. Otherwise create a fresh audio track (`A2`, `A3`, …) at `0` so standalone
///    audio plays alongside linked video audio instead of landing past the end.
fn choose_audio_track_for_auto_place(project: &mut Project) -> (Uuid, Ticks) {
    let unlocked_audio: Vec<Uuid> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio && !t.locked)
        .map(|t| t.id)
        .collect();

    if let Some(empty_id) = unlocked_audio
        .iter()
        .copied()
        .find(|id| !project.clips.iter().any(|c| c.track_id == *id))
    {
        return (empty_id, 0);
    }

    if let Some(standalone_id) = unlocked_audio.iter().copied().find(|id| {
        project
            .clips
            .iter()
            .filter(|c| c.track_id == *id)
            .all(|c| c.link_group.is_none())
    }) {
        let end = track_end(project, &standalone_id);
        return (standalone_id, end);
    }

    let id = create_audio_track(project);
    (id, 0)
}

fn create_audio_track(project: &mut Project) -> Uuid {
    let id = Uuid::new_v4();
    project.tracks.push(Track {
        id,
        kind: TrackKind::Audio,
        name: format!(
            "A{}",
            project
                .tracks
                .iter()
                .filter(|t| t.kind == TrackKind::Audio)
                .count()
                + 1
        ),
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

fn asset_json(project: &Project, asset: &Asset) -> Value {
    json!({
        "asset_id": asset.id,
        "name": asset.name,
        "kind": kind_str(asset.kind),
        "path": asset.path.display().to_string(),
        "duration_ticks": asset.editable_duration(),
        "duration_secs": cutpilot_core::secs_from_ticks(asset.editable_duration()),
        "probe": asset.probe.as_ref().map(|p| serde_json::to_value(p).unwrap_or_default()),
        "generated": asset.generated.is_some(),
        "has_transcript": project.transcript(&asset.id).is_some(),
    })
}

fn transcript_json(project: &Project, asset_id: Option<Uuid>) -> Result<Value, EngineError> {
    if let Some(aid) = asset_id {
        let asset = require_asset(project, &aid)?;
        let Some(tr) = project.transcript(&aid) else {
            return Ok(json!({
                "asset_id": aid,
                "asset_name": asset.name,
                "transcribed": false,
                "full_text": "",
                "segments": [],
                "timeline_segments": [],
            }));
        };
        let full_text = tr
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let segments: Vec<Value> = tr
            .segments
            .iter()
            .map(|s| {
                json!({
                    "start": s.start,
                    "end": s.end,
                    "start_secs": cutpilot_core::secs_from_ticks(s.start),
                    "end_secs": cutpilot_core::secs_from_ticks(s.end),
                    "text": s.text,
                })
            })
            .collect();
        let timeline_segments: Vec<Value> = project
            .timeline_transcript_segments(Some(aid))
            .into_iter()
            .map(|s| {
                json!({
                    "clip_id": s.clip_id,
                    "timeline_start": s.timeline_start,
                    "timeline_end": s.timeline_end,
                    "timeline_start_secs": cutpilot_core::secs_from_ticks(s.timeline_start),
                    "timeline_end_secs": cutpilot_core::secs_from_ticks(s.timeline_end),
                    "source_start": s.source_start,
                    "source_end": s.source_end,
                    "text": s.text,
                })
            })
            .collect();
        return Ok(json!({
            "asset_id": aid,
            "asset_name": asset.name,
            "transcribed": true,
            "model": tr.model,
            "language": tr.language,
            "segment_count": tr.segments.len(),
            "full_text": full_text,
            "segments": segments,
            "timeline_segments": timeline_segments,
        }));
    }

    let mapped = project.timeline_transcript_segments(None);
    let full_text = mapped
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let segments: Vec<Value> = mapped
        .iter()
        .map(|s| {
            json!({
                "asset_id": s.asset_id,
                "clip_id": s.clip_id,
                "timeline_start": s.timeline_start,
                "timeline_end": s.timeline_end,
                "timeline_start_secs": cutpilot_core::secs_from_ticks(s.timeline_start),
                "timeline_end_secs": cutpilot_core::secs_from_ticks(s.timeline_end),
                "source_start": s.source_start,
                "source_end": s.source_end,
                "text": s.text,
            })
        })
        .collect();
    let transcribed_assets: Vec<Value> = project
        .transcripts
        .iter()
        .map(|t| {
            json!({
                "asset_id": t.asset_id,
                "asset_name": project.asset(&t.asset_id).map(|a| a.name.clone()),
                "model": t.model,
                "language": t.language,
                "segment_count": t.segments.len(),
            })
        })
        .collect();
    let untranscribed_audio_assets: Vec<Value> = project
        .assets
        .iter()
        .filter(|a| {
            (a.kind == AssetKind::Audio || a.probe.as_ref().map(|p| p.has_audio).unwrap_or(false))
                && project.transcript(&a.id).is_none()
        })
        .map(|a| json!({"asset_id": a.id, "name": a.name}))
        .collect();

    Ok(json!({
        "scope": "timeline",
        "segment_count": segments.len(),
        "full_text": full_text,
        "segments": segments,
        "transcribed_assets": transcribed_assets,
        "untranscribed_audio_assets": untranscribed_audio_assets,
    }))
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
        "timeline_duration": clip.timeline_duration(),
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
        "speed": clip.speed,
        "rotation_deg": clip.rotation_deg,
        "flip_h": clip.flip_h,
        "flip_v": clip.flip_v,
        "crop": clip.crop,
        "motion": clip.motion,
        "color": clip.color,
    })
}

fn timeline_json(project: &Project) -> Value {
    let mut video_rank = 0usize;
    let tracks: Vec<Value> = project
        .tracks
        .iter()
        .enumerate()
        .map(|(index, t)| {
            let (layer_role, rank_val) = match t.kind {
                TrackKind::Video => {
                    let rank = video_rank;
                    video_rank += 1;
                    let role = if rank == 0 {
                        "base_back"
                    } else {
                        "overlay_front"
                    };
                    (role, json!(rank))
                }
                TrackKind::Audio => ("audio_mix", Value::Null),
            };
            json!({
                "index": index,
                "track_id": t.id,
                "kind": match t.kind { TrackKind::Video => "video", TrackKind::Audio => "audio" },
                "layer_role": layer_role,
                "video_layer_rank": rank_val,
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
        "duration_secs": cutpilot_core::secs_from_ticks(project.timeline_end()),
        "video_compositing_order": "back_to_front_by_index (lowest video track index / video_layer_rank 0 is the back-most base track; higher video track indices composite in front on top; use track_move to reorder)",
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
        "markers": project.markers.iter().map(|m| json!({
            "marker_id": m.id,
            "at": m.at,
            "at_secs": cutpilot_core::secs_from_ticks(m.at),
            "label": m.label,
            "color": m.color,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_project_rejects_zero_or_oversized_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = Engine::new();
        for (w, h) in [(0, 1080), (1920, 0), (8000, 1080), (1921, 1080)] {
            let res = engine.execute(Command::CreateProject {
                name: "bad".into(),
                path: tmp.path().join(format!("p-{w}-{h}")),
                width: Some(w),
                height: Some(h),
                fps_num: None,
                fps_den: None,
            });
            assert!(res.is_err(), "expected {w}x{h} to be rejected");
        }
    }

    #[test]
    fn whisper_model_arg_rejects_arbitrary_filesystem_paths() {
        assert!(validate_whisper_model_arg("base.en").is_ok());
        assert!(validate_whisper_model_arg("/etc/passwd").is_err());
        assert!(validate_whisper_model_arg("../../../etc/shadow").is_err());
    }

    #[test]
    fn cleanup_asset_caches_deletes_proxy_waveform_and_filmstrip() {
        let tmp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let proxy = tmp.path().join("cache/proxies").join(format!("{id}.mp4"));
        let wf = tmp.path().join("cache/waveforms").join(format!("{id}.wf2"));
        let fs = tmp.path().join("cache/filmstrips").join(format!("{id}.fs1"));
        for p in [&proxy, &wf, &fs] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"data").unwrap();
        }
        cleanup_asset_caches(tmp.path(), &id);
        assert!(!proxy.exists());
        assert!(!wf.exists());
        assert!(!fs.exists());
    }
}
