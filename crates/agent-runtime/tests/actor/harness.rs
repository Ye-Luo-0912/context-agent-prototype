use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::{
    AgentResult, AttentionState, ContextConsumptionAck, ContextDiagnostics, ContextEngine,
    ContextIngress, ContextItemId, ContextItemSummary, ContextKind, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextRetention, ContextScope,
    ContextStateTransition, EventJournal, FocusState, MaterializedContext, MaterializedItem,
    ModelCapabilities, ModelChunk, ModelEventSink, ModelOutput, ModelRequest, ModelTransport,
    ModelUsage, RuntimeEvent, RuntimeEventEnvelope, ScopeId, ScopeKind, SemanticState, TaskId,
    ToolCatalogEntry, ToolDispatcher, ToolExecutionRequest, ToolLifecycle, ToolOutcome, ToolRisk,
    ToolSemanticRole, ToolSpec, ToolSurfacePlanReport, ToolSurfacePlanStatus, ToolSurfaceSnapshot,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeServices, spawn_runtime};

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

/// Blocks until the request is cancelled, then reports cancellation.
#[derive(Debug)]
pub(crate) struct HangingModel;

#[async_trait::async_trait]
impl ModelTransport for HangingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        request.cancel.cancelled().await;
        Err(agent_contracts::AgentError::Cancelled)
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

pub(crate) fn kernel(model: Arc<dyn ModelTransport>) -> Arc<RuntimeServices> {
    kernel_with(model, Arc::new(TestContextEngine))
}

pub(crate) fn kernel_with(
    model: Arc<dyn ModelTransport>,
    context: Arc<dyn ContextEngine>,
) -> Arc<RuntimeServices> {
    Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        model,
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ))
}

pub(crate) async fn start(
    model: Arc<dyn ModelTransport>,
) -> (RuntimeHandle, tokio::task::JoinHandle<()>) {
    let kernel = kernel(model);
    let (handle, task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    (handle, task)
}

/// Records every `ContextQuery` the actor hands to the engine, so a test can
/// assert what slice of the pack window actually reaches the working set.
#[derive(Debug, Default)]
pub(crate) struct RecordingContextEngine {
    pub(crate) queries: Mutex<Vec<ContextQuery>>,
    pub(crate) user_messages: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ContextEngine for RecordingContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if let ContextIngress::UserMessage { content } = ingress {
            self.user_messages.lock().unwrap().push(content);
        }
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        self.queries.lock().unwrap().push(query);
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

/// Declares a real provider window and max output so the derived frame
/// budget is meaningful.
#[derive(Debug)]
pub(crate) struct BudgetModel;

#[async_trait::async_trait]
impl ModelTransport for BudgetModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 2_000,
            context_window: Some(30_000),
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
        if request.cancel.is_cancelled() {
            return Err(agent_contracts::AgentError::Cancelled);
        }
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(ModelOutput {
            content: "ok".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// One advertised tool, so the tool-schema layer of the budget is non-empty.
#[derive(Debug)]
pub(crate) struct OneToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for OneToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a workspace file".into(),
            input_schema: serde_json::json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(agent_contracts::AgentError::Tool(
            "no tools configured".into(),
        ))
    }
}

/// Records FocusChanged task ids. Materialize leaves focus empty because
/// production engines no longer own the prompt Focus projection.
#[derive(Default)]
pub(crate) struct RecordingFocusEngine {
    pub received_focus: Mutex<Vec<(TaskId, String)>>,
}

#[async_trait::async_trait]
impl ContextEngine for RecordingFocusEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if let ContextIngress::FocusChanged { focus } = ingress {
            self.received_focus
                .lock()
                .unwrap()
                .push((focus.task_id, focus.goal.clone()));
        }
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

/// The engine rejects the focus change: `FocusChanged` ingest fails, so any
/// task transition that depends on it must fail too — and the runtime's
/// task table must not move.
#[derive(Debug)]
pub(crate) struct FailingFocusContextEngine;

#[async_trait::async_trait]
impl ContextEngine for FailingFocusContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::FocusChanged { .. }) {
            return Err(agent_contracts::AgentError::Internal(
                "focus rejected".into(),
            ));
        }
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

/// Always-empty 0/0 completion. Transport/parser anomaly, not a real stop.
#[derive(Debug, Default)]
pub(crate) struct StructurallyEmptyModel {
    pub(crate) calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for StructurallyEmptyModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// Finishes a model round immediately with an empty reply.
#[derive(Debug)]
pub(crate) struct SilentModel;

#[async_trait::async_trait]
impl ModelTransport for SilentModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: Vec::new(),
            usage: ModelUsage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                attempts: 1,
                retries: 0,
            },
        })
    }
}

