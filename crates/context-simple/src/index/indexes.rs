//! Slot-based secondary indexes over the heap.
//!
//! The heap (`State.items`) stays the storage; the indexes answer the two
//! questions the hot path asks without scanning it:
//!
//! - *which prior items share this entity* (dependency ingest was
//!   O(heap) per new item — the index turns it into O(bucket));
//! - *which items could possibly be selected* (the materializer builds
//!   candidates from the active scope subtree + hot entities instead of
//!   walking the whole heap).
//!
//! Every structural mutation of the heap goes through the index: insert on
//! push, wholesale rebuild after the GC sweep, scope re-stamp on promotion.
//! `ensure_consistent` re-derives the index when a caller mutated the heap
//! without the helpers (direct test pushes, restored checkpoints), so a
//! stale index can never silently drop candidates.

use std::collections::{HashMap, HashSet};

use agent_contracts::{ContextItem, ContextItemId, ScopeId};

#[derive(Debug, Default)]
pub(crate) struct Indexes {
    /// item id -> slot in `State.items`.
    id_index: HashMap<ContextItemId, usize>,
    /// entity -> ids, insertion order = creation order.
    entity_index: HashMap<String, Vec<ContextItemId>>,
    /// scope id -> ids of items stamped with that scope.
    scope_index: HashMap<ScopeId, Vec<ContextItemId>>,
    /// Items without a scope stamp (legacy restored checkpoints) stay
    /// selectable even though they are in no scope bucket.
    unscoped: Vec<ContextItemId>,
    /// Expected `State.items.len()`; a mismatch means the heap changed
    /// without the index and a rebuild is required before use.
    heap_len: usize,
}

impl Indexes {
    pub(crate) fn rebuild(&mut self, items: &[ContextItem]) {
        self.id_index.clear();
        self.entity_index.clear();
        self.scope_index.clear();
        self.unscoped.clear();
        for (slot, item) in items.iter().enumerate() {
            self.insert(item, slot);
        }
        self.heap_len = items.len();
    }

    /// Rebuild only when the heap changed underneath the index (direct
    /// pushes in tests, old restore paths). The production mutation sites
    /// keep it consistent, so this is a safety net, not the hot path.
    pub(crate) fn ensure_consistent(&mut self, items: &[ContextItem]) {
        if self.heap_len != items.len() {
            self.rebuild(items);
        }
    }

    pub(crate) fn insert(&mut self, item: &ContextItem, slot: usize) {
        self.id_index.insert(item.id, slot);
        for entity in &item.entities {
            self.entity_index
                .entry(entity.clone())
                .or_default()
                .push(item.id);
        }
        match item.scope_id {
            Some(scope) => self.scope_index.entry(scope).or_default().push(item.id),
            None => self.unscoped.push(item.id),
        }
        self.heap_len = self.heap_len.max(slot + 1);
    }

    /// An item's authoritative scope stamp changed (promotion on scope
    /// close): move it between scope buckets.
    pub(crate) fn update_scope(
        &mut self,
        id: ContextItemId,
        from: Option<ScopeId>,
        to: Option<ScopeId>,
    ) {
        if from == to {
            return;
        }
        let remove = |bucket: &mut Vec<ContextItemId>| {
            if let Some(pos) = bucket.iter().position(|entry| *entry == id) {
                bucket.swap_remove(pos);
            }
        };
        match from {
            Some(scope) => {
                if let Some(bucket) = self.scope_index.get_mut(&scope) {
                    remove(bucket);
                }
            }
            None => {
                if let Some(pos) = self.unscoped.iter().position(|entry| *entry == id) {
                    self.unscoped.swap_remove(pos);
                }
            }
        }
        match to {
            Some(scope) => self.scope_index.entry(scope).or_default().push(id),
            None => self.unscoped.push(id),
        }
    }

    /// An item's entity signature was re-extracted (content edited): drop
    /// the old signature from the entity buckets and index the new one.
    pub(crate) fn update_entities(
        &mut self,
        id: ContextItemId,
        old_entities: &[String],
        new_entities: &[String],
    ) {
        for entity in old_entities {
            if let Some(bucket) = self.entity_index.get_mut(entity)
                && let Some(pos) = bucket.iter().position(|entry| *entry == id)
            {
                bucket.swap_remove(pos);
            }
        }
        for entity in new_entities {
            self.entity_index
                .entry(entity.clone())
                .or_default()
                .push(id);
        }
    }

