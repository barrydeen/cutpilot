//! OpenPrem core: the versioned, non-destructive edit model and the pure
//! command language shared by the MCP interface and the native UI.

pub mod commands;
pub mod edit;
pub mod error;
pub mod model;
pub mod ticks;

pub use commands::{Command, KINDS, Outcome, TimeRange};
pub use error::CoreError;
pub use model::{
    Asset, AssetKind, Caption, CaptionStyle, Clip, EditorState, Fps, GeneratedAsset, MediaProbe,
    Project, ProjectSettings, Selection, Track, TrackKind, Transition, TransitionKind,
    SCHEMA_VERSION,
};
pub use ticks::{secs_from_ticks, ticks_from_secs, Ticks, TICKS_PER_SEC};
