//! `process.session` — long-running process sessions (start/poll/stop).
//!
//! The session protocol: `start` launches an argv process and
//! returns a session id; `poll` reports its status and drains its output
//! (bounded tail + artifact, exactly like the one-shot tools); `stop`
//! kills the whole process tree and reaps the session. The child lives in
//! the shared session registry between calls, so a session survives the
//! individual tool calls that drive it.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, ContextItemId, RunId, ToolOutcome, ToolOutput,
    ToolRisk, ToolSemanticRole, ToolSpec,
};
use agent_process::kill_process_tree;
use agent_workspace::{ArtifactDraft, Workspace};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    process::Command,
    sync::{Mutex, mpsc},
};

use super::Tool;
use super::stream::{
    MAX_ARTIFACT_BYTES, StreamCapture, StreamChunk, spawn_stderr_reader, spawn_stdout_reader,
};

const MAX_ARGV: usize = 64;
const MAX_ARG_CHARS: usize = 16_384;
const MAX_ENV_KEYS: usize = 64;
const MAX_ENV_VALUE_CHARS: usize = 16_384;
const MAX_SESSIONS: usize = 16;

/// Bounded wait for a child's remaining buffered output after it exits:
/// process exit and pipe EOF are two different events, and a poll must not
/// report "exited" until the readers are at EOF so the model-facing tail
/// is complete. When the bound bites (a reader wedged or heavily delayed,
/// e.g. a grandchild holding the pipe open) the poll reports "running"
/// instead — the output keeps accumulating in the session's tail, so a
/// later poll drains it; "exited" therefore always means the tail is
/// complete.
const EXIT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// A running (or finished-but-unreaped) session: the live child plus the
/// drained-output state. The output reader tasks keep pushing lines into
/// the channel; `poll` drains them into the bounded tail and the artifact.
pub(crate) struct ProcessSession {
    child: tokio::process::Child,
    pid: u32,
    rx: mpsc::Receiver<StreamChunk>,
    capture: StreamCapture,
    artifact_ref: String,
    /// 同一个 pinned draft 句柄贯穿整个 session；stop 时才封成 digest。
    artifact: BufWriter<ArtifactDraft>,
}

impl ProcessSession {
    async fn drain(&mut self) -> AgentResult<(usize, bool)> {
        let mut new_lines = 0usize;
        let mut exited = false;
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    if self.capture.record(chunk, &mut self.artifact).await? {
                        new_lines = new_lines.saturating_add(1);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    exited = true;
                    break;
                }
            }
        }
        if let Ok(Some(_status)) = self.child.try_wait() {
            // The child exited, but its pipes may still hold buffered
            // output the reader tasks have not delivered yet: process exit
            // and pipe EOF are two different events. Block (bounded) until
            // the channel disconnects — the readers' EOF — so a poll that
            // reports "exited" always carries the complete model-facing
            // tail. If the bound bites (a reader wedged or heavily
            // delayed), report "running" instead of "exited": the output
            // already recorded stays in the tail and a later poll keeps
            // draining, so no output is ever fabricated as complete.
            loop {
                match tokio::time::timeout(EXIT_DRAIN_TIMEOUT, self.rx.recv()).await {
                    Ok(Some(chunk)) => {
                        if self.capture.record(chunk, &mut self.artifact).await? {
                            new_lines = new_lines.saturating_add(1);
                        }
                    }
                    Ok(None) => break, // all readers at EOF: the tail is complete
                    Err(_) => {
                        // Bounded: never block a poll forever, but never
                        // claim "exited" with a possibly-incomplete tail.
                        return Ok((new_lines, false));
                    }
                }
            }
            exited = true;
        }
        Ok((new_lines, exited))
    }
}

pub struct ProcessSessionTool {
    workspace: Workspace,
    sessions: Arc<Mutex<HashMap<String, ProcessSession>>>,
}

impl ProcessSessionTool {
    pub fn new(
        workspace: Workspace,
        sessions: Arc<Mutex<HashMap<String, ProcessSession>>>,
    ) -> Self {
        Self {
            workspace,
            sessions,
        }
    }
}

