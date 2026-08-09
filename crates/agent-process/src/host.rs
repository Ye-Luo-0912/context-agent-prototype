//! The generic JSON-lines process host: spawning the child, the startup
//! handshake, bounded framed request/response, per-request deadlines,
//! cancellation, and a poisoned-connection policy so a wedged or malicious
//! child can never be reused or grow the parent's memory without bound.
//!
//! Both the context-service adapter (`ContextEngine` over a process, in
//! `context-contextcore`) and the process capability adapter (a `Capability`
//! over a process, in `agent-capability-process`) are thin protocol layers
//! on top of this host — the framing, deadline, sandbox and failure policy
//! lives here once.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use agent_contracts::{AgentError, AgentResult};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

/// Client protocol version echoed by every request; a mismatched child is
/// poisoned instead of misparsed.
pub const PROTOCOL_VERSION: u32 = 1;

/// The child's execution boundary. Defaults to the historical behavior
/// (inherit the parent environment and cwd, no resource limits); the
/// process-capability adapter overrides it with a strict sandbox so a
/// generated capability cannot read the parent's secrets, roam the
/// parent's cwd or run without limits.
#[derive(Debug, Clone, Default)]
pub struct ProcessSandbox {
    /// If `Some(names)`, the child inherits *only* these parent variables
    /// (plus the explicit `ProcessHostConfig::env` grants) — everything
    /// else, API keys, `HOME`, credentials, is dropped. `None` inherits
    /// the whole parent environment (the context-service default).
    pub env_whitelist: Option<Vec<String>>,
    /// The child's working directory, created at connect when missing.
    /// `None` keeps the parent's cwd.
    pub cwd: Option<std::path::PathBuf>,
    /// Hard CPU-time limit in seconds via `RLIMIT_CPU` (Unix only; ignored
    /// elsewhere). `0` = unlimited.
    pub cpu_time_limit_secs: u64,
    /// Hard process-count limit via `RLIMIT_NPROC` (Unix only; ignored
    /// elsewhere). `0` = unlimited.
    pub process_limit: u64,
}

#[derive(Debug, Clone)]
pub struct ProcessHostConfig {
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment for the child (inherits the parent's env plus
    /// these overrides). Used by tests to re-exec themselves as a mock
    /// server, and by process capabilities to pass sandbox variables.
    pub env: Vec<(String, String)>,
    /// Deadline for the startup handshake (the child must answer ping).
    pub startup_timeout: Duration,
    /// Deadline for every request after the handshake.
    pub request_timeout: Duration,
    /// Hard cap on one response frame.
    pub max_frame_bytes: usize,
    /// The child's execution boundary (env, cwd, resource limits).
    pub sandbox: ProcessSandbox,
}

/// A live child process speaking JSON-lines on stdio. Strict ping-pong:
/// one request in flight at a time (the `Mutex`), because the callers are
/// `&self` traits.
pub struct ProcessHost {
    /// Held in a `Mutex` so `poison` can kill the child from `&self` (a
    /// timed-out or broken exchange must never leave the process running).
    child: Mutex<Child>,
    config: ProcessHostConfig,
    io: Mutex<HostIo>,
    next_id: AtomicU64,
    /// The child's pid, kept outside the child lock so a cancellation can
    /// kill the process *tree* (process group / taskkill) without touching
    /// the io lock.
    pid: AtomicU32,
}

struct HostIo {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// `Some(reason)` once the connection is unusable. Poisoned hosts reject
    /// every further call, so a timed-out or half-read exchange can never
    /// corrupt a later request/response pair.
    poisoned: Option<String>,
}

impl ProcessHost {
    /// Spawn the child and require a successful handshake within the startup
    /// deadline, so a missing or broken program fails at connect time, not
    /// on the first real call.
    pub async fn connect(config: ProcessHostConfig) -> AgentResult<Self> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);

