//! Typed MCP tool parameters. Each struct mirrors one registry command;
//! `commands_execute` / `commands_batch` remain the generic escape hatch
//! that covers the entire registry.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn id_desc() -> &'static str {
    "OpenPrem object id (uuid)"
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmptyParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectCreateParams {
    /// Display name for the project.
    pub name: String,
    /// Directory to create. Must not already contain `project.json`.
    pub path: String,
    /// Canvas width in pixels (even). Default 3840.
    pub width: Option<u32>,
    /// Canvas height in pixels (even). Default 2160.
    pub height: Option<u32>,
    /// Frames-per-second numerator. Default 30.
    pub fps_num: Option<u32>,
    /// Frames-per-second denominator. Use 1001 for NTSC rates (e.g. 30000/1001 = 29.97).
    pub fps_den: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PathParams {
    /// Project directory or path to `project.json`.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveParams {
    /// Optional new project directory to re-home into. Omit to save in place.
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateSettingsParams {
    pub name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps_num: Option<u32>,
    pub fps_den: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MediaImportParams {
    /// Absolute path to a video/audio/image file.
    pub path: String,
    /// When true, place the media on the timeline automatically: video lands
    /// at the end of the base video track (with a linked audio clip when the
    /// file has audio); audio-only files land at the end of the first audio
    /// track.
    pub auto_place: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdParams {
    #[schemars(description = "OpenPrem object id (uuid)")]
    pub asset_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DetectSilenceParams {
    #[schemars(description = "OpenPrem object id (uuid)")]
    pub asset_id: String,
    /// Noise floor in dB. Default -35.
    pub threshold_db: Option<f32>,
    /// Minimum silence length in seconds. Default 0.6.
    pub min_duration_secs: Option<f64>,
    /// Padding kept on each side of detected silence, in seconds. Default 0.15.
    pub padding_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThumbnailParams {
    #[schemars(description = "OpenPrem object id (uuid)")]
    pub asset_id: String,
    /// Source time in seconds. Default 0.
    pub at_secs: Option<f64>,
    /// Maximum JPEG width. Default 640.
    pub max_width: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContactSheetParams {
    #[schemars(description = "OpenPrem object id (uuid)")]
    pub asset_id: String,
    /// Number of frames (2..=24). Default 8.
    pub count: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaveformParams {
    #[schemars(description = "OpenPrem object id (uuid)")]
    pub asset_id: String,
    /// Peak buckets (16..=4096). Default 512.
    pub buckets: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrackAddParams {
    /// `video` or `audio`.
    pub track_kind: String,
    /// Optional display name; defaults to the next V#/A#.
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrackUpdateParams {
    #[schemars(description = id_desc())]
    pub track_id: String,
    pub name: Option<String>,
    pub muted: Option<bool>,
    pub hidden: Option<bool>,
    pub locked: Option<bool>,
    /// Track gain 0..4 (1 = unity).
    pub volume: Option<f32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrackMoveParams {
    #[schemars(description = id_desc())]
    pub track_id: String,
    /// New index in the track list (0-based). Video tracks composite
    /// bottom-to-top, so index 0 is the base track.
    pub to_index: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdOnlyParams {
    #[schemars(description = id_desc())]
    pub track_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipInsertParams {
    #[schemars(description = id_desc())]
    pub asset_id: String,
    #[schemars(description = id_desc())]
    pub track_id: String,
    /// Timeline position in ticks (3,600,000 ticks = 1 second).
    pub timeline_start: i64,
    /// First source tick used.
    pub source_in: i64,
    /// One past the last source tick used.
    pub source_out: i64,
    /// Optional link group uuid; clips sharing a group move/cut together.
    pub link_group: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipMoveParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    pub timeline_start: i64,
    /// Optional target track of the same kind.
    pub track_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipTrimParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    pub timeline_start: Option<i64>,
    pub source_in: Option<i64>,
    pub source_out: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipSplitParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    /// Timeline tick to cut at. Linked clips cut too.
    pub at: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipIdParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipUpdateParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    pub volume: Option<f32>,
    pub opacity: Option<f32>,
    pub scale: Option<f32>,
    /// Overlay offset in project pixels.
    pub x_offset: Option<f32>,
    pub y_offset: Option<f32>,
    pub fade_in_ticks: Option<i64>,
    pub fade_out_ticks: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveRangesParams {
    /// Range start in ticks.
    pub start: i64,
    /// Range end in ticks (exclusive).
    pub end: i64,
    /// When true, later content shifts left to close the gap. Default true.
    pub ripple: Option<bool>,
    /// Restrict to specific tracks; omit for every unlocked track.
    pub tracks: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TransitionAddParams {
    /// Timeline tick of the cut (a clip start or end) to fade through.
    pub at: i64,
    /// Fade duration in ticks. Default half a second.
    pub duration: Option<i64>,
    /// `fade_through_color` (default).
    pub transition_kind: Option<String>,
    /// `#RRGGBB`. Default `#000000`.
    pub color: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TransitionIdParams {
    #[schemars(description = id_desc())]
    pub transition_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptionStyleParams {
    /// Font family name resolved by fontconfig at export.
    pub font: String,
    /// Font size in project pixels (8..512).
    pub font_size: u32,
    /// Fill color `#RRGGBB`.
    pub primary_color: String,
    /// Outline color `#RRGGBB`.
    pub outline_color: String,
    /// Draw an outline.
    pub outline: bool,
    /// Bottom margin in project pixels.
    pub margin_v: u32,
    /// ASS alignment (2 = bottom center, 5 = middle, 8 = top).
    pub alignment: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptionAddParams {
    pub start: i64,
    pub end: i64,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptionUpdateParams {
    #[schemars(description = id_desc())]
    pub caption_id: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptionIdParams {
    #[schemars(description = id_desc())]
    pub caption_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportCaptionsParams {
    /// Path to an `.srt` or `.vtt` file.
    pub path: String,
    /// Offset added to every cue, in ticks.
    pub offset_ticks: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportCaptionsParams {
    /// Destination `.srt` path.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HyperFramesCreateParams {
    /// Composition name; becomes the managed source directory.
    pub name: String,
    /// Full HyperFrames `index.html` source. Author per the HyperFrames HTML
    /// schema (composition root with data-composition-id/data-fps/
    /// data-width/data-height/data-duration and seekable animations). Leave
    /// html/body backgrounds unset for transparent overlays.
    pub html: String,
    /// Composition duration in ticks.
    pub duration_ticks: i64,
    /// Render ProRes 4444 with alpha (overlay on V2). Default true.
    pub transparent: Option<bool>,
    /// Place on an overlay video track (true) or the base track (false).
    /// Default true.
    pub overlay: Option<bool>,
    /// Timeline tick to insert the rendered clip at. Omit to skip insertion.
    pub insert_at_ticks: Option<i64>,
    /// Composition width. Default: project width.
    pub width: Option<u32>,
    /// Composition height. Default: project height.
    pub height: Option<u32>,
    /// Block until the render finishes. Default false: a job is started and
    /// the clip is inserted automatically when `jobs_list`/`render_status`
    /// next observes completion.
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HyperFramesUpdateParams {
    #[schemars(description = id_desc())]
    pub asset_id: String,
    /// New full `index.html` source.
    pub html: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HyperFramesRenderParams {
    #[schemars(description = id_desc())]
    pub asset_id: String,
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PreviewSeekParams {
    /// Playhead position in ticks.
    pub ticks: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionParam {
    Clip(String),
    Caption(String),
    Transition(String),
    Asset(String),
    Track(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditorSelectParams {
    /// Objects to select.
    pub selection: Vec<SelectionParam>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ZoomParams {
    /// Timeline zoom in UI pixels per second (1..5000).
    pub pixels_per_second: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PreviewFrameParams {
    /// Timeline tick; defaults to the playhead.
    pub ticks: Option<i64>,
    /// JPEG width cap. Default 960.
    pub max_width: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenderStartParams {
    /// Destination MP4 path. Default `<project>/export-<timestamp>.mp4`.
    pub output_path: Option<String>,
    /// Video bitrate in Mbps; default scales with resolution/fps (~45 at 4K30).
    pub video_bitrate_mbps: Option<f64>,
    /// Force software x264 encoding. Default: auto-detect NVENC.
    pub use_nvenc: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobIdParams {
    /// Job id from `render_start` / `hyperframes_create`.
    pub job_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommandsExecuteParams {
    /// One registry command as tagged JSON, e.g.
    /// `{"kind":"set_playhead","ticks":0}`. The full command list is in the
    /// `system_capabilities` output.
    pub command: Value,
    /// Fail instead of mutating when this does not equal the current
    /// project revision.
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommandsBatchParams {
    /// Commands applied atomically: either all land or none do. Only pure
    /// project edits (tracks/clips/timeline/transitions/captions/settings)
    /// and editor state are batchable. When cutting multiple ranges, pass
    /// them in descending order.
    pub commands: Vec<Value>,
    /// Fail instead of mutating when this does not equal the current
    /// project revision.
    pub expected_revision: Option<u64>,
    /// Undo label for the batch.
    pub label: Option<String>,
}
