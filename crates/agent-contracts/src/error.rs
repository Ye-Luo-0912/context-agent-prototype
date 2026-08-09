use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("context error: {0}")]
    Context(String),

    #[error("model error: {0}")]
    Model(String),

    #[error("transport error (retryable={retryable}): {message}")]
    Transport { retryable: bool, message: String },

    #[error("cancelled")]
    Cancelled,

    #[error("tool error: {0}")]
    Tool(String),

    #[error("approval denied: {0}")]
    ApprovalDenied(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("I/O error: {0}")]
    Io(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// A mutation may have partially landed and automatic rollback also
    /// failed. Callers must stop ordinary mutation and recover from a
    /// known-good checkpoint instead of treating this as a retryable error.
    #[error("recovery required: {0}")]
    RecoveryRequired(String),
}

pub type AgentResult<T> = Result<T, AgentError>;
