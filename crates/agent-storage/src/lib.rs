use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use agent_contracts::{
    AgentError, AgentResult, AuthorityCheckpointMarker, AuthorityJournalId, AuthorityStateDigest,
    EventJournal, MAX_AUTHORITY_JOURNAL_ANCESTORS, MAX_OPERATION_JOURNAL_RECOVERY_RECORDS,
    OPERATION_JOURNAL_VERSION, OperationId, OperationJournal, OperationJournalRecord,
    OperationJournalRecovery, OperationJournalTransition, OperationSnapshot, OperationState, RunId,
    RuntimeEventEnvelope,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::{mpsc, oneshot};

const JOURNAL_BUFFER: usize = 4_096;
const MAX_OPERATION_JOURNAL_FRAME_BYTES: usize = 16 * 1024;
const MAX_OPERATION_JOURNAL_FILE_BYTES: u64 = 256 * 1024 * 1024;
const AUTHORITY_JOURNAL_METADATA_VERSION: u32 = 1;
const AUTHORITY_JOURNAL_GENERATION: u64 = 1;
/// 压缩祖先表需要比原来 4KiB 更宽的元数据上限，仍然远小于一帧 WAL。
const MAX_AUTHORITY_JOURNAL_METADATA_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredOperationFrame {
    record: OperationJournalRecord,
    checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityJournalMetadata {
    version: u32,
    journal_id: AuthorityJournalId,
    generation: u64,
    /// 被压缩掉的代际 tip（新的在后）。只证明精确压缩点，不证明旧代际中间 prefix。
    /// `skip_serializing_if` 保持 generation-1 空祖先的 checksum 与旧元数据兼容。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ancestors: Vec<AuthorityCheckpointMarker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAuthorityJournalMetadata {
    metadata: AuthorityJournalMetadata,
    checksum: String,
}

#[derive(Debug)]
struct OperationWriterState {
    file: File,
    next_seq: u64,
    recovery: OperationJournalRecovery,
    operation_indexes: HashMap<OperationId, usize>,
    metadata: AuthorityJournalMetadata,
    failed: Option<String>,
}

/// Crash-recoverable authority journal. Unlike `FileEventJournal`, every
/// successful append crosses an OS stable-storage barrier (`sync_all`) before
/// returning. The checksum detects torn/corrupt frames; it is not an
/// authentication mechanism.
pub struct FileOperationJournal {
    path: PathBuf,
    writer: Mutex<OperationWriterState>,
}

impl FileOperationJournal {
    pub fn open(path: impl AsRef<Path>) -> AgentResult<(Self, OperationJournalRecovery)> {
        let path = path.as_ref().to_path_buf();
        let metadata_path = path.with_extension("meta.json");
        let existing_metadata = if metadata_path.exists() {
            Some(read_authority_metadata(&metadata_path)?)
        } else {
            None
        };
        let generation = existing_metadata
            .as_ref()
            .map(|metadata| metadata.generation)
            .unwrap_or(AUTHORITY_JOURNAL_GENERATION);
        let wal_path = authority_wal_path(&path, generation);
        let journal_existed = wal_path.exists();
        if !journal_existed && existing_metadata.is_some() {
            return Err(AgentError::RecoveryRequired(format!(
                "authority journal metadata {} exists but its operation WAL {} is missing",
                metadata_path.display(),
                wal_path.display()
            )));
        }
        let mut directory_sync_chain = Vec::new();
        if let Some(parent) = wal_path.parent() {
            directory_sync_chain.push(parent.to_path_buf());
            let mut cursor = parent;
            while !cursor.exists() {
                let Some(ancestor) = cursor.parent() else {
                    break;
                };
                directory_sync_chain.push(ancestor.to_path_buf());
                cursor = ancestor;
            }
            fs::create_dir_all(parent).map_err(|error| {
                AgentError::Storage(format!(
                    "create operation journal directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&wal_path)
            .map_err(|error| {
                AgentError::Storage(format!(
                    "open operation journal {}: {error}",
                    wal_path.display()
                ))
            })?;
        file.try_lock().map_err(|error| {
            AgentError::Storage(format!(
                "lock operation journal {} exclusively: {error}",
                wal_path.display()
            ))
        })?;
        if !journal_existed {
            file.sync_all().map_err(|error| {
                AgentError::Storage(format!(
                    "sync new operation journal {}: {error}",
                    wal_path.display()
                ))
            })?;
            for directory in directory_sync_chain {
                sync_directory(&directory)?;
            }
        }
        let recovery = recover_operation_file(&mut file, &wal_path)?;
        let metadata = match existing_metadata {
            Some(metadata) => metadata,
            None => load_or_create_authority_metadata(&metadata_path)?,
        };
        validate_recovered_generation(&metadata, &recovery)?;
        let next_seq = recovery.last_seq.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("operation journal sequence exhausted".into())
        })?;
        file.seek(SeekFrom::End(0)).map_err(|error| {
            AgentError::Storage(format!(
                "seek operation journal {}: {error}",
                wal_path.display()
            ))
        })?;
        let operation_indexes = recovery
            .operations
            .iter()
            .enumerate()
            .map(|(index, snapshot)| (snapshot.identity.operation_id, index))
            .collect();
        Ok((
            Self {
                path,
                writer: Mutex::new(OperationWriterState {
                    file,
                    next_seq,
                    recovery: recovery.clone(),
                    operation_indexes,
                    metadata,
                    failed: None,
                }),
            },
            recovery,
        ))
    }
}

fn authority_wal_path(base: &Path, generation: u64) -> PathBuf {
    if generation <= 1 {
        base.to_path_buf()
    } else {
        let name = base
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "operations.jsonl".into());
        base.with_file_name(format!("{name}.g{generation}"))
    }
}

fn validate_recovered_generation(
    metadata: &AuthorityJournalMetadata,
    recovery: &OperationJournalRecovery,
) -> AgentResult<()> {
    match (metadata.generation, recovery.compacted_from.as_ref()) {
        (1, None) => Ok(()),
        (1, Some(_)) => Err(AgentError::RecoveryRequired(
            "generation-1 authority WAL unexpectedly starts with a Compacted baseline".into(),
        )),
        (generation, Some(previous)) => {
            if previous.journal_id != metadata.journal_id
                || previous
                    .generation
                    .checked_add(1)
                    .is_none_or(|expected| expected != generation)
            {
                Err(AgentError::RecoveryRequired(format!(
                    "compacted authority WAL lineage does not match metadata (WAL previous generation {}, metadata generation {generation})",
                    previous.generation
                )))
            } else {
                Ok(())
            }
        }
        (generation, None) => Err(AgentError::RecoveryRequired(format!(
            "authority journal generation {generation} is missing its Compacted baseline"
        ))),
    }
}

fn load_or_create_authority_metadata(path: &Path) -> AgentResult<AuthorityJournalMetadata> {
    if path.exists() {
        return read_authority_metadata(path);
    }

    let metadata = AuthorityJournalMetadata {
        version: AUTHORITY_JOURNAL_METADATA_VERSION,
        journal_id: AuthorityJournalId::new(),
        generation: AUTHORITY_JOURNAL_GENERATION,
        ancestors: Vec::new(),
    };
    persist_authority_metadata(path, &metadata)?;
    Ok(metadata)
}

fn persist_authority_metadata(
    path: &Path,
    metadata: &AuthorityJournalMetadata,
) -> AgentResult<()> {
    let payload = serde_json::to_vec(metadata)
        .map_err(|error| AgentError::Storage(format!("serialize authority metadata: {error}")))?;
    let stored = StoredAuthorityJournalMetadata {
        checksum: checksum_hex(&payload),
        metadata: metadata.clone(),
    };
    let encoded = serde_json::to_vec(&stored)
        .map_err(|error| AgentError::Storage(format!("serialize authority metadata: {error}")))?;
    if encoded.len() as u64 > MAX_AUTHORITY_JOURNAL_METADATA_BYTES {
        return Err(AgentError::Storage(
            "authority journal metadata exceeds its hard limit".into(),
        ));
    }
    let temporary = path.with_extension(format!(
        "meta-{}-{}.tmp",
        metadata.journal_id, metadata.generation
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| {
            AgentError::Storage(format!(
                "create authority metadata temporary file {}: {error}",
                temporary.display()
            ))
        })?;
    if let Err(error) = file
        .write_all(&encoded)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(AgentError::Storage(format!(
            "persist authority journal metadata {}: {error}",
            path.display()
        )));
    }
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(AgentError::Storage(format!(
            "commit authority journal metadata {}: {error}",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    if to.exists() {
        fs::remove_file(to)?;
    }
    fs::rename(from, to)
}

fn read_authority_metadata(path: &Path) -> AgentResult<AuthorityJournalMetadata> {
    let size = fs::metadata(path)
        .map_err(|error| {
            AgentError::Storage(format!(
                "stat authority journal metadata {}: {error}",
                path.display()
            ))
        })?
        .len();
    if size == 0 || size > MAX_AUTHORITY_JOURNAL_METADATA_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "authority journal metadata {} has an invalid size",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| {
        AgentError::Storage(format!(
            "read authority journal metadata {}: {error}",
            path.display()
        ))
    })?;
    let stored: StoredAuthorityJournalMetadata =
        serde_json::from_slice(&bytes).map_err(|error| {
            AgentError::RecoveryRequired(format!(
                "authority journal metadata {} is corrupt: invalid JSON: {error}",
                path.display()
            ))
        })?;
    let payload = serde_json::to_vec(&stored.metadata).map_err(|error| {
        AgentError::Storage(format!("serialize recovered authority metadata: {error}"))
    })?;
    if stored.checksum != checksum_hex(&payload)
        || stored.metadata.version != AUTHORITY_JOURNAL_METADATA_VERSION
        || stored.metadata.journal_id.0.is_nil()
        || stored.metadata.generation == 0
    {
        return Err(AgentError::RecoveryRequired(format!(
            "authority journal metadata {} is corrupt or unsupported",
            path.display()
        )));
    }
    Ok(stored.metadata)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> AgentResult<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            AgentError::Storage(format!(
                "sync operation journal directory {}: {error}",
                path.display()
            ))
        })
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> AgentResult<()> {
    // Windows does not provide a supported FlushFileBuffers barrier for a
    // directory handle. The newly created journal file itself is sync_all'd
    // before Core can publish authority state; the remaining parent-directory
    // power-loss window is documented as a platform limitation rather than
    // turning every first startup into ERROR_ACCESS_DENIED.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> AgentResult<()> {
    Err(AgentError::Storage(format!(
        "operation journal creation cannot establish a directory durability barrier on this platform: {}",
        path.display()
    )))
}

impl OperationJournal for FileOperationJournal {
    fn append_and_sync(
        &self,
        transition: &OperationJournalTransition,
    ) -> AgentResult<OperationJournalRecord> {
        transition.validate().map_err(AgentError::InvalidRequest)?;
        if matches!(
            transition,
            OperationJournalTransition::Compacted { .. }
        ) {
            return Err(AgentError::InvalidRequest(
                "Compacted baseline can only be written by journal compaction".into(),
            ));
        }
        let mut writer = self.writer.lock().expect("operation journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        if writer.next_seq > MAX_OPERATION_JOURNAL_RECOVERY_RECORDS as u64 {
            compact_locked(&self.path, &mut writer)?;
            if writer.next_seq > MAX_OPERATION_JOURNAL_RECOVERY_RECORDS as u64 {
                return Err(AgentError::RecoveryRequired(format!(
                    "operation journal reached its {} record recovery limit after compaction",
                    MAX_OPERATION_JOURNAL_RECOVERY_RECORDS
                )));
            }
        }
        let record = OperationJournalRecord {
            version: OPERATION_JOURNAL_VERSION,
            seq: writer.next_seq,
            transition: transition.clone(),
        };
        validate_cached_record(&writer.recovery, &writer.operation_indexes, &record)
            .map_err(AgentError::RecoveryRequired)?;
        let next_seq = writer.next_seq.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("operation journal sequence exhausted".into())
        })?;
        let result = append_operation_record(&mut writer.file, &record);
        if let Err(error) = result {
            let message = format!(
                "operation journal {} failed permanently: {error}",
                self.path.display()
            );
            writer.failed = Some(message.clone());
            return Err(AgentError::Storage(message));
        }
        let OperationWriterState {
            recovery,
            operation_indexes,
            ..
        } = &mut *writer;
        apply_cached_record(recovery, operation_indexes, &record);
        writer.next_seq = next_seq;
        Ok(record)
    }

    fn recover(&self) -> AgentResult<OperationJournalRecovery> {
        let writer = self.writer.lock().expect("operation journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        Ok(writer.recovery.clone())
    }

    fn authority_checkpoint_marker(&self) -> AgentResult<AuthorityCheckpointMarker> {
        let writer = self.writer.lock().expect("operation journal poisoned");
        ensure_operation_writer_healthy(&writer)?;
        authority_checkpoint_marker(&writer.metadata, &writer.recovery)
    }

    fn validate_authority_checkpoint_marker(
        &self,
        expected: &AuthorityCheckpointMarker,
    ) -> AgentResult<()> {
        expected.validate().map_err(AgentError::InvalidRequest)?;
        let writer = self.writer.lock().expect("operation journal poisoned");
        ensure_operation_writer_healthy(&writer)?;
        if expected.journal_id != writer.metadata.journal_id {
            return Err(authority_marker_mismatch(expected, &writer)?);
        }
        if expected.generation != writer.metadata.generation {
            if writer.metadata.ancestors.iter().any(|ancestor| ancestor == expected) {
                return Ok(());
            }
            return Err(authority_marker_mismatch(expected, &writer)?);
        }
        if expected.last_seq > writer.recovery.last_seq
            || expected.authority_epoch > writer.recovery.authority_epoch
        {
            return Err(authority_marker_mismatch(expected, &writer)?);
        }
        let prefix = recover_operation_prefix(&writer.file, &self.path, expected.last_seq)?;
        let actual = authority_checkpoint_marker(&writer.metadata, &prefix)?;
        if actual == *expected {
            Ok(())
        } else {
            Err(authority_marker_mismatch(expected, &writer)?)
        }
    }

    fn compact(&self) -> AgentResult<AuthorityCheckpointMarker> {
        let mut writer = self.writer.lock().expect("operation journal poisoned");
        compact_locked(&self.path, &mut writer)
    }
}

fn compact_locked(
    base_path: &Path,
    writer: &mut OperationWriterState,
) -> AgentResult<AuthorityCheckpointMarker> {
    ensure_operation_writer_healthy(writer)?;
    if writer.recovery.last_seq == 0 {
        return authority_checkpoint_marker(&writer.metadata, &writer.recovery);
    }
    let previous = authority_checkpoint_marker(&writer.metadata, &writer.recovery)?;
    let next_generation = writer
        .metadata
        .generation
        .checked_add(1)
        .ok_or_else(|| AgentError::RecoveryRequired("authority journal generation exhausted".into()))?;
    let new_path = authority_wal_path(base_path, next_generation);
    let mut operations = writer.recovery.operations.clone();
    operations.sort_by_key(|snapshot| snapshot.identity.operation_id.to_string());

    let mut records = Vec::with_capacity(operations.len().saturating_add(1));
    records.push(OperationJournalRecord {
        version: OPERATION_JOURNAL_VERSION,
        seq: 1,
        transition: OperationJournalTransition::Compacted {
            previous: previous.clone(),
        },
    });
    for (index, snapshot) in operations.iter().enumerate() {
        records.push(OperationJournalRecord {
            version: OPERATION_JOURNAL_VERSION,
            seq: index as u64 + 2,
            transition: OperationJournalTransition::OperationUpsert {
                snapshot: Box::new(snapshot.clone()),
            },
        });
    }

    let mut new_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&new_path)
        .map_err(|error| {
            AgentError::Storage(format!(
                "create compacted operation journal {}: {error}",
                new_path.display()
            ))
        })?;
    write_operation_records(&mut new_file, &records)?;
    new_file.try_lock().map_err(|error| {
        AgentError::Storage(format!(
            "lock compacted operation journal {} exclusively: {error}",
            new_path.display()
        ))
    })?;

    let mut ancestors = writer.metadata.ancestors.clone();
    ancestors.push(previous.clone());
    if ancestors.len() > MAX_AUTHORITY_JOURNAL_ANCESTORS {
        let overflow = ancestors.len() - MAX_AUTHORITY_JOURNAL_ANCESTORS;
        ancestors.drain(..overflow);
    }
    let new_metadata = AuthorityJournalMetadata {
        version: AUTHORITY_JOURNAL_METADATA_VERSION,
        journal_id: writer.metadata.journal_id,
        generation: next_generation,
        ancestors,
    };
    persist_authority_metadata(&base_path.with_extension("meta.json"), &new_metadata)?;

    let last_seq = records.last().map(|record| record.seq).unwrap_or(1);
    let next_seq = last_seq
        .checked_add(1)
        .ok_or_else(|| AgentError::RecoveryRequired("operation journal sequence exhausted".into()))?;
    let mut recovery = writer.recovery.clone();
    recovery.last_seq = last_seq;
    recovery.compacted_from = Some(previous);
    recovery.operations = operations;
    let operation_indexes = recovery
        .operations
        .iter()
        .enumerate()
        .map(|(index, snapshot)| (snapshot.identity.operation_id, index))
        .collect();
    new_file.seek(SeekFrom::End(0)).map_err(|error| {
        AgentError::Storage(format!(
            "seek compacted operation journal {}: {error}",
            new_path.display()
        ))
    })?;

    let old_wal = authority_wal_path(base_path, writer.metadata.generation);
    writer.file = new_file;
    writer.next_seq = next_seq;
    writer.recovery = recovery;
    writer.operation_indexes = operation_indexes;
    writer.metadata = new_metadata;
    let _ = fs::remove_file(old_wal);
    authority_checkpoint_marker(&writer.metadata, &writer.recovery)
}

fn write_operation_records(
    file: &mut File,
    records: &[OperationJournalRecord],
) -> AgentResult<()> {
    let mut encoded_all = Vec::new();
    for record in records {
        let payload = serde_json::to_vec(record).map_err(|error| {
            AgentError::Storage(format!("serialize compacted operation record: {error}"))
        })?;
        let frame = StoredOperationFrame {
            checksum: checksum_hex(&payload),
            record: record.clone(),
        };
        let encoded = serde_json::to_vec(&frame).map_err(|error| {
            AgentError::Storage(format!("serialize compacted operation frame: {error}"))
        })?;
        if encoded.len() > MAX_OPERATION_JOURNAL_FRAME_BYTES {
            return Err(AgentError::Storage(format!(
                "operation journal frame exceeds {MAX_OPERATION_JOURNAL_FRAME_BYTES} bytes"
            )));
        }
        encoded_all.extend_from_slice(&encoded);
        encoded_all.push(b'\n');
    }
    if encoded_all.len() as u64 > MAX_OPERATION_JOURNAL_FILE_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "compacted operation journal exceeds its {} byte hard limit",
            MAX_OPERATION_JOURNAL_FILE_BYTES
        )));
    }
    file.write_all(&encoded_all)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| AgentError::Storage(format!("persist compacted operation journal: {error}")))
}

