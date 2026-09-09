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
    sync::mpsc,
};

use super::Tool;
use super::process::{
    bounded_cwd_listing, executable_seal_intact, resolve_program,
    validate_execution_authority_binding,
};
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
    #[cfg(windows)]
    host_death_job: Option<super::process::host_death_job::HostDeathJob>,
    pub(crate) pid: u32,
    rx: mpsc::Receiver<StreamChunk>,
    capture: StreamCapture,
    artifact_ref: String,
    /// 同一个 pinned draft 句柄贯穿整个 session；stop 时才封成 digest。
    artifact: BufWriter<ArtifactDraft>,
}

impl ProcessSession {
    /// Drain the available output and report the session state. `Ok((n,
    /// Some(code)))` means the child HAS exited and its tail was fully
    /// drained; `Ok((n, None))` means it is still running.
    ///
    /// R05: pipe EOF is NOT process exit — a process can close its output and
    /// keep running — so "exited" is decided only by the child's actual state
    /// (`try_wait`), never by the channel disconnecting. This path is bounded
    /// (non-blocking try_recv plus a per-attempt timeout on the exited-drain)
    /// and responds to `cancel`, so a live or wedged process never makes a
    /// poll hang.
    async fn drain(&mut self, cancel: &CancellationToken) -> AgentResult<(usize, Option<i32>)> {
        let mut new_lines = 0usize;
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    if self.capture.record(chunk, &mut self.artifact).await? {
                        new_lines = new_lines.saturating_add(1);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                // EOF on the session's pipes: the child may have exited or may
                // simply have closed its output. Never treat EOF as exit; the
                // child's actual state below decides.
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        match self
            .child
            .try_wait()
            .map_err(|error| AgentError::Io(format!("inspect session child: {error}")))?
        {
            Some(status) => {
                // The child genuinely exited. Drain the remaining buffered
                // output (bounded and cancellable) so "exited" carries a
                // complete model-facing tail; a wedged reader degrades to
                // "running" rather than report an incomplete tail as exited.
                loop {
                    if cancel.is_cancelled() {
                        return Ok((new_lines, None));
                    }
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
                            return Ok((new_lines, None));
                        }
                    }
                }
                Ok((new_lines, status.code()))
            }
            None => Ok((new_lines, None)), // still running
        }
    }
}

/// One slot in the per-dispatcher session registry. `Pending` is a
/// capacity reservation while `start` is between spawn and commit; the
/// model can never observe it (a start returns the id only after the
/// slot became `Running`), but it closes the race where concurrent
/// starts exceed [`MAX_SESSIONS`]. Every exit path of `start` removes
/// its own reservation.
pub(crate) enum SessionSlot {
    Pending,
    Running(Box<ProcessSession>),
}

/// The per-dispatcher session registry, shared by every
/// `process.session` tool instance.
pub(crate) type SessionRegistry = Arc<tokio::sync::Mutex<HashMap<String, SessionSlot>>>;

/// Owns a just-spawned child until a `start` settles: every post-spawn
/// exit (persistence, artifact or cancellation failure) funnels through
/// [`SpawnedChildGuard::abandon`], which kills the whole process tree
/// and reaps the direct child within a bounded wait. `keep` hands the
/// live child to the registry.
struct SpawnedChildGuard {
    child: Option<tokio::process::Child>,
    pid: u32,
    #[cfg(windows)]
    host_death_job: Option<super::process::host_death_job::HostDeathJob>,
}

impl SpawnedChildGuard {
    fn child_ref(&self) -> &tokio::process::Child {
        self.child
            .as_ref()
            .expect("a guarded child exists until keep or abandon")
    }

    /// Kill the whole tree and reap the direct child within a bounded
    /// wait, so a failed start cannot leave a child or descendant behind.
    async fn abandon(mut self) -> AgentResult<()> {
        // Closing the owned job catches children created while the tree
        // helper would be enumerating. Do this before reaping the leader.
        #[cfg(windows)]
        drop(self.host_death_job.take());
        kill_process_tree(self.pid);
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
            reap_session_child(child).await?;
        }
        Ok(())
    }

    fn keep(mut self) -> tokio::process::Child {
        self.child
            .take()
            .expect("a guarded child exists until keep or abandon")
    }
}

