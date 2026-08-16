//! Actor tests: command serialization, busy rejection, cancellation and
//! stale-result dropping. Uses minimal stubs for context/tools/model so the
//! actor is exercised against the engine contracts only.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{
    AgentResult, AttentionState, ContextConsumptionAck, ContextDiagnostics, ContextEngine,
    ContextIngress, ContextItemId, ContextItemSummary, ContextKind, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextRetention, ContextScope,
    ContextStateTransition, EventJournal, FocusState, MaterializedContext, MaterializedItem,
    ModelCapabilities, ModelChunk, ModelEventSink, ModelMessage, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, RuntimeEventEnvelope, ScopeId, ScopeKind, SemanticState,
    ToolCatalogEntry, ToolDispatcher, ToolExecutionRequest, ToolLifecycle, ToolOutcome, ToolRisk,
    ToolSpec, ToolSurfaceBlockReason, ToolSurfaceDemand, ToolSurfaceOmissionReason,
    ToolSurfacePlanReport, ToolSurfacePlanStatus, ToolSurfaceRequirement, ToolSurfaceSnapshot,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    ModelBudget, ModuleHost, RuntimeHandle, RuntimeInstance, RuntimeServices, approx_layer_tokens,
    engine_pack_window, spawn_runtime,
};

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

fn kernel(model: Arc<dyn ModelTransport>) -> Arc<RuntimeServices> {
    kernel_with(model, Arc::new(TestContextEngine))
}

