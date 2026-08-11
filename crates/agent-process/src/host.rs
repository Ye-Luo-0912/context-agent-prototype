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
use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicU64, Ordering},
};
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
    /// Per-process memory ceiling in bytes enforced by the Windows
    /// Job-Object (`JOB_OBJECT_LIMIT_PROCESS_MEMORY`). `0` = unlimited.
    /// Unix has no equivalent yet.
    #[cfg(windows)]
    pub job_max_memory_bytes: u64,
    /// How many bytes of the child's stderr to capture into a bounded tail
    /// (surfaced on connection errors and diagnostics). `0` inherits the
    /// parent's stderr instead — the context-service default. A sandboxed
    /// capability pipes and drains stderr so a chatty child can never grow
    /// the parent's memory or flood the console without bound.
    pub stderr_capture_bytes: usize,
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
    /// The Windows Job-Object holding the child tree: kernel-enforced
    /// quotas (active-process ceiling from `process_limit`, per-process
    /// memory ceiling from `job_max_memory_bytes`) and
    /// `KILL_ON_JOB_CLOSE`, so closing the handle (host drop) terminates
    /// every descendant. `None` when the sandbox asked for no Windows
    /// quotas; the platform has no equivalent on other OSes.
    #[cfg(windows)]
    job: Mutex<Option<JobObject>>,
    /// The bounded tail of the child's stderr, fed by a drainer task when
    /// `ProcessSandbox::stderr_capture_bytes > 0`; `None` when stderr is
    /// inherited. The tail is surfaced on errors so a failing child says
    /// *why* without ever buffering unbounded stderr in the parent.
    stderr_tail: Arc<Mutex<std::collections::VecDeque<u8>>>,
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
            .stderr(if config.sandbox.stderr_capture_bytes > 0 {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::inherit()
            })
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
        // Sandbox (Windows): create the Job-Object before spawning so the
        // child is assigned in the same breath as it starts. The kernel
        // enforces the active-process and per-process-memory ceilings and
        // kills the whole tree when the handle closes.
        #[cfg(windows)]
        let created_job = job_object::create_job_object(&config.sandbox)?;
        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Context(format!("spawn '{}': {e}", config.program)))?;
        let pid = child.id().unwrap_or(0);

        // Sandbox (Windows): assign the child to the Job-Object. When the
        // kernel refuses (outer Job-Object confinement on CI runners, or a
        // ceiling), degrade to no job rather than failing the connection —
        // the child still runs under env/cwd hardening, it just loses the
        // Windows quota layer here.
        #[cfg(windows)]
        let job = match created_job {
            Some(created) => match created.assign(pid) {
                Ok(true) => Some(created),
                Ok(false) | Err(_) => {
                    eprintln!(
                        "job object assign skipped: process {pid} is already confined by an outer job"
                    );
                    None
                }
            },
            None => None,
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Context("child stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Context("child stdout unavailable".into()))?;

        let stderr_tail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        // Bounded stderr: when the sandbox asks for capture, a dedicated
        // drainer task reads the pipe into a ring that keeps only the last
        // `stderr_capture_bytes`. The child can stream forever without the
        // parent buffering it all — the tail is the only memory it ever
        // occupies, and it is surfaced on connection errors.
        if let Some(stderr) = child.stderr.take() {
            let cap = config.sandbox.stderr_capture_bytes;
            let tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut reader = stderr;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut ring = tail.lock().await;
                            ring.extend(&buf[..n]);
                            while ring.len() > cap {
                                ring.pop_front();
                            }
                        }
                    }
                }
            });
        }

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
            #[cfg(windows)]
            job: Mutex::new(job),
            stderr_tail,
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

    /// The bounded tail of the child's stderr (empty when stderr is
    /// inherited or the child wrote nothing). Surfaced on connection
    /// errors so a failing child says *why* without the parent ever
    /// buffering unbounded stderr.
    pub async fn stderr_tail(&self) -> String {
        let mut ring = self.stderr_tail.lock().await;
        String::from_utf8_lossy(ring.make_contiguous()).into_owned()
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
        #[cfg(windows)]
        {
            // The Job-Object is the authoritative tree kill: every
            // descendant was assigned at connect, so terminating the job
            // reaches them all in one kernel call. Fall back to taskkill
            // when no job exists.
            let terminated = if let Ok(guard) = self.job.try_lock()
                && let Some(job) = guard.as_ref()
            {
                job.terminate()
            } else {
                false
            };
            if !terminated {
                kill_process_tree(pid);
            }
        }
        #[cfg(not(windows))]
        kill_process_tree(pid);
        // Defense in depth: on Unix, a `kill(-pgid)` ESRCH (group already
        // gone) must not leave the direct child alive; on other platforms
        // without a tree kill, the direct child is the whole kill.
        #[cfg(unix)]
        {
            let pid = self.pid.load(Ordering::Relaxed);
            if pid != 0
                && unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } != 0
                && let Ok(mut child) = self.child.try_lock()
            {
                let _ = child.start_kill();
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            if let Ok(mut child) = self.child.try_lock() {
                let _ = child.start_kill();
            }
        }
    }
}

