//! `shell.exec` — process execution with streaming, bounded output.
//!
//! The process's stdout/stderr are read line-by-line into a bounded ring
//! buffer (for the model-facing tail) and appended incrementally to an
//! artifact file (so arbitrarily large logs never live in memory or in the
//! prompt). The command is killed on timeout or on request cancellation.

use std::{collections::VecDeque, process::Stdio};

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::Command,
    sync::mpsc,
    time::Duration,
};

use super::Tool;

const MAX_TIMEOUT_MS: u64 = 120_000;
const MODEL_OUTPUT_CHARS: usize = 12_000;
const BUFFER_LINES: usize = 200;
const MAX_LINE_CHARS: usize = 4_000;

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

enum StreamLine {
    Stdout(String),
    Stderr(String),
}

#[async_trait]
impl Tool for ShellExecTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell.exec".into(),
            description: "Execute a shell command with the workspace as cwd. Output streams to an artifact; only a bounded tail reaches the model.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 120000}
                }
            }),
            risk: ToolRisk::ProcessExecution,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
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

        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Tool(format!("spawn command: {e}")))?;

        let (artifact_ref, artifact_file) = self
            .workspace
            .create_artifact(run_id, "shell", "log")
            .await?;
        let mut artifact = BufWriter::new(artifact_file);

        // Two reader tasks push (bounded) lines into one channel; the main
        // loop selects on lines, cancellation, and the timeout.
        let (line_tx, mut line_rx) = mpsc::channel::<StreamLine>(512);
        if let Some(stdout) = child.stdout.take() {
            let tx = line_tx.clone();
            let mut lines = BufReader::new(stdout).lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(StreamLine::Stdout(line)).await.is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = line_tx.clone();
            let mut lines = BufReader::new(stderr).lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(StreamLine::Stderr(line)).await.is_err() {
                        break;
                    }
                }
            });
        }
        drop(line_tx);

        let mut tail: VecDeque<String> = VecDeque::with_capacity(BUFFER_LINES + 1);
        let mut total_lines = 0usize;
        let mut total_chars = 0usize;

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
                    let _ = child.kill().await;
                    outcome = "cancelled";
                    break;
                }
                _ = &mut deadline => {
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
                        Some(StreamLine::Stdout(line)) => {
                            record_line(&line, &mut tail, &mut artifact, &mut total_lines, &mut total_chars).await?;
                        }
                        Some(StreamLine::Stderr(line)) => {
                            record_line(&line, &mut tail, &mut artifact, &mut total_lines, &mut total_chars).await?;
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
        artifact
            .flush()
            .await
            .map_err(|e| AgentError::Io(format!("flush artifact: {e}")))?;

        let omitted = total_lines.saturating_sub(tail.len());
        let mut model_content = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        if model_content.chars().count() > MODEL_OUTPUT_CHARS {
            model_content = tail_chars(&model_content, MODEL_OUTPUT_CHARS);
        }
        if omitted > 0 {
            model_content =
                format!("[{total_lines} lines total; {omitted} omitted]\n{model_content}");
        }

        let exit_code = exited.as_ref().and_then(|status| status.code());
        let ok = outcome == "completed" && exited.as_ref().is_some_and(|s| s.success());
        let exit_text = exit_code.map(|v| v.to_string()).unwrap_or_else(|| {
            if outcome == "completed" {
                "signal".into()
            } else {
                outcome.into()
            }
        });

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "shell.exec".into(),
            ok,
            summary: format!(
                "command {outcome} (exit={exit_text}, {total_lines} lines, ~{} KB in artifact)",
                total_chars / 1024
            ),
            model_content: format!("{model_content}\n\nFull output: {artifact_ref}"),
            artifact_ref: Some(artifact_ref),
            metadata: json!({
                "exit_code": exit_code,
                "timeout_ms": timeout_ms,
                "lines": total_lines,
                "outcome": outcome,
            }),
        }))
    }
}

async fn record_line(
    line: &str,
    tail: &mut VecDeque<String>,
    artifact: &mut BufWriter<tokio::fs::File>,
    total_lines: &mut usize,
    total_chars: &mut usize,
) -> AgentResult<()> {
    *total_lines += 1;
    *total_chars += line.len();
    artifact
        .write_all(line.as_bytes())
        .await
        .map_err(|e| AgentError::Io(format!("append artifact: {e}")))?;
    artifact
        .write_all(b"\n")
        .await
        .map_err(|e| AgentError::Io(format!("append artifact: {e}")))?;

    let bounded_line: String = if line.chars().count() > MAX_LINE_CHARS {
        let truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
        format!("{truncated}...[line truncated]")
    } else {
        line.to_string()
    };
    if tail.len() >= BUFFER_LINES {
        tail.pop_front();
    }
    tail.push_back(bounded_line);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ToolExecutionRequest;
    use serde_json::json;

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
            cancel: CancellationToken::new(),
        };
        let output = tool
            .execute(run_id, "c", request.call.arguments, request.cancel)
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
}
