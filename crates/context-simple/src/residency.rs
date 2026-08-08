use agent_contracts::{
    AttentionState, ContextItem, ContextMaintenanceTrigger, ContextRetention, ContextScope,
    FocusState, ScoreBreakdown, SemanticState,
};

use crate::engine::SimpleContextConfig;
use crate::gc::reachability::is_excluded;
use crate::index::task::is_stale_task;
use crate::policy::score_item_with_breakdown;

/// The target state for one item under one maintenance pass: the new
/// attention state, an optional semantic transition (only attention leaves
/// attention; semantic death is a one-way door), the reason and the
/// relevance value the pass should store.
pub(crate) struct ResidencyOutcome {
    pub attention: AttentionState,
    /// `Some` when this pass ends the item's semantic lifecycle (TTL or
    /// staleness): the item becomes Tombstoned and is never resurrected.
    pub semantic: Option<SemanticState>,
    pub reason: String,
    pub relevance: f32,
}

/// Decide the next attention state for one item. This is the per-item state
/// machine of the dynamic working set: pinned items stay active,
/// semantically dead items stay archived forever (their death lives in
/// `SemanticState`, not attention), ephemeral turn observations leave
/// attention after the model turn — and leave the semantic lifecycle
/// entirely once their TTL/staleness passes. Everything else is ranked by
/// the policy score and capped by the stale-task gate.
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
            attention: AttentionState::Active,
            semantic: None,
            reason: "explicitly pinned".to_string(),
            relevance: 1.0,
        };
    }

    if is_excluded(item) {
        return ResidencyOutcome {
            attention: AttentionState::Archived,
            semantic: None,
            reason: "semantically dead: excluded from active attention".to_string(),
            relevance: 0.0,
        };
    }

    let age = now_tick.saturating_sub(item.created_tick);

    // The model consumed the observation: it leaves *attention* (Archived),
    // but stays semantically Live so a later hot-entity match can recall it
    // — attention loss is not death.
    let consumed_ephemeral = item.retention == ContextRetention::Ephemeral
        && item.scope == ContextScope::Turn
        && matches!(trigger, ContextMaintenanceTrigger::AfterModel)
        && age >= 1;
    if consumed_ephemeral {
        return ResidencyOutcome {
            attention: AttentionState::Archived,
            semantic: None,
            reason: format!(
                "ephemeral {:?} observation consumed after model turn {}; leaves attention, stays recallable",
                item.kind, turn
            ),
            relevance: item.relevance,
        };
    }

    // TTL expiry ends the item's *information lifecycle*: Tombstoned. Unlike
    // consumption, this is semantic death — GC will evict it and never
    // resurrect it; only Storage GC may delete the store file.
    let ttl_expired = item.retention == ContextRetention::Ephemeral && age > config.turn_ttl_ticks;
    if ttl_expired {
        return ResidencyOutcome {
            attention: AttentionState::Archived,
            semantic: Some(SemanticState::Tombstoned),
            reason: format!(
                "ephemeral TTL expired (age {age} > {} ticks); tombstoned",
                config.turn_ttl_ticks
            ),
            relevance: item.relevance,
        };
    }

    let breakdown = score_item_with_breakdown(item, focus, hot_entities, now_tick);
    let stale_task = is_stale_task(item, focus);

    let next = if stale_task && breakdown.total < config.active_threshold {
        AttentionState::Archived
    } else if breakdown.total >= config.active_threshold {
        AttentionState::Active
    } else if breakdown.total >= config.archive_threshold {
        AttentionState::Cooling
    } else if item.retention == ContextRetention::Durable {
        AttentionState::Archived
    } else if age > config.turn_ttl_ticks * 4 {
        // A working item that outlived every TTL by a wide margin is not
        // coming back: its lifecycle ends here, terminally.
        return ResidencyOutcome {
            attention: AttentionState::Archived,
            semantic: Some(SemanticState::Tombstoned),
            reason: format!(
                "stale (age {age} > ttl x4 = {}); tombstoned",
                config.turn_ttl_ticks * 4
            ),
            relevance: 0.0,
        };
    } else {
        AttentionState::Archived
    };

    let mut reason = transition_reason(
        item.attention,
        next,
        &breakdown,
        config.active_threshold,
        config.archive_threshold,
    );
    if stale_task && next == AttentionState::Archived && item.attention != AttentionState::Archived
    {
        reason = format!(
            "task no longer active: archived (score {:.2} < active threshold {:.2})",
            breakdown.total, config.active_threshold
        );
    }

    ResidencyOutcome {
        attention: next,
        semantic: None,
        reason,
        relevance: breakdown.total.min(1.0),
    }
}

fn transition_reason(
    from: AttentionState,
    to: AttentionState,
    breakdown: &ScoreBreakdown,
    active_threshold: f32,
    archive_threshold: f32,
) -> String {
    match (from, to) {
        (_, AttentionState::Active) => format!(
            "reactivated: score {:.2} >= active threshold {active_threshold:.2}",
            breakdown.total
        ),
        (AttentionState::Active, AttentionState::Cooling) => format!(
            "decayed: score {:.2} below active threshold {active_threshold:.2}",
            breakdown.total
        ),
        (AttentionState::Active, AttentionState::Archived) => format!(
            "archived: score {:.2} below archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (AttentionState::Cooling, AttentionState::Archived) => format!(
            "archived: score {:.2} below archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (AttentionState::Archived, AttentionState::Cooling) => format!(
            "renewed: score {:.2} >= archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (from, to) => format!("attention {from:?} -> {to:?}"),
    }
}
