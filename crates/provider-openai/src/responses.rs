//! Pure OpenAI Responses streaming-wire mapping.
//!
//! The runtime still owns the prompt and current-turn frame. This adapter only
//! converts that bounded frame to Responses input items and converts SSE events
//! back to the provider-neutral model contract.

use std::collections::BTreeMap;

use agent_contracts::{
    AgentError, AgentResult, ModelChunk, ModelProtocolErrorKind, ModelUsage, ToolCall,
};
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
    completed: bool,
}

impl ResponsesAccumulator {
    pub fn apply(&mut self, event: &Value) -> AgentResult<Vec<ModelChunk>> {
        let mut chunks = Vec::new();
        let event_type = required_string(event, "type", "Responses event")?;
        match event_type {
            "response.output_text.delta" => {
                let _ = output_index(event)?;
                let delta = required_string(event, "delta", event_type)?;
                if !delta.is_empty() {
                    self.content.push_str(delta);
                    chunks.push(ModelChunk::TextDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let index = output_index(event)?;
                let item = required_object(event, "item", event_type)?;
                let item_type = required_string(item, "type", "Responses output item")?;
                if item_type == "function_call" {
                    let call_id = required_call_id(item, event_type)?;
                    let name = required_string(item, "name", event_type)?;
                    let arguments = required_string(item, "arguments", event_type)?;
                    let slot = self.calls.entry(index).or_default();
                    if slot.call_id.is_none() {
                        slot.call_id = Some(call_id.to_owned());
                    }
                    if slot.name.is_none() {
                        slot.name = Some(name.to_owned());
                    }
                    if slot.arguments.is_empty() && !arguments.is_empty() {
                        slot.arguments.push_str(arguments);
                        chunks.push(tool_delta(slot, arguments.to_string(), true, index));
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(event)?;
                let call_id = required_call_id(event, event_type)?;
                let delta = required_string(event, "delta", event_type)?;
                if !delta.is_empty() {
                    let slot = self.calls.entry(index).or_default();
                    if slot.call_id.is_none() {
                        slot.call_id = Some(call_id.to_owned());
                    }
                    let first = slot.arguments.is_empty();
                    slot.arguments.push_str(delta);
                    chunks.push(tool_delta(slot, delta.to_string(), first, index));
                }
            }
            "response.function_call_arguments.done" => {
                let index = output_index(event)?;
                let call_id = required_call_id(event, event_type)?;
                let arguments = required_string(event, "arguments", event_type)?;
                let slot = self.calls.entry(index).or_default();
                if slot.call_id.is_none() {
                    slot.call_id = Some(call_id.to_owned());
                }
                if let Some(name) = optional_string(event, "name", event_type)?
                    && slot.name.is_none()
                {
                    slot.name = Some(name.to_owned());
                }
                if slot.arguments.is_empty() && !arguments.is_empty() {
                    slot.arguments.push_str(arguments);
                    chunks.push(tool_delta(slot, arguments.to_string(), true, index));
                }
            }
            "response.completed" => {
                let response = required_object(event, "response", event_type)?;
                self.capture_usage(response)?;
                self.completed = true;
            }
            "response.failed" | "response.incomplete" => {
                let response = required_object(event, "response", event_type)?;
                self.capture_usage(response)?;
                let incomplete = event_type == "response.incomplete";
                let message = if incomplete {
                    let details = required_object(response, "incomplete_details", event_type)?;
                    required_string(details, "reason", event_type)?
                } else {
                    let error = required_object(response, "error", event_type)?;
                    required_string(error, "message", event_type)?
                };
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
                let error = match event.get("error") {
                    Some(value) if value.is_object() => value,
                    Some(_) => {
                        return Err(malformed_event(
                            "Responses error event field `error` must be an object",
                        ));
                    }
                    None => event,
                };
                let message = required_string(error, "message", event_type)?;
                self.terminal_error = Some(ResponseStreamError {
                    retryable: retryable_stream_error(error),
                    message: message.to_string(),
                    kind: ResponseStreamErrorKind::Transport,
                });
            }
            _ => {}
        }
        Ok(chunks)
    }

    fn capture_usage(&mut self, response: &Value) -> AgentResult<()> {
        let Some(usage) = response.get("usage") else {
            return Ok(());
        };
        if !usage.is_object() {
            return Err(malformed_event("Responses usage must be an object"));
        }
        validate_optional_u64(usage, "input_tokens", "Responses usage")?;
        validate_optional_u64(usage, "output_tokens", "Responses usage")?;
        if let Some(details) = usage.get("input_tokens_details") {
            if !details.is_object() {
                return Err(malformed_event(
                    "Responses input_tokens_details must be an object",
                ));
            }
            validate_optional_u64(details, "cached_tokens", "Responses input_tokens_details")?;
        }
        self.usage = Some(ModelUsage {
            input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
            output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
            cached_input_tokens: usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
            ..Default::default()
        });
        Ok(())
    }

    pub fn take_terminal_error(&mut self) -> Option<ResponseStreamError> {
        self.terminal_error.take()
    }

    pub fn is_completed(&self) -> bool {
        self.completed
    }

    pub fn finalize(self) -> AgentResult<(String, Vec<ToolCall>, ModelUsage)> {
        let calls = self
            .calls
            .into_iter()
            .map(|(index, slot)| {
                let arguments = serde_json::from_str(&slot.arguments).map_err(|error| {
                    AgentError::ModelProtocol {
                        kind: ModelProtocolErrorKind::MalformedToolCall,
                        message: format!(
                            "Responses tool call at output index {index} has incomplete or invalid arguments: {error}"
                        ),
                    }
                })?;
                Ok(ToolCall {
                    id: slot.call_id.unwrap_or_else(|| format!("call-{index}")),
                    name: slot.name.unwrap_or_default(),
                    arguments,
                })
            })
            .collect::<AgentResult<Vec<_>>>()?;
        Ok((self.content, calls, self.usage.unwrap_or_default()))
    }
}

fn malformed_event(message: impl Into<String>) -> AgentError {
    AgentError::ModelProtocol {
        kind: ModelProtocolErrorKind::MalformedEvent,
        message: message.into(),
    }
}

fn required_object<'a>(value: &'a Value, field: &str, event_type: &str) -> AgentResult<&'a Value> {
    let field_value = value
        .get(field)
        .ok_or_else(|| malformed_event(format!("{event_type} requires object field `{field}`")))?;
    if !field_value.is_object() {
        return Err(malformed_event(format!(
            "{event_type} requires object field `{field}`"
        )));
    }
    Ok(field_value)
}

fn required_string<'a>(value: &'a Value, field: &str, event_type: &str) -> AgentResult<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed_event(format!("{event_type} requires string field `{field}`")))
}