fn ensure_operation_writer_healthy(writer: &OperationWriterState) -> AgentResult<()> {
    match &writer.failed {
        Some(error) => Err(AgentError::Storage(error.clone())),
        None => Ok(()),
    }
}

fn authority_marker_mismatch(
    expected: &AuthorityCheckpointMarker,
    writer: &OperationWriterState,
) -> AgentResult<AgentError> {
    let actual = authority_checkpoint_marker(&writer.metadata, &writer.recovery)?;
    Ok(AgentError::RecoveryRequired(format!(
        "authority checkpoint marker is not a verified ancestor of current durable authority (expected journal {} generation {} epoch {} seq {}, current journal {} generation {} epoch {} seq {})",
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

fn authority_checkpoint_marker(
    metadata: &AuthorityJournalMetadata,
    recovery: &OperationJournalRecovery,
) -> AgentResult<AuthorityCheckpointMarker> {
    let mut operations = recovery.operations.clone();
    operations.sort_by_key(|snapshot| snapshot.identity.operation_id.0);
    #[derive(Serialize)]
    struct DurableAuthorityTruth<'a> {
        format: &'static str,
        last_seq: u64,
        authority_epoch: u64,
        operations: &'a [OperationSnapshot],
    }
    let bytes = serde_json::to_vec(&DurableAuthorityTruth {
        format: "focus-agent-authority-state-v1",
        last_seq: recovery.last_seq,
        authority_epoch: recovery.authority_epoch,
        operations: &operations,
    })
    .map_err(|error| AgentError::Storage(format!("serialize authority state digest: {error}")))?;
    Ok(AuthorityCheckpointMarker {
        journal_id: metadata.journal_id,
        generation: metadata.generation,
        authority_epoch: recovery.authority_epoch,
        last_seq: recovery.last_seq,
        state_digest: AuthorityStateDigest::sha256_bytes(&bytes),
    })
}

fn validate_cached_record(
    recovery: &OperationJournalRecovery,
    operation_indexes: &HashMap<OperationId, usize>,
    record: &OperationJournalRecord,
) -> Result<(), String> {
    let expected = recovery
        .last_seq
        .checked_add(1)
        .ok_or_else(|| "operation journal sequence overflow".to_string())?;
    if record.seq != expected {
        return Err("operation journal sequence is not contiguous".into());
    }
    match &record.transition {
        OperationJournalTransition::EpochAdvanced { from, .. } => {
            if recovery.authority_epoch != *from {
                return Err(format!(
                    "authority epoch gap: current {}, transition starts at {from}",
                    recovery.authority_epoch
                ));
            }
        }
        OperationJournalTransition::OperationUpsert { snapshot } => {
            if let Some(index) = operation_indexes.get(&snapshot.identity.operation_id) {
                let previous = &recovery.operations[*index];
                if previous.identity != snapshot.identity
                    || !valid_recovered_transition(&previous.state, &snapshot.state)
                {
                    return Err(format!(
                        "operation {} identity/state transition is not monotonic",
                        snapshot.identity.operation_id
                    ));
                }
            } else if !matches!(
                snapshot.state,
                OperationState::Accepted
                    | OperationState::Terminal {
                        terminal: agent_contracts::OperationTerminal::CancelledBeforeCommit,
                        ..
                    }
            ) {
                return Err(format!(
                    "operation {} first record is not Accepted or a cancellation reservation",
                    snapshot.identity.operation_id
                ));
            }
        }
        OperationJournalTransition::Compacted { .. } => {
            return Err("Compacted baseline can only be written by journal compaction".into());
        }
    }
    Ok(())
}

fn apply_cached_record(
    recovery: &mut OperationJournalRecovery,
    operation_indexes: &mut HashMap<OperationId, usize>,
    record: &OperationJournalRecord,
) {
    match &record.transition {
        OperationJournalTransition::EpochAdvanced { to, .. } => {
            recovery.authority_epoch = *to;
        }
        OperationJournalTransition::OperationUpsert { snapshot } => {
            let operation_id = snapshot.identity.operation_id;
            if let Some(index) = operation_indexes.get(&operation_id).copied() {
                recovery.operations[index] = snapshot.as_ref().clone();
            } else {
                operation_indexes.insert(operation_id, recovery.operations.len());
                recovery.operations.push(snapshot.as_ref().clone());
            }
        }
        OperationJournalTransition::Compacted { previous } => {
            recovery.authority_epoch = previous.authority_epoch;
            recovery.compacted_from = Some(previous.clone());
        }
    }
    recovery.last_seq = record.seq;
}

fn append_operation_record(file: &mut File, record: &OperationJournalRecord) -> AgentResult<()> {
    let payload = serde_json::to_vec(record)
        .map_err(|error| AgentError::Storage(format!("serialize operation record: {error}")))?;
    let frame = StoredOperationFrame {
        checksum: checksum_hex(&payload),
        record: record.clone(),
    };
    let encoded = serde_json::to_vec(&frame)
        .map_err(|error| AgentError::Storage(format!("serialize operation frame: {error}")))?;
    if encoded.len() > MAX_OPERATION_JOURNAL_FRAME_BYTES {
        return Err(AgentError::Storage(format!(
            "operation journal frame exceeds {MAX_OPERATION_JOURNAL_FRAME_BYTES} bytes"
        )));
    }
    let projected = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat operation journal: {error}")))?
        .len()
        .checked_add(encoded.len() as u64 + 1)
        .ok_or_else(|| AgentError::Storage("operation journal size overflow".into()))?;
    if projected > MAX_OPERATION_JOURNAL_FILE_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "operation journal reached its {} byte hard limit; checkpoint/compaction is required",
            MAX_OPERATION_JOURNAL_FILE_BYTES
        )));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(&encoded))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| AgentError::Storage(format!("persist operation journal: {error}")))
}

