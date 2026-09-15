//! T6/R6 + V6 ENDPOINT REQUEST-SHAPE FIXTURES — the official Responses
//! explicit-caching request shape versus what the mapper emits.
//!
//! The official prompt-caching documentation
//! (<https://developers.openai.com/api/docs/guides/prompt-caching>) defines
//! the Responses explicit mode as:
//! - a top-level `"prompt_cache_options": {"mode": "explicit"}` field; and
//! - `prompt_cache_breakpoint: {"mode": "explicit"}` placed ON A SUPPORTED
//!   CONTENT BLOCK inside an input message — e.g. inside
//!   `content: [{"type": "input_text", "text": ..., "prompt_cache_breakpoint":
//!   {"mode": "explicit"}}]`, and for `function_call_output` items inside a
//!   content-block `output` array whose blocks are INPUT blocks (the guide's
//!   multi-turn agent example uses `input_text`, because the tool result is
//!   the next request's input). Plain string content cannot host a
//!   breakpoint, the top-level `instructions` field cannot host one, and the
//!   documented shape has NO input-item sibling form.
//!
//! This module records REQUEST SHAPE only. The local capture servers in the
//! acceptance suites collect JSON without validating the provider schema, so
//! neither this file nor those suites have confirmed real endpoint behavior.
//! Since S2b/V6 the mapper's declared per-item breakpoint form follows the
//! documented placement through ONE typed mapping (`place_declared_breakpoint`
//! over `BREAKPOINT_BLOCK_TYPES`); a declared hint whose item cannot legally
//! host it is DROPPED with a recorded outcome, never reshaped into an
//! undocumented field. Real-endpoint verification of the breakpoint encoding
//! is T8's conditional task.

use super::*;
use crate::{
    BREAKPOINT_BLOCK_TYPES, DeclaredBreakpointPlacement, OpenAiConfig,
    build_responses_wire_request, place_declared_breakpoint, wire_names::ToolNameCodec,
};
use agent_contracts::{ModelInput, ModelMessage, ModelRequest, PromptLayout, ToolCall, TurnFrame};
use serde_json::{Value, json};
use std::time::Duration;

/// The official documented placement, transcribed as a fixture: the
/// breakpoint is a field OF a supported content block inside `content`.
fn official_documented_shape() -> Value {
    json!({
        "model": "documented-model",
        "input": [
            {
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": "stable instructions",
                    "prompt_cache_breakpoint": {"mode": "explicit"},
                }],
            },
        ],
        "prompt_cache_options": {"mode": "explicit"},
    })
}

/// The fixture itself stays honest: every breakpoint sits inside a content
/// block, no input item carries a sibling breakpoint, and no breakpoint
/// rides on plain string content (which the official shape cannot express).
#[test]
fn official_fixture_places_breakpoints_on_content_blocks_only() {
    let shape = official_documented_shape();
    for item in shape["input"].as_array().unwrap() {
        assert!(
            item.get("prompt_cache_breakpoint").is_none(),
            "the documented shape never puts the breakpoint on the input item: {item}"
        );
        let content = item["content"]
            .as_array()
            .expect("documented content blocks");
        for block in content {
            assert!(
                block.get("type").and_then(Value::as_str) == Some("input_text")
                    && block.get("prompt_cache_breakpoint").is_some(),
                "the breakpoint belongs to the supported content block: {block}"
            );
        }
    }
}

fn responses_config() -> OpenAiConfig {
    OpenAiConfig {
        api_key: "not-a-secret".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "unknown-model-alias".into(),
        protocol: OpenAiProtocol::Responses,
        max_output_tokens: 128,
        timeout: Duration::from_secs(5),
        send_stream_options: false,
        send_max_tokens: false,
        max_stream_bytes: 4096,
        context_window: None,
        sampling: crate::SamplingPolicy::ProviderDefault,
    }
}

fn mapped(request: &ModelRequest) -> Value {
    build_responses_wire_request(
        request,
        &responses_config(),
        &ToolNameCodec::from_request(request).unwrap(),
        OpenAiPromptCacheMode::ResponsesExplicit,
    )
}

