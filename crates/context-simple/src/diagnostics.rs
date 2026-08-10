use agent_contracts::{AttentionState, ContextDiagnostics, ScopeState, SemanticState};

use crate::engine::State;
use crate::item::approx_tokens;

/// Counts of the current heap by attention state and semantic death, the
/// active token estimate, the scope tree by lifecycle state and the GC
/// residency split (resident / warm buffer / cold store / external).
///
/// `total_items` is the *logical catalog*: every item the engine knows
/// across all body locations (resident heap + warm buffer + cold/external
/// store entries). Each id lives in exactly one location
/// (`has_exactly_one_owner`), so the sum is exact and replay's `final_total`
/// is a real catalog total, not just the resident share.
pub(crate) fn compute(state: &State) -> ContextDiagnostics {
    let mut diagnostics = ContextDiagnostics {
        total_items: state
            .items
            .len()
            .saturating_add(state.eviction_buffer.len())
            .saturating_add(state.external.len()),
        focus_generation: state.focus.as_ref().map_or(0, |f| f.generation),
        turn: state.turn,
        event_seq: state.event_seq,
        tool_round: state.tool_round,
        resident_items: state.items.len(),
        warm_items: state.eviction_buffer.len(),
        cold_items: state
            .external
            .iter()
            .filter(|entry| entry.residency == agent_contracts::ContextResidency::Cold)
            .count(),
        external_items: state
            .external
            .iter()
            .filter(|entry| entry.residency == agent_contracts::ContextResidency::External)
            .count(),
        gc_evicted_total: state.gc_evicted_total,
        gc_reactivated_total: state.gc_reactivated_total,
        gc_externalized_total: state.gc_externalized_total,
        gc_storage_deleted_total: state.gc_storage_deleted_total,
        ..ContextDiagnostics::default()
    };

    for item in &state.items {
        match item.attention {
            AttentionState::Active => diagnostics.active_items += 1,
            AttentionState::Cooling => diagnostics.cooling_items += 1,
            AttentionState::Archived => diagnostics.archived_items += 1,
        }
        if matches!(item.semantic, SemanticState::Tombstoned) {
            diagnostics.tombstoned_items += 1;
        }

        if item.attention == AttentionState::Active {
            diagnostics.approx_active_tokens += approx_tokens(&item.content);
        }
    }

    for scope in &state.scopes {
        match scope.state {
            ScopeState::Open => diagnostics.open_scopes += 1,
            ScopeState::Active => diagnostics.active_scopes += 1,
            ScopeState::Suspended => diagnostics.suspended_scopes += 1,
            ScopeState::Closed => diagnostics.closed_scopes += 1,
        }
    }

    diagnostics
}
