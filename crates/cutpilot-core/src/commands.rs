//! The command registry.
//!
//! Every semantic action CutPilot can perform is a [`Command`]. The registry
//! is the single source of truth shared by the MCP surface, the native UI,
//! and the headless engine. `KINDS` lists every command name; tests and docs
//! are generated against it so no action can be UI-only.

use crate::model::{
    CaptionStyle, ClipMotion, ColorGrade, CropRect, Fps, Selection, TransitionKind,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;


/// Inclusive-exclusive timeline range in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: Ticks,
    pub end: Ticks,
}

use crate::ticks::Ticks;

/// A single semantic action.
///
/// Serialized with an internal `kind` tag in snake_case, e.g.
/// `{"kind":"clip_split","clip_id":"...","at":1234}`. Agents can drive the
/// entire registry through `commands_execute` / `commands_batch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    // ---- project lifecycle ----
    /// Create a new project directory with a fresh 4K timeline.
    CreateProject {
        name: String,
        path: PathBuf,
        width: Option<u32>,
        height: Option<u32>,
        fps_num: Option<u32>,
        fps_den: Option<u32>,
    },
    /// Open a project directory or `project.json` file.
    OpenProject { path: PathBuf },
    /// Write the project to disk. `path` re-homes the project when provided.
    SaveProject { path: Option<PathBuf> },
    CloseProject {},
    /// Change canvas/fps/name settings.
    UpdateProjectSettings {
        name: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        fps_num: Option<u32>,
        fps_den: Option<u32>,
    },
    GetProject {},
    GetTimeline {},

    // ---- media ----
    /// Probe a media file (video, audio such as WAV/MP3/FLAC, or image) and
    /// add it to the bin. When `auto_place` is set, a video/image lands at the
    /// end of the base video track (with a linked audio clip when the file has
    /// audio), and audio-only files land on an unlocked empty audio track (or a
    /// new audio track at `0` when existing audio tracks hold linked video
    /// audio).
    ImportMedia {
        path: PathBuf,
        auto_place: Option<bool>,
    },
    RemoveAsset { asset_id: Uuid },
    MediaList {},
    ProbeMedia { asset_id: Uuid },
    /// Silence analysis for the asset's audio. Returns ranges in ticks.
    DetectSilence {
        asset_id: Uuid,
        threshold_db: Option<f32>,
        min_duration_secs: Option<f64>,
        padding_secs: Option<f64>,
    },
    /// JPEG thumbnail at a source time. Returns an image payload.
    MediaThumbnail {
        asset_id: Uuid,
        at_secs: Option<f64>,
        max_width: Option<u32>,
    },
    /// Contact sheet of evenly spaced frames. Returns an image payload.
    ContactSheet { asset_id: Uuid, count: Option<u32> },
    /// Min/max audio peaks. Returns normalized buckets.
    Waveform { asset_id: Uuid, buckets: Option<u32> },
    /// Extract a timestamped speech transcript from an asset's audio using
    /// embedded Whisper (`whisper.cpp`). Runs as a background job (or blocks
    /// until complete when `wait: true`).
    TranscribeMedia {
        asset_id: Uuid,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        language: Option<String>,
        #[serde(default)]
        translate: Option<bool>,
        #[serde(default)]
        add_captions: Option<bool>,
        #[serde(default)]
        wait: Option<bool>,
    },
    /// Return a stored asset transcript (source ticks) or, when `asset_id` is
    /// omitted, the full timeline-mapped transcript accounting for trims/cuts.
    GetTranscript {
        #[serde(default)]
        asset_id: Option<Uuid>,
    },
    /// Populate the caption track from stored Whisper transcript(s), mapped to
    /// current timeline clip positions.
    TranscriptToCaptions {
        #[serde(default)]
        asset_id: Option<Uuid>,
        #[serde(default)]
        replace: Option<bool>,
    },

    // ---- tracks ----
    AddTrack {
        track_kind: String,
        name: Option<String>,
    },
    UpdateTrack {
        track_id: Uuid,
        name: Option<String>,
        muted: Option<bool>,
        hidden: Option<bool>,
        locked: Option<bool>,
        volume: Option<f32>,
    },
    MoveTrack { track_id: Uuid, to_index: u32 },
    RemoveTrack { track_id: Uuid },

    // ---- clips ----
    InsertClip {
        asset_id: Uuid,
        track_id: Uuid,
        timeline_start: Ticks,
        source_in: Ticks,
        source_out: Ticks,
        link_group: Option<Uuid>,
    },
    MoveClip {
        clip_id: Uuid,
        timeline_start: Ticks,
        track_id: Option<Uuid>,
    },
    TrimClip {
        clip_id: Uuid,
        timeline_start: Option<Ticks>,
        source_in: Option<Ticks>,
        source_out: Option<Ticks>,
    },
    /// Cut a clip (and its linked partner) at a timeline position.
    SplitClip { clip_id: Uuid, at: Ticks },
    RemoveClip { clip_id: Uuid },
    UpdateClip {
        clip_id: Uuid,
        volume: Option<f32>,
        opacity: Option<f32>,
        scale: Option<f32>,
        x_offset: Option<f32>,
        y_offset: Option<f32>,
        fade_in_ticks: Option<Ticks>,
        fade_out_ticks: Option<Ticks>,
        #[serde(default)]
        rotation_deg: Option<f32>,
        #[serde(default)]
        flip_h: Option<bool>,
        #[serde(default)]
        flip_v: Option<bool>,
        #[serde(default)]
        crop: Option<CropRect>,
    },
    /// Animate zoom & pan (Ken Burns, punch-in, smooth pan) across a clip.
    /// Pass `motion: null` to clear.
    SetClipMotion {
        clip_id: Uuid,
        motion: Option<ClipMotion>,
    },
    /// Change clip playback speed (`0.25..=8.0`; `0.5` = slow-mo, `2.0` = 2x fast-forward).
    /// Linked A/V partners scale together automatically.
    SetClipSpeed {
        clip_id: Uuid,
        speed: f32,
    },
    /// Apply color/exposure correction (brightness, contrast, saturation, gamma),
    /// blur, and vignette to a video clip.
    SetClipColor {
        clip_id: Uuid,
        color: ColorGrade,
    },
    /// Duplicate a clip (and its linked A/V partner if linked) onto the timeline.
    DuplicateClip {
        clip_id: Uuid,
        timeline_start: Option<Ticks>,
    },
    /// Detach a clip from its A/V link group so video and audio can be edited independently
    /// (for J-cuts, L-cuts, or replacing camera audio with WAV).
    UnlinkClip {
        clip_id: Uuid,
    },

    // ---- markers ----
    AddMarker {
        at: Ticks,
        label: String,
        color: Option<String>,
    },
    RemoveMarker {
        marker_id: Uuid,
    },

    // ---- timeline operations ----
    /// Delete a range on the given tracks (or every unlocked track when
    /// `tracks` is null), optionally rippling later content left.
    RemoveRanges {
        start: Ticks,
        end: Ticks,
        ripple: Option<bool>,
        tracks: Option<Vec<Uuid>>,
    },

    // ---- transitions ----
    AddTransition {
        at: Ticks,
        duration: Ticks,
        transition_kind: Option<TransitionKind>,
        color: Option<String>,
    },
    RemoveTransition { transition_id: Uuid },

    // ---- captions ----
    SetCaptionStyle { style: CaptionStyle },
    AddCaption { start: Ticks, end: Ticks, text: String },
    UpdateCaption {
        caption_id: Uuid,
        start: Option<Ticks>,
        end: Option<Ticks>,
        text: Option<String>,
    },
    RemoveCaption { caption_id: Uuid },
    /// Import SRT/VTT into the caption track.
    ImportCaptions {
        path: PathBuf,
        offset_ticks: Option<Ticks>,
    },
    ExportCaptions { path: PathBuf },

    // ---- HyperFrames ----
    /// Write a managed HyperFrames composition and render it via the
    /// `hyperframes` CLI. Transparent compositions render to ProRes 4444
    /// (alpha) and belong on an overlay video track; opaque ones render to
    /// MP4 and can sit on the base track as transition screens.
    #[serde(rename = "create_hyperframes")]
    CreateHyperFrames {
        name: String,
        html: String,
        duration_ticks: Ticks,
        transparent: Option<bool>,
        overlay: Option<bool>,
        insert_at_ticks: Option<Ticks>,
        width: Option<u32>,
        height: Option<u32>,
        wait: Option<bool>,
    },
    #[serde(rename = "update_hyperframes_source")]
    UpdateHyperFramesSource { asset_id: Uuid, html: String },
    /// Re-render a managed composition. Inserting uses `clip_insert`.
    #[serde(rename = "render_hyperframes")]
    RenderHyperFrames { asset_id: Uuid, wait: Option<bool> },

    // ---- preview / editor state ----
    SetPlayhead { ticks: Ticks },
    SetSelection { selection: Vec<Selection> },
    SetZoom { pixels_per_second: f32 },
    GetEditorState {},
    /// Composite preview frame at a tick, returned as a JPEG image payload.
    PreviewFrame { ticks: Option<Ticks>, max_width: Option<u32> },

    // ---- render / jobs ----
    StartRender {
        output_path: Option<PathBuf>,
        video_bitrate_mbps: Option<f64>,
        use_nvenc: Option<bool>,
        /// Video codec: `"h264"` (default), `"hevc"`, or `"av1"`.
        #[serde(default)]
        codec: Option<String>,
        /// Speed/quality preset: `"fast"`, `"balanced"` (default), or `"quality"`.
        #[serde(default)]
        preset: Option<String>,
        /// Optional output width in pixels (even). Defaults to project width.
        #[serde(default)]
        width: Option<u32>,
        /// Optional output height in pixels (even). Defaults to project height.
        #[serde(default)]
        height: Option<u32>,
    },
    CancelJob { job_id: Uuid },
    GetJob { job_id: Uuid },
    ListJobs {},

    // ---- system / history ----
    SystemCapabilities {},
    Undo {},
    Redo {},
    HistoryList {},

    // ---- meta ----
    /// Atomically apply a batch: either every command lands or none does.
    /// Pass ranges in descending order when rippling multiple cuts.
    CommandsBatch {
        commands: Vec<Command>,
        expected_revision: Option<u64>,
        label: Option<String>,
    },
}

