//! Interactive approval-gate tests at the kernel level.
//!
//! Verifies the full loop: a workspace-write / process-execution tool call
//! broadcasts an `ApprovalRequest` to the broker, blocks until the UI answers
//! through `InteractiveApprovalGate::respond`, and proceeds or fails
//! accordingly. Read-only tools must never prompt.

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, ContextBuildRequest, ContextDiagnostics,
    ContextEngine, ContextIngress, ContextItemSummary, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextSnapshot, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutput,
    ToolRisk, ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, ApprovalBroker, InteractiveApprovalGate};
use serde_json::json;

#[derive(Debug)]
struct TestContextEngine;

#[async_trait::async_trait]
impl ContextEngine for TestContextEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn build_snapshot(&self, request: ContextBuildRequest) -> AgentResult<ContextSnapshot> {
        Ok(ContextSnapshot {
            messages: vec![
                agent_contracts::ModelMessage::system(request.system_prompt),
                agent_contracts::ModelMessage::user(request.current_input),
            ],
            selected: Vec::new(),
            approx_tokens: 0,
            diagnostics: ContextDiagnostics::default(),
        })
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
}

/// Dispatcher with one tool per risk class; execution always succeeds and
/// echoes the tool name so the test can tell which call ran.
#[derive(Debug)]
struct RiskToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for RiskToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "read.only".into(),
                description: "read-only probe".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "write.tool".into(),
                description: "workspace write probe".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::WorkspaceWrite,
            },
            ToolSpec {
                name: "proc.tool".into(),
                description: "process execution probe".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ProcessExecution,
            },
        ]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let name = request.call.name.clone();
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: name.clone(),
            ok: true,
            summary: format!("ran {name}"),
            model_content: format!("ran {name}"),
            artifact_ref: None,
            metadata: json!({}),
        })
    }
}

/// Emits exactly one tool call for `tool` on the first model round, then
/// finishes with no more calls so the turn can complete.
#[derive(Debug)]
struct OneToolCallModel {
    tool: String,
    fired: std::sync::atomic::AtomicBool,
}

impl OneToolCallModel {
    fn new(tool: &str) -> Self {
        Self {
            tool: tool.into(),
            fired: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for OneToolCallModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let first = !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst);
        Ok(ModelOutput {
            content: "done".into(),
            tool_calls: if first {
                vec![ToolCall {
                    id: "c1".into(),
                    name: self.tool.clone(),
                    arguments: json!({"probe": true}),
                }]
            } else {
                Vec::new()
            },
            usage: Default::default(),
        })
    }
}

fn kernel(model: Arc<dyn ModelTransport>, gate: Arc<dyn ApprovalGate>) -> Arc<AgentKernel> {
    Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(RiskToolDispatcher),
        gate,
        None,
    ))
}

/// Collects the ToolFinished outputs of one turn.
fn finished_outputs(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
) -> Vec<agent_contracts::ToolOutput> {
    let mut outputs = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        if let RuntimeEvent::ToolFinished { output } = envelope.event {
            outputs.push(output);
        }
    }
    outputs
}

#[tokio::test]
async fn read_only_tools_never_prompt() {
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
    let kernel = kernel(Arc::new(OneToolCallModel::new("read.only")), gate);
    let mut events = kernel.subscribe();
    kernel.start().await.unwrap();

    kernel.handle_user_message("go".into()).await.unwrap();

    let outputs = finished_outputs(&mut events);
    assert_eq!(outputs.len(), 1);
    assert!(
        outputs[0].ok,
        "read-only call must run: {}",
        outputs[0].summary
    );
    assert!(
        broker.pending().await.is_empty(),
        "read-only tool must not create an approval request"
    );
}

#[tokio::test]
async fn write_tool_waits_for_allow() {
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
    let mut approval_rx = broker.subscribe();
    let kernel = kernel(Arc::new(OneToolCallModel::new("write.tool")), gate.clone());
    let mut events = kernel.subscribe();
    kernel.start().await.unwrap();

    let turn = kernel.clone();
    let task = tokio::spawn(async move { turn.handle_user_message("go".into()).await });

    // The kernel blocks inside authorize() until the UI answers.
    let request = tokio::time::timeout(Duration::from_secs(2), approval_rx.recv())
        .await
        .expect("write tool must broadcast an approval request")
        .expect("broker channel closed");
    assert_eq!(request.spec.name, "write.tool");
    assert_eq!(request.call.arguments, json!({"probe": true}));

    assert_eq!(
        broker.pending().await.len(),
        1,
        "request visible to late subscribers"
    );

    assert!(
        gate.respond(&request.request_id, ApprovalDecision::Allow)
            .await,
        "respond should resolve the pending request"
    );

    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("turn hung after approval")
        .expect("turn panicked")
        .expect("turn failed after allow");

    let outputs = finished_outputs(&mut events);
    assert_eq!(outputs.len(), 1);
    assert!(outputs[0].ok, "allowed write tool must run");
    assert!(
        broker.pending().await.is_empty(),
        "resolved request leaves the pending queue"
    );
}

#[tokio::test]
async fn write_tool_can_be_denied() {
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
    let mut approval_rx = broker.subscribe();
    let kernel = kernel(Arc::new(OneToolCallModel::new("proc.tool")), gate.clone());
    let mut events = kernel.subscribe();
    kernel.start().await.unwrap();

    let turn = kernel.clone();
    let task = tokio::spawn(async move { turn.handle_user_message("go".into()).await });

    let request = tokio::time::timeout(Duration::from_secs(2), approval_rx.recv())
        .await
        .expect("process tool must broadcast an approval request")
        .expect("broker channel closed");
    assert_eq!(request.spec.name, "proc.tool");

    assert!(
        gate.respond(&request.request_id, ApprovalDecision::Deny)
            .await,
        "respond should resolve the pending request"
    );

    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("turn hung after denial")
        .expect("turn panicked")
        .expect("turn failed after deny");

    let outputs = finished_outputs(&mut events);
    assert_eq!(outputs.len(), 1);
    assert!(!outputs[0].ok, "denied tool must not run");
    assert!(
        outputs[0].summary.contains("denied"),
        "summary should explain the denial: {}",
        outputs[0].summary
    );
    assert!(
        broker.pending().await.is_empty(),
        "resolved request leaves the pending queue"
    );
}

#[tokio::test]
async fn unanswered_request_times_out_and_denies() {
    // A short answer timeout makes the "UI never responds" case both safe and
    // testable: the turn must not hang, and the tool is denied.
    let broker = ApprovalBroker::new();
    let gate = Arc::new(
        InteractiveApprovalGate::new(broker.clone())
            .with_answer_timeout(Duration::from_millis(200)),
    );
    let kernel = kernel(Arc::new(OneToolCallModel::new("write.tool")), gate);
    let mut events = kernel.subscribe();
    kernel.start().await.unwrap();

    kernel.handle_user_message("go".into()).await.unwrap();

    let outputs = finished_outputs(&mut events);
    assert_eq!(outputs.len(), 1);
    assert!(!outputs[0].ok, "unanswered request must be denied");
    assert!(
        outputs[0].summary.contains("timed out"),
        "summary should explain the timeout: {}",
        outputs[0].summary
    );
    assert!(
        broker.pending().await.is_empty(),
        "timed-out request leaves the pending queue"
    );
}
