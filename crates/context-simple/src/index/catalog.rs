//! Canonical `ContextCatalog`: one `item_id -> body location` directory plus
//! the query indexes GC recall and `context.search` share.
//!
//! Authority metadata stays on the body (`ContextItem` / `ExternalizedContext`).
//! GC moves `CatalogLocation`; it does not copy lifecycle fields into a second
//! record. The catalog is derived navigation state (like heap/external
//! indexes): checkpoints serialize the three body stores and rebuild this
//! directory on restore.
//!
//! Hot-path updates are incremental: heap/external dirty ids upsert or
//! remove one record. Wholesale rebuild remains the restore / GC-sweep
//! path and the safety net when an unmarked length change is detected.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextKind, ContextResidency, ContextScope,
    ContextSearchQuery, ExternalizedContext, TaskId, TextIndex,
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

/// Navigation snapshot used only to unindex a dirty id. Not authority.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogKeys {
    task_id: Option<TaskId>,
    scope: ContextScope,
    kind: ContextKind,
    entities: Vec<String>,
    labels: Vec<String>,
    residency: ContextResidency,
    attention: AttentionState,
    live: bool,
    /// Path identity, when the item carries one (`path@rev` sources).
    file_path: Option<String>,
    /// Bounded body text for the shared inverted index: a content prefix
    /// for resident/warm items, the stored summary for externalized ones.
    /// Never a full 16 KiB body — the kernel caps tokens per document.
    body_text: String,
}

/// 正文进倒排的前缀上限：命中靠前导 token，全文召回走 fetch/admit。
const INDEX_BODY_PREFIX_CHARS: usize = 512;

impl CatalogKeys {
    fn from_item(item: &ContextItem) -> Self {
        Self {
            task_id: item.task_id,
            scope: item.scope,
            kind: item.kind,
            entities: item.entities.clone(),
            labels: item
                .tags
                .iter()
                .map(|tag| tag.as_str().to_string())
                .collect(),
            residency: item.residency,
            attention: item.attention,
            live: item.semantic.is_live(),
            file_path: item.file_path.clone(),
            body_text: item.content.chars().take(INDEX_BODY_PREFIX_CHARS).collect(),
        }
    }

    fn from_entry(entry: &ExternalizedContext) -> Self {
        Self {
            task_id: entry.task_id,
            scope: entry.scope,
            kind: entry.kind,
            entities: entry.entities.clone(),
            labels: entry
                .tags
                .iter()
                .map(|tag| tag.as_str().to_string())
                .collect(),
            residency: entry.residency,
            attention: entry.attention,
            live: entry.semantic.is_live(),
            file_path: entry.file_path.clone(),
            body_text: entry.context_ref.summary.clone(),
        }
    }
}

/// Dirty ids plus a rebuild flag drained into [`ContextCatalog::sync`].
#[derive(Debug, Default)]
pub(crate) struct CatalogDirty {
    ids: HashSet<ContextItemId>,
    rebuild: bool,
}

impl CatalogDirty {
    pub(crate) fn mark(&mut self, id: ContextItemId) {
        if !self.rebuild {
            self.ids.insert(id);
        }
    }

    pub(crate) fn mark_rebuild(&mut self) {
        self.rebuild = true;
        self.ids.clear();
    }

    pub(crate) fn merge(&mut self, rebuild: bool, ids: HashSet<ContextItemId>) {
        if rebuild {
            self.mark_rebuild();
            return;
        }
        if self.rebuild {
            return;
        }
        self.ids.extend(ids);
    }

    fn is_empty(&self) -> bool {
        !self.rebuild && self.ids.is_empty()
    }
}

