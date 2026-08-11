//! Shared tool/capability conformance harness (TOOLS-04).
//!
//! Every tool, capability and process adapter must pass the same set of
//! contract checks before it is trusted on the model surface. The checks in
//! this crate encode the normative envelope from
//! `docs/TOOL_RESULT_ENVELOPE.md` (TOOLS-03) and the per-tool matrix from
//! `docs/TOOL_INVENTORY.json` (TOOLS-01):
//!
//! - `check_schema_contract`: a `ToolSpec` is a well-formed, bounded
//!   model-visible schema (name/description present, `type: object`,
//!   schema tokens within the round surface budget);
//! - `check_output_envelope`: a `ToolOutput` (after the trusted
//!   `OutputBroker` bound it) stays within every global cap and carries a
//!   valid artifact reference;
//! - `check_error_envelope`: a tool error is a structured `AgentError`
//!   category, never an unbounded internal leak;
//! - `check_tool_surface`: the dispatcher's default surface and lifecycle
//!   rules match the runtime contract (core + control always visible,
//!   fail-closed omission, core tools cannot be unloaded).
//!
//! The library side depends only on the contracts and the workspace broker;
//! concrete dispatchers (builtin catalog, capability-aware composite) are
//! exercised through the shared harness in integration tests, so the same
//! checks apply to external capabilities once they are loadable.

pub mod checks;
pub mod report;

pub use checks::{
    check_catalog, check_error_envelope, check_output_envelope, check_schema_contract,
    check_tool_surface,
};
pub use report::{ConformanceReport, ConformanceViolation};
