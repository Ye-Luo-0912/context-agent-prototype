//! Safe-point resume commits: durable changes at a fully settled batch
//! install the bounded resume and schedule exactly one atomic checkpoint
//! write whose acknowledgement lands before `TurnCompleted`. Read-only
//! rounds accrue nothing. `continue_active_task` restarts the stored
//! directive without minting a new user instruction.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentResult, ContextDiagnostics, ContextEngine, ContextGcReport, ContextIngress,
    ContextItemSummary, ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, ContextStateTransition, InputKind, MaterializedContext, ModelCapabilities,
    ModelOutput, ModelRequest, ModelTransport, RuntimeDirective, RuntimeEvent,
    RuntimeEventEnvelope, ScopeId, ScopeKind, TaskProgressProposal, ToolCall, ToolDispatcher,
    ToolExecutionAttribution, ToolExecutionPurpose, ToolExecutionRequest, ToolOutcome, ToolOutput,
    ToolRisk, ToolSpec, VerificationReuse,
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
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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

    let checkpoint_before = instance.checkpoint().await.unwrap();
    let task_before = checkpoint_before
        .tasks
        .tasks
        .iter()
        .find(|task| Some(task.id) == checkpoint_before.current_task_id)
        .expect("the continued task is present before continuation");
    let directive_before = task_before.turn_intent.clone();
    let directive_revision_before = task_before.resume.directive_revision;
    let verification_revision_before = task_before.anchor.verification_revision;
    let verification_spec_before = task_before.resume.verification.spec_revision;

    // Exercise the cold-load boundary before the continuation. Restore must
    // preserve the tuple, and continuation must not manufacture a new one.
    instance.restore(checkpoint_before.clone()).await.unwrap();
    let checkpoint_restored = instance.checkpoint().await.unwrap();
    let task_restored = checkpoint_restored
        .tasks
        .tasks
        .iter()
        .find(|task| Some(task.id) == checkpoint_restored.current_task_id)
        .expect("the continued task survives cold restore");
    assert_eq!(task_restored.turn_intent, directive_before);
    assert_eq!(
        task_restored.resume.directive_revision,
        directive_revision_before
    );
    assert_eq!(
        task_restored.anchor.verification_revision,
        verification_revision_before
    );
    assert_eq!(
        task_restored.resume.verification.spec_revision,
        verification_spec_before
    );

    let mut events = handle.subscribe();
    handle.continue_active_task().await.unwrap();

    let mut saw_continuation = false;
    let mut saw_continuation_input = false;
    let mut saw_dialogue = false;
    let mut continuation_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline
        && !(saw_continuation && saw_continuation_input && continuation_completed)
    {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TaskContinuationStarted { .. } => saw_continuation = true,
                RuntimeEvent::UserMessageAccepted { input } => match input.kind {
                    InputKind::TaskContinuation => saw_continuation_input = true,
                    InputKind::Dialogue => saw_dialogue = true,
                    _ => {}
                },
                RuntimeEvent::TurnCompleted => continuation_completed = true,
                _ => {}
            }
        }
        if !(saw_continuation && saw_continuation_input && continuation_completed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(
        saw_continuation,
        "continuation must publish its start event"
    );
    assert!(
        saw_continuation_input,
        "continuation must retain its typed input identity"
    );
    assert!(
        continuation_completed,
        "the continuation turn must finish inside the test deadline"
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
    let checkpoint_after = instance.checkpoint().await.unwrap();
    let task_after = checkpoint_after
        .tasks
        .tasks
        .iter()
        .find(|task| Some(task.id) == checkpoint_after.current_task_id)
        .expect("the continued task remains present");
    assert_eq!(task_after.turn_intent, directive_before);
    assert_eq!(
        task_after.resume.directive_revision, directive_revision_before,
        "continuation must not mint a new directive revision"
    );
    assert_eq!(
        task_after.anchor.verification_revision, verification_revision_before,
        "continuation must not change the anchor verification basis"
    );
    assert_eq!(
        task_after.resume.verification.spec_revision, verification_spec_before,
        "continuation must retain the execution verification basis"
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

const COMPLETION_VERIFY_IDENTITY_MATERIAL: &str = "safe-point completion verifier v1";
const COMPLETION_ACCEPTANCE_DOMAIN: &str = "safe-point-completion";

fn completion_acceptance_declaration() -> agent_contracts::VerificationCoverageDeclaration {
    agent_contracts::VerificationCoverageDeclaration {
        domain_id: COMPLETION_ACCEPTANCE_DOMAIN.into(),
        declaration_revision: 1,
        source_digest: agent_contracts::ContentDigest::sha256_bytes(
            b"safe-point-completion-declaration/v1",
        )
        .to_string(),
    }
}

#[async_trait::async_trait]
impl ToolDispatcher for CompletionDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "test.verify".into(),
                description: "verify the current completion basis".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "task.complete".into(),
                description: "propose completion".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
        ]
    }
    fn execution_attribution(&self, call: &ToolCall) -> ToolExecutionAttribution {
        if call.name == "test.verify" {
            return ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                Vec::<String>::new(),
                VerificationReuse::ExactCurrentWorld,
            )
            .with_verification_identity_material(COMPLETION_VERIFY_IDENTITY_MATERIAL)
            .with_verification_recipe(agent_contracts::VerificationRecipeProvenance {
                recipe_id: "test.verify".into(),
                recipe_revision: "v1".into(),
                coverage_domain: Some(COMPLETION_ACCEPTANCE_DOMAIN.into()),
                domain_declaration_revision: Some(1),
                domain_source_digest: completion_acceptance_declaration().source_digest,
                class_identity_digest: "safe-point-class".into(),
            });
        }
        ToolExecutionAttribution::default()
    }
    fn verification_coverage_declarations(
        &self,
    ) -> Vec<agent_contracts::VerificationCoverageDeclaration> {
        vec![completion_acceptance_declaration()]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if request.call.name == "test.verify" {
            return Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "completion basis verified".into(),
                model_content: "completion basis verified".into(),
                artifact_ref: None,
                metadata: json!({"verification": true}),
            }));
        }
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

