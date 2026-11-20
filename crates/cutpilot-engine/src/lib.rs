//! CutPilot engine: one process owns the open project. Every MCP tool and
//! every UI interaction goes through [`Engine::execute`], so the two
//! surfaces can never drift apart.
//!
//! Long-running work (export, HyperFrames renders) runs on background
//! threads that report into a shared job registry; completed HyperFrames
//! jobs are finalized (asset import + timeline insert) when the engine is
//! next polled.

pub mod engine;
pub mod error;
pub mod ipc;
pub mod jobs;
pub mod preview;
pub mod project_io;

pub use engine::Engine;
pub use error::EngineError;
pub use ipc::{IpcLogEntry, IpcServer, IpcStats};
pub use jobs::{ActiveJobSnapshot, JobInfo, JobRegistry, JobState};
pub use cutpilot_core::{Command, Outcome};