/// Simulates the important half-commit case: focus ingest mutates the
/// engine, then the maintenance step fails. A transaction rollback must put
/// the engine back before the runtime rejects the task transition.
#[derive(Debug, Default)]
pub(crate) struct MutatingThenFailingFocusEngine {
    pub(crate) focus: Mutex<Option<FocusState>>,
    pub(crate) rollback_fails: bool,
}

#[async_trait::async_trait]
impl ContextEngine for MutatingThenFailingFocusEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        match ingress {
            ContextIngress::FocusChanged { focus } => {
                *self.focus.lock().unwrap() = Some(focus);
            }
            ContextIngress::FocusCleared => *self.focus.lock().unwrap() = None,
            _ => {}
        }
        Ok(())
    }

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        if trigger == ContextMaintenanceTrigger::FocusChanged {
            return Err(agent_contracts::AgentError::Context(
                "maintenance failed after focus mutation".into(),
            ));
        }
        Ok(ContextMaintenanceReport::default())
    }

    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: self.focus.lock().unwrap().clone(),
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
        serde_json::to_value(self.focus.lock().unwrap().clone())
            .map_err(|error| agent_contracts::AgentError::Context(error.to_string()))
    }
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        if self.rollback_fails {
            return Err(agent_contracts::AgentError::Context(
                "simulated rollback failure".into(),
            ));
        }
        let focus = serde_json::from_value(data)
            .map_err(|error| agent_contracts::AgentError::Context(error.to_string()))?;
        *self.focus.lock().unwrap() = focus;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct FailFocusEventJournal;

