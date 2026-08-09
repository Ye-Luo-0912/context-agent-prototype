//! A `ContextEngine` adapter over a process boundary.
//!
//! `ContextServiceAdapter` implements the exact `agent-contracts::ContextEngine`
//! trait by talking to the `agent-context-service` process over a JSON-lines
//! stdio protocol (`wire`). This is the ContextCore integration shape: the
//! kernel, tools, provider and UI are untouched — only the composition root
//! picks which engine to run. Swapping the service behind the pipe for a real
//! ContextCore runtime is a deployment detail.

mod adapter;
mod capability_host;
mod host;
mod wire;

pub use adapter::{ContextServiceAdapter, ContextServiceConfig, ServiceEngine, connect_engine};
pub use capability_host::{ProcessCapabilityAdapter, load_process_capability};
pub use host::{
    PROTOCOL_VERSION, ProcessHost, ProcessHostConfig, ProcessSandbox, probe_siblings,
    resolve_program,
};
pub use wire::{ServiceOp, ServiceRequest, ServiceResponse};
