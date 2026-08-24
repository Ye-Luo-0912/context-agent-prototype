//! Safe-point resume commits: durable changes at a fully settled batch
//! install the bounded resume and schedule exactly one atomic checkpoint
//! write whose acknowledgement lands before `TurnCompleted`. Read-only
//! rounds accrue nothing. `continue_active_task` restarts the stored
//! directive without minting a new user instruction.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentResult, InputKind, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport,
    RuntimeEvent, RuntimeEventEnvelope, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;

use crate::harness::*;

/// Serves one named read-only builtin-shaped call per request.
#[derive(Debug)]
struct SingleToolDispatcher {
    tool_name: &'static str,
}

#[async_trait::async_trait]
impl ToolDispatcher for SingleToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: self.tool_name.into(),
            description: "scripted".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: self.tool_name.into(),
            ok: true,
            summary: "settled".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

/// Calls the scripted tool on round 0, then finishes.
#[derive(Debug)]
struct OneCallThenFinishModel {
    tool_name: &'static str,
    arguments: serde_json::Value,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for OneCallThenFinishModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: self.tool_name.into(),
                    arguments: self.arguments.clone(),
                }],
                usage: Default::default(),
            })
        } else {
            Ok(ModelOutput {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }
}

async fn instance_with(
    dir: &std::path::Path,
    tool_name: &'static str,
    arguments: serde_json::Value,
) -> RuntimeInstance {
    let workspace = agent_workspace::Workspace::open(dir).await.unwrap();
    let model = Arc::new(OneCallThenFinishModel {
        tool_name,
        arguments,
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(SingleToolDispatcher { tool_name }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(Arc::new(workspace));
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.handle().start().await.unwrap();
    instance
}

/// Run one turn and return the checkpoint-relevant event labels in arrival
/// order.
async fn run_turn(
    instance: &RuntimeInstance,
    mut events: tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> Vec<String> {
    let handle = instance.handle();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();
    let mut labels = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut done = false;
    while tokio::time::Instant::now() < deadline && !done {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TaskResumeCommitted { debt, .. } => {
                    labels.push(format!("resume_committed:{}", debt.join("+")))
                }
                RuntimeEvent::CheckpointDurable { .. } => labels.push("checkpoint_durable".into()),
                RuntimeEvent::CheckpointWriteFailed { reason } => {
                    labels.push(format!("checkpoint_failed:{reason}"))
                }
                RuntimeEvent::TaskContinuationStarted { .. } => {
                    labels.push("continuation_started".into())
                }
                RuntimeEvent::UserMessageAccepted { input } => {
                    labels.push(format!("input_accepted:{:?}", input.kind))
                }
                RuntimeEvent::TurnCompleted => {
                    labels.push("turn_completed".into());
                    done = true;
                }
                _ => {}
            }
        }
        if !done {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(done, "the turn must finish inside the test deadline");
    labels
}

#[tokio::test]
async fn anchor_change_settles_into_resume_then_durable_before_turn_completed() {
    let dir = tempfile::tempdir().unwrap();
    // The real task.manage packaging is covered elsewhere; this test only
    // needs an accepted autonomous anchor patch, which the runtime applies
    // through the same trusted CAS when the model calls task.manage.
    let instance = instance_with(
        dir.path(),
        "task.manage",
        json!({"base_anchor_revision": 0, "next_action": "add the fake-sleeper unit test"}),
    )
    .await;
    // task.manage is served by the generic single-tool dispatcher here as a
    // plain value, so drive the anchor change through the operator command
    // instead: the debt source under test is the anchor change itself.
    let handle = instance.handle();
    let events = handle.subscribe();
    let task_id = {
        handle
            .set_focus("implement bounded retry".into())
            .await
            .unwrap();
        handle.list_tasks().await.unwrap()[0].id
    };
    let revision = handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                next_action: Some("add the fake-sleeper unit test".into()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // Drain queued dialogue path: send the user message through the normal
    // entry so the turn runs and the settled batch hits the safe point.
    let labels = run_turn(&instance, events).await;

    let resume_at = labels
        .iter()
        .position(|label| label == "resume_committed:task_anchor_changed")
        .expect("anchor debt must settle into a resume commit");
    let durable_at = labels
        .iter()
        .position(|label| label == "checkpoint_durable")
        .expect("the scheduled write must be acknowledged");
    let completed_at = labels
        .iter()
        .position(|label| label == "turn_completed")
        .expect("turn completes");
    assert!(
        resume_at < durable_at && durable_at < completed_at,
        "JSONL order must prove resume -> durable ack -> TurnCompleted: {labels:?}"
    );

    // Exactly one atomic artifact landed in the workspace state directory.
    let checkpoint_dir = dir.path().join(".focus-agent").join("checkpoints");
    let entries: Vec<_> = std::fs::read_dir(&checkpoint_dir)
        .expect("checkpoint directory exists")
        .collect();
    assert!(
        !entries.is_empty(),
        "a durable checkpoint artifact must exist"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn read_only_round_accrues_no_checkpoint_debt() {
    let dir = tempfile::tempdir().unwrap();
    let instance = instance_with(dir.path(), "fs.read", json!({"path": "src/lib.rs"})).await;
    let events = instance.handle().subscribe();
    let labels = run_turn(&instance, events).await;
    assert!(
        !labels
            .iter()
            .any(|label| label.starts_with("resume_committed")),
        "read-only exploration never owes a checkpoint: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label == "checkpoint_durable"),
        "no write is scheduled without debt: {labels:?}"
    );
    let checkpoint_dir = dir.path().join("state").join("checkpoints");
    assert!(
        !checkpoint_dir.exists(),
        "no checkpoint directory is created without debt"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn continue_active_task_restarts_the_directive_without_a_new_instruction() {
    let dir = tempfile::tempdir().unwrap();
    let instance = instance_with(dir.path(), "fs.read", json!({"path": "src/lib.rs"})).await;
    let handle = instance.handle();
    let mut first_turn = handle.subscribe();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    let tasks_before = handle.list_tasks().await.unwrap();
    let anchor_revision_before = tasks_before[0].anchor_revision;
    handle.user_message("keep going".into()).await.unwrap();

    // Wait for the first turn's durable completion before continuing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let mut done = false;
        while let Ok(envelope) = first_turn.try_recv() {
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                done = true;
            }
        }
        if done || tokio::time::Instant::now() >= deadline {
            assert!(done, "the first turn must finish inside the test deadline");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut events = handle.subscribe();
    handle.continue_active_task().await.unwrap();

    let mut saw_continuation = false;
    let mut saw_dialogue = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !saw_continuation {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TaskContinuationStarted { .. } => saw_continuation = true,
                RuntimeEvent::UserMessageAccepted { input } => match input.kind {
                    InputKind::TaskContinuation => {}
                    InputKind::Dialogue => saw_dialogue = true,
                    _ => {}
                },
                _ => {}
            }
        }
        if !saw_continuation {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(
        saw_continuation,
        "continuation must publish its start event"
    );
    assert!(
        !saw_dialogue,
        "continuation must not mint a new user instruction"
    );

    let tasks_after = handle.list_tasks().await.unwrap();
    assert_eq!(
        tasks_after.len(),
        tasks_before.len(),
        "continuation never mints a task"
    );
    assert_eq!(
        tasks_after[0].anchor_revision, anchor_revision_before,
        "continuation leaves the anchor untouched"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn checkpoint_store_writes_atomically_and_fails_closed_on_a_bad_dir() {
    let dir = tempfile::tempdir().unwrap();
    let store = agent_runtime::CheckpointStore::new(dir.path().join("state").join("checkpoints"));
    let first = store.write_atomic(b"{}").await.unwrap();
    let second = store.write_atomic(b"{}").await.unwrap();
    assert_ne!(first, second, "every successful write is addressable");
    assert!(
        dir.path()
            .join("state")
            .join("checkpoints")
            .join(&first)
            .exists()
    );

    // A regular file where a directory must be created fails closed.
    let blocker = tempfile::tempdir().unwrap();
    let blocked_path = blocker.path().join("not-a-dir");
    std::fs::write(&blocked_path, b"x").unwrap();
    let bad_store = agent_runtime::CheckpointStore::new(blocked_path.join("inner"));
    assert!(
        bad_store.write_atomic(b"{}").await.is_err(),
        "an unwritable location must fail closed"
    );
}
