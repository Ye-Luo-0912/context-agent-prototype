use agent_contracts::{
    AttentionState, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextStateTransition,
    SemanticState,
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
        let old_attention = item.attention;
        let outcome = next_residency(
            item,
            config,
            trigger,
            now_tick,
            turn,
            focus.as_ref(),
            &hot_entities,
        );
        item.attention = outcome.attention;
        item.relevance = outcome.relevance;

        // Semantic transitions are terminal and explicit: TTL/staleness
        // tombstone the item; GC and promotion respect that forever. (The
        // residency machine only proposes a semantic transition for live
        // items — semantically dead ones are short-circuited above.)
        let tombstoned = match outcome.semantic {
            Some(SemanticState::Tombstoned) => {
                item.semantic = SemanticState::Tombstoned;
                true
            }
            Some(other) => {
                item.semantic = other;
                true
            }
            None => false,
        };

        if item.attention != old_attention || tombstoned {
            match item.attention {
                AttentionState::Active => report.promoted += 1,
                AttentionState::Cooling => report.cooled += 1,
                AttentionState::Archived => report.archived += 1,
            }
            if tombstoned {
                report.tombstoned += 1;
            }
            let mut reason = outcome.reason;
            if tombstoned {
                reason.push_str(" [semantic]");
            }
            report.transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: old_attention,
                to: item.attention,
                turn,
                reason,
            });
        }
    }

    report.diagnostics = diagnostics::compute(state);
    report
}
