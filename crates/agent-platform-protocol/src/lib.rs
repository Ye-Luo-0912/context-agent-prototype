//! Transport-independent Platform protocol semantics.
//!
//! This crate defines bounded wire DTOs and stateless validation only. It
//! deliberately owns no framing, transport, process supervision, task loop,
//! authority state, retry ledger, router, or cancellation execution. Its
//! typed operation query/cancel DTOs describe semantics only. Adapters migrate
//! onto these contracts only after their compatibility and fault behavior is
//! proved separately.
//!
//! Digest helpers hash caller-supplied bytes exactly unless the caller first
//! produced RFC 8785/JCS bytes via [`agent_contracts::jcs_serialize`].
//! [`ArgumentDigest::from_json`] always hashes the JCS encoding.
//! Artifact bytes use [`ContentDigest`] / [`ArtifactLocator`] instead.
//! Parse-time decoded JSON budgets ([`JsonDecodeBudget`]) bound the DOM
//! while visiting, independent of the encoded frame cap.

mod contract;
mod digest;
mod error;
mod ids;
mod json;
mod operation;
mod validation;

pub use agent_contracts::{
    ArgumentDigest, ArtifactLocator, ContentDigest, EffectId, OperationQueryResult, jcs_serialize,
};
pub use contract::*;
pub use digest::*;
pub use error::*;
pub use ids::*;
pub use json::{
    JsonDecodeBudget, JsonDecodeError, MAX_JSON_CONTROL_ARRAY_LEN, MAX_JSON_CONTROL_DEPTH,
    MAX_JSON_CONTROL_NODES, MAX_JSON_CONTROL_OBJECT_KEYS, MAX_JSON_CONTROL_STRING_BYTES,
    MAX_JSON_CONTROL_TOTAL_STRING_BYTES, MAX_JSON_DATA_ARRAY_LEN, MAX_JSON_DATA_DEPTH,
    MAX_JSON_DATA_NODES, MAX_JSON_DATA_OBJECT_KEYS, decode_value, from_slice_bounded,
};
pub use operation::*;
pub use validation::{ValidationError, ValidationResult};
