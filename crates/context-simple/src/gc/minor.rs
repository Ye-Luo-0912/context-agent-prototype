use agent_contracts::{
    AttentionState, ContextItemId, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextRetention, ContextStateTransition, LifecycleAxis, SemanticState,
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
    now_event_seq: u64,
    turn: u64,
) -> ContextMaintenanceReport {
    let mut report = ContextMaintenanceReport {
        turn,
        ..ContextMaintenanceReport::default()
    };
    // Lifecycle transitions already applied by ingest (focus episode
    // rotation) are surfaced here so they are observable as part of the
    // maintenance report, not silently dropped.
    let ingest_transitions = std::mem::take(&mut state.pending_ingest_transitions);
    report.archived += ingest_transitions.len();
    report.transitions.extend(ingest_transitions);
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
    // Ledger projection for the transitions produced above: same rows,
    // cause and turn, with the trigger that caused them.
    for t in &closed {
        crate::ledger::record(
            state,
            t.item_id,
            LifecycleAxis::Attention,
            format!("{:?}", t.from),
            format!("{:?}", t.to),
            t.reason.clone(),
            "scope_close",
            None,
        );
    }
    report.archived += closed.len();
    report.transitions.extend(closed);

    // Supersession and verification intents recorded by ingest become
    // observable state changes here, with explainable reasons.
    let superseded = drain_supersessions(state, turn);
    for t in &superseded {
        crate::ledger::record(
            state,
            t.item_id,
            LifecycleAxis::Semantic,
            format!("{:?}", t.from),
            format!("{:?}", t.to),
            t.reason.clone(),
            "supersession",
            None,
        );
    }
    report.archived += superseded.len();
    report.transitions.extend(superseded);
    let verified = drain_verifications(state, turn);
    for t in &verified {
        crate::ledger::record(
            state,
            t.item_id,
            LifecycleAxis::Semantic,
            format!("{:?}", t.from),
            format!("{:?}", t.to),
            t.reason.clone(),
            "verification",
            None,
        );
    }
    report.archived += verified.len();
    report.transitions.extend(verified);

    // Ledger rows for the residency pass below. Collected while the heap
    // iterator borrows `state`, then applied after the loop (`record` needs
    // `&mut State`).
    let mut ledger_rows: Vec<(ContextItemId, LifecycleAxis, String, String, String)> = Vec::new();
    for item in &mut state.items {
        let old_attention = item.attention;
        let old_semantic = item.semantic;
        let outcome = next_residency(
            item,
            config,
            trigger,
            now_event_seq,
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

        if item.semantic != old_semantic {
            ledger_rows.push((
                item.id,
                LifecycleAxis::Semantic,
                format!("{old_semantic:?}"),
                format!("{:?}", item.semantic),
                outcome.reason.clone(),
            ));
        }

        if item.attention != old_attention || tombstoned {
            match item.attention {
                AttentionState::Active => report.promoted += 1,
                AttentionState::Cooling => report.cooled += 1,
                AttentionState::Archived => report.archived += 1,
            }
            if tombstoned {
                report.tombstoned += 1;
            }
            let mut reason = outcome.reason.clone();
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
            ledger_rows.push((
                item.id,
                LifecycleAxis::Attention,
                format!("{old_attention:?}"),
                format!("{:?}", item.attention),
                outcome.reason,
            ));
        }
    }
    // Warm-buffer items share the same lifecycle clock: a live item that
    // moved to the reversible buffer must not escape TTL/staleness aging
    // just because it is no longer resident. The same windows residency
    // uses (ephemeral TTL, then the ttl x 4 staleness) tombstone it here,
    // so a dead warm item is never reactivated and Storage GC can
    // eventually delete it. Pinned and keep-alive/lease items are exempt,
    // exactly like the resident root set.
    for item in &mut state.eviction_buffer {
        if !item.semantic.is_live() {
            continue;
        }
        if item.retention == ContextRetention::Pinned
            || item.keep_alive
            || item.lease_until_turn.is_some_and(|until| turn <= until)
        {
            continue;
        }
        let turn_age = turn.saturating_sub(item.created_turn);
        let ttl_expired =
            item.retention == ContextRetention::Ephemeral && turn_age > config.turn_ttl_ticks;
        let stale =
            item.retention != ContextRetention::Durable && turn_age > config.turn_ttl_ticks * 4;
        if !ttl_expired && !stale {
            continue;
        }
        let reason = if ttl_expired {
            format!(
                "ephemeral TTL expired in the warm buffer (age {turn_age} turns > {}); tombstoned",
                config.turn_ttl_ticks
            )
        } else {
            format!(
                "stale in the warm buffer (age {turn_age} turns > ttl x4 = {}); tombstoned",
                config.turn_ttl_ticks * 4
            )
        };
        let old_attention = item.attention;
        item.semantic = SemanticState::Tombstoned;
        item.attention = AttentionState::Archived;
        report.tombstoned += 1;
        report.archived += 1;
        report.transitions.push(ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from: old_attention,
            to: AttentionState::Archived,
            turn,
            reason: format!("{reason} [semantic]"),
        });
        ledger_rows.push((
            item.id,
            LifecycleAxis::Semantic,
            "Live".to_string(),
            "Tombstoned".to_string(),
            reason,
        ));
    }
    for (item_id, axis, from, to, cause) in ledger_rows {
        crate::ledger::record(
            state,
            item_id,
            axis,
            from,
            to,
            cause,
            format!("{trigger:?}"),
            None,
        );
    }

    report.diagnostics = diagnostics::compute(state);
    report
}
