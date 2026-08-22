//! Crash-recoverable evidence for Core-issued workspace effects.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use agent_contracts::{
    AgentError, AgentResult, EffectDurability, EffectReconciler, EffectReconciliation,
    OperationEffectContext,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::{ConfinedDir, MAX_MUTATION_BYTES, Workspace, clean_relative};

const LEGACY_JOURNAL_VERSION: u32 = 1;
const JOURNAL_VERSION: u32 = 2;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const MAX_EFFECT_TRANSACTIONS: usize = 16;
const MAX_REASON_BYTES: usize = 4_000;
/// One reconciliation can inspect at most one target and one staged file for
/// every transaction owned by an effect. Keep the aggregate explicit so a
/// corrupt or adversarial journal cannot turn recovery into unbounded I/O.
const MAX_RECONCILIATION_READ_BYTES: u64 =
    MAX_EFFECT_TRANSACTIONS as u64 * 2 * (MAX_MUTATION_BYTES as u64 + 1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceEffectRecovery {
    NotManaged,
    NotApplied { tx_ids: Vec<String> },
    Applied { tx_ids: Vec<String>, complete: bool },
    Ambiguous { tx_ids: Vec<String>, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalTransition {
    Prepared {
        tx_id: String,
        context: Box<OperationEffectContext>,
        relative_target: String,
        temp_name: String,
        target_existed: bool,
        before_hash: String,
        after_hash: String,
        /// Added in v2. Optional in the serde shape so a real v1 frame
        /// re-serializes byte-for-byte for its stored checksum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes_before: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes_after: Option<u64>,
    },
    Committed {
        tx_id: String,
    },
    RolledBack {
        tx_id: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxTerminal {
    Committed,
    RolledBack,
}

#[derive(Debug, Clone)]
struct TxEvidence {
    journal_version: u32,
    context: OperationEffectContext,
    relative_target: String,
    temp_name: String,
    target_existed: bool,
    before_hash: String,
    after_hash: String,
    bytes_before: Option<u64>,
    bytes_after: Option<u64>,
    terminal: Option<TxTerminal>,
}

pub(crate) struct PreparedEvidence {
    pub tx_id: String,
    pub context: OperationEffectContext,
    pub relative_target: String,
    pub temp_name: String,
    pub target_existed: bool,
    pub before_hash: String,
    pub after_hash: String,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Debug, Default, Clone)]
struct RecoveryState {
    last_seq: u64,
    transactions: HashMap<String, TxEvidence>,
}

struct WriterState {
    file: File,
    recovery: RecoveryState,
    failed: Option<String>,
}

pub(crate) struct WorkspaceEffectJournal {
    _authority_dir: ConfinedDir,
    path: PathBuf,
    writer: Mutex<WriterState>,
}

impl std::fmt::Debug for WorkspaceEffectJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceEffectJournal")
            .field("path", &self.path)
            .finish()
    }
}

impl WorkspaceEffectJournal {
    pub(crate) fn open(authority_dir: ConfinedDir) -> AgentResult<Self> {
        let path = authority_dir.display().join("workspace-effects.jsonl");
        let existed = authority_dir
            .open_existing(OsStr::new("workspace-effects.jsonl"))
            .is_ok();
        let mut file = authority_dir
            .open_or_create_regular_file(OsStr::new("workspace-effects.jsonl"))
            .map_err(|error| {
                AgentError::Storage(format!(
                    "open workspace effect journal {}: {error}",
                    path.display()
                ))
            })?;
        file.try_lock().map_err(|error| {
            AgentError::Storage(format!(
                "lock workspace effect journal {} exclusively: {error}",
                path.display()
            ))
        })?;
        if !existed {
            file.sync_all().map_err(|error| {
                AgentError::Storage(format!(
                    "sync new workspace effect journal {}: {error}",
                    path.display()
                ))
            })?;
            authority_dir.sync_all().map_err(|error| {
                AgentError::Storage(format!(
                    "sync workspace effect journal directory {}: {error}",
                    authority_dir.display().display()
                ))
            })?;
        }
        let recovery = recover_file(&mut file, &path)?;
        file.seek(SeekFrom::End(0)).map_err(|error| {
            AgentError::Storage(format!(
                "seek workspace effect journal {}: {error}",
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

    pub(crate) fn append_prepared(&self, evidence: PreparedEvidence) -> AgentResult<()> {
        self.append(JournalTransition::Prepared {
            tx_id: evidence.tx_id,
            context: Box::new(evidence.context),
            relative_target: evidence.relative_target,
            temp_name: evidence.temp_name,
            target_existed: evidence.target_existed,
            before_hash: evidence.before_hash,
            after_hash: evidence.after_hash,
            bytes_before: Some(evidence.bytes_before),
            bytes_after: Some(evidence.bytes_after),
        })
    }

    pub(crate) fn append_committed(&self, tx_id: &str) -> AgentResult<()> {
        self.append(JournalTransition::Committed {
            tx_id: tx_id.to_string(),
        })
    }

    pub(crate) fn append_rolled_back(&self, tx_id: &str) -> AgentResult<()> {
        self.append(JournalTransition::RolledBack {
            tx_id: tx_id.to_string(),
        })
    }

    fn append(&self, transition: JournalTransition) -> AgentResult<()> {
        validate_transition(JOURNAL_VERSION, &transition).map_err(AgentError::InvalidRequest)?;
        let mut writer = self.writer.lock().expect("workspace journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        if writer.recovery.last_seq >= MAX_RECORDS as u64 {
            return Err(AgentError::RecoveryRequired(format!(
                "workspace effect journal reached its {MAX_RECORDS} record recovery limit"
            )));
        }
        validate_fold(&writer.recovery, &transition).map_err(AgentError::RecoveryRequired)?;
        let seq = writer.recovery.last_seq.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("workspace effect journal sequence exhausted".into())
        })?;
        let record = JournalRecord {
            version: JOURNAL_VERSION,
            seq,
            transition,
        };
        if let Err(error) = append_record(&mut writer.file, &record) {
            let message = format!(
                "workspace effect journal {} failed permanently: {error}",
                self.path.display()
            );
            writer.failed = Some(message.clone());
            return Err(AgentError::Storage(message));
        }
        apply_transition(&mut writer.recovery, record.version, &record.transition)
            .expect("validated transition");
        writer.recovery.last_seq = seq;
        Ok(())
    }

    fn evidence_for(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<Vec<(String, TxEvidence)>> {
        context.validate().map_err(AgentError::InvalidRequest)?;
        let writer = self.writer.lock().expect("workspace journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        let mut result: Vec<_> = writer
            .recovery
            .transactions
            .iter()
            .filter(|(_, evidence)| &evidence.context == context)
            .map(|(tx_id, evidence)| (tx_id.clone(), evidence.clone()))
            .collect();
        result.sort_by(|left, right| left.0.cmp(&right.0));
        if result.len() > MAX_EFFECT_TRANSACTIONS {
            return Err(AgentError::RecoveryRequired(format!(
                "effect {} owns more than {MAX_EFFECT_TRANSACTIONS} workspace transactions",
                context.effect_id
            )));
        }
        Ok(result)
    }
}

fn validate_transition(version: u32, transition: &JournalTransition) -> Result<(), String> {
    if version != LEGACY_JOURNAL_VERSION && version != JOURNAL_VERSION {
        return Err("unsupported workspace effect journal version".into());
    }
    match transition {
        JournalTransition::Prepared {
            tx_id,
            context,
            relative_target,
            temp_name,
            before_hash,
            after_hash,
            bytes_before,
            bytes_after,
            ..
        } => {
            context.validate()?;
            if Uuid::parse_str(tx_id).is_err()
                || temp_name.len() > 255
                || !temp_name.ends_with(".tmp")
                || Path::new(temp_name).components().count() != 1
            {
                return Err("invalid workspace effect transaction/temp identity".into());
            }
            let clean =
                clean_relative(Path::new(relative_target)).map_err(|error| error.to_string())?;
            if clean.as_os_str().is_empty()
                || clean != Path::new(relative_target)
                || relative_target.len() > 4_000
            {
                return Err("workspace effect path/hash exceeds its bound".into());
            }
            match version {
                LEGACY_JOURNAL_VERSION => {
                    if bytes_before.is_some()
                        || bytes_after.is_some()
                        || !valid_hex_digest(before_hash, 16)
                        || !valid_hex_digest(after_hash, 16)
                    {
                        return Err("invalid legacy workspace effect hash/length".into());
                    }
                }
                JOURNAL_VERSION => {
                    let (Some(bytes_before), Some(bytes_after)) = (*bytes_before, *bytes_after)
                    else {
                        return Err("workspace effect v2 requires byte lengths".into());
                    };
                    if temp_name != &format!(".fa-{tx_id}.tmp")
                        || bytes_before > MAX_MUTATION_BYTES as u64
                        || bytes_after > MAX_MUTATION_BYTES as u64
                        || !valid_hex_digest(before_hash, 64)
                        || !valid_hex_digest(after_hash, 64)
                    {
                        return Err("invalid workspace effect v2 digest/length".into());
                    }
                }
                _ => unreachable!("journal version checked above"),
            }
            Ok(())
        }
        JournalTransition::Committed { tx_id } | JournalTransition::RolledBack { tx_id } => {
            if Uuid::parse_str(tx_id).is_ok() {
                Ok(())
            } else {
                Err("invalid workspace transaction id".into())
            }
        }
    }
}

fn validate_fold(state: &RecoveryState, transition: &JournalTransition) -> Result<(), String> {
    match transition {
        JournalTransition::Prepared { tx_id, context, .. } => {
            if state.transactions.contains_key(tx_id) {
                return Err(format!("duplicate workspace prepared transaction {tx_id}"));
            }
            let count = state
                .transactions
                .values()
                .filter(|tx| tx.context == **context)
                .count();
            if count >= MAX_EFFECT_TRANSACTIONS {
                return Err(format!(
                    "effect {} exceeds {MAX_EFFECT_TRANSACTIONS} transactions",
                    context.effect_id
                ));
            }
            // One operation id can never bind multiple exact identities or effects.
            if state.transactions.values().any(|tx| {
                (tx.context.identity.operation_id == context.identity.operation_id
                    || tx.context.effect_id == context.effect_id)
                    && tx.context != **context
            }) {
                return Err(format!(
                    "operation {} has conflicting workspace effect identity",
                    context.identity.operation_id
                ));
            }
            Ok(())
        }
        JournalTransition::Committed { tx_id } | JournalTransition::RolledBack { tx_id } => {
            let evidence = state.transactions.get(tx_id).ok_or_else(|| {
                format!("terminal workspace transaction {tx_id} has no prepared record")
            })?;
            if evidence.terminal.is_some() {
                return Err(format!(
                    "workspace transaction {tx_id} has duplicate/conflicting terminal state"
                ));
            }
            Ok(())
        }
    }
}

fn valid_hex_digest(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn apply_transition(
    state: &mut RecoveryState,
    journal_version: u32,
    transition: &JournalTransition,
) -> Result<(), String> {
    match transition {
        JournalTransition::Prepared {
            tx_id,
            context,
            relative_target,
            temp_name,
            target_existed,
            before_hash,
            after_hash,
            bytes_before,
            bytes_after,
        } => {
            state.transactions.insert(
                tx_id.clone(),
                TxEvidence {
                    journal_version,
                    context: context.as_ref().clone(),
                    relative_target: relative_target.clone(),
                    temp_name: temp_name.clone(),
                    target_existed: *target_existed,
                    before_hash: before_hash.clone(),
                    after_hash: after_hash.clone(),
                    bytes_before: *bytes_before,
                    bytes_after: *bytes_after,
                    terminal: None,
                },
            );
        }
        JournalTransition::Committed { tx_id } => {
            state
                .transactions
                .get_mut(tx_id)
                .ok_or_else(|| "missing prepared transaction".to_string())?
                .terminal = Some(TxTerminal::Committed)
        }
        JournalTransition::RolledBack { tx_id } => {
            state
                .transactions
                .get_mut(tx_id)
                .ok_or_else(|| "missing prepared transaction".to_string())?
                .terminal = Some(TxTerminal::RolledBack)
        }
    }
    Ok(())
}

fn append_record(file: &mut File, record: &JournalRecord) -> AgentResult<()> {
    let payload = serde_json::to_vec(record).map_err(|error| {
        AgentError::Storage(format!("serialize workspace journal record: {error}"))
    })?;
    let frame = StoredFrame {
        checksum: checksum_hex(&payload),
        record: record.clone(),
    };
    let encoded = serde_json::to_vec(&frame).map_err(|error| {
        AgentError::Storage(format!("serialize workspace journal frame: {error}"))
    })?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(AgentError::Storage(format!(
            "workspace effect journal frame exceeds {MAX_FRAME_BYTES} bytes"
        )));
    }
    let projected = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat workspace effect journal: {error}")))?
        .len()
        .checked_add(encoded.len() as u64 + 1)
        .ok_or_else(|| AgentError::Storage("workspace effect journal size overflow".into()))?;
    if projected > MAX_FILE_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "workspace effect journal reached its {MAX_FILE_BYTES} byte hard limit"
        )));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(&encoded))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| AgentError::Storage(format!("persist workspace effect journal: {error}")))
}

fn recover_file(file: &mut File, path: &Path) -> AgentResult<RecoveryState> {
    let size = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat workspace effect journal: {error}")))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(corrupt(path, "file exceeds hard limit"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| AgentError::Storage(format!("seek workspace effect journal: {error}")))?;
    let mut reader = BufReader::new(file.try_clone().map_err(|error| {
        AgentError::Storage(format!("clone workspace effect journal: {error}"))
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
            .map_err(|error| {
                AgentError::Storage(format!("read workspace effect journal: {error}"))
            })?;
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
            return Err(corrupt(path, "oversized complete frame"));
        }
        bytes.pop();
        let frame: StoredFrame = serde_json::from_slice(&bytes)
            .map_err(|error| corrupt(path, &format!("invalid JSON: {error}")))?;
        let payload = serde_json::to_vec(&frame.record)
            .map_err(|error| corrupt(path, &format!("record serialization failed: {error}")))?;
        if frame.checksum != checksum_hex(&payload) {
            return Err(corrupt(path, "checksum mismatch"));
        }
        if frame.record.version != LEGACY_JOURNAL_VERSION && frame.record.version != JOURNAL_VERSION
        {
            return Err(corrupt(path, "unsupported version"));
        }
        if frame.record.seq != state.last_seq + 1 {
            return Err(corrupt(path, "non-contiguous sequence"));
        }
        validate_transition(frame.record.version, &frame.record.transition)
            .map_err(|error| corrupt(path, &error))?;
        validate_fold(&state, &frame.record.transition).map_err(|error| corrupt(path, &error))?;
        apply_transition(&mut state, frame.record.version, &frame.record.transition)
            .map_err(|error| corrupt(path, &error))?;
        state.last_seq = frame.record.seq;
        if state.last_seq > MAX_RECORDS as u64 {
            return Err(corrupt(path, "record recovery limit exceeded"));
        }
        last_good = offset;
    }
    drop(reader);
    if torn_tail {
        file.set_len(last_good)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                AgentError::Storage(format!("repair workspace effect journal tail: {error}"))
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
        "workspace effect journal {} is corrupt: {detail}",
        path.display()
    ))
}

enum CurrentTarget {
    Missing,
    File { bytes: u64, hash: String },
}

#[derive(Debug, Clone, Copy)]
enum RecoveryHash {
    LegacyFnv64,
    Sha256,
}

#[derive(Debug)]
struct RecoveryReadBudget {
    remaining: u64,
}

impl RecoveryReadBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_RECONCILIATION_READ_BYTES,
        }
    }

    fn charge(&mut self, bytes: usize) -> AgentResult<()> {
        self.remaining = self.remaining.checked_sub(bytes as u64).ok_or_else(|| {
            AgentError::RecoveryRequired(format!(
                "workspace effect reconciliation exceeded its {MAX_RECONCILIATION_READ_BYTES}-byte read budget"
            ))
        })?;
        Ok(())
    }
}

