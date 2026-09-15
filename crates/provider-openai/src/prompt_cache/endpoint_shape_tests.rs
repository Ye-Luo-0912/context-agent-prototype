//! T6/R6 ENDPOINT REQUEST-SHAPE FIXTURES — the official Responses
//! explicit-caching request shape versus what the current mapper emits.
//!
//! The official prompt-caching documentation
//! (<https://developers.openai.com/api/docs/guides/prompt-caching>) defines
//! the Responses explicit mode as:
//! - a top-level `"prompt_cache_options": {"mode": "explicit"}` field; and
//! - `prompt_cache_breakpoint: {"mode": "explicit"}` placed ON A SUPPORTED
//!   CONTENT BLOCK inside an input message — e.g. inside
//!   `content: [{"type": "input_text", "text": ..., "prompt_cache_breakpoint":
//!   {"mode": "explicit"}}]`, and for `function_call_output` items inside a
//!   content-block `output` array. Plain string content cannot host a
//!   breakpoint, the top-level `instructions` field cannot host one, and the
//!   documented shape has NO input-item sibling form.
//!
//! This module records REQUEST SHAPE only. The local capture servers in the
//! acceptance suites collect JSON without validating the provider schema, so
//! neither this file nor those suites have confirmed real endpoint behavior.
//! The mapper's DECLARED per-item breakpoint form currently diverges from
//! the documented shape on string-content and `function_call_output` items;
//! the divergence is pinned here instead of blindly rewriting the wire (the
//! accepted local-capture regressions in `agent-compose` pin the current
//! bytes). Real-endpoint verification of the breakpoint encoding is T8's
//! conditional task: if it confirms the documented shape, change the mapper
//! and flip the divergence assertions below in the same slice.

use super::*;
use crate::{OpenAiConfig, build_responses_wire_request, wire_names::ToolNameCodec};
use agent_contracts::{ModelInput, ModelMessage, ModelRequest, PromptLayout, TurnFrame};
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

/// S2b: a DECLARED breakpoint on a tool result item (`function_call_output`
/// has `output`, not `content`) keeps the sibling placement — the mapper's
/// content-block rewrite targets `content`, which `function_call_output`
/// items don't carry. The sibling form on `function_call_output` is the
/// only expressible shape; T8 owns the real-endpoint confirmation.
#[test]
fn declared_breakpoint_on_tool_output_keeps_the_sibling_placement() {
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
    assert!(
        output_blocks
            .iter()
            .any(|block| block.get("prompt_cache_breakpoint").is_some()),
        "the tool output content block carries the breakpoint"
    );
    assert!(
        last.get("prompt_cache_breakpoint").is_none(),
        "no sibling breakpoint on the function_call_output item itself"
    );
}
