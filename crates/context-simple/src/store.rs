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

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use agent_contracts::{
    ContextItem, ContextItemId, ContextRef, ContextResidency, ContextRetention,
    ExternalizedContext, StorageGcReport, StoreReconcileReport,
};

use crate::engine::{SimpleContextConfig, State};

/// How much of an item's content survives in the external map entry.
const SUMMARY_CHARS: usize = 120;

/// Cap on concurrent store IO (writes, reads, deletes, reconcile): the IO
/// phase runs without the state lock, so parallel operations shrink the
/// lock-free window to the slowest single op — but unbounded parallelism
/// would trade one problem for another (fd/thread pressure on a store with
/// tens of thousands of blobs).
pub(crate) const MAX_STORE_IO_CONCURRENCY: usize = 8;

/// FNV-1a 64-bit checksum of a blob, hex-encoded. Not cryptographic: it is
/// a corruption/bit-rot detector for reconcile, which compares the blob
/// against the checksum the owning entry captured at write time. The hot
/// read path skips it so per-item retrieval stays IO-cheap.
fn checksum_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The store directory. The composition root injects the workspace state
/// dir (`workspace.state_dir()/context-store`), so runtime state never
/// scatters under a CWD; the standalone/test fallback is an OS temp dir
/// scoped to this process — never a CWD-relative path, so a misconfigured
/// runtime cannot drop externalized context content into the launch
/// directory.
pub(crate) fn store_dir(config: &SimpleContextConfig) -> PathBuf {
    config.context_store_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("context-agent-store-{}", std::process::id()))
    })
}

fn file_path(dir: &Path, item_id: ContextItemId) -> PathBuf {
    dir.join(format!("{item_id}.json"))
}

fn context_uri(item_id: ContextItemId) -> String {
    format!("context://run/{item_id}")
}

/// Write an item's full content to the store and return its reference.
/// Test-only sync variant: the GC's IO phase uses the async
/// [`externalize_async`] so the state lock is not held across disk writes.
#[cfg(test)]
pub(crate) fn externalize(dir: &Path, item: &ContextItem) -> std::io::Result<ContextRef> {
    std::fs::create_dir_all(dir)?;
    let path = file_path(dir, item.id);
    let bytes = serde_json::to_vec(item)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, bytes)?;
    Ok(make_context_ref(item))
}

/// Async variant of [`externalize`] for the GC's IO phase, which must not
/// hold the state lock across disk writes. The bytes are pre-serialized
/// under the lock by the caller (the IO phase never re-reads state), so a
/// join failure cannot lose the source item; the returned checksum is
/// captured on the owning entry so the reconcile can detect corruption.
///
/// The write is atomic: temp file -> flush + sync -> rename. A crash
/// between the temp write and the rename leaves only a `.tmp` file (cleaned
/// by the startup reconcile), never a half-written blob under the formal
/// name.
pub(crate) async fn externalize_async(
    dir: &Path,
    item_id: ContextItemId,
    bytes: &[u8],
) -> std::io::Result<String> {
    tokio::fs::create_dir_all(dir).await?;
    let path = file_path(dir, item_id);
    let tmp = dir.join(format!("{item_id}.tmp"));
    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
    }
    tokio::fs::rename(&tmp, &path).await?;
    Ok(checksum_hex(bytes))
}

pub(crate) fn make_context_ref(item: &ContextItem) -> ContextRef {
    let summary: String = item.content.chars().take(SUMMARY_CHARS).collect();
    ContextRef {
        uri: context_uri(item.id),
        item_id: item.id,
        kind: item.kind,
        scope: item.scope,
        summary,
        created_tick: item.created_tick,
    }
}

/// Read an externalized item's full content back from the store. `None`
/// when the entry was already deleted by Storage GC.
#[cfg(test)]
pub(crate) fn read_item(dir: &Path, item_id: ContextItemId) -> Option<ContextItem> {
    let bytes = std::fs::read(file_path(dir, item_id)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Async variant of [`read_item`] for the GC's IO phase (recall), which
/// must not hold the state lock across disk reads.
pub(crate) async fn read_item_async(dir: &Path, item_id: ContextItemId) -> Option<ContextItem> {
    let bytes = tokio::fs::read(file_path(dir, item_id)).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Deterministic search over externalized refs — no vectors, no store
/// reads. Filters on the indexed dimensions of the map (entity signature,
/// kind, scope, task), ranks entity matches above recency, and caps the
/// answer. The full content stays in the store; the model decides what to
/// fetch after seeing the refs.
///
/// Bounded by construction: only the `limit` best matches are kept while
/// streaming, so memory stays O(limit) even when the store is large — a
/// model-driven search must not cost proportional to logical history size.
pub(crate) fn search_entries(
    entries: &[ExternalizedContext],
    query: &agent_contracts::ContextSearchQuery,
) -> Vec<ExternalizedContext> {
    let needle = query.query.to_lowercase();
    let limit = if query.limit == 0 { 16 } else { query.limit };
    if limit == 0 {
        return Vec::new();
    }

    // A max-heap of the `limit` *best* matches so far: the peek is the
    // current worst kept, and a better candidate (smaller under
    // `SearchEntry::cmp`, which is the ascending order the old full sort
    // produced) evicts it, so the heap never grows past `limit` regardless
    // of the store's history size. `SearchEntry::cmp` reproduces the
    // previous full sort exactly (entity matches first, then recency, then
    // externalization order, then id).
    let mut top: BinaryHeap<SearchEntry> = BinaryHeap::with_capacity(limit.min(64));
    for entry in entries {
        if !externally_retrievable(entry) {
            continue;
        }
        if let Some(kind) = query.kind
            && entry.kind != kind
        {
            continue;
        }
        if let Some(scope) = query.scope
            && entry.scope != scope
        {
            continue;
        }
        if let Some(task) = query.task_id
            && entry.task_id != Some(task)
        {
            continue;
        }
        let entity_match = entry
            .entities
            .iter()
            .any(|entity| entity.to_lowercase().contains(&needle));
        if !needle.is_empty()
            && !entity_match
            && !entry.context_ref.summary.to_lowercase().contains(&needle)
            && !entry.context_ref.uri.to_lowercase().contains(&needle)
        {
            continue;
        }
        let candidate = SearchEntry {
            entity_match,
            last_access_tick: entry.last_access_tick,
            externalized_at_tick: entry.externalized_at_tick,
            id: entry.item_id,
            entry,
        };
        if top.len() < limit {
            top.push(candidate);
        } else if candidate.cmp(top.peek().expect("non-empty under the guard")) == Ordering::Less {
            top.pop();
            top.push(candidate);
        }
    }
    let mut rows: Vec<SearchEntry> = top.into_vec();
    rows.sort();
    rows.into_iter().map(|row| row.entry.clone()).collect()
}

/// One search hit under its ranking key, so the bounded heap can order by
/// the exact same comparison the previous full sort used: entity matches
/// first, then recency, then externalization order, then id (ascending).
/// `ContextItemId` itself is not `Ord`; the id's `Uuid` is.
struct SearchEntry<'a> {
    entity_match: bool,
    last_access_tick: u64,
    externalized_at_tick: u64,
    id: ContextItemId,
    entry: &'a ExternalizedContext,
}

impl PartialEq for SearchEntry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SearchEntry<'_> {}

impl PartialOrd for SearchEntry<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SearchEntry<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .entity_match
            .cmp(&self.entity_match)
            .then_with(|| other.last_access_tick.cmp(&self.last_access_tick))
            .then_with(|| other.externalized_at_tick.cmp(&self.externalized_at_tick))
            .then_with(|| self.id.0.cmp(&other.id.0))
    }
}