fn recovery_hash_for(evidence: &TxEvidence) -> RecoveryHash {
    if evidence.journal_version == LEGACY_JOURNAL_VERSION {
        RecoveryHash::LegacyFnv64
    } else {
        RecoveryHash::Sha256
    }
}

/// Hash one recovery file through an already-confined handle. Metadata is an
/// early refusal only; the MAX+1 read is the authoritative growth bound.
fn hash_reader_bounded(
    mut reader: impl Read,
    metadata_len: u64,
    algorithm: RecoveryHash,
    budget: &mut RecoveryReadBudget,
) -> AgentResult<(u64, String)> {
    if metadata_len > MAX_MUTATION_BYTES as u64 {
        return Err(AgentError::RecoveryRequired(format!(
            "workspace recovery file is {metadata_len} bytes; the limit is {MAX_MUTATION_BYTES} bytes"
        )));
    }
    let mut legacy_hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut sha256 = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let remaining = (MAX_MUTATION_BYTES as u64 + 1).saturating_sub(total);
        if remaining == 0 {
            return Err(AgentError::RecoveryRequired(format!(
                "workspace recovery file grew beyond {MAX_MUTATION_BYTES} bytes"
            )));
        }
        if budget.remaining == 0 {
            return Err(AgentError::RecoveryRequired(format!(
                "workspace effect reconciliation exceeded its {MAX_RECONCILIATION_READ_BYTES}-byte read budget"
            )));
        }
        let read_len = remaining.min(buffer.len() as u64).min(budget.remaining) as usize;
        let read = reader
            .read(&mut buffer[..read_len])
            .map_err(|error| AgentError::Io(format!("hash reconciliation target: {error}")))?;
        if read == 0 {
            break;
        }
        budget.charge(read)?;
        total += read as u64;
        if total > MAX_MUTATION_BYTES as u64 {
            return Err(AgentError::RecoveryRequired(format!(
                "workspace recovery file grew beyond {MAX_MUTATION_BYTES} bytes"
            )));
        }
        match algorithm {
            RecoveryHash::LegacyFnv64 => {
                for byte in &buffer[..read] {
                    legacy_hash ^= u64::from(*byte);
                    legacy_hash = legacy_hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            RecoveryHash::Sha256 => sha256.update(&buffer[..read]),
        }
    }
    let hash = match algorithm {
        RecoveryHash::LegacyFnv64 => format!("{legacy_hash:016x}"),
        RecoveryHash::Sha256 => sha256
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    };
    Ok((total, hash))
}

fn inspect_target(
    workspace: &Workspace,
    evidence: &TxEvidence,
    budget: &mut RecoveryReadBudget,
) -> AgentResult<CurrentTarget> {
    let clean = clean_relative(Path::new(&evidence.relative_target))?;
    let parts: Vec<OsString> = clean
        .components()
        .map(|part| part.as_os_str().to_owned())
        .collect();
    let (name, parents) = parts.split_last().ok_or_else(|| {
        AgentError::RecoveryRequired("workspace evidence has empty target".into())
    })?;
    let mut parent = ConfinedDir::open_root(workspace.root()).map_err(|error| {
        AgentError::Io(format!("open workspace root for reconciliation: {error}"))
    })?;
    for part in parents {
        match parent.open_child_dir(part) {
            Ok(child) => parent = child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CurrentTarget::Missing);
            }
            Err(error) => {
                return Err(AgentError::RecoveryRequired(format!(
                    "open reconciliation parent: {error}"
                )));
            }
        }
    }
    match parent.open_recovery_target(name) {
        Ok(mut file) => {
            if !parent
                .named_entry_matches_file(name, &file)
                .map_err(|error| {
                    AgentError::RecoveryRequired(format!(
                        "bind reconciliation target to its name: {error}"
                    ))
                })?
            {
                return Err(AgentError::RecoveryRequired(
                    "workspace effect target name does not identify its opened file".into(),
                ));
            }
            let metadata = file.metadata().map_err(|error| {
                AgentError::Io(format!("inspect reconciliation target: {error}"))
            })?;
            if !metadata.is_file() {
                return Err(AgentError::RecoveryRequired(
                    "workspace effect target is not a regular file".into(),
                ));
            }
            let (bytes, hash) = hash_reader_bounded(
                &mut file,
                metadata.len(),
                recovery_hash_for(evidence),
                budget,
            )?;
            if !parent
                .named_entry_matches_file(name, &file)
                .map_err(|error| {
                    AgentError::RecoveryRequired(format!(
                        "rebind reconciliation target after verification: {error}"
                    ))
                })?
            {
                return Err(AgentError::RecoveryRequired(
                    "workspace effect target changed while it was verified".into(),
                ));
            }
            Ok(CurrentTarget::File { bytes, hash })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CurrentTarget::Missing),
        Err(error) => Err(AgentError::RecoveryRequired(format!(
            "open reconciliation target: {error}"
        ))),
    }
}

