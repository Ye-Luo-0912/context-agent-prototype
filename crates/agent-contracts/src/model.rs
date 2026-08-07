use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentResult, CancellationToken, ToolCall, ToolSpec};

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
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::System,
            content: content.into(),
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::User,
            content: content.into(),
            name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: content.into(),
            name: None,
        }
    }

    pub fn tool(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::Tool,
            content: content.into(),
            name: Some(name.into()),
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