fn recover_operation_file(file: &mut File, path: &Path) -> AgentResult<OperationJournalRecovery> {
    recover_operation_file_until(file, path, None, true)
}

fn recover_operation_prefix(
    writer_file: &File,
    path: &Path,
    last_seq: u64,
) -> AgentResult<OperationJournalRecovery> {
    // Duplicate the already exclusively locked writer handle. Opening a
    // second path handle is denied on Windows and would make valid ancestor
    // checks platform-dependent.
    let mut file = writer_file.try_clone().map_err(|error| {
        AgentError::Storage(format!(
            "clone operation journal for prefix validation {}: {error}",
            path.display()
        ))
    })?;
    recover_operation_file_until(&mut file, path, Some(last_seq), false)
}

fn recover_operation_file_until(
    file: &mut File,
    path: &Path,
    stop_after: Option<u64>,
    repair_torn_tail: bool,
) -> AgentResult<OperationJournalRecovery> {
    let size = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat operation journal: {error}")))?
        .len();
    if size > MAX_OPERATION_JOURNAL_FILE_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "operation journal {} exceeds its hard limit",
            path.display()
        )));
    }
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        AgentError::Storage(format!(
            "seek operation journal {}: {error}",
            path.display()
        ))
    })?;
    let clone = file.try_clone().map_err(|error| {
        AgentError::Storage(format!(
            "clone operation journal {}: {error}",
            path.display()
        ))
    })?;
    let mut reader = BufReader::new(clone);
    let mut recovery = OperationJournalRecovery::default();
    let mut operations = HashMap::<OperationId, OperationSnapshot>::new();
    let mut offset = 0_u64;
    let mut last_good = 0_u64;
    if stop_after == Some(0) {
        return Ok(recovery);
    }
    loop {
        let mut bytes = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_OPERATION_JOURNAL_FRAME_BYTES + 2) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|error| AgentError::Storage(format!("read operation journal: {error}")))?;
        if read == 0 {
            break;
        }
        offset = offset.checked_add(read as u64).ok_or_else(|| {
            AgentError::RecoveryRequired("operation journal offset overflow".into())
        })?;
        let final_frame = offset == size;
        if !bytes.ends_with(b"\n") {
            if final_frame {
                recovery.truncated_tail = true;
                break;
            }
            return Err(corrupt_operation_journal(path, "partial middle frame"));
        }
        if bytes.len() > MAX_OPERATION_JOURNAL_FRAME_BYTES + 1 {
            return Err(corrupt_operation_journal(path, "oversized complete frame"));
        }
        bytes.pop();
        let parsed = serde_json::from_slice::<StoredOperationFrame>(&bytes)
            .map_err(|error| format!("invalid JSON: {error}"))
            .and_then(|frame| {
                let payload = serde_json::to_vec(&frame.record)
                    .map_err(|error| format!("record serialization failed: {error}"))?;
                if frame.checksum != checksum_hex(&payload) {
                    return Err("checksum mismatch".into());
                }
                Ok(frame.record)
            });
        let record = parsed.map_err(|error| corrupt_operation_journal(path, &error))?;
        record
            .validate()
            .map_err(|error| corrupt_operation_journal(path, &error))?;
        let expected = recovery.last_seq.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("operation journal sequence overflow".into())
        })?;
        if record.seq != expected {
            return Err(corrupt_operation_journal(path, "non-contiguous sequence"));
        }
        fold_operation_record(&mut recovery, &mut operations, &record)
            .map_err(|error| corrupt_operation_journal(path, &error))?;
        recovery.last_seq = record.seq;
        last_good = offset;
        if recovery.last_seq > MAX_OPERATION_JOURNAL_RECOVERY_RECORDS as u64 {
            return Err(AgentError::RecoveryRequired(format!(
                "operation journal exceeds the {} record recovery limit",
                MAX_OPERATION_JOURNAL_RECOVERY_RECORDS
            )));
        }
        if stop_after == Some(recovery.last_seq) {
            break;
        }
    }
    if let Some(expected) = stop_after
        && recovery.last_seq != expected
    {
        return Err(AgentError::RecoveryRequired(format!(
            "authority checkpoint references future operation journal sequence {expected}; current verified prefix ends at {}",
            recovery.last_seq
        )));
    }
    if recovery.truncated_tail && repair_torn_tail {
        drop(reader);
        file.set_len(last_good).map_err(|error| {
            AgentError::Storage(format!("truncate torn operation journal tail: {error}"))
        })?;
        file.sync_all().map_err(|error| {
            AgentError::Storage(format!("sync repaired operation journal: {error}"))
        })?;
    }
    recovery.operations = operations.into_values().collect();
    recovery
        .operations
        .sort_by_key(|snapshot| snapshot.identity.operation_id.to_string());
    Ok(recovery)
}

