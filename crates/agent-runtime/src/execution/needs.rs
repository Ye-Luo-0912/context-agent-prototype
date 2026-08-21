//! Pure projection of round fact-gaps. No planner and no second LLM.

use agent_contracts::ContextDiagnostics;

use crate::task::TaskAnchor;

/// Deterministic fact-gaps for one BeforeModel round.
///
/// Runtime names missing *facts*, not the model's next action. The LLM
/// decides whether to read, search, edit, run, or answer. Do not grow
/// this into NeedRead / NeedEdit / NeedRun / NeedPlan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundNeeds {
    /// A verification obligation is due *this round*.
    pub verification_due: bool,
    /// Open failed-command rows still need a successful counterpart.
    pub unresolved_failure: bool,
    /// Identity is known outside the selected working set (EXTERNAL CONTEXT
    /// refs, Warm/Cold/Stored catalog, or TaskAnchor evidence claims).
    pub evidence_needed: bool,
    /// TaskAnchor open loops exist; retrieval may be needed, but Runtime
    /// does not PreferSurface Read/Search as an exploration plan.
    pub open_loop_needs_evidence: bool,
}

/// Warm / Cold / Stored catalog entries may appear as EXTERNAL CONTEXT refs.
/// Used at BeforeModel without waiting for materialize.
pub fn catalog_has_external_context(diagnostics: &ContextDiagnostics) -> bool {
    diagnostics.warm_items > 0 || diagnostics.cold_items > 0 || diagnostics.external_items > 0
}

/// Project fact-gaps from TurnIntent + TaskSpec + operational flags.
///
/// `verification_due` / `unresolved_failure` / `evidence_needed` /
/// `open_loop_needs_evidence` are state gaps. Mutation is not inferred
/// from a non-empty user instruction — the model chooses edit/write itself.
pub fn derive_round_needs(
    _turn_intent: Option<&str>,
    _focus_goal: Option<&str>,
    anchor: Option<&TaskAnchor>,
    verification_due: bool,
    has_failures: bool,
    has_external_context: bool,
) -> RoundNeeds {
    let open_loops = anchor.map(|a| !a.open_loops.is_empty()).unwrap_or(false);
    let evidence_refs = anchor.map(|a| !a.evidence_refs.is_empty()).unwrap_or(false);
    RoundNeeds {
        verification_due,
        unresolved_failure: has_failures,
        evidence_needed: has_external_context || evidence_refs,
        open_loop_needs_evidence: open_loops,
    }
}

/// Checkpoint/wire name kept so existing policy comments still compile.
/// Prefer [`RoundNeeds`] in new code.
pub type ExecutionNeeds = RoundNeeds;

pub use derive_round_needs as derive_execution_needs;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{ContextRootClaim, RootClaimRole, RootClaimStrength, TaskAnchor};

    #[test]
    fn empty_inputs_derive_no_needs() {
        let needs = derive_round_needs(None, None, None, false, false, false);
        assert_eq!(needs, RoundNeeds::default());
    }

    #[test]
    fn external_catalog_is_evidence_needed() {
        let needs = derive_round_needs(None, None, None, false, false, true);
        assert!(needs.evidence_needed);
        assert!(!needs.open_loop_needs_evidence);
        assert!(!needs.verification_due);
        assert!(!needs.unresolved_failure);
    }

    #[test]
    fn evidence_refs_are_evidence_needed() {
        let anchor = TaskAnchor {
            evidence_refs: vec![ContextRootClaim {
                item_ref: "context://run/evidence".into(),
                role: RootClaimRole::Verification,
                strength: RootClaimStrength::StorageRequired,
                source_field_id: "evidence_refs".into(),
            }],
            ..TaskAnchor::default()
        };
        let needs = derive_round_needs(None, Some("goal"), Some(&anchor), false, false, false);
        assert!(needs.evidence_needed);
        assert!(!needs.open_loop_needs_evidence);
    }

    #[test]
    fn open_loops_are_open_loop_needs_evidence_not_an_explore_plan() {
        let anchor = TaskAnchor {
            open_loops: vec!["why did tests fail?".into()],
            ..TaskAnchor::default()
        };
        let needs = derive_round_needs(
            Some("please continue"),
            Some("goal"),
            Some(&anchor),
            false,
            false,
            false,
        );
        assert!(needs.open_loop_needs_evidence);
        assert!(!needs.verification_due);
    }

    #[test]
    fn user_instruction_does_not_invent_an_action_need() {
        let needs = derive_round_needs(
            Some("edit src/auth.rs and add a helper"),
            Some("goal"),
            None,
            false,
            false,
            false,
        );
        assert_eq!(needs, RoundNeeds::default());
    }

    #[test]
    fn catalog_predicate_uses_warm_cold_or_external() {
        assert!(!catalog_has_external_context(&ContextDiagnostics::default()));
        assert!(catalog_has_external_context(&ContextDiagnostics {
            warm_items: 1,
            ..ContextDiagnostics::default()
        }));
        assert!(catalog_has_external_context(&ContextDiagnostics {
            cold_items: 1,
            ..ContextDiagnostics::default()
        }));
        assert!(catalog_has_external_context(&ContextDiagnostics {
            external_items: 2,
            ..ContextDiagnostics::default()
        }));
    }
}
