//! Process-neutral identity and state vocabulary for one tool operation.
//!
//! Core owns the state machine and optional persistent journal while Runtime
//! remains the sole scheduler. Composed reconcilers may prove the outcome of
//! managed effects after restart; these DTOs do not claim exactly-once
//! execution for unmanaged or externally applied effects.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::{
    AgentError, AgentResult, AuthorityJournalId, EffectDurability, EffectId,
    OperationEffectContext, OperationId, RunId, ScopeId, TaskId, TurnId,
};

pub const OPERATION_DIGEST_BYTES: usize = 32;
pub const OPERATION_DIGEST_HEX_BYTES: usize = OPERATION_DIGEST_BYTES * 2;
pub const MAX_OPERATION_CALL_ID_BYTES: usize = 256;
pub const MAX_OPERATION_TOOL_NAME_BYTES: usize = 96;
pub const MAX_OPERATION_DIAGNOSTIC_BYTES: usize = 4_000;
pub const MAX_OPERATION_EVIDENCE_BYTES: usize = 512;
pub const OPERATION_JOURNAL_VERSION: u32 = 1;
pub const MAX_OPERATION_JOURNAL_RECOVERY_RECORDS: usize = 65_536;
/// 压缩后元数据里保留的代际 tip 数量。超出后丢掉最旧的祖先，
/// 那些代际上的 checkpoint 无法再证明，必须 fail-closed。
pub const MAX_AUTHORITY_JOURNAL_ANCESTORS: usize = 8;
pub const AUTHORITY_STATE_DIGEST_BYTES: usize = 32;

/// Digest of one validated semantic tool-argument value.
///
/// `from_json` hashes the RFC 8785/JCS encoding of the value, so object key
/// order and equivalent number spellings (`1` vs `1.0`) are the same digest
/// across languages. Artifact bytes use [`crate::ContentDigest`] instead.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArgumentDigest([u8; OPERATION_DIGEST_BYTES]);

impl ArgumentDigest {
    pub const fn from_bytes(bytes: [u8; OPERATION_DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; OPERATION_DIGEST_BYTES] {
        &self.0
    }

    pub fn sha256_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub fn from_json(value: &Value) -> Self {
        let canonical = crate::jcs::serialize(value)
            .expect("serde_json::Value is I-JSON and therefore JCS-serializable");
        Self::sha256_bytes(canonical.as_bytes())
    }
}

impl fmt::Display for ArgumentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            formatter.write_char(char::from(HEX[usize::from(byte >> 4)]))?;
            formatter.write_char(char::from(HEX[usize::from(byte & 0x0f)]))?;
        }
        Ok(())
    }
}

impl fmt::Debug for ArgumentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ArgumentDigest")
            .field(&self.to_string())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgumentDigestParseError;

impl fmt::Display for ArgumentDigestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected exactly 64 lowercase hexadecimal characters")
    }
}

impl std::error::Error for ArgumentDigestParseError {}

impl FromStr for ArgumentDigest {
    type Err = ArgumentDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != OPERATION_DIGEST_HEX_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ArgumentDigestParseError);
        }
        let mut decoded = [0u8; OPERATION_DIGEST_BYTES];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        Ok(Self(decoded))
    }
}

impl Serialize for ArgumentDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ArgumentDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(D::Error::custom)
    }
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("lowercase hexadecimal input was validated"),
    }
}

/// Digest of the complete durable authority truth folded at one WAL cursor.
/// It is deliberately distinct from an argument digest: checkpoint restore
/// compares this value but never uses it to recreate Core state.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuthorityStateDigest([u8; AUTHORITY_STATE_DIGEST_BYTES]);

impl AuthorityStateDigest {
    pub const fn from_bytes(bytes: [u8; AUTHORITY_STATE_DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; AUTHORITY_STATE_DIGEST_BYTES] {
        &self.0
    }

    pub fn sha256_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for AuthorityStateDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            formatter.write_char(char::from(HEX[usize::from(byte >> 4)]))?;
            formatter.write_char(char::from(HEX[usize::from(byte & 0x0f)]))?;
        }
        Ok(())
    }
}

impl fmt::Debug for AuthorityStateDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("AuthorityStateDigest")
            .field(&self.to_string())
            .finish()
    }
}

impl Serialize for AuthorityStateDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for AuthorityStateDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() != AUTHORITY_STATE_DIGEST_BYTES * 2
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(D::Error::custom(
                "authority state digest must be exactly 64 lowercase hexadecimal characters",
            ));
        }
        let mut decoded = [0_u8; AUTHORITY_STATE_DIGEST_BYTES];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        Ok(Self(decoded))
    }
}

