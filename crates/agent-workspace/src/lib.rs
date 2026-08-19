use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
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
use uuid::Uuid;

mod broker;
mod confined;
mod handles;
mod journal;
mod process_journal;
mod remote_journal;
mod runtime_facts;

pub use agent_contracts::{ArtifactLocator, MAX_ARTIFACT_REFERENCE_BYTES};
pub use broker::WorkspaceOutputBroker;
pub use confined::{ConfinedDir, ConfinedFile};
pub use handles::{ArtifactStoreHandle, ConfinedWorkspaceHandle};
pub use journal::WorkspaceEffectRecovery;
pub use remote_journal::RemoteEffectAck;
pub use runtime_facts::capture_host_runtime_facts;

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

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    state_dir: PathBuf,
    effect_journal: Arc<journal::WorkspaceEffectJournal>,
    process_journal: Arc<process_journal::ProcessEffectJournal>,
    remote_journal: Arc<remote_journal::RemoteEffectJournal>,
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
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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
    /// ordinary read cap are skipped (`InvalidRequest`) so this never
    /// becomes a second full-file ingest path.
    pub async fn resource_revision(&self, key: &str) -> AgentResult<Option<String>> {
        const MAX_REVISION_BYTES: u64 = 2 * 1024 * 1024;
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
                let mut file = confined.into_tokio();
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).await.map_err(|error| {
                    AgentError::Io(format!("read {path} for revision: {error}"))
                })?;
                Ok(Some(ContentDigest::sha256_bytes(&bytes).to_string()))
            }
            Err(error) if revision_lookup_missing(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Confine a mutation's parent directory with validation and open fused
    /// into one directory-handle-relative descent. Missing components are
    /// created through the pinned handle chain (never by following a path
    /// string), and the runtime state directory is rejected, mirroring
    /// `resolve_mutation`.
    pub(crate) async fn confined_parent(&self, relative: &Path) -> AgentResult<ConfinedDir> {
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
                    // Create the missing component under the pinned parent,
                    // then reopen it; a concurrent creator only means the
                    // reopen succeeds.
                    match dir.create_child_dir(part.as_os_str()) {
                        Ok(()) => {}
                        Err(ce) if ce.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(ce) => {
                            return Err(AgentError::Io(format!(
                                "create dir {}: {ce}",
                                display.display()
                            )));
                        }
                    }
                    dir = dir
                        .open_child_dir(part.as_os_str())
                        .map_err(|e| confined_io_error("open created dir", &display, e))?;
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
    /// The parent directory is confined with validation and open fused into
    /// a directory-handle-relative descent: the staged temp file and the
    /// final atomic replace both happen relative to the pinned handle, so a
    /// link swap after validation cannot redirect the write outside the
    /// workspace. The old content is read through that same pinned handle.
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
        let parent_rel = target
            .parent()
            .and_then(|p| p.strip_prefix(&self.root).ok())
            .unwrap_or_else(|| Path::new(""));
        let parent = self.confined_parent(parent_rel).await?;

        let mut bytes_before = 0u64;
        let mut target_existed = false;
        let mut before_hash = content_hash(&[]);
        let mut old_content = None;
        match parent.open_existing(target_name) {
            Ok(file) => {
                target_existed = true;
                let meta = file
                    .metadata()
                    .map_err(|e| AgentError::Io(format!("metadata {}: {e}", target.display())))?;
                bytes_before = meta.len();
                if meta.is_file() {
                    // The hash must always reflect the real content — a
                    // recovery pass relies on it to tell "prepared but
                    // never committed" from "committed". Only the *backup*
                    // is bounded: big files get a hash but no journal copy.
                    // The read goes through the pinned handle, so a swap
                    // cannot change which object is hashed.
                    let mut file = file;
                    use std::io::Read;
                    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                    let mut captured = if meta.len() <= CHANGE_CAPTURE_LIMIT as u64 {
                        Some(Vec::with_capacity(meta.len() as usize))
                    } else {
                        None
                    };
                    let mut buffer = [0_u8; 64 * 1024];
                    loop {
                        let read = file.read(&mut buffer).map_err(|e| {
                            AgentError::Io(format!("read {}: {e}", target.display()))
                        })?;
                        if read == 0 {
                            break;
                        }
                        for byte in &buffer[..read] {
                            hash ^= u64::from(*byte);
                            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                        }
                        if let Some(bytes) = &mut captured {
                            bytes.extend_from_slice(&buffer[..read]);
                        }
                    }
                    before_hash = format!("{hash:016x}");
                    old_content = captured.and_then(|bytes| String::from_utf8(bytes).ok());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(confined_io_error("inspect", &target, e)),
        }
        let relative = display_relative(&self.root, &target);
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
            old_content,
            tx_id: Uuid::new_v4().to_string(),
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
        let journal = self.state_dir.join("changes.jsonl");
        let line = serde_json::to_string(&change)
            .map_err(|e| AgentError::Storage(format!("serialize change: {e}")))?;
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).append(true);
        let mut file = options
            .open(&journal)
            .await
            .map_err(|e| AgentError::Storage(format!("open change journal: {e}")))?;
        use tokio::io::AsyncWriteExt;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| AgentError::Storage(format!("append change journal: {e}")))?;
        file.write_all(b"\n")
            .await
            .map_err(|e| AgentError::Storage(format!("append change journal: {e}")))?;
        file.flush()
            .await
            .map_err(|e| AgentError::Storage(format!("flush change journal: {e}")))?;
        Ok(())
    }
}

