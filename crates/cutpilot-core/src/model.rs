//! The CutPilot project document.
//!
//! A project is a small, human-readable JSON document plus the media it
//! references. Everything in this module is plain data: no I/O, no clocks.
//! The same document drives preview, export, the native UI, and MCP.

use crate::ticks::{secs_from_ticks, Ticks, TICKS_PER_SEC};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Frames per second as an exact rational (`num/den`), so NTSC rates stay exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fps {
    pub num: u32,
    pub den: u32,
}

impl Fps {
    pub const fn new(num: u32, den: u32) -> Self {
        Self { num, den }
    }

    /// Integer frame rate, e.g. 24, 25, 30, 50, 60.
    pub const fn from_int(num: u32) -> Self {
        Self { num, den: 1 }
    }

    /// NTSC drop-frame rate from the integer rate it is derived from,
    /// e.g. `Fps::ntsc(30)` is 30000/1001 = 29.97.
    pub const fn ntsc(num: u32) -> Self {
        Self {
            num: num * 1_000,
            den: 1_001,
        }
    }

    pub fn as_f64(&self) -> f64 {
        self.num as f64 / self.den.max(1) as f64
    }

    /// Exact tick duration of one frame for supported rates.
    pub fn frame_duration_ticks(&self) -> Ticks {
        (TICKS_PER_SEC * self.den as i64) / self.num.max(1) as i64
    }

    /// Frame index for a timeline position (floor).
    pub fn frame_at(&self, ticks: Ticks) -> i64 {
        ticks / self.frame_duration_ticks()
    }

    /// Ticks for the start of a frame index.
    pub fn ticks_at_frame(&self, frame: i64) -> Ticks {
        frame * self.frame_duration_ticks()
    }

    /// `"29.97"` for NTSC rates, `"30"` otherwise.
    pub fn display(&self) -> String {
        if self.den == 1 {
            self.num.to_string()
        } else {
            let v = self.as_f64();
            format!("{v:.2}")
        }
    }

    /// Validate the rate itself.
    pub fn validate(&self) -> Result<(), String> {
        if self.num == 0 {
            return Err("fps numerator must be > 0".into());
        }
        if self.den == 0 {
            return Err("fps denominator must be > 0".into());
        }
        if self.as_f64() > 240.0 {
            return Err("fps above 240 is not supported".into());
        }
        Ok(())
    }
}

impl Default for Fps {
    fn default() -> Self {
        Self::from_int(30)
    }
}

/// Canvas and audio settings for the sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSettings {
    pub width: u32,
    pub height: u32,
    pub fps: Fps,
    pub audio_sample_rate: u32,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            width: 3840,
            height: 2160,
            fps: Fps::from_int(30),
            audio_sample_rate: 48_000,
        }
    }
}

/// What kind of file an asset is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Video,
    Audio,
    Image,
}

/// Metadata extracted by ffprobe at import time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaProbe {
    pub duration_ticks: Ticks,
    pub width: u32,
    pub height: u32,
    pub fps: Option<Fps>,
    pub has_audio: bool,
    /// Clockwise rotation metadata in degrees (0/90/180/270).
    pub rotation_deg: u32,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub sample_rate: Option<u32>,
}

impl MediaProbe {
    /// Display dimensions after rotation metadata is applied.
    pub fn oriented_size(&self) -> (u32, u32) {
        if self.rotation_deg % 180 == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

/// Provenance for assets rendered by the HyperFrames sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedAsset {
    /// Directory holding the managed `index.html` source.
    pub source_dir: PathBuf,
    /// `mov` (ProRes 4444, alpha-capable) or `mp4` (opaque).
    pub format: String,
    pub transparent: bool,
    pub width: u32,
    pub height: u32,
    pub fps: Fps,
    pub duration_ticks: Ticks,
    /// HTML of the composition at the time of the last render.
    pub html: String,
}

/// An imported or generated media file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: Uuid,
    pub kind: AssetKind,
    pub name: String,
    pub path: PathBuf,
    pub probe: Option<MediaProbe>,
    pub generated: Option<GeneratedAsset>,
}

