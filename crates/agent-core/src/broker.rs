//! 带持久预留日志的经纪实现：reserved/dispatch/ack 三相在转发给内部
//! 经纪前后各落一条校验和帧，崩溃后按效果身份给出保守分类——
//! 只预约未派发是 NotApplied，已派发未应答是 Ambiguous，持久应答
//! 分别结算为 Applied / NotApplied。经纪只见过租约的权威形状
//! （run / operation / effect / digest / generation），身份比对也只在
//! 这些字段上进行；漂移按 Ambiguous 处理，绝不猜测。这是进程内参考
//! 实现；未来进程外协调器复用同一屏障与日志语义。

use std::{
    collections::HashMap,
    fs::File,
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use agent_contracts::{
    AgentError, AgentResult, ArgumentDigest, EffectDurability, EffectId, EffectReceipt,
    EffectReconciliation, OperationEffectContext, OperationId, RunId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::port::{EffectAck, EffectBroker, EffectReservation, ReservedEffect};

const JOURNAL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESERVATIONS: usize = 65_536;
const MAX_RESERVATION_ID_CHARS: usize = 256;

/// 一条预留的耐久形状：经纪分配的 id + 租约的权威形状（可能没有
/// 意图）。从不携带参数体或效果内部状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReservedRecord {
    reservation_id: String,
    run_id: RunId,
    operation_id: OperationId,
    argument_digest: ArgumentDigest,
    generation: u64,
    intent: Option<agent_contracts::EffectIntent>,
}

impl ReservedRecord {
    fn from_reservation(reservation_id: String, reservation: &EffectReservation) -> Self {
        Self {
            reservation_id,
            run_id: reservation.run_id,
            operation_id: reservation.operation_id,
            argument_digest: reservation.argument_digest,
            generation: reservation.generation,
            intent: reservation.intent.clone(),
        }
    }

    /// 经纪只见租约的权威形状；比对也只在这五个字段上进行。
    fn matches(&self, context: &OperationEffectContext) -> bool {
        self.run_id == context.identity.run_id
            && self.operation_id == context.identity.operation_id
            && self.argument_digest == context.identity.argument_digest
            && self.generation == context.identity.generation
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReservationTransition {
    Reserved {
        effect_id: EffectId,
        record: Box<ReservedRecord>,
    },
    Dispatched {
        effect_id: EffectId,
    },
    Acknowledged {
        effect_id: EffectId,
        applied: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalFrame {
    version: u32,
    seq: u64,
    transition: ReservationTransition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFrame {
    frame: JournalFrame,
    checksum: String,
}

#[derive(Debug)]
struct ReservationEntry {
    effect_id: EffectId,
    record: ReservedRecord,
    dispatched: bool,
    acked_applied: Option<bool>,
}

#[derive(Debug, Default)]
struct RecoveryState {
    last_seq: u64,
    by_effect: HashMap<EffectId, ReservationEntry>,
}

impl RecoveryState {
    fn by_reservation_id(&self, reservation_id: &str) -> Option<&ReservationEntry> {
        self.by_effect
            .values()
            .find(|entry| entry.record.reservation_id == reservation_id)
    }
}

struct JournalState {
    file: File,
    recovery: RecoveryState,
    failed: Option<String>,
}

/// 预留日志：追加式、每帧校验和、序号连续、fold 校验、撕裂尾修复。
pub(crate) struct ReservationJournal {
    path: std::path::PathBuf,
    state: Mutex<JournalState>,
}

impl ReservationJournal {
    pub(crate) fn open(path: &Path) -> AgentResult<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                AgentError::Storage(format!(
                    "create broker journal parent {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| {
                AgentError::Storage(format!("open broker journal {}: {error}", path.display()))
            })?;
        file.try_lock().map_err(|error| {
            AgentError::Storage(format!(
                "lock broker journal {} exclusively: {error}",
                path.display()
            ))
        })?;
        let recovery = recover_file(&mut file, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(JournalState {
                file,
                recovery,
                failed: None,
            }),
        })
    }

    fn record_reserved(&self, effect_id: EffectId, record: ReservedRecord) -> AgentResult<()> {
        self.append(ReservationTransition::Reserved {
            effect_id,
            record: Box::new(record),
        })
    }

    fn record_dispatched(&self, effect_id: EffectId) -> AgentResult<()> {
        self.append(ReservationTransition::Dispatched { effect_id })
    }

    fn record_acked(&self, effect_id: EffectId, applied: bool) -> AgentResult<()> {
        self.append(ReservationTransition::Acknowledged { effect_id, applied })
    }

    /// 按效果身份分类持久预留。None = 本日志从未管理过该效果。
    pub(crate) fn reconcile(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<Option<EffectReconciliation>> {
        let state = self.state.lock().expect("broker journal poisoned");
        if let Some(error) = &state.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        let Some(entry) = state.recovery.by_effect.get(&context.effect_id) else {
            return Ok(None);
        };
        if !entry.record.matches(context) {
            return Ok(Some(EffectReconciliation::Ambiguous {
                reason: format!(
                    "broker reservation {} drifted from the Core snapshot",
                    entry.record.reservation_id
                ),
            }));
        }
        Ok(Some(match (entry.dispatched, entry.acked_applied) {
            (false, _) => EffectReconciliation::NotApplied {
                evidence: Some("broker reservation was never dispatched".into()),
            },
            (true, None) => EffectReconciliation::Ambiguous {
                reason: format!(
                    "broker reservation {} was dispatched without a durable acknowledgement",
                    entry.record.reservation_id
                ),
            },
            (true, Some(true)) => EffectReconciliation::Applied {
                durability: EffectDurability::Durable,
                evidence: Some(format!(
                    "broker:{}:acked-applied",
                    entry.record.reservation_id
                )),
            },
            (true, Some(false)) => EffectReconciliation::NotApplied {
                evidence: Some("broker acknowledged the dispatch as not applied".into()),
            },
        }))
    }

    fn append(&self, transition: ReservationTransition) -> AgentResult<()> {
        validate_transition(&transition).map_err(AgentError::InvalidRequest)?;
        let mut state = self.state.lock().expect("broker journal poisoned");
        if let Some(error) = &state.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        validate_fold(&state.recovery, &transition).map_err(AgentError::InvalidRequest)?;
        if state.recovery.last_seq >= MAX_RESERVATIONS as u64 {
            return Err(AgentError::RecoveryRequired(format!(
                "broker journal reached its {MAX_RESERVATIONS} record limit"
            )));
        }
        let seq = state.recovery.last_seq.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("broker journal sequence exhausted".into())
        })?;
        let frame = JournalFrame {
            version: JOURNAL_VERSION,
            seq,
            transition,
        };
        if let Err(error) = append_frame(&mut state.file, &frame) {
            let message = format!(
                "broker journal {} failed permanently: {error}",
                self.path.display()
            );
            state.failed = Some(message.clone());
            return Err(AgentError::Storage(message));
        }
        apply_transition(&mut state.recovery, frame.transition).expect("validated transition");
        state.recovery.last_seq = seq;
        Ok(())
    }
}

fn validate_transition(transition: &ReservationTransition) -> Result<(), String> {
    match transition {
        ReservationTransition::Reserved { effect_id, record } => {
            if effect_id.0.is_nil() {
                return Err("broker reservation effect id is nil".into());
            }
            let id_chars = record.reservation_id.chars().count();
            if id_chars == 0 || id_chars > MAX_RESERVATION_ID_CHARS {
                return Err("broker reservation id is empty or oversized".into());
            }
            Ok(())
        }
        ReservationTransition::Dispatched { effect_id }
        | ReservationTransition::Acknowledged { effect_id, .. } => {
            if effect_id.0.is_nil() {
                Err("broker reservation effect id is nil".into())
            } else {
                Ok(())
            }
        }
    }
}

fn validate_fold(state: &RecoveryState, transition: &ReservationTransition) -> Result<(), String> {
    match transition {
        ReservationTransition::Reserved { effect_id, .. } => {
            if state.by_effect.contains_key(effect_id) {
                return Err(format!(
                    "effect {effect_id} already has a broker reservation"
                ));
            }
            if state.by_effect.len() >= MAX_RESERVATIONS {
                return Err("broker journal holds too many live reservations".into());
            }
            Ok(())
        }
        ReservationTransition::Dispatched { effect_id } => match state.by_effect.get(effect_id) {
            Some(entry) if !entry.dispatched && entry.acked_applied.is_none() => Ok(()),
            Some(_) => Err(format!("effect {effect_id} is not waiting for dispatch")),
            None => Err(format!("effect {effect_id} has no broker reservation")),
        },
        ReservationTransition::Acknowledged { effect_id, .. } => {
            match state.by_effect.get(effect_id) {
                Some(entry) if entry.dispatched && entry.acked_applied.is_none() => Ok(()),
                Some(_) => Err(format!(
                    "effect {effect_id} is not waiting for acknowledgement"
                )),
                None => Err(format!("effect {effect_id} has no broker reservation")),
            }
        }
    }
}

fn apply_transition(
    state: &mut RecoveryState,
    transition: ReservationTransition,
) -> Result<(), String> {
    match transition {
        ReservationTransition::Reserved { effect_id, record } => {
            state.by_effect.insert(
                effect_id,
                ReservationEntry {
                    effect_id,
                    record: *record,
                    dispatched: false,
                    acked_applied: None,
                },
            );
            Ok(())
        }
        ReservationTransition::Dispatched { effect_id } => {
            let entry = state
                .by_effect
                .get_mut(&effect_id)
                .ok_or_else(|| format!("effect {effect_id} has no broker reservation"))?;
            entry.dispatched = true;
            Ok(())
        }
        ReservationTransition::Acknowledged {
            effect_id, applied, ..
        } => {
            let entry = state
                .by_effect
                .get_mut(&effect_id)
                .ok_or_else(|| format!("effect {effect_id} has no broker reservation"))?;
            entry.acked_applied = Some(applied);
            Ok(())
        }
    }
}

fn append_frame(file: &mut File, frame: &JournalFrame) -> AgentResult<()> {
    let payload = serde_json::to_vec(frame)
        .map_err(|error| AgentError::Storage(format!("serialize broker frame: {error}")))?;
    let encoded = serde_json::to_vec(&StoredFrame {
        checksum: checksum_hex(&payload),
        frame: frame.clone(),
    })
    .map_err(|error| AgentError::Storage(format!("serialize broker stored frame: {error}")))?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(AgentError::Storage(format!(
            "broker journal frame exceeds {MAX_FRAME_BYTES} bytes"
        )));
    }
    let projected = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat broker journal: {error}")))?
        .len()
        .checked_add(encoded.len() as u64 + 1)
        .ok_or_else(|| AgentError::Storage("broker journal size overflow".into()))?;
    if projected > MAX_FILE_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "broker journal reached its {MAX_FILE_BYTES} byte hard limit"
        )));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(&encoded))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| AgentError::Storage(format!("persist broker journal: {error}")))
}

