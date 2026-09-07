//! The thin local Platform host (M17-P3).
//!
//! One process owns one workspace's writable runtime state and serves the
//! C0 run-scoped session routes over a local IPC endpoint, so the native
//! GUI, the TUI and any other client on this machine attach to the *same*
//! authority instead of each opening their own copy. Deliberately absent:
//! a scheduler, a second task authority, a second codec, network listeners.
//!
//! Session discipline (fail closed):
//! * the only endpoints are a current-user-scoped Windows Named Pipe
//!   (`PIPE_REJECT_REMOTE_CLIENTS` + current-user-only DACL + per-connection
//!   client-token check) and a Linux UDS with `SO_PEERCRED`; an
//!   unverifiable peer is dropped before its first frame is read;
//! * each verified connection gets a server-installed [`WorkControlGrant`]
//!   (operator, or read-only when the host was started so) and its own
//!   bound authorizer; wire strings never mint or widen authority;
//! * frames are bounded (1 MiB, matching the .NET client), decoded under
//!   the protocol crate's DOM budget, and every route outside the
//!   negotiated set answers the structured `route.unsupported` error;
//! * a malformed frame is a contract violation: the connection closes,
//!   it is never guessed through.

use std::io::{Read, Write};
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_core::{ApprovalBroker, InteractiveApprovalGate};
use agent_platform_protocol::{
    ApprovalRespondRequest, Causality, EffectStateDisposition,
    EnvelopeKind, JsonDecodeBudget, MAX_JSON_CONTROL_ARRAY_LEN, MAX_JSON_CONTROL_DEPTH,
    MAX_JSON_CONTROL_NODES, MAX_JSON_CONTROL_OBJECT_KEYS, MAX_JSON_CONTROL_STRING_BYTES,
    MAX_JSON_CONTROL_TOTAL_STRING_BYTES, MessageId, NegotiatedContractProfile, PlatformEnvelope,
    PlatformError, PlatformErrorClass, PlatformResponse, RetryDisposition, SchemaDigest,
    WorkCancelRequest, WorkContinueRequest, WorkSnapshotRequest, WorkSubscribeRequest,
    WorkSubmitRequest,
};
use agent_runtime::{RuntimeHandle, WorkControlGrant, WorkControlRouter, WorkControlSessionRegistry};

/// Structured error code for any route outside the negotiated session set.
pub const ERROR_ROUTE_UNSUPPORTED: &str = "route.unsupported";
use anyhow::Context as _;
use serde::de::DeserializeOwned;

/// Frame bound shared with the .NET client (`FrameCodec.DefaultMaxFrameBytes`).
pub const MAX_FRAME_BYTES: u32 = 1_024 * 1_024;
/// Bounded idle/read deadline per frame on deadline-capable transports (UDS).
pub const READ_DEADLINE: Duration = Duration::from_secs(120);
/// Concurrent client cap; a local host never needs more.
pub const MAX_CONNECTIONS: usize = 16;

/// Digest pairing the host and its clients on this session contract surface.
/// The .NET client's `AgentConnectionOptions.SchemaDigest` default must stay
/// byte-identical with this value's hex form.
pub fn session_schema_digest() -> SchemaDigest {
    SchemaDigest::sha256_bytes(b"focus-agent.platform.work.v1|run-scoped")
}

/// The hex form of [`session_schema_digest`].
pub fn session_schema_digest_hex() -> String {
    session_schema_digest().to_string()
}

/// The one protocol identity this host negotiates.
pub fn negotiated_profile() -> anyhow::Result<NegotiatedContractProfile> {
    NegotiatedContractProfile::new(
        "focus-agent.platform",
        agent_platform_protocol::ProtocolVersion { major: 1, minor: 0 },
        agent_platform_protocol::ActiveFeatures::new(Vec::new())
            .map_err(|error| anyhow::anyhow!(error.to_string()))?,
        session_schema_digest(),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

// ---------------------------------------------------------------------------
// Bounded framing (4-byte LE length + JSON), identical to the .NET client.
// ---------------------------------------------------------------------------

pub fn write_frame<W: Write>(stream: &mut W, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame exceeds the negotiated bound",
        ));
    }
    let mut header = [0u8; 4];
    header.copy_from_slice(&(payload.len() as u32).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

/// Reads one whole frame. `Ok(None)` on a clean EOF at a frame boundary;
/// a half frame is an error, never a message.
pub fn read_frame<R: Read>(stream: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(header);
    if length == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty frames are not valid messages",
        ));
    }
    if length > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame announces {length} bytes, above the {MAX_FRAME_BYTES} byte bound"),
        ));
    }
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}