    pub(crate) fn get(&self, id: ContextItemId) -> Option<usize> {
        self.id_index.get(&id).copied()
    }

    pub(crate) fn ids_for_entity(&self, entity: &str) -> &[ContextItemId] {
        self.entity_index
            .get(entity)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn ids_for_scope(&self, scope: ScopeId) -> &[ContextItemId] {
        self.scope_index
            .get(&scope)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Candidate item ids for one materialization: the active scope subtree
    /// plus hot-entity matches plus legacy unscoped items. This is the
    /// selection universe — everything that can be scored this snapshot.
    pub(crate) fn candidate_ids(
        &self,
        active_scope_ids: &HashSet<ScopeId>,
        hot_entities: &[String],
    ) -> Vec<ContextItemId> {
        let mut seen: HashSet<ContextItemId> = HashSet::new();
        let mut out = Vec::new();
        for scope in active_scope_ids {
            for id in self.ids_for_scope(*scope) {
                if seen.insert(*id) {
                    out.push(*id);
                }
            }
        }
        for entity in hot_entities {
            if let Some(ids) = self.entity_index.get(entity) {
                for id in ids {
                    if seen.insert(*id) {
                        out.push(*id);
                    }
                }
            }
        }
        for id in &self.unscoped {
            if seen.insert(*id) {
                out.push(*id);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        AttentionState, ContextItem, ContextKind, ContextRetention, ContextScope, SemanticState,
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
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: entities.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn rebuild_then_queries_cover_scopes_entities_and_ids() {
        let session = ScopeId::new();
        let focus = ScopeId::new();
        let a = ContextItemId::new();
        let b = ContextItemId::new();
        let items = vec![
            item(a, Some(session), &["AuthService.rs"]),
            item(b, Some(focus), &["AuthService.rs", "CacheStore.rs"]),
        ];

        let mut indexes = Indexes::default();
        indexes.rebuild(&items);
        assert_eq!(indexes.get(a), Some(0));
        assert_eq!(indexes.get(b), Some(1));
        assert_eq!(indexes.ids_for_entity("AuthService.rs").len(), 2);
        assert_eq!(indexes.ids_for_entity("CacheStore.rs"), &[b]);

        let mut active = HashSet::new();
        active.insert(session);
        let candidates = indexes.candidate_ids(&active, &["AuthService.rs".to_string()]);
        assert!(candidates.contains(&a) && candidates.contains(&b));
    }

    #[test]
    fn scope_change_moves_items_between_buckets() {
        let session = ScopeId::new();
        let task = ScopeId::new();
        let a = ContextItemId::new();
        let mut indexes = Indexes::default();
        indexes.rebuild(&[item(a, Some(session), &[])]);

        indexes.update_scope(a, Some(session), Some(task));
        assert_eq!(indexes.ids_for_scope(session), &[]);
        assert_eq!(indexes.ids_for_scope(task), &[a]);

        // Promotion to no scope (legacy) lands in the unscoped bucket.
        indexes.update_scope(a, Some(task), None);
        let active = HashSet::new();
        assert!(indexes.candidate_ids(&active, &[]).contains(&a));
    }

    #[test]
    fn heap_changed_without_the_index_triggers_a_rebuild() {
        let session = ScopeId::new();
        let a = ContextItemId::new();
        let mut indexes = Indexes::default();
        indexes.rebuild(&[item(a, Some(session), &[])]);

        // A direct push the index never saw: the length guard forces a
        // rebuild, so the new item is not silently missing from queries.
        let b = ContextItemId::new();
        let mut items = vec![item(a, Some(session), &[]), item(b, Some(session), &[])];
        indexes.ensure_consistent(&items);
        assert_eq!(indexes.get(b), Some(1));

        // And a removal is caught the same way.
        items.pop();
        indexes.ensure_consistent(&items);
        assert!(indexes.get(b).is_none());
    }

    #[test]
    fn hot_entities_reach_items_in_any_scope() {
        let focus = ScopeId::new();
        let other = ScopeId::new();
        let a = ContextItemId::new();
        let mut indexes = Indexes::default();
        indexes.rebuild(&[item(a, Some(other), &["shared.rs"])]);

        // The candidate set for an empty active scope still contains the
        // hot-entity match, exactly like the old full-heap scan would.
        let mut active = HashSet::new();
        active.insert(focus);
        let candidates = indexes.candidate_ids(&active, &["shared.rs".to_string()]);
        assert!(candidates.contains(&a));
    }
}
