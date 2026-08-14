//! 远程/进程外调用的幂等屏障与恢复证据。
//!
//! 不能证明对端世界改了什么。只能证明：请求是否离开本机、是否收到应答。
//! 没有在发送前落下的幂等键，不得声称 at-most-one；崩溃窗口只能是
//! `NotApplied`（从未发出）或 `Ambiguous`（可能已发出），禁止盲放。

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use agent_contracts::{
    AgentError, AgentResult, EffectId, EffectReconciliation, OperationEffectContext,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::ConfinedDir;

const JOURNAL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const MAX_REASON_BYTES: usize = 4_000;

/// 远程调用的耐久应答。`Staged` 表示对端只提交了待 Core 提交的效果，
/// 世界状态交给 workspace journal；其余表示这次调用本身已经结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEffectAck {
    Completed,
    Staged,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalTransition {
    Reserved {
        context: Box<OperationEffectContext>,
        idempotency_key: Option<String>,
    },
    Dispatched {
        effect_id: EffectId,
    },
    Acknowledged {
        effect_id: EffectId,
        ack: RemoteEffectAck,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalRecord {
    version: u32,
    seq: u64,
    transition: JournalTransition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFrame {
    record: JournalRecord,
    checksum: String,
}

#[derive(Debug, Clone)]
struct RemoteEvidence {
    context: OperationEffectContext,
    idempotency_key: Option<String>,
    dispatched: bool,
    ack: Option<RemoteEffectAck>,
}

#[derive(Debug, Default, Clone)]
struct RecoveryState {
    last_seq: u64,
    effects: HashMap<EffectId, RemoteEvidence>,
}

struct WriterState {
    file: File,
    recovery: RecoveryState,
    failed: Option<String>,
}

pub(crate) struct RemoteEffectJournal {
    _authority_dir: ConfinedDir,
    path: PathBuf,
    writer: Mutex<WriterState>,
}

impl std::fmt::Debug for RemoteEffectJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteEffectJournal")
            .field("path", &self.path)
            .finish()
    }
}

impl RemoteEffectJournal {
    pub(crate) fn open(authority_dir: ConfinedDir) -> AgentResult<Self> {
        let path = authority_dir.display().join("remote-effects.jsonl");
        let existed = authority_dir
            .open_existing(OsStr::new("remote-effects.jsonl"))
            .is_ok();
        let mut file = authority_dir
            .open_or_create_regular_file(OsStr::new("remote-effects.jsonl"))
            .map_err(|error| {
                AgentError::Storage(format!(
                    "open remote effect journal {}: {error}",
                    path.display()
                ))
            })?;
        file.try_lock().map_err(|error| {
            AgentError::Storage(format!(
                "lock remote effect journal {} exclusively: {error}",
                path.display()
            ))
        })?;
        if !existed {
            file.sync_all().map_err(|error| {
                AgentError::Storage(format!(
                    "sync new remote effect journal {}: {error}",
                    path.display()
                ))
            })?;
            authority_dir.sync_all().map_err(|error| {
                AgentError::Storage(format!(
                    "sync remote effect journal directory {}: {error}",
                    authority_dir.display().display()
                ))
            })?;
        }
        let recovery = recover_file(&mut file, &path)?;
        file.seek(SeekFrom::End(0)).map_err(|error| {
            AgentError::Storage(format!(
                "seek remote effect journal {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self {
            _authority_dir: authority_dir,
            path,
            writer: Mutex::new(WriterState {
                file,
                recovery,
                failed: None,
            }),
        })
    }

    pub(crate) fn record_reserved(
        &self,
        context: &OperationEffectContext,
        idempotency_key: Option<&str>,
    ) -> AgentResult<()> {
        self.append(JournalTransition::Reserved {
            context: Box::new(context.clone()),
            idempotency_key: normalize_key(idempotency_key)?,
        })
    }

    pub(crate) fn record_dispatched(&self, effect_id: EffectId) -> AgentResult<()> {
        self.append(JournalTransition::Dispatched { effect_id })
    }

    pub(crate) fn record_acked(
        &self,
        effect_id: EffectId,
        ack: RemoteEffectAck,
    ) -> AgentResult<()> {
        self.append(JournalTransition::Acknowledged { effect_id, ack })
    }

    pub(crate) fn reconcile(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<EffectReconciliation> {
        context.validate().map_err(AgentError::InvalidRequest)?;
        let writer = self.writer.lock().expect("remote journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        let Some(evidence) = writer.recovery.effects.get(&context.effect_id) else {
            return Ok(EffectReconciliation::NotManaged);
        };
        if evidence.context != *context {
            return Ok(EffectReconciliation::Ambiguous {
                reason: bounded_reason(format!(
                    "remote effect {} identity drifted from the Core snapshot",
                    context.effect_id
                )),
            });
        }
        if !evidence.dispatched {
            return Ok(EffectReconciliation::NotApplied {
                evidence: Some("remote request was reserved but never dispatched".into()),
            });
        }
        match evidence.ack {
            None => Ok(EffectReconciliation::Ambiguous {
                reason: bounded_reason(format!(
                    "remote effect {} was dispatched without a durable acknowledgement; the peer may have applied it",
                    context.effect_id
                )),
            }),
            Some(RemoteEffectAck::Staged) => Ok(EffectReconciliation::Ambiguous {
                reason: bounded_reason(
                    "remote call staged a Core effect but workspace recovery found no matching mutation evidence"
                        .into(),
                ),
            }),
            Some(ack) => Ok(EffectReconciliation::CompletedValue {
                evidence: Some(bounded_evidence(format!(
                    "remote-effect:{}:ack:{}",
                    context.effect_id,
                    ack_name(ack)
                ))),
            }),
        }
    }

    fn append(&self, transition: JournalTransition) -> AgentResult<()> {
        validate_transition(&transition).map_err(AgentError::InvalidRequest)?;
        let mut writer = self.writer.lock().expect("remote journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        if writer.recovery.last_seq >= MAX_RECORDS as u64 {
            return Err(AgentError::RecoveryRequired(format!(
                "remote effect journal reached its {MAX_RECORDS} record recovery limit"
            )));
        }
        validate_fold(&writer.recovery, &transition).map_err(AgentError::InvalidRequest)?;
        let seq = writer.recovery.last_seq.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("remote effect journal sequence exhausted".into())
        })?;
        let record = JournalRecord {
            version: JOURNAL_VERSION,
            seq,
            transition,
        };
        if let Err(error) = append_record(&mut writer.file, &record) {
            let message = format!(
                "remote effect journal {} failed permanently: {error}",
                self.path.display()
            );
            writer.failed = Some(message.clone());
            return Err(AgentError::Storage(message));
        }
        apply_transition(&mut writer.recovery, &record.transition).expect("validated transition");
        writer.recovery.last_seq = seq;
        Ok(())
    }
}

fn normalize_key(key: Option<&str>) -> AgentResult<Option<String>> {
    let Some(key) = key.map(str::trim).filter(|key| !key.is_empty()) else {
        return Ok(None);
    };
    if key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(AgentError::InvalidRequest(format!(
            "remote idempotency key exceeds {MAX_IDEMPOTENCY_KEY_BYTES} bytes"
        )));
    }
    if key.bytes().any(|byte| byte < 0x20 || byte > 0x7e) {
        return Err(AgentError::InvalidRequest(
            "remote idempotency key must be printable ASCII".into(),
        ));
    }
    Ok(Some(key.to_owned()))
}

fn validate_transition(transition: &JournalTransition) -> Result<(), String> {
    match transition {
        JournalTransition::Reserved {
            context,
            idempotency_key,
        } => {
            context.validate()?;
            if let Some(key) = idempotency_key
                && (key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES)
            {
                return Err("remote idempotency key is out of bounds".into());
            }
            Ok(())
        }
        JournalTransition::Dispatched { effect_id }
        | JournalTransition::Acknowledged { effect_id, .. } => {
            if effect_id.0.is_nil() {
                Err("remote effect id is nil".into())
            } else {
                Ok(())
            }
        }
    }
}

fn in_flight_key<'a>(state: &'a RecoveryState, key: &str) -> Option<&'a RemoteEvidence> {
    state.effects.values().find(|evidence| {
        evidence.ack.is_none()
            && evidence
                .idempotency_key
                .as_deref()
                .is_some_and(|held| held == key)
    })
}

