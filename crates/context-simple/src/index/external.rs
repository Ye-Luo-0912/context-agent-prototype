//! The external context map owns its secondary indexes.
//!
//! `State.external` used to be a bare `Vec<ExternalizedContext>` that any
//! module could mutate while the map's id/entity indexes silently drifted.
//! `ExternalMap` binds the storage and the indexes together: every
//! *structural* mutation (push, retain, wholesale replace, restore) goes
//! through a method that updates the indexes in the same step, so a stale
//! index is a type error instead of a runtime heuristic. Non-indexed field
//! mutations (residency aging, access stamps, tags) remain reachable
//! through `get_mut` / `&mut` iteration, which cannot affect any index
//! bucket.
//!
//! The id index turns the model-driven retrieval loop
//! (`inspect_external` / `fetch_external`, called per item from the
//! `context.manage` tool) from a linear scan into O(1) lookups. The entity
//! index (exact-match buckets) accelerates the GC's hot-entity recall: the
//! common exact-match case is answered per hot entity in O(bucket) instead
//! of a full scan. Substring-tolerant overlaps (hot `AuthService.rs` vs an
//! entry entity `src/auth/AuthService.rs`) are *not* indexable with exact
//! keys, so the recall pass keeps a residual scan over entries the index
//! did not already cover — coverage is preserved, exact matches are fast.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use agent_contracts::{ContextItemId, ContextResidency, ExternalizedContext};

#[derive(Debug, Default)]
pub(crate) struct ExternalMap {
    entries: Vec<ExternalizedContext>,
    /// entry id -> slot in `self.entries`.
    id_index: HashMap<ContextItemId, usize>,
    /// entity -> ids, insertion order = externalization order.
    entity_index: HashMap<String, Vec<ContextItemId>>,
    /// Expected `self.entries.len()`; a mismatch means the map changed
    /// without the index and a rebuild is required before use.
    map_len: usize,
    /// Cold-entry count, so `diagnostics` answers in O(1) instead of
    /// scanning a store that grows with logical history size.
    cold_entries: usize,
    /// External-entry count (the reference-only tail of the store).
    external_entries: usize,
}