/// Kill a process and every descendant, without touching the caller's own
/// process group. A cancelled or timed-out operation must not leave a
/// runaway subtree alive — the child's own side effects (spawned
/// subprocesses, writers) die with it.
///
/// The child must have been spawned as a process-group leader on Unix
/// (`process_group(0)`, so `kill(-pid)` reaches the whole tree); on Windows
/// `taskkill /T` walks the tree from the pid. Shared by `ProcessHost`
/// (its child's whole tree) and the builtin `shell.exec` tool, so every
/// process path kills the same way — a direct `child.kill()` alone would
/// leave descendants running after cancellation.
pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        // Negative pid = the process group. The child was spawned with
        // `process_group(0)`, so its pgid == its pid and SIGKILL to -pid
        // reaches the whole tree. On ESRCH (no such group) the tree is
        // already gone; callers fall back to the direct child.
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
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
    {}
}

/// The Windows Job-Object machinery behind the process sandbox:
/// kernel-enforced quotas (active-process ceiling, per-process memory
/// ceiling) and `KILL_ON_JOB_CLOSE`, so closing the handle — the host's
/// drop — terminates every assigned process even if no explicit kill ran.
#[cfg(windows)]
mod job_object {
    use super::{AgentError, AgentResult, ProcessSandbox};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// RAII handle: the drop closes the handle, and `KILL_ON_JOB_CLOSE`
    /// turns that close into a whole-tree termination.
    pub struct JobObject(HANDLE);