fn validate_fold(state: &RecoveryState, transition: &JournalTransition) -> Result<(), String> {
    match transition {
        JournalTransition::Reserved {
            context,
            idempotency_key,
        } => {
            if state.effects.contains_key(&context.effect_id) {
                return Err(format!(
                    "effect {} already has a remote reservation",
                    context.effect_id
                ));
            }
            if let Some(key) = idempotency_key
                && in_flight_key(state, key).is_some()
            {
                return Err(format!(
                    "remote idempotency key is already in flight; at-most-one commit refuses a second dispatch"
                ));
            }
            Ok(())
        }
        JournalTransition::Dispatched { effect_id } => match state.effects.get(effect_id) {
            Some(evidence) if !evidence.dispatched && evidence.ack.is_none() => Ok(()),
            Some(_) => Err(format!(
                "remote effect {effect_id} is not waiting to be dispatched"
            )),
            None => Err(format!("remote effect {effect_id} has no reservation")),
        },
        JournalTransition::Acknowledged { effect_id, .. } => match state.effects.get(effect_id) {
            Some(evidence) if evidence.dispatched && evidence.ack.is_none() => Ok(()),
            Some(_) => Err(format!(
                "remote effect {effect_id} is not waiting for acknowledgement"
            )),
            None => Err(format!("remote effect {effect_id} has no reservation")),
        },
    }
}