pub struct ProcessSessionTool {
    workspace: Workspace,
    sessions: SessionRegistry,
}

impl ProcessSessionTool {
    pub fn new(workspace: Workspace, sessions: SessionRegistry) -> Self {
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
            "poll" => self.poll(call_id, args, cancel).await,
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
        cancel: CancellationToken,
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

        super::require_process_effect_context(&effect_context, "process.session")?;
        super::require_covered_process_spawn(
            "process.session",
            arguments,
            &agent_contracts::exec_argv_intent(&args.argv),
        )?;

        // Cancellation is selected through the whole start: a token that
        // fired before spawn refuses without reserving a slot, and the
        // post-spawn section is a select so a mid-flight cancel kills the
        // spawned tree and releases the reservation.
        if cancel.is_cancelled() {
            return Ok(cancelled_start_output(call_id, "before spawn"));
        }

        // R11: the session slot is NOT reserved here. It is reserved only
        // after the synchronous admission checks below pass, so a start that
        // returns early (bad program, shadow, seal mismatch) never consumes a
        // Pending slot — repeated refused starts cannot exhaust capacity.

        // Resolution, authority binding and the pre-spawn identity check
        // share process.run semantics: a failed resolution is a typed
        // PathNotFound carrying the attempted candidates and identity
        // fingerprint; a workspace shadow or an execution-control env
        // override refuses the spawn; the canonicalized seal is rechecked
        // immediately before spawn.
        let resolution = match resolve_program(&args.argv[0], &cwd, &args.env) {
            Ok(resolution) => resolution,
            Err(failure) => {
                let entries = bounded_cwd_listing(&cwd);
                let recovery_hint = "process.session start launches an argv executable directly; shell syntax and built-ins require shell.exec (or an explicit shell executable plus its command flag)";
                return Ok(ToolOutcome::Value(agent_contracts::tool_failure_output(
                    call_id,
                    "process.session",
                    agent_contracts::ToolFailureClass::PathNotFound,
                    format!(
                        "process.session refused: program_not_found ({})",
                        args.argv[0]
                    ),
                    format!(
                        "program `{}` was not found.\ncwd `{}` contains: {}\n{recovery_hint}.",
                        args.argv[0],
                        cwd.display(),
                        if entries.is_empty() {
                            "(empty)".into()
                        } else {
                            entries.join(", ")
                        }
                    ),
                    json!({
                        "argv0": args.argv[0],
                        "cwd": cwd.display().to_string(),
                        "entries": entries,
                        "attempted": failure.candidates_tried(),
                        "resolution_scope_key": failure.scope_key(),
                        "resolution_fingerprint": failure.fingerprint_identity(),
                        "recovery_hint": recovery_hint,
                    }),
                )));
            }
        };

        let sealed_executable = validate_execution_authority_binding(
            &self.workspace,
            &args.argv[0],
            &resolution,
            &args.env,
            "process.session",
        )?;

        let seal_intact = executable_seal_intact(resolution.executable(), &sealed_executable);
        if !seal_intact {
            let entries = bounded_cwd_listing(&cwd);
            return Ok(ToolOutcome::Value(agent_contracts::tool_failure_output(
                call_id,
                "process.session",
                agent_contracts::ToolFailureClass::PathNotFound,
                format!(
                    "process.session refused: program_not_found ({})",
                    args.argv[0]
                ),
                format!(
                    "program `{}` changed identity before spawn.\ncwd `{}` contains: {}",
                    args.argv[0],
                    cwd.display(),
                    if entries.is_empty() {
                        "(empty)".into()
                    } else {
                        entries.join(", ")
                    }
                ),
                json!({
                    "argv0": args.argv[0],
                    "cwd": cwd.display().to_string(),
                    "entries": entries,
                    "resolution_scope_key": resolution.scope_key(),
                    "resolution_fingerprint": resolution.fingerprint_identity(),
                    "recovery_hint": "rebuild or reinstall the binary before running it",
                }),
            )));
        }

        // Reserve a session slot only after the synchronous admission checks
        // passed and immediately before spawn: the capacity check and the
        // insertion are one atomic step, so concurrent starts cannot exceed
        // MAX_SESSIONS (`Pending` marks the reservation; the model only ever
        // sees a `Running` session). Every path AFTER this point — pre-spawn
        // cancellation, spawn failure, missing pid and the post-spawn guard
        // cleanup — releases the reservation, so a failed start never leaks a
        // Pending slot that would permanently consume capacity (R11).
        let session_id = ContextItemId::new().to_string();
        {
            let mut sessions = self.sessions.lock().await;
            if sessions.len() >= MAX_SESSIONS {
                return Err(AgentError::InvalidRequest(format!(
                    "process.session is limited to {MAX_SESSIONS} concurrent sessions"
                )));
            }
            sessions.insert(session_id.clone(), SessionSlot::Pending);
        }
        if cancel.is_cancelled() {
            self.sessions.lock().await.remove(&session_id);
            return Ok(cancelled_start_output(call_id, "before spawn"));
        }

        let mut command = Command::new(resolution.executable());
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

        #[cfg(windows)]
        let host_death_job = super::process::host_death_job::HostDeathJob::create();

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.sessions.lock().await.remove(&session_id);
                return Err(AgentError::Tool(format!("spawn {}: {error}", args.argv[0])));
            }
        };
        let pid = match child.id() {
            Some(pid) => pid,
            None => {
                self.sessions.lock().await.remove(&session_id);
                return Err(AgentError::Tool("spawned process has no pid".into()));
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        #[cfg(windows)]
        let host_death_job = host_death_job.filter(|job| job.assign(pid));
        #[allow(unused_mut)]
        let mut guard = SpawnedChildGuard {
            child: Some(child),
            pid,
            #[cfg(windows)]
            host_death_job,
        };
        if let Err(error) = super::persist_spawned_process(
            &self.workspace,
            &effect_context,
            guard.child_ref(),
            "process.session",
        ) {
            let cleanup = guard.abandon().await;
            self.sessions.lock().await.remove(&session_id);
            cleanup?;
            return Err(error);
        }

        // Everything between spawn and commit is cancellable, and every
        // exit funnels through the guard (kill the whole tree, bounded
        // reap) and releases the reservation.
        let (artifact_ref, line_rx, draft) = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let cleanup = guard.abandon().await;
                self.sessions.lock().await.remove(&session_id);
                cleanup?;
                return Ok(cancelled_start_output(call_id, "during start"));
            }
            phase = async {
                // 会话存活期间发布 draft 定位符，stop 时再封口。
                let draft = self
                    .workspace
                    .create_artifact(run_id, "process-session", "log")
                    .await?;
                let artifact_ref = draft.locator().to_string();
                let (line_tx, line_rx) = mpsc::channel::<StreamChunk>(512);
                if let Some(stdout) = stdout {
                    spawn_stdout_reader(stdout, line_tx.clone());
                }
                if let Some(stderr) = stderr {
                    spawn_stderr_reader(stderr, line_tx.clone());
                }
                drop(line_tx);
                Ok::<_, AgentError>((artifact_ref, line_rx, draft))
            } => match phase {
                Ok(phase) => phase,
                Err(error) => {
                    let cleanup = guard.abandon().await;
                    self.sessions.lock().await.remove(&session_id);
                    cleanup?;
                    return Err(error);
                }
            },
        };

        let pid = guard.pid;
        #[cfg(windows)]
        let host_death_job = guard.host_death_job.take();
        let child = guard.keep();
        self.sessions.lock().await.insert(
            session_id.clone(),
            SessionSlot::Running(Box::new(ProcessSession {
                child,
                #[cfg(windows)]
                host_death_job,
                pid,
                rx: line_rx,
                capture: StreamCapture::new(),
                artifact_ref: artifact_ref.clone(),
                artifact: BufWriter::new(draft),
            })),
        );

        Ok(ToolOutcome::Value(
            ToolOutput {
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
            }
            .with_native_execution_facts(super::builtin_bound(true)),
        ))
    }

    async fn poll(
        &self,
        call_id: &str,
        args: SessionArgs,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let session_id = args.session_id.ok_or_else(|| {
            AgentError::InvalidRequest("process.session poll requires a session_id".into())
        })?;
        // R05(c): take a SHORT lock to drain and snapshot the session, then
        // drop it before any process-exit settle / persist — the whole table
        // is never held while waiting. The drain itself is bounded and
        // cancellable (R05(b)), so a live process cannot make a poll hang.
        let (
            new_lines,
            exit_code,
            pid,
            tail,
            total_lines,
            output_bytes,
            artifact_bytes,
            artifact_truncated,
            artifact_ref,
        ) = {
            let mut sessions = self.sessions.lock().await;
            let Some(SessionSlot::Running(session)) = sessions.get_mut(&session_id) else {
                return Err(AgentError::InvalidRequest(format!(
                    "process.session: no session '{session_id}' (unknown or already stopped)"
                )));
            };
            let (new_lines, exit_code) = session.drain(&cancel).await?;
            session
                .artifact
                .flush()
                .await
                .map_err(|e| AgentError::Io(format!("flush session artifact: {e}")))?;
            (
                new_lines,
                exit_code,
                session.pid,
                session.capture.model_tail(),
                session.capture.total_lines(),
                session.capture.total_bytes(),
                session.capture.artifact_bytes(),
                session.capture.artifact_truncated(),
                session.artifact_ref.clone(),
            )
        };

        let (status, exit_code) = if let Some(code) = exit_code {
            super::persist_process_exit(&self.workspace, pid, Some(code))?;
            ("exited", Some(code))
        } else {
            ("running", None)
        };
        let truncation_note = if artifact_truncated {
            format!(
                "\n\nArtifact capture truncated at {MAX_ARTIFACT_BYTES} bytes; remaining output is still drained but not stored. Captured prefix: {}",
                artifact_ref
            )
        } else {
            String::new()
        };
        let truncation_summary = if artifact_truncated {
            ", artifact truncated"
        } else {
            ""
        };

        Ok(ToolOutcome::Value(
            ToolOutput {
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
                artifact_ref: Some(artifact_ref),
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
            }
            .with_native_execution_facts(super::builtin_bound(true)),
        ))
    }

    async fn stop(&self, call_id: &str, args: SessionArgs) -> AgentResult<ToolOutcome> {
        let session_id = args.session_id.ok_or_else(|| {
            AgentError::InvalidRequest("process.session stop requires a session_id".into())
        })?;
        let mut sessions = self.sessions.lock().await;
        let Some(SessionSlot::Running(session)) = sessions.remove(&session_id) else {
            return Err(AgentError::InvalidRequest(format!(
                "process.session: no session '{session_id}' (unknown or already stopped)"
            )));
        };
        let total_lines = session.capture.total_lines();
        let output_bytes = session.capture.total_bytes();
        let artifact_bytes = session.capture.artifact_bytes();
        let artifact_truncated = session.capture.artifact_truncated();
        drop(sessions);

        let artifact_ref = teardown_session(&self.workspace, *session).await?;

        let truncation_summary = if artifact_truncated {
            ", artifact truncated"
        } else {
            ""
        };

        Ok(ToolOutcome::Value(
            ToolOutput {
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
            }
            .with_native_execution_facts(super::builtin_bound(true)),
        ))
    }
}

