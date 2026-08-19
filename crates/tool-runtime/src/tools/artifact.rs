//! `artifact.read` — bounded line-range fetch from a run artifact.
//!
//! Tools that spill large output (fs.list, search.grep, capability.search,
//! git, shell/process logs) hand the model an opaque `artifact://`
//! reference instead of the content (invariant 4: raw tool output is not
//! prompt history). This tool is the read side of that contract: it
//! resolves the reference, confined to the run artifact store, and returns
//! a bounded, numbered line range with paging metadata — so the model can
//! walk a spilled snapshot one page at a time without ever guessing
//! filesystem paths.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk,
    ToolSemanticRole, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use super::Tool;

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_READ_LINES: usize = 400;

pub struct ArtifactReadTool {
    workspace: Workspace,
}

impl ArtifactReadTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ArtifactReadArgs {
    reference: String,
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
impl Tool for ArtifactReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "artifact.read".into(),
            description: "Read a bounded line range from an artifact:// reference returned by another tool (spilled listings, grep hit sets, process logs).".into(),
            input_schema: json!({
                "type": "object",
                "required": ["reference"],
                "properties": {
                    "reference": {"type": "string", "description": "artifact:// reference from a previous tool result"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
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
        let args: ArtifactReadArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("artifact.read args: {e}")))?;
        if args.start_line == 0 || args.end_line < args.start_line {
            return Err(AgentError::InvalidRequest("invalid line range".into()));
        }
        if args.end_line - args.start_line + 1 > MAX_READ_LINES {
            return Err(AgentError::InvalidRequest(format!(
                "artifact.read is limited to {MAX_READ_LINES} lines per call"
            )));
        }

        // The reference resolves to a cleaned relative path confined to the
        // run artifact store; the open itself goes through the pinned
        // directory-handle descent, so a link swap cannot redirect the read.
        let (_normalized, confined) = self
            .workspace
            .open_artifact_for_run(&args.reference, run_id)
            .await?;
        let display_path = confined.display().to_path_buf();

        // Bounded read: artifacts may be append-only logs that grow between
        // a metadata probe and the read, so cap the read itself and refuse
        // anything larger than the bound instead of trusting a size check.
        let file = confined.into_tokio();
        let mut bytes = Vec::new();
        file.take(MAX_READ_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| AgentError::Io(format!("read artifact: {e}")))?;
        if bytes.len() as u64 > MAX_READ_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "artifact is {} bytes; larger artifacts cannot be read in full (use a narrower range or a specialized tool)",
                bytes.len()
            )));
        }
        // Artifacts can carry non-UTF-8 bytes (process logs); show them
        // lossily rather than failing the whole read.
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let start = args.start_line.saturating_sub(1).min(lines.len());
        let end = args.end_line.min(lines.len());
        let selected = lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6} | {}", start + offset + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        let has_more = end < lines.len();
        let next_start_line = if has_more { end + 1 } else { end };

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "artifact.read".into(),
            ok: true,
            summary: format!(
                "read lines {}-{} of {} ({} lines total)",
                start + 1,
                end,
                display_relative(&self.workspace, &display_path),
                lines.len()
            ),
            model_content: if selected.is_empty() {
                "no lines in range".to_string()
            } else {
                selected
            },
            artifact_ref: Some(args.reference),
            metadata: json!({
                "total_lines": lines.len(),
                "bytes": bytes.len(),
                "returned": end - start,
                "has_more": has_more,
                "next_start_line": next_start_line,
            }),
        }))
    }
}

fn display_relative(workspace: &Workspace, path: &std::path::Path) -> String {
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

    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("artifact.read must return a plain value"),
        }
    }

    async fn tool_with_artifact() -> (ArtifactReadTool, tempfile::TempDir, RunId, String) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let reference = workspace
            .write_artifact(run_id, "grep", "txt", b"alpha\nbeta\ngamma\ndelta\n")
            .await
            .unwrap();
        (ArtifactReadTool::new(workspace), dir, run_id, reference)
    }

    fn request(run_id: RunId, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "artifact.read".into(),
                arguments: args,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn reads_a_bounded_range_with_paging_metadata() {
        let (tool, _dir, run_id, reference) = tool_with_artifact().await;

        // Default range is lines 1..=200: the whole 4-line artifact.
        let output = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"reference": reference}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        assert!(output.model_content.contains("alpha"));
        assert!(output.model_content.contains("delta"));
        assert_eq!(output.metadata["total_lines"], 4);
        assert_eq!(output.metadata["has_more"], false);

        // A narrow range reports the paging cursor for the next page.
        let output = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": 2, "end_line": 3}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.model_content.contains("beta"));
        assert!(output.model_content.contains("gamma"));
        assert!(!output.model_content.contains("delta"));
        assert_eq!(output.metadata["has_more"], true);
        assert_eq!(output.metadata["next_start_line"], 4);
    }

    #[tokio::test]
    async fn refuses_invalid_ranges_and_non_artifact_references() {
        let (tool, _dir, run_id, reference) = tool_with_artifact().await;

        let bad_range = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": 5, "end_line": 3}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(bad_range.is_err(), "end before start must be refused");

        let too_wide = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": 1, "end_line": 401}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(too_wide.is_err(), "over-wide ranges must be refused");

        let not_artifact = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"reference": "artifact://src/main.rs"}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(
            not_artifact.is_err(),
            "workspace files are not readable as artifacts"
        );

        let not_a_scheme = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"reference": "https://example.com/x"}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(not_a_scheme.is_err(), "foreign schemes must be refused");
    }

    #[tokio::test]
    async fn missing_artifact_is_a_clean_error() {
        let (tool, _dir, run_id, _reference) = tool_with_artifact().await;
        let output = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": format!(
                        "artifact://v1/{run_id}/grep/0000000000000000000000000000000000000000000000000000000000000000"
                    )}),
                )
                .call.arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(output.is_err(), "a missing artifact must error cleanly");
    }

    #[tokio::test]
    async fn refuses_another_runs_artifact() {
        let (tool, _dir, owner_run, reference) = tool_with_artifact().await;
        let other_run = RunId::new();

        let output = tool
            .execute(
                other_run,
                "c",
                request(other_run, json!({"reference": reference}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await;

        assert_ne!(owner_run, other_run);
        assert!(
            output.is_err(),
            "artifact refs are scoped to their owning run"
        );
    }
}