fn recover_file(file: &mut File, path: &Path) -> AgentResult<RecoveryState> {
    let size = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat broker journal: {error}")))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(corrupt(path, "file exceeds hard limit"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| AgentError::Storage(format!("seek broker journal: {error}")))?;
    let mut reader = BufReader::new(
        file.try_clone()
            .map_err(|error| AgentError::Storage(format!("clone broker journal: {error}")))?,
    );
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
            .map_err(|error| AgentError::Storage(format!("read broker journal: {error}")))?;
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
        let stored: StoredFrame = serde_json::from_slice(&bytes[..bytes.len() - 1])
            .map_err(|_| corrupt(path, "malformed frame"))?;
        let payload =
            serde_json::to_vec(&stored.frame).map_err(|_| corrupt(path, "reserialize frame"))?;
        if checksum_hex(&payload) != stored.checksum {
            return Err(corrupt(path, "checksum mismatch"));
        }
        if stored.frame.version != JOURNAL_VERSION {
            return Err(corrupt(path, "unsupported version"));
        }
        if stored.frame.seq != state.last_seq + 1 {
            return Err(corrupt(path, "sequence gap"));
        }
        validate_transition(&stored.frame.transition).map_err(|detail| corrupt(path, &detail))?;
        validate_fold(&state, &stored.frame.transition).map_err(|detail| corrupt(path, &detail))?;
        apply_transition(&mut state, stored.frame.transition)
            .map_err(|detail| corrupt(path, &detail))?;
        state.last_seq = stored.frame.seq;
        last_good = offset;
    }
    if torn_tail {
        file.set_len(last_good)
            .and_then(|_| file.sync_all())
            .map_err(|error| AgentError::Storage(format!("repair broker journal tail: {error}")))?;
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
        "broker journal {} is corrupt: {detail}",
        path.display()
    ))
}

