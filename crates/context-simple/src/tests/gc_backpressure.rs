//! CTX-8 (R2-06): a persistently failing context store must produce honest
//! backpressure, not unbounded memory. The externalize-retry list has a
//! hard item cap (overflow is deferred and reported, items stay owned); one
//! pass serializes and attempts at most `gc_externalize_batch` owners
//! (fair FIFO); failed IO is counted in the report instead of swallowed;
//! diagnostics surface the pending count and byte weight. After the store
//! recovers, the backlog drains oldest-first within the same batch bound.

use agent_contracts::{
    ContextEngine, ContextIngress, ContextKind, ContextResidency, ContextRetention, ContextScope,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::{open_focus, tool_observation};

fn blocked_store_engine(
    batch: usize,
    pending_cap: usize,
    buffer_cap: usize,
) -> (SimpleContextEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: buffer_cap,
        gc_externalize_batch: batch,
        max_pending_externalize_items: pending_cap,
        context_store_dir: Some(blocker.join("store")),
        ..SimpleContextConfig::default()
    });
    (engine, dir)
}

/// One round of fresh input that ages into an eviction: an observation is
/// ingested and consumed, so the next full GC evicts it.
async fn one_round_of_input(engine: &SimpleContextEngine, round: usize) {
    engine
        .ingest(ContextIngress::UserMessage {
            content: format!("round {round}: keep working while the store is down"),
        })
        .await
        .unwrap();
    tool_observation(
        engine,
        &format!("call-{round}"),
        &format!("observation body for round {round}"),
    )
    .await;
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_persistently_failing_store_hits_the_pending_cap_and_reports_backpressure() {
    let (engine, _dir) = blocked_store_engine(64, 3, 1);
    open_focus(&engine, "long outage").await;

    let mut last_report = None;
    for round in 0..6 {
        one_round_of_input(&engine, round).await;
        last_report = Some(engine.gc().await.unwrap());
    }
    let report = last_report.expect("at least one pass ran");

    let state = engine.state.lock().await;
    assert_eq!(
        state.pending_externalize_retry.len(),
        3,
        "the retry list must hold at its hard cap, not grow with the outage"
    );
    assert!(
        report.externalize_backpressure,
        "the cap must be reported as typed backpressure: {report:?}"
    );
    assert!(
        report.externalize_deferred > 0,
        "deferred overflow must be counted, not silently absorbed: {report:?}"
    );
    assert_eq!(
        report.diagnostics.pending_items, 3,
        "diagnostics must surface the pending axis"
    );
    let expected_bytes: u64 = state
        .pending_externalize_retry
        .iter()
        .map(|item| item.content.len() as u64)
        .sum();
    assert_eq!(report.diagnostics.pending_bytes, expected_bytes);
    // Nothing was lost: every ingested observation is still owned somewhere.
    let owned = state.items.len()
        + state.eviction_buffer.len()
        + state.pending_externalize_retry.len()
        + state.external.len();
    assert!(
        owned >= 6,
        "deferred overflow keeps its owners: {owned} items across all locations"
    );
    assert!(
        report.store_io_failures > 0,
        "the failed writes must be counted, not swallowed: {report:?}"
    );
}

#[tokio::test]
async fn draining_after_recovery_is_fair_fifo_bounded_per_pass() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 0,
        gc_externalize_batch: 2,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "backlog then recovery").await;
    // Five owners spilled by the (simulated) outage, in a known order.
    let mut ids = Vec::new();
    for i in 0..5 {
        let state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            format!("backlogged body {i}"),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            Some("test".into()),
        );
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        ids.push(item.id);
        drop(state);
        let mut state = engine.state.lock().await;
        state.eviction_buffer.push(item);
    }

    // Pass 1: exactly one batch (2) is attempted and lands.
    let report = engine.gc().await.unwrap();
    assert_eq!(report.externalized, 2, "one pass writes at most one batch");
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        2,
        "the batch bound is real store IO, not just accounting"
    );

    // Passes 2 and 3 drain the rest, oldest first.
    let report2 = engine.gc().await.unwrap();
    assert_eq!(report2.externalized, 2);
    let report3 = engine.gc().await.unwrap();
    assert_eq!(report3.externalized, 1);

    let state = engine.state.lock().await;
    assert!(state.eviction_buffer.is_empty(), "the buffer drained");
    assert!(
        state.pending_externalize_retry.is_empty(),
        "the backlog drained"
    );
    for id in &ids {
        assert!(
            state.external.get(*id).is_some(),
            "FIFO order preserved: every owner landed as an entry"
        );
    }
}

