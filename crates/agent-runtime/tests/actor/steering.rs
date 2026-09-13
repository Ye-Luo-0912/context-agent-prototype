//! F5 acceptance: in-task steering and identity-precise control.
//!
//! Two facts are under test and they are deliberately different facts:
//!
//! * a **submission** may create or re-focus a task; a **correction** may not.
//!   Steering that cannot find the task the caller named refuses instead of
//!   inventing a target or minting new work;
//! * every precise operation compares the caller's expected identity against
//!   the live value INSIDE the actor. A client that snapshots, then commands,
//!   must not be able to act on the state it saw if it has since moved.

use std::sync::Arc;

use agent_contracts::TaskId;
use agent_runtime::{
    CancelOutcome, ContinueOutcome, ContinueReason, SteeringDisposition, SteeringOutcome,
    SteeringRejection, SuspendOutcome, TurnIdentityExpectation,
};

use crate::harness::*;

/// Steering is not submission: with nothing active there is no task to correct,
/// and the runtime says so rather than creating one.
#[tokio::test]
async fn steering_never_creates_a_task() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;

    let outcome = handle
        .steer_active_task("tighten the timeout".into(), None)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        SteeringOutcome::Rejected {
            rejection: SteeringRejection::NoActiveTask,
            active_task_id: None,
        }
    );
    assert!(
        handle.list_tasks().await.unwrap().is_empty(),
        "a refused correction must leave no task behind"
    );
}

/// A correction applies to the task the caller named, and the receipt binds to
/// that same task. The task table does not grow: the correction is part of the
/// work already in progress.
#[tokio::test]
async fn steering_applies_to_the_named_active_task() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let mut events = handle.subscribe();

    let submitted = handle
        .start_work("migrate the retry table".into(), "steer-1".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    let outcome = handle
        .steer_active_task(
            "keep the five second timeout".into(),
            Some(submitted.task_id),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        SteeringOutcome::Accepted {
            disposition: SteeringDisposition::Applied,
            task_id: submitted.task_id,
        }
    );
    wait_for_turn_completed(&mut events).await;
    assert_eq!(
        handle.list_tasks().await.unwrap().len(),
        1,
        "a correction is not a second piece of work"
    );
}

/// The identity guard: a correction aimed at a task the runtime is no longer on
/// is refused, and the refusal names what IS live so the caller can re-target
/// instead of retrying into the wrong task.
#[tokio::test]
async fn steering_refuses_a_stale_task_expectation() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let mut events = handle.subscribe();

    let first = handle
        .start_work("first goal".into(), "steer-a".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    let second = handle
        .start_work("second goal".into(), "steer-b".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    assert_ne!(first.task_id, second.task_id);

    let outcome = handle
        .steer_active_task("belongs to the first task".into(), Some(first.task_id))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        SteeringOutcome::Rejected {
            rejection: SteeringRejection::ExpectedTaskMismatch,
            active_task_id: Some(second.task_id),
        },
        "the correction must not be delivered to the task that happens to be current"
    );
}

/// While a turn runs, a correction takes that turn's single input slot and is
/// reported as queued — admitted, not yet executed. A second correction finds
/// the slot taken and is refused rather than silently overwriting the first.
#[tokio::test]
async fn steering_queues_behind_a_running_turn_and_refuses_a_full_slot() {
    let (handle, _task) = start(Arc::new(HangingModel)).await;

    let submitted = handle
        .start_work("long running".into(), "steer-queue".into())
        .await
        .unwrap();

    let queued = handle
        .steer_active_task("first correction".into(), Some(submitted.task_id))
        .await
        .unwrap();
    assert_eq!(
        queued,
        SteeringOutcome::Accepted {
            disposition: SteeringDisposition::Queued,
            task_id: submitted.task_id,
        }
    );

    let refused = handle
        .steer_active_task("second correction".into(), Some(submitted.task_id))
        .await
        .unwrap();
    assert_eq!(
        refused,
        SteeringOutcome::Rejected {
            rejection: SteeringRejection::QueueFull,
            active_task_id: Some(submitted.task_id),
        },
        "a taken correction slot is a typed refusal, never a silent overwrite"
    );

    handle.cancel_turn().await.unwrap();
}