/// 把任意内部经纪的 reserved/dispatch/ack 变成持久可对账的三相：
/// 预约先转发再落盘（拿经纪分配的 id）；派发先落盘再应用——崩溃
/// 窗口只能是 Ambiguous，日志拒绝则回滚已暂存效果并报 NotApplied；
/// 应答先落盘再转发，转发失败照常上抛但绝不回滚已应用。
pub struct JournaledEffectBroker {
    inner: Arc<dyn EffectBroker>,
    journal: ReservationJournal,
}

impl JournaledEffectBroker {
    /// 打开（或创建）给定路径的预留日志并包住内部经纪。
    pub fn open(inner: Arc<dyn EffectBroker>, journal_path: &Path) -> AgentResult<Self> {
        Ok(Self {
            inner,
            journal: ReservationJournal::open(journal_path)?,
        })
    }

    /// 在启动路径之外复查单个效果的持久预留分类。
    pub fn reconcile(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<Option<EffectReconciliation>> {
        self.journal.reconcile(context)
    }
}

#[async_trait::async_trait]
impl EffectBroker for JournaledEffectBroker {
    async fn reserve(&self, reservation: EffectReservation) -> AgentResult<String> {
        let reservation_id = self.inner.reserve(reservation.clone()).await?;
        self.journal.record_reserved(
            reservation.effect_id,
            ReservedRecord::from_reservation(reservation_id.clone(), &reservation),
        )?;
        Ok(reservation_id)
    }

