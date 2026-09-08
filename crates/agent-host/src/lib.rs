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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_core::{ApprovalBroker, InteractiveApprovalGate};
use agent_platform_protocol::{
    ApprovalRespondRequest, Causality, EffectStateDisposition, EnvelopeKind, JsonDecodeBudget,
    MAX_JSON_CONTROL_ARRAY_LEN, MAX_JSON_CONTROL_DEPTH, MAX_JSON_CONTROL_NODES,
    MAX_JSON_CONTROL_OBJECT_KEYS, MAX_JSON_CONTROL_STRING_BYTES,
    MAX_JSON_CONTROL_TOTAL_STRING_BYTES, MessageId, NegotiatedContractProfile, PlatformEnvelope,
    PlatformError, PlatformErrorClass, PlatformResponse, ProtocolIdentity, RetryDisposition, Route,
    SchemaDigest, WorkCancelRequest, WorkContinueRequest, WorkSnapshotRequest, WorkSubmitRequest,
    WorkSubscribeRequest,
};
use agent_runtime::{
    RuntimeHandle, WorkControlGrant, WorkControlRouter, WorkControlSessionRegistry,
};

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
/// Bounded shutdown wind-down: after the stop flag is observed, the
/// connections accepted so far get this long to settle their in-flight
/// frames; anything still parked in a blocking read/write then has its
/// pending I/O cancelled (backend-supplied hook), so a silent client can
/// never hold the shutdown past a hard bound.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
// Live connection table: the cap and the bounded-shutdown cancel target.
// ---------------------------------------------------------------------------

/// Registry of the streams of the connections currently being served. It is
/// the accept loop's concurrency cap and the shutdown wind-down's cancel
/// target: after accept stops, [`LiveStreams::drain`] lets in-flight frames
/// settle within [`SHUTDOWN_GRACE`], then interrupts the still-blocked
/// reads/writes through a backend-supplied cancel hook (`CancelIoEx` on the
/// Windows named pipe, `shutdown(2)` on the UDS), so shutdown cannot wait
/// on a silent or half-frame client past a bounded deadline.
struct LiveStreams<S> {
    next_id: AtomicU64,
    streams: Mutex<HashMap<u64, Arc<S>>>,
}