fn boundary_request() -> ModelRequest {
    // A request with a VALID legacy reuse boundary, bound by `into_request`
    // itself: the stable prefix is system policy + evidence (2 messages).
    ModelInput {
        layout: PromptLayout::CurrentStateLast,
        system_policy: vec![ModelMessage::system("policy")],
        context_frame: vec![ModelMessage::user("stable evidence")],
        focus_frame: Some("changing goal".into()),
        turn_frame: TurnFrame::new("full directive"),
        ..Default::default()
    }
    .into_request(json!({}), Default::default())
}

/// The legacy whole-boundary hint ALREADY matches the official placement:
/// the boundary message's content is rewritten into an `input_text` block
/// and the breakpoint rides inside that block — never on the item.
#[test]
fn legacy_boundary_hint_matches_the_official_content_block_shape() {
    let request = boundary_request();
    assert!(request.prompt_reuse_boundary().is_some());
    let wire = mapped(&request);
    let boundary_item = &wire["input"][1];
    assert!(
        boundary_item.get("prompt_cache_breakpoint").is_none(),
        "no sibling breakpoint on the boundary item: {boundary_item}"
    );
    assert_eq!(
        boundary_item["content"][0]["type"], "input_text",
        "string content is rewritten into a content block"
    );
    assert_eq!(
        boundary_item["content"][0]["prompt_cache_breakpoint"],
        json!({"mode": "explicit"}),
        "the breakpoint is a field of the content block — the documented placement"
    );
}

/// S2b: a DECLARED breakpoint on a message with plain string content is
/// rewritten into the official content-block form — the string becomes an
/// `input_text` block carrying the breakpoint, matching the documented
/// placement (no input-item sibling form).
#[test]
fn declared_breakpoint_on_string_content_matches_the_official_shape() {
    let mut request = boundary_request();
    // No valid boundary is needed for the declared path; drop the hint so
    // the message content stays a plain string.
    request.metadata = json!({});
    assert!(request.prompt_reuse_boundary().is_none());
    request.cache_breakpoints = vec![1];
    let wire = mapped(&request);
    let item = &wire["input"][1];
    assert!(
        item.get("prompt_cache_breakpoint").is_none(),
        "no sibling breakpoint on the input item: {item}"
    );
    let content = item["content"]
        .as_array()
        .expect("content is a block array");
    assert_eq!(
        content[0]["type"], "input_text",
        "string content is rewritten into a content block"
    );
    assert_eq!(
        content[0]["prompt_cache_breakpoint"],
        json!({"mode": "explicit"}),
        "the breakpoint is a field of the content block — the documented placement"
    );
}

/// V6: a DECLARED breakpoint on a tool result item must land on the
/// supported INPUT block type. The official guide's multi-turn agent
/// example wraps `function_call_output` results as
/// `output: [{"type": "input_text", "text": ..., "prompt_cache_breakpoint":
/// ...}]` — the tool result is the NEXT request's input, so the block is an
/// `input_text` (never an `output_text`, which is an assistant-output block
/// type and not a documented breakpoint host).
#[test]
fn declared_breakpoint_on_tool_output_uses_a_supported_input_block() {
    let mut request = boundary_request();
    request.metadata = json!({});
    request
        .messages
        .push(ModelMessage::tool_result("call-1", "fs.read", "tool body"));
    let tool_message_index = request.messages.len() - 1;
    request.cache_breakpoints = vec![tool_message_index];
    let wire = mapped(&request);
    let last = wire["input"].as_array().unwrap().last().unwrap();
    assert_eq!(last["type"], "function_call_output");
    let output_blocks = last["output"]
        .as_array()
        .expect("tool output is a block array");
    assert_eq!(
        output_blocks.len(),
        1,
        "the plain-string tool result is rewritten into exactly one block"
    );
    assert_eq!(
        output_blocks[0]["type"], "input_text",
        "the tool result block type must be the officially supported input block: {}",
        output_blocks[0]
    );
    assert_eq!(
        output_blocks[0]["text"], "tool body",
        "the tool result body survives the rewrite verbatim"
    );
    assert_eq!(
        output_blocks[0]["prompt_cache_breakpoint"],
        json!({"mode": "explicit"}),
        "the breakpoint is a field of the supported content block"
    );
    assert!(
        last.get("prompt_cache_breakpoint").is_none(),
        "no sibling breakpoint on the function_call_output item itself"
    );
}