fn decode_budget() -> JsonDecodeBudget {
    JsonDecodeBudget {
        max_depth: MAX_JSON_CONTROL_DEPTH,
        max_string_bytes: MAX_JSON_CONTROL_STRING_BYTES,
        max_total_string_bytes: MAX_JSON_CONTROL_TOTAL_STRING_BYTES,
        max_array_len: MAX_JSON_CONTROL_ARRAY_LEN,
        max_object_keys: MAX_JSON_CONTROL_OBJECT_KEYS,
        max_nodes: MAX_JSON_CONTROL_NODES,
    }
}

// ---------------------------------------------------------------------------
// Workdir single instance.
// ---------------------------------------------------------------------------

/// One host per workspace state dir: `host.lock` is created exclusively and
/// removed on drop. A lock whose recorded pid is provably dead is stale and
/// is taken over; a live one refuses the second host.
pub struct SingleInstance {
    path: PathBuf,
}

impl SingleInstance {
    pub fn acquire(state_dir: &std::path::Path) -> anyhow::Result<Self> {
        let path = state_dir.join("host.lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                writeln!(file, "{}", std::process::id())?;
                Ok(Self { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let recorded = std::fs::read_to_string(&path).unwrap_or_default();
                let pid: Option<u32> = recorded.trim().parse().ok();
                let alive = pid
                    .filter(|pid| *pid != std::process::id())
                    .map(agent_process::process_is_running)
                    .unwrap_or(false);
                if alive {
                    anyhow::bail!(
                        "another host (pid {}) already serves this workspace ({} exists)",
                        pid.unwrap_or(0),
                        path.display()
                    );
                }
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&path)
                    .with_context(|| format!("taking over stale host lock {}", path.display()))?;
                writeln!(file, "{}", std::process::id())?;
                Ok(Self { path })
            }
            Err(error) => Err(error).with_context(|| format!("creating {}", path.display())),
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Endpoint + serve loop.
// ---------------------------------------------------------------------------

/// The local endpoint the host serves. OS backends stay isolated behind this
/// enum; framing, decoding, routing and session discipline are shared.
#[derive(Debug, Clone)]
pub enum LocalEndpoint {
    /// Linux UDS path (peer verified via SO_PEERCRED).
    UnixSocket(PathBuf),
    /// Windows named pipe (pipe name without the `\\.\pipe\` prefix;
    /// current-user DACL + client-token verification).
    NamedPipe(String),
}

/// Everything a connection's work-control router needs. Shared, read-only.
pub struct HostPlane {
    pub profile: NegotiatedContractProfile,
    pub handle: RuntimeHandle,
    pub broker: Arc<ApprovalBroker>,
    pub gate: Arc<InteractiveApprovalGate>,
    pub registry: Arc<WorkControlSessionRegistry>,
}

pub struct HostServer {
    pub endpoint: LocalEndpoint,
    /// Serve `read_only` grants (snapshot/subscribe only) instead of operator.
    pub read_only: bool,
}

impl HostServer {
    /// Blocks serving until the accept loop fails. Runs on the caller's
    /// thread; per-request work blocks on `runtime` — the tokio runtime
    /// that owns the actor handle, passed explicitly so the accept loop can
    /// live on any thread.
    pub fn serve(
        self,
        plane: HostPlane,
        runtime: tokio::runtime::Handle,
    ) -> anyhow::Result<()> {
        let read_only = self.read_only;
        match self.endpoint {
            LocalEndpoint::UnixSocket(path) => serve_unix(path, read_only, plane, runtime),
            LocalEndpoint::NamedPipe(name) => winpipe::serve(name, read_only, plane, runtime),
        }
    }
}

#[cfg(unix)]
fn serve_unix(
    path: PathBuf,
    read_only: bool,
    plane: HostPlane,
    runtime: tokio::runtime::Handle,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("host: serving UDS {}", path.display());
    let live = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("host: accept failed: {error}");
                continue;
            }
        };
        if !verify_unix_peer(&stream) {
            // Fail closed: an unverifiable peer is dropped unread.
            eprintln!("host: rejected unverifiable UDS peer");
            continue;
        }
        if live.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            eprintln!("host: connection cap reached; refusing");
            continue;
        }
        live.fetch_add(1, Ordering::SeqCst);
        let _ = stream.set_read_timeout(Some(READ_DEADLINE));
        let _ = stream.set_write_timeout(Some(READ_DEADLINE));
        let plane = build_connection_plane(&plane, read_only);
        let runtime = runtime.clone();
        let live = Arc::clone(&live);
        std::thread::spawn(move || {
            let _ = process_connection(&mut stream, &plane, &runtime);
            drop(plane);
            live.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn serve_unix(
    _path: PathBuf,
    _read_only: bool,
    _plane: HostPlane,
    _runtime: tokio::runtime::Handle,
) -> anyhow::Result<()> {
    anyhow::bail!("UDS transport requires a Unix host")
}

#[cfg(target_os = "linux")]
fn verify_unix_peer(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast(),
            std::ptr::from_mut(&mut length),
        )
    };
    if result != 0 {
        return false;
    }
    cred.pid != 0 && cred.uid == unsafe { libc::geteuid() }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn verify_unix_peer(_stream: &std::os::unix::net::UnixStream) -> bool {
    // Fail closed: without a peer-credential answer the host does not serve.
    false
}

/// Install this connection's session grant server-side and build its
/// bound router. The client never sees the session id.
fn build_connection_plane(base: &HostPlane, read_only: bool) -> WorkControlRouter {
    let grant = if read_only {
        WorkControlGrant::read_only()
    } else {
        WorkControlGrant::operator()
    };
    let session_id = base
        .registry
        .install(grant)
        .expect("session registry install cannot fail below its cap");
    let authorizer = base
        .registry
        .bind(&session_id)
        .expect("just-installed session must bind");
    WorkControlRouter::new(
        base.profile.clone(),
        base.handle.clone(),
        Arc::clone(&base.broker),
        Arc::clone(&base.gate),
        Arc::new(authorizer),
    )
    .expect("fixed host profile validates")
}

// ---------------------------------------------------------------------------
// Per-connection request loop.
// ---------------------------------------------------------------------------

fn process_connection<S: Read + Write>(
    stream: &mut S,
    router: &WorkControlRouter,
    runtime: &tokio::runtime::Handle,
) -> anyhow::Result<()> {
    loop {
        let Some(frame) = read_frame(stream)? else {
            return Ok(());
        };
        let request = match agent_platform_protocol::from_slice_bounded::<
            PlatformEnvelope<serde_json::Value>,
        >(&frame, &decode_budget())
        {
            Ok(request) => request,
            Err(_) => anyhow::bail!("undecodable frame; closing connection"),
        };
        let response = dispatch(request, router, runtime);
        write_frame(stream, &serde_json::to_vec(&response)?)?;
    }
}

fn dispatch(
    request: PlatformEnvelope<serde_json::Value>,
    router: &WorkControlRouter,
    runtime: &tokio::runtime::Handle,
) -> serde_json::Value {
    let route = request.route.clone();
    macro_rules! run_route {
        ($payload:ty, $method:ident) => {{
            match retyped::<$payload>(&request) {
                Ok(typed) => {
                    // The router validates the request envelope and payload
                    // and pairs every response it builds; a contract error
                    // here is a structured protocol rejection.
                    match runtime.block_on(router.$method(typed)) {
                        Ok(response) => serde_json::to_value(&response).unwrap_or_else(|_| {
                            serde_json::json!({"status": "error"})
                        }),
                        Err(error) => protocol_error_response(&request, &error.to_string()),
                    }
                }
                Err(_) => unsupported_route_response(&request),
            }
        }};
    }

    match (route.namespace.as_str(), route.operation.as_str()) {
        ("work", "submit") => {
            run_route!(WorkSubmitRequest, submit)
        }
        ("work", "continue") => {
            run_route!(WorkContinueRequest, continue_work)
        }
        ("work", "cancel") => {
            run_route!(WorkCancelRequest, cancel)
        }
        ("work", "snapshot") => {
            run_route!(WorkSnapshotRequest, snapshot)
        }
        ("work", "subscribe") => {
            // The subscribe handshake returns the response; the event
            // receiver is intentionally dropped: the durable event wire
            // contract lands with the typed event projection, and until
            // then clients rebuild from snapshots (honest, never lossy).
            match retyped::<WorkSubscribeRequest>(&request) {
                Ok(typed) => match runtime.block_on(router.subscribe(typed)) {
                    Ok((response, _receiver)) => serde_json::to_value(&response)
                        .unwrap_or_else(|_| serde_json::json!({"status": "error"})),
                    Err(error) => protocol_error_response(&request, &error.to_string()),
                },
                Err(_) => unsupported_route_response(&request),
            }
        }
        ("approval", "respond") => {
            run_route!(ApprovalRespondRequest, respond)
        }
        _ => unsupported_route_response(&request),
    }
}

fn retyped<P: DeserializeOwned>(
    request: &PlatformEnvelope<serde_json::Value>,
) -> Result<PlatformEnvelope<P>, serde_json::Error> {
    Ok(PlatformEnvelope {
        protocol: request.protocol.clone(),
        message_id: request.message_id,
        request_id: request.request_id,
        kind: request.kind,
        route: request.route.clone(),
        work: None,
        causality: request.causality.clone(),
        payload: serde_json::from_value(request.payload.clone())?,
    })
}

fn unsupported_route_response<P>(request: &PlatformEnvelope<P>) -> serde_json::Value {
    let response: PlatformEnvelope<PlatformResponse<serde_json::Value>> = PlatformEnvelope {
        protocol: request.protocol.clone(),
        message_id: MessageId::new(),
        request_id: request.request_id,
        kind: EnvelopeKind::Response,
        route: request.route.clone(),
        work: None,
        causality: Causality::caused_by(request.causality.correlation_id, request.message_id),
        payload: PlatformResponse::Error {
            error: PlatformError {
                class: PlatformErrorClass::Protocol,
                code: ERROR_ROUTE_UNSUPPORTED.into(),
                message: "route is outside the negotiated session set".into(),
                retry: RetryDisposition::Never,
                effect_state: EffectStateDisposition::NotApplicable,
                retry_after_ms: None,
                diagnostic_ref: None,
            },
        },
    };
    serde_json::to_value(&response).unwrap_or_else(|_| serde_json::json!({"status": "error"}))
}

fn protocol_error_response<P>(request: &PlatformEnvelope<P>, message: &str) -> serde_json::Value {
    let response: PlatformEnvelope<PlatformResponse<serde_json::Value>> = PlatformEnvelope {
        protocol: request.protocol.clone(),
        message_id: MessageId::new(),
        request_id: request.request_id,
        kind: EnvelopeKind::Response,
        route: request.route.clone(),
        work: None,
        causality: Causality::caused_by(request.causality.correlation_id, request.message_id),
        payload: PlatformResponse::Error {
            error: PlatformError {
                class: PlatformErrorClass::Protocol,
                code: "protocol.request_invalid".into(),
                message: message.chars().take(400).collect(),
                retry: RetryDisposition::Never,
                effect_state: EffectStateDisposition::NotApplicable,
                retry_after_ms: None,
                diagnostic_ref: None,
            },
        },
    };
    serde_json::to_value(&response).unwrap_or_else(|_| serde_json::json!({"status": "error"}))
}

#[cfg(windows)]
mod winpipe;