impl Command {
    /// Registry name for this command, matching its serde tag exactly.
    pub fn kind(&self) -> &'static str {
        // The compiler enforces that every variant is listed here; the
        // `serde_tags_match_kinds` test enforces the strings match serde.
        use Command::*;
        match self {
            CreateProject { .. } => "create_project",
            OpenProject { .. } => "open_project",
            SaveProject { .. } => "save_project",
            CloseProject { .. } => "close_project",
            UpdateProjectSettings { .. } => "update_project_settings",
            GetProject { .. } => "get_project",
            GetTimeline { .. } => "get_timeline",
            ImportMedia { .. } => "import_media",
            RemoveAsset { .. } => "remove_asset",
            MediaList { .. } => "media_list",
            ProbeMedia { .. } => "probe_media",
            DetectSilence { .. } => "detect_silence",
            MediaThumbnail { .. } => "media_thumbnail",
            ContactSheet { .. } => "contact_sheet",
            Waveform { .. } => "waveform",
            TranscribeMedia { .. } => "transcribe_media",
            GetTranscript { .. } => "get_transcript",
            TranscriptToCaptions { .. } => "transcript_to_captions",
            AddTrack { .. } => "add_track",
            UpdateTrack { .. } => "update_track",
            MoveTrack { .. } => "move_track",
            RemoveTrack { .. } => "remove_track",
            InsertClip { .. } => "insert_clip",
            MoveClip { .. } => "move_clip",
            TrimClip { .. } => "trim_clip",
            SplitClip { .. } => "split_clip",
            RemoveClip { .. } => "remove_clip",
            UpdateClip { .. } => "update_clip",
            SetClipMotion { .. } => "set_clip_motion",
            SetClipSpeed { .. } => "set_clip_speed",
            SetClipColor { .. } => "set_clip_color",
            DuplicateClip { .. } => "duplicate_clip",
            UnlinkClip { .. } => "unlink_clip",
            AddMarker { .. } => "add_marker",
            RemoveMarker { .. } => "remove_marker",
            RemoveRanges { .. } => "remove_ranges",
            AddTransition { .. } => "add_transition",
            RemoveTransition { .. } => "remove_transition",
            SetCaptionStyle { .. } => "set_caption_style",
            AddCaption { .. } => "add_caption",
            UpdateCaption { .. } => "update_caption",
            RemoveCaption { .. } => "remove_caption",
            ImportCaptions { .. } => "import_captions",
            ExportCaptions { .. } => "export_captions",
            CreateHyperFrames { .. } => "create_hyperframes",
            UpdateHyperFramesSource { .. } => "update_hyperframes_source",
            RenderHyperFrames { .. } => "render_hyperframes",
            SetPlayhead { .. } => "set_playhead",
            SetSelection { .. } => "set_selection",
            SetZoom { .. } => "set_zoom",
            GetEditorState { .. } => "get_editor_state",
            PreviewFrame { .. } => "preview_frame",
            StartRender { .. } => "start_render",
            CancelJob { .. } => "cancel_job",
            GetJob { .. } => "get_job",
            ListJobs { .. } => "list_jobs",
            SystemCapabilities { .. } => "system_capabilities",
            Undo { .. } => "undo",
            Redo { .. } => "redo",
            HistoryList { .. } => "history_list",
            CommandsBatch { .. } => "commands_batch",
        }
    }

    /// True when the command only mutates the project document in memory and
    /// can therefore be applied, undone, and dry-run without any I/O.
    pub fn is_pure_edit(&self) -> bool {
        use Command::*;
        matches!(
            self,
            UpdateProjectSettings { .. }
                | AddTrack { .. }
                | UpdateTrack { .. }
                | MoveTrack { .. }
                | RemoveTrack { .. }
                | InsertClip { .. }
                | MoveClip { .. }
                | TrimClip { .. }
                | SplitClip { .. }
                | RemoveClip { .. }
                | UpdateClip { .. }
                | SetClipMotion { .. }
                | SetClipSpeed { .. }
                | SetClipColor { .. }
                | DuplicateClip { .. }
                | UnlinkClip { .. }
                | AddMarker { .. }
                | RemoveMarker { .. }
                | RemoveRanges { .. }
                | AddTransition { .. }
                | RemoveTransition { .. }
                | SetCaptionStyle { .. }
                | AddCaption { .. }
                | UpdateCaption { .. }
                | RemoveCaption { .. }
                | TranscriptToCaptions { .. }
        )
    }

    /// True when the command mutates only viewer state (not undoable).
    pub fn is_editor_state(&self) -> bool {
        use Command::*;
        matches!(
            self,
            SetPlayhead { .. } | SetSelection { .. } | SetZoom { .. }
        )
    }
}