fn fold_operation_record(
    recovery: &mut OperationJournalRecovery,
    operations: &mut HashMap<OperationId, OperationSnapshot>,
    record: &OperationJournalRecord,
) -> Result<(), String> {
    match &record.transition {
        OperationJournalTransition::EpochAdvanced { from, to } => {
            if recovery.authority_epoch != *from {
                return Err(format!(
                    "authority epoch gap: current {}, transition {from}->{to}",
                    recovery.authority_epoch
                ));
            }
            recovery.authority_epoch = *to;
        }
        OperationJournalTransition::OperationUpsert { snapshot } => {
            if let Some(previous) = operations.get(&snapshot.identity.operation_id) {
                if previous.identity != snapshot.identity
                    || !valid_recovered_transition(&previous.state, &snapshot.state)
                {
                    return Err(format!(
                        "operation {} identity/state transition is not monotonic",
                        snapshot.identity.operation_id
                    ));
                }
            } else if recovery.compacted_from.is_none()
                && !matches!(
                    snapshot.state,
                    OperationState::Accepted
                        | OperationState::Terminal {
                            terminal: agent_contracts::OperationTerminal::CancelledBeforeCommit,
                            ..
                        }
                )
            {
                return Err(format!(
                    "operation {} first record is not Accepted or a cancellation reservation",
                    snapshot.identity.operation_id
                ));
            }
            operations.insert(snapshot.identity.operation_id, snapshot.as_ref().clone());
        }
        OperationJournalTransition::Compacted { previous } => {
            if recovery.last_seq != 0 || recovery.compacted_from.is_some() {
                return Err("Compacted baseline must be the first record of a compacted WAL".into());
            }
            previous.validate()?;
            recovery.authority_epoch = previous.authority_epoch;
            recovery.compacted_from = Some(previous.clone());
        }
    }
    Ok(())
}