fn kernel_with(
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
        busy.is_ok(),
        "a second user message while a turn runs is queued"
    );
    let overflow = handle.user_message("third".into()).await;
    assert!(
        overflow.is_err(),
        "a third user message while a turn runs and one is queued must be rejected"
    );
    assert!(
        overflow.unwrap_err().to_string().contains("queued"),
        "the overflow rejection must mention the queue"
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

    handle.cancel_turn().await.unwrap();
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

    handle.cancel_turn().await.unwrap();

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
    let mut consumed = false;
    while let Ok(envelope) = events.try_recv() {
        consumed |= matches!(&envelope.event, RuntimeEvent::ContextConsumed { .. });
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
    assert!(
        !consumed,
        "cancelled/stale model operations must not commit context consumption"
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
/// assert what slice of the pack window actually reaches the working set.
#[derive(Debug, Default)]
struct RecordingContextEngine {
    queries: Mutex<Vec<ContextQuery>>,
    user_messages: Mutex<Vec<String>>,
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
            output_budget: None,
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
    let config = CoreAuthorityConfig::default();
    let system_tokens = approx_tokens(&config.system_prompt);
    let tool_specs = OneToolDispatcher.specs();
    let tools_tokens = approx_layer_tokens(&tool_specs);
    let kernel = Arc::new(RuntimeServices::new(
        config,
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
    let turn_tokens = approx_layer_tokens(&[ModelMessage::user("hello")]);
    let pack_window = engine_pack_window(Some(30_000), 24_000);
    let expected =
        ModelBudget::compute(pack_window, 2_000, system_tokens, turn_tokens, tools_tokens)
            .context_frame_budget;

    {
        let queries = context.queries.lock().unwrap();
        assert_eq!(queries.len(), 1, "one model round -> one materialization");
        assert_eq!(
            queries[0].budget_tokens, expected,
            "the engine must receive the kernel pack cap minus output/system/turn/tools, not the larger provider send window"
        );
        assert!(
            pack_window < 30_000,
            "a 30k send window must not raise C's pack cap above the 24k kernel budget"
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
    let (handle, task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
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

/// Simulates the important half-commit case: focus ingest mutates the
/// engine, then the maintenance step fails. A transaction rollback must put
/// the engine back before the runtime rejects the task transition.
#[derive(Debug, Default)]
struct MutatingThenFailingFocusEngine {
    focus: Mutex<Option<FocusState>>,
    rollback_fails: bool,
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

#[tokio::test]
async fn maintenance_failure_rolls_back_context_before_rejecting_focus() {
    let context = Arc::new(MutatingThenFailingFocusEngine::default());
    let kernel = kernel_with(Arc::new(SilentModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let result = handle.set_focus("goal A".into()).await;
    assert!(result.is_err());
    assert!(handle.list_tasks().await.unwrap().is_empty());
    let materialized = context
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 0,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized.focus.is_none(),
        "the focus mutation must be rolled back with the rejected task transition"
    );
}

#[tokio::test]
async fn rollback_failure_poison_fences_further_runtime_mutation() {
    let context = Arc::new(MutatingThenFailingFocusEngine {
        focus: Mutex::new(None),
        rollback_fails: true,
    });
    let kernel = kernel_with(Arc::new(SilentModel), context);
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let first = handle.set_focus("goal A".into()).await.unwrap_err();
    assert!(first.to_string().contains("rollback failed"));
    let second = handle.set_focus("goal B".into()).await.unwrap_err();
    assert!(
        second.to_string().contains("runtime recovery is required"),
        "once alignment cannot be proven, later mutations must be fenced: {second}"
    );
    assert!(handle.list_tasks().await.unwrap().is_empty());
}

#[derive(Debug)]
struct FailFocusEventJournal;

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
struct FailSurfaceEventJournal;

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
struct FailConsumptionEventJournal;

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

#[tokio::test]
async fn journal_failure_after_focus_never_splits_task_and_context() {
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailFocusEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let result = handle.set_focus("goal A".into()).await;
    assert!(result.is_err(), "the journal failure must stay observable");
    let tasks = handle.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].status, agent_runtime::TaskStatus::Active);
    let focus = context
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 0,
            hints: Default::default(),
        })
        .await
        .unwrap()
        .focus;
    assert_eq!(
        focus.map(|focus| focus.task_id),
        Some(tasks[0].id),
        "journal failure may create an audit gap, but not split task authority from context"
    );
    let next = handle.set_focus("goal B".into()).await.unwrap_err();
    assert!(
        matches!(next, agent_contracts::AgentError::RecoveryRequired(_)),
        "an applied transition with a missing audit record must fence later mutation"
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
#[derive(Debug)]
struct RecordingModel {
    requests: Mutex<Vec<ModelRequest>>,
    calls: AtomicUsize,
    context_window: usize,
    max_output_tokens: usize,
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
struct BigContextEngine {
    acks: Mutex<Vec<ContextConsumptionAck>>,
    item_count: usize,
}

impl Default for BigContextEngine {
    fn default() -> Self {
        Self::with_items(3)
    }
}

impl BigContextEngine {
    fn with_items(item_count: usize) -> Self {
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
struct TinyWindowModel {
    calls: AtomicUsize,
}

/// A provider whose input window can change between rounds. The first round
/// is deliberately small enough that the optional schema must be omitted;
/// the second restores a large window so the same still-loaded tool can
/// reappear without a catalog mutation.
#[derive(Debug)]
struct VariableWindowModel {
    context_window: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
}

impl VariableWindowModel {
    fn new(context_window: usize) -> Self {
        Self {
            context_window: AtomicUsize::new(context_window),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn set_context_window(&self, context_window: usize) {
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
struct RoundLocalToolDispatcher {
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
    fn new() -> Self {
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

    fn roots_seen(&self) -> Vec<Vec<String>> {
        self.roots_seen.lock().unwrap().clone()
    }

    fn evicting_on_gc() -> Self {
        let dispatcher = Self::new();
        dispatcher.evict_on_gc.store(true, Ordering::SeqCst);
        dispatcher
    }

    fn schema_overflow() -> Self {
        Self {
            optional_description_chars: 20_000,
            ..Self::new()
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn optional_loaded(&self) -> bool {
        self.optional_loaded.load(Ordering::SeqCst)
    }

    fn unload_calls(&self) -> usize {
        self.unload_calls.load(Ordering::SeqCst)
    }

    fn load_calls(&self) -> usize {
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

async fn wait_for_turn_completed(
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

async fn wait_for_surface_plan(
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

async fn wait_for_ready_surface_and_model_start(
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
    let context = Arc::new(BigContextEngine::default());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
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
    {
        let acks = context.acks.lock().unwrap();
        assert_eq!(acks.len(), 1);
        assert!(
            !acks[0].item_ids.is_empty() && acks[0].item_ids.len() < 3,
            "the ack must name the final post-trim subset, got {:?}",
            acks[0].item_ids
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn send_guard_uses_provider_window_pack_uses_kernel_cap() {
    // Kernel pack 24k, provider send 50k, output reserve 4k → input 46k.
    // Append-like engine returns ~36k of history. Coupled 24k send would
    // trim below 24k; a competent A baseline must keep the larger send.
    let model = Arc::new(RecordingModel {
        context_window: 50_000,
        max_output_tokens: 4_000,
        ..RecordingModel::default()
    });
    let context = Arc::new(BigContextEngine::with_items(12));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
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
            total > 24_000,
            "A must be allowed to grow past the 24k pack cap when the send window is larger (got {total})"
        );
        assert!(
            total <= 46_000,
            "the send guard is still window - output_reserve (got {total})"
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn final_guard_omits_optional_schema_without_unloading_it() {
    let model = Arc::new(VariableWindowModel::new(1_600));
    let tools = Arc::new(RoundLocalToolDispatcher::new());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();

    handle.user_message("small budget".into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;
    let first_surface = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the small-budget round must complete");
        let total =
            approx_layer_tokens(&requests[0].messages) + approx_layer_tokens(&requests[0].tools);
        assert!(
            total <= 1_088,
            "the small round must fit context_window - output_reserve (got {total})"
        );
        let names: Vec<_> = requests[0]
            .tools
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(
            names.contains(&"core.read"),
            "core schema must be preserved"
        );
        assert!(
            !names.contains(&"optional.large"),
            "optional schema must be omitted from the small round"
        );
    }
    assert!(
        tools.optional_loaded(),
        "round-local omission must leave the catalog entry Loaded"
    );
    assert_eq!(tools.unload_calls(), 0, "the actor must never call unload");
    assert_eq!(tools.generation(), 17, "catalog generation must not change");
    assert!(first_surface.omitted.iter().any(|row| {
        row.tool_name == "optional.large"
            && row.reason == ToolSurfaceOmissionReason::ProviderInputBudget
    }));

    model.set_context_window(16_000);
    handle.user_message("restored budget".into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;
    let second_surface = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "the restored-budget round must complete");
        let names: Vec<_> = requests[1]
            .tools
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(names.contains(&"core.read"));
        assert!(
            names.contains(&"optional.large"),
            "the still-loaded optional schema must reappear when budget returns"
        );
    }
    assert!(tools.optional_loaded());
    assert_eq!(tools.unload_calls(), 0);
    assert_eq!(tools.generation(), 17);
    assert!(
        second_surface.surface_revision > first_surface.surface_revision,
        "every successful round needs a unique monotonic surface revision"
    );
    assert_eq!(
        first_surface.source_revisions.builtin_catalog_generation,
        second_surface.source_revisions.builtin_catalog_generation,
        "budget recovery must not masquerade as catalog mutation"
    );
    assert!(
        second_surface
            .selected
            .iter()
            .any(|row| row.tool_name == "optional.large")
    );

    handle.stop().await.unwrap();
}

#[tokio::test]
async fn keep_ready_reloads_after_gc_without_entering_the_model_surface() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::evicting_on_gc());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut turn_events = handle.subscribe();
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("keep the large tool ready".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let revision = handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::KeepReady,
                reason: "likely needed after the current step".into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    handle.user_message("run one round".into()).await.unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    let report = wait_for_ready_surface_and_model_start(&mut surface_events).await;

    assert_eq!(tools.load_calls(), 1, "Task demand must repair GC eviction");
    assert!(
        tools.optional_loaded(),
        "KeepReady leaves the catalog loaded"
    );
    assert!(
        report
            .selected
            .iter()
            .all(|row| row.tool_name != "optional.large")
    );
    assert!(report.omitted.iter().any(|row| {
        row.tool_name == "optional.large"
            && row.demand == ToolSurfaceDemand::KeepReady
            && row.reason == ToolSurfaceOmissionReason::KeepReady
    }));
    assert_eq!(report.source_revisions.task_requirement_revision, Some(1));
    assert!(report.source_revisions.focus_revision.is_some());
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .tools
                .iter()
                .all(|spec| spec.name != "optional.large"),
            "KeepReady is a lifecycle root, not a prompt-visibility request"
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn active_task_tool_demand_reaches_gc_as_roots() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::new());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut turn_events = handle.subscribe();
    handle.start().await.unwrap();

    // Before any task exists, the runtime passes no roots.
    handle
        .user_message("first round, no requirements yet".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    assert!(
        tools.roots_seen().last().unwrap().is_empty(),
        "no active task means no tool roots"
    );

    // A task that requires a tool must hand that tool's name to gc() as a
    // root on every later round — idle GC cannot age it off the surface.
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "the task needs the large tool".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("rooted round".into()).await.unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    let seen = tools.roots_seen();
    let roots = seen.last().unwrap();
    assert!(
        roots.iter().any(|root| root == "optional.large"),
        "the task-demanded tool must be a gc root, got {roots:?}"
    );

    // Suspending the task drops the roots again (no active task demand).
    handle.suspend_task().await.unwrap();
    handle.user_message("taskless round".into()).await.unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    assert!(
        tools.roots_seen().last().unwrap().is_empty(),
        "a suspended task must not keep rooting tools"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn checkpoint_restore_rebuilds_surface_from_suspended_task_requirements() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::evicting_on_gc());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    let mut surface_events = handle.subscribe();
    let mut turn_events = handle.subscribe();
    handle.start().await.unwrap();
    handle.set_focus("restore task roots".into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let requirement = ToolSurfaceRequirement {
        tool_name: "optional.large".into(),
        demand: ToolSurfaceDemand::KeepReady,
        reason: "survive suspension and restore".into(),
    };
    handle
        .replace_task_tool_requirements(task_id, 0, vec![requirement])
        .await
        .unwrap();

    handle
        .user_message("first rooted round".into())
        .await
        .unwrap();
    let first = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    wait_for_turn_completed(&mut turn_events).await;
    handle.suspend_task().await.unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();

    // Diverge both the task requirements and the issued surface counter.
    handle
        .replace_task_tool_requirements(task_id, 1, Vec::new())
        .await
        .unwrap();
    handle.activate_task(task_id).await.unwrap();
    handle.user_message("diverged round".into()).await.unwrap();
    let second = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    wait_for_turn_completed(&mut turn_events).await;
    handle.suspend_task().await.unwrap();

    // Restoring an older checkpoint must recover requirement revision 1,
    // rebuild the surface rather than reuse a snapshot, and preserve
    // monotonic focus/surface identities from the live process.
    instance.restore(checkpoint).await.unwrap();
    assert!(
        handle
            .replace_task_tool_requirements(task_id, 2, Vec::new())
            .await
            .is_err(),
        "a writer holding the pre-restore revision must not pass CAS after restore"
    );
    handle.activate_task(task_id).await.unwrap();
    handle
        .user_message("continue restored work".into())
        .await
        .unwrap();
    let third = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    wait_for_turn_completed(&mut turn_events).await;

    assert_eq!(second.source_revisions.task_requirement_revision, Some(2));
    assert_eq!(third.source_revisions.task_requirement_revision, Some(3));
    assert!(third.omitted.iter().any(|row| {
        row.tool_name == "optional.large"
            && row.demand == ToolSurfaceDemand::KeepReady
            && row.reason == ToolSurfaceOmissionReason::KeepReady
    }));
    assert!(first.surface_revision < second.surface_revision);
    assert!(second.surface_revision < third.surface_revision);
    assert!(
        second.source_revisions.focus_revision < third.source_revisions.focus_revision,
        "restoring an older checkpoint must not move the runtime focus epoch backwards"
    );
    assert_eq!(tools.load_calls(), 2);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_restore_cas_high_water_survives_a_checkpoint_that_removes_the_task() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    let empty_checkpoint = instance.checkpoint().await.unwrap();

    handle
        .set_focus("task that will disappear".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::PreferSurface,
                reason: String::new(),
            }],
        )
        .await
        .unwrap();
    handle.suspend_task().await.unwrap();
    let task_checkpoint = instance.checkpoint().await.unwrap();
    handle
        .replace_task_tool_requirements(task_id, 1, Vec::new())
        .await
        .unwrap();

    instance.restore(empty_checkpoint).await.unwrap();
    assert!(handle.list_tasks().await.unwrap().is_empty());
    instance.restore(task_checkpoint).await.unwrap();
    let restored = handle.list_tasks().await.unwrap();
    assert_eq!(restored[0].tool_requirement_revision, 3);
    assert_eq!(restored[0].tool_requirement_count, 1);
    assert!(
        handle
            .replace_task_tool_requirements(task_id, 2, Vec::new())
            .await
            .is_err(),
        "a task disappearing from an intermediate restore must not erase its CAS high-water mark"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn must_surface_overflow_is_unsatisfiable_before_model_start() {
    let model = Arc::new(VariableWindowModel::new(1_600));
    let tools = Arc::new(RoundLocalToolDispatcher::new());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools,
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut surface_events = handle.subscribe();
    let mut all_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("must use the large tool".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "the task cannot proceed without it".into(),
            }],
        )
        .await
        .unwrap();

    handle
        .user_message("try the constrained round".into())
        .await
        .unwrap();
    let report = wait_for_surface_plan(&mut surface_events).await;
    assert_eq!(
        report.status,
        ToolSurfacePlanStatus::Unsatisfiable {
            reason: ToolSurfaceBlockReason::ProviderInputBudget,
        }
    );
    assert!(report.blocked.iter().any(|row| {
        row.tool_name == "optional.large" && row.demand == ToolSurfaceDemand::MustSurface
    }));

    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut saw_model_started = false;
    while let Ok(envelope) = all_events.try_recv() {
        saw_model_started |= matches!(envelope.event, RuntimeEvent::ModelStarted { .. });
    }
    assert!(
        !saw_model_started,
        "an unsatisfiable round must not claim the model started"
    );
    assert!(
        model.requests.lock().unwrap().is_empty(),
        "an unsatisfiable MustSurface requirement must never reach the provider"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn unavailable_must_surface_is_reported_before_model_start() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("missing required tool".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "missing.tool".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "exercise unavailable handling".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("start".into()).await.unwrap();

    let report = wait_for_surface_plan(&mut surface_events).await;
    assert_eq!(
        report.status,
        ToolSurfacePlanStatus::Unsatisfiable {
            reason: ToolSurfaceBlockReason::Unavailable,
        }
    );
    assert!(
        report
            .blocked
            .iter()
            .any(|row| row.tool_name == "missing.tool")
    );
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn mandatory_schema_cap_is_reported_before_model_start() {
    let model = Arc::new(VariableWindowModel::new(32_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::schema_overflow()),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("oversized required schema".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "exercise schema cap handling".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("start".into()).await.unwrap();

    let report = wait_for_surface_plan(&mut surface_events).await;
    assert_eq!(
        report.status,
        ToolSurfacePlanStatus::Unsatisfiable {
            reason: ToolSurfaceBlockReason::SchemaBudget,
        }
    );
    assert!(report.mandatory_schema_tokens > agent_runtime::budget::MAX_TOOL_SURFACE_TOKENS);
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn surface_event_failure_aborts_before_model_start_and_provider_call() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailSurfaceEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("exercise event fence".into())
        .await
        .unwrap();

    let saw_model_started = tokio::time::timeout(Duration::from_secs(3), async {
        let mut saw_model_started = false;
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::Error { .. },
                    ..
                }) => break saw_model_started,
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ModelStarted { .. },
                    ..
                }) => saw_model_started = true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime closed before surfacing the journal failure")
                }
            }
        }
    })
    .await
    .expect("surface event failure was not surfaced");

    assert!(!saw_model_started);
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn consumption_event_failure_rolls_back_reinforcement_and_aborts_the_turn() {
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailConsumptionEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .pin("journal-protected constraint".into())
        .await
        .unwrap();
    handle
        .user_message("exercise context ack rollback".into())
        .await
        .unwrap();

    let (saw_consumed, saw_assistant, saw_turn_completed) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut saw_consumed = false;
            let mut saw_assistant = false;
            let mut saw_turn_completed = false;
            loop {
                match events.recv().await {
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::ContextConsumed { .. },
                        ..
                    }) => saw_consumed = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::AssistantMessage { .. },
                        ..
                    }) => saw_assistant = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::TurnCompleted,
                        ..
                    }) => saw_turn_completed = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::Error { message },
                        ..
                    }) if message.contains("failed to commit model context consumption") => {
                        break (saw_consumed, saw_assistant, saw_turn_completed);
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("runtime closed before surfacing the consumption journal failure")
                    }
                }
            }
        })
        .await
        .expect("context-consumption journal failure was not surfaced");

    assert!(!saw_consumed, "a failed append must not be broadcast");
    assert!(
        !saw_assistant,
        "the failed consumption commit must abort output commit"
    );
    assert!(!saw_turn_completed, "the failed turn must not commit");
    let pinned = context
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.kind == ContextKind::Constraint)
        .expect("pinned constraint survives rollback");
    assert_eq!(
        pinned.access_count, 0,
        "the checkpoint rollback must remove the unjournaled reinforcement"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn unsatisfiable_surface_event_failure_is_reported_and_provider_fenced() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailSurfaceEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("missing required tool with a failing journal".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "missing.tool".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "exercise the unsatisfiable audit fence".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("start".into()).await.unwrap();

    let (message, saw_model_started) = tokio::time::timeout(Duration::from_secs(3), async {
        let mut saw_model_started = false;
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::Error { message },
                    ..
                }) if message
                    .contains("failed to persist the unavailable-tool surface decision") =>
                {
                    break (message, saw_model_started);
                }
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ModelStarted { .. },
                    ..
                }) => saw_model_started = true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime closed before surfacing the unsatisfiable journal failure")
                }
            }
        }
    })
    .await
    .expect("unsatisfiable surface event failure was not surfaced");

    assert!(message.contains("simulated surface-plan journal failure"));
    assert!(!saw_model_started);
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn final_guard_refuses_an_unshrinkable_over_budget_request() {
    // Window 1_000, output reserve 2_000 -> max input budget 0. The fixed
    // layers alone (system prompt, turn frame, mandatory tool schema)
    // overshoot, so no amount of round-local omission helps: refuse to
    // send instead of silently over-budgeting the provider.
    let model = Arc::new(TinyWindowModel::default());
    let context = Arc::new(BigContextEngine::default());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
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
    assert!(
        context.acks.lock().unwrap().is_empty(),
        "a refused provider request must not commit context consumption"
    );
    handle.stop().await.unwrap();
}

/// Records every event the actor appends and every barrier flush, with a
/// switch to make the barrier fail. Lets a test assert the turn-commit
/// ordering: all mandatory state writes precede `TurnCompleted`, and
/// `TurnCompleted` is broadcast only after the barrier succeeds.
#[derive(Debug, Default)]
struct BarrierJournal {
    appended: Mutex<Vec<String>>,
    flushes: AtomicUsize,
    fail_flush: AtomicBool,
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

fn kernel_with_journal(journal: Arc<dyn EventJournal>) -> Arc<RuntimeServices> {
    Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal),
    ))
}

#[tokio::test]
async fn turn_completed_is_broadcast_only_after_the_barrier() {
    let journal = Arc::new(BarrierJournal::default());
    let kernel = kernel_with_journal(journal.clone());
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();

    handle.user_message("hello".into()).await.unwrap();

    // The actor flushes the journal before broadcasting, so by the time the
    // subscriber sees TurnCompleted the barrier has already passed.
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

    assert!(
        journal.flushes.load(Ordering::SeqCst) >= 1,
        "TurnCompleted must be broadcast only after a barrier flush"
    );
    {
        let appended = journal.appended.lock().unwrap();
        assert_eq!(
            appended.last().map(String::as_str),
            Some("TurnCompleted"),
            "TurnCompleted must be the last event appended before the barrier"
        );
        assert!(
            appended
                .iter()
                .any(|name| name.starts_with("AssistantMessage")),
            "mandatory state writes must be appended before TurnCompleted: {appended:?}"
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn failed_barrier_blocks_turn_completed_and_marks_recovery_required() {
    let journal = Arc::new(BarrierJournal {
        fail_flush: AtomicBool::new(true),
        ..BarrierJournal::default()
    });
    let kernel = kernel_with_journal(journal.clone());
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();

    handle.user_message("hello".into()).await.unwrap();

    let mut commit_failed_phase = None;
    let mut saw_recovery = false;
    let mut saw_turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && (commit_failed_phase.is_none() || !saw_recovery)
    {
        match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(envelope)) => match envelope.event {
                RuntimeEvent::TurnCommitFailed { phase, .. } => commit_failed_phase = Some(phase),
                RuntimeEvent::RecoveryRequired => saw_recovery = true,
                RuntimeEvent::TurnCompleted => saw_turn_completed = true,
                _ => {}
            },
            _ => break,
        }
    }

    assert_eq!(
        commit_failed_phase.as_deref(),
        Some("turn_completed_event"),
        "the failure must be reported at the barrier step, after every mandatory state write"
    );
    assert!(
        saw_recovery,
        "a failed barrier must mark the runtime recovery required"
    );
    assert!(
        !saw_turn_completed,
        "TurnCompleted must never be broadcast when the barrier fails"
    );
    assert!(
        journal.flushes.load(Ordering::SeqCst) >= 1,
        "the barrier must have been attempted"
    );
    {
        let appended = journal.appended.lock().unwrap();
        assert!(
            appended.iter().any(|name| name == "TurnCompleted"),
            "TurnCompleted must be appended into the FIFO before the failed flush: {appended:?}"
        );
    }
    let next = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("a failed durability barrier must fence later mutation");
    assert!(
        matches!(next, agent_contracts::AgentError::RecoveryRequired(_)),
        "the runtime must stay fenced after the failed barrier: {next}"
    );
    // The operator repairs storage before the runtime may run again: with
    // the barrier healthy, stop's own flush succeeds and teardown is clean.
    journal.fail_flush.store(false, Ordering::SeqCst);
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn failed_cancel_barrier_returns_recovery_required_and_never_claims_completion() {
    let journal = Arc::new(BarrierJournal::default());
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(HangingModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal.clone()),
    ));
    let (handle, _task) = spawn_runtime(services);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("cancel me".into()).await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ModelStarted { .. },
                    ..
                }) => break,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime closed before the model operation started")
                }
            }
        }
    })
    .await
    .expect("model operation did not start");

    journal.fail_flush.store(true, Ordering::SeqCst);
    let error = handle
        .cancel_turn()
        .await
        .expect_err("a failed cancellation barrier must reach the caller");
    assert!(matches!(
        error,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));

    let mut saw_cancelled = false;
    let mut saw_completed = false;
    while let Ok(envelope) = events.try_recv() {
        saw_cancelled |= matches!(envelope.event, RuntimeEvent::TurnCancelled { .. });
        saw_completed |= matches!(envelope.event, RuntimeEvent::TurnCompleted);
    }
    assert!(
        !saw_cancelled,
        "TurnCancelled is broadcast only after its durable barrier passes"
    );
    assert!(
        !saw_completed,
        "cancellation must never reuse the successful completion marker"
    );
    {
        let appended = journal.appended.lock().unwrap();
        assert!(
            appended
                .iter()
                .any(|name| name.starts_with("TurnCancelled")),
            "the cancellation marker must be the event covered by the attempted barrier"
        );
    }
    let next = handle
        .user_message("must recover first".into())
        .await
        .expect_err("failed cancellation persistence must fence later mutation");
    assert!(matches!(
        next,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));

    journal.fail_flush.store(false, Ordering::SeqCst);
    handle.stop().await.unwrap();
}