/// Every command name, in registry order. These strings are the serde tags
/// agents use with `commands_execute` / `commands_batch`.
pub const KINDS: &[&str] = &[
    "create_project",
    "open_project",
    "save_project",
    "close_project",
    "update_project_settings",
    "get_project",
    "get_timeline",
    "import_media",
    "remove_asset",
    "media_list",
    "probe_media",
    "detect_silence",
    "media_thumbnail",
    "contact_sheet",
    "waveform",
    "transcribe_media",
    "get_transcript",
    "transcript_to_captions",
    "add_track",
    "update_track",
    "move_track",
    "remove_track",
    "insert_clip",
    "move_clip",
    "trim_clip",
    "split_clip",
    "remove_clip",
    "update_clip",
    "set_clip_motion",
    "set_clip_speed",
    "set_clip_color",
    "duplicate_clip",
    "unlink_clip",
    "add_marker",
    "remove_marker",
    "remove_ranges",
    "add_transition",
    "remove_transition",
    "set_caption_style",
    "add_caption",
    "update_caption",
    "remove_caption",
    "import_captions",
    "export_captions",
    "create_hyperframes",
    "update_hyperframes_source",
    "render_hyperframes",
    "set_playhead",
    "set_selection",
    "set_zoom",
    "get_editor_state",
    "preview_frame",
    "start_render",
    "cancel_job",
    "get_job",
    "list_jobs",
    "system_capabilities",
    "undo",
    "redo",
    "history_list",
    "commands_batch",
];