fn valid_recovered_transition(previous: &OperationState, next: &OperationState) -> bool {
    let accepted = matches!(
        (previous, next),
        (OperationState::Accepted, OperationState::Executing { .. })
            | (
                OperationState::Accepted,
                OperationState::Terminal {
                    effect_id: None,
                    ..
                }
            )
    );
    let executing_prepared = matches!(
        (previous, next),
        (
            OperationState::Executing {
                effect_id: Some(left),
            },
            OperationState::Prepared { effect_id: right },
        ) if left == right
    );
    // A side-effecting declaration may legitimately return an ordinary
    // value without ever preparing its reserved effect; that path discards
    // the reservation and records a value terminal with no effect id.
    // Every terminal that *does* claim an effect must preserve the exact id
    // already reserved in Executing. This prevents a corrupt recovery row
    // from attaching workspace evidence for one effect to another.
    let executing_terminal = matches!(
        (previous, next),
        (
            OperationState::Executing { effect_id: None },
            OperationState::Terminal {
                effect_id: None,
                ..
            }
        ) | (
            OperationState::Executing { effect_id: Some(_) },
            OperationState::Terminal {
                effect_id: None,
                terminal: agent_contracts::OperationTerminal::CompletedValue,
            }
        )
    ) || matches!(
        (previous, next),
        (
            OperationState::Executing {
                effect_id: Some(left),
            },
            OperationState::Terminal {
                effect_id: Some(right),
                ..
            }
        ) if left == right
    );
    let prepared_commit = matches!(
        (previous, next),
        (OperationState::Prepared { effect_id: left }, OperationState::CommitStarted { effect_id: right }) if left == right
    );
    accepted
        || executing_prepared
        || executing_terminal
        || prepared_commit
        || matches!(
            (previous, next),
            (OperationState::Prepared { effect_id: left }, OperationState::Terminal { effect_id: Some(right), .. }) if left == right
        )
        || matches!(
            (previous, next),
            (OperationState::CommitStarted { effect_id: left }, OperationState::Terminal { effect_id: Some(right), .. }) if left == right
        )
        || previous == next
}

fn checksum_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn corrupt_operation_journal(path: &Path, detail: &str) -> AgentError {
    AgentError::RecoveryRequired(format!(
        "operation journal {} is corrupt: {detail}",
        path.display()
    ))
}

enum JournalCommand {
    Append(Box<RuntimeEventEnvelope>),
    Flush(oneshot::Sender<AgentResult<()>>),
}

