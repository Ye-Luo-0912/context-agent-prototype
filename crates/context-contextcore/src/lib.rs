//! A `ContextEngine` adapter over a process boundary.
//!
//! `ContextServiceAdapter` implements the exact `agent-contracts::ContextEngine`
//! trait by talking to the `agent-context-service` process over a JSON-lines
//! stdio protocol (`wire`). This is the ContextCore integration shape: the
//! kernel, tools, provider and UI are untouched — only the composition root
//! picks which engine to run. Swapping the service behind the pipe for a real
//! ContextCore runtime is a deployment detail.
//!
//! The shared framed transport lives in `agent-process`; this crate is only
//! the protocol layer mapping `ContextEngine` operations onto the service's
//! wire operations.

mod adapter;
mod wire;

pub use adapter::{ContextServiceAdapter, ContextServiceConfig, ServiceEngine, connect_engine};
pub use wire::{
    DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES, MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES, PROTOCOL_VERSION,
    ServiceOp, ServiceRequest, ServiceResponse,
};
