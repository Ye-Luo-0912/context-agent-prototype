use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_contracts::{RuntimeEvent, RuntimeEventEnvelope};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};

use crate::harness::*;

#[tokio::test]
async fn turn_completed_is_broadcast_only_after_the_barrier() {
    let journal = Arc::new(BarrierJournal::default());
    let kernel = kernel_with_journal(journal.clone());
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();

    handle.user_message("hello".into()).await.unwrap();

    // The actor flushes the journal before broadcasting, so by the time the
    // subscriber sees TurnCompleted the barrier has already passed.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Ok(envelope) if matches!(envelope.event, RuntimeEvent::TurnCompleted) => break,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime event stream closed before TurnCompleted")
                }
            }
        }
    })
    .await
    .expect("turn did not commit within the test deadline");

    assert!(
        journal.flushes.load(Ordering::SeqCst) >= 1,
        "TurnCompleted must be broadcast only after a barrier flush"
    );
    {
        let appended = journal.appended.lock().unwrap();
        assert_eq!(
            appended.last().map(String::as_str),
            Some("RuntimeCommitBarrier { kind: Turn, checkpoint_sequence: None }"),
            "the explicit marker must close the durable turn batch"
        );
        assert!(
            appended
                .iter()
                .any(|name| name.starts_with("AssistantMessage")),
            "mandatory state writes must be appended before the commit batch: {appended:?}"
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn work_requires_one_successful_start_and_duplicate_start_writes_no_marker() {
    let journal = Arc::new(BarrierJournal::default());
    let kernel = kernel_with_journal(journal.clone());
    let (handle, _task) = spawn_runtime(kernel);

    let before_start = handle
        .user_message("must not run before start".into())
        .await
        .expect_err("business work must wait for the durable startup marker");
    assert!(matches!(
        before_start,
        agent_contracts::AgentError::InvalidRequest(_)
    ));
    assert!(journal.appended.lock().unwrap().is_empty());

    handle.start().await.unwrap();
    let duplicate = handle
        .start()
        .await
        .expect_err("startup is a one-shot actor transition");
    assert!(matches!(
        duplicate,
        agent_contracts::AgentError::InvalidRequest(_)
    ));
    assert_eq!(
        journal
            .appended
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.starts_with("RuntimeCommitBarrier { kind: RunStart"))
            .count(),
        1,
        "a duplicate Start must not append another format marker"
    );

    // Rejecting a duplicate does not poison the already serving actor.
    handle.set_focus("still serving".into()).await.unwrap();
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn partial_startup_append_failure_fences_work_without_stop_repairing_the_prefix() {
    let journal = Arc::new(BarrierJournal {
        fail_append_at: std::sync::atomic::AtomicUsize::new(2),
        ..BarrierJournal::default()
    });
    let kernel = kernel_with_journal(journal.clone());
    let (handle, _task) = spawn_runtime(kernel);

    let start = handle
        .start()
        .await
        .expect_err("the RunStart marker append must fail");
    assert!(matches!(
        start,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));
    assert_eq!(
        journal.appended.lock().unwrap().as_slice(),
        ["RunStarted"],
        "only the accepted first batch member may remain forensic"
    );

    let work = handle
        .user_message("must remain fenced".into())
        .await
        .expect_err("startup failure must reject later turns");
    assert!(matches!(
        work,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));
    let retry = handle
        .start()
        .await
        .expect_err("a later Start must not sweep the partial prefix");
    assert!(matches!(
        retry,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));
    assert_eq!(journal.append_attempts.load(Ordering::SeqCst), 2);

    handle.stop().await.unwrap();
    assert_eq!(
        journal.append_attempts.load(Ordering::SeqCst),
        2,
        "shutdown after failed startup must not append RunCompleted"
    );
    assert_eq!(journal.flushes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn startup_flush_failure_fences_work_without_retrying_flush_on_stop() {
    let journal = Arc::new(BarrierJournal {
        fail_flush: AtomicBool::new(true),
        ..BarrierJournal::default()
    });
    let kernel = kernel_with_journal(journal.clone());
    let (handle, _task) = spawn_runtime(kernel);

    let start = handle
        .start()
        .await
        .expect_err("the startup durability flush must fail");
    assert!(matches!(
        start,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));
    assert_eq!(journal.flushes.load(Ordering::SeqCst), 1);
    assert_eq!(
        journal
            .appended
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.starts_with("RuntimeCommitBarrier { kind: RunStart"))
            .count(),
        1
    );

    let work = handle
        .user_message("must remain fenced".into())
        .await
        .expect_err("a failed startup flush must reject later turns");
    assert!(matches!(
        work,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));
    let retry = handle
        .start()
        .await
        .expect_err("the actor must not retry a failed startup flush");
    assert!(matches!(
        retry,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));

    handle.stop().await.unwrap();
    assert_eq!(
        journal.flushes.load(Ordering::SeqCst),
        1,
        "shutdown after failed startup must not retry the failed prefix"
    );
    assert_eq!(journal.append_attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_barrier_blocks_turn_completed_and_marks_recovery_required() {
    let journal = Arc::new(BarrierJournal::default());
    let kernel = kernel_with_journal(journal.clone());
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    journal.fail_flush.store(true, Ordering::SeqCst);

    handle.user_message("hello".into()).await.unwrap();

    let mut commit_failed_phase = None;
    let mut saw_recovery = false;
    let mut saw_turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && (commit_failed_phase.is_none() || !saw_recovery)
    {
        match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(envelope)) => match envelope.event {
                RuntimeEvent::TurnCommitFailed { phase, .. } => commit_failed_phase = Some(phase),
                RuntimeEvent::RecoveryRequired => saw_recovery = true,
                RuntimeEvent::TurnCompleted => saw_turn_completed = true,
                _ => {}
            },
            _ => break,
        }
    }

    assert_eq!(
        commit_failed_phase.as_deref(),
        Some("turn_completed_event"),
        "the failure must be reported at the barrier step, after every mandatory state write"
    );
    assert!(
        saw_recovery,
        "a failed barrier must mark the runtime recovery required"
    );
    assert!(
        !saw_turn_completed,
        "TurnCompleted must never be broadcast when the barrier fails"
    );
    assert!(
        journal.flushes.load(Ordering::SeqCst) >= 1,
        "the barrier must have been attempted"
    );
    {
        let appended = journal.appended.lock().unwrap();
        assert!(
            appended.iter().any(|name| name == "TurnCompleted"),
            "TurnCompleted must be appended into the FIFO before the failed flush: {appended:?}"
        );
        assert!(
            appended
                .iter()
                .any(|name| name.starts_with("RuntimeCommitBarrier")),
            "the failed batch may leave an orphan marker in the forensic suffix: {appended:?}"
        );
    }
    let next = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("a failed durability barrier must fence later mutation");
    assert!(
        matches!(next, agent_contracts::AgentError::RecoveryRequired(_)),
        "the runtime must stay fenced after the failed barrier: {next}"
    );
    // The permanent same-run fence is what prevents any later successful
    // barrier from sweeping the orphan failed batch into a trusted prefix.
    // The operator repairs storage before the runtime may run again: with
    // the barrier healthy, stop's own flush succeeds and teardown is clean.
    journal.fail_flush.store(false, Ordering::SeqCst);
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn failed_cancel_barrier_returns_recovery_required_and_never_claims_completion() {
    let journal = Arc::new(BarrierJournal::default());
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(HangingModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal.clone()),
    ));
    let (handle, _task) = spawn_runtime(services);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("cancel me".into()).await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ModelStarted { .. },
                    ..
                }) => break,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime closed before the model operation started")
                }
            }
        }
    })
    .await
    .expect("model operation did not start");

    journal.fail_flush.store(true, Ordering::SeqCst);
    let error = handle
        .cancel_turn()
        .await
        .expect_err("a failed cancellation barrier must reach the caller");
    assert!(matches!(
        error,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));

    let mut saw_cancelled = false;
    let mut saw_completed = false;
    while let Ok(envelope) = events.try_recv() {
        saw_cancelled |= matches!(envelope.event, RuntimeEvent::TurnCancelled { .. });
        saw_completed |= matches!(envelope.event, RuntimeEvent::TurnCompleted);
    }
    assert!(
        !saw_cancelled,
        "TurnCancelled is broadcast only after its durable barrier passes"
    );
    assert!(
        !saw_completed,
        "cancellation must never reuse the successful completion marker"
    );
    {
        let appended = journal.appended.lock().unwrap();
        assert!(
            appended
                .iter()
                .any(|name| name.starts_with("TurnCancelled")),
            "the cancellation marker must be the event covered by the attempted barrier"
        );
    }
    let next = handle
        .user_message("must recover first".into())
        .await
        .expect_err("failed cancellation persistence must fence later mutation");
    assert!(matches!(
        next,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));

    journal.fail_flush.store(false, Ordering::SeqCst);
    handle.stop().await.unwrap();
}