#[async_trait::async_trait]
impl EventJournal for FailFocusEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::FocusChanged { .. }) {
            return Err(agent_contracts::AgentError::Storage(
                "simulated focus journal failure".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct FailSurfaceEventJournal;

#[async_trait::async_trait]
impl EventJournal for FailSurfaceEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::ToolSurfacePlanned { .. }) {
            return Err(agent_contracts::AgentError::Storage(
                "simulated surface-plan journal failure".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct FailConsumptionEventJournal;

#[async_trait::async_trait]
impl EventJournal for FailConsumptionEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::ContextConsumed { .. }) {
            return Err(agent_contracts::AgentError::Storage(
                "simulated context-consumption journal failure".into(),
            ));
        }
        Ok(())
    }
}

/// The engine rejects the clear-focus transition (`FocusCleared` ingest
/// fails), so a suspend that depends on it must fail too — and the task
/// table must not move.
#[derive(Debug)]
pub(crate) struct FailingClearFocusContextEngine;

#[async_trait::async_trait]
impl ContextEngine for FailingClearFocusContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::FocusCleared) {
            return Err(agent_contracts::AgentError::Internal(
                "clear focus rejected".into(),
            ));
        }
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

/// Records every request it receives and finishes immediately, so the test
/// can assert on exactly what the final budget guard let through.
#[derive(Debug)]
pub(crate) struct RecordingModel {
    pub(crate) requests: Mutex<Vec<ModelRequest>>,
    pub(crate) calls: AtomicUsize,
    pub(crate) context_window: usize,
    pub(crate) max_output_tokens: usize,
}

impl Default for RecordingModel {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            context_window: 10_000,
            max_output_tokens: 4_000,
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for RecordingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: self.max_output_tokens,
            context_window: Some(self.context_window),
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        if request.cancel.is_cancelled() {
            return Err(agent_contracts::AgentError::Cancelled);
        }
        self.requests.lock().unwrap().push(request);
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(ModelOutput {
            content: "ok".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// A context engine whose materialization returns large working-set items
/// and ignores `query.budget_tokens` — append-only A behavior.
#[derive(Debug)]
pub(crate) struct BigContextEngine {
    pub(crate) acks: Mutex<Vec<ContextConsumptionAck>>,
    pub(crate) item_count: usize,
}

impl Default for BigContextEngine {
    fn default() -> Self {
        Self::with_items(3)
    }
}

impl BigContextEngine {
    pub(crate) fn with_items(item_count: usize) -> Self {
        Self {
            acks: Mutex::new(Vec::new()),
            item_count,
        }
    }
}

#[async_trait::async_trait]
impl ContextEngine for BigContextEngine {
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
        // ~3_000 tokens per item (ASCII estimate: 4 chars per token).
        let items: Vec<MaterializedItem> = (0..self.item_count)
            .map(|i| MaterializedItem {
                item_id: ContextItemId::new(),
                kind: ContextKind::FileObservation,
                scope: ContextScope::Task,
                attention: AttentionState::Active,
                semantic: SemanticState::Live,
                retention: ContextRetention::Working,
                content: format!("{}:{}", "data ".repeat(2400), i),
                source: None,
                file_path: None,
                file_revision: None,
            })
            .collect();
        let approx_tokens = self.item_count.saturating_mul(3_000);
        Ok(MaterializedContext {
            materialization_id: 1,
            focus: None,
            task: None,
            items,
            external: agent_contracts::ContextMapView::default(),
            selected: Vec::new(),
            approx_tokens,
            foreground: Vec::new(),
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn acknowledge_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()> {
        ack.validate()?;
        self.acks.lock().unwrap().push(ack);
        Ok(())
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

/// A model whose window is almost entirely reserved for output: the input
/// budget is tiny, so any request is refused by the final guard.
#[derive(Debug, Default)]
pub(crate) struct TinyWindowModel {
    calls: AtomicUsize,
}

/// A provider whose input window can change between rounds. The first round
/// is deliberately small enough that the optional schema must be omitted;
/// the second restores a large window so the same still-loaded tool can
/// reappear without a catalog mutation.
#[derive(Debug)]
pub(crate) struct VariableWindowModel {
    pub(crate) context_window: AtomicUsize,
    pub(crate) requests: Mutex<Vec<ModelRequest>>,
}

impl VariableWindowModel {
    pub(crate) fn new(context_window: usize) -> Self {
        Self {
            context_window: AtomicUsize::new(context_window),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn set_context_window(&self, context_window: usize) {
        self.context_window.store(context_window, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ModelTransport for VariableWindowModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 512,
            context_window: Some(self.context_window.load(Ordering::SeqCst)),
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
        self.requests.lock().unwrap().push(request);
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(ModelOutput {
            content: "ok".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// Catalog stub that makes the old bug observable: `unload_tool` really
/// removes the optional schema and bumps the generation. The fixed actor
/// must only omit that schema from its local round snapshot, so neither
/// value changes and a later large-budget snapshot contains it again.
#[derive(Debug)]
pub(crate) struct RoundLocalToolDispatcher {
    optional_loaded: AtomicBool,
    evict_on_gc: AtomicBool,
    optional_description_chars: usize,
    generation: AtomicU64,
    load_calls: AtomicUsize,
    unload_calls: AtomicUsize,
    /// Every gc() roots set the runtime passed (active-task tool-demand
    /// names), for asserting the TaskAnchor-driven roots wiring.
    roots_seen: Mutex<Vec<Vec<String>>>,
}

impl RoundLocalToolDispatcher {
    pub(crate) fn new() -> Self {
        Self {
            optional_loaded: AtomicBool::new(true),
            evict_on_gc: AtomicBool::new(false),
            optional_description_chars: 10_000,
            generation: AtomicU64::new(17),
            load_calls: AtomicUsize::new(0),
            unload_calls: AtomicUsize::new(0),
            roots_seen: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn roots_seen(&self) -> Vec<Vec<String>> {
        self.roots_seen.lock().unwrap().clone()
    }

    pub(crate) fn evicting_on_gc() -> Self {
        let dispatcher = Self::new();
        dispatcher.evict_on_gc.store(true, Ordering::SeqCst);
        dispatcher
    }

    pub(crate) fn schema_overflow() -> Self {
        Self {
            optional_description_chars: 20_000,
            ..Self::new()
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub(crate) fn optional_loaded(&self) -> bool {
        self.optional_loaded.load(Ordering::SeqCst)
    }

    pub(crate) fn unload_calls(&self) -> usize {
        self.unload_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn load_calls(&self) -> usize {
        self.load_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ToolDispatcher for RoundLocalToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![ToolSpec {
            name: "core.read".into(),
            description: "mandatory core reader".into(),
            input_schema: serde_json::json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }];
        if self.optional_loaded() {
            specs.push(ToolSpec {
                name: "optional.large".into(),
                // ~2,500 tokens: too large for the first round's input
                // budget but comfortably inside the restored window.
                description: "x".repeat(self.optional_description_chars),
                input_schema: serde_json::json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            });
        }
        specs
    }

    fn snapshot(&self) -> ToolSurfaceSnapshot {
        ToolSurfaceSnapshot {
            specs: self.specs(),
            generation: self.generation(),
            ..Default::default()
        }
    }

    fn may_omit_from_round(&self, name: &str) -> bool {
        name == "optional.large"
    }

    fn gc(&self, roots: &[String]) {
        self.roots_seen.lock().unwrap().push(roots.to_vec());
        if self.evict_on_gc.load(Ordering::SeqCst)
            && self.optional_loaded.swap(false, Ordering::SeqCst)
        {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn catalog(&self) -> Vec<ToolCatalogEntry> {
        vec![
            ToolCatalogEntry {
                name: "core.read".into(),
                state: ToolLifecycle::Loaded,
                owner: "test".into(),
                description: "mandatory core reader".into(),
                risk: agent_contracts::ToolRisk::ReadOnly,
            },
            ToolCatalogEntry {
                name: "optional.large".into(),
                state: if self.optional_loaded() {
                    ToolLifecycle::Loaded
                } else {
                    ToolLifecycle::Unloaded
                },
                owner: "test".into(),
                description: "large optional schema".into(),
                risk: agent_contracts::ToolRisk::ReadOnly,
            },
        ]
    }

    fn load_tool(&self, name: &str) -> AgentResult<()> {
        if name != "optional.large" {
            return Err(agent_contracts::AgentError::InvalidRequest(format!(
                "unknown loadable tool '{name}'"
            )));
        }
        self.load_calls.fetch_add(1, Ordering::SeqCst);
        if !self.optional_loaded.swap(true, Ordering::SeqCst) {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn unload_tool(&self, name: &str) -> AgentResult<()> {
        if name != "optional.large" {
            return Err(agent_contracts::AgentError::InvalidRequest(format!(
                "core tool '{name}' cannot be unloaded"
            )));
        }
        self.unload_calls.fetch_add(1, Ordering::SeqCst);
        self.optional_loaded.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(agent_contracts::AgentError::Tool(
            "no tools are executed in this test".into(),
        ))
    }
}

pub(crate) async fn wait_for_turn_completed(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) {
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
}

pub(crate) async fn wait_for_surface_plan(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> ToolSurfacePlanReport {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ToolSurfacePlanned { report },
                    ..
                }) => break report,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime event stream closed before ToolSurfacePlanned")
                }
            }
        }
    })
    .await
    .expect("surface plan was not published within the test deadline")
}

pub(crate) async fn wait_for_ready_surface_and_model_start(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> ToolSurfacePlanReport {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut planned = None;
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ToolSurfacePlanned { report },
                    ..
                }) => planned = Some(report),
                Ok(RuntimeEventEnvelope {
                    event:
                        RuntimeEvent::ModelStarted {
                            surface_revision, ..
                        },
                    ..
                }) => {
                    let report = planned
                        .take()
                        .expect("ToolSurfacePlanned must precede ModelStarted");
                    assert_eq!(report.status, ToolSurfacePlanStatus::Ready);
                    assert_eq!(report.surface_revision, surface_revision);
                    break report;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime event stream closed before ModelStarted")
                }
            }
        }
    })
    .await
    .expect("ready surface was not published within the test deadline")
}

impl TinyWindowModel {
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ModelTransport for TinyWindowModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 2_000,
            context_window: Some(1_000),
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        if request.cancel.is_cancelled() {
            return Err(agent_contracts::AgentError::Cancelled);
        }
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(ModelOutput {
            content: "ok".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// Records every event the actor appends and every barrier flush, with a
/// switch to make the barrier fail. Lets a test assert the turn-commit
/// ordering: all mandatory state writes precede `TurnCompleted`, and
/// `TurnCompleted` is broadcast only after the barrier succeeds.
#[derive(Debug, Default)]
pub(crate) struct BarrierJournal {
    pub(crate) appended: Mutex<Vec<String>>,
    pub(crate) flushes: AtomicUsize,
    pub(crate) fail_flush: AtomicBool,
}

#[async_trait::async_trait]
impl EventJournal for BarrierJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        self.appended
            .lock()
            .unwrap()
            .push(format!("{:?}", envelope.event));
        Ok(())
    }
    async fn flush(&self) -> AgentResult<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        if self.fail_flush.load(Ordering::SeqCst) {
            return Err(agent_contracts::AgentError::Storage(
                "simulated barrier failure".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn kernel_with_journal(journal: Arc<dyn EventJournal>) -> Arc<RuntimeServices> {
    Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal),
    ))
}
