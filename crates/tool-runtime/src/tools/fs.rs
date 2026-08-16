use std::path::Path;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, Effect, RunId, ToolOutcome, ToolOutput, ToolRisk,
    ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::{Tool, content_digest};

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
    /// Opaque paging token returned by a previous `fs.list` call. When
    /// present, the next page is served from that call's snapshot artifact
    /// instead of a fresh directory scan, so paging stays consistent even
    /// if the directory changes between pages.
    #[serde(default)]
    cursor: Option<String>,
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
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
                    "cursor": {"type": "string", "description": "Opaque token from a previous fs.list result; serves the next page from that call's snapshot"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ListArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.list args: {e}")))?;
        let limit = args.limit.clamp(1, MAX_LIST_ENTRIES);
        if let Some(cursor) = args.cursor.as_deref() {
            return self
                .page_from_snapshot(run_id, call_id, cursor, limit)
                .await;
        }
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
        let (cursor, has_more) = match &artifact_ref {
            Some(reference) => (Some(format!("{reference}#{limit}")), true),
            None => (None, false),
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

        Ok(ToolOutcome::Value(ToolOutput {
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
            metadata: json!({
                "entry_count": entries.len(),
                "returned": visible.len(),
                "has_more": has_more,
                "cursor": cursor,
            }),
        }))
    }
}

impl FsListTool {
    /// Serve one page from a previous call's snapshot artifact (cursor is
    /// `<artifact_ref>#<offset>`). Pages come from the immutable snapshot,
    /// so later changes to the directory cannot cause duplicates or gaps
    /// between pages.
    async fn page_from_snapshot(
        &self,
        run_id: RunId,
        call_id: &str,
        cursor: &str,
        limit: usize,
    ) -> AgentResult<ToolOutcome> {
        use super::{parse_cursor, read_snapshot_lines};

        let (reference, offset) = parse_cursor(cursor)?;
        let lines = read_snapshot_lines(&self.workspace, run_id, reference).await?;
        if offset > lines.len() {
            return Err(AgentError::InvalidRequest(format!(
                "cursor is past the end of the snapshot ({offset} > {} lines)",
                lines.len()
            )));
        }
        let page: Vec<&str> = lines
            .iter()
            .skip(offset)
            .take(limit)
            .map(String::as_str)
            .collect();
        let next_offset = offset + page.len();
        let has_more = next_offset < lines.len();
        let next_cursor = has_more.then(|| format!("{reference}#{next_offset}"));

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.list".into(),
            ok: true,
            summary: format!(
                "listed entries {}-{} of {} (snapshot)",
                offset + 1,
                next_offset,
                lines.len()
            ),
            model_content: if page.is_empty() {
                "no more entries".to_string()
            } else {
                page.join("\n")
            },
            artifact_ref: Some(reference.to_string()),
            metadata: json!({
                "entry_count": lines.len(),
                "returned": page.len(),
                "has_more": has_more,
                "cursor": next_cursor,
            }),
        }))
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
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
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

        // Validation and open are fused into a directory-handle-relative
        // descent; the size check and the content read both go through the
        // pinned handle, so a link swap cannot redirect the read.
        let confined = self.workspace.confined_open_read(&args.path).await?;
        let metadata = confined.metadata().map_err(|e| {
            AgentError::Io(format!("metadata {}: {e}", confined.display().display()))
        })?;
        if metadata.len() > MAX_READ_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file is {} bytes; use a narrower/specialized tool for files above {} bytes",
                metadata.len(),
                MAX_READ_BYTES
            )));
        }

        use tokio::io::AsyncReadExt;
        let display_path = confined.display().to_path_buf();
        let mut file = confined.into_tokio();
        let mut text = String::new();
        file.read_to_string(&mut text)
            .await
            .map_err(|e| AgentError::Io(format!("read {}: {e}", display_path.display())))?;
        let lines: Vec<&str> = text.lines().collect();
        let start = args.start_line.saturating_sub(1).min(lines.len());
        let end = args.end_line.min(lines.len());
        let selected = lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6} | {}", start + offset + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: format!(
                "read lines {}-{} of {}",
                start + 1,
                end,
                display_relative(&self.workspace, &display_path)
            ),
            model_content: selected,
            artifact_ref: None,
            metadata: json!({
                "path": display_relative(&self.workspace, &display_path),
                "line_count": lines.len(),
                "bytes": metadata.len(),
                // The content revision (SHA-256 hex): stable for the same
                // bytes, changes with any edit — the patch tool's
                // `base_revision` precondition is checked against this.
                "revision": content_digest(text.as_bytes()),
            }),
        }))
    }
}