/// Stable reference to one durable authority-journal prefix.
///
/// A Runtime checkpoint carries this marker as a cross-check only. Restore
/// must never install its epoch, cursor, generation, or digest into Core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityCheckpointMarker {
    pub journal_id: AuthorityJournalId,
    /// WAL 代际。未压缩时为 1；每次成功压缩必须 +1，不能复用旧代际序号。
    pub generation: u64,
    pub authority_epoch: u64,
    pub last_seq: u64,
    pub state_digest: AuthorityStateDigest,
}

impl AuthorityCheckpointMarker {
    pub fn validate(&self) -> Result<(), String> {
        if self.journal_id.0.is_nil() {
            return Err("authority checkpoint marker contains a nil journal id".into());
        }
        if self.generation == 0 {
            return Err("authority checkpoint marker generation must be non-zero".into());
        }
        if self.authority_epoch == 0 {
            return Err("authority checkpoint marker epoch must be non-zero".into());
        }
        Ok(())
    }
}

/// Immutable identity registered by Core before a tool can execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOperationIdentity {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub turn_id: TurnId,
    pub scope_id: Option<ScopeId>,
    pub operation_id: OperationId,
    pub generation: u64,
    pub call_id: String,
    pub tool_name: String,
    pub argument_digest: ArgumentDigest,
}

impl ToolOperationIdentity {
    pub fn validate(&self) -> Result<(), String> {
        if self.run_id.0.is_nil()
            || self.turn_id.0.is_nil()
            || self.operation_id.0.is_nil()
            || self.task_id.is_some_and(|id| id.0.is_nil())
            || self.scope_id.is_some_and(|id| id.0.is_nil())
        {
            return Err("operation identity contains a nil UUID".into());
        }
        if self.generation == 0 {
            return Err("operation identity generation must be non-zero".into());
        }
        validate_bounded_text("call_id", &self.call_id, MAX_OPERATION_CALL_ID_BYTES)?;
        validate_bounded_text("tool_name", &self.tool_name, MAX_OPERATION_TOOL_NAME_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationTerminal {
    CompletedValue,
    Refused {
        error: String,
    },
    NotApplied {
        error: String,
    },
    Applied {
        durability: EffectDurability,
        evidence: Option<String>,
    },
    OutcomeUnknown {
        error: String,
    },
    CancelledBeforeCommit,
}

impl OperationTerminal {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Refused { error }
            | Self::NotApplied { error }
            | Self::OutcomeUnknown { error } => {
                validate_bounded_text("operation error", error, MAX_OPERATION_DIAGNOSTIC_BYTES)
            }
            Self::Applied {
                durability,
                evidence,
            } => {
                if let EffectDurability::DurabilityFailed(error) = durability {
                    validate_bounded_text(
                        "operation durability error",
                        error,
                        MAX_OPERATION_DIAGNOSTIC_BYTES,
                    )?;
                }
                if let Some(evidence) = evidence {
                    validate_bounded_text(
                        "operation evidence",
                        evidence,
                        MAX_OPERATION_EVIDENCE_BYTES,
                    )?;
                }
                Ok(())
            }
            Self::CompletedValue | Self::CancelledBeforeCommit => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationState {
    Accepted,
    Executing {
        /// Stable Core-issued id reserved before a side-effecting dispatch.
        /// Read-only operations carry `None`. Reserving the id here lets an
        /// effect broker bind staged recovery evidence before `Prepared`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect_id: Option<EffectId>,
    },
    Prepared {
        effect_id: EffectId,
    },
    CommitStarted {
        effect_id: EffectId,
    },
    Terminal {
        effect_id: Option<EffectId>,
        terminal: OperationTerminal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationSnapshot {
    pub identity: ToolOperationIdentity,
    pub state: OperationState,
}

impl OperationSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        self.identity.validate()?;
        match &self.state {
            OperationState::Executing {
                effect_id: Some(effect_id),
            }
            | OperationState::Prepared { effect_id }
            | OperationState::CommitStarted { effect_id }
                if effect_id.0.is_nil() =>
            {
                return Err("operation state contains a nil effect id".into());
            }
            OperationState::Terminal {
                effect_id: Some(effect_id),
                ..
            } if effect_id.0.is_nil() => {
                return Err("operation terminal contains a nil effect id".into());
            }
            OperationState::Terminal { terminal, .. } => terminal.validate()?,
            _ => {}
        }
        Ok(())
    }
}

/// Result of consulting Core's bounded resident operation authority.
/// `ExpiredOrPossiblySeen` is deliberately fail-closed: it includes both ids
/// evicted from the recent snapshot cache and conservative collisions in the
/// bounded seen-id filter. Such an id must never be blindly executed again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationQueryResult {
    Found { snapshot: Box<OperationSnapshot> },
    ExpiredOrPossiblySeen,
    NotFound,
}

/// Best-known world-state truth returned by an effect-specific recovery
/// adapter. `NotManaged` lets a composed reconciler decline an effect kind
/// without claiming that the effect did or did not land.
/// `CompletedValue` is for non-transactional process effects whose live
/// path would have finished as a value terminal, not an applied mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EffectReconciliation {
    NotManaged,
    NotApplied {
        evidence: Option<String>,
    },
    Applied {
        durability: EffectDurability,
        evidence: Option<String>,
    },
    CompletedValue {
        evidence: Option<String>,
    },
    Ambiguous {
        reason: String,
    },
}

impl EffectReconciliation {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::NotManaged => Ok(()),
            Self::NotApplied { evidence }
            | Self::Applied { evidence, .. }
            | Self::CompletedValue { evidence } => {
                if let Self::Applied {
                    durability: EffectDurability::DurabilityFailed(reason),
                    ..
                } = self
                {
                    validate_bounded_text(
                        "effect reconciliation durability error",
                        reason,
                        MAX_OPERATION_DIAGNOSTIC_BYTES,
                    )?;
                }
                if let Some(evidence) = evidence {
                    validate_bounded_text(
                        "effect reconciliation evidence",
                        evidence,
                        MAX_OPERATION_EVIDENCE_BYTES,
                    )?;
                }
                Ok(())
            }
            Self::Ambiguous { reason } => validate_bounded_text(
                "effect reconciliation reason",
                reason,
                MAX_OPERATION_DIAGNOSTIC_BYTES,
            ),
        }
    }
}

