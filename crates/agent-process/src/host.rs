//! The generic JSON-lines process host: protocol ping-pong on top of
//! [`crate::ProcessSupervisor`] (child lifecycle) and
//! [`crate::DuplexTransport`] (bounded framed bytes; stdio is the first
//! backend).
//!
//! Both the context-service adapter (`ContextEngine` over a process, in
//! `context-contextcore`) and the process capability adapter (a `Capability`
//! over a process, in `agent-capability-process`) are thin protocol layers
//! on top of this host — the framing, deadline, sandbox and failure policy
//! lives here once.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::time::Duration;

use agent_contracts::{AgentError, AgentResult};
use agent_platform_protocol::{ActiveFeatures, JsonDecodeBudget, decode_value};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout};

use crate::health::{ConnectionEpoch, ConnectionHealth, ConnectionStatus};
use crate::session::{DuplexTransport, FramedProtocolSession, StdioDuplexTransport};
use crate::supervisor::ProcessSupervisor;

/// Client protocol version echoed by every request; a mismatched child is
/// poisoned instead of misparsed.
pub const PROTOCOL_VERSION: u32 = 1;

/// How many mid-invoke system requests a child may make in one call. The
/// broker is the child's sanctioned I/O path, but it must stay bounded: a
/// child that spams system frames instead of answering is killed instead
/// of grown into the parent's time budget.
pub const MAX_SYSTEM_REQUESTS_PER_CALL: usize = 256;

/// Bound on waiting for a peer cancel-ACK after the host writes `op=cancel`.
/// Settlement is still kill-then-reap: a silent peer cannot delay cancel
/// past this window. Connection ACK is not Core `OperationCancelAck`.
pub const DEFAULT_CANCEL_ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// Bound on writing the `op=cancel` frame itself. A peer that stopped
/// reading must not stall the abort on a full pipe: if the frame cannot be
/// written within this window, the connection is settled kill-then-reap
/// exactly like a cancel-ACK timeout.
pub const CANCEL_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Coalesced progress notes stay tiny. The latest frame wins; earlier notes
/// are dropped. This is connection backpressure, never task or Core state.
pub const MAX_PROGRESS_NOTE_CHARS: usize = 128;

/// How many progress frames one call may emit. They are coalesced, so more
/// than this is a flood, not extra meaning.
pub const MAX_PROGRESS_FRAMES_PER_CALL: usize = 64;

/// A broker for the child's *system requests*: mid-invoke frames tagged
/// `{"system": <op>, ...}` that ask the host to perform access on the
/// child's behalf (a brokered filesystem read, later a network policy).
/// The caller (`ProcessHost::call_with_cancel_and_broker`) answers each
/// frame with `{"system_ok": bool, ...}` and the exchange continues until
/// the normal `{id, version, ok, value}` response arrives. The broker is
/// the enforcement point for "experimental code cannot exceed the
/// permissions granted to it": it checks the grant and confines every
/// path itself, so the child never reaches the filesystem directly.
#[async_trait::async_trait]
pub trait SystemBroker: Send + Sync {
    /// Handle one system request. `Err` is reported back to the child as
    /// `{"system_ok": false, "error": ...}`; `Ok(value)` as
    /// `{"system_ok": true, "value": value}`.
    async fn handle(&self, request: serde_json::Value) -> AgentResult<serde_json::Value>;
}

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
    /// Linux landlock confinement (kernel 5.13+): the child may create,
    /// modify or destroy filesystem state only under these roots, and on
    /// ABI v4+ (kernel 6.7+) it cannot bind or connect TCP, on ABI v5+
    /// (kernel 6.10+) newly opened devices cannot ioctl, and on ABI v6
    /// (kernel 6.12+) it cannot signal processes outside its landlock
    /// domain. Enforced by
    /// the kernel in the child before `exec`, inherited by every
    /// descendant. Reads are deliberately unhandled (the executable and
    /// loader must stay readable; reads are gated by the app-level
    /// broker). UDP, raw sockets and pathname Unix stay unhandled.
    /// Empty = no landlock. Unsupported kernels degrade to a warning
    /// (the app-level broker remains); a configured root that cannot be
    /// opened fails the spawn.
    #[cfg(target_os = "linux")]
    pub landlock_write_roots: Vec<std::path::PathBuf>,
    /// Per-process memory ceiling. `0` = unlimited.
    /// Windows: Job-Object `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (committed).
    /// Unix: `RLIMIT_AS` (virtual address space, including mappings; this
    /// is coarser than the Windows commit charge). Combined with landlock
    /// in one `pre_exec` so a second hook cannot replace the rlimits.
    #[cfg(windows)]
    pub job_max_memory_bytes: u64,
    #[cfg(unix)]
    pub max_memory_bytes: u64,
    /// Unix `RLIMIT_FSIZE`: the largest file the child may
    /// create or extend. `0` = unlimited. A write past this limit fails
    /// with `EFBIG` (and `SIGXFSZ` unless ignored). This is a per-file
    /// size cap, not I/O bandwidth and not a Windows Job-Object feature.
    #[cfg(unix)]
    pub max_file_bytes: u64,
    /// Unix `RLIMIT_NOFILE`: how many file descriptors the
    /// child may have open. `0` = unlimited. Combined with
    /// [`close_inherited_fds`] in the same `pre_exec` so a parent fd
    /// without `O_CLOEXEC` cannot leak across exec. The close scan is
    /// capped (see that function); this is not I/O bandwidth.
    #[cfg(unix)]
    pub max_open_files: u64,
    // Unix `RLIMIT_CORE` is not a field: other rlimit fields
    // use `0` = unlimited, so a `max_core_bytes: 0` would be ambiguous.
    // Whenever `apply_unix_rlimits` runs (capability / MCP `pre_exec`),
    // core dumps are forced to zero so a crash cannot leak sandbox
    // secrets into a dump file. Probe via `getrlimit`, not by crashing.
    // Linux also clamps `RLIMIT_NICE` / `RLIMIT_RTPRIO` to zero and sets
    // `no_new_privs`; those are not fields either.
    /// Windows integrity write confinement (Low IL): the child
    /// may create/modify/destroy filesystem state only under these roots.
    /// The parent labels each root Low and re-spawns through this process
    /// as a wrap that drops to Low IL before CreateProcess of the real
    /// program. Empty = no integrity wrap. A root that cannot be labeled
    /// fails the spawn. Reads and TCP stay unhandled.
    #[cfg(windows)]
    pub integrity_write_roots: Vec<std::path::PathBuf>,
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
    /// Hard cap on one frame in *both* directions: a response frame read
    /// from the child is rejected while reading, and a request/system
    /// answer frame written by the host is rejected before a byte is
    /// written (the connection stays usable).
    pub max_frame_bytes: usize,
    /// Hard cap on the total bytes exchanged in ONE call (request frame +
    /// every system frame and answer + the response frame). A child that
    /// dribbles many bounded frames can no longer move unbounded data per
    /// call; exceeding it poisons and terminates the session.
    pub max_call_bytes: usize,
    /// Cap on one broker answer's decoded value. The broker is trusted and
    /// itself bounded (see the capability adapter's fs.read range), so an
    /// over-cap answer degrades to a bounded `system_ok: false` answer to
    /// the child instead of growing the control plane without limit.
    pub max_system_answer_bytes: usize,
    /// The child's execution boundary (env, cwd, resource limits).
    pub sandbox: ProcessSandbox,
    /// 本端在 ping 中提供的特性。对端不能扩充；交集写入 [`ProcessHost::negotiated_features`]。
    pub offered_features: ActiveFeatures,
}

