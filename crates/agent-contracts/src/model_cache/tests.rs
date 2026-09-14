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

// ---------------------------------------------------------------------------
// R6: the routing key encodes the five-field routing tuple unambiguously —
// a versioned, length-prefixed serialization digested to a fixed-length
// opaque string. Separator confusion, multibyte boundaries and oversized
// components can never alias two different routing identities.
// ---------------------------------------------------------------------------

fn routing(isolation: &str, workspace: &str, endpoint: &str) -> PromptCacheRouting {
    PromptCacheRouting {
        isolation: isolation.into(),
        workspace: workspace.into(),
        endpoint: endpoint.into(),
    }
}

/// The historical `|`-joined encoding serialized both of these identities to
/// the same string (`a|b|c|d|t|main`): the separator carried no escaping, so
/// a `|` inside one component was indistinguishable from the join itself.
#[test]
fn separator_ambiguous_component_pairs_never_share_a_key() {
    let pipe_in_isolation = routing("a|b", "c", "d").key_for("t", "main");
    let pipe_in_workspace = routing("a", "b|c", "d").key_for("t", "main");
    assert_ne!(
        pipe_in_isolation, pipe_in_workspace,
        "a separator inside a component is component text, not a join"
    );

    // The same confusion across the task/lane boundary.
    let pipe_in_task = routing("i", "w", "e").key_for("t|main", "");
    let pipe_in_lane = routing("i", "w", "e").key_for("t", "main|");
    assert_ne!(
        pipe_in_task, pipe_in_lane,
        "the task/lane join must not be confusable with task or lane text"
    );
}

/// Components are hashed as exact UTF-8 byte strings: multibyte characters
/// never blur into separator positions, and every identity stays stable.
#[test]
fn unicode_component_boundaries_never_alias() {
    let cjk_pipe_isolation = routing("工作|区", "w", "e").key_for("t", "main");
    let cjk_pipe_workspace = routing("工作", "区|w", "e").key_for("t", "main");
    assert_ne!(cjk_pipe_isolation, cjk_pipe_workspace);
    assert_eq!(
        cjk_pipe_isolation,
        routing("工作|区", "w", "e").key_for("t", "main"),
        "the same identity serializes to the same key across calls"
    );
    assert_ne!(
        routing("工作区", "w", "e").key_for("t", "main"),
        routing("工|作区", "w", "e").key_for("t", "main")
    );
}

/// The historical encoding kept every ≤256-char component verbatim but then
/// cut the composed string at 1024 chars — cutting into the trailing fields,
/// so identities that differed only there (here: different LANES) collapsed
/// into one key. The structure is now hashed whole, never truncated.
#[test]
fn oversized_compositions_are_never_truncated_into_collisions() {
    let padded = routing(&"a".repeat(256), &"b".repeat(256), &"c".repeat(256));
    let long_task = "d".repeat(256);
    let main = padded.key_for(&long_task, "main");
    let maintenance = padded.key_for(&long_task, "maintenance");
    assert_ne!(
        main, maintenance,
        "different lanes of the same identity must never share a key, whatever the component sizes"
    );
    let other_task = padded.key_for(&format!("{}e", "d".repeat(255)), "main");
    assert_ne!(
        main, other_task,
        "identities differing in the last character of a long component stay distinct"
    );
}

/// The key is a bounded, opaque wire string: fixed length, no join
/// separators, no readable component text — a hostile configuration cannot
/// turn it into an unbounded or content-leaking header value.
#[test]
fn key_shape_is_fixed_length_opaque_and_component_free() {
    let tiny = routing("i", "w", "e").key_for("t", "main");
    let huge = routing(
        &"隔离域".repeat(500),
        &"/very/long/workspace/root/".repeat(200),
        &"https://endpoint.example.com/v1?".repeat(50),
    )
    .key_for("任务", "maintenance");
    for key in [&tiny, &huge] {
        assert!(
            key.starts_with("rc2-") && key.len() == "rc2-".len() + 64,
            "the key is the versioned 64-hex routing digest: {key}"
        );
        assert!(
            key["rc2-".len()..]
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "the digest suffix is lowercase hex: {key}"
        );
        assert!(
            !key.contains('|'),
            "the key carries no join separators: {key}"
        );
    }
    assert_ne!(tiny, huge);
    assert!(
        !huge.contains("隔离域") && !huge.contains("endpoint.example.com"),
        "no component text survives into the opaque key: {huge}"
    );
}

/// The identity relation survives the migration unchanged: one task on one
/// lane keeps its key across calls and equivalent routings, while a
/// different task or the maintenance lane is always a different namespace.
#[test]
fn keys_stay_stable_per_task_and_lane_and_separated_across_both() {
    let first = routing("iso", "ws", "ep");
    let again = routing("iso", "ws", "ep");
    assert_eq!(
        first.key_for("task-42", "main"),
        again.key_for("task-42", "main")
    );
    let main = first.key_for("task-42", "main");
    let maintenance = first.key_for("compaction", "maintenance");
    assert_ne!(
        main, maintenance,
        "the main call and the maintenance call are different namespaces of one identity"
    );
    assert_ne!(main, first.key_for("task-43", "main"));
    assert_eq!(
        maintenance,
        again.key_for("compaction", "maintenance"),
        "the maintenance lane key is just as stable as the main lane key"
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
