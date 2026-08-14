//! Read-only git tools (`git.status`, `git.diff`).
//!
//! Both run git inside the workspace, bound the model-facing output to a tail,
//! and store the full output as an artifact when it overflows.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_process::kill_process_tree;
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
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
    cancel: CancellationToken,
) -> AgentResult<ToolOutput> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(workspace.root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping on timeout/cancellation must not leave the direct git
        // child running in the background.
        .kill_on_drop(true);

    // Same process-tree guarantee as `shell.exec`: a cancelled or
    // timed-out git must not leave descendants (hooks, aliases spawning
    // subprocesses) alive — every process path kills the same way.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|e| AgentError::Tool(format!("run git {args:?}: {e}")))?;
    let pid = child.id().unwrap_or(0);
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Tool("git stdout unavailable".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Tool("git stderr unavailable".into()))?;

    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            kill_process_tree(pid);
            let _ = child.kill().await;
            return Err(AgentError::Cancelled);
        }
        result = timeout(Duration::from_millis(GIT_TIMEOUT_MS), child.wait()) => {
            match result {
                Ok(Ok(status)) => status,
                Ok(Err(e)) => return Err(AgentError::Tool(format!("run git {args:?}: {e}"))),
                Err(_) => {
                    kill_process_tree(pid);
                    let _ = child.kill().await;
                    return Err(AgentError::Tool(format!("git {args:?} timed out")));
                }
            }
        }
    };

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .await
        .map_err(|e| AgentError::Tool(format!("read git stdout: {e}")))?;
    stderr
        .read_to_end(&mut stderr_bytes)
        .await
        .map_err(|e| AgentError::Tool(format!("read git stderr: {e}")))?;

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    let combined = if stderr.trim().is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n[stderr]\n{stderr}")
    };

    let ok = status.success();
    if !ok && combined.trim().is_empty() {
        return Ok(ToolOutput {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            ok: false,
            summary: format!("git {args:?} failed (exit={:?})", status.code()),
            model_content: "(empty output; is this a git repository?)".into(),
            artifact_ref: None,
            metadata: json!({"exit_code": status.code()}),
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
            status
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
        metadata: json!({"exit_code": status.code(), "bytes": combined.len()}),
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
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        _arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let output = run_git(
            &self.workspace,
            &["status", "--short"],
            run_id,
            call_id,
            "git.status",
            cancel,
        )
        .await?;
        Ok(ToolOutcome::Value(output))
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
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: DiffArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("git.diff args: {e}")))?;
        let mut git_args: Vec<String> = vec!["diff".into()];
        if args.staged {
            git_args.push("--staged".into());
        }
        if let Some(path) = args.path
            && !path.is_empty()
        {
            // Reject absolute/parent escapes and links that leave the
            // workspace. The explicit `--` then makes even an option-looking
            // filename a pathspec rather than a git option.
            self.workspace.resolve_relative(&path).await?;
            git_args.push("--".into());
            git_args.push(path);
        }
        let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let output = run_git(&self.workspace, &refs, run_id, call_id, "git.diff", cancel).await?;
        Ok(ToolOutcome::Value(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git_command(root: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git executable is required for git tool tests")
    }

    #[tokio::test]
    async fn diff_treats_option_looking_path_as_a_path_without_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let init = git_command(dir.path(), &["init", "--quiet"]);
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        tokio::fs::write(dir.path().join("tracked.txt"), "before\n")
            .await
            .unwrap();
        let add = git_command(dir.path(), &["add", "--", "tracked.txt"]);
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        tokio::fs::write(dir.path().join("tracked.txt"), "after\n")
            .await
            .unwrap();

        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitDiffTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "diff-call",
                json!({"path": "--output=leak.patch"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ToolOutcome::Value(output) if output.ok));
        assert!(
            !dir.path().join("leak.patch").exists(),
            "an option-looking path must not activate git's --output option"
        );
    }

    #[tokio::test]
    async fn diff_rejects_paths_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitDiffTool::new(workspace);
        let result = tool
            .execute(
                RunId::new(),
                "diff-call",
                json!({"path": "../outside.txt"}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AgentError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn status_honors_preexisting_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitStatusTool::new(workspace);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = tool
            .execute(RunId::new(), "status-call", json!({}), None, cancel)
            .await;
        assert!(matches!(result, Err(AgentError::Cancelled)));
    }
}
