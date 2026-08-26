//! Derived, advisory completion-opportunity facts (LT-RUN-04 Slice C).
//!
//! The opportunity is a safe-point observation, never completion authority:
//! eligibility means the existing acceptance gate would accept, no proposal
//! is pending, task-relevant durable work exists, and a positive trusted
//! verification pass is current on the same basis. Everything here is pure
//! so the mandatory deterministic negatives are testable without an actor.

use sha2::{Digest, Sha256};

use agent_contracts::TaskId;

use crate::execution::{ExecutionState, ResourceProvenance, VerificationState};
use crate::task::TaskAnchor;

/// Hard bound for a persisted opportunity key.
pub(crate) const MAX_OPPORTUNITY_KEY_CHARS: usize = 160;
/// Stable prompt statement projected while the lease is outstanding.
pub(crate) const OPPORTUNITY_PROMPT_LINE: &str = "Completion opportunity: known blockers are resolved, durable work exists, and trusted verification is current. If this finishes the whole task, you may call task.complete to close it; otherwise keep working.";

/// One eligible opportunity: the body-free key plus the anchor revision it
/// was derived against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpportunityKey {
    pub key: String,
    pub anchor_revision: u64,
}

/// Outcome of one derivation consult.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpportunityDecision {
    /// `Some` when every eligibility condition holds.
    pub ready: Option<OpportunityKey>,
    /// Typed blocker class when not ready; "eligible" otherwise.
    pub reason: String,
}

impl OpportunityDecision {
    fn blocked(reason: &str) -> Self {
        Self {
            ready: None,
            reason: reason.to_string(),
        }
    }
}

/// Derive one completion-opportunity observation for the active task.
///
/// The checks mirror the actor's acceptance gate (`completion_gate`) in the
/// same order, then add the two positive-evidence conditions the gate
/// cannot see (durable task work and a current trusted verification pass).
pub(crate) fn derive_completion_opportunity(
    task_id: TaskId,
    anchor: &TaskAnchor,
    execution: &ExecutionState,
    pending_completion: bool,
    recovery_required: bool,
    unsettled_cleanup: bool,
) -> OpportunityDecision {
    if recovery_required {
        return OpportunityDecision::blocked("recovery fence active");
    }
    if unsettled_cleanup {
        return OpportunityDecision::blocked("cancelled operation still unsettled");
    }
    if pending_completion {
        return OpportunityDecision::blocked("completion proposal already pending");
    }
    if !anchor.open_loops.is_empty() {
        return OpportunityDecision::blocked(&format!(
            "{} explicit open loop(s) remain",
            anchor.open_loops.len()
        ));
    }
    let open = execution.open_obligation_count();
    if open > 0 {
        return OpportunityDecision::blocked(&format!("{open} unresolved execution obligation(s)"));
    }
    if !execution
        .checked_files
        .iter()
        .any(|fact| fact.provenance == ResourceProvenance::MutationResult)
    {
        return OpportunityDecision::blocked("no task-relevant durable mutation observed");
    }
    let Some(fact) = current_trusted_pass(execution) else {
        if execution.verification.state != VerificationState::Current {
            return OpportunityDecision::blocked("trusted verification is not current");
        }
        return OpportunityDecision::blocked("no trusted verification receipt on this basis");
    };
    OpportunityDecision {
        ready: Some(OpportunityKey {
            key: build_key(task_id, execution, fact),
            anchor_revision: execution.anchor_revision,
        }),
        reason: "eligible".to_string(),
    }
}

/// The latest positive, exactly-attributed verification pass that still
/// matches the full currentness tuple (task anchor, directive, admitted
/// workspace world). Empty provenance fields are legacy/non-exact evidence
/// and stay fail-closed.
fn current_trusted_pass(execution: &ExecutionState) -> Option<&crate::execution::VerificationFact> {
    if execution.validity() != VerificationState::Current {
        return None;
    }
    execution.verifications.iter().rev().find(|fact| {
        fact.ok
            && !fact.source_tool_name.is_empty()
            && !fact.verification_identity.is_empty()
            && fact.anchor_revision == execution.anchor_revision
            && fact.directive_revision == execution.directive_revision
            && fact.workspace_revision == execution.workspace_revision
    })
}

/// Body-free stable identity of one opportunity: task id + the revisions
/// that define its basis + a short digest over the verifier identity, exact
/// argument digest and evidence ref. A relevant mutation advances the world
/// revision, so the same task derives a fresh key after re-verification.
fn build_key(
    task_id: TaskId,
    execution: &ExecutionState,
    fact: &crate::execution::VerificationFact,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        fact.source_tool_name.as_str(),
        fact.argument_digest.as_str(),
        fact.verification_identity.as_str(),
        fact.evidence_ref.as_deref().unwrap_or(""),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0x1f]);
    }
    let digest = hasher.finalize();
    let short: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "opp/{task_id}/a{}/d{}/w{}/{}",
        execution.anchor_revision,
        execution.directive_revision,
        execution.workspace_revision,
        short
    )
}