/// A typed failure for a `start` that never created a session: pre-spawn
/// cancellation and the post-spawn select both land here, so the model
/// sees one shape and cannot poll or stop an id that was never issued.
fn cancelled_start_output(call_id: &str, phase: &str) -> ToolOutcome {
    let mut metadata = json!({
        "action": "start",
        "outcome": "cancelled",
        "phase": phase,
    });
    agent_contracts::attach_failure_class(
        &mut metadata,
        agent_contracts::ToolFailureClass::Cancellation,
    );
    ToolOutcome::Value(ToolOutput {
        call_id: call_id.into(),
        tool_name: "process.session".into(),
        ok: false,
        summary: format!("process.session start cancelled ({phase})"),
        model_content: format!(
            "process.session start was cancelled {phase}; no session was created and its process tree was terminated."
        ),
        artifact_ref: None,
        metadata,
    })
}

/// The single teardown path for a running session, shared by `stop` and
/// module shutdown: kill the whole process tree, reap the direct child
/// within a bounded wait, persist the exit and seal the buffered
/// artifact. Bounded per session so teardown itself cannot hang.
async fn teardown_session(
    workspace: &Workspace,
    mut session: ProcessSession,
) -> AgentResult<String> {
    #[cfg(windows)]
    drop(session.host_death_job.take());
    kill_process_tree(session.pid);
    let _ = session.child.start_kill();
    let exit_status = reap_session_child(&mut session.child).await?;
    super::persist_process_exit(workspace, session.pid, exit_status.code())?;
    workspace.seal_buffered_artifact(session.artifact).await
}