impl Asset {
    /// Duration available for editing, in ticks. Images get a nominal hour.
    pub fn editable_duration(&self) -> Ticks {
        match self.kind {
            AssetKind::Image => TICKS_PER_SEC * 3600,
            _ => self.probe.as_ref().map(|p| p.duration_ticks).unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
}

/// A timeline track. `project.tracks` order is bottom-to-top for video.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: Uuid,
    pub kind: TrackKind,
    pub name: String,
    pub muted: bool,
    pub hidden: bool,
    pub locked: bool,
    /// Track gain, 0.0..=4.0 (1.0 = unity).
    pub volume: f32,
}

fn default_one() -> f32 {
    1.0
}

/// Easing curve for animated zoom & pan motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Easing {
    Linear,
    #[default]
    EaseInOut,
    EaseIn,
    EaseOut,
}

impl Easing {
    pub fn apply(self, u: f32) -> f32 {
        let u = u.clamp(0.0, 1.0);
        match self {
            Self::Linear => u,
            Self::EaseIn => u * u,
            Self::EaseOut => 1.0 - (1.0 - u) * (1.0 - u),
            Self::EaseInOut => u * u * (3.0 - 2.0 * u),
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "linear" => Ok(Self::Linear),
            "ease_in_out" | "easeinout" | "smooth" => Ok(Self::EaseInOut),
            "ease_in" | "easein" => Ok(Self::EaseIn),
            "ease_out" | "easeout" => Ok(Self::EaseOut),
            other => Err(format!(
                "unknown easing `{other}` (expected linear, ease_in_out, ease_in, or ease_out)"
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::EaseInOut => "ease_in_out",
            Self::EaseIn => "ease_in",
            Self::EaseOut => "ease_out",
        }
    }
}

/// Animated zoom and pan (Ken Burns / punch-in / smooth pan) across a clip.
/// Focus coordinates `pan_x` / `pan_y` are normalized `-1.0..=1.0` (`0` = center,
/// `-1` = left/top edge, `+1` = right/bottom edge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipMotion {
    pub start_scale: f32,
    pub end_scale: f32,
    #[serde(default)]
    pub start_pan_x: f32,
    #[serde(default)]
    pub start_pan_y: f32,
    #[serde(default)]
    pub end_pan_x: f32,
    #[serde(default)]
    pub end_pan_y: f32,
    #[serde(default)]
    pub easing: Easing,
}

impl ClipMotion {
    /// Evaluate `(scale, pan_x, pan_y)` at normalized progress `u ∈ [0.0, 1.0]`.
    pub fn evaluate(&self, u: f32) -> (f32, f32, f32) {
        let p = self.easing.apply(u);
        let scale = self.start_scale + (self.end_scale - self.start_scale) * p;
        let pan_x = self.start_pan_x + (self.end_pan_x - self.start_pan_x) * p;
        let pan_y = self.start_pan_y + (self.end_pan_y - self.start_pan_y) * p;
        (scale.max(1.0), pan_x.clamp(-1.0, 1.0), pan_y.clamp(-1.0, 1.0))
    }

    pub fn preset(name: &str) -> Result<Option<Self>, String> {
        match name.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "clear" => Ok(None),
            "zoom_in" | "slow_zoom_in" | "ken_burns_in" => Ok(Some(Self {
                start_scale: 1.0,
                end_scale: 1.2,
                start_pan_x: 0.0,
                start_pan_y: 0.0,
                end_pan_x: 0.0,
                end_pan_y: 0.0,
                easing: Easing::EaseInOut,
            })),
            "zoom_out" | "slow_zoom_out" | "ken_burns_out" => Ok(Some(Self {
                start_scale: 1.2,
                end_scale: 1.0,
                start_pan_x: 0.0,
                start_pan_y: 0.0,
                end_pan_x: 0.0,
                end_pan_y: 0.0,
                easing: Easing::EaseInOut,
            })),
            "punch_in" => Ok(Some(Self {
                start_scale: 1.0,
                end_scale: 1.35,
                start_pan_x: 0.0,
                start_pan_y: 0.0,
                end_pan_x: 0.0,
                end_pan_y: -0.1,
                easing: Easing::EaseOut,
            })),
            "pan_right" => Ok(Some(Self {
                start_scale: 1.2,
                end_scale: 1.2,
                start_pan_x: -0.7,
                start_pan_y: 0.0,
                end_pan_x: 0.7,
                end_pan_y: 0.0,
                easing: Easing::Linear,
            })),
            "pan_left" => Ok(Some(Self {
                start_scale: 1.2,
                end_scale: 1.2,
                start_pan_x: 0.7,
                start_pan_y: 0.0,
                end_pan_x: -0.7,
                end_pan_y: 0.0,
                easing: Easing::Linear,
            })),
            other => Err(format!(
                "unknown zoom/pan preset `{other}` (expected none, zoom_in, zoom_out, punch_in, pan_right, pan_left)"
            )),
        }
    }
}

