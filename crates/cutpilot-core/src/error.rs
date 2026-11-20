use thiserror::Error;

/// Errors produced while validating or applying an edit command.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("no project is open")]
    NoProject,
    #[error("invalid id: {0}")]
    InvalidId(String),
    #[error("invalid time: {0}")]
    InvalidTime(String),
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("project revision mismatch: client expected {expected}, project is at {actual}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("command `{kind}` is not a pure project edit")]
    NotPure { kind: &'static str },
}

impl CoreError {
    /// Machine-readable error code for the MCP surface.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::NoProject => "no_project",
            CoreError::InvalidId(_) => "invalid_id",
            CoreError::InvalidTime(_) => "invalid_time",
            CoreError::Validation(_) => "validation",
            CoreError::RevisionMismatch { .. } => "revision_mismatch",
            CoreError::NotPure { .. } => "not_pure",
        }
    }
}
