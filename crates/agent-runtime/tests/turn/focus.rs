use std::{sync::Arc, time::Duration};

use agent_contracts::{RuntimeEvent, TaskId};

use agent_runtime::RuntimeHandle;

use crate::harness::*;

// ---------------------------------------------------------------------------
// Task vs focus: the TaskManager keeps long-lived task identity stable, so
// re-focusing a goal resumes the same task instead of minting a new one.
// ---------------------------------------------------------------------------

async fn collect_focus_events(handle: &RuntimeHandle, goal: &str) -> (TaskId, u64) {
    let mut events = handle.subscribe();
    handle.set_focus(goal.into()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::FocusChanged { task_id, .. } = envelope.event {
                return (task_id, envelope.seq);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no FocusChanged event arrived"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn refocusing_the_same_goal_resumes_the_same_task() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;

    let (task_a, _) = collect_focus_events(&handle, "fix AuthService").await;
    let (task_b, _) = collect_focus_events(&handle, "write docs").await;
    let (task_a_again, _) = collect_focus_events(&handle, "fix AuthService").await;

    assert_ne!(task_a, task_b, "different goals are different tasks");
    assert_eq!(
        task_a, task_a_again,
        "re-focusing the same goal must resume the original task"
    );
}

#[tokio::test]
async fn suspend_then_refocus_resumes_the_same_task() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let (task_a, _) = collect_focus_events(&handle, "fix AuthService").await;

    let mut events = handle.subscribe();
    handle.suspend_task().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_cleared = false;
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::FocusCleared = envelope.event {
                saw_cleared = true;
            }
        }
        if saw_cleared {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "suspend must emit FocusCleared"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Re-focusing the same goal resumes the suspended task, not a new one.
    let (resumed, _) = collect_focus_events(&handle, "fix AuthService").await;
    assert_eq!(resumed, task_a, "suspend -> refocus must resume the task");
}
