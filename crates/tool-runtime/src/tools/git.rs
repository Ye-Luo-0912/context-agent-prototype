//! Read-only git tools (`git.status`, `git.diff`).
//!
//! Both run git inside the workspace, bound the model-facing output to a tail,
//! and store the full output as an artifact when it overflows.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use super::Tool;

const GIT_TIMEOUT_MS: u64 = 20_000;
const MODEL_OUTPUT_CHARS: usize = 12_000;

pub struct GitStatusTool {
    workspace: Workspace,
}

impl GitStatusTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

pub struct GitDiffTool {
    workspace: Workspace,
}

impl GitDiffTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct DiffArgs {
    /// Optional path filter; `--staged`-style flags are not accepted (kept read-only).
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    staged: bool,
}

async fn run_git(
    workspace: &Workspace,
    args: &[&str],
    run_id: RunId,
    call_id: &str,
    tool_name: &str,
) -> AgentResult<ToolOutput> {
    let output = timeout(
        Duration::from_millis(GIT_TIMEOUT_MS),
        Command::new("git")
            .args(args)
            .current_dir(workspace.root())
            .output(),
    )
    .await
    .map_err(|_| AgentError::Tool(format!("git {args:?} timed out")))?
    .map_err(|e| AgentError::Tool(format!("run git {args:?}: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.trim().is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n[stderr]\n{stderr}")
    };

    let ok = output.status.success();
    if !ok && combined.trim().is_empty() {
        return Ok(ToolOutput {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            ok: false,
            summary: format!("git {args:?} failed (exit={:?})", output.status.code()),
            model_content: "(empty output; is this a git repository?)".into(),
            artifact_ref: None,
            metadata: json!({"exit_code": output.status.code()}),
        });
    }

    let truncated = combined.chars().count() > MODEL_OUTPUT_CHARS;
    let bounded = tail_chars(&combined, MODEL_OUTPUT_CHARS);
    let artifact_ref = if truncated {
        Some(
            workspace
                .write_artifact(
                    run_id,
                    tool_name.trim_start_matches("git."),
                    "txt",
                    combined.as_bytes(),
                )
                .await?,
        )
    } else {
        None
    };

    Ok(ToolOutput {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        ok,
        summary: format!(
            "git {args:?} (exit={}, {} bytes)",
            output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |v| v.to_string()),
            combined.len()
        ),
        model_content: if truncated {
            format!(
                "{}\n\nFull output: {}",
                bounded,
                artifact_ref.as_deref().unwrap_or("")
            )
        } else {
            bounded
        },
        artifact_ref,
        metadata: json!({"exit_code": output.status.code(), "bytes": combined.len()}),
    })
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    format!(
        "...[{} chars omitted; showing tail]\n{}",
        skip,
        text.chars().skip(skip).collect::<String>()
    )
}

#[async_trait]
impl Tool for GitStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git.status".into(),
            description: "Show `git status --short` for the workspace (read-only).".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            risk: ToolRisk::ReadOnly,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        _arguments: Value,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutput> {
        run_git(
            &self.workspace,
            &["status", "--short"],
            run_id,
            call_id,
            "git.status",
        )
        .await
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git.diff".into(),
            description:
                "Show `git diff` (optionally --staged or a path) for the workspace (read-only)."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "staged": {"type": "boolean"}
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
        let args: DiffArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("git.diff args: {e}")))?;
        let mut git_args: Vec<String> = vec!["diff".into()];
        if args.staged {
            git_args.push("--staged".into());
        }
        if let Some(path) = args.path
            && !path.is_empty()
        {
            git_args.push(path);
        }
        let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        run_git(&self.workspace, &refs, run_id, call_id, "git.diff").await
    }
}
