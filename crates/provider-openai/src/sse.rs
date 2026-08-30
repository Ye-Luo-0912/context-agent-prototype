//! Pure OpenAI-compatible streaming-wire parsing.
//!
//! Kept free of I/O so the mapping from wire chunks to `ModelChunk` events is
//! unit-testable without a network.

use agent_contracts::{
    AgentError, AgentResult, ModelChunk, ModelProtocolErrorKind, ModelUsage, ToolCall,
};
use serde::Deserialize;
use serde_json::Value;

/// Extract the payload of an SSE `data:` line. Returns `None` for comment
/// lines, event lines, and blanks. The payload is trimmed (including a
/// trailing `\r` for `\r\n` line endings).
pub fn parse_sse_data(line: &str) -> Option<&str> {
    let line = line.trim_end_matches('\r');
    line.strip_prefix("data:")
        .map(str::trim)
        .filter(|payload| !payload.is_empty())
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
}

impl StreamAccumulator {
    /// Apply one wire chunk, returning the normalized `ModelChunk` events that
    /// should be forwarded to the sink (text deltas and tool-call deltas).
    pub fn apply(&mut self, chunk: &WireChunk) -> Vec<ModelChunk> {
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
                self.content.push_str(content);
                events.push(ModelChunk::TextDelta {
                    delta: content.clone(),
                });
            }

            for delta in &choice.delta.tool_calls {
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

                if let Some(id) = &delta.id
                    && slot.id.is_none()
                {
                    slot.id = Some(id.clone());
                }
                if let Some(name) = delta.function.as_ref().and_then(|f| f.name.clone())
                    && slot.name.is_none()
                {
                    slot.name = Some(name);
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

        events
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
    fn parses_sse_data_lines() {
        assert_eq!(parse_sse_data("data: hello world"), Some("hello world"));
        assert_eq!(parse_sse_data("data: [DONE]"), Some("[DONE]"));
        assert_eq!(parse_sse_data("data: hello\r"), Some("hello"));
        assert_eq!(parse_sse_data("data:"), None);
        assert_eq!(parse_sse_data(": keep-alive comment"), None);
        assert_eq!(parse_sse_data("event: message"), None);
        assert_eq!(parse_sse_data(""), None);
    }

    #[test]
    fn accumulates_text_and_tool_calls() {
        let mut acc = StreamAccumulator::default();

        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#,
        )
        .unwrap();
        let events = acc.apply(&chunk);
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
        let events = acc.apply(&chunk);
        assert_eq!(events, vec![ModelChunk::TextDelta { delta: "lo".into() }]);

        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"fs.read","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
        )
        .unwrap();
        let events = acc.apply(&chunk);
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
        let events = acc.apply(&chunk);
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
        acc.apply(&chunk);
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
        acc.apply(&chunk);
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
        assert!(acc.apply(&chunk).is_empty());
        assert_eq!(
            acc.take_terminal_error().as_deref(),
            Some("provider reported finish_reason=network_error")
        );
    }
}
