use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::task::{Context, Poll};

use agent_contracts::{
    AgentError, AgentResult, ContentDigest, Effect, EffectDurability, EffectReceipt,
    OperationEffectContext, ResourceVersionOracle, RunId, artifact_owner_from_prefix,
    message_looks_like_not_found,
};
use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWrite;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

mod broker;
mod confined;
mod handles;
mod journal;
mod process_journal;
mod remote_journal;
mod runtime_facts;
mod storage_faults;

pub use agent_contracts::{ArtifactLocator, MAX_ARTIFACT_REFERENCE_BYTES};
pub use broker::WorkspaceOutputBroker;
pub use confined::{ConfinedDir, ConfinedFile};
pub use handles::{ArtifactStoreHandle, ConfinedWorkspaceHandle};
pub use journal::WorkspaceEffectRecovery;
pub use remote_journal::RemoteEffectAck;
pub use runtime_facts::capture_host_runtime_facts;
pub use storage_faults::StorageFaultPlan;

/// One record in the workspace change journal (`.focus-agent/changes.jsonl`).
///
/// Mutations are journaled as a three-phase transaction so a recovery tool
/// can tell exactly what happened: `MutationPrepared` (staged, target not
/// touched), then `MutationCommitted` (atomic rename landed) or
/// `MutationRolledBack` (staged file removed, target untouched). The
/// prepared record carries both content hashes, so a later recovery pass can
/// verify the target still matches what was committed. Kept bounded: old
/// content is captured only for small files so the journal stays reviewable
/// without duplicating the whole repository.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeRecord {
    MutationPrepared {
        tx_id: String,
        timestamp_ms: u64,
        tool: String,
        path: String,
        action: String,
        bytes_before: u64,
        bytes_after: u64,
        before_hash: String,
        after_hash: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_content: Option<String>,
    },
    MutationCommitted {
        tx_id: String,
        timestamp_ms: u64,
    },
    MutationRolledBack {
        tx_id: String,
        timestamp_ms: u64,
        reason: String,
    },
}

/// Old-content capture limit for `ChangeRecord::MutationPrepared` (bounded
/// journal).
pub const CHANGE_CAPTURE_LIMIT: usize = 256 * 1024;
/// Serialized JSONL frame ceiling. This covers worst-case JSON escaping of
/// the bounded old-content capture while keeping the indivisible append
/// latency and memory use finite.
const MAX_CHANGE_RECORD_BYTES: usize = 2 * 1024 * 1024;
/// Hard ceiling for one ordinary workspace mutation. Large raw results
/// belong in the artifact store; no builtin or capability handle may bypass
/// this boundary by calling `MutationTransaction::prepare` directly.
pub const MAX_MUTATION_BYTES: usize = 4 * 1024 * 1024;

/// FNV-1a 64-bit content hash: deterministic across runs and platforms, so
/// a journaled `before_hash`/`after_hash` can be re-derived later without a
/// hash dependency in the workspace crate.
fn content_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", hash)
}

/// Hash one already-open regular file without ever reading past the
/// workspace mutation ceiling. The handle, not a path, is the authority.
fn bounded_open_file_revision(
    file: &mut std::fs::File,
    max_bytes: usize,
) -> io::Result<(u64, ContentDigest)> {
    use std::io::{Read, Seek, SeekFrom};

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mutation target is not a regular file",
        ));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file is {} bytes; workspace mutations are limited to {max_bytes} bytes",
                metadata.len()
            ),
        ));
    }

    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > max_bytes as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file grew beyond the workspace mutation limit of {max_bytes} bytes"),
            ));
        }
        hasher.update(&buffer[..read]);
    }
    Ok((total, ContentDigest::from_bytes(hasher.finalize().into())))
}

/// 身份定位符到仓库内相对路径。URI 本身不再泄露 `.focus-agent` 路径。
fn locator_relative_path(locator: &ArtifactLocator) -> PathBuf {
    let mut path = PathBuf::from(".focus-agent");
    path.push("artifacts");
    path.push(locator.run_id().to_string());
    path.push(locator.owner());
    if let Some(digest) = locator.digest() {
        path.push(digest.to_string());
    } else {
        path.push(format!(
            ".tmp-{}",
            locator
                .staging_id()
                .expect("draft locators always carry a staging id")
        ));
    }
    path
}

/// 流式写入中的暂存制品。`locator()` 是 draft 身份；写完后必须 `seal_artifact`
/// 才得到带内容 digest 的不可变定位符。
pub struct ArtifactDraft {
    file: tokio::fs::File,
    hasher: Sha256,
    run_id: RunId,
    owner: String,
    staging_id: Uuid,
    staging_name: String,
}

impl ArtifactDraft {
    /// 仍在增长的 draft 定位符。完成写入前不得当作 completion 证据。
    pub fn locator(&self) -> ArtifactLocator {
        ArtifactLocator::draft(self.run_id, self.owner.clone(), self.staging_id)
            .expect("draft owner was validated at create")
    }
}

impl AsyncWrite for ArtifactDraft {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.file).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                this.hasher.update(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_shutdown(cx)
    }
}

/// 用同一个 pinned handle 流式哈希再 seek 回去，避免 TOCTOU 换文件。
async fn verify_sealed_digest(
    confined: ConfinedFile,
    expected: ContentDigest,
) -> AgentResult<ConfinedFile> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let display = confined.display().to_path_buf();
    let mut file = confined.into_tokio();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| AgentError::Io(format!("hash artifact '{}': {e}", display.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = ContentDigest::from_bytes(hasher.finalize().into());
    if actual != expected {
        return Err(AgentError::InvalidRequest(format!(
            "artifact content digest mismatch for {}",
            display.display()
        )));
    }
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(|e| AgentError::Io(format!("rewind artifact '{}': {e}", display.display())))?;
    let std_file = file.try_into_std().map_err(|_| {
        AgentError::Io(format!(
            "artifact handle still busy after digest verify: {}",
            display.display()
        ))
    })?;
    Ok(ConfinedFile::new(std_file, display))
}

/// Open a child directory under a pinned parent, creating it relative to the
/// same handle when absent. A concurrent legitimate creator is harmless; a
/// pre-planted link/reparse point is refused by the subsequent open.
fn open_or_create_child_dir(
    parent: &ConfinedDir,
    name: &std::ffi::OsStr,
    label: &str,
) -> AgentResult<ConfinedDir> {
    match parent.open_child_dir(name) {
        Ok(child) => return Ok(child),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(confined_io_error(
                &format!("open {label} dir"),
                &parent.display().join(name),
                error,
            ));
        }
    }
    match parent.create_child_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(confined_io_error(
                &format!("create {label} dir"),
                &parent.display().join(name),
                error,
            ));
        }
    }
    parent.open_child_dir(name).map_err(|error| {
        confined_io_error(
            &format!("open {label} dir"),
            &parent.display().join(name),
            error,
        )
    })
}

/// `fs::canonicalize` returns verbatim (`\\?\`) prefixed paths on Windows;
/// strip the marker so resolved paths stay display-friendly for tools while
/// remaining absolute and link-resolved.
fn normalize_canonical(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
        PathBuf::from(stripped)
    }
    #[cfg(not(windows))]
    {
        path
    }
}

