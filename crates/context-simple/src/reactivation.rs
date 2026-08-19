//! Hot-reactivation utility: count whether a reactivated item was later
//! selected and consumed. Policy is unchanged; this is measurement only.
//!
//! `recovery_auto_reactivation` (eval) is first-recovery attribution on
//! forgotten ids. These counters are reactivation-trace unique ids and
//! events; do not report them as the same utilization rate.

use agent_contracts::{ContextItem, ContextItemId, ContextKind};

use crate::engine::State;
use crate::item::approx_tokens;

#[derive(Debug, Clone)]
pub(crate) struct ReactivationTrace {
    pub kind: ContextKind,
    #[allow(dead_code)]
    pub reason: String,
    pub tokens: usize,
    pub selected: bool,
    pub consumed: bool,
}

pub(crate) fn clear_segment(state: &mut State) {
    state.reactivation_traces.clear();
    state.reactivation_selected = 0;
    state.reactivation_consumed = 0;
    state.reactivation_selected_tokens = 0;
    state.reactivation_consumed_tokens = 0;
    state.reactivation_events = 0;
    state.unique_reactivated = 0;
    state.reactivated_tokens = 0;
    state.reactivation_tool_observation_selected = 0;
    state.reactivation_tool_observation_consumed = 0;
    state.reactivation_file_observation_selected = 0;
    state.reactivation_file_observation_consumed = 0;
}

pub(crate) fn record(state: &mut State, item: &ContextItem, reason: &str) {
    let tokens = approx_tokens(&item.content);
    state.reactivation_events = state.reactivation_events.saturating_add(1);
    if state.reactivation_traces.contains_key(&item.id) {
        return;
    }
    state.unique_reactivated = state.unique_reactivated.saturating_add(1);
    state.reactivated_tokens = state.reactivated_tokens.saturating_add(tokens as u64);
    state.reactivation_traces.insert(
        item.id,
        ReactivationTrace {
            kind: item.kind,
            reason: reason.chars().take(96).collect(),
            tokens,
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
    let kind = trace.kind;
    let add = trace.tokens as u64;
    state.reactivation_selected = state.reactivation_selected.saturating_add(1);
    state.reactivation_selected_tokens = state.reactivation_selected_tokens.saturating_add(add);
    match kind {
        ContextKind::ToolObservation => {
            state.reactivation_tool_observation_selected = state
                .reactivation_tool_observation_selected
                .saturating_add(1);
        }
        ContextKind::FileObservation => {
            state.reactivation_file_observation_selected = state
                .reactivation_file_observation_selected
                .saturating_add(1);
        }
        _ => {}
    }
}

pub(crate) fn mark_consumed(state: &mut State, item_id: ContextItemId) {
    let Some(trace) = state.reactivation_traces.get_mut(&item_id) else {
        return;
    };
    if trace.consumed {
        return;
    }
    trace.consumed = true;
    let kind = trace.kind;
    let add = trace.tokens as u64;
    state.reactivation_consumed = state.reactivation_consumed.saturating_add(1);
    state.reactivation_consumed_tokens = state.reactivation_consumed_tokens.saturating_add(add);
    match kind {
        ContextKind::ToolObservation => {
            state.reactivation_tool_observation_consumed = state
                .reactivation_tool_observation_consumed
                .saturating_add(1);
        }
        ContextKind::FileObservation => {
            state.reactivation_file_observation_consumed = state
                .reactivation_file_observation_consumed
                .saturating_add(1);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ContextKind, ContextRetention, ContextScope};

    use crate::engine::{SimpleContextConfig, State};
    use crate::item::make_item;

    #[test]
    fn same_id_counts_as_events_not_unique_ids() {
        let config = SimpleContextConfig::default();
        let mut state = State::default();
        let tool = make_item(
            &state,
            &config,
            "shell dump".into(),
            ContextKind::ToolObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            Some("shell.exec".into()),
        );
        record(&mut state, &tool, "hot");
        record(&mut state, &tool, "hot again");
        assert_eq!(state.reactivation_events, 2);
        assert_eq!(state.unique_reactivated, 1);
        mark_selected(&mut state, tool.id, 12);
        mark_selected(&mut state, tool.id, 12);
        mark_consumed(&mut state, tool.id);
        mark_consumed(&mut state, tool.id);
        assert_eq!(state.reactivation_selected, 1);
        assert_eq!(state.reactivation_consumed, 1);
        assert_eq!(state.reactivation_tool_observation_selected, 1);
        assert_eq!(state.reactivation_tool_observation_consumed, 1);

        let file = make_item(
            &state,
            &config,
            "src/auth.rs".into(),
            ContextKind::FileObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            Some("fs.read".into()),
        );
        record(&mut state, &file, "hot file");
        mark_selected(&mut state, file.id, 8);
        mark_consumed(&mut state, file.id);
        assert_eq!(state.reactivation_events, 3);
        assert_eq!(state.unique_reactivated, 2);
        assert_eq!(state.reactivation_file_observation_selected, 1);
        assert_eq!(state.reactivation_file_observation_consumed, 1);
        clear_segment(&mut state);
        assert_eq!(state.reactivation_events, 0);
        assert_eq!(state.unique_reactivated, 0);
        assert!(state.reactivation_traces.is_empty());
    }
}
