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
/// descriptor has no per-item lifecycle stamps (content lives in the store);
/// `externalized_at_tick` stands in for `created_tick` so ordering stays
/// meaningful.
pub(crate) fn external_summary(entry: &ExternalizedContext) -> ContextItemSummary {
    ContextItemSummary {
        id: entry.item_id,
        kind: entry.kind,
        scope: entry.scope,
        scope_id: entry.scope_id,
        attention: entry.attention,
        semantic: entry.semantic,
        importance: 0.0,
        relevance: 0.0,
        created_tick: entry.externalized_at_tick,
        created_turn: 0,
        last_access_turn: 0,
        last_selected_turn: 0,
        access_count: 0,
        dependencies: entry.dependencies.iter().map(|edge| edge.target).collect(),
        keep_alive: false,
        lease_until_turn: None,
        source: Some("externalized".to_string()),
    }
}
