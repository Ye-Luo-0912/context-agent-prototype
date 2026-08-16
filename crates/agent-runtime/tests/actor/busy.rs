use std::sync::Arc;
use std::time::Duration;

use agent_contracts::RuntimeEvent;
use agent_runtime::spawn_runtime;

use crate::harness::*;

#[tokio::test]
async fn actor_rejects_mutation_commands_while_a_turn_runs() {
    let (handle, _task) = start(Arc::new(HangingModel)).await;

    let turn = handle.clone();
    let turn_task = tokio::spawn(async move { turn.user_message("first".into()).await });

    // Give the turn time to start and block inside the model call.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let busy = handle.user_message("second".into()).await;
    assert!(
        busy.is_ok(),
        "a second user message while a turn runs is queued"
    );
    let overflow = handle.user_message("third".into()).await;
    assert!(
        overflow.is_err(),
        "a third user message while a turn runs and one is queued must be rejected"
    );
    assert!(
        overflow.unwrap_err().to_string().contains("queued"),
        "the overflow rejection must mention the queue"
    );
    let focus = handle.set_focus("new goal".into()).await;
    assert!(
        focus.is_err() && focus.unwrap_err().to_string().contains("busy"),
        "a focus change during a turn must be rejected (the old race)"
    );
    let pin = handle.pin("never edit generated files".into()).await;
    assert!(pin.is_err(), "a pin during a turn must be rejected");
    let done = handle.complete_current_task("sum".into()).await;
    assert!(
        done.is_err(),
        "task completion during a turn must be rejected"
    );

    handle.cancel_turn().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), turn_task)
        .await
        .expect("turn did not stop after cancellation")
        .expect("turn task panicked");
    assert!(
        result.is_ok(),
        "cancelled turn should end cleanly, got: {result:?}"
    );
}

#[tokio::test]
async fn cancel_then_new_turn_drops_stale_completion() {
    let (handle, _task) = start(Arc::new(HangingModel)).await;
    let mut events = handle.subscribe();

    let turn1 = handle.clone();
    let first = tokio::spawn(async move { turn1.user_message("first".into()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    handle.cancel_turn().await.unwrap();

    // The actor clears the busy marker on cancel, so a new turn is accepted
    // immediately; the cancelled turn's late completion must be dropped.
    let accepted = handle.user_message("second".into()).await;
    assert!(accepted.is_ok(), "a new turn after cancel must be accepted");

    let first_result = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("cancelled turn did not stop")
        .expect("cancelled turn panicked");
    assert!(first_result.is_ok());

    // Wait for the actor to process both completions.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut stale_warning = false;
    let mut consumed = false;
    while let Ok(envelope) = events.try_recv() {
        consumed |= matches!(&envelope.event, RuntimeEvent::ContextConsumed { .. });
        if let RuntimeEvent::Warning { message } = envelope.event
            && message.contains("stale model result dropped")
        {
            stale_warning = true;
        }
    }
    assert!(
        stale_warning,
        "the cancelled turn's late completion must be dropped with a warning"
    );
    assert!(
        !consumed,
        "cancelled/stale model operations must not commit context consumption"
    );
}

#[tokio::test]
async fn stop_ends_the_actor_cleanly() {
    let (handle, task) = start(Arc::new(StreamingModel)).await;
    let mut events = handle.subscribe();

    handle.user_message("hello".into()).await.unwrap();
    // Let the fast turn finish.
    tokio::time::sleep(Duration::from_millis(150)).await;

    handle.stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("actor task did not end after stop")
        .expect("actor task panicked");

    let mut run_completed = false;
    while let Ok(envelope) = events.try_recv() {
        if matches!(envelope.event, RuntimeEvent::RunCompleted) {
            run_completed = true;
        }
    }
    assert!(run_completed, "stop must emit RunCompleted");

    let after = handle.user_message("late".into()).await;
    assert!(
        after.is_err(),
        "commands after stop must fail, got: {after:?}"
    );
}

#[tokio::test]
async fn dropping_all_handles_still_shuts_down_cleanly() {
    let kernel = kernel(Arc::new(StreamingModel));
    // An independent subscriber survives the handle drop and must still see
    // the teardown events: the actor runs full shutdown when every caller
    // handle is gone instead of returning silently.
    let (handle, task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    drop(handle);

    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("actor task did not end after all handles dropped")
        .expect("actor task panicked");

    let mut run_completed = false;
    while let Ok(envelope) = events.try_recv() {
        if matches!(envelope.event, RuntimeEvent::RunCompleted) {
            run_completed = true;
        }
    }
    assert!(
        run_completed,
        "dropping all handles must still run the full shutdown"
    );
}
