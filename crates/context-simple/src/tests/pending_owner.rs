//! CTX-6 (R2-04): the externalize-retry list (`pending_externalize_retry`)
//! owns full in-memory bodies while the context store is unavailable. Every
//! semantic and materialization path must resolve the item's *unique owner*
//! wherever it lives — terminal transitions (supersession, verification),
//! required-body planning, admit/derive/lease directives, scope promotion
//! and quota accounting. A pending body must never be invisible to one path
//! and searchable through another; after the disk recovers, externalization
//! must land the *current* semantics, not stale ones.
//!
//! Setup note: the tests place items directly into the retry list — byte
//! for byte the state a failed store write leaves behind (plan put them
//! there, the IO phase failed, the commit kept them). The real
//! store-unreachable overflow path itself is exercised by
//! `checkpoint_roundtrip_keeps_exactly_one_pending_owner` via a blocked
//! store directory.

use agent_contracts::{
    AnchorRootClaim, AnchorRootStrength, ContextAction, ContextEngine, ContextIngress,
    ContextItemId, ContextKind, ContextMaintenanceTrigger, ContextMaterializationMissReason,
    ContextQuery, ContextResidency, ContextRetention, ContextScope, DependencyKind, RootReason,
    ScopeKind, SemanticState,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::{open_focus, verify_observation_for_recipe};

/// An engine whose context store directory can never be created: its parent
/// is a plain file, so every externalize write fails and spilled items stay
/// in the retry list.
fn blocked_store_engine() -> (SimpleContextEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 0, // every eviction overflows into the retry list
        context_store_dir: Some(blocker.join("store")),
        ..SimpleContextConfig::default()
    });
    (engine, dir)
}

async fn pending_decision(engine: &SimpleContextEngine, content: &str) -> ContextItemId {
    let mut state = engine.state.lock().await;
    let item = crate::item::make_item(
        &state,
        &engine.config,
        content.into(),
        ContextKind::Decision,
        ContextScope::Task,
        ContextRetention::Working,
        0.72,
        Some("user".into()),
    );
    let id = item.id;
    state.pending_externalize_retry.push(item);
    id
}

#[tokio::test]
async fn a_pending_decision_is_superseded_by_an_explicit_replacement() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "config format").await;
    let old_id = pending_decision(&engine, "use TOML for config").await;

    engine
        .ingest(ContextIngress::UserMessage {
            content: "switch to YAML instead of TOML".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let old = state
        .pending_externalize_retry
        .iter()
        .find(|item| item.id == old_id)
        .expect("the pending owner keeps the body");
    let replacement = state
        .items
        .iter()
        .find(|item| item.content == "switch to YAML instead of TOML")
        .expect("the replacing decision exists");
    assert_eq!(
        old.semantic,
        SemanticState::Superseded {
            by: Some(replacement.id)
        },
        "a pending decision must be superseded exactly like a resident one, naming the replacement: {report:?}"
    );
}

#[tokio::test]
async fn a_pending_error_is_verified_fixed_by_a_matching_probe() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix the flaky test").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "run the failing suite".into(),
        })
        .await
        .unwrap();
    super::harness::verify_failure_observation(
        &engine,
        "f1",
        "auth.tests failed: flaky assertion",
        "auth.tests",
    )
    .await;
    // The store outage spilled the recorded error: it now lives in the
    // retry list with its full body and recipe identity. (take_all /
    // replace_all is the documented wholesale-heap pattern; removing through
    // the raw vec would leave a stale slot index behind.)
    {
        let mut state = engine.state.lock().await;
        let mut items = state.items.take_all();
        let index = items
            .iter()
            .position(|item| item.kind == ContextKind::Error)
            .expect("the failure was recorded");
        let mut item = items.remove(index);
        item.residency = ContextResidency::Warm;
        state.pending_externalize_retry.push(item);
        state.items.replace_all(items);
    }

    verify_observation_for_recipe(&engine, "v1", "auth.tests passed", "auth.tests").await;
    let report = engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let error = state
        .pending_externalize_retry
        .iter()
        .find(|item| item.kind == ContextKind::Error)
        .expect("the pending error keeps its owner");
    assert!(
        matches!(error.semantic, SemanticState::VerifiedFixed { .. }),
        "a pending error must be verified-fixed by its own recipe's success: {report:?}"
    );
}

