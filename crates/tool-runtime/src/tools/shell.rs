//! `shell.exec` — shell-string process execution with streaming, bounded
//! output.
//!
//! The process's stdout/stderr are read as bounded byte fragments into a
//! bounded ring buffer (for the model-facing tail). A bounded raw prefix is
//! captured as an artifact; overflow is still drained but is not stored. The
//! command is killed on timeout or on request cancellation.
//! `process.run` is the structured argv alternative; the raw shell string
//! stays as the controlled escape hatch (TOOLS-06).

use std::process::Stdio;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_process::kill_process_tree;
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::BufWriter, process::Command, sync::mpsc, time::Duration};

use super::Tool;
use super::stream::{
    MAX_ARTIFACT_BYTES, StreamCapture, StreamChunk, spawn_stderr_reader, spawn_stdout_reader,
};

const MAX_TIMEOUT_MS: u64 = 120_000;

pub struct ShellExecTool {
    workspace: Workspace,
}

impl ShellExecTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[async_trait]
impl Tool for ShellExecTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell.exec".into(),
            description: "Execute a shell command with the workspace as cwd. A bounded output prefix streams to an artifact; only a bounded tail reaches the model.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 120000}
                }
            }),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ShellArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("shell.exec args: {e}")))?;
        let timeout_ms = args.timeout_ms.clamp(100, MAX_TIMEOUT_MS);

        #[cfg(windows)]
        let mut command = {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(&args.command);
            cmd
        };

        #[cfg(not(windows))]
        let mut command = {
            let mut cmd = Command::new("sh");
            cmd.arg("-lc").arg(&args.command);
            cmd
        };

        command.current_dir(self.workspace.root());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);

        // Make the shell a process-group leader on Unix so a cancellation
        // or timeout can kill its whole tree (`kill(-pgid)`), not just the
        // direct shell — a `&` background job must not survive its caller
        // and keep mutating after the operation was cancelled.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Tool(format!("spawn command: {e}")))?;
        let pid = match super::persist_spawned_process(&self.workspace, &effect_context, &child) {
            Ok(pid) => pid,
            Err(error) => {
                super::abandon_spawned_process(&mut child);
                let _ = child.kill().await;
                return Err(error);
            }
        };

        let mut artifact = BufWriter::new(
            self.workspace
                .create_artifact(run_id, "shell", "log")
                .await?,
        );

        // Two fixed-buffer readers push bounded line fragments into one
        // bounded channel; a missing newline can never grow an allocation.
        let (line_tx, mut line_rx) = mpsc::channel::<StreamChunk>(512);
        if let Some(stdout) = child.stdout.take() {
            spawn_stdout_reader(stdout, line_tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_reader(stderr, line_tx.clone());
        }
        drop(line_tx);

        let mut capture = StreamCapture::new();

        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        // After the process exits, keep draining pipe remnants for a short
        // grace window (background children can hold the pipe open).
        let grace = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(grace);

        let mut exited: Option<std::process::ExitStatus> = None;
        let mut grace_started = false;
        let mut outcome: &str = "completed";

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // Kill the whole process tree, not just the shell: a
                    // descendant that outlives the cancel is an avoidable
                    // stale mutation (the M12 boundary).
                    kill_process_tree(child.id().unwrap_or(0));
                    let _ = child.kill().await;
                    outcome = "cancelled";
                    break;
                }
                _ = &mut deadline => {
                    kill_process_tree(child.id().unwrap_or(0));
                    let _ = child.kill().await;
                    outcome = "timed out";
                    break;
                }
                status = child.wait(), if exited.is_none() => {
                    exited = Some(status.map_err(|e| AgentError::Tool(format!("wait: {e}")))?);
                    grace_started = true;
                }
                _ = &mut grace, if grace_started => break,
                line = line_rx.recv() => {
                    match line {
                        Some(line) => {
                            capture.record(line, &mut artifact).await?;
                        }
                        None => {
                            // All output drained; the process may still be
                            // exiting, so reap it before reporting a status.
                            if exited.is_none() {
                                exited = Some(child.wait().await.map_err(|e| AgentError::Tool(format!("wait: {e}")))?);
                            }
                            break;
                        }
                    }
                }
            }
        }
        if exited.is_none() {
            exited = Some(
                child
                    .wait()
                    .await
                    .map_err(|e| AgentError::Tool(format!("wait: {e}")))?,
            );
        }
        super::persist_process_exit(
            &self.workspace,
            pid,
            exited.as_ref().and_then(|status| status.code()),
        )?;
        let artifact_ref = self.workspace.seal_buffered_artifact(artifact).await?;

        let model_content = capture.model_tail();
        let total_lines = capture.total_lines();
        let total_bytes = capture.total_bytes();
        let artifact_bytes = capture.artifact_bytes();
        let artifact_truncated = capture.artifact_truncated();

        let exit_code = exited.as_ref().and_then(|status| status.code());
        let ok = outcome == "completed" && exited.as_ref().is_some_and(|s| s.success());
        let exit_text = exit_code.map(|v| v.to_string()).unwrap_or_else(|| {
            if outcome == "completed" {
                "signal".into()
            } else {
                outcome.into()
            }
        });
        let artifact_note = if artifact_truncated {
            format!(
                "Artifact capture truncated at {MAX_ARTIFACT_BYTES} bytes; remaining output was drained but not stored. Captured prefix: {artifact_ref}"
            )
        } else {
            format!("Full output: {artifact_ref}")
        };
        let truncation_summary = if artifact_truncated {
            ", artifact truncated"
        } else {
            ""
        };

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "shell.exec".into(),
            ok,
            summary: format!(
                "command {outcome} (exit={exit_text}, {total_lines} lines, ~{} KB output, ~{} KB captured{truncation_summary})",
                total_bytes / 1024,
                artifact_bytes / 1024,
            ),
            model_content: format!("{model_content}\n\n{artifact_note}"),
            artifact_ref: Some(artifact_ref),
            metadata: json!({
                "exit_code": exit_code,
                "timeout_ms": timeout_ms,
                "lines": total_lines,
                "output_bytes": total_bytes,
                "artifact_bytes": artifact_bytes,
                "artifact_limit_bytes": MAX_ARTIFACT_BYTES,
                "artifact_truncated": artifact_truncated,
                "outcome": outcome,
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ToolExecutionRequest;
    use serde_json::json;

    use crate::tools::stream::MODEL_OUTPUT_CHARS;

    /// Unwrap a plain tool value (shell.exec never stages an effect).
    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("shell.exec must return a plain value"),
        }
    }

    fn long_command() -> String {
        #[cfg(windows)]
        {
            "for /L %i in (1,1,1000) do @echo line %i".to_string()
        }
        #[cfg(not(windows))]
        {
            "for i in $(seq 1 1000); do echo line $i; done".to_string()
        }
    }

    #[tokio::test]
    async fn shell_bounds_model_output_and_writes_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ShellExecTool::new(workspace.clone());
        let run_id = RunId::new();

        let request = ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "shell.exec".into(),
                arguments: json!({"command": long_command(), "timeout_ms": 20000}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        };
        let output = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok, "command failed: {}", output.summary);
        assert!(
            output.artifact_ref.is_some(),
            "large output must be an artifact"
        );
        assert!(
            output.model_content.len() < MODEL_OUTPUT_CHARS + 256,
            "model content exceeded the bound"
        );
        assert!(
            output.model_content.contains("omitted"),
            "expected an omitted-lines note in the tail"
        );
        assert!(
            output.model_content.contains("line 1000"),
            "tail should end with the last lines"
        );
    }

    #[tokio::test]
    async fn shell_honors_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ShellExecTool::new(workspace.clone());

        #[cfg(windows)]
        let command = "ping -n 20 127.0.0.1".to_string();
        #[cfg(not(windows))]
        let command = "sleep 30".to_string();

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let output = tool
                .execute(
                    RunId::new(),
                    "c",
                    json!({"command": command, "timeout_ms": 60000}),
                    None,
                    cancel_for_task,
                )
                .await
                .unwrap();
            (output, started.elapsed())
        });

        tokio::time::sleep(Duration::from_millis(400)).await;
        cancel.cancel();

        let (output, elapsed) = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("tool did not stop after cancellation")
            .unwrap();
        let output = value(output);
        assert!(!output.ok, "cancelled command must report failure");
        assert!(
            output.summary.contains("cancel"),
            "summary should mention cancellation: {}",
            output.summary
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "cancellation took too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn shell_cancellation_kills_descendants() {
        // A cancelled shell must not leave its background children alive:
        // the command starts a long-lived foreground process that also
        // spawned a background descendant rewriting a heartbeat file every
        // ~50 ms. After the cancel, the counter must freeze — the whole
        // process tree is dead, not just the direct shell (the M12
        // boundary: a cancelled operation produces no avoidable stale
        // mutation).
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ShellExecTool::new(workspace.clone());
        let heartbeat = dir.path().join("descendant-heartbeat.txt");

        // A foreground loop that itself spawns a per-iteration child
        // (`ping`) is the descendant: the tree kill must stop the loop and
        // the in-flight child, not just the direct shell. The command
        // carries no quotes — on Windows `Command::arg` would escape any
        // `"` for CreateProcess, and cmd.exe does not honor `\"`, so nested
        // quotes would never reach the loop.
        #[cfg(windows)]
        let command = format!(
            "for /l %i in (1,1,6000) do (echo tick>> {} & ping -n 1 -w 50 127.0.0.1 >nul)",
            heartbeat.display()
        );
        #[cfg(not(windows))]
        let command = format!(
            "(while true; do echo tick >> '{}'; sleep 0.05; done) >/dev/null 2>&1 & sleep 300",
            heartbeat.display()
        );

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let handle = tokio::spawn(async move {
            tool.execute(
                RunId::new(),
                "c",
                json!({"command": command, "timeout_ms": 60000}),
                None,
                cancel_for_task,
            )
            .await
            .unwrap()
        });

        // Wait until the descendant's heartbeat visibly advances: the
        // background writer is alive before we cancel anything.
        let baseline = std::fs::read_to_string(&heartbeat).unwrap_or_default();
        let mut saw_advance = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if std::fs::read_to_string(&heartbeat).unwrap_or_default() != baseline {
                saw_advance = true;
                break;
            }
        }

        // Always cancel, even if the heartbeat never advanced: a failed
        // assertion must not leave the command running until its 300 s
        // timeout — the test binary would hang on the spawned task.
        cancel.cancel();
        assert!(
            saw_advance,
            "the descendant must write the heartbeat while alive"
        );

        let output = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("tool did not stop after cancellation")
            .unwrap();
        let output = value(output);
        assert!(!output.ok, "cancelled command must report failure");
        assert!(
            output.summary.contains("cancel"),
            "summary should mention cancellation: {}",
            output.summary
        );

        // The heartbeat must freeze: the descendant is dead, not a
        // background process still producing side effects.
        let frozen = std::fs::read_to_string(&heartbeat).unwrap_or_default();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            std::fs::read_to_string(&heartbeat).unwrap_or_default(),
            frozen,
            "the descendant must be terminated after cancellation — the heartbeat stopped"
        );
    }
}
