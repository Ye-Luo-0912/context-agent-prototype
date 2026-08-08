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

/// Build the lightweight external-map entry for an item just written to the
/// store.
pub(crate) fn to_external_entry(
    item: &ContextItem,
    context_ref: ContextRef,
    now_tick: u64,
) -> ExternalizedContext {
    ExternalizedContext {
        item_id: item.id,
        kind: item.kind,
        scope: item.scope,
        retention: item.retention,
        attention: item.attention,
        semantic: item.semantic,
        context_ref,
        externalized_at_tick: now_tick,
        last_access_tick: now_tick,
        residency: ContextResidency::Cold,
    }
}

/// Delete one store file; the caller is responsible for removing the
/// in-memory entry. Returns whether a file existed and was removed.
fn delete_file(dir: &Path, item_id: ContextItemId) -> bool {
    let path = file_path(dir, item_id);
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => false,
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
/// and not referenced by any resident/warm item. This is the only place
/// information is permanently removed.
pub(crate) fn run_storage_gc(
    state: &mut State,
    config: &SimpleContextConfig,
    now_tick: u64,
) -> StorageGcReport {
    let dir = store_dir(config);
    // Ids referenced by dependency edges from the resident heap or the warm
    // buffer protect their store files from deletion.
    let referenced: std::collections::HashSet<ContextItemId> = state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        .flat_map(|item| item.dependencies.iter().copied())
        .collect();

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
        if delete_file(&dir, entry.item_id) {
            report.deleted += 1;
            state.gc_storage_deleted_total += 1;
            report
                .reasons
                .push(format!("deleted {} ({})", entry.context_ref.uri, reason));
        } else {
            // No file behind the entry (already gone): drop the entry too.
            report.deleted += 1;
            state.gc_storage_deleted_total += 1;
            report
                .reasons
                .push(format!("entry {} had no store file", entry.context_ref.uri));
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
/// passes without access.
pub(crate) fn age_external_entries(
    state: &mut State,
    config: &SimpleContextConfig,
    passes: u64,
) -> usize {
    let mut aged = 0usize;
    for entry in &mut state.external {
        if entry.residency != ContextResidency::Cold {
            continue;
        }
        let idle = passes.saturating_sub(entry.last_access_tick);
        if idle >= config.gc_external_ttl_passes as u64 {
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
            state.external.push(to_external_entry(item, reference, 1));
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
}
