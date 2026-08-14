//! Turn execution and streaming behavior at the actor level: the five-layer
//! model input, the execution stack (Turn Frame) versus the long-term
//! working set (Context Frame), and cancellation of a hanging model round.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, AttentionState, Capability, CapabilityInvocationContext,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemId,
    ContextItemSummary, ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, ContextScope, ContextStateTransition, Effect, EffectDurability, EffectReceipt,
    EventJournal, MaterializedContext, ModelCapabilities, ModelChunk, ModelEventSink, ModelMessage,
    ModelOutput, ModelRequest, ModelRole, ModelTransport, OperationId, RunId, RuntimeEvent,
    RuntimeEventEnvelope, ScopeId, ScopeKind, TaskId, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec, TurnCancelAck,
    TurnCancellationReason,
};

use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    CapabilityAwareDispatcher, CapabilityRegistry, ModuleHost, RuntimeHandle, RuntimeInstance,
    RuntimeServices, spawn_runtime,
};
use serde_json::json;
use tokio::sync::Mutex;

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

/// Emits one live-only delta, then stays in-flight until cancellation.
#[derive(Debug)]
struct StreamingHangingModel;

#[async_trait::async_trait]
impl ModelTransport for StreamingHangingModel {
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
        sink.on_chunk(ModelChunk::TextDelta {
            delta: "partial".into(),
        })
        .await?;
        request.cancel.cancelled().await;
        Err(AgentError::Cancelled)
    }
}

/// Records every ingest and maintain call the runtime makes, so the test can
/// assert *when* observations reached the long-term context. Also counts
/// full GC passes, so `context.collect` routing is observable. `activity`
/// is a strictly ordered log of ingests and materializations, so tests can
/// assert that a runtime directive took effect before the next model round.
#[derive(Debug, Default)]
struct RecordingContextEngine {
    ingests: Arc<Mutex<Vec<String>>>,
    maintains: Arc<Mutex<Vec<ContextMaintenanceTrigger>>>,
    gcs: Arc<Mutex<usize>>,
    activity: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ContextEngine for RecordingContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let label = match &ingress {
            ContextIngress::UserMessage { .. } => "UserMessage",
            ContextIngress::AssistantMessage { .. } => "AssistantMessage",
            ContextIngress::ToolObservation { .. } => "ToolObservation",
            ContextIngress::FocusChanged { .. } => "FocusChanged",
            ContextIngress::FocusCleared => "FocusCleared",
            ContextIngress::Pin { .. } => "Pin",
            ContextIngress::TaskCompleted { .. } => "TaskCompleted",
            ContextIngress::ContextDirective { .. } => "ContextDirective",
            ContextIngress::WorkingSetSignal { .. } => "WorkingSetSignal",
        };
        self.ingests.lock().await.push(label.to_string());
        self.activity.lock().await.push(label.to_string());
        Ok(())
    }
    async fn gc(&self) -> AgentResult<agent_contracts::ContextGcReport> {
        *self.gcs.lock().await += 1;
        Ok(agent_contracts::ContextGcReport::default())
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.maintains.lock().await.push(trigger);
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        self.activity.lock().await.push("Materialize".into());
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
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

/// One tool call on the first model round, plain text on the second, and a
/// record of every message list it was given.
#[derive(Debug, Default)]
struct TwoRoundToolModel {
    rounds: AtomicUsize,
    requests: Arc<Mutex<Vec<Vec<ModelMessage>>>>,
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

#[derive(Debug)]
struct OkToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for OkToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: true,
            summary: "ok".into(),
            model_content: "ok from fs.read".into(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

#[derive(Debug)]
struct CountingToolDispatcher {
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolDispatcher for CountingToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        OkToolDispatcher.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        OkToolDispatcher.execute(request).await
    }
}

async fn spawn_with(
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

async fn spawn_with_approval(
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

#[derive(Debug, Default)]
struct SequenceJournal {
    envelopes: std::sync::Mutex<Vec<RuntimeEventEnvelope>>,
}

#[async_trait::async_trait]
impl EventJournal for SequenceJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        self.envelopes.lock().unwrap().push(envelope.clone());
        Ok(())
    }
}

async fn spawn_with_journal(
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

#[tokio::test]
async fn actor_streams_model_deltas_to_subscribers() {
    let journal = Arc::new(SequenceJournal::default());
    let handle = spawn_with_journal(Arc::new(StreamingModel), journal.clone()).await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut deltas = Vec::new();
    let mut delta_cursors = Vec::new();
    let mut model_started_cursor = None;
    let mut final_content = None;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            let cursor = envelope.seq;
            match envelope.event {
                RuntimeEvent::ModelStarted { .. } => model_started_cursor = Some(cursor),
                RuntimeEvent::ModelDelta {
                    delta,
                    operation_id,
                    ..
                } => {
                    // Every delta must belong to the round that emitted it:
                    // the fence identity is present, not defaulted.
                    assert!(operation_id != OperationId::default());
                    deltas.push(delta);
                    delta_cursors.push(cursor);
                }
                RuntimeEvent::AssistantMessage { content } => final_content = Some(content),
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if turn_completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(turn_completed, "the turn must complete");
    assert_eq!(deltas, vec!["Hello ".to_string(), "world".to_string()]);
    assert_eq!(
        delta_cursors,
        vec![model_started_cursor.unwrap(); 2],
        "live deltas repeat their opening durable cursor; their operation identity is the fence"
    );
    assert_eq!(final_content.as_deref(), Some("Hello world"));

    handle.stop().await.unwrap();
    let durable = journal.envelopes.lock().unwrap();
    assert!(
        durable
            .iter()
            .all(|envelope| !matches!(envelope.event, RuntimeEvent::ModelDelta { .. })),
        "streaming deltas are live-only and must never enter the recovery trace"
    );
    for (expected, envelope) in (1u64..).zip(durable.iter()) {
        assert_eq!(
            envelope.seq, expected,
            "live-only deltas must not consume durable journal sequence numbers"
        );
    }
    assert!(
        matches!(
            durable.last().map(|envelope| &envelope.event),
            Some(RuntimeEvent::RunCompleted)
        ),
        "the contiguous healthy trace must include terminal shutdown"
    );
}

#[tokio::test]
async fn streamed_cancellation_and_shutdown_leave_a_contiguous_recovery_trace() {
    let journal = Arc::new(SequenceJournal::default());
    let handle = spawn_with_journal(Arc::new(StreamingHangingModel), journal.clone()).await;
    let mut events = handle.subscribe();
    handle
        .user_message("cancel after streaming".into())
        .await
        .unwrap();

    let (model_started_cursor, delta_cursor): (u64, u64) =
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut started = None;
            loop {
                let envelope = events.recv().await.unwrap();
                match envelope.event {
                    RuntimeEvent::ModelStarted { .. } => started = Some(envelope.seq),
                    RuntimeEvent::ModelDelta { .. } => {
                        break (
                            started.expect("ModelStarted precedes its deltas"),
                            envelope.seq,
                        );
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("stream never started");

    handle.cancel_turn().await.unwrap();
    handle.stop().await.unwrap();

    let durable = journal.envelopes.lock().unwrap();
    assert_eq!(delta_cursor, model_started_cursor);
    assert!(
        durable
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TurnCancelled { .. }))
    );
    assert!(
        durable
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::RunCompleted))
    );
    assert!(
        durable
            .iter()
            .all(|envelope| !matches!(envelope.event, RuntimeEvent::TurnCompleted)),
        "cancellation must not become a successful commit marker"
    );
    for (expected, envelope) in (1u64..).zip(durable.iter()) {
        assert_eq!(envelope.seq, expected);
    }
}

/// Returns one plain text answer with a fixed provider usage report.
#[derive(Debug)]
struct UsageModel(u64, u64);

#[async_trait::async_trait]
impl ModelTransport for UsageModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "done".into(),
            tool_calls: Vec::new(),
            usage: agent_contracts::ModelUsage {
                input_tokens: Some(self.0),
                output_tokens: Some(self.1),
            },
        })
    }
}