/// Recursive scan over the whole wire body: every `prompt_cache_breakpoint`
/// field must sit ON a content block whose `type` is one of the officially
/// supported hosts (`input_text`/`input_image`/`input_file`) — never on an
/// input item, a bare string, or any other unconfirmed position.
fn assert_breakpoints_only_on_supported_blocks(value: &Value) {
    match value {
        Value::Object(map) => {
            for (name, nested) in map {
                if name == "prompt_cache_breakpoint" {
                    assert_eq!(
                        map.get("type").and_then(Value::as_str),
                        Some("input_text"),
                        "a breakpoint field must ride on a supported input block, got {value}"
                    );
                    assert_eq!(
                        nested,
                        &json!({"mode": "explicit"}),
                        "the breakpoint value is the documented explicit mode"
                    );
                } else {
                    assert_breakpoints_only_on_supported_blocks(nested);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_breakpoints_only_on_supported_blocks(item);
            }
        }
        _ => {}
    }
}

/// A minimal request whose flat message list and breakpoints are fully
/// controlled by the test — no packing-layer tail, so wire indexes under
/// empty-message filtering and tool-call expansion are exactly the ones
/// the test writes down.
fn declared_only_request(messages: Vec<ModelMessage>, breakpoints: Vec<usize>) -> ModelRequest {
    ModelRequest {
        messages,
        cache_breakpoints: breakpoints,
        ..Default::default()
    }
}

/// V6: a declared breakpoint whose message cannot legally host one — an
/// assistant message whose LAST wire item is an expanded `function_call`
/// (tool-call items carry neither `content` nor `output`, and no documented
/// form puts a breakpoint on them) — is DROPPED, never reshaped into the
/// undocumented input-item sibling field. The following message keeps its
/// own breakpoint on its OWN item: expansion shifts wire indexes, never
/// breakpoint ownership (the last-non-empty declared index stays on the
/// message that produced it).
#[test]
fn unhostable_breakpoint_is_dropped_and_expansion_does_not_shift_ownership() {
    let request = declared_only_request(
        vec![
            ModelMessage::system("policy"),
            ModelMessage::user("evidence"),
            ModelMessage::assistant_tool_calls(vec![
                ToolCall {
                    id: "call-9".into(),
                    name: "fs.list".into(),
                    arguments: json!({"path": ""}),
                },
                ToolCall {
                    id: "call-10".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "a"}),
                },
            ]),
            ModelMessage::user("the next turn after the tool round"),
        ],
        // Both an unhostable (the assistant message) and a hostable (the
        // user message after it) declared breakpoint in ONE request.
        vec![2, 3],
    );
    let wire = mapped(&request);
    let input = wire["input"].as_array().unwrap();
    // Expansion shape: the assistant message became two function_call items.
    assert_eq!(input.len(), 5, "4 messages expand into 5 wire items");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call-9");
    assert_eq!(input[3]["type"], "function_call");
    assert_eq!(input[3]["call_id"], "call-10");
    // The USER message's breakpoint lands on the user item AFTER the two
    // expanded function_call items — its own message, its own item.
    assert_eq!(input[4]["role"], "user");
    assert_eq!(
        input[4]["content"][0]["type"], "input_text",
        "the user message's string content is rewritten into a supported block"
    );
    assert_eq!(
        input[4]["content"][0]["prompt_cache_breakpoint"],
        json!({"mode": "explicit"})
    );
    assert_eq!(
        input[4]["content"][0]["text"], "the next turn after the tool round",
        "the rewritten block carries the message body verbatim"
    );
    // The unhostable assistant breakpoint was dropped — no sibling field on
    // the function_call items, no breakpoint anywhere off a supported block.
    assert!(
        input[2].get("prompt_cache_breakpoint").is_none()
            && input[3].get("prompt_cache_breakpoint").is_none(),
        "expanded function_call items carry no breakpoint sibling field"
    );
    assert_breakpoints_only_on_supported_blocks(&wire);
}

