//! Typed MCP tool parameters. Each struct mirrors one registry command;
//! `commands_execute` / `commands_batch` remain the generic escape hatch
//! that covers the entire registry.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn id_desc() -> &'static str {
    "CutPilot object id (uuid)"
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
    /// Absolute path to a video, audio (e.g. `.wav`, `.mp3`, `.flac`), or image file.
    pub path: String,
    /// When true, place the media on the timeline automatically: video lands
    /// at the end of the base video track (with a linked audio clip when the
    /// file has audio); audio-only files land on an unlocked empty/standalone
    /// audio track (creating `A2` at `0` if `A1` already holds linked video
    /// audio).
    pub auto_place: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdParams {
    #[schemars(description = "CutPilot object id (uuid)")]
    pub asset_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DetectSilenceParams {
    #[schemars(description = "CutPilot object id (uuid)")]
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
    #[schemars(description = "CutPilot object id (uuid)")]
    pub asset_id: String,
    /// Source time in seconds. Default 0.
    pub at_secs: Option<f64>,
    /// Maximum JPEG width. Default 640.
    pub max_width: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContactSheetParams {
    #[schemars(description = "CutPilot object id (uuid)")]
    pub asset_id: String,
    /// Number of frames (2..=24). Default 8.
    pub count: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaveformParams {
    #[schemars(description = "CutPilot object id (uuid)")]
    pub asset_id: String,
    /// Peak buckets (16..=131072). Default 512. Each bucket returns min, max, and rms.
    pub buckets: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MediaTranscribeParams {
    #[schemars(description = "CutPilot object id (uuid)")]
    pub asset_id: String,
    /// Whisper GGML model name (`tiny.en`, `tiny`, `base.en` [default], `base`,
    /// `small.en`, `small`, `medium.en`, `medium`, `large-v3-turbo`, `large-v3`)
    /// or path to a local `.bin` model file. Downloaded automatically on first use.
    pub model: Option<String>,
    /// ISO-639-1 language code (e.g. `en`, `es`, `fr`) or `auto`.
    pub language: Option<String>,
    /// Translate non-English speech to English. Default false.
    pub translate: Option<bool>,
    /// Also populate the caption track from the resulting transcript when finished. Default false.
    pub add_captions: Option<bool>,
    /// Block until transcription completes and return the transcript inline.
    /// Default false (returns a `job_id`; poll `render_status` / `jobs_list` then call `transcript_get`).
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TranscriptGetParams {
    /// Optional asset uuid. When provided, returns that asset's source-timed
    /// transcript (plus where it appears on the timeline). When omitted,
    /// returns the full timeline-mapped transcript accounting for all cuts and trims.
    pub asset_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TranscriptToCaptionsParams {
    /// Optional asset uuid to generate captions from; omit to convert all transcribed timeline clips.
    pub asset_id: Option<String>,
    /// Replace existing captions when true; append when false (default).
    pub replace: Option<bool>,
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
    /// New 0-based index in `project.tracks`. Video tracks composite back-to-front:
    /// lowest video track index is the back-most base layer (background); higher
    /// video track indices composite in front on top (foreground overlays).
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
    /// Static zoom/scale multiplier (`0.05..=20.0`; `1.0` = fit canvas). Works on BOTH base and overlay video clips.
    pub scale: Option<f32>,
    /// Horizontal pan/position offset in project pixels (`+` = right, `-` = left). Works on BOTH base and overlay video clips.
    pub x_offset: Option<f32>,
    /// Vertical pan/position offset in project pixels (`+` = down, `-` = up). Works on BOTH base and overlay video clips.
    pub y_offset: Option<f32>,
    pub fade_in_ticks: Option<i64>,
    pub fade_out_ticks: Option<i64>,
    /// Clockwise rotation in degrees (`-360..=360`).
    pub rotation_deg: Option<f32>,
    /// Mirror horizontally (left-right flip).
    pub flip_h: Option<bool>,
    /// Mirror vertically (upside-down flip).
    pub flip_v: Option<bool>,
    /// Normalized left edge crop fraction (`0.0..=0.45`).
    pub crop_left: Option<f32>,
    /// Normalized right edge crop fraction (`0.0..=0.45`).
    pub crop_right: Option<f32>,
    /// Normalized top edge crop fraction (`0.0..=0.45`).
    pub crop_top: Option<f32>,
    /// Normalized bottom edge crop fraction (`0.0..=0.45`).
    pub crop_bottom: Option<f32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipZoomPanParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    /// Optional quick preset: `zoom_in`, `zoom_out`, `punch_in`, `pan_right`, `pan_left`, or `none` (clears motion).
    /// Explicit start/end fields override the preset.
    pub preset: Option<String>,
    /// Starting zoom factor (`1.0..=10.0`; `1.0` = full frame, `1.3` = 130% zoomed in).
    pub start_scale: Option<f32>,
    /// Ending zoom factor (`1.0..=10.0`).
    pub end_scale: Option<f32>,
    /// Starting horizontal focus (`-1.0` = left edge, `0.0` = center, `+1.0` = right edge).
    pub start_pan_x: Option<f32>,
    /// Starting vertical focus (`-1.0` = top edge, `0.0` = center, `+1.0` = bottom edge).
    pub start_pan_y: Option<f32>,
    /// Ending horizontal focus (`-1.0..=1.0`).
    pub end_pan_x: Option<f32>,
    /// Ending vertical focus (`-1.0..=1.0`).
    pub end_pan_y: Option<f32>,
    /// Easing curve: `linear`, `ease_in_out` (default), `ease_in`, or `ease_out`.
    pub easing: Option<String>,
    /// Set `true` to remove animated zoom & pan from the clip.
    pub clear: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipSpeedParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    /// Playback speed multiplier (`0.25..=8.0`; `0.5` = half-speed slow-mo, `1.0` = normal, `2.0` = 2x fast-forward).
    /// Linked A/V partners scale together automatically.
    pub speed: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipColorParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    /// Brightness offset `-1.0..=1.0` (default `0.0`).
    pub brightness: Option<f32>,
    /// Contrast multiplier `0.0..=3.0` (default `1.0`).
    pub contrast: Option<f32>,
    /// Saturation multiplier `0.0..=3.0` (default `1.0`; `0.0` = black & white).
    pub saturation: Option<f32>,
    /// Gamma `0.1..=4.0` (default `1.0`).
    pub gamma: Option<f32>,
    /// Gaussian blur sigma `0.0..=20.0` (default `0.0` = off).
    pub blur: Option<f32>,
    /// Vignette intensity `0.0..=1.0` (default `0.0` = off).
    pub vignette: Option<f32>,
    /// Set `true` to reset all color/filter adjustments back to neutral.
    pub reset: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipDuplicateParams {
    #[schemars(description = id_desc())]
    pub clip_id: String,
    /// Optional destination start tick. Omit to place immediately after the clip (or at track end if occupied).
    pub timeline_start: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MarkerAddParams {
    /// Timeline position in ticks (`3,600,000` = 1 second).
    pub at: i64,
    /// Chapter title, edit note, or beat label.
    pub label: String,
    /// Optional `#RRGGBB` marker color (default `#F59E0B`).
    pub color: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MarkerIdParams {
    #[schemars(description = id_desc())]
    pub marker_id: String,
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
    /// Timeline zoom in UI pixels per second (0.0001..10000).
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
    /// Force software encoding when false. Default: auto-detect NVIDIA NVENC.
    pub use_nvenc: Option<bool>,
    /// Video codec: `h264` (default), `hevc`, or `av1`.
    pub codec: Option<String>,
    /// Speed/quality preset: `fast`, `balanced` (default), or `quality`.
    pub preset: Option<String>,
    /// Optional output width in pixels (even, 16..=7680). Omit to match project width
    /// (or scale proportionally when only `height` is provided).
    pub width: Option<u32>,
    /// Optional output height in pixels (even, 16..=7680). Omit to match project height
    /// (or scale proportionally when only `width` is provided).
    pub height: Option<u32>,
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
