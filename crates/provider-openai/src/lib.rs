//! OpenAI-compatible streaming model provider.
//!
//! Speaks the OpenAI Chat Completions streaming protocol (`data:` SSE events),
//! which is also implemented by DeepSeek, Qwen/DashScope, Moonshot/Kimi, Zhipu
//! GLM, and most other vendors. Point `OpenAiConfig::base_url` at any of them.
//!
//! The provider normalizes vendor wire chunks into `ModelChunk` events and
//! returns the final assembled `ModelOutput` (content, tool calls, usage). All
//! vendor-specific parsing lives here; the kernel only sees the contract.

mod retry;
mod sse;

pub use retry::RetryingTransport;

use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ModelCapabilities, ModelChunk, ModelEventSink, ModelOutput,
    ModelRequest, ModelRole, ModelTransport,
};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::sse::{StreamAccumulator, WireChunk, parse_sse_data};

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    /// e.g. `https://api.openai.com/v1`, `https://api.deepseek.com/v1`, ...
    pub base_url: String,
    /// e.g. `gpt-4o-mini`, `deepseek-chat`, `qwen-plus`, ...
    pub model: String,
    pub max_output_tokens: usize,
    pub timeout: Duration,
    /// Send `stream_options: { "include_usage": true }`. Some compatible
    /// providers reject the field; turn it off per provider.
    pub send_stream_options: bool,
    /// Send `max_tokens`. Some compatible providers reject it for certain
    /// models (reasoning models may want `max_completion_tokens` instead).
    pub send_max_tokens: bool,
}

/// Cap on the provider error body carried in the error string, so a huge
/// HTML error page cannot blow up the failure message.
const MAX_ERROR_BODY_CHARS: usize = 512;

fn truncate_error_body(body: &str) -> String {
    let trimmed = body.trim();
    let total = trimmed.chars().count();
    if total <= MAX_ERROR_BODY_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX_ERROR_BODY_CHARS).collect();
    format!("{head}... ({total} chars total)")
}

pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("build reqwest client");
        Self { config, client }
    }
}

#[async_trait]
impl ModelTransport for OpenAiProvider {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: self.config.max_output_tokens,
            context_window: None,
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.complete_stream(request, &NoopSink).await
    }

    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        let payload = build_wire_request(&request, &self.config);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|error| AgentError::Transport {
                retryable: true,
                message: format!("request failed: {error}"),
            })?;

        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            let body = response.text().await.unwrap_or_default();
            let retryable = status.is_server_error() || code == 429;
            return Err(AgentError::Transport {
                retryable,
                message: format!("HTTP {code}: {}", truncate_error_body(&body)),
            });
        }

        let byte_stream = response.bytes_stream().map_err(std::io::Error::other);
        let reader = StreamReader::new(byte_stream);
        let mut lines = FramedRead::new(reader, LinesCodec::new());

        let mut accumulator = StreamAccumulator::default();
        loop {
            tokio::select! {
                _ = request.cancel.cancelled() => {
                    return Err(AgentError::Cancelled);
                }
                line = lines.next() => {
                    match line {
                        Some(Ok(line)) => {
                            let Some(payload) = parse_sse_data(&line) else {
                                continue;
                            };
                            if payload == "[DONE]" {
                                break;
                            }
                            match serde_json::from_str::<WireChunk>(payload) {
                                Ok(chunk) => {
                                    for event in accumulator.apply(&chunk) {
                                        sink.on_chunk(event).await?;
                                    }
                                }
                                Err(error) => {
                                    tracing::debug!(%error, %payload, "skipping unparseable stream chunk");
                                }
                            }
                        }
                        Some(Err(error)) => {
                            return Err(AgentError::Transport {
                                retryable: true,
                                message: format!("stream error: {error}"),
                            });
                        }
                        None => break,
                    }
                }
            }
        }

        let usage = accumulator.usage.clone().unwrap_or_default();
        let (content, tool_calls) = accumulator.finalize();
        sink.on_chunk(ModelChunk::Done).await?;

        Ok(ModelOutput {
            content,
            tool_calls,
            usage,
        })
    }
}

