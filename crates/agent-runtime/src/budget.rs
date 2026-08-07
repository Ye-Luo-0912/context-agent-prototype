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
use serde::Serialize;

/// Reserve carved out of the context window when the provider does not
/// declare a `max_output_tokens`: the answer must always have room.
pub const DEFAULT_OUTPUT_RESERVE: usize = 1_024;

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
}
