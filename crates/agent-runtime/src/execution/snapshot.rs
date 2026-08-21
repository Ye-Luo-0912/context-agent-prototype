//! One BeforeModel projection of execution truth.
//!
//! Prompt, ContextHints, and tool-surface policy all read this snapshot.
//! Do not clone `ExecutionState` and replay `TurnFrame` per consumer.

use agent_contracts::{ResourceKey, TaskProgressView};

use crate::task::TaskAnchor;

use super::{
    ExecutionState, RoundNeeds, VerificationCause, VerificationCoverage, VerificationState,
    derive_round_needs,
};

/// Derived verification fields for one round. Stored obligation/evidence
/// facts live on [`ExecutionState`]; this is the projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationProjection {
    pub validity: VerificationState,
    pub due: bool,
    pub cause: VerificationCause,
    pub coverage: VerificationCoverage,
    pub required_for_completion: bool,
}

/// Single BeforeModel snapshot. Capture once after revalidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundExecutionSnapshot {
    pub progress: TaskProgressView,
    pub foreground_resources: Vec<ResourceKey>,
    pub verification: VerificationProjection,
    pub needs: RoundNeeds,
}

impl RoundExecutionSnapshot {
    pub fn capture(
        state: &ExecutionState,
        turn_intent: &str,
        focus_goal: Option<&str>,
        anchor: Option<&TaskAnchor>,
        has_external_context: bool,
    ) -> Self {
        let due = state.verification_due_now(turn_intent);
        Self {
            progress: state.view(),
            foreground_resources: state.foreground_resources(turn_intent),
            verification: VerificationProjection {
                validity: state.validity(),
                due,
                cause: state.verification.cause,
                coverage: state.verification.coverage.clone(),
                required_for_completion: state.verification.required_for_completion,
            },
            needs: derive_round_needs(
                Some(turn_intent),
                focus_goal,
                anchor,
                due,
                state.has_failures(),
                has_external_context,
            ),
        }
    }
}