fn build_wire_request(request: &ModelRequest, config: &OpenAiConfig) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let mut wire = json!({
                "role": role_name(message.role),
                "content": message.content,
            });
            if message.role == ModelRole::Assistant && !message.tool_calls.is_empty() {
                // OpenAI serializes arguments as a JSON string inside tool_calls.
                wire["tool_calls"] = json!(
                    message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments.to_string(),
                                }
                            })
                        })
                        .collect::<Vec<_>>()
                );
            }
            if message.role == ModelRole::Tool {
                // Tool results pair via tool_call_id; name is not part of the
                // OpenAI tool-message shape.
                if let Some(call_id) = &message.tool_call_id {
                    wire["tool_call_id"] = json!(call_id);
                }
            } else if let Some(name) = &message.name {
                wire["name"] = json!(name);
            }
            wire
        })
        .collect();

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect();

    let mut wire = json!({
        "model": config.model,
        "messages": messages,
        "tools": tools,
        "stream": true,
    });
    // Per-provider wire negotiation: not every OpenAI-compatible endpoint
    // accepts stream_options or max_tokens, so both are configurable.
    if config.send_stream_options {
        wire["stream_options"] = json!({ "include_usage": true });
    }
    if config.send_max_tokens {
        wire["max_tokens"] = json!(config.max_output_tokens);
    }
    wire
}

fn role_name(role: ModelRole) -> &'static str {
    match role {
        ModelRole::System => "system",
        ModelRole::User => "user",
        ModelRole::Assistant => "assistant",
        ModelRole::Tool => "tool",
    }
}

struct NoopSink;

#[async_trait]
impl ModelEventSink for NoopSink {
    async fn on_chunk(&self, _chunk: ModelChunk) -> AgentResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ModelMessage, ToolSpec};

    #[test]
    fn builds_wire_request() {
        let request = ModelRequest {
            messages: vec![
                ModelMessage::system("be focused"),
                ModelMessage::user("list files"),
                ModelMessage::assistant_tool_calls(vec![agent_contracts::ToolCall {
                    id: "call-1".into(),
                    name: "fs.list".into(),
                    arguments: json!({"path": ""}),
                }]),
                ModelMessage::tool_result("call-1", "fs.list", "a, b, c"),
            ],
            tools: vec![ToolSpec {
                name: "fs.list".into(),
                description: "list files".into(),
                input_schema: json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
            }],
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };
        let config = OpenAiConfig {
            api_key: "secret".into(),
            base_url: "https://example.com/v1".into(),
            model: "deepseek-chat".into(),
            max_output_tokens: 2048,
            timeout: Duration::from_secs(30),
            send_stream_options: true,
            send_max_tokens: true,
        };
        let wire = build_wire_request(&request, &config);
        assert_eq!(wire["model"], "deepseek-chat");
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["stream_options"]["include_usage"], true);
        assert_eq!(wire["messages"][0]["role"], "system");
        assert_eq!(wire["messages"][1]["role"], "user");
        assert_eq!(wire["tools"][0]["function"]["name"], "fs.list");
        assert_eq!(wire["max_tokens"], 2048);

        // Assistant tool calls serialize as function calls with string args.
        let assistant = &wire["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "fs.list");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"\"}"
        );

        // Tool results pair via tool_call_id, not name.
        let tool = &wire["messages"][3];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call-1");
        assert!(tool.get("name").is_none());
    }

    #[test]
    fn wire_negotiation_drops_provider_rejected_fields() {
        let request = ModelRequest {
            messages: vec![ModelMessage::user("hi")],
            tools: Vec::new(),
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };
        let config = OpenAiConfig {
            api_key: "secret".into(),
            base_url: "https://example.com/v1".into(),
            model: "strict-model".into(),
            max_output_tokens: 2048,
            timeout: Duration::from_secs(30),
            send_stream_options: false,
            send_max_tokens: false,
        };
        let wire = build_wire_request(&request, &config);
        assert!(
            wire.get("stream_options").is_none(),
            "a provider that rejects stream_options must not receive it"
        );
        assert!(
            wire.get("max_tokens").is_none(),
            "a provider that rejects max_tokens must not receive it"
        );
        assert_eq!(wire["stream"], true);
    }

    #[test]
    fn error_body_is_bounded() {
        let huge = "x".repeat(10_000);
        let bounded = truncate_error_body(&huge);
        assert!(
            bounded.chars().count() < 600,
            "must truncate, got {}",
            bounded.len()
        );
        assert!(bounded.contains("(10000 chars total)"));
        let small = "boom";
        assert_eq!(truncate_error_body(small), "boom");
    }
}
