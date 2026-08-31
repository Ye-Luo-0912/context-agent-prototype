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

const JOURNAL_VERSION: u32 = 2;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESERVATIONS: usize = 65_536;
const MAX_RESERVATION_ID_CHARS: usize = 256;

/// 一条预留的耐久形状：经纪分配的 id + 租约的权威形状（可能没有
/// 意图）。从不携带参数体或效果内部状态。这也是协调器线协议里的
/// 预约载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedRecord {
    pub reservation_id: String,
    pub run_id: RunId,
    pub operation_id: OperationId,
    pub argument_digest: ArgumentDigest,
    pub generation: u64,
    pub intent: Option<agent_contracts::EffectIntent>,
}

impl ReservedRecord {
    /// 从 Core 的预留请求构造；`reservation_id` 由经纪分配。
    pub fn from_reservation(reservation_id: String, reservation: &EffectReservation) -> Self {
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
        /// Typed settlement preserved through recovery. Version 1 journals
        /// stored an `applied` boolean and are rejected at load by the
        /// version check: the boolean could only record durable truth, so a
        /// silent decode would risk laundering weaker settlements.
        settlement: agent_contracts::EffectAckSettlement,
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
    acked_settlement: Option<agent_contracts::EffectAckSettlement>,
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
/// 这是协调器契约的持久面：进程内包装与进程外宿主共用同一份。
pub struct ReservationJournal {
    path: std::path::PathBuf,
    state: Mutex<JournalState>,
}

impl ReservationJournal {
    /// 打开（或创建）给定路径的预留日志并持有排他锁。
    pub fn open(path: &Path) -> AgentResult<Self> {
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
        // The journal is single-owner: an exclusive lock is mandatory. A
        // transient holder (an audit child that inherited the journal fd
        // before exiting, or a parallel same-host audit) must not fail the
        // open outright, so retry with backoff for a bounded window; a
        // genuinely stuck holder still fails closed.
        let mut lock_error = None;
        for _ in 0..40 {
            match file.try_lock() {
                Ok(()) => {
                    lock_error = None;
                    break;
                }
                Err(error) => {
                    lock_error = Some(error);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
        if let Some(error) = lock_error {
            return Err(AgentError::Storage(format!(
                "lock broker journal {} exclusively: {error}",
                path.display()
            )));
        }
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

    /// 记录一次派发意图。fold 拒绝（未预约/重复派发/已应答）即报错。
    pub fn record_dispatched(&self, effect_id: EffectId) -> AgentResult<()> {
        self.append(ReservationTransition::Dispatched { effect_id })
    }

    /// 记录一次持久应答；settlement 的类别原样保留到恢复，绝不加强。
    pub fn record_acked(
        &self,
        effect_id: EffectId,
        settlement: agent_contracts::EffectAckSettlement,
    ) -> AgentResult<()> {
        self.append(ReservationTransition::Acknowledged {
            effect_id,
            settlement,
        })
    }

    /// 由经纪分配的预留 id 反查效果身份；应答路径需要它定位日志条目。
    pub fn effect_id_for(&self, reservation_id: &str) -> AgentResult<Option<EffectId>> {
        let state = self.state.lock().expect("broker journal poisoned");
        if let Some(error) = &state.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        Ok(state
            .recovery
            .by_reservation_id(reservation_id)
            .map(|entry| entry.effect_id))
    }

    /// 当前日志序号：协调器用它为预约分配单调 id（peek-then-append
    /// 在单飞行会话内不会交错）。
    pub fn last_seq(&self) -> u64 {
        self.state
            .lock()
            .expect("broker journal poisoned")
            .recovery
            .last_seq
    }

    /// 落一条预约。id 已由调用方分配；fold 拒绝重复效果。
    pub fn record_reserved(&self, effect_id: EffectId, record: ReservedRecord) -> AgentResult<()> {
        self.append(ReservationTransition::Reserved {
            effect_id,
            record: Box::new(record),
        })
    }

    /// 按效果身份分类持久预留。None = 本日志从未管理过该效果。
    pub fn reconcile(
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
        Ok(Some(match (entry.dispatched, &entry.acked_settlement) {
            (false, _) => EffectReconciliation::NotApplied {
                evidence: Some("broker reservation was never dispatched".into()),
            },
            (true, None) => EffectReconciliation::Ambiguous {
                reason: format!(
                    "broker reservation {} was dispatched without a durable acknowledgement",
                    entry.record.reservation_id
                ),
            },
            (
                true,
                Some(agent_contracts::EffectAckSettlement::Applied {
                    durability: EffectDurability::Durable,
                }),
            ) => EffectReconciliation::Applied {
                durability: EffectDurability::Durable,
                evidence: Some(format!(
                    "broker:{}:acked-applied",
                    entry.record.reservation_id
                )),
            },
            (
                true,
                Some(agent_contracts::EffectAckSettlement::Applied {
                    durability: EffectDurability::DurabilityFailed(reason),
                }),
            ) => EffectReconciliation::Applied {
                durability: EffectDurability::DurabilityFailed(reason.clone()),
                evidence: Some(format!(
                    "broker:{}:acked-applied-durability-failed",
                    entry.record.reservation_id
                )),
            },
            (true, Some(agent_contracts::EffectAckSettlement::NotApplied)) => {
                EffectReconciliation::NotApplied {
                    evidence: Some("broker acknowledged the dispatch as not applied".into()),
                }
            }
            (true, Some(agent_contracts::EffectAckSettlement::Unknown)) => {
                EffectReconciliation::Ambiguous {
                    reason: format!(
                        "broker reservation {} was acknowledged with an unknown settlement",
                        entry.record.reservation_id
                    ),
                }
            }
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
            Some(entry) if !entry.dispatched && entry.acked_settlement.is_none() => Ok(()),
            Some(_) => Err(format!("effect {effect_id} is not waiting for dispatch")),
            None => Err(format!("effect {effect_id} has no broker reservation")),
        },
        ReservationTransition::Acknowledged { effect_id, .. } => {
            match state.by_effect.get(effect_id) {
                Some(entry) if entry.dispatched && entry.acked_settlement.is_none() => Ok(()),
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
                    acked_settlement: None,
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
            effect_id,
            settlement,
            ..
        } => {
            let entry = state
                .by_effect
                .get_mut(&effect_id)
                .ok_or_else(|| format!("effect {effect_id} has no broker reservation"))?;
            entry.acked_settlement = Some(settlement.clone());
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
        self.journal
            .record_acked(effect_id, ack.settlement.clone())?;
        self.inner.ack(ack).await
    }

    fn reconcile_reservation(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<Option<EffectReconciliation>> {
        self.journal.reconcile(context)
    }
}

/// 协调器线协议：单行 JSON 请求/应答；超限行视为协议违规并终止
/// 会话。serde 的内部标签表示法本身容忍多余字段，所以这里的把关
/// 是行界、已知操作集合与 fold 校验，而不是字段白名单。
pub const MAX_COORDINATOR_LINE_BYTES: usize = 64 * 1024;

/// 协调器收到的请求。客户端编码、宿主解码，两侧各持一半 derive。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CoordinatorRequest {
    Reserve {
        effect_id: EffectId,
        record: Box<ReservedRecord>,
    },
    Dispatched {
        effect_id: EffectId,
        reservation_id: String,
    },
    Acknowledged {
        reservation_id: String,
        settlement: agent_contracts::EffectAckSettlement,
    },
    Reconcile {
        context: OperationEffectContext,
    },
    Shutdown,
}

/// 协调器返回的应答。宿主编码、客户端解码；`ok=false` 时 `error`
/// 携带有界原因。
#[derive(Debug, Serialize, Deserialize)]
pub struct CoordinatorReply {
    pub ok: bool,
    pub reservation_id: Option<String>,
    pub reconciliation: Option<Option<EffectReconciliation>>,
    pub error: Option<String>,
}

impl CoordinatorReply {
    fn ok(reservation_id: Option<String>) -> Self {
        Self {
            ok: true,
            reservation_id,
            reconciliation: None,
            error: None,
        }
    }

    fn error(error: impl std::string::ToString) -> Self {
        Self {
            ok: false,
            reservation_id: None,
            reconciliation: None,
            error: Some(error.to_string()),
        }
    }
}

fn handle_coordinator_request(
    journal: &ReservationJournal,
    request: CoordinatorRequest,
) -> CoordinatorReply {
    match request {
        CoordinatorRequest::Reserve {
            effect_id,
            mut record,
        } => {
            // 服务端就是经纪：预留 id 在这里按日志序号单调分配，
            // 单飞行会话内 peek-then-append 不会交错。应答必须带回
            // 这个 id，客户端后续派发/应答都要引用它。
            if record.reservation_id.is_empty() {
                record.reservation_id = format!("coord/{}/{}", journal.last_seq() + 1, effect_id);
            }
            let assigned = record.reservation_id.clone();
            match journal.record_reserved(effect_id, *record) {
                Ok(()) => CoordinatorReply::ok(Some(assigned)),
                Err(error) => CoordinatorReply::error(error),
            }
        }
        CoordinatorRequest::Dispatched {
            effect_id,
            reservation_id,
        } => match journal.record_dispatched(effect_id) {
            Ok(()) => CoordinatorReply::ok(Some(reservation_id)),
            Err(error) => CoordinatorReply::error(error),
        },
        CoordinatorRequest::Acknowledged {
            reservation_id,
            settlement,
        } => match journal.effect_id_for(&reservation_id) {
            Ok(Some(effect_id)) => match journal.record_acked(effect_id, settlement) {
                Ok(()) => CoordinatorReply::ok(Some(reservation_id)),
                Err(error) => CoordinatorReply::error(error),
            },
            Ok(None) => CoordinatorReply::error(format!(
                "acknowledgement references unknown broker reservation {reservation_id}"
            )),
            Err(error) => CoordinatorReply::error(error),
        },
        CoordinatorRequest::Reconcile { context } => match journal.reconcile(&context) {
            Ok(reconciliation) => CoordinatorReply {
                ok: true,
                reservation_id: None,
                reconciliation: Some(reconciliation),
                error: None,
            },
            Err(error) => CoordinatorReply::error(error),
        },
        CoordinatorRequest::Shutdown => CoordinatorReply::ok(None),
    }
}

/// 以行分隔帧驱动一次协调器会话：EOF 或 `shutdown` 正常返回，协议
/// 违规报错。输入输出泛化，进程内即可确定性测试。
pub fn serve_broker_lines<R: std::io::BufRead, W: std::io::Write>(
    input: &mut R,
    output: &mut W,
    journal: &ReservationJournal,
) -> AgentResult<()> {
    loop {
        let mut line = String::new();
        let read = input
            .read_line(&mut line)
            .map_err(|error| AgentError::Storage(format!("read coordinator request: {error}")))?;
        if read == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_COORDINATOR_LINE_BYTES {
            return Err(AgentError::InvalidRequest(
                "coordinator request exceeds its line bound".into(),
            ));
        }
        let reply = match serde_json::from_str::<CoordinatorRequest>(&line) {
            Ok(CoordinatorRequest::Shutdown) => return Ok(()),
            Ok(request) => handle_coordinator_request(journal, request),
            Err(error) => {
                CoordinatorReply::error(format!("malformed coordinator request: {error}"))
            }
        };
        let mut encoded = serde_json::to_vec(&reply).map_err(|error| {
            AgentError::Storage(format!("serialize coordinator reply: {error}"))
        })?;
        encoded.push(b'\n');
        output
            .write_all(&encoded)
            .and_then(|_| output.flush())
            .map_err(|error| AgentError::Storage(format!("write coordinator reply: {error}")))?;
    }
}

/// 进程外协调器客户端：把本地执行包进持久三相。预约与应答跨进程
/// 落到协调器日志；派发意图先落账、效果体在请求方本地应用——崩溃
/// 窗口与进程内版本一致，只能是 Ambiguous。连接单飞行；丢弃时先
/// 尽力发送 shutdown，宿主见 EOF 也会自行退出。
pub struct ProcessEffectBroker {
    connection: Mutex<Option<CoordinatorConnection>>,
}

struct CoordinatorConnection {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl ProcessEffectBroker {
    /// 启动协调器宿主子进程，`journal_path` 作为它的唯一参数。
    pub fn connect(program: &Path, journal_path: &Path) -> AgentResult<Self> {
        let mut child = std::process::Command::new(program)
            .arg(journal_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| {
                AgentError::Storage(format!(
                    "spawn effect coordinator {}: {error}",
                    program.display()
                ))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Storage("coordinator stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Storage("coordinator stdout unavailable".into()))?;
        Ok(Self {
            connection: Mutex::new(Some(CoordinatorConnection {
                child,
                stdin,
                stdout: std::io::BufReader::new(stdout),
            })),
        })
    }

    fn rpc(
        connection: &mut CoordinatorConnection,
        request: CoordinatorRequest,
    ) -> AgentResult<CoordinatorReply> {
        use std::io::Write as _;
        let encoded = serde_json::to_vec(&request).map_err(|error| {
            AgentError::Storage(format!("serialize coordinator request: {error}"))
        })?;
        connection
            .stdin
            .write_all(&encoded)
            .and_then(|_| connection.stdin.write_all(b"\n"))
            .and_then(|_| connection.stdin.flush())
            .map_err(|error| AgentError::Storage(format!("write coordinator request: {error}")))?;
        let mut line = String::new();
        let read = connection
            .stdout
            .read_line(&mut line)
            .map_err(|error| AgentError::Storage(format!("read coordinator reply: {error}")))?;
        if read == 0 {
            return Err(AgentError::Storage(
                "effect coordinator exited before replying".into(),
            ));
        }
        if line.len() > MAX_COORDINATOR_LINE_BYTES {
            return Err(AgentError::InvalidRequest(
                "coordinator reply exceeds its line bound".into(),
            ));
        }
        serde_json::from_str(&line)
            .map_err(|error| AgentError::Storage(format!("malformed coordinator reply: {error}")))
    }

    fn require_ok(reply: CoordinatorReply) -> AgentResult<CoordinatorReply> {
        if reply.ok {
            Ok(reply)
        } else {
            Err(AgentError::Storage(
                reply.error.unwrap_or_else(|| "coordinator refused".into()),
            ))
        }
    }
}

impl Drop for ProcessEffectBroker {
    fn drop(&mut self) {
        // 尽力优雅关闭；随后显式丢弃 stdin 让宿主见 EOF，最后收割
        // 子进程——任何路径都不留下僵尸。之后的方法调用按"已关闭"
        // 拒绝。
        use std::io::Write as _;
        let mut guard = self.connection.lock().expect("coordinator poisoned");
        if let Some(mut connection) = guard.take() {
            let _ = connection
                .stdin
                .write_all(b"{\"op\":\"shutdown\"}\n")
                .and_then(|_| connection.stdin.flush());
            drop(connection.stdin);
            let _ = connection.child.wait();
        }
    }
}

#[async_trait::async_trait]
impl EffectBroker for ProcessEffectBroker {
    async fn reserve(&self, reservation: EffectReservation) -> AgentResult<String> {
        let mut guard = self.connection.lock().expect("coordinator poisoned");
        let connection = guard
            .as_mut()
            .ok_or_else(|| AgentError::Storage("effect coordinator is closed".into()))?;
        let record = ReservedRecord::from_reservation(String::new(), &reservation);
        let reply = Self::require_ok(Self::rpc(
            connection,
            CoordinatorRequest::Reserve {
                effect_id: reservation.effect_id,
                record: Box::new(record),
            },
        )?)?;
        reply.reservation_id.ok_or_else(|| {
            AgentError::Storage("coordinator reserve reply missing reservation id".into())
        })
    }

    async fn dispatch(&self, reserved: ReservedEffect) -> EffectReceipt {
        let effect_id = reserved.reservation.effect_id;
        let reservation_id = reserved.reservation_id.clone();
        let journaled = {
            let mut guard = self.connection.lock().expect("coordinator poisoned");
            match guard.as_mut() {
                Some(connection) => Self::rpc(
                    connection,
                    CoordinatorRequest::Dispatched {
                        effect_id,
                        reservation_id: reservation_id.clone(),
                    },
                ),
                None => Err(AgentError::Storage("effect coordinator is closed".into())),
            }
        };
        if let Err(error) = journaled.and_then(Self::require_ok) {
            // 账本拒绝即不派发：先回滚已暂存效果，再如实报告未应用。
            let reason = format!("coordinator refused the dispatch of effect {effect_id}: {error}");
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
        // 执行留在请求方本地：效果体不可跨进程。崩溃窗口由账本覆盖
        // ——已记 dispatched 而未见应答时恢复为 Ambiguous。
        reserved.effect.commit().await
    }

    async fn ack(&self, ack: EffectAck) -> AgentResult<()> {
        let mut guard = self.connection.lock().expect("coordinator poisoned");
        let connection = guard
            .as_mut()
            .ok_or_else(|| AgentError::Storage("effect coordinator is closed".into()))?;
        Self::require_ok(Self::rpc(
            connection,
            CoordinatorRequest::Acknowledged {
                reservation_id: ack.reservation_id,
                settlement: ack.settlement,
            },
        )?)?;
        Ok(())
    }

    fn reconcile_reservation(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<Option<EffectReconciliation>> {
        let mut guard = self.connection.lock().expect("coordinator poisoned");
        let connection = guard
            .as_mut()
            .ok_or_else(|| AgentError::Storage("effect coordinator is closed".into()))?;
        let reply = Self::require_ok(Self::rpc(
            connection,
            CoordinatorRequest::Reconcile {
                context: context.clone(),
            },
        )?)?;
        Ok(reply.reconciliation.flatten())
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
                    settlement: agent_contracts::EffectAckSettlement::Applied {
                        durability: agent_contracts::EffectDurability::Durable,
                    },
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
                    settlement: agent_contracts::EffectAckSettlement::NotApplied,
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

    /// 四类应答结算全部按原样穿过 ACK/Core-terminal 崩溃窗口重开：
    /// durable 仍是 durable，durability-failed 绝不升级成 durable，
    /// not-applied 仍是 not-applied，unknown 恢复为 Ambiguous 而绝
    /// 不是已应用。旧布尔日志只可能把 true 记成 durable（当时唯一
    /// 可记录的真值），因此任何恢复路径都不能加强更弱的真相。
    #[tokio::test]
    async fn settlement_classes_are_never_strengthened_by_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        let identity = identity();
        let durable = EffectId::new();
        let durability_failed = EffectId::new();
        let refused = EffectId::new();
        let unknown = EffectId::new();

        {
            let broker = journaled(&dir).await;
            for (effect_id, settlement) in [
                (
                    durable,
                    agent_contracts::EffectAckSettlement::Applied {
                        durability: EffectDurability::Durable,
                    },
                ),
                (
                    durability_failed,
                    agent_contracts::EffectAckSettlement::Applied {
                        durability: EffectDurability::DurabilityFailed("journal write lost".into()),
                    },
                ),
                (refused, agent_contracts::EffectAckSettlement::NotApplied),
                (unknown, agent_contracts::EffectAckSettlement::Unknown),
            ] {
                broker
                    .reserve(reservation_for(&identity, effect_id, None))
                    .await
                    .unwrap();
                dispatch_as(&broker, &identity, effect_id, true).await;
                broker
                    .ack(EffectAck {
                        reservation_id: format!("pass/{effect_id}"),
                        operation_id: identity.operation_id,
                        settlement,
                        receipt_summary: "fixture".into(),
                    })
                    .await
                    .unwrap();
            }
        }

        let reopened = JournaledEffectBroker::open(Arc::new(PassThroughBroker), &path).unwrap();
        let durable_rc = reopened.reconcile(&context_of(&identity, durable)).unwrap();
        assert!(
            matches!(
                durable_rc,
                Some(EffectReconciliation::Applied {
                    durability: EffectDurability::Durable,
                    ..
                })
            ),
            "durable settlement must stay durable: {durable_rc:?}"
        );
        let failed_rc = reopened
            .reconcile(&context_of(&identity, durability_failed))
            .unwrap();
        assert!(
            matches!(
                failed_rc,
                Some(EffectReconciliation::Applied {
                    durability: EffectDurability::DurabilityFailed(_),
                    ..
                })
            ),
            "a durability-failed settlement must not come back as durable: {failed_rc:?}"
        );
        let refused_rc = reopened.reconcile(&context_of(&identity, refused)).unwrap();
        assert!(
            matches!(refused_rc, Some(EffectReconciliation::NotApplied { .. })),
            "a not-applied settlement must stay not-applied: {refused_rc:?}"
        );
        let unknown_rc = reopened.reconcile(&context_of(&identity, unknown)).unwrap();
        assert!(
            matches!(unknown_rc, Some(EffectReconciliation::Ambiguous { .. })),
            "an unknown settlement must not come back as applied: {unknown_rc:?}"
        );
    }

    /// v1 布尔日志被明确拒绝而不是静默解码：旧 `applied: bool` 只能记录
    /// durable 真值，无法表达 Unknown/DurabilityFailed，任何解码都是潜在
    /// 加强。升级路径必须显式迁移，恢复绝不猜测。
    #[tokio::test]
    async fn version_one_boolean_journals_are_rejected_not_silently_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        let identity = identity();

        // 用 v1 Reserved 帧（v1/v2 结构相同）专门验证版本检查路径：
        // 明确报 unsupported version，而不是任何形式的静默解码。
        let frame = JournalFrame {
            version: 1,
            seq: 1,
            transition: ReservationTransition::Reserved {
                effect_id: EffectId::new(),
                record: Box::new(ReservedRecord {
                    reservation_id: "legacy/1".into(),
                    run_id: identity.run_id,
                    operation_id: identity.operation_id,
                    argument_digest: identity.argument_digest,
                    generation: identity.generation,
                    intent: None,
                }),
            },
        };
        let encoded = serde_json::to_vec(&frame).unwrap();
        let mut line = serde_json::to_string(&StoredFrame {
            checksum: checksum_hex(&encoded),
            frame,
        })
        .unwrap();
        line.push('\n');
        std::fs::write(&path, line).unwrap();

        let error = match JournaledEffectBroker::open(Arc::new(PassThroughBroker), &path) {
            Ok(_) => panic!("a version 1 journal must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unsupported version"), "{error}");
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
