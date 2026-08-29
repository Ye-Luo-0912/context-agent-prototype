//! Pure OpenAI Responses streaming-wire mapping.
//!
//! The runtime still owns the prompt and current-turn frame. This adapter only
//! converts that bounded frame to Responses input items and converts SSE events
//! back to the provider-neutral model contract.

use std::collections::BTreeMap;

use agent_contracts::{ModelChunk, ModelUsage, ToolCall};
use serde_json::Value;

#[derive(Debug, Default)]
struct AccFunctionCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Debug)]
pub struct ResponseStreamError {
    pub message: String,
    pub retryable: bool,
    pub kind: ResponseStreamErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseStreamErrorKind {
    Transport,
    OutputLimit,
    Model,
}

/// Accumulates the small subset of Responses events the coding runtime owns:
/// assistant text, function calls, usage, and terminal provider errors.
#[derive(Debug, Default)]
pub struct ResponsesAccumulator {
    content: String,
    calls: BTreeMap<usize, AccFunctionCall>,
    usage: Option<ModelUsage>,
    terminal_error: Option<ResponseStreamError>,
}

impl ResponsesAccumulator {
    pub fn apply(&mut self, event: &Value) -> Vec<ModelChunk> {
        let mut chunks = Vec::new();
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    self.content.push_str(delta);
                    chunks.push(ModelChunk::TextDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = event.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    let index = output_index(event);
                    let slot = self.calls.entry(index).or_default();
                    if slot.call_id.is_none() {
                        slot.call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("id").and_then(Value::as_str))
                            .map(ToOwned::to_owned);
                    }
                    if slot.name.is_none() {
                        slot.name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                    }
                    if slot.arguments.is_empty()
                        && let Some(arguments) = item.get("arguments").and_then(Value::as_str)
                        && !arguments.is_empty()
                    {
                        slot.arguments.push_str(arguments);
                        chunks.push(tool_delta(slot, arguments.to_string(), true, index));
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(event);
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    let slot = self.calls.entry(index).or_default();
                    if slot.call_id.is_none() {
                        slot.call_id = event
                            .get("call_id")
                            .and_then(Value::as_str)
                            .or_else(|| event.get("item_id").and_then(Value::as_str))
                            .map(ToOwned::to_owned);
                    }
                    let first = slot.arguments.is_empty();
                    slot.arguments.push_str(delta);
                    chunks.push(tool_delta(slot, delta.to_string(), first, index));
                }
            }
            "response.function_call_arguments.done" => {
                let index = output_index(event);
                let slot = self.calls.entry(index).or_default();
                if slot.call_id.is_none() {
                    slot.call_id = event
                        .get("call_id")
                        .and_then(Value::as_str)
                        .or_else(|| event.get("item_id").and_then(Value::as_str))
                        .map(ToOwned::to_owned);
                }
                if slot.name.is_none() {
                    slot.name = event
                        .get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                if slot.arguments.is_empty()
                    && let Some(arguments) = event.get("arguments").and_then(Value::as_str)
                    && !arguments.is_empty()
                {
                    slot.arguments.push_str(arguments);
                    chunks.push(tool_delta(slot, arguments.to_string(), true, index));
                }
            }
            "response.completed" => {
                self.capture_usage(event.get("response"));
            }
            "response.failed" | "response.incomplete" => {
                self.capture_usage(event.get("response"));
                let response = event.get("response").unwrap_or(event);
                let message = response
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        response
                            .pointer("/incomplete_details/reason")
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("Responses stream did not complete");
                let incomplete =
                    event.get("type").and_then(Value::as_str) == Some("response.incomplete");
                let output_limit = response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    == Some("max_output_tokens");
                self.terminal_error = Some(ResponseStreamError {
                    retryable: retryable_stream_error(response),
                    message: message.to_string(),
                    kind: if output_limit {
                        ResponseStreamErrorKind::OutputLimit
                    } else if incomplete {
                        ResponseStreamErrorKind::Model
                    } else {
                        ResponseStreamErrorKind::Transport
                    },
                });
            }
            "error" => {
                let error = event.get("error").unwrap_or(event);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Responses stream error");
                self.terminal_error = Some(ResponseStreamError {
                    retryable: retryable_stream_error(error),
                    message: message.to_string(),
                    kind: ResponseStreamErrorKind::Transport,
                });
            }
            _ => {}
        }
        chunks
    }

