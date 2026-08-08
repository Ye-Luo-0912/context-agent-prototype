//! Actor tests: command serialization, busy rejection, cancellation and
//! stale-result dropping. Uses minimal stubs for context/tools/model so the
//! actor is exercised against the engine contracts only.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{
    AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    MaterializedContext, ModelCapabilities, ModelChunk, ModelEventSink, ModelMessage, ModelOutput,
    ModelRequest, ModelTransport, RuntimeEvent, ScopeId, ScopeKind, ToolDispatcher,
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
    Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        Arc::new(TestContextEngine),
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
