//! Canonical `ContextCatalog`: one `item_id -> body location` directory plus
//! the query indexes GC recall and `context.search` share.
//!
//! Authority metadata stays on the body (`ContextItem` / `ExternalizedContext`).
//! GC moves `CatalogLocation`; it does not copy lifecycle fields into a second
//! record. The catalog is derived navigation state (like heap/external
//! indexes): checkpoints serialize the three body stores and rebuild this
//! directory on restore.

use std::collections::{HashMap, HashSet};

use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextKind, ContextResidency, ContextScope,
    ContextSearchQuery, ExternalizedContext, SemanticState, TaskId,
};

/// Where the item's body currently lives. Exactly one location per id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogLocation {
    Resident,
    Warm,
    Stored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CatalogFingerprint {
    heap: usize,
    warm: usize,
    stored: usize,
    event_seq: u64,
}

/// Unified directory + provider-owned query indexes over every residency.
#[derive(Debug, Default)]
pub(crate) struct ContextCatalog {
    by_id: HashMap<ContextItemId, CatalogLocation>,
    by_task: HashMap<TaskId, Vec<ContextItemId>>,
    by_scope: HashMap<ContextScope, Vec<ContextItemId>>,
    by_kind: HashMap<ContextKind, Vec<ContextItemId>>,
    by_entity: HashMap<String, Vec<ContextItemId>>,
    by_label: HashMap<String, Vec<ContextItemId>>,
    by_residency: HashMap<ContextResidency, Vec<ContextItemId>>,
    by_attention: HashMap<AttentionState, Vec<ContextItemId>>,
    live: HashSet<ContextItemId>,
    fingerprint: CatalogFingerprint,
}

impl ContextCatalog {
    /// Rebuild from the three body stores when the fingerprint no longer
    /// matches. `event_seq` catches same-length field edits (semantic/tags)
    /// so search never ranks from a stale label/lifecycle bucket.
    pub(crate) fn sync(
        &mut self,
        heap: &[ContextItem],
        warm: &[ContextItem],
        stored: &[ExternalizedContext],
        event_seq: u64,
    ) {
        let fingerprint = CatalogFingerprint {
            heap: heap.len(),
            warm: warm.len(),
            stored: stored.len(),
            event_seq,
        };
        if self.fingerprint == fingerprint {
            return;
        }
        self.rebuild(heap, warm, stored);
        self.fingerprint = fingerprint;
    }

    pub(crate) fn rebuild(
        &mut self,
        heap: &[ContextItem],
        warm: &[ContextItem],
        stored: &[ExternalizedContext],
    ) {
        self.clear();
        for item in heap {
            self.insert_item(item, CatalogLocation::Resident);
        }
        for item in warm {
            self.insert_item(item, CatalogLocation::Warm);
        }
        for entry in stored {
            self.insert_entry(entry);
        }
    }

    pub(crate) fn location(&self, id: ContextItemId) -> Option<CatalogLocation> {
        self.by_id.get(&id).copied()
    }

