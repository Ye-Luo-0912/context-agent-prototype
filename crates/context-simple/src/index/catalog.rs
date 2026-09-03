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
    ContextSearchQuery, ExternalizedContext, SearchCandidates, SearchIncompleteReason, TaskId,
    TextIndex, query_needs_text_residual, tokenize,
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
    /// Bounded text for the shared inverted index: a semantic content prefix
    /// for resident/warm items, a stored semantic summary, or the path@rev
    /// identity of raw evidence. Never a full 16 KiB body.
    body_text: String,
    /// This entry has matchable body text outside `body_text` (or outside
    /// the shared kernel's per-document token budget). Kept per entry so
    /// one long document cannot make every query residual-scan the catalog.
    body_truncated: bool,
}

/// 正文进倒排的前缀上限：命中靠前导 token，全文召回走 fetch/admit。
const INDEX_BODY_PREFIX_CHARS: usize = 512;

impl CatalogKeys {
    fn from_item(item: &ContextItem) -> Self {
        let searchable_body = kind_has_searchable_body(item.kind);
        let body_truncated =
            searchable_body && item.content.chars().count() > INDEX_BODY_PREFIX_CHARS;
        Self {
            task_id: item.task_id,
            scope: item.scope,
            kind: item.kind,
            entities: catalog_entities(item.kind, &item.entities, item.file_path.as_deref()),
            labels: item
                .tags
                .iter()
                .map(|tag| tag.as_str().to_string())
                .collect(),
            residency: item.residency,
            attention: item.attention,
            live: item.semantic.is_live(),
            file_path: item.file_path.clone(),
            body_text: if searchable_body {
                item.content.chars().take(INDEX_BODY_PREFIX_CHARS).collect()
            } else {
                crate::item::raw_evidence_identity(
                    item.kind,
                    item.file_path.as_deref(),
                    item.file_revision.as_deref(),
                )
                .expect("raw evidence kind has an identity")
            },
            body_truncated,
        }
    }

