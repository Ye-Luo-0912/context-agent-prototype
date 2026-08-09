//! The external context store: where eviction-buffer overflow puts item
//! content instead of deleting it.
//!
//! Context GC never permanently deletes information. When the reversible
//! buffer overflows, the item's full content is written to the store under
//! a stable `ContextRef` (`context://run/<item-id>`), and only a lightweight
//! entry stays in memory (`ContextResidency::Cold`, then `External`).
//! Reading an externalized item back is possible for recall (hot-entity
//! matches) and is a deliberate operation.
//!
//! The *only* place information is deleted is the conservative Storage GC
//! (`run_storage_gc`): semantically dead entries whose retention allows it,
//! older than the storage TTL, and not referenced by any resident/warm
//! item's dependency edges. Pinned and durable items are never touched.

use std::path::{Path, PathBuf};

use agent_contracts::{
    ContextItem, ContextItemId, ContextRef, ContextResidency, ContextRetention,
    ExternalizedContext, StorageGcReport,
};

use crate::engine::{SimpleContextConfig, State};

/// How much of an item's content survives in the external map entry.
const SUMMARY_CHARS: usize = 120;

/// The store directory, defaulting to `.focus-agent/context-store` under
/// the current working directory.
pub(crate) fn store_dir(config: &SimpleContextConfig) -> PathBuf {
    config
        .context_store_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(".focus-agent").join("context-store"))
}

fn file_path(dir: &Path, item_id: ContextItemId) -> PathBuf {
    dir.join(format!("{item_id}.json"))
}

fn context_uri(item_id: ContextItemId) -> String {
    format!("context://run/{item_id}")
}

/// Write an item's full content to the store and return its reference.
/// Callers (the GC pass) keep the lightweight entry; the file is the
/// authoritative copy of the content.
pub(crate) fn externalize(dir: &Path, item: &ContextItem) -> std::io::Result<ContextRef> {
    std::fs::create_dir_all(dir)?;
    let path = file_path(dir, item.id);
    let bytes = serde_json::to_vec(item)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, bytes)?;
    let summary: String = item.content.chars().take(SUMMARY_CHARS).collect();
    Ok(ContextRef {
        uri: context_uri(item.id),
        item_id: item.id,
        kind: item.kind,
        scope: item.scope,
        summary,
        created_tick: item.created_tick,
    })
}