impl ExternalMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Push one externalized entry and index it at its slot. The entry
    /// must be fully formed (entities captured) before the push.
    pub(crate) fn push(&mut self, entry: ExternalizedContext) {
        let slot = self.entries.len();
        self.id_index.insert(entry.item_id, slot);
        for entity in &entry.entities {
            self.entity_index
                .entry(entity.clone())
                .or_default()
                .push(entry.item_id);
        }
        let (cold, external) = Self::residency_delta(&entry);
        self.cold_entries = self.cold_entries.saturating_add(cold);
        self.external_entries = self.external_entries.saturating_add(external);
        self.entries.push(entry);
        self.map_len = self.entries.len();
    }

    /// Age `Cold` entries toward `External` after the configured number of
    /// full GC *generations* without access, maintaining the residency
    /// counts. The unit is `gc_epoch`, which only a full GC pass
    /// increments — comparing against the tick counter would let unrelated
    /// runtime activity (ingest, maintain, materialize) age entries out
    /// without a single GC pass having run. Entries restored from
    /// pre-epoch checkpoints (`last_access_gc_epoch == None`) start fresh
    /// at the current epoch instead of aging out instantly.
    pub(crate) fn age_entries(&mut self, ttl_generations: u64, gc_epoch: u64) -> usize {
        let mut aged = 0usize;
        for entry in &mut self.entries {
            if entry.residency != ContextResidency::Cold {
                continue;
            }
            let Some(last) = entry.last_access_gc_epoch else {
                // A pre-epoch checkpoint has no generation anchor. Establish
                // one on its first full GC so subsequent passes can age it
                // normally; merely substituting `gc_epoch` here would leave
                // the field `None` forever and make the entry immortal.
                entry.last_access_gc_epoch = Some(gc_epoch);
                continue;
            };
            let idle = gc_epoch.saturating_sub(last);
            if idle >= ttl_generations {
                entry.residency = ContextResidency::External;
                self.cold_entries = self.cold_entries.saturating_sub(1);
                self.external_entries += 1;
                aged += 1;
            }
        }
        aged
    }

    /// O(1) count of `Cold` entries (diagnostics; the store can grow with
    /// logical history, so scanning it per diagnostics call would let a
    /// model-driven meta-tool cost grow with history).
    pub(crate) fn cold_entries(&self) -> usize {
        self.cold_entries
    }

    /// O(1) count of `External` (reference-only) entries.
    pub(crate) fn external_entries(&self) -> usize {
        self.external_entries
    }

    /// How this entry contributes to the Cold/External counts. A free
    /// function so callers can update the counters field-by-field without
    /// a whole-`self` borrow (the counts and the entry vec are disjoint).
    fn residency_delta(entry: &ExternalizedContext) -> (usize, usize) {
        match entry.residency {
            ContextResidency::Cold => (1, 0),
            ContextResidency::External => (0, 1),
            ContextResidency::Resident | ContextResidency::Warm => (0, 0),
        }
    }

    /// Filter the map in place (GC commit: recalled entries leave the map)
    /// and rebuild both indexes from the survivors. O(n) at commit time —
    /// the GC commit is not the hot path, and a partial index update would
    /// risk drifting on the entity buckets.
    pub(crate) fn retain(&mut self, keep: impl FnMut(&ExternalizedContext) -> bool) {
        self.entries.retain(keep);
        self.rebuild_indexes();
    }

    /// Take the map out for wholesale processing (storage-GC commit); the
    /// caller must `replace_all` the survivors before any indexed query
    /// runs again.
    pub(crate) fn take_all(&mut self) -> Vec<ExternalizedContext> {
        self.id_index.clear();
        self.entity_index.clear();
        self.map_len = 0;
        std::mem::take(&mut self.entries)
    }

    /// Replace the whole map (storage-GC commit, restore) and rebuild the
    /// indexes.
    pub(crate) fn replace_all(&mut self, entries: Vec<ExternalizedContext>) {
        self.entries = entries;
        self.rebuild_indexes();
    }

    /// O(1) lookup by id (the model's per-item retrieval loop).
    pub(crate) fn get(&self, id: ContextItemId) -> Option<&ExternalizedContext> {
        self.id_index
            .get(&id)
            .and_then(|slot| self.entries.get(*slot))
    }

    /// O(1) mutable lookup for *non-indexed* fields only (access stamps,
    /// residency aging). Mutating `item_id` or `entities` through this
    /// handle would silently desync the indexes.
    pub(crate) fn get_mut(&mut self, id: ContextItemId) -> Option<&mut ExternalizedContext> {
        self.id_index
            .get(&id)
            .copied()
            .and_then(move |slot| self.entries.get_mut(slot))
    }

    /// Immutable iteration over every entry in externalization order
    /// (restore validation scans ownership against the live id index).
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, ExternalizedContext> {
        self.entries.iter()
    }

    /// Whether the map holds no entries (the GC pass-skip check and
    /// restore validation use this).
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Exact-match entity bucket: entries whose captured entity signature
    /// contains `entity`. Used by the GC recall fast path.
    pub(crate) fn ids_for_entity(&self, entity: &str) -> &[ContextItemId] {
        self.entity_index
            .get(entity)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Safety net for direct pushes that bypassed the map (tests): rebuild
    /// when the map length no longer matches the indexed length. Field
    /// edits to indexed dimensions (`entities`, `item_id`) are caught by
    /// the structured methods, not by this guard.
    pub(crate) fn ensure_consistent(&mut self) {
        if self.map_len != self.entries.len() {
            self.rebuild_indexes();
        }
    }

    fn rebuild_indexes(&mut self) {
        self.id_index.clear();
        self.entity_index.clear();
        self.cold_entries = 0;
        self.external_entries = 0;
        for (slot, entry) in self.entries.iter().enumerate() {
            self.id_index.insert(entry.item_id, slot);
            for entity in &entry.entities {
                self.entity_index
                    .entry(entity.clone())
                    .or_default()
                    .push(entry.item_id);
            }
            let (cold, external) = Self::residency_delta(entry);
            self.cold_entries = self.cold_entries.saturating_add(cold);
            self.external_entries = self.external_entries.saturating_add(external);
        }
        self.map_len = self.entries.len();
    }
}

/// Checkpoints serialize only the entries; the in-memory indexes are
/// derived state and rebuilt on restore.
impl Serialize for ExternalMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.entries.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExternalMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let entries = Vec::<ExternalizedContext>::deserialize(deserializer)?;
        let mut map = Self::new();
        map.replace_all(entries);
        Ok(map)
    }
}

