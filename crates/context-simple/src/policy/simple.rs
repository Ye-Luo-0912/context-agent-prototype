use std::collections::HashSet;

use agent_contracts::{ContextItem, ContextRetention, ContextScope, FocusState, ScoreBreakdown};

/// Explicit, feature-wise score. Each component is kept separate so selection
/// reasons and replay traces can explain *why* an item scored what it did.
///
/// `hot_entities` is the current working set of entities (named by the last
/// user message and touched by recent tool observations). It feeds the
/// `entity_affinity` component.
///
/// Recency reads `now_turn` (the user-turn clock) against the item's
/// `last_selected_turn`, never the event sequence: materializing a preview
/// is a read that must not age an item's recency.
pub(crate) fn score_item_with_breakdown(
    item: &ContextItem,
    focus: Option<&FocusState>,
    hot_entities: &[String],
    now_turn: u64,
) -> ScoreBreakdown {
    if item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned {
        return ScoreBreakdown {
            importance: 1.0,
            total: 2.0,
            ..ScoreBreakdown::default()
        };
    }

    let age = now_turn.saturating_sub(item.last_selected_turn) as f32;
    let recency = 1.0 / (1.0 + age / 4.0);
    let access = (item.access_count.min(8) as f32) / 8.0;

    let mut focus_match = 0.0;
    let mut same_task = false;
    if let Some(focus) = focus {
        focus_match = lexical_overlap(&item.content, &focus.current_query)
            .max(lexical_overlap(&item.content, &focus.goal));
        for entity in &focus.active_entities {
            if !entity.is_empty() && item.content.contains(entity) {
                focus_match = (focus_match + 0.18).min(1.0);
            }
        }
        same_task = item.task_id == Some(focus.task_id);
    }

    // How much of this item's entity signature is hot right now. 0 when the
    // item (or the hot set) has no entities. This complements focus_match:
    // focus entities come from the user message, hot_entities additionally
    // covers files/symbols the agent actually touched via tools. The item's
    // signature is precomputed at ingest, so scoring never re-parses content.
    let entity_affinity = if hot_entities.is_empty() || item.entities.is_empty() {
        0.0
    } else {
        let matched = item
            .entities
            .iter()
            .filter(|entity| {
                hot_entities
                    .iter()
                    .any(|hot| hot.contains(*entity) || entity.contains(hot))
            })
            .count();
        0.18 * (matched as f32 / item.entities.len() as f32)
    };

    let scope_bonus = match item.scope {
        ContextScope::Pinned => 1.0,
        ContextScope::Task if same_task => 0.16,
        ContextScope::Session => 0.08,
        ContextScope::Turn => 0.05,
        ContextScope::Message => 0.02,
        _ => 0.0,
    };

    let retention_bonus = match item.retention {
        ContextRetention::Pinned => 1.0,
        ContextRetention::Durable => 0.08,
        ContextRetention::Working => 0.04,
        ContextRetention::Ephemeral => 0.0,
    };

    let importance = 0.24 * item.importance.clamp(0.0, 1.0);
    let focus_component = 0.34 * focus_match;
    let recency_component = 0.14 * recency;
    let access_component = 0.06 * access;
    let total = (importance
        + focus_component
        + recency_component
        + access_component
        + scope_bonus
        + retention_bonus
        + entity_affinity)
        .clamp(0.0, 2.0);

    ScoreBreakdown {
        importance,
        focus_match: focus_component,
        recency: recency_component,
        access: access_component,
        scope_bonus,
        retention_bonus,
        entity_affinity,
        total,
    }
}

pub(crate) fn lexical_overlap(left: &str, right: &str) -> f32 {
    let left = tokens(left);
    let right = tokens(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }

    let common = left.intersection(&right).count() as f32;
    common / right.len().max(1) as f32
}

fn tokens(text: &str) -> HashSet<String> {
    text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '\\')))
        .filter(|s| s.chars().count() >= 2)
        .map(|s| s.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{AttentionState, ContextItemId, ContextKind, SemanticState};

    fn tool_item(content: &str, importance: f32) -> ContextItem {
        let entities = crate::index::entity::extract_entities(content);
        ContextItem {
            id: ContextItemId::new(),
            task_id: None,
            scope_id: None,
            content: content.to_string(),
            kind: ContextKind::ToolObservation,
            scope: ContextScope::Turn,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance,
            relevance: 0.5,
            created_tick: 0,
            last_access_tick: 0,
            access_count: 0,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: Some("tool:shell.exec".to_string()),
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities,
            file_path: None,
            file_revision: None,
        }
    }

    #[test]
    fn entity_affinity_rewards_hot_entities_and_is_zero_when_cold() {
        let item = tool_item("tests passed in AuthService.rs and CacheStore.rs", 0.5);
        let cold = score_item_with_breakdown(&item, None, &[], 100);
        assert_eq!(cold.entity_affinity, 0.0);

        // Half the item's entities are hot -> half the 0.18 weight.
        let half = score_item_with_breakdown(&item, None, &["AuthService.rs".to_string()], 100);
        assert!(
            (half.entity_affinity - 0.09).abs() < 1e-6,
            "got {}",
            half.entity_affinity
        );
        assert!(half.total > cold.total);

        let full = score_item_with_breakdown(
            &item,
            None,
            &["AuthService.rs".to_string(), "CacheStore.rs".to_string()],
            100,
        );
        assert!(
            (full.entity_affinity - 0.18).abs() < 1e-6,
            "got {}",
            full.entity_affinity
        );
        assert!(full.total > half.total);
    }

    #[test]
    fn entity_affinity_is_zero_for_items_without_entities() {
        let item = tool_item("all good", 0.5);
        let score = score_item_with_breakdown(&item, None, &["AuthService.rs".to_string()], 100);
        assert_eq!(score.entity_affinity, 0.0);
    }
}