    fn from_entry(entry: &ExternalizedContext) -> Self {
        Self {
            task_id: entry.task_id,
            scope: entry.scope,
            kind: entry.kind,
            entities: catalog_entities(entry.kind, &entry.entities, entry.file_path.as_deref()),
            labels: entry
                .tags
                .iter()
                .map(|tag| tag.as_str().to_string())
                .collect(),
            residency: entry.residency,
            attention: entry.attention,
            live: entry.semantic.is_live(),
            file_path: entry.file_path.clone(),
            body_text: if kind_has_searchable_body(entry.kind) {
                entry.context_ref.summary.clone()
            } else {
                crate::item::raw_evidence_identity(
                    entry.kind,
                    entry.file_path.as_deref(),
                    entry.file_revision.as_deref(),
                )
                .expect("raw evidence kind has an identity")
            },
            // A stored descriptor never proves that its summary is the full
            // semantic body. Raw tool/file bodies are intentionally excluded:
            // their path@revision identity is searchable, their body is Fetch-only.
            body_truncated: kind_has_searchable_body(entry.kind),
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
    /// Documents whose matchable text is not fully represented by the
    /// inverted index. Unlike the former global sticky bit, ids leave this
    /// set on removal/reindex and structured filters can narrow it.
    truncated_text_ids: HashSet<ContextItemId>,
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
    /// writes signal strength onto the body, not into these
    /// buckets; keep the method so the index is not a write-only field.
    #[allow(dead_code)]
    pub(crate) fn ids_for_attention(&self, attention: AttentionState) -> &[ContextItemId] {
        self.by_attention
            .get(&attention)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// 目录候选 id。仅测试用；生产调用方需要完备性说明，走
    /// [`Self::search_candidates`]。
    #[cfg(test)]
    pub(crate) fn search_ids(&self, query: &ContextSearchQuery) -> Option<Vec<ContextItemId>> {
        self.search_candidates(query)
            .map(|candidates| candidates.ids)
    }

    /// 目录候选 id 加显式完备性说明：索引有上限（饱和倒排、截断正文）
    /// 时 `incomplete` 非空，调用方须对非候选做有界残差校验。检索是
    /// GC 的兜底网，召回是否完整必须明说。
    pub(crate) fn search_candidates(&self, query: &ContextSearchQuery) -> Option<SearchCandidates> {
        let done = |ids: Vec<ContextItemId>, incomplete: Option<SearchIncompleteReason>| {
            Some(SearchCandidates { ids, incomplete })
        };
        let needle = query.query.trim();
        if let Ok(item_id) = ContextItemId::parse_ref(needle) {
            let ids = self
                .matches_structured_filters(item_id, query)
                .then_some(item_id)
                .into_iter()
                .collect();
            return done(ids, None);
        }
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
                let incomplete = self.index_incomplete_reason(needle, candidates.as_ref());
                return done(ids, incomplete);
            }
            if saw_candidate {
                // 文本层有候选但全部语义死亡：索引已经界定了集合，
                // 如实返回空集（终态永不复活，也绝不走残差扫描放大）。
                let ids = match candidates.as_ref() {
                    Some(set) => set
                        .iter()
                        .copied()
                        .filter(|id| self.live.contains(id))
                        .collect(),
                    None => Vec::new(),
                };
                return done(
                    ids,
                    self.index_incomplete_reason(needle, candidates.as_ref()),
                );
            }
            // 真正无文本候选：有过滤时过滤桶本身就是候选集（旧行为）；
            // 无过滤且没有不完整正文时交给调用方的普通残差扫描。
            // 若有截断正文，空候选也必须携带 incomplete，否则 Stored
            // 中部/尾部的唯一命中永远没有机会进入锁外校验。
            let incomplete = self.index_incomplete_reason(needle, candidates.as_ref());
            return match candidates.as_ref() {
                Some(set) => done(
                    set.iter()
                        .copied()
                        .filter(|id| self.live.contains(id))
                        .collect(),
                    incomplete,
                ),
                None if incomplete.is_some() => done(Vec::new(), incomplete),
                None => None,
            };
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
        done(ids, None)
    }

    /// 文本索引为何可能漏掉该词的命中：仅在文本层已被查询过时有意义。
    fn index_incomplete_reason(
        &self,
        needle: &str,
        filtered_ids: Option<&HashSet<ContextItemId>>,
    ) -> Option<SearchIncompleteReason> {
        if query_needs_text_residual(needle) {
            let has_live_candidate = match filtered_ids {
                Some(ids) => ids.iter().any(|id| self.live.contains(id)),
                None => !self.live.is_empty(),
            };
            return has_live_candidate.then_some(SearchIncompleteReason::UnindexedQueryShape);
        }
        if self.text.query_has_saturated_match(needle) {
            return Some(SearchIncompleteReason::SaturatedPosting);
        }
        let truncated_in_filter = match filtered_ids {
            Some(ids) if ids.len() <= self.truncated_text_ids.len() => {
                ids.iter().any(|id| self.truncated_text_ids.contains(id))
            }
            Some(ids) => self.truncated_text_ids.iter().any(|id| ids.contains(id)),
            None => !self.truncated_text_ids.is_empty(),
        };
        if truncated_in_filter {
            return Some(SearchIncompleteReason::TruncatedIndexedText);
        }
        None
    }

    /// Whether this document's own indexed text is incomplete and it
    /// satisfies the query's structured filters. Callers can apply this
    /// predicate while streaming catalog rows, without allocating an
    /// O(history) id list for the truncated-text residual path.
    #[allow(dead_code)] // Wired by the lock-free stored-body search phase.
    pub(crate) fn is_truncated_search_candidate(
        &self,
        id: ContextItemId,
        query: &ContextSearchQuery,
    ) -> bool {
        self.truncated_text_ids.contains(&id) && self.matches_structured_filters(id, query)
    }

    /// Whether an id is live and satisfies the query's non-text filters.
    /// Used only when the text index cannot represent the query shape.
    pub(crate) fn is_search_candidate(
        &self,
        id: ContextItemId,
        query: &ContextSearchQuery,
    ) -> bool {
        self.matches_structured_filters(id, query)
    }

    fn matches_structured_filters(&self, id: ContextItemId, query: &ContextSearchQuery) -> bool {
        let Some(keys) = self.records.get(&id) else {
            return false;
        };
        if !keys.live {
            return false;
        }
        if query.kind.is_some_and(|kind| keys.kind != kind)
            || query.scope.is_some_and(|scope| keys.scope != scope)
            || query.task_id.is_some_and(|task| keys.task_id != Some(task))
        {
            return false;
        }
        if let Some(label) = query.label.as_deref() {
            let label = label.to_lowercase();
            if !keys
                .labels
                .iter()
                .any(|candidate| candidate.to_lowercase() == label)
            {
                return false;
            }
        }
        true
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
        for (id, keys) in &self.records {
            if keys
                .file_path
                .as_deref()
                .is_some_and(|path| path.to_lowercase().contains(&needle))
            {
                ids.push(*id);
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
        self.truncated_text_ids.clear();
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
        self.truncated_text_ids.remove(&id);
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
        if keys.live && (keys.body_truncated || fields_exceed_text_token_budget(&fields)) {
            self.truncated_text_ids.insert(id);
        }
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

/// Raw tool/file evidence is searchable only by its descriptor identity
/// (path, revision, entities and labels). Reading its body is always an
/// explicit Fetch and must never become a full-text search side channel.
pub(crate) fn kind_has_searchable_body(kind: ContextKind) -> bool {
    !crate::item::is_raw_evidence_kind(kind)
}

/// Raw bodies may contain path-looking or CamelCase text, so their cached
/// entity extraction is not a trustworthy search descriptor. Preserve only
/// the explicit stamped path; semantic kinds keep their normal signatures.
fn catalog_entities(
    kind: ContextKind,
    entities: &[String],
    file_path: Option<&str>,
) -> Vec<String> {
    if !crate::item::is_raw_evidence_kind(kind) {
        return entities.to_vec();
    }
    file_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| vec![path.to_string()])
        .unwrap_or_default()
}

/// Mirror the shared kernel's per-document token bound so catalog
/// completeness includes token-budget truncation as well as character-window
/// truncation. The set stops at `MAX + 1`, keeping this check bounded.
fn fields_exceed_text_token_budget(fields: &[&str]) -> bool {
    let mut unique = HashSet::new();
    for field in fields {
        let tokens = tokenize(field);
        let field_reached_cap = tokens.len() >= agent_contracts::search::MAX_TOKENS_PER_DOC;
        for token in tokens {
            unique.insert(token);
            if unique.len() > agent_contracts::search::MAX_TOKENS_PER_DOC {
                return true;
            }
        }
        // `tokenize` itself caps at MAX. Equality is conservatively treated
        // as incomplete because the field may contain a dropped next token.
        if field_reached_cap {
            return true;
        }
    }
    false
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

        let unmatched = ContextSearchQuery::new("not-an-entity", 8);
        let candidates = catalog
            .search_candidates(&unmatched)
            .expect("stored summaries cannot prove their full bodies do not match");
        assert!(candidates.ids.is_empty());
        assert_eq!(
            candidates.incomplete,
            Some(SearchIncompleteReason::TruncatedIndexedText)
        );
    }

    #[test]
    fn truncated_bodies_report_incomplete_candidates() {
        // 关键词只出现在超出索引前缀的正文里：实体不截断，不能替它携带。
        let mut long = item(ContextItemId::new(), "", None);
        long.content = format!("{} zebra tail beyond the bound", "x".repeat(600));
        long.entities.clear();
        let short = item(ContextItemId::new(), "zebra marker", None);
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[long, short.clone()], &[], &[]);

        let candidates = catalog
            .search_candidates(&ContextSearchQuery::new("zebra", 8))
            .expect("the short doc is an indexed candidate");
        assert_eq!(candidates.ids, vec![short.id]);
        assert_eq!(
            candidates.incomplete,
            Some(agent_contracts::SearchIncompleteReason::TruncatedIndexedText),
            "a 600-char body hides its tail keyword from the index"
        );

        let mut plain = ContextCatalog::default();
        let only_short = item(ContextItemId::new(), "zebra marker", None);
        plain.rebuild(&[only_short], &[], &[]);
        let complete = plain
            .search_candidates(&ContextSearchQuery::new("zebra", 8))
            .expect("candidate");
        assert_eq!(complete.incomplete, None, "no truncation: complete");
    }

    #[test]
    fn truncation_metadata_is_per_item_filter_aware_and_terminal_safe() {
        let task_a = TaskId::new();
        let task_b = TaskId::new();
        let mut long_note = item(ContextItemId::new(), "", None);
        long_note.content = format!("{} hidden tail", "x".repeat(600));
        long_note.entities.clear();
        long_note.task_id = Some(task_a);
        long_note.kind = ContextKind::Note;

        let mut decision = item(ContextItemId::new(), "zebra decision", None);
        decision.task_id = Some(task_b);
        decision.kind = ContextKind::Decision;

        let mut dead_long = long_note.clone();
        dead_long.id = ContextItemId::new();
        dead_long.semantic = SemanticState::Tombstoned;

        let mut catalog = ContextCatalog::default();
        catalog.rebuild(
            &[long_note.clone(), decision.clone(), dead_long.clone()],
            &[],
            &[],
        );

        let decision_query = ContextSearchQuery {
            query: "zebra".into(),
            kind: Some(ContextKind::Decision),
            task_id: Some(task_b),
            limit: 8,
            ..ContextSearchQuery::default()
        };
        let candidates = catalog
            .search_candidates(&decision_query)
            .expect("the decision text is indexed");
        assert_eq!(candidates.ids, vec![decision.id]);
        assert_eq!(
            candidates.incomplete, None,
            "a truncated note outside the structured filters cannot widen the decision query"
        );
        assert!(!catalog.is_truncated_search_candidate(long_note.id, &decision_query));

        let note_query = ContextSearchQuery {
            query: "zebra".into(),
            kind: Some(ContextKind::Note),
            task_id: Some(task_a),
            limit: 8,
            ..ContextSearchQuery::default()
        };
        assert!(catalog.is_truncated_search_candidate(long_note.id, &note_query));
        assert!(
            !catalog.is_truncated_search_candidate(dead_long.id, &note_query),
            "terminal semantics never enter a residual retrieval plan"
        );
    }

    #[test]
    fn stored_descriptors_are_explicitly_potentially_truncated() {
        let stored_item = item(ContextItemId::new(), "short stored summary", None);
        let marker = item(ContextItemId::new(), "zebra marker", None);
        let stored = stored(&stored_item);
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(
            std::slice::from_ref(&marker),
            &[],
            std::slice::from_ref(&stored),
        );

        let query = ContextSearchQuery::new("zebra", 8);
        let candidates = catalog
            .search_candidates(&query)
            .expect("the resident marker is indexed");
        assert_eq!(candidates.ids, vec![marker.id]);
        assert_eq!(
            candidates.incomplete,
            Some(SearchIncompleteReason::TruncatedIndexedText),
            "a stored summary cannot prove its unseen blob body lacks the query"
        );
        assert!(catalog.is_truncated_search_candidate(stored_item.id, &query));
    }

    #[test]
    fn unindexed_short_query_reports_an_explicit_residual_reason() {
        let mut note = item(
            ContextItemId::new(),
            "semantic body ends with 你好世界",
            None,
        );
        note.entities.clear();
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(std::slice::from_ref(&note), &[], &[]);

        let candidates = catalog
            .search_candidates(&ContextSearchQuery::new("界", 8))
            .expect("an unindexable non-empty query must not become a complete miss");
        assert!(candidates.ids.is_empty());
        assert_eq!(
            candidates.incomplete,
            Some(SearchIncompleteReason::UnindexedQueryShape)
        );
        assert!(catalog.is_search_candidate(note.id, &ContextSearchQuery::new("界", 8)));

        let cjk_candidates = catalog
            .search_candidates(&ContextSearchQuery::new("世界", 8))
            .expect("CJK word boundaries also require residual verification");
        assert_eq!(
            cjk_candidates.incomplete,
            Some(SearchIncompleteReason::UnindexedQueryShape)
        );
    }

    #[test]
    fn exact_context_ref_bypasses_saturated_common_tokens() {
        let mut items = Vec::with_capacity(agent_contracts::search::MAX_POSTINGS_PER_TOKEN + 1);
        for _ in 0..=agent_contracts::search::MAX_POSTINGS_PER_TOKEN {
            items.push(item(ContextItemId::new(), "context common", None));
        }
        let target = items.last().unwrap().id;
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&items, &[], &[]);

        let candidates = catalog
            .search_candidates(&ContextSearchQuery::new(
                format!("context://run/{target}"),
                8,
            ))
            .expect("an exact ref resolves directly");
        assert_eq!(candidates.ids, vec![target]);
        assert_eq!(
            candidates.incomplete, None,
            "common-token saturation must not widen an exact id lookup"
        );
    }

    #[test]
    fn legacy_key_substring_hit_still_reports_other_incomplete_bodies() {
        let keyed = item(ContextItemId::new(), "AuthService.rs", None);
        let mut long = item(ContextItemId::new(), "", None);
        long.content = format!("{} hService hidden in the tail", "x".repeat(600));
        long.entities.clear();
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[keyed.clone(), long], &[], &[]);

        let candidates = catalog
            .search_candidates(&ContextSearchQuery::new("hService", 8))
            .expect("legacy entity-key substring is a candidate");
        assert_eq!(candidates.ids, vec![keyed.id]);
        assert_eq!(
            candidates.incomplete,
            Some(SearchIncompleteReason::TruncatedIndexedText),
            "legacy-only candidates must not suppress another document's incomplete body"
        );
    }

    #[test]
    fn raw_evidence_body_is_fetch_only_even_when_long_or_stored() {
        let mut raw = item(ContextItemId::new(), "", None);
        raw.kind = ContextKind::FileObservation;
        raw.content = format!("{} secret_tail_token", "x".repeat(600));
        raw.entities.clear();
        raw.file_path = Some("src/auth.rs".into());
        raw.file_revision = Some("rev-9".into());

        let marker = item(ContextItemId::new(), "secret_tail_token marker", None);
        let mut stored_raw_item = raw.clone();
        stored_raw_item.id = ContextItemId::new();
        let mut stored_raw = stored(&stored_raw_item);
        stored_raw.context_ref.summary = "x".repeat(120);
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(
            &[raw.clone(), marker.clone()],
            &[],
            std::slice::from_ref(&stored_raw),
        );

        let body_query = ContextSearchQuery::new("secret_tail_token", 8);
        let candidates = catalog
            .search_candidates(&body_query)
            .expect("the semantic marker is indexed");
        assert_eq!(candidates.ids, vec![marker.id]);
        assert_eq!(
            candidates.incomplete, None,
            "raw evidence bodies must not schedule full-text residual reads"
        );
        assert!(!catalog.is_truncated_search_candidate(raw.id, &body_query));
        assert!(!catalog.is_truncated_search_candidate(stored_raw_item.id, &body_query));

        let path_query = ContextSearchQuery::new("src/auth.rs@rev-9", 8);
        let path_hits = catalog
            .search_candidates(&path_query)
            .expect("path identity remains searchable");
        assert!(path_hits.ids.contains(&raw.id));
    }

    #[test]
    fn incremental_reindex_removes_stale_truncation_metadata() {
        let long_id = ContextItemId::new();
        let mut long = item(long_id, "", None);
        long.content = "word ".repeat(120);
        long.entities.clear();
        let marker = item(ContextItemId::new(), "zebra marker", None);
        let mut heap = vec![long, marker.clone()];
        let mut catalog = ContextCatalog::default();
        catalog.sync(&heap, &[], &[], 1, CatalogDirty::default());

        let query = ContextSearchQuery::new("zebra", 8);
        assert_eq!(
            catalog
                .search_candidates(&query)
                .expect("marker candidate")
                .incomplete,
            Some(SearchIncompleteReason::TruncatedIndexedText)
        );

        heap[0].content = "short and fully indexed".into();
        let mut dirty = CatalogDirty::default();
        dirty.mark(long_id);
        catalog.sync(&heap, &[], &[], 2, dirty);

        assert_eq!(
            catalog
                .search_candidates(&query)
                .expect("marker candidate")
                .incomplete,
            None,
            "reindexing a now-short document clears its old incomplete bit"
        );
        assert!(!catalog.is_truncated_search_candidate(long_id, &query));
    }

    #[test]
    fn shared_kernel_token_budget_is_part_of_per_item_completeness() {
        let mut many_tokens = item(ContextItemId::new(), "", None);
        many_tokens.content = (0..70).map(|i| format!("t{i:02} ")).collect();
        many_tokens.entities.clear();
        assert!(
            many_tokens.content.chars().count() < INDEX_BODY_PREFIX_CHARS,
            "fixture must exercise token truncation, not the character window"
        );
        let marker = item(ContextItemId::new(), "zebra marker", None);
        let mut catalog = ContextCatalog::default();
        catalog.rebuild(&[many_tokens.clone(), marker], &[], &[]);

        let query = ContextSearchQuery::new("zebra", 8);
        assert_eq!(
            catalog
                .search_candidates(&query)
                .expect("marker candidate")
                .incomplete,
            Some(SearchIncompleteReason::TruncatedIndexedText)
        );
        assert!(catalog.is_truncated_search_candidate(many_tokens.id, &query));
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
        catalog.rebuild(std::slice::from_ref(&decision), &[], &[]);

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
        catalog.rebuild(std::slice::from_ref(&second), &[], &[]);

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
