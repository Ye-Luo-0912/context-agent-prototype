//! Hot-reactivation utility: count whether a reactivated item was later
//! selected and consumed. Policy is unchanged; this is measurement only.

use agent_contracts::{ContextItem, ContextItemId, ContextKind};

use crate::engine::State;
use crate::item::approx_tokens;

#[derive(Debug, Clone)]
pub(crate) struct ReactivationTrace {
    pub kind: ContextKind,
    pub reason: String,
    pub tokens: usize,
    pub selected: bool,
    pub consumed: bool,
}

pub(crate) fn record(state: &mut State, item: &ContextItem, reason: &str) {
    state.reactivation_traces.insert(
        item.id,
        ReactivationTrace {
            kind: item.kind,
            reason: reason.chars().take(96).collect(),
            tokens: approx_tokens(&item.content),
            selected: false,
            consumed: false,
        },
    );
}

pub(crate) fn mark_selected(state: &mut State, item_id: ContextItemId, tokens: usize) {
    let Some(trace) = state.reactivation_traces.get_mut(&item_id) else {
        return;
    };
    if trace.selected {
        return;
    }
    trace.selected = true;
    if tokens > 0 {
        trace.tokens = tokens;
    }
    let _classified = (trace.kind, trace.reason.as_str());
    state.reactivation_selected = state.reactivation_selected.saturating_add(1);
    state.reactivation_selected_tokens = state
        .reactivation_selected_tokens
        .saturating_add(trace.tokens as u64);
}

pub(crate) fn mark_consumed(state: &mut State, item_id: ContextItemId) {
    let Some(trace) = state.reactivation_traces.get_mut(&item_id) else {
        return;
    };
    if trace.consumed {
        return;
    }
    trace.consumed = true;
    let _classified = (trace.kind, trace.reason.as_str());
    state.reactivation_consumed = state.reactivation_consumed.saturating_add(1);
    state.reactivation_consumed_tokens = state
        .reactivation_consumed_tokens
        .saturating_add(trace.tokens as u64);
}
