//! P1 acceptance: the atomic `start_work` submission. Two clients can never
//! cross-deliver focus and message (F07); a repeated client request id never
//! secretly executes twice; a conflicting id is rejected; a single client's
//! task identity stays stable.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_contracts::{RuntimeEvent, RuntimeEventEnvelope, TaskId};
use tokio::sync::broadcast::error::RecvError;

use crate::harness::*;

async fn next_user_input(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> (Option<TaskId>, String) {
    loop {
        match events.recv().await {
            Ok(envelope) => {
                if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                    return (input.task_id, input.preview);
                }
            }
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => panic!("event stream closed before the user input"),
        }
    }
}

/// The F07 race: client A's focus and A's message must land on the same task
/// even though client B switches focus between the two submissions. With the
/// atomic command the pairing inside every receipt is the pairing the runtime
/// applied.
#[tokio::test]
async fn interleaved_set_focus_and_submit_do_not_cross_deliver() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let mut events = handle.subscribe();

    let a = handle
        .start_work("goal alpha".into(), "req-a".into())
        .await
        .unwrap();
    assert_eq!(
        a.disposition,
        agent_runtime::WorkSubmissionDisposition::Accepted
    );
    let (task_of_a, preview_a) = next_user_input(&mut events).await;
    assert_eq!(preview_a, "goal alpha");
    assert_eq!(
        task_of_a,
        Some(a.task_id),
        "A's message must bind to A's task"
    );
    wait_for_turn_completed(&mut events).await;

    // Client B submits its own work; focus and message move together, so B's
    // input can never land on A's task or vice versa.
    let b = handle
        .start_work("goal beta".into(), "req-b".into())
        .await
        .unwrap();
    let (task_of_b, preview_b) = next_user_input(&mut events).await;
    assert_eq!(preview_b, "goal beta");
    assert_eq!(
        task_of_b,
        Some(b.task_id),
        "B's message must bind to B's task"
    );
    assert_ne!(a.task_id, b.task_id);
    wait_for_turn_completed(&mut events).await;

    // Resuming A's goal under a new request id returns to the same task, and
    // the fresh message binds to that same task identity.
    let again = handle
        .start_work("goal alpha".into(), "req-a2".into())
        .await
        .unwrap();
    assert_eq!(again.task_id, a.task_id);
    let (task_of_again, preview_again) = next_user_input(&mut events).await;
    assert_eq!(preview_again, "goal alpha");
    assert_eq!(task_of_again, Some(a.task_id));
}

/// A retried request with the same client request id and the same goal is an
/// idempotent replay of the receipt: the original admission is returned and
/// no second turn secretly executes.
#[tokio::test]
async fn retry_while_original_work_runs_returns_its_receipt() {
    let (handle, _task) = start(Arc::new(HangingModel)).await;
    let first = handle
        .start_work("in flight".into(), "retry-id".into())
        .await
        .unwrap();
    let retry = handle
        .start_work("in flight".into(), "retry-id".into())
        .await
        .unwrap();
    assert_eq!(retry.task_id, first.task_id);
    assert_eq!(
        retry.disposition,
        agent_runtime::WorkSubmissionDisposition::AlreadyAccepted
    );
    assert!(
        handle
            .start_work("different".into(), "retry-id".into())
            .await
            .unwrap_err()
            .to_string()
            .contains("different payload")
    );
    assert!(
        handle
            .start_work("new work".into(), "new-id".into())
            .await
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    handle.cancel_turn().await.unwrap();
}

#[tokio::test]
async fn duplicate_start_work_does_not_execute_twice() {
    let model = Arc::new(RecordingModel::default());
    let (handle, _task) = start(model.clone()).await;
    let mut events = handle.subscribe();

    let first = handle
        .start_work("duplicate goal".into(), "same-id".into())
        .await
        .unwrap();
    assert_eq!(
        first.disposition,
        agent_runtime::WorkSubmissionDisposition::Accepted
    );
    wait_for_turn_completed(&mut events).await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);

    let retried = handle
        .start_work("duplicate goal".into(), "same-id".into())
        .await
        .unwrap();
    assert_eq!(
        retried.disposition,
        agent_runtime::WorkSubmissionDisposition::AlreadyAccepted
    );
    assert_eq!(retried.task_id, first.task_id);
    assert!(retried.task_manage_notice.is_none());

    // Give the actor a beat: the dedup path must not start another turn.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "a retried id must not re-execute the goal"
    );
}

