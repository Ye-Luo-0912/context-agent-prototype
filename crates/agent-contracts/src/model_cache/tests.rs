use super::*;
use crate::{ModelInput, ModelMessage, PromptLayout, ToolSpec, TurnFrame};
use serde_json::json;

fn input(layout: PromptLayout) -> ModelInput {
    ModelInput {
        layout,
        system_policy: vec![ModelMessage::system("policy")],
        context_frame: vec![ModelMessage::user("证据\nrevision=one")],
        current_state_frame: vec![ModelMessage::system("catalog")],
        focus_frame: Some("current goal".into()),
        turn_frame: TurnFrame::new("complete directive"),
        tool_schemas: vec![ToolSpec {
            name: "fs.read".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn changed_current_state_reuses_evidence_boundary_with_every_message_preserved() {
    let original = input(PromptLayout::CurrentStateLast);
    let expected = serde_json::to_value(original.into_messages()).unwrap();
    let request = original
        .clone()
        .into_request(json!({"run":"one"}), Default::default());
    let boundary = request.prompt_reuse_boundary().unwrap();
    assert_eq!(boundary.message_count(), 2);
    assert_eq!(serde_json::to_value(&request.messages).unwrap(), expected);
    assert_eq!(request.metadata["run"], "one");
    let mut changed = original;
    changed.focus_frame = Some("a different current goal".into());
    changed.turn_frame.user_message = "a different complete directive".into();
    changed.current_state_frame[0].content = "changed catalog".into();
    assert_eq!(
        changed
            .into_request(json!({}), Default::default())
            .prompt_reuse_boundary(),
        Some(boundary)
    );

    let legacy = input(PromptLayout::Legacy).into_request(json!({}), Default::default());
    assert_eq!(legacy.prompt_reuse_boundary().unwrap().message_count(), 1);
}

#[test]
fn edited_dropped_reordered_prefix_and_changed_schema_invalidate_old_hint() {
    let original =
        input(PromptLayout::CurrentStateLast).into_request(json!({}), Default::default());
    let mut edited = original.clone();
    edited.messages[1].content.push_str(" new revision");
    let mut dropped = original.clone();
    dropped.messages.remove(1);
    let mut reordered = original.clone();
    reordered.messages.swap(0, 1);
    let mut role = original.clone();
    role.messages[1].role = ModelRole::System;
    let mut schema = original.clone();
    schema.tools[0].input_schema = json!({"type":"object"});
    let mut out_of_range = original.clone();
    out_of_range.metadata[METADATA_KEY]["message_count"] = json!(usize::MAX);
    let mut unknown_version = original.clone();
    unknown_version.metadata[METADATA_KEY]["version"] = json!(2);
    for request in [
        edited,
        dropped,
        reordered,
        role,
        schema,
        out_of_range,
        unknown_version,
    ] {
        assert!(request.prompt_reuse_boundary().is_none());
    }
    let mut suffix = original.clone();
    suffix.messages.last_mut().unwrap().content = "new goal".into();
    assert_eq!(
        suffix.prompt_reuse_boundary(),
        original.prompt_reuse_boundary()
    );
}

#[test]
fn binding_uses_the_final_retained_evidence_and_legacy_requests_need_no_hint() {
    let mut packed = input(PromptLayout::CurrentStateLast);
    packed
        .context_frame
        .push(ModelMessage::user("discarded by packing"));
    let old = packed.clone().into_request(json!({}), Default::default());
    packed.context_frame.pop();
    let final_request = packed.into_request(old.metadata.clone(), Default::default());
    let boundary = final_request.prompt_reuse_boundary().unwrap();
    assert_eq!(boundary.message_count(), 2);
    assert_ne!(Some(boundary), old.prompt_reuse_boundary());
    assert!(
        !final_request
            .messages
            .iter()
            .any(|m| m.content == "discarded by packing")
    );
    let mut legacy_json = serde_json::to_value(final_request).unwrap();
    legacy_json["metadata"] = json!({});
    assert!(
        serde_json::from_value::<ModelRequest>(legacy_json)
            .unwrap()
            .prompt_reuse_boundary()
            .is_none()
    );
}

#[test]
fn empty_or_protocol_prefixes_do_not_create_invalid_boundaries() {
    let empty = ModelInput::default().into_request(json!(null), Default::default());
    assert!(empty.prompt_reuse_boundary().is_none());
    let mut protocol = input(PromptLayout::CurrentStateLast);
    protocol
        .context_frame
        .push(ModelMessage::tool_result("call", "tool", "body"));
    assert!(
        protocol
            .into_request(json!({}), Default::default())
            .prompt_reuse_boundary()
            .is_none()
    );
    let mut trailing_empty = input(PromptLayout::CurrentStateLast);
    trailing_empty.context_frame.push(ModelMessage::user(""));
    assert_eq!(
        trailing_empty
            .into_request(json!({}), Default::default())
            .prompt_reuse_boundary()
            .unwrap()
            .message_count(),
        2
    );
}
