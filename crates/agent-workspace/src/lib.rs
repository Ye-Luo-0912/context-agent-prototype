use std::path::{Component, Path, PathBuf};

use agent_contracts::{AgentError, AgentResult, RunId};
use serde::Serialize;
use tokio::fs;
use uuid::Uuid;

/// One recorded workspace mutation, appended to `.focus-agent/changes.jsonl`.
/// Kept bounded: old content is captured only for small files so the journal
/// stays reviewable without duplicating the whole repository.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceChange {
    pub timestamp_ms: u64,
    pub tool: String,
    pub path: String,
    pub action: String,
    pub bytes_before: u64,
    pub bytes_after: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_content: Option<String>,
}

/// Old-content capture limit for `WorkspaceChange` (bounded journal).
pub const CHANGE_CAPTURE_LIMIT: usize = 256 * 1024;

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
        let state_dir = root.join(".focus-agent");
        fs::create_dir_all(state_dir.join("artifacts"))
            .await
            .map_err(|e| AgentError::Io(format!("create state dir: {e}")))?;
        let state_dir = normalize_canonical(
            fs::canonicalize(&state_dir)
                .await
                .map_err(|e| AgentError::Io(format!("canonicalize state dir: {e}")))?,
        );
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
    pub async fn resolve_relative(&self, relative: impl AsRef<Path>) -> AgentResult<PathBuf> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }

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
        for part in clean.components() {
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
                    tail.clear();
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tail.push(part.as_os_str().to_owned());
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

    /// Begin a journaled, atomic mutation. The target is confined like
    /// `resolve_mutation` and its old content is captured (bounded) as the
    /// journal backup before anything is written.
    pub async fn begin_mutation(
        &self,
        tool: &str,
        action: &str,
        relative: impl AsRef<Path>,
    ) -> AgentResult<MutationTransaction> {
        let target = self.resolve_mutation(relative).await?;
        let mut bytes_before = 0u64;
        let mut old_content = None;
        match fs::metadata(&target).await {
            Ok(meta) => {
                bytes_before = meta.len();
                if meta.is_file() && meta.len() as usize <= CHANGE_CAPTURE_LIMIT {
                    let bytes = fs::read(&target)
                        .await
                        .map_err(|e| AgentError::Io(format!("read {}: {e}", target.display())))?;
                    old_content = String::from_utf8(bytes).ok();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(AgentError::Io(format!("inspect {}: {e}", target.display())));
            }
        }
        Ok(MutationTransaction {
            workspace: self.clone(),
            relative: display_relative(&self.root, &target),
            target,
            tool: tool.to_string(),
            action: action.to_string(),
            bytes_before,
            old_content,
            temp: None,
            finished: false,
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
    /// (`.focus-agent/changes.jsonl`). Mutating tools call this so every write
    /// is visible and reviewable.
    pub async fn record_change(&self, change: WorkspaceChange) -> AgentResult<()> {
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

/// A single journaled, atomic file mutation.
///
/// Ordering contract: the change journal entry is written *before* the target
/// file is swapped in, so a journal failure never leaves the target
/// half-mutated and a caller retrying the tool cannot double-apply a mutation
/// that already landed. The content is staged in a hidden temporary file next
/// to the target and swapped in with an atomic rename.
pub struct MutationTransaction {
    workspace: Workspace,
    target: PathBuf,
    relative: String,
    tool: String,
    action: String,
    bytes_before: u64,
    old_content: Option<String>,
    temp: Option<PathBuf>,
    finished: bool,
}

impl MutationTransaction {
    /// Stage `content`, record the journal entry, then atomically replace the
    /// target. On any failure the temporary file is removed and the target is
    /// left untouched.
    pub async fn apply(mut self, content: &[u8]) -> AgentResult<()> {
        let parent = self.target.parent().ok_or_else(|| {
            AgentError::InvalidRequest(format!("no parent for {}", self.target.display()))
        })?;
        fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::Io(format!("create parent dir: {e}")))?;
        let file_name = self
            .target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let temp = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));

        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = options
            .open(&temp)
            .await
            .map_err(|e| AgentError::Io(format!("create temp file: {e}")))?;
        use tokio::io::AsyncWriteExt;
        if let Err(e) = file.write_all(content).await {
            let _ = fs::remove_file(&temp).await;
            return Err(AgentError::Io(format!("write temp file: {e}")));
        }
        if let Err(e) = file.flush().await {
            let _ = fs::remove_file(&temp).await;
            return Err(AgentError::Io(format!("flush temp file: {e}")));
        }
        drop(file);
        self.temp = Some(temp.clone());

        let change = WorkspaceChange {
            timestamp_ms: now_ms(),
            tool: self.tool.clone(),
            path: self.relative.clone(),
            action: self.action.clone(),
            bytes_before: self.bytes_before,
            bytes_after: content.len() as u64,
            old_content: self.old_content.take(),
        };
        if let Err(e) = self.workspace.record_change(change).await {
            let _ = fs::remove_file(&temp).await;
            self.temp = None;
            return Err(e);
        }

        if let Err(e) = fs::rename(&temp, &self.target).await {
            let _ = fs::remove_file(&temp).await;
            self.temp = None;
            return Err(AgentError::Io(format!(
                "commit {}: {e}",
                self.target.display()
            )));
        }
        self.temp = None;
        self.finished = true;
        Ok(())
    }
}

impl Drop for MutationTransaction {
    fn drop(&mut self) {
        if !self.finished
            && let Some(temp) = &self.temp
        {
            let _ = std::fs::remove_file(temp);
        }
    }
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
        assert!(workspace.resolve_relative("C:\\x").await.is_err());
        assert!(workspace.resolve_relative("\\x").await.is_err());
    }

    #[tokio::test]
    async fn resolves_clean_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let resolved = workspace.resolve_relative("src/lib.rs").await.unwrap();
        assert_eq!(resolved, dir.path().join("src/lib.rs"));
        let root = workspace.resolve_relative(".").await.unwrap();
        assert_eq!(root, workspace.root());
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
        let record: serde_json::Value = serde_json::from_str(journal.trim()).unwrap();
        assert_eq!(record["tool"], "fs.write");
        assert_eq!(record["action"], "write");
        assert_eq!(record["bytes_before"], 0);
        assert_eq!(record["bytes_after"], 5);

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
        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(record["bytes_before"], 5);
        assert_eq!(record["old_content"], "hello");
    }

    #[tokio::test]
    async fn failed_apply_leaves_target_untouched() {
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
    }
}
