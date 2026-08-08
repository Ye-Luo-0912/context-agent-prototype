mod context;
mod edit;
mod fs;
mod git;
mod search;
mod shell;

pub(crate) use context::ContextDirectiveTool;
pub(crate) use edit::EditReplaceTool;
pub(crate) use fs::{FsListTool, FsReadTool, FsWriteTool};
pub(crate) use git::{GitDiffTool, GitStatusTool};
pub(crate) use search::SearchGrepTool;
pub(crate) use shell::ShellExecTool;

use agent_contracts::{AgentResult, CancellationToken, RunId, ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

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