#[cfg(test)]
thread_local! {
    static SIMULATE_UNCONFIRMED_REAP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

async fn reap_session_child(
    child: &mut tokio::process::Child,
) -> AgentResult<std::process::ExitStatus> {
    #[cfg(test)]
    if SIMULATE_UNCONFIRMED_REAP.with(|flag| flag.replace(false)) {
        return Err(AgentError::RecoveryRequired(
            "injected missing session exit observation".into(),
        ));
    }
    tokio::time::timeout(EXIT_DRAIN_TIMEOUT, child.wait())
        .await
        .map_err(|_| {
            AgentError::RecoveryRequired(
                "session exit was not confirmed before the cleanup deadline".into(),
            )
        })?
        .map_err(|error| {
            AgentError::RecoveryRequired(format!("session exit could not be observed: {error}"))
        })
}

/// Module shutdown: stop every live session with the same teardown as
/// `stop`, so a dispatcher teardown kills and reaps children instead of
/// leaving them to direct-child drop. Pending reservations carry no
/// child and are simply dropped. Best effort across sessions; the first
/// error is reported after the rest have been drained.
pub(crate) async fn drain_sessions(
    workspace: &Workspace,
    sessions: &SessionRegistry,
) -> AgentResult<()> {
    let mut slots = sessions.lock().await;
    let entries: Vec<(String, SessionSlot)> = std::mem::take(&mut *slots).into_iter().collect();
    drop(slots);
    let mut first_error: Option<AgentError> = None;
    for (_, slot) in entries {
        if let SessionSlot::Running(session) = slot
            && let Err(error) = teardown_session(workspace, *session).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
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
            SessionRegistry::default(),
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
            SessionRegistry::default(),
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
            SessionRegistry::default(),
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
        let tool = ProcessSessionTool::new(workspace, SessionRegistry::default());
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
    async fn session_start_refuses_bare_name_shadow_and_control_env() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessSessionTool::new(workspace, SessionRegistry::default());
        #[cfg(windows)]
        let src = std::path::PathBuf::from(
            std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()),
        );
        #[cfg(not(windows))]
        let src = std::path::PathBuf::from("/bin/echo");
        assert!(src.is_file(), "source exe missing: {}", src.display());
        let program_name = if cfg!(windows) { "probe.exe" } else { "probe" };
        std::fs::copy(&src, dir.path().join(program_name)).unwrap();

        // 裸名命中 cwd 内的工作区文件：与 process.run 同一套 shadow 拒绝。
        let run_id = RunId::new();
        let arguments = json!({
            "action": "start",
            "argv": [program_name],
            "cwd": "."
        });
        let context = start_ctx(run_id, &arguments);
        let error = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .expect_err("bare-name workspace shadow must be refused");
        assert!(
            error.to_string().contains("shadow"),
            "the refusal must name the shadow mechanism: {error}"
        );

        // 执行控制 env 覆盖：拒绝且不启动子进程。
        let run_id = RunId::new();
        let arguments = json!({
            "action": "start",
            "argv": write_marker_argv(),
            "env": {"LD_PRELOAD": "/tmp/injected.so"}
        });
        let context = start_ctx(run_id, &arguments);
        let error = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .expect_err("execution-control env override must be refused");
        let message = error.to_string();
        assert!(
            message.contains("LD_PRELOAD"),
            "the refusal must name the variable: {message}"
        );
        assert!(
            !dir.path().join("marker.txt").exists(),
            "no child may spawn under a refused env override"
        );
    }

    #[tokio::test]
    async fn session_stop_settles_start_as_completed_value_not_unapplied() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessSessionTool::new(workspace.clone(), SessionRegistry::default());
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
            SessionRegistry::default(),
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
            SessionRegistry::default(),
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

    /// R11: a start that returns early (here: an unresolvable program) must
    /// release its capacity. MAX_SESSIONS+1 refused starts are followed by a
    /// valid start that is still accepted — a failed start never leaks a
    /// Pending slot that would permanently consume session capacity.
    #[tokio::test]
    async fn failed_starts_do_not_consume_session_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let sessions = SessionRegistry::default();
        let tool = ProcessSessionTool::new(workspace, sessions.clone());

        for _ in 0..=(MAX_SESSIONS) {
            let run_id = RunId::new();
            let arguments = json!({"action": "start", "argv": ["definitely-not-a-real-bin-xyz"]});
            let context = start_ctx(run_id, &arguments);
            let result = tool
                .execute(
                    run_id,
                    "c",
                    arguments,
                    Some(context),
                    CancellationToken::new(),
                )
                .await;
            if let Ok(outcome) = result {
                let output = value(outcome);
                assert!(!output.ok, "a nonexistent program must not start");
            }
        }
        assert!(
            sessions.lock().await.is_empty(),
            "refused starts must not leak Pending slots"
        );

        // Capacity is intact: a valid start is still accepted.
        let run_id = RunId::new();
        let start_args = json!({"action": "start", "argv": long_argv()});
        let context = start_ctx(run_id, &start_args);
        let output = value(
            tool.execute(
                run_id,
                "c",
                start_args,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok, "{}", output.summary);
        assert_eq!(sessions.lock().await.len(), 1);
        let session_id = output.metadata["session_id"].as_str().unwrap();
        let stop = tool
            .execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(value(stop).metadata["status"], "stopped");
    }

    /// R05(a): EOF on the session's output pipe is NOT process exit. A child
    /// that closes its output but keeps running must be reported "running",
    /// and the poll must return promptly (bounded, R05(b)) rather than wait
    /// on the live process.
    #[tokio::test]
    async fn eof_on_live_process_output_does_not_report_exit() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            SessionRegistry::default(),
        );
        let run_id = RunId::new();
        // Redirect the child's output to a device/null so the session's pipes
        // EOF immediately while the process keeps running for ~30s.
        #[cfg(windows)]
        let argv: Vec<String> = vec![
            "cmd".into(),
            "/C".into(),
            ">nul 2>&1 ping -n 30 127.0.0.1".into(),
        ];
        #[cfg(not(windows))]
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "exec >/dev/null 2>&1; sleep 30".into(),
        ];

        let start_args = json!({"action": "start", "argv": argv});
        let context = start_ctx(run_id, &start_args);
        let output = value(
            tool.execute(
                run_id,
                "c",
                start_args,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok, "{}", output.summary);
        let session_id = output.metadata["session_id"].as_str().unwrap().to_string();

        // Let the pipe readers reach EOF on the redirected output.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let start = std::time::Instant::now();
        let output = value(
            tool.execute(
                run_id,
                "c",
                json!({"action": "poll", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        let elapsed = start.elapsed();
        assert_eq!(
            output.metadata["status"], "running",
            "EOF on a live process's output must not be reported as exit"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "poll on a live process must return promptly (bounded), took {elapsed:?}"
        );

        let stop = tool
            .execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(value(stop).metadata["status"], "stopped");
    }

    /// R05(b): a poll on a live process responds to cancellation and returns
    /// within a bound instead of waiting indefinitely on the process.
    #[tokio::test]
    async fn poll_responds_to_cancellation_and_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            SessionRegistry::default(),
        );
        let run_id = RunId::new();
        let start_args = json!({"action": "start", "argv": long_argv()});
        let context = start_ctx(run_id, &start_args);
        let output = value(
            tool.execute(
                run_id,
                "c",
                start_args,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok, "{}", output.summary);
        let session_id = output.metadata["session_id"].as_str().unwrap().to_string();

        let cancel = CancellationToken::new();
        cancel.cancel();
        let start = std::time::Instant::now();
        let output = value(
            tool.execute(
                run_id,
                "c",
                json!({"action": "poll", "session_id": session_id}),
                None,
                cancel,
            )
            .await
            .unwrap(),
        );
        let elapsed = start.elapsed();
        assert_eq!(output.metadata["status"], "running");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a cancelled poll on a live process must return promptly, took {elapsed:?}"
        );

        let stop = tool
            .execute(
                run_id,
                "c",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(value(stop).metadata["status"], "stopped");
    }

    #[tokio::test]
    async fn pre_cancelled_start_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProcessSessionTool::new(
            Workspace::open(dir.path()).await.unwrap(),
            SessionRegistry::default(),
        );
        let marker = dir.path().join("marker.txt");
        let run_id = RunId::new();
        let arguments = json!({"action": "start", "argv": write_marker_argv()});
        let context = start_ctx(run_id, &arguments);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let output = value(
            tool.execute(run_id, "c", arguments, Some(context), cancel)
                .await
                .unwrap(),
        );
        assert!(!output.ok, "a pre-cancelled start must report failure");
        assert!(
            output.summary.contains("cancelled"),
            "summary should mention cancellation: {}",
            output.summary
        );
        assert!(
            !marker.exists(),
            "no child may spawn under a pre-cancelled start"
        );
    }

    #[tokio::test]
    async fn concurrent_starts_at_the_cap_are_reserved_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let sessions = SessionRegistry::default();
        let tool = std::sync::Arc::new(ProcessSessionTool::new(workspace, sessions.clone()));
        let attempts = MAX_SESSIONS + 4;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..attempts {
            let tool = tool.clone();
            set.spawn(async move {
                let run_id = RunId::new();
                let arguments = json!({"action": "start", "argv": long_argv()});
                let context = start_ctx(run_id, &arguments);
                tool.execute(
                    run_id,
                    "c",
                    arguments,
                    Some(context),
                    CancellationToken::new(),
                )
                .await
            });
        }
        let mut started = 0usize;
        let mut refused = 0usize;
        while let Some(result) = set.join_next().await {
            match result.expect("the start task must not panic") {
                Ok(ToolOutcome::Value(output)) => {
                    assert!(
                        output.ok,
                        "a reserved start must succeed: {}",
                        output.summary
                    );
                    started += 1;
                }
                Ok(other) => panic!("session start must return a plain value: {other:?}"),
                Err(error) => {
                    assert!(
                        error
                            .to_string()
                            .contains("limited to 16 concurrent sessions"),
                        "{error}"
                    );
                    refused += 1;
                }
            }
        }
        assert_eq!(started, MAX_SESSIONS);
        assert_eq!(refused, attempts - MAX_SESSIONS);
        assert_eq!(
            sessions.lock().await.len(),
            MAX_SESSIONS,
            "every reservation must have become a running session"
        );
        drain_sessions(&tool.workspace, &sessions).await.unwrap();
        assert!(sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cancel_mid_start_kills_the_spawned_tree() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let sessions = SessionRegistry::default();
        let tool =
            std::sync::Arc::new(ProcessSessionTool::new(workspace.clone(), sessions.clone()));
        let (argv, child_pidfile, heir_pidfile) =
            super::super::test_procs::tree_pidfile_argv(dir.path());
        let run_id = RunId::new();
        let arguments = json!({"action": "start", "argv": argv});
        let context = start_ctx(run_id, &arguments);
        let cancel = CancellationToken::new();
        let task_tool = tool.clone();
        let start_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            let output = task_tool
                .execute(run_id, "c", arguments, Some(context), start_cancel)
                .await
                .unwrap();
            value(output)
        });
        super::super::test_procs::wait_for_path(&child_pidfile).await;
        let child_pid = super::super::test_procs::read_pid(&child_pidfile);
        cancel.cancel();
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("start must stop after cancellation")
            .unwrap();
        let heir_pid: Option<u32> = std::fs::read_to_string(&heir_pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        if output.ok {
            // The start committed before the cancel landed: the session is
            // running, and its own stop path tears the tree down.
            let session_id = output.metadata["session_id"].as_str().unwrap();
            let stop = tool
                .execute(
                    run_id,
                    "c",
                    json!({"action": "stop", "session_id": session_id}),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(value(stop).metadata["status"], "stopped");
        } else {
            assert!(output.summary.contains("cancelled"), "{}", output.summary);
            assert!(
                sessions.lock().await.is_empty(),
                "a cancelled start must release its reservation"
            );
        }
        let mut tracked = vec![child_pid];
        if let Some(heir_pid) = heir_pid {
            tracked.push(heir_pid);
        }
        super::super::test_procs::wait_for_all_dead(&tracked, "the cancelled start's tree");
    }

    #[tokio::test]
    async fn artifact_failure_after_spawn_releases_the_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let sessions = SessionRegistry::default();
        let tool = ProcessSessionTool::new(workspace.clone(), sessions.clone());
        let run_id = RunId::new();
        // Sabotage the artifact owner directory so the post-spawn artifact
        // phase fails deterministically.
        let artifact_run_dir = workspace
            .state_dir()
            .join("artifacts")
            .join(run_id.to_string());
        std::fs::create_dir_all(&artifact_run_dir).unwrap();
        std::fs::write(artifact_run_dir.join("process-session"), b"block").unwrap();
        let (argv, child_pidfile, heir_pidfile) =
            super::super::test_procs::tree_pidfile_argv(dir.path());
        let arguments = json!({"action": "start", "argv": argv});
        let context = start_ctx(run_id, &arguments);
        let error = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .expect_err("the sabotaged artifact phase must fail the start");
        assert!(error.to_string().contains("artifact"), "{error}");
        assert!(
            sessions.lock().await.is_empty(),
            "a failed start must release its reservation"
        );
        // The child races its own pidfile write against the artifact
        // failure; whichever wins, nothing may be left running afterwards.
        // Existence is not content: a shell redirect truncates the pid
        // file before writing the pid, so only read a pid the child
        // actually wrote.
        let child_pid = std::fs::read_to_string(&child_pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        let heir_pid: Option<u32> = std::fs::read_to_string(&heir_pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        let mut tracked = Vec::new();
        if let Some(child_pid) = child_pid {
            tracked.push(child_pid);
        }
        if let Some(heir_pid) = heir_pid {
            tracked.push(heir_pid);
        }
        if !tracked.is_empty() {
            super::super::test_procs::wait_for_all_dead(&tracked, "the failed start's tree");
        }
    }

    #[tokio::test]
    async fn unconfirmed_stop_never_persists_a_completed_process_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let sessions = SessionRegistry::default();
        let tool = ProcessSessionTool::new(workspace.clone(), sessions.clone());
        let run_id = RunId::new();
        let arguments = json!({"action": "start", "argv": long_argv()});
        let context = start_ctx(run_id, &arguments);
        let started = value(
            tool.execute(
                run_id,
                "start",
                arguments,
                Some(context.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        let session_id = started.metadata["session_id"].as_str().unwrap();
        SIMULATE_UNCONFIRMED_REAP.with(|flag| flag.set(true));
        let result = tool
            .execute(
                run_id,
                "stop",
                json!({"action": "stop", "session_id": session_id}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AgentError::RecoveryRequired(_))));
        assert!(sessions.lock().await.is_empty());
        assert!(
            matches!(
                workspace.reconcile(&context).unwrap(),
                EffectReconciliation::Ambiguous { .. }
            ),
            "a missing wait receipt cannot become durable completion"
        );
    }

    #[tokio::test]
    async fn drain_stops_every_session_with_its_tree() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let sessions = SessionRegistry::default();
        let tool = ProcessSessionTool::new(workspace.clone(), sessions.clone());
        let mut tracked = Vec::new();
        for index in 0..2 {
            let pid_dir = dir.path().join(format!("run{index}"));
            std::fs::create_dir_all(&pid_dir).unwrap();
            let (argv, child_pidfile, heir_pidfile) =
                super::super::test_procs::tree_pidfile_argv(&pid_dir);
            let run_id = RunId::new();
            let arguments = json!({"action": "start", "argv": argv});
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
            assert!(output.ok, "{}", output.summary);
            super::super::test_procs::wait_for_path(&child_pidfile).await;
            tracked.push(super::super::test_procs::read_pid(&child_pidfile));
            super::super::test_procs::wait_for_path(&heir_pidfile).await;
            tracked.push(super::super::test_procs::read_pid(&heir_pidfile));
        }
        drain_sessions(&workspace, &sessions).await.unwrap();
        assert!(
            sessions.lock().await.is_empty(),
            "shutdown drain must empty the registry"
        );
        super::super::test_procs::wait_for_all_dead(&tracked, "the drained sessions");
    }
}
