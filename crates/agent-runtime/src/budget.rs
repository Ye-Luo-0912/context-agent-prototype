//! The model budget: how much of the provider's context window the context
//! engine may spend on the working set for one request.
//!
//! The budget is computed at the runtime top level, where the provider's
//! declared window and every non-engine layer are known:
//!
//! ```text
//! Provider Context Window
//!         - Output Reserve
//!         - System Policy
//!         - Turn Frame
//!         - Active Tool Schemas
//!         = Context Frame Budget   (what the engine receives)
//! ```
//!
//! The engine then only sees "you have N tokens" — it never has to
//! understand the model request shape, and every layer is a hard
//! subtraction (the frame budget is the remaining slice, saturated at zero).

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{
    CAPABILITY_INSPECT, CAPABILITY_LOAD, CAPABILITY_MANAGE, CAPABILITY_SEARCH, CAPABILITY_UNLOAD,
    CONTEXT_MANAGE, ToolSpec,
};
use serde::Serialize;

/// Reserve carved out of the context window when the provider does not
/// declare a `max_output_tokens`: the answer must always have room.
pub const DEFAULT_OUTPUT_RESERVE: usize = 1_024;

/// Deterministic cap on the *always-visible* tool schema cost of one model
/// round, applied at surface capture — the single safe point the budget,
/// the prompt and tool-call validation all read from, so pricing is honest
/// and the prompt cannot grow with every loaded capability. Control tools
/// and core builtin tools are never trimmed; optional tools (loaded builtin
/// and capability tools alike) are kept smallest-schema-first until the
/// cap, so the model keeps the widest surface that fits the bound. The
/// runtime's final budget guard stays as the backstop for the fixed layers
/// (system + turn + tools) that still overshoot a small provider window.
pub const MAX_TOOL_SURFACE_TOKENS: usize = 4096;

/// The runtime's own control tools are always visible regardless of what
/// the base catalog reports as loaded: they are how the model discovers
/// and changes the surface itself.
fn is_runtime_control(name: &str) -> bool {
    matches!(
        name,
        CAPABILITY_MANAGE
            | CONTEXT_MANAGE
            | CAPABILITY_SEARCH
            | CAPABILITY_LOAD
            | CAPABILITY_UNLOAD
            | CAPABILITY_INSPECT
    )
}

