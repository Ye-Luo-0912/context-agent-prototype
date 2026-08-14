use std::{fmt, str::FromStr};

use agent_contracts::OperationId;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// Parse failure for a typed Platform UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdParseError;

impl fmt::Display for IdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected a UUID Platform id")
    }
}

impl std::error::Error for IdParseError {}

macro_rules! platform_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(OperationId);

        impl $name {
            pub fn new() -> Self {
                Self(OperationId::new())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let parsed = OperationId::from_str(value).map_err(|_| IdParseError)?;
                if parsed.0.is_nil() || parsed.to_string() != value {
                    return Err(IdParseError);
                }
                Ok(Self(parsed))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::from_str(&value).map_err(D::Error::custom)
            }
        }
    };
}

platform_id!(
    /// Identity of one encoded protocol message. A transport redelivery of
    /// the same message preserves it; an application-level retry creates a
    /// new message id.
    MessageId
);

platform_id!(
    /// Identity pairing one physical request/response exchange. A logical
    /// retry uses a new request id while preserving its `OperationId`.
    RequestId
);
