//! Actor tests: command serialization, busy rejection, cancellation and
//! stale-result dropping. Uses minimal stubs for context/tools/model so the
//! actor is exercised against the engine contracts only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{
    AgentResult, AttentionState, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemId,
    ContextItemSummary, ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, ContextRetention, ContextScope, ContextStateTransition, MaterializedContext,
    MaterializedItem, ModelCapabilities, ModelChunk, ModelEventSink, ModelMessage, ModelOutput,
    ModelRequest, ModelTransport, RuntimeEvent, ScopeId, ScopeKind, SemanticState, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolRisk, ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};
use agent_runtime::{ModelBudget, RuntimeHandle, approx_layer_tokens, spawn_runtime};

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
            focus: None,
            items: Vec::new(),
            external: Vec::new(),
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
struct TestToolDispatcher;

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
struct HangingModel;

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
struct StreamingModel;

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

fn kernel(model: Arc<dyn ModelTransport>) -> Arc<AgentKernel> {
    kernel_with(model, Arc::new(TestContextEngine))
}

fn kernel_with(
    model: Arc<dyn ModelTransport>,
    context: Arc<dyn ContextEngine>,
) -> Arc<AgentKernel> {
    Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        context,
        model,
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ))
}

async fn start(model: Arc<dyn ModelTransport>) -> (RuntimeHandle, tokio::task::JoinHandle<()>) {
    let kernel = kernel(model);
    let (handle, task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    (handle, task)
}

#[tokio::test]
async fn actor_rejects_mutation_commands_while_a_turn_runs() {
    let (handle, _task) = start(Arc::new(HangingModel)).await;

    let turn = handle.clone();
    let turn_task = tokio::spawn(async move { turn.user_message("first".into()).await });

    // Give the turn time to start and block inside the model call.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let busy = handle.user_message("second".into()).await;
    assert!(
        busy.is_err(),
        "a second user message while a turn runs must be rejected"
    );
    assert!(
        busy.unwrap_err().to_string().contains("busy"),
        "the rejection must say the agent is busy"
    );
    let focus = handle.set_focus("new goal".into()).await;
    assert!(
        focus.is_err() && focus.unwrap_err().to_string().contains("busy"),
        "a focus change during a turn must be rejected (the old race)"
    );
    let pin = handle.pin("never edit generated files".into()).await;
    assert!(pin.is_err(), "a pin during a turn must be rejected");
    let done = handle.complete_current_task("sum".into()).await;
    assert!(
        done.is_err(),
        "task completion during a turn must be rejected"
    );

    handle.cancel_turn().await;
    let result = tokio::time::timeout(Duration::from_secs(2), turn_task)
        .await
        .expect("turn did not stop after cancellation")
        .expect("turn task panicked");
    assert!(
        result.is_ok(),
        "cancelled turn should end cleanly, got: {result:?}"
    );
}

#[tokio::test]
async fn cancel_then_new_turn_drops_stale_completion() {
    let (handle, _task) = start(Arc::new(HangingModel)).await;
    let mut events = handle.subscribe();

    let turn1 = handle.clone();
    let first = tokio::spawn(async move { turn1.user_message("first".into()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    handle.cancel_turn().await;

    // The actor clears the busy marker on cancel, so a new turn is accepted
    // immediately; the cancelled turn's late completion must be dropped.
    let accepted = handle.user_message("second".into()).await;
    assert!(accepted.is_ok(), "a new turn after cancel must be accepted");

    let first_result = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("cancelled turn did not stop")
        .expect("cancelled turn panicked");
    assert!(first_result.is_ok());

    // Wait for the actor to process both completions.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut stale_warning = false;
    while let Ok(envelope) = events.try_recv() {
        if let RuntimeEvent::Warning { message } = envelope.event
            && message.contains("stale model result dropped")
        {
            stale_warning = true;
        }
    }
    assert!(
        stale_warning,
        "the cancelled turn's late completion must be dropped with a warning"
    );
}

#[tokio::test]
async fn stop_ends_the_actor_cleanly() {
    let (handle, task) = start(Arc::new(StreamingModel)).await;
    let mut events = handle.subscribe();

    handle.user_message("hello".into()).await.unwrap();
    // Let the fast turn finish.
    tokio::time::sleep(Duration::from_millis(150)).await;

    handle.stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("actor task did not end after stop")
        .expect("actor task panicked");

    let mut run_completed = false;
    while let Ok(envelope) = events.try_recv() {
        if matches!(envelope.event, RuntimeEvent::RunCompleted) {
            run_completed = true;
        }
    }
    assert!(run_completed, "stop must emit RunCompleted");

    let after = handle.user_message("late".into()).await;
    assert!(
        after.is_err(),
        "commands after stop must fail, got: {after:?}"
    );
}

/// Records every `ContextQuery` the actor hands to the engine, so a test can
/// assert what slice of the provider window actually reaches the working set.
#[derive(Debug, Default)]
struct RecordingContextEngine {
    queries: Mutex<Vec<ContextQuery>>,
}

#[async_trait::async_trait]
impl ContextEngine for RecordingContextEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
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
            focus: None,
            items: Vec::new(),
            external: Vec::new(),
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

/// Declares a real provider window and max output so the derived frame
/// budget is meaningful.
#[derive(Debug)]
struct BudgetModel;

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
struct OneToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for OneToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a workspace file".into(),
            input_schema: serde_json::json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
        }]
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(agent_contracts::AgentError::Tool(
            "no tools configured".into(),
        ))
    }
}