/// Default frame rate helpers exposed to tools/docs.
pub const FPS_PRESETS: &[(&str, u32, u32)] = &[
    ("23.976", 24_000, 1_001),
    ("24", 24, 1),
    ("25", 25, 1),
    ("29.97", 30_000, 1_001),
    ("30", 30, 1),
    ("50", 50, 1),
    ("59.94", 60_000, 1_001),
    ("60", 60, 1),
];

/// Result of executing one command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    /// Project revision after the command.
    pub revision: u64,
    /// Registry name of the executed command.
    pub kind: String,
    /// Command-specific JSON summary (created ids, ranges, job state, ...).
    pub data: serde_json::Value,
    /// Binary payload (images) for MCP/UI consumption. Not serialized.
    #[serde(skip)]
    pub binary: Option<BinaryPayload>,
    /// Human-readable one-line result.
    pub message: String,
}

/// Binary content returned alongside an [`Outcome`].
#[derive(Debug, Clone)]
pub struct BinaryPayload {
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl Outcome {
    pub fn new(revision: u64, kind: &str, data: serde_json::Value, message: impl Into<String>) -> Self {
        Self {
            revision,
            kind: kind.to_string(),
            data,
            binary: None,
            message: message.into(),
        }
    }

    pub fn with_binary(mut self, mime: &str, bytes: Vec<u8>) -> Self {
        self.binary = Some(BinaryPayload {
            mime: mime.into(),
            bytes,
        });
        self
    }
}

/// Validate a `#RRGGBB` color string (used by captions and transitions).
pub fn validate_hex_color(s: &str) -> Result<(), String> {
    let valid = s.len() == 7
        && s.starts_with('#')
        && s[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(format!("invalid color `{s}`: expected #RRGGBB"))
    }
}

/// Default transition duration helper (half a second).
pub const DEFAULT_TRANSITION_TICKS: Ticks = crate::ticks::TICKS_PER_SEC / 2;

/// Convenience: build an [`Fps`] from preset components with validation.
pub fn fps_from_parts(num: u32, den: u32) -> Result<Fps, String> {
    let fps = Fps::new(num, den);
    fps.validate()?;
    Ok(fps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CaptionStyle;

    /// One instance of every registry command, in KINDS order. The parity
    /// test proves: serde tag == Command::kind() == KINDS entry, with no
    /// duplicates, and full enum coverage enforced by the compiler's
    /// exhaustive `kind()` match.
    fn sample_commands() -> Vec<Command> {
        use Command::*;
        let id = Uuid::new_v4();
        vec![
            CreateProject {
                name: "p".into(),
                path: "/tmp/p".into(),
                width: None,
                height: None,
                fps_num: None,
                fps_den: None,
            },
            OpenProject { path: "/tmp/p".into() },
            SaveProject { path: None },
            CloseProject {},
            UpdateProjectSettings {
                name: None,
                width: None,
                height: None,
                fps_num: None,
                fps_den: None,
            },
            GetProject {},
            GetTimeline {},
            ImportMedia {
                path: "/tmp/a.mp4".into(),
                auto_place: Some(true),
            },
            RemoveAsset { asset_id: id },
            MediaList {},
            ProbeMedia { asset_id: id },
            DetectSilence {
                asset_id: id,
                threshold_db: Some(-35.0),
                min_duration_secs: Some(0.6),
                padding_secs: Some(0.15),
            },
            MediaThumbnail {
                asset_id: id,
                at_secs: Some(0.0),
                max_width: Some(960),
            },
            ContactSheet { asset_id: id, count: Some(8) },
            Waveform { asset_id: id, buckets: Some(512) },
            TranscribeMedia {
                asset_id: id,
                model: Some("base.en".into()),
                language: Some("en".into()),
                translate: Some(false),
                add_captions: Some(false),
                wait: Some(false),
            },
            GetTranscript { asset_id: None },
            TranscriptToCaptions {
                asset_id: None,
                replace: Some(false),
            },
            AddTrack {
                track_kind: "video".into(),
                name: None,
            },
            UpdateTrack {
                track_id: id,
                name: None,
                muted: None,
                hidden: None,
                locked: None,
                volume: Some(1.0),
            },
            MoveTrack { track_id: id, to_index: 0 },
            RemoveTrack { track_id: id },
            InsertClip {
                asset_id: id,
                track_id: id,
                timeline_start: 0,
                source_in: 0,
                source_out: 10,
                link_group: None,
            },
            MoveClip {
                clip_id: id,
                timeline_start: 0,
                track_id: None,
            },
            TrimClip {
                clip_id: id,
                timeline_start: None,
                source_in: None,
                source_out: None,
            },
            SplitClip { clip_id: id, at: 1 },
            RemoveClip { clip_id: id },
            UpdateClip {
                clip_id: id,
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
                crop: None,
            },
            SetClipMotion {
                clip_id: id,
                motion: None,
            },
            SetClipSpeed {
                clip_id: id,
                speed: 1.0,
            },
            SetClipColor {
                clip_id: id,
                color: ColorGrade::default(),
            },
            DuplicateClip {
                clip_id: id,
                timeline_start: None,
            },
            UnlinkClip { clip_id: id },
            AddMarker {
                at: 0,
                label: "Intro".into(),
                color: Some("#4DA6FF".into()),
            },
            RemoveMarker { marker_id: id },
            RemoveRanges {
                start: 0,
                end: 1,
                ripple: Some(true),
                tracks: None,
            },
            AddTransition {
                at: 0,
                duration: 1,
                transition_kind: None,
                color: Some("#000000".into()),
            },
            RemoveTransition { transition_id: id },
            SetCaptionStyle {
                style: CaptionStyle::default(),
            },
            AddCaption {
                start: 0,
                end: 1,
                text: "hi".into(),
            },
            UpdateCaption {
                caption_id: id,
                start: None,
                end: None,
                text: None,
            },
            RemoveCaption { caption_id: id },
            ImportCaptions {
                path: "/tmp/c.srt".into(),
                offset_ticks: None,
            },
            ExportCaptions { path: "/tmp/c.srt".into() },
            CreateHyperFrames {
                name: "intro".into(),
                html: "<div></div>".into(),
                duration_ticks: 1_000,
                transparent: Some(true),
                overlay: Some(true),
                insert_at_ticks: Some(0),
                width: None,
                height: None,
                wait: Some(false),
            },
            UpdateHyperFramesSource {
                asset_id: id,
                html: "<div></div>".into(),
            },
            RenderHyperFrames { asset_id: id, wait: Some(false) },
            SetPlayhead { ticks: 0 },
            SetSelection { selection: Vec::new() },
            SetZoom {
                pixels_per_second: 80.0,
            },
            GetEditorState {},
            PreviewFrame { ticks: Some(0), max_width: Some(960) },
            StartRender {
                output_path: None,
                video_bitrate_mbps: None,
                use_nvenc: None,
                codec: None,
                preset: None,
                width: None,
                height: None,
            },
            CancelJob { job_id: id },
            GetJob { job_id: id },
            ListJobs {},
            SystemCapabilities {},
            Undo {},
            Redo {},
            HistoryList {},
            CommandsBatch {
                commands: Vec::new(),
                expected_revision: None,
                label: None,
            },
        ]
    }

    #[test]
    fn serde_tags_match_kinds_exactly() {
        let samples = sample_commands();
        assert_eq!(
            samples.len(),
            KINDS.len(),
            "sample list and KINDS table must cover the same commands"
        );
        let mut seen = std::collections::HashSet::new();
        for (i, cmd) in samples.iter().enumerate() {
            let expected = KINDS[i];
            assert_eq!(cmd.kind(), expected, "kind() mismatch at index {i}");
            let json = serde_json::to_value(cmd).unwrap();
            let tag = json["kind"].as_str().expect("internal tag");
            assert_eq!(tag, expected, "serde tag mismatch at index {i}");
            assert!(seen.insert(expected), "duplicate kind {expected}");
        }
    }

    #[test]
    fn commands_batch_round_trips_nested_commands() {
        let batch = Command::CommandsBatch {
            commands: vec![Command::SetPlayhead { ticks: 5 }],
            expected_revision: Some(1),
            label: Some("test".into()),
        };
        let json = serde_json::to_value(&batch).unwrap();
        assert_eq!(json["kind"], "commands_batch");
        assert_eq!(json["commands"][0]["kind"], "set_playhead");
        let back: Command = serde_json::from_value(json).unwrap();
        match back {
            Command::CommandsBatch { commands, .. } => {
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0].kind(), "set_playhead");
            }
            other => panic!("wrong variant {other:?}"),
        }
    }
}