/// V6: an assistant message with BOTH text content and tool calls produces
/// [content item, function_call items]; the declared breakpoint attaches to
/// the message's LAST wire item (the function_call), which cannot host it —
/// so the hint is dropped rather than silently sliding back onto the
/// content item (that would shrink the declared cache boundary).
#[test]
fn breakpoint_on_assistant_with_tool_calls_attaches_to_the_last_wire_item_only() {
    let assistant = ModelMessage {
        content: "assistant narration".into(),
        ..ModelMessage::assistant_tool_calls(vec![ToolCall {
            id: "call-11".into(),
            name: "fs.list".into(),
            arguments: json!({"path": ""}),
        }])
    };
    let request = declared_only_request(
        vec![
            ModelMessage::system("policy"),
            ModelMessage::user("evidence"),
            assistant,
        ],
        vec![2],
    );
    let wire = mapped(&request);
    let input = wire["input"].as_array().unwrap();
    assert_eq!(input.len(), 4, "the assistant message expands to two items");
    assert_eq!(input[2]["role"], "assistant");
    assert_eq!(input[2]["content"], "assistant narration");
    assert_eq!(input[3]["type"], "function_call");
    // The LAST wire item is the function_call: the breakpoint is dropped
    // there (recorded), NOT retro-fitted onto the content item.
    assert!(
        input[2].get("prompt_cache_breakpoint").is_none(),
        "the breakpoint never slides back onto an earlier item of the message"
    );
    assert!(
        input[2]["content"].as_array().is_none(),
        "non-breakpoint content is not rewritten"
    );
    assert!(
        input[3].get("prompt_cache_breakpoint").is_none(),
        "no sibling breakpoint on the expanded function_call item"
    );
    assert_breakpoints_only_on_supported_blocks(&wire);
}

/// V6 seam: the typed placement mapping — every payload shape maps to
/// exactly one outcome, and the drop is a RETURNED record (the builder
/// logs it under a stable reason), never a silent reshape.
#[test]
fn place_declared_breakpoint_maps_every_payload_shape_once() {
    // A bare string becomes exactly one supported input_text block.
    let mut string_payload = json!("tool body");
    assert_eq!(
        place_declared_breakpoint(&mut string_payload),
        DeclaredBreakpointPlacement::PlacedOnContentBlock
    );
    assert_eq!(
        string_payload,
        json!([{
            "type": "input_text",
            "text": "tool body",
            "prompt_cache_breakpoint": {"mode": "explicit"},
        }])
    );

    // A block array hosts the breakpoint on the LAST supported block —
    // every type in the single supported-type list, not just input_text.
    let mut array_payload = json!([
        {"type": "input_text", "text": "first"},
        {"type": "output_text", "text": "not a host"},
        {"type": "input_image", "image_url": "https://example.invalid/x.png"},
    ]);
    assert_eq!(
        place_declared_breakpoint(&mut array_payload),
        DeclaredBreakpointPlacement::PlacedOnContentBlock
    );
    assert!(
        array_payload[2].get("prompt_cache_breakpoint").is_some(),
        "the last supported block (input_image) hosts the breakpoint"
    );
    assert!(
        array_payload[0].get("prompt_cache_breakpoint").is_none()
            && array_payload[1].get("prompt_cache_breakpoint").is_none(),
        "only the last supported block carries it: {array_payload}"
    );

    // An array with NO supported block type is a recorded drop, unchanged.
    let mut unsupported_array = json!([
        {"type": "output_text", "text": "assistant output is not an input host"},
    ]);
    assert_eq!(
        place_declared_breakpoint(&mut unsupported_array),
        DeclaredBreakpointPlacement::DroppedNoSupportedBlock
    );
    assert_eq!(
        unsupported_array,
        json!([{"type": "output_text", "text": "assistant output is not an input host"}]),
        "a dropped hint never mutates the payload"
    );

    // Non-block payloads (numbers/objects/bools) are recorded drops too.
    let mut number_payload = json!(42);
    assert_eq!(
        place_declared_breakpoint(&mut number_payload),
        DeclaredBreakpointPlacement::DroppedNoSupportedBlock
    );
    assert_eq!(number_payload, json!(42));

    // The supported-type list is exactly the documented host set.
    assert_eq!(
        BREAKPOINT_BLOCK_TYPES,
        ["input_text", "input_image", "input_file"],
        "the single supported-block list matches the official guide"
    );
}
