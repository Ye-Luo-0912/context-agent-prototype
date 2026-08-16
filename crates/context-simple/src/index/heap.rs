//! The heap owns its secondary indexes.
//!
//! `State.items` used to be a bare `Vec<ContextItem>` that any module could
//! mutate while the slot/entity/scope indexes silently drifted — the
//! consistency guard could only catch length changes, never a same-length
//! `entities` or `scope_id` edit. `ContextHeap` binds the storage and the
//! indexes together: every *structural* mutation (push, remove, replace,
//! scope re-stamp, entity re-extraction) goes through a method that updates
//! the indexes in the same step, so a stale index is a type error instead
//! of a runtime heuristic. Non-indexed field mutations (semantic state,
//! tags, keep_alive, lease, access stamps) remain reachable through
//! `iter_mut`, which cannot affect any index bucket.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use agent_contracts::{ContextItem, ContextItemId, ScopeId};

use super::indexes::Indexes;

#[derive(Debug, Default)]
pub(crate) struct ContextHeap {
    items: Vec<ContextItem>,
    indexes: Indexes,
    catalog_dirty: HashSet<ContextItemId>,
    catalog_rebuild: bool,
}

impl ContextHeap {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            indexes: Indexes::default(),
            catalog_dirty: HashSet::new(),
            catalog_rebuild: false,
        }
    }
    /// Push an item and index it at its slot. The item must be fully
    /// formed (entities and scope stamp set) before the push.
    pub(crate) fn push(&mut self, item: ContextItem) {
        self.mark_catalog(item.id);
        let slot = self.items.len();
        self.indexes.insert(&item, slot);
        self.items.push(item);
    }

    /// Replace the whole heap (GC sweep, restore) and rebuild the indexes.
    pub(crate) fn replace_all(&mut self, items: Vec<ContextItem>) {
        self.catalog_rebuild = true;
        self.catalog_dirty.clear();
        self.items = items;
        self.indexes.rebuild(&self.items);
    }

    /// Take the heap out for wholesale processing (the GC sweep moves every
    /// item); the caller must `replace_all` or push the survivors back
    /// before any indexed query runs again.
    pub(crate) fn take_all(&mut self) -> Vec<ContextItem> {
        self.catalog_rebuild = true;
        self.catalog_dirty.clear();
        self.indexes = Indexes::default();
        std::mem::take(&mut self.items)
    }

    /// Re-stamp an item's scope and move it between scope buckets in the
    /// same step — the pairing a length-only guard could never check.
    pub(crate) fn update_scope(
        &mut self,
        index: usize,
        from: Option<ScopeId>,
        to: Option<ScopeId>,
    ) {
        let id = self.items[index].id;
        self.mark_catalog(id);
        self.items[index].scope_id = to;
        self.indexes.update_scope(id, from, to);
    }

    /// Replace an item's entity signature (re-extraction after content
    /// edits) and fix the entity index — the case the old length guard
    /// missed entirely.
    pub(crate) fn update_entities(&mut self, index: usize, entities: Vec<String>) {
        let item = &mut self.items[index];
        let old = std::mem::replace(&mut item.entities, entities);
        let id = item.id;
        self.mark_catalog(id);
        self.indexes
            .update_entities(id, &old, &self.items[index].entities);
    }

    pub(crate) fn drain_catalog_dirty(&mut self) -> (bool, HashSet<ContextItemId>) {
        (
            std::mem::take(&mut self.catalog_rebuild),
            std::mem::take(&mut self.catalog_dirty),
        )
    }

    fn mark_catalog(&mut self, id: ContextItemId) {
        if !self.catalog_rebuild {
            self.catalog_dirty.insert(id);
        }
    }

    pub(crate) fn indexes(&self) -> &Indexes {
        &self.indexes
    }

    /// Safety net for direct pushes that bypassed the heap (tests): rebuild
    /// when the heap length no longer matches the indexed length. Field
    /// edits to indexed dimensions (`entities`, `scope_id`) are caught by
    /// the structured methods, not by this guard.
    pub(crate) fn ensure_consistent(&mut self) {
        self.indexes.ensure_consistent(&self.items);
    }
}