fn apply_transition(
    state: &mut RecoveryState,
    transition: &JournalTransition,
) -> Result<(), String> {
    match transition {
        JournalTransition::Reserved {
            context,
            idempotency_key,
        } => {
            state.effects.insert(
                context.effect_id,
                RemoteEvidence {
                    context: context.as_ref().clone(),
                    idempotency_key: idempotency_key.clone(),
                    dispatched: false,
                    ack: None,
                },
            );
            Ok(())
        }
        JournalTransition::Dispatched { effect_id } => {
            let evidence = state
                .effects
                .get_mut(effect_id)
                .ok_or_else(|| "missing remote reservation".to_string())?;
            evidence.dispatched = true;
            Ok(())
        }
        JournalTransition::Acknowledged { effect_id, ack } => {
            let evidence = state
                .effects
                .get_mut(effect_id)
                .ok_or_else(|| "missing remote reservation".to_string())?;
            evidence.ack = Some(*ack);
            Ok(())
        }
    }
}

fn append_record(file: &mut File, record: &JournalRecord) -> AgentResult<()> {
    let payload = serde_json::to_vec(record).map_err(|error| {
        AgentError::Storage(format!("serialize remote journal record: {error}"))
    })?;
    let frame = StoredFrame {
        checksum: checksum_hex(&payload),
        record: record.clone(),
    };
    let encoded = serde_json::to_vec(&frame)
        .map_err(|error| AgentError::Storage(format!("serialize remote journal frame: {error}")))?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(AgentError::Storage(format!(
            "remote effect journal frame exceeds {MAX_FRAME_BYTES} bytes"
        )));
    }
    let projected = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat remote effect journal: {error}")))?
        .len()
        .checked_add(encoded.len() as u64 + 1)
        .ok_or_else(|| AgentError::Storage("remote effect journal size overflow".into()))?;
    if projected > MAX_FILE_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "remote effect journal reached its {MAX_FILE_BYTES} byte hard limit"
        )));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(&encoded))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| AgentError::Storage(format!("persist remote effect journal: {error}")))
}

fn recover_file(file: &mut File, path: &Path) -> AgentResult<RecoveryState> {
    let size = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat remote effect journal: {error}")))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(corrupt(path, "file exceeds hard limit"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| AgentError::Storage(format!("seek remote effect journal: {error}")))?;
    let mut reader =
        BufReader::new(file.try_clone().map_err(|error| {
            AgentError::Storage(format!("clone remote effect journal: {error}"))
        })?);
    let mut state = RecoveryState::default();
    let mut offset = 0_u64;
    let mut last_good = 0_u64;
    let mut torn_tail = false;
    loop {
        let mut bytes = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_FRAME_BYTES + 2) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|error| AgentError::Storage(format!("read remote effect journal: {error}")))?;
        if read == 0 {
            break;
        }
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| corrupt(path, "offset overflow"))?;
        if !bytes.ends_with(b"\n") {
            if offset == size {
                torn_tail = true;
                break;
            }
            return Err(corrupt(path, "partial middle frame"));
        }
        if bytes.len() > MAX_FRAME_BYTES + 1 {
            return Err(corrupt(path, "frame exceeds bound"));
        }
        let frame: StoredFrame = serde_json::from_slice(&bytes[..bytes.len() - 1])
            .map_err(|_| corrupt(path, "malformed frame"))?;
        let payload =
            serde_json::to_vec(&frame.record).map_err(|_| corrupt(path, "reserialize frame"))?;
        if checksum_hex(&payload) != frame.checksum {
            return Err(corrupt(path, "checksum mismatch"));
        }
        if frame.record.version != JOURNAL_VERSION {
            return Err(corrupt(path, "unsupported version"));
        }
        if frame.record.seq != state.last_seq + 1 {
            return Err(corrupt(path, "sequence gap"));
        }
        validate_transition(&frame.record.transition).map_err(|detail| corrupt(path, &detail))?;
        validate_fold(&state, &frame.record.transition).map_err(|detail| corrupt(path, &detail))?;
        apply_transition(&mut state, &frame.record.transition)
            .map_err(|detail| corrupt(path, &detail))?;
        state.last_seq = frame.record.seq;
        last_good = offset;
    }
    if torn_tail {
        file.set_len(last_good)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                AgentError::Storage(format!("repair remote effect journal tail: {error}"))
            })?;
    }
    Ok(state)
}