/// Unified directory + provider-owned query indexes over every residency.
#[derive(Debug, Default)]
pub(crate) struct ContextCatalog {
    by_id: HashMap<ContextItemId, CatalogLocation>,
    records: HashMap<ContextItemId, CatalogKeys>,
    by_task: HashMap<TaskId, Vec<ContextItemId>>,
    by_scope: HashMap<ContextScope, Vec<ContextItemId>>,
    by_kind: HashMap<ContextKind, Vec<ContextItemId>>,
    by_entity: HashMap<String, Vec<ContextItemId>>,
    by_label: HashMap<String, Vec<ContextItemId>>,
    by_residency: HashMap<ContextResidency, Vec<ContextItemId>>,
    by_attention: HashMap<AttentionState, Vec<ContextItemId>>,
    live: HashSet<ContextItemId>,
    /// Shared-kernel inverted index (`agent_contracts::search`) over
    /// entities/labels/path/body text. Mechanics only — ranking below is
    /// this catalog's own policy.
    text: TextIndex,
    /// `ContextItemId -> doc handle` for index removal. Handles are arena
    /// positions in `text_docs`; removal leaves holes until the next
    /// wholesale rebuild resets the arena (ids are unique UUIDs, so slots
    /// never collide within a generation).
    text_ids: HashMap<ContextItemId, u32>,
    text_docs: Vec<ContextItemId>,
    fingerprint: CatalogFingerprint,
    #[cfg(test)]
    last_sync_rebuilt: bool,
}

impl ContextCatalog {
    /// Apply dirty ids, or rebuild when the stores were replaced wholesale
    /// or an unmarked length change is detected. Same-length field edits
    /// (semantic/tags/attention) must be in `dirty` so search never ranks
    /// from a stale label/lifecycle bucket.
    pub(crate) fn sync(
        &mut self,
        heap: &[ContextItem],
        warm: &[ContextItem],
        stored: &[ExternalizedContext],
        event_seq: u64,
        dirty: CatalogDirty,
    ) {
        let fingerprint = CatalogFingerprint {
            heap: heap.len(),
            warm: warm.len(),
            stored: stored.len(),
            event_seq,
        };
        let lengths_unchanged = fingerprint.heap == self.fingerprint.heap
            && fingerprint.warm == self.fingerprint.warm
            && fingerprint.stored == self.fingerprint.stored;
        if dirty.is_empty() && lengths_unchanged {
            self.fingerprint.event_seq = event_seq;
            return;
        }
        if dirty.rebuild || dirty.ids.is_empty() || (!lengths_unchanged && dirty.ids.len() > 64) {
            self.rebuild(heap, warm, stored);
            self.fingerprint = fingerprint;
            return;
        }
        for id in dirty.ids {
            self.apply_id(id, heap, warm, stored);
        }
        if self.by_id.len()
            != heap
                .len()
                .saturating_add(warm.len())
                .saturating_add(stored.len())
            && !self.covers_first_locations(heap, warm, stored)
        {
            self.rebuild(heap, warm, stored);
        } else {
            #[cfg(test)]
            {
                self.last_sync_rebuilt = false;
            }
        }
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
        #[cfg(test)]
        {
            self.last_sync_rebuilt = true;
        }
    }