/// Whether an external-map entry may be exposed through the model-facing
/// retrieval surface. Semantic death is terminal regardless of physical
/// residency: the store retains dead entries for audit/storage GC, but they
/// must not be searchable, inspectable or fetchable back into model context.
pub(crate) fn externally_retrievable(entry: &ExternalizedContext) -> bool {
    entry.semantic.is_live()
}

/// Build the lightweight external-map entry for an item just written to the
/// store. The entry keeps the item's entity signature and dependency edges
/// so recall and Storage GC can decide *without* reading the file: recall
/// pre-filters on `entities` in memory, and Storage GC runs a reachability
/// closure over `dependencies` (resident heap, warm buffer and external
/// entries alike). `blob_checksum` is the hash captured at write time so
/// the startup reconcile can detect corruption.
pub(crate) fn to_external_entry(
    item: &ContextItem,
    context_ref: ContextRef,
    now_tick: u64,
    gc_epoch: u64,
    blob_checksum: Option<String>,
) -> ExternalizedContext {
    ExternalizedContext {
        item_id: item.id,
        task_id: item.task_id,
        scope_id: item.scope_id,
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
        keep_alive: item.keep_alive,
        lease_until_turn: item.lease_until_turn,
        last_access_gc_epoch: Some(gc_epoch),
        blob_checksum,
        // 来源权威随条目一起外部化：检索/审查能看到条目来自哪里，
        // 无需读 store 文件；admit/fetch 的权威校验以此为前提。
        source: item.source.clone(),
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
#[cfg(test)]
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

/// Phase 1 of the conservative Storage GC (under the state lock): decide
/// which store entries are deletable and why. Pure in-memory — reachability
/// closure over the reference graph, then retention/semantic/TTL checks —
/// so the plan itself never touches the disk.
///
/// Reachability is a closure over *strong* edges only. `SharesEntities` is
/// weak affinity (auto-minted from entity overlap at ingest) and is never a
/// permanent-delete guard: two stored facts sharing a topic must not pin
/// each other's terminal history forever. Roots are:
///
/// - the strong-edge targets of resident/warm items (a live working-set
///   member's deliberate citation);
/// - every non-deletable stored record itself — Live, Pinned or Durable
///   records are never candidates, and their strong edges must keep their
///   evidence targets alive even when nothing resident references the
///   record (a Live stored fact citing a terminal evidence file must not
///   lose that file to permanent deletion).
///
/// From any referenced record the closure traverses strong edges only, so
/// external -> external chains survive exactly when each hop is a strong
/// citation.
pub(crate) fn plan_storage_gc(
    state: &State,
    config: &SimpleContextConfig,
    now_tick: u64,
) -> StorageGcPlan {
    fn non_deletable(entry: &ExternalizedContext) -> bool {
        !entry.semantic.is_dead()
            || matches!(
                entry.retention,
                ContextRetention::Pinned | ContextRetention::Durable
            )
    }

    let mut referenced: std::collections::HashSet<ContextItemId> = state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        .flat_map(|item| {
            item.dependencies
                .iter()
                .filter(|edge| edge.kind.is_strong())
                .map(|edge| edge.target)
        })
        .collect();
    loop {
        let mut grew = false;
        for entry in &state.external {
            let contributes = referenced.contains(&entry.item_id) || non_deletable(entry);
            if !contributes {
                continue;
            }
            for edge in &entry.dependencies {
                if edge.kind.is_strong() && referenced.insert(edge.target) {
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }

    let candidates = state
        .external
        .iter()
        .filter_map(|entry| {
            storage_candidate(
                entry,
                now_tick,
                config.storage_ttl_ticks,
                referenced.contains(&entry.item_id),
            )
            .map(|reason| (entry.item_id, reason))
        })
        .collect();
    StorageGcPlan { candidates }
}

/// The Storage GC plan: which entries to delete and why (for the report).
pub(crate) struct StorageGcPlan {
    pub(crate) candidates: Vec<(ContextItemId, String)>,
}

/// Phase 2 (no lock held): remove the planned store files. Real IO errors
/// keep their entries — degraded storage is surfaced, not silently treated
/// as "already deleted". Deletions run concurrently (the phase holds no
/// lock, so parallel `remove_file` calls shrink the lock-free window to
/// the slowest single deletion instead of the sum).
pub(crate) async fn run_storage_io(
    dir: &Path,
    plan: &StorageGcPlan,
) -> Vec<(ContextItemId, Result<DeleteOutcome, std::io::Error>)> {
    let mut tasks = tokio::task::JoinSet::new();
    for (item_id, _) in &plan.candidates {
        let item_id = *item_id;
        let path = file_path(dir, item_id);
        tasks.spawn(async move {
            let outcome = match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(DeleteOutcome::Deleted),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeleteOutcome::NotFound),
                Err(e) => Err(e),
            };
            (item_id, outcome)
        });
    }
    let mut results = Vec::with_capacity(plan.candidates.len());
    while let Some(joined) = tasks.join_next().await {
        // `remove_file` returns a `Result`, so a `JoinError` here is
        // unreachable in practice (no panic path); if one ever fires, that
        // entry simply stays in the in-memory map, which is the same
        // conservative outcome as an IO error.
        if let Ok((item_id, outcome)) = joined {
            results.push((item_id, outcome));
        }
    }
    results
}

/// Phase 3 (under a fresh lock): apply the IO results to the external map
/// and build the report. Deleted/NotFound drop their entries; IO errors
/// keep them; non-candidates are untouched.
pub(crate) fn commit_storage_gc(
    state: &mut State,
    plan: StorageGcPlan,
    io: Vec<(ContextItemId, Result<DeleteOutcome, std::io::Error>)>,
) -> StorageGcReport {
    let reasons: std::collections::HashMap<ContextItemId, String> =
        plan.candidates.into_iter().collect();
    let outcomes: std::collections::HashMap<_, _> = io.into_iter().collect();

    let mut report = StorageGcReport::default();
    let mut kept: Vec<ExternalizedContext> = Vec::with_capacity(state.external.len());
    for entry in state.external.take_all() {
        match outcomes.get(&entry.item_id) {
            Some(Ok(DeleteOutcome::Deleted | DeleteOutcome::NotFound)) => {
                report.deleted += 1;
                state.gc_storage_deleted_total += 1;
                let reason = reasons
                    .get(&entry.item_id)
                    .map(String::as_str)
                    .unwrap_or("deleted by storage GC");
                report
                    .reasons
                    .push(format!("deleted {} ({reason})", entry.context_ref.uri));
            }
            Some(Err(e)) => {
                // Real IO failure: keep the entry and its metadata. The
                // content still exists on disk; deleting the reference would
                // orphan it silently.
                report.io_errors += 1;
                let uri = entry.context_ref.uri.clone();
                let reason = reasons
                    .get(&entry.item_id)
                    .map(String::as_str)
                    .unwrap_or("storage GC");
                kept.push(entry);
                report
                    .reasons
                    .push(format!("kept {uri}: storage IO error: {e} ({reason})"));
            }
            None => kept.push(entry),
        }
    }
    // The map re-indexes the survivors in one step (take/replace pair).
    state.external.replace_all(kept);
    report.scanned = state.external.len() + report.deleted;
    report
}

/// Sync composition of the Storage GC, used by tests (which hold `&mut
/// State` directly). The engine uses the async plan/io/commit split so the
/// state lock is never held across disk IO.
#[cfg(test)]
pub(crate) fn run_storage_gc(
    state: &mut State,
    config: &SimpleContextConfig,
    now_tick: u64,
) -> StorageGcReport {
    let plan = plan_storage_gc(state, config, now_tick);
    let dir = store_dir(config);
    let io = plan
        .candidates
        .iter()
        .map(|(item_id, _)| (*item_id, delete_file(&dir, *item_id)))
        .collect();
    commit_storage_gc(state, plan, io)
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
    state
        .external
        .age_entries(config.gc_external_ttl_generations as u64, gc_epoch)
}

/// Whether the store can provide a read path (used by recall).
pub(crate) fn store_ready(config: &SimpleContextConfig) -> bool {
    store_dir(config).exists()
}

/// Delete the store blobs for the given ids. Used *after* a successful
/// recall commit: only once the recalled content is resident again (and the
/// entry left the map) is the blob removed, so a crash between commit and
/// delete leaves an orphan the startup reconcile re-owns instead of losing
/// content. Deletions run with bounded concurrency; real IO errors are
/// surfaced per id (the reconcile pass converges on them later).
pub(crate) async fn delete_blobs_async(
    dir: &Path,
    ids: &[ContextItemId],
) -> Vec<(ContextItemId, Result<DeleteOutcome, std::io::Error>)> {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_STORE_IO_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for &item_id in ids {
        let dir = dir.to_path_buf();
        let semaphore = std::sync::Arc::clone(&semaphore);
        let permit = semaphore.acquire_owned().await.expect("semaphore");
        tasks.spawn(async move {
            let _permit = permit;
            let path = file_path(&dir, item_id);
            let outcome = match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(DeleteOutcome::Deleted),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeleteOutcome::NotFound),
                Err(e) => Err(e),
            };
            (item_id, outcome)
        });
    }
    let mut results = Vec::with_capacity(ids.len());
    while let Some(joined) = tasks.join_next().await {
        if let Ok(result) = joined {
            results.push(result);
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Startup reconcile: bring the on-disk blob directory back in line with the
// external map after a crash or an interrupted IO phase. Every formal blob
// gets exactly one owner; uncertain state is quarantined, never ignored.
// ---------------------------------------------------------------------------

/// What one blob became, decided without the state lock. The plan/io/commit
/// split mirrors the GC: the lock is never held across disk IO.
#[derive(Default)]
pub(crate) struct ReconcileIo {
    /// Valid, ownerless blobs whose content should re-enter the map
    /// (item + the checksum captured from the file).
    pub(crate) rebuilt_candidates: Vec<(ContextItem, String)>,
    pub(crate) scanned: usize,
    pub(crate) deleted_stale: usize,
    pub(crate) quarantined: usize,
    pub(crate) temp_cleaned: usize,
    pub(crate) io_errors: usize,
    pub(crate) reasons: Vec<String>,
}

/// Phase 2 of the reconcile (no lock held): scan the store directory, read
/// every formal blob, and classify it. `map_checksums` is the id -> owned
/// checksum snapshot taken under the lock; `resident_ids` is the heap +
/// warm-buffer id snapshot. Blobs the map owns are kept when their checksum
/// matches; corrupt / id-mismatched blobs are moved to `quarantine/`;
/// abandoned `.tmp` files are removed.
pub(crate) async fn run_reconcile_io(
    dir: &Path,
    map_checksums: &HashMap<ContextItemId, Option<String>>,
    resident_ids: &HashSet<ContextItemId>,
) -> ReconcileIo {
    let mut io = ReconcileIo::default();
    let quarantine_dir = dir.join("quarantine");

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return io,
        Err(e) => {
            io.io_errors += 1;
            io.reasons.push(format!("store dir unreadable: {e}"));
            return io;
        }
    };

    while let Some(entry) = entries.next_entry().await.unwrap_or_else(|e| {
        io.io_errors += 1;
        io.reasons.push(format!("store dir read error: {e}"));
        None
    }) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        // Abandoned temp file: the atomic write never reached its rename.
        if name.ends_with(".tmp") {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    io.temp_cleaned += 1;
                    io.reasons
                        .push(format!("removed abandoned temp file {name}"));
                }
                Err(e) => {
                    io.io_errors += 1;
                    io.reasons.push(format!("could not remove {name}: {e}"));
                }
            }
            continue;
        }
        if !name.ends_with(".json") {
            continue;
        }
        io.scanned += 1;

        // The file name must parse as the id it claims to hold.
        let Ok(item_id) = name.trim_end_matches(".json").parse::<ContextItemId>() else {
            quarantine(
                &quarantine_dir,
                &path,
                &name,
                &mut io,
                "file name is not an item id",
            )
            .await;
            continue;
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                io.io_errors += 1;
                io.reasons.push(format!("unreadable blob {name}: {e}"));
                continue;
            }
        };
        let checksum = checksum_hex(&bytes);
        let item: ContextItem = match serde_json::from_slice(&bytes) {
            Ok(item) => item,
            Err(e) => {
                quarantine(
                    &quarantine_dir,
                    &path,
                    &name,
                    &mut io,
                    &format!("unparseable blob: {e}"),
                )
                .await;
                continue;
            }
        };
        if item.id != item_id {
            quarantine(
                &quarantine_dir,
                &path,
                &name,
                &mut io,
                &format!("blob content id {} != file name id {item_id}", item.id),
            )
            .await;
            continue;
        }

        match map_checksums.get(&item_id) {
            // The map owns this blob: a checksum mismatch means the file
            // was corrupted or tampered with after the write.
            Some(Some(expected)) if *expected != checksum => {
                quarantine(
                    &quarantine_dir,
                    &path,
                    &name,
                    &mut io,
                    "checksum mismatch: blob changed since the owning entry captured it",
                )
                .await;
            }
            // Consistent owner (or a pre-checksum entry: parse + id match
            // is the best available signal).
            Some(_) => {}
            // No owner: if the same id is live in the heap or warm buffer
            // (a crash between an externalize write and its commit, or a
            // stale duplicate after recall), the file is a stale copy of
            // resident content and is deleted; otherwise the blob is
            // re-owned as an external entry — the conservative choice.
            None => {
                if resident_ids.contains(&item_id) {
                    match tokio::fs::remove_file(&path).await {
                        Ok(()) => {
                            io.deleted_stale += 1;
                            io.reasons
                                .push(format!("deleted stale blob {name}: id already resident"));
                        }
                        Err(e) => {
                            io.io_errors += 1;
                            io.reasons
                                .push(format!("could not remove stale blob {name}: {e}"));
                        }
                    }
                } else {
                    io.rebuilt_candidates.push((item, checksum));
                }
            }
        }
    }
    io
}