#[derive(Debug, Default)]
struct MutationLockRegistry {
    locks: StdMutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl MutationLockRegistry {
    async fn acquire_key(&self, key: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Weak entries make the registry bounded by active/waiting
            // mutations rather than by every path ever edited.
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key.to_string(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}

struct MutationLeaseGroup {
    _guards: Vec<OwnedMutexGuard<()>>,
}

impl std::fmt::Debug for MutationLeaseGroup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MutationLeaseGroup")
            .field("paths", &self._guards.len())
            .finish()
    }
}

fn mutation_lock_key(relative: &str) -> String {
    let normalized = relative.replace('\\', "/");
    #[cfg(windows)]
    {
        // Windows workspace paths are normally case-insensitive. It is safe
        // to over-serialize a case-sensitive directory; under-serializing
        // aliases could permit two in-process writers to race.
        normalized.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    state_dir: PathBuf,
    effect_journal: Arc<journal::WorkspaceEffectJournal>,
    process_journal: Arc<process_journal::ProcessEffectJournal>,
    remote_journal: Arc<remote_journal::RemoteEffectJournal>,
    mutation_locks: Arc<MutationLockRegistry>,
    change_journal_lock: Arc<StdMutex<()>>,
    /// Inert unless a fixture arms a plan through
    /// [`Self::arm_storage_faults`]; default behavior never reads it.
    storage_faults: storage_faults::SharedFaultPlan,
}

/// One existing-file snapshot and the journal transaction that owns its
/// mutation lease. The exact bytes are read once after every batch lease is
/// acquired; callers transform those bytes and then consume the transaction
/// into `prepare`.
pub struct MutationSnapshot {
    transaction: MutationTransaction,
    bytes: Vec<u8>,
}

impl MutationSnapshot {
    pub fn relative_path(&self) -> &str {
        &self.transaction.relative
    }

    pub fn revision(&self) -> ContentDigest {
        self.transaction
            .before_revision
            .expect("existing regular-file snapshots always carry a revision")
    }

    pub fn into_parts(self) -> (MutationTransaction, Vec<u8>) {
        (self.transaction, self.bytes)
    }
}

impl Workspace {
    pub async fn open(root: impl AsRef<Path>) -> AgentResult<Self> {
        let root = normalize_canonical(
            fs::canonicalize(root.as_ref())
                .await
                .map_err(|e| AgentError::Io(format!("canonicalize workspace: {e}")))?,
        );
        let requested_state_dir = root.join(".focus-agent");
        match fs::create_dir(&requested_state_dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(AgentError::Io(format!(
                    "create state dir {}: {e}",
                    requested_state_dir.display()
                )));
            }
        }
        let state_dir = normalize_canonical(
            fs::canonicalize(&requested_state_dir)
                .await
                .map_err(|e| AgentError::Io(format!("canonicalize state dir: {e}")))?,
        );
        // Validate the state directory before creating anything beneath it.
        // `create_dir_all(requested_state_dir/artifacts)` would otherwise
        // follow a pre-planted symlink/junction and write outside the
        // workspace before we had a chance to inspect its canonical target.
        if state_dir == root || !state_dir.starts_with(&root) {
            return Err(AgentError::InvalidRequest(format!(
                "runtime state directory resolves outside its dedicated workspace location: {}",
                requested_state_dir.display()
            )));
        }
        let state_metadata = fs::metadata(&state_dir)
            .await
            .map_err(|e| AgentError::Io(format!("inspect state dir: {e}")))?;
        if !state_metadata.is_dir() {
            return Err(AgentError::InvalidRequest(format!(
                "runtime state path is not a directory: {}",
                requested_state_dir.display()
            )));
        }
        // Pin the runtime-state path before creating anything underneath it.
        // A path-based `create_dir_all(.focus-agent/artifacts)` could be
        // redirected if `.focus-agent` or `artifacts` were swapped for a
        // symlink/junction after the canonicalization above.
        let root_dir = ConfinedDir::open_root(&root)
            .map_err(|e| AgentError::Io(format!("open workspace root handle: {e}")))?;
        let state_handle = root_dir
            .open_child_dir(std::ffi::OsStr::new(".focus-agent"))
            .map_err(|e| confined_io_error("open runtime state dir", &state_dir, e))?;
        open_or_create_child_dir(
            &state_handle,
            std::ffi::OsStr::new("artifacts"),
            "artifacts",
        )?;
        let authority_existed = state_handle
            .open_child_dir(std::ffi::OsStr::new("authority"))
            .is_ok();
        let authority = open_or_create_child_dir(
            &state_handle,
            std::ffi::OsStr::new("authority"),
            "authority",
        )?;
        if !authority_existed {
            state_handle.sync_all().map_err(|error| {
                AgentError::Storage(format!("sync authority parent directory: {error}"))
            })?;
        }
        let effect_journal = Arc::new(journal::WorkspaceEffectJournal::open(authority)?);
        let process_authority = state_handle
            .open_child_dir(std::ffi::OsStr::new("authority"))
            .map_err(|error| {
                confined_io_error(
                    "reopen authority dir for process journal",
                    &state_dir.join("authority"),
                    error,
                )
            })?;
        let process_journal = Arc::new(process_journal::ProcessEffectJournal::open(
            process_authority,
        )?);
        let remote_authority = state_handle
            .open_child_dir(std::ffi::OsStr::new("authority"))
            .map_err(|error| {
                confined_io_error(
                    "reopen authority dir for remote journal",
                    &state_dir.join("authority"),
                    error,
                )
            })?;
        let remote_journal = Arc::new(remote_journal::RemoteEffectJournal::open(remote_authority)?);
        Ok(Self {
            root,
            state_dir,
            effect_journal,
            process_journal,
            remote_journal,
            mutation_locks: Arc::new(MutationLockRegistry::default()),
            change_journal_lock: Arc::new(StdMutex::new(())),
            storage_faults: StorageFaultPlan::shared(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Arm a portable storage-full fault plan on this instance. Only
    /// compiled into test builds (`test-faults`); passing `None` disarms
    /// every point again.
    #[cfg(feature = "test-faults")]
    pub fn arm_storage_faults(&self, plan: Option<StorageFaultPlan>) {
        *self
            .storage_faults
            .lock()
            .expect("storage fault plan poisoned") = plan;
    }

    async fn acquire_mutation_keys(&self, mut keys: Vec<String>) -> Arc<MutationLeaseGroup> {
        keys.sort();
        keys.dedup();
        let mut guards = Vec::with_capacity(keys.len());
        for key in keys {
            guards.push(self.mutation_locks.acquire_key(&key).await);
        }
        Arc::new(MutationLeaseGroup { _guards: guards })
    }

    /// Capture one bounded, exact snapshot for every existing target while
    /// holding their shared in-process mutation lease. All canonical lock
    /// keys are acquired in sorted order before any file is read, so two
    /// reversed multi-file batches cannot deadlock and every successful edit
    /// can transform the same bytes its transaction journaled.
    pub async fn begin_existing_mutations(
        &self,
        tool: &str,
        action: &str,
        relatives: &[String],
        max_snapshot_bytes: usize,
    ) -> AgentResult<Vec<MutationSnapshot>> {
        if relatives.is_empty() {
            return Err(AgentError::InvalidRequest(
                "existing mutation snapshot requires at least one target".into(),
            ));
        }
        if max_snapshot_bytes > MAX_MUTATION_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "mutation snapshot limit {max_snapshot_bytes} exceeds the workspace limit of {MAX_MUTATION_BYTES} bytes"
            )));
        }

        struct Target {
            target: PathBuf,
            target_name: std::ffi::OsString,
            relative: String,
            key: String,
        }

        let mut targets = Vec::with_capacity(relatives.len());
        let mut unique_keys = HashSet::with_capacity(relatives.len());
        for requested in relatives {
            let target = self.resolve_mutation(requested).await?;
            let target_name = target
                .file_name()
                .ok_or_else(|| {
                    AgentError::InvalidRequest(format!("no file name for {}", target.display()))
                })?
                .to_os_string();
            let relative = display_relative(&self.root, &target);
            let key = mutation_lock_key(&relative);
            if !unique_keys.insert(key.clone()) {
                return Err(AgentError::InvalidRequest(format!(
                    "mutation target appears more than once: {relative}"
                )));
            }
            targets.push(Target {
                target,
                target_name,
                relative,
                key,
            });
        }

        let lease_group = self
            .acquire_mutation_keys(targets.iter().map(|target| target.key.clone()).collect())
            .await;
        let mut snapshots = Vec::with_capacity(targets.len());
        for target in targets {
            let parent_rel = target
                .target
                .parent()
                .and_then(|parent| parent.strip_prefix(&self.root).ok())
                .unwrap_or_else(|| Path::new(""));
            let parent = self.confined_existing_parent(parent_rel).await?;
            let file = parent
                .open_existing(&target.target_name)
                .map_err(|error| confined_io_error("open", &target.target, error))?;
            let metadata = file.metadata().map_err(|error| {
                AgentError::Io(format!("metadata {}: {error}", target.target.display()))
            })?;
            if !metadata.is_file() {
                return Err(AgentError::InvalidRequest(format!(
                    "mutation target is not a regular file: {}",
                    target.relative
                )));
            }
            if metadata.len() > max_snapshot_bytes as u64 {
                return Err(AgentError::InvalidRequest(format!(
                    "file is {} bytes; mutation snapshot is limited to {max_snapshot_bytes} bytes",
                    metadata.len()
                )));
            }
            let original_permissions = Some(metadata.permissions());

            use tokio::io::AsyncReadExt;
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            tokio::fs::File::from_std(file)
                .take(max_snapshot_bytes as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| {
                    AgentError::Io(format!("read {}: {error}", target.target.display()))
                })?;
            if bytes.len() > max_snapshot_bytes {
                return Err(AgentError::InvalidRequest(format!(
                    "file grew beyond the mutation snapshot limit of {max_snapshot_bytes} bytes while it was read"
                )));
            }

            let before_revision = ContentDigest::sha256_bytes(&bytes);
            let old_content = (bytes.len() <= CHANGE_CAPTURE_LIMIT)
                .then(|| std::str::from_utf8(&bytes).ok().map(str::to_owned))
                .flatten();
            snapshots.push(MutationSnapshot {
                transaction: MutationTransaction {
                    workspace: self.clone(),
                    parent,
                    target_name: target.target_name,
                    target: target.target,
                    relative: target.relative,
                    tool: tool.to_string(),
                    action: action.to_string(),
                    bytes_before: bytes.len() as u64,
                    target_existed: true,
                    before_hash: content_hash(&bytes),
                    before_revision: Some(before_revision),
                    old_content,
                    original_permissions,
                    tx_id: Uuid::new_v4().to_string(),
                    lease_group: lease_group.clone(),
                    #[cfg(test)]
                    prepare_crash_point: None,
                },
                bytes,
            });
        }
        Ok(snapshots)
    }

    /// Reconcile one exact Core-issued operation/effect identity against the
    /// strict workspace authority journal and current files.
    pub fn reconcile_effect(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<WorkspaceEffectRecovery> {
        journal::reconcile_workspace_effect(self, context)
    }

    /// 在子进程 PID 可用后立刻记下 spawn，作为恢复证据。
    pub fn record_process_spawn(
        &self,
        context: &OperationEffectContext,
        pid: u32,
    ) -> AgentResult<()> {
        self.process_journal.record_spawned(context, pid)
    }

    /// 记下 wait 到的退出。没有对应 spawn 时是空操作，便于测试直调工具。
    pub fn record_process_exit(&self, pid: u32, exit_code: Option<i32>) -> AgentResult<()> {
        self.process_journal.record_exited(pid, exit_code)
    }

    /// 发送前先落下远程预约。带幂等键时，未应答的同一键不得再发。
    pub fn record_remote_reserved(
        &self,
        context: &OperationEffectContext,
        idempotency_key: Option<&str>,
    ) -> AgentResult<()> {
        self.remote_journal
            .record_reserved(context, idempotency_key)
    }

    /// 在字节离开本机之前记下 dispatched。崩溃后只能视为可能已发出。
    pub fn record_remote_dispatched(
        &self,
        effect_id: agent_contracts::EffectId,
    ) -> AgentResult<()> {
        self.remote_journal.record_dispatched(effect_id)
    }

    /// 记下对端应答。没有这条记录的 dispatched 调用恢复为 Ambiguous。
    pub fn record_remote_acked(
        &self,
        effect_id: agent_contracts::EffectId,
        ack: RemoteEffectAck,
    ) -> AgentResult<()> {
        self.remote_journal.record_acked(effect_id, ack)
    }

    /// Resolve a user-provided path without allowing absolute paths or `..`
    /// escape, and without letting symlinks, junctions or reparse points
    /// redirect the result outside the workspace root.
    ///
    /// The candidate is walked component by component from the (already
    /// canonical) root; every existing intermediate is canonicalized and its
    /// target verified to stay under the root, so a link anywhere along the
    /// path cannot smuggle the final result outside. Missing tail components
    /// are appended lexically afterwards, which keeps new-file writes working.
    ///
    /// Resolution returns a path *string*; for reads and mutations the
    /// caller should prefer `confined_open_read` / `begin_mutation`, which
    /// fuse validation and open into a directory-handle-relative descent so
    /// a link swap between validation and use cannot redirect the operation.
    pub async fn resolve_relative(&self, relative: impl AsRef<Path>) -> AgentResult<PathBuf> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }
        let clean = clean_relative(relative)?;
        self.confine(clean).await
    }

    /// Resolve a path for a mutation (write/edit): same confinement as
    /// `resolve_relative`, plus a hard rejection of the runtime state
    /// directory, so ordinary tools cannot overwrite traces, checkpoints,
    /// artifacts or the change journal.
    pub async fn resolve_mutation(&self, relative: impl AsRef<Path>) -> AgentResult<PathBuf> {
        let path = self.resolve_relative(relative).await?;
        if path == self.state_dir || path.starts_with(&self.state_dir) {
            return Err(AgentError::InvalidRequest(format!(
                "mutations inside the runtime state directory are not allowed: {}",
                display_relative(&self.root, &path)
            )));
        }
        Ok(path)
    }

    /// 校验身份定位符并返回仓库内相对路径，且必须属于 `run_id`。
    ///
    /// 最终打开仍走 `confined_open_read`。路径形 `artifact://.focus-agent/...`
    /// 不再被接受；sealed 定位符在打开时核验内容 digest。
    pub fn artifact_relative_path_for_run(
        &self,
        reference: &str,
        run_id: RunId,
    ) -> AgentResult<PathBuf> {
        let locator = ArtifactLocator::parse(reference)?;
        locator.ensure_run(run_id)?;
        Ok(locator_relative_path(&locator))
    }