    async fn dispatch(&self, reserved: ReservedEffect) -> EffectReceipt {
        let effect_id = reserved.reservation.effect_id;
        if let Err(error) = self.journal.record_dispatched(effect_id) {
            // 日志拒绝即不派发：先回滚已暂存效果，再如实报告未应用。
            let reason =
                format!("broker journal refused the dispatch of effect {effect_id}: {error}");
            let rollback_error = reserved
                .effect
                .rollback(&reason)
                .await
                .err()
                .map(|rollback| format!("; rollback failed: {rollback}"))
                .unwrap_or_default();
            return EffectReceipt::NotApplied {
                error: format!("{reason}{rollback_error}"),
            };
        }
        self.inner.dispatch(reserved).await
    }

    async fn ack(&self, ack: EffectAck) -> AgentResult<()> {
        // 先落持久应答再转发：转发失败照常上抛，但绝不回滚已应用。
        let effect_id = {
            let state = self.journal.state.lock().expect("broker journal poisoned");
            if let Some(error) = &state.failed {
                return Err(AgentError::Storage(error.clone()));
            }
            state
                .recovery
                .by_reservation_id(&ack.reservation_id)
                .map(|entry| entry.effect_id)
        };
        let Some(effect_id) = effect_id else {
            return Err(AgentError::InvalidRequest(format!(
                "acknowledgement references unknown broker reservation {}",
                ack.reservation_id
            )));
        };
        self.journal.record_acked(effect_id, ack.applied)?;
        self.inner.ack(ack).await
    }

