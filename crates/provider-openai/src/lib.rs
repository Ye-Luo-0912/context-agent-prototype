//! OpenAI-compatible streaming model provider.
//!
//! Speaks both the Responses and Chat Completions SSE protocols. `Auto` probes
//! Responses once and falls back only when the endpoint explicitly reports it
//! unsupported; the negotiated result is cached for the life of the provider.
//!
//! The provider normalizes vendor wire chunks into `ModelChunk` events and
//! returns the final assembled `ModelOutput` (content, tool calls, usage). All
//! vendor-specific parsing lives here; the kernel only sees the contract.
//!
//! OpenAI 函数名不允许 `.` / `:`。出网时由 `wire_names` 换成 `_`，回包还原成
//! Core 工具 id，避免 `fs.list` 这类内建名被上游 400。

mod responses;
mod retry;
mod sse;
mod wire_names;

pub use retry::{
    CallStage, JsonlRetryObserver, NullRetryObserver, RetryClass, RetryIncident, RetryObserver,
    RetryingTransport, StageOutcome,
};

use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::{Duration, SystemTime},
};

use agent_contracts::{
    AgentError, AgentResult, ModelCapabilities, ModelChunk, ModelEventSink, ModelOutput,
    ModelProtocolErrorKind, ModelRequest, ModelRole, ModelTransport, RetryAfterMillis,
};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::{Client, header::RETRY_AFTER};
use serde_json::{Value, json};
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};
use tokio_util::io::StreamReader;

use crate::responses::{ResponseStreamErrorKind, ResponsesAccumulator};
use crate::sse::{SseEventFramer, StreamAccumulator, parse_wire_chunk};
use crate::wire_names::ToolNameCodec;

const PROTOCOL_UNKNOWN: u8 = 0;
const PROTOCOL_CHAT: u8 = 1;
const PROTOCOL_RESPONSES: u8 = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenAiProtocol {
    ChatCompletions,
    Responses,
    #[default]
    Auto,
}

impl OpenAiProtocol {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "responses" | "response" => Ok(Self::Responses),
            "chat" | "chat_completions" | "chat-completions" => Ok(Self::ChatCompletions),
            other => Err(format!(
                "unsupported OPENAI_API_PROTOCOL '{other}'; expected auto, responses, or chat"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    /// e.g. `https://api.openai.com/v1`, `https://api.deepseek.com/v1`, ...
    pub base_url: String,
    /// e.g. `gpt-4o-mini`, `deepseek-chat`, `qwen-plus`, ...
    pub model: String,
    /// Wire endpoint selection. `Auto` prefers `/responses` and caches an
    /// explicit unsupported-endpoint fallback to `/chat/completions`.
    pub protocol: OpenAiProtocol,
    pub max_output_tokens: usize,
    pub timeout: Duration,
    /// Send `stream_options: { "include_usage": true }`. Some compatible
    /// providers reject the field; turn it off per provider.
    pub send_stream_options: bool,
    /// Send `max_tokens`. Some compatible providers reject it for certain
    /// models (reasoning models may want `max_completion_tokens` instead).
    pub send_max_tokens: bool,
    /// Total bytes accepted from one streaming response before the
    /// transport fails (`DEFAULT_MAX_STREAM_BYTES` unless overridden). A
    /// broken or malicious provider cannot make the runtime buffer an
    /// unbounded stream.
    pub max_stream_bytes: usize,
    /// Declared provider context window in tokens. `None` keeps the adapter
    /// silent; the runtime then falls back to the kernel pack budget for
    /// both send and pack. Live eval/TUI should set this so a tool-loop
    /// turn is sendable while C still packs to the kernel working-set cap.
    pub context_window: Option<usize>,
}

/// Cap on the provider error body carried in the error string, so a huge
/// HTML error page cannot blow up the failure message.
const MAX_ERROR_BODY_CHARS: usize = 512;

/// Total bytes accepted from one streaming response before the transport
/// fails: a broken or malicious provider must not make the runtime buffer
/// an unbounded model stream (the resource boundary).
pub const DEFAULT_MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// Conservative declared send window when the compatible API does not
/// advertise one through this adapter. Not a claim about any specific
/// model's real limit; override with `OPENAI_CONTEXT_WINDOW`.
pub const DEFAULT_DECLARED_CONTEXT_WINDOW: usize = 128_000;

/// 5xx and 429 are retryable. A genuine 400 (bad schema, illegal tool
/// name, context overflow) is not. Some compatible gateways wrap a
/// transient upstream failure as HTTP 400 `invalid_request_error` with
/// message `Upstream request failed`; that body is retryable, because the
/// same payload later succeeds. Do not treat every 400 as retryable.
fn http_status_retryable(code: u16, body: &str) -> bool {
    if (500..600).contains(&code) || code == 429 {
        return true;
    }
    code == 400 && gateway_wrapped_upstream_failure(body)
}

fn gateway_wrapped_upstream_failure(body: &str) -> bool {
    body.to_ascii_lowercase()
        .contains("upstream request failed")
}

fn responses_endpoint_unsupported(code: u16, body: &str) -> bool {
    if matches!(code, 404 | 405 | 501) {
        return true;
    }
    let body = body.to_ascii_lowercase();
    body.contains("unsupported_protocol")
        || body.contains("responses endpoint is not supported")
        || body.contains("responses api is not supported")
}

fn truncate_error_body(body: &str) -> String {
    let trimmed = body.trim();
    let total = trimmed.chars().count();
    if total <= MAX_ERROR_BODY_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX_ERROR_BODY_CHARS).collect();
    format!("{head}... ({total} chars total)")
}

fn parse_retry_after(
    value: Option<&reqwest::header::HeaderValue>,
    now: SystemTime,
) -> Option<RetryAfterMillis> {
    let value = value?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(RetryAfterMillis::new_saturating(
            seconds.saturating_mul(1_000),
        ));
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    let millis = deadline
        .duration_since(now)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    Some(RetryAfterMillis::new_saturating(millis))
}

fn parse_responses_event(payload: &str) -> AgentResult<Value> {
    let event: Value =
        serde_json::from_str(payload).map_err(|error| AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedEvent,
            message: format!("malformed Responses SSE JSON: {error}"),
        })?;
    if !event.is_object() {
        return Err(AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedEvent,
            message: "Responses SSE data must be a JSON object".into(),
        });
    }
    Ok(event)
}

pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: Client,
    negotiated_protocol: AtomicU8,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Self {
        // Keep auto_sys_proxy (reqwest default). The workspace crate must
        // enable reqwest's `system-proxy` feature so Windows Internet
        // Settings are honored; HTTP_PROXY/HTTPS_PROXY are still read
        // from the process environment either way.
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("build reqwest client");
        Self {
            config,
            client,
            negotiated_protocol: AtomicU8::new(PROTOCOL_UNKNOWN),
        }
    }

    /// Build with an injected HTTP client. The composition root owns
    /// transport policy (timeouts, proxies, connection pools); tests use
    /// this to pin a `no_proxy` client so a machine-wide proxy can never
    /// intercept loopback mock servers.
    pub fn with_client(config: OpenAiConfig, client: Client) -> Self {
        Self {
            config,
            client,
            negotiated_protocol: AtomicU8::new(PROTOCOL_UNKNOWN),
        }
    }
}

#[async_trait]
impl ModelTransport for OpenAiProvider {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: self.config.max_output_tokens,
            context_window: self.config.context_window,
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
        let codec = ToolNameCodec::from_request(&request)?;
        let selected = match self.config.protocol {
            OpenAiProtocol::ChatCompletions => PROTOCOL_CHAT,
            OpenAiProtocol::Responses => PROTOCOL_RESPONSES,
            OpenAiProtocol::Auto => self.negotiated_protocol.load(Ordering::Acquire),
        };

        if selected == PROTOCOL_CHAT {
            return self
                .complete_chat_stream(&request, sink, &codec)
                .await
                .map_err(|error| error.error);
        }
        if selected == PROTOCOL_RESPONSES {
            return self
                .complete_responses_stream(&request, sink, &codec)
                .await
                .map_err(|error| error.error);
        }

        match self.complete_responses_stream(&request, sink, &codec).await {
            Ok(output) => {
                self.negotiated_protocol
                    .store(PROTOCOL_RESPONSES, Ordering::Release);
                Ok(output)
            }
            Err(error) if error.endpoint_unsupported => {
                let output = self
                    .complete_chat_stream(&request, sink, &codec)
                    .await
                    .map_err(|error| error.error)?;
                self.negotiated_protocol
                    .store(PROTOCOL_CHAT, Ordering::Release);
                Ok(output)
            }
            Err(error) => Err(error.error),
        }
    }
}

struct ProtocolError {
    error: AgentError,
    endpoint_unsupported: bool,
}

impl ProtocolError {
    fn transport(retryable: bool, message: String) -> Self {
        Self {
            error: AgentError::Transport { retryable, message },
            endpoint_unsupported: false,
        }
    }

    fn http_transport(
        retryable: bool,
        retry_after: Option<RetryAfterMillis>,
        message: String,
    ) -> Self {
        let error = match (retryable, retry_after) {
            (true, Some(retry_after_ms)) => AgentError::TransportRetryAfter {
                retry_after_ms,
                message,
            },
            _ => AgentError::Transport { retryable, message },
        };
        Self {
            error,
            endpoint_unsupported: false,
        }
    }

    fn unsupported(code: u16, body: &str) -> Self {
        Self {
            error: AgentError::Transport {
                retryable: false,
                message: format!("HTTP {code}: {}", truncate_error_body(body)),
            },
            endpoint_unsupported: true,
        }
    }
}

impl From<AgentError> for ProtocolError {
    fn from(error: AgentError) -> Self {
        Self {
            error,
            endpoint_unsupported: false,
        }
    }
}

