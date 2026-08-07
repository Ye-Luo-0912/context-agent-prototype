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
    AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, MaterializedContext,
    ModelCapabilities, ModelChunk, ModelEventSink, ModelMessage, ModelOutput, ModelRequest,
    ModelRole, ModelTransport, RuntimeEvent, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolOutput, ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeHandle, spawn_runtime};
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
            focus: None,
            items: Vec::new(),
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

#[derive(Debug)]
struct TestToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for TestToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
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

/// Records every ingest and maintain call the runtime makes, so the test can
/// assert *when* observations reached the long-term context.
#[derive(Debug, Default)]
struct RecordingContextEngine {
    ingests: Arc<Mutex<Vec<String>>>,
    maintains: Arc<Mutex<Vec<ContextMaintenanceTrigger>>>,
}

#[async_trait::async_trait]
impl ContextEngine for RecordingContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let label = match &ingress {
            ContextIngress::UserMessage { .. } => "UserMessage",
            ContextIngress::AssistantMessage { .. } => "AssistantMessage",
            ContextIngress::ToolObservation { .. } => "ToolObservation",
            ContextIngress::FocusChanged { .. } => "FocusChanged",
            ContextIngress::Pin { .. } => "Pin",
            ContextIngress::TaskCompleted { .. } => "TaskCompleted",
        };
        self.ingests.lock().await.push(label.to_string());
        Ok(())
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.maintains.lock().await.push(trigger);
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            focus: None,
            items: Vec::new(),
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
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: true,
            summary: "ok".into(),
            model_content: "ok from fs.read".into(),
            artifact_ref: None,
            metadata: json!({}),
        })
    }
}

async fn spawn_with(
    model: Arc<dyn ModelTransport>,
    context: Arc<dyn ContextEngine>,
    tools: Arc<dyn ToolDispatcher>,
) -> RuntimeHandle {
    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        context,
        model,
        tools,
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    handle
}

#[tokio::test]
async fn actor_streams_model_deltas_to_subscribers() {
    let handle = spawn_with(
        Arc::new(StreamingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut deltas = Vec::new();
    let mut final_content = None;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ModelDelta { delta } => deltas.push(delta),
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
    assert_eq!(final_content.as_deref(), Some("Hello world"));
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
    handle.cancel_turn().await;

    let mut saw_cancel_warning = false;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::Warning { message } if message == "turn cancelled" => {
                    saw_cancel_warning = true
                }
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if turn_completed && saw_cancel_warning {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_cancel_warning, "expected a turn-cancelled warning");
    assert!(turn_completed, "UI must return to idle after cancellation");
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

    // Wait for the turn to persist its observations (user + tool + assistant).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let count = context.ingests.lock().await.len();
        if count >= 3 {
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
    // in ingest order: user message, then the persisted tool observation,
    // then the final assistant message.
    let ingests = context.ingests.lock().await;
    assert_eq!(
        ingests.as_slice(),
        &["UserMessage", "ToolObservation", "AssistantMessage"]
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
