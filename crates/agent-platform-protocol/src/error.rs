use serde::{Deserialize, Serialize};

use crate::{
    DeadlineRemainingMs, ValidationError, ValidationResult,
    validation::{validate_identifier, validate_opaque},
};

pub const MAX_ERROR_CODE_BYTES: usize = 96;
pub const MAX_ERROR_MESSAGE_BYTES: usize = 4_000;
pub const MAX_DIAGNOSTIC_REF_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformErrorClass {
    Protocol,
    Domain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryDisposition {
    /// Do not automatically repeat or query this operation.
    Never,
    /// A new physical attempt may repeat the same logical operation id.
    SameOperation,
    /// The applied state is ambiguous and must be queried before any replay.
    QueryBeforeRetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStateDisposition {
    NotApplicable,
    NotApplied,
    Applied,
    OutcomeUnknown,
}

/// Structured protocol/domain error semantics. Retry and effect state are
/// deliberately orthogonal fields with a validated legal product; a peer
/// cannot describe an unknown effect as blindly retryable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformError {
    pub class: PlatformErrorClass,
    pub code: String,
    pub message: String,
    pub retry: RetryDisposition,
    pub effect_state: EffectStateDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<DeadlineRemainingMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_ref: Option<String>,
}

impl PlatformError {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_identifier("error.code", &self.code, MAX_ERROR_CODE_BYTES)?;
        validate_opaque("error.message", &self.message, MAX_ERROR_MESSAGE_BYTES)?;
        if let Some(reference) = &self.diagnostic_ref {
            validate_opaque("error.diagnostic_ref", reference, MAX_DIAGNOSTIC_REF_BYTES)?;
        }

        match (self.retry, self.effect_state) {
            (RetryDisposition::SameOperation, EffectStateDisposition::NotApplicable)
            | (RetryDisposition::SameOperation, EffectStateDisposition::NotApplied)
            | (RetryDisposition::QueryBeforeRetry, EffectStateDisposition::OutcomeUnknown)
            | (RetryDisposition::Never, EffectStateDisposition::NotApplicable)
            | (RetryDisposition::Never, EffectStateDisposition::NotApplied)
            | (RetryDisposition::Never, EffectStateDisposition::Applied)
            | (RetryDisposition::Never, EffectStateDisposition::OutcomeUnknown) => {}
            _ => {
                return Err(ValidationError::new(
                    "error.disposition",
                    "retry and effect-state dispositions form an illegal combination",
                ));
            }
        }

        if self.retry_after_ms.is_some() && self.retry != RetryDisposition::SameOperation {
            return Err(ValidationError::new(
                "error.retry_after_ms",
                "is allowed only for same-operation retry",
            ));
        }
        Ok(())
    }
}

/// A response body has one explicit success/error shape. Route-specific
/// success payloads remain typed; failures always carry the common validated
/// error algebra instead of relying on missing fields or shape guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlatformResponse<T> {
    Success { value: T },
    Error { error: PlatformError },
}

impl<T> PlatformResponse<T> {
    pub fn validate(&self) -> ValidationResult<()> {
        match self {
            Self::Success { .. } => Ok(()),
            Self::Error { error } => error.validate(),
        }
    }
}
