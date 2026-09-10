//! OpenAI Chat Completions 函数名编解码。
//!
//! 上游要求 `tools[].function.name` 匹配 `^[a-zA-Z0-9_-]+$`。Core 工具 id
//! 允许 `.` 和 `:`（例如 `fs.list`）。本模块只在 provider 出网时把这两类
//! 字符换成 `_`，回包再还原成 Core id；内核工具名不变。
//!
//! 两个不同的 Core id 若落到同一个线名（`fs.list` 与 `fs_list`），在发请求
//! 之前 fail-closed，避免模型调用被派发到错误的工具。
//!
//! History tool calls may hold raw wire names recorded while the tool was not
//! yet exposed (e.g. `fs_mkdir` for `fs.mkdir`). Those are not Core ids: once
//! the spec is exposed its mapping is authoritative and the raw name is that
//! tool's wire form, so such collisions never fail the request.

use std::collections::HashMap;

use agent_contracts::{AgentError, AgentResult, ModelChunk, ModelRequest, ToolCall};

/// `.` 与 `:` 换成 `_`，得到 OpenAI 可接受的函数名。
pub(crate) fn to_wire_tool_name(name: &str) -> String {
    name.replace(['.', ':'], "_")
}

/// 一次请求内的线名 ↔ Core id 对照。
#[derive(Debug, Default)]
pub(crate) struct ToolNameCodec {
    /// 线名 → 原始 Core id。线名已合法时是恒等映射。
    from_wire: HashMap<String, String>,
}

impl ToolNameCodec {
    pub(crate) fn from_request(request: &ModelRequest) -> AgentResult<Self> {
        let mut from_wire = HashMap::new();
        // Tool specs are authoritative: two exposed tool ids that collide on
        // one wire name fail closed before the request leaves, because the
        // id the model means would be ambiguous.
        for original in request.tools.iter().map(|tool| tool.name.as_str()) {
            insert_spec_mapping(&mut from_wire, original)?;
        }
        // History may hold raw wire names recorded when the tool was not yet
        // exposed (e.g. `fs_mkdir` for `fs.mkdir`). Such a name is the wire
        // form of the exposed spec, so the spec mapping already present wins
        // and the name is skipped instead of failing the request. A history
        // name with no matching spec keeps its identity mapping so a later
        // call to it can still be decoded.
        for original in request
            .messages
            .iter()
            .flat_map(|message| message.tool_calls.iter().map(|call| call.name.as_str()))
        {
            from_wire
                .entry(to_wire_tool_name(original))
                .or_insert_with(|| original.to_string());
        }
        Ok(Self { from_wire })
    }

    pub(crate) fn to_wire(&self, name: &str) -> String {
        to_wire_tool_name(name)
    }

    pub(crate) fn decode_wire_name(&self, wire: &str) -> String {
        self.from_wire
            .get(wire)
            .cloned()
            .unwrap_or_else(|| wire.to_string())
    }

    pub(crate) fn remap_chunk(&self, chunk: ModelChunk) -> ModelChunk {
        match chunk {
            ModelChunk::ToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => ModelChunk::ToolCallDelta {
                call_id,
                name: name.map(|wire| self.decode_wire_name(&wire)),
                arguments_delta,
            },
            other => other,
        }
    }

    pub(crate) fn remap_calls(&self, calls: Vec<ToolCall>) -> Vec<ToolCall> {
        calls
            .into_iter()
            .map(|mut call| {
                call.name = self.decode_wire_name(&call.name);
                call
            })
            .collect()
    }
}

