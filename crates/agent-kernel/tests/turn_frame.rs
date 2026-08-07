//! Turn-frame behavior at the kernel level.
//!
//! Verifies the separation between the runtime-owned execution stack (Turn
//! Frame) and the long-term working set (Context Frame): during a turn, tool
//! results reach the model as protocol-paired messages (assistant tool calls
//! -> tool results with matching tool_call_id) and do not enter the context
//! engine until the turn ends, when they are persisted as observations.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agent_contracts::{
    AgentResult, ContextBuildRequest, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextSnapshot,
    ModelCapabilities, ModelMessage, ModelOutput, ModelRequest, ModelRole, ModelTransport,
    ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutput, ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};
use serde_json::json;
use tokio::sync::Mutex;

/// Records every ingest and maintain call the kernel makes, so the test can
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

#[tokio::test]
async fn turn_frame_is_execution_stack_not_long_term_memory() {
    let context = Arc::new(RecordingContextEngine::default());
    let model = Arc::new(TwoRoundToolModel::default());
    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        context.clone() as Arc<dyn ContextEngine>,
        model.clone() as Arc<dyn ModelTransport>,
        Arc::new(OkToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    kernel.start().await.unwrap();
    kernel.handle_user_message("go".into()).await.unwrap();

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