    #[cfg(test)]
    pub(crate) fn last_sync_rebuilt(&self) -> bool {
        self.last_sync_rebuilt
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
    /// was set, so the caller must residual-scan summaries/uris.
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
            // 命中率优先的候选并集，两层各补对方的盲区：
            // 1) 共享倒排核 —— 多词覆盖、稀有度分层、唯一前缀扩展；
            //    摘要/正文前缀/路径都可命中（旧实现只有实体/标签键）。
            // 2) 旧键包含扫描 —— 整段 needle 作为子串命中实体/标签键，
            //    保留跨 token 边界的旧语义（如 "th ti" 这类碎片）。
            // 候选只是预过滤：store.rs 的命中校验仍逐条独立执行，
            // 因此并集只增召回、不降精度。顺序即排名：覆盖度/稀有度
            // 在前，旧语义补充在后；最终排序由 search_entries 决定。
            let mut ids: Vec<ContextItemId> = Vec::new();
            // 饱和 token 的候选可达 4096 条：去重走 HashSet，避免
            // 候选集平方级的线性 contains。
            let mut seen: HashSet<ContextItemId> = HashSet::new();
            let mut saw_candidate = false;
            for matched in self.text.search(needle) {
                saw_candidate = true;
                let id = self.text_docs[matched.doc as usize];
                if self.live.contains(&id) && seen.insert(id) {
                    ids.push(id);
                }
            }
            for id in self.text_key_ids(needle) {
                saw_candidate = true;
                if self.live.contains(&id) && seen.insert(id) {
                    ids.push(id);
                }
            }
            if !ids.is_empty() {
                if let Some(set) = candidates.as_ref() {
                    ids.retain(|id| set.contains(id));
                }
                return Some(ids);
            }
            if saw_candidate {
                // 文本层有候选但全部语义死亡：索引已经界定了集合，
                // 如实返回空集（终态永不复活，也绝不走残差扫描放大）。
                return match candidates.as_ref() {
                    Some(set) => Some(
                        set.iter()
                            .copied()
                            .filter(|id| self.live.contains(id))
                            .collect(),
                    ),
                    None => Some(Vec::new()),
                };
            }
            // 真正无文本候选：有过滤时过滤桶本身就是候选集（旧行为）；
            // 无过滤时交给调用方残差扫描。
            let set = candidates.as_ref()?;
            return Some(
                set.iter()
                    .copied()
                    .filter(|id| self.live.contains(id))
                    .collect(),
            );
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

    fn apply_id(
        &mut self,
        id: ContextItemId,
        heap: &[ContextItem],
        warm: &[ContextItem],
        stored: &[ExternalizedContext],
    ) {
        if let Some(item) = heap.iter().find(|item| item.id == id) {
            self.upsert_item(item, CatalogLocation::Resident);
            return;
        }
        if let Some(item) = warm.iter().find(|item| item.id == id) {
            self.upsert_item(item, CatalogLocation::Warm);
            return;
        }
        if let Some(entry) = stored.iter().find(|entry| entry.item_id == id) {
            self.upsert_entry(entry);
            return;
        }
        self.remove_id(id);
    }

    fn covers_first_locations(
        &self,
        heap: &[ContextItem],
        warm: &[ContextItem],
        stored: &[ExternalizedContext],
    ) -> bool {
        let mut seen = HashSet::new();
        for item in heap {
            if !seen.insert(item.id) {
                continue;
            }
            if self.location(item.id) != Some(CatalogLocation::Resident) {
                return false;
            }
        }
        for item in warm {
            if !seen.insert(item.id) {
                continue;
            }
            if self.location(item.id) != Some(CatalogLocation::Warm) {
                return false;
            }
        }
        for entry in stored {
            if !seen.insert(entry.item_id) {
                continue;
            }
            if self.location(entry.item_id) != Some(CatalogLocation::Stored) {
                return false;
            }
        }
        self.by_id.len() == seen.len()
    }

    fn clear(&mut self) {
        self.by_id.clear();
        self.records.clear();
        self.by_task.clear();
        self.by_scope.clear();
        self.by_kind.clear();
        self.by_entity.clear();
        self.by_label.clear();
        self.by_residency.clear();
        self.by_attention.clear();
        self.live.clear();
        self.text.clear();
        self.text_ids.clear();
        self.text_docs.clear();
    }

    fn insert_item(&mut self, item: &ContextItem, location: CatalogLocation) {
        if self.by_id.contains_key(&item.id) {
            return;
        }
        self.install(item.id, location, CatalogKeys::from_item(item));
    }

    fn insert_entry(&mut self, entry: &ExternalizedContext) {
        if self.by_id.contains_key(&entry.item_id) {
            return;
        }
        self.install(
            entry.item_id,
            CatalogLocation::Stored,
            CatalogKeys::from_entry(entry),
        );
    }

    fn upsert_item(&mut self, item: &ContextItem, location: CatalogLocation) {
        self.remove_id(item.id);
        self.install(item.id, location, CatalogKeys::from_item(item));
    }

    fn upsert_entry(&mut self, entry: &ExternalizedContext) {
        self.remove_id(entry.item_id);
        self.install(
            entry.item_id,
            CatalogLocation::Stored,
            CatalogKeys::from_entry(entry),
        );
    }

    fn remove_id(&mut self, id: ContextItemId) {
        if let Some(keys) = self.records.remove(&id) {
            self.unindex(id, &keys);
        }
        if let Some(doc) = self.text_ids.remove(&id) {
            self.text.remove(doc);
        }
        self.by_id.remove(&id);
    }

    fn install(&mut self, id: ContextItemId, location: CatalogLocation, keys: CatalogKeys) {
        self.by_id.insert(id, location);
        self.index_keys(id, &keys);
        self.index_text(id, &keys);
        self.records.insert(id, keys);
    }

    /// Feed the shared inverted-index kernel. Fields mirror what a search
    /// hit may legitimately match — entities, labels, path identity, and a
    /// bounded body prefix / stored summary.
    fn index_text(&mut self, id: ContextItemId, keys: &CatalogKeys) {
        let doc = self.text_docs.len() as u32;
        let mut fields: Vec<&str> = Vec::with_capacity(4 + keys.entities.len() + keys.labels.len());
        for entity in &keys.entities {
            fields.push(entity);
        }
        for label in &keys.labels {
            fields.push(label);
        }
        if let Some(path) = keys.file_path.as_deref() {
            fields.push(path);
        }
        fields.push(keys.body_text.as_str());
        if self.text.insert(doc, &fields) {
            self.text_ids.insert(id, doc);
            self.text_docs.push(id);
        }
    }

    fn index_keys(&mut self, id: ContextItemId, keys: &CatalogKeys) {
        if let Some(task) = keys.task_id {
            self.by_task.entry(task).or_default().push(id);
        }
        self.by_scope.entry(keys.scope).or_default().push(id);
        self.by_kind.entry(keys.kind).or_default().push(id);
        for entity in &keys.entities {
            self.by_entity.entry(entity.clone()).or_default().push(id);
        }
        for label in &keys.labels {
            self.by_label.entry(label.clone()).or_default().push(id);
        }
        self.by_residency
            .entry(keys.residency)
            .or_default()
            .push(id);
        self.by_attention
            .entry(keys.attention)
            .or_default()
            .push(id);
        if keys.live {
            self.live.insert(id);
        }
    }

    fn unindex(&mut self, id: ContextItemId, keys: &CatalogKeys) {
        if let Some(task) = keys.task_id {
            remove_from_bucket(&mut self.by_task, task, id);
        }
        remove_from_bucket(&mut self.by_scope, keys.scope, id);
        remove_from_bucket(&mut self.by_kind, keys.kind, id);
        for entity in &keys.entities {
            remove_from_bucket(&mut self.by_entity, entity.clone(), id);
        }
        for label in &keys.labels {
            remove_from_bucket(&mut self.by_label, label.clone(), id);
        }
        remove_from_bucket(&mut self.by_residency, keys.residency, id);
        remove_from_bucket(&mut self.by_attention, keys.attention, id);
        self.live.remove(&id);
    }
}

fn remove_from_bucket<K: Eq + Hash>(
    map: &mut HashMap<K, Vec<ContextItemId>>,
    key: K,
    id: ContextItemId,
) {
    let empty = {
        let Some(bucket) = map.get_mut(&key) else {
            return;
        };
        if let Some(pos) = bucket.iter().position(|entry| *entry == id) {
            bucket.swap_remove(pos);
        }
        bucket.is_empty()
    };
    if empty {
        map.remove(&key);
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
            file_path: None,
            file_revision: None,
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
            file_path: item.file_path.clone(),
            file_revision: item.file_revision.clone(),
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
            "a needle matching nothing anywhere must residual-scan"
        );
    }

