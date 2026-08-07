//! Kernel-level streaming and cancellation tests.
//!
//! Uses minimal stubs for context/tools so the kernel is exercised against the
//! `ContextEngine`/`ToolDispatcher`/`ModelTransport` contracts only (the
//! kernel never depends on a concrete context implementation).

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ContextBuildRequest, ContextDiagnostics, ContextEngine,
    ContextIngress, ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextSnapshot, ModelCapabilities, ModelChunk, ModelEventSink, ModelMessage, ModelOutput,
    ModelRequest, ModelTransport, RuntimeEvent, ToolDispatcher, ToolExecutionRequest, ToolOutput,
    ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};

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
                ModelMessage::system(request.system_prompt),
                ModelMessage::user(request.current_input),
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

#[derive(Debug)]
struct TestToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for TestToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        Err(AgentError::Tool("no tools configured in test".into()))
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
                return Err(AgentError::Cancelled);
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
        Err(AgentError::Cancelled)
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

#[tokio::test]
async fn kernel_streams_model_deltas_to_subscribers() {
    let kernel = kernel(Arc::new(StreamingModel));
    let mut events = kernel.subscribe();
    kernel.start().await.unwrap();

    kernel.handle_user_message("hello".into()).await.unwrap();

    let mut deltas = Vec::new();
    let mut final_content = None;
    let mut turn_completed = false;
    while let Ok(envelope) = events.try_recv() {
        match envelope.event {
            RuntimeEvent::ModelDelta { delta } => deltas.push(delta),
            RuntimeEvent::AssistantMessage { content } => final_content = Some(content),
            RuntimeEvent::TurnCompleted => turn_completed = true,
            _ => {}
        }
    }

    assert_eq!(deltas, vec!["Hello ".to_string(), "world".to_string()]);
    assert_eq!(final_content.as_deref(), Some("Hello world"));
    assert!(turn_completed);
}

#[tokio::test]
async fn kernel_cancels_hanging_model_cleanly() {
    let kernel = kernel(Arc::new(HangingModel));
    let mut events = kernel.subscribe();
    kernel.start().await.unwrap();

    let turn = kernel.clone();
    let task = tokio::spawn(async move { turn.handle_user_message("hello".into()).await });

    // Give the turn time to start and block inside the model call.
    tokio::time::sleep(Duration::from_millis(50)).await;
    kernel.cancel_current_turn().await;

    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("turn did not stop after cancellation")
        .expect("turn task panicked");
    assert!(
        result.is_ok(),
        "cancelled turn should end cleanly, got: {result:?}"
    );

    let mut saw_cancel_warning = false;
    let mut turn_completed = false;
    while let Ok(envelope) = events.try_recv() {
        match envelope.event {
            RuntimeEvent::Warning { message } if message == "turn cancelled" => {
                saw_cancel_warning = true
            }
            RuntimeEvent::TurnCompleted => turn_completed = true,
            _ => {}
        }
    }
    assert!(saw_cancel_warning, "expected a turn-cancelled warning");
    assert!(turn_completed, "UI must return to idle after cancellation");
}
