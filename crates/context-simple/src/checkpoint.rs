use agent_contracts::{AgentError, AgentResult, ContextItemId};
use serde_json::Value;

use crate::engine::State;

/// Export the runtime state (items, focus, counters, queues) as JSON, kept
/// separate from the event journal.
pub(crate) fn serialize(state: &State) -> AgentResult<Value> {
    serde_json::to_value(state)
        .map_err(|e| AgentError::Context(format!("checkpoint serialize: {e}")))
}

/// Replace runtime state from a previously exported checkpoint.
pub(crate) fn deserialize(data: Value) -> AgentResult<State> {
    let mut state: State = serde_json::from_value(data)
        .map_err(|e| AgentError::Context(format!("checkpoint restore: {e}")))?;
    if state.user_hot_entities.is_empty()
        && state.tool_hot.is_empty()
        && !state.hot_entities.is_empty()
    {
        state.user_hot_entities = state.hot_entities.clone();
    }
    state.rebuild_hot_entities();
    state.sync_catalog();
    Ok(state)
}

/// Structural validation every restore runs before the state becomes live.
/// The engine maintains these invariants at runtime; a checkpoint that
/// violates them is corrupt or hostile, not a legacy format. All checks are
/// in-memory and O(total ids/scopes): a duplicate id inside one location
/// (the heap index hides these with last-wins), an id owned by more than
/// one location, a scope whose parent is missing from the tree, and an item
/// whose scope reference is missing. Store-file existence is deliberately
/// out of scope here — startup reconcile owns blob recovery.
pub(crate) fn validate(state: &State) -> AgentResult<()> {
    let mut owners: std::collections::HashMap<ContextItemId, &'static str> =
        std::collections::HashMap::new();

    for item in state.items.iter() {
        if let Some(owner) = owners.insert(item.id, "heap") {
            return Err(violation(format!(
                "item {} appears more than once in the heap (also marked {owner})",
                item.id
            )));
        }
    }
    for item in &state.eviction_buffer {
        if let Some(owner) = owners.insert(item.id, "eviction buffer") {
            return Err(violation(format!(
                "item {} is owned by both {owner} and the eviction buffer",
                item.id
            )));
        }
    }
    for entry in state.external.iter() {
        if let Some(owner) = owners.insert(entry.item_id, "external map") {
            return Err(violation(format!(
                "item {} is owned by both {owner} and the external map",
                entry.item_id
            )));
        }
    }

    for scope in state.scopes.iter() {
        if let Some(parent) = scope.parent
            && state.scopes.by_id(parent).is_none()
        {
            return Err(violation(format!(
                "scope {} references missing parent scope {parent}",
                scope.id
            )));
        }
    }

    for item in state.items.iter().chain(state.eviction_buffer.iter()) {
        if let Some(scope_id) = item.scope_id
            && state.scopes.by_id(scope_id).is_none()
        {
            return Err(violation(format!(
                "item {} references missing scope {scope_id}",
                item.id
            )));
        }
    }
    for entry in state.external.iter() {
        if let Some(scope_id) = entry.scope_id
            && state.scopes.by_id(scope_id).is_none()
        {
            return Err(violation(format!(
                "external entry {} references missing scope {scope_id}",
                entry.item_id
            )));
        }
    }

    if state.catalog.len() != owners.len() {
        return Err(violation(format!(
            "context catalog has {} ids but the body stores own {}",
            state.catalog.len(),
            owners.len()
        )));
    }
    for id in owners.keys() {
        if !state.catalog.contains(*id) {
            return Err(violation(format!(
                "item {id} is in a body store but missing from the context catalog"
            )));
        }
    }

    Ok(())
}

fn violation(message: String) -> AgentError {
    AgentError::Context(format!("checkpoint restore validation: {message}"))
}