/// Append-only JSONL trace storage.
///
/// The async hot path only enqueues events. A dedicated blocking writer owns
/// all files. `flush` is the *durability barrier*: because the channel is
/// FIFO, a successful `flush` guarantees every event appended before it has
/// left the process (the writer drained and flushed each `BufWriter` to the
/// OS), which is the turn-commit durability contract — events buffered in
/// userspace or in the pipe are not durable until the barrier passes.
/// Writer errors are sticky: the first failed write poisons the writer, and
/// every later barrier reports that error instead of pretending the trace
/// is intact.
pub struct FileEventJournal {
    tx: mpsc::Sender<JournalCommand>,
}

impl FileEventJournal {
    pub async fn open(directory: impl AsRef<Path>) -> AgentResult<Self> {
        let directory = directory.as_ref().to_path_buf();
        tokio::task::spawn_blocking({
            let directory = directory.clone();
            move || {
                fs::create_dir_all(&directory)
                    .map_err(|e| AgentError::Storage(format!("create trace directory: {e}")))
            }
        })
        .await
        .map_err(|e| AgentError::Storage(format!("trace init task: {e}")))??;

        let (tx, mut rx) = mpsc::channel::<JournalCommand>(JOURNAL_BUFFER);
        tokio::task::spawn_blocking(move || {
            let mut writers: HashMap<RunId, BufWriter<File>> = HashMap::new();
            // Sticky failure: once any write fails, every later barrier
            // reports it and the writer stops touching files — a trace with
            // a gap must never be mistaken for a complete one, and a broken
            // `BufWriter` is not safe to reuse. Appends after the failure
            // are still drained from the channel (the sequence stays
            // consistent) but dropped.
            let mut failed: Option<String> = None;

            while let Some(command) = rx.blocking_recv() {
                match command {
                    JournalCommand::Append(envelope) => {
                        if failed.is_none()
                            && let Err(error) = append_event(&directory, &mut writers, &envelope)
                        {
                            failed = Some(error.to_string());
                        }
                    }
                    JournalCommand::Flush(reply) => {
                        let result = match &failed {
                            Some(error) => Err(AgentError::Storage(error.clone())),
                            None => match flush_all(&mut writers) {
                                Ok(()) => Ok(()),
                                Err(error) => {
                                    failed = Some(error.to_string());
                                    Err(error)
                                }
                            },
                        };
                        let _ = reply.send(result);
                    }
                }
            }

            if failed.is_none() {
                let _ = flush_all(&mut writers);
            }
        });

        Ok(Self { tx })
    }
}

#[async_trait::async_trait]
impl EventJournal for FileEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        self.tx
            .send(JournalCommand::Append(Box::new(envelope.clone())))
            .await
            .map_err(|_| AgentError::Storage("event journal writer stopped".into()))
    }

    async fn flush(&self) -> AgentResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(JournalCommand::Flush(tx))
            .await
            .map_err(|_| AgentError::Storage("event journal writer stopped".into()))?;
        rx.await
            .map_err(|_| AgentError::Storage("event journal flush failed".into()))?
    }
}

fn append_event(
    directory: &Path,
    writers: &mut HashMap<RunId, BufWriter<File>>,
    envelope: &RuntimeEventEnvelope,
) -> AgentResult<()> {
    match writers.entry(envelope.run_id) {
        std::collections::hash_map::Entry::Occupied(_) => {}
        std::collections::hash_map::Entry::Vacant(entry) => {
            let path = trace_path(directory, envelope.run_id);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| AgentError::Storage(format!("open trace {}: {e}", path.display())))?;
            entry.insert(BufWriter::with_capacity(64 * 1024, file));
        }
    }

    let writer = writers
        .get_mut(&envelope.run_id)
        .ok_or_else(|| AgentError::Storage("trace writer disappeared".into()))?;
    serde_json::to_writer(&mut *writer, envelope)
        .map_err(|e| AgentError::Storage(format!("serialize trace event: {e}")))?;
    writer
        .write_all(b"\n")
        .map_err(|e| AgentError::Storage(format!("append trace event: {e}")))?;
    Ok(())
}

fn flush_all(writers: &mut HashMap<RunId, BufWriter<File>>) -> AgentResult<()> {
    for writer in writers.values_mut() {
        writer
            .flush()
            .map_err(|e| AgentError::Storage(format!("flush trace: {e}")))?;
    }
    Ok(())
}