/// Read an externalized item's full content back from the store. `None`
/// when the entry was already deleted by Storage GC.
pub(crate) fn read_item(dir: &Path, item_id: ContextItemId) -> Option<ContextItem> {
    let bytes = std::fs::read(file_path(dir, item_id)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Deterministic search over externalized refs — no vectors, no store
/// reads. Filters on the indexed dimensions of the map (entity signature,
/// kind, scope, task), ranks entity matches above recency, and caps the
/// answer. The full content stays in the store; the model decides what to
/// fetch after seeing the refs.
pub(crate) fn search_entries(
    entries: &[ExternalizedContext],
    query: &agent_contracts::ContextSearchQuery,
) -> Vec<ExternalizedContext> {
    let needle = query.query.to_lowercase();
    let mut hits: Vec<&ExternalizedContext> = entries
        .iter()
        .filter(|entry| {
            if let Some(kind) = query.kind
                && entry.kind != kind
            {
                return false;
            }
            if let Some(scope) = query.scope
                && entry.scope != scope
            {
                return false;
            }
            if let Some(task) = query.task_id
                && entry.task_id != Some(task)
            {
                return false;
            }
            if needle.is_empty() {
                return true;
            }
            entry.entities.iter().any(|entity| {
                entity.to_lowercase().contains(&needle)
            }) || entry.context_ref.summary.to_lowercase().contains(&needle)
                || entry.context_ref.uri.to_lowercase().contains(&needle)
        })
        .collect();
    hits.sort_by(|a, b| {
        let a_entity = a
            .entities
            .iter()
            .any(|entity| entity.to_lowercase().contains(&needle));
        let b_entity = b
            .entities
            .iter()
            .any(|entity| entity.to_lowercase().contains(&needle));
        b_entity
            .cmp(&a_entity)
            .then_with(|| b.last_access_tick.cmp(&a.last_access_tick))
            .then_with(|| b.externalized_at_tick.cmp(&a.externalized_at_tick))
            .then_with(|| a.item_id.0.cmp(&b.item_id.0))
    });
    let limit = if query.limit == 0 { 16 } else { query.limit };
    hits.truncate(limit);
    hits.into_iter().cloned().collect()
}

/// Build the lightweight external-map entry for an item just written to the
/// store. The entry keeps the item's entity signature and dependency edges
/// so recall and Storage GC can decide *without* reading the file: recall
/// pre-filters on `entities` in memory, and Storage GC runs a reachability
/// closure over `dependencies` (resident heap, warm buffer and external
/// entries alike).
pub(crate) fn to_external_entry(
    item: &ContextItem,
    context_ref: ContextRef,
    now_tick: u64,
    gc_epoch: u64,
) -> ExternalizedContext {
    ExternalizedContext {
        item_id: item.id,
        task_id: item.task_id,
        kind: item.kind,
        scope: item.scope,
        retention: item.retention,
        attention: item.attention,
        semantic: item.semantic,
        context_ref,
        externalized_at_tick: now_tick,
        last_access_tick: now_tick,
        residency: ContextResidency::Cold,
        entities: item.entities.clone(),
        tags: item.tags.clone(),
        dependencies: item.dependencies.clone(),
        last_access_gc_epoch: Some(gc_epoch),
    }
}

/// The outcome of removing one store file. `Deleted` means the file is
/// gone; `NotFound` means it was already gone. Any other IO condition is an
/// *error* — the caller must keep the in-memory entry and surface it,
/// because mistaking a permission/disk failure for "the file is gone" would
/// drop the metadata while the content still exists.
#[derive(Debug)]
pub(crate) enum DeleteOutcome {
    Deleted,
    NotFound,
}

/// Delete one store file; the caller is responsible for removing the
/// in-memory entry only on `Deleted`/`NotFound`. Real IO errors propagate.
fn delete_file(dir: &Path, item_id: ContextItemId) -> Result<DeleteOutcome, std::io::Error> {
    let path = file_path(dir, item_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(DeleteOutcome::Deleted),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeleteOutcome::NotFound),
        Err(e) => Err(e),
    }
}

/// A candidate for the conservative Storage GC, with the reason it is
/// eligible.
fn storage_candidate(
    entry: &ExternalizedContext,
    now_tick: u64,
    storage_ttl_ticks: u64,
    referenced: bool,
) -> Option<String> {
    if !entry.semantic.is_dead() {
        return None;
    }
    // Pinned and durable outcomes are never deleted by Storage GC.
    if matches!(
        entry.retention,
        ContextRetention::Pinned | ContextRetention::Durable
    ) {
        return None;
    }
    // Nothing may reference the entry anymore (dependency edges from the
    // resident heap or the warm buffer).
    if referenced {
        return None;
    }
    let age = now_tick.saturating_sub(entry.externalized_at_tick);
    if age < storage_ttl_ticks {
        return None;
    }
    Some(format!(
        "semantically dead ({:?}), retention {:?}, no references, external for {age} ticks >= storage TTL {storage_ttl_ticks}",
        entry.semantic, entry.retention
    ))
}

/// Run the conservative Storage GC: delete store files whose entries are
/// semantically dead, deletable by retention, older than the storage TTL
/// and unreachable from the reference graph. This is the only place
/// information is permanently removed.
///
/// Reachability is a closure, not a single incoming-edge check: roots are
/// the dependency edges of resident/warm items, and every external entry
/// whose id becomes reachable contributes its own dependencies — so
/// external -> external chains, semantic links that surface as dependencies,
/// and any future audit/evidence/OpenLoop edges keep their targets alive
/// transitively. A store file that fails to delete on a *real* IO error
/// keeps its entry (degraded storage is surfaced, not silently treated as
/// "already deleted").
pub(crate) fn run_storage_gc(
    state: &mut State,
    config: &SimpleContextConfig,
    now_tick: u64,
) -> StorageGcReport {
    let dir = store_dir(config);

    // Reachability closure over the reference graph: resident/warm items
    // are roots; each reachable external entry contributes its own edges.
    let mut referenced: std::collections::HashSet<ContextItemId> = state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        .flat_map(|item| item.dependencies.iter().copied())
        .collect();
    loop {
        let mut grew = false;
        for entry in &state.external {
            if referenced.contains(&entry.item_id)
                && entry
                    .dependencies
                    .iter()
                    .any(|dep| !referenced.contains(dep))
            {
                referenced.extend(entry.dependencies.iter().copied());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut report = StorageGcReport::default();
    let mut kept: Vec<ExternalizedContext> = Vec::with_capacity(state.external.len());
    for entry in state.external.drain(..) {
        let referenced_now = referenced.contains(&entry.item_id);
        let Some(reason) =
            storage_candidate(&entry, now_tick, config.storage_ttl_ticks, referenced_now)
        else {
            kept.push(entry);
            continue;
        };
        match delete_file(&dir, entry.item_id) {
            Ok(DeleteOutcome::Deleted) => {
                report.deleted += 1;
                state.gc_storage_deleted_total += 1;
                report
                    .reasons
                    .push(format!("deleted {} ({reason})", entry.context_ref.uri));
            }
            Ok(DeleteOutcome::NotFound) => {
                // No file behind the entry (already gone): drop the entry too.
                report.deleted += 1;
                state.gc_storage_deleted_total += 1;
                report.reasons.push(format!(
                    "entry {} had no store file",
                    entry.context_ref.uri
                ));
            }
            Err(e) => {
                // Real IO failure: keep the entry and its metadata. The
                // content still exists on disk; deleting the reference would
                // orphan it silently.
                report.io_errors += 1;
                kept.push(entry);
                report
                    .reasons
                    .push(format!("kept {}: storage IO error: {e}", reason));
            }
        }
    }
    state.external = kept;
    report.scanned = state.external.len() + report.deleted;
    report
}

/// Whether a semantically dead item may still be recalled. Only `Cold`
/// entries get a second chance via hot-entity matches; `External` entries
/// exist as references only.
pub(crate) fn recallable(entry: &ExternalizedContext) -> bool {
    entry.residency == ContextResidency::Cold
}

/// Age `Cold` entries toward `External` after the configured number of full
/// GC *generations* without access. The unit is `gc_epoch`, which only a
/// full GC pass increments — comparing against the tick counter would let
/// unrelated runtime activity (ingest, maintain, materialize) age entries
/// out without a single GC pass having run. Entries restored from
/// pre-epoch checkpoints (`last_access_gc_epoch == None`) start fresh at
/// the current epoch instead of aging out instantly.
pub(crate) fn age_external_entries(
    state: &mut State,
    config: &SimpleContextConfig,
    gc_epoch: u64,
) -> usize {
    let mut aged = 0usize;
    for entry in &mut state.external {
        if entry.residency != ContextResidency::Cold {
            continue;
        }
        let last = entry.last_access_gc_epoch.unwrap_or(gc_epoch);
        let idle = gc_epoch.saturating_sub(last);
        if idle >= config.gc_external_ttl_generations as u64 {
            entry.residency = ContextResidency::External;
            aged += 1;
        }
    }
    aged
}

/// Whether the store can provide a read path (used by recall).
pub(crate) fn store_ready(config: &SimpleContextConfig) -> bool {
    store_dir(config).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{SimpleContextConfig, State};
    use crate::item::make_item;
    use agent_contracts::{ContextKind, ContextScope, SemanticState};

    fn store_config(dir: &Path) -> SimpleContextConfig {
        SimpleContextConfig {
            context_store_dir: Some(dir.to_path_buf()),
            ..SimpleContextConfig::default()
        }
    }

    fn test_item(id: ContextItemId, content: &str) -> ContextItem {
        let state = State {
            tick: 1,
            ..State::default()
        };
        let mut item = make_item(
            &state,
            &SimpleContextConfig::default(),
            content.into(),
            ContextKind::FileObservation,
            ContextScope::Turn,
            ContextRetention::Working,
            0.5,
            Some("test".into()),
        );
        item.id = id;
        item
    }

    #[test]
    fn externalize_roundtrips_content_and_uri() {
        let dir = tempfile::tempdir().unwrap();
        let item = test_item(ContextItemId::new(), "some working content");
        let reference = externalize(dir.path(), &item).unwrap();
        assert!(reference.uri.starts_with("context://run/"));
        let restored = read_item(dir.path(), item.id).expect("item readable back");
        assert_eq!(restored.content, item.content);
        assert_eq!(restored.semantic, item.semantic);
    }

    #[tokio::test]
    async fn storage_gc_deletes_only_dead_unreferenced_old_entries() {
        let dir = tempfile::tempdir().unwrap();
        let config = store_config(dir.path());
        let mut state = State::default();
        let dead_id = ContextItemId::new();
        let live_id = ContextItemId::new();
        let pinned_id = ContextItemId::new();
        let orphan_id = ContextItemId::new();
        let mut dead = test_item(dead_id, "old dead content");
        dead.semantic = SemanticState::Tombstoned;
        let live = test_item(live_id, "live content");
        let mut pinned = test_item(pinned_id, "pinned content");
        pinned.retention = ContextRetention::Pinned;
        pinned.semantic = SemanticState::Tombstoned;
        let mut orphan = test_item(orphan_id, "dead orphan");
        orphan.semantic = SemanticState::Tombstoned;

        for item in [&dead, &live, &pinned, &orphan] {
            let reference = externalize(dir.path(), item).unwrap();
            state
                .external
                .push(to_external_entry(item, reference, 1, 1));
        }
        // A resident item depends on the dead entry: it must survive.
        let mut holder = test_item(ContextItemId::new(), "holder");
        holder.dependencies.push(dead_id);
        state.items.push(holder);

        let report = run_storage_gc(&mut state, &config, 100);
        assert_eq!(
            report.deleted, 1,
            "only the dead, unreferenced, unpinned orphan is deleted"
        );
        assert!(
            report.reasons[0].contains("no references"),
            "{:?}",
            report.reasons
        );
        assert!(
            state.external.iter().any(|e| e.item_id == dead_id),
            "referenced dead entry must survive"
        );
        assert!(
            state.external.iter().any(|e| e.item_id == pinned_id),
            "pinned entries are never deleted"
        );
        assert!(
            state.external.iter().any(|e| e.item_id == live_id),
            "live entries are never deleted"
        );
        assert!(
            state.external.iter().all(|e| e.item_id != orphan_id),
            "the deletable orphan leaves the map"
        );
        assert_eq!(state.gc_storage_deleted_total, 1);
    }

    #[test]
    fn external_entries_carry_the_entity_signature_and_dependencies() {
        let mut item = test_item(ContextItemId::new(), "fix AuthService.rs");
        item.dependencies.push(ContextItemId::new());
        let reference = ContextRef {
            uri: "context://run/x".into(),
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            summary: "fix AuthService.rs".into(),
            created_tick: 1,
        };
        let entry = to_external_entry(&item, reference, 7, 3);
        assert!(
            !entry.entities.is_empty(),
            "the entry keeps the entity signature for in-memory recall"
        );
        assert_eq!(entry.dependencies, item.dependencies);
        assert_eq!(entry.last_access_gc_epoch, Some(3));
        assert!(entry.entities.iter().any(|e| e == "AuthService.rs"));
    }

    #[test]
    fn storage_gc_reaches_through_external_dependency_chains() {
        let dir = tempfile::tempdir().unwrap();
        let config = store_config(dir.path());
        let mut state = State::default();
        // target <- chain <- resident: the resident item only references
        // the chain entry, and the chain entry references the target. The
        // target must survive through the external -> external edge.
        let target_id = ContextItemId::new();
        let chain_id = ContextItemId::new();
        let mut target = test_item(target_id, "deeply referenced dead content");
        target.semantic = SemanticState::Tombstoned;
        let mut chain = test_item(chain_id, "chain dead content");
        chain.semantic = SemanticState::Tombstoned;
        chain.dependencies.push(target_id);
        let mut holder = test_item(ContextItemId::new(), "resident holder");
        holder.dependencies.push(chain_id);
        state.items.push(holder);
        for item in [&target, &chain] {
            let reference = externalize(dir.path(), item).unwrap();
            state
                .external
                .push(to_external_entry(item, reference, 1, 1));
        }

        let report = run_storage_gc(&mut state, &config, 100);
        assert_eq!(
            report.deleted, 0,
            "the whole reachable chain survives: {report:?}"
        );
        assert!(state.external.iter().any(|e| e.item_id == target_id));
        assert!(state.external.iter().any(|e| e.item_id == chain_id));
    }

    #[test]
    fn delete_file_distinguishes_not_found_from_other_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = ContextItemId::new();
        match delete_file(dir.path(), missing) {
            Ok(DeleteOutcome::NotFound) => {}
            other => panic!("missing file must be NotFound, got {other:?}"),
        }
        // A real entry deletes cleanly.
        let item = test_item(ContextItemId::new(), "to delete");
        externalize(dir.path(), &item).unwrap();
        match delete_file(dir.path(), item.id) {
            Ok(DeleteOutcome::Deleted) => {}
            other => panic!("existing file must delete, got {other:?}"),
        }
    }

    #[test]
    fn cold_aging_counts_generations_not_ticks() {
        let config = store_config(tempfile::tempdir().unwrap().path());
        let mut state = State::default();
        // Externalized at epoch 2; a tick-based TTL would age it out after
        // 4 ticks of unrelated activity, a generation-based one only after
        // 4 full GC passes.
        let item = test_item(ContextItemId::new(), "aging candidate");
        let reference = ContextRef {
            uri: "context://run/a".into(),
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            summary: "aging".into(),
            created_tick: 1,
        };
        state
            .external
            .push(to_external_entry(&item, reference, 100, 2));
        assert_eq!(
            age_external_entries(&mut state, &config, 5),
            0,
            "only 3 generations passed, TTL is 4"
        );
        assert_eq!(
            age_external_entries(&mut state, &config, 6),
            1,
            "4 generations passed: the entry ages to External"
        );
        assert_eq!(
            state.external[0].residency,
            ContextResidency::External
        );
    }
}
