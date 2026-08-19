use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AgentResult, CancellationToken, ScopeId, ToolCall, ToolOutput, ToolResultDisposition, ToolSpec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Assistant tool calls attached to this message (role == Assistant).
    /// Part of the runtime turn frame, never part of the long-term working set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Which assistant tool call this result answers (role == Tool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::System,
            content: content.into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::User,
            content: content.into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: content.into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Assistant message that carries one or more tool calls (empty content).
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: String::new(),
            name: None,
            tool_calls: calls,
            tool_call_id: None,
        }
    }

    pub fn tool(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::Tool,
            content: content.into(),
            name: Some(name.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Tool result paired with the assistant call it answers.
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: ModelRole::Tool,
            content: content.into(),
            name: Some(name.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// One ordered step of the current turn's execution stack: either the
/// assistant's tool calls or the result that answers them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnFrameStep {
    AssistantToolCalls {
        calls: Vec<ToolCall>,
    },
    ToolResult {
        output: ToolOutput,
        /// The tool scope this result belongs to, opened by the runtime at
        /// tool start; the persisted observation is tagged with it.
        #[serde(default)]
        scope_id: Option<ScopeId>,
        /// Whether this result becomes a long-term observation at turn end.
        /// Context retrieval results are transient: they must not duplicate
        /// fetched evidence under a new item id.
        #[serde(default)]
        disposition: ToolResultDisposition,
    },
}

/// The runtime-owned execution stack of one turn: the current user message
/// followed by every assistant tool call / tool result in order.
///
/// This is not long-term memory. It is held by the runtime and never scored,
/// garbage-collected or evicted while the turn is open; when the turn ends it
/// is dropped, and the observations it carried are persisted to the context
/// engine as the long-term record of the turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnFrame {
    pub user_message: String,
    pub steps: Vec<TurnFrameStep>,
}

impl TurnFrame {
    pub fn new(user_message: impl Into<String>) -> Self {
        Self {
            user_message: user_message.into(),
            steps: Vec::new(),
        }
    }

    pub fn push_tool_calls(&mut self, calls: Vec<ToolCall>) {
        if !calls.is_empty() {
            self.steps.push(TurnFrameStep::AssistantToolCalls { calls });
        }
    }

    pub fn push_tool_result(&mut self, output: ToolOutput, scope_id: Option<ScopeId>) {
        self.push_tool_result_with(output, scope_id, ToolResultDisposition::PersistObservation);
    }

    /// Push a tool result with an explicit persist disposition: context
    /// retrieval results are `TransientNoPersist`.
    pub fn push_tool_result_with(
        &mut self,
        output: ToolOutput,
        scope_id: Option<ScopeId>,
        disposition: ToolResultDisposition,
    ) {
        self.steps.push(TurnFrameStep::ToolResult {
            output,
            scope_id,
            disposition,
        });
    }

    pub fn has_tool_steps(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, TurnFrameStep::AssistantToolCalls { .. }))
    }

    /// Render the stack as protocol messages: the user message first, then
    /// assistant(tool_calls) / tool(tool_call_id) pairs in execution order.
    pub fn messages(&self) -> Vec<ModelMessage> {
        let mut messages = vec![ModelMessage::user(self.user_message.clone())];
        for step in &self.steps {
            match step {
                TurnFrameStep::AssistantToolCalls { calls } => {
                    messages.push(ModelMessage::assistant_tool_calls(calls.clone()));
                }
                TurnFrameStep::ToolResult { output, .. } => {
                    messages.push(ModelMessage::tool_result(
                        &output.call_id,
                        &output.tool_name,
                        &output.model_content,
                    ));
                }
            }
        }
        messages
    }
}

/// The five-layer model input assembled by the runtime for one model request:
///
/// ```text
/// System Policy        - standing instructions for every request
/// Focus Frame          - the current task/goal (structured in a later phase)
/// Context Frame        - the long-term working set, from ContextEngine::materialize
/// Turn Frame           - the current turn's execution stack, owned by the runtime
/// Active Tool Schemas  - tool definitions for this request (ModelRequest.tools)
/// ```
///
/// Layers are kept separate so the context engine never has to understand the
/// execution protocol, and the runtime never has to score long-term memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInput {
    pub system_policy: Vec<ModelMessage>,
    pub focus_frame: Option<String>,
    pub context_frame: Vec<ModelMessage>,
    pub turn_frame: TurnFrame,
    pub tool_schemas: Vec<ToolSpec>,
}

impl ModelInput {
    /// Flatten the layers into the wire message sequence. Order matters for
    /// OpenAI-style protocols: policy and context first, then the turn stack
    /// (user -> assistant tool calls -> tool results), so the model sees the
    /// current execution state as the most recent protocol messages.
    pub fn into_messages(&self) -> Vec<ModelMessage> {
        let mut messages = Vec::new();
        messages.extend(self.system_policy.iter().cloned());
        if let Some(focus) = &self.focus_frame {
            messages.push(ModelMessage::system(focus));
        }
        messages.extend(self.context_frame.iter().cloned());
        messages.extend(self.turn_frame.messages());
        messages
    }
}

/// Per-request prompt-layer token accounting. Sums across `ModelStarted`
/// events tell whether C grew because of historical context or because
/// TaskProgress / Focus itself got longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PromptLayerCosts {
    pub system_tokens: u64,
    pub runtime_facts_tokens: u64,
    pub task_anchor_tokens: u64,
    pub task_progress_tokens: u64,
    pub current_focus_tokens: u64,
    pub historical_context_tokens: u64,
    pub turn_frame_tokens: u64,
    pub tool_schema_tokens: u64,
}

