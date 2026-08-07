use agent_contracts::{
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextState, ContextStateTransition,
};

use crate::diagnostics;
use crate::engine::{SimpleContextConfig, State};
use crate::gc::reachability::{drain_supersessions, drain_verifications};
use crate::residency::next_residency;
use crate::scope;

/// One maintenance pass over the whole heap: queued scope closes (task
/// completion, tool results consumed), then supersession / verification
/// intents, then the per-item residency state machine. All resulting state
/// changes are recorded as explainable transitions.
pub(crate) fn run_minor(
    state: &mut State,
    config: &SimpleContextConfig,
    trigger: ContextMaintenanceTrigger,
    now_tick: u64,
    turn: u64,
) -> ContextMaintenanceReport {
    let mut report = ContextMaintenanceReport {
        turn,
        ..ContextMaintenanceReport::default()
    };
    let focus = state.focus.clone();
    // The hot set is capped at 24 entries and only changes on ingest; clone
    // once so the loop can read it while items are mutated.
    let hot_entities = state.hot_entities.clone();

    // The model consumed the last round's tool results. Their scopes are
    // execution frames driven by the runtime (opened at tool start, closed
    // when the next model round begins), so nothing to queue here; the
    // ephemeral results leave through the residency pass below.

    // Scope closes queued by ingest (task completed) become observable state
    // changes here: durable outcomes are promoted to the parent scope, the
    // rest of the working set is evicted.
    let closed = scope::drain_closed_scopes(state, turn);
    report.archived += closed.len();
    report.transitions.extend(closed);

    // Supersession and verification intents recorded by ingest become
    // observable state changes here, with explainable reasons.
    let superseded = drain_supersessions(state, turn);
    report.archived += superseded.len();
    report.transitions.extend(superseded);
    let verified = drain_verifications(state, turn);
    report.archived += verified.len();
    report.transitions.extend(verified);

    for item in &mut state.items {
        let old_state = item.state;
        let outcome = next_residency(
            item,
            config,
            trigger,
            now_tick,
            turn,
            focus.as_ref(),
            &hot_entities,
        );
        item.state = outcome.state;
        item.relevance = outcome.relevance;

        if item.state != old_state {
            match item.state {
                ContextState::Active => report.promoted += 1,
                ContextState::Cooling => report.cooled += 1,
                ContextState::Archived => report.archived += 1,
                ContextState::Dropped => report.dropped += 1,
            }
            report.transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: old_state,
                to: item.state,
                turn,
                reason: outcome.reason,
            });
        }
    }

    report.diagnostics = diagnostics::compute(state);
    report
}
