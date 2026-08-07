use agent_contracts::{ContextItem, ContextItemId, ContextItemSummary};

/// Locate an item by id. Linear scan is fine for the bounded heap sizes this
/// engine targets; a real id -> index map is a later optimization, not a
/// requirement at this scale.
pub(crate) fn find_index(items: &[ContextItem], id: ContextItemId) -> Option<usize> {
    items.iter().position(|item| item.id == id)
}

/// Project items into bounded UI/replay summaries.
pub(crate) fn to_summaries(items: &[ContextItem]) -> Vec<ContextItemSummary> {
    items
        .iter()
        .map(|item| ContextItemSummary {
            id: item.id,
            kind: item.kind,
            scope: item.scope,
            scope_id: item.scope_id,
            state: item.state,
            importance: item.importance,
            relevance: item.relevance,
            created_tick: item.created_tick,
            created_turn: item.created_turn,
            last_access_turn: item.last_access_turn,
            access_count: item.access_count,
            dependencies: item.dependencies.clone(),
            source: item.source.clone(),
        })
        .collect()
}
