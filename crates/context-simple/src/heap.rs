use std::cmp::Ordering;

use agent_contracts::{ContextItem, ContextItemSummary, ExternalizedContext};

/// Project items into bounded UI/replay summaries.
pub(crate) fn to_summaries(items: &[ContextItem]) -> Vec<ContextItemSummary> {
    items
        .iter()
        .map(|item| ContextItemSummary {
            id: item.id,
            kind: item.kind,
            scope: item.scope,
            scope_id: item.scope_id,
            attention: item.attention,
            semantic: item.semantic,
            importance: item.importance,
            relevance: item.relevance,
            created_tick: item.created_tick,
            created_turn: item.created_turn,
            last_access_turn: item.last_access_turn,
            last_selected_turn: item.last_selected_turn,
            access_count: item.access_count,
            // The summary is a projection: it exposes the dependency target
            // ids, not the edge kinds (the typed graph lives on the item).
            dependencies: item.dependencies.iter().map(|edge| edge.target).collect(),
            keep_alive: item.keep_alive,
            lease_until_turn: item.lease_until_turn,
            source: item.source.clone(),
        })
        .collect()
}

/// Project an external store entry into a summary so `inspect` covers the
/// whole logical catalog, not just the resident share. The lightweight
/// descriptor carries the authoritative lifecycle stamps captured at
/// externalize time, so the projection reports the item's real
/// importance/relevance and creation clock instead of zeros or the
/// externalization tick (the body lives in the store, but the authority
/// does not degrade).
pub(crate) fn external_summary(entry: &ExternalizedContext) -> ContextItemSummary {
    ContextItemSummary {
        id: entry.item_id,
        kind: entry.kind,
        scope: entry.scope,
        scope_id: entry.scope_id,
        attention: entry.attention,
        semantic: entry.semantic,
        importance: entry.importance,
        relevance: entry.relevance,
        created_tick: entry.created_tick,
        created_turn: entry.created_turn,
        last_access_turn: entry.last_access_turn,
        last_selected_turn: entry.last_selected_turn,
        access_count: entry.access_count,
        dependencies: entry.dependencies.iter().map(|edge| edge.target).collect(),
        keep_alive: false,
        lease_until_turn: None,
        // 来源权威随条目外部化保留，inspect 如实显示原始来源，而不是
        // 一个固定的 "externalized" 占位——否则外部化会抹掉来源信息。
        source: entry.source.clone(),
    }
}

/// One catalog row under its `(created_tick, stream-order)` key. The order
/// tiebreaker keeps equal ticks deterministic — matching a stable sort
/// followed by truncate — without requiring `ContextItemSummary` to be
/// `Ord`.
struct CatalogEntry {
    key: (u64, usize),
    summary: ContextItemSummary,
}

impl PartialEq for CatalogEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for CatalogEntry {}

impl PartialOrd for CatalogEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CatalogEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

/// Keep only the `limit` summaries with the smallest `created_tick` while
/// streaming, so memory stays O(limit) even when the external store is
/// large (M14 resource policy: a model-driven catalog call must not cost
/// proportional to logical history size). Equal ticks resolve by stream
/// order (heap slots, then buffer order, then externalization order),
/// matching a stable sort followed by truncate. `limit == 0` yields
/// nothing.
pub(crate) fn bounded_catalog(
    limit: usize,
    summaries: impl Iterator<Item = ContextItemSummary>,
) -> Vec<ContextItemSummary> {
    if limit == 0 {
        return Vec::new();
    }
    // A max-heap of the `limit` smallest keys seen so far: pushing an
    // entry only ever evicts the current largest, so the heap never grows
    // past `limit` regardless of the store's history size.
    let mut top = std::collections::BinaryHeap::with_capacity(limit.min(64));
    for (order, summary) in summaries.enumerate() {
        let entry = CatalogEntry {
            key: (summary.created_tick, order),
            summary,
        };
        if top.len() < limit {
            top.push(entry);
        } else if entry.key < top.peek().expect("non-empty under the guard").key {
            top.pop();
            top.push(entry);
        }
    }
    let mut rows: Vec<CatalogEntry> = top.into_vec();
    rows.sort();
    rows.into_iter().map(|row| row.summary).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        AttentionState, ContextItemId, ContextKind, ContextScope, SemanticState,
    };

    fn summary(tick: u64, tag: &str) -> ContextItemSummary {
        ContextItemSummary {
            id: ContextItemId::new(),
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            scope_id: None,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 0.0,
            relevance: 0.0,
            created_tick: tick,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            access_count: 0,
            dependencies: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: Some(tag.to_string()),
        }
    }

    #[test]
    fn bounded_catalog_keeps_the_smallest_ticks_up_to_the_limit() {
        let kept = bounded_catalog(
            3,
            [9, 2, 7, 4, 1, 8, 3, 6, 5]
                .into_iter()
                .map(|t| summary(t, "x")),
        );
        let ticks: Vec<u64> = kept.iter().map(|s| s.created_tick).collect();
        assert_eq!(ticks, vec![1, 2, 3], "the three smallest ticks, ascending");
    }

    #[test]
    fn bounded_catalog_returns_everything_when_the_limit_is_large_enough() {
        let kept = bounded_catalog(10, [5, 3, 8].into_iter().map(|t| summary(t, "x")));
        let ticks: Vec<u64> = kept.iter().map(|s| s.created_tick).collect();
        assert_eq!(ticks, vec![3, 5, 8], "ascending when nothing is cut");
    }

    #[test]
    fn bounded_catalog_zero_limit_is_empty() {
        assert!(bounded_catalog(0, [summary(1, "a"), summary(2, "b")].into_iter()).is_empty());
    }

    #[test]
    fn bounded_catalog_resolves_equal_ticks_by_stream_order() {
        // Equal ticks must be deterministic — the first `limit` stream
        // entries win, exactly like a stable sort followed by truncate.
        let kept = bounded_catalog(
            2,
            [summary(1, "a"), summary(1, "b"), summary(1, "c")].into_iter(),
        );
        let tags: Vec<Option<String>> = kept.iter().map(|s| s.source.clone()).collect();
        assert_eq!(
            tags,
            vec![Some("a".to_string()), Some("b".to_string())],
            "the first two stream entries hold the equal ticks"
        );
    }
}
