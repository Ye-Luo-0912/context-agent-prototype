//! `edit.replace` — exact, occurrence-aware text replacement.
//!
//! The intended primary mutating tool (instead of whole-file writes). It is
//! explicit by construction: with no `occurrence`/`replace_all`, the old text
//! must match exactly once. Every successful edit is recorded in the
//! workspace change journal (`.focus-agent/changes.jsonl`).

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::Tool;

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub struct EditReplaceTool {
    workspace: Workspace,
}

impl EditReplaceTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ReplaceArgs {
    path: String,
    old: String,
    new: String,
    /// 1-based occurrence to replace (requires `old` to appear at least that many times).
    #[serde(default)]
    occurrence: Option<usize>,
    #[serde(default)]
    replace_all: bool,
}

fn display_relative(workspace: &Workspace, path: &std::path::Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[async_trait]
impl Tool for EditReplaceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit.replace".into(),
            description: "Replace an exact substring in a workspace file (occurrence-aware). Records the change in the workspace change journal.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "old", "new"],
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string", "description": "Exact text to replace (must match exactly once unless occurrence/replace_all is given)"},
                    "new": {"type": "string"},
                    "occurrence": {"type": "integer", "minimum": 1, "description": "1-based occurrence to replace"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"}
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
        let args: ReplaceArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("edit.replace args: {e}")))?;
        if args.old.is_empty() {
            return Err(AgentError::InvalidRequest(
                "edit.replace `old` must not be empty".into(),
            ));
        }
        if args.occurrence.is_some() && args.replace_all {
            return Err(AgentError::InvalidRequest(
                "edit.replace: `occurrence` and `replace_all` are mutually exclusive".into(),
            ));
        }

        let path = self.workspace.resolve_mutation(&args.path).await?;
        let metadata = fs::metadata(&path)
            .await
            .map_err(|e| AgentError::Io(format!("metadata {}: {e}", path.display())))?;
        if metadata.len() > MAX_FILE_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file is {} bytes; edit.replace is limited to {} bytes",
                metadata.len(),
                MAX_FILE_BYTES
            )));
        }

        let original = fs::read_to_string(&path)
            .await
            .map_err(|e| AgentError::Io(format!("read {}: {e}", path.display())))?;
        let occurrences: Vec<_> = original.match_indices(&args.old).collect();
        let count = occurrences.len();

        let updated = if args.replace_all {
            original.replace(&args.old, &args.new)
        } else {
            match args.occurrence {
                Some(n) if n >= 1 && n <= count => {
                    let mut result = String::with_capacity(original.len());
                    let (start, end) = {
                        let (idx, matched) = occurrences[n - 1];
                        (idx, idx + matched.len())
                    };
                    result.push_str(&original[..start]);
                    result.push_str(&args.new);
                    result.push_str(&original[end..]);
                    result
                }
                Some(n) => {
                    return Err(AgentError::InvalidRequest(format!(
                        "edit.replace: occurrence {n} requested but `old` appears only {count} times"
                    )));
                }
                None if count == 1 => original.replacen(&args.old, &args.new, 1),
                None => {
                    return Err(AgentError::InvalidRequest(format!(
                        "edit.replace: `old` appears {count} times; pass `occurrence` or `replace_all` to disambiguate"
                    )));
                }
            }
        };

        if updated == original {
            return Ok(ToolOutput {
                call_id: call_id.into(),
                tool_name: "edit.replace".into(),
                ok: true,
                summary: "no-op: replacement text equals original".into(),
                model_content: format!("no change: {}", display_relative(&self.workspace, &path)),
                artifact_ref: None,
                context_action: None,
                metadata: json!({"changed": false, "occurrences": count}),
            });
        }

        let transaction = self
            .workspace
            .begin_mutation("edit.replace", "replace", &args.path)
            .await?;
        transaction.apply(updated.as_bytes()).await?;

        Ok(ToolOutput {
            call_id: call_id.into(),
            tool_name: "edit.replace".into(),
            ok: true,
            summary: format!(
                "replaced {} occurrence(s) in {}",
                count.min(1),
                display_relative(&self.workspace, &path)
            ),
            model_content: format!(
                "edit applied: {} ({} occurrence(s) of old text; bytes {} -> {})",
                display_relative(&self.workspace, &path),
                count,
                metadata.len(),
                updated.len()
            ),
            artifact_ref: None,
            context_action: None,
            metadata: json!({"changed": true, "occurrences": count, "bytes_before": metadata.len(), "bytes_after": updated.len()}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};
    use serde_json::json;
    use tokio::fs as tfs;

    fn request(run_id: RunId, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "edit.replace".into(),
                arguments: args,
            },
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn replace_single_occurrence_and_journal_it() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("lib.rs");
        tfs::write(&file, "fn auth() {}\nfn main() { auth(); }\n")
            .await
            .unwrap();

        let tool = EditReplaceTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(
            run_id,
            json!({"path": "lib.rs", "old": "auth() {}", "new": "auth() -> bool { true }"}),
        );
        let output = tool
            .execute(run_id, "c", request.call.arguments, request.cancel)
            .await
            .unwrap();
        assert!(output.ok);

        let content = tfs::read_to_string(&file).await.unwrap();
        assert!(content.contains("auth() -> bool { true }"));
        assert!(
            !content.contains("auth() {}"),
            "old snippet fully replaced: {content}"
        );

        // The change journal recorded the mutation with old content captured.
        let journal = tfs::read_to_string(dir.path().join(".focus-agent/changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value = serde_json::from_str(journal.trim()).unwrap();
        assert_eq!(record["tool"], "edit.replace");
        assert_eq!(record["path"], "lib.rs");
        assert!(
            record["old_content"]
                .as_str()
                .unwrap()
                .contains("fn auth() {}")
        );
    }

    #[tokio::test]
    async fn ambiguous_match_requires_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "a b a\n").await.unwrap();

        let tool = EditReplaceTool::new(workspace.clone());
        let run_id = RunId::new();
        let result = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"path": "f.txt", "old": "a", "new": "x"}))
                    .call
                    .arguments,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err(), "ambiguous match must be rejected");

        // With replace_all it succeeds.
        let request = request(
            run_id,
            json!({"path": "f.txt", "old": "a", "new": "x", "replace_all": true}),
        );
        let output = tool
            .execute(run_id, "c", request.call.arguments, request.cancel)
            .await
            .unwrap();
        assert!(output.ok);
        assert_eq!(tfs::read_to_string(&file).await.unwrap(), "x b x\n");
    }
}