#[tokio::test]
async fn a_pending_body_satisfies_a_prompt_required_claim() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "evidence is required").await;
    let id = pending_decision(&engine, "the required constraint body").await;

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![AnchorRootClaim {
                    item_ref: id.to_string(),
                    strength: AnchorRootStrength::PromptRequired,
                    source_field_id: "working_refs".into(),
                    anchor_revision: 4,
                    reason: RootReason::HardConstraint,
                }],
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
            .all(|miss| miss.reason != ContextMaterializationMissReason::Missing),
        "a pending body in memory must not be reported Missing: {:?}",
        materialized.required_misses
    );
    assert!(
        materialized.items.iter().any(|entry| entry.item_id == id),
        "the required pending body must reach the frame: {:?}",
        materialized.items
    );
}

#[tokio::test]
async fn admit_returns_a_pending_item_to_the_working_set_under_its_own_id() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "admit from the outage").await;
    let id = pending_decision(&engine, "the durable decision body").await;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: id,
                reason: "needed for the current round".into(),
            },
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    assert!(
        state.items.iter().any(|item| item.id == id),
        "admit must re-enter the pending body under the same id"
    );
    assert!(
        state
            .pending_externalize_retry
            .iter()
            .all(|item| item.id != id),
        "the item left the retry list: it has a new owner"
    );
    let owners = usize::from(state.items.iter().any(|item| item.id == id))
        + usize::from(state.eviction_buffer.iter().any(|item| item.id == id))
        + usize::from(
            state
                .pending_externalize_retry
                .iter()
                .any(|item| item.id == id),
        )
        + usize::from(state.external.iter().any(|entry| entry.item_id == id));
    assert_eq!(owners, 1, "exactly one owner after admit");
}

#[tokio::test]
async fn derive_from_a_pending_source_mints_a_traced_fact() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "derive from the outage").await;
    let id = pending_decision(&engine, "the pending source body").await;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Derive {
                item_id: id,
                fact: "the derived fact".into(),
                reason: "summarize".into(),
            },
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let derived = state
        .items
        .iter()
        .find(|item| item.content == "the derived fact")
        .expect("the derive must actually execute against a pending source");
    assert!(
        derived
            .dependencies
            .iter()
            .any(|edge| edge.target == id && edge.kind == DependencyKind::DerivedFrom),
        "the derived fact must trace to the pending source id"
    );
}

#[tokio::test]
async fn a_lease_on_a_pending_item_is_real_and_counts_toward_the_quota() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        max_leased_items_per_task: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "lease across the outage").await;
    let id = pending_decision(&engine, "the leased pending body").await;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Lease {
                item_id: id,
                turns: 5,
            },
        })
        .await
        .unwrap();

    let task_of_pending = {
        let state = engine.state.lock().await;
        let item = state
            .pending_externalize_retry
            .iter()
            .find(|item| item.id == id)
            .expect("the pending item stays where it is");
        assert!(
            item.lease_until_turn.is_some(),
            "the lease must actually land on the pending body, not silently no-op"
        );
        item.task_id
    };

    // The quota must see the leased pending item: this task's cap of one is
    // already consumed.
    let second = {
        let state = engine.state.lock().await;
        crate::item::make_item(
            &state,
            &engine.config,
            "a second lease target".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            Some("test".into()),
        )
    };
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Lease {
                item_id: second.id,
                turns: 5,
            },
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert!(
        state
            .items
            .iter()
            .find(|item| item.id == second.id)
            .and_then(|item| item.lease_until_turn)
            .is_none(),
        "the second lease must be refused: the pending lease counts toward the task cap (task {task_of_pending:?})"
    );
}