fn checksum_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn corrupt(path: &Path, detail: &str) -> AgentError {
    AgentError::RecoveryRequired(format!(
        "remote effect journal {} is corrupt: {detail}",
        path.display()
    ))
}

fn bounded_evidence(mut text: String) -> String {
    if text.len() > 512 {
        let mut boundary = 512;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
    text
}

fn bounded_reason(mut reason: String) -> String {
    if reason.len() > MAX_REASON_BYTES {
        let mut boundary = MAX_REASON_BYTES;
        while !reason.is_char_boundary(boundary) {
            boundary -= 1;
        }
        reason.truncate(boundary);
    }
    reason
}

fn ack_name(ack: RemoteEffectAck) -> &'static str {
    match ack {
        RemoteEffectAck::Completed => "completed",
        RemoteEffectAck::Staged => "staged",
        RemoteEffectAck::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ArgumentDigest, EffectId, EffectReconciler, OperationId, RunId, ToolOperationIdentity,
        TurnId,
    };

    fn remote_context(tool_name: &str) -> OperationEffectContext {
        OperationEffectContext {
            identity: ToolOperationIdentity {
                run_id: RunId::new(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id: OperationId::new(),
                generation: 1,
                call_id: "call-1".into(),
                tool_name: tool_name.into(),
                argument_digest: ArgumentDigest::sha256_bytes(b"args"),
            },
            effect_id: EffectId::new(),
        }
    }

    async fn workspace() -> (crate::Workspace, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::Workspace::open(directory.path()).await.unwrap();
        (workspace, directory)
    }

    #[tokio::test]
    async fn unknown_effect_stays_unmanaged() {
        let (workspace, _directory) = workspace().await;
        assert!(matches!(
            workspace.reconcile(&remote_context("cap.remote")).unwrap(),
            EffectReconciliation::NotManaged
        ));
    }

    #[tokio::test]
    async fn reserved_without_dispatch_is_not_applied() {
        let (workspace, _directory) = workspace().await;
        let context = remote_context("cap.remote");
        workspace
            .record_remote_reserved(&context, Some(&context.identity.operation_id.to_string()))
            .unwrap();
        assert!(matches!(
            workspace.reconcile(&context).unwrap(),
            EffectReconciliation::NotApplied { .. }
        ));
    }

    #[tokio::test]
    async fn dispatched_without_ack_is_ambiguous() {
        let (workspace, directory) = workspace().await;
        let context = remote_context("mcp.call");
        workspace
            .record_remote_reserved(&context, Some("remote-key-1"))
            .unwrap();
        workspace
            .record_remote_dispatched(context.effect_id)
            .unwrap();
        drop(workspace);

        let reopened = crate::Workspace::open(directory.path()).await.unwrap();
        assert!(matches!(
            reopened.reconcile(&context).unwrap(),
            EffectReconciliation::Ambiguous { .. }
        ));
    }

    #[tokio::test]
    async fn durable_ack_settles_as_completed_value() {
        let (workspace, _directory) = workspace().await;
        let context = remote_context("cap.remote");
        workspace.record_remote_reserved(&context, None).unwrap();
        workspace
            .record_remote_dispatched(context.effect_id)
            .unwrap();
        workspace
            .record_remote_acked(context.effect_id, RemoteEffectAck::Completed)
            .unwrap();
        assert!(matches!(
            workspace.reconcile(&context).unwrap(),
            EffectReconciliation::CompletedValue { .. }
        ));
    }

    #[tokio::test]
    async fn in_flight_idempotency_key_refuses_a_second_dispatch() {
        let (workspace, _directory) = workspace().await;
        let first = remote_context("cap.remote");
        let second = remote_context("cap.remote");
        workspace
            .record_remote_reserved(&first, Some("same-key"))
            .unwrap();
        workspace.record_remote_dispatched(first.effect_id).unwrap();
        let error = workspace
            .record_remote_reserved(&second, Some("same-key"))
            .unwrap_err();
        assert!(
            error.to_string().contains("already in flight"),
            "duplicate in-flight key must fail closed: {error}"
        );
        workspace
            .record_remote_acked(first.effect_id, RemoteEffectAck::Failed)
            .unwrap();
        workspace
            .record_remote_reserved(&second, Some("same-key"))
            .unwrap();
    }

    #[tokio::test]
    async fn staged_ack_without_workspace_evidence_stays_ambiguous() {
        let (workspace, _directory) = workspace().await;
        let context = remote_context("cap.write");
        workspace.record_remote_reserved(&context, None).unwrap();
        workspace
            .record_remote_dispatched(context.effect_id)
            .unwrap();
        workspace
            .record_remote_acked(context.effect_id, RemoteEffectAck::Staged)
            .unwrap();
        assert!(matches!(
            workspace.reconcile(&context).unwrap(),
            EffectReconciliation::Ambiguous { .. }
        ));
    }
}
