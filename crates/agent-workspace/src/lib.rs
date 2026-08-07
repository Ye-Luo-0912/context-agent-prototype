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

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    state_dir: PathBuf,
}

impl Workspace {
    pub async fn open(root: impl AsRef<Path>) -> AgentResult<Self> {
        let root = fs::canonicalize(root.as_ref())
            .await
            .map_err(|e| AgentError::Io(format!("canonicalize workspace: {e}")))?;
        let state_dir = root.join(".focus-agent");
        fs::create_dir_all(state_dir.join("artifacts"))
            .await
            .map_err(|e| AgentError::Io(format!("create state dir: {e}")))?;
        Ok(Self { root, state_dir })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Resolve a user-provided path without allowing absolute paths or `..` escape.
    pub fn resolve_relative(&self, relative: impl AsRef<Path>) -> AgentResult<PathBuf> {
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
        Ok(self.root.join(clean))
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