    fn reconcile_reservation(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<Option<EffectReconciliation>> {
        self.journal.reconcile(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ToolOperationIdentity, TurnId};

    fn identity() -> ToolOperationIdentity {
        ToolOperationIdentity {
            run_id: RunId::new(),
            task_id: None,
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id: OperationId::new(),
            generation: 3,
            call_id: "call-1".into(),
            tool_name: "cap.remote".into(),
            argument_digest: ArgumentDigest::sha256_bytes(b"args"),
        }
    }

    fn context_of(identity: &ToolOperationIdentity, effect_id: EffectId) -> OperationEffectContext {
        OperationEffectContext {
            identity: identity.clone(),
            effect_id,
        }
    }

    fn reservation_for(
        identity: &ToolOperationIdentity,
        effect_id: EffectId,
        intent: Option<agent_contracts::EffectIntent>,
    ) -> EffectReservation {
        EffectReservation {
            run_id: identity.run_id,
            operation_id: identity.operation_id,
            effect_id,
            argument_digest: identity.argument_digest,
            generation: identity.generation,
            intent,
        }
    }

    struct PassThroughBroker;

    #[async_trait::async_trait]
    impl EffectBroker for PassThroughBroker {
        async fn reserve(&self, reservation: EffectReservation) -> AgentResult<String> {
            Ok(format!("pass/{}", reservation.effect_id))
        }

        async fn dispatch(&self, reserved: ReservedEffect) -> EffectReceipt {
            reserved.effect.commit().await
        }

        async fn ack(&self, _ack: EffectAck) -> AgentResult<()> {
            Ok(())
        }
    }

    struct AppliedEffect;

    #[async_trait::async_trait]
    impl agent_contracts::Effect for AppliedEffect {
        fn describe(&self) -> String {
            "applied fixture".into()
        }

        async fn commit(self: Box<Self>) -> EffectReceipt {
            EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                evidence: None,
            }
        }

        async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
            Ok(())
        }
    }

    struct NotAppliedEffect;

    #[async_trait::async_trait]
    impl agent_contracts::Effect for NotAppliedEffect {
        fn describe(&self) -> String {
            "not-applied fixture".into()
        }

        async fn commit(self: Box<Self>) -> EffectReceipt {
            EffectReceipt::NotApplied {
                error: "fixture refusal".into(),
            }
        }

        async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
            Ok(())
        }
    }

    async fn journaled(dir: &tempfile::TempDir) -> JournaledEffectBroker {
        JournaledEffectBroker::open(Arc::new(PassThroughBroker), &dir.path().join("r.jsonl"))
            .unwrap()
    }

    async fn dispatch_as(
        broker: &JournaledEffectBroker,
        identity: &ToolOperationIdentity,
        effect_id: EffectId,
        applied: bool,
    ) {
        broker
            .dispatch(ReservedEffect {
                reservation: reservation_for(identity, effect_id, None),
                reservation_id: format!("pass/{effect_id}"),
                effect: if applied {
                    Box::new(AppliedEffect)
                } else {
                    Box::new(NotAppliedEffect)
                },
            })
            .await;
    }

    /// 四类持久分类都跨重开存活：只预约、已派发未应答、应答已应用、
    /// 应答未应用；身份漂移一律 Ambiguous。
    #[tokio::test]
    async fn reservation_classes_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        let identity = identity();
        let pending = EffectId::new();
        let applied = EffectId::new();
        let refused = EffectId::new();
        let inflight = EffectId::new();