/// Normalized edge crop fractions (`0.0..=0.45` per side; `left + right < 0.95`, `top + bottom < 0.95`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CropRect {
    #[serde(default)]
    pub left: f32,
    #[serde(default)]
    pub right: f32,
    #[serde(default)]
    pub top: f32,
    #[serde(default)]
    pub bottom: f32,
}

impl CropRect {
    pub fn is_empty(&self) -> bool {
        self.left <= 0.0001
            && self.right <= 0.0001
            && self.top <= 0.0001
            && self.bottom <= 0.0001
    }
}

/// Basic color/exposure correction and classic visual filters on a clip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColorGrade {
    /// -1.0..=1.0 (0.0 = neutral).
    #[serde(default)]
    pub brightness: f32,
    /// 0.0..=3.0 (1.0 = neutral).
    #[serde(default = "default_one")]
    pub contrast: f32,
    /// 0.0..=3.0 (1.0 = neutral; 0.0 = black & white).
    #[serde(default = "default_one")]
    pub saturation: f32,
    /// 0.1..=4.0 (1.0 = neutral).
    #[serde(default = "default_one")]
    pub gamma: f32,
    /// Gaussian blur sigma `0.0..=20.0` (0.0 = off).
    #[serde(default)]
    pub blur: f32,
    /// Vignette intensity `0.0..=1.0` (0.0 = off).
    #[serde(default)]
    pub vignette: f32,
}

impl Default for ColorGrade {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            gamma: 1.0,
            blur: 0.0,
            vignette: 0.0,
        }
    }
}

impl ColorGrade {
    pub fn is_neutral(&self) -> bool {
        self.brightness.abs() <= 0.001
            && (self.contrast - 1.0).abs() <= 0.001
            && (self.saturation - 1.0).abs() <= 0.001
            && (self.gamma - 1.0).abs() <= 0.001
            && self.blur <= 0.01
            && self.vignette <= 0.01
    }

    pub fn has_eq(&self) -> bool {
        self.brightness.abs() > 0.001
            || (self.contrast - 1.0).abs() > 0.001
            || (self.saturation - 1.0).abs() > 0.001
            || (self.gamma - 1.0).abs() > 0.001
    }
}

/// A named timeline marker (chapter point, beat, edit note).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub id: Uuid,
    pub at: Ticks,
    pub label: String,
    /// `#RRGGBB` display color.
    pub color: String,
}

/// A placed range of an asset on a track. Non-destructive: the source file is
/// never modified; `source_in`/`source_out` select the used range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub track_id: Uuid,
    pub timeline_start: Ticks,
    /// First source tick used.
    pub source_in: Ticks,
    /// One past the last source tick used.
    pub source_out: Ticks,
    /// Clips sharing a link group move and cut together (e.g. phone A/V).
    pub link_group: Option<Uuid>,
    /// 0.0..=1.0; applied to overlay video clips.
    pub opacity: f32,
    /// Scale / static zoom factor (1.0 = default fit / authored size).
    pub scale: f32,
    /// Horizontal pan/offset in project pixels, relative to centered placement.
    pub x_offset: f32,
    /// Vertical pan/offset in project pixels, relative to centered placement.
    pub y_offset: f32,
    /// Audio gain for the clip's audio, 0.0..=4.0.
    pub volume: f32,
    pub fade_in_ticks: Ticks,
    pub fade_out_ticks: Ticks,
    /// Playback speed multiplier (`0.25..=8.0`, default `1.0`).
    #[serde(default = "default_one")]
    pub speed: f32,
    /// Clockwise rotation in degrees (`-360.0..=360.0`, default `0.0`).
    #[serde(default)]
    pub rotation_deg: f32,
    /// Mirror horizontally.
    #[serde(default)]
    pub flip_h: bool,
    /// Mirror vertically.
    #[serde(default)]
    pub flip_v: bool,
    /// Optional edge crop (normalized fractions).
    #[serde(default)]
    pub crop: CropRect,
    /// Optional animated zoom & pan (Ken Burns / punch-in / pan).
    #[serde(default)]
    pub motion: Option<ClipMotion>,
    /// Basic color/exposure correction, blur, and vignette.
    #[serde(default)]
    pub color: ColorGrade,
}

