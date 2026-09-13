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

/// Item ids one stored context checkpoint references as external blobs —
/// the strong recovery roots a later store reconcile must not delete while
/// the checkpoint is retained and restorable (R03).
///
/// A missing `external` payload still degrades to "nothing inline
/// protected" (legacy / unparseable body). A **present** `external_spilled`
/// list is parsed strictly: malformed rows are a structural error so the
/// caller treats the root set as incomplete and defers deletion, rather
/// than silently dropping ids that still name live blobs.
pub(crate) fn recovery_item_ids(data: &Value) -> AgentResult<Vec<ContextItemId>> {
    let mut ids: Vec<ContextItemId> = match serde_json::from_value::<State>(data.clone()) {
        Ok(state) => state.external.iter().map(|entry| entry.item_id).collect(),
        // A spilled-tail checkpoint may carry more external ids than the
        // inline array alone; the id parse below still owns those roots.
        Err(_) => Vec::new(),
    };
    for (id, _) in spilled_entries_from_value(data)? {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Capture writes a 12-hex prefix of the card checksum; accept up to the
/// full 16-hex FNV digest. Longer or non-hex values are not filenames we
/// will ever produce, and must not be interpolated into a store path.
const MAX_SPILL_HASH_CHARS: usize = 16;
/// Bare UUID (36) or `context://run/<uuid>` (~50). Anything longer is not
/// an item id this engine emits.
const MAX_SPILL_ID_CHARS: usize = 80;
/// Fail-closed bound on a present spill list. Far above any checkpoint
/// that fits the 16 MiB payload cap; small enough to refuse a hostile
/// million-row array before card I/O.
const MAX_EXTERNAL_SPILLED_ENTRIES: usize = 262_144;

/// CTX-9 残余：读取 checkpoint 外置尾分片的 `(id, card-hash)` 清单。
/// 缺键（旧 checkpoint / 未分片 capture）→ 空清单。键在但不是合法
/// 数组、或任一现行列格式非法 → 结构性错误，不得静默丢行。
pub(crate) fn spilled_entries_from_value(
    data: &Value,
) -> AgentResult<Vec<(ContextItemId, String)>> {
    let Some(raw) = data.get("external_spilled") else {
        return Ok(Vec::new());
    };
    let Some(list) = raw.as_array() else {
        return Err(violation(
            "external_spilled is present but is not a list".into(),
        ));
    };
    if list.len() > MAX_EXTERNAL_SPILLED_ENTRIES {
        return Err(violation(format!(
            "external_spilled has {} entries; maximum is {MAX_EXTERNAL_SPILLED_ENTRIES}",
            list.len()
        )));
    }
    let mut out = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        out.push(parse_spilled_entry(index, item)?);
    }
    Ok(out)
}

fn parse_spilled_entry(index: usize, item: &Value) -> AgentResult<(ContextItemId, String)> {
    let Some(obj) = item.as_object() else {
        return Err(violation(format!(
            "external_spilled[{index}] is not an object"
        )));
    };
    let id_raw = obj
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| violation(format!("external_spilled[{index}] is missing a string id")))?;
    if id_raw.len() > MAX_SPILL_ID_CHARS {
        return Err(violation(format!(
            "external_spilled[{index}] id exceeds {MAX_SPILL_ID_CHARS} bytes"
        )));
    }
    let id = ContextItemId::parse_ref(id_raw)
        .map_err(|_| violation(format!("external_spilled[{index}] has an invalid id")))?;
    let hash = obj.get("hash").and_then(Value::as_str).ok_or_else(|| {
        violation(format!(
            "external_spilled[{index}] is missing a string hash"
        ))
    })?;
    if hash.is_empty() || hash.len() > MAX_SPILL_HASH_CHARS {
        return Err(violation(format!(
            "external_spilled[{index}] hash length is {}; expected 1..={MAX_SPILL_HASH_CHARS}",
            hash.len()
        )));
    }
    if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(violation(format!(
            "external_spilled[{index}] hash is not hexadecimal"
        )));
    }
    Ok((id, hash.to_ascii_lowercase()))
}

/// Spill ids are a fifth owner: each must be unique against the four live
/// body stores (heap, Warm buffer, Pending retry list, external map) and
/// against the rest of the manifest. A duplicate is a structural
/// contradiction — not a missing card.
pub(crate) fn reject_duplicate_spill_ownership(
    state: &State,
    spilled: &[(ContextItemId, String)],
) -> AgentResult<()> {
    let mut owned: std::collections::HashSet<ContextItemId> = std::collections::HashSet::new();
    for item in state.items.iter() {
        owned.insert(item.id);
    }
    for item in &state.eviction_buffer {
        owned.insert(item.id);
    }
    for item in &state.pending_externalize_retry {
        owned.insert(item.id);
    }
    for entry in state.external.iter() {
        owned.insert(entry.item_id);
    }
    for (id, _) in spilled {
        if !owned.insert(*id) {
            return Err(AgentError::Context(format!(
                "checkpoint external entry {id} is both spilled and already owned"
            )));
        }
    }
    Ok(())
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
    // Retry-list items are live owners of their id (their store write has
    // not landed), so the duplicate check must cover them too — otherwise a
    // checkpoint could hold the same id both mid-retry and elsewhere.
    for item in &state.pending_externalize_retry {
        if let Some(owner) = owners.insert(item.id, "externalize retry list") {
            return Err(violation(format!(
                "item {} is owned by both {owner} and the externalize retry list",
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

    // Build from the raw sequence: ScopeTree's index deliberately cannot
    // prove uniqueness (a duplicate would otherwise be hidden by last-wins).
    let mut parents = std::collections::HashMap::new();
    for scope in state.scopes.iter() {
        if parents.insert(scope.id, scope.parent).is_some() {
            return Err(violation(format!(
                "scope {} appears more than once",
                scope.id
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

    // Iterative three-colour traversal: O(scopes), no recursive stack and no
    // quadratic ancestor walk for deep trees. Every parent chain must end.
    let mut colours = std::collections::HashMap::new();
    for scope in state.scopes.iter() {
        let mut path = Vec::new();
        let mut current = Some(scope.id);
        while let Some(id) = current {
            match colours.get(&id) {
                Some(2) => break,
                Some(1) => {
                    return Err(violation(format!(
                        "scope ancestry contains a cycle at {id}"
                    )));
                }
                _ => {}
            }
            colours.insert(id, 1_u8);
            path.push(id);
            current = parents.get(&id).copied().flatten();
        }
        for id in path {
            colours.insert(id, 2);
        }
    }
    if let Some(active) = state.active_scope_id
        && !parents.contains_key(&active)
    {
        return Err(violation(format!(
            "active scope references missing scope {active}"
        )));
    }

    for item in state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        .chain(state.pending_externalize_retry.iter())
    {
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