/// Suspend, activate and continue all compare the caller's expectation against
/// the live task, and a mismatch changes nothing at all.
#[tokio::test]
async fn suspend_activate_and_continue_honour_the_expected_task() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let mut events = handle.subscribe();

    let first = handle
        .start_work("first goal".into(), "identity-a".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    let second = handle
        .start_work("second goal".into(), "identity-b".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    // Suspending the wrong task suspends nothing.
    assert_eq!(
        handle
            .suspend_task_expecting(Some(first.task_id))
            .await
            .unwrap(),
        SuspendOutcome::ExpectedTaskMismatch {
            active_task_id: Some(second.task_id),
        }
    );
    let status = handle.status_snapshot().await.unwrap();
    assert_eq!(
        status.focus_task_id,
        Some(second.task_id),
        "a refused suspension must leave focus exactly where it was"
    );

    // Suspending the live task returns the identity it suspended.
    assert_eq!(
        handle
            .suspend_task_expecting(Some(second.task_id))
            .await
            .unwrap(),
        SuspendOutcome::Suspended {
            task_id: second.task_id,
        }
    );

    // Activation goes through the same actor and reports what moved.
    let activation = handle.activate_task_reporting(first.task_id).await.unwrap();
    assert_eq!(activation.task_id, first.task_id);
    assert!(!activation.already_active);
    let repeat = handle.activate_task_reporting(first.task_id).await.unwrap();
    assert!(
        repeat.already_active,
        "re-activating the live task must report that nothing moved"
    );

    // Continuing the wrong task starts no turn.
    assert_eq!(
        handle
            .continue_active_task_expecting(Some(second.task_id))
            .await
            .unwrap(),
        ContinueOutcome::ExpectedTaskMismatch {
            active_task_id: Some(first.task_id),
        }
    );
    assert_eq!(
        handle.status_snapshot().await.unwrap().focus_task_id,
        Some(first.task_id)
    );

    // Continuing the live task resumes its retained directive.
    assert_eq!(
        handle
            .continue_active_task_expecting(Some(first.task_id))
            .await
            .unwrap(),
        ContinueOutcome::Continued {
            task_id: first.task_id,
        }
    );
    wait_for_turn_completed(&mut events).await;
}

/// A precise cancel must not kill the successor of the turn the client saw. An
/// expectation that no longer matches cancels nothing and reports the live
/// identity; the matching expectation cancels exactly that turn.
#[tokio::test]
async fn cancel_only_stops_the_turn_the_caller_named() {
    let (handle, _task) = start(Arc::new(HangingModel)).await;
    let submitted = handle
        .start_work("long running".into(), "cancel-identity".into())
        .await
        .unwrap();

    // A turn id from another era matches nothing that is live.
    let stale = handle
        .cancel_turn_expecting(Some(TurnIdentityExpectation {
            task_id: Some(submitted.task_id),
            turn_id: Some(agent_contracts::TurnId::new()),
        }))
        .await
        .unwrap();
    match stale {
        CancelOutcome::ExpectedIdentityMismatch {
            active_task_id,
            active_turn_id,
        } => {
            assert_eq!(active_task_id, Some(submitted.task_id));
            assert!(
                active_turn_id.is_some(),
                "the live turn is named so the caller can retry precisely"
            );
        }
        CancelOutcome::Acknowledged(ack) => {
            panic!("a stale turn expectation must not cancel anything, got {ack:?}")
        }
    }

    // The turn the mismatch reported is the one a precise cancel can stop.
    let live_turn = match handle
        .cancel_turn_expecting(Some(TurnIdentityExpectation {
            task_id: Some(submitted.task_id),
            turn_id: None,
        }))
        .await
        .unwrap()
    {
        CancelOutcome::Acknowledged(ack) => ack,
        CancelOutcome::ExpectedIdentityMismatch { .. } => {
            panic!("the live task expectation must match")
        }
    };
    assert!(
        matches!(live_turn, agent_contracts::TurnCancelAck::Cancelled { .. }),
        "cancelling the named task's running turn must reach the durable barrier"
    );

    // Naming a task that is not focused refuses even when a turn exists.
    let foreign = handle
        .cancel_turn_expecting(Some(TurnIdentityExpectation {
            task_id: Some(TaskId::new()),
            turn_id: None,
        }))
        .await
        .unwrap();
    assert!(matches!(
        foreign,
        CancelOutcome::ExpectedIdentityMismatch { .. }
    ));
}

/// The status snapshot answers "can I continue, and if not why" and "what round
/// budget is enforced" without a GUI and without attempting the operation.
#[tokio::test]
async fn status_reports_the_round_budget_and_why_continuation_is_unavailable() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let mut events = handle.subscribe();

    let idle = handle.status_snapshot().await.unwrap();
    assert!(
        idle.max_model_rounds >= 1,
        "the enforced model-round budget must be finite and positive"
    );
    assert_eq!(idle.continue_readiness.reason, ContinueReason::NoActiveTask);
    assert!(!idle.continue_readiness.can_continue);

    let submitted = handle
        .start_work("budget probe".into(), "status-1".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    let ready = handle.status_snapshot().await.unwrap();
    assert_eq!(ready.continue_readiness.reason, ContinueReason::Ready);
    assert!(ready.continue_readiness.can_continue);
    assert_eq!(ready.focus_task_id, Some(submitted.task_id));

    handle
        .suspend_task_expecting(Some(submitted.task_id))
        .await
        .unwrap();
    let suspended = handle.status_snapshot().await.unwrap();
    assert_eq!(
        suspended.continue_readiness.reason,
        ContinueReason::NoActiveTask,
        "a suspended run reports no continuation instead of offering one"
    );
}
