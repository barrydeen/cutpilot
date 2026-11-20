//! The MCP tool surface. Every tool builds a registry [`Command`] and runs
//! it on the engine — the same commands the UI uses — so no action can be
//! UI-only. Long operations return job ids; tool responses carry a JSON
//! summary plus inline images when relevant.

use crate::params::*;
use base64::prelude::{Engine as _, BASE64_STANDARD};
use openprem_core::{
    CaptionStyle, Command, Outcome, Selection, TransitionKind,
};
use openprem_engine::Engine;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, ErrorData as McpError,
    },
    tool, tool_handler, tool_router,
    transport::stdio,
    ServiceExt, ServerHandler,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct OpenPremServer {
    engine: Arc<Mutex<Engine>>,
    tool_router: ToolRouter<Self>,
}

impl Clone for OpenPremServer {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            tool_router: self.tool_router.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn uuid_field(s: &str, what: &str) -> Result<Uuid, McpError> {
    Uuid::parse_str(s).map_err(|_| {
        McpError::invalid_params(format!("invalid {what} id `{s}` (expected uuid)"), None)
    })
}

fn path_field(s: &str, what: &str) -> std::path::PathBuf {
    let _ = what;
    std::path::PathBuf::from(s)
}

fn decode_command(value: Value) -> Result<Command, McpError> {
    let kind = value
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or_default()
        .to_string();
    serde_json::from_value(value).map_err(|e| {
        McpError::invalid_params(format!("invalid `{kind}` command: {e}"), None)
    })
}

fn selection_from(param: SelectionParam) -> Result<Selection, McpError> {
    Ok(match param {
        SelectionParam::Clip(id) => Selection::Clip(uuid_field(&id, "clip")?),
        SelectionParam::Caption(id) => Selection::Caption(uuid_field(&id, "caption")?),
        SelectionParam::Transition(id) => Selection::Transition(uuid_field(&id, "transition")?),
        SelectionParam::Asset(id) => Selection::Asset(uuid_field(&id, "asset")?),
        SelectionParam::Track(id) => Selection::Track(uuid_field(&id, "track")?),
    })
}

fn transition_kind_from(s: &str) -> Result<TransitionKind, McpError> {
    match s {
        "fade_through_color" => Ok(TransitionKind::FadeThroughColor),
        other => Err(McpError::invalid_params(
            format!("unknown transition kind `{other}`"),
            None,
        )),
    }
}

fn style_from(p: &CaptionStyleParams) -> CaptionStyle {
    CaptionStyle {
        font: p.font.clone(),
        font_size: p.font_size,
        primary_color: p.primary_color.clone(),
        outline_color: p.outline_color.clone(),
        outline: p.outline,
        margin_v: p.margin_v,
        alignment: p.alignment,
    }
}

/// Run a command on the engine, blocking off the async runtime. Engine
/// failures are tool-level errors (visible to the agent); only transport
/// problems become protocol errors.
async fn exec(
    engine: &Arc<Mutex<Engine>>,
    cmd: Command,
    expected_revision: Option<u64>,
) -> Result<CallToolResult, McpError> {
    let engine = engine.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let mut guard = engine.lock().map_err(|e| {
            McpError::internal_error(format!("engine lock poisoned: {e}"), None)
        })?;
        let result = guard.execute_checked(cmd, expected_revision);
        match result {
            Ok(outcome) => Ok(outcome_to_result(outcome)),
            Err(e) => Ok(tool_error(e.to_string(), e.code().to_string())),
        }
    })
    .await
    .map_err(|e| McpError::internal_error(format!("engine task failed: {e}"), None))?;
    joined
}

fn tool_error(message: String, code: String) -> CallToolResult {
    let payload = json!({"error": message, "code": code});
    CallToolResult::error(vec![ContentBlock::text(
        serde_json::to_string_pretty(&payload).unwrap_or(message),
    )])
}

fn outcome_to_result(outcome: Outcome) -> CallToolResult {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if let Some(bin) = &outcome.binary {
        blocks.push(ContentBlock::image(
            BASE64_STANDARD.encode(&bin.bytes),
            bin.mime.clone(),
        ));
    }
    let summary = json!({
        "kind": outcome.kind,
        "revision": outcome.revision,
        "message": outcome.message,
        "data": outcome.data,
    });
    blocks.push(ContentBlock::text(
        serde_json::to_string_pretty(&summary).expect("outcome is serializable"),
    ));
    CallToolResult::success(blocks)
}

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

