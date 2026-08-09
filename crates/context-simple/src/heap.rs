use agent_contracts::{ContextItem, ContextItemSummary};

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
