//! Operational execution state: resource facts, freshness, and needs.
//!
//! `TaskAnchor` remains the only task authority. This module is the
//! checkpointable operational subset formerly called `ResumePoint` — not a
//! second task table and not a planner. Prompt assembly reads
//! `TaskAnchor` + `TurnIntent` + [`ExecutionState`]. Tool-surface policy
//! maps [`ExecutionNeeds`] onto catalog `ToolSpec.roles`.
//!
//! Phase 2 ([`memo`]) may cache read/query observations. It must never
//! intercept writes, patches, or shell side-effects, and it does not
//! replace this state algorithm.

mod classify;
mod freshness;
pub mod memo;
mod needs;
mod state;

pub use classify::{classify_fs_read_motive, stamp_fs_read_motive};
pub use needs::{ExecutionNeeds, derive_execution_needs};
pub use state::{ExecutionState, ResourceFact};

/// Checkpoint/wire name for [`ExecutionState`]. The `TaskRecord` field is
/// still `resume` so existing checkpoints load. Prefer `ExecutionState` in
/// new code; do not introduce a third operational table.
#[allow(dead_code)]
pub type ResumePoint = ExecutionState;

pub(crate) use state::validate_execution_state as validate_resume;

#[cfg(test)]
mod tests;
