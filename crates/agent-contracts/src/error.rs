use std::fmt;

use thiserror::Error;

/// Stable categories for model-provider wire damage. These are distinct from
/// provider-declared model outcomes and retryable connection failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelProtocolErrorKind {
    MalformedEvent,
    MalformedToolCall,
}

impl fmt::Display for ModelProtocolErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MalformedEvent => "malformed-event",
            Self::MalformedToolCall => "malformed-tool-call",
        })
    }
}

/// A bounded local resource owned by the runtime or one of its adapters.
///
/// This is deliberately separate from [`ModelProtocolErrorKind`]: exhausting
/// a local buffer says nothing about whether the provider's wire data was
/// malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocalResourceLimitKind {
    BufferedModelStreamChunks,
    BufferedModelStreamBytes,
}

impl fmt::Display for LocalResourceLimitKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BufferedModelStreamChunks => "buffered-model-stream-chunks",
            Self::BufferedModelStreamBytes => "buffered-model-stream-bytes",
        })
    }
}

/// A server-requested retry delay with a hard cross-runtime bound.
///
/// Providers may report arbitrarily large values. The contract caps one retry
/// interval at a minute so an upstream header cannot create an unbounded wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RetryAfterMillis(u32);

impl RetryAfterMillis {
    pub const MAX_MILLIS: u32 = 60_000;

    pub const fn new(millis: u32) -> Option<Self> {
        if millis <= Self::MAX_MILLIS {
            Some(Self(millis))
        } else {
            None
        }
    }

    pub const fn new_saturating(millis: u64) -> Self {
        if millis > Self::MAX_MILLIS as u64 {
            Self(Self::MAX_MILLIS)
        } else {
            Self(millis as u32)
        }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for RetryAfterMillis {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("context error: {0}")]
    Context(String),

    #[error("model error: {0}")]
    Model(String),

    /// The provider's stream violated the selected model wire protocol.
    /// Adapters fail closed; policy may regenerate a malformed tool-call body
    /// only under a separate bounded format budget and only before the sink's
    /// replay boundary. Damaged wire events are never regenerated.
    #[error("model protocol error ({kind}): {message}")]
    ModelProtocol {
        kind: ModelProtocolErrorKind,
        message: String,
    },

    /// The provider completed the request protocol correctly but stopped the
    /// model because its configured output allowance was exhausted. This is
    /// a model/resource outcome, not a transport outage and not retryable
    /// with the same request budget.
    #[error("model output limit reached: {reason}")]
    ModelOutputLimit { reason: String },

    /// A process-local safety bound was reached while handling otherwise
    /// unclassified data. This is non-retryable by default: retrying the same
    /// request against the same local limit cannot make more capacity appear.
    #[error("local resource limit exceeded ({kind}): observed={observed}, limit={limit}")]
    LocalResourceLimit {
        kind: LocalResourceLimitKind,
        observed: u64,
        limit: u64,
    },

    #[error("transport error (retryable={retryable}): {message}")]
    Transport { retryable: bool, message: String },

    /// A retryable transport failure with a provider-requested minimum wait.
    /// The delay is bounded by construction and retry policy may impose a
    /// lower ceiling before sleeping.
    #[error("transport error (retryable=true, retry_after_ms={retry_after_ms}): {message}")]
    TransportRetryAfter {
        retry_after_ms: RetryAfterMillis,
        message: String,
    },

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_bounded_by_construction() {
        assert_eq!(RetryAfterMillis::new(1_500).unwrap().get(), 1_500);
        assert!(RetryAfterMillis::new(60_001).is_none());
        assert_eq!(
            RetryAfterMillis::new_saturating(u64::MAX).get(),
            RetryAfterMillis::MAX_MILLIS
        );
    }

    #[test]
    fn protocol_kind_is_preserved_without_message_parsing() {
        let error = AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedToolCall,
            message: "arguments ended early".into(),
        };
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                ..
            }
        ));
    }

    #[test]
    fn local_resource_limit_is_not_protocol_damage() {
        let error = AgentError::LocalResourceLimit {
            kind: LocalResourceLimitKind::BufferedModelStreamChunks,
            observed: 17,
            limit: 16,
        };
        assert!(matches!(
            error,
            AgentError::LocalResourceLimit {
                kind: LocalResourceLimitKind::BufferedModelStreamChunks,
                observed: 17,
                limit: 16,
            }
        ));
    }
}