impl OpenAiProvider {
    async fn complete_chat_stream(
        &self,
        request: &ModelRequest,
        sink: &dyn ModelEventSink,
        codec: &ToolNameCodec,
    ) -> Result<ModelOutput, ProtocolError> {
        let payload = build_chat_wire_request(request, &self.config, codec);
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
            .map_err(|error| ProtocolError::transport(true, format!("request failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            let retry_after =
                parse_retry_after(response.headers().get(RETRY_AFTER), SystemTime::now());
            let body = response.text().await.unwrap_or_default();
            return Err(ProtocolError::http_transport(
                http_status_retryable(code, &body),
                retry_after,
                format!("HTTP {code}: {}", truncate_error_body(&body)),
            ));
        }

        let byte_stream = response.bytes_stream().map_err(std::io::Error::other);
        let reader = StreamReader::new(byte_stream);
        let mut lines = FramedRead::new(
            reader,
            LinesCodec::new_with_max_length(self.config.max_stream_bytes.max(1)),
        );

        let max_stream_bytes = self.config.max_stream_bytes;
        let mut total_bytes = 0usize;
        let mut accumulator = StreamAccumulator::default();
        let mut framer = SseEventFramer::new(max_stream_bytes);
        let mut saw_done = false;
        // A stream that stops delivering bytes without closing is a stalled
        // connection, not a slow model: bound the silent gap so the turn
        // fails retryable instead of hanging until the peer gives up.
        let mut idle_deadline = tokio::time::Instant::now() + self.config.timeout;
        loop {
            tokio::select! {
                _ = request.cancel.cancelled() => {
                    return Err(ProtocolError::from(AgentError::Cancelled));
                }
                _ = tokio::time::sleep_until(idle_deadline) => {
                    return Err(ProtocolError::transport(
                        true,
                        format!(
                            "provider stream stalled: no bytes for {:?}",
                            self.config.timeout
                        ),
                    ));
                }
                line = lines.next() => {
                    idle_deadline = tokio::time::Instant::now() + self.config.timeout;
                    match line {
                        Some(Ok(line)) => {
                            // Every decoded line counts toward the stream
                            // cap (line content plus its newline): a
                            // provider that streams without end is refused
                            // instead of growing the accumulator forever.
                            total_bytes = total_bytes.saturating_add(line.len() + 1);
                            if total_bytes > max_stream_bytes {
                                return Err(ProtocolError::transport(
                                    false,
                                    format!(
                                        "stream exceeded the {max_stream_bytes} byte cap; provider response is not bounded"
                                    ),
                                ));
                            }
                            match framer.push_line(&line) {
                                Ok(Some(event)) => {
                                    if event.data == "[DONE]" {
                                        saw_done = true;
                                        break;
                                    }
                                    match parse_wire_chunk(&event.data)
                                        .map_err(ProtocolError::from)?
                                    {
                                        Some(chunk) => {
                                            for event in accumulator.apply(&chunk)
                                                .map_err(ProtocolError::from)?
                                            {
                                                sink.on_chunk(codec.remap_chunk(event)).await.map_err(ProtocolError::from)?;
                                            }
                                        }
                                        None => tracing::debug!(%event.data, "ignoring unknown Chat Completions extension event"),
                                    }
                                }
                                Ok(None) => {}
                                Err(message) => {
                                    return Err(ProtocolError::transport(false, message));
                                }
                            }
                        }
                        Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                            return Err(ProtocolError::transport(
                                false,
                                format!(
                                    "SSE line exceeded the {max_stream_bytes} byte cap before framing"
                                ),
                            ));
                        }
                        Some(Err(error)) => {
                            return Err(ProtocolError::transport(
                                true,
                                format!("stream error: {error}"),
                            ));
                        }
                        None => {
                            // The stream closed without a trailing blank
                            // line; flush a residual event per the SSE spec.
                            if let Some(event) = framer.finish() {
                                if event.data == "[DONE]" {
                                    saw_done = true;
                                } else if let Some(chunk) =
                                    parse_wire_chunk(&event.data).map_err(ProtocolError::from)?
                                {
                                    for event in accumulator.apply(&chunk)
                                        .map_err(ProtocolError::from)?
                                    {
                                        sink.on_chunk(codec.remap_chunk(event))
                                            .await
                                            .map_err(ProtocolError::from)?;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        if let Some(message) = accumulator.take_terminal_error() {
            return Err(ProtocolError::transport(true, message));
        }
        if !saw_done {
            return Err(ProtocolError::transport(
                true,
                "Chat Completions stream ended before the [DONE] marker".into(),
            ));
        }
        let usage = accumulator.usage.clone().unwrap_or_default();
        let (content, tool_calls) = accumulator.finalize().map_err(ProtocolError::from)?;
        let tool_calls = codec.remap_calls(tool_calls);
        sink.on_chunk(ModelChunk::Done)
            .await
            .map_err(ProtocolError::from)?;

        Ok(ModelOutput {
            content,
            tool_calls,
            usage,
        })
    }

    async fn complete_responses_stream(
        &self,
        request: &ModelRequest,
        sink: &dyn ModelEventSink,
        codec: &ToolNameCodec,
    ) -> Result<ModelOutput, ProtocolError> {
        let payload = build_responses_wire_request(request, &self.config, codec);
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|error| ProtocolError::transport(true, format!("request failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            let retry_after =
                parse_retry_after(response.headers().get(RETRY_AFTER), SystemTime::now());
            let body = response.text().await.unwrap_or_default();
            if responses_endpoint_unsupported(code, &body) {
                return Err(ProtocolError::unsupported(code, &body));
            }
            return Err(ProtocolError::http_transport(
                http_status_retryable(code, &body),
                retry_after,
                format!("HTTP {code}: {}", truncate_error_body(&body)),
            ));
        }

        let byte_stream = response.bytes_stream().map_err(std::io::Error::other);
        let reader = StreamReader::new(byte_stream);
        let mut lines = FramedRead::new(
            reader,
            LinesCodec::new_with_max_length(self.config.max_stream_bytes.max(1)),
        );
        let max_stream_bytes = self.config.max_stream_bytes;
        let mut total_bytes = 0usize;
        let mut accumulator = ResponsesAccumulator::default();
        let mut framer = SseEventFramer::new(max_stream_bytes);
        // Same stalled-connection bound as the chat path: fail retryable
        // instead of hanging on a silent peer.
        let mut idle_deadline = tokio::time::Instant::now() + self.config.timeout;
        loop {
            tokio::select! {
                _ = request.cancel.cancelled() => return Err(ProtocolError::from(AgentError::Cancelled)),
                _ = tokio::time::sleep_until(idle_deadline) => {
                    return Err(ProtocolError::transport(
                        true,
                        format!(
                            "provider stream stalled: no bytes for {:?}",
                            self.config.timeout
                        ),
                    ));
                }
                line = lines.next() => {
                    idle_deadline = tokio::time::Instant::now() + self.config.timeout;
                    match line {
                        Some(Ok(line)) => {
                            total_bytes = total_bytes.saturating_add(line.len() + 1);
                            if total_bytes > max_stream_bytes {
                                return Err(ProtocolError::transport(
                                    false,
                                    format!("stream exceeded the {max_stream_bytes} byte cap; provider response is not bounded"),
                                ));
                            }
                            match framer.push_line(&line) {
                                Ok(Some(frame)) => {
                                    if frame.data == "[DONE]" { break; }
                                    let event = parse_responses_event(&frame.data)
                                        .map_err(ProtocolError::from)?;
                                    crate::sse::validate_sse_event_routing(
                                        frame.event.as_deref(),
                                        &event,
                                    )
                                    .map_err(ProtocolError::from)?;
                                    for chunk in accumulator.apply(&event).map_err(ProtocolError::from)? {
                                        sink.on_chunk(codec.remap_chunk(chunk)).await.map_err(ProtocolError::from)?;
                                    }
                                    if accumulator.is_completed() {
                                        break;
                                    }
                                }
                                Ok(None) => {}
                                Err(message) => {
                                    return Err(ProtocolError::transport(false, message));
                                }
                            }
                        }
                        Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                            return Err(ProtocolError::transport(
                                false,
                                format!("SSE line exceeded the {max_stream_bytes} byte cap before framing"),
                            ));
                        }
                        Some(Err(error)) => {
                            return Err(ProtocolError::transport(true, format!("stream error: {error}")));
                        }
                        None => {
                            // Stream closed without a trailing blank line;
                            // flush a residual event per the SSE spec.
                            if let Some(event) = framer.finish() && event.data != "[DONE]" {
                                let event = parse_responses_event(&event.data)
                                    .map_err(ProtocolError::from)?;
                                for chunk in accumulator.apply(&event).map_err(ProtocolError::from)? {
                                    sink.on_chunk(codec.remap_chunk(chunk)).await.map_err(ProtocolError::from)?;
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        if let Some(error) = accumulator.take_terminal_error() {
            return Err(match error.kind {
                ResponseStreamErrorKind::OutputLimit => {
                    ProtocolError::from(AgentError::ModelOutputLimit {
                        reason: error.message,
                    })
                }
                ResponseStreamErrorKind::Model => {
                    ProtocolError::from(AgentError::Model(error.message))
                }
                ResponseStreamErrorKind::Transport => {
                    ProtocolError::transport(error.retryable, error.message)
                }
            });
        }
        if !accumulator.is_completed() {
            return Err(ProtocolError::transport(
                true,
                "Responses stream ended before the response.completed marker".into(),
            ));
        }
        let (content, tool_calls, usage) = accumulator.finalize().map_err(ProtocolError::from)?;
        let tool_calls = codec.remap_calls(tool_calls);
        sink.on_chunk(ModelChunk::Done)
            .await
            .map_err(ProtocolError::from)?;
        Ok(ModelOutput {
            content,
            tool_calls,
            usage,
        })
    }
}

fn build_chat_wire_request(
    request: &ModelRequest,
    config: &OpenAiConfig,
    codec: &ToolNameCodec,
) -> Value {
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
                                    "name": codec.to_wire(&call.name),
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
                    "name": codec.to_wire(&tool.name),
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

fn build_responses_wire_request(
    request: &ModelRequest,
    config: &OpenAiConfig,
    codec: &ToolNameCodec,
) -> Value {
    let mut input = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        match message.role {
            ModelRole::System | ModelRole::User | ModelRole::Assistant => {
                if !message.content.is_empty() {
                    input.push(json!({
                        "role": role_name(message.role),
                        "content": message.content,
                    }));
                }
                if message.role == ModelRole::Assistant {
                    input.extend(message.tool_calls.iter().map(|call| {
                        json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": codec.to_wire(&call.name),
                            "arguments": call.arguments.to_string(),
                        })
                    }));
                }
            }
            ModelRole::Tool => {
                if let Some(call_id) = &message.tool_call_id {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": message.content,
                    }));
                }
            }
        }
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": codec.to_wire(&tool.name),
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect();
    let mut wire = json!({
        "model": config.model,
        "input": input,
        "tools": tools,
        "stream": true,
        "store": false,
    });
    if config.send_stream_options {
        wire["stream_options"] = json!({ "include_obfuscation": false });
    }
    if config.send_max_tokens {
        wire["max_output_tokens"] = json!(config.max_output_tokens);
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
    use agent_contracts::{
        CancellationToken, ModelMessage, ModelTransport, ToolSemanticRole, ToolSpec,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            }],
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };
        let config = OpenAiConfig {
            api_key: "secret".into(),
            base_url: "https://example.com/v1".into(),
            model: "deepseek-chat".into(),
            protocol: OpenAiProtocol::ChatCompletions,
            max_output_tokens: 2048,
            timeout: Duration::from_secs(30),
            send_stream_options: true,
            send_max_tokens: true,
            max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
            context_window: None,
        };
        let codec = ToolNameCodec::from_request(&request).expect("no name collision");
        let wire = build_chat_wire_request(&request, &config, &codec);
        assert_eq!(wire["model"], "deepseek-chat");
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["stream_options"]["include_usage"], true);
        assert_eq!(wire["messages"][0]["role"], "system");
        assert_eq!(wire["messages"][1]["role"], "user");
        assert_eq!(wire["tools"][0]["function"]["name"], "fs_list");
        assert_eq!(wire["max_tokens"], 2048);

        // Assistant tool calls serialize as function calls with string args.
        let assistant = &wire["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "fs_list");
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
    fn builds_stateless_responses_tool_continuation() {
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
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            }],
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };
        let mut config = dummy_config("https://example.com/v1".into());
        config.protocol = OpenAiProtocol::Responses;
        config.send_stream_options = true;
        config.send_max_tokens = true;
        let codec = ToolNameCodec::from_request(&request).unwrap();
        let wire = build_responses_wire_request(&request, &config, &codec);

        assert_eq!(wire["stream"], true);
        assert_eq!(wire["store"], false);
        assert_eq!(wire["max_output_tokens"], 64);
        assert_eq!(wire["stream_options"]["include_obfuscation"], false);
        assert_eq!(wire["input"][0]["role"], "system");
        assert_eq!(wire["input"][2]["type"], "function_call");
        assert_eq!(wire["input"][2]["call_id"], "call-1");
        assert_eq!(wire["input"][2]["name"], "fs_list");
        assert_eq!(wire["input"][3]["type"], "function_call_output");
        assert_eq!(wire["input"][3]["call_id"], "call-1");
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["name"], "fs_list");
        assert!(wire["tools"][0].get("function").is_none());
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
            protocol: OpenAiProtocol::ChatCompletions,
            max_output_tokens: 2048,
            timeout: Duration::from_secs(30),
            send_stream_options: false,
            send_max_tokens: false,
            max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
            context_window: None,
        };
        let codec = ToolNameCodec::from_request(&request).expect("no name collision");
        let wire = build_chat_wire_request(&request, &config, &codec);
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

    fn dummy_config(base_url: String) -> OpenAiConfig {
        OpenAiConfig {
            api_key: "secret".into(),
            base_url,
            model: "mock".into(),
            protocol: OpenAiProtocol::ChatCompletions,
            max_output_tokens: 64,
            timeout: Duration::from_secs(5),
            send_stream_options: false,
            send_max_tokens: false,
            max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
            context_window: None,
        }
    }

    fn fs_list_request() -> ModelRequest {
        ModelRequest {
            messages: vec![ModelMessage::user("list files")],
            tools: vec![ToolSpec {
                name: "fs.list".into(),
                description: "list files".into(),
                input_schema: json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            }],
            metadata: json!({}),
            cancel: CancellationToken::new(),
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        chunks: std::sync::Mutex<Vec<ModelChunk>>,
    }

    #[async_trait]
    impl ModelEventSink for RecordingSink {
        async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
            self.chunks.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    async fn read_complete_http_request(socket: &mut tokio::net::TcpStream) {
        const MAX_TEST_REQUEST_BYTES: usize = 64 * 1024;
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let read = socket.read(&mut buf).await.unwrap();
            assert!(read > 0, "client closed before sending the request");
            request.extend_from_slice(&buf[..read]);
            assert!(
                request.len() <= MAX_TEST_REQUEST_BYTES,
                "mock request exceeded its test bound"
            );

            let Some(header_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|offset| offset + 4)
            else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or_default();
            if request.len() >= header_end.saturating_add(content_length) {
                return;
            }
        }
    }

    async fn serve_sse_once(sse: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16 * 1024];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn colliding_wire_names_fail_before_the_http_call() {
        let provider = OpenAiProvider::new(dummy_config("http://127.0.0.1:1/v1".into()));
        let mut request = fs_list_request();
        request.tools.push(ToolSpec {
            name: "fs_list".into(),
            description: "already underscored".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        });
        let error = provider.complete(request).await.unwrap_err().to_string();
        assert!(
            error.contains("both serialize"),
            "collision must be named: {error}"
        );
    }

    #[tokio::test]
    async fn dotted_core_tool_ids_round_trip_on_the_wire() {
        // 上游看到 fs_list；内核仍收到 fs.list。
        // A machine-wide proxy (Clash/V2Ray WinINET settings — the reason
        // this workspace enables reqwest's `system-proxy` feature) must
        // not intercept the loopback mock server: a proxied request never
        // carries the wire body this test asserts on and comes back as a
        // gateway 502. The injected client pins `no_proxy`.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.contains("\"fs_list\""),
                "wire function name must be OpenAI-legal: {request}"
            );
            assert!(
                !request.contains("\"fs.list\""),
                "Core id must not appear as the function name: {request}"
            );
            let sse = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"fs_list\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let provider = OpenAiProvider::with_client(
            dummy_config(format!("http://{addr}/v1")),
            Client::builder().no_proxy().build().unwrap(),
        );
        let output = provider.complete(fs_list_request()).await.unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].id, "c1");
        assert_eq!(output.tool_calls[0].name, "fs.list");
    }

    #[tokio::test]
    async fn stalled_stream_fails_retryable_after_the_idle_bound() {
        // The mock server sends valid response headers and then goes silent
        // with an open body. The provider must fail retryable on the idle
        // bound instead of hanging until the peer or the client deadline
        // gives up — that hang is what turned relay hiccups into
        // multi-minute dead cells.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            let headers =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            socket.write_all(headers.as_bytes()).await.unwrap();
            // No body, no close: the stream stalls by construction.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let mut config = dummy_config(format!("http://{addr}/v1"));
        config.timeout = Duration::from_millis(150);
        let provider = OpenAiProvider::with_client(
            config,
            // Keep the injected client's total timeout far above the idle
            // bound so this test exercises the idle path, not the client
            // deadline.
            Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        );

        let started = std::time::Instant::now();
        let error = provider.complete(fs_list_request()).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the idle bound must fire long before the client deadline"
        );
        assert!(
            matches!(
                &error,
                AgentError::Transport {
                    retryable: true,
                    ..
                }
            ) && error.to_string().contains("stalled"),
            "a silent stream must surface as a retryable stall: {error}"
        );
    }

    #[tokio::test]
    async fn responses_endpoint_round_trips_function_calls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
            assert!(request.contains("\"input\""));
            assert!(request.contains("\"name\":\"fs_list\""));
            assert!(!request.contains("\"function\":{\"name\":\"fs_list\""));
            let sse = concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"fs_list\",\"arguments\":\"\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":6}}}}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut config = dummy_config(format!("http://{addr}/v1"));
        config.protocol = OpenAiProtocol::Responses;
        let provider =
            OpenAiProvider::with_client(config, Client::builder().no_proxy().build().unwrap());
        let output = provider.complete(fs_list_request()).await.unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].id, "call_1");
        assert_eq!(output.tool_calls[0].name, "fs.list");
        assert_eq!(output.tool_calls[0].arguments, json!({}));
        assert_eq!(output.usage.input_tokens, Some(10));
        assert_eq!(output.usage.output_tokens, Some(2));
        assert_eq!(output.usage.cached_input_tokens, Some(6));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn responses_stream_with_declared_event_names_must_agree_with_payload_types() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
            // The event name and the payload type say different things; the
            // second routing identity must fail closed instead of winning
            // silently over the payload.
            let sse = concat!(
                "event: response.output_item.done\n",
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"fs_list\",\"arguments\":\"{}\"}}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut config = dummy_config(format!("http://{addr}/v1"));
        config.protocol = OpenAiProtocol::Responses;
        let provider =
            OpenAiProvider::with_client(config, Client::builder().no_proxy().build().unwrap());
        let error = provider.complete(fs_list_request()).await.unwrap_err();
        assert!(
            error.to_string().contains("contradicts the payload type"),
            "a contradicting SSE event name must surface as a typed protocol error: {error}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn responses_stream_with_matching_event_names_round_trips() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
            let sse = concat!(
                "event: response.output_item.added\n",
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"fs_list\",\"arguments\":\"\"}}\n\n",
                "event: response.function_call_arguments.delta\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":6}}}}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut config = dummy_config(format!("http://{addr}/v1"));
        config.protocol = OpenAiProtocol::Responses;
        let provider =
            OpenAiProvider::with_client(config, Client::builder().no_proxy().build().unwrap());
        let output = provider.complete(fs_list_request()).await.unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].name, "fs.list");
        assert_eq!(output.tool_calls[0].arguments, json!({}));
        assert_eq!(output.usage.cached_input_tokens, Some(6));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn chat_joins_multiline_data_events_before_the_blank_boundary() {
        // A compliant provider may split one event's JSON across several
        // `data:` lines; the payload joins with `\n` and only a blank line
        // closes the event. Per-line JSON parsing would reject the
        // unterminated first line and fail the turn.
        let addr = serve_sse_once(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}\n",
            "data: }]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let provider = OpenAiProvider::with_client(
            dummy_config(format!("http://{addr}/v1")),
            Client::builder().no_proxy().build().unwrap(),
        );
        let sink = RecordingSink::default();
        let output = provider
            .complete_stream(fs_list_request(), &sink)
            .await
            .unwrap();
        assert_eq!(output.content, "Hello");
        assert_eq!(
            &sink.chunks.lock().unwrap()[..],
            &[
                ModelChunk::TextDelta {
                    delta: "Hello".into()
                },
                ModelChunk::Done,
            ]
        );
    }

    #[tokio::test]
    async fn responses_joins_multiline_data_events_before_the_blank_boundary() {
        let addr = serve_sse_once(concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\n",
            "data: \"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"fs_list\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":6}}}}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let mut config = dummy_config(format!("http://{addr}/v1"));
        config.protocol = OpenAiProtocol::Responses;
        let provider =
            OpenAiProvider::with_client(config, Client::builder().no_proxy().build().unwrap());
        let sink = RecordingSink::default();
        let output = provider
            .complete_stream(fs_list_request(), &sink)
            .await
            .unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].id, "call_1");
        assert_eq!(output.tool_calls[0].name, "fs.list");
        assert_eq!(output.tool_calls[0].arguments, json!({}));
    }

    #[tokio::test]
    async fn chat_flushes_trailing_done_when_the_stream_closes_without_a_blank_line() {
        let addr = serve_sse_once(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: [DONE]",
        ))
        .await;
        let provider = OpenAiProvider::with_client(
            dummy_config(format!("http://{addr}/v1")),
            Client::builder().no_proxy().build().unwrap(),
        );
        let sink = RecordingSink::default();
        let output = provider
            .complete_stream(fs_list_request(), &sink)
            .await
            .unwrap();
        assert_eq!(output.content, "hi");
        assert_eq!(
            &sink.chunks.lock().unwrap()[..],
            &[
                ModelChunk::TextDelta { delta: "hi".into() },
                ModelChunk::Done,
            ]
        );
    }

    #[test]
    fn gateway_wrapped_upstream_400_is_retryable_real_400_is_not() {
        let wrapped =
            r#"{"error":{"message":"Upstream request failed","type":"invalid_request_error"}}"#;
        assert!(http_status_retryable(400, wrapped));
        assert!(http_status_retryable(502, "bad gateway"));
        assert!(http_status_retryable(429, "rate limited"));
        assert!(!http_status_retryable(
            400,
            r#"{"error":{"message":"Invalid 'tools[0].name': string does not match pattern. Expected '^[a-zA-Z0-9_-]+$'."}}"#
        ));
        assert!(!http_status_retryable(
            400,
            r#"{"error":{"message":"context_length_exceeded"}}"#
        ));
        assert!(!http_status_retryable(401, wrapped));
    }

    #[test]
    fn retry_after_accepts_delta_and_http_date_and_clamps_both() {
        let seconds = reqwest::header::HeaderValue::from_static("2");
        assert_eq!(
            parse_retry_after(Some(&seconds), SystemTime::UNIX_EPOCH)
                .unwrap()
                .get(),
            2_000
        );

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let future = httpdate::fmt_http_date(now + Duration::from_secs(3));
        let date: reqwest::header::HeaderValue = future.parse().unwrap();
        assert_eq!(parse_retry_after(Some(&date), now).unwrap().get(), 3_000);

        let excessive = reqwest::header::HeaderValue::from_static("999999999");
        assert_eq!(
            parse_retry_after(Some(&excessive), now).unwrap().get(),
            RetryAfterMillis::MAX_MILLIS
        );
        let invalid = reqwest::header::HeaderValue::from_static("later");
        assert!(parse_retry_after(Some(&invalid), now).is_none());
    }

    #[tokio::test]
    async fn retry_after_is_exposed_as_bounded_typed_transport_metadata() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"error":{"message":"rate limited"}}"#;
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = OpenAiProvider::with_client(
            dummy_config(format!("http://{addr}/v1")),
            Client::builder().no_proxy().build().unwrap(),
        );
        let error = provider.complete(fs_list_request()).await.unwrap_err();
        assert!(matches!(
            error,
            AgentError::TransportRetryAfter {
                retry_after_ms,
                ..
            } if retry_after_ms.get() == RetryAfterMillis::MAX_MILLIS
        ));
    }

    #[tokio::test]
    async fn malformed_chat_sse_data_fails_with_a_typed_protocol_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let sse = "data: {\"choices\":[}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = OpenAiProvider::with_client(
            dummy_config(format!("http://{addr}/v1")),
            Client::builder().no_proxy().build().unwrap(),
        );
        let error = provider.complete(fs_list_request()).await.unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn chat_eof_without_done_refuses_the_partial_stream() {
        let addr =
            serve_sse_once("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n").await;
        let provider = OpenAiProvider::with_client(
            dummy_config(format!("http://{addr}/v1")),
            Client::builder().no_proxy().build().unwrap(),
        );
        let sink = RecordingSink::default();
        let error = provider
            .complete_stream(fs_list_request(), &sink)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::Transport {
                retryable: true,
                ..
            }
        ));
        assert_eq!(
            &sink.chunks.lock().unwrap()[..],
            &[ModelChunk::TextDelta {
                delta: "partial".into()
            }],
            "a partial live delta may be observed, but Done must never be published"
        );
    }

    #[tokio::test]
    async fn valid_partial_arguments_do_not_replace_response_completed() {
        let addr = serve_sse_once(concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"fs_list\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let mut config = dummy_config(format!("http://{addr}/v1"));
        config.protocol = OpenAiProtocol::Responses;
        let provider =
            OpenAiProvider::with_client(config, Client::builder().no_proxy().build().unwrap());
        let sink = RecordingSink::default();
        let error = provider
            .complete_stream(fs_list_request(), &sink)
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            AgentError::Transport {
                retryable: true,
                ..
            }
        ));
        assert!(error.to_string().contains("response.completed"));
        assert!(matches!(
            &sink.chunks.lock().unwrap()[..],
            [ModelChunk::ToolCallDelta { .. }]
        ));
    }

    #[test]
    fn malformed_responses_sse_data_is_not_an_ignorable_extension() {
        let error = parse_responses_event(r#"{"type":"response.completed"#).unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));
        assert!(parse_responses_event(r#"{"type":"response.vendor_extension","value":7}"#).is_ok());
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

    #[tokio::test]
    async fn stream_over_cap_fails_bounded() {
        // A provider that streams SSE without end must be refused at the
        // byte cap instead of growing the accumulator forever: a broken or
        // malicious upstream cannot exhaust the runtime's memory.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = socket.write_all(headers.as_bytes()).await;
            // Stream deltas forever; the client must stop at its cap.
            let mut n = 0u64;
            loop {
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"chunk{n}\"}}}}]}}\n\n"
                );
                let frame = format!("{:x}\r\n{}\r\n", body.len(), body);
                if socket.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                n += 1;
            }
        });

        let config = OpenAiConfig {
            api_key: "secret".into(),
            base_url: format!("http://{addr}/v1"),
            model: "mock".into(),
            protocol: OpenAiProtocol::ChatCompletions,
            max_output_tokens: 2048,
            timeout: Duration::from_secs(10),
            send_stream_options: true,
            send_max_tokens: true,
            max_stream_bytes: 512, // deliberately tiny cap
            context_window: None,
        };
        let provider = OpenAiProvider::new(config);
        let request = ModelRequest {
            messages: vec![ModelMessage::user("hi")],
            tools: Vec::new(),
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };
        let error = provider.complete(request).await.unwrap_err().to_string();
        assert!(
            error.contains("stream exceeded"),
            "the cap refusal must name the bound: {error}"
        );
        assert!(
            error.contains("512"),
            "the cap value must be surfaced: {error}"
        );
    }

    #[tokio::test]
    async fn unterminated_sse_line_is_bounded_before_decode() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_complete_http_request(&mut socket).await;
            let body = format!("data: {}", "x".repeat(1_024));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let config = OpenAiConfig {
            api_key: "secret".into(),
            base_url: format!("http://{addr}/v1"),
            model: "mock".into(),
            protocol: OpenAiProtocol::ChatCompletions,
            max_output_tokens: 2_048,
            timeout: Duration::from_secs(10),
            send_stream_options: true,
            send_max_tokens: true,
            max_stream_bytes: 512,
            context_window: None,
        };
        let provider = OpenAiProvider::new(config);
        let request = ModelRequest {
            messages: vec![ModelMessage::user("hi")],
            tools: Vec::new(),
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };

        let error = provider.complete(request).await.unwrap_err().to_string();
        assert!(
            error.contains("SSE line exceeded the 512 byte cap before framing"),
            "the decoder must fail at the line bound: {error}"
        );
    }

    #[test]
    fn capabilities_surface_the_declared_window() {
        let mut config = dummy_config("http://127.0.0.1:1/v1".into());
        config.context_window = Some(DEFAULT_DECLARED_CONTEXT_WINDOW);
        let provider = OpenAiProvider::new(config);
        assert_eq!(
            provider.capabilities().context_window,
            Some(DEFAULT_DECLARED_CONTEXT_WINDOW)
        );
    }
}