fn trace_path(directory: &Path, run_id: RunId) -> PathBuf {
    directory.join(format!("{run_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ArgumentDigest, EffectId, OperationJournal, OperationJournalTransition, OperationState,
        OperationTerminal, RunId, RuntimeEvent, ToolOperationIdentity, TurnId,
    };

    fn operation_snapshot(operation_id: OperationId, state: OperationState) -> OperationSnapshot {
        OperationSnapshot {
            identity: ToolOperationIdentity {
                run_id: RunId::new(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id,
                generation: 1,
                call_id: "call-1".into(),
                tool_name: "fs.read".into(),
                argument_digest: ArgumentDigest::sha256_bytes(b"args"),
            },
            state,
        }
    }

    fn upsert(snapshot: OperationSnapshot) -> OperationJournalTransition {
        OperationJournalTransition::OperationUpsert {
            snapshot: Box::new(snapshot),
        }
    }

    #[test]
    fn operation_journal_round_trips_and_rejects_a_second_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        let (journal, recovery) = FileOperationJournal::open(&path).unwrap();
        assert_eq!(recovery, OperationJournalRecovery::default());
        let operation_id = OperationId::new();
        let accepted = operation_snapshot(operation_id, OperationState::Accepted);
        journal.append_and_sync(&upsert(accepted.clone())).unwrap();
        journal
            .append_and_sync(&OperationJournalTransition::EpochAdvanced { from: 1, to: 2 })
            .unwrap();
        let cached = journal.recover().unwrap();
        assert_eq!(cached.last_seq, 2);
        assert_eq!(cached.authority_epoch, 2);
        assert_eq!(cached.operations, vec![accepted.clone()]);
        assert!(FileOperationJournal::open(&path).is_err());
        drop(journal);

        let (_journal, recovery) = FileOperationJournal::open(&path).unwrap();
        assert_eq!(recovery.last_seq, 2);
        assert_eq!(recovery.authority_epoch, 2);
        assert_eq!(recovery.operations, vec![accepted]);
        assert!(!recovery.truncated_tail);
    }

    #[test]
    fn authority_marker_is_stable_and_verified_as_an_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        let (journal, _) = FileOperationJournal::open(&path).unwrap();
        let initial = journal.authority_checkpoint_marker().unwrap();
        assert_eq!(initial.generation, 1);
        assert_eq!(initial.last_seq, 0);
        assert_eq!(initial.authority_epoch, 1);

        journal
            .append_and_sync(&OperationJournalTransition::EpochAdvanced { from: 1, to: 2 })
            .unwrap();
        let ancestor = journal.authority_checkpoint_marker().unwrap();
        journal
            .append_and_sync(&upsert(operation_snapshot(
                OperationId::new(),
                OperationState::Accepted,
            )))
            .unwrap();
        journal
            .validate_authority_checkpoint_marker(&ancestor)
            .unwrap();

        let mut tampered = ancestor.clone();
        tampered.state_digest = AuthorityStateDigest::sha256_bytes(b"tampered");
        assert!(matches!(
            journal.validate_authority_checkpoint_marker(&tampered),
            Err(AgentError::RecoveryRequired(_))
        ));
        let mut future = ancestor.clone();
        future.last_seq = 99;
        assert!(matches!(
            journal.validate_authority_checkpoint_marker(&future),
            Err(AgentError::RecoveryRequired(_))
        ));
        let stable_id = ancestor.journal_id;
        drop(journal);

        let (reopened, _) = FileOperationJournal::open(&path).unwrap();
        assert_eq!(
            reopened.authority_checkpoint_marker().unwrap().journal_id,
            stable_id
        );
        reopened
            .validate_authority_checkpoint_marker(&ancestor)
            .unwrap();
    }

    #[test]
    fn compaction_folds_history_bumps_generation_and_preserves_exact_tip_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        let (journal, _) = FileOperationJournal::open(&path).unwrap();
        let empty = journal.compact().unwrap();
        assert_eq!(empty.generation, 1);
        assert_eq!(empty.last_seq, 0);

        let operation_id = OperationId::new();
        let effect_id = EffectId::new();
        let mut snapshot = operation_snapshot(operation_id, OperationState::Accepted);
        journal.append_and_sync(&upsert(snapshot.clone())).unwrap();
        let mid_prefix = journal.authority_checkpoint_marker().unwrap();
        snapshot.state = OperationState::Executing {
            effect_id: Some(effect_id),
        };
        journal.append_and_sync(&upsert(snapshot.clone())).unwrap();
        snapshot.state = OperationState::Terminal {
            effect_id: Some(effect_id),
            terminal: OperationTerminal::Applied {
                durability: agent_contracts::EffectDurability::Durable,
                evidence: None,
            },
        };
        journal.append_and_sync(&upsert(snapshot.clone())).unwrap();
        journal
            .append_and_sync(&OperationJournalTransition::EpochAdvanced { from: 1, to: 2 })
            .unwrap();
        journal
            .append_and_sync(&OperationJournalTransition::EpochAdvanced { from: 2, to: 3 })
            .unwrap();
        assert_eq!(journal.recover().unwrap().last_seq, 5);
        let tip = journal.authority_checkpoint_marker().unwrap();
        assert_eq!(tip.generation, 1);
        assert_eq!(tip.authority_epoch, 3);

        let compacted = journal.compact().unwrap();
        assert_eq!(compacted.generation, 2);
        assert_eq!(compacted.authority_epoch, 3);
        assert_eq!(compacted.last_seq, 2, "Compacted + one folded snapshot");
        assert_eq!(compacted.journal_id, tip.journal_id);
        let recovered = journal.recover().unwrap();
        assert_eq!(recovered.authority_epoch, 3);
        assert_eq!(recovered.operations.len(), 1);
        assert!(matches!(
            recovered.operations[0].state,
            OperationState::Terminal {
                effect_id: Some(id),
                terminal: OperationTerminal::Applied { .. },
            } if id == effect_id
        ));
        journal.validate_authority_checkpoint_marker(&tip).unwrap();
        journal
            .validate_authority_checkpoint_marker(&compacted)
            .unwrap();
        assert!(
            matches!(
                journal.validate_authority_checkpoint_marker(&mid_prefix),
                Err(AgentError::RecoveryRequired(_))
            ),
            "旧代际中间 prefix 在压缩后必须 fail-closed"
        );
        assert!(
            matches!(
                journal.append_and_sync(&OperationJournalTransition::Compacted {
                    previous: compacted.clone(),
                }),
                Err(AgentError::InvalidRequest(_))
            ),
            "Compacted 不能经 append_and_sync 写入"
        );

        drop(journal);
        assert!(!path.exists(), "generation-1 WAL 压缩后应删除");
        let gen2 = path.with_file_name("operations.jsonl.g2");
        assert!(gen2.exists(), "generation-2 WAL 必须存在");

        let (reopened, recovery) = FileOperationJournal::open(&path).unwrap();
        assert_eq!(recovery.last_seq, 2);
        assert_eq!(recovery.authority_epoch, 3);
        assert_eq!(recovery.operations.len(), 1);
        assert!(recovery.compacted_from.is_some());
        reopened.validate_authority_checkpoint_marker(&tip).unwrap();
        reopened
            .validate_authority_checkpoint_marker(&compacted)
            .unwrap();
        let fresh = OperationId::new();
        reopened
            .append_and_sync(&upsert(operation_snapshot(fresh, OperationState::Accepted)))
            .unwrap();
        let after = reopened.recover().unwrap();
        assert_eq!(after.last_seq, 3);
        assert_eq!(after.operations.len(), 2);
        assert_eq!(after.authority_epoch, 3);
    }

    #[test]
    fn authority_marker_rejects_foreign_lineage_and_corrupt_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.jsonl");
        let path_b = dir.path().join("b.jsonl");
        let (journal_a, _) = FileOperationJournal::open(&path_a).unwrap();
        let marker_a = journal_a.authority_checkpoint_marker().unwrap();
        let (journal_b, _) = FileOperationJournal::open(&path_b).unwrap();
        assert!(matches!(
            journal_b.validate_authority_checkpoint_marker(&marker_a),
            Err(AgentError::RecoveryRequired(_))
        ));
        drop(journal_a);
        fs::write(path_a.with_extension("meta.json"), b"{}").unwrap();
        assert!(matches!(
            FileOperationJournal::open(&path_a),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn authority_journal_never_reuses_metadata_when_the_wal_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        let (journal, _) = FileOperationJournal::open(&path).unwrap();
        drop(journal);
        fs::remove_file(&path).unwrap();

        assert!(matches!(
            FileOperationJournal::open(&path),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn operation_journal_repairs_only_a_torn_final_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        let operation_id = OperationId::new();
        {
            let (journal, _) = FileOperationJournal::open(&path).unwrap();
            journal
                .append_and_sync(&upsert(operation_snapshot(
                    operation_id,
                    OperationState::Accepted,
                )))
                .unwrap();
        }
        let good_len = fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"record":{"version":1"#)
            .unwrap();
        let (journal, recovery) = FileOperationJournal::open(&path).unwrap();
        assert!(recovery.truncated_tail);
        assert_eq!(recovery.last_seq, 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), good_len);
        drop(journal);

        let (_, second) = FileOperationJournal::open(&path).unwrap();
        assert!(!second.truncated_tail);
        assert_eq!(second.last_seq, 1);
    }

    #[test]
    fn operation_journal_fails_closed_on_middle_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        {
            let (journal, _) = FileOperationJournal::open(&path).unwrap();
            let operation_id = OperationId::new();
            let accepted = operation_snapshot(operation_id, OperationState::Accepted);
            journal.append_and_sync(&upsert(accepted.clone())).unwrap();
            journal
                .append_and_sync(&upsert(OperationSnapshot {
                    identity: accepted.identity,
                    state: OperationState::Terminal {
                        effect_id: None,
                        terminal: OperationTerminal::CancelledBeforeCommit,
                    },
                }))
                .unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        let first_newline = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        bytes[first_newline / 2] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            FileOperationJournal::open(&path),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn operation_journal_rejects_a_corrupt_complete_final_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        {
            let (journal, _) = FileOperationJournal::open(&path).unwrap();
            journal
                .append_and_sync(&upsert(operation_snapshot(
                    OperationId::new(),
                    OperationState::Accepted,
                )))
                .unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let checksum = bytes
            .windows(b"checksum".len())
            .position(|window| window == b"checksum")
            .expect("stored frame includes checksum");
        bytes[checksum + b"checksum".len() + 3] ^= 1;
        fs::write(&path, bytes).unwrap();

        assert!(matches!(
            FileOperationJournal::open(&path),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn operation_journal_never_writes_past_the_recovery_record_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        let (journal, _) = FileOperationJournal::open(&path).unwrap();
        journal.writer.lock().unwrap().next_seq = MAX_OPERATION_JOURNAL_RECOVERY_RECORDS as u64 + 1;
        let before = fs::metadata(&path).unwrap().len();

        assert!(matches!(
            journal.append_and_sync(&upsert(operation_snapshot(
                OperationId::new(),
                OperationState::Accepted,
            ))),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert_eq!(fs::metadata(&path).unwrap().len(), before);
    }

    #[test]
    fn operation_journal_rejects_effect_identity_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.jsonl");
        let (journal, _) = FileOperationJournal::open(&path).unwrap();
        let operation_id = OperationId::new();
        let accepted = operation_snapshot(operation_id, OperationState::Accepted);
        journal.append_and_sync(&upsert(accepted.clone())).unwrap();
        let reserved = agent_contracts::EffectId::new();
        journal
            .append_and_sync(&upsert(OperationSnapshot {
                identity: accepted.identity.clone(),
                state: OperationState::Executing {
                    effect_id: Some(reserved),
                },
            }))
            .unwrap();

        let before = fs::metadata(&path).unwrap().len();
        let error = journal
            .append_and_sync(&upsert(OperationSnapshot {
                identity: accepted.identity,
                state: OperationState::Terminal {
                    effect_id: Some(agent_contracts::EffectId::new()),
                    terminal: OperationTerminal::Applied {
                        durability: agent_contracts::EffectDurability::Durable,
                        evidence: None,
                    },
                },
            }))
            .unwrap_err();

        assert!(matches!(error, AgentError::RecoveryRequired(_)));
        assert_eq!(fs::metadata(&path).unwrap().len(), before);
    }

    fn envelope(run_id: RunId) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id,
            seq: 1,
            timestamp_ms: 0,
            event: RuntimeEvent::RunStarted,
        }
    }

    fn trace_lines(dir: &Path, run_id: RunId) -> usize {
        std::fs::read_to_string(trace_path(dir, run_id))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// `flush` is the durability barrier: because the command channel is
    /// FIFO, a successful flush guarantees every append sent before it has
    /// left the process (drained and flushed out of the `BufWriter`s), and
    /// the trace file reflects them.
    #[tokio::test]
    async fn flush_is_a_durability_barrier_over_prior_appends() {
        let dir = std::env::temp_dir().join(format!("journal-barrier-{}", RunId::new()));
        let journal = FileEventJournal::open(&dir).await.unwrap();
        let run = RunId::new();

        for _ in 0..8 {
            journal.append(&envelope(run)).await.unwrap();
        }
        // Nothing durable until the barrier: the file may not exist yet.
        assert!(trace_lines(&dir, run) <= 8);

        journal.flush().await.expect("barrier must succeed");
        assert_eq!(
            trace_lines(&dir, run),
            8,
            "the barrier must have written every prior append"
        );

        // Appends after the barrier are covered by the next one (FIFO).
        journal.append(&envelope(run)).await.unwrap();
        journal.flush().await.unwrap();
        assert_eq!(trace_lines(&dir, run), 9);

        drop(journal);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writer errors are sticky: once a write fails, every later barrier
    /// reports that failure (never cleared, never mistaken for a complete
    /// trace). A directory squatting on a trace path makes the writer's
    /// open fail on every platform (is-a-directory on unix,
    /// access-denied on windows).
    #[tokio::test]
    async fn writer_errors_are_sticky_at_the_next_barrier() {
        let dir = std::env::temp_dir().join(format!("journal-sticky-{}", RunId::new()));
        fs::create_dir_all(&dir).unwrap();
        let journal = FileEventJournal::open(&dir).await.unwrap();

        let good_run = RunId::new();
        journal.append(&envelope(good_run)).await.unwrap();

        // The next trace path is a directory: the writer cannot open it.
        let bad_run = RunId::new();
        fs::create_dir(trace_path(&dir, bad_run)).unwrap();
        journal.append(&envelope(bad_run)).await.unwrap();

        let first = journal
            .flush()
            .await
            .expect_err("the barrier must surface the write failure");
        assert!(
            first.to_string().contains("open trace"),
            "the failure must name the trace open, got: {first}"
        );
        let second = journal
            .flush()
            .await
            .expect_err("the error is sticky, not cleared");
        assert_eq!(
            second.to_string(),
            first.to_string(),
            "a later barrier must report the same sticky failure"
        );

        drop(journal);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Crash-immediately-after-commit shape: the barrier (flush) is the
    /// crash point. Events past the last barrier live only in the writer's
    /// userspace `BufWriter` and are invisible on disk — a process killed
    /// between barriers loses exactly that tail, never the flushed prefix.
    #[tokio::test]
    async fn events_are_not_durable_until_the_barrier() {
        let dir = std::env::temp_dir().join(format!("journal-crash-{}", RunId::new()));
        fs::create_dir_all(&dir).unwrap();
        let run = RunId::new();

        let journal = FileEventJournal::open(&dir).await.unwrap();
        journal.append(&envelope(run)).await.unwrap();
        journal.append(&envelope(run)).await.unwrap();
        // Barrier 1: two events are durable and visible on disk.
        journal.flush().await.unwrap();
        assert_eq!(trace_lines(&dir, run), 2);

        // Two more events ride the writer's buffer only: until the next
        // barrier they are not on disk, so a crash loses them while the
        // flushed prefix survives.
        journal.append(&envelope(run)).await.unwrap();
        journal.append(&envelope(run)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            trace_lines(&dir, run),
            2,
            "the buffered tail must be invisible on disk until the barrier"
        );

        // The next barrier makes them durable.
        journal.flush().await.unwrap();
        assert_eq!(trace_lines(&dir, run), 4);

        drop(journal);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