/// CORE-04 end to end: the composition-root output broker runs inside the
/// kernel, spills an oversized tool result to the run's artifact directory
/// and the model-facing preview stays bounded — the truncated middle is no
/// longer lost for a producer that did not spill.
#[tokio::test]
async fn output_broker_spills_oversized_tool_output_end_to_end() {
    use agent_contracts::{CancellationToken, MAX_TOOL_MODEL_CONTENT_CHARS, ToolCall, ToolOutput};
    use agent_workspace::{Workspace, WorkspaceOutputBroker};

    struct BigOutputDispatcher {
        output: ToolOutput,
    }
    #[async_trait::async_trait]
    impl ToolDispatcher for BigOutputDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "big.tool".into(),
                description: "oversized".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
            }]
        }
        async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            Ok(ToolOutcome::Value(self.output.clone()))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(dir.path()).await.unwrap());
    let full_content = format!("BEGIN{}\nEND", "payload".repeat(10_000));
    let surface = ToolSurfaceSnapshot {
        specs: vec![ToolSpec {
            name: "big.tool".into(),
            description: "oversized".into(),
            input_schema: serde_json::json!({}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }],
        ..ToolSurfaceSnapshot::default()
    };
    let core = agent_core::build_core_port(
        CoreAuthorityConfig {
            output_broker: Some(Arc::new(WorkspaceOutputBroker::new(workspace.clone()))),
            ..CoreAuthorityConfig::default()
        },
        Arc::new(TestContextEngine),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "big.tool".into(),
                ok: true,
                summary: "done".into(),
                model_content: full_content.clone(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let run_id = core.run_id();
    let generation = core.current_authority_epoch();
    let tool_call = ToolCall {
        id: "c1".into(),
        name: "big.tool".into(),
        arguments: serde_json::json!({}),
    };
    let identity = agent_contracts::ToolOperationIdentity {
        run_id,
        task_id: None,
        turn_id: agent_contracts::TurnId::new(),
        scope_id: None,
        operation_id: agent_contracts::OperationId::new(),
        generation,
        call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        argument_digest: agent_contracts::ArgumentDigest::from_json(&tool_call.arguments),
    };
    let agent_core::ToolOperationAdmission::Accepted { permit, .. } = core
        .admit_tool_operation(identity, &tool_call, generation)
        .expect("test operation admission must succeed")
    else {
        panic!("fresh test operation must receive a dispatch permit")
    };
    let permit = core
        .publish_tool_operation(permit, &tool_call)
        .await
        .unwrap();
    let execution = core
        .execute_published_tool(permit, tool_call, CancellationToken::new(), &surface)
        .await;
    let agent_core::CoreToolExecution { outcome, lease, .. } = execution;
    assert!(
        lease.is_none(),
        "a read-only call carries no commit-time lease"
    );
    let ToolOutcome::Value(output) = outcome else {
        panic!("expected a plain value outcome");
    };
    assert!(
        output.model_content.chars().count() <= MAX_TOOL_MODEL_CONTENT_CHARS,
        "the model-facing preview must stay bounded"
    );
    assert!(output.model_content.contains("output broker truncated"));
    let reference = output.artifact_ref.expect("oversized output must spill");
    assert!(reference.starts_with("artifact://v1/"));
    let locator = agent_contracts::ArtifactLocator::parse(&reference).expect("sealed locator");
    assert_eq!(locator.owner(), "tool-output");
    assert!(locator.is_sealed());

    // The full content was stored once under the run's artifact directory.
    let path = workspace
        .state_dir()
        .join("artifacts")
        .join(run_id.to_string())
        .join(locator.owner())
        .join(locator.digest().unwrap().to_string());
    let stored = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(stored, full_content);
}

#[tokio::test]
async fn user_message_event_is_a_bounded_preview_while_ingest_keeps_the_body() {
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = kernel_with(Arc::new(StreamingModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    let body = "unique-user-input-".to_string() + &"x".repeat(400);
    handle.user_message(body.clone()).await.unwrap();

    let mut accepted = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                accepted = Some(input);
            }
        }
        if accepted.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let input = accepted.expect("UserMessageAccepted");
    assert_eq!(
        input.preview.chars().count(),
        agent_contracts::USER_INPUT_PREVIEW_CHARS
    );
    assert!(
        !input.preview.contains(&body),
        "the journal must not carry the full body"
    );
    assert_eq!(input.bytes, body.len() as u64);
    assert_eq!(input.kind, agent_contracts::InputKind::Dialogue);
    assert_eq!(input.lifecycle, agent_contracts::InputLifecycle::Applied);
    assert_eq!(input.source, agent_contracts::InputSource::User);
    assert_eq!(
        input.authority,
        agent_contracts::InputAuthority::UserSteering
    );
    assert!(input.proposal.is_none());
    assert!(
        input.body_ref.is_none(),
        "this kernel has no artifact workspace"
    );
    {
        let ingested = context.user_messages.lock().unwrap();
        assert_eq!(ingested.as_slice(), std::slice::from_ref(&body));
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn focus_and_cancel_commands_do_not_go_through_the_user_message_envelope() {
    let (handle, _task) = start(Arc::new(StreamingModel)).await;
    let mut events = handle.subscribe();
    handle
        .set_focus("keep the auth service".into())
        .await
        .unwrap();
    handle.cancel_turn().await.unwrap();

    let mut saw_user_message = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::UserMessageAccepted { .. }) {
                saw_user_message = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !saw_user_message,
        "/focus and /cancel must stay direct RuntimeCommand paths"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn user_message_stores_the_exact_body_once_when_a_workspace_is_wired() {
    let dir = tempfile::tempdir().unwrap();
    let workspace =
        std::sync::Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let context = Arc::new(RecordingContextEngine::default());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(StreamingModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace.clone());
    let (handle, _task) = spawn_runtime(std::sync::Arc::new(services));
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    let body = "exact user body for evidence plane";
    handle.user_message(body.into()).await.unwrap();

    let mut accepted = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                accepted = Some(input);
            }
        }
        if accepted.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let input = accepted.expect("UserMessageAccepted");
    let reference = input.body_ref.expect("workspace must seal a body_ref");
    let locator = agent_contracts::ArtifactLocator::parse(&reference).expect("sealed locator");
    assert_eq!(locator.owner(), "user-input");
    assert_eq!(
        input.digest.as_deref(),
        locator.digest().map(|d| d.to_string()).as_deref()
    );
    let path = workspace
        .state_dir()
        .join("artifacts")
        .join(locator.run_id().to_string())
        .join(locator.owner())
        .join(locator.digest().unwrap().to_string());
    let stored = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(stored, body);
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn busy_user_message_is_recorded_as_rejected_and_not_ingested() {
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = kernel_with(Arc::new(HangingModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    let turn = handle.clone();
    let turn_task = tokio::spawn(async move { turn.user_message("first".into()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    handle.user_message("second".into()).await.unwrap();
    let overflow = handle.user_message("third".into()).await;
    assert!(
        overflow.unwrap_err().to_string().contains("queued"),
        "overflow UserMessage still fail-closes"
    );

    let mut rejected = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event
                && input.lifecycle == agent_contracts::InputLifecycle::Rejected
            {
                rejected = Some(input);
            }
        }
        if rejected.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let input = rejected.expect("overflow UserMessage must leave a Rejected record");
    assert_eq!(input.preview, "third");
    assert!(
        input.turn_id.is_none(),
        "rejected input never started a turn"
    );
    assert!(input.body_ref.is_none(), "rejected input is not sealed");
    {
        let ingested = context.user_messages.lock().unwrap();
        assert_eq!(
            ingested.as_slice(),
            &["first".to_string()],
            "rejected body must not enter context"
        );
    }

    handle.cancel_turn().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), turn_task).await;
    handle.cancel_turn().await.unwrap();
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn queued_user_message_applies_after_the_busy_turn_is_cancelled() {
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = kernel_with(Arc::new(HangingModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    let turn = handle.clone();
    let turn_task = tokio::spawn(async move { turn.user_message("first".into()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.user_message("second".into()).await.unwrap();

    let mut queued = None;
    let mut applied_first = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                if input.lifecycle == agent_contracts::InputLifecycle::Applied
                    && input.preview == "first"
                {
                    applied_first = Some(input);
                } else if input.lifecycle == agent_contracts::InputLifecycle::Queued {
                    queued = Some(input);
                }
            }
        }
        if queued.is_some() && applied_first.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued = queued.expect("second UserMessage must be Queued");
    let applied_first = applied_first.expect("first UserMessage must be Applied");
    assert_eq!(queued.preview, "second");
    assert_eq!(queued.causal_parent, applied_first.input_id);
    assert!(
        context.user_messages.lock().unwrap().as_slice() == ["first".to_string()],
        "queued body must not ingest until the busy turn ends"
    );

    handle.cancel_turn().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), turn_task).await;

    let mut applied_second = false;
    let mut interrupted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                if input.lifecycle == agent_contracts::InputLifecycle::InterruptCommitted {
                    interrupted = true;
                    assert_eq!(input.kind, agent_contracts::InputKind::CancelTurn);
                    assert_eq!(input.causal_parent, applied_first.input_id);
                }
                if input.lifecycle == agent_contracts::InputLifecycle::Applied
                    && input.preview == "second"
                {
                    applied_second = true;
                    assert_eq!(input.input_id, queued.input_id);
                }
            }
        }
        if applied_second && interrupted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(interrupted, "cancel must publish InterruptCommitted");
    assert!(applied_second, "queued dialogue must apply after cancel");
    assert!(
        context
            .user_messages
            .lock()
            .unwrap()
            .iter()
            .any(|body| body == "second"),
        "drained queue must ingest the queued body"
    );

    handle.cancel_turn().await.unwrap();
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn applied_user_input_is_consumed_then_archived_when_the_turn_commits() {
    let (handle, _task) = start(Arc::new(StreamingModel)).await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut lifecycles = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                lifecycles.push(input.lifecycle);
            }
        }
        if lifecycles.contains(&agent_contracts::InputLifecycle::Archived) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        lifecycles.contains(&agent_contracts::InputLifecycle::Applied),
        "turn start must publish Applied, got {lifecycles:?}"
    );
    assert!(
        lifecycles.contains(&agent_contracts::InputLifecycle::Consumed),
        "model consumption must publish Consumed, got {lifecycles:?}"
    );
    assert!(
        lifecycles.contains(&agent_contracts::InputLifecycle::Archived),
        "TurnCompleted must publish Archived, got {lifecycles:?}"
    );
    handle.stop().await.unwrap();
}
