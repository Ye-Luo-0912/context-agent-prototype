//! Interactive approval-gate tests at the actor level.
//!
//! Verifies the full loop through the runtime: a workspace-write /
//! process-execution tool call broadcasts an `ApprovalRequest` to the broker,
//! blocks until the UI answers through `InteractiveApprovalGate::respond`,
//! and proceeds or fails accordingly. Read-only tools must never prompt.

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, ScopeId, ScopeKind, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};

use agent_core::{ApprovalBroker, CoreAuthorityConfig, InteractiveApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeServices, spawn_runtime};
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
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
            task: None,
            items: Vec::new(),
            external: agent_contracts::ContextMapView::default(),
            selected: Vec::new(),
            approx_tokens: 0,
            foreground: Vec::new(),
            diagnostics: ContextDiagnostics::default(),
        })
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
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "write.tool".into(),
                description: "workspace write probe".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::WorkspaceWrite,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "proc.tool".into(),
                description: "process execution probe".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ProcessExecution,
                output_budget: None,
                roles: Vec::new(),
            },
        ]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let name = request.call.name.clone();
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: name.clone(),
            ok: true,
            summary: format!("ran {name}"),
            model_content: format!("ran {name}"),
            artifact_ref: None,
            metadata: json!({}),
        }))
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

async fn spawn_with(model: Arc<dyn ModelTransport>, gate: Arc<dyn ApprovalGate>) -> RuntimeHandle {
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(RiskToolDispatcher),
        gate,
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    handle
}

/// Wait for a ToolFinished event carrying one output; fails on timeout.
async fn wait_for_tool_finished(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
) -> agent_contracts::ToolOutput {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::ToolFinished { output } = envelope.event {
                return output;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("ToolFinished was not emitted before the deadline");
}

#[tokio::test]
async fn read_only_tools_never_prompt() {
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
    let handle = spawn_with(Arc::new(OneToolCallModel::new("read.only")), gate).await;
    let mut events = handle.subscribe();

    handle.user_message("go".into()).await.unwrap();
    let output = wait_for_tool_finished(&mut events).await;
    assert!(output.ok, "read-only call must run: {}", output.summary);
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
    let handle = spawn_with(Arc::new(OneToolCallModel::new("write.tool")), gate.clone()).await;
    let mut events = handle.subscribe();

    handle.user_message("go".into()).await.unwrap();

    // The tool operation blocks inside authorize() until the UI answers.
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

    let output = wait_for_tool_finished(&mut events).await;
    assert!(output.ok, "allowed write tool must run");
    let pending = broker.pending().await;
    eprintln!("pending after allow: {pending:?}");
    assert!(
        pending.is_empty(),
        "resolved request leaves the pending queue"
    );
}

#[tokio::test]
async fn write_tool_can_be_denied() {
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
    let mut approval_rx = broker.subscribe();
    let handle = spawn_with(Arc::new(OneToolCallModel::new("proc.tool")), gate.clone()).await;
    let mut events = handle.subscribe();

    handle.user_message("go".into()).await.unwrap();

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

    let output = wait_for_tool_finished(&mut events).await;
    assert!(!output.ok, "denied tool must not run");
    assert!(
        output.summary.contains("denied"),
        "summary should explain the denial: {}",
        output.summary
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
    let handle = spawn_with(Arc::new(OneToolCallModel::new("write.tool")), gate).await;
    let mut events = handle.subscribe();

    handle.user_message("go".into()).await.unwrap();

    let output = wait_for_tool_finished(&mut events).await;
    assert!(!output.ok, "unanswered request must be denied");
    assert!(
        output.summary.contains("timed out"),
        "summary should explain the timeout: {}",
        output.summary
    );
    assert!(
        broker.pending().await.is_empty(),
        "timed-out request leaves the pending queue"
    );
}
