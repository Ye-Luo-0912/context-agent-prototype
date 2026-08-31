//! Bounded task progress proposals: `task.manage` attaches a typed
//! directive that the runtime applies through the trusted anchor CAS at
//! operation-commit time, writing the authoritative outcome back into the
//! model-visible result. A stale base revision refuses without touching
//! task state, so the model can re-read and retry from the reported
//! current revision.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeDirective,
    RuntimeEvent, RuntimeEventEnvelope, TaskProgressProposal, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;

use crate::harness::*;

/// Serves `task.manage` by attaching the typed progress directive, exactly
/// like the real tool.
#[derive(Debug)]
struct ProgressToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for ProgressToolDispatcher {
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
                tool_name: request.call.name,
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

/// Calls `task.manage` with the given arguments on round 0, then finishes.
#[derive(Debug)]
struct ProgressProposalModel {
    arguments: serde_json::Value,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for ProgressProposalModel {
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
                    name: "task.manage".into(),
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

async fn progress_instance(
    arguments: serde_json::Value,
) -> (
    RuntimeInstance,
    tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) {
    let model = Arc::new(ProgressProposalModel {
        arguments,
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(ProgressToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let events = handle.subscribe();
    (instance, events)
}

/// Drain events until the turn completes, keeping every progress/anchor
/// observation in arrival order.
async fn run_progress_turn(
    instance: &RuntimeInstance,
    mut events: tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> Vec<(String, u64, bool, Vec<String>)> {
    let handle = instance.handle();
    handle.start().await.unwrap();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    handle
        .user_message("continue the long task".into())
        .await
        .unwrap();

    let mut observations = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut turn_done = false;
    while tokio::time::Instant::now() < deadline && !turn_done {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TaskAnchorChanged {
                    task_id,
                    revision,
                    changed_fields,
                    ..
                } => {
                    observations.push((format!("anchor:{task_id}"), revision, true, changed_fields))
                }
                RuntimeEvent::TaskProgressUpdated {
                    task_id,
                    accepted,
                    anchor_revision,
                    changed_fields,
                    reason,
                } => observations.push((
                    format!("{reason}:{task_id}"),
                    anchor_revision,
                    accepted,
                    changed_fields,
                )),
                RuntimeEvent::TurnCompleted => turn_done = true,
                _ => {}
            }
        }
        if !turn_done {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(turn_done, "the turn must finish inside the test deadline");
    observations
}

#[tokio::test]
async fn progress_proposal_applies_and_orders_its_events() {
    let (instance, events) = progress_instance(json!({
        "base_anchor_revision": 0,
        "current_interpretation": "retry policy spans config, errors and execution",
        "plan_progress": ["read the runner"],
        "next_action": "add the fake-sleeper unit test",
    }))
    .await;
    let observations = run_progress_turn(&instance, events).await;

    // The audit row precedes the typed CAS outcome, and eval can prove
    // both from the event stream alone.
    let anchor_at = observations
        .iter()
        .position(|(label, _, _, fields)| label.starts_with("anchor:") && !fields.is_empty())
        .expect("a moved anchor must publish its audit row");
    let updated_at = observations
        .iter()
        .position(|(label, _, accepted, _)| {
            *accepted && !label.starts_with("anchor:") && !label.starts_with("idempotent")
        })
        .expect("an accepted proposal must publish its outcome");
    assert!(
        anchor_at < updated_at,
        "TaskAnchorChanged must precede TaskProgressUpdated: {observations:?}"
    );

    let (_, revision, accepted, changed_fields) = &observations[updated_at];
    assert_eq!(*revision, 1, "the proposal moves the anchor to revision 1");
    assert!(accepted);
    for field in ["current_interpretation", "plan_progress", "next_action"] {
        assert!(
            changed_fields.iter().any(|name| name == field),
            "the outcome must name the moved field {field}: {changed_fields:?}"
        );
    }

    let tasks = instance.handle().list_tasks().await.unwrap();
    assert_eq!(
        tasks[0].anchor_revision, 1,
        "the anchor stays at the CAS result"
    );
}

#[tokio::test]
async fn stale_base_revision_refuses_without_changing_task_state() {
    let (instance, events) = progress_instance(json!({
        "base_anchor_revision": 99,
        "next_action": "written against a stale anchor",
    }))
    .await;
    let observations = run_progress_turn(&instance, events).await;

    let refusal = observations
        .iter()
        .find(|(_, _, accepted, _)| !accepted)
        .expect("a stale base revision must publish its refusal");
    let (_, revision, _, changed_fields) = refusal;
    assert_eq!(*revision, 0, "a refusal reports no new revision");
    assert!(changed_fields.is_empty(), "a refusal moves nothing");
    assert!(
        refusal.0.contains("revision mismatch"),
        "the typed reason names the CAS failure: {}",
        refusal.0
    );
    assert!(
        !observations
            .iter()
            .any(|(label, _, _, fields)| label.starts_with("anchor:") && !fields.is_empty()),
        "no anchor audit row may exist for a refused proposal"
    );

    let tasks = instance.handle().list_tasks().await.unwrap();
    assert_eq!(
        tasks[0].anchor_revision, 0,
        "task state must be untouched by a stale proposal"
    );
}