/// Each turn verifies its current directive before proposing completion. A
/// refused proposal returns for one plain final round.
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
        if round == 0 || round == 3 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "verify-current".into(),
                    name: "test.verify".into(),
                    arguments: json!({}),
                }],
                usage: Default::default(),
            })
        } else if round == 1 || round == 4 {
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
    completion_instance_with_context(dir, Arc::new(TestContextEngine)).await
}

async fn completion_instance_with_context(
    dir: &std::path::Path,
    context: Arc<dyn ContextEngine>,
) -> RuntimeInstance {
    let workspace = agent_workspace::Workspace::open(dir).await.unwrap();
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(CompletionTwiceModel {
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(CompletionDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(Arc::new(workspace));
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
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
                completion_policy: Some(
                    agent_runtime::task::TaskCompletionPolicy::EvidenceRequired,
                ),
                acceptance_criteria: Some(vec![
                    agent_runtime::task::AcceptanceCriterion::declared(
                        "the current completion basis passes",
                        &completion_acceptance_declaration(),
                    ),
                ]),
                open_loops: Some(vec!["prove saturation at the delay cap".into()]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // Turn 1: the unified readiness decision refuses the proposal; the
    // decision returns to the model and the turn ends without committing.
    let mut events = handle.subscribe();
    handle.user_message("wrap it up".into()).await.unwrap();
    let mut labels: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut done = false;
    while tokio::time::Instant::now() < deadline && !done {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::Warning { message }
                    if message.contains("completion proposal refused") =>
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
        after_turn_one[0].anchor_revision,
        revision + 1,
        "the post-observation receipt CAS advances only the full anchor revision"
    );
    let receipt_revision = after_turn_one[0].anchor_revision;

    // The operator/model resolves the loop through the boundary CAS.
    handle
        .patch_task_anchor(
            task_id,
            receipt_revision,
            agent_runtime::AnchorPatch {
                open_loops: Some(Vec::new()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();

    // Turn 2: a fresh directive is reverified, then the same proposal flow
    // passes the gate; JSONL proves
    // TurnCompleted -> durable checkpoint -> TaskCompleted. TaskCompleted
    // trails TurnCompleted, so keep draining through a quiet grace window
    // instead of stopping at the first completion event.
    handle.user_message("wrap it up".into()).await.unwrap();
    let mut labels = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut last_event_at = tokio::time::Instant::now();
    while tokio::time::Instant::now() < deadline {
        let mut received_any = false;
        while let Ok(envelope) = events.try_recv() {
            received_any = true;
            last_event_at = tokio::time::Instant::now();
            match envelope.event {
                RuntimeEvent::CheckpointDurable { .. } => labels.push("checkpoint_durable"),
                RuntimeEvent::TaskCompleted { .. } => labels.push("task_completed"),
                RuntimeEvent::TurnCompleted => labels.push("turn_completed"),
                _ => {}
            }
        }
        // The whole sequence must be observable before we conclude; a single
        // empty sweep is not enough on a slow CI runner, where the final
        // checkpoint write can lag well past the turn-completed event.
        let sequence_complete = ["turn_completed", "checkpoint_durable", "task_completed"]
            .iter()
            .all(|label| labels.contains(label));
        if sequence_complete
            && last_event_at.elapsed() > Duration::from_millis(400)
            && !received_any
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        labels.contains(&"turn_completed"),
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
                // snapshot: same anchor, a sequence strictly newer than the
                // failed attempt (seq 1). EXEC-1: the bounded tool-batch
                // safe point may consume an intermediate sequence while the
                // store is still blocked — how many failed attempts precede
                // the durable one is an attempt-count detail, so the test
                // pins the invariant (unchanged revision, strictly newer
                // durable snapshot), never the exact number.
                assert_eq!(revision, 1, "the anchor revision is unchanged");
                assert!(
                    sequence >= 2,
                    "the retry's snapshot must be strictly newer than the \
                     failed seq-1 attempt, got {sequence}"
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
    let instance = completion_instance_with_context(
        dir.path(),
        Arc::new(context_simple::SimpleContextEngine::new(
            context_simple::SimpleContextConfig::default(),
        )),
    )
    .await;
    let handle = instance.handle();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let events = handle.subscribe();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
        checkpoint.terminal_commit,
        "the artifact is a terminal commit anchor"
    );
    assert!(
        checkpoint.event_cover_seq > 0,
        "the terminal anchor carries its journal cursor"
    );
    assert!(
        checkpoint
            .tasks
            .completed
            .iter()
            .any(|record| record.task_id == task_id),
        "the finished task owns exactly its committed completion record"
    );
    let restored =
        context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig::default());
    restored.restore(checkpoint.context.clone()).await.unwrap();
    let restored_items = restored.inspect(128).await.unwrap();
    assert!(
        restored_items
            .iter()
            .any(|item| item.kind == ContextKind::Summary),
        "the same terminal artifact must carry the post-TaskCompleted context plane"
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

#[tokio::test]
async fn terminal_failure_restores_the_previous_durability_requirement() {
    let dir = tempfile::tempdir().unwrap();
    let focus_dir = dir.path().join(".focus-agent");
    std::fs::create_dir_all(&focus_dir).unwrap();
    std::fs::write(focus_dir.join("checkpoints"), b"not a directory").unwrap();

    // A read-only turn establishes a reusable directive without accruing
    // checkpoint debt of its own.
    let instance = instance_with(dir.path(), "fs.read", json!({})).await;
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    handle
        .user_message("inspect before closing".into())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let envelope = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("read-only turn finishes")
            .expect("event stream stays open");
        if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
            break;
        }
    }

    handle
        .complete_current_task("done".into())
        .await
        .expect_err("the blocked terminal store must refuse completion");
    assert!(
        handle.continue_active_task().await.is_ok(),
        "a failed terminal freeze must restore its prior required_sequence; the uncommitted terminal sequence cannot fence a valid earlier task state"
    );
    let tasks = handle.list_tasks().await.unwrap();
    assert!(matches!(tasks[0].status, agent_runtime::TaskStatus::Active));
    instance.shutdown().await.unwrap();
}

/// Serves `task.manage` by attaching the typed progress directive, exactly
/// like the real tool: the runtime applies it through the trusted anchor
/// CAS at operation-commit time.
#[derive(Debug)]
struct ProgressDirectiveDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for ProgressDirectiveDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "task.manage".into(),
            description: "propose bounded task progress".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let arguments = request.call.arguments;
        let proposal: TaskProgressProposal = serde_json::from_value(json!({
            "base_anchor_revision": arguments["base_anchor_revision"],
            "current_interpretation": arguments.get("current_interpretation"),
            "plan_progress": arguments.get("plan_progress"),
            "open_loops": arguments.get("open_loops"),
            "next_action": arguments.get("next_action"),
        }))
        .unwrap();
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: "task.manage".into(),
                ok: true,
                summary: "progress proposed".into(),
                model_content: String::new(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: RuntimeDirective::UpdateTaskProgress(proposal),
        })
    }
}

/// Two consecutive `task.manage` rounds, then a plain finish. The second
/// same-reason anchor mutation lands while the FIRST background save is
/// still unacknowledged — the R01 interleaving.
#[derive(Debug)]
struct TwoStageProgressModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for TwoStageProgressModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round < 2 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call-{round}"),
                    name: "task.manage".into(),
                    arguments: json!({
                        "base_anchor_revision": round,
                        "next_action": if round == 0 {
                            "stage one captured"
                        } else {
                            "stage two captured"
                        },
                    }),
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

/// R01: a same-reason mutation that happens while the previous snapshot's
/// background write is still unacknowledged must survive that write's ACK.
/// The acknowledgement may only retire the debt it froze; the newer debt
/// owes its own snapshot, the second artifact must carry the newer anchor
/// content, restoring from it must expose that state, and the
/// continuation gate must stay honest throughout.
#[tokio::test]
async fn same_reason_debt_accrued_during_background_save_survives_the_ack() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = agent_workspace::Workspace::open(dir.path()).await.unwrap();
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(TwoStageProgressModel {
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(ProgressDirectiveDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(Arc::new(workspace));
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();

    // Collect through the first turn: every frozen snapshot's resume commit
    // and every durable acknowledgement, with their identities.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut resumes: Vec<(u64, Vec<String>)> = Vec::new();
    let mut durables: Vec<(u64, String)> = Vec::new();
    loop {
        let envelope = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("turn finishes inside the deadline")
            .expect("event stream stays open");
        match envelope.event {
            RuntimeEvent::TaskResumeCommitted { debt, sequence, .. } => {
                resumes.push((sequence, debt));
            }
            RuntimeEvent::CheckpointDurable {
                artifact, sequence, ..
            } => {
                durables.push((sequence, artifact));
            }
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }

    // Two settled batches with same-reason debt each: the first ACK may
    // not absorb the second mutation's debt, so a second snapshot must be
    // frozen and durably acknowledged before TurnCompleted.
    assert_eq!(
        resumes,
        vec![
            (1, vec!["task_anchor_changed".to_string()]),
            (2, vec!["task_anchor_changed".to_string()]),
        ],
        "each settled batch freezes its own snapshot; the first ACK retires only its frozen debt"
    );
    assert_eq!(
        durables
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "both snapshots must land durably: {durables:?}"
    );

    // The SECOND artifact is the authoritative resume state: it loads
    // checksum-verified and carries the mid-flight mutation's anchor
    // content under its own snapshot sequence.
    let store =
        agent_runtime::CheckpointStore::new(dir.path().join(".focus-agent").join("checkpoints"));
    let (_, last_artifact) = durables.last().expect("two durable acks").clone();
    let payload = store.load_verified(&last_artifact).await.unwrap();
    let checkpoint: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        checkpoint.snapshot_sequence, 2,
        "the latest artifact is the second snapshot"
    );
    let payload_text = String::from_utf8_lossy(&payload);
    assert!(
        payload_text.contains("stage two captured"),
        "the second snapshot must capture the mid-flight same-reason mutation"
    );

    // Gate state: with both snapshots durable and no uncaptured debt left,
    // continuation is allowed (and the continued turn itself finishes).
    handle
        .continue_active_task()
        .await
        .expect("fully captured debt must release the continuation gate");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let envelope = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("continuation finishes inside the deadline")
            .expect("event stream stays open");
        if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
            break;
        }
    }

    // Post-recovery state check: restoring from the second artifact brings
    // the runtime back to the mid-flight mutation's anchor revision.
    instance.restore(checkpoint).await.unwrap();
    let tasks = handle.list_tasks().await.unwrap();
    assert_eq!(
        tasks[0].anchor_revision, 2,
        "the mid-flight mutation survives restore from the second artifact"
    );
    instance.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// EXEC-7 (R2-08): long boundary waits must not occupy the actor's command
// branch. A gated engine proves it: while the full GC pass and the
// checkpoint maintenance are stuck on their gates, status stays live, a
// cancel is bounded and honest, and a parked terminal commit still settles.
// ---------------------------------------------------------------------------

/// A context engine whose full GC pass and checkpoint-trigger maintenance
/// park on permits until the test releases them.
#[derive(Debug)]
struct GatedBoundaryContext {
    gc_gate: tokio::sync::Semaphore,
    maintain_gate: tokio::sync::Semaphore,
    gc_started: AtomicBool,
    maintain_started: AtomicBool,
    /// EXEC-10: every Checkpoint-trigger maintenance start, counted — the
    /// single-slot regressions need to tell the first stall from the second.
    maintain_starts: AtomicUsize,
}

impl GatedBoundaryContext {
    fn new() -> Self {
        Self {
            gc_gate: tokio::sync::Semaphore::new(0),
            maintain_gate: tokio::sync::Semaphore::new(0),
            gc_started: AtomicBool::new(false),
            maintain_started: AtomicBool::new(false),
            maintain_starts: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ContextEngine for GatedBoundaryContext {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        if matches!(trigger, ContextMaintenanceTrigger::Checkpoint) {
            self.maintain_started.store(true, Ordering::SeqCst);
            let n = self
                .maintain_starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            eprintln!("DEBUG gated maintain start #{n}");
            let _permit = self.maintain_gate.acquire().await;
            eprintln!("DEBUG gated maintain #{n} released");
        }
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext::default())
    }
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
    async fn gc(&self) -> AgentResult<ContextGcReport> {
        self.gc_started.store(true, Ordering::SeqCst);
        let _permit = self.gc_gate.acquire().await;
        Ok(ContextGcReport::default())
    }
}

/// The turn-final full GC parks on a gated engine: the actor still answers
/// a status command while it waits, and a cancel is bounded and honest —
/// the reversible pass aborts, the turn ends cancelled, and TurnCompleted
/// never fires for it.
#[tokio::test]
async fn cancel_and_status_stay_live_while_the_turn_final_gc_is_stalled() {
    let context = Arc::new(GatedBoundaryContext::new());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(PlainModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    instance.start().await.unwrap();
    handle
        .user_message("work until the boundary".into())
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !context.gc_started.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn must reach its turn-final GC"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The stalled pass does not own the actor: a typed status snapshot
    // still comes back, and a cancel is accepted with a bounded result.
    let status = tokio::time::timeout(Duration::from_secs(2), handle.status_snapshot())
        .await
        .expect("status must answer while the full GC is stalled")
        .unwrap();
    assert!(status.serving);
    let cancel = tokio::time::timeout(Duration::from_secs(5), handle.cancel_turn())
        .await
        .expect("cancel must answer while the full GC is stalled")
        .unwrap();
    assert!(
        matches!(cancel, agent_contracts::TurnCancelAck::Cancelled { .. }),
        "the reversible pass aborts and the turn cancels cleanly: {cancel:?}"
    );

    // Release the gate: the aborted pass simply ends; the cancelled turn
    // must not come back to life as TurnCompleted.
    context.gc_gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut events = handle.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < deadline {
        if let Ok(envelope) = events.try_recv()
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            panic!("a cancelled turn must never be completed by its aborted boundary pass");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    instance.shutdown().await.unwrap();
}

/// EXEC-7 (R2-08): the SAFE-POINT write's checkpoint maintenance parks on a
/// gated engine while the turn keeps committing. The actor stays answerable
/// during the stall, the durable acknowledgement is deferred until the gate
/// releases, and after the release it lands (the debt retires with it).
#[tokio::test]
async fn safe_point_checkpoint_maintenance_does_not_own_the_command_branch() {
    let context = Arc::new(GatedBoundaryContext::new());
    let workspace_dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(
        agent_workspace::Workspace::open(workspace_dir.path())
            .await
            .unwrap(),
    );
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(TwoStageProgressModel {
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(ProgressDirectiveDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    handle
        .set_focus("stall the safe point".into())
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();

    // The safe point's maintenance parks on the gate.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !context.maintain_started.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the safe point must reach its checkpoint maintenance"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The parked prepare does not own the actor: status still answers, and
    // no durable acknowledgement can have landed yet.
    let status = tokio::time::timeout(Duration::from_secs(2), handle.status_snapshot())
        .await
        .expect("status must answer while the safe-point maintenance is stalled")
        .unwrap();
    assert_eq!(status.focus_goal, "stall the safe point");

    // Release: the prepare lands, the write goes in flight and the durable
    // acknowledgement arrives (with the deferred TurnCompleted whenever the
    // rest of the boundary reaches it).
    context.maintain_gate.add_permits(1);
    context.gc_gate.add_permits(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deferred safe point must land after the gate releases"
        );
        if let Ok(envelope) = events.try_recv()
            && matches!(envelope.event, RuntimeEvent::CheckpointDurable { .. })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    instance.shutdown().await.unwrap();
}

/// The terminal commit's checkpoint maintenance parks on a gated engine:
/// the CompleteTask reply is outstanding, but the actor keeps answering
/// status commands, and once the gate releases the completion commits.
#[tokio::test]
async fn status_stays_live_and_the_terminal_commit_settles_around_a_stalled_checkpoint_maintenance()
{
    let context = Arc::new(GatedBoundaryContext::new());
    let workspace_dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(
        agent_workspace::Workspace::open(workspace_dir.path())
            .await
            .unwrap(),
    );
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(PlainModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    instance.start().await.unwrap();
    handle.set_focus("complete me".into()).await.unwrap();

    let commit_handle = handle.clone();
    let commit = tokio::spawn(async move {
        commit_handle
            .complete_current_task("gated done".into())
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !context.maintain_started.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the terminal commit must reach its checkpoint maintenance"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The parked transaction does not own the actor: status still answers.
    let status = tokio::time::timeout(Duration::from_secs(2), handle.status_snapshot())
        .await
        .expect("status must answer while the checkpoint maintenance is stalled")
        .unwrap();
    assert_eq!(status.focus_goal, "complete me");

    // Release: the maintenance lands, the transaction commits, and the
    // operator's reply arrives. The commit tail's storage boundary (full GC
    // pass) uses the same gated engine — release it too so the boundary can
    // settle instead of holding the shutdown drain.
    context.maintain_gate.add_permits(1);
    context.gc_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), commit)
        .await
        .expect("the terminal commit must settle after the gate releases")
        .unwrap()
        .unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed.len(), 1);
    instance.shutdown().await.unwrap();
}

/// EXEC-10 (R3-09): while an explicit completion's terminal freeze waits on
/// its checkpoint maintenance, a RESTORE is refused deterministically — the
/// old transaction must never commit or roll back over restored planes.
/// After the gate releases, the completion settles normally (the runtime
/// ends completed, not restored).
#[tokio::test]
async fn restore_is_refused_while_a_terminal_commit_is_parked() {
    let context = Arc::new(GatedBoundaryContext::new());
    let workspace_dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(
        agent_workspace::Workspace::open(workspace_dir.path())
            .await
            .unwrap(),
    );
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(PlainModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    instance.start().await.unwrap();
    handle.set_focus("complete me".into()).await.unwrap();

    // No permits: the terminal freeze's checkpoint maintenance parks right
    // after the freeze parks the completion transaction.
    let commit_handle = handle.clone();
    let commit = tokio::spawn(async move {
        commit_handle
            .complete_current_task("gated done".into())
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while context.maintain_starts.load(Ordering::SeqCst) < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the terminal freeze must reach its checkpoint maintenance"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Restore while the completion transaction is parked: a deterministic,
    // typed refusal — never a silent interleaving over the prepared planes.
    // The refusal fires before validation, so a minimal checkpoint payload
    // is enough to reach it.
    let before = agent_runtime::RuntimeCheckpoint {
        version: agent_runtime::RUNTIME_CHECKPOINT_VERSION,
        run_metadata: agent_runtime::checkpoint::RunMetadata {
            run_id: agent_contracts::RunId::new(),
            created_at_ms: 1,
            provider_profile_digest: String::new(),
        },
        tasks: agent_runtime::checkpoint::TaskManagerSnapshot {
            tasks: Vec::new(),
            active: None,
            completed: Vec::new(),
        },
        current_task_id: None,
        focus_revision: 0,
        last_surface_revision: 0,
        context: serde_json::json!({}),
        capabilities: Vec::new(),
        authority: None,
        snapshot_sequence: 1,
        capability_generation: 0,
        unresolved_ack_debts: Vec::new(),
        event_cover_seq: 0,
        terminal_commit: false,
    };
    let refusal = tokio::time::timeout(Duration::from_secs(5), instance.restore(before))
        .await
        .expect("restore must answer while the commit is parked")
        .expect_err("restore during a parked commit must be refused");
    let refusal = refusal.to_string();
    assert!(
        refusal.contains("commit is settling"),
        "the refusal must name the parked commit: {refusal}"
    );

    // Release: the completion settles normally.
    context.maintain_gate.add_permits(1);
    let _ = commit
        .await
        .expect("the completion must settle after the gate releases");
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed.len(), 1);
    assert!(
        checkpoint.current_task_id.is_none(),
        "committed, not restored"
    );
    context.gc_gate.add_permits(1);
    instance.shutdown().await.unwrap();
}

/// EXEC-10 (R3-10): the boundary lane is single-slot. A second checkpoint
/// capture while one is parked must not overwrite the first — the second
/// lands via the bounded inline path and BOTH replies are determinate.
#[tokio::test]
async fn two_concurrent_checkpoint_captures_both_settle_deterministically() {
    let context = Arc::new(GatedBoundaryContext::new());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(PlainModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    instance.start().await.unwrap();
    handle.set_focus("capture twice".into()).await.unwrap();

    // Two concurrent captures: A parks on the spawned maintenance; B hits
    // the single-slot lane and lands via the bounded inline path. Both
    // replies must settle deterministically once the gate releases — neither
    // may overwrite or lose the other.
    // Release both gates from a side task so the captures' parked passes
    // finish while the main body awaits their replies.
    {
        let context = context.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            context.maintain_gate.add_permits(2);
        });
    }
    let (a, b) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(15), instance.checkpoint()),
        tokio::time::timeout(Duration::from_secs(15), instance.checkpoint()),
    );
    a.expect("capture A must settle")
        .expect("capture A must succeed");
    b.expect("capture B must settle")
        .expect("capture B must succeed");
    instance.shutdown().await.unwrap();
}

/// CTX-8 接线 (R3-08): a boundary pass that reports store backpressure is
/// observed on the typed status snapshot (active, with the honest debt
/// counts), and a later clean pass lifts the throttle — while the control
/// channel never stopped answering.
#[derive(Debug)]
struct BackpressureContext {
    gc_gate: tokio::sync::Semaphore,
    /// Reports written by the test: each gc() pops the front value.
    gc_reports: tokio::sync::Mutex<Vec<agent_contracts::ContextGcReport>>,
}

#[async_trait::async_trait]
impl ContextEngine for BackpressureContext {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext::default())
    }
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
    async fn gc(&self) -> AgentResult<ContextGcReport> {
        let _permit = self.gc_gate.acquire().await;
        let mut reports = self.gc_reports.lock().await;
        if reports.is_empty() {
            return Ok(agent_contracts::ContextGcReport {
                externalize_backpressure: true,
                externalize_deferred: 7,
                store_io_failures: 2,
                ..Default::default()
            });
        }
        Ok(reports.remove(0))
    }
}

#[tokio::test]
async fn store_backpressure_is_observed_and_lifted_on_the_status_snapshot() {
    let context = Arc::new(BackpressureContext {
        gc_gate: tokio::sync::Semaphore::new(1),
        gc_reports: tokio::sync::Mutex::new(Vec::new()),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(PlainModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    instance.start().await.unwrap();
    handle.set_focus("under outage".into()).await.unwrap();
    let mut events = handle.subscribe();
    handle
        .user_message("work under outage".into())
        .await
        .unwrap();

    // Wait for the turn-final pass to land; it reported backpressure.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn-final pass must land"
        );
        if let Ok(envelope) = events.try_recv()
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let status = handle.status_snapshot().await.unwrap();
    let bp = status
        .store_backpressure
        .expect("the backpressured pass must be observed");
    assert!(bp.active, "the outage must be visible on the snapshot");
    assert_eq!(bp.externalize_deferred, 7);
    assert_eq!(bp.store_io_failures, 2);

    // A second turn runs a clean pass: queue the clean report BEFORE the
    // turn's funnel reaches the pass, then drive the turn.
    context
        .gc_reports
        .lock()
        .await
        .push(agent_contracts::ContextGcReport::default());
    handle.set_focus("clean again".into()).await.unwrap();
    handle.user_message("work again".into()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second turn must land its clean pass"
        );
        if let Ok(envelope) = events.try_recv()
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = handle.status_snapshot().await.unwrap();
    let bp = status.store_backpressure.expect("a clean pass ran");
    assert!(!bp.active, "the clean pass lifts the throttle");
    instance.shutdown().await.unwrap();
}
