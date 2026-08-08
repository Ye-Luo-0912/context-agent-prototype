use std::path::Path;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::Tool;

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_READ_LINES: usize = 400;
const MAX_LIST_ENTRIES: usize = 2_000;

pub struct FsListTool {
    workspace: Workspace,
}

impl FsListTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: String,
    #[serde(default = "default_list_limit")]
    limit: usize,
}

fn default_list_limit() -> usize {
    200
}

#[async_trait]
impl Tool for FsListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.list".into(),
            description: "List files/directories inside the current workspace.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                }
            }),
            risk: ToolRisk::ReadOnly,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutput> {
        let args: ListArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.list args: {e}")))?;
        let path = self.workspace.resolve_relative(&args.path).await?;
        let mut reader = fs::read_dir(&path)
            .await
            .map_err(|e| AgentError::Io(format!("list {}: {e}", path.display())))?;

        let mut entries = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(format!("read directory: {e}")))?
        {
            if entries.len() >= MAX_LIST_ENTRIES {
                break;
            }
            let metadata = entry.metadata().await.ok();
            let kind = metadata
                .as_ref()
                .map(|m| {
                    if m.is_dir() {
                        "dir"
                    } else if m.is_file() {
                        "file"
                    } else {
                        "other"
                    }
                })
                .unwrap_or("unknown");
            entries.push(format!("{kind}\t{}", entry.file_name().to_string_lossy()));
        }
        entries.sort();

        let limit = args.limit.clamp(1, MAX_LIST_ENTRIES);
        let visible = entries.iter().take(limit).cloned().collect::<Vec<_>>();
        let full = entries.join("\n");
        let artifact_ref = if entries.len() > limit {
            Some(
                self.workspace
                    .write_artifact(run_id, "fs-list", "txt", full.as_bytes())
                    .await?,
            )
        } else {
            None
        };
        let truncated_note = artifact_ref
            .as_ref()
            .map(|r| {
                format!(
                    "\n... {} more entries; full listing: {r}",
                    entries.len() - visible.len()
                )
            })
            .unwrap_or_default();

        Ok(ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.list".into(),
            ok: true,
            summary: format!(
                "listed {} entries in {}",
                entries.len(),
                display_relative(&self.workspace, &path)
            ),
            model_content: format!("{}{}", visible.join("\n"), truncated_note),
            artifact_ref,
            context_action: None,
            metadata: json!({"entry_count": entries.len()}),
        })
    }
}

pub struct FsReadTool {
    workspace: Workspace,
}

impl FsReadTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default = "default_start_line")]
    start_line: usize,
    #[serde(default = "default_end_line")]
    end_line: usize,
}

fn default_start_line() -> usize {
    1
}
fn default_end_line() -> usize {
    200
}

#[async_trait]
impl Tool for FsReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.read".into(),
            description: "Read a bounded line range from a UTF-8 text file in the workspace."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                }
            }),
            risk: ToolRisk::ReadOnly,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutput> {
        let args: ReadArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.read args: {e}")))?;
        if args.start_line == 0 || args.end_line < args.start_line {
            return Err(AgentError::InvalidRequest("invalid line range".into()));
        }
        if args.end_line - args.start_line + 1 > MAX_READ_LINES {
            return Err(AgentError::InvalidRequest(format!(
                "fs.read is limited to {MAX_READ_LINES} lines per call"
            )));
        }

        let path = self.workspace.resolve_relative(&args.path).await?;
        let metadata = fs::metadata(&path)
            .await
            .map_err(|e| AgentError::Io(format!("metadata {}: {e}", path.display())))?;
        if metadata.len() > MAX_READ_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file is {} bytes; use a narrower/specialized tool for files above {} bytes",
                metadata.len(),
                MAX_READ_BYTES
            )));
        }

        let text = fs::read_to_string(&path)
            .await
            .map_err(|e| AgentError::Io(format!("read {}: {e}", path.display())))?;
        let lines: Vec<&str> = text.lines().collect();
        let start = args.start_line.saturating_sub(1).min(lines.len());
        let end = args.end_line.min(lines.len());
        let selected = lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6} | {}", start + offset + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: format!(
                "read lines {}-{} of {}",
                start + 1,
                end,
                display_relative(&self.workspace, &path)
            ),
            model_content: selected,
            artifact_ref: None,
            context_action: None,
            metadata: json!({"line_count": lines.len(), "bytes": metadata.len()}),
        })
    }
}

pub struct FsWriteTool {
    workspace: Workspace,
}

impl FsWriteTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for FsWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.write".into(),
            description: "Write/replace a UTF-8 text file inside the workspace.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }
            }),
            risk: ToolRisk::WorkspaceWrite,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutput> {
        let args: WriteArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.write args: {e}")))?;
        let path = self.workspace.resolve_mutation(&args.path).await?;
        let transaction = self
            .workspace
            .begin_mutation("fs.write", "write", &args.path)
            .await?;
        transaction.apply(args.content.as_bytes()).await?;

        Ok(ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.write".into(),
            ok: true,
            summary: format!(
                "wrote {} bytes to {}",
                args.content.len(),
                display_relative(&self.workspace, &path)
            ),
            model_content: format!("file updated: {}", display_relative(&self.workspace, &path)),
            artifact_ref: None,
            context_action: None,
            metadata: json!({"bytes": args.content.len()}),
        })
    }
}

fn display_relative(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};
    use serde_json::json;

    #[tokio::test]
    async fn fs_write_journals_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace.clone());
        let run_id = RunId::new();

        let write = |path: &str, content: &str| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.write".into(),
                arguments: json!({"path": path, "content": content}),
            };
            async move {
                tool.execute(run_id, "c", call.arguments, CancellationToken::new())
                    .await
            }
        };

        // Writing a new file records the mutation with zero bytes before.
        let output = write("notes.txt", "first").await.unwrap();
        assert!(output.ok);
        assert_eq!(
            fs::read_to_string(dir.path().join("notes.txt"))
                .await
                .unwrap(),
            "first"
        );
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value = serde_json::from_str(journal.trim()).unwrap();
        assert_eq!(record["tool"], "fs.write");
        assert_eq!(record["action"], "write");
        assert_eq!(record["path"], "notes.txt");
        assert_eq!(record["bytes_before"], 0);
        assert_eq!(record["bytes_after"], 5);

        // Overwriting captures the previous content as the journal backup.
        write("notes.txt", "second").await.unwrap();
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(record["old_content"], "first");
    }

    #[tokio::test]
    async fn fs_write_rejects_state_dir_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace);
        let run_id = RunId::new();
        let request = ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.write".into(),
                arguments: json!({"path": ".focus-agent/traces.jsonl", "content": "x"}),
            },
            cancel: CancellationToken::new(),
        };
        let result = tool
            .execute(run_id, "c", request.call.arguments, request.cancel)
            .await;
        assert!(result.is_err(), "state dir writes must be rejected");
    }
}
