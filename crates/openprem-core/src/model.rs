//! The OpenPrem project document.
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
    /// Extra scale for overlay video clips (1.0 = authored size).
    pub scale: f32,
    /// Overlay offset in project pixels, relative to centered placement.
    pub x_offset: f32,
    pub y_offset: f32,
    /// Audio gain for the clip's audio, 0.0..=4.0.
    pub volume: f32,
    pub fade_in_ticks: Ticks,
    pub fade_out_ticks: Ticks,
}

impl Clip {
    pub fn source_duration(&self) -> Ticks {
        self.source_out - self.source_in
    }

    pub fn timeline_end(&self) -> Ticks {
        self.timeline_start + self.source_duration()
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