fn pinned_parent(workspace: &Workspace, relative: &str) -> AgentResult<(ConfinedDir, OsString)> {
    let clean = clean_relative(Path::new(relative))?;
    let parts: Vec<OsString> = clean
        .components()
        .map(|part| part.as_os_str().to_owned())
        .collect();
    let (name, parents) = parts.split_last().ok_or_else(|| {
        AgentError::RecoveryRequired("workspace evidence has empty target".into())
    })?;
    let mut parent = ConfinedDir::open_root(workspace.root())
        .map_err(|error| AgentError::Io(format!("open workspace root for cleanup: {error}")))?;
    for part in parents {
        parent = parent.open_child_dir(part).map_err(|error| {
            AgentError::RecoveryRequired(format!("open cleanup parent: {error}"))
        })?;
    }
    Ok((parent, name.clone()))
}

fn recorded_state_matches(evidence: &TxEvidence, bytes: u64, hash: &str, after: bool) -> bool {
    let expected_hash = if after {
        &evidence.after_hash
    } else {
        &evidence.before_hash
    };
    if hash != expected_hash {
        return false;
    }
    if evidence.journal_version == LEGACY_JOURNAL_VERSION {
        return true;
    }
    let expected_bytes = if after {
        evidence.bytes_after
    } else {
        evidence.bytes_before
    };
    expected_bytes == Some(bytes)
}