#[tool_router]
impl OpenPremServer {
    pub fn new(engine: Arc<Mutex<Engine>>) -> Self {
        Self {
            engine,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Report OpenPrem capabilities: ffmpeg/ffprobe/NVENC/Node availability, GPU name, the full command registry, and schema version.")]
    async fn system_capabilities(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::SystemCapabilities {}, None).await
    }

    #[tool(description = "Create a new project directory with a fresh timeline (default 3840x2160 @ 30fps, one video + one audio track) and open it in the engine.")]
    async fn project_create(
        &self,
        Parameters(p): Parameters<ProjectCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::CreateProject {
            name: p.name,
            path: path_field(&p.path, "project"),
            width: p.width,
            height: p.height,
            fps_num: p.fps_num,
            fps_den: p.fps_den,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Open an existing OpenPrem project directory (or its project.json path).")]
    async fn project_open(
        &self,
        Parameters(p): Parameters<PathParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::OpenProject { path: path_field(&p.path, "project") }, None).await
    }

    #[tool(description = "Write the project to disk. Pass `path` to re-home it into a new directory.")]
    async fn project_save(
        &self,
        Parameters(p): Parameters<SaveParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::SaveProject { path: p.path.map(|s| path_field(&s, "project")) }, None).await
    }

    #[tool(description = "Close the current project (unsaved changes are lost; autosave usually covers you).")]
    async fn project_close(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::CloseProject {}, None).await
    }