/// A single journaled, atomic file mutation, split into prepare and commit.
///
/// Ordering contract: `prepare` stages the new content in a hidden
/// temporary file next to the target and records `MutationPrepared` *before*
/// the target is swapped in, so a journal failure never leaves the target
/// half-mutated and a caller retrying the tool cannot double-apply a
/// mutation that already landed. `commit` then atomically renames the staged
/// file over the target and records `MutationCommitted`; a commit failure
/// rolls the staging back and records `MutationRolledBack`, so the journal
/// never claims a mutation that did not land. The runtime owns the
/// commit/rollback decision: it validates the operation is still current
/// (generation fence) before committing, and rolls back when a stale
/// operation's prepared effect would otherwise leak.
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
    old_content: Option<String>,
    tx_id: String,
}

impl MutationTransaction {
    /// Stage `content`: write the temporary file and record
    /// `MutationPrepared`. The target is not touched. Returns the prepared
    /// mutation the runtime commits or rolls back.
    pub async fn prepare(self, content: &[u8]) -> AgentResult<PreparedMutation> {
        self.prepare_inner(content, None).await
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
        // The parent is already confined (with missing components created
        // through the pinned handle chain); the staged file is created
        // exclusively under that handle, so a link swap cannot redirect it.
        // Staging is a short synchronous write through the exclusive handle
        // (the same style as the atomic replace below).
        let file_name = self.target_name.to_string_lossy().to_string();
        let temp_name = std::ffi::OsString::from(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
        let mut file = self
            .parent
            .create_new_file(&temp_name)
            .map_err(|e| confined_io_error("create temp", &self.target, e))?;

        use std::io::Write;
        if let Err(e) = file.write_all(content) {
            let _ = self.parent.remove_file(&temp_name);
            return Err(AgentError::Io(format!("write temp file: {e}")));
        }
        if let Err(e) = file.flush() {
            let _ = self.parent.remove_file(&temp_name);
            return Err(AgentError::Io(format!("flush temp file: {e}")));
        }
        if let Err(e) = file.sync_all() {
            let _ = self.parent.remove_file(&temp_name);
            return Err(AgentError::Io(format!("sync temp file: {e}")));
        }

        let after_hash = content_hash(content);

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
        if let Err(e) = self.workspace.record_change(record).await {
            let _ = self.parent.remove_file(&temp_name);
            return Err(e);
        }
        if let Some(context) = &effect_context
            && let Err(error) =
                self.workspace
                    .effect_journal
                    .append_prepared(journal::PreparedEvidence {
                        tx_id: self.tx_id.clone(),
                        context: context.clone(),
                        relative_target: self.relative.clone(),
                        temp_name: temp_name.to_string_lossy().into_owned(),
                        target_existed: self.target_existed,
                        before_hash: self.before_hash,
                        after_hash,
                    })
        {
            let _ = self.parent.remove_file(&temp_name);
            return Err(error);
        }

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
        })
    }

    /// Convenience: prepare then commit immediately (used where the caller
    /// is itself the only judge of the operation's validity — the runtime's
    /// generation fence is bypassed, so only trusted core paths may use it).
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
        let Some(temp_name) = self.temp_name.take() else {
            return EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                evidence: Some(self.tx_id.clone()),
            };
        };
        let from = self
            .temp_file
            .as_ref()
            .expect("prepare created the staged file");
        if let Err(e) = self
            .parent
            .replace_file(from, &temp_name, &self.target_name)
        {
            if self.cleanup_staged(&temp_name) {
                self.record_rolled_back(format!("commit failed: {e}")).await;
            }
            return EffectReceipt::NotApplied {
                error: confined_io_error("commit", &self.target, e).to_string(),
            };
        }
        // The target changed; from here on any failure is a durability
        // problem, not a "did not apply" one.
        self.finished = true;
        if let Err(error) = self.parent.sync_all() {
            return EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(format!(
                    "sync committed mutation parent: {error}"
                )),
                evidence: Some(self.tx_id.clone()),
            };
        }
        if self.effect_context.is_some()
            && let Err(error) = self.workspace.effect_journal.append_committed(&self.tx_id)
        {
            return EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(error.to_string()),
                evidence: Some(self.tx_id.clone()),
            };
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
        EffectReceipt::Applied {
            durability: EffectDurability::Durable,
            evidence: Some(self.tx_id.clone()),
        }
    }

    /// Remove the staged file and record `MutationRolledBack` (best effort).
    /// Called when the owning operation is stale and the mutation must not
    /// land. The staging file is deleted here, not left behind for the
    /// `Drop` impl — a stale or cancelled mutation must never leak temp
    /// files.
    pub async fn rollback(mut self, reason: &str) {
        let cleaned = self
            .temp_name
            .take()
            .is_none_or(|temp_name| self.cleanup_staged(&temp_name));
        self.finished = true;
        if cleaned {
            self.record_rolled_back(reason.to_string()).await;
        }
    }

    fn cleanup_staged(&self, temp_name: &std::ffi::OsStr) -> bool {
        match self.parent.remove_file(temp_name) {
            Ok(()) => self.parent.sync_all().is_ok(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }

    async fn record_rolled_back(&self, reason: String) {
        if self.effect_context.is_some() {
            let _ = self
                .workspace
                .effect_journal
                .append_rolled_back(&self.tx_id);
        }
        let record = ChangeRecord::MutationRolledBack {
            tx_id: self.tx_id.clone(),
            timestamp_ms: now_ms(),
            reason,
        };
        let _ = self.workspace.record_change(record).await;
    }
}

impl Drop for PreparedMutation {
    fn drop(&mut self) {
        if !self.finished
            && let Some(temp_name) = &self.temp_name
            && self.parent.remove_file(temp_name).is_ok()
        {
            let _ = self.parent.sync_all();
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

    async fn commit(self: Box<Self>) -> EffectReceipt {
        (*self).commit().await
    }

    async fn rollback(self: Box<Self>, reason: &str) {
        (*self).rollback(reason).await;
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
        prepared.rollback("stale operation").await;

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
        prepared.rollback("stale operation").await;
        assert_eq!(temp_count().await, 0, "rollback must delete the temp file");
        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "original",
            "the target is untouched"
        );
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
            // Swap back: link out, real dir back at `d`. The victim may
            // have recreated `d` (confined_parent creates missing parents),
            // which makes the restore fail — the write still landed inside
            // the workspace, which is what the test asserts.
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
    async fn confined_mutation_creates_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
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