impl std::ops::Deref for ExternalMap {
    type Target = Vec<ExternalizedContext>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl<'a> IntoIterator for &'a ExternalMap {
    type Item = &'a ExternalizedContext;
    type IntoIter = std::slice::Iter<'a, ExternalizedContext>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

impl<'a> IntoIterator for &'a mut ExternalMap {
    type Item = &'a mut ExternalizedContext;
    type IntoIter = std::slice::IterMut<'a, ExternalizedContext>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use agent_contracts::{
        ContextItemId, ContextKind, ContextRef, ContextResidency, ContextRetention, ContextScope,
        SemanticState,
    };

    fn entry(id: ContextItemId, entities: &[&str]) -> ExternalizedContext {
        ExternalizedContext {
            item_id: id,
            task_id: None,
            scope_id: None,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: agent_contracts::AttentionState::Archived,
            semantic: SemanticState::Live,
            context_ref: ContextRef {
                uri: format!("context://run/{id}"),
                item_id: id,
                kind: ContextKind::Note,
                scope: ContextScope::Task,
                summary: String::new(),
                created_tick: 0,
            },
            externalized_at_tick: 0,
            last_access_tick: 0,
            residency: ContextResidency::Cold,
            entities: entities.iter().map(|s| s.to_string()).collect(),
            tags: Vec::new(),
            dependencies: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            last_access_gc_epoch: Some(0),
            blob_checksum: None,
            source: None,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 0,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            access_count: 0,
            last_access_signal: agent_contracts::AccessSignal::None,
            search_reinforce_count: 0,
            gc_generation: 0,
            evicted_at_tick: None,
            file_path: None,
            file_revision: None,
        }
    }

    #[test]
    fn push_then_id_lookup_is_o1_and_indexed() {
        let mut map = ExternalMap::new();
        let a = ContextItemId::new();
        let b = ContextItemId::new();
        map.push(entry(a, &["AuthService.rs"]));
        map.push(entry(b, &["CacheStore.rs"]));

        assert_eq!(map.get(a).unwrap().item_id, a);
        assert_eq!(map.get(b).unwrap().item_id, b);
        assert!(map.get(a).is_some());
        assert!(map.get(ContextItemId::new()).is_none());
        assert_eq!(map.ids_for_entity("AuthService.rs"), &[a]);
        assert_eq!(map.ids_for_entity("CacheStore.rs"), &[b]);
    }

    #[test]
    fn retain_drops_ids_and_keeps_the_index_honest() {
        let mut map = ExternalMap::new();
        let a = ContextItemId::new();
        let b = ContextItemId::new();
        map.push(entry(a, &["shared.rs"]));
        map.push(entry(b, &["shared.rs"]));

        let removed: HashSet<ContextItemId> = [a].into();
        map.retain(|entry| !removed.contains(&entry.item_id));
        assert_eq!(map.len(), 1);
        assert!(map.get(a).is_none());
        assert_eq!(map.get(b).unwrap().item_id, b);
        // The entity bucket must have dropped the removed id too.
        assert_eq!(map.ids_for_entity("shared.rs"), &[b]);
    }

    #[test]
    fn take_and_replace_keep_id_and_slot_in_sync() {
        let mut map = ExternalMap::new();
        let a = ContextItemId::new();
        let b = ContextItemId::new();
        map.push(entry(a, &["a.rs"]));
        map.push(entry(b, &["b.rs"]));

        let mut all = map.take_all();
        all.retain(|entry| entry.item_id != a);
        map.replace_all(all);
        assert_eq!(map.len(), 1);
        assert!(map.get(a).is_none());
        assert_eq!(map.get(b).unwrap().item_id, b);
        assert_eq!(map.ids_for_entity("b.rs"), &[b]);
        assert!(map.ids_for_entity("a.rs").is_empty());
    }

    #[test]
    fn non_indexed_stamps_do_not_require_a_rebuild() {
        let mut map = ExternalMap::new();
        let a = ContextItemId::new();
        map.push(entry(a, &["AuthService.rs"]));

        // fetch_external's access stamp: non-indexed fields, safe to
        // mutate in place without touching the indexes.
        let stamp = map.get_mut(a).unwrap();
        stamp.last_access_tick = 42;
        stamp.last_access_gc_epoch = Some(7);
        map.ensure_consistent();
        assert_eq!(map.get(a).unwrap().last_access_tick, 42);
        assert_eq!(map.ids_for_entity("AuthService.rs"), &[a]);
    }

    #[test]
    fn serialize_roundtrip_rebuilds_the_indexes() {
        let mut map = ExternalMap::new();
        let a = ContextItemId::new();
        map.push(entry(a, &["AuthService.rs"]));

        let bytes = serde_json::to_vec(&map).unwrap();
        let restored: ExternalMap = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored.get(a).unwrap().item_id, a);
        assert_eq!(restored.ids_for_entity("AuthService.rs"), &[a]);
    }

    #[test]
    fn residency_counts_track_push_aging_and_retain() {
        let mut map = ExternalMap::new();
        let a = ContextItemId::new();
        let b = ContextItemId::new();
        let c = ContextItemId::new();
        map.push(entry(a, &["a.rs"]));
        map.push(entry(b, &["b.rs"]));
        let mut external = entry(c, &["c.rs"]);
        external.residency = ContextResidency::External;
        map.push(external);
        assert_eq!(map.cold_entries(), 2, "two Cold entries counted");
        assert_eq!(map.external_entries(), 1, "one External entry counted");

        // Aging: the test entries carry `last_access_gc_epoch = Some(0)`,
        // so below the TTL nothing ages and past it the Cold entries move
        // to External with the counts following.
        assert_eq!(map.age_entries(5, 1), 0, "below the TTL nothing ages");
        assert_eq!(map.cold_entries(), 2);
        assert_eq!(map.external_entries(), 1);
        assert_eq!(map.age_entries(1, 1), 2, "idle Cold entries age");
        assert_eq!(map.cold_entries(), 0);
        assert_eq!(map.external_entries(), 3);

        // retain rebuilds and recounts the survivors.
        let removed: HashSet<ContextItemId> = [b].into();
        map.retain(|e| !removed.contains(&e.item_id));
        assert_eq!(map.len(), 2);
        assert_eq!(map.cold_entries(), 0);
        assert_eq!(map.external_entries(), 2, "a and c survived as External");
    }
}