pub struct FsWriteTool {
    workspace: Workspace,
}

impl FsWriteTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }

    async fn execute_inner(
        &self,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
    ) -> AgentResult<ToolOutcome> {
        let args: WriteArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.write args: {e}")))?;
        let path = self.workspace.resolve_mutation(&args.path).await?;
        let transaction = self
            .workspace
            .begin_mutation("fs.write", "write", &args.path)
            .await?;
        // Computation is staged, the side effect is not applied yet: the
        // runtime owns the commit after the generation fence. Production
        // dispatches attach Core's stable identity; direct legacy tests can
        // still exercise the transaction primitive without one.
        let prepared = match effect_context {
            Some(context) => {
                transaction
                    .prepare_with_effect_context(args.content.as_bytes(), context)
                    .await?
            }
            None => transaction.prepare(args.content.as_bytes()).await?,
        };
        let effect: Box<dyn Effect> = Box::new(prepared);
        let relative = display_relative(&self.workspace, &path);
        let output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.write".into(),
            ok: true,
            summary: format!("wrote {} bytes to {}", args.content.len(), relative),
            model_content: format!("file updated: {relative}"),
            artifact_ref: None,
            metadata: json!({"bytes": args.content.len()}),
        };
        Ok(ToolOutcome::PreparedEffect { output, effect })
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
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        self.execute_inner(call_id, arguments, effect_context).await
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
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        // Writing a new file stages the mutation; the runtime would commit
        // it after validating the operation — the test plays that role.
        let outcome = write("notes.txt", "first").await.unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("fs.write must prepare a committed effect");
        };
        assert!(output.ok);
        assert!(
            matches!(
                effect.commit().await,
                agent_contracts::EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    ..
                }
            ),
            "the staged effect must commit durably"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("notes.txt"))
                .await
                .unwrap(),
            "first"
        );
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(record["kind"], "mutation_prepared");
        assert_eq!(record["tool"], "fs.write");
        assert_eq!(record["action"], "write");
        assert_eq!(record["path"], "notes.txt");
        assert_eq!(record["bytes_before"], 0);
        assert_eq!(record["bytes_after"], 5);

        // Overwriting captures the previous content as the journal backup.
        let outcome = write("notes.txt", "second").await.unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("fs.write must prepare a committed effect");
        };
        assert!(
            matches!(
                effect.commit().await,
                agent_contracts::EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    ..
                }
            ),
            "the staged effect must commit durably"
        );
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let record: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
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
            effect_context: None,
            cancel: CancellationToken::new(),
        };
        let result = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await;
        assert!(result.is_err(), "state dir writes must be rejected");
    }

    #[tokio::test]
    async fn fs_read_reports_a_stable_content_revision() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello revision\n").unwrap();
        let tool = FsReadTool::new(workspace.clone());
        let run_id = RunId::new();

        let read = |path: &str| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.read".into(),
                arguments: json!({"path": path}),
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        let first = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = first else {
            panic!("fs.read returns a plain value");
        };
        let revision_a = output.metadata["revision"].as_str().unwrap().to_string();
        assert_eq!(revision_a.len(), 64, "a full SHA-256 hex revision");
        assert_eq!(
            output.metadata["path"].as_str(),
            Some("notes.txt"),
            "fs.read must stamp the workspace-relative path; ingest cannot recover it from numbered lines"
        );

        // Same bytes, same revision.
        let second = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = second else {
            panic!("fs.read returns a plain value");
        };
        assert_eq!(
            output.metadata["revision"].as_str().unwrap(),
            revision_a,
            "reading the same content must not change the revision"
        );

        // Any edit changes the revision.
        std::fs::write(dir.path().join("notes.txt"), "hello revision!\n").unwrap();
        let third = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = third else {
            panic!("fs.read returns a plain value");
        };
        assert_ne!(
            output.metadata["revision"].as_str().unwrap(),
            revision_a,
            "an edited file must report a different revision"
        );

        // The revision is exactly the digest helper the patch tool will
        // use for its `base_revision` precondition.
        let bytes = std::fs::read(dir.path().join("notes.txt")).unwrap();
        assert_eq!(
            output.metadata["revision"].as_str().unwrap(),
            content_digest(&bytes)
        );
    }

    #[tokio::test]
    async fn fs_list_pages_a_consistent_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        // List a subdirectory so the runtime state dir (.focus-agent) does
        // not enter the count.
        std::fs::create_dir(dir.path().join("d")).unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join("d").join(format!("file-{i:02}.txt")), "x").unwrap();
        }
        let tool = FsListTool::new(workspace.clone());
        let run_id = RunId::new();

        let list = |args: Value| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.list".into(),
                arguments: args,
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        // Page 1: limit 4 of 10 entries → spills a snapshot + cursor.
        let first = list(json!({"path": "d", "limit": 4})).await.unwrap();
        let ToolOutcome::Value(output) = first else {
            panic!("fs.list returns a plain value");
        };
        assert_eq!(output.metadata["entry_count"], 10);
        assert_eq!(output.metadata["returned"], 4);
        assert_eq!(output.metadata["has_more"], true);
        assert!(
            output.artifact_ref.is_some(),
            "an overflowing listing must spill a snapshot"
        );
        let cursor = output.metadata["cursor"].as_str().unwrap().to_string();
        let first_content = output.model_content.clone();

        // The directory changes between pages — paging must not notice.
        std::fs::write(dir.path().join("d").join("zz-new.txt"), "x").unwrap();
        std::fs::remove_file(dir.path().join("d").join("file-00.txt")).unwrap();

        // Page 2 from the snapshot: exactly the next 4 snapshot entries,
        // no duplicates from page 1, no gaps, no new file leaking in.
        let second = list(json!({"path": "d", "limit": 4, "cursor": cursor}))
            .await
            .unwrap();
        let ToolOutcome::Value(output) = second else {
            panic!("fs.list returns a plain value");
        };
        let second_lines: Vec<&str> = output.model_content.lines().collect();
        assert_eq!(second_lines.len(), 4);
        assert_eq!(output.metadata["returned"], 4);
        assert_eq!(output.metadata["has_more"], true);
        let first_lines: Vec<&str> = first_content.lines().collect();
        assert!(
            second_lines.iter().all(|line| !first_lines.contains(line)),
            "pages must not overlap: {first_lines:?} vs {second_lines:?}"
        );
        assert!(
            !second_lines.iter().any(|line| line.contains("zz-new")),
            "the snapshot must not see later directory changes"
        );
        let cursor2 = output.metadata["cursor"].as_str().unwrap().to_string();

        // Page 3 drains the remaining 2 entries.
        let third = list(json!({"path": "d", "limit": 4, "cursor": cursor2}))
            .await
            .unwrap();
        let ToolOutcome::Value(output) = third else {
            panic!("fs.list returns a plain value");
        };
        assert_eq!(output.metadata["returned"], 2);
        assert_eq!(output.metadata["has_more"], false);
        assert!(output.metadata["cursor"].is_null());

        // A corrupted cursor (offset beyond the snapshot) is a clean error.
        let bad = format!("{}#9999", output.artifact_ref.as_deref().unwrap());
        let result = list(json!({"path": "d", "limit": 4, "cursor": bad})).await;
        assert!(result.is_err(), "a cursor past the snapshot must error");
    }
}