    /// 打开当前 run 的制品。sealed 定位符会对流式哈希结果与 URI digest
    /// 比对；draft 只确认仍是普通文件。返回的字符串永远是规范身份拼写。
    pub async fn open_artifact_for_run(
        &self,
        reference: &str,
        run_id: RunId,
    ) -> AgentResult<(String, ConfinedFile)> {
        let locator = ArtifactLocator::parse(reference)?;
        locator.ensure_run(run_id)?;
        let relative = locator_relative_path(&locator);
        let confined = self.confined_open_read(&relative).await?;
        let metadata = confined.metadata().map_err(|e| {
            AgentError::Io(format!(
                "inspect artifact '{}': {e}",
                confined.display().display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(AgentError::InvalidRequest(format!(
                "artifact reference is not a regular file: {reference:?}"
            )));
        }
        if let Some(expected) = locator.digest() {
            let confined = verify_sealed_digest(confined, expected).await?;
            return Ok((locator.to_string(), confined));
        }
        Ok((locator.to_string(), confined))
    }

    async fn confine(&self, clean: PathBuf) -> AgentResult<PathBuf> {
        let mut base = self.root.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut missing_prefix = false;
        for part in clean.components() {
            // Once a component is missing, every remaining component belongs
            // to that lexical tail. Probing later names against the old base
            // can alias `missing/file.txt` to an existing root `file.txt`.
            if missing_prefix {
                tail.push(part.as_os_str().to_owned());
                continue;
            }
            let candidate = base.join(part);
            match fs::symlink_metadata(&candidate).await {
                Ok(_) => {
                    let canonical =
                        normalize_canonical(fs::canonicalize(&candidate).await.map_err(|e| {
                            AgentError::Io(format!("canonicalize {}: {e}", candidate.display()))
                        })?);
                    if !canonical.starts_with(&self.root) {
                        return Err(AgentError::InvalidRequest(format!(
                            "path resolves outside the workspace: {}",
                            clean.display()
                        )));
                    }
                    base = canonical;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tail.push(part.as_os_str().to_owned());
                    missing_prefix = true;
                }
                Err(e) => {
                    return Err(AgentError::Io(format!(
                        "inspect {}: {e}",
                        candidate.display()
                    )));
                }
            }
        }
        for part in tail {
            base.push(part);
        }
        Ok(base)
    }

    /// Open a workspace file for reading with validation and open fused
    /// into one directory-handle-relative descent (`openat` style): each
    /// component is opened relative to the already-open parent handle with
    /// link-following disabled, so a link swap between a validation pass
    /// and the open can never redirect the read outside the workspace.
    /// Metadata and content taken through the returned handle refer to the
    /// opened object even if its path is swapped afterwards.
    pub async fn confined_open_read(
        &self,
        relative: impl AsRef<Path>,
    ) -> AgentResult<ConfinedFile> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty() {
            return Err(AgentError::InvalidRequest(
                "cannot open the workspace root as a file".into(),
            ));
        }
        let clean = clean_relative(relative)?;
        let parts: Vec<std::ffi::OsString> = clean
            .components()
            .map(|c| c.as_os_str().to_owned())
            .collect();
        let (last, parents) = parts.split_last().ok_or_else(|| {
            AgentError::InvalidRequest("cannot open the workspace root as a file".into())
        })?;
        let mut dir = ConfinedDir::open_root(&self.root)
            .map_err(|e| AgentError::Io(format!("open workspace root handle: {e}")))?;
        for part in parents {
            let next_display = dir.display().join(part);
            dir = dir
                .open_child_dir(part)
                .map_err(|e| confined_io_error("open dir", &next_display, e))?;
        }
        let file_display = dir.display().join(last);
        let file = dir
            .open_existing(last)
            .map_err(|e| confined_io_error("open", &file_display, e))?;
        Ok(ConfinedFile::new(file, self.root.join(clean)))
    }

    /// SHA-256 hex of a workspace-relative file, matching `fs.read`'s
    /// content revision. Missing paths return `None`. Files above the
    /// canonical workspace mutation cap are skipped (`InvalidRequest`) so
    /// this never becomes an unbounded second full-file ingest path.
    pub async fn resource_revision(&self, key: &str) -> AgentResult<Option<String>> {
        const MAX_REVISION_BYTES: u64 = MAX_MUTATION_BYTES as u64;
        let path = agent_contracts::normalize_resource_path(key);
        if path.is_empty() {
            return Ok(None);
        }
        match self.confined_open_read(&path).await {
            Ok(confined) => {
                let metadata = confined.metadata().map_err(|error| {
                    AgentError::Io(format!(
                        "metadata {}: {error}",
                        confined.display().display()
                    ))
                })?;
                if metadata.len() > MAX_REVISION_BYTES {
                    return Err(AgentError::InvalidRequest(format!(
                        "file is {} bytes; revision lookup is capped at {MAX_REVISION_BYTES}",
                        metadata.len()
                    )));
                }
                use tokio::io::AsyncReadExt;
                let file = confined.into_tokio();
                let mut bytes = Vec::with_capacity(metadata.len() as usize);
                file.take(MAX_REVISION_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|error| {
                        AgentError::Io(format!("read {path} for revision: {error}"))
                    })?;
                if bytes.len() as u64 > MAX_REVISION_BYTES {
                    return Err(AgentError::InvalidRequest(format!(
                        "file grew beyond the revision lookup limit of {MAX_REVISION_BYTES} bytes while it was read"
                    )));
                }
                Ok(Some(ContentDigest::sha256_bytes(&bytes).to_string()))
            }
            Err(error) if revision_lookup_missing(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Open an already-existing mutation parent through one confined handle
    /// chain. File effects never create directory topology implicitly: a
    /// missing parent is refused before a transaction (and therefore before
    /// any authority intent, staged file, or review evidence) can exist.
    async fn confined_existing_parent(&self, relative: &Path) -> AgentResult<ConfinedDir> {
        let clean = clean_relative(relative)?;
        let mut dir = ConfinedDir::open_root(&self.root)
            .map_err(|e| AgentError::Io(format!("open workspace root handle: {e}")))?;
        let mut display = self.root.clone();
        for part in clean.components() {
            display.push(part.as_os_str());
            // The state directory is a trusted-core-owned region: no
            // mutation may descend into it (mirrors resolve_mutation).
            if display == self.state_dir || display.starts_with(&self.state_dir) {
                return Err(AgentError::InvalidRequest(format!(
                    "mutations inside the runtime state directory are not allowed: {}",
                    display_relative(&self.root, &display)
                )));
            }
            match dir.open_child_dir(part.as_os_str()) {
                Ok(child) => dir = child,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(AgentError::InvalidRequest(format!(
                        "mutation parent directory not found: {}; file mutations only create a file inside an existing parent directory",
                        display_relative(&self.root, &display)
                    )));
                }
                Err(e) => return Err(confined_io_error("open dir", &display, e)),
            }
        }
        Ok(dir)
    }

    /// Begin a journaled, atomic mutation. The target is confined like
    /// `resolve_mutation` and its old content is captured (bounded) as the
    /// journal backup before anything is written.
    ///
    /// The parent directory must already exist and is confined with validation
    /// and open fused into a directory-handle-relative descent. The staged
    /// temp file and final atomic replace both happen relative to the pinned
    /// handle, so a link swap after validation cannot redirect the write
    /// outside the workspace. The target file itself may be absent; for a
    /// Core-managed write it is staged only after authority preparation,
    /// inside that already-existing parent.
    pub async fn begin_mutation(
        &self,
        tool: &str,
        action: &str,
        relative: impl AsRef<Path>,
    ) -> AgentResult<MutationTransaction> {
        let target = self.resolve_mutation(relative).await?;
        let target_name = target.file_name().ok_or_else(|| {
            AgentError::InvalidRequest(format!("no file name for {}", target.display()))
        })?;
        let relative = display_relative(&self.root, &target);
        let lease_group = self
            .acquire_mutation_keys(vec![mutation_lock_key(&relative)])
            .await;
        let parent_rel = target
            .parent()
            .and_then(|p| p.strip_prefix(&self.root).ok())
            .unwrap_or_else(|| Path::new(""));
        let parent = self.confined_existing_parent(parent_rel).await?;

        let mut bytes_before = 0u64;
        let mut target_existed = false;
        let mut before_hash = content_hash(&[]);
        let mut before_revision = None;
        let mut old_content = None;
        let mut original_permissions = None;
        match parent.open_existing(target_name) {
            Ok(file) => {
                target_existed = true;
                let meta = file
                    .metadata()
                    .map_err(|e| AgentError::Io(format!("metadata {}: {e}", target.display())))?;
                bytes_before = meta.len();
                if meta.is_file() {
                    if meta.len() > MAX_MUTATION_BYTES as u64 {
                        return Err(AgentError::InvalidRequest(format!(
                            "file is {} bytes; workspace mutations are limited to {MAX_MUTATION_BYTES} bytes",
                            meta.len()
                        )));
                    }
                    original_permissions = Some(meta.permissions());

                    // Recovery identity and the optional backup are derived
                    // from the same pinned, bounded read. A file that grows
                    // while being read is rejected at MAX + 1 rather than
                    // turning begin_mutation into unbounded synchronous I/O.
                    let mut file = file;
                    use std::io::Read;
                    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                    let mut revision_hasher = Sha256::new();
                    let mut captured = if meta.len() <= CHANGE_CAPTURE_LIMIT as u64 {
                        Some(Vec::with_capacity(meta.len() as usize))
                    } else {
                        None
                    };
                    let mut buffer = [0_u8; 64 * 1024];
                    let mut total = 0_usize;
                    loop {
                        let remaining = (MAX_MUTATION_BYTES + 1).saturating_sub(total);
                        if remaining == 0 {
                            return Err(AgentError::InvalidRequest(format!(
                                "file grew beyond the workspace mutation limit of {MAX_MUTATION_BYTES} bytes while it was read"
                            )));
                        }
                        let read =
                            file.read(&mut buffer[..remaining.min(64 * 1024)])
                                .map_err(|e| {
                                    AgentError::Io(format!("read {}: {e}", target.display()))
                                })?;
                        if read == 0 {
                            break;
                        }
                        total += read;
                        if total > MAX_MUTATION_BYTES {
                            return Err(AgentError::InvalidRequest(format!(
                                "file grew beyond the workspace mutation limit of {MAX_MUTATION_BYTES} bytes while it was read"
                            )));
                        }
                        for byte in &buffer[..read] {
                            hash ^= u64::from(*byte);
                            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                        }
                        revision_hasher.update(&buffer[..read]);
                        if let Some(bytes) = &mut captured {
                            if bytes.len().saturating_add(read) <= CHANGE_CAPTURE_LIMIT {
                                bytes.extend_from_slice(&buffer[..read]);
                            } else {
                                // Metadata can become stale if an external
                                // writer grows the file while it is read.
                                // Never let the journal backup cross its
                                // allocation boundary in that race.
                                captured = None;
                            }
                        }
                    }
                    bytes_before = total as u64;
                    before_hash = format!("{hash:016x}");
                    before_revision =
                        Some(ContentDigest::from_bytes(revision_hasher.finalize().into()));
                    old_content = captured.and_then(|bytes| String::from_utf8(bytes).ok());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(confined_io_error("inspect", &target, e)),
        }
        Ok(MutationTransaction {
            workspace: self.clone(),
            parent,
            target_name: target_name.to_os_string(),
            target,
            relative,
            tool: tool.to_string(),
            action: action.to_string(),
            bytes_before,
            target_existed,
            before_hash,
            before_revision,
            old_content,
            original_permissions,
            tx_id: Uuid::new_v4().to_string(),
            lease_group,
            #[cfg(test)]
            prepare_crash_point: None,
        })
    }

    pub async fn write_artifact(
        &self,
        run_id: RunId,
        prefix: &str,
        _extension: &str,
        bytes: &[u8],
    ) -> AgentResult<String> {
        use tokio::io::AsyncWriteExt;

        let mut draft = self.create_artifact(run_id, prefix, _extension).await?;
        draft
            .write_all(bytes)
            .await
            .map_err(|e| AgentError::Io(format!("write artifact: {e}")))?;
        self.seal_artifact(draft).await
    }

    /// 打开仅用于写入的暂存文件。返回的 draft 在 `seal_artifact` 之前
    /// 不得作为不可变证据进入 completion。
    pub async fn create_artifact(
        &self,
        run_id: RunId,
        prefix: &str,
        _extension: &str,
    ) -> AgentResult<ArtifactDraft> {
        let owner = artifact_owner_from_prefix(prefix)?;
        let staging_id = Uuid::new_v4();
        let staging_name = format!(".tmp-{staging_id}");
        let owner_dir = self.artifact_owner_dir(run_id, &owner)?;
        let path = owner_dir.display().join(&staging_name);
        let file = owner_dir
            .create_new_file(std::ffi::OsStr::new(&staging_name))
            .map_err(|e| confined_io_error("create artifact", &path, e))?;
        Ok(ArtifactDraft {
            file: tokio::fs::File::from_std(file),
            hasher: Sha256::new(),
            run_id,
            owner,
            staging_id,
            staging_name,
        })
    }

    /// 把 draft 封成 owner/digest 身份：按写入哈希重命名，不再重读整文件。
    pub async fn seal_artifact(&self, mut draft: ArtifactDraft) -> AgentResult<String> {
        use tokio::io::AsyncWriteExt;

        draft
            .flush()
            .await
            .map_err(|e| AgentError::Io(format!("flush artifact: {e}")))?;
        let hasher = std::mem::replace(&mut draft.hasher, Sha256::new());
        let digest = ContentDigest::from_bytes(hasher.finalize().into());
        let locator = ArtifactLocator::sealed(draft.run_id, draft.owner.clone(), digest)?;
        let std_file = draft
            .file
            .try_into_std()
            .map_err(|_| AgentError::Io("artifact handle still busy at seal".into()))?;
        let owner_dir = self.artifact_owner_dir(draft.run_id, &draft.owner)?;
        let digest_name = std::ffi::OsString::from(digest.to_string());
        let staging_name = std::ffi::OsString::from(&draft.staging_name);
        owner_dir
            .replace_file(&std_file, &staging_name, &digest_name)
            .map_err(|e| {
                confined_io_error("seal artifact", &owner_dir.display().join(&digest_name), e)
            })?;
        Ok(locator.to_string())
    }

    /// 刷新 BufWriter 后再封口，给 shell/process 流式写入复用。
    pub async fn seal_buffered_artifact(
        &self,
        mut artifact: tokio::io::BufWriter<ArtifactDraft>,
    ) -> AgentResult<String> {
        use tokio::io::AsyncWriteExt;

        artifact
            .flush()
            .await
            .map_err(|e| AgentError::Io(format!("flush artifact: {e}")))?;
        // Tokio's BufWriter::into_inner returns the writer directly after flush.
        self.seal_artifact(artifact.into_inner()).await
    }

    /// Pin `.focus-agent/artifacts/<run>` and create the run directory
    /// relative to its already-open parent when needed. No path component is
    /// followed as a symlink/junction, and the returned handle remains the
    /// authority for the subsequent exclusive file creation.
    fn artifact_run_dir(&self, run_id: RunId) -> AgentResult<ConfinedDir> {
        let root = ConfinedDir::open_root(&self.root)
            .map_err(|e| AgentError::Io(format!("open workspace root handle: {e}")))?;
        let state = root
            .open_child_dir(std::ffi::OsStr::new(".focus-agent"))
            .map_err(|e| confined_io_error("open runtime state dir", &self.state_dir, e))?;
        let artifacts =
            open_or_create_child_dir(&state, std::ffi::OsStr::new("artifacts"), "artifacts")?;
        open_or_create_child_dir(
            &artifacts,
            std::ffi::OsStr::new(&run_id.to_string()),
            "artifact run",
        )
    }

    fn artifact_owner_dir(&self, run_id: RunId, owner: &str) -> AgentResult<ConfinedDir> {
        let run_dir = self.artifact_run_dir(run_id)?;
        open_or_create_child_dir(&run_dir, std::ffi::OsStr::new(owner), "artifact owner")
    }

    /// Append a mutation record to the workspace change journal
    /// (`.focus-agent/changes.jsonl`). Mutating tools call this so every
    /// write is visible and reviewable.
    pub async fn record_change(&self, change: ChangeRecord) -> AgentResult<()> {
        let mut line = serde_json::to_string(&change)
            .map_err(|e| AgentError::Storage(format!("serialize change: {e}")))?;
        line.push('\n');
        if line.len() > MAX_CHANGE_RECORD_BYTES {
            return Err(AgentError::Storage(format!(
                "change journal record is {} bytes; the limit is {MAX_CHANGE_RECORD_BYTES} bytes",
                line.len()
            )));
        }
        // This deliberately has no suspension point after taking the lock.
        // Dropping an async caller can therefore happen before a record or
        // after a complete JSON line, never halfway through `write_all`.
        // Mutation staging/rename already uses bounded synchronous confined
        // I/O; keeping this small audit append in the same indivisible poll
        // is cheaper and safer than an independently cancellable async write.
        let _journal_guard = self
            .change_journal_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let journal = self.state_dir.join("changes.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal)
            .map_err(|e| AgentError::Storage(format!("open change journal: {e}")))?;
        use std::io::Write;
        file.write_all(line.as_bytes())
            .map_err(|e| AgentError::Storage(format!("append change journal: {e}")))?;
        file.flush()
            .map_err(|e| AgentError::Storage(format!("flush change journal: {e}")))?;
        Ok(())
    }
}

/// Removes a newly-created staging entry unless ownership is explicitly
/// transferred to `PreparedMutation`. The guard is installed immediately
/// after exclusive creation, so every later error or unwind cleans the temp
/// before releasing the path lease.
struct StagedTempCleanup<'a> {
    parent: &'a ConfinedDir,
    temp_name: &'a std::ffi::OsStr,
    file: Option<std::fs::File>,
    armed: bool,
}

impl<'a> StagedTempCleanup<'a> {
    fn new(parent: &'a ConfinedDir, temp_name: &'a std::ffi::OsStr, file: std::fs::File) -> Self {
        Self {
            parent,
            temp_name,
            file: Some(file),
            armed: true,
        }
    }

    fn file_mut(&mut self) -> &mut std::fs::File {
        self.file
            .as_mut()
            .expect("staged cleanup owns the open file")
    }

    fn cleanup_now(&mut self) -> bool {
        let cleaned = cleanup_staged_file(self.parent, self.temp_name, self.file.as_ref());
        if cleaned {
            // Windows deletion is pending until this final handle closes.
            // Unix already unlinked the verified inode. Keep the handle on
            // failure so a retry never degrades into an unverified path-only
            // delete.
            self.file.take();
            self.armed = false;
        }
        cleaned
    }

    fn disarm_and_take_file(&mut self) -> std::fs::File {
        self.armed = false;
        self.file.take().expect("staged cleanup owns the open file")
    }
}

impl Drop for StagedTempCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.cleanup_now();
        }
    }
}

fn cleanup_staged_file(
    parent: &ConfinedDir,
    temp_name: &std::ffi::OsStr,
    open_file: Option<&std::fs::File>,
) -> bool {
    let removal = match open_file {
        Some(file) => parent.remove_open_file(file, temp_name),
        None => parent.remove_file(temp_name),
    };
    match removal {
        Ok(()) => parent.sync_all().is_ok(),
        // Without an open handle, NotFound proves there is no staging entry
        // left to clean. With a handle it can mean the inode was renamed to
        // an unknown name, so recovery must retain the uncertainty.
        Err(error) if open_file.is_none() && error.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// A single journaled, atomic file mutation, split into prepare and commit.
///
/// For Core-managed effects, `prepare` first persists an authority intent
/// that owns the deterministic staging name, then creates and syncs the
/// hidden file, and only then writes the review journal's
/// `MutationPrepared`. Thus every staging entry that can survive a crash is
/// already mapped by durable authority evidence. `commit` atomically renames
/// the staged file over the target and records `MutationCommitted`; a commit
/// failure rolls the staging back and records `MutationRolledBack`. The
/// runtime owns the commit/rollback decision behind its generation fence.
pub struct MutationTransaction {
    workspace: Workspace,
    /// Pinned parent directory handle: staging and the atomic replace both
    /// happen relative to this handle, never through a swappable path.
    parent: ConfinedDir,
    target_name: std::ffi::OsString,
    target: PathBuf,
    relative: String,
    tool: String,
    action: String,
    bytes_before: u64,
    target_existed: bool,
    /// Content hash of the real file at prepare time (computed even when the
    /// backup copy is truncated — recovery integrity must not depend on the
    /// capture limit).
    before_hash: String,
    /// SHA-256 of the regular file captured by `begin_mutation`. Tools use
    /// this to prove that the snapshot they transformed is the same bytes
    /// the transaction journaled; the prepared effect retains it for a
    /// commit-time compare-before-swap guard.
    before_revision: Option<ContentDigest>,
    old_content: Option<String>,
    /// Platform permissions copied to the replacement. On Unix this retains
    /// mode bits; on Windows it retains the readonly attribute expressible
    /// through `std::fs::Permissions` (not the complete ACL/attribute set).
    original_permissions: Option<std::fs::Permissions>,
    tx_id: String,
    /// Shared lease from snapshot acquisition through final commit,
    /// rollback, or drop. A multi-file batch gives every child the same
    /// group so no in-process writer can enter between sibling commits.
    lease_group: Arc<MutationLeaseGroup>,
    #[cfg(test)]
    prepare_crash_point: Option<PrepareCrashPoint>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrepareCrashPoint {
    IntentPersisted,
    StageSynced,
    ReviewRecorded,
}

impl MutationTransaction {
    /// Exact byte revision observed while beginning this transaction.
    /// `None` means the target did not exist or was not a regular file.
    pub fn before_revision(&self) -> Option<ContentDigest> {
        self.before_revision
    }

    /// Stage `content` without a Core effect identity. This is retained for
    /// trusted tests and maintenance callers, but it is not crash-recoverable:
    /// production model/capability writes must use
    /// [`Self::prepare_with_effect_context`].
    pub async fn prepare(self, content: &[u8]) -> AgentResult<PreparedMutation> {
        self.prepare_inner(content, None).await
    }

    #[cfg(test)]
    fn with_prepare_crash_point(mut self, point: PrepareCrashPoint) -> Self {
        self.prepare_crash_point = Some(point);
        self
    }

    pub async fn prepare_with_effect_context(
        self,
        content: &[u8],
        context: OperationEffectContext,
    ) -> AgentResult<PreparedMutation> {
        context.validate().map_err(AgentError::InvalidRequest)?;
        self.prepare_inner(content, Some(context)).await
    }

    async fn prepare_inner(
        mut self,
        content: &[u8],
        effect_context: Option<OperationEffectContext>,
    ) -> AgentResult<PreparedMutation> {
        if content.len() > MAX_MUTATION_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "workspace mutation is {} bytes; the limit is {MAX_MUTATION_BYTES} bytes",
                content.len()
            )));
        }
        // One identity owns both journal evidence and the reserved staging
        // name. For a Core-managed effect the durable authority intent must
        // land before create_new: a torn/failed authority append therefore
        // cannot leave a filesystem entry behind.
        let temp_name = std::ffi::OsString::from(format!(".fa-{}.tmp", self.tx_id));
        let after_hash = content_hash(content);
        let staged_revision = ContentDigest::sha256_bytes(content);
        let before_authority_hash = self
            .before_revision
            .unwrap_or_else(|| ContentDigest::sha256_bytes(&[]))
            .to_string();
        if let Some(context) = &effect_context {
            if let Some(plan) = storage_faults::active_plan(&self.workspace.storage_faults)
                && plan.refuse_prepare_intent_append
            {
                return Err(AgentError::Storage(
                    storage_faults::StorageFaultPlan::storage_full("authority intent append")
                        .to_string(),
                ));
            }
            self.workspace
                .effect_journal
                .append_prepared(journal::PreparedEvidence {
                    tx_id: self.tx_id.clone(),
                    context: context.clone(),
                    relative_target: self.relative.clone(),
                    temp_name: temp_name.to_string_lossy().into_owned(),
                    target_existed: self.target_existed,
                    before_hash: before_authority_hash,
                    after_hash: staged_revision.to_string(),
                    bytes_before: self.bytes_before,
                    bytes_after: content.len() as u64,
                })?;
        }

        #[cfg(test)]
        if self.prepare_crash_point == Some(PrepareCrashPoint::IntentPersisted) {
            return Err(AgentError::RecoveryRequired(
                "simulated crash after workspace authority intent".into(),
            ));
        }

        // The parent is already confined (with missing components created
        // through the pinned handle chain); the staged file is created
        // exclusively under that handle, so a link swap cannot redirect it.
        // If create_new fails, no entry owned by this transaction exists and
        // it must never remove the colliding name.
        let file = match self.parent.create_new_file(&temp_name) {
            Ok(file) => file,
            Err(error) => {
                let create_error = confined_io_error("create temp", &self.target, error);
                if effect_context.is_some()
                    && let Err(rollback_error) = self
                        .workspace
                        .effect_journal
                        .append_rolled_back(&self.tx_id)
                {
                    return Err(AgentError::RecoveryRequired(format!(
                        "{create_error}; close workspace authority intent: {rollback_error}"
                    )));
                }
                return Err(create_error);
            }
        };
        let mut staged_cleanup = StagedTempCleanup::new(&self.parent, &temp_name, file);

        let stage_result = (|| -> io::Result<()> {
            use std::io::Write;

            let file = staged_cleanup.file_mut();
            // A truncated stage leaves exactly what a full disk would:
            // partial bytes in the exclusively-created temp, which the
            // prepare path must clean up itself before rolling the intent
            // back.
            if let Some(budget) = storage_faults::active_plan(&self.workspace.storage_faults)
                .and_then(|plan| plan.stage_write_budget_bytes)
                && (budget as usize) < content.len()
            {
                let budget = budget as usize;
                file.write_all(&content[..budget])?;
                file.flush()?;
                return Err(storage_faults::StorageFaultPlan::storage_full(
                    "staged temp write",
                ));
            }
            file.write_all(content)?;
            file.flush()?;
            #[cfg(not(windows))]
            if let Some(permissions) = &self.original_permissions {
                file.set_permissions(permissions.clone())?;
            }
            file.sync_all()
        })();
        if let Err(error) = stage_result {
            if !staged_cleanup.cleanup_now() {
                return Err(AgentError::RecoveryRequired(format!(
                    "stage mutation failed ({error}); staged file cleanup could not be confirmed"
                )));
            }
            if effect_context.is_some()
                && let Err(rollback_error) = self
                    .workspace
                    .effect_journal
                    .append_rolled_back(&self.tx_id)
            {
                return Err(AgentError::RecoveryRequired(format!(
                    "stage mutation failed ({error}); close workspace authority intent: {rollback_error}"
                )));
            }
            return Err(AgentError::Io(format!("stage temp file: {error}")));
        }

        #[cfg(test)]
        if self.prepare_crash_point == Some(PrepareCrashPoint::StageSynced) {
            let file = staged_cleanup.disarm_and_take_file();
            drop(file);
            drop(staged_cleanup);
            return Err(AgentError::RecoveryRequired(
                "simulated crash after staged file sync".into(),
            ));
        }

        let record = ChangeRecord::MutationPrepared {
            tx_id: self.tx_id.clone(),
            timestamp_ms: now_ms(),
            tool: self.tool.clone(),
            path: self.relative.clone(),
            action: self.action.clone(),
            bytes_before: self.bytes_before,
            bytes_after: content.len() as u64,
            before_hash: self.before_hash.clone(),
            after_hash: after_hash.clone(),
            old_content: self.old_content.take(),
        };
        if let Err(error) = self.workspace.record_change(record).await {
            if !staged_cleanup.cleanup_now() {
                return Err(AgentError::RecoveryRequired(format!(
                    "record mutation prepare failed ({error}); staged file cleanup could not be confirmed"
                )));
            }
            if effect_context.is_some()
                && let Err(rollback_error) = self
                    .workspace
                    .effect_journal
                    .append_rolled_back(&self.tx_id)
            {
                return Err(AgentError::RecoveryRequired(format!(
                    "record mutation prepare failed ({error}); close workspace authority intent: {rollback_error}"
                )));
            }
            return Err(error);
        }

        #[cfg(test)]
        if self.prepare_crash_point == Some(PrepareCrashPoint::ReviewRecorded) {
            let file = staged_cleanup.disarm_and_take_file();
            drop(file);
            drop(staged_cleanup);
            return Err(AgentError::RecoveryRequired(
                "simulated crash after mutation review record".into(),
            ));
        }

        // From this point `PreparedMutation` owns both the name and cleanup
        // responsibility. Disarm before moving the pinned parent/name out of
        // the transaction.
        let file = staged_cleanup.disarm_and_take_file();
        drop(staged_cleanup);
        Ok(PreparedMutation {
            workspace: self.workspace,
            parent: self.parent,
            target: self.target,
            target_name: self.target_name,
            tx_id: self.tx_id,
            temp_name: Some(temp_name),
            temp_file: Some(file),
            effect_context,
            finished: false,
            relative_target: self.relative,
            staged_bytes: content.len() as u64,
            staged_revision,
            bytes_before: self.bytes_before,
            target_existed: self.target_existed,
            before_revision: self.before_revision,
            #[cfg(windows)]
            original_permissions: self.original_permissions,
            _lease_group: self.lease_group,
            #[cfg(test)]
            cleanup_failures_remaining: 0,
            #[cfg(test)]
            post_replace_corruption: None,
        })
    }

    /// Convenience for trusted, non-crash-recoverable maintenance paths.
    /// This carries no [`OperationEffectContext`], writes no authority intent,
    /// and bypasses the runtime generation fence; production model or
    /// capability effects must use `prepare_with_effect_context` instead.
    pub async fn apply(self, content: &[u8]) -> AgentResult<()> {
        let prepared = self.prepare(content).await?;
        match prepared.commit().await {
            // Apply collapses the structured receipt: the caller only needs
            // to know it failed and whether the world changed.
            EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                ..
            } => Ok(()),
            EffectReceipt::NotApplied { error } => Err(AgentError::Internal(error)),
            EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(error),
                ..
            } => Err(AgentError::Internal(format!(
                "mutation applied but its journal record failed: {error}"
            ))),
            EffectReceipt::Unknown { error } => Err(AgentError::Internal(format!(
                "mutation applied state unknown: {error}"
            ))),
        }
    }
}