/// Remove a staged file only after opening the exact confined entry and
/// proving it is a regular file containing the journaled after-state. A
/// planned-but-partially-written file or a colliding entry stays in place and
/// forces explicit recovery rather than risking deletion of unknown bytes.
fn remove_verified_staged_file(
    parent: &ConfinedDir,
    tx_id: &str,
    evidence: &TxEvidence,
    budget: &mut RecoveryReadBudget,
) -> Result<(), String> {
    let temp_name = OsStr::new(&evidence.temp_name);
    let mut file = match parent.open_staged_for_cleanup(temp_name) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not open staged transaction {tx_id}: {error}"
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect staged transaction {tx_id}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("staged transaction {tx_id} is not a regular file"));
    }
    if !parent
        .named_entry_matches_file(temp_name, &file)
        .map_err(|error| format!("could not bind staged transaction {tx_id}: {error}"))?
    {
        return Err(format!(
            "staged transaction {tx_id} name does not identify its opened file"
        ));
    }
    let (bytes, hash) = hash_reader_bounded(
        &mut file,
        metadata.len(),
        recovery_hash_for(evidence),
        budget,
    )
    .map_err(|error| format!("could not verify staged transaction {tx_id}: {error}"))?;
    if !recorded_state_matches(evidence, bytes, &hash, true) {
        return Err(format!(
            "staged transaction {tx_id} does not match its expected content"
        ));
    }
    if !parent
        .named_entry_matches_file(temp_name, &file)
        .map_err(|error| format!("could not rebind staged transaction {tx_id}: {error}"))?
    {
        return Err(format!(
            "staged transaction {tx_id} changed while it was verified"
        ));
    }
    parent
        .remove_open_file(&file, temp_name)
        .map_err(|error| format!("could not clean staged transaction {tx_id}: {error}"))?;
    parent
        .sync_all()
        .map_err(|error| format!("could not sync staged cleanup for {tx_id}: {error}"))
}

