//! The shared JSON-lines process host for every process-boundary
//! integration: spawning the child, the startup handshake, bounded framed
//! request/response, per-request deadlines, cancellation, and a
//! poisoned-connection policy so a wedged or malicious child can never be
//! reused or grow the parent's memory without bound. Connection health,
//! epochs and a bounded restart circuit (`PLAT-06` slice 1) live here, as
//! do peer cancel-ACK frames and coalescible progress (`PLAT-06` slice 2);
//! they are never task state or Core authority.
//!
//! Thin protocol layers build on top of this host — the context-service
//! adapter (`ContextEngine` over a process, in `context-contextcore`) and
//! the process capability adapter (`Capability` over a process, in
//! `agent-capability-process`). [`FramedProtocolSession`] implements
//! [`DuplexTransport`]; child lifecycle lives in [`ProcessSupervisor`]
//! (`ProcessHost` and MCP stdio both own one). Local transport identity
//! is never a Core grant.

mod frame;
mod health;
mod host;
mod lifecycle;
mod session;
mod supervisor;

#[cfg(windows)]
pub mod integrity;
#[cfg(target_os = "linux")]
pub mod landlock;

pub use frame::{FrameError, FrameErrorKind, encode_frame, encode_frame_bytes, read_frame};
pub use health::{
    ConnectKind, ConnectionEpoch, ConnectionHealth, ConnectionStatus,
    DEFAULT_MAX_CONNECTION_RESTARTS, HostLifecycle, RestartCircuit,
};
pub use host::{
    DEFAULT_CANCEL_ACK_TIMEOUT, MAX_PROGRESS_FRAMES_PER_CALL, MAX_PROGRESS_NOTE_CHARS,
    MAX_SYSTEM_REQUESTS_PER_CALL, PROTOCOL_VERSION, ProcessHost, ProcessHostConfig, ProcessSandbox,
    SystemBroker, kill_process_tree, probe_siblings, resolve_program,
};
#[cfg(unix)]
pub use host::{apply_unix_rlimits, close_inherited_fds};
pub use lifecycle::{
    ProcessIdentity, capture_process_identity, kill_matching_process_tree,
    process_identity_matches, process_is_running,
};
pub use session::{DuplexTransport, FramedProtocolSession, StdioDuplexTransport};
pub use supervisor::ProcessSupervisor;
