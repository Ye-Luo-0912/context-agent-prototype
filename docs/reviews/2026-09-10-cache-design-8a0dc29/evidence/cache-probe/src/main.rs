//! Offline locality probe of the production assembler, not a token/cache simulator.
//! Record-delimited ModelMessage JSON preserves message boundaries. This is NOT
//! provider wire rendering or tokenization. Tool equality is reported separately.
use agent_contracts::*;
use agent_runtime::PromptAssembler;
use serde_json::{Value, json};

fn item(n: u128) -> MaterializedItem {
    MaterializedItem {
        item_id: format!("00000000-0000-0000-0000-{n:012x}").parse().unwrap(),
        kind: ContextKind::Note,
        scope: ContextScope::Task,
        attention: AttentionState::Active,
        semantic: SemanticState::Live,
        retention: ContextRetention::Working,
        content: format!("evidence-{n}\n{}", "stable bounded evidence\n".repeat(80)),
        source: None,
        file_path: None,
        file_revision: None,
        file_start_line: None,
        file_end_line: None,
        partial_body: false,
    }
}

fn exchange(turn: &mut TurnFrame, n: usize) {
    let id = format!("call-{n}");
    turn.push_tool_calls(vec![ToolCall {
        id: id.clone(),
        name: "context.manage".into(),
        arguments: json!({"op":"inspect", "limit":n + 1}),
    }]);
    turn.push_tool_result(ToolOutput {
        call_id: id,
        tool_name: "context.manage".into(),
        ok: true,
        summary: format!("observation-{n}"),
        model_content: format!("observation-{n}\n{}", "bounded result\n".repeat(40)),
        artifact_ref: None,
        metadata: json!({}),
    }, None, ToolExecutionFacts::default());
}

fn encoded(messages: &[ModelMessage]) -> Vec<u8> {
    let mut out = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut out, message).unwrap();
        out.push(b'\n');
    }
    out
}

fn measure(a: &[ModelMessage], b: &[ModelMessage]) -> Value {
    let a_bytes = encoded(a);
    let b_bytes = encoded(b);
    let shared = a_bytes.iter().zip(&b_bytes).take_while(|(a, b)| a == b).count();
    let same_messages = a.iter().zip(b).take_while(|(a, b)| {
        serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
    }).count();
    json!({"before_bytes":a_bytes.len(), "after_bytes":b_bytes.len(),
        "common_prefix_bytes":shared, "same_leading_messages":same_messages})
}

fn tail_focus(input: &ModelInput) -> Vec<ModelMessage> {
    // A layout-only counterfactual. Same messages, roles, and body bytes;
    // no claim that reordering preserves model behavior or protocol support.
    let mut messages = input.system_policy.clone();
    messages.extend(input.context_frame.clone());
    messages.extend(input.turn_frame_wire_messages());
    if let Some(focus) = &input.focus_frame {
        messages.push(ModelMessage::system(focus));
    }
    messages
}

fn pair(name: &str, a: &ModelInput, b: &ModelInput) -> Value {
    json!({"case":name,
        "tools_equal":serde_json::to_value(&a.tool_schemas).unwrap() == serde_json::to_value(&b.tool_schemas).unwrap(),
        "production":measure(&a.into_messages(), &b.into_messages()),
        "focus_at_tail_counterfactual":measure(&tail_focus(a), &tail_focus(b))})
}

fn main() {
    let assembler = PromptAssembler::new("Deterministic review policy. Preserve authority and evidence.")
        .with_runtime_facts(RuntimeFactsView::new("fixture-os", "fixture-arch", vec!["Cargo.toml".into()]));
    let task = TaskAnchorView { original_goal:"repair a bounded worker".into(), revision:1, ..Default::default() };
    let progress = TaskProgressView { anchor_revision:1, workspace_revision:1, ..Default::default() };
    let history = MaterializedContext {
        items:vec![item(1), item(2)],
        diagnostics:ContextDiagnostics { total_items:2, resident_items:2, ..Default::default() },
        ..Default::default()
    };
    let tools = vec![ToolSpec { name:"context.manage".into(), description:"bounded context access".into(),
        input_schema:json!({"type":"object"}), ..Default::default() }];
    let assemble = |p: &TaskProgressView, h: &MaterializedContext, t: &TurnFrame| {
        assembler.assemble(None, Some(&task), Some(p), h, t, tools.clone())
    };
    let mut turn = TurnFrame::new("Repair the worker; preserve cancellation semantics.");
    exchange(&mut turn, 0);
    let base = assemble(&progress, &history, &turn);
    let mut cases = vec![pair("identical_request_control", &base, &assemble(&progress, &history, &turn))];
    let mut next_progress = progress.clone();
    next_progress.checked_files.push("src/worker.rs@rev-1".into());
    cases.push(pair("progress_only", &base, &assemble(&next_progress, &history, &turn)));
    let mut next_turn = turn.clone();
    exchange(&mut next_turn, 1);
    cases.push(pair("append_exchange_stable_other_layers", &base, &assemble(&progress, &history, &next_turn)));
    cases.push(pair("append_exchange_and_progress", &base, &assemble(&next_progress, &history, &next_turn)));
    let mut residency = history.clone();
    residency.diagnostics.resident_items = 1;
    residency.diagnostics.warm_items = 1;
    cases.push(pair("diagnostics_only_same_bodies", &base, &assemble(&progress, &residency, &turn)));
    let mut attention = history.clone();
    attention.items[0].attention = AttentionState::Cooling;
    cases.push(pair("attention_only_same_bodies", &base, &assemble(&progress, &attention, &turn)));
    let mut reordered = history.clone();
    reordered.items.reverse();
    cases.push(pair("selection_order_only_same_bodies", &base, &assemble(&progress, &reordered, &turn)));
    let mut long_turn = turn.clone();
    for n in 1..8 { exchange(&mut long_turn, n); }
    let checkpointed = assemble(&progress, &history, &long_turn);
    exchange(&mut long_turn, 8);
    cases.push(pair("rolling_checkpoint_8_to_9", &checkpointed, &assemble(&progress, &history, &long_turn)));
    let optional = ToolSpec { name:"fs.read".into(), description:"bounded file read".into(),
        input_schema:json!({"type":"object"}), ..Default::default() };
    let catalog = vec![ToolCatalogEntry {name:optional.name.clone(), state:ToolLifecycle::Available,
        owner:"builtin".into(), description:optional.description.clone(), risk:ToolRisk::ReadOnly, roles:vec![]}];
    let absent = assembler.assemble_with_catalog(None, Some(&task), Some(&progress), &history,
        &turn, tools.clone(), &catalog, &[]);
    let mut expanded = tools;
    expanded.push(optional);
    let present = assembler.assemble_with_catalog(None, Some(&task), Some(&progress), &history,
        &turn, expanded, &catalog, &[]);
    cases.push(pair("load_tool_changes_schema_and_catalog", &absent, &present));
    assert_eq!(cases[0]["production"]["common_prefix_bytes"], cases[0]["production"]["before_bytes"]);
    assert!(cases[1]["production"]["common_prefix_bytes"].as_u64().unwrap()
        < cases[1]["focus_at_tail_counterfactual"]["common_prefix_bytes"].as_u64().unwrap());
    assert_eq!(cases.last().unwrap()["tools_equal"], false);
    println!("{}", serde_json::to_string_pretty(&json!({
        "method":"Production PromptAssembler + ModelInput::into_messages; delimited contract JSON byte LCP. NOT provider tokens, hidden prefix, hit-rate, cost, or behavior equivalence.",
        "cases":cases
    })).unwrap());
}