#[tokio::test]
async fn actor_reports_provider_usage_via_model_used() {
    let handle = spawn_with(
        Arc::new(UsageModel(321, 78)),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut used: Option<(u64, u64)> = None;
    let mut turn_completed = false;
    for _ in 0..500 {
        if let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ModelUsed {
                    input_tokens,
                    output_tokens,
                } => used = Some((input_tokens, output_tokens)),
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if turn_completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(turn_completed, "the turn must complete");
    assert_eq!(
        used,
        Some((321, 78)),
        "the provider-reported usage must reach subscribers as ModelUsed"
    );
}

#[tokio::test]
async fn actor_cancels_hanging_model_cleanly() {
    let handle = spawn_with(
        Arc::new(HangingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    // Give the model round time to start and block inside the model call.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let acknowledgement = handle.cancel_turn().await.unwrap();

    let (ack_turn_id, ack_generation) = match acknowledgement {
        TurnCancelAck::Cancelled {
            turn_id,
            effective_generation,
            ..
        } => (turn_id, effective_generation),
        TurnCancelAck::NoActiveTurn => panic!("the hanging turn must still be active"),
    };
    let mut cancelled_event = None;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TurnCancelled {
                    turn_id,
                    effective_generation,
                    reason,
                    ..
                } => cancelled_event = Some((turn_id, effective_generation, reason)),
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if cancelled_event.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        cancelled_event,
        Some((
            ack_turn_id,
            ack_generation,
            TurnCancellationReason::Requested
        )),
        "the durable event and caller acknowledgement must describe the same cancellation"
    );
    assert!(
        !turn_completed,
        "cancellation must never masquerade as a successful turn commit"
    );
}

#[tokio::test]
async fn turn_frame_is_execution_stack_not_long_term_memory() {
    let context = Arc::new(RecordingContextEngine::default());
    let model = Arc::new(TwoRoundToolModel::default());
    let handle = spawn_with(
        model.clone() as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(OkToolDispatcher),
    )
    .await;
    handle.user_message("go".into()).await.unwrap();

    // Wait for the turn to persist its observations (focus + user + tool +
    // assistant).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let count = context.ingests.lock().await.len();
        if count >= 4 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not persist its observations in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // First model round: policy + user only, no tool frame yet.
    let requests = model.requests.lock().await;
    assert_eq!(requests.len(), 2, "two model rounds expected");
    let first = &requests[0];
    assert_eq!(
        first.iter().map(|message| message.role).collect::<Vec<_>>(),
        vec![ModelRole::System, ModelRole::User]
    );

    // Second round: the tool call and its result appear as protocol-paired
    // turn-frame messages with a matching tool_call_id.
    let second = &requests[1];
    let assistant = second
        .iter()
        .find(|message| message.role == ModelRole::Assistant && !message.tool_calls.is_empty())
        .expect("assistant tool-call message");
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(assistant.tool_calls[0].id, "call-1");
    let tool = second
        .iter()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool result message");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(tool.content, "ok from fs.read");
    drop(requests);

    // The observation reached the context engine only after the turn ended,
    // in ingest order: the implicit task's focus (established before the
    // message), then the user message, then the mid-turn working-set signal
    // from the tool commit (the tool's discovered entities become hot for
    // the next round), then the persisted tool observation, then the final
    // assistant message.
    let ingests = context.ingests.lock().await;
    assert_eq!(
        ingests.as_slice(),
        &[
            "FocusChanged",
            "UserMessage",
            "WorkingSetSignal",
            "ToolObservation",
            "AssistantMessage"
        ]
    );
    drop(ingests);

    // AfterTool maintenance runs once at turn end (after both model rounds),
    // so the whole turn's observations are observed together.
    let maintains = context.maintains.lock().await;
    let after_tool = maintains
        .iter()
        .position(|trigger| *trigger == ContextMaintenanceTrigger::AfterTool)
        .expect("AfterTool maintenance must run when the turn is persisted");
    assert!(
        after_tool >= 2,
        "AfterTool must run after the model rounds, got index {after_tool}"
    );
    let after_model = maintains
        .iter()
        .position(|trigger| *trigger == ContextMaintenanceTrigger::AfterModel)
        .expect("AfterModel maintenance must run at the end");
    assert!(
        after_model > after_tool,
        "AfterModel must run after AfterTool"
    );
}

/// Records the scope lifecycle the actor drives and the scope ids carried
/// by persisted tool observations, so the test can assert that tool scopes
/// are execution frames — opened at tool start, closed when the model
/// consumes the result, and that the observations stay tagged with them.
#[derive(Debug, Default)]
struct ScopeRecordingEngine {
    opens: Arc<Mutex<Vec<(ScopeKind, ScopeId)>>>,
    closes: Arc<Mutex<Vec<ScopeId>>>,
    observation_scopes: Arc<Mutex<Vec<Option<ScopeId>>>>,
}

#[async_trait::async_trait]
impl ContextEngine for ScopeRecordingEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if let ContextIngress::ToolObservation { scope_id, .. } = ingress {
            self.observation_scopes.lock().await.push(scope_id);
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
            items: Vec::new(),
            external: agent_contracts::ContextMapView::default(),
            selected: Vec::new(),
            approx_tokens: 0,
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn open_scope(&self, kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        let id = ScopeId::new();
        self.opens.lock().await.push((kind, id));
        Ok(id)
    }
    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        self.closes.lock().await.push(scope_id);
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
async fn tool_scope_opens_at_tool_start_and_closes_when_consumed() {
    let context = Arc::new(ScopeRecordingEngine::default());
    let model = Arc::new(TwoRoundToolModel::default());
    let handle = spawn_with(
        model.clone() as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();

    handle.user_message("go".into()).await.unwrap();

    // Wait for the turn to complete (two model rounds, one tool call).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let mut done = false;
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                done = true;
            }
        }
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Exactly one tool frame: opened at tool start, closed once the second
    // model round consumed the result.
    let opens = context.opens.lock().await;
    assert_eq!(opens.len(), 1, "one tool call -> one tool scope");
    assert_eq!(opens[0].0, ScopeKind::Tool);
    let tool_scope = opens[0].1;
    drop(opens);

    let closes = context.closes.lock().await;
    assert_eq!(
        closes.as_slice(),
        &[tool_scope],
        "the consumed tool scope must close with its own id"
    );
    drop(closes);

    // The persisted observation is tagged with the tool frame even though
    // persistence happens at turn end, after the frame closed.
    let observation_scopes = context.observation_scopes.lock().await;
    assert_eq!(
        observation_scopes.as_slice(),
        &[Some(tool_scope)],
        "the tool observation must carry its producing scope"
    );
}

#[tokio::test]
async fn tool_operation_identity_is_published_after_core_admission_before_tool_start() {
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut accepted = None;
    let mut accepted_position = None;
    let mut started_position = None;
    let mut position = 0_usize;
    while tokio::time::Instant::now() < deadline {
        if let Ok(envelope) = tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
            position += 1;
            match envelope.unwrap().event {
                RuntimeEvent::OperationAccepted { snapshot } => {
                    assert!(matches!(
                        snapshot.state,
                        agent_contracts::OperationState::Accepted
                    ));
                    accepted_position = Some(position);
                    accepted = Some(snapshot);
                }
                RuntimeEvent::ToolStarted { .. } => started_position = Some(position),
                RuntimeEvent::TurnCompleted => break,
                _ => {}
            }
        }
    }
    let snapshot = accepted.expect("tool operation must publish its WAL-backed identity");
    assert_eq!(snapshot.identity.call_id, "call-1");
    assert_eq!(snapshot.identity.tool_name, "fs.read");
    assert!(snapshot.identity.scope_id.is_some());
    assert!(
        accepted_position < started_position,
        "OperationAccepted must precede ToolStarted"
    );
    let queried = handle
        .query_operation(snapshot.identity.operation_id)
        .await
        .unwrap();
    assert!(matches!(
        queried,
        agent_contracts::OperationQueryResult::Found { snapshot: retained }
            if retained.identity == snapshot.identity
    ));
}

#[derive(Debug, Default)]
struct FailOperationAcceptedJournal {
    accepted: std::sync::Mutex<Option<agent_contracts::OperationSnapshot>>,
    persisted_sequences: std::sync::Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl EventJournal for FailOperationAcceptedJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if let RuntimeEvent::OperationAccepted { snapshot } = &envelope.event {
            *self.accepted.lock().unwrap() = Some((**snapshot).clone());
            return Err(AgentError::Storage(
                "simulated operation-accepted journal failure".into(),
            ));
        }
        self.persisted_sequences.lock().unwrap().push(envelope.seq);
        Ok(())
    }
}

#[tokio::test]
async fn operation_accepted_audit_failure_closes_scope_without_dispatch() {
    let context = Arc::new(ScopeRecordingEngine::default());
    let executions = Arc::new(AtomicUsize::new(0));
    let journal = Arc::new(FailOperationAcceptedJournal::default());
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(CountingToolDispatcher {
            executions: executions.clone(),
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal.clone()),
    ));
    let (handle, _task) = spawn_runtime(services);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_failure = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(
                envelope.event,
                RuntimeEvent::Error { ref message }
                    if message.contains("simulated operation-accepted journal failure")
            ) {
                saw_failure = true;
            }
        }
        if saw_failure && !context.closes.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        saw_failure,
        "the failed discovery event must remain observable"
    );
    let opens = context.opens.lock().await;
    assert_eq!(opens.len(), 1, "one attempted tool must open one scope");
    let tool_scope = opens[0].1;
    drop(opens);
    assert_eq!(
        context.closes.lock().await.as_slice(),
        &[tool_scope],
        "the admitted-but-undispatched tool scope must be closed"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "dropping the one-shot permit must prevent tool dispatch"
    );

    let accepted = journal
        .accepted
        .lock()
        .unwrap()
        .clone()
        .expect("the failing journal must observe the admitted snapshot");
    let query = handle
        .query_operation(accepted.identity.operation_id)
        .await
        .unwrap();
    assert!(matches!(
        query,
        agent_contracts::OperationQueryResult::Found { snapshot }
            if matches!(
                snapshot.state,
                agent_contracts::OperationState::Terminal {
                    terminal: agent_contracts::OperationTerminal::CancelledBeforeCommit,
                    ..
                }
            )
    ));
    assert!(matches!(
        handle.user_message("must remain fenced".into()).await,
        Err(AgentError::RecoveryRequired(_))
    ));
    let persisted = journal.persisted_sequences.lock().unwrap();
    assert_eq!(
        persisted.as_slice(),
        &(1..=u64::try_from(persisted.len()).unwrap()).collect::<Vec<_>>(),
        "a rejected event append must not leave a durable sequence gap"
    );
}