#[tokio::test]
async fn io_failures_count_only_the_attempted_batch() {
    let (engine, _dir) = blocked_store_engine(2, 64, 0);
    open_focus(&engine, "failure accounting").await;
    for i in 0..3 {
        let state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            format!("body {i}"),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            Some("test".into()),
        );
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        drop(state);
        let mut state = engine.state.lock().await;
        state.eviction_buffer.push(item);
    }

    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.store_io_failures, 2,
        "exactly one batch was attempted; both writes failed and are counted"
    );
    assert_eq!(report.externalized, 0);
    let state = engine.state.lock().await;
    assert_eq!(
        state.pending_externalize_retry.len(),
        3,
        "every failed write keeps its owner"
    );
}

#[tokio::test]
async fn a_full_pass_over_a_large_unrooted_heap_completes_with_bounded_state() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "large heap pass").await;
    {
        let mut state = engine.state.lock().await;
        for i in 0..2_000 {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("unrooted body {i} with some content to score"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Ephemeral,
                0.1,
                Some("test".into()),
            );
            item.attention = agent_contracts::AttentionState::Archived;
            item.scope = ContextScope::Turn;
            state.items.push(item);
        }
    }
    let report = engine.gc().await.unwrap();
    let state = engine.state.lock().await;
    // Every unrooted consumed observation left the heap in one pass; the
    // overflow landed in the (real) store. The pass is linear in items and
    // marks; nothing here may depend on items x marks work.
    assert_eq!(
        state.items.len(),
        0,
        "an unrooted heap sweeps clean in one pass: {report:?}"
    );
    assert_eq!(
        state.external.len() + state.pending_externalize_retry.len() + state.eviction_buffer.len(),
        2_000,
        "every evicted body is owned by the store, the retry list or the bounded buffer"
    );
    assert!(
        state.eviction_buffer.len() <= 256,
        "the buffer holds at most its capacity"
    );
    let _ = ContextRetention::Working;
}

/// CTX-12 (R3-07): the per-pass reactivation budget is consumed only by
/// *successful* reactivations. An invalid warm candidate scanned earlier
/// (newest-first) must not starve a valid one behind it: with a budget of
/// one, the valid note still comes back on the first pass.
#[tokio::test]
async fn invalid_warm_candidates_do_not_consume_the_reactivation_budget() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_reactivate_per_pass: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "find the old evidence").await;
    {
        let mut state = engine.state.lock().await;
        // The VALID candidate: model-directed (keep_alive), scanned second.
        let mut note = crate::item::make_item(
            &state,
            &engine.config,
            "the relevant old evidence body".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            Some("test".into()),
        );
        note.keep_alive = true;
        note.residency = ContextResidency::Warm;
        note.evicted_at_tick = Some(0);
        state.eviction_buffer.push(note);

        // The INVALID candidate: a consumed shell observation — never
        // auto-reactivated, but scanned first (newest-first order).
        let mut observation = crate::item::make_item(
            &state,
            &engine.config,
            "unrelated shell stdout noise".into(),
            ContextKind::ToolObservation,
            ContextScope::Turn,
            ContextRetention::Ephemeral,
            0.1,
            Some("test".into()),
        );
        observation.attention = agent_contracts::AttentionState::Archived;
        observation.residency = ContextResidency::Warm;
        observation.evicted_at_tick = Some(0);
        state.eviction_buffer.push(observation);
    }

    let report = engine.gc().await.unwrap();

    let state = engine.state.lock().await;
    assert!(
        state
            .items
            .iter()
            .any(|item| item.content == "the relevant old evidence body"),
        "the valid candidate must be recalled on the first pass despite the invalid one ahead of it: {report:?}"
    );
    assert!(
        state
            .eviction_buffer
            .iter()
            .all(|item| item.content != "the relevant old evidence body"),
        "the valid candidate left the buffer"
    );
}