#[tokio::test]
async fn engine_receives_only_the_context_frame_budget() {
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        context.clone(),
        Arc::new(BudgetModel),
        Arc::new(OneToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel.clone());
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();

    // The turn is a single model round; the engine query is recorded before
    // the actor replies, so the budget is observable immediately.
    let system_tokens = approx_tokens(&kernel.system_prompt());
    let turn_tokens = approx_layer_tokens(&[ModelMessage::user("hello")]);
    let tools_tokens = approx_layer_tokens(&kernel.tool_specs());
    let expected = ModelBudget::compute(30_000, 2_000, system_tokens, turn_tokens, tools_tokens)
        .context_frame_budget;

    {
        let queries = context.queries.lock().unwrap();
        assert_eq!(queries.len(), 1, "one model round -> one materialization");
        assert_eq!(
            queries[0].budget_tokens, expected,
            "the engine must receive the window minus output/system/turn/tools"
        );
    }

    handle.stop().await.unwrap();
}

#[tokio::test]
async fn dropping_all_handles_still_shuts_down_cleanly() {
    let kernel = kernel(Arc::new(StreamingModel));
    // An independent subscriber survives the handle drop and must still see
    // the teardown events: the actor runs full shutdown when every caller
    // handle is gone instead of returning silently.
    let mut events = kernel.subscribe();
    let (handle, task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    drop(handle);

    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("actor task did not end after all handles dropped")
        .expect("actor task panicked");

    let mut run_completed = false;
    while let Ok(envelope) = events.try_recv() {
        if matches!(envelope.event, RuntimeEvent::RunCompleted) {
            run_completed = true;
        }
    }
    assert!(
        run_completed,
        "dropping all handles must still run the full shutdown"
    );
}

/// The engine rejects the focus change: `FocusChanged` ingest fails, so any
/// task transition that depends on it must fail too — and the runtime's
/// task table must not move.
#[derive(Debug)]
struct FailingFocusContextEngine;

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
            focus: None,
            items: Vec::new(),
            external: Vec::new(),
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

/// Finishes a model round immediately with an empty reply.
#[derive(Debug)]
struct SilentModel;

#[async_trait::async_trait]
impl ModelTransport for SilentModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

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

/// The engine rejects the clear-focus transition (`FocusCleared` ingest
/// fails), so a suspend that depends on it must fail too — and the task
/// table must not move.
#[derive(Debug)]
struct FailingClearFocusContextEngine;

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
            focus: None,
            items: Vec::new(),
            external: Vec::new(),
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

/// Records every request it receives and finishes immediately, so the test
/// can assert on exactly what the final budget guard let through.
#[derive(Debug, Default)]
struct RecordingModel {
    requests: Mutex<Vec<ModelRequest>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for RecordingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 4_000,
            context_window: Some(10_000),
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

/// A context engine whose materialization returns three large working-set
/// items — enough to overshoot the input budget once assembled.
#[derive(Debug)]
struct BigContextEngine;

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
        let items: Vec<MaterializedItem> = (0..3)
            .map(|i| MaterializedItem {
                item_id: ContextItemId::new(),
                kind: ContextKind::FileObservation,
                scope: ContextScope::Task,
                attention: AttentionState::Active,
                semantic: SemanticState::Live,
                retention: ContextRetention::Working,
                content: format!("{}:{}", "data ".repeat(2400), i),
                source: None,
            })
            .collect();
        Ok(MaterializedContext {
            focus: None,
            items,
            external: Vec::new(),
            selected: Vec::new(),
            approx_tokens: 9_000,
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

/// A model whose window is almost entirely reserved for output: the input
/// budget is tiny, so any request is refused by the final guard.
#[derive(Debug, Default)]
struct TinyWindowModel {
    calls: AtomicUsize,
}

impl TinyWindowModel {
    fn calls(&self) -> usize {
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

#[tokio::test]
async fn final_guard_trims_to_the_input_budget_not_the_window() {
    // Window 10_000, output reserve 4_000 -> max input budget 6_000. Three
    // large context items assemble to ~9_000 + fixed layers, so the guard
    // must trim (the old guard compared against the window and let the
    // request eat into the output reserve).
    let model = Arc::new(RecordingModel::default());
    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        Arc::new(BigContextEngine),
        model.clone(),
        Arc::new(OneToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel.clone());
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the turn must send exactly one request");
        let request = &requests[0];
        let total = approx_layer_tokens(&request.messages) + approx_layer_tokens(&request.tools);
        assert!(
            total <= 6_000,
            "the assembled request must fit window - output_reserve (got {total})"
        );
        assert!(
            total > 3_000,
            "the guard must trim, not empty the context frame (got {total})"
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn final_guard_refuses_an_unshrinkable_over_budget_request() {
    // Window 1_000, output reserve 2_000 -> max input budget 0. The fixed
    // layers alone (system prompt, turn frame, tool schema) overshoot, so
    // no amount of trimming or unloading helps: the runtime must refuse to
    // send instead of silently over-budgeting the provider.
    let model = Arc::new(TinyWindowModel::default());
    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(OneToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel.clone());
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();

    let mut saw_error = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::Error { .. }) {
                saw_error = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        saw_error,
        "an unshrinkable over-budget request must surface a hard error"
    );
    assert_eq!(
        model.calls(),
        0,
        "an over-budget request must never reach the provider"
    );
    handle.stop().await.unwrap();
}