fn optional_string<'a>(
    value: &'a Value,
    field: &str,
    event_type: &str,
) -> AgentResult<Option<&'a str>> {
    match value.get(field) {
        None => Ok(None),
        Some(value) => value.as_str().map(Some).ok_or_else(|| {
            malformed_event(format!("{event_type} field `{field}` must be a string"))
        }),
    }
}

fn required_call_id<'a>(value: &'a Value, event_type: &str) -> AgentResult<&'a str> {
    if let Some(call_id) = optional_string(value, "call_id", event_type)? {
        return Ok(call_id);
    }
    if let Some(item_id) = optional_string(value, "item_id", event_type)? {
        return Ok(item_id);
    }
    if let Some(id) = optional_string(value, "id", event_type)? {
        return Ok(id);
    }
    Err(malformed_event(format!(
        "{event_type} requires a string call identifier"
    )))
}

fn validate_optional_u64(value: &Value, field: &str, owner: &str) -> AgentResult<()> {
    if value
        .get(field)
        .is_some_and(|value| value.as_u64().is_none())
    {
        return Err(malformed_event(format!(
            "{owner} field `{field}` must be an unsigned integer"
        )));
    }
    Ok(())
}

fn output_index(event: &Value) -> AgentResult<usize> {
    let index = event
        .get("output_index")
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed_event("Responses event requires unsigned `output_index`"))?;
    usize::try_from(index)
        .map_err(|_| malformed_event("Responses event `output_index` exceeds platform bounds"))
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
        })).unwrap();
        let chunks = accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": "{\"path\":\"README.md\"}"
            }))
            .unwrap();
        assert_eq!(
            chunks,
            vec![ModelChunk::ToolCallDelta {
                call_id: "call_1".into(),
                name: Some("fs_read".into()),
                arguments_delta: "{\"path\":\"README.md\"}".into(),
            }]
        );
        accumulator
            .apply(&json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 120, "output_tokens": 15}}
            }))
            .unwrap();
        let (content, calls, usage) = accumulator.finalize().unwrap();
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
        let chunks = accumulator
            .apply(&json!({
                "type": "response.output_item.done",
                "output_index": 2,
                "item": {
                    "type": "function_call",
                    "call_id": "call_2",
                    "name": "probe",
                    "arguments": "{\"value\":\"ready\"}"
                }
            }))
            .unwrap();
        assert_eq!(chunks.len(), 1);
        let (_, calls, _) = accumulator.finalize().unwrap();
        assert_eq!(calls[0].id, "call_2");
        assert_eq!(calls[0].arguments, json!({"value": "ready"}));
    }

    #[test]
    fn terminal_errors_are_not_silently_empty_completions() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "error",
                "error": {"code": "upstream_network_error", "message": "link dropped"}
            }))
            .unwrap();
        let error = accumulator.take_terminal_error().unwrap();
        assert!(error.retryable);
        assert_eq!(error.message, "link dropped");
        assert_eq!(error.kind, ResponseStreamErrorKind::Transport);
    }

    #[test]
    fn max_output_tokens_is_a_model_limit_not_a_transport_outage() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.incomplete",
                "response": {
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {"input_tokens": 120, "output_tokens": 4096}
                }
            }))
            .unwrap();
        let error = accumulator.take_terminal_error().unwrap();
        assert!(!error.retryable);
        assert_eq!(error.message, "max_output_tokens");
        assert_eq!(error.kind, ResponseStreamErrorKind::OutputLimit);
    }

    #[test]
    fn other_incomplete_reasons_are_model_outcomes() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.incomplete",
                "response": {"incomplete_details": {"reason": "content_filter"}}
            }))
            .unwrap();
        let error = accumulator.take_terminal_error().unwrap();
        assert!(!error.retryable);
        assert_eq!(error.kind, ResponseStreamErrorKind::Model);
    }

    #[test]
    fn incomplete_function_arguments_are_a_typed_protocol_error() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 4,
                "item": {
                    "type": "function_call",
                    "call_id": "call_4",
                    "name": "fs_read",
                    "arguments": "{\"path\":"
                }
            }))
            .unwrap();
        let error = accumulator.finalize().unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                ..
            }
        ));
    }

    #[test]
    fn unknown_well_formed_extension_event_is_ignored() {
        let mut accumulator = ResponsesAccumulator::default();
        assert!(
            accumulator
                .apply(&json!({"type": "response.vendor_extension", "value": 7}))
                .unwrap()
                .is_empty()
        );
        let (content, calls, usage) = accumulator.finalize().unwrap();
        assert!(content.is_empty());
        assert!(calls.is_empty());
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.attempts, 0);
    }

    #[test]
    fn known_events_require_output_index_and_typed_fields() {
        let mut accumulator = ResponsesAccumulator::default();
        for event in [
            json!({
                "type": "response.output_text.delta",
                "delta": "missing index"
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": "zero",
                "delta": "wrong index type"
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": {"not": "a string"}
            }),
        ] {
            let error = accumulator.apply(&event).unwrap_err();
            assert!(matches!(
                error,
                AgentError::ModelProtocol {
                    kind: ModelProtocolErrorKind::MalformedEvent,
                    ..
                }
            ));
        }
    }

    #[test]
    fn known_terminal_events_require_their_response_shape() {
        let mut accumulator = ResponsesAccumulator::default();
        let error = accumulator
            .apply(&json!({"type": "response.completed"}))
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));
        assert!(!accumulator.is_completed());

        accumulator
            .apply(&json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 1, "output_tokens": 2}}
            }))
            .unwrap();
        assert!(accumulator.is_completed());
    }

    #[test]
    fn missing_or_non_string_event_type_is_not_an_extension() {
        let mut accumulator = ResponsesAccumulator::default();
        for event in [json!({"value": 7}), json!({"type": 7})] {
            let error = accumulator.apply(&event).unwrap_err();
            assert!(matches!(
                error,
                AgentError::ModelProtocol {
                    kind: ModelProtocolErrorKind::MalformedEvent,
                    ..
                }
            ));
        }
    }
}
