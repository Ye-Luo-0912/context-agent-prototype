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

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, OperationEffectContext, RunId, ToolOutcome,
    ToolSpec,
};
use agent_process::kill_process_tree;
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

/// 每处理这么多目录项就协作让出一次，让 `search.grep` 的取消能打断 walk。
const WALK_YIELD_EVERY: u32 = 32;

/// Depth-first walk collecting regular files under `root`, honoring
/// `IGNORED_DIRS` and stopping once `budget` files have been collected.
///
/// `cancel` 为 `Some` 时在目录项之间检查 token 并协作让出；已收集的路径留在
/// `out` 中。`code.symbols` 传 `None`，walk 语义与取消前一致。
pub(crate) async fn walk_files(
    root: &Path,
    out: &mut Vec<std::path::PathBuf>,
    budget: &mut usize,
    cancel: Option<&CancellationToken>,
) -> AgentResult<()> {
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    let mut entries_since_yield = 0u32;
    while let Some(dir) = stack.pop() {
        if *budget == 0 {
            return Ok(());
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
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
            if let Some(token) = cancel {
                if token.is_cancelled() {
                    return Ok(());
                }
                entries_since_yield += 1;
                if entries_since_yield >= WALK_YIELD_EVERY {
                    entries_since_yield = 0;
                    tokio::task::yield_now().await;
                }
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

/// Parse a snapshot paging cursor: `<artifact_ref>#<line_offset>`. Identity
/// locators never contain `#`, so the last `#` is the offset; a crafted
/// cursor with extra `#`s fails `ArtifactLocator::parse`.
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
    run_id: RunId,
    reference: &str,
) -> AgentResult<Vec<String>> {
    let (_normalized, confined) = workspace.open_artifact_for_run(reference, run_id).await?;
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

/// spawn 成功后立刻记下 PID；失败时由调用方杀树，避免留下无证据孩子。
pub(crate) fn persist_spawned_process(
    workspace: &Workspace,
    effect_context: &Option<OperationEffectContext>,
    child: &tokio::process::Child,
) -> AgentResult<u32> {
    let pid = child
        .id()
        .ok_or_else(|| AgentError::Tool("spawned process has no pid".into()))?;
    if let Some(context) = effect_context {
        workspace.record_process_spawn(context, pid)?;
    }
    Ok(pid)
}

pub(crate) fn persist_process_exit(
    workspace: &Workspace,
    pid: u32,
    exit_code: Option<i32>,
) -> AgentResult<()> {
    workspace.record_process_exit(pid, exit_code)
}

pub(crate) fn abandon_spawned_process(child: &mut tokio::process::Child) {
    kill_process_tree(child.id().unwrap_or(0));
}

#[async_trait]
pub(crate) trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome>;
}