#[derive(Deserialize)]
struct SessionArgs {
    action: String,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    session_id: Option<String>,
}

#[async_trait]
impl Tool for ProcessSessionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process.session".into(),
            description: "Manage a long-running process session: start (argv, no shell) returns a session id; poll reports status and drains output into a bounded tail/artifact prefix; stop kills the whole process tree and reaps the session.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {"type": "string", "enum": ["start", "poll", "stop"]},
                    "argv": {"type": "array", "minItems": 1, "maxItems": 64, "items": {"type": "string"}, "description": "start: program and arguments, passed verbatim"},
                    "cwd": {"type": "string", "description": "start: workspace-relative working directory"},
                    "env": {"type": "object", "description": "start: explicit environment overrides"},
                    "session_id": {"type": "string", "description": "poll/stop: the session id returned by start"}
                }
            }),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
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
        let args: SessionArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| AgentError::InvalidRequest(format!("process.session args: {e}")))?;
        match args.action.as_str() {
            "start" => {
                self.start(run_id, call_id, &arguments, args, effect_context, cancel)
                    .await
            }
            "poll" => self.poll(call_id, args).await,
            "stop" => self.stop(call_id, args).await,
            other => Err(AgentError::InvalidRequest(format!(
                "process.session: unknown action '{other}'"
            ))),
        }
    }
}

impl ProcessSessionTool {
    async fn start(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: &Value,
        args: SessionArgs,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        if args.argv.is_empty() {
            return Err(AgentError::InvalidRequest(
                "process.session start requires a non-empty argv".into(),
            ));
        }
        if args.argv.len() > MAX_ARGV {
            return Err(AgentError::InvalidRequest(format!(
                "process.session argv is limited to {MAX_ARGV} arguments"
            )));
        }
        if args
            .argv
            .iter()
            .any(|arg| arg.chars().count() > MAX_ARG_CHARS)
        {
            return Err(AgentError::InvalidRequest(format!(
                "process.session argv arguments are limited to {MAX_ARG_CHARS} chars"
            )));
        }
        if args.env.len() > MAX_ENV_KEYS {
            return Err(AgentError::InvalidRequest(format!(
                "process.session env is limited to {MAX_ENV_KEYS} keys"
            )));
        }
        if args
            .env
            .values()
            .any(|value| value.chars().count() > MAX_ENV_VALUE_CHARS)
        {
            return Err(AgentError::InvalidRequest(format!(
                "process.session env values are limited to {MAX_ENV_VALUE_CHARS} chars"
            )));
        }
        let cwd = match &args.cwd {
            Some(relative) => self.workspace.resolve_relative(relative).await?,
            None => self.workspace.root().to_path_buf(),
        };

        {
            let sessions = self.sessions.lock().await;
            if sessions.len() >= MAX_SESSIONS {
                return Err(AgentError::InvalidRequest(format!(
                    "process.session is limited to {MAX_SESSIONS} concurrent sessions"
                )));
            }
        }

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
        #[cfg(unix)]
        command.process_group(0);

        super::require_process_effect_context(&effect_context, "process.session")?;
        super::require_covered_process_spawn(
            "process.session",
            arguments,
            &agent_contracts::exec_argv_intent(&args.argv),
        )?;

        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Tool(format!("spawn {}: {e}", args.argv[0])))?;
        let pid = match super::persist_spawned_process(
            &self.workspace,
            &effect_context,
            &child,
            "process.session",
        ) {
            Ok(pid) => pid,
            Err(error) => {
                super::abandon_spawned_process(&mut child);
                let _ = child.kill().await;
                return Err(error);
            }
        };

        // 会话存活期间发布 draft 定位符，stop 时再封口。
        let draft = self
            .workspace
            .create_artifact(run_id, "process-session", "log")
            .await?;
        let artifact_ref = draft.locator().to_string();

