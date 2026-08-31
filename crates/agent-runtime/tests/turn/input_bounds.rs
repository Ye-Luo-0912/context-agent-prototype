//! Input-state bounds: one byte/character policy before live acceptance,
//! artifact write, task commit and event publication. Oversized input,
//! focus, and pins fail closed instead of persisting or publishing a body
//! that later checkpoints would refuse.

use std::sync::Arc;

use agent_contracts::{
    MAX_PINNED_CONTENT_CHARS, MAX_TASK_ANCHOR_TEXT_CHARS, RuntimeEvent, USER_INPUT_MAX_BYTES,
};

use crate::harness::*;

#[tokio::test]
async fn oversized_user_message_fails_closed_before_persist() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;

    let body = "x".repeat(USER_INPUT_MAX_BYTES + 1);
    let error = handle
        .user_message(body)
        .await
        .expect_err("oversized input must fail closed");
    assert!(error.to_string().contains("byte cap"), "{error}");
    assert!(
        handle.list_tasks().await.unwrap().is_empty(),
        "an oversized message must not auto-create a task"
    );
}

#[tokio::test]
async fn explicit_focus_over_anchor_bound_is_rejected() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;

    let goal = "g".repeat(MAX_TASK_ANCHOR_TEXT_CHARS + 1);
    let error = handle
        .set_focus(goal)
        .await
        .expect_err("oversized explicit focus must fail closed");
    assert!(error.to_string().contains("cap"), "{error}");
    assert!(
        handle.list_tasks().await.unwrap().is_empty(),
        "a rejected focus must not create a task"
    );
}

#[tokio::test]
async fn implicit_focus_normalizes_a_long_first_message_goal() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;

    let body = "m".repeat(MAX_TASK_ANCHOR_TEXT_CHARS + 1);
    let mut events = handle.subscribe();
    handle.user_message(body.clone()).await.unwrap();

    // The full message is legitimate dialogue but the auto-created task
    // goal (and the FocusChanged event) is the bounded prefix.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            while let Ok(envelope) = events.try_recv() {
                if let RuntimeEvent::FocusChanged { goal, .. } = envelope.event {
                    assert_eq!(
                        goal.chars().count(),
                        MAX_TASK_ANCHOR_TEXT_CHARS,
                        "implicit focus goal must be the bounded prefix"
                    );
                    assert_eq!(goal, &body[..MAX_TASK_ANCHOR_TEXT_CHARS]);
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("an implicit FocusChanged event must arrive");

    let tasks = handle.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].goal.chars().count(),
        MAX_TASK_ANCHOR_TEXT_CHARS,
        "the committed task goal must be normalized before commit"
    );
}

#[tokio::test]
async fn oversized_pin_is_rejected() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;

    let content = "p".repeat(MAX_PINNED_CONTENT_CHARS + 1);
    let error = handle
        .pin(content)
        .await
        .expect_err("oversized pinned content must fail closed");
    assert!(error.to_string().contains("cap"), "{error}");
}