/// Effect-specific, synchronous recovery seam composed into trusted Core.
/// Implementations inspect durable evidence only; they do not schedule work.
pub trait EffectReconciler: Send + Sync {
    fn reconcile(&self, context: &OperationEffectContext) -> AgentResult<EffectReconciliation>;

    /// 清理已结算操作留下的孤儿进程树。默认无操作。
    /// 实现不得回放或提交效果，只能按已记录身份杀树。
    fn recover_orphans(&self) -> AgentResult<()> {
        Ok(())
    }
}

/// Whether Core can safely accept new effectful work after authority recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthorityRecoveryStatus {
    Ready,
    RecoveryRequired { reason: String },
}

impl AuthorityRecoveryStatus {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Ready => Ok(()),
            Self::RecoveryRequired { reason } => validate_bounded_text(
                "authority recovery reason",
                reason,
                MAX_OPERATION_DIAGNOSTIC_BYTES,
            ),
        }
    }
}

/// One authority transition persisted by Core before the corresponding
/// in-memory state becomes visible. Full snapshots keep recovery folding
/// simple and make identity drift detectable without consulting Runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationJournalTransition {
    EpochAdvanced {
        from: u64,
        to: u64,
    },
    OperationUpsert {
        snapshot: Box<OperationSnapshot>,
    },
    /// 压缩代际的第一条记录：把折叠后的 epoch 安装为起点。
    /// 后续 `OperationUpsert` 写入折叠后的当前快照（可以是任意合法状态，
    /// 不必从 Accepted 重放）。只允许 journal `compact()` 写入，调用方
    /// 不能经 `append_and_sync` 提交。
    Compacted {
        previous: AuthorityCheckpointMarker,
    },
}

impl OperationJournalTransition {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::EpochAdvanced { from, to } => {
                if *from == 0 || from.checked_add(1) != Some(*to) {
                    return Err("authority epoch transition must advance one non-zero step".into());
                }
                Ok(())
            }
            Self::OperationUpsert { snapshot } => snapshot.validate(),
            Self::Compacted { previous } => previous.validate(),
        }
    }
}

/// Storage-owned sequence plus one validated authority transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationJournalRecord {
    pub version: u32,
    pub seq: u64,
    pub transition: OperationJournalTransition,
}

impl OperationJournalRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != OPERATION_JOURNAL_VERSION {
            return Err(format!(
                "unsupported operation journal version {}",
                self.version
            ));
        }
        if self.seq == 0 {
            return Err("operation journal sequence must be non-zero".into());
        }
        self.transition.validate()
    }
}