impl Clip {
    pub fn effective_speed(&self) -> f32 {
        if self.speed.is_finite() && self.speed > 0.05 {
            self.speed.clamp(0.25, 8.0)
        } else {
            1.0
        }
    }

    pub fn source_duration(&self) -> Ticks {
        self.source_out - self.source_in
    }

    /// Duration occupied on the timeline, accounting for playback `speed`.
    pub fn timeline_duration(&self) -> Ticks {
        let s = self.effective_speed();
        if (s - 1.0).abs() <= 1e-4 {
            self.source_duration()
        } else {
            ((self.source_duration() as f64) / (s as f64)).round().max(1.0) as Ticks
        }
    }

    pub fn timeline_end(&self) -> Ticks {
        self.timeline_start + self.timeline_duration()
    }

    /// Convert a timeline tick delta inside this clip into source ticks.
    pub fn source_offset_for_timeline_delta(&self, tl_delta: Ticks) -> Ticks {
        let s = self.effective_speed();
        if (s - 1.0).abs() <= 1e-4 {
            tl_delta
        } else {
            ((tl_delta as f64) * (s as f64)).round() as Ticks
        }
    }

    /// True when the clip has any visual transform/effect/speed that requires
    /// the CPU/filtergraph pipeline instead of zero-copy CUDA passthrough.
    pub fn has_video_effects(&self) -> bool {
        (self.effective_speed() - 1.0).abs() > 1e-4
            || (self.scale - 1.0).abs() > 0.001
            || self.x_offset.abs() > 0.5
            || self.y_offset.abs() > 0.5
            || self.rotation_deg.abs() > 0.05
            || self.flip_h
            || self.flip_v
            || !self.crop.is_empty()
            || self.motion.is_some()
            || !self.color.is_neutral()
    }
}

/// A timed caption on the semantic caption track. Burned in at export and
/// drawn as an overlay in preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caption {
    pub id: Uuid,
    pub start: Ticks,
    pub end: Ticks,
    pub text: String,
}

/// One timestamped segment produced by Whisper transcription, in **source**
/// ticks of the asset it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start: Ticks,
    pub end: Ticks,
    pub text: String,
}

/// Stored Whisper transcript for a single media asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub asset_id: Uuid,
    pub model: String,
    pub language: String,
    pub segments: Vec<TranscriptSegment>,
}

/// A transcript segment mapped onto its current position on the timeline
/// (accounting for clip trims, splits, and moves).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTranscriptSegment {
    pub asset_id: Uuid,
    pub clip_id: Uuid,
    pub timeline_start: Ticks,
    pub timeline_end: Ticks,
    pub source_start: Ticks,
    pub source_end: Ticks,
    pub text: String,
}

/// Styling for burned-in captions (ASS/Libass semantics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptionStyle {
    pub font: String,
    /// Font size in project pixels.
    pub font_size: u32,
    /// `#RRGGBB` fill color.
    pub primary_color: String,
    /// `#RRGGBB` outline color.
    pub outline_color: String,
    pub outline: bool,
    /// Bottom margin in project pixels.
    pub margin_v: u32,
    /// ASS alignment: 2 = bottom-center, 5 = middle-center, 8 = top-center.
    pub alignment: u8,
}