/// Bound one round's tool surface to [`MAX_TOOL_SURFACE_TOKENS`] schema
/// tokens. Deterministic: always-visible tools (runtime control + the
/// base catalog's core set) stay; the rest are kept in ascending schema
/// cost (name as the tie-break) until the cap, so the widest possible
/// surface survives within the bound. The output keeps the canonical
/// name-sorted order the dispatcher already uses.
pub fn bounded_tool_surface(
    specs: Vec<ToolSpec>,
    core_tools: &std::collections::HashSet<String>,
) -> Vec<ToolSpec> {
    let mut always = Vec::new();
    let mut optional = Vec::new();
    for spec in specs {
        if core_tools.contains(&spec.name) || is_runtime_control(&spec.name) {
            always.push(spec);
        } else {
            optional.push(spec);
        }
    }
    // Cheapest schemas first: under a token cap the model keeps the most
    // tools visible (a dozen small tools beat one huge one), deterministically.
    optional.sort_by(|a, b| {
        approx_layer_tokens(a)
            .cmp(&approx_layer_tokens(b))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut budget = MAX_TOOL_SURFACE_TOKENS.saturating_sub(approx_layer_tokens(&always));
    for spec in optional {
        let cost = approx_layer_tokens(&spec);
        if cost > budget {
            break;
        }
        always.push(spec);
        budget -= cost;
    }
    always.sort_by(|a, b| a.name.cmp(&b.name));
    always
}

/// One request's budget breakdown. `context_frame_budget` is what gets handed
/// to `ContextQuery::budget_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelBudget {
    pub context_window: usize,
    pub output_reserve: usize,
    pub system_policy_tokens: usize,
    pub turn_frame_tokens: usize,
    pub active_tools_tokens: usize,
    pub context_frame_budget: usize,
}

impl ModelBudget {
    pub fn compute(
        context_window: usize,
        output_reserve: usize,
        system_policy_tokens: usize,
        turn_frame_tokens: usize,
        active_tools_tokens: usize,
    ) -> Self {
        let context_frame_budget = context_window
            .saturating_sub(output_reserve)
            .saturating_sub(system_policy_tokens)
            .saturating_sub(turn_frame_tokens)
            .saturating_sub(active_tools_tokens);
        Self {
            context_window,
            output_reserve,
            system_policy_tokens,
            turn_frame_tokens,
            active_tools_tokens,
            context_frame_budget,
        }
    }
}

/// Token estimate for one serialized request layer (turn frame, tool
/// schemas): the JSON wire form is what the provider actually ingests, so it
/// is the honest measure of that layer's cost.
pub fn approx_layer_tokens(layer: &impl Serialize) -> usize {
    let bytes = serde_json::to_vec(layer).unwrap_or_default();
    approx_tokens(&String::from_utf8_lossy(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ModelMessage;

    #[test]
    fn budget_subtracts_every_layer_from_the_window() {
        let budget = ModelBudget::compute(30_000, 2_000, 1_500, 800, 300);
        assert_eq!(
            budget.context_frame_budget,
            30_000 - 2_000 - 1_500 - 800 - 300
        );
        assert_eq!(budget.context_window, 30_000);
    }

    #[test]
    fn budget_saturates_at_zero() {
        let budget = ModelBudget::compute(1_000, 2_000, 1_500, 800, 300);
        assert_eq!(budget.context_frame_budget, 0);
    }

    #[test]
    fn layer_tokens_follow_the_wire_form() {
        let messages = vec![
            ModelMessage::user("hello world"),
            ModelMessage::tool_result("c1", "fs.read", "one two three"),
        ];
        let estimated = approx_layer_tokens(&messages);
        assert!(estimated > 0, "a non-empty layer must cost something");
    }

    #[test]
    fn empty_layer_costs_at_most_the_wire_overhead() {
        let messages: Vec<ModelMessage> = Vec::new();
        // `[]` serializes to two bytes; the estimator rounds up to one token.
        assert!(approx_layer_tokens(&messages) <= 1);
    }

    #[test]
    fn surface_bound_never_trims_core_or_control_tools() {
        use agent_contracts::{ToolRisk, ToolSpec};
        use serde_json::json;

        let core: std::collections::HashSet<String> =
            ["fs.read".into(), "context.manage".into()].into();
        let mut specs = vec![
            ToolSpec {
                name: "fs.read".into(),
                description: "read a file".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: CONTEXT_MANAGE.into(),
                description: "runtime control".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
        ];
        // A wall of optional tools far above the cap.
        for i in 0..100 {
            specs.push(ToolSpec {
                name: format!("cap.ext.tool_{i:02}"),
                description: "x".repeat(200),
                input_schema: json!({"type": "object", "properties": {"p": {"type": "string"}}}),
                risk: ToolRisk::ReadOnly,
            });
        }
        let bounded = bounded_tool_surface(specs, &core);
        assert!(
            bounded.iter().any(|s| s.name == "fs.read"),
            "core tools are never trimmed"
        );
        assert!(
            bounded.iter().any(|s| s.name == CONTEXT_MANAGE),
            "control tools are never trimmed"
        );
        let total = bounded.iter().map(approx_layer_tokens).sum::<usize>();
        assert!(
            total <= MAX_TOOL_SURFACE_TOKENS,
            "the bounded surface stays under the cap: {total}"
        );
        assert!(bounded.len() < 102, "the optional wall was trimmed");
    }

    #[test]
    fn surface_bound_is_deterministic_and_smallest_first() {
        use agent_contracts::{ToolRisk, ToolSpec};
        use serde_json::json;

        let core = std::collections::HashSet::new();
        let make = |name: &str, schema: serde_json::Value| ToolSpec {
            name: name.into(),
            description: "d".into(),
            input_schema: schema,
            risk: ToolRisk::ReadOnly,
        };
        // A big-schema tool and many small ones: smallest-schema-first keeps
        // the most tools visible under the cap, and the result is stable.
        let mut specs = vec![
            make(
                "z.big",
                json!({"type": "object", "properties": {"p": {"type": "string", "description": "x".repeat(20_000)}}}),
            ),
            make("a.small", json!({"type": "object"})),
            make("b.small", json!({"type": "object"})),
        ];
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        let first = bounded_tool_surface(specs.clone(), &core);
        let second = bounded_tool_surface(specs, &core);
        let names = |v: &[ToolSpec]| v.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&first), names(&second), "the policy is deterministic");
        assert!(
            first.iter().any(|s| s.name == "a.small"),
            "the smallest tool survives"
        );
        assert!(
            first.iter().all(|s| s.name != "z.big"),
            "the huge schema is trimmed first"
        );
        let names: Vec<&str> = first.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "the surface keeps its canonical order");
    }
}
