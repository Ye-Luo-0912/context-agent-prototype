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
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_contracts::RuntimeEventEnvelope;
use agent_core::{ApprovalBroker, InteractiveApprovalGate};
use agent_platform_protocol::{
    ApprovalRespondRequest, Causality, EffectStateDisposition, EnvelopeKind, JsonDecodeBudget,
    MAX_JSON_CONTROL_ARRAY_LEN, MAX_JSON_CONTROL_DEPTH, MAX_JSON_CONTROL_NODES,
    MAX_JSON_CONTROL_OBJECT_KEYS, MAX_JSON_CONTROL_STRING_BYTES,
    MAX_JSON_CONTROL_TOTAL_STRING_BYTES, MessageId, NegotiatedContractProfile, PlatformEnvelope,
    PlatformError, PlatformErrorClass, PlatformResponse, ProtocolIdentity, RetryDisposition, Route,
    SchemaDigest, WorkArtifactRequest, WorkCancelRequest, WorkChangesRequest, WorkContextRequest,
    WorkContinueRequest, WorkEventNotification, WorkSnapshotRequest, WorkSubmitRequest,
    WorkSubscribeRequest, WorkTaskDetailRequest,
};
use agent_runtime::{
    RuntimeHandle, WorkControlGrant, WorkControlRouter, WorkControlSessionRegistry,
};
use tokio::sync::broadcast;

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
// Startup restore (`--restore-latest`).
// ---------------------------------------------------------------------------

/// Resolve and decode the newest *verifiable* runtime checkpoint from the
/// workspace checkpoint store, for `--restore-latest` startup.
///
/// Selection policy (external review F10): discovery, ordering and
/// verification all go through the runtime's own [`agent_runtime::CheckpointStore`]
/// and unified decode entry — never a raw directory scan.
///
/// * Candidates are only artifacts the store itself wrote
///   (`checkpoint-*.json` with a well-formed envelope header), ordered
///   newest-first by modification time — not by lexicographic file name.
/// * "Latest" means the newest candidate that *verifies*: the envelope
///   checksum, payload bounds and full checkpoint compatibility (version +
///   structural validation) must all pass before a candidate is eligible.
/// * A corrupt, truncated, oversized or version-incompatible candidate is
///   skipped with a visible note, and startup fails closed when no
///   candidate verifies. Restoring a guessed older state silently would be
///   worse than refusing to start.
///
/// Returns the decoded checkpoint plus the artifact path it came from (for
/// diagnostics); nothing has been started or mutated when this returns.
pub async fn resolve_latest_verified_checkpoint(
    checkpoint_dir: &std::path::Path,
) -> anyhow::Result<(agent_runtime::RuntimeCheckpoint, PathBuf)> {
    let store = agent_runtime::CheckpointStore::new(checkpoint_dir);
    let listed = store
        .list(agent_runtime::checkpoint::MAX_CHECKPOINT_LIST_ROWS)
        .await
        .map_err(|error| {
            anyhow::Error::new(error).context(format!(
                "listing checkpoint store {}",
                checkpoint_dir.display()
            ))
        })?;
    if listed.is_empty() {
        anyhow::bail!(
            "no verifiable checkpoints exist in {}; start without --restore-latest or save one first",
            checkpoint_dir.display()
        );
    }
    for row in &listed {
        let path = checkpoint_dir.join(&row.artifact);
        let decoded = match store.load_verified(&row.artifact).await {
            Ok(payload) => agent_runtime::decode_checkpoint_bytes(&payload),
            Err(error) => Err(error),
        };
        match decoded {
            Ok(checkpoint) => return Ok((checkpoint, path)),
            Err(error) => {
                eprintln!(
                    "host: skipping checkpoint {} (newest-first order): {error}",
                    row.artifact
                );
            }
        }
    }
    anyhow::bail!(
        "all {} checkpoint(s) in {} failed verification; refusing to restore (fail closed)",
        listed.len(),
        checkpoint_dir.display()
    )
}

// ---------------------------------------------------------------------------
// Live connection table: the cap and the bounded-shutdown cancel target.
// ---------------------------------------------------------------------------