impl Default for CaptionStyle {
    fn default() -> Self {
        Self {
            font: "Sans".into(),
            font_size: 64,
            primary_color: "#FFFFFF".into(),
            outline_color: "#000000".into(),
            outline: true,
            margin_v: 120,
            alignment: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    /// Fade the outgoing clip to a color and the incoming clip back in.
    FadeThroughColor,
}

impl TransitionKind {
    pub fn display(&self) -> &'static str {
        match self {
            TransitionKind::FadeThroughColor => "fade_through_color",
        }
    }
}

/// A transition anchored at a timeline position (typically a cut between two
/// base clips). `at` equal to a clip's start gives that clip a fade-in;
/// equal to a clip's end gives it a fade-out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub id: Uuid,
    pub at: Ticks,
    pub duration: Ticks,
    pub kind: TransitionKind,
    /// `#RRGGBB` color for fade-through-color.
    pub color: String,
}

/// What the UI (or an agent) currently has selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Clip(Uuid),
    Caption(Uuid),
    Transition(Uuid),
    Asset(Uuid),
    Track(Uuid),
}

/// Ephemeral viewer state. Saved alongside the project for convenience, but
/// edits to it are not undoable and do not change exported output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorState {
    pub playhead_ticks: Ticks,
    pub selection: Vec<Selection>,
    /// Timeline zoom in UI pixels per second.
    pub zoom_pixels_per_second: f32,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            playhead_ticks: 0,
            selection: Vec::new(),
            zoom_pixels_per_second: 80.0,
        }
    }
}

/// The whole project document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub schema_version: u32,
    pub id: Uuid,
    pub revision: u64,
    pub name: String,
    pub settings: ProjectSettings,
    pub assets: Vec<Asset>,
    /// Ordered bottom-to-top for video.
    pub tracks: Vec<Track>,
    pub clips: Vec<Clip>,
    pub captions: Vec<Caption>,
    pub caption_style: CaptionStyle,
    pub transitions: Vec<Transition>,
    /// Named timeline markers (chapters, beats, edit notes).
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Whisper transcripts attached to assets (persisted with the project).
    #[serde(default)]
    pub transcripts: Vec<Transcript>,
    pub editor: EditorState,
}

impl Project {
    /// A fresh 4K 30fps YouTube project with one video and one audio track.
    pub fn new(name: impl Into<String>, settings: ProjectSettings) -> Self {
        let video = Track {
            id: Uuid::new_v4(),
            kind: TrackKind::Video,
            name: "V1".into(),
            muted: false,
            hidden: false,
            locked: false,
            volume: 1.0,
        };
        let audio = Track {
            id: Uuid::new_v4(),
            kind: TrackKind::Audio,
            name: "A1".into(),
            muted: false,
            hidden: false,
            locked: false,
            volume: 1.0,
        };
        Self {
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            revision: 1,
            name: name.into(),
            settings,
            assets: Vec::new(),
            tracks: vec![video, audio],
            clips: Vec::new(),
            captions: Vec::new(),
            caption_style: CaptionStyle::default(),
            transitions: Vec::new(),
            markers: Vec::new(),
            transcripts: Vec::new(),
            editor: EditorState::default(),
        }
    }

    pub fn asset(&self, id: &Uuid) -> Option<&Asset> {
        self.assets.iter().find(|a| &a.id == id)
    }

    pub fn track(&self, id: &Uuid) -> Option<&Track> {
        self.tracks.iter().find(|t| &t.id == id)
    }