impl<'a> IntoIterator for &'a ContextHeap {
    type Item = &'a ContextItem;
    type IntoIter = std::slice::Iter<'a, ContextItem>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<'a> IntoIterator for &'a mut ContextHeap {
    type Item = &'a mut ContextItem;
    type IntoIter = std::slice::IterMut<'a, ContextItem>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter_mut()
    }
}

/// Checkpoints serialize only the items; the in-memory indexes are derived
/// state and rebuilt on restore.
impl Serialize for ContextHeap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.items.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContextHeap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let items = Vec::<ContextItem>::deserialize(deserializer)?;
        let mut heap = Self::new();
        heap.replace_all(items);
        Ok(heap)
    }
}

impl std::ops::Deref for ContextHeap {
    type Target = Vec<ContextItem>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl ContextHeap {
    /// Mutable iteration for *non-indexed* fields only (semantic state,
    /// tags, keep_alive, lease, access stamps). Mutating `entities` or
    /// `scope_id` through this iterator would silently desync the indexes —
    /// use `update_entities` / `update_scope` instead.
    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, ContextItem> {
        self.items.iter_mut()
    }

    /// Mutable access to the underlying vector for wholesale rewrites that
    /// end in `rebuild_indexes` (restore backfill).
    pub(crate) fn items_mut(&mut self) -> &mut Vec<ContextItem> {
        &mut self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use agent_contracts::{
        AttentionState, ContextItem, ContextItemId, ContextKind, ContextRetention, ContextScope,
        SemanticState,
    };

    fn item(id: ContextItemId, scope: Option<ScopeId>, entities: &[&str]) -> ContextItem {
        ContextItem {
            id,
            task_id: None,
            scope_id: scope,
            content: String::new(),
            kind: ContextKind::ToolObservation,
            scope: ContextScope::Turn,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 0.5,
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
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: entities.iter().map(|s| s.to_string()).collect(),
            file_path: None,
            file_revision: None,
        }
    }

    #[test]
    fn same_length_entity_change_keeps_the_index_honest() {
        let mut heap = ContextHeap::new();
        let session = ScopeId::new();
        let a = ContextItemId::new();
        // Scoped item: candidate_ids reaches it only through the entity
        // index (the active-scope set is empty in this query).
        heap.push(item(a, Some(session), &["AuthService.rs"]));

        // Re-extract the entity signature in place: the heap length does
        // not change, so a length-only guard would miss it entirely. The
        // structured method keeps the entity bucket honest.
        heap.update_entities(0, vec!["CacheStore.rs".to_string()]);
        let candidates = heap
            .indexes()
            .candidate_ids(&HashSet::new(), &["CacheStore.rs".to_string()]);
        assert!(candidates.contains(&a), "new entities are searchable");
        let stale = heap
            .indexes()
            .candidate_ids(&HashSet::new(), &["AuthService.rs".to_string()]);
        assert!(!stale.contains(&a), "the old signature must be dropped");
    }

    #[test]
    fn update_scope_keeps_the_scope_buckets_honest() {
        let mut heap = ContextHeap::new();
        let session = ScopeId::new();
        let task = ScopeId::new();
        let a = ContextItemId::new();
        heap.push(item(a, Some(session), &[]));

        // Same-length re-stamp (promotion): the bucket move happens in the
        // same step as the field write.
        heap.update_scope(0, Some(session), Some(task));
        assert_eq!(heap.indexes().ids_for_scope(session), &[]);
        assert_eq!(heap.indexes().ids_for_scope(task), &[a]);
        assert_eq!(heap[0].scope_id, Some(task));
    }

    #[test]
    fn take_and_replace_keep_id_and_slot_in_sync() {
        let mut heap = ContextHeap::new();
        let a = ContextItemId::new();
        let b = ContextItemId::new();
        heap.push(item(a, None, &[]));
        heap.push(item(b, None, &[]));

        // The GC sweep takes the whole heap, drops one item and puts the
        // survivors back: the id index must track the new slots.
        let mut all = heap.take_all();
        all.retain(|item| item.id != a);
        heap.replace_all(all);
        assert_eq!(heap.len(), 1);
        assert!(heap.indexes().get(a).is_none());
        assert_eq!(heap.indexes().get(b), Some(0));
    }
}