/// A context engine that records the scopes the actor closes and returns a
/// fixed promotion transition from `close_scope`, so the test can assert
/// that the runtime publishes the close as an auditable event instead of
/// discarding it.
#[derive(Debug, Default)]
struct PublishingScopeEngine {
    closes: Arc<Mutex<Vec<ScopeId>>>,
}

#[async_trait::async_trait]
impl ContextEngine for PublishingScopeEngine {
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
    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        self.closes.lock().await.push(scope_id);
        Ok(vec![ContextStateTransition {
            item_id: ContextItemId::new(),
            kind: ContextKind::Note,
            scope: ContextScope::Turn,
            from: AttentionState::Archived,
            to: AttentionState::Active,
            turn: 0,
            reason: "promoted by tool scope close".into(),
        }])
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

/// A context engine whose `close_scope` always fails, so the test can assert
/// that a failed tool-frame close is surfaced as an `Error` event instead of
/// being swallowed by `let _ =`.
#[derive(Debug, Default)]
struct FailingCloseScopeEngine;

#[async_trait::async_trait]
impl ContextEngine for FailingCloseScopeEngine {
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
        Err(AgentError::Context("simulated close failure".into()))
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

/// A context engine that never completes tool-scope closure. Cancellation
/// must bound this untrusted/replaceable engine call instead of holding the
/// actor and its cancellation acknowledgement forever.
#[derive(Debug, Default)]
struct HangingCloseScopeEngine;

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

/// Wait for the turn to complete (two model rounds, one tool call),
/// collecting every event seen on the way so the caller can assert on
/// events that precede `TurnCompleted` (a broadcast receiver drops events
/// once they are consumed, so a separate post-completion read would miss
/// them).
async fn wait_for_turn_completion(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> Vec<RuntimeEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let mut done = false;
        while let Ok(envelope) = events.try_recv() {
            done |= matches!(envelope.event, RuntimeEvent::TurnCompleted);
            seen.push(envelope.event);
        }
        if done {
            return seen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The tool-frame close is published as an auditable result: the runtime
/// emits `ToolScopeClosed` with the transitions the close produced instead
/// of discarding them.
#[tokio::test]
async fn tool_scope_close_publishes_its_transitions() {
    let context = Arc::new(PublishingScopeEngine::default());
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()) as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    let seen = wait_for_turn_completion(&mut events).await;

    let (scope_id, transitions) = seen
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolScopeClosed {
                scope_id,
                transitions,
            } => Some((*scope_id, transitions)),
            _ => None,
        })
        .expect("a ToolScopeClosed event must be published");
    assert!(
        !transitions.is_empty(),
        "the transitions produced by the close must ride the event"
    );
    let closes = context.closes.lock().await;
    assert_eq!(
        closes.as_slice(),
        &[scope_id],
        "the closed scope id must match the scope the engine actually closed"
    );
}

/// A failed tool-frame close is surfaced as an `Error` event instead of
/// being silently discarded.
#[tokio::test]
async fn tool_scope_close_failure_is_published_as_an_error() {
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()) as Arc<dyn ModelTransport>,
        Arc::new(FailingCloseScopeEngine),
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    let seen = wait_for_turn_completion(&mut events).await;

    let error = seen
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("a failed tool-frame close must publish an Error event");
    assert!(
        error.contains("closing tool scope"),
        "the error must name the failing close, got: {error}"
    );
}

/// Records every `ContextIngress` the actor sends, so the test can assert
/// that a tool commit signals its discovered entities *before* the turn-end
/// observation is persisted.
#[derive(Debug, Default)]
struct IngestRecordingEngine {
    ingests: Arc<Mutex<Vec<ContextIngress>>>,
}

#[async_trait::async_trait]
impl ContextEngine for IngestRecordingEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.ingests.lock().await.push(ingress);
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

/// A read-only tool whose bounded output names a discovered entity, so the
/// test can assert the runtime signals it before the next model round.
struct EntitySignalingDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for EntitySignalingDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: true,
            summary: "found it".into(),
            model_content: "discovered AuthService.rs".into(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

/// A tool commit signals the entities its output discovered to the context
/// engine — before the observation body is persisted at turn end — so the
/// very next model round can recall evidence without duplicating the tool
/// body.
#[tokio::test]
async fn tool_commit_signals_discovered_entities_before_the_next_round() {
    let context = Arc::new(IngestRecordingEngine::default());
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()) as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(EntitySignalingDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    wait_for_turn_completion(&mut events).await;

    let ingests = context.ingests.lock().await;
    let signal = ingests
        .iter()
        .find_map(|ingress| match ingress {
            ContextIngress::WorkingSetSignal { content } => Some(content.clone()),
            _ => None,
        })
        .expect("a WorkingSetSignal must be sent at tool commit");
    assert!(
        signal.contains("AuthService.rs"),
        "the tool's discovered entity must be signaled, got: {signal}"
    );
    let signal_pos = ingests
        .iter()
        .position(|ingress| matches!(ingress, ContextIngress::WorkingSetSignal { .. }))
        .expect("the signal must be present");
    let observation_pos = ingests
        .iter()
        .position(|ingress| matches!(ingress, ContextIngress::ToolObservation { .. }))
        .expect("the observation must be persisted at turn end");
    assert!(
        signal_pos < observation_pos,
        "the signal must reach the engine before the observation body is \
         persisted at turn end"
    );
}

// ---------------------------------------------------------------------------
// Context directive routing: a tool's `RuntimeDirective` is executed at
// operation-commit time — right after any staged effect, before the result
// enters the turn frame — so a "manual collect now" is actually now and a
// lease lands before the next model round, not at turn end. Tools never
// touch the engine — the runtime routes.
// ---------------------------------------------------------------------------

/// Emits one tool call (`name`) with the given arguments, then plain text.
#[derive(Debug)]
struct DirectiveModel {
    tool_name: &'static str,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for DirectiveModel {
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
                    arguments: json!({}),
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

/// Serves the context meta-tools: each returns a `RuntimeDirective` with the
/// matching `ContextAction`, exactly like the real `context.*` tools.
#[derive(Debug)]
struct DirectiveToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for DirectiveToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "context.lease".into(),
                description: "lease an item".into(),
                input_schema: json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
            },
            ToolSpec {
                name: "context.collect".into(),
                description: "run GC now".into(),
                input_schema: json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
            },
        ]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let directive = match request.call.name.as_str() {
            "context.lease" => {
                agent_contracts::RuntimeDirective::Context(agent_contracts::ContextAction::Lease {
                    item_id: agent_contracts::ContextItemId::new(),
                    turns: 3,
                })
            }
            "context.collect" => {
                agent_contracts::RuntimeDirective::Context(agent_contracts::ContextAction::Collect)
            }
            other => {
                return Err(agent_contracts::AgentError::Tool(format!(
                    "unknown tool: {other}"
                )));
            }
        };
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "directive queued".into(),
                model_content: "directive queued".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive,
        })
    }
}