pub(crate) fn reconcile_workspace_effect(
    workspace: &Workspace,
    context: &OperationEffectContext,
) -> AgentResult<WorkspaceEffectRecovery> {
    let entries = workspace.effect_journal.evidence_for(context)?;
    if entries.is_empty() {
        return Ok(WorkspaceEffectRecovery::NotManaged);
    }
    let tx_ids: Vec<_> = entries.iter().map(|(id, _)| id.clone()).collect();
    let mut applied = 0usize;
    let mut durably_committed = 0usize;
    let mut not_applied = Vec::new();
    let mut read_budget = RecoveryReadBudget::new();
    for (tx_id, evidence) in &entries {
        let state = inspect_target(workspace, evidence, &mut read_budget)?;
        let is_after = matches!(
            &state,
            CurrentTarget::File { bytes, hash }
                if recorded_state_matches(evidence, *bytes, hash, true)
        ) && (evidence.target_existed || !matches!(state, CurrentTarget::Missing));
        let is_before = match &state {
            CurrentTarget::Missing => !evidence.target_existed,
            CurrentTarget::File { bytes, hash } => {
                evidence.target_existed && recorded_state_matches(evidence, *bytes, hash, false)
            }
        };
        match evidence.terminal {
            Some(TxTerminal::Committed) if is_after => {
                applied += 1;
                durably_committed += 1;
            }
            Some(TxTerminal::RolledBack) if is_before => {}
            Some(TxTerminal::Committed) => {
                return Ok(ambiguous(
                    tx_ids,
                    format!("committed transaction {tx_id} target does not match after hash"),
                ));
            }
            Some(TxTerminal::RolledBack) => {
                return Ok(ambiguous(
                    tx_ids,
                    format!("rolled-back transaction {tx_id} target does not match before state"),
                ));
            }
            None if is_after && !is_before => applied += 1,
            None if is_before => not_applied.push((tx_id.clone(), evidence.clone())),
            None if is_after && is_before => {
                // A no-op write is observationally equivalent; absence of a
                // committed record means recovery chooses the safe pre-commit truth.
                not_applied.push((tx_id.clone(), evidence.clone()));
            }
            None => {
                return Ok(ambiguous(
                    tx_ids,
                    format!(
                        "prepared transaction {tx_id} target matches neither before nor after state"
                    ),
                ));
            }
        }
    }
    // Prove and durably record cleanup for every transaction that did not
    // land. This is mandatory for NotApplied and prevents a partial
    // composite from leaking the still-staged remainder.
    for (tx_id, evidence) in not_applied {
        let (parent, _) = pinned_parent(workspace, &evidence.relative_target)?;
        if let Err(error) =
            remove_verified_staged_file(&parent, &tx_id, &evidence, &mut read_budget)
        {
            return Ok(ambiguous(tx_ids, error));
        }
        if let Err(error) = workspace.effect_journal.append_rolled_back(&tx_id) {
            return Ok(ambiguous(
                tx_ids,
                format!("could not persist staged cleanup for {tx_id}: {error}"),
            ));
        }
    }
    if applied > 0 {
        return Ok(WorkspaceEffectRecovery::Applied {
            tx_ids,
            complete: applied == entries.len() && durably_committed == entries.len(),
        });
    }
    Ok(WorkspaceEffectRecovery::NotApplied { tx_ids })
}

fn ambiguous(tx_ids: Vec<String>, mut reason: String) -> WorkspaceEffectRecovery {
    if reason.len() > MAX_REASON_BYTES {
        let mut boundary = MAX_REASON_BYTES;
        while !reason.is_char_boundary(boundary) {
            boundary -= 1;
        }
        reason.truncate(boundary);
    }
    WorkspaceEffectRecovery::Ambiguous { tx_ids, reason }
}

impl EffectReconciler for Workspace {
    fn reconcile(&self, context: &OperationEffectContext) -> AgentResult<EffectReconciliation> {
        let result = self.reconcile_effect(context)?;
        let evidence = |ids: Vec<String>| {
            let mut text = format!("workspace-tx:{}", ids.join(","));
            if text.len() > 512 {
                let mut boundary = 512;
                while !text.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text.truncate(boundary);
            }
            Some(text)
        };
        Ok(match result {
            WorkspaceEffectRecovery::NotManaged => match self.process_journal.reconcile(context)? {
                EffectReconciliation::NotManaged => self.remote_journal.reconcile(context)?,
                other => other,
            },
            WorkspaceEffectRecovery::NotApplied { tx_ids } => EffectReconciliation::NotApplied {
                evidence: evidence(tx_ids),
            },
            WorkspaceEffectRecovery::Applied {
                tx_ids,
                complete: true,
            } => EffectReconciliation::Applied {
                durability: EffectDurability::Durable,
                evidence: evidence(tx_ids),
            },
            WorkspaceEffectRecovery::Applied {
                tx_ids,
                complete: false,
            } => EffectReconciliation::Applied {
                durability: EffectDurability::DurabilityFailed(
                    "workspace composite effect was only partially applied".into(),
                ),
                evidence: evidence(tx_ids),
            },
            WorkspaceEffectRecovery::Ambiguous { reason, .. } => {
                EffectReconciliation::Ambiguous { reason }
            }
        })
    }

    fn recover_orphans(&self) -> AgentResult<()> {
        self.process_journal.recover_orphans()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ArgumentDigest, EffectId, EffectReceipt, OperationId, RunId, ToolOperationIdentity, TurnId,
    };
    use std::fs::OpenOptions;

    fn context() -> OperationEffectContext {
        OperationEffectContext {
            identity: ToolOperationIdentity {
                run_id: RunId::new(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id: OperationId::new(),
                generation: 1,
                call_id: "call-1".into(),
                tool_name: "fs.write".into(),
                argument_digest: ArgumentDigest::sha256_bytes(b"args"),
            },
            effect_id: EffectId::new(),
        }
    }

    async fn workspace() -> (Workspace, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).await.unwrap();
        (workspace, directory)
    }

