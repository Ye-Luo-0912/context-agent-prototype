use agent_contracts::{
    ContextEngine, ContextHints, ContextIngress, ContextKind, ContextMaintenanceTrigger,
    ContextQuery,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

/// Long-task acceptance (`long_task_10k_turns`): over 10,000 task turns the
/// resident working set is bounded by the current episode plus unresolved
/// semantic state, not by turn count. Required decisions stay recallable;
/// stale ordinary dialogue leaves Resident.
#[tokio::test]
async fn long_task_10k_turns_keeps_the_working_set_episode_bounded() {
    let store = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Test-only semantic boundary: consecutive per-turn messages from
        // different workstreams share almost no tokens, so the episode
        // rotates on the semantic signal (the default threshold is
        // deliberately more conservative).
        episode_rotate_threshold: 0.35,
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "maintain the auth service").await;

    // Turn 0: a durable decision the task must keep recalling.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "0", "reviewed AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    // Most turns come from different workstreams (semantic rotation); the
    // final burst is a stream of related messages (turn-budget rotation).
    const WORKSTREAMS: &[&str] = &[
        "fix the auth cache invalidation",
        "refactor retry backoff for shards",
        "add request tracing to the gateway",
        "tune connection pool sizing",
        "investigate the token bucket throttle",
        "rework circuit breaker thresholds",
        "profile event bus dispatch latency",
        "harden the input validation path",
        "reduce index rebuild cost",
        "document the deployment runbook",
    ];
    let mut max_resident = 0usize;
    let mut max_resident_bytes = 0usize;
    let mut early_ordinary_id = None;
    let mut resident_at_2000 = 0usize;
    let mut resident_bytes_at_2000 = 0usize;
    for turn in 1..=10_000u64 {
        let content = if turn <= 9_000 {
            format!(
                "{} in round {}",
                WORKSTREAMS[turn as usize % WORKSTREAMS.len()],
                turn
            )
        } else {
            // Related messages: the semantic signal never fires, so the
            // episode must rotate on the 500-turn budget instead.
            format!("keep working on the auth cache and the retry backoff in round {turn}")
        };
        engine
            .ingest(ContextIngress::UserMessage { content })
            .await
            .unwrap();
        tool_observation(
            &engine,
            &turn.to_string(),
            &format!("patched Item{}", turn % 13),
        )
        .await;
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        if turn % 50 == 0 {
            engine.gc().await.unwrap();
            let state = engine.state.lock().await;
            let resident = state.items.len();
            let resident_bytes: usize = state.items.iter().map(|item| item.content.len()).sum();
            drop(state);
            max_resident = max_resident.max(resident);
            max_resident_bytes = max_resident_bytes.max(resident_bytes);
            if turn == 2_000 {
                resident_at_2000 = resident;
                resident_bytes_at_2000 = resident_bytes;
            }
        }
        // Record the turn-100 ordinary message while it is still resident
        // (it is evicted by the very next episode rotation).
        if turn == 100 {
            early_ordinary_id = engine
                .state
                .lock()
                .await
                .items
                .iter()
                .find(|item| item.kind == ContextKind::UserMessage && item.created_turn == 100)
                .map(|item| item.id);
        }
    }

    // 1. Bounded working set: without episode rotation 10,000 turns would
    // leave ~20,000 resident items; rotation keeps the peak to the current
    // episode plus hot recalls (the 500-turn budget burst is dominated by
    // GC and never accumulates).
    assert!(
        max_resident < 200,
        "resident working set must stay bounded, peak was {max_resident}"
    );
    // 2. Bounded *over time*: the working set must not grow with turn
    // count. A linear-growth engine would show a large delta between turn
    // 2,000 and turn 10,000.
    let resident_at_10000 = engine.state.lock().await.items.len();
    assert!(
        resident_at_10000 <= resident_at_2000.saturating_add(20),
        "the working set must not grow with turn count: {resident_at_2000} -> {resident_at_10000}"
    );
    // 3. Resident *bytes* flatten too: a smaller item count must not hide
    // a growing heap. Same 20% growth allowance as the count check, plus
    // a small absolute slack for variable message length.
    let resident_bytes_at_10000: usize = engine
        .state
        .lock()
        .await
        .items
        .iter()
        .map(|item| item.content.len())
        .sum();
    assert!(
        max_resident_bytes < 80_000,
        "resident heap bytes must stay bounded, peak was {max_resident_bytes}"
    );
    let byte_slack = resident_bytes_at_2000 / 5 + 4_096;
    assert!(
        resident_bytes_at_10000 <= resident_bytes_at_2000.saturating_add(byte_slack),
        "resident bytes must not grow with turn count: {resident_bytes_at_2000} -> {resident_bytes_at_10000}"
    );

    // 2. Stale ordinary dialogue leaves Resident.
    let early = early_ordinary_id.expect("an early ordinary message id");
    {
        let state = engine.state.lock().await;
        assert!(
            !state.items.iter().any(|item| item.id == early),
            "stale ordinary dialogue must leave the resident heap"
        );
    }

    // 3. The required decision stays recallable: touch its entity, then
    // materialize and expect it back in the working set.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we decide about AuthService.rs?".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "final", "touched AuthService.rs again").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "what did we decide about AuthService.rs?".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.kind == ContextKind::UserMessage
                && item.content.contains("AuthService.rs")),
        "the required decision must stay recallable, selected: {:?}",
        materialized
            .items
            .iter()
            .map(|item| &item.content)
            .collect::<Vec<_>>()
    );
}

