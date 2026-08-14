//! The shared JSON-lines process host for every process-boundary
//! integration: spawning the child, the startup handshake, bounded framed
//! request/response, per-request deadlines, cancellation, and a
//! poisoned-connection policy so a wedged or malicious child can never be
//! reused or grow the parent's memory without bound.
//!
//! Thin protocol layers build on top of this host — the context-service
//! adapter (`ContextEngine` over a process, in `context-contextcore`) and
//! the process capability adapter (`Capability` over a process, in
//! `agent-capability-process`). The framing, deadline, sandbox and failure
//! policy lives here once. [`FramedProtocolSession`] is the reusable
//! JSON-lines session for inherited-pipe analogue streams; local transport
//! identity is never a Core grant.

mod frame;
mod host;
mod lifecycle;
mod session;

#[cfg(target_os = "linux")]
pub mod landlock;

pub use frame::{FrameError, FrameErrorKind, encode_frame, encode_frame_bytes, read_frame};
pub use host::{
    MAX_SYSTEM_REQUESTS_PER_CALL, PROTOCOL_VERSION, ProcessHost, ProcessHostConfig, ProcessSandbox,
    SystemBroker, kill_process_tree, probe_siblings, resolve_program,
};
pub use lifecycle::{
    ProcessIdentity, capture_process_identity, kill_matching_process_tree,
    process_identity_matches, process_is_running,
};
pub use session::FramedProtocolSession;
