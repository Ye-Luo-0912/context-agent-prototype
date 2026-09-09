//! Resource bounds at allocation time: report rows are capped with omitted
//! counters, failed overflow writes spill into a retry list instead of
//! growing the warm buffer past its cap, and the external view stops
//! collecting once the 32-row limit is reached.
//!
//! Cancellation and durability of the externalize/recall pipeline live here
//! too: a dropped IO phase never loses items (ownership stays in the retry
//! list), and a recalled blob is not deleted until the reconcile reclaims
//! it against the live state.

use agent_contracts::{
    ContextEngine, ContextIngress, ContextKind, ContextQuery, ContextRetention, ContextScope,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine, State, truncate_report_rows};

use super::harness::*;

#[test]
fn report_rows_are_capped_with_an_omitted_count() {
    let mut rows = Vec::new();
    for i in 0..10 {
        rows.push(format!("row {i}"));
    }
    let omitted = truncate_report_rows(&mut rows, 4);
    assert_eq!(omitted, 6);
    assert_eq!(rows.len(), 4, "the explainable rows stay within the budget");
    let none_omitted = truncate_report_rows(&mut rows, 4);
    assert_eq!(none_omitted, 0);
}

/// A store outage must not grow the warm buffer past its cap: failed
/// overflow writes spill into the retry list (content preserved) and the
/// next pass retries them without re-bloating the buffer.
#[tokio::test]
async fn failed_externalization_spills_into_retry_not_into_the_buffer() {
    // A regular *file* where the store directory should be: every blob write
    // fails with a filesystem error, the persistent-store-failure case.
    let dir = tempfile::tempdir().unwrap();
    let store_broken = dir.path().join("store");
    std::fs::write(&store_broken, b"not a directory").unwrap();

    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 2,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(store_broken),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "flood").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fill the working set".into(),
        })
        .await
        .unwrap();
    for i in 0..5 {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: observation_touching(
                    &format!("step-{i}"),
                    true,
                    &format!("step {i}: nothing to see"),
                    None,
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let first = engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.len() <= 2,
            "the warm buffer must stay within its cap: {} > 2",
            state.eviction_buffer.len()
        );
        assert!(
            !state.pending_externalize_retry.is_empty(),
            "the failed writes spill into the retry list"
        );
    }
    assert!(first.externalized == 0, "no write can succeed: {first:?}");

    // Another pass while the store is still down: the retry is attempted,
    // fails again, and the buffer still never exceeds its cap.
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let second = engine.gc().await.unwrap();
    assert_eq!(second.externalized, 0);
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.len() <= 2,
            "repeated failure must not re-bloat the buffer"
        );
        assert_eq!(
            state.pending_externalize_retry.len(),
            first.evicted.saturating_sub(2),
            "the spill keeps every failed overflow item"
        );
    }
}

/// The externalize pipeline is cancellation-safe: the plan carries only
/// ids and pre-serialized bytes, while the items stay in
/// `pending_externalize_retry` until the commit applies a successful
/// write. Dropping the IO phase (a cancelled future, a panicked join)
/// abandons writes — never content.
#[test]
fn dropped_externalize_plan_keeps_items_owned_by_the_state() {
    let config = SimpleContextConfig {
        gc_enabled: true,
        gc_buffer_capacity: 1,
        ..SimpleContextConfig::default()
    };
    let mut state = State::default();
    let working = |state: &State, content: &str| {
        crate::item::make_item(
            state,
            &config,
            content.into(),
            ContextKind::ToolObservation,
            ContextScope::Session,
            ContextRetention::Working,
            0.5,
            Some("tool:shell.exec".into()),
        )
    };
    let spill = working(&state, "spilled by the previous failed pass");
    let overflow_a = working(&state, "overflow beyond the buffer cap: a");
    let overflow_b = working(&state, "overflow beyond the buffer cap: b");
    let spill_id = spill.id;
    let overflow_a_id = overflow_a.id;
    let overflow_b_id = overflow_b.id;
    state.pending_externalize_retry.push(spill);
    state.eviction_buffer.push(overflow_a);
    state.eviction_buffer.push(overflow_b);

    let plan = crate::gc::full::plan_full_gc(&mut state, &config, 1, 1)
        .expect("a pass is due: the buffer holds content");
    // Cap 1 with 2 buffered items: one overflows (the oldest), joining the
    // prior spill — both are planned, both stay owned by the retry list.
    assert_eq!(
        plan.externalize.len(),
        2,
        "spill + one overflow are planned"
    );
    assert_eq!(
        state.eviction_buffer.len(),
        1,
        "the plan brings the buffer back to its cap"
    );
    assert!(
        state
            .eviction_buffer
            .iter()
            .any(|item| item.id == overflow_b_id)
    );
    assert_eq!(
        state.pending_externalize_retry.len(),
        2,
        "ownership never leaves the state during the IO window"
    );

    // Cancelled mid-IO: the plan is dropped without store writes or commit.
    drop(plan);
    assert_eq!(
        state.pending_externalize_retry.len(),
        2,
        "a dropped plan loses nothing"
    );

    // The next pass plans again; at commit, only the writes that actually
    // landed leave the retry list.
    let plan = crate::gc::full::plan_full_gc(&mut state, &config, 2, 2)
        .expect("the retry list keeps the pass due");
    let io = crate::gc::full::GcIoResult {
        externalized: vec![(spill_id, "checksum".to_string())],
        recalled: Vec::new(),
    };
    let report = crate::gc::full::commit_full_gc(&mut state, 2, plan, io);
    assert_eq!(report.externalized, 1);
    assert!(
        state.external.get(spill_id).is_some(),
        "the landed write joined the external map"
    );
    assert!(
        state.external.get(overflow_a_id).is_none(),
        "writes that did not land must not join the map"
    );
    assert_eq!(
        state.pending_externalize_retry.len(),
        1,
        "the unwritten overflow stays for the next pass"
    );
    assert!(state.pending_externalize_retry[0].id == overflow_a_id);
}

