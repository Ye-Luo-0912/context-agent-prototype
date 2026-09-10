//! Layout invariants: selection, content, roles, budgets and focus survive
//! the move of current state. No provider-cache hit is inferred here.
use super::*;
use agent_contracts::{
    AttentionState, ContextItemId, ContextRetention, ContextScope, ModelRole, SemanticState,
    ToolCall, ToolExecutionFacts, ToolLifecycle, ToolOutput, ToolRisk,
};
use serde_json::{Value, json};

fn evidence(body: &str, attention: AttentionState) -> MaterializedItem {
    MaterializedItem {
        item_id: ContextItemId::new(),
        kind: ContextKind::Note,
        scope: ContextScope::Task,
        attention,
        semantic: SemanticState::Live,
        retention: ContextRetention::Working,
        content: body.into(),
        source: None,
        file_path: None,
        file_revision: None,
        file_start_line: None,
        file_end_line: None,
        partial_body: false,
    }
}

fn assembler(layout: PromptLayout) -> PromptAssembler {
    PromptAssembler::new("policy: preserve user constraints")
        .with_runtime_facts(RuntimeFactsView::new("fixture-os", "fixture-arch", vec![]))
        .with_layout(layout)
}

fn tools() -> Vec<ToolSpec> {
    ["z.read", "a.read"]
        .into_iter()
        .map(|name| ToolSpec {
            name: name.into(),
            description: "read an exact bounded resource".into(),
            input_schema: json!({"type":"object", "properties":{"path":{"type":"string"}}}),
            ..Default::default()
        })
        .collect()
}

fn catalog(state: ToolLifecycle) -> Vec<ToolCatalogEntry> {
    vec![ToolCatalogEntry {
        name: "other.inspect".into(),
        state,
        owner: "builtin".into(),
        description: "inspect a resource".into(),
        risk: ToolRisk::ReadOnly,
        roles: vec![],
    }]
}

fn turn(groups: usize) -> TurnFrame {
    let mut turn = TurnFrame::new(format!(
        "{}\n最后约束：保留焦点与取消语义。",
        "完整指令 ".repeat(500)
    ));
    for n in 0..groups {
        let id = format!("call-{n}");
        turn.push_tool_calls(vec![ToolCall {
            id: id.clone(),
            name: "z.read".into(),
            arguments: json!({"path":format!("file-{n}")}),
        }]);
        turn.push_tool_result(
            ToolOutput {
                call_id: id,
                tool_name: "z.read".into(),
                ok: true,
                summary: format!("read {n}"),
                model_content: format!("body-{n}"),
                artifact_ref: None,
                metadata: json!({}),
            },
            None,
            ToolExecutionFacts::default(),
        );
    }
    turn
}

fn message_set(input: &ModelInput) -> Vec<String> {
    let mut rows: Vec<_> = input
        .into_messages()
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect();
    rows.sort();
    rows
}