    pub fn track_mut(&mut self, id: &Uuid) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| &t.id == id)
    }

    pub fn clip(&self, id: &Uuid) -> Option<&Clip> {
        self.clips.iter().find(|c| &c.id == id)
    }

    pub fn clips_on_track(&self, track_id: &Uuid) -> Vec<&Clip> {
        let mut clips: Vec<&Clip> = self
            .clips
            .iter()
            .filter(|c| &c.track_id == track_id)
            .collect();
        clips.sort_by_key(|c| c.timeline_start);
        clips
    }

    /// The bottom-most video track (the export "base" track).
    pub fn base_video_track(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    /// End of the last clip on the timeline.
    pub fn timeline_end(&self) -> Ticks {
        self.clips
            .iter()
            .map(|c| c.timeline_end())
            .max()
            .unwrap_or(0)
    }

    /// Base-track clips under the playhead / at a tick.
    pub fn clip_at(&self, track_id: &Uuid, ticks: Ticks) -> Option<&Clip> {
        self.clips
            .iter()
            .find(|c| &c.track_id == track_id && c.timeline_start <= ticks && ticks < c.timeline_end())
    }

    /// Video clips from all visible video tracks at `ticks`, bottom to top.
    pub fn video_layers_at(&self, ticks: Ticks) -> Vec<&Clip> {
        self.tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video && !t.hidden)
            .filter_map(|t| self.clip_at(&t.id, ticks))
            .collect()
    }

    /// Captions active at a tick.
    pub fn captions_at(&self, ticks: Ticks) -> Vec<&Caption> {
        self.captions
            .iter()
            .filter(|c| c.start <= ticks && ticks < c.end)
            .collect()
    }

    /// Stored Whisper transcript for an asset, if transcribed.
    pub fn transcript(&self, asset_id: &Uuid) -> Option<&Transcript> {
        self.transcripts.iter().find(|t| &t.asset_id == asset_id)
    }

    /// Insert or replace an asset's stored Whisper transcript.
    pub fn upsert_transcript(&mut self, transcript: Transcript) {
        if let Some(slot) = self
            .transcripts
            .iter_mut()
            .find(|t| t.asset_id == transcript.asset_id)
        {
            *slot = transcript;
        } else {
            self.transcripts.push(transcript);
        }
    }

    /// Map stored asset transcripts onto their active timeline clips,
    /// accounting for trims, splits, and moves. Deduplicates linked A/V clip
    /// pairs so each spoken segment appears once per timeline occurrence.
    pub fn timeline_transcript_segments(
        &self,
        filter_asset: Option<Uuid>,
    ) -> Vec<TimelineTranscriptSegment> {
        let mut seen_link_groups = std::collections::HashSet::new();
        let mut clips: Vec<&Clip> = self
            .clips
            .iter()
            .filter(|c| filter_asset.map(|id| c.asset_id == id).unwrap_or(true))
            .filter(|c| self.transcript(&c.asset_id).is_some())
            .collect();
        // Prefer audio track clips first when deduplicating linked A/V pairs,
        // then deterministic timeline order.
        clips.sort_by_key(|c| {
            let is_audio = self
                .track(&c.track_id)
                .map(|t| t.kind == TrackKind::Audio)
                .unwrap_or(false);
            (c.timeline_start, !is_audio, c.id)
        });

        let mut out = Vec::new();
        for clip in clips {
            if let Some(group) = clip.link_group {
                if !seen_link_groups.insert(group) {
                    continue;
                }
            }
            let Some(tr) = self.transcript(&clip.asset_id) else {
                continue;
            };
            for seg in &tr.segments {
                let s = seg.start.max(clip.source_in);
                let e = seg.end.min(clip.source_out);
                if e <= s {
                    continue;
                }
                let tl_start = clip.timeline_start + (s - clip.source_in);
                let tl_end = clip.timeline_start + (e - clip.source_in);
                out.push(TimelineTranscriptSegment {
                    asset_id: clip.asset_id,
                    clip_id: clip.id,
                    timeline_start: tl_start,
                    timeline_end: tl_end,
                    source_start: s,
                    source_end: e,
                    text: seg.text.clone(),
                });
            }
        }
        out.sort_by_key(|s| (s.timeline_start, s.timeline_end));
        out
    }
}

/// Format helper used in summaries and the UI.
pub fn short_secs(ticks: Ticks) -> String {
    format!("{:.3}s", secs_from_ticks(ticks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_project_has_base_tracks() {
        let p = Project::new("test", ProjectSettings::default());
        assert_eq!(p.tracks.len(), 2);
        assert_eq!(p.tracks[0].kind, TrackKind::Video);
        assert_eq!(p.tracks[1].kind, TrackKind::Audio);
        assert!(p.base_video_track().is_some());
    }

    #[test]
    fn fps_display() {
        assert_eq!(Fps::from_int(30).display(), "30");
        assert_eq!(Fps::ntsc(30).display(), "29.97");
    }
}