fn insert_spec_mapping(from_wire: &mut HashMap<String, String>, original: &str) -> AgentResult<()> {
    let wire = to_wire_tool_name(original);
    if let Some(existing) = from_wire.get(&wire) {
        if existing != original {
            return Err(AgentError::InvalidRequest(format!(
                "tool names '{existing}' and '{original}' both serialize to '{wire}'; OpenAI-compatible function names cannot contain '.' or ':'"
            )));
        }
        return Ok(());
    }
    from_wire.insert(wire, original.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ModelMessage, ToolSpec};
    use serde_json::json;

    fn request_with_tools(names: &[&str]) -> ModelRequest {
        ModelRequest {
            messages: vec![ModelMessage::user("hi")],
            tools: names
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).into(),
                    description: "probe".into(),
                    input_schema: json!({"type": "object"}),
                    risk: agent_contracts::ToolRisk::ReadOnly,
                    output_budget: None,
                    roles: Vec::new(),
                })
                .collect(),
            metadata: json!({}),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn dotted_and_colon_names_become_underscores() {
        assert_eq!(to_wire_tool_name("fs.list"), "fs_list");
        assert_eq!(to_wire_tool_name("mcp:tool"), "mcp_tool");
        assert_eq!(to_wire_tool_name("get_time"), "get_time");
    }

    #[test]
    fn codec_restores_core_ids_and_passes_unknown_wire_names() {
        let codec = ToolNameCodec::from_request(&request_with_tools(&["fs.list", "get_time"]))
            .expect("no collision");
        assert_eq!(codec.to_wire("fs.list"), "fs_list");
        assert_eq!(codec.decode_wire_name("fs_list"), "fs.list");
        assert_eq!(codec.decode_wire_name("get_time"), "get_time");
        assert_eq!(codec.decode_wire_name("hallucinated"), "hallucinated");
    }

    #[test]
    fn colliding_core_ids_fail_closed() {
        let error = ToolNameCodec::from_request(&request_with_tools(&["fs.list", "fs_list"]))
            .expect_err("collision must refuse the request");
        let text = error.to_string();
        assert!(text.contains("fs.list"), "{text}");
        assert!(text.contains("fs_list"), "{text}");
    }

    #[test]
    fn history_tool_calls_join_the_reverse_map() {
        let request = ModelRequest {
            messages: vec![ModelMessage::assistant_tool_calls(vec![
                agent_contracts::ToolCall {
                    id: "call-1".into(),
                    name: "edit.replace".into(),
                    arguments: json!({}),
                },
            ])],
            tools: Vec::new(),
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };
        let codec = ToolNameCodec::from_request(&request).expect("no collision");
        assert_eq!(codec.decode_wire_name("edit_replace"), "edit.replace");
    }

    #[test]
    fn raw_history_wire_name_is_decoded_once_its_spec_is_exposed() {
        // Regression: a model called `fs_mkdir` while `fs.mkdir` was not yet
        // exposed; the raw wire name stayed in history. Once `fs.mkdir` is
        // exposed the request carries both, and the spec mapping must win
        // instead of failing the request.
        let request = ModelRequest {
            messages: vec![ModelMessage::assistant_tool_calls(vec![
                agent_contracts::ToolCall {
                    id: "call-raw".into(),
                    name: "fs_mkdir".into(),
                    arguments: json!({"path": "tests"}),
                },
            ])],
            tools: vec![ToolSpec {
                name: "fs.mkdir".into(),
                description: "create one workspace directory".into(),
                input_schema: json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            }],
            metadata: json!({}),
            cancel: CancellationToken::new(),
        };
        let codec = ToolNameCodec::from_request(&request)
            .expect("a raw history wire name must not fail the request");
        assert_eq!(codec.decode_wire_name("fs_mkdir"), "fs.mkdir");
        assert_eq!(codec.to_wire("fs.mkdir"), "fs_mkdir");
    }

    #[test]
    fn streaming_delta_names_are_restored() {
        let codec =
            ToolNameCodec::from_request(&request_with_tools(&["fs.read"])).expect("no collision");
        let remapped = codec.remap_chunk(ModelChunk::ToolCallDelta {
            call_id: "call_1".into(),
            name: Some("fs_read".into()),
            arguments_delta: "{\"path\":".into(),
        });
        assert_eq!(
            remapped,
            ModelChunk::ToolCallDelta {
                call_id: "call_1".into(),
                name: Some("fs.read".into()),
                arguments_delta: "{\"path\":".into(),
            }
        );
    }
}
