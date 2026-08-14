use std::fmt;

/// A stateless contract-validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    field: &'static str,
    reason: String,
}

impl ValidationError {
    pub(crate) fn new(field: &'static str, reason: impl Into<String>) -> Self {
        Self {
            field,
            reason: reason.into(),
        }
    }

    /// Stable field name suitable for a structured protocol error code.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// Human-readable bounded-contract reason for diagnostics.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid {}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ValidationError {}

pub type ValidationResult<T> = Result<T, ValidationError>;

pub(crate) fn validate_identifier(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> ValidationResult<()> {
    if value.is_empty() {
        return Err(ValidationError::new(field, "must not be empty"));
    }
    if value.len() > max_bytes {
        return Err(ValidationError::new(
            field,
            format!("is {} bytes, above the {max_bytes} byte bound", value.len()),
        ));
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(ValidationError::new(field, "must not be empty"));
    };
    if !first.is_ascii_lowercase() {
        return Err(ValidationError::new(
            field,
            "must start with a lowercase ASCII letter",
        ));
    }
    if !bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        return Err(ValidationError::new(
            field,
            "must contain only lowercase ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
}

pub(crate) fn validate_opaque(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> ValidationResult<()> {
    if value.is_empty() {
        return Err(ValidationError::new(field, "must not be empty"));
    }
    if value.len() > max_bytes {
        return Err(ValidationError::new(
            field,
            format!("is {} bytes, above the {max_bytes} byte bound", value.len()),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::new(
            field,
            "must not contain control characters",
        ));
    }
    Ok(())
}