        // Sandbox: an explicit env whitelist drops every unlisted parent
        // variable (secrets never cross), then the explicit grants land.
        if let Some(whitelist) = &config.sandbox.env_whitelist {
            command.env_clear();
            for name in whitelist {
                if let Ok(value) = std::env::var(name) {
                    command.env(name, value);
                }
            }
        }
        command.envs(config.env.iter().cloned());

        // Sandbox: the child runs in its own directory, created on demand,
        // never the parent's cwd.
        if let Some(cwd) = &config.sandbox.cwd {
            std::fs::create_dir_all(cwd).map_err(|e| {
                AgentError::Context(format!("create sandbox cwd '{}': {e}", cwd.display()))
            })?;
            command.current_dir(cwd);
        }

        // Sandbox (Unix): hard CPU-time and process-count ceilings enforced
        // by the kernel via rlimits, applied right after fork.
        #[cfg(unix)]
        {
            let cpu = config.sandbox.cpu_time_limit_secs;
            let nproc = config.sandbox.process_limit;
            if cpu > 0 || nproc > 0 {
                unsafe {
                    command.pre_exec(move || {
                        if cpu > 0 {
                            let limit = libc::rlimit {
                                rlim_cur: cpu as libc::rlim_t,
                                rlim_max: cpu as libc::rlim_t,
                            };
                            if libc::setrlimit(libc::RLIMIT_CPU, &limit) != 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                        if nproc > 0 {
                            let limit = libc::rlimit {
                                rlim_cur: nproc as libc::rlim_t,
                                rlim_max: nproc as libc::rlim_t,
                            };
                            if libc::setrlimit(libc::RLIMIT_NPROC, &limit) != 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                        Ok(())
                    });
                }
            }
        }

        // Make the child a process-group leader on Unix so a cancellation
        // can kill its whole tree (`kill(-pgid)`), not just the direct
        // child — a runaway subprocess must not survive its caller.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Context(format!("spawn '{}': {e}", config.program)))?;
        let pid = child.id().unwrap_or(0);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Context("child stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Context("child stdout unavailable".into()))?;

        let host = Self {
            child: Mutex::new(child),
            config,
            io: Mutex::new(HostIo {
                stdin,
                stdout: BufReader::new(stdout),
                poisoned: None,
            }),
            next_id: AtomicU64::new(1),
            pid: AtomicU32::new(pid),
        };
        timeout(
            host.config.startup_timeout,
            host.call_unbounded(json!({ "op": "ping" })),
        )
        .await
        .map_err(|_| {
            host.poison("handshake ping timed out".into());
            AgentError::Context(format!(
                "process '{}' did not respond to ping within {:?}",
                host.config.program, host.config.startup_timeout
            ))
        })?
        .map_err(|e| AgentError::Context(format!("process handshake: {e}")))?;
        Ok(host)
    }

    /// One framed call with the per-request deadline. A timeout poisons the
    /// connection: the request may have been written, so the response — if
    /// it ever arrives — must not be mistaken for a later request's answer.
    pub async fn call(&self, op: Value) -> AgentResult<Value> {
        timeout(self.config.request_timeout, self.call_unbounded(op))
            .await
            .map_err(|_| {
                self.poison(format!(
                    "request timed out after {:?}",
                    self.config.request_timeout
                ));
                AgentError::Context(format!(
                    "process '{}' request timed out after {:?}; connection poisoned",
                    self.config.program, self.config.request_timeout
                ))
            })?
    }

    /// One framed call that also aborts when `cancel` fires (a user
    /// `/cancel` or a superseded operation must stop the subprocess
    /// *now*, not at the request deadline). Cancellation poisons the
    /// connection and kills the child's whole process tree — a cancelled
    /// capability must not keep producing side effects in the background.
    pub async fn call_with_cancel(
        &self,
        op: Value,
        cancel: &agent_contracts::CancellationToken,
    ) -> AgentResult<Value> {
        tokio::select! {
            _ = cancel.cancelled() => {
                self.poison("cancelled by the runtime".into());
                Err(AgentError::Cancelled)
            }
            result = timeout(self.config.request_timeout, self.call_unbounded(op)) => {
                match result {
                    Ok(inner) => inner,
                    Err(_) => {
                        self.poison(format!(
                            "request timed out after {:?}",
                            self.config.request_timeout
                        ));
                        Err(AgentError::Context(format!(
                            "process '{}' request timed out after {:?}; connection poisoned",
                            self.config.program, self.config.request_timeout
                        )))
                    }
                }
            }
        }
    }

    async fn call_unbounded(&self, op: Value) -> AgentResult<Value> {
        let mut io = self.io.lock().await;
        if let Some(reason) = &io.poisoned {
            return Err(AgentError::Context(format!(
                "process '{}' connection poisoned: {reason}",
                self.config.program
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = Map::new();
        request.insert("id".into(), json!(id));
        request.insert("version".into(), json!(PROTOCOL_VERSION));
        if let Value::Object(fields) = op {
            request.extend(fields);
        }
        let line = serde_json::to_string(&Value::Object(request))
            .map_err(|e| AgentError::Context(format!("serialize request: {e}")))?;

        // Write the frame: line + newline + flush. Any failure poisons the
        // connection (the child may have read a partial frame).
        if let Err(e) = io.stdin.write_all(line.as_bytes()).await {
            return Err(self.io_error(&mut io, "write", e));
        }
        if let Err(e) = io.stdin.write_all(b"\n").await {
            return Err(self.io_error(&mut io, "write", e));
        }
        if let Err(e) = io.stdin.flush().await {
            return Err(self.io_error(&mut io, "write", e));
        }

        // Bounded frame read: never buffer more than `max_frame_bytes + 1`
        // bytes, so a child that streams megabytes without a newline is
        // rejected while reading instead of grown into memory first and
        // checked after. EOF without a newline is treated as a crash.
        let mut frame: Vec<u8> = Vec::with_capacity(256);
        let limit = self.config.max_frame_bytes + 1;
        let mut scratch = [0u8; 1024];
        let mut read = 0usize;
        loop {
            if frame.len() >= limit {
                io.poisoned = Some("response frame exceeded the byte limit".into());
                return Err(AgentError::Context(format!(
                    "process '{}' response frame exceeds the {} byte limit",
                    self.config.program, self.config.max_frame_bytes
                )));
            }
            let want = (limit - frame.len()).min(scratch.len());
            let n = match io.stdout.read(&mut scratch[..want]).await {
                Ok(n) => n,
                Err(e) => return Err(self.io_error(&mut io, "read", e)),
            };
            if n == 0 {
                break; // EOF
            }
            read += n;
            frame.extend_from_slice(&scratch[..n]);
            if frame.last() == Some(&b'\n') {
                break;
            }
        }
        if read == 0 {
            io.poisoned = Some("child closed its stdout".into());
            return Err(AgentError::Context(format!(
                "process '{}' closed its stdout (did it crash?)",
                self.config.program
            )));
        }

        // The frame may lack the trailing newline when the limit cut it off;
        // trim before parsing so the size check above is the only limit.
        let text = String::from_utf8(frame)
            .map_err(|e| AgentError::Context(format!("response is not UTF-8: {e}")))?;
        let response: Value = serde_json::from_str(text.trim_end())
            .map_err(|e| AgentError::Context(format!("parse response: {e} (line: {text})")))?;

        if response.get("id").and_then(Value::as_u64) != Some(id) {
            io.poisoned = Some(format!(
                "response id mismatch: got {:?}, expected {id}",
                response.get("id")
            ));
            return Err(AgentError::Context(format!(
                "process '{}' response id mismatch; connection poisoned",
                self.config.program
            )));
        }
        if response.get("version").and_then(Value::as_u64) != Some(PROTOCOL_VERSION as u64) {
            io.poisoned = Some("protocol version mismatch".into());
            return Err(AgentError::Context(format!(
                "process '{}' protocol version mismatch; connection poisoned",
                self.config.program
            )));
        }
        if response.get("ok").and_then(Value::as_bool) == Some(false) {
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown child error");
            return Err(AgentError::Context(format!("process error: {error}")));
        }
        Ok(response.get("value").cloned().unwrap_or(Value::Null))
    }

    /// Ask the child to exit gracefully, then reap it. The host is consumed,
    /// so no further calls can ride a closing pipe.
    pub async fn shutdown(self) {
        let _ = self.call(json!({ "op": "shutdown" })).await;
        let child = self.child;
        let _ = child.lock().await.wait().await;
    }

    fn io_error(&self, io: &mut HostIo, stage: &str, e: std::io::Error) -> AgentError {
        io.poisoned = Some(format!("{stage} failed: {e}"));
        if e.kind() == ErrorKind::BrokenPipe || e.kind() == ErrorKind::UnexpectedEof {
            AgentError::Context(format!(
                "process '{}' connection closed: {e}",
                self.config.program
            ))
        } else {
            AgentError::Io(format!("process '{}' {stage}: {e}", self.config.program))
        }
    }

    /// Mark the connection poisoned and kill the child's process tree.
    /// `try_lock` so this can be called from a timeout/cancel path without
    /// deadlocking on a guard that the cancelled future still holds.
    fn poison(&self, reason: String) {
        if let Ok(mut io) = self.io.try_lock() {
            io.poisoned = Some(reason);
        }
        self.kill_tree();
    }

    /// Kill the child and every descendant. A cancelled or timed-out call
    /// must not leave a runaway subtree alive — the child's own side
    /// effects (spawned subprocesses, writers) die with it.
    fn kill_tree(&self) {
        let pid = self.pid.load(Ordering::Relaxed);
        if pid == 0 {
            return;
        }
        #[cfg(unix)]
        {
            // Negative pid = the process group. The child was spawned with
            // `process_group(0)`, so its pgid == its pid and SIGKILL to
            // -pid reaches the whole tree. On ESRCH (no such group) fall
            // back to killing the direct child.
            let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            if result != 0 {
                if let Ok(mut child) = self.child.try_lock() {
                    let _ = child.start_kill();
                }
            }
        }
        #[cfg(windows)]
        {
            // `taskkill /T` walks the tree from the pid; `/F` force-kills.
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(not(any(unix, windows)))]
        {
            if let Ok(mut child) = self.child.try_lock() {
                let _ = child.start_kill();
            }
        }
    }
}

/// Locate a process binary: explicit env var (`CARGO_BIN_EXE_*` in tests),
/// then the standard cargo layout around the current executable, then PATH.
pub fn resolve_program(env_var: Option<&str>, binary_name: &str) -> String {
    if let Some(var) = env_var
        && let Ok(program) = std::env::var(var)
    {
        return program;
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(found) = probe_siblings(&current, binary_name)
    {
        return found.to_string_lossy().into_owned();
    }
    binary_name.to_string()
}

/// Probe the standard cargo layout around the current executable: a test
/// binary lives in `target/<profile>/deps/` while a helper/service binary
/// lands in `target/<profile>/`, so check both instead of depending on
/// cargo injecting the target dir into PATH (which not every runner does).
pub fn probe_siblings(current_exe: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let parent = current_exe.parent()?;
    [
        parent.join(name),
        parent.parent().map(|profile| profile.join(name))?,
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_finds_the_service_next_to_the_profile_dir() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("debug");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let service = profile.join("agent-context-service");
        std::fs::write(&service, "bin").unwrap();
        let test_exe = deps.join("service-abc123");
        std::fs::write(&test_exe, "test").unwrap();

        assert_eq!(
            probe_siblings(&test_exe, "agent-context-service"),
            Some(service)
        );
    }

    #[test]
    fn probe_returns_none_when_nowhere_to_be_found() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("deps").join("service-abc123");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "test").unwrap();
        assert_eq!(probe_siblings(&exe, "agent-context-service"), None);
    }
}