/// The surface requirement leased while an offer is outstanding: exactly
/// one decision sees `task.complete` as a preferred schema.
pub(crate) fn opportunity_surface_requirement() -> agent_contracts::ToolSurfaceRequirement {
    agent_contracts::ToolSurfaceRequirement {
        tool_name: "task.complete".into(),
        demand: agent_contracts::ToolSurfaceDemand::PreferSurface,
        reason: "derived completion opportunity is currently eligible".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ResourceFact, VerificationCause, VerificationFact};
    use crate::task::TaskManager;
    use agent_contracts::ResourceFreshness;

    fn seeded_task() -> (TaskManager, TaskId, TaskAnchor) {
        let mut tasks = TaskManager::new();
        let (txn, id) = tasks.prepare_create("close the loop");
        tasks.commit(txn);
        let anchor = tasks.get(id).unwrap().anchor.clone();
        (tasks, id, anchor)
    }

    /// Make `execution` look like real production evidence: one durable
    /// mutation stamp plus a current trusted verification pass whose
    /// identity tuple matches the state's revisions.
    fn make_eligible(execution: &mut ExecutionState) {
        execution.checked_files.push(ResourceFact {
            path: "src/lib.rs".into(),
            digest: "deadbeef".into(),
            freshness: ResourceFreshness::Fresh,
            turn: 1,
            provenance: ResourceProvenance::MutationResult,
        });
        execution.anchor_revision = 1;
        execution.directive_revision = 2;
        execution.workspace_revision = 3;
        execution.verification.state = VerificationState::Current;
        execution.verifications.push(VerificationFact {
            summary: "cargo test".into(),
            ok: true,
            turn: 1,
            anchor_revision: 1,
            workspace_revision: 3,
            source_tool_name: "shell.exec".into(),
            argument_digest: "arg-digest".into(),
            verification_identity: "cargo-test-identity".into(),
            directive_revision: 2,
            evidence_ref: Some("artifact://v1/run/owner/digest".into()),
            recipe_provenance: None,
        });
    }

    #[test]
    fn initial_task_is_not_ready_without_durable_work_or_verification() {
        let (_tasks, id, anchor) = seeded_task();
        let decision = derive_completion_opportunity(
            id,
            &anchor,
            &ExecutionState::default(),
            false,
            false,
            false,
        );
        assert!(decision.ready.is_none());
        assert!(
            decision
                .reason
                .contains("no task-relevant durable mutation"),
            "a fresh task must be blocked on missing work, got: {}",
            decision.reason
        );

        // Read-only exploration alone stays blocked too: reads are not
        // mutations even when they are current and checked.
        let mut read_only = ExecutionState::default();
        read_only.checked_files.push(ResourceFact {
            path: "src/lib.rs".into(),
            digest: "cafe".into(),
            freshness: ResourceFreshness::Fresh,
            turn: 1,
            provenance: ResourceProvenance::Read,
        });
        let decision = derive_completion_opportunity(id, &anchor, &read_only, false, false, false);
        assert!(decision.ready.is_none());
        assert!(
            decision
                .reason
                .contains("no task-relevant durable mutation")
        );
    }

    #[test]
    fn mutation_without_current_verification_is_not_ready() {
        let (_tasks, id, anchor) = seeded_task();
        let mut execution = ExecutionState::default();
        execution.checked_files.push(ResourceFact {
            path: "src/lib.rs".into(),
            digest: "deadbeef".into(),
            freshness: ResourceFreshness::Fresh,
            turn: 1,
            provenance: ResourceProvenance::MutationResult,
        });
        // NotRun / Pending / Stale / Failed all block; each names its class.
        for (state, expected) in [
            (VerificationState::NotRun, "not current"),
            (VerificationState::Pending, "not current"),
            (VerificationState::Stale, "not current"),
            (VerificationState::Failed, "not current"),
        ] {
            execution.verification.state = state;
            let decision =
                derive_completion_opportunity(id, &anchor, &execution, false, false, false);
            assert!(decision.ready.is_none(), "{state:?} must block");
            assert!(decision.reason.contains(expected), "{}", decision.reason);
        }

        // Current but with no exact trusted receipt also blocks: a bare
        // ok row without attribution is not a positive receipt identity.
        execution.verification.state = VerificationState::Current;
        execution.verifications.push(VerificationFact {
            summary: "looks fine".into(),
            ok: true,
            turn: 2,
            anchor_revision: 0,
            workspace_revision: 0,
            source_tool_name: String::new(),
            argument_digest: String::new(),
            verification_identity: String::new(),
            directive_revision: 0,
            evidence_ref: None,
            recipe_provenance: None,
        });
        let decision = derive_completion_opportunity(id, &anchor, &execution, false, false, false);
        assert!(decision.ready.is_none());
        assert!(decision.reason.contains("receipt"));
    }

    #[test]
    fn basis_mismatch_between_fact_and_state_is_not_ready() {
        let (_tasks, id, anchor) = seeded_task();
        let mut execution = ExecutionState::default();
        make_eligible(&mut execution);
        // Any component of the tuple drifting (here: the workspace moved on)
        // breaks the same-basis requirement.
        execution.workspace_revision = 4;
        let decision = derive_completion_opportunity(id, &anchor, &execution, false, false, false);
        assert!(decision.ready.is_none());
        assert!(decision.reason.contains("receipt") || decision.reason.contains("current"));
    }

    #[test]
    fn open_loops_obligations_recovery_cleanup_and_pending_proposal_block_first() {
        let (_tasks, id, mut anchor) = seeded_task();
        let mut execution = ExecutionState::default();
        make_eligible(&mut execution);

        anchor.open_loops.push("verify edge cases".into());
        let decision = derive_completion_opportunity(id, &anchor, &execution, false, false, false);
        assert!(decision.ready.is_none());
        assert!(decision.reason.contains("open loop"));
        anchor.open_loops.clear();

        execution
            .obligations
            .push(crate::execution::ExecutionObligation {
                domain: agent_contracts::ToolFailureDomain::ExecutableResolution,
                scope_key: "scope".into(),
                precondition: "pre".into(),
                attempts: 1,
                total_attempts: 1,
                epoch: 0,
                opened_at_evidence_revision: 0,
                tried_targets: Vec::new(),
                source_tool_name: String::new(),
            });
        let decision = derive_completion_opportunity(id, &anchor, &execution, false, false, false);
        assert!(decision.ready.is_none());
        assert!(decision.reason.contains("obligation"));

        let decision = derive_completion_opportunity(id, &anchor, &execution, true, false, false);
        assert!(decision.ready.is_none());
        assert!(decision.reason.contains("pending"));

        let decision = derive_completion_opportunity(id, &anchor, &execution, false, true, false);
        assert!(decision.ready.is_none());
        assert!(decision.reason.contains("recovery"));

        let decision = derive_completion_opportunity(id, &anchor, &execution, false, false, true);
        assert!(decision.ready.is_none());
        assert!(decision.reason.contains("unsettled"));
    }

    #[test]
    fn eligible_key_is_stable_per_basis_and_moves_with_the_world() {
        let (_tasks, id, anchor) = seeded_task();
        let mut execution = ExecutionState::default();
        make_eligible(&mut execution);

        let first = derive_completion_opportunity(id, &anchor, &execution, false, false, false);
        let first_key = first.ready.clone().expect("fixture must be eligible");
        assert_eq!(first.reason, "eligible");

        // Unchanged reads and progress-only edits do not move any basis
        // component: the derivation returns the identical key.
        let again = derive_completion_opportunity(id, &anchor, &execution, false, false, false);
        assert_eq!(again.ready, Some(first_key.clone()));

        // A relevant mutation advances the world revision; after a fresh
        // current pass the derived key differs, so one offer per key holds.
        execution.workspace_revision = 9;
        execution.verifications[0].workspace_revision = 9;
        let moved = derive_completion_opportunity(id, &anchor, &execution, false, false, false);
        let moved_key = moved.ready.expect("re-verified basis is eligible again");
        assert_ne!(moved_key.key, first_key.key);
        assert!(moved_key.key.starts_with(&format!("opp/{id}/a1/d2/w9/")));

        // Keys stay bounded regardless of input sizes.
        assert!(first_key.key.chars().count() <= MAX_OPPORTUNITY_KEY_CHARS);
    }

    #[test]
    fn recorded_offer_keys_stay_bounded_in_execution_state() {
        let mut execution = ExecutionState::default();
        let long_key = "x".repeat(MAX_OPPORTUNITY_KEY_CHARS + 40);
        execution.record_opportunity_offer(long_key);
        assert_eq!(
            execution
                .last_offered_opportunity
                .as_ref()
                .map(|key| key.chars().count()),
            Some(MAX_OPPORTUNITY_KEY_CHARS)
        );
        // VerificationCause::None is untouched by offers: recording an offer
        // never masquerades as a spec change.
        assert_eq!(execution.verification.cause, VerificationCause::None);
    }
}