fn value(value: &(impl serde::Serialize + ?Sized)) -> Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn layouts_preserve_focus_selected_order_content_roles_and_checkpoint() {
    let mut focus = FocusState::for_task(TaskId::new(), "repair the worker");
    focus.current_query = "只修改 worker；必须保留取消语义".into();
    focus.phase = "verify".into();
    focus.active_entities = vec!["worker".into(), "cancellation".into()];
    let task = TaskAnchorView {
        revision: 7,
        original_goal: "repair the worker".into(),
        constraints: vec!["do not alter public behavior".into()],
        acceptance_criteria: vec!["cancellation remains observable".into()],
        ..Default::default()
    };
    let progress = TaskProgressView {
        anchor_revision: 7,
        workspace_revision: 3,
        checked_files: vec!["src/worker.rs@rev-3".into()],
        ..Default::default()
    };
    let mut required = evidence("REQUIRED-BODY\nexact source", AttentionState::Active);
    required.retention = ContextRetention::Pinned;
    let history = MaterializedContext {
        required_item_ids: vec![required.item_id],
        items: vec![
            required,
            evidence("LOWER-FOCUS-BODY", AttentionState::Cooling),
        ],
        foreground: vec![evidence("FOREGROUND-BODY", AttentionState::Active)],
        ..Default::default()
    };
    let history_before = value(&history);
    let full_turn = turn(8);
    let build = |layout| {
        assembler(layout).assemble_with_catalog(
            Some(&focus),
            Some(&task),
            Some(&progress),
            &history,
            &full_turn,
            tools(),
            &catalog(ToolLifecycle::Available),
            &[],
        )
    };
    let legacy = build(PromptLayout::Legacy);
    let current = build(PromptLayout::CurrentStateLast);
    assert_eq!(
        value(&history),
        history_before,
        "selection and eligibility are not rewritten"
    );
    assert_eq!(value(&legacy.context_frame), value(&current.context_frame));
    assert_eq!(legacy.focus_frame, current.focus_frame);
    assert_eq!(value(&legacy.tool_schemas), value(&current.tool_schemas));
    assert_eq!(
        current
            .tool_schemas
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["z.read", "a.read"]
    );
    assert_eq!(value(&legacy.turn_frame), value(&current.turn_frame));
    assert_eq!(legacy.turn_checkpoint, current.turn_checkpoint);
    assert_eq!(
        current
            .turn_checkpoint
            .as_ref()
            .unwrap()
            .compacted_exchanges,
        2
    );
    assert_eq!(current.turn_frame.user_message, full_turn.user_message);
    assert_eq!(
        message_set(&legacy),
        message_set(&current),
        "every byte and role survives exactly once"
    );
    assert_eq!(
        crate::budget::approx_layer_tokens(&legacy.into_messages()),
        crate::budget::approx_layer_tokens(&current.into_messages()),
        "same packing cost"
    );
    let messages = current.into_messages();
    let last = messages.last().unwrap();
    assert_eq!(last.role, ModelRole::System);
    assert_eq!(Some(&last.content), current.focus_frame.as_ref());
    assert!(last.content.contains("只修改 worker"));
    assert!(last.content.contains("Phase: verify"));
    assert!(last.content.contains("worker, cancellation"));
    assert!(last.content.contains("do not alter public behavior"));
    assert!(messages.iter().filter(|m| m.role == ModelRole::System)
        .all(|m| !m.content.contains("REQUIRED-BODY") && !m.content.contains("FOREGROUND-BODY")));
    let selected = messages
        .iter()
        .find(|m| m.content.starts_with("SELECTED WORKING CONTEXT"))
        .unwrap();
    assert!(
        selected.content.find("REQUIRED-BODY").unwrap()
            < selected.content.find("LOWER-FOCUS-BODY").unwrap()
    );
}

#[test]
fn progress_changes_only_the_last_message_when_evidence_is_unchanged() {
    let history = MaterializedContext {
        items: vec![evidence(
            &"unchanged evidence\n".repeat(100),
            AttentionState::Active,
        )],
        ..Default::default()
    };
    let before = TaskProgressView {
        anchor_revision: 1,
        checked_files: vec!["src/unchanged.rs@rev-1".into()],
        ..Default::default()
    };
    let mut after = before.clone();
    after.checked_files.push("src/worker.rs@rev-1".into());
    let turn = turn(2);
    let a = PromptAssembler::new("policy").assemble(
        None,
        None,
        Some(&before),
        &history,
        &turn,
        tools(),
    );
    let b =
        PromptAssembler::new("policy").assemble(None, None, Some(&after), &history, &turn, tools());
    assert_eq!(a.layout, PromptLayout::CurrentStateLast);
    let a = a.into_messages();
    let b = b.into_messages();
    assert_eq!(a.len(), b.len());
    assert_eq!(value(&a[..a.len() - 1]), value(&b[..b.len() - 1]));
    assert_ne!(a.last().unwrap().content, b.last().unwrap().content);
    assert!(b.last().unwrap().content.contains("src/worker.rs@rev-1"));
}

#[test]
fn catalog_state_moves_after_history_without_freezing_or_expanding_tools() {
    let assembler = assembler(PromptLayout::CurrentStateLast);
    let history = MaterializedContext::default();
    let turn = turn(1);
    let a = assembler.assemble_with_catalog(
        None,
        None,
        None,
        &history,
        &turn,
        tools(),
        &catalog(ToolLifecycle::Available),
        &[],
    );
    let b = assembler.assemble_with_catalog(
        None,
        None,
        None,
        &history,
        &turn,
        tools(),
        &catalog(ToolLifecycle::Warm),
        &[],
    );
    assert_eq!(value(&a.tool_schemas), value(&b.tool_schemas));
    let a = a.into_messages();
    let b = b.into_messages();
    assert_eq!(value(&a[..a.len() - 1]), value(&b[..b.len() - 1]));
    assert!(
        a.last()
            .unwrap()
            .content
            .contains("available\tother.inspect")
    );
    assert!(b.last().unwrap().content.contains("warm\tother.inspect"));
    assert_eq!(a.last().unwrap().role, ModelRole::System);
}
