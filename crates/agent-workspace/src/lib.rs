use std::io;
use std::path::{Component, Path, PathBuf};

use agent_contracts::{AgentError, AgentResult, Effect, EffectCommitError, RunId};
use serde::Serialize;
use tokio::fs;
use uuid::Uuid;

mod broker;
mod confined;
mod handles;

pub use broker::WorkspaceOutputBroker;
pub use confined::{ConfinedDir, ConfinedFile};
pub use handles::{ArtifactStoreHandle, ConfinedWorkspaceHandle};

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

fn artifact_ref(root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    format!("artifact://{relative}")
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
        fs::create_dir_all(state_dir.join("artifacts"))
            .await
            .map_err(|e| AgentError::Io(format!("create state artifacts dir: {e}")))?;
        Ok(Self { root, state_dir })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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
        let mut before_hash = content_hash(&[]);
        let mut old_content = None;
        match parent.open_existing(target_name) {
            Ok(file) => {
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
                    use std::io::Read;
                    let mut file = file;
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)
                        .map_err(|e| AgentError::Io(format!("read {}: {e}", target.display())))?;
                    before_hash = content_hash(&bytes);
                    if bytes.len() as u64 <= CHANGE_CAPTURE_LIMIT as u64 {
                        old_content = String::from_utf8(bytes).ok();
                    }
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
            before_hash,
            old_content,
            tx_id: Uuid::new_v4().to_string(),
        })
    }

    pub async fn write_artifact(
        &self,
        run_id: RunId,
        prefix: &str,
        extension: &str,
        bytes: &[u8],
    ) -> AgentResult<String> {
        let run_dir = self.state_dir.join("artifacts").join(run_id.to_string());
        fs::create_dir_all(&run_dir)
            .await
            .map_err(|e| AgentError::Io(format!("create artifact dir: {e}")))?;

        let safe_prefix: String = prefix
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(48)
            .collect();
        let safe_ext: String = extension
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(12)
            .collect();
        let filename = if safe_ext.is_empty() {
            format!("{}-{}", safe_prefix, Uuid::new_v4())
        } else {
            format!("{}-{}.{}", safe_prefix, Uuid::new_v4(), safe_ext)
        };
        let path = run_dir.join(filename);
        fs::write(&path, bytes)
            .await
            .map_err(|e| AgentError::Io(format!("write artifact: {e}")))?;

        Ok(artifact_ref(&self.root, &path))
    }

    /// Open a new artifact file for incremental appends (streaming process
    /// output). Returns the artifact reference and the open file.
    pub async fn create_artifact(
        &self,
        run_id: RunId,
        prefix: &str,
        extension: &str,
    ) -> AgentResult<(String, tokio::fs::File)> {
        let run_dir = self.state_dir.join("artifacts").join(run_id.to_string());
        fs::create_dir_all(&run_dir)
            .await
            .map_err(|e| AgentError::Io(format!("create artifact dir: {e}")))?;

        let safe_prefix: String = prefix
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(48)
            .collect();
        let safe_ext: String = extension
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(12)
            .collect();
        let filename = if safe_ext.is_empty() {
            format!("{}-{}", safe_prefix, Uuid::new_v4())
        } else {
            format!("{}-{}.{}", safe_prefix, Uuid::new_v4(), safe_ext)
        };
        let path = run_dir.join(filename);
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let file = options
            .open(&path)
            .await
            .map_err(|e| AgentError::Io(format!("create artifact: {e}")))?;
        Ok((artifact_ref(&self.root, &path), file))
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
    pub async fn prepare(mut self, content: &[u8]) -> AgentResult<PreparedMutation> {
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

        let record = ChangeRecord::MutationPrepared {
            tx_id: self.tx_id.clone(),
            timestamp_ms: now_ms(),
            tool: self.tool.clone(),
            path: self.relative.clone(),
            action: self.action.clone(),
            bytes_before: self.bytes_before,
            bytes_after: content.len() as u64,
            before_hash: self.before_hash,
            after_hash: content_hash(content),
            old_content: self.old_content.take(),
        };
        if let Err(e) = self.workspace.record_change(record).await {
            let _ = self.parent.remove_file(&temp_name);
            return Err(e);
        }

        Ok(PreparedMutation {
            workspace: self.workspace,
            parent: self.parent,
            target: self.target,
            target_name: self.target_name,
            tx_id: self.tx_id,
            temp_name: Some(temp_name),
            temp_file: Some(file),
            finished: false,
        })
    }

    /// Convenience: prepare then commit immediately (used where the caller
    /// is itself the only judge of the operation's validity — the runtime's
    /// generation fence is bypassed, so only trusted core paths may use it).
    pub async fn apply(self, content: &[u8]) -> AgentResult<()> {
        let prepared = self.prepare(content).await?;
        match prepared.commit().await {
            Ok(()) => Ok(()),
            // Apply collapses the structured failure: the caller only needs
            // to know it failed and whether the world changed.
            Err(EffectCommitError::NotApplied(error)) => Err(error),
            Err(EffectCommitError::AppliedButDurabilityFailed(error)) => Err(AgentError::Internal(
                format!("mutation applied but its journal record failed: {error}"),
            )),
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
    finished: bool,
}

impl PreparedMutation {
    /// The journal transaction id of this staged mutation.
    pub fn tx_id(&self) -> &str {
        &self.tx_id
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
    pub async fn commit(mut self) -> Result<(), EffectCommitError> {
        let Some(temp_name) = self.temp_name.take() else {
            return Ok(());
        };
        let from = self
            .temp_file
            .as_ref()
            .expect("prepare created the staged file");
        if let Err(e) = self
            .parent
            .replace_file(from, &temp_name, &self.target_name)
        {
            let _ = self.parent.remove_file(&temp_name);
            self.record_rolled_back(format!("commit failed: {e}")).await;
            return Err(EffectCommitError::NotApplied(confined_io_error(
                "commit",
                &self.target,
                e,
            )));
        }
        // The target changed; from here on any failure is a durability
        // problem, not a "did not apply" one.
        self.finished = true;
        let record = ChangeRecord::MutationCommitted {
            tx_id: self.tx_id.clone(),
            timestamp_ms: now_ms(),
        };
        if let Err(error) = self.workspace.record_change(record).await {
            return Err(EffectCommitError::AppliedButDurabilityFailed(error));
        }
        Ok(())
    }

    /// Remove the staged file and record `MutationRolledBack` (best effort).
    /// Called when the owning operation is stale and the mutation must not
    /// land. The staging file is deleted here, not left behind for the
    /// `Drop` impl — a stale or cancelled mutation must never leak temp
    /// files.
    pub async fn rollback(mut self, reason: &str) {
        if let Some(temp_name) = self.temp_name.take() {
            let _ = self.parent.remove_file(&temp_name);
        }
        self.finished = true;
        self.record_rolled_back(reason.to_string()).await;
    }

    async fn record_rolled_back(&self, reason: String) {
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
        {
            let _ = self.parent.remove_file(temp_name);
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

    async fn commit(self: Box<Self>) -> Result<(), EffectCommitError> {
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

        let err = prepared.commit().await.unwrap_err();
        match &err {
            EffectCommitError::AppliedButDurabilityFailed(_) => {}
            other => panic!("expected AppliedButDurabilityFailed, got {other:?}"),
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
}