#[tokio::test]
async fn closing_a_scope_promotes_a_pending_durable_outcome_in_place() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_id = open_focus(&engine, "episode with a pending outcome").await;
    let (focus_scope, item_scope, id) = {
        let mut state = engine.state.lock().await;
        let item = {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                "the durable outcome".into(),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Durable,
                0.7,
                Some("test".into()),
            );
            item.scope_id = state.active_scope_id;
            item
        };
        let id = item.id;
        let focus_scope = state.active_scope_id;
        state.pending_externalize_retry.push(item);
        (focus_scope, focus_scope, id)
    };
    assert_eq!(
        item_scope, focus_scope,
        "the item was created in the focus scope"
    );

    // Episode boundary: the runtime closes the focus scope; the pending
    // durable outcome must promote to the task scope in place.
    engine.close_scope(focus_scope.unwrap()).await.unwrap();

    let state = engine.state.lock().await;
    let item = state
        .pending_externalize_retry
        .iter()
        .find(|item| item.id == id)
        .expect("promotion never migrates the pending owner");
    assert_ne!(
        item.scope_id, focus_scope,
        "the pending outcome must be re-stamped out of the closed scope"
    );
    assert_eq!(
        item.scope_id
            .map(|sid| state.scopes.by_id(sid).map(|s| s.kind)),
        Some(Some(ScopeKind::Task)),
        "the promotion target is the task scope of task {task_id:?}"
    );
    assert_eq!(item.retention, ContextRetention::Durable);
}

#[tokio::test]
async fn checkpoint_roundtrip_keeps_exactly_one_pending_owner() {
    let (engine, _dir) = blocked_store_engine();
    open_focus(&engine, "outage across a checkpoint").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work while the store is down".into(),
        })
        .await
        .unwrap();
    super::harness::tool_observation(&engine, "t1", "step done while the store is down").await;
    // Consume the observation (AfterModel archives it), so the next full GC
    // evicts it; the overflow cannot be written (blocked store), so the
    // retry list owns it.
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            !state.pending_externalize_retry.is_empty(),
            "the blocked store must leave the overflow in the retry list"
        );
    }

    let checkpoint = engine.checkpoint().await.unwrap();
    engine.restore(checkpoint).await.unwrap();

    let state = engine.state.lock().await;
    let mut owners = 0usize;
    for item in state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        .chain(state.pending_externalize_retry.iter())
    {
        if item.content == "work while the store is down" {
            owners += 1;
        }
    }
    assert_eq!(
        owners, 1,
        "the checkpoint roundtrip must keep exactly one owner for the pending body"
    );
}

#[tokio::test]
async fn draining_after_recovery_lands_the_current_semantics_not_stale_ones() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "recovery lands truth").await;
    // Two pending items: one still live, one superseded while the store was
    // down (its pending owner carries the terminal state).
    let live_id = pending_decision(&engine, "still valid after recovery").await;
    let dead_id = pending_decision(&engine, "withdrawn while the store was down").await;
    {
        let mut state = engine.state.lock().await;
        let item = state
            .pending_externalize_retry
            .iter_mut()
            .find(|item| item.id == dead_id)
            .unwrap();
        item.semantic = SemanticState::Superseded { by: None };
    }

    // The disk is writable again: the next pass externalizes both.
    engine.gc().await.unwrap();

    let state = engine.state.lock().await;
    assert!(
        state
            .pending_externalize_retry
            .iter()
            .all(|item| item.id != live_id && item.id != dead_id),
        "both items drained out of the retry list"
    );
    let live = state
        .external
        .get(live_id)
        .expect("the live item landed in the external map");
    assert!(live.semantic.is_live(), "live semantics are preserved");
    let dead = state
        .external
        .get(dead_id)
        .expect("the withdrawn item's entry exists (evidence, not retrieval)");
    assert!(
        dead.semantic.is_dead(),
        "the terminal state reached during the outage must NOT be reverted by externalization"
    );
    assert!(
        !crate::store::externally_retrievable(dead),
        "a dead entry is never served back to the model"
    );
    let _ = live_id;
}
