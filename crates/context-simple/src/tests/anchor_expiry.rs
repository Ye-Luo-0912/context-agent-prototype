//! CTX-5 (R2-03): live items explicitly held by a *current* anchor root
//! claim must not be terminated by the heuristic expiry paths — resident
//! TTL/staleness, warm-buffer aging, or the full sweep's ordinary-dialogue
//! aging. Every test walks the full claim → maintain/gc → (materialize)
//! chain, never just `mark_roots`. Terminal items stay terminal, a released
//! projection ends the protection, and `StorageRequired` keeps protecting
//! storage only (it never extends residency).

use agent_contracts::{
    AnchorRootClaim, AnchorRootStrength, AttentionState, ContextAction, ContextEngine,
    ContextIngress, ContextItem, ContextItemId, ContextKind, ContextMaintenanceTrigger,
    ContextMaterializationMissReason, ContextQuery, ContextResidency, ContextRetention,
    ContextScope, RootReason, SemanticState,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

fn claim_for(item_ref: &ContextItemId, strength: AnchorRootStrength) -> AnchorRootClaim {
    AnchorRootClaim {
        item_ref: item_ref.to_string(),
        strength,
        source_field_id: "working_refs".into(),
        anchor_revision: 3,
        reason: RootReason::HardConstraint,
    }
}

async fn push_claims(engine: &SimpleContextEngine, roots: Vec<AnchorRootClaim>) {
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots { roots },
        })
        .await
        .unwrap();
}

/// A plain Working note: no promotable tag, no hot entities (no user
/// message follows), so every heuristic aging path applies to it.
async fn plain_working_item(engine: &SimpleContextEngine, content: &str) -> ContextItem {
    let state = engine.state.lock().await;
    crate::item::make_item(
        &state,
        &engine.config,
        content.into(),
        ContextKind::Note,
        ContextScope::Task,
        ContextRetention::Working,
        0.4,
        Some("test:plain".into()),
    )
}

/// Age the item (and the world) past every TTL and staleness window.
async fn age_beyond_all_windows(engine: &SimpleContextEngine, item_id: ContextItemId) {
    let mut state = engine.state.lock().await;
    state.turn = 100;
    if let Some(index) = state.items.indexes().get(item_id) {
        state.items.items_mut()[index].created_turn = 0;
    }
}

#[tokio::test]
async fn resident_required_working_item_survives_the_full_sweep_across_ttl_x4() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        turn_ttl_ticks: 1, // stale window = 4 turns; we age to 100
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "hold this constraint").await;
    let item = plain_working_item(&engine, "timeout budget is 5 seconds for AuthService").await;
    let id = item.id;
    {
        let mut state = engine.state.lock().await;
        state.items.push(item);
    }
    age_beyond_all_windows(&engine, id).await;
    push_claims(
        &engine,
        vec![claim_for(&id, AnchorRootStrength::ResidentRequired)],
    )
    .await;

    let report = engine.gc().await.unwrap();
    let state = engine.state.lock().await;
    assert!(
        state.items.iter().any(|item| item.id == id),
        "a ResidentRequired live item must not be aged out of the heap by the sweep: {report:?}"
    );
    assert!(
        state
            .items
            .iter()
            .all(|item| item.id != id || item.semantic.is_live()),
        "protection must not depend on semantic death: {report:?}"
    );
}

#[tokio::test]
async fn prompt_required_ephemeral_survives_minor_ttl_and_reaches_the_final_frame() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        turn_ttl_ticks: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "evidence stays required").await;
    let item = {
        let state = engine.state.lock().await;
        crate::item::make_item(
            &state,
            &engine.config,
            "probe output: status OK from AuthService".into(),
            ContextKind::Note,
            ContextScope::Turn,
            ContextRetention::Ephemeral,
            0.3,
            Some("test:ephemeral".into()),
        )
    };
    let id = item.id;
    {
        let mut state = engine.state.lock().await;
        state.items.push(item);
    }
    age_beyond_all_windows(&engine, id).await;
    push_claims(
        &engine,
        vec![claim_for(&id, AnchorRootStrength::PromptRequired)],
    )
    .await;

    // A non-AfterModel trigger reaches the ephemeral TTL branch directly
    // (AfterModel first *consumes* the observation — attention only); the
    // claim must defer the semantic death that would otherwise follow.
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state
                .items
                .iter()
                .any(|item| item.id == id && item.semantic.is_live()),
            "a PromptRequired live item must not be tombstoned by the ephemeral TTL: {report:?}"
        );
    }

    // The final model request must actually carry the body — a premature
    // tombstone surfaces as a PolicyExcluded required miss instead.
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![claim_for(&id, AnchorRootStrength::PromptRequired)],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(
        materialized
            .required_misses
            .as_slice()
            .iter()
            .all(|miss| miss.reason != ContextMaterializationMissReason::PolicyExcluded),
        "the claimed body must not be reported as policy-excluded: {:?}",
        materialized.required_misses
    );
    assert!(
        materialized.items.iter().any(|entry| entry.item_id == id),
        "the claimed body must reach the final frame: {:?}",
        materialized.items
    );
}