#[tokio::test]
async fn actor_routes_lease_directive_into_the_context_engine() {
    let context = Arc::new(RecordingContextEngine::default());
    let handle = spawn_with(
        Arc::new(DirectiveModel {
            tool_name: "context.lease",
            rounds: AtomicUsize::new(0),
        }),
        context.clone(),
        Arc::new(DirectiveToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("lease something".into()).await.unwrap();
    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter().any(|e| e.contains("TurnCompleted")),
        "the turn must complete; saw: {seen:?}"
    );

    let ingests = context.ingests.lock().await;
    assert!(
        ingests.iter().any(|label| label == "ContextDirective"),
        "the directive must be routed as a ContextDirective ingest, got: {ingests:?}"
    );
    // The directive executes at operation-commit time: it must land BEFORE
    // the observation is persisted at turn end — "now", not "later".
    let directive_index = ingests.iter().position(|label| label == "ContextDirective");
    let observation_index = ingests.iter().position(|label| label == "ToolObservation");
    assert!(
        directive_index.is_some()
            && observation_index.is_some()
            && directive_index < observation_index,
        "the directive must be executed before the observation is persisted, got: {ingests:?}"
    );
    drop(ingests);

    // Stronger timing invariant: the directive must take effect before the
    // NEXT model round materializes, not just before turn-end persistence.
    // The model calls the tool on round 0 and finishes on round 1, so the
    // second materialization happens after the directive — prove it by
    // ordering on the shared activity log.
    let activity = context.activity.lock().await;
    let directive_index = activity
        .iter()
        .position(|entry| entry == "ContextDirective");
    let second_materialize = activity
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.as_str() == "Materialize")
        .nth(1)
        .map(|(index, _)| index);
    assert!(
        directive_index.is_some()
            && second_materialize.is_some()
            && directive_index < second_materialize,
        "a ContextAction must be effective before the next model round, got: {activity:?}"
    );
}

#[tokio::test]
async fn actor_routes_collect_directive_into_a_full_gc_pass() {
    let context = Arc::new(RecordingContextEngine::default());
    let handle = spawn_with(
        Arc::new(DirectiveModel {
            tool_name: "context.collect",
            rounds: AtomicUsize::new(0),
        }),
        context.clone(),
        Arc::new(DirectiveToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("collect now".into()).await.unwrap();
    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter().any(|e| e.contains("TurnCompleted")),
        "the turn must complete; saw: {seen:?}"
    );

    // `context.collect` bypasses ingest entirely: it is the one directive
    // the runtime executes itself (it owns the GC pass).
    let ingests = context.ingests.lock().await;
    assert!(
        !ingests.iter().any(|label| label == "ContextDirective"),
        "collect is not an ingest directive, got: {ingests:?}"
    );
    drop(ingests);
    let gcs = context.gcs.lock().await;
    assert_eq!(
        *gcs, 2,
        "the manual collect adds one GC pass on top of the regular turn-boundary pass"
    );
}

// ---------------------------------------------------------------------------
// Audit failures must be propagated, not silent: a state change must never
// outrun its journal event (CTX-09 audit-failure propagation).
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FailBeforeModelJournal;

#[async_trait::async_trait]
impl EventJournal for FailBeforeModelJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(
            envelope.event,
            RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::BeforeModel,
                ..
            }
        ) {
            return Err(AgentError::Storage(
                "simulated before-model journal failure".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FailGcEventJournal;

#[async_trait::async_trait]
impl EventJournal for FailGcEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::ContextGc { .. }) {
            return Err(AgentError::Storage(
                "simulated gc-event journal failure".into(),
            ));
        }
        Ok(())
    }
}

#[tokio::test]
async fn before_model_audit_failure_fences_the_turn() {
    // The BeforeModel maintenance state change landed, but its
    // ContextMaintained audit event did not: the turn must be fenced —
    // the model is never called and no TurnCompleted is emitted, so state
    // cannot silently outrun its journal event.
    let model = Arc::new(DirectiveModel {
        tool_name: "context.collect",
        rounds: AtomicUsize::new(0),
    });
    let rounds_ref = model.clone();
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(RecordingContextEngine::default()),
        model,
        Arc::new(DirectiveToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailBeforeModelJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();

    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
        }
        if seen.iter().any(|e| e.contains("Error")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter()
            .any(|e| e.contains("simulated before-model journal failure")),
        "the audit failure must surface as an Error event, saw: {seen:?}"
    );
    assert_eq!(
        rounds_ref.rounds.load(Ordering::SeqCst),
        0,
        "the fenced turn must never reach the model"
    );
    assert!(
        !seen.iter().any(|e| e.contains("TurnCompleted")),
        "no TurnCompleted may be emitted for a turn whose audit event failed, saw: {seen:?}"
    );
}

