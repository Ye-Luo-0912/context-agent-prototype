//! Completion-opportunity advisory wiring at the actor level: the
//! consult is fully silent while the host switch is off (the default),
//! and once enabled it emits typed, body-free
//! observations instead of ever leasing `task.complete` from an initial
//! task with no durable work.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentResult, CompletionOpportunityDisposition, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, RuntimeEventEnvelope, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;

use crate::harness::*;

/// Calls fs.read on round 0, then finishes.
#[derive(Debug)]
struct OneRoundToolThenFinishModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for OneRoundToolThenFinishModel {
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
                    name: "fs.read".into(),
                    arguments: json!({"path": "README.md"}),
                }],
                usage: Default::default(),
            })
        } else {
            Ok(ModelOutput {
                content: "done reading".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }
}

/// Serves one read-only builtin-shaped call per request; its settled batch
/// gives the advisory consult a safe point to observe.
#[derive(Debug)]
struct SingleToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for SingleToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
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
            tool_name: "fs.read".into(),
            ok: true,
            summary: "settled".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

async fn instance_with(dir: &std::path::Path, opportunity_switch: bool) -> RuntimeInstance {
    let workspace = agent_workspace::Workspace::open(dir).await.unwrap();
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(OneRoundToolThenFinishModel {
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(SingleToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(Arc::new(workspace));
    let services = if opportunity_switch {
        services.with_project_completion_opportunity(true)
    } else {
        services
    };
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    instance.handle().start().await.unwrap();
    instance
}

/// Run one turn and collect every completion-opportunity disposition in
/// arrival order alongside the turn-completed marker.
async fn run_turn(
    instance: &RuntimeInstance,
    mut events: tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> (Vec<CompletionOpportunityDisposition>, bool) {
    let handle = instance.handle();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();
    let mut dispositions = Vec::new();
    let mut completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !completed {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::CompletionOpportunity { disposition, .. } => {
                    dispositions.push(disposition);
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if !completed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(completed, "the turn must finish inside the test deadline");
    (dispositions, completed)
}

#[tokio::test]
async fn opportunity_consult_is_silent_while_the_host_switch_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let instance = instance_with(dir.path(), false).await;
    let events = instance.handle().subscribe();
    let (dispositions, _) = run_turn(&instance, events).await;
    assert!(
        dispositions.is_empty(),
        "the default-off switch must emit no opportunity observations, got {dispositions:?}"
    );
}

#[tokio::test]
async fn enabled_switch_observes_but_never_leases_an_initial_task() {
    let dir = tempfile::tempdir().unwrap();
    let instance = instance_with(dir.path(), true).await;
    let events = instance.handle().subscribe();
    let (dispositions, _) = run_turn(&instance, events).await;
    assert!(
        dispositions.contains(&CompletionOpportunityDisposition::NotReady),
        "an enabled consult must account the settled batch, got {dispositions:?}"
    );
    assert!(
        !dispositions.contains(&CompletionOpportunityDisposition::Offered),
        "a task with no durable work must never obtain the lease, got {dispositions:?}"
    );
}
