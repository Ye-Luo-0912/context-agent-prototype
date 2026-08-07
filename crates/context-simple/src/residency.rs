use agent_contracts::{
    ContextItem, ContextMaintenanceTrigger, ContextRetention, ContextScope, ContextState,
    FocusState, ScoreBreakdown,
};

use crate::engine::SimpleContextConfig;
use crate::gc::reachability::is_excluded;
use crate::index::task::is_stale_task;
use crate::policy::score_item_with_breakdown;

/// The target residency state for one item under one maintenance pass, plus
/// the reason and the relevance value the pass should store.
pub(crate) struct ResidencyOutcome {
    pub state: ContextState,
    pub reason: String,
    pub relevance: f32,
}

/// Decide the next residency state for one item. This is the per-item state
/// machine of the dynamic working set: pinned items stay active, superseded /
/// verified-fixed items stay archived, ephemeral turn observations leave after
/// the model turn or a TTL, everything else is ranked by the policy score and
/// capped by the stale-task gate.
pub(crate) fn next_residency(
    item: &ContextItem,
    config: &SimpleContextConfig,
    trigger: ContextMaintenanceTrigger,
    now_tick: u64,
    turn: u64,
    focus: Option<&FocusState>,
    hot_entities: &[String],
) -> ResidencyOutcome {
    if item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned {
        return ResidencyOutcome {
            state: ContextState::Active,
            reason: "explicitly pinned".to_string(),
            relevance: 1.0,
        };
    }

    if is_excluded(item) {
        return ResidencyOutcome {
            state: ContextState::Archived,
            reason: "excluded from active attention".to_string(),
            relevance: 0.0,
        };
    }

    let age = now_tick.saturating_sub(item.created_tick);

    let should_drop_ephemeral = item.retention == ContextRetention::Ephemeral
        && item.scope == ContextScope::Turn
        && matches!(trigger, ContextMaintenanceTrigger::AfterModel)
        && age >= 1;
    if should_drop_ephemeral {
        return ResidencyOutcome {
            state: ContextState::Dropped,
            reason: format!(
                "ephemeral {:?} observation dropped after model turn {}",
                item.kind, turn
            ),
            relevance: item.relevance,
        };
    }

    let ttl_expired = item.retention == ContextRetention::Ephemeral && age > config.turn_ttl_ticks;
    if ttl_expired {
        return ResidencyOutcome {
            state: ContextState::Dropped,
            reason: format!(
                "ephemeral TTL expired (age {age} > {} ticks)",
                config.turn_ttl_ticks
            ),
            relevance: item.relevance,
        };
    }

    let breakdown = score_item_with_breakdown(item, focus, hot_entities, now_tick);
    let stale_task = is_stale_task(item, focus);

    let next = if stale_task && breakdown.total < config.active_threshold {
        ContextState::Archived
    } else if breakdown.total >= config.active_threshold {
        ContextState::Active
    } else if breakdown.total >= config.archive_threshold {
        ContextState::Cooling
    } else if item.retention == ContextRetention::Durable {
        ContextState::Archived
    } else if age > config.turn_ttl_ticks * 4 {
        ContextState::Dropped
    } else {
        ContextState::Archived
    };

    let mut reason = transition_reason(
        item.state,
        next,
        &breakdown,
        config.active_threshold,
        config.archive_threshold,
        age,
        config.turn_ttl_ticks,
    );
    if stale_task && next == ContextState::Archived && item.state != ContextState::Archived {
        reason = format!(
            "task no longer active: archived (score {:.2} < active threshold {:.2})",
            breakdown.total, config.active_threshold
        );
    }

    ResidencyOutcome {
        state: next,
        reason,
        relevance: breakdown.total.min(1.0),
    }
}

fn transition_reason(
    from: ContextState,
    to: ContextState,
    breakdown: &ScoreBreakdown,
    active_threshold: f32,
    archive_threshold: f32,
    age: u64,
    turn_ttl_ticks: u64,
) -> String {
    match (from, to) {
        (_, ContextState::Active) => format!(
            "reactivated: score {:.2} >= active threshold {active_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Active, ContextState::Cooling) => format!(
            "decayed: score {:.2} below active threshold {active_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Active, ContextState::Archived) => format!(
            "archived: score {:.2} below archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Cooling, ContextState::Archived) => format!(
            "archived: score {:.2} below archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Archived, ContextState::Cooling) => format!(
            "renewed: score {:.2} >= archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (_, ContextState::Dropped) => format!(
            "dropped: stale (age {age} > ttl x4 = {})",
            turn_ttl_ticks * 4
        ),
        (from, to) => format!("state {from:?} -> {to:?}"),
    }
}