    fn capture_usage(&mut self, response: Option<&Value>) {
        let Some(usage) = response.and_then(|value| value.get("usage")) else {
            return;
        };
        self.usage = Some(ModelUsage {
            input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
            output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
            cached_input_tokens: usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
            ..Default::default()
        });
    }

    pub fn take_terminal_error(&mut self) -> Option<ResponseStreamError> {
        self.terminal_error.take()
    }

    pub fn finalize(self) -> (String, Vec<ToolCall>, ModelUsage) {
        let calls = self
            .calls
            .into_iter()
            .map(|(index, slot)| ToolCall {
                id: slot.call_id.unwrap_or_else(|| format!("call-{index}")),
                name: slot.name.unwrap_or_default(),
                arguments: serde_json::from_str(&slot.arguments).unwrap_or(Value::Null),
            })
            .collect();
        (self.content, calls, self.usage.unwrap_or_default())
    }
}

fn output_index(event: &Value) -> usize {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(0)
}

fn tool_delta(
    slot: &AccFunctionCall,
    arguments_delta: String,
    include_name: bool,
    index: usize,
) -> ModelChunk {
    ModelChunk::ToolCallDelta {
        call_id: slot
            .call_id
            .clone()
            .unwrap_or_else(|| format!("call-{index}")),
        name: include_name.then(|| slot.name.clone()).flatten(),
        arguments_delta,
    }
}

fn retryable_stream_error(value: &Value) -> bool {
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/error/code").and_then(Value::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        code.as_str(),
        "server_error" | "rate_limit_exceeded" | "upstream_network_error" | "timeout"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accumulates_text_function_calls_and_usage() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.apply(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
        }));
        let chunks = accumulator.apply(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_1",
            "delta": "{\"path\":\"README.md\"}"
        }));
        assert_eq!(
            chunks,
            vec![ModelChunk::ToolCallDelta {
                call_id: "call_1".into(),
                name: Some("fs_read".into()),
                arguments_delta: "{\"path\":\"README.md\"}".into(),
            }]
        );
        accumulator.apply(&json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 120, "output_tokens": 15}}
        }));
        let (content, calls, usage) = accumulator.finalize();
        assert!(content.is_empty());
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "fs_read");
        assert_eq!(calls[0].arguments, json!({"path": "README.md"}));
        assert_eq!(usage.input_tokens, Some(120));
        assert_eq!(usage.output_tokens, Some(15));
    }

    #[test]
    fn output_item_done_is_a_bounded_non_delta_fallback() {
        let mut accumulator = ResponsesAccumulator::default();
        let chunks = accumulator.apply(&json!({
            "type": "response.output_item.done",
            "output_index": 2,
            "item": {
                "type": "function_call",
                "call_id": "call_2",
                "name": "probe",
                "arguments": "{\"value\":\"ready\"}"
            }
        }));
        assert_eq!(chunks.len(), 1);
        let (_, calls, _) = accumulator.finalize();
        assert_eq!(calls[0].id, "call_2");
        assert_eq!(calls[0].arguments, json!({"value": "ready"}));
    }

    #[test]
    fn terminal_errors_are_not_silently_empty_completions() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.apply(&json!({
            "type": "error",
            "error": {"code": "upstream_network_error", "message": "link dropped"}
        }));
        let error = accumulator.take_terminal_error().unwrap();
        assert!(error.retryable);
        assert_eq!(error.message, "link dropped");
        assert_eq!(error.kind, ResponseStreamErrorKind::Transport);
    }

    #[test]
    fn max_output_tokens_is_a_model_limit_not_a_transport_outage() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.apply(&json!({
            "type": "response.incomplete",
            "response": {
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 120, "output_tokens": 4096}
            }
        }));
        let error = accumulator.take_terminal_error().unwrap();
        assert!(!error.retryable);
        assert_eq!(error.message, "max_output_tokens");
        assert_eq!(error.kind, ResponseStreamErrorKind::OutputLimit);
    }

    #[test]
    fn other_incomplete_reasons_are_model_outcomes() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.apply(&json!({
            "type": "response.incomplete",
            "response": {"incomplete_details": {"reason": "content_filter"}}
        }));
        let error = accumulator.take_terminal_error().unwrap();
        assert!(!error.retryable);
        assert_eq!(error.kind, ResponseStreamErrorKind::Model);
    }
}