/// A staged mutation whose commit is owned by the runtime: commit after the
/// generation fence passes, roll back when the operation turned stale.
pub struct PreparedMutation {
    workspace: Workspace,
    /// Pinned parent directory handle the staged file lives under.
    parent: ConfinedDir,
    target: PathBuf,
    target_name: std::ffi::OsString,
    tx_id: String,
    temp_name: Option<std::ffi::OsString>,
    /// The staged file handle, kept for the Windows atomic replace (which
    /// renames through the handle); Unix renames by name under the pinned
    /// directory.
    temp_file: Option<std::fs::File>,
    effect_context: Option<OperationEffectContext>,
    finished: bool,
    /// Canonical actual write target + real staged byte count, reported to
    /// Core for the commit-time Actual ⊆ Approved check (MOD-AUTH-02).
    relative_target: String,
    staged_bytes: u64,
    /// Expected SHA-256 of the exact staged bytes. Commit re-derives this
    /// from the still-open staging handle before any target replacement.
    staged_revision: ContentDigest,
    bytes_before: u64,
    target_existed: bool,
    before_revision: Option<ContentDigest>,
    #[cfg(windows)]
    original_permissions: Option<std::fs::Permissions>,
    /// Retained solely for its drop semantics; releasing the final child
    /// releases every sorted path lease held by the batch.
    _lease_group: Arc<MutationLeaseGroup>,
    #[cfg(test)]
    cleanup_failures_remaining: usize,
    #[cfg(test)]
    post_replace_corruption: Option<Vec<u8>>,
}

impl PreparedMutation {
    /// The journal transaction id of this staged mutation.
    pub fn tx_id(&self) -> &str {
        &self.tx_id
    }