        let (line_tx, line_rx) = mpsc::channel::<StreamChunk>(512);
        if let Some(stdout) = child.stdout.take() {
            spawn_stdout_reader(stdout, line_tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_reader(stderr, line_tx.clone());
        }
        drop(line_tx);

        let session_id = ContextItemId::new().to_string();
        self.sessions.lock().await.insert(
            session_id.clone(),
            ProcessSession {
                child,
                pid,
                rx: line_rx,
                capture: StreamCapture::new(),
                artifact_ref: artifact_ref.clone(),
                artifact: BufWriter::new(draft),
            },
        );

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "process.session".into(),
            ok: true,
            summary: format!("session {session_id} started (pid {pid})"),
            model_content: format!(
                "session started: {session_id} (pid {pid})\nDrain output with process.session poll.",
            ),
            artifact_ref: Some(artifact_ref),
            metadata: json!({
                "action": "start",
                "session_id": session_id,
                "pid": pid,
                "output_bytes": 0,
                "artifact_bytes": 0,
                "artifact_limit_bytes": MAX_ARTIFACT_BYTES,
                "artifact_truncated": false,
            }),
        }))
    }

    async fn poll(&self, call_id: &str, args: SessionArgs) -> AgentResult<ToolOutcome> {
        let session_id = args.session_id.ok_or_else(|| {
            AgentError::InvalidRequest("process.session poll requires a session_id".into())
        })?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(&session_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!(
                "process.session: no session '{session_id}' (unknown or already stopped)"
            ))
        })?;

        let (new_lines, exited) = session.drain().await?;
        session
            .artifact
            .flush()
            .await
            .map_err(|e| AgentError::Io(format!("flush session artifact: {e}")))?;

        let (status, exit_code) = if exited {
            let code = session
                .child
                .wait()
                .await
                .ok()
                .and_then(|status| status.code());
            super::persist_process_exit(&self.workspace, session.pid, code)?;
            ("exited", code)
        } else {
            ("running", None)
        };
        let tail = session.capture.model_tail();
        let total_lines = session.capture.total_lines();
        let output_bytes = session.capture.total_bytes();
        let artifact_bytes = session.capture.artifact_bytes();
        let artifact_truncated = session.capture.artifact_truncated();
        let truncation_note = if artifact_truncated {
            format!(
                "\n\nArtifact capture truncated at {MAX_ARTIFACT_BYTES} bytes; remaining output is still drained but not stored. Captured prefix: {}",
                session.artifact_ref
            )
        } else {
            String::new()
        };
        let truncation_summary = if artifact_truncated {
            ", artifact truncated"
        } else {
            ""
        };

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "process.session".into(),
            ok: true,
            summary: format!(
                "session {session_id} {status} ({new_lines} new line(s), {total_lines} total{truncation_summary})",
            ),
            model_content: if new_lines == 0 && tail.is_empty() {
                format!("session {session_id} {status}; no output yet{truncation_note}")
            } else {
                format!(
                    "[session {session_id} {status}; {new_lines} new line(s); {total_lines} total]\n{tail}{truncation_note}",
                )
            },
            artifact_ref: Some(session.artifact_ref.clone()),
            metadata: json!({
                "action": "poll",
                "session_id": session_id,
                "status": status,
                "exit_code": exit_code,
                "new_lines": new_lines,
                "total_lines": total_lines,
                "output_bytes": output_bytes,
                "artifact_bytes": artifact_bytes,
                "artifact_limit_bytes": MAX_ARTIFACT_BYTES,
                "artifact_truncated": artifact_truncated,
            }),
        }))
    }

    async fn stop(&self, call_id: &str, args: SessionArgs) -> AgentResult<ToolOutcome> {
        let session_id = args.session_id.ok_or_else(|| {
            AgentError::InvalidRequest("process.session stop requires a session_id".into())
        })?;
        let mut sessions = self.sessions.lock().await;
        let mut session = sessions.remove(&session_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!(
                "process.session: no session '{session_id}' (unknown or already stopped)"
            ))
        })?;

        kill_process_tree(session.pid);
        let _ = session.child.kill().await;
        let exit_code = session
            .child
            .wait()
            .await
            .ok()
            .and_then(|status| status.code());
        super::persist_process_exit(&self.workspace, session.pid, exit_code)?;
        let artifact_ref = self
            .workspace
            .seal_buffered_artifact(session.artifact)
            .await?;

        let total_lines = session.capture.total_lines();
        let output_bytes = session.capture.total_bytes();
        let artifact_bytes = session.capture.artifact_bytes();
        let artifact_truncated = session.capture.artifact_truncated();
        let truncation_summary = if artifact_truncated {
            ", artifact truncated"
        } else {
            ""
        };

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "process.session".into(),
            ok: true,
            summary: format!(
                "session {session_id} stopped ({total_lines} total lines{truncation_summary})",
            ),
            model_content: if artifact_truncated {
                format!(
                    "session {session_id} stopped; artifact capture was truncated at {MAX_ARTIFACT_BYTES} bytes"
                )
            } else {
                format!("session {session_id} stopped")
            },
            artifact_ref: Some(artifact_ref),
            metadata: json!({
                "action": "stop",
                "session_id": session_id,
                "status": "stopped",
                "total_lines": total_lines,
                "output_bytes": output_bytes,
                "artifact_bytes": artifact_bytes,
                "artifact_limit_bytes": MAX_ARTIFACT_BYTES,
                "artifact_truncated": artifact_truncated,
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{EffectReconciler, EffectReconciliation};
    use serde_json::json;

    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => {
                panic!("process.session must return a plain value")
            }
        }
    }

    /// A long-running argv that writes one line immediately and then
    /// sleeps; platform-independent enough for the tests.
    fn long_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec![
                "ping".to_string(),
                "-n".to_string(),
                "20".to_string(),
                "127.0.0.1".to_string(),
            ]
        }
        #[cfg(not(windows))]
        {
            vec!["sleep".to_string(), "30".to_string()]
        }
    }

    fn start_ctx(run_id: RunId, arguments: &Value) -> agent_contracts::OperationEffectContext {
        crate::tools::test_process_effect_context(run_id, "c", "process.session", arguments)
    }

    fn write_marker_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "echo spawned> marker.txt".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "echo spawned > marker.txt".into()]
        }
    }

    #[tokio::test]
    async fn session_start_without_effect_identity_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let marker = dir.path().join("marker.txt");
        let error = tool
            .execute(
                RunId::new(),
                "c",
                json!({"action": "start", "argv": write_marker_argv()}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot spawn without Core-issued effect identity"),
            "{error}"
        );
        assert!(
            !marker.exists(),
            "fail-closed admission must happen before the child can mutate"
        );
    }

    #[tokio::test]
    async fn session_start_rejects_a_mismatched_identity_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let marker = dir.path().join("marker.txt");
        let run_id = RunId::new();
        let arguments = json!({"action": "start", "argv": write_marker_argv()});
        let stolen =
            crate::tools::test_process_effect_context(run_id, "c", "process.run", &arguments);
        let error = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(stolen),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("process spawn identity is for 'process.run'"),
            "{error}"
        );
        assert!(
            !marker.exists(),
            "a process.run lease must not start a session child"
        );
    }

    #[tokio::test]
    async fn session_start_rejects_escaping_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let error = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "action": "start",
                    "argv": write_marker_argv(),
                    "cwd": "../escape"
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("path must stay inside workspace"),
            "cwd confinement must fail before spawn, not as a missing identity: {error}"
        );
    }

    #[tokio::test]
    async fn session_start_ignores_an_unused_command_field() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessSessionTool::new(workspace, Arc::new(Mutex::new(HashMap::new())));
        let marker = dir.path().join("marker.txt");
        let unused = dir.path().join("unused.txt");
        let run_id = RunId::new();
        let arguments = json!({
            "action": "start",
            "command": "echo unused> unused.txt",
            "argv": write_marker_argv()
        });
        let context = start_ctx(run_id, &arguments);
        let output = value(
            tool.execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        let session_id = output.metadata["session_id"].as_str().unwrap().to_string();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = tool
            .execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await;

        assert!(
            marker.exists(),
            "spawn must follow argv, not an unused command field"
        );
        assert!(
            !unused.exists(),
            "the unused command field must not be executed"
        );
    }

    #[tokio::test]
    async fn session_stop_settles_start_as_completed_value_not_unapplied() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessSessionTool::new(workspace.clone(), Arc::new(Mutex::new(HashMap::new())));
        let run_id = RunId::new();
        let start_args = json!({"action": "start", "argv": long_argv()});
        let start_context = start_ctx(run_id, &start_args);
        let reconcile_start = start_context.clone();
        let output = value(
            tool.execute(
                run_id,
                "c",
                start_args,
                Some(start_context),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok, "session start failed: {}", output.summary);
        let session_id = output.metadata["session_id"].as_str().unwrap().to_string();

        let output = value(
            tool.execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(output.metadata["status"], "stopped");

        match workspace.reconcile(&reconcile_start).unwrap() {
            EffectReconciliation::CompletedValue { .. } => {}
            EffectReconciliation::NotApplied { .. } => {
                panic!(
                    "stop records exit against the start spawn; recovery must not treat start as never spawned"
                )
            }
            EffectReconciliation::NotManaged => {
                panic!("process.session start is a managed non-transactional process effect")
            }
            other => {
                panic!("session start after stop should settle as CompletedValue, got {other:?}")
            }
        }

        let poll_context = start_ctx(run_id, &json!({"action": "poll", "session_id": session_id}));
        match workspace.reconcile(&poll_context).unwrap() {
            EffectReconciliation::NotApplied { .. } => {}
            other => {
                panic!("poll never spawned; its own identity must stay NotApplied, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn session_start_poll_stop_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let run_id = RunId::new();

        // start
        let start_args = json!({"action": "start", "argv": long_argv()});
        let context = start_ctx(run_id, &start_args);
        let output = tool
            .execute(
                run_id,
                "c",
                start_args,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        let session_id = output.metadata["session_id"].as_str().unwrap().to_string();

        // poll: running
        let output = tool
            .execute(
                run_id,
                "c",
                json!({"action": "poll", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert_eq!(output.metadata["status"], "running");

        // stop: kills the tree and reaps the session.
        let output = tool
            .execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert_eq!(output.metadata["status"], "stopped");

        // A second stop must fail: the session is gone.
        let result = tool
            .execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err(), "stopping an unknown session must fail");
    }

    #[tokio::test]
    async fn session_poll_reports_exit_and_drains_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let run_id = RunId::new();

        // A short argv that prints and exits: `echo` on both platforms.
        #[cfg(windows)]
        let argv: Vec<String> = vec!["cmd".into(), "/C".into(), "echo hello session".into()];
        #[cfg(not(windows))]
        let argv: Vec<String> = vec!["echo".into(), "hello session".into()];

        let start_args = json!({"action": "start", "argv": argv});
        let context = start_ctx(run_id, &start_args);
        let output = tool
            .execute(
                run_id,
                "c",
                start_args,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        let session_id = output.metadata["session_id"].as_str().unwrap().to_string();

        // Poll until the process has exited and its output has drained
        // into the model-facing tail. `exited` is only reported once the
        // readers are at EOF, but a wedged reader degrades to "running",
        // so the test keeps polling and never asserts on a half-drained
        // tail.
        let mut last: Option<ToolOutput> = None;
        for _ in 0..50 {
            let output = tool
                .execute(
                    run_id,
                    "c",
                    json!({"action": "poll", "session_id": session_id}),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let output = value(output);
            if output.metadata["status"] == "exited"
                && output.model_content.contains("hello session")
            {
                last = Some(output);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let output = last.expect("the short process must exit within the poll window");
        assert!(
            output.model_content.contains("hello session"),
            "the drained output must reach the model: {}",
            output.model_content
        );
        assert!(
            output.metadata["exit_code"].is_number(),
            "an exited session reports its exit code"
        );

        // Reap it.
        let output = tool
            .execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(value(output).metadata["status"], "stopped");
    }
}
