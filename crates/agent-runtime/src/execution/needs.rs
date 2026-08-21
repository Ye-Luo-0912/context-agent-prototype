//! Pure projection of execution needs. No planner and no second LLM.

use agent_contracts::ContextDiagnostics;

use crate::task::{RootClaimRole, TaskAnchor};

/// Deterministic execution needs for one BeforeModel round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecutionNeeds {
    pub verify: bool,
    pub mutate: bool,
    pub explore: bool,
    pub repair: bool,
    /// Identity is known outside the selected working set (EXTERNAL CONTEXT
    /// refs, Warm/Cold/Stored catalog, or TaskAnchor evidence claims).
    /// Surfaces `context.manage` as the catalog retrieval safety net.
    pub evidence: bool,
}

/// Warm / Cold / Stored catalog entries may appear as EXTERNAL CONTEXT refs.
/// Used at BeforeModel without waiting for materialize.
pub fn catalog_has_external_context(diagnostics: &ContextDiagnostics) -> bool {
    diagnostics.warm_items > 0 || diagnostics.cold_items > 0 || diagnostics.external_items > 0
}

/// Project needs from TurnIntent + TaskSpec + operational flags.
///
/// Runtime derives **missing facts**, not the model's next action.
/// `verify` / `repair` / `evidence` are state gaps. `mutate` is not inferred
/// from a non-empty user instruction — the model chooses edit/write itself.
pub fn derive_execution_needs(
    turn_intent: Option<&str>,
    focus_goal: Option<&str>,
    anchor: Option<&TaskAnchor>,
    verification_due: bool,
    has_failures: bool,
    has_external_context: bool,
) -> ExecutionNeeds {
    let intent = turn_intent.map(str::trim).unwrap_or("");
    let focus = focus_goal.map(str::trim).unwrap_or("");
    let open_loops = anchor.map(|a| !a.open_loops.is_empty()).unwrap_or(false);
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
    let evidence_refs = anchor.map(|a| !a.evidence_refs.is_empty()).unwrap_or(false);
    ExecutionNeeds {
        verify: verification_due,
        mutate: false,
        explore: open_loops
            || artifact_access
            || (anchor.is_none() && (!intent.is_empty() || !focus.is_empty())),
        repair: has_failures,
        evidence: has_external_context || evidence_refs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{ContextRootClaim, RootClaimStrength, TaskAnchor};

    #[test]
    fn empty_inputs_derive_no_needs() {
        let needs = derive_execution_needs(None, None, None, false, false, false);
        assert_eq!(needs, ExecutionNeeds::default());
    }

    #[test]
    fn external_catalog_is_need_evidence() {
        let needs = derive_execution_needs(None, None, None, false, false, true);
        assert!(needs.evidence);
        assert!(!needs.explore);
        assert!(!needs.verify);
    }

    #[test]
    fn evidence_refs_are_need_evidence() {
        let anchor = TaskAnchor {
            evidence_refs: vec![ContextRootClaim {
                item_ref: "context://run/evidence".into(),
                role: RootClaimRole::Verification,
                strength: RootClaimStrength::StorageRequired,
                source_field_id: "evidence_refs".into(),
            }],
            ..TaskAnchor::default()
        };
        let needs = derive_execution_needs(None, Some("goal"), Some(&anchor), false, false, false);
        assert!(needs.evidence);
        assert!(!needs.explore);
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
