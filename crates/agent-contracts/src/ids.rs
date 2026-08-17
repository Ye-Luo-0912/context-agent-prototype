use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Canonical catalog uri prefix for a context item. Search hits emit
/// `context://run/<uuid>`; mutation/query ops must accept that same string.
pub const CONTEXT_ITEM_URI_PREFIX: &str = "context://run/";

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

id_type!(RunId);
id_type!(TaskId);
id_type!(ScopeId);
id_type!(TurnId);
id_type!(OperationId);
id_type!(EffectId);
id_type!(AuthorityJournalId);
id_type!(RuntimeInputId);

/// Item identity. Serializes as a bare UUID so checkpoints stay stable.
/// Parses and deserializes either that UUID or the catalog uri
/// `context://run/<uuid>` so a search hit can be fed back into inspect /
/// fetch / admit / derive without rewriting the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ContextItemId(pub Uuid);

impl ContextItemId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse a canonical item ref: a bare UUID or `context://run/<uuid>`.
    pub fn parse_ref(s: &str) -> Result<Self, uuid::Error> {
        let s = s.trim();
        let s = s.strip_prefix(CONTEXT_ITEM_URI_PREFIX).unwrap_or(s);
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for ContextItemId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ContextItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ContextItemId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_ref(s)
    }
}

impl<'de> Deserialize<'de> for ContextItemId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse_ref(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn context_item_id_accepts_bare_uuid_and_catalog_uri() {
        let id = ContextItemId::new();
        let bare = id.to_string();
        let uri = format!("{CONTEXT_ITEM_URI_PREFIX}{id}");
        assert_eq!(ContextItemId::parse_ref(&bare).unwrap(), id);
        assert_eq!(ContextItemId::parse_ref(&uri).unwrap(), id);
        assert_eq!(ContextItemId::from_str(&uri).unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{bare}\""));
        let from_uri: ContextItemId = serde_json::from_str(&format!("\"{uri}\"")).unwrap();
        assert_eq!(from_uri, id);
        let from_bare: ContextItemId = serde_json::from_str(&json).unwrap();
        assert_eq!(from_bare, id);
    }

    #[test]
    fn other_ids_still_reject_catalog_uris() {
        let id = RunId::new();
        let uri = format!("{CONTEXT_ITEM_URI_PREFIX}{id}");
        assert!(uri.parse::<RunId>().is_err());
        assert!(uri.parse::<TaskId>().is_err());
    }
}
