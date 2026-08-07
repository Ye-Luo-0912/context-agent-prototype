use agent_contracts::{ContextDiagnostics, ContextState};

use crate::engine::State;
use crate::item::approx_tokens;

/// Counts of the current heap by state, plus the active token estimate.
pub(crate) fn compute(state: &State) -> ContextDiagnostics {
    let mut diagnostics = ContextDiagnostics {
        total_items: state.items.len(),
        focus_generation: state.focus.as_ref().map_or(0, |f| f.generation),
        turn: state.turn,
        tool_round: state.tool_round,
        ..ContextDiagnostics::default()
    };

    for item in &state.items {
        match item.state {
            ContextState::Active => diagnostics.active_items += 1,
            ContextState::Cooling => diagnostics.cooling_items += 1,
            ContextState::Archived => diagnostics.archived_items += 1,
            ContextState::Dropped => diagnostics.dropped_items += 1,
        }

        if item.state == ContextState::Active {
            diagnostics.approx_active_tokens += approx_tokens(&item.content);
        }
    }

    diagnostics
}