/// Strictly validated state recovered from the durable authority journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationJournalRecovery {
    pub last_seq: u64,
    pub authority_epoch: u64,
    pub operations: Vec<OperationSnapshot>,
    /// A structurally incomplete final fragment (no terminating newline) was
    /// quarantined and removed. A complete frame with invalid JSON, checksum,
    /// version or state is corruption and must fail closed.
    pub truncated_tail: bool,
    /// 若本 WAL 以 `Compacted` 为起点，记录被折叠掉的那一代 tip。
    /// 用于和 metadata.generation 交叉校验；未压缩的 generation-1 WAL 为 `None`。
    pub compacted_from: Option<AuthorityCheckpointMarker>,
}

impl Default for OperationJournalRecovery {
    fn default() -> Self {
        Self {
            last_seq: 0,
            authority_epoch: 1,
            operations: Vec::new(),
            truncated_tail: false,
            compacted_from: None,
        }
    }
}

/// Synchronous authority WAL contract. `append_and_sync` is intentionally
/// one operation: callers must never mutate Core authority state between an
/// append and a separate flush. Concrete file storage lives in
/// `agent-storage`; Core depends only on this trait.
pub trait OperationJournal: Send + Sync {
    fn append_and_sync(
        &self,
        transition: &OperationJournalTransition,
    ) -> AgentResult<OperationJournalRecord>;

    fn recover(&self) -> AgentResult<OperationJournalRecovery>;

    /// Return a marker for the exact durable folded state while holding the
    /// journal's writer lock. Implementations that cannot provide one fail
    /// closed rather than synthesizing identity.
    fn authority_checkpoint_marker(&self) -> AgentResult<AuthorityCheckpointMarker> {
        Err(AgentError::RecoveryRequired(
            "operation journal does not expose a durable authority checkpoint marker".into(),
        ))
    }

    /// Prove that a checkpoint marker names this journal's exact durable
    /// state at `expected.last_seq`. A later current cursor is valid only
    /// when the implementation can verify the expected digest as an ancestor
    /// prefix of the same journal generation. This never rewinds or mutates
    /// authority state.
    fn validate_authority_checkpoint_marker(
        &self,
        expected: &AuthorityCheckpointMarker,
    ) -> AgentResult<()> {
        expected.validate().map_err(AgentError::InvalidRequest)?;
        let actual = self.authority_checkpoint_marker()?;
        // The generic implementation can only prove the current point.
        // Persistent journals should override this with prefix validation so
        // a checkpoint remains usable after later authority transitions.
        if actual == *expected {
            Ok(())
        } else {
            Err(AgentError::RecoveryRequired(format!(
                "authority checkpoint marker does not match current durable authority state (expected journal {} generation {} epoch {} seq {}, current journal {} generation {} epoch {} seq {})",
                expected.journal_id,
                expected.generation,
                expected.authority_epoch,
                expected.last_seq,
                actual.journal_id,
                actual.generation,
                actual.authority_epoch,
                actual.last_seq,
            )))
        }
    }

    /// 把当前折叠状态写成新代际 WAL。未解析操作必须原样保留；中间 upsert
    /// 与 epoch 链被丢掉。压缩点 tip 进入有界祖先表，旧代际的中间 prefix
    /// 无法再证明，restore 必须 fail-closed。空 journal 是 no-op。
    fn compact(&self) -> AgentResult<AuthorityCheckpointMarker> {
        Err(AgentError::Storage(
            "operation journal does not support compaction".into(),
        ))
    }
}

impl OperationQueryResult {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Found { snapshot } => snapshot.validate(),
            Self::ExpiredOrPossiblySeen | Self::NotFound => Ok(()),
        }
    }
}