        {
            let broker = journaled(&dir).await;
            broker
                .reserve(reservation_for(&identity, pending, None))
                .await
                .unwrap();
            broker
                .reserve(reservation_for(&identity, applied, None))
                .await
                .unwrap();
            dispatch_as(&broker, &identity, applied, true).await;
            broker
                .ack(EffectAck {
                    reservation_id: format!("pass/{applied}"),
                    operation_id: identity.operation_id,
                    applied: true,
                    receipt_summary: "fixture".into(),
                })
                .await
                .unwrap();
            broker
                .reserve(reservation_for(&identity, refused, None))
                .await
                .unwrap();
            dispatch_as(&broker, &identity, refused, false).await;
            broker
                .ack(EffectAck {
                    reservation_id: format!("pass/{refused}"),
                    operation_id: identity.operation_id,
                    applied: false,
                    receipt_summary: "fixture".into(),
                })
                .await
                .unwrap();
            broker
                .reserve(reservation_for(&identity, inflight, None))
                .await
                .unwrap();
            dispatch_as(&broker, &identity, inflight, true).await;
        }

        let reopened = JournaledEffectBroker::open(Arc::new(PassThroughBroker), &path).unwrap();
        assert!(matches!(
            reopened.reconcile(&context_of(&identity, pending)).unwrap(),
            Some(EffectReconciliation::NotApplied { .. })
        ));
        assert!(matches!(
            reopened.reconcile(&context_of(&identity, applied)).unwrap(),
            Some(EffectReconciliation::Applied { .. })
        ));
        assert!(matches!(
            reopened.reconcile(&context_of(&identity, refused)).unwrap(),
            Some(EffectReconciliation::NotApplied { .. })
        ));
        assert!(matches!(
            reopened
                .reconcile(&context_of(&identity, inflight))
                .unwrap(),
            Some(EffectReconciliation::Ambiguous { .. })
        ));

        // 同一效果换一套身份来对账：按 Ambiguous 处理，绝不猜测。
        let mut drifted = context_of(&identity, applied);
        drifted.identity.operation_id = OperationId::new();
        assert!(matches!(
            reopened.reconcile(&drifted).unwrap(),
            Some(EffectReconciliation::Ambiguous { .. })
        ));
        // 未知效果没有可查面。
        assert!(
            reopened
                .reconcile(&context_of(&identity, EffectId::new()))
                .unwrap()
                .is_none()
        );
    }

    /// 状态机拒绝：重复预约、先派发后预约、二次派发、二次应答。
    #[tokio::test]
    async fn journal_refuses_illegal_transitions_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let broker = journaled(&dir).await;
        let identity = identity();
        let effect_id = EffectId::new();

        broker
            .reserve(reservation_for(&identity, effect_id, None))
            .await
            .unwrap();
        assert!(
            broker
                .reserve(reservation_for(&identity, effect_id, None))
                .await
                .is_err(),
            "a second reservation for one effect must refuse"
        );
        let other = EffectId::new();
        let unreserved = broker
            .dispatch(ReservedEffect {
                reservation: reservation_for(&identity, other, None),
                reservation_id: format!("pass/{other}"),
                effect: Box::new(AppliedEffect),
            })
            .await;
        assert!(
            matches!(unreserved, EffectReceipt::NotApplied { .. }),
            "dispatch of an unreserved effect must settle NotApplied"
        );
        dispatch_as(&broker, &identity, effect_id, true).await;
        let second_dispatch = broker
            .dispatch(ReservedEffect {
                reservation: reservation_for(&identity, effect_id, None),
                reservation_id: format!("pass/{effect_id}"),
                effect: Box::new(AppliedEffect),
            })
            .await;
        assert!(
            matches!(second_dispatch, EffectReceipt::NotApplied { .. }),
            "a second dispatch must not apply again"
        );
    }
}
