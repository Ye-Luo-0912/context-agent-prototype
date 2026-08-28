use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("context error: {0}")]
    Context(String),

    #[error("model error: {0}")]
    Model(String),

    /// The provider completed the request protocol correctly but stopped the
    /// model because its configured output allowance was exhausted. This is
    /// a model/resource outcome, not a transport outage and not retryable
    /// with the same request budget.
    #[error("model output limit reached: {reason}")]
    ModelOutputLimit { reason: String },

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

    /// A mutation may have partially landed, or cleanup/durable settlement
    /// of a prepared mutation could not be confirmed. Callers must stop
    /// ordinary mutation and reconcile from known authority state instead
    /// of treating this as a retryable error or claiming rollback succeeded.
    #[error("recovery required: {0}")]
    RecoveryRequired(String),

    /// An operation referenced a Core authority epoch that has already
    /// advanced. Distinct from [`AgentError::InvalidRequest`] so callers
    /// can classify staleness structurally instead of parsing error text.
    #[error("operation epoch {expected} is stale; current Core epoch is {current}")]
    StaleEpoch { expected: u64, current: u64 },
}

pub type AgentResult<T> = Result<T, AgentError>;
