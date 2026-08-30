//! Pure OpenAI-compatible streaming-wire parsing.
//!
//! Kept free of I/O so the mapping from wire chunks to `ModelChunk` events is
//! unit-testable without a network.

use agent_contracts::{
    AgentError, AgentResult, ModelChunk, ModelProtocolErrorKind, ModelUsage, ToolCall,
};
use serde::Deserialize;
use serde_json::Value;

/// One complete SSE event at a blank-line boundary. Multi-line `data:`
/// payloads are joined with `\n` per the SSE specification; `event` is
/// `None` when the stream declared no event name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Byte-bounded SSE event framer. Consumes raw stream lines and emits one
/// event when the blank-line boundary arrives, joining standard multi-`data:`
/// events exactly once. Comment, `id:` and unknown fields are ignored; the
/// joined payload of a single event cannot grow past `max_event_bytes` while
/// waiting for the boundary, so a hostile or broken provider cannot make the
/// accumulator unbounded.
#[derive(Debug)]
pub struct SseEventFramer {
    data_lines: Vec<String>,
    data_bytes: usize,
    event_type: Option<String>,
    max_event_bytes: usize,
}

impl SseEventFramer {
    /// A new framer. `max_event_bytes` bounds one joined event payload.
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            data_lines: Vec::new(),
            data_bytes: 0,
            event_type: None,
            max_event_bytes: max_event_bytes.max(1),
        }
    }

    /// Feed one raw stream line. Returns the completed event when the line is
    /// the blank boundary, otherwise `None`. An event whose joined payload
    /// exceeds the cap is an error: the stream is already consumed past a
    /// point a replay could not restore.
    pub fn push_line(&mut self, line: &str) -> Result<Option<SseEvent>, String> {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            return Ok(self.flush_event());
        }
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if !payload.is_empty() {
                self.data_bytes = self.data_bytes.saturating_add(payload.len());
                if self.data_bytes > self.max_event_bytes {
                    return Err(format!(
                        "SSE event exceeded the {} byte cap before the blank-line boundary",
                        self.max_event_bytes
                    ));
                }
                self.data_lines.push(payload.to_string());
            }
        } else if let Some(event) = line.strip_prefix("event:") {
            self.event_type = Some(event.trim().to_string());
        }
        // Comment/`id:`/`retry:` and unknown fields carry no event payload.
        Ok(None)
    }

    /// Flush a residual event when the stream ends without a blank line
    /// (most providers still terminate the final event with a blank line).
    pub fn finish(&mut self) -> Option<SseEvent> {
        self.flush_event()
    }

    fn flush_event(&mut self) -> Option<SseEvent> {
        if self.data_lines.is_empty() {
            self.event_type = None;
            return None;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        self.data_bytes = 0;
        Some(SseEvent {
            event: self.event_type.take(),
            data,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct WireChunk {
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
    #[serde(default)]
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub struct WireChoice {
    #[serde(default)]
    pub delta: WireDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireError {
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WireDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Debug, Deserialize)]
pub struct WireToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireFunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WireFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub struct WirePromptTokensDetails {
    pub cached_tokens: Option<u64>,
}

fn protocol_event_error(message: impl Into<String>) -> AgentError {
    AgentError::ModelProtocol {
        kind: ModelProtocolErrorKind::MalformedEvent,
        message: message.into(),
    }
}

/// Parse one Chat Completions SSE payload.
///
/// A syntactically valid object that does not carry any Chat Completions
/// fields is an extension event and may be ignored. Once an event claims a
/// known field, however, it must satisfy that field's wire shape. This keeps
/// forward compatibility without turning damaged response bytes into a
/// partial success.
pub fn parse_wire_chunk(payload: &str) -> AgentResult<Option<WireChunk>> {
    let value: Value =
        serde_json::from_str(payload).map_err(|error| AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedEvent,
            message: format!("malformed Chat Completions SSE JSON: {error}"),
        })?;
    let Some(object) = value.as_object() else {
        return Err(AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedEvent,
            message: "Chat Completions SSE data must be a JSON object".into(),
        });
    };
    if !["choices", "usage", "error"]
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedEvent,
            message: format!("invalid Chat Completions SSE event shape: {error}"),
        })
}

#[derive(Debug, Default)]
struct AccToolCall {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Accumulates streamed deltas into the final response shape.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    content: String,
    tool_calls: Vec<AccToolCall>,
    pub usage: Option<ModelUsage>,
    terminal_error: Option<String>,
    sealed: bool,
}

impl StreamAccumulator {
    /// Apply one wire chunk, returning the normalized `ModelChunk` events that
    /// should be forwarded to the sink (text deltas and tool-call deltas).
    /// Identity contradictions and deltas after the terminal chunk are typed
    /// protocol errors; the accumulator never silently rewrites a bound call.
    pub fn apply(&mut self, chunk: &WireChunk) -> AgentResult<Vec<ModelChunk>> {
        let mut events = Vec::new();

        if let Some(error) = &chunk.error {
            self.terminal_error = Some(
                error
                    .message
                    .clone()
                    .unwrap_or_else(|| "provider stream error".to_string()),
            );
        }

        if let Some(usage) = &chunk.usage {
            self.usage = Some(ModelUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cached_input_tokens: usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens),
                ..Default::default()
            });
        }

        for choice in &chunk.choices {
            if choice.finish_reason.as_deref() == Some("network_error") {
                self.terminal_error = Some("provider reported finish_reason=network_error".into());
            }
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                if self.sealed {
                    return Err(protocol_event_error(
                        "Chat Completions text delta arrived after the terminal chunk",
                    ));
                }
                self.content.push_str(content);
                events.push(ModelChunk::TextDelta {
                    delta: content.clone(),
                });
            }

            for delta in &choice.delta.tool_calls {
                if self.sealed {
                    return Err(protocol_event_error(format!(
                        "Chat Completions tool-call delta for index {} arrived after the terminal chunk",
                        delta.index
                    )));
                }
                if !self.tool_calls.iter().any(|slot| slot.index == delta.index) {
                    self.tool_calls.push(AccToolCall {
                        index: delta.index,
                        id: None,
                        name: None,
                        arguments: String::new(),
                    });
                }
                let slot = self
                    .tool_calls
                    .iter_mut()
                    .find(|slot| slot.index == delta.index)
                    .expect("slot was just ensured");

                if let Some(id) = &delta.id {
                    if slot.id.as_deref().is_some_and(|bound| bound != id) {
                        return Err(protocol_event_error(format!(
                            "Chat Completions tool call at index {} bound id `{id}` but is already `{}`",
                            delta.index,
                            slot.id.as_deref().unwrap_or_default()
                        )));
                    }
                    if slot.id.is_none() {
                        slot.id = Some(id.clone());
                    }
                }
                if let Some(name) = delta.function.as_ref().and_then(|f| f.name.clone()) {
                    if slot
                        .name
                        .as_deref()
                        .is_some_and(|bound| bound != name.as_str())
                    {
                        return Err(protocol_event_error(format!(
                            "Chat Completions tool call at index {} bound name `{name}` but is already `{}`",
                            delta.index,
                            slot.name.as_deref().unwrap_or_default()
                        )));
                    }
                    if slot.name.is_none() {
                        slot.name = Some(name);
                    }
                }
                if let Some(arguments) = delta.function.as_ref().and_then(|f| f.arguments.clone()) {
                    let first_args = slot.arguments.is_empty();
                    slot.arguments.push_str(&arguments);
                    events.push(ModelChunk::ToolCallDelta {
                        call_id: slot
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call-{}", slot.index)),
                        name: if first_args { slot.name.clone() } else { None },
                        arguments_delta: arguments,
                    });
                }
            }
        }

        // Any terminal chunk seals the stream so a deltas-after-done stream
        // fails closed instead of appending onto a finished tool call.
        if !self.sealed
            && chunk
                .choices
                .iter()
                .any(|choice| choice.finish_reason.is_some())
        {
            self.sealed = true;
        }

        Ok(events)
    }

    pub fn take_terminal_error(&mut self) -> Option<String> {
        self.terminal_error.take()
    }

    /// Finalize into `(content, tool_calls)` and fail closed when a streamed
    /// function-call argument never became complete JSON.
    pub fn finalize(self) -> AgentResult<(String, Vec<ToolCall>)> {
        let mut tool_calls = Vec::new();
        for slot in self.tool_calls {
            let id = slot.id.unwrap_or_else(|| format!("call-{}", slot.index));
            let name = slot.name.unwrap_or_default();
            let arguments: Value = serde_json::from_str(&slot.arguments).map_err(|error| {
                AgentError::ModelProtocol {
                    kind: ModelProtocolErrorKind::MalformedToolCall,
                    message: format!(
                        "Chat Completions tool call at index {} has incomplete or invalid arguments: {error}",
                        slot.index
                    ),
                }
            })?;
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok((self.content, tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ModelChunk;

    #[test]
    fn framer_joins_multi_data_events_at_the_blank_line() {
        let mut framer = SseEventFramer::new(4096);
        assert_eq!(framer.push_line("data: {\"a\":1}"), Ok(None));
        assert_eq!(framer.push_line("data: {\"b\":2}\r"), Ok(None));
        let event = framer.push_line("").unwrap().expect("blank boundary");
        assert_eq!(event.event, None);
        assert_eq!(event.data, "{\"a\":1}\n{\"b\":2}");
    }

    #[test]
    fn framer_keeps_declared_event_names_and_ignores_comments_and_ids() {
        let mut framer = SseEventFramer::new(4096);
        assert_eq!(framer.push_line(": keep-alive"), Ok(None));
        assert_eq!(framer.push_line("event: response.custom"), Ok(None));
        assert_eq!(framer.push_line("id: 7"), Ok(None));
        assert_eq!(framer.push_line("data: payload"), Ok(None));
        let event = framer.push_line("").unwrap().expect("blank boundary");
        assert_eq!(event.event.as_deref(), Some("response.custom"));
        assert_eq!(event.data, "payload");
    }

    #[test]
    fn framer_emits_nothing_for_empty_events_and_flushes_at_eof() {
        let mut framer = SseEventFramer::new(4096);
        assert_eq!(framer.push_line("event: ping"), Ok(None));
        assert_eq!(framer.push_line(""), Ok(None), "no data line, no event");
        assert_eq!(framer.push_line("data: [DONE]"), Ok(None));
        let done = framer.finish().expect("eof flushes the residual event");
        assert_eq!(done.data, "[DONE]");
        assert_eq!(framer.finish(), None, "a second flush has nothing left");
    }

    #[test]
    fn framer_bounds_one_event_before_the_blank_line() {
        let mut framer = SseEventFramer::new(16);
        assert_eq!(framer.push_line("data: 0123456789abcde"), Ok(None));
        let error = framer
            .push_line("data: FF")
            .expect_err("joining past the cap must fail closed");
        assert!(error.contains("byte cap"), "{error}");
    }

    #[test]
    fn accumulates_text_and_tool_calls() {
        let mut acc = StreamAccumulator::default();

        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#,
        )
        .unwrap();
        let events = acc.apply(&chunk).unwrap();
        assert_eq!(
            events,
            vec![ModelChunk::TextDelta {
                delta: "Hel".into()
            }]
        );

        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
        )
        .unwrap();
        let events = acc.apply(&chunk).unwrap();
        assert_eq!(events, vec![ModelChunk::TextDelta { delta: "lo".into() }]);

        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"fs.read","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
        )
        .unwrap();
        let events = acc.apply(&chunk).unwrap();
        assert_eq!(
            events,
            vec![ModelChunk::ToolCallDelta {
                call_id: "call_1".into(),
                name: Some("fs.read".into()),
                arguments_delta: "{\"path\":".into(),
            }]
        );

        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"AuthService.rs\"}"}}]},"finish_reason":null}]}"#,
        )
        .unwrap();
        let events = acc.apply(&chunk).unwrap();
        assert_eq!(
            events,
            vec![ModelChunk::ToolCallDelta {
                call_id: "call_1".into(),
                name: None,
                arguments_delta: "\"AuthService.rs\"}".into(),
            }]
        );

        let (content, calls) = acc.finalize().unwrap();
        assert_eq!(content, "Hello");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "fs.read");
        assert_eq!(
            calls[0].arguments,
            serde_json::json!({"path": "AuthService.rs"})
        );
    }

    #[test]
    fn captures_usage_from_stream() {
        let mut acc = StreamAccumulator::default();
        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":120,"completion_tokens":45,"total_tokens":165,"prompt_tokens_details":{"cached_tokens":30}}}"#,
        )
        .unwrap();
        acc.apply(&chunk).unwrap();
        let usage = acc.usage.expect("usage should be captured");
        assert_eq!(usage.input_tokens, Some(120));
        assert_eq!(usage.output_tokens, Some(45));
        assert_eq!(usage.cached_input_tokens, Some(30));
    }

    #[test]
    fn malformed_tool_arguments_fail_closed() {
        let mut acc = StreamAccumulator::default();
        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"fs.read","arguments":"not json"}}]}}]}"#,
        )
        .unwrap();
        acc.apply(&chunk).unwrap();
        let error = acc.finalize().unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                ..
            }
        ));
    }

    #[test]
    fn malformed_data_and_known_shapes_fail_but_extensions_are_ignored() {
        let error = parse_wire_chunk(r#"{"choices": [}"#).unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));

        let error = parse_wire_chunk(r#"{"choices":"not-an-array"}"#).unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));

        assert!(
            parse_wire_chunk(r#"{"type":"provider.keepalive","sequence":7}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn provider_network_error_is_not_an_empty_success() {
        let mut acc = StreamAccumulator::default();
        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"network_error"}]}"#,
        )
        .unwrap();
        assert!(acc.apply(&chunk).unwrap().is_empty());
        assert_eq!(
            acc.take_terminal_error().as_deref(),
            Some("provider reported finish_reason=network_error")
        );
    }

    #[test]
    fn deltas_after_the_terminal_chunk_are_rejected() {
        let mut acc = StreamAccumulator::default();
        let terminal: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        acc.apply(&terminal).unwrap();
        let late: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"fs.read","arguments":"{}"}}]}}]}"#,
        )
        .unwrap();
        let error = acc.apply(&late).unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));
        assert!(
            error.to_string().contains("after the terminal chunk"),
            "{error}"
        );
    }

    #[test]
    fn identity_contradictions_in_tool_call_deltas_are_rejected() {
        let mut acc = StreamAccumulator::default();
        let first: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"fs.read","arguments":""}}]}}]}"#,
        )
        .unwrap();
        acc.apply(&first).unwrap();
        let conflicting_id: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"arguments":"{}"}}]}}]}"#,
        )
        .unwrap();
        let error = acc.apply(&conflicting_id).unwrap_err();
        assert!(error.to_string().contains("bound id `c2`"), "{error}");

        let mut acc = StreamAccumulator::default();
        acc.apply(&first).unwrap();
        let conflicting_name: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"fs_write","arguments":"{}"}}]}}]}"#,
        )
        .unwrap();
        let error = acc.apply(&conflicting_name).unwrap_err();
        assert!(
            error.to_string().contains("bound name `fs_write`"),
            "{error}"
        );
    }
}