/// R02 (liveness): when only the externalize-retry list holds content — every
/// owner spilled by a store outage, with the heap, the warm buffer and the
/// external map all empty — the maintenance pass must still run. Otherwise
/// the retry the outage postponed never happens, even after IO recovers.
#[test]
fn a_pending_only_state_still_runs_the_pass_and_lands_the_retry() {
    let config = SimpleContextConfig {
        gc_enabled: true,
        gc_buffer_capacity: 1,
        ..SimpleContextConfig::default()
    };
    let mut state = State::default();
    let owner = {
        let mut item = crate::item::make_item(
            &state,
            &config,
            "spilled by the failed pass".into(),
            ContextKind::ToolObservation,
            ContextScope::Session,
            ContextRetention::Working,
            0.5,
            Some("tool:shell.exec".into()),
        );
        item.dependencies.push(agent_contracts::DependencyEdge {
            target: item.id,
            kind: agent_contracts::DependencyKind::SharesEntities,
        });
        item
    };
    let owner_id = owner.id;
    state.pending_externalize_retry.push(owner);
    assert!(state.items.is_empty());
    assert!(state.eviction_buffer.is_empty());
    assert!(
        state.external.is_empty(),
        "the precondition: only pending holds content"
    );

    let plan = crate::gc::full::plan_full_gc(&mut state, &config, 1, 1)
        .expect("the pending retry list keeps the maintenance pass due");
    assert_eq!(
        plan.externalize.len(),
        1,
        "the retry list owner is planned for its store write again"
    );

    // IO recovered: the write lands and the commit hands ownership to the
    // external map; the retry list drains.
    let io = crate::gc::full::GcIoResult {
        externalized: vec![(owner_id, "checksum".to_string())],
        recalled: Vec::new(),
    };
    let report = crate::gc::full::commit_full_gc(&mut state, 1, plan, io);
    assert_eq!(report.externalized, 1);
    assert!(
        state.external.get(owner_id).is_some(),
        "the landed write joined the external map"
    );
    assert!(
        state.pending_externalize_retry.is_empty(),
        "the landed write leaves the retry list"
    );
}

/// R02 end to end: a real store outage spills a live owner into the retry
/// list; the pending owner stays visible in the logical catalog (inspect)
/// and in checkpoints, and once the store recovers the next maintenance pass
/// retries the write successfully.
#[tokio::test]
async fn pending_owners_retry_after_the_store_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("store");
    // A regular file where the store directory should be: every blob write
    // fails with a filesystem error — the persistent outage.
    std::fs::write(&store_path, b"not a directory").unwrap();

    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 0,
        context_store_dir: Some(store_path.clone()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "outage").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work under outage".into(),
        })
        .await
        .unwrap();
    // A consumed ephemeral observation leaves attention after the turn, so
    // the GC pass evicts it; buffer capacity 0 spills it straight into the
    // retry list, where the outage kills the write.
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: observation_touching("obs-1", true, "observation under outage", None),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let first = engine.gc().await.unwrap();
    assert_eq!(
        first.externalized, 0,
        "no write can succeed while the store is down: {first:?}"
    );
    let pending_id = {
        let state = engine.state.lock().await;
        assert!(
            !state.pending_externalize_retry.is_empty(),
            "the outage spills the owner into the retry list"
        );
        state.pending_externalize_retry[0].id
    };

    // Catalog visibility: the pending owner is a logical-catalog member —
    // the model can still see (and fetch) the record whose write is pending.
    let summaries = engine.inspect(10).await.unwrap();
    assert!(
        summaries.iter().any(|summary| summary.id == pending_id),
        "the pending owner must stay visible in the logical catalog: {summaries:?}"
    );
    let fetched = engine.fetch_external(pending_id).await.unwrap();
    assert!(
        fetched.is_some(),
        "the pending owner's in-memory body must be fetchable"
    );

    // A checkpoint taken mid-outage restores with the pending owner intact
    // (ownership + catalog validation must accept the retry list).
    let snapshot = engine.checkpoint().await.unwrap();
    engine.restore(snapshot).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_externalize_retry.len(),
            1,
            "the restored checkpoint keeps the pending owner"
        );
    }

    // The store recovers: the blocking file becomes the directory the
    // writes always wanted, and the next pass retries successfully.
    std::fs::remove_file(&store_path).unwrap();
    std::fs::create_dir(&store_path).unwrap();
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let second = engine.gc().await.unwrap();
    assert!(
        second.externalized >= 1,
        "the recovered store drains the retry list: {second:?}"
    );
    {
        let state = engine.state.lock().await;
        assert!(
            state.pending_externalize_retry.is_empty(),
            "the retried write must leave the retry list: {:?}",
            state.pending_externalize_retry
        );
        assert!(
            state.external.get(pending_id).is_some(),
            "the retried write joined the external map"
        );
    }
}