    #[cfg(test)]
    fn simulate_process_exit(mut self) {
        // Test-only crash seam: preserve the staged file and authority
        // record while releasing all in-process handles/locks.
        self.finished = true;
    }

    /// Atomically replace the target and record `MutationCommitted`. On
    /// failure the staged file is removed and `MutationRolledBack` is
    /// recorded, so the journal reflects reality. The failure is
    /// structured: `NotApplied` when the replace never landed,
    /// `AppliedButDurabilityFailed` when the replace landed but the
    /// `MutationCommitted` record could not be appended — the caller must
    /// treat that as a degraded state, never as "nothing happened".
    ///
    /// The replace is relative to the pinned parent handle (`renameat` /
    /// `SetFileInformationByHandle` with `FILE_RENAME_INFO`), so a link
    /// swap cannot redirect it outside the workspace.
    pub async fn commit(mut self) -> EffectReceipt {
        let Some(temp_name) = self.temp_name.clone() else {
            return EffectReceipt::Unknown {
                error: "prepared mutation lost its staged temp identity".into(),
            };
        };
        match self.target_still_matches_snapshot() {
            Ok(true) => {}
            Ok(false) => {
                return self
                    .settle_not_applied(
                        &temp_name,
                        "stale_revision: target changed after mutation preflight; staged content was not applied"
                            .into(),
                    )
                    .await;
            }
            Err(error) => {
                let reason = format!(
                    "commit precondition could not inspect current target; staged content was not applied: {error}"
                );
                return self.settle_not_applied(&temp_name, reason).await;
            }
        }

        match self.staged_file_matches_expected(&temp_name) {
            Ok(true) => {}
            Ok(false) => {
                return self
                    .settle_not_applied(
                        &temp_name,
                        "staged_integrity: staged name, length, or content changed before commit"
                            .into(),
                    )
                    .await;
            }
            Err(error) => {
                return self
                    .settle_not_applied(
                        &temp_name,
                        format!("staged_integrity: could not verify staged content: {error}"),
                    )
                    .await;
            }
        }

        let from = self
            .temp_file
            .as_ref()
            .expect("prepare created the staged file");
        if let Err(e) = self
            .parent
            .replace_file(from, &temp_name, &self.target_name)
        {
            let reason = confined_io_error("commit", &self.target, e).to_string();
            return self
                .settle_not_applied(&temp_name, format!("commit failed: {reason}"))
                .await;
        }
        // The target changed; from here on any failure is a durability
        // problem, not a "did not apply" one.
        self.temp_name = None;
        self.finished = true;

        #[cfg(windows)]
        if let Some(permissions) = &self.original_permissions {
            let from = self
                .temp_file
                .as_ref()
                .expect("committed mutation retains its open handle");
            if let Err(error) = from
                .set_permissions(permissions.clone())
                .and_then(|()| from.sync_all())
            {
                return EffectReceipt::Applied {
                    durability: EffectDurability::DurabilityFailed(format!(
                        "restore committed mutation permissions: {error}"
                    )),
                    evidence: Some(self.tx_id.clone()),
                };
            }
        }

        #[cfg(test)]
        if let Some(content) = self.post_replace_corruption.take() {
            use std::io::{Seek, SeekFrom, Write};
            let from = self
                .temp_file
                .as_mut()
                .expect("committed mutation retains its open handle");
            let _ = from
                .set_len(0)
                .and_then(|()| from.seek(SeekFrom::Start(0)).map(|_| ()))
                .and_then(|()| from.write_all(&content))
                .and_then(|()| from.sync_all());
        }

        match self.committed_target_matches_staged() {
            Ok(true) => {}
            Ok(false) => {
                return EffectReceipt::Unknown {
                    error: "atomic replace returned success but the target does not contain the staged bytes"
                        .into(),
                };
            }
            Err(error) => {
                return EffectReceipt::Unknown {
                    error: format!(
                        "atomic replace returned success but committed target verification failed: {error}"
                    ),
                };
            }
        }

        if let Err(error) = self.parent.sync_all() {
            return EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(format!(
                    "sync committed mutation parent: {error}"
                )),
                evidence: Some(self.tx_id.clone()),
            };
        }
        if self.effect_context.is_some() {
            // The replace already landed; a full disk here is an applied-
            // but-not-durably-acknowledged outcome, never a rollback.
            if let Some(plan) = storage_faults::active_plan(&self.workspace.storage_faults)
                && plan.refuse_commit_record_append
            {
                return EffectReceipt::Applied {
                    durability: EffectDurability::DurabilityFailed(
                        "injected storage full during the committed-record append".into(),
                    ),
                    evidence: Some(self.tx_id.clone()),
                };
            }
            if let Err(error) = self.workspace.effect_journal.append_committed(&self.tx_id) {
                return EffectReceipt::Applied {
                    durability: EffectDurability::DurabilityFailed(error.to_string()),
                    evidence: Some(self.tx_id.clone()),
                };
            }
        }
        let record = ChangeRecord::MutationCommitted {
            tx_id: self.tx_id.clone(),
            timestamp_ms: now_ms(),
        };
        if let Err(error) = self.workspace.record_change(record).await {
            return EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(error.to_string()),
                evidence: Some(self.tx_id.clone()),
            };
        }
        match self.committed_target_matches_staged() {
            Ok(true) => {}
            Ok(false) => {
                return EffectReceipt::Applied {
                    durability: EffectDurability::DurabilityFailed(
                        "committed target changed before durable acknowledgement".into(),
                    ),
                    evidence: Some(self.tx_id.clone()),
                };
            }
            Err(error) => {
                return EffectReceipt::Applied {
                    durability: EffectDurability::DurabilityFailed(format!(
                        "final committed target verification failed: {error}"
                    )),
                    evidence: Some(self.tx_id.clone()),
                };
            }
        }
        EffectReceipt::Applied {
            durability: EffectDurability::Durable,
            evidence: Some(self.tx_id.clone()),
        }
    }

    /// Re-open through the pinned parent immediately before replace and
    /// compare the current bytes with the snapshot captured by
    /// `begin_mutation`. This detects drift already visible when the check
    /// begins and narrows the lost-update window; hash then rename is not an
    /// atomic filesystem CAS, so a simultaneous writer can still race it.
    fn target_still_matches_snapshot(&self) -> io::Result<bool> {
        match self.parent.open_existing(&self.target_name) {
            Ok(mut file) => {
                if !self.target_existed {
                    return Ok(false);
                }
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    return Ok(self.before_revision.is_none());
                }
                let (bytes, current) = bounded_open_file_revision(&mut file, MAX_MUTATION_BYTES)?;
                Ok(bytes == self.bytes_before && self.before_revision == Some(current))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(!self.target_existed),
            Err(error) => Err(error),
        }
    }

    fn staged_file_matches_expected(&mut self, temp_name: &std::ffi::OsStr) -> io::Result<bool> {
        let file = self.temp_file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "staged file handle is missing")
        })?;
        if !self.parent.named_entry_matches_file(temp_name, file)? {
            return Ok(false);
        }
        let (bytes, revision) = bounded_open_file_revision(file, MAX_MUTATION_BYTES)?;
        if !self.parent.named_entry_matches_file(temp_name, file)? {
            return Ok(false);
        }
        Ok(bytes == self.staged_bytes && revision == self.staged_revision)
    }

    fn committed_target_matches_staged(&mut self) -> io::Result<bool> {
        let file = self.temp_file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "committed file handle is missing")
        })?;
        if !self
            .parent
            .named_entry_matches_file(&self.target_name, file)?
        {
            return Ok(false);
        }
        let (bytes, revision) = bounded_open_file_revision(file, MAX_MUTATION_BYTES)?;
        if !self
            .parent
            .named_entry_matches_file(&self.target_name, file)?
        {
            return Ok(false);
        }
        Ok(bytes == self.staged_bytes && revision == self.staged_revision)
    }

    async fn settle_not_applied(
        &mut self,
        temp_name: &std::ffi::OsStr,
        reason: String,
    ) -> EffectReceipt {
        if !self.cleanup_staged(temp_name) {
            return EffectReceipt::Unknown {
                error: format!(
                    "{reason}; staged temp cleanup could not be confirmed and recovery is required"
                ),
            };
        }
        self.temp_name = None;
        self.finished = true;
        match self.record_rolled_back(reason.clone()).await {
            Ok(()) => EffectReceipt::NotApplied { error: reason },
            Err(error) => EffectReceipt::Unknown {
                error: format!(
                    "{reason}; rollback journal could not be confirmed and recovery is required: {error}"
                ),
            },
        }
    }

    /// Remove the staged file and record `MutationRolledBack`.
    /// Called when the owning operation is stale and the mutation must not
    /// land. The staging file is deleted here, not left behind for the
    /// `Drop` impl — a stale or cancelled mutation must never leak temp
    /// files.
    pub async fn rollback(mut self, reason: &str) -> AgentResult<()> {
        let cleaned = match self.temp_name.clone() {
            Some(temp_name) => self.cleanup_staged(&temp_name),
            None => true,
        };
        if !cleaned {
            return Err(AgentError::RecoveryRequired(format!(
                "workspace mutation {} staged temp cleanup could not be confirmed",
                self.tx_id
            )));
        }
        self.temp_name = None;
        self.finished = true;
        self.record_rolled_back(reason.to_string())
            .await
            .map_err(|error| {
                AgentError::RecoveryRequired(format!(
                    "workspace mutation {} rollback journal could not be confirmed: {error}",
                    self.tx_id
                ))
            })
    }

    fn cleanup_staged(&mut self, temp_name: &std::ffi::OsStr) -> bool {
        #[cfg(test)]
        if self.cleanup_failures_remaining > 0 {
            self.cleanup_failures_remaining -= 1;
            return false;
        }
        let cleaned = cleanup_staged_file(&self.parent, temp_name, self.temp_file.as_ref());
        if cleaned {
            self.temp_file.take();
        }
        cleaned
    }

    async fn record_rolled_back(&self, reason: String) -> AgentResult<()> {
        if self.effect_context.is_some() {
            self.workspace
                .effect_journal
                .append_rolled_back(&self.tx_id)?;
        }
        let record = ChangeRecord::MutationRolledBack {
            tx_id: self.tx_id.clone(),
            timestamp_ms: now_ms(),
            reason,
        };
        self.workspace.record_change(record).await
    }
}

impl Drop for PreparedMutation {
    fn drop(&mut self) {
        if !self.finished
            && let Some(temp_name) = self.temp_name.clone()
            && self.cleanup_staged(&temp_name)
        {
            self.temp_name = None;
            self.finished = true;
        }
    }
}

/// The contract seam for effect commit: a staged workspace mutation is an
/// `Effect` the runtime commits (after the generation fence) or rolls back
/// (stale operation). The runtime only ever sees the trait — it never knows
/// about temp files or the journal.
#[async_trait::async_trait]
impl Effect for PreparedMutation {
    fn describe(&self) -> String {
        format!("workspace mutation {}", self.tx_id)
    }

    fn actual_workspace_writes(&self) -> Option<Vec<agent_contracts::ActualWorkspaceWrite>> {
        // The real staged target and the real byte count — not an
        // approval-time estimate. Core compares these against the leased
        // intent's approved path set before committing (MOD-AUTH-02).
        Some(vec![agent_contracts::ActualWorkspaceWrite {
            path: self.relative_target.clone(),
            bytes: self.staged_bytes,
        }])
    }

    async fn commit(self: Box<Self>) -> EffectReceipt {
        (*self).commit().await
    }

    async fn rollback(self: Box<Self>, reason: &str) -> AgentResult<()> {
        (*self).rollback(reason).await
    }
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn revision_lookup_missing(error: &AgentError) -> bool {
    match error {
        AgentError::Io(message) | AgentError::InvalidRequest(message) => {
            message_looks_like_not_found(message)
        }
        _ => false,
    }
}

#[async_trait]
impl ResourceVersionOracle for Workspace {
    async fn revision(&self, key: &str) -> AgentResult<Option<String>> {
        self.resource_revision(key).await
    }
}

/// Lexically clean a user-provided workspace-relative path: no absolute
/// prefixes, no `..`, no `.` segments. Shared by the path-string resolver
/// and the directory-handle-relative confined operations.
fn clean_relative(relative: &Path) -> AgentResult<PathBuf> {
    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(AgentError::InvalidRequest(format!(
                    "path must stay inside workspace: {}",
                    relative.display()
                )));
            }
        }
    }
    Ok(clean)
}

