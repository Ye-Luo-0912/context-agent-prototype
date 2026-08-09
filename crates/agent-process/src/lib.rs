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
//! policy lives here once.

mod host;

pub use host::{
    PROTOCOL_VERSION, ProcessHost, ProcessHostConfig, ProcessSandbox, probe_siblings,
    resolve_program,
};