    // A Windows HANDLE is a kernel handle-table index: safe to move between
    // threads and share behind a lock (CloseHandle / TerminateJobObject are
    // thread-safe). Without this the host would not be Send, which would
    // break `Arc<dyn Capability>`.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        /// Assign one process (by pid) to this job. `Ok(false)` means the
        /// kernel refused the assignment — most commonly because the
        /// process already belongs to an outer Job-Object (CI runners
        /// confine every process under one), which blocks nesting, or the
        /// active-process ceiling is already reached.
        pub fn assign(&self, pid: u32) -> AgentResult<bool> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    return Err(AgentError::Context(format!(
                        "open process {pid} to assign the job object failed"
                    )));
                }
                let assigned = AssignProcessToJobObject(self.0, process);
                let _ = CloseHandle(process);
                Ok(assigned != 0)
            }
        }

        /// Terminate every process in the job in one kernel call.
        pub fn terminate(&self) -> bool {
            unsafe { TerminateJobObject(self.0, 1) != 0 }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Create a Job-Object from the sandbox's Windows quotas. Returns
    /// `None` when no Windows quota was requested (the host then falls back
    /// to `taskkill /T`). The caller assigns processes with
    /// `JobObject::assign`.
    pub fn create_job_object(sandbox: &ProcessSandbox) -> AgentResult<Option<JobObject>> {
        if sandbox.process_limit == 0 && sandbox.job_max_memory_bytes == 0 {
            return Ok(None);
        }
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == INVALID_HANDLE_VALUE || job.is_null() {
                return Err(AgentError::Context(
                    "create job object for the sandbox failed".into(),
                ));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if sandbox.process_limit > 0 {
                flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.BasicLimitInformation.ActiveProcessLimit = sandbox.process_limit as u32;
            }
            if sandbox.job_max_memory_bytes > 0 {
                flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                info.ProcessMemoryLimit = sandbox.job_max_memory_bytes as usize;
            }
            info.BasicLimitInformation.LimitFlags = flags;
            let configured = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if configured == 0 {
                let _ = CloseHandle(job);
                return Err(AgentError::Context(
                    "set job object limits for the sandbox failed".into(),
                ));
            }
            Ok(Some(JobObject(job)))
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

#[cfg(windows)]
use job_object::JobObject;

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

    #[cfg(windows)]
    #[test]
    fn job_object_assigns_and_terminates() {
        // Assigning a real process to the sandbox's Job-Object must let the
        // host terminate it in one kernel call. Skipped (not failed) when
        // the runner itself is confined by an outer Job-Object — CI runners
        // cannot nest, and the production path degrades the same way.
        use super::job_object::create_job_object;
        let sandbox = ProcessSandbox {
            process_limit: 4,
            ..ProcessSandbox::default()
        };
        let job = create_job_object(&sandbox)
            .expect("create the job")
            .expect("a quota was requested");
        let mut child = std::process::Command::new("ping")
            .args(["-n", "300", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping");
        if !job.assign(child.id()).expect("assign the process") {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: outer job confinement prevents nesting");
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the assigned process must still be running"
        );
        assert!(job.terminate(), "terminating the job must succeed");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success(), "a terminated child reports failure");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child survived job termination"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(windows)]
    #[test]
    fn job_object_caps_active_processes() {
        // The kernel enforces the active-process ceiling: once one job
        // holds its limit, assigning another process to it fails. Skipped
        // (not failed) under an outer job on CI runners.
        use super::job_object::create_job_object;
        let sandbox = ProcessSandbox {
            process_limit: 2,
            ..ProcessSandbox::default()
        };
        let job = create_job_object(&sandbox)
            .expect("create the job")
            .expect("a quota was requested");
        let spawn_ping = || {
            std::process::Command::new("ping")
                .args(["-n", "300", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn ping")
        };
        let mut first = spawn_ping();
        if !job.assign(first.id()).expect("assign first") {
            let _ = first.kill();
            let _ = first.wait();
            eprintln!("skipped: outer job confinement prevents nesting");
            return;
        }
        // Second process sits exactly at the ceiling: assignment succeeds.
        let mut second = spawn_ping();
        assert!(
            job.assign(second.id()).expect("assign second"),
            "the second process fits within the ceiling"
        );
        // Third process exceeds the ceiling: the kernel refuses.
        let mut third = spawn_ping();
        assert!(
            !job.assign(third.id()).expect("assign third"),
            "the third process must be refused by the active-process ceiling"
        );
        // Terminating the job reaps the two assigned processes; the
        // unassigned third is killed and reaped directly.
        let _ = job.terminate();
        let _ = first.wait();
        let _ = second.wait();
        let _ = third.kill();
        let _ = third.wait();
    }

    /// A chatty child must not grow the parent's memory or flood the
    /// console: with `stderr_capture_bytes` set, the host pipes stderr and
    /// drains it into a bounded tail that keeps the newest bytes only.
    #[tokio::test]
    async fn stderr_is_drained_into_a_bounded_tail() {
        let program = resolve_program(Some("CARGO_BIN_EXE_mock_host"), "mock_host");
        let host = ProcessHost::connect(ProcessHostConfig {
            program: program.clone(),
            args: vec!["--serve".into()],
            env: vec![
                ("MOCK_MARKER".into(), "1".into()),
                // The mock writes 4 MiB of junk plus a tail marker to
                // stderr at startup.
                (
                    "MOCK_STDERR_FLOOD_BYTES".into(),
                    (4 * 1024 * 1024).to_string(),
                ),
            ],
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            max_frame_bytes: 1024 * 1024,
            sandbox: ProcessSandbox {
                env_whitelist: Some(vec![
                    "PATH".into(),
                    "SystemRoot".into(),
                    "SystemDrive".into(),
                    "TEMP".into(),
                    "TMP".into(),
                ]),
                // The bounded tail: 8 KiB, far below the 4 MiB flood.
                stderr_capture_bytes: 8 * 1024,
                ..ProcessSandbox::default()
            },
        })
        .await
        .expect("spawn mock host");

        // Give the drainer time to consume the flood.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tail = host.stderr_tail().await;
        assert!(
            tail.len() <= 8 * 1024,
            "the stderr tail must be bounded: {} bytes",
            tail.len()
        );
        assert!(
            tail.trim_end().ends_with("STDERR_TAIL_MARKER"),
            "the tail keeps the newest bytes, got: {:?}",
            tail.chars().rev().take(40).collect::<String>()
        );
        host.shutdown().await;
    }
}