    #[test]
    fn multi_word_body_hits_rank_by_coverage() {
        // 命中率核心：正文/摘要词现在可命中。两词都命中的条目排在单词
        // 命中之前；旧实现里这类查询只能依赖残差扫描甚至空手而归。
        let both = item(ContextItemId::new(), "auth-timeout", None);
        let single = item(ContextItemId::new(), "timeout", None);
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[both.clone(), single.clone()], &[], &[]);

        let hits = catalog
            .search_ids(&ContextSearchQuery::new("auth timeout", 8))
            .expect("body tokens are indexed");
        assert_eq!(hits, vec![both.id, single.id]);
    }

    #[test]
    fn whole_needle_substring_over_keys_still_matches() {
        // 旧语义保留：整段 needle 作为子串命中实体键（跨 token 边界的
        // 碎片查询），由候选并集的第二层提供。
        let decision = item(ContextItemId::new(), "AuthService.rs", None);
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[decision.clone()], &[], &[]);

        let hits = catalog
            .search_ids(&ContextSearchQuery::new("hService", 8))
            .expect("legacy key-substring candidates survive");
        assert_eq!(hits, vec![decision.id]);
    }

    #[test]
    fn text_candidates_exclude_semantically_dead_items() {
        let live = item(ContextItemId::new(), "migration plan", None);
        let mut dead = item(ContextItemId::new(), "migration plan draft", None);
        dead.semantic = SemanticState::Tombstoned;
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[live.clone(), dead], &[], &[]);

        let hits = catalog
            .search_ids(&ContextSearchQuery::new("migration plan", 8))
            .expect("the live item matches");
        assert_eq!(hits, vec![live.id], "terminal semantics never resurface");
    }

    #[test]
    fn dead_only_candidates_return_an_empty_set_not_a_residual_scan() {
        // 索引界定了候选（全部语义死亡）时必须返回 Some(空)：残差扫描
        // 是"索引没 bounded"的信号，不得被死条目触发。
        let mut dead = item(ContextItemId::new(), "superseded design", None);
        dead.semantic = SemanticState::Tombstoned;
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[dead], &[], &[]);

        let hits = catalog
            .search_ids(&ContextSearchQuery::new("design", 8))
            .expect("the index bounded the set; empty means no live hit");
        assert!(hits.is_empty());
    }

    #[test]
    fn wholesale_rebuild_resets_the_text_arena() {
        // 稳定性：doc 句柄随重建代际重置，跨代际不得串号。
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[item(ContextItemId::new(), "alpha", None)], &[], &[]);
        catalog.rebuild(&[], &[], &[]);
        let second = item(ContextItemId::new(), "alpha", None);
        catalog.rebuild(&[second.clone()], &[], &[]);

        let hits = catalog
            .search_ids(&ContextSearchQuery::new("alpha", 4))
            .expect("reindexed after rebuild");
        assert_eq!(hits, vec![second.id]);
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

    #[test]
    fn sync_upserts_dirty_ids_without_rebuilding() {
        let resident_id = ContextItemId::new();
        let stored_id = ContextItemId::new();
        let mut heap = vec![item(resident_id, "AuthService.rs", None)];
        let stored = vec![stored(&item(stored_id, "CacheStore.rs", None))];
        let mut catalog = ContextCatalog::default();
        catalog.sync(&heap, &[], &stored, 1, CatalogDirty::default());
        assert!(catalog.last_sync_rebuilt(), "first fill is a rebuild");

        heap[0].semantic = SemanticState::Tombstoned;
        heap[0].attention = AttentionState::Archived;
        let mut dirty = CatalogDirty::default();
        dirty.mark(resident_id);
        catalog.sync(&heap, &[], &stored, 2, dirty);
        assert!(
            !catalog.last_sync_rebuilt(),
            "same-length dirty field edits stay incremental"
        );
        assert!(
            catalog
                .ids_for_attention(AttentionState::Archived)
                .contains(&resident_id),
            "attention bucket follows the dirty upsert"
        );
        assert!(
            !catalog
                .search_ids(&ContextSearchQuery::new("AuthService", 8))
                .expect("entity keys still bound the set")
                .contains(&resident_id),
            "tombstoned items leave the live search set"
        );
        assert_eq!(
            catalog
                .search_ids(&ContextSearchQuery::new("CacheStore", 8))
                .expect("stored live hit remains"),
            vec![stored_id]
        );
    }

    #[test]
    fn unmarked_length_change_rebuilds() {
        let first = ContextItemId::new();
        let second = ContextItemId::new();
        let heap = vec![item(first, "One.rs", None)];
        let mut catalog = ContextCatalog::default();
        catalog.sync(&heap, &[], &[], 1, CatalogDirty::default());

        let heap = vec![item(first, "One.rs", None), item(second, "Two.rs", None)];
        catalog.sync(&heap, &[], &[], 2, CatalogDirty::default());
        assert!(
            catalog.last_sync_rebuilt(),
            "an unmarked push is the rebuild safety net"
        );
        assert_eq!(catalog.len(), 2);
        assert_eq!(
            catalog
                .search_ids(&ContextSearchQuery::new("Two", 8))
                .expect("new entity is indexed after rebuild"),
            vec![second]
        );
    }
}