    pub(crate) fn contains(&self, id: ContextItemId) -> bool {
        self.by_id.contains_key(&id)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Exact entity bucket across every residency (GC hot-entity recall).
    pub(crate) fn ids_for_entity(&self, entity: &str) -> &[ContextItemId] {
        self.by_entity
            .get(entity)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn ids_for_residency(&self, residency: ContextResidency) -> &[ContextItemId] {
        self.by_residency
            .get(&residency)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Attention buckets are part of the lifecycle index. Graded access
    /// (`CTX-GC-11`) writes signal strength onto the body, not into these
    /// buckets; keep the method so the index is not a write-only field.
    #[allow(dead_code)]
    pub(crate) fn ids_for_attention(&self, attention: AttentionState) -> &[ContextItemId] {
        self.by_attention
            .get(&attention)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Catalog candidate ids for `context.search` (Resident, Warm, Stored).
    ///
    /// `Some(ids)` means the catalog indexes bounded the set. `None` means
    /// the free-text needle did not hit an entity/label key and no filter
    /// was set, so the caller must residual-scan summaries/uris/bodies.
    pub(crate) fn search_ids(&self, query: &ContextSearchQuery) -> Option<Vec<ContextItemId>> {
        let mut candidates: Option<HashSet<ContextItemId>> = None;
        let mut intersect = |bucket: &[ContextItemId]| {
            let incoming: HashSet<ContextItemId> = bucket.iter().copied().collect();
            candidates = Some(match candidates.take() {
                None => incoming,
                Some(set) => set.intersection(&incoming).copied().collect(),
            });
        };

        if let Some(kind) = query.kind {
            intersect(
                self.by_kind
                    .get(&kind)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
        }
        if let Some(scope) = query.scope {
            intersect(
                self.by_scope
                    .get(&scope)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
        }
        if let Some(task) = query.task_id {
            intersect(
                self.by_task
                    .get(&task)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
        }
        if let Some(label) = query.label.as_deref() {
            let ids = self.label_ids(label);
            intersect(&ids);
        }

        let needle = query.query.trim();
        if !needle.is_empty() {
            let text_ids = self.text_key_ids(needle);
            if text_ids.is_empty() {
                // 无过滤、实体/标签键也未命中：摘要/uri/正文只能残差扫描。
                candidates.as_ref()?;
            } else {
                intersect(&text_ids);
            }
        }

        let ids = match candidates {
            Some(set) => set
                .into_iter()
                .filter(|id| self.live.contains(id))
                .collect(),
            None => {
                let mut ids = Vec::new();
                for residency in [
                    ContextResidency::Resident,
                    ContextResidency::Warm,
                    ContextResidency::Cold,
                    ContextResidency::External,
                ] {
                    for id in self.ids_for_residency(residency) {
                        if self.live.contains(id) {
                            ids.push(*id);
                        }
                    }
                }
                ids
            }
        };
        Some(ids)
    }

    /// Stored (Cold/External) candidate ids for store-only ranking tests.
    ///
    /// Production search uses [`Self::search_ids`]. This filter exists so
    /// catalog tests can still assert that a resident-only entity is not a
    /// store hit.
    #[cfg(test)]
    pub(crate) fn stored_search_ids(
        &self,
        query: &ContextSearchQuery,
    ) -> Option<Vec<ContextItemId>> {
        self.search_ids(query)
            .map(|ids| ids.into_iter().filter(|id| self.is_stored(*id)).collect())
    }

    #[cfg(test)]
    fn is_stored(&self, id: ContextItemId) -> bool {
        matches!(self.by_id.get(&id), Some(CatalogLocation::Stored))
    }

    fn label_ids(&self, label: &str) -> Vec<ContextItemId> {
        let needle = label.to_lowercase();
        let mut ids = Vec::new();
        for (key, bucket) in &self.by_label {
            if key.to_lowercase() == needle {
                ids.extend_from_slice(bucket);
            }
        }
        ids
    }

    fn text_key_ids(&self, needle: &str) -> Vec<ContextItemId> {
        let needle = needle.to_lowercase();
        let mut ids = Vec::new();
        for (key, bucket) in &self.by_entity {
            if key.to_lowercase().contains(&needle) {
                ids.extend_from_slice(bucket);
            }
        }
        for (key, bucket) in &self.by_label {
            if key.to_lowercase().contains(&needle) {
                ids.extend_from_slice(bucket);
            }
        }
        ids
    }

    fn clear(&mut self) {
        self.by_id.clear();
        self.by_task.clear();
        self.by_scope.clear();
        self.by_kind.clear();
        self.by_entity.clear();
        self.by_label.clear();
        self.by_residency.clear();
        self.by_attention.clear();
        self.live.clear();
    }

    fn insert_item(&mut self, item: &ContextItem, location: CatalogLocation) {
        if self.by_id.contains_key(&item.id) {
            return;
        }
        self.by_id.insert(item.id, location);
        self.index_keys(
            item.id,
            item.task_id,
            item.scope,
            item.kind,
            &item.entities,
            item.tags.iter().map(|tag| tag.as_str()),
            item.residency,
            item.attention,
            item.semantic,
        );
    }

    fn insert_entry(&mut self, entry: &ExternalizedContext) {
        if self.by_id.contains_key(&entry.item_id) {
            return;
        }
        self.by_id.insert(entry.item_id, CatalogLocation::Stored);
        self.index_keys(
            entry.item_id,
            entry.task_id,
            entry.scope,
            entry.kind,
            &entry.entities,
            entry.tags.iter().map(|tag| tag.as_str()),
            entry.residency,
            entry.attention,
            entry.semantic,
        );
    }

    /// 各生命周期维度各自入索引，参数就是这些正交键，不是漏收成 struct。
    #[allow(clippy::too_many_arguments)]
    fn index_keys<'a>(
        &mut self,
        id: ContextItemId,
        task_id: Option<TaskId>,
        scope: ContextScope,
        kind: ContextKind,
        entities: &[String],
        labels: impl IntoIterator<Item = &'a str>,
        residency: ContextResidency,
        attention: AttentionState,
        semantic: SemanticState,
    ) {
        if let Some(task) = task_id {
            self.by_task.entry(task).or_default().push(id);
        }
        self.by_scope.entry(scope).or_default().push(id);
        self.by_kind.entry(kind).or_default().push(id);
        for entity in entities {
            self.by_entity.entry(entity.clone()).or_default().push(id);
        }
        for label in labels {
            self.by_label.entry(label.to_string()).or_default().push(id);
        }
        self.by_residency.entry(residency).or_default().push(id);
        self.by_attention.entry(attention).or_default().push(id);
        if semantic.is_live() {
            self.live.insert(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        AccessSignal, ContextRef, ContextRetention, CoreLabel, Label, SemanticState,
    };

    fn item(id: ContextItemId, entity: &str, label: Option<Label>) -> ContextItem {
        ContextItem {
            id,
            task_id: None,
            scope_id: None,
            content: entity.to_string(),
            kind: ContextKind::Note,
            scope: ContextScope::Task,
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
            tags: label.into_iter().collect(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: vec![entity.to_string()],
        }
    }

    fn stored(item: &ContextItem) -> ExternalizedContext {
        ExternalizedContext {
            item_id: item.id,
            task_id: item.task_id,
            scope_id: item.scope_id,
            kind: item.kind,
            scope: item.scope,
            retention: item.retention,
            attention: item.attention,
            semantic: item.semantic,
            context_ref: ContextRef {
                uri: format!("context://run/{}", item.id),
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                summary: item.content.clone(),
                created_tick: 0,
            },
            externalized_at_tick: 1,
            last_access_tick: 1,
            residency: ContextResidency::Cold,
            entities: item.entities.clone(),
            tags: item.tags.clone(),
            dependencies: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            last_access_gc_epoch: Some(0),
            blob_checksum: None,
            source: None,
            importance: item.importance,
            relevance: item.relevance,
            created_tick: 0,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            access_count: 0,
            last_access_signal: AccessSignal::None,
            search_reinforce_count: 0,
            gc_generation: 0,
            evicted_at_tick: None,
        }
    }

    #[test]
    fn catalog_assigns_exactly_one_location_per_id() {
        let resident_id = ContextItemId::new();
        let warm_id = ContextItemId::new();
        let stored_id = ContextItemId::new();
        let heap = vec![item(resident_id, "Heap.rs", None)];
        let warm = vec![item(warm_id, "Warm.rs", None)];
        let stored_item = item(
            stored_id,
            "Store.rs",
            Some(Label::core(CoreLabel::Decision)),
        );
        let stored = vec![stored(&stored_item)];

        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&heap, &warm, &stored);

        assert_eq!(catalog.len(), 3);
        assert_eq!(
            catalog.location(resident_id),
            Some(CatalogLocation::Resident)
        );
        assert_eq!(catalog.location(warm_id), Some(CatalogLocation::Warm));
        assert_eq!(catalog.location(stored_id), Some(CatalogLocation::Stored));
        assert_eq!(catalog.ids_for_entity("Store.rs"), &[stored_id]);
        assert_eq!(
            catalog.ids_for_residency(ContextResidency::Cold),
            &[stored_id]
        );
        assert_eq!(catalog.ids_for_attention(AttentionState::Active).len(), 3);
    }

    #[test]
    fn stored_search_ids_use_label_and_entity_indexes() {
        let decision_id = ContextItemId::new();
        let note_id = ContextItemId::new();
        let decision = item(
            decision_id,
            "AuthService.rs",
            Some(Label::core(CoreLabel::Decision)),
        );
        let note = item(note_id, "CacheStore.rs", None);
        let stored = vec![stored(&decision), stored(&note)];

        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[], &[], &stored);

        let by_label = catalog
            .stored_search_ids(&ContextSearchQuery {
                query: String::new(),
                label: Some("decision".into()),
                limit: 8,
                ..ContextSearchQuery::default()
            })
            .expect("label is an indexed filter");
        assert_eq!(by_label, vec![decision_id]);

        let by_entity = catalog
            .stored_search_ids(&ContextSearchQuery::new("AuthService", 8))
            .expect("entity keys bound the set");
        assert_eq!(by_entity, vec![decision_id]);

        assert!(
            catalog
                .stored_search_ids(&ContextSearchQuery::new("not-an-entity", 8))
                .is_none(),
            "summary-only needles must residual-scan"
        );
    }

    #[test]
    fn search_ids_include_resident_hits() {
        let resident_id = ContextItemId::new();
        let stored_id = ContextItemId::new();
        let heap = vec![item(resident_id, "AuthService.rs", None)];
        let stored = vec![stored(&item(stored_id, "CacheStore.rs", None))];
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&heap, &[], &stored);

        let hits = catalog
            .search_ids(&ContextSearchQuery::new("AuthService", 8))
            .expect("entity keys bound the set");
        assert_eq!(hits, vec![resident_id]);
        assert!(
            catalog
                .stored_search_ids(&ContextSearchQuery::new("AuthService", 8))
                .expect("stored filter still runs")
                .is_empty(),
            "a resident-only entity must not appear in stored_search_ids"
        );
    }
}