#[tokio::test]
async fn collect_audit_failure_is_not_silent() {
    // `context.collect` runs a full GC pass at operation-commit time; when
    // the resulting ContextGc audit event cannot be journaled, the runtime
    // must surface the failure as an Error event instead of dropping it.
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(DirectiveModel {
            tool_name: "context.collect",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(DirectiveToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailGcEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("collect now".into()).await.unwrap();

    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
        }
        if seen.iter().any(|e| e.contains("RecoveryRequired")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter()
            .any(|e| e.contains("simulated gc-event journal failure")),
        "the collect audit failure must surface as an Error event, saw: {seen:?}"
    );
    // The GC state change itself still happened (the failure is the event,
    // not the pass): the turn-boundary GC plus the manual collect.
    let gcs = context.gcs.lock().await;
    assert_eq!(*gcs, 2, "both GC passes still ran");
    // And the same journal fault at the turn-boundary GC audit is not
    // silent either: the turn commit fails and the runtime demands
    // recovery instead of claiming a commit whose audit never landed.
    assert!(
        seen.iter().any(|e| e.contains("RecoveryRequired")),
        "a failed GC audit event must fence the turn into recovery, saw: {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Task vs focus: the TaskManager keeps long-lived task identity stable, so
// re-focusing a goal resumes the same task instead of minting a new one.
// ---------------------------------------------------------------------------

async fn collect_focus_events(handle: &RuntimeHandle, goal: &str) -> (TaskId, u64) {
    let mut events = handle.subscribe();
    handle.set_focus(goal.into()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::FocusChanged { task_id, .. } = envelope.event {
                return (task_id, envelope.seq);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no FocusChanged event arrived"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn refocusing_the_same_goal_resumes_the_same_task() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;

    let (task_a, _) = collect_focus_events(&handle, "fix AuthService").await;
    let (task_b, _) = collect_focus_events(&handle, "write docs").await;
    let (task_a_again, _) = collect_focus_events(&handle, "fix AuthService").await;

    assert_ne!(task_a, task_b, "different goals are different tasks");
    assert_eq!(
        task_a, task_a_again,
        "re-focusing the same goal must resume the original task"
    );
}

#[tokio::test]
async fn suspend_then_refocus_resumes_the_same_task() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let (task_a, _) = collect_focus_events(&handle, "fix AuthService").await;

    let mut events = handle.subscribe();
    handle.suspend_task().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_cleared = false;
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::FocusCleared = envelope.event {
                saw_cleared = true;
            }
        }
        if saw_cleared {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "suspend must emit FocusCleared"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Re-focusing the same goal resumes the suspended task, not a new one.
    let (resumed, _) = collect_focus_events(&handle, "fix AuthService").await;
    assert_eq!(resumed, task_a, "suspend -> refocus must resume the task");
}

// ---------------------------------------------------------------------------
// Effect commit: a tool's computation is separate from its side-effect
// commit. The actor commits after the generation fence and rolls back a
// stale operation's prepared effect.
// ---------------------------------------------------------------------------

/// A staged effect whose commit/rollback calls are observable.
struct FlagEffect {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl agent_contracts::Effect for FlagEffect {
    fn describe(&self) -> String {
        "test effect".into()
    }
    async fn commit(self: Box<Self>) -> agent_contracts::EffectReceipt {
        self.committed.fetch_add(1, Ordering::SeqCst);
        agent_contracts::EffectReceipt::Applied {
            durability: agent_contracts::EffectDurability::Durable,
            evidence: None,
        }
    }
    async fn rollback(self: Box<Self>, _reason: &str) {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
    }
}

/// A dispatcher whose mutating tool stages a `FlagEffect` instead of
/// returning a plain value. `release` lets a test hold the execution open.
struct EffectToolDispatcher {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
    release: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl ToolDispatcher for EffectToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "stages an effect".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::WorkspaceWrite,
            output_budget: None,
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "staged".into(),
                model_content: "staged".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(FlagEffect {
                committed: self.committed.clone(),
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

#[tokio::test]
async fn committed_effect_lands_after_the_generation_fence() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: None,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TurnCompleted = envelope.event {
                assert_eq!(committed.load(Ordering::SeqCst), 1);
                assert_eq!(rolled_back.load(Ordering::SeqCst), 0);
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn stale_tool_rolls_back_its_prepared_effect() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: Some(release.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    handle.user_message("go".into()).await.unwrap();

    // Give the tool operation time to start and block inside execute.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel_turn().await.unwrap();

    // The tool finishes after the cancel: the generation fence has moved, so
    // the actor must roll the prepared effect back instead of committing it.
    release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && rolled_back.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        1,
        "stale effect must roll back"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        0,
        "stale effect must never commit"
    );
}

#[tokio::test]
async fn stop_drains_a_cancelled_tool_before_dropping_its_prepared_effect() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: Some(release.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    handle.user_message("go".into()).await.unwrap();

    // Cancellation durably fences the operation but deliberately does not
    // wait for arbitrary tool code. The following Stop must remember that
    // pending cleanup and keep consuming operation completions.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel_turn().await.unwrap();
    let stop_handle = handle.clone();
    let stop = tokio::spawn(async move { stop_handle.stop().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !stop.is_finished(),
        "Stop must wait for the cancelled tool's explicit cleanup result"
    );

    // The tool returns a PreparedEffect only after cancellation. The actor
    // must route that late completion through the stale rollback path before
    // ending the run, rather than dropping the boxed effect with the channel.
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), stop)
        .await
        .expect("Stop must finish once the cancelled tool returns")
        .expect("the actor task must not panic")
        .expect("shutdown cleanup must succeed");
    assert_eq!(rolled_back.load(Ordering::SeqCst), 1);
    assert_eq!(committed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_bounds_a_hanging_tool_scope_close_and_fences_mutation() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(HangingCloseScopeEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: Some(release.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let error = tokio::time::timeout(Duration::from_secs(4), handle.cancel_turn())
        .await
        .expect("a replaceable context engine cannot block cancellation forever")
        .expect_err("a timed-out scope close must not acknowledge cancellation as durable");
    assert!(matches!(error, AgentError::RecoveryRequired(_)));
    let mutation_error = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("scope cleanup uncertainty must fence later mutation");
    assert!(matches!(mutation_error, AgentError::RecoveryRequired(_)));

    let mut saw_recovery = false;
    while let Ok(envelope) = events.try_recv() {
        saw_recovery |= matches!(envelope.event, RuntimeEvent::RecoveryRequired);
    }
    assert!(
        saw_recovery,
        "the bounded cleanup failure must be observable"
    );

    // Release the still-running tool so its late PreparedEffect is explicitly
    // rolled back; Stop then has no unresolved operation to abandon.
    release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while rolled_back.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the late prepared effect was not rolled back"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(committed.load(Ordering::SeqCst), 0);
    handle.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// Capability effects: a capability stages side effects through the same
// unified `EffectRequest` channel as a builtin tool's `PreparedEffect`, and
// the actor commits or rolls them back behind the same generation fence —
// the capability computes, the core executes.
// ---------------------------------------------------------------------------

/// A capability whose one tool stages a `FlagEffect` instead of returning a
/// plain value. `release` lets a test hold the invocation open.
struct StagingCapability {
    manifest: CapabilityManifest,
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
    release: Option<Arc<tokio::sync::Notify>>,
}

impl StagingCapability {
    fn new(committed: Arc<AtomicUsize>, rolled_back: Arc<AtomicUsize>) -> Self {
        Self {
            manifest: CapabilityManifest {
                id: "staging".into(),
                version: "1.0.0".into(),
                name: "staging capability".into(),
                summary: "stages an effect".into(),
                status: CapabilityStatus::Experimental,
                provides: vec![agent_contracts::CapabilityKind::Tool],
                permissions: vec!["workspace:write".into()],
                requires: Vec::new(),
                tools: vec![ToolSpec {
                    name: "cap.stage".into(),
                    description: "stages an effect".into(),
                    input_schema: json!({"type": "object"}),
                    risk: ToolRisk::WorkspaceWrite,
                    output_budget: None,
                }],
                lifecycle: CapabilityLifecycle::Lazy,
                transport: CapabilityTransport::Builtin,
            },
            committed,
            rolled_back,
            release: None,
        }
    }
}

#[async_trait::async_trait]
impl Capability for StagingCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    async fn invoke(
        &self,
        call: ToolCall,
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(CapabilityOutcome::EffectRequest {
            output: ToolOutput {
                call_id: call.id,
                tool_name: call.name,
                ok: true,
                summary: "staged by capability".into(),
                model_content: "staged by capability".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(FlagEffect {
                committed: self.committed.clone(),
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

/// Calls the capability tool once, then replies plain.
#[derive(Debug, Default)]
struct CapabilityToolModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for CapabilityToolModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "cap-1".into(),
                    name: "cap.stage".into(),
                    arguments: json!({}),
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

/// Wire a `StagingCapability` into the actor through the capability-aware
/// dispatcher — the composition a real host performs.
async fn spawn_with_staging_capability(capability: StagingCapability) -> RuntimeHandle {
    let registry = Arc::new(CapabilityRegistry::new());
    registry
        .register(Arc::new(capability))
        .expect("capability registers");
    // Loaded tools are the model surface; without a load the actor's
    // round-surface validation would refuse the call before it executes.
    registry
        .load_tool("cap.stage")
        .expect("capability tool loads");
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(TestToolDispatcher),
        registry,
    ));
    spawn_with_approval(
        Arc::new(CapabilityToolModel::default()),
        Arc::new(TestContextEngine),
        dispatcher,
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await
}

#[tokio::test]
async fn capability_effect_requests_commit_behind_the_generation_fence() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_staging_capability(StagingCapability::new(
        committed.clone(),
        rolled_back.clone(),
    ))
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TurnCompleted = envelope.event {
                assert_eq!(
                    committed.load(Ordering::SeqCst),
                    1,
                    "the capability's staged effect must commit once"
                );
                assert_eq!(
                    rolled_back.load(Ordering::SeqCst),
                    0,
                    "a live capability effect must never roll back"
                );
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn stale_capability_effect_rolls_back() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut capability = StagingCapability::new(committed.clone(), rolled_back.clone());
    capability.release = Some(release.clone());
    let handle = spawn_with_staging_capability(capability).await;
    handle.user_message("go".into()).await.unwrap();

    // Give the capability invocation time to start and block inside invoke.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel_turn().await.unwrap();

    // The capability finishes after the cancel: the generation fence has
    // moved, so the actor must roll its staged effect back — a cancelled
    // capability never mutates the world.
    release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && rolled_back.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        1,
        "a stale capability effect must roll back"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        0,
        "a stale capability effect must never commit"
    );
}

// ---------------------------------------------------------------------------
// Commit receipt classification: `NotApplied` tells the model nothing
// happened; durability failure and `Unknown` fence later mutation because
// the world cannot safely be used as the base for more work.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum CommitResult {
    Durable,
    NotApplied,
    AppliedButDurabilityFailed,
    Unknown,
}

/// An effect that returns the selected structured receipt.
struct ReceiptEffect {
    result: CommitResult,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Effect for ReceiptEffect {
    fn describe(&self) -> String {
        "receipt test effect".into()
    }
    async fn commit(self: Box<Self>) -> EffectReceipt {
        match self.result {
            CommitResult::Durable => EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                evidence: Some("durable-test-effect".into()),
            },
            CommitResult::NotApplied => EffectReceipt::NotApplied {
                error: "simulated disk failure".into(),
            },
            CommitResult::AppliedButDurabilityFailed => EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed("simulated journal failure".into()),
                evidence: None,
            },
            CommitResult::Unknown => EffectReceipt::Unknown {
                error: "simulated remote timeout".into(),
            },
        }
    }
    async fn rollback(self: Box<Self>, _reason: &str) {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
    }
}

/// A dispatcher whose mutating tool stages a `ReceiptEffect`.
struct ReceiptEffectDispatcher {
    result: CommitResult,
    rolled_back: Arc<AtomicUsize>,
    execute_count: Option<Arc<AtomicUsize>>,
}

#[async_trait::async_trait]
impl ToolDispatcher for ReceiptEffectDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "stages a failing effect".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if let Some(count) = &self.execute_count {
            count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "staged".into(),
                model_content: "staged".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(ReceiptEffect {
                result: self.result,
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

/// Wait for the tool's finished output (the model-visible result) and the
/// turn completion, returning the handle and observed recovery signal.
async fn run_effect_receipt_turn(
    result: CommitResult,
) -> (RuntimeHandle, ToolOutput, Vec<String>, bool) {
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(ReceiptEffectDispatcher {
            result,
            rolled_back: rolled_back.clone(),
            execute_count: None,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut finished = None;
    let mut warnings = Vec::new();
    let mut recovery_required = false;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolFinished { output } => finished = Some(output),
                RuntimeEvent::Warning { message } => warnings.push(message),
                RuntimeEvent::RecoveryRequired => recovery_required = true,
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && finished.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "the turn must complete");
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        0,
        "a current-generation commit receipt must not enter the stale rollback path"
    );
    (
        handle,
        finished.expect("the tool must finish"),
        warnings,
        recovery_required,
    )
}

async fn assert_normal_mutation_is_fenced(handle: &RuntimeHandle) {
    let next_message = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("an uncertain effect result must fence the next user turn");
    assert!(
        matches!(next_message, AgentError::RecoveryRequired(_)),
        "the next user message must require recovery: {next_message}"
    );

    let next_task_mutation = handle
        .set_focus("must also wait for recovery".into())
        .await
        .expect_err("an uncertain effect result must fence task mutation");
    assert!(
        matches!(next_task_mutation, AgentError::RecoveryRequired(_)),
        "task mutation must require recovery: {next_task_mutation}"
    );
}

#[tokio::test]
async fn not_applied_commit_failure_reports_nothing_happened() {
    let (handle, finished, _, recovery_required) =
        run_effect_receipt_turn(CommitResult::NotApplied).await;
    assert!(
        !finished.ok,
        "the failed effect must surface as a failed result"
    );
    assert!(
        finished.model_content.contains("could not be committed"),
        "the model must be told nothing happened, got: {}",
        finished.model_content
    );
    assert!(
        !recovery_required,
        "a definite NotApplied receipt must not poison the runtime"
    );
    handle
        .set_focus("ordinary work may continue".into())
        .await
        .expect("NotApplied leaves a safe base for task mutation");
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn applied_but_durability_failure_surfaces_a_recovery_state() {
    let (handle, finished, warnings, recovery_required) =
        run_effect_receipt_turn(CommitResult::AppliedButDurabilityFailed).await;
    assert!(
        !finished.ok,
        "the durability failure must surface as a failed result"
    );
    assert!(
        finished.model_content.contains("WAS applied"),
        "the model must be told the change landed but the record failed, got: {}",
        finished.model_content
    );
    assert!(
        warnings
            .iter()
            .any(|message| message.contains("applied but recovery is required")),
        "the runtime must surface a degraded/recovery warning, got: {warnings:?}"
    );
    assert!(
        recovery_required,
        "the runtime must publish its recovery-required state"
    );
    assert_normal_mutation_is_fenced(&handle).await;
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn unknown_effect_state_surfaces_truth_and_fences_later_mutation() {
    let (handle, finished, warnings, recovery_required) =
        run_effect_receipt_turn(CommitResult::Unknown).await;
    assert!(!finished.ok, "an unknown applied state is not success");
    assert!(
        finished
            .model_content
            .contains("may or may not have been applied"),
        "the model must receive the uncertain world state: {}",
        finished.model_content
    );
    assert!(
        warnings
            .iter()
            .any(|message| message.contains("effect applied state unknown")),
        "the runtime must surface an unknown-state warning, got: {warnings:?}"
    );
    assert!(recovery_required, "unknown state must demand recovery");
    assert_normal_mutation_is_fenced(&handle).await;
    handle.stop().await.unwrap();
}

#[derive(Debug, Default)]
struct RetryAfterUnknownModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for RetryAfterUnknownModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        let tool_calls = match round {
            0 => vec![ToolCall {
                id: "uncertain".into(),
                name: "fs.read".into(),
                arguments: json!({"path": "x"}),
            }],
            1 => vec![ToolCall {
                id: "must-be-refused".into(),
                name: "fs.read".into(),
                arguments: json!({"path": "y"}),
            }],
            _ => Vec::new(),
        };
        Ok(ModelOutput {
            content: if tool_calls.is_empty() {
                "stopped after recovery refusal".into()
            } else {
                String::new()
            },
            tool_calls,
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn recovery_state_refuses_another_tool_in_the_same_turn() {
    let execute_count = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_approval(
        Arc::new(RetryAfterUnknownModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(ReceiptEffectDispatcher {
            result: CommitResult::Unknown,
            rolled_back: Arc::new(AtomicUsize::new(0)),
            execute_count: Some(execute_count.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut refused = None;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolFinished { output } if output.call_id == "must-be-refused" => {
                    refused = Some(output)
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && refused.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let refused = refused.expect("the second call must receive a typed refusal");
    assert!(!refused.ok);
    assert_eq!(refused.metadata["executed"], false);
    assert_eq!(refused.metadata["code"], "runtime.recovery_required");
    assert_eq!(
        execute_count.load(Ordering::SeqCst),
        1,
        "only the first uncertain effect may reach the dispatcher"
    );
    assert!(completed, "the model must still be able to close the turn");
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn durable_effect_success_does_not_fence_later_mutation() {
    let (handle, finished, warnings, recovery_required) =
        run_effect_receipt_turn(CommitResult::Durable).await;
    assert!(finished.ok, "a durable commit keeps the successful output");
    assert!(warnings.is_empty(), "durable success needs no warning");
    assert!(
        !recovery_required,
        "durable success must not demand recovery"
    );
    handle
        .set_focus("ordinary work may continue".into())
        .await
        .expect("durable success leaves task mutation enabled");
    handle.stop().await.unwrap();
}

/// A context engine whose `AssistantMessage` ingest always fails: the
/// finalization commit must surface `TurnCommitFailed` + `RecoveryRequired`
/// instead of swallowing the error and clearing the turn silently.
#[derive(Debug)]
struct FailingAssistantIngestEngine;

#[async_trait::async_trait]
impl ContextEngine for FailingAssistantIngestEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::AssistantMessage { .. }) {
            return Err(agent_contracts::AgentError::Context(
                "journal backend unavailable".into(),
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

/// A plain, non-streaming model: one fixed assistant reply, no tool calls.
#[derive(Debug)]
struct PlainModel;

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

#[tokio::test]
async fn failed_turn_commit_emits_turn_commit_failed_and_recovery_required() {
    let handle = spawn_with(
        Arc::new(PlainModel),
        Arc::new(FailingAssistantIngestEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut commit_failed = None;
    let mut recovery_required = false;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TurnCommitFailed { phase, message } => {
                    commit_failed = Some((phase, message));
                }
                RuntimeEvent::RecoveryRequired => recovery_required = true,
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if commit_failed.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (phase, message) = commit_failed.expect("the failed commit must be journaled");
    assert_eq!(
        phase, "assistant_message_ingest",
        "the failing step must be named"
    );
    assert!(
        message.contains("journal backend unavailable"),
        "the journaled failure must carry the engine error: {message}"
    );
    assert!(
        recovery_required,
        "a failed turn commit must require recovery"
    );
    assert!(
        !turn_completed,
        "a turn whose commit failed must never emit TurnCompleted"
    );

    let next = handle
        .user_message("must not run before recovery".into())
        .await
        .expect_err("a failed mandatory turn commit must fence later mutation");
    assert!(
        matches!(next, agent_contracts::AgentError::RecoveryRequired(_)),
        "the runtime must require a known-good restore after a failed turn commit: {next}"
    );
}

// ---------------------------------------------------------------------------
// Resource policy: the context meta-tools are bounded by quotas in the
// engine, and a refused directive surfaces to the model as a warning — the
// LLM cannot root the whole heap (or exhaust runtime resources) through
// context.manage. Tools never touch the engine — the runtime routes the
// directive and the engine's quota answers.
// ---------------------------------------------------------------------------

/// Emits `context.hint` on the first two rounds, then a plain reply.
#[derive(Debug)]
struct HintModel {
    item_id: ContextItemId,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for HintModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round < 2 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("hint-{round}"),
                    name: "context.hint".into(),
                    arguments: json!({"item_id": self.item_id.to_string(), "keep": true}),
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

/// Serves `context.hint`: emits a `GcHint` directive with the requested
/// item id and keep flag, exactly like the real `context.manage gc_hint`.
#[derive(Debug)]
struct HintToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for HintToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "context.hint".into(),
            description: "keep an item alive across GC passes".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let item_id: ContextItemId = request
            .call
            .arguments
            .get("item_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .parse()
            .map_err(|error| AgentError::InvalidRequest(format!("bad item id: {error}")))?;
        let keep_alive = request
            .call
            .arguments
            .get("keep")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "hint queued".into(),
                model_content: "hint queued".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::Context(
                agent_contracts::ContextAction::GcHint {
                    item_id,
                    keep_alive,
                },
            ),
        })
    }
}

#[tokio::test]
async fn hint_quota_refuses_excess_meta_tool_requests() {
    // A real reference engine with a keep-alive cap of one item: the model
    // hints the same item twice, and the second hint must be refused by
    // the quota — the meta-tool cannot root the whole heap.
    let engine = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig {
            max_keep_alive_items: 1,
            ..context_simple::SimpleContextConfig::default()
        },
    ));
    engine
        .ingest(ContextIngress::UserMessage {
            content: "pin this".into(),
        })
        .await
        .unwrap();
    let summaries = engine.inspect(usize::MAX).await.unwrap();
    let item_id = summaries[0].id;

    let handle = spawn_with(
        Arc::new(HintModel {
            item_id,
            rounds: AtomicUsize::new(0),
        }),
        engine.clone(),
        Arc::new(HintToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("pin the item".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut refused = None;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::Warning { message } => {
                    if message.contains("context directive refused") {
                        refused = Some(message);
                    }
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && refused.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "the turn must complete");
    let refused = refused.expect("the second hint must be refused by the quota");
    assert!(
        refused.contains("keep_alive") && refused.contains("cap 1"),
        "the refusal must name the quota and its cap, got: {refused}"
    );
}

// ---------------------------------------------------------------------------
// Commit-time authority lease (ACI v2 §6): a side-effecting call that
// overruns its lease window is rolled back at commit time and reported as
// a failed tool result — the world must not change after the
// authorization expired, even though the tool computation finished.
// ---------------------------------------------------------------------------

/// A staged write that records whether it was committed or rolled back.
#[derive(Debug, Default)]
struct TracingWriteEffect {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Effect for TracingWriteEffect {
    fn describe(&self) -> String {
        "tracing write".into()
    }
    async fn commit(self: Box<Self>) -> EffectReceipt {
        self.committed.fetch_add(1, Ordering::SeqCst);
        EffectReceipt::Applied {
            durability: EffectDurability::Durable,
            evidence: Some("tx-1".into()),
        }
    }
    async fn rollback(self: Box<Self>, _reason: &str) {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
    }
}

/// One `fs.write` on the first round, a plain reply on the second; records
/// every message list it received.
#[derive(Debug, Default)]
struct LeaseToolModel {
    rounds: AtomicUsize,
    requests: Mutex<Vec<Vec<ModelMessage>>>,
}

#[async_trait::async_trait]
impl ModelTransport for LeaseToolModel {
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
                    name: "fs.write".into(),
                    arguments: json!({"path": "src/main.rs", "content": "fn main() {}"}),
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

/// Serves `fs.write` by sleeping far past the lease window, then staging
/// its write effect — the commit arrives after the authorization expired.
#[derive(Debug)]
struct SlowWriteDispatcher {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolDispatcher for SlowWriteDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.write".into(),
            description: "write a file".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        // Overrun the 1ms lease window, then stage the effect.
        tokio::time::sleep(Duration::from_millis(80)).await;
        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "staged".into(),
                model_content: "staged write".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(TracingWriteEffect {
                committed: self.committed.clone(),
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

#[tokio::test]
async fn expired_authority_lease_rolls_back_the_staged_effect() {
    let model = Arc::new(LeaseToolModel::default());
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(SlowWriteDispatcher {
        committed: committed.clone(),
        rolled_back: rolled_back.clone(),
    });
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig {
            // 1ms window: any real dispatch overruns it deterministically.
            lease_ttl_ms: Some(1),
            ..CoreAuthorityConfig::default()
        },
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("write it".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut refused_output = None;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolFinished { output } => {
                    if output.tool_name == "fs.write" {
                        refused_output = Some(output);
                    }
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && refused_output.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(completed, "the turn must complete");
    let output = refused_output.expect("the write tool result must be published");
    assert!(
        !output.ok && output.model_content.contains("not applied"),
        "the overrun write must surface as a failed tool result: {output:?}"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        0,
        "the overrun effect must never commit"
    );
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        1,
        "the overrun effect must be rolled back"
    );

    // The second model round must see the failure, not a success.
    let requests = model.requests.lock().await;
    assert!(
        requests.len() >= 2,
        "the turn must have a second model round"
    );
    let serialized = serde_json::to_string(requests.last().unwrap()).unwrap();
    assert!(
        serialized.contains("not applied"),
        "the failed write must reach the model: {serialized}"
    );
}

// ---------------------------------------------------------------------------
// Structured completion: `task.complete` attaches a typed proposal that the
// runtime commits at the turn's safe point (after the turn commits) as the
// active task's CompletionRecord — the CTX-10 transaction.
// ---------------------------------------------------------------------------

/// Calls `task.complete` with the given summary on round 0, then finishes.
#[derive(Debug)]
struct CompletionProposalModel {
    summary: &'static str,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for CompletionProposalModel {
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
                    name: "task.complete".into(),
                    arguments: json!({"summary": self.summary, "artifacts": []}),
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

/// Serves `task.complete` by attaching the typed completion directive,
/// exactly like the real tool.
#[derive(Debug)]
struct CompletionToolDispatcher {
    workspace: Option<agent_workspace::Workspace>,
}

#[async_trait::async_trait]
impl ToolDispatcher for CompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "task.complete".into(),
            description: "propose completion".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let summary: String = request.call.arguments["summary"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut artifacts: Vec<String> = request.call.arguments["artifacts"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(workspace) = &self.workspace {
            artifacts.push(
                workspace
                    .write_artifact(request.run_id, "completion", "txt", b"completion evidence")
                    .await?,
            );
        }
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "completion proposed".into(),
                model_content: "completion proposed".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal { summary, artifacts },
            ),
        })
    }
}

#[tokio::test]
async fn task_complete_proposal_commits_the_typed_record_at_turn_end() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionProposalModel {
            summary: "the task is done",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(CompletionToolDispatcher {
            workspace: Some((*workspace).clone()),
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("finish the work".into()).await.unwrap();

    let mut completed_event = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskCompleted {
                task_id,
                anchor_revision,
                summary,
            } = &envelope.event
            {
                completed_event = Some((*task_id, *anchor_revision, summary.clone()));
            }
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                break;
            }
        }
        if completed_event.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (task_id, anchor_revision, summary) =
        completed_event.expect("the completion proposal must commit");
    assert_eq!(summary, "the task is done");

    // The typed record is durable in the checkpoint, with the proposal's
    // artifact ref attached — the CTX-10 transaction end to end.
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint
        .tasks
        .completed
        .iter()
        .find(|record| record.task_id == task_id)
        .expect("a completed task owns exactly one CompletionRecord");
    assert_eq!(record.anchor_revision, anchor_revision);
    assert_eq!(record.summary, "the task is done");
    assert_eq!(record.artifacts.len(), 2);
    assert!(
        record
            .artifacts
            .iter()
            .any(|reference| reference.contains("/completion/"))
    );
    assert!(
        record
            .artifacts
            .iter()
            .any(|reference| reference.contains("/assistant-response/"))
    );
    assert!(
        record.final_output_digest.is_some(),
        "the final output digest must be retained"
    );
    instance.shutdown().await.unwrap();
}
// ---------------------------------------------------------------------------

/// A model that answers with one very long plain-text message — far beyond
/// the engine's bounded ContextItem cap — so the raw-evidence artifact is
/// the only place the *complete* final response survives.
#[derive(Debug)]
struct LongResponseModel(usize);

#[async_trait::async_trait]
impl ModelTransport for LongResponseModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "x".repeat(self.0),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        sink.on_chunk(ModelChunk::Done).await?;
        self.complete(request).await
    }
}

/// Raw-evidence retention (CONTEXT_RUNTIME_TODO "Persist the exact final
/// response before ContextItem truncation"): with an artifact workspace
/// wired, the actor writes the *full* final assistant response to an
/// artifact before the bounded ContextItem is built, so an oversized
/// response survives intact even though the engine's copy would truncate
/// it.
#[tokio::test]
async fn final_assistant_response_is_persisted_in_full_before_contextitem_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    // Far beyond the default ContextItem cap (16,000 chars): only an
    // untruncated artifact preserves the raw output.
    let content_len = 40_000;
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(LongResponseModel(content_len)),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace.clone());
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    handle
        .user_message("write the report".into())
        .await
        .unwrap();

    // The file is created before it is populated, so path existence is not
    // a publication barrier. `TurnCompleted` is emitted only after the
    // pinned artifact handle has been fully written and flushed.
    tokio::time::timeout(Duration::from_secs(5), async {
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
    .expect("the turn must complete before reading its raw evidence");

    // Read the single published assistant-response artifact back.
    // user-input bodies also live under artifacts/; this assertion is about
    // the final assistant response only.
    let artifacts_dir = workspace.state_dir().join("artifacts");
    let artifacts = collect_owner_files(&artifacts_dir, "assistant-response");
    assert_eq!(
        artifacts.len(),
        1,
        "exactly one assistant-response artifact per final response, got {artifacts:?}"
    );
    let content = std::fs::read_to_string(&artifacts[0]).unwrap();
    assert_eq!(
        content.len(),
        content_len,
        "the artifact must carry the complete untruncated response"
    );
}

fn collect_txt_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(collect_txt_files(&path));
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !name.starts_with('.'))
            {
                out.push(path);
            }
        }
    }
    out
}

fn collect_owner_files(dir: &std::path::Path, owner: &str) -> Vec<std::path::PathBuf> {
    collect_txt_files(dir)
        .into_iter()
        .filter(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .contains(&format!("/{owner}/"))
        })
        .collect()
}

/// Round 0 proposes `task.complete`; round 1 answers with one very long
/// plain-text response — so the raw-evidence artifact of the *final*
/// response is written, and the CompletionRecord must attach its ref even
/// though the model declared no artifacts.
#[derive(Debug)]
struct CompletingLongModel {
    rounds: AtomicUsize,
    content_len: usize,
}

#[async_trait::async_trait]
impl ModelTransport for CompletingLongModel {
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
                    name: "task.complete".into(),
                    arguments: json!({"summary": "the task is done", "artifacts": []}),
                }],
                usage: Default::default(),
            })
        } else {
            Ok(ModelOutput {
                content: "x".repeat(self.content_len),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }
}

/// The CompletionRecord carries the raw-evidence artifact of the final
/// assistant response, independent of the model's self-declared artifact
/// list — the raw output stays reachable after the bounded ContextItem
/// truncated it.
#[tokio::test]
async fn completion_record_attaches_the_raw_final_response_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let content_len = 40_000;
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletingLongModel {
            rounds: AtomicUsize::new(0),
            content_len,
        }),
        Arc::new(CompletionToolDispatcher { workspace: None }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace.clone());
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    handle.user_message("finish the work".into()).await.unwrap();

    let mut task_id = None;
    let mut events = handle.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskCompleted {
                task_id: completed_task,
                ..
            } = envelope.event
            {
                task_id = Some(completed_task);
            }
        }
        if task_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let task_id = task_id.expect("the completion proposal must commit");

    // The CompletionRecord carries exactly one raw-evidence ref, naming the
    // assistant-response artifact.
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint
        .tasks
        .completed
        .iter()
        .find(|record| record.task_id == task_id)
        .expect("a completed task owns exactly one CompletionRecord");
    let raw_refs: Vec<&String> = record
        .artifacts
        .iter()
        .filter(|reference| reference.contains("assistant-response"))
        .collect();
    assert_eq!(
        raw_refs.len(),
        1,
        "the CompletionRecord must attach the raw final-response artifact: {:?}",
        record.artifacts
    );

    // The artifact exists and carries the complete untruncated response.
    let artifacts_dir = workspace.state_dir().join("artifacts");
    let files = collect_owner_files(&artifacts_dir, "assistant-response");
    assert_eq!(
        files.len(),
        1,
        "one assistant-response artifact per final response"
    );
    let content = std::fs::read_to_string(&files[0]).unwrap();
    assert_eq!(
        content.len(),
        content_len,
        "the raw response must be intact"
    );
    assert!(
        files[0]
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("assistant-response")
            || raw_refs[0].contains("assistant-response"),
        "the attached ref must name the assistant-response artifact"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_proposal_cannot_attach_a_cross_run_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let foreign_ref = workspace
        .write_artifact(RunId::new(), "foreign", "txt", b"foreign evidence")
        .await
        .unwrap();
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionProposalModel {
            summary: "must not commit",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(FixedCompletionToolDispatcher {
            artifact: foreign_ref,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    handle.user_message("finish".into()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(
        checkpoint.tasks.completed.is_empty(),
        "a foreign-run evidence ref must not enter a CompletionRecord"
    );
    instance.shutdown().await.unwrap();
}

#[derive(Debug)]
struct FixedCompletionToolDispatcher {
    artifact: String,
}

#[async_trait::async_trait]
impl ToolDispatcher for FixedCompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        CompletionToolDispatcher { workspace: None }.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let summary = request.call.arguments["summary"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "completion proposed".into(),
                model_content: "completion proposed".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary,
                    artifacts: vec![self.artifact.clone()],
                },
            ),
        })
    }
}

#[derive(Debug)]
struct BulkCompletionToolDispatcher {
    workspace: agent_workspace::Workspace,
    unique_artifacts: usize,
    duplicate_first: bool,
}

#[async_trait::async_trait]
impl ToolDispatcher for BulkCompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        CompletionToolDispatcher { workspace: None }.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let mut artifacts = Vec::new();
        for index in 0..self.unique_artifacts {
            artifacts.push(
                self.workspace
                    .write_artifact(
                        request.run_id,
                        &format!("proposal-{index:02}"),
                        "txt",
                        format!("evidence {index}").as_bytes(),
                    )
                    .await?,
            );
        }
        if self.duplicate_first && !artifacts.is_empty() {
            artifacts.insert(1, artifacts[0].clone());
        }
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "completion proposed".into(),
                model_content: "completion proposed".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary: "complete with evidence".into(),
                    artifacts,
                },
            ),
        })
    }
}

#[derive(Debug)]
struct DirectoryCompletionToolDispatcher {
    workspace: agent_workspace::Workspace,
}

#[async_trait::async_trait]
impl ToolDispatcher for DirectoryCompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        CompletionToolDispatcher { workspace: None }.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        // Materialize the run directory, then try to smuggle that directory
        // into the proposal as though it were an artifact file.
        self.workspace
            .write_artifact(request.run_id, "seed", "txt", b"seed")
            .await?;
        let directory = format!("artifact://.focus-agent/artifacts/{}", request.run_id);
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "completion proposed".into(),
                model_content: "completion proposed".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary: "must not commit".into(),
                    artifacts: vec![directory],
                },
            ),
        })
    }
}

async fn wait_for_completed_record(
    instance: &RuntimeInstance,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> agent_runtime::checkpoint::RuntimeCheckpoint {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })
        {
            return instance.checkpoint().await.unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "completion did not commit before deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn completion_artifacts_keep_raw_evidence_first_and_cap_the_merged_set() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionProposalModel {
            summary: "ignored by dispatcher",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(BulkCompletionToolDispatcher {
            workspace: (*workspace).clone(),
            unique_artifacts: agent_contracts::MAX_COMPLETION_ARTIFACTS,
            duplicate_first: false,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let mut events = instance.handle().subscribe();
    instance.start().await.unwrap();
    instance
        .handle()
        .user_message("finish with many artifacts".into())
        .await
        .unwrap();

    let checkpoint = wait_for_completed_record(&instance, &mut events).await;
    let artifacts = &checkpoint.tasks.completed[0].artifacts;
    assert_eq!(artifacts.len(), agent_contracts::MAX_COMPLETION_ARTIFACTS);
    assert!(artifacts[0].contains("assistant-response"));
    assert!(artifacts[1].contains("proposal-00"));
    assert!(artifacts.iter().any(|item| item.contains("proposal-30")));
    assert!(!artifacts.iter().any(|item| item.contains("proposal-31")));
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_artifacts_are_normalized_and_stably_deduplicated() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionProposalModel {
            summary: "ignored by dispatcher",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(BulkCompletionToolDispatcher {
            workspace: (*workspace).clone(),
            unique_artifacts: 1,
            duplicate_first: true,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let mut events = instance.handle().subscribe();
    instance.start().await.unwrap();
    instance
        .handle()
        .user_message("finish with duplicate artifacts".into())
        .await
        .unwrap();

    let checkpoint = wait_for_completed_record(&instance, &mut events).await;
    let artifacts = &checkpoint.tasks.completed[0].artifacts;
    assert_eq!(artifacts.len(), 2, "raw evidence plus one unique proposal");
    assert!(artifacts[0].contains("assistant-response"));
    assert!(artifacts[1].contains("proposal-00"));
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_safe_point_rejects_a_current_run_directory_reference() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionProposalModel {
            summary: "must not commit",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(DirectoryCompletionToolDispatcher {
            workspace: (*workspace).clone(),
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    instance
        .handle()
        .user_message("finish with a directory".into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        instance
            .checkpoint()
            .await
            .unwrap()
            .tasks
            .completed
            .is_empty()
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn focus_switch_clears_previous_tasks_raw_assistant_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(PlainModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
    handle.user_message("task A work".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
    }

    handle.set_focus("task B".into()).await.unwrap();
    handle
        .complete_current_task("task B complete".into())
        .await
        .unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint.tasks.completed.last().unwrap();
    assert_eq!(record.summary, "task B complete");
    assert!(
        record.artifacts.is_empty(),
        "task B must not inherit task A's raw assistant artifact: {:?}",
        record.artifacts
    );
    instance.shutdown().await.unwrap();
}