#[tokio::test]
async fn resident_required_warm_item_is_not_tombstoned_by_warm_aging() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        turn_ttl_ticks: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "warm but held").await;
    let mut item = plain_working_item(&engine, "middleware ordering constraint").await;
    let id = item.id;
    item.created_turn = 0;
    item.residency = ContextResidency::Warm;
    item.evicted_at_tick = Some(1);
    let claim = claim_for(&id, AnchorRootStrength::ResidentRequired);
    {
        let mut state = engine.state.lock().await;
        state.turn = 100;
        state.eviction_buffer.push(item);
    }
    push_claims(&engine, vec![claim]).await;

    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert!(
        state
            .eviction_buffer
            .iter()
            .any(|item| item.id == id && item.semantic.is_live()),
        "warm aging must not tombstone a claim-held live item: {report:?}"
    );
}

#[tokio::test]
async fn a_claim_cannot_resurrect_a_superseded_item() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "dead stays dead").await;
    let mut item = plain_working_item(&engine, "old decision").await;
    item.semantic = SemanticState::Superseded { by: None };
    item.attention = AttentionState::Archived;
    let id = item.id;
    let claim = claim_for(&id, AnchorRootStrength::ResidentRequired);
    {
        let mut state = engine.state.lock().await;
        state.items.push(item);
    }
    push_claims(&engine, vec![claim]).await;

    let report = engine.gc().await.unwrap();
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == id),
        "terminal semantic death is never resurrected by a claim: {report:?}"
    );
}

#[tokio::test]
async fn releasing_the_projection_ends_the_protection_without_resurrection() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        turn_ttl_ticks: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "protection ends with release").await;
    let item = plain_working_item(&engine, "held while claimed").await;
    let id = item.id;
    {
        let mut state = engine.state.lock().await;
        state.items.push(item);
    }
    age_beyond_all_windows(&engine, id).await;
    push_claims(
        &engine,
        vec![claim_for(&id, AnchorRootStrength::ResidentRequired)],
    )
    .await;
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.items.iter().any(|item| item.id == id),
            "protected while the projection holds it"
        );
    }

    // Release: an empty projection (what the runtime pushes at a completion
    // boundary) must end the protection — the item then ages normally.
    push_claims(&engine, Vec::new()).await;
    let report = engine.gc().await.unwrap();
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == id),
        "after release the aged item must leave the heap: {report:?}"
    );
    assert!(
        state
            .eviction_buffer
            .iter()
            .any(|item| item.id == id && item.semantic.is_live()),
        "aging is reversible eviction, not death"
    );
}

#[tokio::test]
async fn storage_required_does_not_extend_residency() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        turn_ttl_ticks: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "storage only").await;
    let item = plain_working_item(&engine, "archival evidence body").await;
    let id = item.id;
    let claim = claim_for(&id, AnchorRootStrength::StorageRequired);
    {
        let mut state = engine.state.lock().await;
        state.items.push(item);
    }
    age_beyond_all_windows(&engine, id).await;
    push_claims(&engine, vec![claim]).await;

    let report = engine.gc().await.unwrap();
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == id),
        "StorageRequired protects storage, not residency: {report:?}"
    );
    assert!(
        state
            .eviction_buffer
            .iter()
            .any(|item| item.id == id && item.semantic.is_live()),
        "the storage-required item must still be alive (reversibly evicted, recallable)"
    );
}

#[tokio::test]
async fn gc_report_names_residency_claims_that_hold_nothing_live() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "unsatisfied claim is visible").await;
    // The pass only runs when some body location is non-empty; give it one
    // item so the commit (and its claim accounting) happens.
    let item = plain_working_item(&engine, "unrelated resident body").await;
    {
        let mut state = engine.state.lock().await;
        state.items.push(item);
    }
    push_claims(
        &engine,
        vec![AnchorRootClaim {
            item_ref: "context://run/00000000-0000-0000-0000-00000000000f".into(),
            strength: AnchorRootStrength::ResidentRequired,
            source_field_id: "working_refs".into(),
            anchor_revision: 9,
            reason: RootReason::HardConstraint,
        }],
    )
    .await;

    let report = engine.gc().await.unwrap();
    assert!(
        report
            .anchor_root_misses
            .iter()
            .any(|miss| miss.strength == AnchorRootStrength::ResidentRequired),
        "a residency claim matched to no live item must be explicitly reported: {report:?}"
    );
}