/// The same client request id with different content is a conflict: it is
/// rejected instead of being executed under a foreign identity.
#[tokio::test]
async fn conflicting_client_request_id_is_rejected() {
    let model = Arc::new(RecordingModel::default());
    let (handle, _task) = start(model.clone()).await;
    let mut events = handle.subscribe();

    handle
        .start_work("goal one".into(), "shared-id".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    let conflict = handle
        .start_work("goal two".into(), "shared-id".into())
        .await;
    assert!(
        conflict.is_err(),
        "the same id may not submit other content"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "a rejected conflict must not start a turn"
    );

    // A different id for the other goal still works.
    let other = handle
        .start_work("goal two".into(), "other-id".into())
        .await
        .unwrap();
    assert_eq!(
        other.disposition,
        agent_runtime::WorkSubmissionDisposition::Accepted
    );
}

/// The long-task checklist surface attaches to an empty requirement set, the
/// receipt reports it, and a single client's task identity stays stable
/// across a same-goal resubmission under a new id.
#[tokio::test]
async fn checklist_attaches_and_single_client_task_identity_is_stable() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let mut events = handle.subscribe();

    let first = handle
        .start_work("checklist goal".into(), "req-1".into())
        .await
        .unwrap();
    assert!(first.task_manage_notice.is_none());
    wait_for_turn_completed(&mut events).await;

    let tasks = handle.list_tasks().await.unwrap();
    let task = tasks
        .iter()
        .find(|task| task.id == first.task_id)
        .expect("the submitted task is listed");
    assert_eq!(task.tool_requirement_count, 1);
    assert_eq!(task.tool_requirement_revision, 1);
    assert!(matches!(task.status, agent_runtime::TaskStatus::Active));

    // Same goal, new id: resumes the same task instead of forking a second
    // TaskManager identity, and the empty-set guard keeps the checklist.
    let again = handle
        .start_work("checklist goal".into(), "req-2".into())
        .await
        .unwrap();
    assert_eq!(again.task_id, first.task_id);
    assert!(again.task_manage_notice.is_none());
}

/// Admission fails closed on out-of-bounds goals and ids, before any state
/// moves.
#[tokio::test]
async fn out_of_bounds_submissions_fail_closed() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;

    assert!(handle.start_work("   ".into(), "req".into()).await.is_err());
    assert!(
        handle
            .start_work("goal".into(), String::new())
            .await
            .is_err()
    );
    let long_id = "x".repeat(agent_runtime::work::MAX_CLIENT_REQUEST_ID_BYTES + 1);
    assert!(handle.start_work("goal".into(), long_id).await.is_err());
    assert!(
        handle.list_tasks().await.unwrap().is_empty(),
        "no task may exist after rejected submissions"
    );
}

// ---------------------------------------------------------------------------
// PLATFORM-1 (F06): the exact-request receipt query.
// ---------------------------------------------------------------------------

/// An id this run never admitted is `Unknown` — never "not executed", never a
/// reason to resend. A pre-existing task with the SAME goal does not make the
/// new request admitted: the query answers about the id, not the goal text.
#[tokio::test]
async fn unknown_request_is_unknown_even_when_a_same_goal_task_exists() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;

    // No submission at all: unknown, and it must not be mistaken for a
    // negative proof.
    assert_eq!(
        handle
            .query_work_submission("never-seen".into(), None)
            .await
            .unwrap(),
        agent_runtime::WorkSubmissionQuery::Unknown
    );

    // A task with the goal text already exists from another request id. A
    // brand-new id with the SAME goal is still unknown before it is
    // submitted — the goal is not an idempotency key.
    let mut events = handle.subscribe();
    handle
        .start_work("shared goal text".into(), "req-old".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    assert_eq!(
        handle
            .query_work_submission("req-new".into(), None)
            .await
            .unwrap(),
        agent_runtime::WorkSubmissionQuery::Unknown
    );
}

