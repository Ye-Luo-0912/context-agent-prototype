mod context;
mod edit;
mod fs;
mod git;
mod patch;
mod search;
mod shell;

pub(crate) use context::ContextManageTool;
pub(crate) use edit::EditReplaceTool;
pub(crate) use fs::{FsListTool, FsReadTool, FsWriteTool};
pub(crate) use git::{GitDiffTool, GitStatusTool};
pub(crate) use patch::EditPatchTool;
pub(crate) use search::SearchGrepTool;
pub(crate) use shell::ShellExecTool;

use agent_contracts::{AgentResult, CancellationToken, RunId, ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

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