/// A recalled blob is not deleted at recall time: the in-memory commit is
/// not a persistence barrier, and the newest durable checkpoint may still
/// reference the blob through the external entry this pass just removed.
/// The blob survives until the reconcile reclaims it as a stale duplicate
/// against the live state.
#[tokio::test]
async fn recalled_blob_survives_until_the_reconcile_reclaims_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;

    // One cold entry with a hot entity, its blob actually on disk. A Note
    // (not raw evidence) so the recall is not subject to the raw-evidence
    // reactivation guard.
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "decision: cache the auth token behind AuthService".into(),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.5,
            Some("derived".into()),
        );
        item.entities = vec!["auth-cache".into()];
        item.scope_id = None;
        let context_ref = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item,
            context_ref,
            1,
            1,
            None,
        ));
        state.user_hot_entities.push("auth-cache".into());
        state.rebuild_hot_entities();
        item.id
    };

    // The durable checkpoint that still references the blob.
    let before_recall = engine.checkpoint().await.unwrap();

    let report = engine.gc().await.unwrap();
    assert!(
        report.store_recalled_items >= 1,
        "the hot entity must recall the cold entry: {report:?}"
    );
    assert!(
        engine
            .state
            .lock()
            .await
            .items
            .iter()
            .any(|item| item.id == item_id),
        "the recalled item must be resident again"
    );
    let blob = dir.path().join(format!("{item_id}.json"));
    assert!(
        blob.exists(),
        "the recalled blob must survive the recall commit — the newest \
         durable checkpoint may still reference it"
    );

    // Crash replay: restoring the pre-recall checkpoint must still find the
    // blob readable through the restored external entry.
    engine.restore(before_recall).await.unwrap();
    assert!(
        engine.fetch_external(item_id).await.unwrap().is_some(),
        "the restored checkpoint's external entry must still be readable"
    );

    // Only the reconcile, running against the live state, reclaims the
    // leftover duplicate.
    let _ = engine.gc().await.unwrap();
    let reconcile = engine.reconcile_store().await.unwrap();
    assert!(
        reconcile.deleted_stale >= 1,
        "the reconcile reclaims the stale duplicate: {reconcile:?}"
    );
    assert!(
        !blob.exists(),
        "the stale blob is reclaimed once the live state no longer references it"
    );
}

/// One hot entity with a huge bucket must not stage every descriptor before
/// the 32-row view limit cuts in.
#[tokio::test]
async fn external_view_stops_collecting_at_the_row_limit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "hot bucket").await;
    {
        let mut state = engine.state.lock().await;
        for i in 0..2_000 {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("ref {i} under one hot entity"),
                agent_contracts::ContextKind::ToolObservation,
                agent_contracts::ContextScope::Session,
                agent_contracts::ContextRetention::Working,
                0.5,
                Some("tool:shell.exec".into()),
            );
            item.entities = vec!["hot-bucket".into()];
            item.scope_id = None;
            let context_ref = crate::store::make_context_ref(&item);
            state.external.push(crate::store::to_external_entry(
                &item,
                context_ref,
                1,
                1,
                None,
            ));
        }
    }
    engine
        .ingest(ContextIngress::UserMessage {
            content: "hot-bucket".into(),
        })
        .await
        .unwrap();
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "hot-bucket".into(),
            budget_tokens: 10_000,
            hints: agent_contracts::ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized.external.len() <= 32,
        "the external view must not stage the whole bucket: {} rows",
        materialized.external.len()
    );
    assert!(
        (1..=32).contains(&materialized.external.len()),
        "the external view must stay within the 32-row surface without staging the bucket: {} rows",
        materialized.external.len()
    );
}