/// Map a confined-filesystem error onto the agent error space: a reparse
/// rejection is a policy violation (`InvalidRequest`), anything else is an
/// IO failure.
fn confined_io_error(operation: &str, path: &Path, e: io::Error) -> AgentError {
    if e.kind() == io::ErrorKind::InvalidData {
        AgentError::InvalidRequest(format!("{operation} {}: {e}", path.display()))
    } else if e.kind() == io::ErrorKind::NotFound {
        // Custom Windows NTSTATUS payloads display as `NTSTATUS 0xc0000034`
        // without the words "not found"; keep the kind in the agent string
        // so ordinary file tools can return a typed path_not_found hint.
        AgentError::Io(format!("{operation} {}: not found ({e})", path.display()))
    } else {
        AgentError::Io(format!("{operation} {}: {e}", path.display()))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn try_make_link(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (link, target);
            false
        }
    }

    async fn staged_temp_count(parent: &Path) -> usize {
        let mut count = 0usize;
        let mut entries = fs::read_dir(parent).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            if entry.file_name().to_string_lossy().ends_with(".tmp") {
                count += 1;
            }
        }
        count
    }

    #[tokio::test]
    async fn rejects_parent_escape_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        assert!(workspace.resolve_relative("../x").await.is_err());
        assert!(workspace.resolve_relative("a/../../x").await.is_err());
        // "Absolute" is platform-defined: `C:\x` and `\x` are absolute on
        // Windows but legal relative filenames on Unix, where `/x` is the
        // absolute form.
        #[cfg(windows)]
        {
            assert!(workspace.resolve_relative("C:\\x").await.is_err());
            assert!(workspace.resolve_relative("\\x").await.is_err());
        }
        #[cfg(not(windows))]
        {
            assert!(workspace.resolve_relative("/x").await.is_err());
            assert!(workspace.resolve_relative("//x").await.is_err());
        }
    }

    #[tokio::test]
    async fn resource_revision_matches_sha256_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/util.py"), b"hello revision\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let digest = workspace
            .resource_revision("src/util.py")
            .await
            .unwrap()
            .expect("file exists");
        assert_eq!(
            digest,
            ContentDigest::sha256_bytes(b"hello revision\n").to_string()
        );
        assert_eq!(
            workspace.resource_revision("src/missing.py").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn resource_revision_uses_the_workspace_mutation_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let at_limit = vec![b'x'; MAX_MUTATION_BYTES];
        std::fs::write(dir.path().join("at-limit.txt"), &at_limit).unwrap();
        std::fs::write(
            dir.path().join("over-limit.txt"),
            vec![b'x'; MAX_MUTATION_BYTES + 1],
        )
        .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();

        assert_eq!(
            workspace.resource_revision("at-limit.txt").await.unwrap(),
            Some(ContentDigest::sha256_bytes(&at_limit).to_string())
        );
        assert!(matches!(
            workspace.resource_revision("over-limit.txt").await,
            Err(AgentError::InvalidRequest(message))
                if message.contains(&MAX_MUTATION_BYTES.to_string())
        ));
    }

    #[tokio::test]
    async fn resolves_clean_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let resolved = workspace.resolve_relative("src/lib.rs").await.unwrap();
        // The workspace root is canonicalized on open; on Windows that
        // expands 8.3 short names (e.g. RUNNER~1 -> runneradmin) that
        // `tempdir().path()` still reports, so compare against the
        // canonicalized root instead of the raw tempdir path.
        let canonical_root = normalize_canonical(std::fs::canonicalize(dir.path()).unwrap());
        assert_eq!(resolved, canonical_root.join("src/lib.rs"));
        let root = workspace.resolve_relative(".").await.unwrap();
        assert_eq!(root, workspace.root());
    }

    #[tokio::test]
    async fn missing_prefix_keeps_its_entire_lexical_tail() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "root file")
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();

        let resolved = workspace
            .resolve_relative("missing/file.txt")
            .await
            .unwrap();
        assert_eq!(resolved, workspace.root().join("missing/file.txt"));

        // Directory topology is established explicitly; the file mutation
        // only proves the missing lexical prefix did not alias the root file.
        fs::create_dir(workspace.root().join("missing"))
            .await
            .unwrap();
        workspace
            .begin_mutation("fs.write", "write", "missing/file.txt")
            .await
            .unwrap()
            .apply(b"nested file")
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.root().join("file.txt"))
                .await
                .unwrap(),
            "root file",
            "a missing prefix must never collapse onto an existing root file"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("missing/file.txt"))
                .await
                .unwrap(),
            "nested file"
        );
    }

    #[tokio::test]
    async fn open_rejects_state_dir_link_outside_workspace_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state_link = dir.path().join(".focus-agent");
        if !try_make_link(&state_link, outside.path()) {
            return;
        }

        let result = Workspace::open(dir.path()).await;
        assert!(
            result.is_err(),
            "a pre-planted state-dir link must not escape the workspace"
        );
        assert!(
            !outside.path().join("artifacts").exists(),
            "open must validate the state-dir target before creating children"
        );
    }

    #[tokio::test]
    async fn artifact_write_rejects_a_replaced_artifact_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let artifacts = workspace.state_dir().join("artifacts");
        fs::remove_dir(&artifacts).await.unwrap();
        if !try_make_link(&artifacts, outside.path()) {
            return;
        }

        let result = workspace
            .write_artifact(RunId::new(), "escape", "txt", b"must stay confined")
            .await;
        assert!(
            result.is_err(),
            "a replaced artifact directory must be rejected, not followed"
        );
        assert!(
            fs::read_dir(outside.path())
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none(),
            "artifact bytes must never land through the replacement link"
        );
    }

    #[tokio::test]
    async fn symlink_escape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let link = dir.path().join("link");
        if !try_make_link(&link, outside.path()) {
            return; // platform cannot create links here; nothing to assert
        }
        let result = workspace.resolve_relative("link/secret.txt").await;
        assert!(result.is_err(), "link must not escape the workspace");
    }

    #[tokio::test]
    async fn symlinked_state_dir_mutation_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let link = dir.path().join("state-link");
        if !try_make_link(&link, workspace.state_dir()) {
            return;
        }
        let result = workspace
            .begin_mutation("fs.write", "write", "state-link/x.txt")
            .await;
        assert!(result.is_err(), "state dir must not be reachable via links");
    }

    #[tokio::test]
    async fn mutation_inside_state_dir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        assert!(
            workspace
                .begin_mutation("fs.write", "write", ".focus-agent/x.txt")
                .await
                .is_err()
        );
        assert!(
            workspace
                .begin_mutation("fs.write", "write", ".focus-agent")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn transaction_applies_journaled_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("new.txt").await.unwrap();

        let tx = workspace
            .begin_mutation("fs.write", "write", "new.txt")
            .await
            .unwrap();
        tx.apply(b"hello").await.unwrap();
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "hello");

        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let prepared: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(prepared["kind"], "mutation_prepared");
        assert_eq!(prepared["tool"], "fs.write");
        assert_eq!(prepared["action"], "write");
        assert_eq!(prepared["bytes_before"], 0);
        assert_eq!(prepared["bytes_after"], 5);
        assert!(
            prepared["before_hash"].as_str().unwrap().len() == 16,
            "before hash must be present"
        );
        let committed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(committed["kind"], "mutation_committed");
        assert_eq!(committed["tx_id"], prepared["tx_id"]);
        assert_eq!(
            prepared["after_hash"],
            content_hash(b"hello"),
            "the journaled hash must re-derive from the written content"
        );

        // Overwriting captures the old content in the journal backup.
        let tx = workspace
            .begin_mutation("fs.write", "write", "new.txt")
            .await
            .unwrap();
        tx.apply(b"world").await.unwrap();
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "world");

        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let record: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(record["kind"], "mutation_prepared");
        assert_eq!(record["bytes_before"], 5);
        assert_eq!(record["old_content"], "hello");
        assert_eq!(record["before_hash"], content_hash(b"hello"));
        assert_eq!(record["after_hash"], content_hash(b"world"));
    }

    #[tokio::test]
    async fn concurrent_change_records_remain_complete_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let mut tasks = Vec::new();
        for index in 0..32 {
            let workspace = workspace.clone();
            tasks.push(tokio::spawn(async move {
                let tx_id = format!("concurrent-{index}");
                workspace
                    .record_change(ChangeRecord::MutationPrepared {
                        tx_id: tx_id.clone(),
                        timestamp_ms: index,
                        tool: "test".into(),
                        path: format!("file-{index}.txt"),
                        action: "write".into(),
                        bytes_before: 0,
                        bytes_after: 16 * 1024,
                        before_hash: content_hash(&[]),
                        after_hash: content_hash(b"after"),
                        // Make each append large enough to expose split-write
                        // interleaving in the absence of serialization.
                        old_content: Some("x".repeat(16 * 1024)),
                    })
                    .await
                    .unwrap();
                tokio::task::yield_now().await;
                workspace
                    .record_change(ChangeRecord::MutationCommitted {
                        tx_id,
                        timestamp_ms: index + 1,
                    })
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let mut transitions: HashMap<String, Vec<String>> = HashMap::new();
        for line in journal.lines() {
            let record: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("journal line is not standalone JSON: {error}"));
            transitions
                .entry(record["tx_id"].as_str().unwrap().to_string())
                .or_default()
                .push(record["kind"].as_str().unwrap().to_string());
        }
        assert_eq!(transitions.len(), 32);
        for kinds in transitions.values() {
            assert_eq!(
                kinds,
                &["mutation_prepared", "mutation_committed"],
                "each transaction must retain its own ordered terminal pair"
            );
        }
    }

    #[tokio::test]
    async fn prepared_mutation_rolls_back_without_touching_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("notes.txt").await.unwrap();
        fs::write(&target, "original").await.unwrap();

        let tx = workspace
            .begin_mutation("fs.write", "write", "notes.txt")
            .await
            .unwrap();
        let prepared = tx.prepare(b"staged but rolled back").await.unwrap();
        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "original",
            "prepare must not touch the target"
        );
        prepared.rollback("stale operation").await.unwrap();

        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "original",
            "rollback must leave the target untouched"
        );
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let prepared_record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let rolled_back: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(rolled_back["kind"], "mutation_rolled_back");
        assert_eq!(rolled_back["tx_id"], prepared_record["tx_id"]);
        assert_eq!(rolled_back["reason"], "stale operation");
    }

    #[tokio::test]
    async fn failed_apply_leaves_target_untouched_and_records_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("dir/file.txt").await.unwrap();
        fs::create_dir_all(target.parent().unwrap()).await.unwrap();
        fs::write(&target, "original").await.unwrap();

        // Renaming over a non-empty directory is an error on every platform;
        // the target file must stay intact and the temp file must be cleaned up.
        let blocked = dir.path().join("dir");
        fs::create_dir(blocked.join("child")).await.unwrap();
        let tx = workspace
            .begin_mutation("fs.write", "write", "dir")
            .await
            .unwrap();
        let result = tx.apply(b"x").await;
        assert!(result.is_err(), "dir target must fail to commit");
        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "original",
            "unrelated target must be untouched"
        );

        // The journal must tell the truth: prepared, then rolled back.
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let prepared: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(prepared["kind"], "mutation_prepared");
        let rolled_back: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(rolled_back["kind"], "mutation_rolled_back");
        assert_eq!(rolled_back["tx_id"], prepared["tx_id"]);
    }

    #[tokio::test]
    async fn rollback_deletes_the_staged_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("notes.txt").await.unwrap();
        fs::write(&target, "original").await.unwrap();

        let tx = workspace
            .begin_mutation("fs.write", "write", "notes.txt")
            .await
            .unwrap();
        let prepared = tx.prepare(b"staged").await.unwrap();

        let parent = target.parent().unwrap();
        let temp_count = || async {
            let mut count = 0usize;
            let mut entries = fs::read_dir(parent).await.unwrap();
            while let Some(entry) = entries.next_entry().await.unwrap() {
                if entry.file_name().to_string_lossy().ends_with(".tmp") {
                    count += 1;
                }
            }
            count
        };
        assert_eq!(temp_count().await, 1, "staging must leave one temp file");

        // A stale/cancelled mutation rolls back — and the staging file must
        // be gone, not leaked for the Drop impl to clean up later.
        prepared.rollback("stale operation").await.unwrap();
        assert_eq!(temp_count().await, 0, "rollback must delete the temp file");
        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "original",
            "the target is untouched"
        );
    }

    #[tokio::test]
    async fn change_journal_append_completes_in_one_poll_as_one_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let future = workspace.record_change(ChangeRecord::MutationRolledBack {
            tx_id: "one-poll".into(),
            timestamp_ms: 1,
            reason: "test".into(),
        });
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(
            matches!(
                std::future::Future::poll(future.as_mut(), &mut context),
                std::task::Poll::Ready(Ok(()))
            ),
            "the append must not expose a cancellable partial-write await"
        );

        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines = journal.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(record["tx_id"], "one-poll");
    }

    #[tokio::test]
    async fn change_journal_refuses_an_oversized_serialized_record() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let error = workspace
            .record_change(ChangeRecord::MutationRolledBack {
                tx_id: "oversized".into(),
                timestamp_ms: 1,
                reason: "x".repeat(MAX_CHANGE_RECORD_BYTES + 1),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Storage(message) if message.contains("limit")));
        assert!(!workspace.state_dir().join("changes.jsonl").exists());
    }

    #[tokio::test]
    async fn rollback_cleanup_failure_keeps_temp_name_for_drop_retry() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("notes.txt").await.unwrap();
        fs::write(&target, "original").await.unwrap();

        let mut prepared = workspace
            .begin_mutation("fs.write", "write", "notes.txt")
            .await
            .unwrap()
            .prepare(b"staged")
            .await
            .unwrap();
        prepared.cleanup_failures_remaining = 1;
        let error = prepared
            .rollback("injected cleanup failure")
            .await
            .unwrap_err();
        assert!(
            matches!(error, AgentError::RecoveryRequired(message) if message.contains("cleanup could not be confirmed"))
        );

        assert_eq!(
            staged_temp_count(dir.path()).await,
            0,
            "rollback must leave the name armed so Drop can retry cleanup"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "original");
    }

    #[tokio::test]
    async fn rollback_journal_failure_is_reported_after_confirmed_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("notes.txt").await.unwrap();
        fs::write(&target, "original").await.unwrap();

        let prepared = workspace
            .begin_mutation("fs.write", "write", "notes.txt")
            .await
            .unwrap()
            .prepare(b"staged")
            .await
            .unwrap();
        let error = prepared
            .rollback(&"x".repeat(MAX_CHANGE_RECORD_BYTES + 1))
            .await
            .unwrap_err();

        assert!(
            matches!(error, AgentError::RecoveryRequired(message) if message.contains("rollback journal could not be confirmed"))
        );
        assert_eq!(
            staged_temp_count(dir.path()).await,
            0,
            "journal failure occurs only after staged cleanup is confirmed"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "original");
    }

    #[tokio::test]
    async fn commit_cleanup_failure_keeps_temp_name_for_drop_retry() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("notes.txt").await.unwrap();
        fs::write(&target, "base").await.unwrap();

        let mut prepared = workspace
            .begin_mutation("edit.patch", "patch", "notes.txt")
            .await
            .unwrap()
            .prepare(b"agent")
            .await
            .unwrap();
        prepared.cleanup_failures_remaining = 1;
        fs::write(&target, "external").await.unwrap();

        let receipt = prepared.commit().await;
        assert!(
            matches!(receipt, EffectReceipt::Unknown { ref error }
                if error.contains("stale_revision") && error.contains("recovery is required")),
            "cleanup uncertainty must fence the operation for recovery: {receipt:?}"
        );
        assert_eq!(
            staged_temp_count(dir.path()).await,
            0,
            "commit must leave the name armed so Drop can retry cleanup"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "external");
    }

    #[tokio::test]
    async fn staged_temp_name_is_fixed_and_short() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(dir.path().join("descriptive-target-name.txt"), "before")
            .await
            .unwrap();

        let prepared = workspace
            .begin_mutation("fs.write", "write", "descriptive-target-name.txt")
            .await
            .unwrap()
            .prepare(b"after")
            .await
            .unwrap();
        let temp_name = prepared
            .temp_name
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(temp_name, format!(".fa-{}.tmp", prepared.tx_id));
        assert!(temp_name.starts_with(".fa-"));
        assert!(temp_name.ends_with(".tmp"));
        assert!(temp_name.len() <= 48, "unexpected temp name: {temp_name}");
        assert!(!temp_name.contains("descriptive-target-name"));

        prepared.rollback("test complete").await.unwrap();
    }

    #[tokio::test]
    async fn commit_rehashes_the_open_staged_file_before_replace() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = dir.path().join("notes.txt");
        fs::write(&target, "before").await.unwrap();

        let mut prepared = workspace
            .begin_mutation("edit.replace", "replace", "notes.txt")
            .await
            .unwrap()
            .prepare(b"approved bytes")
            .await
            .unwrap();
        let staged = prepared.temp_file.as_mut().unwrap();
        staged.set_len(0).unwrap();
        staged.seek(SeekFrom::Start(0)).unwrap();
        staged.write_all(b"tampered bytes").unwrap();
        staged.sync_all().unwrap();

        let receipt = prepared.commit().await;
        assert!(
            matches!(receipt, EffectReceipt::NotApplied { ref error }
                if error.contains("staged_integrity")),
            "staged tampering must be rejected before replace: {receipt:?}"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "before");
        assert_eq!(staged_temp_count(dir.path()).await, 0);
    }

    #[tokio::test]
    async fn post_replace_verification_never_reports_durable_for_wrong_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = dir.path().join("notes.txt");
        fs::write(&target, "before").await.unwrap();

        let mut prepared = workspace
            .begin_mutation("fs.write", "write", "notes.txt")
            .await
            .unwrap()
            .prepare(b"approved bytes")
            .await
            .unwrap();
        prepared.post_replace_corruption = Some(b"wrong bytes".to_vec());

        let receipt = prepared.commit().await;
        assert!(
            matches!(receipt, EffectReceipt::Unknown { ref error }
                if error.contains("does not contain the staged bytes")),
            "a mismatched committed target must require recovery: {receipt:?}"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "wrong bytes");
    }

    #[tokio::test]
    async fn rollback_journal_uncertainty_returns_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = dir.path().join("notes.txt");
        fs::write(&target, "base").await.unwrap();

        let prepared = workspace
            .begin_mutation("edit.patch", "patch", "notes.txt")
            .await
            .unwrap()
            .prepare(b"agent")
            .await
            .unwrap();
        let journal = workspace.state_dir().join("changes.jsonl");
        fs::remove_file(&journal).await.unwrap();
        fs::create_dir(&journal).await.unwrap();
        fs::write(&target, "external").await.unwrap();

        let receipt = prepared.commit().await;
        assert!(
            matches!(receipt, EffectReceipt::Unknown { ref error }
                if error.contains("rollback journal") && error.contains("recovery")),
            "a missing rollback terminal must not report a settled non-application: {receipt:?}"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "external");
        assert_eq!(staged_temp_count(dir.path()).await, 0);
        fs::remove_dir(&journal).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn substituted_unix_temp_name_is_never_renamed_or_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = dir.path().join("notes.txt");
        fs::write(&target, "before").await.unwrap();

        let prepared = workspace
            .begin_mutation("fs.write", "write", "notes.txt")
            .await
            .unwrap()
            .prepare(b"approved")
            .await
            .unwrap();
        let temp_name = prepared.temp_name.as_ref().unwrap().clone();
        let temp_path = dir.path().join(&temp_name);
        let moved_path = dir.path().join("moved-stage");
        fs::rename(&temp_path, &moved_path).await.unwrap();
        fs::write(&temp_path, "substitute").await.unwrap();

        let receipt = prepared.commit().await;
        assert!(
            matches!(receipt, EffectReceipt::Unknown { ref error }
                if error.contains("staged") && error.contains("cleanup")),
            "a substituted staging name must fence the operation: {receipt:?}"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "before");
        assert_eq!(fs::read_to_string(&temp_path).await.unwrap(), "substitute");
        assert_eq!(fs::read_to_string(&moved_path).await.unwrap(), "approved");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_staged_handle_denies_shared_write_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(dir.path().join("notes.txt"), "before")
            .await
            .unwrap();

        let prepared = workspace
            .begin_mutation("fs.write", "write", "notes.txt")
            .await
            .unwrap()
            .prepare(b"approved")
            .await
            .unwrap();
        let temp_path = dir.path().join(prepared.temp_name.as_ref().unwrap());
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&temp_path)
                .is_err(),
            "the live staging handle must deny another writer"
        );
        assert!(
            std::fs::remove_file(&temp_path).is_err(),
            "the live staging handle must deny path-based deletion"
        );
        prepared.rollback("test complete").await.unwrap();
        assert!(!temp_path.exists());
    }

    #[tokio::test]
    async fn same_path_mutations_wait_and_snapshot_the_committed_winner() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("notes.txt").await.unwrap();
        fs::write(&target, "base").await.unwrap();

        // The first prepared effect owns the path lease through settlement.
        let first = workspace
            .begin_mutation("edit.patch", "patch", "notes.txt")
            .await
            .unwrap()
            .prepare(b"first")
            .await
            .unwrap();

        let contender_workspace = workspace.clone();
        let mut contender = tokio::spawn(async move {
            let transaction = contender_workspace
                .begin_mutation("edit.patch", "patch", "notes.txt")
                .await
                .unwrap();
            let revision = transaction.before_revision().unwrap();
            let prepared = transaction.prepare(b"second").await.unwrap();
            (revision, prepared)
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut contender)
                .await
                .is_err(),
            "a same-path contender must wait while the prepared winner is unsettled"
        );

        assert!(matches!(
            first.commit().await,
            EffectReceipt::Applied { .. }
        ));
        let (second_revision, second) =
            tokio::time::timeout(std::time::Duration::from_secs(2), contender)
                .await
                .expect("the contender must resume after settlement")
                .unwrap();
        assert_eq!(
            second_revision,
            ContentDigest::sha256_bytes(b"first"),
            "the resumed writer must snapshot the committed winner, not the old base"
        );
        assert!(matches!(
            second.commit().await,
            EffectReceipt::Applied { .. }
        ));
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "second");
    }

    #[tokio::test]
    async fn different_path_mutations_remain_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(dir.path().join("a.txt"), "a").await.unwrap();
        fs::write(dir.path().join("b.txt"), "b").await.unwrap();

        let first = workspace
            .begin_mutation("fs.write", "write", "a.txt")
            .await
            .unwrap()
            .prepare(b"held")
            .await
            .unwrap();
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            workspace.begin_mutation("fs.write", "write", "b.txt"),
        )
        .await
        .expect("an unrelated path must not wait on the first lease")
        .unwrap();
        second
            .prepare(b"independent")
            .await
            .unwrap()
            .rollback("test complete")
            .await
            .unwrap();
        first.rollback("test complete").await.unwrap();
    }

    #[tokio::test]
    async fn reversed_batches_share_one_deadlock_free_lock_order() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(dir.path().join("a.txt"), "a").await.unwrap();
        fs::write(dir.path().join("b.txt"), "b").await.unwrap();

        let first_paths = vec!["b.txt".to_string(), "a.txt".to_string()];
        let first = workspace
            .begin_existing_mutations("edit.patch", "patch", &first_paths, 1024)
            .await
            .unwrap();
        let contender_workspace = workspace.clone();
        let mut contender = tokio::spawn(async move {
            let paths = vec!["a.txt".to_string(), "b.txt".to_string()];
            contender_workspace
                .begin_existing_mutations("edit.patch", "patch", &paths, 1024)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut contender)
                .await
                .is_err(),
            "the reverse batch must wait rather than entering with a partial lease set"
        );
        drop(first);
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), contender)
            .await
            .expect("sorted acquisition must resume without deadlock")
            .unwrap()
            .unwrap();
        assert_eq!(second.len(), 2);
        drop(second);

        // A later acquisition prunes expired Weak entries; the registry is
        // bounded by active/waiting paths, not path history.
        let paths = vec!["a.txt".to_string()];
        let active = workspace
            .begin_existing_mutations("edit.replace", "replace", &paths, 1024)
            .await
            .unwrap();
        let registry_len = workspace
            .mutation_locks
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(registry_len, 1);
        drop(active);
    }

    #[tokio::test]
    async fn cancelling_a_partial_batch_releases_every_acquired_lease() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(dir.path().join("a.txt"), "a").await.unwrap();
        fs::write(dir.path().join("b.txt"), "b").await.unwrap();

        // Hold b so the sorted [a, b] batch can acquire a and then wait.
        let held_b = workspace
            .begin_mutation("fs.write", "write", "b.txt")
            .await
            .unwrap()
            .prepare(b"held")
            .await
            .unwrap();
        let batch_workspace = workspace.clone();
        let batch = tokio::spawn(async move {
            let paths = vec!["a.txt".to_string(), "b.txt".to_string()];
            batch_workspace
                .begin_existing_mutations("edit.patch", "patch", &paths, 1024)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let contender_workspace = workspace.clone();
        let mut a_contender = tokio::spawn(async move {
            contender_workspace
                .begin_mutation("fs.write", "write", "a.txt")
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut a_contender)
                .await
                .is_err(),
            "the partial batch must already own a while it waits for b"
        );

        batch.abort();
        assert!(matches!(batch.await, Err(error) if error.is_cancelled()));
        let a_transaction = tokio::time::timeout(std::time::Duration::from_secs(1), a_contender)
            .await
            .expect("cancelling the batch must release its already-acquired a lease")
            .unwrap()
            .unwrap();
        drop(a_transaction);
        held_b.rollback("test complete").await.unwrap();
    }

    #[tokio::test]
    async fn external_change_after_prepare_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("notes.txt").await.unwrap();
        fs::write(&target, "base").await.unwrap();

        let prepared = workspace
            .begin_mutation("edit.patch", "patch", "notes.txt")
            .await
            .unwrap()
            .prepare(b"agent")
            .await
            .unwrap();
        fs::write(&target, "external").await.unwrap();

        let receipt = prepared.commit().await;
        assert!(
            matches!(receipt, EffectReceipt::NotApplied { ref error } if error.contains("stale_revision")),
            "external drift must prevent the replace: {receipt:?}"
        );
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "external");
    }

    #[tokio::test]
    async fn mutation_boundary_rejects_oversized_content_before_staging() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let transaction = workspace
            .begin_mutation("capability.write", "write", "large.bin")
            .await
            .unwrap();
        let result = transaction
            .prepare(&vec![b'x'; MAX_MUTATION_BYTES + 1])
            .await;
        assert!(result.is_err());
        assert!(!dir.path().join("large.bin").exists());
        assert!(!workspace.state_dir().join("changes.jsonl").exists());
        let temp_files = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temp_files, 0);
    }

    #[tokio::test]
    async fn begin_mutation_rejects_an_oversized_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(
            dir.path().join("large.bin"),
            vec![b'x'; MAX_MUTATION_BYTES + 1],
        )
        .await
        .unwrap();

        let result = workspace
            .begin_mutation("fs.write", "write", "large.bin")
            .await;
        assert!(
            matches!(result, Err(AgentError::InvalidRequest(message)) if message.contains("limited")),
            "existing targets must be bounded before hashing"
        );
        assert!(!workspace.state_dir().join("changes.jsonl").exists());
        assert_eq!(staged_temp_count(dir.path()).await, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn batch_mutation_preserves_existing_unix_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("script.sh");
        fs::write(&target, "#!/bin/sh\nexit 0\n").await.unwrap();
        fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751))
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let paths = vec!["script.sh".to_string()];
        let snapshot = workspace
            .begin_existing_mutations("edit.patch", "patch", &paths, 1024)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let (transaction, _) = snapshot.into_parts();

        let receipt = transaction
            .prepare(b"#!/bin/sh\nexit 1\n")
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
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o7777, 0o751);
    }

    #[tokio::test]
    async fn commit_that_lands_but_cannot_journal_is_a_durability_failure() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("file.txt").await.unwrap();
        fs::write(&target, "before").await.unwrap();

        workspace
            .begin_mutation("fs.write", "write", "file.txt")
            .await
            .unwrap()
            .apply(b"after")
            .await
            .unwrap();

        // Stage the second mutation while the journal is still healthy...
        let tx = workspace
            .begin_mutation("fs.write", "write", "file.txt")
            .await
            .unwrap();
        let prepared = tx.prepare(b"second").await.unwrap();

        // ...then break the journal *between* prepare and commit: the rename
        // lands, but the MutationCommitted record cannot be appended.
        let journal = workspace.state_dir().join("changes.jsonl");
        fs::remove_file(&journal).await.unwrap();
        fs::create_dir(&journal).await.unwrap();

        let receipt = prepared.commit().await;
        match &receipt {
            EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(_),
                ..
            } => {}
            other => panic!("expected Applied + DurabilityFailed, got {other:?}"),
        }

        // The world did change: the caller must treat this as a degraded
        // state needing recovery, never as "nothing happened".
        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "second",
            "the mutation landed even though the journal record failed"
        );

        fs::remove_dir(&journal).await.unwrap();
    }

    #[tokio::test]
    async fn before_hash_hashes_real_content_beyond_the_capture_limit() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let target = workspace.resolve_relative("big.bin").await.unwrap();
        let big = vec![b'z'; CHANGE_CAPTURE_LIMIT + 1000];
        fs::write(&target, &big).await.unwrap();

        let tx = workspace
            .begin_mutation("fs.write", "write", "big.bin")
            .await
            .unwrap();
        tx.prepare(b"small").await.unwrap();

        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(record["kind"], "mutation_prepared");
        assert_eq!(record["bytes_before"], big.len() as u64);
        assert_eq!(
            record["before_hash"],
            content_hash(&big),
            "the hash must reflect the real 256KB+ content, not an empty fallback"
        );
        assert!(
            record["old_content"].is_null(),
            "the bounded backup copy must not capture a 256KB+ file"
        );
    }

    /// Rename with retry: transient sharing violations from the victim's
    /// open handles are expected on Windows and retried briefly.
    fn retry_rename(from: &Path, to: &Path) -> bool {
        for _ in 0..50 {
            if std::fs::rename(from, to).is_ok() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        false
    }

    /// Swap `d` between a real directory (`real`, holding the inside file)
    /// and a link (`link`, pointing outside the workspace), both parked
    /// under `root`. Every step is a plain rename — no subprocess — so the
    /// swap genuinely races the victim. Returns whether the link ever
    /// occupied `d` (a real competition window existed); a later step can
    /// lose to the victim recreating `d`, which is exactly the workspace-
    /// confined behavior under test.
    fn swap_d_between_real_and_link(root: &Path) -> bool {
        let d = root.join("d");
        let real = root.join("real");
        let link = root.join("link");
        let hold = root.join("hold");
        // Initial state (established by the test): `d` is the real
        // directory and `link` is the pre-planted junction/symlink.
        let mut saw_link_at_d = false;
        for _ in 0..3 {
            // Move the current `d` (the real dir) away, put the link at
            // `d`, then park the real dir at `real` again.
            if !(retry_rename(&d, &hold) && retry_rename(&link, &d) && retry_rename(&hold, &real)) {
                return saw_link_at_d;
            }
            // Hold the link in place so a racing open can hit it.
            std::thread::sleep(std::time::Duration::from_millis(1));
            saw_link_at_d = true;
            // Swap back: link out, real dir back at `d`. A racing file
            // mutation refuses the temporary missing-parent gap; it never
            // recreates directory topology while the attacker restores it.
            if !(retry_rename(&d, &hold) && retry_rename(&real, &d) && retry_rename(&hold, &link)) {
                return saw_link_at_d;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        true
    }

    #[tokio::test]
    async fn confined_read_rejects_preplanted_reparse_link() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let link = dir.path().join("link");
        if !try_make_link(&link, outside.path()) {
            return; // platform cannot create links; nothing to assert
        }
        fs::write(outside.path().join("secret.txt"), "secret")
            .await
            .unwrap();
        let result = workspace.confined_open_read("link/secret.txt").await;
        assert!(
            result.is_err(),
            "a pre-planted link must be rejected at open time, not followed"
        );
    }

    #[tokio::test]
    async fn confined_mutation_refuses_missing_parent_before_any_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let result = workspace
            .begin_mutation("fs.write", "write", "a/b/c.txt")
            .await;
        let error = match result {
            Ok(_) => panic!("a file mutation must not create missing parent directories"),
            Err(error) => error,
        };
        assert!(
            matches!(
                &error,
                AgentError::InvalidRequest(message)
                    if message.contains("mutation parent directory not found: a")
                        && message.contains("existing parent directory")
            ),
            "unexpected missing-parent error: {error}"
        );
        assert!(
            !dir.path().join("a").exists(),
            "begin_mutation must leave missing directory topology untouched"
        );
        assert_eq!(
            staged_temp_count(dir.path()).await,
            0,
            "begin_mutation must fail before creating a staged file"
        );
        assert!(
            !workspace.state_dir().join("changes.jsonl").exists(),
            "begin_mutation must fail before recording review evidence"
        );
        let authority = fs::metadata(
            workspace
                .state_dir()
                .join("authority/workspace-effects.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(
            authority.len(),
            0,
            "begin_mutation must fail before recording workspace authority evidence"
        );
    }

    #[tokio::test]
    async fn confined_mutation_creates_file_inside_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).await.unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        workspace
            .begin_mutation("fs.write", "write", "a/b/c.txt")
            .await
            .unwrap()
            .apply(b"deep")
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt"))
                .await
                .unwrap(),
            "deep"
        );
    }

    #[tokio::test]
    async fn confined_replace_file_overwrites_and_creates() {
        use std::ffi::OsStr;
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cdir = ConfinedDir::open_root(dir.path()).unwrap();

        // Pre-plant the destination, then replace it through the handle.
        std::fs::write(dir.path().join("target.txt"), b"old").unwrap();
        let mut src = cdir.create_new_file(OsStr::new("stage.tmp")).unwrap();
        src.write_all(b"new content").unwrap();
        src.flush().unwrap();
        cdir.replace_file(&src, OsStr::new("stage.tmp"), OsStr::new("target.txt"))
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("target.txt")).unwrap(),
            b"new content"
        );
        assert!(
            !dir.path().join("stage.tmp").exists(),
            "the staged file is consumed by the replace"
        );

        // A missing destination is created by the same operation.
        let mut src2 = cdir.create_new_file(OsStr::new("stage2.tmp")).unwrap();
        src2.write_all(b"fresh").unwrap();
        src2.flush().unwrap();
        cdir.replace_file(&src2, OsStr::new("stage2.tmp"), OsStr::new("fresh.txt"))
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("fresh.txt")).unwrap(),
            b"fresh"
        );
    }

    #[tokio::test]
    async fn concurrent_dir_swap_never_reads_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();

        // Park a real directory (holding the inside file) and a pre-planted
        // link pointing outside; the attacker swaps them in and out of `d`.
        let root = dir.path();
        fs::create_dir(root.join("real")).await.unwrap();
        fs::write(root.join("real/file.txt"), "inside")
            .await
            .unwrap();
        fs::write(outside.path().join("file.txt"), "outside")
            .await
            .unwrap();
        if !try_make_link(&root.join("link"), outside.path()) {
            return; // platform cannot create links; nothing to assert
        }
        assert!(
            std::fs::rename(root.join("real"), root.join("d")).is_ok(),
            "initial d must be the real directory"
        );

        let root = root.to_path_buf();
        let attacker = std::thread::spawn(move || swap_d_between_real_and_link(&root));

        let mut reads = 0usize;
        for _ in 0..600 {
            if let Ok(confined) = workspace.confined_open_read("d/file.txt").await {
                use std::io::Read;
                let mut content = String::new();
                let mut file = confined.into_std();
                if file.read_to_string(&mut content).is_ok() {
                    reads += 1;
                    assert_eq!(
                        content, "inside",
                        "a confined read must never surface content from outside the workspace"
                    );
                }
            }
        }
        let supported = attacker.join().unwrap();
        assert!(
            !supported || reads > 0,
            "the swap loop must race with real reads to be a meaningful test"
        );
    }

    #[tokio::test]
    async fn concurrent_dir_swap_never_writes_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();

        let root = dir.path();
        fs::create_dir(root.join("real")).await.unwrap();
        fs::write(root.join("real/file.txt"), "inside")
            .await
            .unwrap();
        fs::write(outside.path().join("file.txt"), "outside")
            .await
            .unwrap();
        if !try_make_link(&root.join("link"), outside.path()) {
            return; // platform cannot create links; nothing to assert
        }
        assert!(
            std::fs::rename(root.join("real"), root.join("d")).is_ok(),
            "initial d must be the real directory"
        );

        let root = root.to_path_buf();
        let attacker = std::thread::spawn(move || swap_d_between_real_and_link(&root));

        let mut applied = 0usize;
        for _ in 0..150 {
            if let Ok(tx) = workspace
                .begin_mutation("fs.write", "write", "d/new.txt")
                .await
                && tx.apply(b"payload").await.is_ok()
            {
                applied += 1;
            }
        }
        let supported = attacker.join().unwrap();
        assert!(
            !supported || applied > 0,
            "the swap loop must race with real mutations to be a meaningful test"
        );

        // The write must never have landed outside the workspace: the
        // outside directory only ever holds its pre-planted file.
        let mut entries = fs::read_dir(outside.path()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            assert_ne!(
                entry.file_name().to_string_lossy(),
                "new.txt",
                "a mutation escaped the workspace through the directory swap"
            );
        }
        // Every applied mutation must be visible inside the workspace (the
        // pinned handle keeps writing into the real directory even while
        // the path is being swapped). The real directory parks at `real`
        // or `d` depending on where the swap stopped.
        if applied > 0 {
            let landed = dir.path().join("real/new.txt").exists()
                || dir.path().join("d/new.txt").exists()
                || dir.path().join("hold/new.txt").exists();
            assert!(landed, "applied mutations must land inside the workspace");
        }
    }

    #[tokio::test]
    async fn artifact_relative_path_resolves_only_current_run_identity_locators() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let reference = workspace
            .write_artifact(run_id, "fs-list", "txt", b"one\n")
            .await
            .unwrap();
        let locator = ArtifactLocator::parse(&reference).unwrap();
        assert!(locator.is_sealed());
        assert_eq!(locator.owner(), "fs-list");
        assert_eq!(locator.run_id(), run_id);

        let valid = workspace
            .artifact_relative_path_for_run(&reference, run_id)
            .expect("a genuine sealed artifact reference must resolve");
        assert_eq!(
            valid,
            std::path::PathBuf::from(format!(
                ".focus-agent/artifacts/{run_id}/fs-list/{}",
                locator.digest().unwrap()
            ))
        );

        // 路径形 URI、query/fragment、穿越和异 run 在解析阶段拒绝。
        for bad in [
            "https://example.com/x",
            "artifact://.focus-agent/artifacts/x/y.txt?page=2",
            "artifact://.focus-agent/artifacts/x/y.txt#L10",
            "artifact://.focus-agent/artifacts/../secret.txt",
            "artifact://C:/Windows/system32/notepad.exe",
            "artifact:///etc/passwd",
            "artifact://src/main.rs",
        ] {
            assert!(
                workspace
                    .artifact_relative_path_for_run(bad, run_id)
                    .is_err(),
                "must refuse {bad:?}"
            );
        }

        let other_run = RunId::new();
        let cross_run = workspace
            .write_artifact(other_run, "fs-list", "txt", b"other\n")
            .await
            .unwrap();
        assert!(
            workspace
                .artifact_relative_path_for_run(&cross_run, run_id)
                .is_err(),
            "a run must not read another run's artifacts"
        );
    }

    #[tokio::test]
    async fn sealed_artifact_open_rejects_a_tampered_body() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let reference = workspace
            .write_artifact(run_id, "grep", "txt", b"trusted body")
            .await
            .unwrap();
        let relative = workspace
            .artifact_relative_path_for_run(&reference, run_id)
            .unwrap();
        tokio::fs::write(workspace.root().join(&relative), b"forged body")
            .await
            .unwrap();
        let error = match workspace.open_artifact_for_run(&reference, run_id).await {
            Ok(_) => panic!("a digest mismatch must fail closed"),
            Err(error) => error,
        };
        assert!(
            matches!(error, AgentError::InvalidRequest(ref message) if message.contains("digest mismatch")),
            "mismatch must be a typed invalid request, got {error}"
        );
    }

    #[tokio::test]
    async fn draft_locator_reads_before_seal_and_identity_after() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut draft = workspace
            .create_artifact(run_id, "shell", "log")
            .await
            .unwrap();
        let draft_ref = draft.locator().to_string();
        draft.write_all(b"partial").await.unwrap();
        let (normalized, _file) = workspace
            .open_artifact_for_run(&draft_ref, run_id)
            .await
            .expect("a live draft must be readable");
        assert_eq!(normalized, draft_ref);
        assert!(!ArtifactLocator::parse(&draft_ref).unwrap().is_sealed());

        let sealed = workspace.seal_artifact(draft).await.unwrap();
        assert!(ArtifactLocator::parse(&sealed).unwrap().is_sealed());
        assert!(
            workspace
                .open_artifact_for_run(&draft_ref, run_id)
                .await
                .is_err(),
            "sealing must retire the draft locator"
        );
        let (_normalized, file) = workspace
            .open_artifact_for_run(&sealed, run_id)
            .await
            .unwrap();
        let bytes = tokio::fs::read(file.display()).await.unwrap();
        assert_eq!(bytes, b"partial");
    }
}
