mod artifact;
mod code;
mod context;
mod edit;
mod fs;
mod git;
mod patch;
mod process;
mod search;
mod session;
mod shell;
mod stream;
mod task;

pub(crate) use artifact::ArtifactReadTool;
pub(crate) use code::{CodeDiagnosticsTool, CodeSymbolsTool};
pub(crate) use context::ContextManageTool;
pub(crate) use edit::EditReplaceTool;
pub(crate) use fs::{FsListTool, FsReadTool, FsWriteTool};
pub(crate) use git::{GitDiffTool, GitStatusTool};
pub(crate) use patch::EditPatchTool;
pub(crate) use process::ProcessRunTool;
pub(crate) use search::SearchGrepTool;
pub(crate) use session::{ProcessSession, ProcessSessionTool};
pub(crate) use shell::ShellExecTool;
pub(crate) use task::TaskCompleteTool;

use agent_contracts::{AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolSpec};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use tokio::fs as tokio_fs;
use tokio::io::AsyncReadExt;

/// Directories the workspace scanners skip by default, so build artifacts
/// and runtime state never pollute the working set.
pub(crate) const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".focus-agent",
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    ".idea",
    ".vscode",
];

pub(crate) fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name)
}

/// Depth-first walk collecting regular files under `root`, honoring
/// `IGNORED_DIRS` and stopping once `budget` files have been collected.
pub(crate) async fn walk_files(
    root: &Path,
    out: &mut Vec<std::path::PathBuf>,
    budget: &mut usize,
) -> AgentResult<()> {
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if *budget == 0 {
            return Ok(());
        }
        let mut reader = tokio_fs::read_dir(&dir)
            .await
            .map_err(|e| AgentError::Io(format!("read dir {}: {e}", dir.display())))?;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(format!("read dir entry: {e}")))?
        {
            if *budget == 0 {
                return Ok(());
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry
                .file_type()
                .await
                .map_err(|e| AgentError::Io(format!("file type: {e}")))?;
            if file_type.is_dir() {
                if is_ignored_dir(&name) {
                    continue;
                }
                stack.push(entry.path());
            } else if file_type.is_file() {
                *budget = budget.saturating_sub(1);
                out.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Render a path relative to the workspace with forward slashes, so tool
/// results are stable across platforms.
pub(crate) fn display_relative(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The file-revision identity shared by the read and patch tools: the
/// SHA-256 of the file's bytes as a lowercase hex string. `fs.read`
/// reports it (`revision`), and `edit.patch` requires it as
/// `base_revision`, so an edit is refused unless the file is exactly the
/// revision the model based its change on (TOOLS-05 file-revision
/// semantics).
pub(crate) fn content_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Upper bound for reading a spilled snapshot back during cursor paging.
/// Snapshots are constructed under their own caps (fs.list ≤ 2000 entries,
/// search.grep ≤ 1000 hits), so this is a defensive ceiling, not a budget.
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// Parse a snapshot paging cursor: `<artifact_ref>#<line_offset>`. The
/// artifact reference is built from safe filename characters and never
/// contains `#`, so the last `#` is unambiguous; a crafted cursor with
/// extra `#`s is caught by `artifact_relative_path`'s fragment check.
pub(crate) fn parse_cursor(cursor: &str) -> AgentResult<(&str, usize)> {
    let (reference, offset) = cursor.rsplit_once('#').ok_or_else(|| {
        AgentError::InvalidRequest(format!(
            "malformed cursor (expected <artifact_ref>#<offset>): {cursor:?}"
        ))
    })?;
    let offset: usize = offset
        .parse()
        .map_err(|_| AgentError::InvalidRequest(format!("malformed cursor offset: {cursor:?}")))?;
    Ok((reference, offset))
}

/// Read a spilled snapshot artifact (bounded) and return its lines. Cursor
/// paging serves every page from the *same immutable snapshot*, so the
/// paging is consistent: changes to the underlying directory or file set
/// between pages cannot cause duplicates or gaps.
pub(crate) async fn read_snapshot_lines(
    workspace: &Workspace,
    reference: &str,
) -> AgentResult<Vec<String>> {
    let relative = workspace.artifact_relative_path(reference)?;
    let confined = workspace.confined_open_read(&relative).await?;
    let file = confined.into_tokio();
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| AgentError::Io(format!("read snapshot artifact: {e}")))?;
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        return Err(AgentError::InvalidRequest(format!(
            "snapshot artifact exceeds {MAX_SNAPSHOT_BYTES} bytes"
        )));
    }
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(String::from)
        .collect())
}

#[async_trait]
pub(crate) trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome>;
}
