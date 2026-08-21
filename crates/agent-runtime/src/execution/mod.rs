//! Execution Coherence V1: provable world state, not a planner.
//!
//! `TaskAnchor` remains the only task authority. This module is the
//! checkpointable operational subset formerly called `ResumePoint` — not a
//! second task table. The LLM still chooses actions. Runtime only
//! maintains what the world can currently prove.
//!
//! Four phases, in order:
//!
//! ```text
//! World Facts (path@rev / errors)
//!   → Freshness Engine (Fresh / NeedsRevalidation / Missing)
//!   → Obligation Ledger (verify / failure / unresolved evidence)
//!   → Round Projection (due_now / foreground refs / missing evidence)
//!        → Prompt and Tool roles
//!        → LLM
//! ```
//!
//! Invariants:
//!
//! 1. Unknown ≠ False, and NeedsRevalidation ≠ Fresh. Do not delete facts
//!    to hide uncertainty.
//! 2. Obligation exists ≠ Due now. Do not wipe a real obligation just to
//!    avoid surfacing Verify.
//! 3. Resource identity known ≠ body available in prompt. Descriptor-only
//!    selected items, Checked omit, and Foreground Evidence exist because
//!    of this gap.
//!
//! Prompt assembly reads `TaskAnchor` + `TurnIntent` + [`ExecutionState`].
//! Tool-surface policy maps [`ExecutionNeeds`] onto catalog `ToolSpec.roles`.
//! NeedEvidence PreferSurfaces `context.manage` when Warm/Cold/Stored catalog
//! entries or TaskAnchor `evidence_refs` exist.
//!
//! Phase 2 ([`memo`]) stays unwired. [`memo::lookup`] is always a miss.
//! When it is wired, the first version caches only `fs.read` keyed by
//! path + line range + content revision. `search.grep` / `git.diff` /
//! `git.status` wait for a workspace snapshot identity. Memo never
//! intercepts writes, patches, or shell side-effects, and it cannot
//! replace this algorithm (the model round has already happened).

mod classify;
mod freshness;
pub mod memo;
mod needs;
mod state;

pub use classify::{classify_fs_read_motive, stamp_fs_read_motive};
pub use needs::{ExecutionNeeds, catalog_has_external_context, derive_execution_needs};
pub use state::{ExecutionState, ResourceFact};

/// Checkpoint/wire name for [`ExecutionState`]. The `TaskRecord` field is
/// still `resume` so existing checkpoints load. Prefer `ExecutionState` in
/// new code; do not introduce a third operational table.
#[allow(dead_code)]
pub type ResumePoint = ExecutionState;

pub(crate) use state::validate_execution_state as validate_resume;

#[cfg(test)]
mod tests;