/// An admitted id is reported with its task and its recorded payload digest;
/// supplying the matching digest confirms the exact payload, a foreign digest
/// is an explicit mismatch, and no digest proves identity only.
#[tokio::test]
async fn recorded_request_reports_its_task_and_matches_the_named_payload() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let mut events = handle.subscribe();

    let submitted = handle
        .start_work("query me".into(), "req-q1".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    let digest = agent_platform_protocol::submission_payload_digest("query me");

    // Matching payload: the exact request is admitted.
    assert_eq!(
        handle
            .query_work_submission("req-q1".into(), Some(digest.clone()))
            .await
            .unwrap(),
        agent_runtime::WorkSubmissionQuery::Recorded {
            task_id: submitted.task_id,
            payload_digest: digest.clone(),
            matches: Some(true),
        }
    );

    // No payload named: identity is proven, the payload comparison is not.
    assert_eq!(
        handle
            .query_work_submission("req-q1".into(), None)
            .await
            .unwrap(),
        agent_runtime::WorkSubmissionQuery::Recorded {
            task_id: submitted.task_id,
            payload_digest: digest.clone(),
            matches: None,
        }
    );

    // A foreign payload under a recorded id is an explicit mismatch — the
    // query still reports the recorded digest so the client can show
    // "same id, different content" without guessing.
    let other = agent_platform_protocol::submission_payload_digest("something else");
    assert_eq!(
        handle
            .query_work_submission("req-q1".into(), Some(other))
            .await
            .unwrap(),
        agent_runtime::WorkSubmissionQuery::Recorded {
            task_id: submitted.task_id,
            payload_digest: digest,
            matches: Some(false),
        }
    );
}

/// The query is read-only: it never starts a turn, never mutates the ledger
/// and never forks a task.
#[tokio::test]
async fn query_is_read_only_and_does_not_execute() {
    let model = Arc::new(RecordingModel::default());
    let (handle, _task) = start(model.clone()).await;
    let mut events = handle.subscribe();

    handle
        .start_work("read only goal".into(), "req-ro".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    let calls_after_submit = model.calls.load(Ordering::SeqCst);
    let tasks_after_submit = handle.list_tasks().await.unwrap().len();

    for _ in 0..3 {
        assert!(matches!(
            handle
                .query_work_submission("req-ro".into(), None)
                .await
                .unwrap(),
            agent_runtime::WorkSubmissionQuery::Recorded { .. }
        ));
    }
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        calls_after_submit,
        "a query must never start a model round"
    );
    assert_eq!(
        handle.list_tasks().await.unwrap().len(),
        tasks_after_submit,
        "a query must never create or fork a task"
    );
}

/// The ledger is bounded: the oldest receipt is evicted, and an evicted id
/// becomes `Unknown` again — honest about lost evidence rather than claiming
/// the request never ran.
#[tokio::test]
async fn evicted_receipt_becomes_unknown_not_a_negative_proof() {
    // A silent model completes a turn promptly, so the fill loop below drains
    // through the actor's serialized command lane without deadlocking.
    let (handle, _task) = start(Arc::new(SilentModel)).await;

    let first = handle
        .start_work("eviction goal".into(), "req-first".into())
        .await
        .unwrap();
    assert!(matches!(
        handle
            .query_work_submission("req-first".into(), None)
            .await
            .unwrap(),
        agent_runtime::WorkSubmissionQuery::Recorded { .. }
    ));

    // Push the ledger past its bound with distinct ids reusing the same goal
    // text: the goal is not the identity, so this is legal and proves each id
    // is tracked individually.
    let bound = agent_runtime::work::MAX_PENDING_WORK_SUBMISSIONS;
    for index in 0..bound {
        let id = format!("req-fill-{index}");
        let receipt = handle.start_work("eviction goal".into(), id).await.unwrap();
        assert_eq!(
            receipt.disposition,
            agent_runtime::WorkSubmissionDisposition::Accepted
        );
    }

    // The very first receipt has fallen out of the bounded window.
    assert_eq!(
        handle
            .query_work_submission("req-first".into(), None)
            .await
            .unwrap(),
        agent_runtime::WorkSubmissionQuery::Unknown,
        "an evicted id must read as unknown, never as never-executed"
    );
    // Eviction changed nothing about the original task identity.
    assert_eq!(
        handle
            .list_tasks()
            .await
            .unwrap()
            .iter()
            .filter(|task| task.id == first.task_id)
            .count(),
        1
    );
}