fn validate_bounded_text(name: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        Err(format!(
            "{name} must be non-empty control-free UTF-8 of at most {max_bytes} bytes"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn argument_digest_is_stable_across_object_key_order() {
        assert_eq!(
            ArgumentDigest::from_json(&json!({"a": 1, "nested": {"x": 2, "y": 3}})),
            ArgumentDigest::from_json(&json!({"nested": {"y": 3, "x": 2}, "a": 1}))
        );
        assert_ne!(
            ArgumentDigest::from_json(&json!({"a": 1})),
            ArgumentDigest::from_json(&json!({"a": 2}))
        );
        assert_eq!(
            ArgumentDigest::from_json(&json!({"a": 1})),
            ArgumentDigest::from_json(&json!({"a": 1.0}))
        );
    }

    #[test]
    fn digest_wire_is_strict_lower_hex() {
        let digest = ArgumentDigest::sha256_bytes(b"abc");
        let encoded = serde_json::to_string(&digest).unwrap();
        let decoded: ArgumentDigest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, digest);
        assert!(serde_json::from_str::<ArgumentDigest>(&encoded.to_uppercase()).is_err());
    }

    #[test]
    fn effect_reconciliation_round_trips_and_enforces_bounds() {
        for reconciliation in [
            EffectReconciliation::NotManaged,
            EffectReconciliation::NotApplied {
                evidence: Some("mutation:1".into()),
            },
            EffectReconciliation::Applied {
                durability: EffectDurability::Durable,
                evidence: Some("mutation:2".into()),
            },
            EffectReconciliation::CompletedValue {
                evidence: Some("process-pid:4242".into()),
            },
            EffectReconciliation::Ambiguous {
                reason: "target hash matches neither durable state".into(),
            },
        ] {
            reconciliation.validate().unwrap();
            let encoded = serde_json::to_value(&reconciliation).unwrap();
            let decoded: EffectReconciliation = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, reconciliation);
        }

        assert!(
            EffectReconciliation::NotApplied {
                evidence: Some("x".repeat(MAX_OPERATION_EVIDENCE_BYTES + 1)),
            }
            .validate()
            .is_err()
        );
        assert!(
            EffectReconciliation::Ambiguous {
                reason: "x".repeat(MAX_OPERATION_DIAGNOSTIC_BYTES + 1),
            }
            .validate()
            .is_err()
        );
        assert!(
            EffectReconciliation::Applied {
                durability: EffectDurability::DurabilityFailed(String::new()),
                evidence: None,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn recovery_status_round_trips_and_enforces_bounds() {
        for status in [
            AuthorityRecoveryStatus::Ready,
            AuthorityRecoveryStatus::RecoveryRequired {
                reason: "effect outcome needs operator recovery".into(),
            },
        ] {
            status.validate().unwrap();
            let encoded = serde_json::to_value(&status).unwrap();
            let decoded: AuthorityRecoveryStatus = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, status);
        }
        assert!(
            AuthorityRecoveryStatus::RecoveryRequired {
                reason: String::new(),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn compacted_transition_round_trips_and_rejects_a_nil_previous_marker() {
        let previous = AuthorityCheckpointMarker {
            journal_id: AuthorityJournalId::new(),
            generation: 1,
            authority_epoch: 3,
            last_seq: 9,
            state_digest: AuthorityStateDigest::sha256_bytes(b"pre-compact"),
        };
        let transition = OperationJournalTransition::Compacted {
            previous: previous.clone(),
        };
        transition.validate().unwrap();
        let encoded = serde_json::to_value(&transition).unwrap();
        assert_eq!(encoded["type"], "compacted");
        let decoded: OperationJournalTransition = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, transition);

        let mut bad = previous;
        bad.generation = 0;
        assert!(
            OperationJournalTransition::Compacted { previous: bad }
                .validate()
                .is_err()
        );
    }

    #[test]
    fn authority_checkpoint_marker_round_trips_strict_digest_and_identity() {
        let marker = AuthorityCheckpointMarker {
            journal_id: AuthorityJournalId::new(),
            generation: 1,
            authority_epoch: 7,
            last_seq: 11,
            state_digest: AuthorityStateDigest::sha256_bytes(b"folded authority"),
        };
        marker.validate().unwrap();
        let encoded = serde_json::to_value(&marker).unwrap();
        let decoded: AuthorityCheckpointMarker = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, marker);

        let mut bad = encoded;
        bad["state_digest"] = serde_json::json!("ABC");
        assert!(serde_json::from_value::<AuthorityCheckpointMarker>(bad).is_err());
        let mut nil = marker;
        nil.journal_id = AuthorityJournalId(uuid::Uuid::nil());
        assert!(nil.validate().is_err());
    }

    #[test]
    fn effect_reconciler_is_object_safe() {
        struct Unmanaged;
        impl EffectReconciler for Unmanaged {
            fn reconcile(
                &self,
                _context: &OperationEffectContext,
            ) -> AgentResult<EffectReconciliation> {
                Ok(EffectReconciliation::NotManaged)
            }
        }

        let reconciler: &dyn EffectReconciler = &Unmanaged;
        let identity = ToolOperationIdentity {
            run_id: RunId::new(),
            task_id: None,
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id: OperationId::new(),
            generation: 1,
            call_id: "call-1".into(),
            tool_name: "fs.write".into(),
            argument_digest: ArgumentDigest::sha256_bytes(b"args"),
        };
        assert_eq!(
            reconciler
                .reconcile(&OperationEffectContext {
                    identity,
                    effect_id: EffectId::new(),
                })
                .unwrap(),
            EffectReconciliation::NotManaged
        );
    }
}