    #[tool(description = "Full project document JSON: settings, assets, tracks, clips, captions, transitions, editor state.")]
    async fn project_get(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::GetProject {}, None).await
    }

    #[tool(description = "Update project name, canvas size, or frame rate (fps_num/fps_den, e.g. 30000/1001).")]
    async fn project_update_settings(
        &self,
        Parameters(p): Parameters<UpdateSettingsParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::UpdateProjectSettings {
            name: p.name,
            width: p.width,
            height: p.height,
            fps_num: p.fps_num,
            fps_den: p.fps_den,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Timeline summary: duration, tracks with their clips, transitions, captions, and caption style.")]
    async fn timeline_get(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::GetTimeline {}, None).await
    }

    #[tool(description = "Probe a media file (video/audio/image) and add it to the bin. With auto_place=true it also lands on the timeline: video at the end of the base track (linked audio clip included), audio-only at the end of the first audio track. Returns the asset id and probe metadata.")]
    async fn media_import(
        &self,
        Parameters(p): Parameters<MediaImportParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::ImportMedia {
            path: path_field(&p.path, "media"),
            auto_place: p.auto_place,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Remove an asset from the bin. Fails while timeline clips still use it.")]
    async fn media_remove(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        exec(&self.engine, Command::RemoveAsset { asset_id: id }, None).await
    }

    #[tool(description = "List every asset with duration, probe metadata, and HyperFrames provenance.")]
    async fn media_list(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::MediaList {}, None).await
    }

    #[tool(description = "Re-probe one asset and return fresh stream metadata.")]
    async fn media_probe(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        exec(&self.engine, Command::ProbeMedia { asset_id: id }, None).await
    }

    #[tool(description = "Detect silent ranges in an asset's audio. Returns tick ranges (with padding applied) ready for timeline_remove_ranges. Typical dead-space workflow: media_detect_silence -> timeline_remove_ranges (or commands_batch).")]
    async fn media_detect_silence(
        &self,
        Parameters(p): Parameters<DetectSilenceParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        let cmd = Command::DetectSilence {
            asset_id: id,
            threshold_db: p.threshold_db,
            min_duration_secs: p.min_duration_secs,
            padding_secs: p.padding_secs,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "JPEG thumbnail of an asset at a source time. Returns an image.")]
    async fn media_thumbnail(
        &self,
        Parameters(p): Parameters<ThumbnailParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        let cmd = Command::MediaThumbnail {
            asset_id: id,
            at_secs: p.at_secs,
            max_width: p.max_width,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Contact sheet of evenly spaced frames from an asset. Returns an image.")]
    async fn media_contact_sheet(
        &self,
        Parameters(p): Parameters<ContactSheetParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        exec(&self.engine, Command::ContactSheet { asset_id: id, count: p.count }, None).await
    }

    #[tool(description = "Audio waveform min/max peaks for an asset.")]
    async fn media_waveform(
        &self,
        Parameters(p): Parameters<WaveformParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        exec(&self.engine, Command::Waveform { asset_id: id, buckets: p.buckets }, None).await
    }

    #[tool(description = "Add a track (`video` or `audio`). Video tracks composite bottom-to-top; the first video track is the export base.")]
    async fn track_add(
        &self,
        Parameters(p): Parameters<TrackAddParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::AddTrack {
            track_kind: p.track_kind,
            name: p.name,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Update track name, mute, hide, lock, or gain.")]
    async fn track_update(
        &self,
        Parameters(p): Parameters<TrackUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.track_id, "track")?;
        let cmd = Command::UpdateTrack {
            track_id: id,
            name: p.name,
            muted: p.muted,
            hidden: p.hidden,
            locked: p.locked,
            volume: p.volume,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Reorder a track (0-based index).")]
    async fn track_move(
        &self,
        Parameters(p): Parameters<TrackMoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.track_id, "track")?;
        exec(&self.engine, Command::MoveTrack { track_id: id, to_index: p.to_index }, None).await
    }

    #[tool(description = "Remove an empty track.")]
    async fn track_remove(
        &self,
        Parameters(p): Parameters<IdOnlyParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.track_id, "track")?;
        exec(&self.engine, Command::RemoveTrack { track_id: id }, None).await
    }

    #[tool(description = "Place an asset range on a track. Times are ticks (3,600,000 per second). source_in/source_out select the used part of the source; the file is never modified.")]
    async fn clip_insert(
        &self,
        Parameters(p): Parameters<ClipInsertParams>,
    ) -> Result<CallToolResult, McpError> {
        let asset_id = uuid_field(&p.asset_id, "asset")?;
        let track_id = uuid_field(&p.track_id, "track")?;
        let link_group = match p.link_group {
            Some(s) => Some(uuid_field(&s, "link_group")?),
            None => None,
        };
        let cmd = Command::InsertClip {
            asset_id,
            track_id,
            timeline_start: p.timeline_start,
            source_in: p.source_in,
            source_out: p.source_out,
            link_group,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Move a clip on the timeline. Linked clips (same link_group) move with it.")]
    async fn clip_move(
        &self,
        Parameters(p): Parameters<ClipMoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.clip_id, "clip")?;
        let track_id = match p.track_id {
            Some(s) => Some(uuid_field(&s, "track")?),
            None => None,
        };
        let cmd = Command::MoveClip {
            clip_id: id,
            timeline_start: p.timeline_start,
            track_id,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Trim a clip by adjusting its timeline start and/or source in/out ticks.")]
    async fn clip_trim(
        &self,
        Parameters(p): Parameters<ClipTrimParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.clip_id, "clip")?;
        let cmd = Command::TrimClip {
            clip_id: id,
            timeline_start: p.timeline_start,
            source_in: p.source_in,
            source_out: p.source_out,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Cut a clip at a timeline tick into two clips. Linked clips cut at the same position.")]
    async fn clip_split(
        &self,
        Parameters(p): Parameters<ClipSplitParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.clip_id, "clip")?;
        exec(&self.engine, Command::SplitClip { clip_id: id, at: p.at }, None).await
    }

    #[tool(description = "Remove a clip from the timeline.")]
    async fn clip_remove(
        &self,
        Parameters(p): Parameters<ClipIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.clip_id, "clip")?;
        exec(&self.engine, Command::RemoveClip { clip_id: id }, None).await
    }

    #[tool(description = "Update clip volume, opacity, scale, overlay position, or audio fades.")]
    async fn clip_update(
        &self,
        Parameters(p): Parameters<ClipUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.clip_id, "clip")?;
        let cmd = Command::UpdateClip {
            clip_id: id,
            volume: p.volume,
            opacity: p.opacity,
            scale: p.scale,
            x_offset: p.x_offset,
            y_offset: p.y_offset,
            fade_in_ticks: p.fade_in_ticks,
            fade_out_ticks: p.fade_out_ticks,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Delete a tick range, optionally rippling later content left. With `tracks` omitted it applies to every unlocked track (linked A/V cuts together). For multiple ranges pass descending order via commands_batch.")]
    async fn timeline_remove_ranges(
        &self,
        Parameters(p): Parameters<RemoveRangesParams>,
    ) -> Result<CallToolResult, McpError> {
        let tracks = match p.tracks {
            Some(list) => {
                let mut ids = Vec::new();
                for t in list {
                    ids.push(uuid_field(&t, "track")?);
                }
                Some(ids)
            }
            None => None,
        };
        let cmd = Command::RemoveRanges {
            start: p.start,
            end: p.end,
            ripple: p.ripple,
            tracks,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Add a fade-through-color transition at a cut (a base clip start/end tick).")]
    async fn transition_add(
        &self,
        Parameters(p): Parameters<TransitionAddParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = match p.transition_kind.as_deref() {
            Some(s) => Some(transition_kind_from(s)?),
            None => None,
        };
        let cmd = Command::AddTransition {
            at: p.at,
            duration: p
                .duration
                .unwrap_or(openprem_core::commands::DEFAULT_TRANSITION_TICKS),
            transition_kind: kind,
            color: p.color,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Remove a transition.")]
    async fn transition_remove(
        &self,
        Parameters(p): Parameters<TransitionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.transition_id, "transition")?;
        exec(&self.engine, Command::RemoveTransition { transition_id: id }, None).await
    }

    #[tool(description = "Set the burned-in caption style (font, size, colors, margin, alignment).")]
    async fn caption_style_set(
        &self,
        Parameters(p): Parameters<CaptionStyleParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::SetCaptionStyle { style: style_from(&p) };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Add a timed caption (burned in at export, drawn in preview).")]
    async fn caption_add(
        &self,
        Parameters(p): Parameters<CaptionAddParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::AddCaption { start: p.start, end: p.end, text: p.text }, None).await
    }

    #[tool(description = "Update a caption's timing or text.")]
    async fn caption_update(
        &self,
        Parameters(p): Parameters<CaptionUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.caption_id, "caption")?;
        let cmd = Command::UpdateCaption {
            caption_id: id,
            start: p.start,
            end: p.end,
            text: p.text,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Remove a caption.")]
    async fn caption_remove(
        &self,
        Parameters(p): Parameters<CaptionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.caption_id, "caption")?;
        exec(&self.engine, Command::RemoveCaption { caption_id: id }, None).await
    }

    #[tool(description = "Import an .srt or .vtt file into the caption track (with optional tick offset). One undo step.")]
    async fn captions_import(
        &self,
        Parameters(p): Parameters<ImportCaptionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::ImportCaptions {
            path: path_field(&p.path, "captions"),
            offset_ticks: p.offset_ticks,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Export the caption track to an .srt file.")]
    async fn captions_export(
        &self,
        Parameters(p): Parameters<ExportCaptionsParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::ExportCaptions { path: path_field(&p.path, "captions") }, None).await
    }

    #[tool(description = "Author a HyperFrames composition (plain HTML with seekable animations) and render it via the `hyperframes` CLI. Transparent overlays render to ProRes 4444 with alpha; opaque clips render to MP4. With wait=false (default) a job starts and the clip auto-inserts when a status call observes completion. Node 22+ and the hyperframes CLI are required.")]
    async fn hyperframes_create(
        &self,
        Parameters(p): Parameters<HyperFramesCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::CreateHyperFrames {
            name: p.name,
            html: p.html,
            duration_ticks: p.duration_ticks,
            transparent: p.transparent,
            overlay: p.overlay,
            insert_at_ticks: p.insert_at_ticks,
            width: p.width,
            height: p.height,
            wait: p.wait,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Replace a managed composition's index.html source (re-render with hyperframes_render).")]
    async fn hyperframes_update(
        &self,
        Parameters(p): Parameters<HyperFramesUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        exec(&self.engine, Command::UpdateHyperFramesSource { asset_id: id, html: p.html }, None).await
    }

    #[tool(description = "Re-render a managed composition to its cached output file.")]
    async fn hyperframes_render(
        &self,
        Parameters(p): Parameters<HyperFramesRenderParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.asset_id, "asset")?;
        exec(&self.engine, Command::RenderHyperFrames { asset_id: id, wait: p.wait }, None).await
    }

    #[tool(description = "Move the playhead (viewer state; not undoable).")]
    async fn preview_seek(
        &self,
        Parameters(p): Parameters<PreviewSeekParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::SetPlayhead { ticks: p.ticks }, None).await
    }

    #[tool(description = "Set the UI selection (viewer state).")]
    async fn editor_select(
        &self,
        Parameters(p): Parameters<EditorSelectParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut selection = Vec::new();
        for s in p.selection {
            selection.push(selection_from(s)?);
        }
        exec(&self.engine, Command::SetSelection { selection }, None).await
    }

    #[tool(description = "Set timeline zoom in UI pixels per second (viewer state).")]
    async fn editor_set_zoom(
        &self,
        Parameters(p): Parameters<ZoomParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::SetZoom { pixels_per_second: p.pixels_per_second }, None).await
    }

    #[tool(description = "Current playhead, selection, and zoom.")]
    async fn editor_state_get(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::GetEditorState {}, None).await
    }

    #[tool(description = "Composite preview frame (base + overlays + captions text listed) at a tick, returned as a JPEG image. Use this to visually verify edits.")]
    async fn preview_frame(
        &self,
        Parameters(p): Parameters<PreviewFrameParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::PreviewFrame {
            ticks: p.ticks,
            max_width: p.max_width,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Start the final MP4 export as a background job (NVENC when available, automatic x264 fallback, AAC audio, faststart). Poll render_status until done. The output is ffprobe-verified before the job reports success.")]
    async fn render_start(
        &self,
        Parameters(p): Parameters<RenderStartParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = Command::StartRender {
            output_path: p.output_path.map(|s| path_field(&s, "output")),
            video_bitrate_mbps: p.video_bitrate_mbps,
            use_nvenc: p.use_nvenc,
        };
        exec(&self.engine, cmd, None).await
    }

    #[tool(description = "Check a job (render or HyperFrames). Also finalizes completed HyperFrames renders into the timeline.")]
    async fn render_status(
        &self,
        Parameters(p): Parameters<JobIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.job_id, "job")?;
        exec(&self.engine, Command::GetJob { job_id: id }, None).await
    }

    #[tool(description = "Request cancellation of a running job.")]
    async fn render_cancel(
        &self,
        Parameters(p): Parameters<JobIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = uuid_field(&p.job_id, "job")?;
        exec(&self.engine, Command::CancelJob { job_id: id }, None).await
    }

    #[tool(description = "List recent jobs with progress and results.")]
    async fn jobs_list(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::ListJobs {}, None).await
    }

    #[tool(description = "Undo the last edit step (one revision).")]
    async fn history_undo(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::Undo {}, None).await
    }

    #[tool(description = "Redo one undone step.")]
    async fn history_redo(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::Redo {}, None).await
    }

    #[tool(description = "Undo stack labels, most recent last.")]
    async fn history_list(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        exec(&self.engine, Command::HistoryList {}, None).await
    }

    #[tool(description = "Execute any registry command by tagged JSON, e.g. {\"kind\":\"set_playhead\",\"ticks\":0}. Optionally with expected_revision for optimistic concurrency. The full registry is in system_capabilities.")]
    async fn commands_execute(
        &self,
        Parameters(p): Parameters<CommandsExecuteParams>,
    ) -> Result<CallToolResult, McpError> {
        let cmd = decode_command(p.command)?;
        exec(&self.engine, cmd, p.expected_revision).await
    }

    #[tool(description = "Apply a list of registry commands atomically: either every command lands or none does. Pure edits and editor state only. Pass expected_revision to guard against concurrent edits.")]
    async fn commands_batch(
        &self,
        Parameters(p): Parameters<CommandsBatchParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut commands = Vec::new();
        for value in p.commands {
            commands.push(decode_command(value)?);
        }
        let cmd = Command::CommandsBatch {
            commands,
            expected_revision: p.expected_revision,
            label: p.label,
        };
        exec(&self.engine, cmd, None).await
    }
}

#[tool_handler(name = "openprem", version = "0.1.0", instructions = "OpenPrem is an MCP-first video editor for YouTube. \
Typical workflow: project_create -> media_import (auto_place) -> media_detect_silence -> \
timeline_remove_ranges or commands_batch to cut dead space -> caption_add / captions_import \
for captions -> transition_add and hyperframes_create for transitions and animated overlays \
-> preview_frame to inspect -> render_start + render_status polling to export an MP4. All \
times are integer ticks: 3,600,000 ticks = 1 second (frame-exact for 29.97/23.976 too). Use \
commands_execute / commands_batch for anything the named tools do not cover; the registry is \
in system_capabilities.")]
impl ServerHandler for OpenPremServer {}

/// Run the stdio MCP server. Never write to stdout here — it is the protocol.
pub async fn serve() -> anyhow::Result<()> {
    let engine = Arc::new(Mutex::new(Engine::new()));
    let service = OpenPremServer::new(engine);
    let running = service.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
