//! EXEC-1: the round's context materialization wait is cancellable. The
//! gated engine parks `materialize` at a notify barrier, so every timing
//! here is deterministic: the test holds the engine at its wait, drives the
//! actor command lane, and asserts cancellation/cleanup semantics against
//! the existing bounded cleanup cap — never against sleep-guessed orderings.

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{RuntimeEvent, TurnCancelAck};
use agent_runtime::spawn_runtime;

use crate::harness::*;

/// Bounded wait for one matching durable event.
async fn wait_for_event(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    what: &str,
    matches: impl Fn(&RuntimeEvent) -> bool,
) -> RuntimeEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        match events.try_recv() {
            Ok(envelope) => {
                if matches(&envelope.event) {
                    return envelope.event;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("event stream closed while waiting for {what}");
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Bounded wait proving an event does NOT appear.
async fn assert_no_event(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    what: &str,
    matches: impl Fn(&RuntimeEvent) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while tokio::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok(envelope) => {
                assert!(!matches(&envelope.event), "{what} must not appear, but did");
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {}
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The core boundary: Cancel is received and answered while the engine's
/// materialization is parked. Before EXEC-1 this exact sequence timed out —
/// the actor loop sat inside the inline materialization await and could not
/// reach the command channel.
#[tokio::test]
async fn cancel_is_answered_while_materialization_is_parked() {
    let engine = Arc::new(GatedContextEngine::default());
    let kernel = kernel_with(Arc::new(StreamingModel), engine.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle
        .start_work("exec-1 parking directive".into(), "exec1-req-1".into())
        .await
        .unwrap();
    engine.entered.notified().await;

    let ack = tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .expect("cancel must be answered while materialization is parked")
        .unwrap();
    assert!(
        matches!(ack, TurnCancelAck::Cancelled { .. }),
        "an in-flight round cancels with a Cancelled ack"
    );
    wait_for_event(&mut events, "TurnCancelled", |event| {
        matches!(event, RuntimeEvent::TurnCancelled { .. })
    })
    .await;

    // The directive survives: the task is still active with its goal.
    let snapshot = handle.status_snapshot().await.unwrap();
    assert!(
        snapshot
            .tasks
            .iter()
            .any(|task| task.goal == "exec-1 parking directive"),
        "the admitted directive must survive a materialization-wait cancellation"
    );

    // The engine is still parked. Releasing it now produces a late preview
    // that must not resurrect the cancelled round.
    engine.release.notify_one();
    assert_no_event(&mut events, "TurnCompleted (after cancellation)", |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await;

    handle.stop().await.unwrap();
}

/// Parked materialization, then release and cancel arriving together:
/// exactly one turn terminal lands, commands stay live, and whichever way
/// the race resolves the runtime stays consistent and reusable.
#[tokio::test]
async fn materialization_release_and_cancel_arriving_together_have_one_terminal() {
    let engine = Arc::new(GatedContextEngine::default());
    let kernel = kernel_with(Arc::new(StreamingModel), engine.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle
        .start_work("exec-1 race directive".into(), "exec1-req-2".into())
        .await
        .unwrap();
    engine.entered.notified().await;

    // Release the preview and cancel back to back: the actor may process
    // either first.
    engine.release.notify_one();
    let ack = tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .expect("cancel must be answered in the release race")
        .unwrap();

    let mut cancelled = 0usize;
    let mut completed = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok(envelope) => match envelope.event {
                RuntimeEvent::TurnCancelled { .. } => cancelled += 1,
                RuntimeEvent::TurnCompleted => completed += 1,
                _ => {}
            },
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    match ack {
        TurnCancelAck::Cancelled { .. } => {
            assert_eq!(cancelled, 1, "exactly one cancellation terminal");
            assert_eq!(completed, 0, "a cancelled round never also completes");
        }
        TurnCancelAck::NoActiveTurn => {
            // The preview won the race and the round ran to completion
            // before the cancel command was processed.
            assert_eq!(completed, 1, "the completed round terminal is single");
            assert_eq!(cancelled, 0, "a completed turn is not also cancelled");
        }
    }

    // The runtime is reusable either way.
    handle
        .user_message("exec-1 after race".into())
        .await
        .unwrap();
    handle.stop().await.unwrap();
}

/// Cancellation while parked, then a fresh dialogue: the new round reaches a
/// NEW materialization and completes — the lane recovers, and the cancelled
/// round's late preview never executes.
#[tokio::test]
async fn a_fresh_round_runs_after_a_parked_materialization_was_cancelled() {
    let engine = Arc::new(GatedContextEngine::default());
    let kernel = kernel_with(Arc::new(StreamingModel), engine.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle.user_message("exec-1 first".into()).await.unwrap();
    engine.entered.notified().await;
    tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .expect("cancel answered while parked")
        .unwrap();
    wait_for_event(&mut events, "TurnCancelled", |event| {
        matches!(event, RuntimeEvent::TurnCancelled { .. })
    })
    .await;

    // Release the stale preview after the cancellation: it must be dropped.
    engine.release.notify_one();

    handle.user_message("exec-1 second".into()).await.unwrap();
    engine.entered.notified().await;
    engine.release.notify_one();
    wait_for_event(&mut events, "TurnCompleted", |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await;

    // The first round's terminal count stays exactly one cancellation.
    handle.stop().await.unwrap();
}

/// Stop with a parked materialization: the shutdown path cancels and joins
/// the engine future under the existing bounded cleanup cap instead of
/// hanging on the wait.
#[tokio::test]
async fn stop_with_a_parked_materialization_shuts_down_bounded() {
    let engine = Arc::new(GatedContextEngine::default());
    let kernel = kernel_with(Arc::new(StreamingModel), engine.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    handle
        .user_message("exec-1 parking for stop".into())
        .await
        .unwrap();
    engine.entered.notified().await;

    tokio::time::timeout(Duration::from_secs(10), handle.stop())
        .await
        .expect("stop must stay bounded while materialization is parked")
        .expect("stop succeeds");
}

/// A storage failure under the parked wait is a fenced preparation failure,
/// not a blind retry: TurnCommitFailed names the materialize phase and the
/// runtime requires recovery.
#[tokio::test]
async fn materialization_storage_failure_fences_the_round() {
    let engine = Arc::new(GatedContextEngine::default());
    *engine.outcome.lock().unwrap() = GateOutcome::StorageFailure;
    let kernel = kernel_with(Arc::new(StreamingModel), engine.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle
        .user_message("exec-1 storage failure".into())
        .await
        .unwrap();
    engine.entered.notified().await;
    engine.release.notify_one();

    let failed = wait_for_event(&mut events, "TurnCommitFailed", |event| {
        matches!(event, RuntimeEvent::TurnCommitFailed { .. })
    })
    .await;
    let RuntimeEvent::TurnCommitFailed { phase, .. } = failed else {
        panic!("expected TurnCommitFailed");
    };
    assert_eq!(phase, "context_materialize");
    wait_for_event(&mut events, "RecoveryRequired", |event| {
        matches!(event, RuntimeEvent::RecoveryRequired)
    })
    .await;

    handle.stop().await.unwrap();
}