impl PromptLayerCosts {
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            system_tokens: self.system_tokens.saturating_add(other.system_tokens),
            runtime_facts_tokens: self
                .runtime_facts_tokens
                .saturating_add(other.runtime_facts_tokens),
            task_anchor_tokens: self
                .task_anchor_tokens
                .saturating_add(other.task_anchor_tokens),
            task_progress_tokens: self
                .task_progress_tokens
                .saturating_add(other.task_progress_tokens),
            current_focus_tokens: self
                .current_focus_tokens
                .saturating_add(other.current_focus_tokens),
            historical_context_tokens: self
                .historical_context_tokens
                .saturating_add(other.historical_context_tokens),
            turn_frame_tokens: self
                .turn_frame_tokens
                .saturating_add(other.turn_frame_tokens),
            tool_schema_tokens: self
                .tool_schema_tokens
                .saturating_add(other.tool_schema_tokens),
        }
    }
}

/// Provider capability declaration. The kernel/UI can branch on this without
/// vendor-specific knowledge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calls: bool,
    pub max_output_tokens: usize,
    /// The provider's declared context window in tokens. When absent the
    /// runtime falls back to its configured budget — the context engine only
    /// ever sees the derived context-frame share either way.
    #[serde(default)]
    pub context_window: Option<usize>,
}

/// A bounded chunk of a streaming model response, normalized by the provider
/// adapter. The kernel forwards these to live UI subscribers; the final
/// `ModelOutput` remains the source of truth for the model turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelChunk {
    TextDelta {
        delta: String,
    },
    ToolCallDelta {
        call_id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments_delta: String,
    },
    Done,
}

/// Receives streaming chunks. Implementations must be cheap: this runs on the
/// model hot path.
#[async_trait]
pub trait ModelEventSink: Send + Sync {
    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub metadata: Value,
    /// Cooperative cancellation handle for this request. Not serialized.
    #[serde(skip)]
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Transport attempts that produced this output. `0` on legacy events
    /// means unknown (treat as one successful attempt). Failed attempts
    /// usually report no usage, so recorded tokens are a lower bound when
    /// `retries > 0`.
    #[serde(default)]
    pub attempts: u32,
    /// `attempts.saturating_sub(1)` when known.
    #[serde(default)]
    pub retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOutput {
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: ModelUsage,
}

#[async_trait]
pub trait ModelTransport: Send + Sync {
    fn capabilities(&self) -> ModelCapabilities;

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput>;

    /// Stream the response into `sink` and return the final assembled output.
    ///
    /// The default implementation bridges a non-streaming `complete` into a
    /// single delta, so every transport can be used with the streaming kernel
    /// loop. Streaming-capable providers override this to emit real deltas.
    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        let output = self.complete(request).await?;
        if !output.content.is_empty() {
            sink.on_chunk(ModelChunk::TextDelta {
                delta: output.content.clone(),
            })
            .await?;
        }
        for call in &output.tool_calls {
            sink.on_chunk(ModelChunk::ToolCallDelta {
                call_id: call.id.clone(),
                name: Some(call.name.clone()),
                arguments_delta: call.arguments.to_string(),
            })
            .await?;
        }
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "fs.read".into(),
            arguments: json!({"path": "src/main.rs"}),
        }
    }

    #[test]
    fn turn_frame_renders_protocol_order() {
        let mut turn = TurnFrame::new("list the files");
        turn.push_tool_calls(vec![tool_call("call-1")]);
        turn.push_tool_result(
            ToolOutput {
                call_id: "call-1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "fn main() {}".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            None,
        );

        let messages = turn.messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, ModelRole::User);
        assert_eq!(messages[0].content, "list the files");

        assert_eq!(messages[1].role, ModelRole::Assistant);
        assert!(messages[1].content.is_empty());
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].id, "call-1");

        assert_eq!(messages[2].role, ModelRole::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(messages[2].content, "fn main() {}");
    }

    #[test]
    fn model_input_flattens_five_layers_in_order() {
        let mut turn = TurnFrame::new("continue");
        turn.push_tool_calls(vec![tool_call("c2")]);
        turn.push_tool_result(
            ToolOutput {
                call_id: "c2".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "content".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            None,
        );
        let input = ModelInput {
            system_policy: vec![ModelMessage::system("policy")],
            focus_frame: Some("goal text".into()),
            context_frame: vec![ModelMessage::user("SELECTED WORKING CONTEXT")],
            turn_frame: turn,
            tool_schemas: Vec::new(),
        };

        let messages = input.into_messages();
        assert_eq!(
            messages
                .iter()
                .map(|m| (m.role, m.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ModelRole::System, "policy"),
                (ModelRole::System, "goal text"),
                (ModelRole::User, "SELECTED WORKING CONTEXT"),
                (ModelRole::User, "continue"),
                (ModelRole::Assistant, ""),
                (ModelRole::Tool, "content"),
            ]
        );
    }

    #[test]
    fn message_serde_roundtrips_and_old_format_parses() {
        let message = ModelMessage::tool_result("call-9", "fs.read", "ok");
        let json = serde_json::to_string(&message).unwrap();
        let parsed: ModelMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_call_id.as_deref(), Some("call-9"));

        // A message serialized before tool frames existed still parses.
        // ModelRole derives PascalCase wire names (e.g. "User", not "user").
        let old = r#"{"role":"User","content":"hello"}"#;
        let parsed: ModelMessage = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.role, ModelRole::User);
        assert!(parsed.tool_calls.is_empty());
        assert!(parsed.tool_call_id.is_none());
    }
}
