//! Pure projection of execution needs. No planner and no second LLM.

use crate::task::{RootClaimRole, TaskAnchor};

/// Deterministic execution needs for one BeforeModel round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecutionNeeds {
    pub verify: bool,
    pub mutate: bool,
    pub explore: bool,
    pub repair: bool,
}

/// Project needs from TurnIntent + TaskSpec + operational flags.
pub fn derive_execution_needs(
    turn_intent: Option<&str>,
    focus_goal: Option<&str>,
    anchor: Option<&TaskAnchor>,
    verification_due: bool,
    has_failures: bool,
) -> ExecutionNeeds {
    let intent = turn_intent.map(str::trim).unwrap_or("");
    let focus = focus_goal.map(str::trim).unwrap_or("");
    let open_loops = anchor.map(|a| !a.open_loops.is_empty()).unwrap_or(false);
    let plan_progress = anchor.map(|a| !a.plan_progress.is_empty()).unwrap_or(false);
    let artifact_access = anchor
        .map(|a| {
            a.working_refs.iter().any(|claim| {
                matches!(
                    claim.role,
                    RootClaimRole::WorkingArtifact | RootClaimRole::Verification
                )
            })
        })
        .unwrap_or(false);
    ExecutionNeeds {
        verify: verification_due,
        mutate: !intent.is_empty() || plan_progress,
        explore: open_loops
            || artifact_access
            || (anchor.is_none() && (!intent.is_empty() || !focus.is_empty())),
        repair: has_failures,
    }
}
