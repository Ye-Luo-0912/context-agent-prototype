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
    assert_ne!(
        first.artifact, second.artifact,
        "every successful write is addressable"
    );
    assert!(
        dir.path()
            .join("state")
            .join("checkpoints")
            .join(&first.artifact)
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

/// Serves `task.complete` by attaching the typed completion directive,
/// exactly like the real tool.
#[derive(Debug)]
struct CompletionDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for CompletionDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "task.complete".into(),
            description: "propose completion".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: "task.complete".into(),
                ok: true,
                summary: "completion proposed".into(),
                model_content: String::new(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary: "the retry policy is done".into(),
                    artifacts: Vec::new(),
                },
            ),
        })
    }
}

/// Round pattern per turn: complete, done, complete, done ...
#[derive(Debug)]
struct CompletionTwiceModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for CompletionTwiceModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round.is_multiple_of(2) {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call-{round}"),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "done"}),
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

async fn completion_instance(dir: &std::path::Path) -> RuntimeInstance {
    let workspace = agent_workspace::Workspace::open(dir).await.unwrap();
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionTwiceModel {
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(CompletionDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(Arc::new(workspace));
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.handle().start().await.unwrap();
    instance
}

#[tokio::test]
async fn open_loops_return_completion_to_the_model_until_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let instance = completion_instance(dir.path()).await;
    let handle = instance.handle();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    // An explicit open loop exists before the first completion attempt.
    let revision = handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                open_loops: Some(vec!["prove saturation at the delay cap".into()]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // Turn 1: the proposal is stored but the gate refuses it; the decision
    // returns to the model and the turn ends without committing the task.
    let mut events = handle.subscribe();
    handle.user_message("wrap it up".into()).await.unwrap();
    let mut labels: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut done = false;
    while tokio::time::Instant::now() < deadline && !done {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::Warning { message }
                    if message.contains("completion gate refused") =>
                {
                    labels.push(format!("gate_refused:{message}"));
                }
                RuntimeEvent::TaskCompleted { .. } => labels.push("task_completed".into()),
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
    assert!(done, "turn one must finish inside the test deadline");
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("gate_refused:") && label.contains("open loop")),
        "the open-loop refusal must surface once: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label == "task_completed"),
        "a gated completion must never commit: {labels:?}"
    );
    let after_turn_one = handle.list_tasks().await.unwrap();
    assert_eq!(
        after_turn_one[0].anchor_revision, 1,
        "the anchor keeps its open loop while the gate holds"
    );

    // The operator/model resolves the loop through the boundary CAS.
    handle
        .patch_task_anchor(
            task_id,
            1,
            agent_runtime::AnchorPatch {
                open_loops: Some(Vec::new()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();

    // Turn 2: the same proposal flow now passes the gate; JSONL proves
    // TurnCompleted -> durable checkpoint -> TaskCompleted. TaskCompleted
    // trails TurnCompleted, so keep draining through a quiet grace window
    // instead of stopping at the first completion event.
    handle.user_message("wrap it up".into()).await.unwrap();
    let mut labels = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut turn_completed_at: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < deadline {
        let mut quiet = true;
        while let Ok(envelope) = events.try_recv() {
            quiet = false;
            match envelope.event {
                RuntimeEvent::CheckpointDurable { .. } => labels.push("checkpoint_durable"),
                RuntimeEvent::TaskCompleted { .. } => labels.push("task_completed"),
                RuntimeEvent::TurnCompleted => {
                    labels.push("turn_completed");
                    turn_completed_at.get_or_insert(tokio::time::Instant::now());
                }
                _ => {}
            }
        }
        if turn_completed_at
            .is_some_and(|seen| seen.elapsed() > Duration::from_millis(400) && quiet)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        turn_completed_at.is_some(),
        "turn two must finish inside the test deadline"
    );
    let turn_at = labels
        .iter()
        .position(|label| *label == "turn_completed")
        .expect("turn completes");
    // A mid-turn safe point may also land a write; the gate requirement is
    // that the FINAL durable acknowledgement precedes the completed report.
    let durable_at = labels
        .iter()
        .rposition(|label| *label == "checkpoint_durable")
        .expect("the final checkpoint lands durably");
    let completed_at = labels
        .iter()
        .position(|label| *label == "task_completed")
        .expect("the task completes once loops resolve");
    assert!(
        turn_at < durable_at && durable_at < completed_at,
        "final checkpoint order must be provable: {labels:?}"
    );
    let tasks = handle.list_tasks().await.unwrap();
    let closed = tasks
        .iter()
        .find(|task| task.id == task_id)
        .expect("the completed task stays listed");
    assert!(
        matches!(closed.status, agent_runtime::TaskStatus::Completed),
        "the task must be closed after the gate passes: {:?}",
        closed.status
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_ack_carries_revision_artifact_and_verifiable_payload() {
    let dir = tempfile::tempdir().unwrap();
    let instance = instance_with(
        dir.path(),
        "task.manage",
        json!({"base_anchor_revision": 0, "next_action": "record progress"}),
    )
    .await;
    let handle = instance.handle();
    let mut events = handle.subscribe();
    let task_id = {
        handle
            .set_focus("implement bounded retry".into())
            .await
            .unwrap();
        handle.list_tasks().await.unwrap()[0].id
    };
    handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                next_action: Some("record progress".into()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();

    // Collect until the turn ends, keeping full envelopes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut durable = None;
    let mut resume_revisions = Vec::new();
    loop {
        let envelope = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("turn finishes inside the deadline")
            .expect("event stream stays open");
        match envelope.event {
            RuntimeEvent::TaskResumeCommitted {
                anchor_revision, ..
            } => {
                resume_revisions.push(anchor_revision);
            }
            RuntimeEvent::CheckpointDurable {
                bytes,
                artifact,
                revision,
                checksum,
                sequence,
                ..
            } => {
                durable = Some((bytes, artifact, revision, checksum, sequence));
            }
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }
    assert_eq!(
        resume_revisions,
        vec![1],
        "the anchor patch drives the debt"
    );
    let (_bytes, artifact, revision, checksum, sequence) =
        durable.expect("the write must be acknowledged");
    assert_eq!(revision, 1, "the legacy field names the anchor revision");
    assert_eq!(sequence, 1, "the ack names the snapshot sequence it covers");
    assert!(!artifact.is_empty());
    assert_eq!(checksum.len(), 64, "the ack pins a sha256 digest");

    // The acknowledged artifact loads back, checksum-verified, and its
    // payload deserializes into the current checkpoint shape.
    let store =
        agent_runtime::CheckpointStore::new(dir.path().join(".focus-agent").join("checkpoints"));
    let payload = store.load_verified(&artifact).await.unwrap();
    let checkpoint: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        checkpoint.version,
        agent_runtime::RUNTIME_CHECKPOINT_VERSION
    );
    assert_eq!(
        checkpoint.snapshot_sequence, 1,
        "the persisted allocator watermark matches the acknowledged snapshot"
    );
    let payload_text = String::from_utf8_lossy(&payload);
    assert!(
        payload_text.contains("record progress"),
        "the artifact carries the installed resume knowledge"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_checkpoint_write_fences_continuation_until_a_retry_lands() {
    let dir = tempfile::tempdir().unwrap();
    // Block the store with a regular file before the runtime writes.
    let checkpoints_dir = dir.path().join(".focus-agent");
    std::fs::create_dir_all(&checkpoints_dir).unwrap();
    std::fs::write(checkpoints_dir.join("checkpoints"), b"not a directory").unwrap();

    let instance = instance_with(
        dir.path(),
        "task.manage",
        json!({"base_anchor_revision": 0, "next_action": "record progress"}),
    )
    .await;
    let handle = instance.handle();
    let mut events = handle.subscribe();
    let task_id = {
        handle
            .set_focus("implement bounded retry".into())
            .await
            .unwrap();
        handle.list_tasks().await.unwrap()[0].id
    };
    handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                next_action: Some("record progress".into()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();

    // The safe-point write fails; nothing claims resumability.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_failed = false;
    loop {
        let envelope = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("turn finishes inside the deadline")
            .expect("event stream stays open");
        match envelope.event {
            RuntimeEvent::CheckpointWriteFailed { reason } => {
                saw_failed = true;
                assert!(
                    reason.contains("rename failed")
                        || reason.contains("write failed")
                        || reason.contains("dir unavailable"),
                    "the failure names its cause: {reason}"
                );
            }
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }
    assert!(saw_failed, "the blocked store must surface a write failure");

    // Continuation is fenced while the durability watermark is unmet.
    let refusal = handle
        .continue_active_task()
        .await
        .expect_err("continuation must fail closed on a failed write");
    assert!(
        refusal.to_string().contains("never landed durably"),
        "the fence names the missing durability: {refusal}"
    );

    // Repairing the store and retrying at the next settled batch releases
    // the fence: a new turn settles, the write lands, continuation passes.
    std::fs::remove_file(checkpoints_dir.join("checkpoints")).unwrap();
    let mut repaired_events = handle.subscribe();
    handle.user_message("one more round".into()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut durable_seen = false;
    loop {
        let envelope = tokio::time::timeout_at(deadline, repaired_events.recv())
            .await
            .expect("retry turn finishes inside the deadline")
            .expect("event stream stays open");
        match envelope.event {
            RuntimeEvent::CheckpointDurable {
                revision, sequence, ..
            } => {
                durable_seen = true;
                // The retry captured the SAME anchor revision under a fresh
                // snapshot: same anchor, distinct (higher) sequence.
                assert_eq!(
                    (revision, sequence),
                    (1, 2),
                    "the retry acknowledges a new snapshot of the unchanged anchor"
                );
            }
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }
    assert!(durable_seen, "the retried write must be acknowledged");

    handle
        .continue_active_task()
        .await
        .expect("a landed watermark releases the continuation fence");
    instance.shutdown().await.unwrap();
}

/// Snapshot identity stays honest across task switches and repeat debt
/// cycles: every frozen snapshot allocates a strictly increasing sequence,
/// including two snapshots taken under identical anchor revisions from
/// different tasks, so durability order can never alias or move backwards.
#[tokio::test]
async fn snapshot_sequences_increase_across_tasks_and_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let instance = instance_with(
        dir.path(),
        "task.manage",
        json!({"base_anchor_revision": 0, "next_action": "advance"}),
    )
    .await;
    let handle = instance.handle();
    let mut events = handle.subscribe();

    // Two independent tasks, both patched to anchor revision 1.
    handle
        .set_focus("first bounded retry".into())
        .await
        .unwrap();
    let first = handle.list_tasks().await.unwrap()[0].id;
    handle
        .set_focus("second bounded retry".into())
        .await
        .unwrap();
    let tasks_now = handle.list_tasks().await.unwrap();
    let second = tasks_now
        .iter()
        .find(|t| t.id != first)
        .expect("a second task")
        .id;

    let mut pairs = Vec::new();
    for task in [first, second] {
        let revision = handle
            .patch_task_anchor(
                task,
                0,
                agent_runtime::AnchorPatch {
                    next_action: Some("advance".into()),
                    ..agent_runtime::AnchorPatch::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(revision, 1);
        pairs.push((task, revision));
    }
    // Only the ACTIVE task's batch settles into a resume commit: one turn,
    // one snapshot. Collect its sequence.
    handle.user_message("one round".into()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut seen_sequences: Vec<(u64, u64)> = Vec::new(); // (anchor, sequence)
    loop {
        let envelope = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("turn finishes inside the deadline")
            .expect("event stream stays open");
        match envelope.event {
            RuntimeEvent::TaskResumeCommitted {
                anchor_revision,
                sequence,
                ..
            } => {
                seen_sequences.push((anchor_revision, sequence));
            }
            RuntimeEvent::CheckpointDurable {
                revision, sequence, ..
            } => {
                let _ = (revision, sequence);
            }
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }
    assert_eq!(
        seen_sequences.len(),
        1,
        "one settled batch freezes one snapshot"
    );
    assert_eq!(
        seen_sequences[0],
        (1, 1),
        "the first snapshot sits at anchor 1 / sequence 1"
    );

    // The inactive task's debt cycle (its own anchor patched to 2) settles
    // only once it becomes... the handler surfaces global debt, so the next
    // turn freezes ANOTHER snapshot for the active task under a higher
    // sequence even though nothing about IT moved. This is precisely the
    // decoupling under test: durable order follows snapshot allocation,
    // never an anchor revision.
    handle.set_focus("third segment".into()).await.unwrap();
    let third = handle.list_tasks().await.unwrap()[0].id;
    let _ = (second, third);
    handle
        .patch_task_anchor(
            second,
            1,
            agent_runtime::AnchorPatch {
                next_action: Some("advance again".into()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();

    handle.user_message("another round".into()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let envelope = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("turn finishes inside the deadline")
            .expect("event stream stays open");
        match envelope.event {
            RuntimeEvent::TaskResumeCommitted {
                anchor_revision,
                sequence,
                ..
            } => {
                seen_sequences.push((anchor_revision, sequence));
            }
            RuntimeEvent::CheckpointDurable { sequence, .. } => {
                let _ = sequence;
            }
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }

    let seqs: Vec<u64> = seen_sequences.iter().map(|(_, s)| *s).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "snapshots allocate strictly increasing sequences: {seen_sequences:?}"
    );
    assert_eq!(
        seen_sequences[0],
        (1, 1),
        "first observed identity stays (anchor 1, sequence 1)"
    );
    instance.shutdown().await.unwrap();
}

/// The acknowledged TERMINAL snapshot must be a real durable fact: it loads
/// checksum-verified, passes full validation, carries no active authority,
/// owns the finished task's completion record, and its acknowledgement is
/// published before `TaskCompleted`.
#[tokio::test]
async fn final_terminal_artifact_loads_verified_and_names_no_active_task() {
    let dir = tempfile::tempdir().unwrap();
    let instance = completion_instance(dir.path()).await;
    let handle = instance.handle();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let events = handle.subscribe();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let collector = tokio::spawn({
        let mut events = events;
        async move {
            let mut last_artifact = None;
            let mut ordered = true;
            loop {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Ok(envelope)) => match envelope.event {
                        RuntimeEvent::CheckpointDurable { artifact, .. } => {
                            last_artifact = Some(artifact);
                        }
                        RuntimeEvent::TaskCompleted { .. } => {
                            if last_artifact.is_none() {
                                ordered = false;
                            }
                            return (last_artifact, ordered);
                        }
                        _ => {}
                    },
                    Ok(Err(_)) => break,
                    Err(_) => break,
                }
            }
            (last_artifact, ordered)
        }
    });

    handle
        .complete_current_task("the retry policy is done".into())
        .await
        .expect("/done must succeed once the terminal ack lands");
    let (artifact, ordered) = collector.await.unwrap();
    assert!(ordered, "the durable ack must precede TaskCompleted");
    assert!(
        artifact.is_some(),
        "the terminal write must be acknowledged"
    );

    let store =
        agent_runtime::CheckpointStore::new(dir.path().join(".focus-agent").join("checkpoints"));
    let payload = store
        .load_verified(artifact.as_deref().unwrap())
        .await
        .unwrap();
    let checkpoint: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&payload).unwrap();
    checkpoint
        .validate()
        .expect("the terminal snapshot validates");
    assert!(
        checkpoint.current_task_id.is_none() && checkpoint.tasks.active.is_none(),
        "terminal authority must be cleared consistently"
    );
    assert!(
        checkpoint.snapshot_sequence >= 1,
        "the terminal snapshot allocates its own sequence"
    );
    assert!(
        checkpoint
            .tasks
            .completed
            .iter()
            .any(|record| record.task_id == task_id),
        "the finished task owns exactly its committed completion record"
    );
    instance.shutdown().await.unwrap();
}

/// With the store path blocked, phase P fails closed: `/done` surfaces the
/// typed error and the task stays active/completion-pending.
#[tokio::test]
async fn blocked_terminal_write_leaves_the_task_completion_pending() {
    let dir = tempfile::tempdir().unwrap();
    let checkpoints_dir = dir.path().join(".focus-agent");
    std::fs::create_dir_all(&checkpoints_dir).unwrap();
    std::fs::write(checkpoints_dir.join("checkpoints"), b"not a directory").unwrap();

    let instance = completion_instance(dir.path()).await;
    let handle = instance.handle();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;

    let refusal = handle
        .complete_current_task("the retry policy is done".into())
        .await
        .expect_err("a blocked terminal write must fail closed");
    let refusal_text = refusal.to_string();
    assert!(
        refusal_text.contains("never landed durably")
            || refusal_text.contains("stays completion-pending"),
        "the fence names the missing durability: {refusal_text}"
    );

    let tasks = handle.list_tasks().await.unwrap();
    let pending = tasks.iter().find(|task| task.id == task_id).unwrap();
    assert!(
        matches!(pending.status, agent_runtime::TaskStatus::Active),
        "the task must stay completion-pending: {:?}",
        pending.status
    );
    instance.shutdown().await.unwrap();
}
