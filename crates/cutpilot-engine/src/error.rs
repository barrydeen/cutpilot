use thiserror::Error;

/// Errors surfaced by the engine (and therefore by MCP tools).
#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Core(#[from] cutpilot_core::CoreError),
    #[error(transparent)]
    Media(#[from] cutpilot_media::MediaError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Validation(String),
    #[error("command `{kind}` cannot be part of an atomic batch: {reason}")]
    Batch { kind: String, reason: String },
}

impl EngineError {
    /// Machine-readable error code for MCP clients.
    pub fn code(&self) -> &'static str {
        match self {
            EngineError::Core(e) => e.code(),
            EngineError::Media(_) => "media",
            EngineError::Io(_) => "io",
            EngineError::Validation(_) => "validation",
            EngineError::Batch { .. } => "batch",
        }
    }
}

impl From<String> for EngineError {
    fn from(s: String) -> Self {
        EngineError::Validation(s)
    }
}

impl From<&str> for EngineError {
    fn from(s: &str) -> Self {
        EngineError::Validation(s.to_string())
    }
}
