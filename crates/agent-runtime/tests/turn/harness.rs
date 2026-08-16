use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agent_contracts::{
    AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    EventJournal, MaterializedContext, ModelCapabilities, ModelChunk, ModelEventSink, ModelMessage,
    ModelOutput, ModelRequest, ModelTransport, ScopeId, ScopeKind, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolSpec,
};

use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeServices, spawn_runtime};
use serde_json::json;
use tokio::sync::Mutex;

#[derive(Debug)]
pub(crate) struct TestContextEngine;

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

#[derive(Debug)]
pub(crate) struct TestToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for TestToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(agent_contracts::AgentError::Tool(
            "no tools configured".into(),
        ))
    }
}

/// Emits two text deltas then finishes.
#[derive(Debug)]
pub(crate) struct StreamingModel;

#[async_trait::async_trait]
impl ModelTransport for StreamingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            ..ModelCapabilities::default()
        }
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        unreachable!("streaming model should be driven through complete_stream")
    }
    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        for delta in ["Hello ", "world"] {
            if request.cancel.is_cancelled() {
                return Err(agent_contracts::AgentError::Cancelled);
            }
            sink.on_chunk(ModelChunk::TextDelta {
                delta: delta.to_string(),
            })
            .await?;
            tokio::task::yield_now().await;
        }
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(ModelOutput {
            content: "Hello world".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// One tool call on the first model round, plain text on the second, and a
/// record of every message list it was given.
#[derive(Debug, Default)]
pub(crate) struct TwoRoundToolModel {
    pub(crate) rounds: AtomicUsize,
    pub(crate) requests: Arc<Mutex<Vec<Vec<ModelMessage>>>>,
}

#[async_trait::async_trait]
impl ModelTransport for TwoRoundToolModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().await.push(request.messages.clone());
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "x"}),
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

pub(crate) async fn spawn_with(
    model: Arc<dyn ModelTransport>,
    context: Arc<dyn ContextEngine>,
    tools: Arc<dyn ToolDispatcher>,
) -> RuntimeHandle {
    spawn_with_approval(
        model,
        context,
        tools,
        Arc::new(PolicyApprovalGate::read_only()),
    )
    .await
}

pub(crate) async fn spawn_with_approval(
    model: Arc<dyn ModelTransport>,
    context: Arc<dyn ContextEngine>,
    tools: Arc<dyn ToolDispatcher>,
    approval: Arc<dyn agent_contracts::ApprovalGate>,
) -> RuntimeHandle {
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        model,
        tools,
        approval,
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    handle
}

pub(crate) async fn spawn_with_journal(
    model: Arc<dyn ModelTransport>,
    journal: Arc<dyn EventJournal>,
) -> RuntimeHandle {
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal),
    ));
    let (handle, _task) = spawn_runtime(services);
    handle.start().await.unwrap();
    handle
}

/// A context engine that never completes tool-scope closure. Cancellation
/// must bound this untrusted/replaceable engine call instead of holding the
/// actor and its cancellation acknowledgement forever.
#[derive(Debug, Default)]
pub(crate) struct HangingCloseScopeEngine;

#[async_trait::async_trait]
impl ContextEngine for HangingCloseScopeEngine {
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
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        std::future::pending().await
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

/// A plain, non-streaming model: one fixed assistant reply, no tool calls.
#[derive(Debug)]
pub(crate) struct PlainModel;

#[async_trait::async_trait]
impl ModelTransport for PlainModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "final answer".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}