/// Apply hard rlimits in the child after `fork`, before `exec`.
///
/// Only `setrlimit` / `prctl` (via `syscall`) are used (async-signal-safe).
/// For CPU / NPROC / AS / FSIZE / NOFILE, `0` leaves that resource
/// unchanged (unlimited). `RLIMIT_CORE` is always set to zero when this
/// function runs so a crash cannot dump sandbox secrets; that
/// is not expressed as a sixth `0` = unlimited argument. On Linux the
/// same hook also clamps `RLIMIT_NICE` / `RLIMIT_RTPRIO` to zero and
/// sets `PR_SET_NO_NEW_PRIVS` so a parent with a raised
/// priority ceiling cannot leak into the child and a setuid exec cannot
/// escalate even when landlock is skipped. Linux GNU types the resource
/// id as `u32`; macOS uses `c_int` — calling `setrlimit` with the
/// `RLIMIT_*` constants keeps both compiling.
#[cfg(unix)]
pub fn apply_unix_rlimits(
    cpu_secs: u64,
    nproc: u64,
    memory_bytes: u64,
    file_bytes: u64,
    open_files: u64,
) -> std::io::Result<()> {
    macro_rules! set {
        ($resource:expr, $value:expr) => {{
            if $value > 0 {
                let limit = libc::rlimit {
                    rlim_cur: $value as libc::rlim_t,
                    rlim_max: $value as libc::rlim_t,
                };
                if unsafe { libc::setrlimit($resource, &limit) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }};
    }
    set!(libc::RLIMIT_CPU, cpu_secs);
    set!(libc::RLIMIT_NPROC, nproc);
    set!(libc::RLIMIT_AS, memory_bytes);
    set!(libc::RLIMIT_FSIZE, file_bytes);
    set!(libc::RLIMIT_NOFILE, open_files);
    // Always-zero: unlike the five arguments above, 0 here means no core
    // file, not "leave unlimited". Fail closed if the kernel refuses.
    let core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    {
        // Same always-zero meaning as CORE: pin the child's nice/rtprio
        // ceiling so a raised parent limit cannot leak across exec.
        if unsafe { libc::setrlimit(libc::RLIMIT_NICE, &core) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::setrlimit(libc::RLIMIT_RTPRIO, &core) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // `PR_SET_NO_NEW_PRIVS` (38). libc 0.2 does not export `prctl` /
        // the `PR_*` constants on gnu, so use the syscall directly.
        const PR_SET_NO_NEW_PRIVS: libc::c_long = 38;
        if unsafe { libc::syscall(libc::SYS_prctl, PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Close inherited descriptors other than stdin/stdout/stderr.
///
/// Call this *after* landlock `apply_in_child`: the write-root `O_PATH`
/// fds must stay open through `landlock_restrict_self`. Only `close` is
/// used (async-signal-safe). The scan is a fixed `MAX_INHERITED_FD_SCAN`
/// so an unlimited `RLIMIT_NOFILE` cannot turn this into an unbounded
/// loop, and so fds already open above a newly applied `RLIMIT_NOFILE`
/// are still closed. Fds at or above the cap are not closed; the parent
/// is trusted and Rust typically sets `O_CLOEXEC` on new files.
#[cfg(unix)]
pub fn close_inherited_fds() {
    const MAX_INHERITED_FD_SCAN: libc::c_int = 4096;
    for fd in 3..MAX_INHERITED_FD_SCAN {
        // Skip descriptors that exec closes anyway (FD_CLOEXEC): the
        // spawn error pipe relies on surviving until exec, so sweeping it
        // would turn a missing executable into a silent immediate child
        // exit instead of a spawn error.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && (flags & libc::FD_CLOEXEC) != 0 {
            continue;
        }
        let _ = unsafe { libc::close(fd) };
    }
}

fn attest_sandbox(
    sandbox: &ProcessSandbox,
    landlock_applied: bool,
    windows_job: bool,
) -> agent_contracts::SandboxAttestation {
    let mut actual = agent_contracts::SandboxCapabilities::default();
    let mut evidence = agent_contracts::SandboxEvidence::default();
    #[cfg(target_os = "linux")]
    if landlock_applied {
        // The claimed write floor must name only what the kernel actually
        // enforces. LANDLOCK_ACCESS_FS_TRUNCATE (truncate / open with
        // O_TRUNC) exists from ABI v3 (kernel 6.2+) and
        // LANDLOCK_ACCESS_FS_REFER (cross-hierarchy rename) from ABI v2
        // (kernel 5.19+); on ABI 1/2 the applied ruleset cannot block
        // those mutations outside the roots. Attest the full flag only
        // when every mutation it names is kernel-enforced; below that the
        // flag stays false so `Restricted` fails closed instead of
        // trusting a partial fence. `backend_version` still names the
        // probed ABI, so the operator sees which floor is missing.
        let abi = crate::landlock::abi_level();
        if abi >= 3 {
            actual.fs_write_confined = true;
            evidence.fs_write_confined = Some(format!(
                "landlock ruleset (abi{abi}) confines writes to {} roots",
                sandbox.landlock_write_roots.len()
            ));
        }
        actual.tcp_connect_denied = crate::landlock::tcp_deny_available();
        if actual.tcp_connect_denied {
            evidence.tcp_connect_denied =
                Some("landlock handled_access_net denies tcp bind/connect".into());
        }
        actual.signal_scoped = crate::landlock::signal_scope_available();
        if actual.signal_scoped {
            evidence.signal_scoped =
                Some("landlock ipc scope blocks signals outside the domain".into());
        }
    }
    #[cfg(unix)]
    {
        actual.cpu_quota = sandbox.cpu_time_limit_secs > 0;
        if actual.cpu_quota {
            evidence.cpu_quota = Some(format!("rlimit_cpu hard={}s", sandbox.cpu_time_limit_secs));
        }
        actual.memory_quota = sandbox.max_memory_bytes > 0;
        if actual.memory_quota {
            evidence.memory_quota = Some(format!("rlimit_as={} bytes", sandbox.max_memory_bytes));
        }
        actual.fd_quota = sandbox.max_open_files > 0;
        if actual.fd_quota {
            evidence.fd_quota = Some(format!("rlimit_nofile={}", sandbox.max_open_files));
        }
        // RLIMIT_NPROC is a user-level *count quota*, not proof that
        // arbitrary spawning is impossible; the attestation field is
        // named for exactly that guarantee.
        actual.process_count_quota = sandbox.process_limit > 0;
        if actual.process_count_quota {
            evidence.process_count_quota = Some(format!(
                "rlimit_nproc={} (count quota, not spawn denial)",
                sandbox.process_limit
            ));
        }
    }
    #[cfg(windows)]
    {
        actual.fs_write_confined = !sandbox.integrity_write_roots.is_empty();
        if actual.fs_write_confined {
            evidence.fs_write_confined = Some(format!(
                "integrity labels confine writes to {} roots",
                sandbox.integrity_write_roots.len()
            ));
        }
        actual.process_count_quota = windows_job;
        if actual.process_count_quota {
            evidence.process_count_quota = Some("job object active-process count quota".into());
        }
        actual.memory_quota = windows_job && sandbox.job_max_memory_bytes > 0;
        if actual.memory_quota {
            evidence.memory_quota = Some(format!(
                "job object memory={} bytes",
                sandbox.job_max_memory_bytes
            ));
        }
    }
    #[cfg(target_os = "linux")]
    let (backend, backend_version) = if landlock_applied {
        (
            "landlock+rlimits".to_string(),
            format!("abi{}", crate::landlock::abi_level()),
        )
    } else {
        ("rlimits".to_string(), "none".to_string())
    };
    #[cfg(all(unix, not(target_os = "linux")))]
    let (backend, backend_version) = ("rlimits".to_string(), "none".to_string());
    #[cfg(windows)]
    let (backend, backend_version) = ("integrity+jobobject".to_string(), "1".to_string());
    // Platform-specific branches consume only their own parameters.
    let _ = (landlock_applied, windows_job);
    agent_contracts::SandboxAttestation {
        capabilities: actual,
        backend,
        backend_version,
        evidence,
    }
}

/// A live child process speaking JSON-lines on stdio. Strict ping-pong:
/// one request in flight at a time (the `Mutex`), because the callers are
/// `&self` traits. Supervision ([`ProcessSupervisor`]) and framed bytes
/// ([`DuplexTransport`], stdio backend [`StdioDuplexTransport`]) are
/// separate from the protocol exchange.
pub struct ProcessHost {
    supervisor: ProcessSupervisor,
    config: ProcessHostConfig,
    transport: Mutex<StdioDuplexTransport>,
    /// Sticky first session fault, independent of the transport mutex.
    /// Timeout and cancellation paths must fence every later call even while
    /// the cancelled exchange still owns the async IO guard.
    poisoned: std::sync::Mutex<Option<String>>,
    next_id: AtomicU32,
    peer_epoch: AtomicU64,
    stderr_saturated: Arc<AtomicBool>,
    /// ping 握手交叉后的特性。缺省为空：历史纯 ToolOutput 默认关闭。
    negotiated_features: ActiveFeatures,
    /// Capabilities actually applied at spawn, not the configured policy.
    attestation: agent_contracts::SandboxAttestation,
}

impl ProcessHost {
    /// Spawn the child and require a successful handshake within the startup
    /// deadline, so a missing or broken program fails at connect time, not
    /// on the first real call.
    pub async fn connect(config: ProcessHostConfig) -> AgentResult<Self> {
        #[cfg(windows)]
        let (spawn_program, spawn_args) = if config.sandbox.integrity_write_roots.is_empty() {
            (config.program.clone(), config.args.clone())
        } else {
            if let Some(cwd) = &config.sandbox.cwd {
                std::fs::create_dir_all(cwd).map_err(|e| {
                    AgentError::Context(format!("create sandbox cwd '{}': {e}", cwd.display()))
                })?;
            }
            crate::integrity::label_write_roots(&config.sandbox.integrity_write_roots)
                .map_err(|e| AgentError::Context(format!("integrity sandbox setup: {e}")))?;
            crate::integrity::wrap_command(&config.program, &config.args)
                .map_err(|e| AgentError::Context(format!("integrity wrap: {e}")))?
        };
        #[cfg(not(windows))]
        let (spawn_program, spawn_args) = (config.program.clone(), config.args.clone());

        let mut command = Command::new(&spawn_program);
        command
            .args(&spawn_args)
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

        // Sandbox (Unix): hard CPU / process / address-space / file-size /
        // open-file ceilings via rlimits, applied right after fork. The
        // same hook always zeros RLIMIT_CORE so a crash cannot
        // dump secrets, and on Linux clamps NICE/RTPRIO and sets
        // no_new_privs even when landlock is skipped. On Linux
        // the same pre_exec also applies landlock so a second closure
        // cannot replace this one on toolchains that keep only the last
        // hook. Inherited fds other than stdio are closed after landlock
        // so a parent descriptor without O_CLOEXEC cannot leak across
        // exec.
        #[cfg(target_os = "linux")]
        let landlock_rules = if config.sandbox.landlock_write_roots.is_empty() {
            None
        } else if !crate::landlock::available() {
            eprintln!(
                "landlock sandbox skipped: kernel support unavailable \
                 (OS-level write/TCP confinement off for '{}')",
                config.program
            );
            None
        } else {
            Some(
                crate::landlock::ChildRules::open(&config.sandbox.landlock_write_roots)
                    .map_err(|e| AgentError::Context(format!("landlock sandbox setup: {e}")))?,
            )
        };
        #[cfg(target_os = "linux")]
        let landlock_applied = landlock_rules.is_some();
        #[cfg(not(target_os = "linux"))]
        let landlock_applied = false;
        #[cfg(unix)]
        {
            let cpu = config.sandbox.cpu_time_limit_secs;
            let nproc = config.sandbox.process_limit;
            let memory = config.sandbox.max_memory_bytes;
            let file = config.sandbox.max_file_bytes;
            let nofile = config.sandbox.max_open_files;
            #[cfg(target_os = "linux")]
            let apply_landlock = landlock_rules.is_some();
            #[cfg(not(target_os = "linux"))]
            let apply_landlock = false;
            if cpu > 0 || nproc > 0 || memory > 0 || file > 0 || nofile > 0 || apply_landlock {
                unsafe {
                    command.pre_exec(move || {
                        apply_unix_rlimits(cpu, nproc, memory, file, nofile)?;
                        #[cfg(target_os = "linux")]
                        if let Some(rules) = landlock_rules.as_ref() {
                            crate::landlock::apply_in_child(rules)?;
                        }
                        close_inherited_fds();
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

        let attestation = attest_sandbox(
            &config.sandbox,
            landlock_applied,
            #[cfg(windows)]
            job.is_some(),
            #[cfg(not(windows))]
            false,
        );
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Context("child stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Context("child stdout unavailable".into()))?;

        let stderr_tail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let stderr_saturated = Arc::new(AtomicBool::new(false));
        // Bounded stderr: when the sandbox asks for capture, a dedicated
        // drainer task reads the pipe into a ring that keeps only the last
        // `stderr_capture_bytes`. The child can stream forever without the
        // parent buffering it all — the tail is the only memory it ever
        // occupies, and it is surfaced on connection errors. A full ring
        // marks the connection Degraded; it does not poison.
        if let Some(stderr) = child.stderr.take() {
            let cap = config.sandbox.stderr_capture_bytes;
            let tail = Arc::clone(&stderr_tail);
            let saturated = Arc::clone(&stderr_saturated);
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
                            if cap > 0 && ring.len() >= cap {
                                saturated.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
            });
        }

        let max_frame_bytes = config.max_frame_bytes;
        let mut host = Self {
            supervisor: ProcessSupervisor::new(
                child,
                pid,
                #[cfg(windows)]
                job,
                Arc::clone(&stderr_tail),
            ),
            config,
            transport: Mutex::new(FramedProtocolSession::new(
                BufReader::new(stdout),
                stdin,
                max_frame_bytes,
            )?),
            poisoned: std::sync::Mutex::new(None),
            next_id: AtomicU32::new(1),
            peer_epoch: AtomicU64::new(0),
            stderr_saturated,
            negotiated_features: ActiveFeatures::default(),
            attestation,
        };
        let mut ping = json!({ "op": "ping", "host_epoch": 1u64 });
        if !host.config.offered_features.is_empty() {
            ping["features"] = json!(host.config.offered_features.as_slice());
        }
        let response = match timeout(
            host.config.startup_timeout,
            host.exchange_response(ping, None, None),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                host.poison_and_reap(format!("process handshake: {error}"))
                    .await;
                return Err(AgentError::Context(format!("process handshake: {error}")));
            }
            Err(_) => {
                host.poison_and_reap("handshake ping timed out".into())
                    .await;
                return Err(AgentError::Context(format!(
                    "process '{}' did not respond to ping within {:?}",
                    host.config.program, host.config.startup_timeout
                )));
            }
        };
        let advertised = match ActiveFeatures::from_json_value(response.get("features")) {
            Ok(features) => features,
            Err(error) => {
                host.poison_and_reap(format!("handshake features: {error}"))
                    .await;
                return Err(AgentError::Context(format!(
                    "process '{}' handshake features: {error}",
                    host.config.program
                )));
            }
        };
        host.negotiated_features = host.config.offered_features.intersect(&advertised);
        if let Some(peer) = response.get("epoch").and_then(Value::as_u64) {
            host.peer_epoch.store(peer, Ordering::Relaxed);
        }
        Ok(host)
    }

    /// Capabilities the OS actually applied to this child, with the
    /// mechanism behind every enforced flag.
    pub fn sandbox_attestation(&self) -> agent_contracts::SandboxAttestation {
        self.attestation.clone()
    }

    /// Connection health and epochs. Never task or Core authority.
    pub fn status(&self) -> ConnectionStatus {
        if let Some(reason) = self.poison_reason() {
            return ConnectionStatus {
                health: ConnectionHealth::Quarantined,
                epoch: self.epoch(),
                reason: Some(reason),
            };
        }
        if self.stderr_saturated.load(Ordering::Relaxed) {
            return ConnectionStatus {
                health: ConnectionHealth::Degraded,
                epoch: self.epoch(),
                reason: Some("stderr capture saturated".into()),
            };
        }
        ConnectionStatus {
            health: ConnectionHealth::Ready,
            epoch: self.epoch(),
            reason: None,
        }
    }

    pub fn epoch(&self) -> ConnectionEpoch {
        ConnectionEpoch {
            host: 1,
            peer: self.peer_epoch.load(Ordering::Relaxed),
        }
    }

    /// 握手交叉后的特性。子端不能单方面打开未提供的项。
    pub fn negotiated_features(&self) -> &ActiveFeatures {
        &self.negotiated_features
    }

    pub fn allows_feature(&self, feature: &str) -> bool {
        self.negotiated_features.contains(feature)
    }

    /// One framed call with the per-request deadline. A timeout poisons the
    /// connection: the request may have been written, so the response — if
    /// it ever arrives — must not be mistaken for a later request's answer.
    pub async fn call(&self, op: Value) -> AgentResult<Value> {
        match timeout(self.config.request_timeout, self.call_unbounded(op)).await {
            Ok(inner) => inner,
            Err(_) => {
                self.poison_and_reap(format!(
                    "request timed out after {:?}",
                    self.config.request_timeout
                ))
                .await;
                Err(AgentError::Context(format!(
                    "process '{}' request timed out after {:?}; connection poisoned",
                    self.config.program, self.config.request_timeout
                )))
            }
        }
    }

    /// One framed call that also aborts when `cancel` fires (a user
    /// `/cancel` or a superseded operation must stop the subprocess
    /// *now*, not at the request deadline). If the request was already
    /// written, the host sends a peer `op=cancel` and waits a bounded
    /// cancel-ACK; settlement is still kill-then-reap so a silent child
    /// cannot stall cancel. A late value is never admitted. Cancel before
    /// any write leaves the connection usable.
    pub async fn call_with_cancel(
        &self,
        op: Value,
        cancel: &agent_contracts::CancellationToken,
    ) -> AgentResult<Value> {
        match timeout(
            self.config.request_timeout,
            self.exchange(op, None, Some(cancel)),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => {
                self.poison_and_reap(format!(
                    "request timed out after {:?}",
                    self.config.request_timeout
                ))
                .await;
                Err(AgentError::Context(format!(
                    "process '{}' request timed out after {:?}; connection poisoned",
                    self.config.program, self.config.request_timeout
                )))
            }
        }
    }

    /// One framed call with the per-request deadline and a broker for
    /// mid-invoke system requests (a brokered filesystem read). A timeout
    /// or a cancel after write poisons the connection and kills the child
    /// tree. Peer cancel-ACK is bounded; kill-then-reap remains the
    /// settlement.
    pub async fn call_with_cancel_and_broker<B>(
        &self,
        op: Value,
        cancel: &agent_contracts::CancellationToken,
        broker: &B,
    ) -> AgentResult<Value>
    where
        B: SystemBroker,
    {
        match timeout(
            self.config.request_timeout,
            self.exchange(op, Some(broker), Some(cancel)),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => {
                self.poison_and_reap(format!(
                    "request timed out after {:?}",
                    self.config.request_timeout
                ))
                .await;
                Err(AgentError::Context(format!(
                    "process '{}' request timed out after {:?}; connection poisoned",
                    self.config.program, self.config.request_timeout
                )))
            }
        }
    }

    async fn call_unbounded(&self, op: Value) -> AgentResult<Value> {
        self.exchange(op, None, None).await
    }

    /// Write one request frame, then read frames until the final response.
    /// With a broker, a frame tagged `{"system": <op>, ...}` is a *system
    /// request* from the child: the broker answers it (fail-closed: a
    /// refusal goes back to the child as `{"system_ok": false, ...}`) and
    /// the exchange continues. Without a broker the first frame is the
    /// response, exactly as before — a system frame with no broker to
    /// answer it poisons the connection instead of being misparsed.
    ///
    /// Mid-call `progress` frames are coalesced (latest wins, extras
    /// dropped) and never become task or Core state. After a written
    /// request is cancelled the host sends `op=cancel` and waits a bounded
    /// peer ACK; a late `{ok, value}` is discarded. Settlement is still
    /// kill-then-reap.
    ///
    /// Every direction is bounded: the request and the system answers are
    /// capped before a byte is written (`encode_frame`), response frames are
    /// capped while reading, and the total bytes moved by one call are
    /// capped by `max_call_bytes`. Every framing violation — oversize,
    /// partial EOF, non-UTF-8 or unparseable frames —
    /// poisons the connection and terminates the child tree, so a
    /// half-consumed exchange can never corrupt a later request/response
    /// pair.
    async fn exchange(
        &self,
        op: Value,
        broker: Option<&dyn SystemBroker>,
        cancel: Option<&agent_contracts::CancellationToken>,
    ) -> AgentResult<Value> {
        Ok(self
            .exchange_response(op, broker, cancel)
            .await?
            .get("value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn exchange_response(
        &self,
        op: Value,
        broker: Option<&dyn SystemBroker>,
        cancel: Option<&agent_contracts::CancellationToken>,
    ) -> AgentResult<Value> {
        let result = self.exchange_once(op, broker, cancel).await;
        if self.poison_reason().is_some() {
            self.supervisor.reap().await;
        }
        result
    }

    async fn exchange_once(
        &self,
        op: Value,
        broker: Option<&dyn SystemBroker>,
        cancel: Option<&agent_contracts::CancellationToken>,
    ) -> AgentResult<Value> {
        if let Some(reason) = self.poison_reason() {
            return Err(AgentError::Context(format!(
                "process '{}' connection poisoned: {reason}",
                self.config.program
            )));
        }
        let mut transport = self.transport.lock().await;
        if let Some(reason) = transport.poison_reason() {
            return Err(AgentError::Context(format!(
                "process '{}' connection poisoned: {reason}",
                self.config.program
            )));
        }
        if cancel.is_some_and(agent_contracts::CancellationToken::is_cancelled) {
            // Nothing was written; the pipe is still in sync.
            return Err(AgentError::Cancelled);
        }
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        if sequence == u32::MAX {
            return Err(self.frame_violation(&mut *transport, "request id space exhausted".into()));
        }
        // UUID v4 supplies an unpredictable per-request correlation id.
        // The child cannot pre-send the next response merely by observing
        // earlier ids; the low sequence bits remain useful in diagnostics.
        let random = uuid::Uuid::new_v4().as_u128() as u64;
        let id = (random & 0xffff_ffff_0000_0000) | u64::from(sequence);
        let mut request = Map::new();
        request.insert("id".into(), json!(id));
        request.insert("version".into(), json!(PROTOCOL_VERSION));
        if let Value::Object(mut fields) = op {
            // Caller operations are payload only. Transport identity and
            // version are host-owned and cannot be overwritten by an
            // adapter-provided JSON object.
            fields.remove("id");
            fields.remove("version");
            request.extend(fields);
        }
        // Outbound cap: the request is rejected *before* any byte is
        // written, so an over-cap call can never leave a half-written frame
        // on the pipe and the connection stays usable.
        let request_line =
            crate::frame::encode_frame(&Value::Object(request), self.config.max_frame_bytes)?;
        let mut exchanged = request_line.len();

        if let Err(error) = transport.send_encoded_line(&request_line).await {
            return Err(self.transport_error(&mut *transport, "write", error));
        }

        let mut system_calls = 0usize;
        let mut progress_frames = 0usize;
        let mut cancel_sent = false;
        let mut ack_deadline: Option<Instant> = None;
        loop {
            tokio::select! {
                biased;
                _ = async {
                    match cancel {
                        Some(token) => token.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                }, if !cancel_sent && cancel.is_some() => {
                    let cancel_frame = json!({
                        "id": id,
                        "version": PROTOCOL_VERSION,
                        "op": "cancel",
                    });
                    if let Ok(line) =
                        crate::frame::encode_frame(&cancel_frame, self.config.max_frame_bytes)
                        && let Some(total) = exchanged.checked_add(line.len())
                        && total <= self.config.max_call_bytes
                    {
                        exchanged = total;
                        // The cancel frame write is itself bounded: a peer
                        // that stopped reading must not stall the abort on
                        // a full pipe. Settlement is kill-then-reap either
                        // way.
                        if tokio::time::timeout(
                            CANCEL_SEND_TIMEOUT,
                            transport.send_encoded_line(&line),
                        )
                        .await
                        .is_err()
                        {
                            return Err(self.settle_cancelled(&mut *transport));
                        }
                    }
                    cancel_sent = true;
                    ack_deadline = Some(Instant::now() + DEFAULT_CANCEL_ACK_TIMEOUT);
                }
                _ = async {
                    if let Some(deadline) = ack_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if ack_deadline.is_some() => {
                    return Err(self.settle_cancelled(&mut *transport));
                }
                recv = transport.recv() => {
                    let frame = match recv {
                        Ok(frame) => frame,
                        Err(error) => {
                            return Err(self.frame_violation(&mut *transport, error.to_string()));
                        }
                    };
                    exchanged = match exchanged.checked_add(frame.len() + 1) {
                        Some(total) => total,
                        None => {
                            return Err(self.frame_violation(
                                &mut *transport,
                                "call byte accounting overflowed".into(),
                            ));
                        }
                    };
                    if exchanged > self.config.max_call_bytes {
                        return Err(self.frame_violation(
                            &mut *transport,
                            format!(
                                "call exchanged {exchanged} bytes, above the {} byte per-call bound",
                                self.config.max_call_bytes
                            ),
                        ));
                    }

                    let budget = JsonDecodeBudget::for_frame_bytes(self.config.max_frame_bytes);
                    let response: Value = match decode_value(&frame, &budget) {
                        Ok(response) => response,
                        Err(error) => {
                            return Err(self.frame_violation(
                                &mut *transport,
                                format!("parse response: {error}"),
                            ));
                        }
                    };

                    if response.get("system").and_then(Value::as_str).is_some() {
                        if cancel_sent {
                            // Do not extend the child's work after cancel.
                            continue;
                        }
                        let Some(broker) = broker else {
                            return Err(self.frame_violation(
                                &mut *transport,
                                "child sent a system request with no broker installed".into(),
                            ));
                        };
                        system_calls += 1;
                        if system_calls > MAX_SYSTEM_REQUESTS_PER_CALL {
                            return Err(self.frame_violation(
                                &mut *transport,
                                format!(
                                    "too many system requests in one call (>{MAX_SYSTEM_REQUESTS_PER_CALL})"
                                ),
                            ));
                        }
                        // Broker work must not defuse cancel: a cancel that
                        // arrives while the broker answers the child's
                        // system request settles like any other
                        // cancel-after-write (kill-then-reap) instead of
                        // letting the broker run to completion.
                        let answered = tokio::select! {
                            biased;
                            _ = async {
                                match cancel {
                                    Some(token) => token.cancelled().await,
                                    None => std::future::pending::<()>().await,
                                }
                            } => {
                                return Err(self.settle_cancelled(&mut *transport));
                            }
                            handled = broker.handle(response) => handled,
                        };
                        let answer = match answered {
                            Ok(value) => json!({ "system_ok": true, "value": value }),
                            Err(error) => {
                                json!({ "system_ok": false, "error": error.to_string() })
                            }
                        };
                        let encoded = serde_json::to_string(&answer).map_err(|e| {
                            AgentError::Context(format!("serialize system answer: {e}"))
                        })?;
                        let answer = if encoded.len() > self.config.max_system_answer_bytes {
                            json!({
                                "system_ok": false,
                                "error": format!(
                                    "system answer exceeds the {} byte control-plane bound",
                                    self.config.max_system_answer_bytes
                                ),
                            })
                        } else {
                            answer
                        };
                        let answer_line =
                            crate::frame::encode_frame(&answer, self.config.max_frame_bytes)
                                .map_err(|e| self.frame_violation(&mut *transport, e.to_string()))?;
                        exchanged = match exchanged.checked_add(answer_line.len()) {
                            Some(total) => total,
                            None => {
                                return Err(self.frame_violation(
                                    &mut *transport,
                                    "call byte accounting overflowed".into(),
                                ));
                            }
                        };
                        if exchanged > self.config.max_call_bytes {
                            return Err(self.frame_violation(
                                &mut *transport,
                                format!(
                                    "call exchanged {exchanged} bytes, above the {} byte per-call bound",
                                    self.config.max_call_bytes
                                ),
                            ));
                        }
                        if let Err(error) = transport.send_encoded_line(&answer_line).await {
                            return Err(self.transport_error(&mut *transport, "write", error));
                        }
                        continue;
                    }

                    if response.get("cancelled").and_then(Value::as_bool) == Some(true) {
                        if response.get("id").and_then(Value::as_u64) != Some(id) {
                            return Err(self.frame_violation(
                                &mut *transport,
                                format!(
                                    "cancel-ACK id mismatch: got {:?}, expected {id}",
                                    response.get("id")
                                ),
                            ));
                        }
                        if !cancel_sent {
                            return Err(self.frame_violation(
                                &mut *transport,
                                "peer sent cancel-ACK with no host cancel".into(),
                            ));
                        }
                        return Err(self.settle_cancelled(&mut *transport));
                    }

                    if response.get("progress").and_then(Value::as_bool) == Some(true) {
                        if response.get("id").and_then(Value::as_u64) != Some(id) {
                            return Err(self.frame_violation(
                                &mut *transport,
                                format!(
                                    "progress id mismatch: got {:?}, expected {id}",
                                    response.get("id")
                                ),
                            ));
                        }
                        progress_frames += 1;
                        if progress_frames > MAX_PROGRESS_FRAMES_PER_CALL {
                            return Err(self.frame_violation(
                                &mut *transport,
                                format!(
                                    "too many progress frames in one call (>{MAX_PROGRESS_FRAMES_PER_CALL})"
                                ),
                            ));
                        }
                        let _ = bounded_progress_note(&response);
                        continue;
                    }

                    if response.get("id").and_then(Value::as_u64) != Some(id) {
                        return Err(self.frame_violation(
                            &mut *transport,
                            format!(
                                "response id mismatch: got {:?}, expected {id}",
                                response.get("id")
                            ),
                        ));
                    }
                    if response.get("version").and_then(Value::as_u64) != Some(PROTOCOL_VERSION as u64)
                    {
                        return Err(self.frame_violation(
                            &mut *transport,
                            "protocol version mismatch".into(),
                        ));
                    }
                    if cancel_sent {
                        // A late value after cancel is not this call's result.
                        return Err(self.settle_cancelled(&mut *transport));
                    }
                    match response.get("ok").and_then(Value::as_bool) {
                        Some(true) => {}
                        Some(false) => {
                            // The context service carries a bounded typed
                            // envelope; other children may still answer with
                            // a plain string. Both collapse onto the same
                            // Context failure so callers classify identically.
                            let error = match response.get("error") {
                                Some(Value::Object(map)) => {
                                    let category = map
                                        .get("category")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown");
                                    let message = map
                                        .get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown child error");
                                    format!("process error [{category}]: {message}")
                                }
                                Some(Value::String(message)) => {
                                    format!("process error: {message}")
                                }
                                _ => "unknown child error".into(),
                            };
                            return Err(AgentError::Context(error));
                        }
                        None => {
                            return Err(self.frame_violation(
                                &mut *transport,
                                "response field 'ok' must be a boolean".into(),
                            ));
                        }
                    }
                    return Ok(response);
                }
            }
        }
    }

    /// A framing violation: the connection can no longer be trusted (the
    /// reader may hold a half-consumed frame or a wrong response for a
    /// later request), so it is poisoned *and* the child tree is terminated
    /// — fail closed, never guessed past.
    fn frame_violation(&self, transport: &mut impl DuplexTransport, reason: String) -> AgentError {
        let reason = self.record_poison(reason);
        transport.poison(reason.clone());
        self.supervisor.kill_tree();
        AgentError::Context(format!(
            "process '{}' {reason}; connection poisoned and terminated",
            self.config.program
        ))
    }

    /// Cancel after a write: poison so a late value cannot be reused, kill
    /// the tree, return `Cancelled`. Peer ACK is optional evidence, not Core
    /// truth and not permission to keep the connection.
    fn settle_cancelled(&self, transport: &mut impl DuplexTransport) -> AgentError {
        let reason = self.record_poison("cancelled by the runtime".into());
        transport.poison(reason);
        self.supervisor.kill_tree();
        AgentError::Cancelled
    }

    /// Ask the child to exit gracefully, then reap it. The host is consumed,
    /// so no further calls can ride a closing pipe.
    pub async fn shutdown(self) {
        let _ = self.call(json!({ "op": "shutdown" })).await;
        self.supervisor.reap().await;
    }

    fn transport_error(
        &self,
        transport: &mut impl DuplexTransport,
        stage: &str,
        error: AgentError,
    ) -> AgentError {
        let message = error.to_string();
        let reason = self.record_poison(format!("{stage} failed: {message}"));
        transport.poison(reason);
        self.supervisor.kill_tree();
        let closed = message.contains("broken pipe")
            || message.contains("BrokenPipe")
            || message.contains("unexpected eof")
            || message.contains("UnexpectedEof");
        if closed {
            AgentError::Context(format!(
                "process '{}' connection closed: {message}",
                self.config.program
            ))
        } else {
            AgentError::Io(format!(
                "process '{}' {stage}: {message}",
                self.config.program
            ))
        }
    }

    /// The bounded tail of the child's stderr (empty when stderr is
    /// inherited or the child wrote nothing). Surfaced on connection
    /// errors so a failing child says *why* without the parent ever
    /// buffering unbounded stderr.
    pub async fn stderr_tail(&self) -> String {
        self.supervisor.stderr_tail().await
    }

    /// Mark the connection poisoned and kill the child's process tree.
    /// `try_lock` so this can be called from a timeout/cancel path without
    /// deadlocking on a guard that the cancelled future still holds.
    fn poison(&self, reason: String) {
        let reason = self.record_poison(reason);
        if let Ok(mut transport) = self.transport.try_lock() {
            transport.poison(reason);
        }
        self.supervisor.kill_tree();
    }

    async fn poison_and_reap(&self, reason: String) {
        self.poison(reason);
        self.supervisor.reap().await;
    }

    fn record_poison(&self, reason: String) -> String {
        let mut poisoned = self.poisoned.lock().unwrap_or_else(|e| e.into_inner());
        poisoned.get_or_insert(reason).clone()
    }

    fn poison_reason(&self) -> Option<String> {
        self.poisoned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn bounded_progress_note(response: &Value) -> String {
    let note = response.get("note").and_then(Value::as_str).unwrap_or("");
    note.chars().take(MAX_PROGRESS_NOTE_CHARS).collect()
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
        // Negative pid = the process group. Production children are spawned
        // as group leaders (`process_group(0)`), so their pgid == their pid
        // and SIGKILL to -pid reaches the whole tree. ESRCH means the child
        // was not spawned as a group leader; the direct child may still be
        // alive, so every spawn that can reach this path must set
        // `process_group(0)` to honor the kill contract.
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
        // taskkill 可能被沙箱挡住或不在 PATH 里；根 PID 再用
        // TerminateProcess 钉死，避免恢复路径把仍存活的孩子当成成功。
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_TERMINATE, TerminateProcess,
            };
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {}
}

/// The Windows Job-Object machinery behind the process sandbox:
/// kernel-enforced quotas (active-process ceiling, per-process memory
/// ceiling), `KILL_ON_JOB_CLOSE`, `DIE_ON_UNHANDLED_EXCEPTION`, and
/// `PRIORITY_CLASS=NORMAL` so the child cannot raise
/// HIGH/REALTIME. Breakaway stays default-deny. Closing the handle — the
/// host's drop — terminates every assigned process even if no explicit
/// kill ran.
#[cfg(windows)]
mod job_object {
    use super::{AgentError, AgentResult, ProcessSandbox};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PRIORITY_CLASS, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
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
            // PRIORITY_CLASS pins NORMAL so the child cannot raise to
            // HIGH/REALTIME. BREAKAWAY_OK / SILENT_BREAKAWAY_OK
            // stay unset (default-deny). Not a rate limit and not UI.
            let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_PRIORITY_CLASS;
            info.BasicLimitInformation.PriorityClass = NORMAL_PRIORITY_CLASS;
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
            QueryInformationJobObject,
        };

        #[test]
        fn created_job_pins_normal_priority_and_denies_breakaway() {
            let sandbox = ProcessSandbox {
                process_limit: 1,
                ..ProcessSandbox::default()
            };
            let job = create_job_object(&sandbox)
                .expect("create the job")
                .expect("a quota was requested");
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                let mut returned = 0u32;
                let ok = QueryInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_mut(&mut info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    &mut returned,
                );
                assert_ne!(
                    ok,
                    0,
                    "query job limits: {}",
                    std::io::Error::last_os_error()
                );
                assert_eq!(
                    info.BasicLimitInformation.PriorityClass, NORMAL_PRIORITY_CLASS,
                    "sandbox jobs must pin NORMAL so the child cannot raise HIGH/REALTIME"
                );
                let flags = info.BasicLimitInformation.LimitFlags;
                assert_ne!(
                    flags & JOB_OBJECT_LIMIT_PRIORITY_CLASS,
                    0,
                    "PRIORITY_CLASS must be in LimitFlags"
                );
                assert_eq!(
                    flags & JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                    0,
                    "BREAKAWAY_OK must stay unset (default-deny)"
                );
                assert_eq!(
                    flags & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
                    0,
                    "SILENT_BREAKAWAY_OK must stay unset"
                );
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

#[cfg(windows)]
pub(crate) use job_object::JobObject;

#[cfg(test)]
mod tests {
    use super::*;

    /// 证明串必须从真实配置值生成，且整体通过契约校验：布尔位与
    /// 证明一一对应，backend 标签有界。
    #[test]
    fn attestation_proves_every_enforced_flag_from_real_inputs() {
        let sandbox = ProcessSandbox {
            #[cfg(unix)]
            cpu_time_limit_secs: 20,
            #[cfg(unix)]
            process_limit: 64,
            #[cfg(unix)]
            max_memory_bytes: 512 * 1024 * 1024,
            #[cfg(unix)]
            max_open_files: 256,
            #[cfg(target_os = "linux")]
            landlock_write_roots: vec![std::path::PathBuf::from("/tmp/sandbox")],
            ..ProcessSandbox::default()
        };
        let attestation = attest_sandbox(&sandbox, cfg!(target_os = "linux"), false);
        attestation
            .validate()
            .expect("a real configuration must attest validly");
        assert!(
            !attestation.backend.is_empty(),
            "the backend must name its OS mechanism family"
        );
        #[cfg(unix)]
        {
            assert!(attestation.capabilities.cpu_quota);
            assert_eq!(
                attestation.evidence.cpu_quota.as_deref(),
                Some("rlimit_cpu hard=20s")
            );
            assert_eq!(
                attestation.evidence.memory_quota.as_deref(),
                Some("rlimit_as=536870912 bytes")
            );
            assert!(
                attestation
                    .evidence
                    .fd_quota
                    .as_deref()
                    .unwrap()
                    .contains("256")
            );
            assert!(
                attestation
                    .evidence
                    .process_count_quota
                    .as_deref()
                    .unwrap()
                    .contains("rlimit_nproc=64")
            );
            #[cfg(target_os = "linux")]
            {
                // The write floor is attested only when the kernel can
                // enforce every mutation it names (TRUNCATE needs ABI
                // v3+); on older ABIs the flag must stay false.
                let abi = crate::landlock::abi_level();
                assert_eq!(
                    attestation.capabilities.fs_write_confined,
                    abi >= 3,
                    "fs_write_confined must match the kernel's truncate coverage (abi {abi})"
                );
                if abi >= 3 {
                    let proof = attestation.evidence.fs_write_confined.as_deref().unwrap();
                    assert!(
                        proof.starts_with("landlock ruleset") && proof.contains("1 roots"),
                        "{proof}"
                    );
                } else {
                    assert!(attestation.evidence.fs_write_confined.is_none());
                }
            }
        }
        // 未强制的标志不得携带证明（validate 已整体检查，这里抽查）。
        assert!(attestation.evidence.udp_denied.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn attestation_leaves_the_write_floor_false_below_truncate_coverage() {
        let sandbox = ProcessSandbox {
            landlock_write_roots: vec![std::path::PathBuf::from("/tmp/sandbox")],
            ..ProcessSandbox::default()
        };
        let attestation = attest_sandbox(&sandbox, true, false);
        attestation
            .validate()
            .expect("a partial write floor must still attest validly");
        let abi = crate::landlock::abi_level();
        assert_eq!(
            attestation.capabilities.fs_write_confined,
            abi >= 3,
            "the write floor must be claimed exactly when the kernel handles truncate (abi {abi})"
        );
        assert_eq!(
            attestation.backend_version,
            format!("abi{abi}"),
            "the partial floor must still name its ABI"
        );
        if abi >= 3 {
            assert!(attestation.evidence.fs_write_confined.is_some());
        } else {
            assert!(
                attestation.evidence.fs_write_confined.is_none(),
                "a false flag must not carry proof text"
            );
        }
    }

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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
