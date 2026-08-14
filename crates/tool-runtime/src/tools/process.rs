//! `process.run` — structured argv process execution.
//!
//! The TOOLS-06 alternative to the raw `shell.exec` string: the command is
//! an explicit argv vector, so there is no shell to parse (and no shell
//! injection to guard). cwd is a workspace-relative directory, env is an
//! explicit override map layered on the inherited environment, and the
//! timeout/cancel paths kill the whole process tree (not just the direct
//! child). Output streams into the same bounded tail + artifact shape as
//! `shell.exec`.

use std::{collections::HashMap, process::Stdio};

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
const MAX_ARGV: usize = 64;
const MAX_ARG_CHARS: usize = 16_384;
const MAX_ENV_KEYS: usize = 64;
const MAX_ENV_VALUE_CHARS: usize = 16_384;

pub struct ProcessRunTool {
    workspace: Workspace,
}

impl ProcessRunTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ProcessArgs {
    argv: Vec<String>,
    /// Workspace-relative working directory for the process (defaults to
    /// the workspace root).
    #[serde(default)]
    cwd: Option<String>,
    /// Explicit environment overrides layered on the inherited environment.
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[async_trait]
impl Tool for ProcessRunTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process.run".into(),
            description: "Run a program with an explicit argv (no shell), optionally in a workspace-relative cwd with explicit env overrides. A bounded output prefix streams to an artifact; only a bounded tail reaches the model. Timeout/cancel kill the whole process tree.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["argv"],
                "properties": {
                    "argv": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 64,
                        "items": {"type": "string"},
                        "description": "Program and arguments, passed verbatim (no shell parsing)"
                    },
                    "cwd": {"type": "string", "description": "Workspace-relative working directory"},
                    "env": {"type": "object", "description": "Explicit environment overrides (layered on the inherited environment)"},
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
        let args: ProcessArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("process.run args: {e}")))?;
        if args.argv.is_empty() {
            return Err(AgentError::InvalidRequest(
                "process.run requires a non-empty argv".into(),
            ));
        }
        if args.argv.len() > MAX_ARGV {
            return Err(AgentError::InvalidRequest(format!(
                "process.run argv is limited to {MAX_ARGV} arguments"
            )));
        }
        if args
            .argv
            .iter()
            .any(|arg| arg.chars().count() > MAX_ARG_CHARS)
        {
            return Err(AgentError::InvalidRequest(format!(
                "process.run argv arguments are limited to {MAX_ARG_CHARS} chars"
            )));
        }
        if args.env.len() > MAX_ENV_KEYS {
            return Err(AgentError::InvalidRequest(format!(
                "process.run env is limited to {MAX_ENV_KEYS} keys"
            )));
        }
        if args
            .env
            .values()
            .any(|value| value.chars().count() > MAX_ENV_VALUE_CHARS)
        {
            return Err(AgentError::InvalidRequest(format!(
                "process.run env values are limited to {MAX_ENV_VALUE_CHARS} chars"
            )));
        }
        let timeout_ms = args.timeout_ms.clamp(100, MAX_TIMEOUT_MS);

        // The cwd is confined to the workspace (lexical `..` rejection
        // lives in the workspace's path resolution); the default is the
        // workspace root.
        let cwd = match &args.cwd {
            Some(relative) => self.workspace.resolve_relative(relative).await?,
            None => self.workspace.root().to_path_buf(),
        };

        let mut command = Command::new(&args.argv[0]);
        command
            .args(&args.argv[1..])
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &args.env {
            command.env(key, value);
        }

        // Make the process a process-group leader on Unix so a
        // cancellation or timeout can kill its whole tree (`kill(-pgid)`),
        // not just the direct child.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Tool(format!("spawn {}: {e}", args.argv[0])))?;
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
                .create_artifact(run_id, "process", "log")
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
                    // Kill the whole process tree, not just the direct
                    // child: a descendant that outlives the cancel is an
                    // avoidable stale mutation.
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
        let cwd_text = cwd
            .strip_prefix(self.workspace.root())
            .unwrap_or(&cwd)
            .to_string_lossy()
            .replace('\\', "/");
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
            tool_name: "process.run".into(),
            ok,
            summary: format!(
                "process {outcome} (exit={exit_text}, {total_lines} lines, ~{} KB output, ~{} KB captured{truncation_summary})",
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
                "cwd": if cwd_text.is_empty() { "." } else { &cwd_text },
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ToolExecutionRequest;
    use serde_json::json;

    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("process.run must return a plain value"),
        }
    }

    /// An argv that echoes its argument; platform-independent.
    fn echo_argv(text: &str) -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "echo".into(), text.into()]
        }
        #[cfg(not(windows))]
        {
            vec!["echo".into(), text.into()]
        }
    }

    #[tokio::test]
    async fn process_run_executes_argv_without_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "process.run".into(),
                arguments: json!({"argv": echo_argv("argv no shell")}),
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
            output.model_content.contains("argv no shell"),
            "the arg must arrive verbatim: {}",
            output.model_content
        );
        assert_eq!(output.metadata["cwd"], ".");
    }

    #[tokio::test]
    async fn process_run_honors_cwd_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let run_id = RunId::new();

        // Print the cwd and an env override through the platform echo.
        #[cfg(windows)]
        let argv: Vec<String> = vec![
            "cmd".into(),
            "/C".into(),
            "echo %CD% && echo %TOOLS_06_VAR%".into(),
        ];
        #[cfg(not(windows))]
        let argv: Vec<String> = vec!["sh".into(), "-c".into(), "pwd && echo $TOOLS_06_VAR".into()];

        let request = ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "process.run".into(),
                arguments: json!({
                    "argv": argv,
                    "cwd": "sub",
                    "env": {"TOOLS_06_VAR": "injected"},
                    "timeout_ms": 15000
                }),
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
        assert_eq!(output.metadata["cwd"], "sub");
        assert!(
            output.model_content.contains("injected"),
            "the env override must reach the process: {}",
            output.model_content
        );
    }

    #[tokio::test]
    async fn process_run_rejects_escaping_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "process.run".into(),
                arguments: json!({"argv": echo_argv("x"), "cwd": "../escape"}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        };
        let result = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await;
        assert!(
            result.is_err(),
            "a cwd escaping the workspace must be refused"
        );
    }

    #[tokio::test]
    async fn process_run_cancellation_kills_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());

        #[cfg(windows)]
        let argv = vec![
            "ping".to_string(),
            "-n".to_string(),
            "20".to_string(),
            "127.0.0.1".to_string(),
        ];
        #[cfg(not(windows))]
        let argv = vec!["sleep".to_string(), "30".to_string()];

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let output = tool
                .execute(
                    RunId::new(),
                    "c",
                    json!({"argv": argv, "timeout_ms": 60000}),
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
        assert!(!output.ok, "cancelled process must report failure");
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