    #[test]
    fn recovery_target_open_rejects_non_regular_entries() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("target-dir")).unwrap();
        let parent = ConfinedDir::open_root(directory.path()).unwrap();

        assert!(
            parent
                .open_recovery_target(OsStr::new("target-dir"))
                .is_err(),
            "recovery must never hash a directory through the target path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_recovery_target_binding_detects_name_substitution() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        let moved = directory.path().join("moved.txt");
        std::fs::write(&target, b"before").unwrap();
        let parent = ConfinedDir::open_root(directory.path()).unwrap();
        let file = parent
            .open_recovery_target(OsStr::new("target.txt"))
            .unwrap();
        assert!(
            parent
                .named_entry_matches_file(OsStr::new("target.txt"), &file)
                .unwrap()
        );

        std::fs::rename(&target, &moved).unwrap();
        std::fs::write(&target, b"substitute").unwrap();
        assert!(
            !parent
                .named_entry_matches_file(OsStr::new("target.txt"), &file)
                .unwrap(),
            "the recovery checks around hashing must reject a substituted directory entry"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_recovery_target_handle_denies_write_delete_and_rename() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        let moved = directory.path().join("moved.txt");
        std::fs::write(&target, b"before").unwrap();
        let parent = ConfinedDir::open_root(directory.path()).unwrap();
        let file = parent
            .open_recovery_target(OsStr::new("target.txt"))
            .unwrap();

        assert!(
            OpenOptions::new().write(true).open(&target).is_err(),
            "the recovery snapshot must exclude a concurrent writer"
        );
        assert!(
            std::fs::remove_file(&target).is_err(),
            "the recovery snapshot must exclude path deletion"
        );
        assert!(
            std::fs::rename(&target, &moved).is_err(),
            "the recovery snapshot must exclude path renaming"
        );
        drop(file);
        OpenOptions::new()
            .write(true)
            .open(&target)
            .expect("dropping the recovery handle releases the sharing fence");
    }

    #[tokio::test]
    async fn failed_authority_intent_creates_no_temp_or_review_record() {
        let (workspace, directory) = workspace().await;
        std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
        {
            let mut writer = workspace.effect_journal.writer.lock().unwrap();
            writer.failed = Some("injected effect journal failure".into());
        }

        let result = workspace
            .begin_mutation("edit.patch", "patch", "value.txt")
            .await
            .unwrap()
            .prepare_with_effect_context(b"new", context())
            .await;
        assert!(matches!(result, Err(AgentError::Storage(_))));
        assert_eq!(
            std::fs::read(directory.path().join("value.txt")).unwrap(),
            b"old"
        );

        assert!(
            !directory.path().join(".focus-agent/changes.jsonl").exists(),
            "review evidence is written only after durable authority and staging"
        );

        let leaked_temp = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(
            !leaked_temp,
            "an authority failure happens before temp creation"
        );
    }

    #[tokio::test]
    async fn prepared_reopens_cleans_temp_and_reports_not_applied() {
        let (workspace, directory) = workspace().await;
        std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
        let context = context();
        let prepared = workspace
            .begin_mutation("fs.write", "write", "value.txt")
            .await
            .unwrap()
            .prepare_with_effect_context(b"new", context.clone())
            .await
            .unwrap();
        let temp = prepared.temp_name.as_ref().unwrap().clone();
        let tx_id = prepared.tx_id.clone();
        prepared.simulate_process_exit();
        drop(workspace);

        let authority = std::fs::read_to_string(
            directory
                .path()
                .join(".focus-agent/authority/workspace-effects.jsonl"),
        )
        .unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(authority.lines().next().unwrap()).unwrap();
        assert_eq!(frame["record"]["version"], JOURNAL_VERSION);
        let transition = &frame["record"]["transition"];
        assert_eq!(transition["temp_name"], format!(".fa-{tx_id}.tmp"));
        assert_eq!(transition["bytes_before"], 3);
        assert_eq!(transition["bytes_after"], 3);
        assert_eq!(transition["before_hash"].as_str().unwrap().len(), 64);
        assert_eq!(transition["after_hash"].as_str().unwrap().len(), 64);

        let reopened = Workspace::open(directory.path()).await.unwrap();
        let recovered = reopened.reconcile_effect(&context).unwrap();
        assert!(
            matches!(
                &recovered,
                WorkspaceEffectRecovery::NotApplied { tx_ids } if tx_ids.len() == 1
            ),
            "unexpected recovery: {recovered:?}"
        );
        let parent = ConfinedDir::open_root(directory.path()).unwrap();
        assert!(matches!(
            parent.open_existing(&temp),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
        drop(reopened);

        let reopened_again = Workspace::open(directory.path()).await.unwrap();
        assert!(matches!(
            reopened_again.reconcile_effect(&context).unwrap(),
            WorkspaceEffectRecovery::NotApplied { .. }
        ));
    }

    #[tokio::test]
    async fn committed_reopens_and_reports_durable_applied() {
        let (workspace, directory) = workspace().await;
        let context = context();
        let receipt = workspace
            .begin_mutation("fs.write", "write", "created.txt")
            .await
            .unwrap()
            .prepare_with_effect_context(b"new", context.clone())
            .await
            .unwrap()
            .commit()
            .await;
        assert!(matches!(
            receipt,
            EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                ..
            }
        ));
        drop(workspace);

        let reopened = Workspace::open(directory.path()).await.unwrap();
        assert!(matches!(
            reopened.reconcile_effect(&context).unwrap(),
            WorkspaceEffectRecovery::Applied { complete: true, .. }
        ));
    }

    #[tokio::test]
    async fn partial_composite_is_applied_but_incomplete_and_cleans_remainder() {
        let (workspace, directory) = workspace().await;
        let context = context();
        let first = workspace
            .begin_mutation("fs.write", "write", "one.txt")
            .await
            .unwrap()
            .prepare_with_effect_context(b"one", context.clone())
            .await
            .unwrap();
        let second = workspace
            .begin_mutation("fs.write", "write", "two.txt")
            .await
            .unwrap()
            .prepare_with_effect_context(b"two", context.clone())
            .await
            .unwrap();
        assert!(matches!(
            first.commit().await,
            EffectReceipt::Applied { .. }
        ));
        let second_temp = second.temp_name.as_ref().unwrap().clone();
        second.simulate_process_exit();

        assert!(matches!(
            workspace.reconcile_effect(&context).unwrap(),
            WorkspaceEffectRecovery::Applied { complete: false, ref tx_ids } if tx_ids.len() == 2
        ));
        let parent = ConfinedDir::open_root(directory.path()).unwrap();
        assert!(
            matches!(parent.open_existing(&second_temp), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[tokio::test]
    async fn a_third_target_hash_is_ambiguous() {
        let (workspace, directory) = workspace().await;
        std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
        let context = context();
        let prepared = workspace
            .begin_mutation("fs.write", "write", "value.txt")
            .await
            .unwrap()
            .prepare_with_effect_context(b"new", context.clone())
            .await
            .unwrap();
        prepared.simulate_process_exit();
        std::fs::write(directory.path().join("value.txt"), b"third").unwrap();
        assert!(matches!(
            workspace.reconcile_effect(&context).unwrap(),
            WorkspaceEffectRecovery::Ambiguous { .. }
        ));
    }

    #[tokio::test]
    async fn empty_create_and_empty_overwrite_do_not_alias() {
        let (workspace, directory) = workspace().await;
        let created = context();
        let create = workspace
            .begin_mutation("fs.write", "write", "new-empty")
            .await
            .unwrap()
            .prepare_with_effect_context(b"", created.clone())
            .await
            .unwrap();
        create.simulate_process_exit();
        assert!(matches!(
            workspace.reconcile_effect(&created).unwrap(),
            WorkspaceEffectRecovery::NotApplied { .. }
        ));

        std::fs::write(directory.path().join("old-empty"), b"").unwrap();
        let overwritten = context();
        let overwrite = workspace
            .begin_mutation("fs.write", "write", "old-empty")
            .await
            .unwrap()
            .prepare_with_effect_context(b"", overwritten.clone())
            .await
            .unwrap();
        overwrite.simulate_process_exit();
        assert!(matches!(
            workspace.reconcile_effect(&overwritten).unwrap(),
            WorkspaceEffectRecovery::NotApplied { .. }
        ));

        let committed = context();
        let receipt = workspace
            .begin_mutation("fs.write", "write", "committed-empty")
            .await
            .unwrap()
            .prepare_with_effect_context(b"", committed.clone())
            .await
            .unwrap()
            .commit()
            .await;
        assert!(matches!(receipt, EffectReceipt::Applied { .. }));
        assert!(matches!(
            workspace.reconcile_effect(&committed).unwrap(),
            WorkspaceEffectRecovery::Applied { complete: true, .. }
        ));
    }

    #[tokio::test]
    async fn second_writer_is_refused() {
        let (workspace, directory) = workspace().await;
        let error = Workspace::open(directory.path()).await.unwrap_err();
        assert!(matches!(error, AgentError::Storage(_)));
        drop(workspace);
        Workspace::open(directory.path()).await.unwrap();
    }

    #[tokio::test]
    async fn torn_final_frame_is_repaired_but_complete_corruption_fails_closed() {
        let (workspace, directory) = workspace().await;
        let context = context();
        let prepared = workspace
            .begin_mutation("fs.write", "write", "value.txt")
            .await
            .unwrap()
            .prepare_with_effect_context(b"new", context)
            .await
            .unwrap();
        prepared.simulate_process_exit();
        drop(workspace);
        let path = directory
            .path()
            .join(".focus-agent/authority/workspace-effects.jsonl");
        let good_len = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"record":{"version":1"#)
            .unwrap();
        let reopened = Workspace::open(directory.path()).await.unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        drop(reopened);

        let mut bytes = std::fs::read(&path).unwrap();
        let checksum = bytes
            .windows(b"checksum".len())
            .position(|window| window == b"checksum")
            .unwrap();
        bytes[checksum + b"checksum".len() + 3] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            Workspace::open(directory.path()).await,
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[tokio::test]
    async fn exact_identity_conflicts_and_transaction_cap_fail_before_append() {
        let (workspace, _directory) = workspace().await;
        let first = context();
        let mut conflicting = first.clone();
        conflicting.effect_id = EffectId::new();
        let one = workspace
            .begin_mutation("fs.write", "write", "one")
            .await
            .unwrap()
            .prepare_with_effect_context(b"one", first.clone())
            .await
            .unwrap();
        one.simulate_process_exit();
        let conflict = workspace
            .begin_mutation("fs.write", "write", "conflict")
            .await
            .unwrap()
            .prepare_with_effect_context(b"conflict", conflicting)
            .await;
        assert!(matches!(conflict, Err(AgentError::RecoveryRequired(_))));

        for index in 1..MAX_EFFECT_TRANSACTIONS {
            let prepared = workspace
                .begin_mutation("fs.write", "write", format!("item-{index}"))
                .await
                .unwrap()
                .prepare_with_effect_context(b"x", first.clone())
                .await
                .unwrap();
            prepared.simulate_process_exit();
        }
        let overflow = workspace
            .begin_mutation("fs.write", "write", "overflow")
            .await
            .unwrap()
            .prepare_with_effect_context(b"x", first)
            .await;
        assert!(matches!(overflow, Err(AgentError::RecoveryRequired(_))));
    }

    #[tokio::test]
    async fn core_prepare_crash_points_remain_mapped_and_recover_not_applied() {
        for point in [
            crate::PrepareCrashPoint::IntentPersisted,
            crate::PrepareCrashPoint::StageSynced,
            crate::PrepareCrashPoint::ReviewRecorded,
        ] {
            let (workspace, directory) = workspace().await;
            std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
            let context = context();
            let transaction = workspace
                .begin_mutation("edit.patch", "patch", "value.txt")
                .await
                .unwrap();
            let temp_name = format!(".fa-{}.tmp", transaction.tx_id);
            let result = transaction
                .with_prepare_crash_point(point)
                .prepare_with_effect_context(b"new", context.clone())
                .await;
            assert!(matches!(result, Err(AgentError::RecoveryRequired(_))));
            assert_eq!(
                directory.path().join(&temp_name).exists(),
                point != crate::PrepareCrashPoint::IntentPersisted,
                "unexpected staged-file state at {point:?}"
            );
            drop(workspace);

            let reopened = Workspace::open(directory.path()).await.unwrap();
            let recovered = reopened.reconcile_effect(&context).unwrap();
            assert!(
                matches!(
                    &recovered,
                    WorkspaceEffectRecovery::NotApplied { tx_ids } if tx_ids.len() == 1
                ),
                "unexpected recovery at {point:?}: {recovered:?}"
            );
            assert!(
                !directory.path().join(&temp_name).exists(),
                "recovery must remove a fully verified stage at {point:?}"
            );
        }
    }

    #[tokio::test]
    async fn deterministic_temp_collision_is_never_deleted_by_prepare_failure() {
        let (workspace, directory) = workspace().await;
        std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
        let context = context();
        let transaction = workspace
            .begin_mutation("edit.patch", "patch", "value.txt")
            .await
            .unwrap();
        let temp_path = directory
            .path()
            .join(format!(".fa-{}.tmp", transaction.tx_id));
        std::fs::write(&temp_path, b"foreign").unwrap();

        let result = transaction
            .prepare_with_effect_context(b"new", context.clone())
            .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(&temp_path).unwrap(), b"foreign");
        assert!(matches!(
            workspace.reconcile_effect(&context).unwrap(),
            WorkspaceEffectRecovery::NotApplied { .. }
        ));
        assert_eq!(
            std::fs::read(&temp_path).unwrap(),
            b"foreign",
            "a create_new collision is not owned by this transaction"
        );
    }

    #[tokio::test]
    async fn partial_or_substituted_stage_is_ambiguous_and_is_not_deleted() {
        let (workspace, directory) = workspace().await;
        std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
        let context = context();
        let transaction = workspace
            .begin_mutation("edit.patch", "patch", "value.txt")
            .await
            .unwrap();
        let temp_path = directory
            .path()
            .join(format!(".fa-{}.tmp", transaction.tx_id));
        let result = transaction
            .with_prepare_crash_point(crate::PrepareCrashPoint::StageSynced)
            .prepare_with_effect_context(b"expected complete bytes", context.clone())
            .await;
        assert!(matches!(result, Err(AgentError::RecoveryRequired(_))));
        std::fs::write(&temp_path, b"partial").unwrap();
        drop(workspace);

        let reopened = Workspace::open(directory.path()).await.unwrap();
        let recovered = reopened.reconcile_effect(&context).unwrap();
        assert!(matches!(
            recovered,
            WorkspaceEffectRecovery::Ambiguous { .. }
        ));
        assert_eq!(
            std::fs::read(&temp_path).unwrap(),
            b"partial",
            "recovery must not delete unverified staged content"
        );
    }

    fn append_raw_record(workspace: &Workspace, record: &JournalRecord) {
        let mut writer = workspace.effect_journal.writer.lock().unwrap();
        append_record(&mut writer.file, record).unwrap();
    }

    #[tokio::test]
    async fn real_v1_frame_keeps_checksum_compatibility_and_recovers_bounded() {
        // Frozen output from the v1 writer. In particular, it has neither of
        // the v2 length fields and its checksum covers this exact old record
        // shape rather than a frame synthesized by the current writer.
        const V1_FRAME: &str = r#"{"record":{"version":1,"seq":1,"transition":{"kind":"prepared","tx_id":"00000000-0000-0000-0000-000000000005","context":{"identity":{"run_id":"00000000-0000-0000-0000-000000000001","task_id":null,"turn_id":"00000000-0000-0000-0000-000000000002","scope_id":null,"operation_id":"00000000-0000-0000-0000-000000000003","generation":1,"call_id":"call","tool_name":"edit.patch","argument_digest":"0000000000000000000000000000000000000000000000000000000000000000"},"effect_id":"00000000-0000-0000-0000-000000000004"},"relative_target":"value.txt","temp_name":".fa-00000000-0000-0000-0000-000000000006.tmp","target_existed":true,"before_hash":"1a0fad1921d08076","after_hash":"2138d5192571b731"}},"checksum":"8a70891b9b9743535e4e360c84396a7a0553d2f2adfde59ac99acd66f94ce2cf"}"#;
        const TEMP_NAME: &str = ".fa-00000000-0000-0000-0000-000000000006.tmp";

        let (workspace, directory) = workspace().await;
        std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
        std::fs::write(directory.path().join(TEMP_NAME), b"new").unwrap();
        let authority_path = directory
            .path()
            .join(".focus-agent/authority/workspace-effects.jsonl");
        drop(workspace);

        let frozen: StoredFrame = serde_json::from_str(V1_FRAME).unwrap();
        let context = match &frozen.record.transition {
            JournalTransition::Prepared { context, .. } => (**context).clone(),
            _ => unreachable!("fixture is a prepared v1 frame"),
        };
        let payload = serde_json::to_vec(&frozen.record).unwrap();
        assert_eq!(checksum_hex(&payload), frozen.checksum);
        std::fs::write(&authority_path, format!("{V1_FRAME}\n")).unwrap();

        // Reopening independently verifies the frozen checksum after
        // deserializing and re-serializing the legacy record.
        let reopened = Workspace::open(directory.path()).await.unwrap();
        assert!(matches!(
            reopened.reconcile_effect(&context).unwrap(),
            WorkspaceEffectRecovery::NotApplied { .. }
        ));
        assert!(!directory.path().join(TEMP_NAME).exists());
    }

    #[tokio::test]
    async fn v1_and_v2_recovery_refuse_oversized_targets_without_full_reads() {
        let (workspace, directory) = workspace().await;
        let legacy_context = context();
        let current_context = context();
        let legacy_target = directory.path().join("legacy-large.bin");
        let current_target = directory.path().join("current-large.bin");
        std::fs::File::create(&legacy_target)
            .unwrap()
            .set_len(MAX_MUTATION_BYTES as u64 + 1)
            .unwrap();
        std::fs::File::create(&current_target)
            .unwrap()
            .set_len(MAX_MUTATION_BYTES as u64 + 1)
            .unwrap();

        append_raw_record(
            &workspace,
            &JournalRecord {
                version: LEGACY_JOURNAL_VERSION,
                seq: 1,
                transition: JournalTransition::Prepared {
                    tx_id: Uuid::new_v4().to_string(),
                    context: Box::new(legacy_context.clone()),
                    relative_target: "legacy-large.bin".into(),
                    temp_name: format!(".fa-{}.tmp", Uuid::new_v4()),
                    target_existed: true,
                    before_hash: crate::content_hash(b"legacy"),
                    after_hash: crate::content_hash(b"after"),
                    bytes_before: None,
                    bytes_after: None,
                },
            },
        );
        let current_tx = Uuid::new_v4().to_string();
        append_raw_record(
            &workspace,
            &JournalRecord {
                version: JOURNAL_VERSION,
                seq: 2,
                transition: JournalTransition::Prepared {
                    tx_id: current_tx.clone(),
                    context: Box::new(current_context.clone()),
                    relative_target: "current-large.bin".into(),
                    temp_name: format!(".fa-{current_tx}.tmp"),
                    target_existed: true,
                    before_hash: crate::ContentDigest::sha256_bytes(b"current").to_string(),
                    after_hash: crate::ContentDigest::sha256_bytes(b"after").to_string(),
                    bytes_before: Some(MAX_MUTATION_BYTES as u64),
                    bytes_after: Some(5),
                },
            },
        );
        drop(workspace);

        let reopened = Workspace::open(directory.path()).await.unwrap();
        assert!(matches!(
            reopened.reconcile_effect(&legacy_context),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert!(matches!(
            reopened.reconcile_effect(&current_context),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn reconciliation_read_budget_is_a_hard_aggregate_limit() {
        let mut budget = RecoveryReadBudget::new();
        budget
            .charge(MAX_RECONCILIATION_READ_BYTES as usize)
            .unwrap();
        assert!(matches!(
            budget.charge(1),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn append_refuses_record_and_file_caps_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let authority = ConfinedDir::open_root(directory.path()).unwrap();
        let journal = WorkspaceEffectJournal::open(authority).unwrap();
        {
            let mut writer = journal.writer.lock().unwrap();
            writer.recovery.last_seq = MAX_RECORDS as u64;
        }
        let before = std::fs::metadata(&journal.path).unwrap().len();
        assert!(matches!(
            journal.append_committed(&Uuid::new_v4().to_string()),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert_eq!(std::fs::metadata(&journal.path).unwrap().len(), before);

        let file = tempfile::tempfile().unwrap();
        file.set_len(MAX_FILE_BYTES).unwrap();
        let mut file = file;
        let record = JournalRecord {
            version: JOURNAL_VERSION,
            seq: 1,
            transition: JournalTransition::RolledBack {
                tx_id: Uuid::new_v4().to_string(),
            },
        };
        assert!(matches!(
            append_record(&mut file, &record),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert_eq!(file.metadata().unwrap().len(), MAX_FILE_BYTES);
    }
}