/// Move one unreadable/inconsistent blob to the quarantine subdirectory.
/// The file is preserved (evidence), just no longer treated as a formal
/// blob.
async fn quarantine(
    quarantine_dir: &Path,
    path: &Path,
    name: &str,
    io: &mut ReconcileIo,
    reason: &str,
) {
    let _ = tokio::fs::create_dir_all(quarantine_dir).await;
    match tokio::fs::rename(path, quarantine_dir.join(name)).await {
        Ok(()) => {
            io.quarantined += 1;
            io.reasons.push(format!("quarantined {name}: {reason}"));
        }
        Err(e) => {
            io.io_errors += 1;
            io.reasons.push(format!("could not quarantine {name}: {e}"));
        }
    }
}

/// Phase 3 of the reconcile (under a fresh lock): apply the IO results —
/// rebuilt blobs re-enter the map as external entries (re-checking that
/// nothing claimed the id while the lock was down), and the report is
/// assembled.
pub(crate) fn commit_reconcile(
    state: &mut State,
    io: ReconcileIo,
    now_tick: u64,
    gc_epoch: u64,
) -> StoreReconcileReport {
    let mut rebuilt = 0usize;
    for (item, checksum) in io.rebuilt_candidates {
        if state.external.get(item.id).is_some() {
            continue; // claimed concurrently; the blob keeps its owner
        }
        let context_ref = make_context_ref(&item);
        state.external.push(to_external_entry(
            &item,
            context_ref,
            now_tick,
            gc_epoch,
            Some(checksum),
        ));
        state.gc_externalized_total += 1;
        rebuilt += 1;
    }
    StoreReconcileReport {
        scanned: io.scanned,
        rebuilt,
        deleted_stale: io.deleted_stale,
        quarantined: io.quarantined,
        temp_cleaned: io.temp_cleaned,
        io_errors: io.io_errors,
        reasons: io.reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{SimpleContextConfig, State};
    use crate::item::make_item;
    use agent_contracts::{ContextKind, ContextScope, DependencyEdge, SemanticState};
    use std::collections::{HashMap, HashSet};

    fn store_config(dir: &Path) -> SimpleContextConfig {
        SimpleContextConfig {
            context_store_dir: Some(dir.to_path_buf()),
            ..SimpleContextConfig::default()
        }
    }

    fn test_item(id: ContextItemId, content: &str) -> ContextItem {
        let state = State {
            event_seq: 1,
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
                .push(to_external_entry(item, reference, 1, 1, None));
        }
        // A resident item strongly cites the dead entry (evidence): it must
        // survive. The weak-affinity case (auto-minted entity overlap) is
        // covered by the dedicated test below — `SharesEntities` is never a
        // permanent-delete guard.
        let mut holder = test_item(ContextItemId::new(), "holder");
        holder.dependencies.push(DependencyEdge {
            target: dead_id,
            kind: agent_contracts::DependencyKind::EvidenceFor,
        });
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
        item.dependencies
            .push(DependencyEdge::shares(ContextItemId::new()));
        let reference = ContextRef {
            uri: "context://run/x".into(),
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            summary: "fix AuthService.rs".into(),
            created_tick: 1,
        };
        let entry = to_external_entry(&item, reference, 7, 3, None);
        assert!(
            !entry.entities.is_empty(),
            "the entry keeps the entity signature for in-memory recall"
        );
        assert_eq!(entry.dependencies, item.dependencies);
        assert_eq!(entry.last_access_gc_epoch, Some(3));
        assert!(entry.entities.iter().any(|e| e == "AuthService.rs"));
    }

    #[test]
    fn search_entries_is_bounded_and_ranks_matches_first() {
        // Six entries, three carrying the `AuthService.rs` entity. The
        // bounded heap must answer at most `limit` hits with entity
        // matches first and recency breaking ties — identical to the old
        // collect-all-then-sort, but with O(limit) memory even when the
        // store is large.
        let mut entries: Vec<ExternalizedContext> = Vec::new();
        let contents = [
            "fix AuthService.rs",
            "fix CacheStore.rs",
            "touch AuthService.rs",
            "read AuthService docs",
            "garden plan",
            "shopping list",
        ];
        for (i, content) in contents.into_iter().enumerate() {
            let item = test_item(ContextItemId::new(), content);
            let reference = ContextRef {
                uri: format!("context://run/{i}"),
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                summary: String::new(),
                created_tick: 0,
            };
            let mut entry = to_external_entry(&item, reference, i as u64 + 1, 1, None);
            if content.contains("AuthService") {
                entry.entities = vec!["AuthService.rs".to_string()];
            }
            entry.last_access_tick = i as u64;
            entries.push(entry);
        }

        let hits = search_entries(
            &entries,
            &agent_contracts::ContextSearchQuery::new("AuthService", 2),
        );
        assert_eq!(hits.len(), 2, "the limit caps the answer");
        assert!(
            hits.iter()
                .all(|entry| entry.entities.iter().any(|e| e == "AuthService.rs")),
            "entity matches rank above everything else: {:?}",
            hits.iter()
                .map(|e| (&e.entities, e.last_access_tick))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            hits[0].last_access_tick, 3,
            "among equal entity matches the newest is first"
        );
        assert_eq!(hits[1].last_access_tick, 2);

        // No limit: all three matches, newest first.
        let all = search_entries(
            &entries,
            &agent_contracts::ContextSearchQuery::new("AuthService", 0),
        );
        assert_eq!(all.len(), 3);
        let ticks: Vec<u64> = all.iter().map(|e| e.last_access_tick).collect();
        assert_eq!(ticks, vec![3, 2, 0], "recency-descending");
    }

    #[test]
    fn external_entries_carry_the_model_protection_fields() {
        let mut item = test_item(ContextItemId::new(), "fix AuthService.rs");
        item.keep_alive = true;
        item.lease_until_turn = Some(42);
        let reference = ContextRef {
            uri: "context://run/x".into(),
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            summary: "fix AuthService.rs".into(),
            created_tick: 1,
        };
        let entry = to_external_entry(&item, reference, 7, 3, None);
        assert!(entry.keep_alive, "keep_alive survives externalization");
        assert_eq!(
            entry.lease_until_turn,
            Some(42),
            "a lease survives externalization"
        );
    }

    #[test]
    fn storage_gc_reaches_through_external_dependency_chains() {
        let dir = tempfile::tempdir().unwrap();
        let config = store_config(dir.path());
        let mut state = State::default();
        // target <- chain <- resident, with a strong edge at every hop: the
        // resident item cites the chain entry, and the chain entry cites
        // the target as evidence. The target must survive through the
        // external -> external edge.
        let target_id = ContextItemId::new();
        let chain_id = ContextItemId::new();
        let mut target = test_item(target_id, "deeply referenced dead content");
        target.semantic = SemanticState::Tombstoned;
        let mut chain = test_item(chain_id, "chain dead content");
        chain.semantic = SemanticState::Tombstoned;
        chain.dependencies.push(DependencyEdge {
            target: target_id,
            kind: agent_contracts::DependencyKind::EvidenceFor,
        });
        let mut holder = test_item(ContextItemId::new(), "resident holder");
        holder.dependencies.push(DependencyEdge {
            target: chain_id,
            kind: agent_contracts::DependencyKind::DerivedFrom,
        });
        state.items.push(holder);
        for item in [&target, &chain] {
            let reference = externalize(dir.path(), item).unwrap();
            state
                .external
                .push(to_external_entry(item, reference, 1, 1, None));
        }

        let report = run_storage_gc(&mut state, &config, 100);
        assert_eq!(
            report.deleted, 0,
            "the whole strong-edge chain survives: {report:?}"
        );
        assert!(state.external.iter().any(|e| e.item_id == target_id));
        assert!(state.external.iter().any(|e| e.item_id == chain_id));
    }

    /// `SharesEntities` is weak affinity, never a permanent-delete guard:
    /// a resident item's auto-minted entity-overlap link, or a stored
    /// record's entity overlap with a terminal neighbor, must not pin that
    /// neighbor forever. Only strong edges protect.
    #[test]
    fn weak_shares_edges_never_protect_from_permanent_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let config = store_config(dir.path());
        let mut state = State::default();

        // (a) A resident item shares entities with a dead stored entry
        // (the ingest-time auto edge): the entry is still deletable.
        let resident_weak_id = ContextItemId::new();
        let mut resident_weak = test_item(resident_weak_id, "weak resident target");
        resident_weak.semantic = SemanticState::Tombstoned;
        let mut weak_holder = test_item(ContextItemId::new(), "weak holder");
        weak_holder
            .dependencies
            .push(DependencyEdge::shares(resident_weak_id));
        state.items.push(weak_holder);

        // (b) A terminal entry shares entities with a live stored record:
        // the live record is a root, but its weak edge must not protect the
        // terminal neighbor.
        let stored_weak_id = ContextItemId::new();
        let mut stored_weak = test_item(stored_weak_id, "stored weak target");
        stored_weak.semantic = SemanticState::Tombstoned;
        let mut live_anchor = test_item(ContextItemId::new(), "live anchor");
        live_anchor
            .dependencies
            .push(DependencyEdge::shares(stored_weak_id));

        for item in [&resident_weak, &stored_weak, &live_anchor] {
            let reference = externalize(dir.path(), item).unwrap();
            state
                .external
                .push(to_external_entry(item, reference, 1, 1, None));
        }

        let report = run_storage_gc(&mut state, &config, 100);
        assert_eq!(
            report.deleted, 2,
            "both weak targets are deletable: {report:?}"
        );
        assert!(
            state.external.iter().all(|e| e.item_id != resident_weak_id),
            "a resident weak edge must not pin the target"
        );
        assert!(
            state.external.iter().all(|e| e.item_id != stored_weak_id),
            "a stored weak edge must not pin the target"
        );
        assert!(
            state.external.iter().any(|e| e.item_id == live_anchor.id),
            "the live anchor itself survives"
        );
    }

    /// A Live stored record that nothing resident references is still a
    /// root: it cannot be deleted, and its strong edges must keep its
    /// evidence targets alive. The weak-edge counterpart is covered by
    /// `weak_shares_edges_never_protect...`.
    #[test]
    fn storage_gc_roots_live_stored_records_through_strong_edges() {
        let dir = tempfile::tempdir().unwrap();
        let config = store_config(dir.path());
        let mut state = State::default();

        // A live stored decision cites a terminal evidence file.
        let evidence_id = ContextItemId::new();
        let mut evidence = test_item(evidence_id, "terminal evidence log");
        evidence.semantic = SemanticState::Tombstoned;
        let mut decision = test_item(ContextItemId::new(), "live stored decision");
        decision.dependencies.push(DependencyEdge {
            target: evidence_id,
            kind: agent_contracts::DependencyKind::EvidenceFor,
        });
        // A second live stored record only *shares entities* with another
        // terminal file: affinity, not a citation — the file stays
        // deletable.
        let affinity_id = ContextItemId::new();
        let mut affinity_target = test_item(affinity_id, "terminal affinity target");
        affinity_target.semantic = SemanticState::Tombstoned;
        let mut affinity_anchor = test_item(ContextItemId::new(), "live affinity anchor");
        affinity_anchor
            .dependencies
            .push(DependencyEdge::shares(affinity_id));

        for item in [&evidence, &decision, &affinity_target, &affinity_anchor] {
            let reference = externalize(dir.path(), item).unwrap();
            state
                .external
                .push(to_external_entry(item, reference, 1, 1, None));
        }

        let report = run_storage_gc(&mut state, &config, 100);
        assert_eq!(
            report.deleted, 1,
            "only the weak-affinity terminal target is deleted: {report:?}"
        );
        assert!(
            state.external.iter().any(|e| e.item_id == evidence_id),
            "a live stored record's evidence must survive permanent deletion"
        );
        assert!(
            state.external.iter().any(|e| e.item_id == decision.id),
            "the live decision survives"
        );
        assert!(
            state.external.iter().all(|e| e.item_id != affinity_id),
            "a live record's weak affinity must not pin the terminal file"
        );
        assert!(
            state
                .external
                .iter()
                .any(|e| e.item_id == affinity_anchor.id),
            "the live affinity anchor survives"
        );
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
            .push(to_external_entry(&item, reference, 100, 2, None));
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
        assert_eq!(state.external[0].residency, ContextResidency::External);
    }

    #[test]
    fn pre_epoch_cold_entry_gets_an_anchor_then_ages_normally() {
        let config = store_config(tempfile::tempdir().unwrap().path());
        let mut state = State::default();
        let item = test_item(ContextItemId::new(), "pre-epoch aging candidate");
        let reference = ContextRef {
            uri: "context://run/pre-epoch".into(),
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            summary: "pre-epoch aging".into(),
            created_tick: 1,
        };
        let mut entry = to_external_entry(&item, reference, 100, 2, None);
        entry.last_access_gc_epoch = None;
        state.external.push(entry);

        assert_eq!(age_external_entries(&mut state, &config, 5), 0);
        assert_eq!(
            state.external[0].last_access_gc_epoch,
            Some(5),
            "the first post-restore GC establishes the generation anchor"
        );
        assert_eq!(age_external_entries(&mut state, &config, 8), 0);
        assert_eq!(
            age_external_entries(&mut state, &config, 9),
            1,
            "the restored entry ages after four full generations"
        );
        assert_eq!(state.external[0].residency, ContextResidency::External);
    }

    /// Simulate the four crash windows around one externalize/recall cycle
    /// and assert the reconcile converges in every one: a leftover temp
    /// file is removed, an uncommitted blob is rebuilt into an entry, a
    /// healthy store needs no intervention, and a blob whose content came
    /// back resident is reclaimed. No window may lose the content or leave
    /// an unowned file.
    #[tokio::test]
    async fn reconcile_heals_each_crash_window_without_losing_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();
        let id = ContextItemId::new();
        let item = test_item(id, "crash window content");
        let bytes = serde_json::to_vec(&item).unwrap();
        let checksum = checksum_hex(&bytes);

        // Window 1: crash after the temp write, before the rename. Only a
        // `.tmp` file exists; no formal blob, no owner needed.
        let tmp = dir.path().join(format!("{id}.tmp"));
        std::fs::write(&tmp, &bytes).unwrap();
        let io = run_reconcile_io(dir.path(), &HashMap::new(), &HashSet::new()).await;
        let report = commit_reconcile(&mut state, io, 1, 1);
        assert_eq!(report.temp_cleaned, 1, "temp cleaned: {report:?}");
        assert_eq!(report.rebuilt, 0);
        assert!(state.external.get(id).is_none());
        assert!(!tmp.exists());

        // Window 2: crash after the rename, before the map commit. The
        // blob is valid and unowned → rebuilt into an entry that captures
        // its checksum, so the content stays reachable.
        externalize_async(dir.path(), id, &bytes).await.unwrap();
        let io = run_reconcile_io(dir.path(), &HashMap::new(), &HashSet::new()).await;
        let report = commit_reconcile(&mut state, io, 1, 1);
        assert_eq!(report.rebuilt, 1, "orphan rebuilt: {report:?}");
        let entry = state.external.get(id).expect("rebuilt entry owns the blob");
        assert_eq!(entry.blob_checksum.as_deref(), Some(checksum.as_str()));

        // Window 3: healthy — the map owns the blob and the checksum
        // matches. One owner, zero interventions.
        let owned: HashMap<_, _> = [(id, Some(checksum.clone()))].into_iter().collect();
        let io = run_reconcile_io(dir.path(), &owned, &HashSet::new()).await;
        let report = commit_reconcile(&mut state, io, 1, 1);
        assert_eq!(report.scanned, 1);
        assert_eq!(
            report.rebuilt + report.deleted_stale + report.quarantined,
            0
        );

        // Window 4: crash after a recall commit, before the blob delete.
        // The content is resident again; the leftover blob is a stale
        // duplicate and is reclaimed, content stays in the heap.
        state.external.retain(|e| e.item_id != id);
        state.items.push(item.clone());
        let resident: HashSet<_> = [id].into_iter().collect();
        let io = run_reconcile_io(dir.path(), &HashMap::new(), &resident).await;
        let report = commit_reconcile(&mut state, io, 1, 1);
        assert_eq!(report.deleted_stale, 1, "stale blob reclaimed: {report:?}");
        assert!(
            !dir.path().join(format!("{id}.json")).exists(),
            "the reclaimed blob is gone"
        );
        assert!(
            state.items.iter().any(|i| i.id == id),
            "the recalled content stayed resident"
        );
    }

    /// One reconcile pass over a store holding every damaged state at once:
    /// an owned blob, an orphan, a stale duplicate, an unparseable file, a
    /// tampered blob and an abandoned temp file. Each lands in exactly the
    /// right bucket, and the damaged evidence is quarantined, not deleted.
    #[tokio::test]
    async fn reconcile_classifies_every_blob_state_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();

        let owned_id = ContextItemId::new();
        let owned = test_item(owned_id, "owned content");
        let owned_bytes = serde_json::to_vec(&owned).unwrap();
        let owned_checksum = externalize_async(dir.path(), owned_id, &owned_bytes)
            .await
            .unwrap();

        let orphan_id = ContextItemId::new();
        let orphan = test_item(orphan_id, "orphan content");
        externalize_async(dir.path(), orphan_id, &serde_json::to_vec(&orphan).unwrap())
            .await
            .unwrap();

        let stale_id = ContextItemId::new();
        let stale = test_item(stale_id, "stale content");
        externalize_async(dir.path(), stale_id, &serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();
        state.items.push(stale);

        let damaged_id = ContextItemId::new();
        std::fs::write(
            dir.path().join(format!("{damaged_id}.json")),
            b"not a context item{{{",
        )
        .unwrap();

        let tampered_id = ContextItemId::new();
        let tampered = test_item(tampered_id, "tampered content");
        let tampered_checksum = externalize_async(
            dir.path(),
            tampered_id,
            &serde_json::to_vec(&tampered).unwrap(),
        )
        .await
        .unwrap();
        // Tamper after the entry captured its checksum: different bytes
        // under the same name.
        std::fs::write(
            dir.path().join(format!("{tampered_id}.json")),
            serde_json::to_vec(&test_item(tampered_id, "changed content")).unwrap(),
        )
        .unwrap();

        let abandoned_id = ContextItemId::new();
        std::fs::write(dir.path().join(format!("{abandoned_id}.tmp")), b"partial").unwrap();

        let map_checksums: HashMap<_, _> = [
            (owned_id, Some(owned_checksum)),
            (tampered_id, Some(tampered_checksum)),
        ]
        .into_iter()
        .collect();
        let resident: HashSet<_> = [stale_id].into_iter().collect();
        let io = run_reconcile_io(dir.path(), &map_checksums, &resident).await;
        let report = commit_reconcile(&mut state, io, 2, 1);

        assert_eq!(report.scanned, 5, "five formal blobs scanned: {report:?}");
        assert_eq!(report.rebuilt, 1, "orphan rebuilt: {report:?}");
        assert_eq!(
            report.deleted_stale, 1,
            "stale duplicate removed: {report:?}"
        );
        assert_eq!(
            report.quarantined, 2,
            "damaged + tampered quarantined: {report:?}"
        );
        assert_eq!(report.temp_cleaned, 1, "abandoned temp removed: {report:?}");
        assert_eq!(report.io_errors, 0);
        assert!(
            dir.path()
                .join("quarantine")
                .join(format!("{damaged_id}.json"))
                .exists(),
            "damaged evidence is preserved"
        );
        assert!(
            dir.path()
                .join("quarantine")
                .join(format!("{tampered_id}.json"))
                .exists(),
            "tampered evidence is preserved"
        );
        assert!(
            state.external.get(orphan_id).is_some(),
            "the rebuilt orphan owns its blob"
        );
        assert!(
            state.external.iter().all(|e| e.item_id != stale_id),
            "the reclaimed id has no entry anymore"
        );
    }

    /// The ownership invariant after a full externalize → recall → reconcile
    /// cycle: every map entry has one readable blob whose bytes match the
    /// entry's captured checksum, and every formal blob has exactly one
    /// owner — no orphans, no dangling records.
    #[tokio::test]
    async fn reconcile_leaves_exactly_one_owner_per_blob() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();

        let mut ids = Vec::new();
        for index in 0..5 {
            let id = ContextItemId::new();
            let item = test_item(id, &format!("invariant item {index}"));
            let bytes = serde_json::to_vec(&item).unwrap();
            let checksum = externalize_async(dir.path(), id, &bytes).await.unwrap();
            state.external.push(to_external_entry(
                &item,
                make_context_ref(&item),
                1,
                1,
                Some(checksum),
            ));
            ids.push(id);
        }

        // Simulate a recall whose post-commit delete never ran: the content
        // is resident again but the blob is still on disk.
        let recalled = ids[0];
        state
            .items
            .push(test_item(recalled, "recalled invariant item"));
        state.external.retain(|e| e.item_id != recalled);

        let map_checksums: HashMap<_, _> = state
            .external
            .iter()
            .map(|e| (e.item_id, e.blob_checksum.clone()))
            .collect();
        let resident: HashSet<_> = state.items.iter().map(|i| i.id).collect();
        let io = run_reconcile_io(dir.path(), &map_checksums, &resident).await;
        let report = commit_reconcile(&mut state, io, 2, 1);
        assert_eq!(
            report.deleted_stale, 1,
            "recalled blob reclaimed: {report:?}"
        );

        // Every entry owns one readable blob matching its checksum.
        for entry in state.external.iter() {
            let bytes = std::fs::read(dir.path().join(format!("{}.json", entry.item_id)))
                .expect("every entry has a readable blob");
            let expected = entry
                .blob_checksum
                .clone()
                .expect("every entry captures a checksum");
            assert_eq!(
                checksum_hex(&bytes),
                expected,
                "blob matches its entry checksum"
            );
        }
        // Every formal blob is owned by exactly one entry.
        let formal: HashSet<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| {
                e.file_name()
                    .to_str()
                    .unwrap()
                    .trim_end_matches(".json")
                    .to_owned()
            })
            .collect();
        assert_eq!(
            formal.len(),
            state.external.len(),
            "no orphan or dangling blobs after reconcile"
        );
        assert!(formal.iter().all(|name| {
            state
                .external
                .iter()
                .any(|e| e.item_id.to_string() == *name)
        }));
    }

    #[tokio::test]
    async fn reconcile_quarantines_a_tampered_blob_against_its_entry_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();
        let id = ContextItemId::new();
        let item = test_item(id, "original content");
        let checksum = externalize_async(dir.path(), id, &serde_json::to_vec(&item).unwrap())
            .await
            .unwrap();
        // Bit rot / tampering after the write: same id, different bytes.
        std::fs::write(
            dir.path().join(format!("{id}.json")),
            serde_json::to_vec(&test_item(id, "changed content")).unwrap(),
        )
        .unwrap();

        let map_checksums: HashMap<_, _> = [(id, Some(checksum))].into_iter().collect();
        let io = run_reconcile_io(dir.path(), &map_checksums, &HashSet::new()).await;
        let report = commit_reconcile(&mut state, io, 1, 1);
        assert_eq!(
            report.quarantined, 1,
            "tampered blob quarantined: {report:?}"
        );
        assert!(
            dir.path()
                .join("quarantine")
                .join(format!("{id}.json"))
                .exists(),
            "the corrupted evidence is preserved for inspection"
        );
        assert!(
            !dir.path().join(format!("{id}.json")).exists(),
            "the corrupted blob no longer masquerades as a formal blob"
        );
    }

    /// Deterministic PRNG for the graph property test (no external
    /// dependency): a simple xorshift64*.
    struct TestRng(u64);

    impl TestRng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
    }

    /// Random-graph property test across every body location: the Storage
    /// GC plan must equal a manually computed strong-edge closure, and the
    /// delete pass must leave exactly the closure survivors alive — every
    /// strong-edge-reachable record (from resident roots or from
    /// non-deletable stored records) survives, and every weak-affinity
    /// link is ignored.
    #[test]
    fn storage_gc_strong_edge_closure_matches_manual_reachability() {
        let dir = tempfile::tempdir().unwrap();
        let config = store_config(dir.path());
        let mut rng = TestRng(0xC7C0_5EED_1234_5678); // deterministic seed
        let entry_count = 60;

        // Build the graph: every entry is external; semantics/retention
        // random; edges point at other entries with random kinds.
        let ids: Vec<ContextItemId> = (0..entry_count).map(|_| ContextItemId::new()).collect();
        let mut entries: Vec<(usize, ExternalizedContext)> = Vec::new();
        for index in 0..entry_count {
            let mut item = test_item(ids[index], &format!("graph item {index}"));
            item.semantic = if rng.below(4) == 0 {
                SemanticState::Live
            } else {
                SemanticState::Tombstoned
            };
            item.retention = if rng.below(10) == 0 {
                ContextRetention::Pinned
            } else {
                ContextRetention::Working
            };
            for _ in 0..rng.below(4) {
                let target = ids[rng.below(entry_count)];
                let strong = rng.below(2) == 0;
                item.dependencies.push(DependencyEdge {
                    target,
                    kind: if strong {
                        agent_contracts::DependencyKind::DerivedFrom
                    } else {
                        agent_contracts::DependencyKind::SharesEntities
                    },
                });
            }
            let reference = externalize(dir.path(), &item).unwrap();
            let entry = to_external_entry(&item, reference, 1, 1, None);
            entries.push((index, entry));
        }

        // Resident/warm roots: a handful of items citing random entries
        // with random edge kinds.
        let mut state = State::default();
        for _ in 0..6 {
            let mut item = test_item(ContextItemId::new(), "resident root");
            for _ in 0..2 {
                let target = ids[rng.below(entry_count)];
                let strong = rng.below(2) == 0;
                item.dependencies.push(DependencyEdge {
                    target,
                    kind: if strong {
                        agent_contracts::DependencyKind::EvidenceFor
                    } else {
                        agent_contracts::DependencyKind::SharesEntities
                    },
                });
            }
            state.items.push(item);
        }
        for (_, entry) in entries {
            state.external.push(entry);
        }

        // Manual closure, mirroring `plan_storage_gc` exactly.
        let non_deletable = |entry: &ExternalizedContext| {
            !entry.semantic.is_dead()
                || matches!(
                    entry.retention,
                    ContextRetention::Pinned | ContextRetention::Durable
                )
        };
        let mut protected: HashSet<ContextItemId> = state
            .items
            .iter()
            .chain(state.eviction_buffer.iter())
            .flat_map(|item| {
                item.dependencies
                    .iter()
                    .filter(|edge| edge.kind.is_strong())
                    .map(|edge| edge.target)
            })
            .collect();
        loop {
            let mut grew = false;
            for entry in &state.external {
                let contributes = protected.contains(&entry.item_id) || non_deletable(entry);
                if !contributes {
                    continue;
                }
                for edge in &entry.dependencies {
                    if edge.kind.is_strong() && protected.insert(edge.target) {
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }

        let now_tick = 100;
        let plan = plan_storage_gc(&state, &config, now_tick);
        let planned: HashSet<ContextItemId> = plan.candidates.iter().map(|(id, _)| *id).collect();

        // The manual closure's complement: dead, retention-eligible, old and
        // unreachable entries — exactly what the plan must select.
        let expected: HashSet<ContextItemId> = state
            .external
            .iter()
            .filter(|entry| {
                entry.semantic.is_dead()
                    && !matches!(
                        entry.retention,
                        ContextRetention::Pinned | ContextRetention::Durable
                    )
                    && !protected.contains(&entry.item_id)
                    && now_tick.saturating_sub(entry.externalized_at_tick)
                        >= config.storage_ttl_ticks
            })
            .map(|entry| entry.item_id)
            .collect();

        assert_eq!(
            planned, expected,
            "the plan must match the manual strong-edge closure exactly"
        );

        // Liveness: run the full delete pass; survivors are exactly the
        // non-candidates.
        let report = run_storage_gc(&mut state, &config, now_tick);
        assert_eq!(report.deleted, planned.len(), "{report:?}");
        for entry in state.external.iter() {
            assert!(
                !planned.contains(&entry.item_id),
                "a planned deletion must not survive"
            );
            assert!(
                non_deletable(entry) || protected.contains(&entry.item_id),
                "a strong-edge-reachable or non-deletable record must survive"
            );
        }
    }
}