/// Cadence regression for the episode-local turn budget: one overlong
/// episode that rotates on the `episode_max_user_turns` guard must not
/// permanently exhaust every later episode's budget. The rotation resets
/// the counter, so the next episode's related messages do not rotate until
/// their own turn budget is exhausted — without the reset the guard fires
/// on the very next user message and rotates a fresh single-turn episode.
#[tokio::test]
async fn one_overlong_episode_does_not_exhaust_later_episode_budgets() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Related messages never fire the semantic signal (threshold 0
        // means overlap can never fall below it), so only the turn budget
        // can rotate the episode.
        episode_rotate_threshold: 0.0,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "keep working on the auth service").await;

    // Drive the episode past its turn budget. `FocusState.generation` is
    // bumped once by `FocusChanged`, so the guard fires at turn `max_turns`
    // (the budget itself): the counter reaches the cap on the last
    // in-budget message and the next message observes it.
    let max_turns = SimpleContextConfig::default().episode_max_user_turns as u64;
    let mut rotated_at: Option<u64> = None;
    for turn in 1..=max_turns + 1 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("keep working on the auth cache in round {turn}"),
            })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        if report
            .transitions
            .iter()
            .any(|t| t.reason.contains("episode rotated"))
            && rotated_at.is_none()
        {
            rotated_at = Some(turn);
        }
    }
    // The overlong episode survives its full budget: the guard fires at
    // the budget boundary, not immediately.
    let rotated_at = rotated_at.expect("the turn-budget guard must rotate the overlong episode");
    assert!(
        rotated_at >= max_turns.saturating_sub(1),
        "the episode must survive its full turn budget before rotating, rotated at turn {rotated_at}"
    );

    // A fresh episode starts with a reset budget: five related messages
    // must not rotate again (a rotation would evict this episode's
    // ordinary dialogue), and the dialogue stays resident.
    let mut resident_turns = Vec::new();
    for turn in 1..=5u64 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("keep working on the auth cache in round {turn}"),
            })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        assert!(
            !report
                .transitions
                .iter()
                .any(|t| t.reason.contains("episode rotated")),
            "a fresh episode must not rotate on the exhausted-budget guard (round {turn})"
        );
        resident_turns = engine
            .state
            .lock()
            .await
            .items
            .iter()
            .filter(|item| item.kind == ContextKind::UserMessage)
            .map(|item| item.created_turn)
            .collect();
    }
    assert!(
        resident_turns.len() >= 5,
        "the fresh episode's ordinary dialogue must stay resident, got {resident_turns:?}"
    );
}