impl<S> LiveStreams<S> {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            streams: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, stream: Arc<S>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.streams
            .lock()
            .expect("live connection table poisoned")
            .insert(id, stream);
        id
    }

    fn remove(&self, id: u64) {
        self.streams
            .lock()
            .expect("live connection table poisoned")
            .remove(&id);
    }

    fn len(&self) -> usize {
        self.streams
            .lock()
            .expect("live connection table poisoned")
            .len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Interrupts every still-registered connection's pending I/O and
    /// empties the table. Called while every stream is provably alive: a
    /// worker removes its entry only after its stream work is done.
    fn cancel_all(&self, cancel: impl Fn(&S)) {
        let mut streams = self.streams.lock().expect("live connection table poisoned");
        for (_, stream) in streams.drain() {
            cancel(&stream);
        }
    }

    fn wait_empty(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if self.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The bounded wind-down: settle what is in flight within `grace`,
    /// cancel what is still parked, then wait one further bound for the
    /// workers to unwind. In-request actor work is separately bounded by
    /// the router's per-request deadline, so the whole stop path has a hard
    /// upper bound.
    fn drain(&self, grace: Duration, cancel: impl Fn(&S)) {
        if self.wait_empty(grace) {
            return;
        }
        self.cancel_all(cancel);
        if !self.wait_empty(grace) {
            // Workers must unwind once their I/O is cancelled; if one is
            // still wedged past this second bound, shutdown stays bounded
            // and the residue is reported, never hidden.
            eprintln!("host: some connections did not wind down within the shutdown grace");
        }
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
    /// Cooperative stop switch. The owner sets it, then opens one throwaway
    /// local connection to wake the parked accept loop; the loop then exits
    /// `Ok` instead of serving further connections. After accept stops, the
    /// connections accepted so far get a bounded wind-down: they may settle
    /// in-flight frames within [`SHUTDOWN_GRACE`], then any still-blocked
    /// read/write is interrupted (transport-supplied cancel), so the stop
    /// path always terminates within a hard bound.
    pub stop: Arc<AtomicBool>,
}

impl HostServer {
    /// Blocks serving until the accept loop fails or [`HostServer::stop`] is
    /// set and the loop is poked. Runs on the caller's thread; per-request
    /// work blocks on `runtime` — the tokio runtime that owns the actor
    /// handle, passed explicitly so the accept loop can live on any thread.
    pub fn serve(self, plane: HostPlane, runtime: tokio::runtime::Handle) -> anyhow::Result<()> {
        let read_only = self.read_only;
        let stop = Arc::clone(&self.stop);
        match self.endpoint {
            LocalEndpoint::UnixSocket(path) => serve_unix(path, read_only, plane, runtime, &stop),
            LocalEndpoint::NamedPipe(name) => {
                winpipe::serve(name, read_only, plane, runtime, &stop)
            }
        }
    }
}

#[cfg(unix)]
fn serve_unix(
    path: PathBuf,
    read_only: bool,
    plane: HostPlane,
    runtime: tokio::runtime::Handle,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    reconcile_socket_path(&path)?;
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    // From here the endpoint file is provably the one this process bound:
    // the exit path removes exactly that file, nothing else.
    let _own_endpoint = SocketEndpointGuard { path: path.clone() };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("host: serving UDS {}", path.display());
    let live: Arc<LiveStreams<UnixStream>> = Arc::new(LiveStreams::new());
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            // Woken by the stop poke; the waker connection is unserved.
            break;
        }
        let stream = match stream {
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
        if live.len() >= MAX_CONNECTIONS {
            eprintln!("host: connection cap reached; refusing");
            continue;
        }
        let _ = stream.set_read_timeout(Some(READ_DEADLINE));
        let _ = stream.set_write_timeout(Some(READ_DEADLINE));
        let (router, guard) = match open_connection_plane(&plane, read_only) {
            Ok(connection) => connection,
            Err(error) => {
                // Controlled refusal at the session cap: one structured
                // error frame, then close. The accept loop lives on.
                eprintln!("host: refusing connection; session grant unavailable: {error}");
                let mut writer = &stream;
                let _ = write_frame(&mut writer, &session_unavailable_frame(&plane.profile));
                continue;
            }
        };
        let shared = Arc::new(stream);
        let id = live.insert(Arc::clone(&shared));
        let live_in_worker = Arc::clone(&live);
        let runtime = runtime.clone();
        std::thread::spawn(move || {
            let mut stream: &UnixStream = shared.as_ref();
            let _ = process_connection(&mut stream, &router, &runtime);
            drop(guard);
            live_in_worker.remove(id);
        });
    }
    // Bounded wind-down for the connections accepted before the stop:
    // settle in flight, then interrupt still-blocked reads/writes.
    live.drain(SHUTDOWN_GRACE, |stream| {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    Ok(())
}

/// Fail-closed endpoint takeover check: inspect whatever sits at `path`
/// before touching it. A regular file (or anything that is not a socket)
/// is somebody's data — refuse startup and leave it untouched. A socket
/// owned by another local user is refused as well. A socket we own is
/// probed: a live listener means another host already serves this endpoint
/// (refuse), while a dead socket is ours to remove so this host can bind.
#[cfg(unix)]
fn reconcile_socket_path(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixStream;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() {
        anyhow::bail!(
            "refusing to start: {} exists and is not a socket; it was left untouched",
            path.display()
        );
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!(
            "refusing to start: existing socket {} belongs to another local user",
            path.display()
        );
    }
    if UnixStream::connect(path).is_ok() {
        anyhow::bail!(
            "refusing to start: {} is already served by a live listener",
            path.display()
        );
    }
    std::fs::remove_file(path)
        .with_context(|| format!("removing stale socket {}", path.display()))?;
    Ok(())
}

/// Owns the socket file this host bound: dropping it removes exactly that
/// endpoint, so the host cleans up only what it provably created and
/// never an endpoint it merely found on disk.
#[cfg(unix)]
struct SocketEndpointGuard {
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for SocketEndpointGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A stable per-workspace endpoint discriminator: 16 hex chars of the
/// SHA-256 over the canonical workspace path. One workspace always
/// resolves to the same suffix, two workspaces never share a default
/// endpoint, and no default is a fixed global name.
pub fn workspace_endpoint_suffix(workspace_root: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let mut suffix = String::with_capacity(16);
    for byte in &digest[..8] {
        suffix.push_str(&format!("{byte:02x}"));
    }
    suffix
}

/// The default UDS endpoint for one workspace: the user's runtime directory
/// when the platform provides one, else the temp dir — plus the workspace
/// discriminator, so the path is user-private, workspace-scoped, and never
/// a fixed global name in `/tmp`.
#[cfg(unix)]
pub fn default_socket_path_for(workspace_root: &std::path::Path) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!(
        "focus-agent-platform-{}.sock",
        workspace_endpoint_suffix(workspace_root)
    ))
}

#[cfg(not(unix))]
fn serve_unix(
    _path: PathBuf,
    _read_only: bool,
    _plane: HostPlane,
    _runtime: tokio::runtime::Handle,
    _stop: &AtomicBool,
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

/// Owns one connection's installed session grant. Dropping it revokes the
/// grant, so every connection exit path — clean disconnect, bad frame,
/// read deadline, shutdown-driven I/O cancel — releases its registry slot
/// and the slot count returns to its baseline. The connection worker drops
/// it after the router is done; a refused/never-served connection never
/// gets one.
struct SessionGrantGuard {
    registry: Arc<WorkControlSessionRegistry>,
    session_id: String,
}

impl Drop for SessionGrantGuard {
    fn drop(&mut self) {
        // Best effort: a failed revoke would leak one slot until the host
        // exits. It can never widen authority — revoke only.
        if let Err(error) = self.registry.revoke(&self.session_id) {
            eprintln!("host: revoking session grant failed: {error}");
        }
    }
}

/// Install this connection's session grant server-side and build its bound
/// router plus the grant's ownership guard. The client never sees the
/// session id. An exhausted registry is a controlled refusal, not a panic:
/// the caller answers the structured error frame, closes the connection,
/// and the accept loop keeps serving.
fn open_connection_plane(
    base: &HostPlane,
    read_only: bool,
) -> anyhow::Result<(WorkControlRouter, SessionGrantGuard)> {
    let grant = if read_only {
        WorkControlGrant::read_only()
    } else {
        WorkControlGrant::operator()
    };
    let session_id = base
        .registry
        .install(grant)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    // The guard exists before bind, so a bind or router failure below still
    // releases the just-installed slot.
    let guard = SessionGrantGuard {
        registry: Arc::clone(&base.registry),
        session_id: session_id.clone(),
    };
    let authorizer = base
        .registry
        .bind(&session_id)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let router = WorkControlRouter::new(
        base.profile.clone(),
        base.handle.clone(),
        Arc::clone(&base.broker),
        Arc::clone(&base.gate),
        Arc::new(authorizer),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok((router, guard))
}

/// The framed structured refusal for a connection that cannot get its
/// session grant. Sent once, then the connection closes: nothing was
/// executed, nothing is ambiguous, and a blind retry would hit the same
/// full table.
fn session_unavailable_frame(profile: &NegotiatedContractProfile) -> Vec<u8> {
    let message_id = MessageId::new();
    let envelope: PlatformEnvelope<PlatformResponse<serde_json::Value>> = PlatformEnvelope {
        protocol: ProtocolIdentity {
            name: profile.name.clone(),
            version: profile.version,
            active_features: profile.active_features.clone(),
            schema_digest: profile.schema_digest,
        },
        message_id,
        request_id: None,
        kind: EnvelopeKind::Response,
        route: Route {
            namespace: "work".into(),
            operation: "session".into(),
        },
        work: None,
        causality: Causality::root(message_id),
        payload: PlatformResponse::Error {
            error: PlatformError {
                class: PlatformErrorClass::Domain,
                code: "work.control_unavailable".into(),
                message: "the host cannot install a session grant right now; reconnect later"
                    .into(),
                retry: RetryDisposition::Never,
                effect_state: EffectStateDisposition::NotApplicable,
                retry_after_ms: None,
                diagnostic_ref: None,
            },
        },
    };
    serde_json::to_vec(&envelope).unwrap_or_else(|_| b"{\"status\":\"error\"}".to_vec())
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

/// Non-Windows stand-in for the named-pipe backend: the transport only
/// exists on Windows, so a misconfigured [`LocalEndpoint::NamedPipe`] fails
/// closed with a typed error here instead of not compiling at all. The Unix
/// endpoint on this platform is [`LocalEndpoint::UnixSocket`], whose serving
/// path is implemented in [`serve_unix`].
#[cfg(not(windows))]
mod winpipe {
    use std::sync::atomic::AtomicBool;

    use super::HostPlane;

    pub(super) fn serve(
        _name: String,
        _read_only: bool,
        _plane: HostPlane,
        _runtime: tokio::runtime::Handle,
        _stop: &AtomicBool,
    ) -> anyhow::Result<()> {
        anyhow::bail!(
            "named-pipe transport is Windows-only; use the UnixSocket endpoint on Unix hosts"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_endpoint_suffix_is_stable_and_discriminating() {
        let alpha = std::path::Path::new("/workspaces/alpha");
        let beta = std::path::Path::new("/workspaces/beta");
        let suffix = workspace_endpoint_suffix(alpha);
        assert_eq!(
            suffix,
            workspace_endpoint_suffix(alpha),
            "one workspace always resolves to the same endpoint id"
        );
        assert_eq!(suffix.len(), 16);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            suffix,
            workspace_endpoint_suffix(beta),
            "two workspaces must not share a default endpoint"
        );
    }

    #[cfg(unix)]
    #[test]
    fn default_socket_path_is_workspace_scoped_and_private() {
        let alpha = std::path::Path::new("/workspaces/alpha");
        let path = default_socket_path_for(alpha);
        assert!(
            path.to_string_lossy()
                .contains(&workspace_endpoint_suffix(alpha)),
            "the default path must carry the workspace discriminator"
        );
        assert_ne!(
            path,
            default_socket_path_for(std::path::Path::new("/workspaces/beta")),
            "different workspaces must not share a default socket path"
        );
    }
}
