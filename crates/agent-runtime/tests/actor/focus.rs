use std::sync::{Arc, Mutex};

use agent_contracts::{ContextEngine, ContextQuery};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};

use crate::harness::*;

#[tokio::test]
async fn failed_focus_never_mutates_the_task_table() {
    let kernel = kernel_with(Arc::new(SilentModel), Arc::new(FailingFocusContextEngine));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    // An explicit /focus whose engine transition fails must leave the
    // runtime without a task: TaskManager state changes only on commit.
    let result = handle.set_focus("goal A".into()).await;
    assert!(result.is_err(), "focus must fail");
    assert!(
        handle.list_tasks().await.unwrap().is_empty(),
        "no task may be registered when the focus transition failed"
    );

    // The first user message auto-creates an implicit task; when the focus
    // transition fails there too, the implicit task must not be registered.
    let result = handle.user_message("hello".into()).await;
    assert!(result.is_err(), "the turn must fail with the focus error");
    assert!(
        handle.list_tasks().await.unwrap().is_empty(),
        "an implicit task exists only after its focus committed"
    );
}

#[tokio::test]
async fn maintenance_failure_rolls_back_context_before_rejecting_focus() {
    let context = Arc::new(MutatingThenFailingFocusEngine::default());
    let kernel = kernel_with(Arc::new(SilentModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let result = handle.set_focus("goal A".into()).await;
    assert!(result.is_err());
    assert!(handle.list_tasks().await.unwrap().is_empty());
    let materialized = context
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 0,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized.focus.is_none(),
        "the focus mutation must be rolled back with the rejected task transition"
    );
}

#[tokio::test]
async fn rollback_failure_poison_fences_further_runtime_mutation() {
    let context = Arc::new(MutatingThenFailingFocusEngine {
        focus: Mutex::new(None),
        rollback_fails: true,
    });
    let kernel = kernel_with(Arc::new(SilentModel), context);
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let first = handle.set_focus("goal A".into()).await.unwrap_err();
    assert!(first.to_string().contains("rollback failed"));
    let second = handle.set_focus("goal B".into()).await.unwrap_err();
    assert!(
        second.to_string().contains("runtime recovery is required"),
        "once alignment cannot be proven, later mutations must be fenced: {second}"
    );
    assert!(handle.list_tasks().await.unwrap().is_empty());
}

#[tokio::test]
async fn journal_failure_after_focus_never_splits_task_and_context() {
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailFocusEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let result = handle.set_focus("goal A".into()).await;
    assert!(result.is_err(), "the journal failure must stay observable");
    let tasks = handle.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].status, agent_runtime::TaskStatus::Active);
    let focus = context
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 0,
            hints: Default::default(),
        })
        .await
        .unwrap()
        .focus;
    assert_eq!(
        focus.map(|focus| focus.task_id),
        Some(tasks[0].id),
        "journal failure may create an audit gap, but not split task authority from context"
    );
    let next = handle.set_focus("goal B".into()).await.unwrap_err();
    assert!(
        matches!(next, agent_contracts::AgentError::RecoveryRequired(_)),
        "an applied transition with a missing audit record must fence later mutation"
    );
}

#[tokio::test]
async fn failed_clear_focus_never_mutates_the_task_table() {
    let kernel = kernel_with(
        Arc::new(SilentModel),
        Arc::new(FailingClearFocusContextEngine),
    );
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    handle.set_focus("goal A".into()).await.unwrap();
    assert_eq!(handle.list_tasks().await.unwrap().len(), 1);

    // The engine rejects clear_focus: the suspend must fail and the task
    // must stay registered and active — TaskManager commits only after the
    // engine transition succeeds.
    let result = handle.suspend_task().await;
    assert!(result.is_err(), "suspend must fail with the engine error");
    let tasks = handle.list_tasks().await.unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "a failed clear_focus must not mutate the task table"
    );
    assert_eq!(
        tasks[0].status,
        agent_runtime::TaskStatus::Active,
        "the task must stay active after a failed suspend"
    );
}