/// Registry of the connections currently being served. It is the accept
/// loop's concurrency cap and the shutdown wind-down's cancel target: after
/// accept stops, [`LiveStreams::drain`] lets in-flight frames settle within
/// [`SHUTDOWN_GRACE`], then interrupts the still-blocked reads/writes through
/// each connection's cancel hook (`shutdown(2)` on the UDS halves,
/// `CancelIoEx` on the Windows named-pipe handles), so shutdown cannot wait
/// on a silent or half-frame client past a bounded deadline.
///
/// Each entry is one connection's pending-I/O interrupt, covering both
/// transport halves (the reader the request loop parks on and the writer the
/// response path and the event forwarder share). The same hook also closes
/// the connection when the event forwarder gives up on it, so a lagged
/// subscriber is torn down through exactly the interrupt path shutdown uses.
type CancelHook = Arc<dyn Fn() + Send + Sync + 'static>;

struct LiveStreams {
    next_id: AtomicU64,
    cancels: Mutex<HashMap<u64, CancelHook>>,
}

impl LiveStreams {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            cancels: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, cancel: CancelHook) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.cancels
            .lock()
            .expect("live connection table poisoned")
            .insert(id, cancel);
        id
    }

    fn remove(&self, id: u64) {
        self.cancels
            .lock()
            .expect("live connection table poisoned")
            .remove(&id);
    }

    fn len(&self) -> usize {
        self.cancels
            .lock()
            .expect("live connection table poisoned")
            .len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Interrupts every still-registered connection's pending I/O. The table
    /// is NOT emptied here (B2 CANCEL-ALL): a stop request is not a worker
    /// exit, and an emptied table would make [`LiveStreams::wait_empty`]
    /// report completion the moment the hooks fire — granting released and
    /// connections unwound would have been inferred, never observed. Each
    /// worker removes its own entry after its request loop and event
    /// forwarder are done, so the post-cancel [`LiveStreams::wait_empty`]
    /// observes the real unwinding and the drain timeout leaves an explicit
    /// unconfirmed residue instead of a fake success.
    fn cancel_all(&self) {
        let hooks: Vec<CancelHook> = {
            let cancels = self.cancels.lock().expect("live connection table poisoned");
            cancels.values().cloned().collect()
        };
        // Hooks run outside the table lock: an interrupt path that touches
        // the table must not deadlock against the shutdown drain.
        for cancel in hooks {
            cancel();
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
    fn drain(&self, grace: Duration) {
        if self.wait_empty(grace) {
            return;
        }
        self.cancel_all();
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
    /// The run's workspace: B3 read-only routes (change journal, artifact
    /// bytes) read through it. Shared and read-only at the router boundary.
    pub workspace: Arc<agent_workspace::Workspace>,
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
    let live: Arc<LiveStreams> = Arc::new(LiveStreams::new());
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
        // Split the socket into a reader (owned by the request loop) and a
        // duplicated writer shared by the response path and the event
        // forwarder under one whole-frame lock. The cancel hook covers both
        // halves, so shutdown and the forwarder's close path interrupt every
        // pending I/O this connection can be parked in.
        let write_half = match stream.try_clone() {
            Ok(write_half) => write_half,
            Err(error) => {
                eprintln!("host: dropping connection; write half unavailable: {error}");
                drop(guard);
                continue;
            }
        };
        let cancel_write = match stream.try_clone() {
            Ok(cancel_write) => cancel_write,
            Err(error) => {
                eprintln!("host: dropping connection; cancel half unavailable: {error}");
                drop(guard);
                continue;
            }
        };
        let shared = Arc::new(stream);
        let hang_up: CancelHook = {
            let read = Arc::clone(&shared);
            Arc::new(move || {
                let _ = read.shutdown(std::net::Shutdown::Both);
                let _ = cancel_write.shutdown(std::net::Shutdown::Both);
            })
        };
        let events = ConnectionEvents::new(write_half, plane.profile.clone(), hang_up);
        let id = live.insert(events.cancel_hook());
        let live_in_worker = Arc::clone(&live);
        let runtime = runtime.clone();
        std::thread::spawn(move || {
            let mut events = events;
            let mut reader: &UnixStream = shared.as_ref();
            // Unix socket reads have no cross-dup serialization, so the
            // request loop can simply block on the next frame; the socket's
            // read deadline and the shutdown shutdown(2) bound the wait.
            let read_next_frame = |reader: &mut &UnixStream, _may_block: bool| read_frame(reader);
            let _ = process_connection(
                &mut reader,
                &mut events,
                &router,
                &runtime,
                &read_next_frame,
            );
            // The connection is over: stop the event forwarder, interrupt
            // anything still parked (a forwarder wedged writing to a silent
            // consumer included), then release the grant and the live entry.
            events.shutdown();
            drop(guard);
            live_in_worker.remove(id);
        });
    }
    // Bounded wind-down for the connections accepted before the stop:
    // settle in flight, then interrupt still-blocked reads/writes.
    live.drain(SHUTDOWN_GRACE);
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
///
/// The digest rule itself lives in the platform protocol
/// ([`agent_platform_protocol::workspace_endpoint_suffix`]) so the desktop
/// client derives the same default endpoint from the same bytes (N4); the
/// host-only part is the canonicalization before hashing.
pub fn workspace_endpoint_suffix(workspace_root: &std::path::Path) -> String {
    let canonical = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    agent_platform_protocol::workspace_endpoint_suffix(&canonical)
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
        Arc::clone(&base.workspace),
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
// Per-connection output plane: single writer + event forwarding.
// ---------------------------------------------------------------------------

/// How long a parked event forwarder waits before re-checking its stop
/// flags. This is the worst added latency for a notification on an otherwise
/// quiet stream and the wind-down bound for a forwarder with nothing to do.
const FORWARDER_TICK: Duration = Duration::from_millis(50);

/// Upper bound for reaping a replaced or stopped forwarder. The forwarder
/// re-checks its flags at least once per [`FORWARDER_TICK`], so the normal
/// reap is immediate; a forwarder wedged inside a blocking write is left to
/// the connection's cancel hook (which the worker invokes before waiting)
/// and must never stall the subscribe handshake or the worker unwind past
/// this bound.
const FORWARDER_REAP: Duration = Duration::from_millis(200);

/// The connection's single serialized writer: the duplicated write half
/// every outbound frame goes through. Holding the lock across one whole
/// [`write_frame`] call (header + payload + flush) is what keeps response
/// frames and event-notification frames from interleaving bytes.
struct SharedWriter<W> {
    stream: Mutex<W>,
}

impl<W: Write> SharedWriter<W> {
    fn new(stream: W) -> Self {
        Self {
            stream: Mutex::new(stream),
        }
    }

    /// Writes one whole frame under the lock: responses and notifications
    /// share this writer, and a frame is never split by a concurrent one.
    fn write_frame(&self, payload: &[u8]) -> std::io::Result<()> {
        let mut stream = self.stream.lock().expect("connection writer poisoned");
        write_frame(&mut *stream, payload)
    }
}

/// Handle for a running event forwarder, kept so a re-subscribe or a
/// connection teardown can stop and reap it in bounded time.
struct ForwarderHandle {
    stop: Arc<AtomicBool>,
    done: mpsc::Receiver<()>,
}

/// One connection's event plane: everything the subscribe route needs to
/// turn the runtime broadcast receiver it is handed into typed
/// [`WorkEventNotification`] frames on this connection, plus the teardown
/// path shared with the transport.
struct ConnectionEvents<W> {
    writer: Arc<SharedWriter<W>>,
    profile: NegotiatedContractProfile,
    identity: ProtocolIdentity,
    /// Set when the connection must stop serving and forwarding: by the
    /// worker at teardown, or by the forwarder when it closes the
    /// connection (stream dead or subscriber lag overflow).
    closed: Arc<AtomicBool>,
    /// Interrupts every pending I/O on both transport halves. Shared with
    /// the live-connection table so shutdown uses the identical close path.
    hang_up: CancelHook,
    forwarder: Option<ForwarderHandle>,
}

impl<W: Write + Send + 'static> ConnectionEvents<W> {
    fn new(stream: W, profile: NegotiatedContractProfile, hang_up: CancelHook) -> Self {
        let identity = ProtocolIdentity {
            name: profile.name.clone(),
            version: profile.version,
            active_features: profile.active_features.clone(),
            schema_digest: profile.schema_digest,
        };
        Self {
            writer: Arc::new(SharedWriter::new(stream)),
            profile,
            identity,
            closed: Arc::new(AtomicBool::new(false)),
            hang_up,
            forwarder: None,
        }
    }

    /// The registration form of this connection's cancel hook for the live
    /// connection table.
    fn cancel_hook(&self) -> CancelHook {
        Arc::clone(&self.hang_up)
    }

    /// Whether a subscribed event stream is currently installed on this
    /// connection. The request loop uses this to pick a wait that cannot
    /// starve the forwarder (see the named-pipe backend).
    fn is_forwarding(&self) -> bool {
        self.forwarder.is_some()
    }

    /// Installs the subscribed event stream on this connection.
    ///
    /// Repeated subscribe on the same connection REPLACES the previous
    /// stream rather than refusing. Rationale: a re-subscribe is a client
    /// recovering its stream — after it observed a gap, a lag-driven close
    /// hint on the wire, or its own UI restart — and the fresh handshake
    /// watermark makes the new stream authoritative, so refusing would only
    /// force a full reconnect without adding safety. The previous forwarder
    /// is stopped and reaped (bounded) before the new one starts, so the
    /// connection never carries two live streams; a forwarder wedged in a
    /// blocking write is left to the cancel hook and may emit at most the
    /// frames already in flight before it observes the stop flag.
    fn register_event_stream(
        &mut self,
        receiver: broadcast::Receiver<RuntimeEventEnvelope>,
        watermark: u64,
        runtime: &tokio::runtime::Handle,
    ) {
        if let Some(previous) = self.forwarder.take() {
            previous.stop.store(true, Ordering::Relaxed);
            let _ = previous.done.recv_timeout(FORWARDER_REAP);
        }
        let (stop, done) = spawn_event_forwarder(receiver, watermark, self, runtime);
        self.forwarder = Some(ForwarderHandle { stop, done });
    }

    /// Stops forwarding and tears the connection down. Called by the worker
    /// once its request loop has ended.
    fn shutdown(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        (self.hang_up)();
        if let Some(forwarder) = self.forwarder.take() {
            forwarder.stop.store(true, Ordering::Relaxed);
            // Bounded reap; the cancel above unblocks a forwarder parked in
            // a write, and a quiet forwarder exits at its next tick.
            let _ = forwarder.done.recv_timeout(FORWARDER_REAP);
        }
    }
}

/// Builds the typed notification envelope for one runtime event: the
/// kernel's own envelope forwarded verbatim on the `work/event` route,
/// server-initiated (no request correlation, no work identity).
fn event_notification_envelope(
    identity: &ProtocolIdentity,
    envelope: RuntimeEventEnvelope,
) -> PlatformEnvelope<WorkEventNotification> {
    let message_id = MessageId::new();
    PlatformEnvelope {
        protocol: identity.clone(),
        message_id,
        request_id: None,
        kind: EnvelopeKind::Notification,
        route: Route::work_event(),
        work: None,
        causality: Causality::root(message_id),
        payload: WorkEventNotification { envelope },
    }
}

/// Forwards one subscribed runtime event stream to one connection.
///
/// The bounded queue is the runtime's own broadcast channel (per-receiver,
/// capacity fixed by the kernel): the forwarder buffers nothing beyond the
/// frame it is currently writing, so a slow consumer first backs up into
/// the kernel socket buffer and then into that channel. Overflow is
/// therefore always explicit, never silent:
///
/// * `Lagged(skipped)` — this subscriber provably lost events and the host
///   has no replay. Writing a resync frame to the very consumer that caused
///   the lag would block on the same congested pipe, so the explicit resync
///   is the connection close itself: the client reconnects and rebuilds
///   from a fresh snapshot. This is the same recovery the subscribe
///   handshake's `resync_required` points at.
/// * a failed write — the consumer is gone or wedged past the shutdown
///   cancel; the connection is torn down through the shared cancel hook.
///
/// Events are filtered against the handshake watermark by kind (B1
/// LIVE-DELTA):
///
/// * a **durable** event at or below the watermark is dropped on purpose:
///   `WorkControlRouter::subscribe` registers the receiver *before* its
///   snapshot barrier, so every durable event at or below that watermark is
///   already reflected in the snapshot the client holds; forwarding it would
///   double-count it.
/// * a **live-only** event (`ModelDelta`/`ModelRetrying`) is always
///   forwarded: it repeats the preceding durable cursor instead of
///   consuming one, and its content never enters any snapshot, so a raw
///   cursor comparison would erase the whole streaming segment that follows
///   the cursor event. Supersession is the consumer's job, by the
///   turn/operation/generation identity the contract names as the fence —
///   the durable truth still arrives later in the journal.
fn forward_against_watermark(envelope: &RuntimeEventEnvelope, watermark: u64) -> bool {
    envelope.seq > watermark || envelope.event.is_live_only()
}
fn spawn_event_forwarder<W: Write + Send + 'static>(
    mut receiver: broadcast::Receiver<RuntimeEventEnvelope>,
    watermark: u64,
    events: &ConnectionEvents<W>,
    runtime: &tokio::runtime::Handle,
) -> (Arc<AtomicBool>, mpsc::Receiver<()>) {
    let writer = Arc::clone(&events.writer);
    let identity = events.identity.clone();
    let profile = events.profile.clone();
    let closed = Arc::clone(&events.closed);
    let hang_up = Arc::clone(&events.hang_up);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let (done_tx, done_rx) = mpsc::channel();
    let runtime = runtime.clone();
    std::thread::spawn(move || {
        loop {
            if stop_flag.load(Ordering::Relaxed) || closed.load(Ordering::Relaxed) {
                break;
            }
            // Bounded wait: a quiet stream still observes the stop flags,
            // so teardown never waits on an event that never comes.
            let received = runtime
                .block_on(async { tokio::time::timeout(FORWARDER_TICK, receiver.recv()).await });
            match received {
                Ok(Ok(envelope)) => {
                    if !forward_against_watermark(&envelope, watermark) {
                        continue;
                    }
                    let notification = event_notification_envelope(&identity, envelope);
                    if notification.validate(&profile).is_err() {
                        // Impossible by construction (the shape is fixed at
                        // compile time), but a contract violation never gets
                        // written to the wire: close instead.
                        eprintln!("host: event notification failed validation; closing");
                        closed.store(true, Ordering::Relaxed);
                        (hang_up)();
                        break;
                    }
                    let payload = match serde_json::to_vec(&notification) {
                        Ok(payload) => payload,
                        Err(error) => {
                            eprintln!("host: serializing event notification failed: {error}");
                            closed.store(true, Ordering::Relaxed);
                            (hang_up)();
                            break;
                        }
                    };
                    if let Err(error) = writer.write_frame(&payload) {
                        eprintln!("host: event forwarder write failed: {error}");
                        closed.store(true, Ordering::Relaxed);
                        (hang_up)();
                        break;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    eprintln!(
                        "host: subscriber lagged {skipped} events; closing connection for resync"
                    );
                    closed.store(true, Ordering::Relaxed);
                    (hang_up)();
                    break;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_tick) => continue,
            }
        }
        let _ = done_tx.send(());
    });
    (stop, done_rx)
}

// ---------------------------------------------------------------------------
// Per-connection request loop.
// ---------------------------------------------------------------------------

/// The transport-supplied frame reader: fetches the next whole request
/// frame, or `Ok(None)` on a clean disconnect. `may_block` is false while a
/// subscription is live — the named-pipe backend explains why a pending
/// read would starve the forwarder there.
type NextFrameReader<S> = dyn Fn(&mut S, bool) -> std::io::Result<Option<Vec<u8>>>;

fn process_connection<S: Read, W: Write + Send + 'static>(
    reader: &mut S,
    events: &mut ConnectionEvents<W>,
    router: &WorkControlRouter,
    runtime: &tokio::runtime::Handle,
    read_next_frame: &NextFrameReader<S>,
) -> anyhow::Result<()> {
    loop {
        if events.closed.load(Ordering::Relaxed) {
            // The event forwarder closed the connection (dead stream or
            // lag overflow): the session ends here and the client
            // reconnects, rebuilding from a fresh snapshot.
            return Ok(());
        }
        // The transport-supplied reader waits for the next frame. While a
        // subscription is live it must not hold a pending I/O that would
        // starve the forwarder (`may_block` is false); with nothing to
        // starve, blocking reads are preferred for their prompt disconnect
        // detection (the named-pipe backend explains both sides).
        let Some(frame) = read_next_frame(reader, !events.is_forwarding())? else {
            return Ok(());
        };
        let request = match agent_platform_protocol::from_slice_bounded::<
            PlatformEnvelope<serde_json::Value>,
        >(&frame, &decode_budget())
        {
            Ok(request) => request,
            Err(_) => anyhow::bail!("undecodable frame; closing connection"),
        };
        let response = dispatch(request, router, runtime, events);
        events.writer.write_frame(&serde_json::to_vec(&response)?)?;
    }
}

fn dispatch<W: Write + Send + 'static>(
    request: PlatformEnvelope<serde_json::Value>,
    router: &WorkControlRouter,
    runtime: &tokio::runtime::Handle,
    events: &mut ConnectionEvents<W>,
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
        ("work", "task_detail") => {
            run_route!(WorkTaskDetailRequest, task_detail)
        }
        ("work", "changes") => {
            run_route!(WorkChangesRequest, changes)
        }
        ("work", "artifact") => {
            run_route!(WorkArtifactRequest, artifact)
        }
        ("work", "context") => {
            run_route!(WorkContextRequest, context)
        }
        ("work", "subscribe") => {
            // The subscribe handshake returns its receipt and, on success,
            // the live event stream: the receiver is registered on this
            // connection's output plane and every subsequent runtime event
            // above the receipt's watermark is forwarded as a typed
            // WorkEventNotification frame. A failed handshake carries a
            // closed placeholder receiver, which is dropped here — nothing
            // is subscribed on an error receipt.
            match retyped::<WorkSubscribeRequest>(&request) {
                Ok(typed) => match runtime.block_on(router.subscribe(typed)) {
                    Ok((response, receiver)) => {
                        if let PlatformResponse::Success { value } = &response.payload {
                            events.register_event_stream(receiver, value.watermark, runtime);
                        }
                        serde_json::to_value(&response)
                            .unwrap_or_else(|_| serde_json::json!({"status": "error"}))
                    }
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

/// Re-decodes the payload half of an already-parsed envelope. The `work`
/// identity of the ORIGINAL envelope is carried over untouched (B2 RETYPED):
/// the router's validators decide on the request the client actually sent —
/// a run-scoped envelope that arrived carrying a tool-operation work
/// identity must be rejected by them, not silently cleaned into a legal
/// request by the retype step.
fn retyped<P: DeserializeOwned>(
    request: &PlatformEnvelope<serde_json::Value>,
) -> Result<PlatformEnvelope<P>, serde_json::Error> {
    Ok(PlatformEnvelope {
        protocol: request.protocol.clone(),
        message_id: request.message_id,
        request_id: request.request_id,
        kind: request.kind,
        route: request.route.clone(),
        work: request.work.clone(),
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

    /// B1 LIVE-DELTA: the watermark fence applies to durable facts only.
    /// Live-only progress repeats the preceding durable cursor and must pass
    /// regardless of it — a raw comparison erases the streaming segment.
    #[test]
    fn watermark_fence_splits_durable_from_live_only() {
        let delta = |seq| RuntimeEventEnvelope {
            run_id: agent_contracts::RunId::new(),
            seq,
            timestamp_ms: 0,
            event: agent_contracts::RuntimeEvent::ModelDelta {
                turn_id: agent_contracts::TurnId::new(),
                operation_id: agent_contracts::OperationId::new(),
                generation: 0,
                delta: "chunk".into(),
            },
        };
        let durable = |seq| RuntimeEventEnvelope {
            run_id: agent_contracts::RunId::new(),
            seq,
            timestamp_ms: 0,
            event: agent_contracts::RuntimeEvent::RunCompleted,
        };

        let watermark = 5;
        // Durable at/below the watermark is already in the snapshot: dropped.
        assert!(!forward_against_watermark(&durable(5), watermark));
        assert!(!forward_against_watermark(&durable(1), watermark));
        // Durable above it is stream-only: forwarded.
        assert!(forward_against_watermark(&durable(6), watermark));
        // Live-only progress repeats any cursor — including exactly the
        // watermark's — and is always forwarded: no snapshot ever carried it.
        assert!(forward_against_watermark(&delta(5), watermark));
        assert!(forward_against_watermark(&delta(1), watermark));
        // A fresh subscribe (watermark 0) forwards every durable event too.
        assert!(forward_against_watermark(&durable(1), 0));
        assert!(forward_against_watermark(&delta(0), 0));
    }

    /// B2 RETYPED: the retype step must carry the ORIGINAL envelope's work
    /// identity through to the router's validators — a run-scoped request
    /// that arrived carrying a tool-operation work identity is rejected by
    /// them (the validator's own contract), not silently cleaned into a
    /// legal request by the retype.
    #[test]
    fn retyped_preserves_the_original_work_identity_for_validation() {
        let message_id = MessageId::new();
        let work = agent_platform_protocol::WorkIdentity {
            run_id: agent_contracts::RunId::new(),
            task_id: None,
            turn_id: None,
            scope_id: None,
            operation_id: agent_contracts::OperationId::new(),
            generation: 0,
            attempt: agent_platform_protocol::Attempt::new(1).unwrap(),
            call_id: None,
            effect_id: None,
            argument_digest: agent_platform_protocol::ArgumentDigest::from_bytes([0x22; 32]),
            deadline_remaining_ms: agent_platform_protocol::DeadlineRemainingMs::new(30_000)
                .unwrap(),
            authority_ref: None,
        };
        let request = PlatformEnvelope {
            protocol: ProtocolIdentity {
                name: "focus-agent.platform".into(),
                version: agent_platform_protocol::ProtocolVersion { major: 1, minor: 0 },
                active_features: agent_platform_protocol::ActiveFeatures::default(),
                schema_digest: session_schema_digest(),
            },
            message_id,
            request_id: Some(agent_platform_protocol::RequestId::new()),
            kind: EnvelopeKind::Request,
            route: Route::work_submit(),
            work: Some(work),
            causality: Causality::root(message_id),
            payload: serde_json::json!({
                "goal": "retyped drill",
                "client_request_id": "retyped-1",
            }),
        };

        let retyped: PlatformEnvelope<WorkSubmitRequest> = retyped(&request).unwrap();
        // The identity the client actually sent is what the validator sees.
        assert_eq!(retyped.work, request.work);
        let profile = negotiated_profile().unwrap();
        assert!(
            agent_platform_protocol::validate_work_submit_request(&profile, &retyped).is_err(),
            "a run-scoped request carrying a work identity must be rejected by the validator"
        );
    }

    /// B2 CANCEL-ALL: interrupting the connections must not empty the live
    /// table — a stop request is not a worker exit. The hook fires, the
    /// entry stays, and only the worker's own removal makes `wait_empty`
    /// report completion; interrupting alone never does.
    #[test]
    fn cancel_all_keeps_the_registry_until_workers_remove_their_own_entries() {
        let live = LiveStreams::new();
        let interrupted = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&interrupted);
        let id = live.insert(Arc::new(move || {
            seen.store(true, Ordering::SeqCst);
        }));
        assert_eq!(live.len(), 1);

        live.cancel_all();
        assert!(
            interrupted.load(Ordering::SeqCst),
            "the connection's pending I/O was interrupted"
        );
        // The entry survives the interrupt: an emptied table would let
        // wait_empty fake completion the moment the hooks fire.
        assert_eq!(live.len(), 1);
        assert!(
            !live.wait_empty(Duration::from_millis(20)),
            "an interrupted-but-unwound worker is not an exited one"
        );

        // The worker unwinds and removes its own entry: only now is the
        // table empty.
        live.remove(id);
        assert!(live.wait_empty(Duration::from_millis(20)));
    }

    /// B2 CANCEL-ALL: a wedged worker keeps drain bounded and leaves the
    /// unconfirmed entry visible in the table — the timeout reports an
    /// explicit residue, it never empties the registry to fake completion.
    #[test]
    fn drain_leaves_a_wedged_worker_as_visible_residue() {
        let live = LiveStreams::new();
        live.insert(Arc::new(|| {}));
        // The "worker" never unwinds: drain stays bounded (two tiny graces)
        // and the residue stays countable.
        live.drain(Duration::from_millis(20));
        assert_eq!(
            live.len(),
            1,
            "a wedged worker must stay visible as unconfirmed residue"
        );
    }

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
