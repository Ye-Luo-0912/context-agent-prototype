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
    /// User-facing call id from the item's `call_id` field.
    call_id: Option<String>,
    /// Routing identity from `item.id` / arguments event `item_id`. This is a
    /// different namespace from `call_id` and is only compared against itself.
    item_id: Option<String>,
    name: Option<String>,
    arguments: String,
    /// A terminal full-arguments snapshot beyond what the deltas assembled;
    /// used as an authoritative repair source only when the assembled text
    /// does not parse at finalize time.
    terminal_arguments: Option<String>,
    /// `output_item.done` received: the slot is sealed and no further
    /// non-terminal event may touch it.
    sealed: bool,
    /// `function_call_arguments.done` received: deltas must not follow.
    arguments_done: bool,
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
    /// `call_id` namespace bound to its output index, global to the whole
    /// response so one identity cannot be split across two calls.
    call_id_indexes: BTreeMap<String, usize>,
    /// `item_id` namespace bound to its output index, global to the whole
    /// response so one identity cannot be split across two calls.
    item_id_indexes: BTreeMap<String, usize>,
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
                    let is_terminal = event_type == "response.output_item.done";
                    let slot = self.calls.entry(index).or_default();
                    if !is_terminal && slot.sealed {
                        return Err(malformed_event(format!(
                            "{event_type} for output index {index} arrived after the item was done; provider re-opened a sealed call"
                        )));
                    }
                    bind_identities(
                        slot,
                        index,
                        call_id,
                        name,
                        event_type,
                        &mut self.call_id_indexes,
                    )?;
                    if let Some(item_id) = optional_string(item, "id", event_type)? {
                        bind_item_id(slot, index, item_id, event_type, &mut self.item_id_indexes)?;
                    }
                    if is_terminal {
                        slot.sealed = true;
                        slot.arguments_done = true;
                        apply_terminal_arguments(
                            slot,
                            index,
                            item,
                            "arguments",
                            event_type,
                            &mut chunks,
                        )?;
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(event)?;
                let item_id = required_call_id(event, event_type)?;
                let delta = required_string(event, "delta", event_type)?;
                if !delta.is_empty() {
                    let slot = self.calls.entry(index).or_default();
                    bind_item_id(slot, index, item_id, event_type, &mut self.item_id_indexes)?;
                    if slot.sealed || slot.arguments_done {
                        return Err(malformed_event(format!(
                            "{event_type} for output index {index} arrived after the arguments/item was done; provider ordered or repeated the call stream"
                        )));
                    }
                    let first = slot.arguments.is_empty();
                    slot.arguments.push_str(delta);
                    chunks.push(tool_delta(slot, delta.to_string(), first, index));
                }
            }
            "response.function_call_arguments.done" => {
                let index = output_index(event)?;
                let item_id = required_call_id(event, event_type)?;
                let slot = self.calls.entry(index).or_default();
                bind_item_id(slot, index, item_id, event_type, &mut self.item_id_indexes)?;
                // A conflicting name is a broken stream even when the slot is
                // already sealed: a terminal event must agree with the bound
                // identity or fail closed, never silently drop the name.
                if let Some(name) = optional_string(event, "name", event_type)?
                    && slot.name.as_deref().is_some_and(|bound| bound != name)
                {
                    return Err(malformed_event(format!(
                        "{event_type} for output index {index} bound name `{name}` but the slot is already `{}`",
                        slot.name.as_deref().unwrap_or_default()
                    )));
                }
                if let Some(name) = optional_string(event, "name", event_type)?
                    && slot.name.is_none()
                {
                    slot.name = Some(name.to_owned());
                }
                if slot.sealed {
                    // The item terminal already landed; this arguments
                    // terminal must be an idempotent duplicate snapshot.
                    apply_terminal_arguments(
                        slot,
                        index,
                        event,
                        "arguments",
                        event_type,
                        &mut chunks,
                    )?;
                    return Ok(chunks);
                }
                slot.arguments_done = true;
                apply_terminal_arguments(slot, index, event, "arguments", event_type, &mut chunks)?;
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
                let arguments =
                    assemble_arguments(&slot.arguments, slot.terminal_arguments.as_deref(), index)?;
                let id = slot
                    .call_id
                    .clone()
                    .or_else(|| slot.item_id.clone())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        malformed_event(format!(
                            "Responses tool call at output index {index} has no call identity"
                        ))
                    })?;
                let name = slot
                    .name
                    .clone()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        malformed_event(format!(
                            "Responses tool call at output index {index} has no function name"
                        ))
                    })?;
                Ok(ToolCall {
                    id,
                    name,
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

/// Reject an identity contradiction: every event that names a call already
/// bound to a different id for the same output index is a broken stream, and
/// one call id can never be bound to two different output indexes.
fn bind_call_id(
    slot: &mut AccFunctionCall,
    index: usize,
    call_id: &str,
    event_type: &str,
    owners: &mut BTreeMap<String, usize>,
) -> AgentResult<()> {
    if let Some(owner) = owners.get(call_id).copied()
        && owner != index
    {
        return Err(malformed_event(format!(
            "{event_type} bound call id `{call_id}` to output index {index}, but that id is already the call id of output index {owner}"
        )));
    }
    if slot
        .call_id
        .as_deref()
        .is_some_and(|bound| bound != call_id)
    {
        return Err(malformed_event(format!(
            "{event_type} for output index {index} bound call id `{call_id}` but the slot is already `{}`",
            slot.call_id.as_deref().unwrap_or_default()
        )));
    }
    if slot.call_id.is_none() {
        slot.call_id = Some(call_id.to_owned());
        owners.insert(call_id.to_owned(), index);
    }
    Ok(())
}

/// Bind the item/routing identity of a slot, rejecting a contradiction with
/// any item id already bound to that output index or to another index.
fn bind_item_id(
    slot: &mut AccFunctionCall,
    index: usize,
    item_id: &str,
    event_type: &str,
    owners: &mut BTreeMap<String, usize>,
) -> AgentResult<()> {
    if let Some(owner) = owners.get(item_id).copied()
        && owner != index
    {
        return Err(malformed_event(format!(
            "{event_type} bound item id `{item_id}` to output index {index}, but that id is already the item id of output index {owner}"
        )));
    }
    if slot
        .item_id
        .as_deref()
        .is_some_and(|bound| bound != item_id)
    {
        return Err(malformed_event(format!(
            "{event_type} for output index {index} bound item id `{item_id}` but the slot is already `{}`",
            slot.item_id.as_deref().unwrap_or_default()
        )));
    }
    if slot.item_id.is_none() {
        slot.item_id = Some(item_id.to_owned());
        owners.insert(item_id.to_owned(), index);
    }
    Ok(())
}

/// Bind an id/name pair for an output index, rejecting a contradiction with
/// anything already bound to that slot.
fn bind_identities(
    slot: &mut AccFunctionCall,
    index: usize,
    call_id: &str,
    name: &str,
    event_type: &str,
    owners: &mut BTreeMap<String, usize>,
) -> AgentResult<()> {
    bind_call_id(slot, index, call_id, event_type, owners)?;
    if slot.call_id.is_none() {
        slot.call_id = Some(call_id.to_owned());
    }
    if slot.name.as_deref().is_some_and(|bound| bound != name) {
        return Err(malformed_event(format!(
            "{event_type} for output index {index} bound name `{name}` but the slot is already `{}`",
            slot.name.as_deref().unwrap_or_default()
        )));
    }
    if slot.name.is_none() {
        slot.name = Some(name.to_owned());
    }
    Ok(())
}

/// Order a terminal full-arguments snapshot into the slot. Assembled deltas
/// stay authoritative while they parse; the snapshot is kept as a repair
/// candidate and only seeds directly when nothing was assembled, so a
/// provider that never streams deltas still yields the call. A repeated
/// terminal snapshot must be byte-identical (idempotent) or the stream is
/// broken: two different authoritative argument bodies cannot both be true.
fn apply_terminal_arguments(
    slot: &mut AccFunctionCall,
    index: usize,
    source: &Value,
    field: &str,
    event_type: &str,
    chunks: &mut Vec<ModelChunk>,
) -> AgentResult<()> {
    let arguments = required_string(source, field, event_type)?;
    if slot.terminal_arguments.as_deref() == Some(arguments) {
        return Ok(());
    }
    if slot.terminal_arguments.is_some() {
        return Err(malformed_event(format!(
            "{event_type} for output index {index} carries arguments that conflict with the earlier terminal snapshot"
        )));
    }
    slot.terminal_arguments = Some(arguments.to_string());
    if slot.arguments.is_empty() && !arguments.is_empty() {
        slot.arguments.push_str(arguments);
        chunks.push(tool_delta(slot, arguments.to_string(), true, index));
    }
    Ok(())
}

/// The assembled deltas parse, or the authoritative terminal snapshot does,
/// or the call is a typed failure. The terminal snapshot only rescues an
/// assembly the provider itself cut short; it never overrides live deltas.
/// When both parse, they must describe the same arguments: a different
/// terminal body is a broken stream, not a silent winner. Parsing is strict:
/// duplicate object keys and unbounded documents fail.
fn assemble_arguments(
    assembled: &str,
    terminal: Option<&str>,
    index: usize,
) -> Result<Value, AgentError> {
    if let Ok(value) = agent_contracts::parse_arguments_strict(assembled) {
        return match terminal.filter(|snapshot| !snapshot.is_empty()) {
            Some(snapshot) => match agent_contracts::parse_arguments_strict(snapshot) {
                Ok(terminal_value) if terminal_value == value => Ok(value),
                Ok(_) => Err(malformed_event(format!(
                    "Responses tool call at output index {index} assembled from deltas differs from its terminal arguments snapshot"
                ))),
                Err(_) => Ok(value),
            },
            None => Ok(value),
        };
    }
    match terminal.filter(|snapshot| !snapshot.is_empty()) {
        Some(snapshot) => agent_contracts::parse_arguments_strict(snapshot).map_err(|error| {
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                message: format!(
                    "Responses tool call at output index {index} has incomplete or invalid arguments: {error}"
                ),
            }
        }),
        None => Err(AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedToolCall,
            message: format!(
                "Responses tool call at output index {index} has incomplete or invalid arguments"
            ),
        }),
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
            .or_else(|| slot.item_id.clone())
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

    #[test]
    fn call_and_item_ids_are_distinct_namespaces_and_conflicts_are_rejected() {
        // The item carries `call_id` (call_1) while the arguments events
        // carry `item_id` (fc_1). Those namespaces never contradict each
        // other; a real contradiction inside one namespace is rejected.
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "fs_read",
                    "arguments": ""
                }
            }))
            .unwrap();
        let error = accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_2",
                    "name": "fs_read",
                    "arguments": ""
                }
            }))
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));
        assert!(error.to_string().contains("call id `call_2`"));
    }

    #[test]
    fn deltas_after_the_item_is_done_are_rejected() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": "{\"a\":1}"}
            }))
            .unwrap();
        let error = accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": "{}"
            }))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("after the arguments/item was done"),
            "{error}"
        );
    }

    #[test]
    fn terminal_snapshot_rescues_an_incomplete_delta_assembly() {
        // Deltas were cut mid-object; the terminal done event carries the
        // authoritative full arguments and finalize must use it.
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": "{\"pa"
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "item_id": "fc_1",
                "arguments": "{\"path\":\"README.md\"}"
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 1, "output_tokens": 2}}
            }))
            .unwrap();
        let (_, calls, _) = accumulator.finalize().unwrap();
        assert_eq!(calls[0].arguments, json!({"path": "README.md"}));
    }

    #[test]
    fn duplicate_terminal_events_are_idempotent_snapshots() {
        let mut accumulator = ResponsesAccumulator::default();
        for _ in 0..2 {
            accumulator
                .apply(&json!({
                    "type": "response.function_call_arguments.done",
                    "output_index": 0,
                    "item_id": "fc_1",
                    "name": "fs_read",
                    "arguments": "{}"
                }))
                .unwrap();
        }
        let (_, calls, _) = accumulator.finalize().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "fc_1");
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn added_full_arguments_are_not_authoritative_until_a_terminal_event() {
        // `output_item.added` may hint at arguments but is not a terminal
        // snapshot; without deltas or a done event the call fails typed.
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": "{\"a\":1}"}
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
    fn output_item_done_seals_the_slot_against_reopening() {
        // `output_item.done` seals the slot; a later `output_item.added` for
        // the same index is a reopened call and must fail closed.
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": "{\"a\":1}"}
            }))
            .unwrap();
        let error = accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_2", "name": "fs_write", "arguments": ""}
            }))
            .unwrap_err();
        assert!(
            error.to_string().contains("re-opened a sealed call"),
            "{error}"
        );
    }

    #[test]
    fn arguments_done_name_conflict_is_rejected_not_ignored() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
            }))
            .unwrap();
        let error = accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "item_id": "fc_1",
                "name": "fs_write",
                "arguments": "{\"path\":\"README.md\"}"
            }))
            .unwrap_err();
        assert!(
            error.to_string().contains("bound name `fs_write`"),
            "{error}"
        );
    }

    #[test]
    fn assembled_deltas_that_contradict_the_terminal_snapshot_fail_closed() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": "{\"a\":1}"
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "item_id": "fc_1",
                "arguments": "{\"b\":2}"
            }))
            .unwrap();
        let error = accumulator.finalize().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differs from its terminal arguments"),
            "{error}"
        );
    }

    #[test]
    fn conflicting_terminal_snapshots_are_rejected_not_silently_overridden() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "item_id": "fc_1",
                "name": "fs_read",
                "arguments": "{\"a\":1}"
            }))
            .unwrap();
        let error = accumulator
            .apply(&json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": "{\"b\":2}"}
            }))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflict with the earlier terminal snapshot"),
            "{error}"
        );
    }

    #[test]
    fn identical_terminal_snapshots_are_idempotent_across_event_kinds() {
        // `arguments.done` then an `output_item.done` carrying the same full
        // arguments is a duplicate authoritative snapshot, not a conflict.
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "item_id": "fc_1",
                "name": "fs_read",
                "arguments": "{}"
            }))
            .unwrap();
        accumulator
            .apply(&json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": "{}"}
            }))
            .unwrap();
        let (_, calls, _) = accumulator.finalize().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn finalize_refuses_missing_name_without_synthesizing_one() {
        // Only an arguments delta arrived, so the slot has an item id but no
        // function name; finalize must not invent an empty name.
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": "{\"a\":1}"
            }))
            .unwrap();
        let error = accumulator.finalize().unwrap_err();
        assert!(error.to_string().contains("no function name"), "{error}");
    }

    #[test]
    fn one_call_id_bound_to_two_indexes_is_rejected() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "fs_read", "arguments": ""}
            }))
            .unwrap();
        let error = accumulator
            .apply(&json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "function_call", "call_id": "call_1", "name": "probe", "arguments": ""}
            }))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already the call id of output index 0"),
            "{error}"
        );
    }

    #[test]
    fn one_item_id_bound_to_two_indexes_is_rejected() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": "{\"a\":1}"
            }))
            .unwrap();
        let error = accumulator
            .apply(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 1,
                "item_id": "fc_1",
                "delta": "{\"b\":2}"
            }))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already the item id of output index 0"),
            "{error}"
        );
    }
}
