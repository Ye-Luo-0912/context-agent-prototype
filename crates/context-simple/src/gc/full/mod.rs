use std::collections::HashSet;

use agent_contracts::{
    AttentionState, ContextEviction, ContextGcReport, ContextItem, ContextItemId,
    ContextReactivation, ContextRef, ContextResidency, ContextRetention, ContextScope,
    DependencyEdge, FocusState, LifecycleAxis, ScopeId, ScopeKind, ScopeState, TaskId,
};

use crate::diagnostics;
use crate::engine::{SimpleContextConfig, State};
use crate::index::entity::entities_match;
use crate::policy::score_item_with_breakdown;
use crate::store;

/// Cap on transitive dependency expansion during the mark phase: the root
/// set stays a bounded, cheap reachability view.
const MAX_MARKED_DEPENDENCIES: usize = 8;

/// Everything one full GC pass decided under the state lock. The heap and
/// the eviction buffer are already in their post-sweep state when the plan
/// returns — the sweep, warm-buffer reactivation and Cold -> External aging
/// are all in-memory. What remains is store IO (writing overflow items,
/// reading back hot-entity recall candidates) which must not hold the
/// lock, plus the commit that applies the IO results.
pub(crate) struct GcPlan {
    /// Buffer items whose store write is deferred to the IO phase (oldest
    /// first; removed from the buffer by the plan). Each carries the bytes
    /// serialized under the lock, so the IO phase never needs the state
    /// lock to re-read the item.
    pub(crate) externalize: Vec<(ContextItem, Vec<u8>)>,
    /// Cold-store entry ids whose entities match the hot set; read back in
    /// the IO phase. Entries stay in the map until a successful read.
    pub(crate) recall_candidates: Vec<ContextItemId>,
    /// Report data decided under lock.
    pub(crate) evictions: Vec<ContextEviction>,
    pub(crate) buffer_reactivations: Vec<ContextReactivation>,
    pub(crate) marked_roots: usize,
    pub(crate) evicted: usize,
    pub(crate) reactivated: usize,
    pub(crate) aged_external: usize,
    /// Live resident items protected this pass by anchor root claims.
    pub(crate) anchor_roots_protected: usize,
}

/// The store IO outcomes, applied by the commit under a fresh lock.
pub(crate) struct GcIoResult {
    /// (item, reference, checksum) successfully written to the store.
    pub(crate) externalized: Vec<(ContextItem, ContextRef, String)>,
    /// Items whose store write failed, oldest first; the commit reinserts
    /// them at the front of the buffer so the overflow retries next pass
    /// (the store-unavailable fallback, decided outside the lock).
    pub(crate) externalize_failed: Vec<ContextItem>,
    /// Recalled items with full content read back from the store.
    pub(crate) recalled: Vec<ContextItem>,
}

/// One full GC pass, phase 1 (planning, under the state lock): mark roots,
/// sweep unmarked items into the bounded reversible eviction buffer,
/// reactivate items that became relevant again, age Cold entries, and
/// decide which overflow items to externalize and which store entries to
/// recall — *without touching the disk*. `None` when there is nothing to
/// do (GC disabled or an empty heap/buffer).
///
/// The GC dimensions are separated:
/// - `residency` (Resident / Warm / Cold / External) says where the item
///   physically lives;
/// - `gc_generation` counts how many passes an item survived without being
///   a root (the generational heuristic);
/// - the attention `ContextState` (Active/Cooling/Archived) is owned by the
///   per-event residency machine, not by GC;
/// - semantic death (`SemanticState`) is terminal: dead items are evicted
///   and never reactivated — only Storage GC may delete them.
///
/// Every eviction, reactivation and externalization carries an explicit
/// reason, so the report can explain *why* an item moved where it did.
pub(crate) fn plan_full_gc(
    state: &mut State,
    config: &SimpleContextConfig,
    now_tick: u64,
    turn: u64,
) -> Option<GcPlan> {
    if !config.gc_enabled
        || (state.items.is_empty() && state.eviction_buffer.is_empty() && state.external.is_empty())
    {
        // A pass only makes sense when something can change: resident
        // items to sweep, buffer entries to recall, or external entries to
        // age (Cold -> External) and recall. An external-only state must
        // still run the pass — the heap and buffer being empty is exactly
        // when aging and recall would otherwise stop forever.
        return None;
    }

    // One full GC generation. External aging and TTLs count generations —
    // only this counter advances on a real pass, unlike `tick` which also
    // grows on ingest/maintain/materialize.
    state.gc_epoch += 1;
    let gc_epoch = state.gc_epoch;
    state.sync_catalog();

    let mut plan = GcPlan {
        externalize: Vec::new(),
        recall_candidates: Vec::new(),
        evictions: Vec::new(),
        buffer_reactivations: Vec::new(),
        marked_roots: 0,
        evicted: 0,
        reactivated: 0,
        aged_external: 0,
        anchor_roots_protected: 0,
    };

    // ----- Mark phase: the root set --------------------------------
    // Roots are the current attention: pins, members of the active focus
    // scope (including open tool frames under it), durable task
    // constraints, durable session memory, items whose entities are hot,
    // the latest body of each recent file in the active task, plus a
    // bounded slice of their dependencies. The task id alone is a
    // boundary, not a root — a long task's cooled history is not protected
    // just because it shares the task id.
    let focus = state.focus.clone();
    let hot_entities = state.hot_entities.clone();
    let latest_file_bodies = state.latest_file_body_ids();
    let (marked, anchor_roots_protected) = mark_roots(
        state,
        config,
        focus.as_ref(),
        &hot_entities,
        &latest_file_bodies,
    );
    plan.anchor_roots_protected = anchor_roots_protected;
    // The ids of items whose entities are hot right now, mirroring the
    // mark phase's `hot` test. The sweep exempts hot items from the
    // ordinary-dialogue aging rule, and the reactivate phase applies the
    // same exemption, so "hot" stays a fresh causal reason in both passes.
    let hot_ids: HashSet<ContextItemId> = state
        .items
        .iter()
        .filter(|item| {
            !task_completed(state, item.task_id)
                && !hot_entities.is_empty()
                && entities_match(&item.entities, &hot_entities)
        })
        .map(|item| item.id)
        .collect();

    // ----- Sweep phase: unmarked items ------------------------------
    // Roots protect *live* items. A semantically dead item is dead no
    // matter how reachable it looks — the residency machine decided its
    // lifecycle ended, so GC physically removes it (into the reversible
    // buffer, then the store) instead of letting it linger in the heap and
    // checkpoints forever.
    let mut survivors: Vec<ContextItem> = Vec::with_capacity(state.items.len());
    for item in state.items.take_all() {
        // A consumed ephemeral observation left attention (Archived) even
        // though it is still a member of the focus scope: root protection
        // is for the working set, not for spent turn observations — they
        // leave the heap and stay recallable from the buffer. An explicit
        // model directive (`context.gc_hint` / `context.lease`) overrides
        // that heuristic: the model asked for the item to stay.
        let model_directed = item.keep_alive
            || item
                .lease_until_turn
                .is_some_and(|until| state.turn <= until);
        let latest_file_body = latest_file_bodies.contains(&item.id);
        let consumed_ephemeral = item.attention == AttentionState::Archived
            && item.retention == ContextRetention::Ephemeral
            && item.scope == ContextScope::Turn
            && !latest_file_body;
        // Ordinary dialogue of the *open* focus episode has a shelf life
        // too: related messages share tokens, so the score floor keeps
        // them Active forever and the focus-scope root would accumulate
        // every turn of a long episode. A Working item with no promotable
        // outcome, no hot entities, no model directive, older than the
        // staleness window (ttl x 4), leaves the heap even though it is
        // marked — it stays Live in the reversible buffer, so this is
        // aging, not death.
        let aged_ordinary = aged_ordinary_dialogue(&item, config, turn, hot_ids.contains(&item.id));
        let alive_root = !aged_ordinary
            && item.semantic.is_live()
            && marked.contains(&item.id)
            && (model_directed || !consumed_ephemeral);
        if alive_root {
            // A root is currently relevant: "young" again.
            let mut root = item;
            root.gc_generation = 0;
            survivors.push(root);
            continue;
        }
        let generation = item.gc_generation;
        // A member of a closed scope is outside the working set: the
        // residency pass may still score it Active (a same-template message
        // keeps a high focus match), but its scope ended, so no root can
        // protect it and no candidate can select it. Evict regardless of
        // attention — the working set tracks open scopes, not task turns.
        let closed_member = item.scope_id.is_some_and(|sid| {
            state
                .scopes
                .by_id(sid)
                .is_none_or(|scope| scope.state == ScopeState::Closed)
        });
        if aged_ordinary || closed_member || eviction_candidate(&item, config, turn, generation) {
            let reason = if aged_ordinary {
                let age = turn.saturating_sub(item.created_turn);
                format!(
                    "ordinary dialogue aged out of the open focus episode (age {age} turns > ttl x4 = {}); evicted to reversible buffer (generation {generation})",
                    config.turn_ttl_ticks * 4
                )
            } else if closed_member {
                format!(
                    "member of a closed {} scope; evicted to reversible buffer (generation {generation})",
                    state
                        .scopes
                        .by_id(item.scope_id.expect("closed_member implies a scope id"))
                        .map(|scope| format!("{:?}", scope.kind))
                        .unwrap_or_default()
                )
            } else {
                eviction_reason(&item, config, turn, generation)
            };
            plan.evictions.push(ContextEviction {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                generation,
                evicted_at_tick: now_tick,
                reason: reason.clone(),
            });
            plan.evicted += 1;
            state.gc_evicted_total += 1;
            let mut evicted = item;
            evicted.residency = ContextResidency::Warm;
            evicted.evicted_at_tick = Some(now_tick);
            crate::ledger::record(
                state,
                evicted.id,
                LifecycleAxis::Gc,
                "Resident",
                "Warm",
                reason,
                "gc",
                None,
            );
            state.eviction_buffer.push(evicted);
        } else {
            // Survived this pass without being a root: the generational
            // counter climbs so a stale item cannot hide in the heap forever.
            let mut survivor = item;
            survivor.gc_generation = generation.saturating_add(1);
            survivors.push(survivor);
        }
    }
    // The sweep rebuilt the heap wholesale; the heap re-indexes itself.
    state.items.replace_all(survivors);

    // ----- Reactivate phase: items that became relevant again ------
    // Warm buffer items (content in memory) and Cold store entries (ids
    // only — the content read happens in the IO phase) both get a second
    // chance on hot entities / a high score. A marked dependency is a
    // stronger reason: the root that depends on it is live right now.
    let marked_set: HashSet<ContextItemId> = marked.iter().copied().collect();
    reactivate(state, config, now_tick, &mut plan, &marked_set);

    // ----- Externalize phase: the buffer is bounded -----------------
    // Context GC never purges: overflow writes the item to the context
    // store and keeps only a lightweight entry (Cold), which later ages to
    // External. The *decision* is taken here; the bytes are serialized
    // under the lock so the IO phase can write without re-reading state;
    // the writes happen in the IO phase so the lock is not held across
    // disk IO. Only Storage GC may delete store files.
    while state.eviction_buffer.len() > config.gc_buffer_capacity {
        let item = state.eviction_buffer.remove(0);
        let bytes = serde_json::to_vec(&item).expect("context items serialize");
        plan.externalize.push((item, bytes));
    }

    // Cold -> External aging: entries untouched for the configured number
    // of full GC generations become references only.
    plan.aged_external = store::age_external_entries(state, config, gc_epoch);

    plan.marked_roots = marked.len();
    Some(plan)
}

/// One full GC pass, phase 2 (IO, *without* the state lock): write the
/// overflow items to the store and read back the recall candidates. The
/// heap/buffer stay in their post-plan state during this window, so
/// concurrent ingests see a consistent post-sweep heap.
///
/// IO concurrency is bounded (`store::MAX_STORE_IO_CONCURRENCY`): the
/// phase holds no state lock, so parallelism shrinks the lock-free window
/// to the slowest single op — but unbounded parallelism would pile up file
/// descriptors on a store with many blobs. A `JoinError` (task panic) is
/// unreachable in practice because every fallible step returns a `Result`,
/// yet the source item is still recovered: items never move *into* the
/// spawned tasks (only pre-serialized bytes do), so on any join failure the
/// remaining pending items return to the buffer instead of being lost with
/// their task.
pub(crate) async fn run_store_io(config: &SimpleContextConfig, plan: &mut GcPlan) -> GcIoResult {
    let dir = store::store_dir(config);
    let semaphore =
        std::sync::Arc::new(tokio::sync::Semaphore::new(store::MAX_STORE_IO_CONCURRENCY));
    let mut io = GcIoResult {
        externalized: Vec::new(),
        externalize_failed: Vec::new(),
        recalled: Vec::new(),
    };

    // Write overflow items concurrently. The item itself stays with the
    // caller (id-keyed) so a join failure cannot lose it; the task receives
    // only the pre-serialized bytes and writes them atomically.
    let pending = std::mem::take(&mut plan.externalize);
    let mut pending_items: std::collections::HashMap<ContextItemId, (ContextItem, Vec<u8>)> =
        pending
            .into_iter()
            .map(|(item, bytes)| (item.id, (item, bytes)))
            .collect();
    let mut writes = tokio::task::JoinSet::new();
    for (id, (_, bytes)) in pending_items.clone() {
        let dir = dir.clone();
        let semaphore = std::sync::Arc::clone(&semaphore);
        let permit = semaphore.acquire_owned().await.expect("semaphore");
        writes.spawn(async move {
            let _permit = permit;
            let outcome = store::externalize_async(&dir, id, &bytes).await;
            (id, outcome)
        });
    }
    while let Some(joined) = writes.join_next().await {
        match joined {
            Ok((id, Ok(checksum))) => {
                if let Some((item, _)) = pending_items.remove(&id) {
                    let context_ref = store::make_context_ref(&item);
                    io.externalized.push((item, context_ref, checksum));
                }
            }
            Ok((id, Err(_))) => {
                if let Some((item, _)) = pending_items.remove(&id) {
                    io.externalize_failed.push(item);
                }
            }
            // A task panicked: its id is unknowable, so the conservative
            // recovery returns *every* item that has not been consumed yet
            // to the buffer (a partially written blob is re-owned or
            // deleted by the startup reconcile). No item is lost with its
            // task.
            Err(_) => {
                io.externalize_failed
                    .extend(pending_items.drain().map(|(_, (item, _))| item));
                break;
            }
        }
    }
    // Any items whose tasks never completed (loop exited early) go back to
    // the buffer too.
    io.externalize_failed
        .extend(pending_items.into_iter().map(|(_, (item, _))| item));

    // Recall reads: only entries whose entities matched are read; failed
    // reads leave the entry in the map for a later pass. Same bounded
    // concurrency rationale as the writes.
    let recall: Vec<ContextItemId> = plan.recall_candidates.drain(..).collect();
    let mut reads = tokio::task::JoinSet::new();
    for item_id in recall {
        let dir = dir.clone();
        let semaphore = std::sync::Arc::clone(&semaphore);
        let permit = semaphore.acquire_owned().await.expect("semaphore");
        reads.spawn(async move {
            let _permit = permit;
            (item_id, store::read_item_async(&dir, item_id).await)
        });
    }
    while let Some(joined) = reads.join_next().await {
        if let Ok((_item_id, Some(item))) = joined {
            io.recalled.push(item);
        }
    }
    io
}

/// One full GC pass, phase 3 (commit, under a fresh state lock): apply the
/// IO results — externalized entries join the map, recalled items re-enter
/// the heap, failed writes return to the buffer — and assemble the report.
///
/// Returns the report plus the ids of store blobs that must be deleted
/// *after* the commit: successfully recalled content is resident again, so
/// its blob is only removed once the commit landed (a crash between commit
/// and delete leaves an orphan the startup reconcile re-owns).
pub(crate) fn commit_full_gc(
    state: &mut State,
    now_tick: u64,
    plan: GcPlan,
    io: GcIoResult,
) -> (ContextGcReport, Vec<ContextItemId>) {
    // The buffer: failed/undone writes come back at the front, so the
    // overflow retries on the next pass (order preserved, oldest first).
    let mut buffer = io.externalize_failed;
    buffer.append(&mut state.eviction_buffer);
    state.eviction_buffer = buffer;

    // The store map: successful writes become Cold entries (carrying the
    // checksum captured at write time for the reconcile)...
    let externalized_count = io.externalized.len();
    let externalized_ids: Vec<ContextItemId> =
        io.externalized.iter().map(|(item, _, _)| item.id).collect();
    // Store I/O accounting: the bodies written this pass, read back this
    // pass, and how many items were recalled (M15 baseline, aggregated by
    // the eval harness from the event stream).
    let store_write_bytes = io
        .externalized
        .iter()
        .map(|(item, _, _)| item.content.len() as u64)
        .sum::<u64>();
    for (item, context_ref, checksum) in io.externalized {
        state.gc_externalized_total += 1;
        state.external.push(store::to_external_entry(
            &item,
            context_ref,
            now_tick,
            state.gc_epoch,
            Some(checksum),
        ));
        crate::ledger::record(
            state,
            item.id,
            LifecycleAxis::Gc,
            "Warm",
            "Cold",
            "eviction buffer overflow; externalized to the context store",
            "gc",
            None,
        );
    }
    // ...and successfully recalled entries leave the map: their content is
    // resident again, so keeping the reference would duplicate it.
    let recalled_ids: HashSet<ContextItemId> = io.recalled.iter().map(|item| item.id).collect();
    if !recalled_ids.is_empty() {
        state
            .external
            .retain(|entry| !recalled_ids.contains(&entry.item_id));
    }
    let store_read_bytes = io
        .recalled
        .iter()
        .map(|item| item.content.len() as u64)
        .sum::<u64>();
    let store_recalled_items = io.recalled.len() as u64;

    // Recalled items re-enter the heap as active residents, exactly like a
    // warm-buffer reactivation.
    let mut recalled_reactivations: Vec<ContextReactivation> = Vec::new();
    for mut item in io.recalled {
        let reason = "entities are hot again in the working set (recalled from the context store)"
            .to_string();
        crate::ledger::record(
            state,
            item.id,
            LifecycleAxis::Gc,
            "Cold",
            "Resident",
            reason.clone(),
            "gc",
            None,
        );
        recalled_reactivations.push(ContextReactivation {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            reactivated_at_tick: now_tick,
            reason,
        });
        item.attention = AttentionState::Active;
        item.relevance = item.relevance.max(0.5);
        item.residency = ContextResidency::Resident;
        item.gc_generation = 0;
        item.evicted_at_tick = None;
        item.last_access_tick = now_tick;
        state.items.push(item);
        state.gc_reactivated_total += 1;
    }

    let mut report = ContextGcReport {
        resident: state.items.len(),
        evicted: plan.evicted,
        marked_roots: plan.marked_roots,
        externalized: externalized_count,
        reactivated: plan.reactivated + recalled_reactivations.len(),
        aged_external: plan.aged_external,
        anchor_roots_protected: plan.anchor_roots_protected,
        anchor_root_protections: collect_residency_protections(state),
        store_write_bytes,
        store_read_bytes,
        store_recalled_items,
        diagnostics: diagnostics::compute(state),
        ..ContextGcReport::default()
    };
    report.externalized_ids = externalized_ids;
    report.evictions = plan.evictions;
    let mut reactivations = plan.buffer_reactivations;
    reactivations.extend(recalled_reactivations);
    report.reactivations = reactivations;
    (report, recalled_ids.into_iter().collect())
}

/// Mark the root set: pins, members of the active focus scope tree, durable
/// task constraints, durable session memory, hot-entity matches, the latest
/// body of each recent file in the active task, and a bounded transitive
/// slice of their dependencies.
fn mark_roots(
    state: &State,
    config: &SimpleContextConfig,
    focus: Option<&FocusState>,
    hot_entities: &[String],
    latest_file_bodies: &HashSet<ContextItemId>,
) -> (Vec<ContextItemId>, usize) {
    let active_task = focus.map(|f| f.task_id);
    // The active focus scope of the current task: the attention container.
    // Members of the whole open subtree under it (open tool frames) are
    // roots; a closed frame drops out of the chain and loses protection.
    let active_focus_id = focus.as_ref().and_then(|f| {
        state
            .scopes
            .iter()
            .find(|scope| {
                scope.kind == ScopeKind::Focus
                    && scope.task_id == Some(f.task_id)
                    && scope.state == ScopeState::Active
            })
            .map(|scope| scope.id)
    });
    let mut marked: Vec<ContextItemId> = Vec::new();

    for item in &state.items {
        let is_pin =
            item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned;
        let in_active_focus_scope =
            active_focus_id.is_some_and(|focus_id| in_scope_chain(state, item, focus_id));
        // Durable task constraints (decisions, constraints of the current
        // task) are roots; the task itself is a boundary, not a root.
        let durable_task_constraint = active_task.is_some_and(|task| {
            item.task_id == Some(task)
                && item.retention == ContextRetention::Durable
                && item.scope == ContextScope::Task
        });
        // Pre-scope legacy items: only *active* working items of the current
        // task are protected (narrowed, so a long task's cooled history is
        // not rooted forever).
        let legacy_active_task_member = item.scope_id.is_none()
            && active_task.is_some_and(|task| item.task_id == Some(task))
            && item.attention == AttentionState::Active;
        let durable_session_memory = item.retention == ContextRetention::Durable
            && item.scope == ContextScope::Session
            // A completed task's outcome is a *storage* root, not a
            // residency root: the summary/decision it promoted to the
            // session is durable (storage GC protects it) but must not keep
            // the resident heap growing with every completed task. Only an
            // explicit reason (hot entity of a live task, pin, model
            // hint/lease) brings it back into the working set.
            && !task_completed(state, item.task_id);
        // A completed task's records are never roots through the hot set:
        // automatic recall of finished work requires an explicit reason,
        // and the task's own entities may linger in the hot set
        // after completion.
        let hot = !task_completed(state, item.task_id)
            && !hot_entities.is_empty()
            && entities_match(&item.entities, hot_entities);
        // Model/operator-directed protection (`context.gc_hint` /
        // `context.lease`): the model asked for this item to stay, so GC
        // treats it as a root until the hint is cleared or the lease runs
        // out. Explainable like every other root: "kept because the model
        // leased it until turn N / set keep_alive".
        let model_directed_root = item.keep_alive
            || item
                .lease_until_turn
                .is_some_and(|until| state.turn <= until);
        // TaskAnchor 投影的根声明（runtime 推送）：ResidentRequired /
        // PromptRequired 的声明指向的条目是根。任务权威在 TaskManager，
        // 这里只消费投影；semantic 死亡是终态，sweep 的 alive_root 仍
        // 要求 live，所以声明从不复活死条目。
        let anchor_rooted = state.anchor_roots.iter().any(|claim| {
            claim.strength.requires_residency()
                && crate::engine::anchor_claim_matches_item(claim, item)
        });
        let latest_file_body = latest_file_bodies.contains(&item.id);
        if is_pin
            || in_active_focus_scope
            || durable_task_constraint
            || legacy_active_task_member
            || durable_session_memory
            || hot
            || model_directed_root
            || anchor_rooted
            || latest_file_body
        {
            marked.push(item.id);
        }
    }

    // 本 pass 由 anchor 根声明保护的 *live* resident 条目数（报告可
    // 解释性）。semantic 死亡是终态，即使声明匹配也不算"受保护"——
    // sweep 不会让它存活，计数必须与 sweep 的存活判定一致。
    let anchor_roots_protected = state
        .items
        .iter()
        .filter(|item| {
            item.semantic.is_live()
                && state.anchor_roots.iter().any(|claim| {
                    claim.strength.requires_residency()
                        && crate::engine::anchor_claim_matches_item(claim, item)
                })
        })
        .count();

    // Reachability through dependency edges, bounded: a root pulls in the
    // items it *depends on*, so the evidence behind a working item stays
    // protected. The traversal follows `item.dependencies` (new -> old)
    // outward from the roots; dependents of a root are not protected — a
    // root's descendants carry no evidence the working set relies on.
    // Dependencies are resolved across every residency: the heap, the warm
    // buffer and the external map all carry edges, so a dependency that
    // was demoted Warm/Cold is marked here and the reactivate phase below
    // (which honors the same mark) recalls it.
    if config.dependency_expansion && !marked.is_empty() {
        let mut seen: HashSet<ContextItemId> = marked.iter().copied().collect();
        let mut queue: Vec<ContextItemId> = marked.clone();
        let mut added = 0usize;
        while let Some(id) = queue.pop() {
            if added >= MAX_MARKED_DEPENDENCIES {
                break;
            }
            let Some(edges) = dependency_edges(state, id) else {
                continue;
            };
            for edge in edges {
                if seen.insert(edge.target) {
                    marked.push(edge.target);
                    queue.push(edge.target);
                    added += 1;
                }
            }
        }
    }
    (marked, anchor_roots_protected)
}

/// Per-claim explanations for residency protections this pass. Bounded by
/// `MAX_ANCHOR_ROOT_CLAIMS`. StorageRequired claims are omitted — they are
/// not residency roots.
fn collect_residency_protections(state: &State) -> Vec<agent_contracts::AnchorRootProtection> {
    let mut out = Vec::new();
    for claim in &state.anchor_roots {
        if !claim.strength.requires_residency() {
            continue;
        }
        let hits = state.items.iter().any(|item| {
            item.semantic.is_live() && crate::engine::anchor_claim_matches_item(claim, item)
        });
        if hits {
            out.push(claim.into());
            if out.len() >= agent_contracts::MAX_ANCHOR_ROOT_CLAIMS {
                break;
            }
        }
    }
    out
}

/// The dependency edges of an item wherever its record lives: the resident
/// heap, the warm reversible buffer, or the external map (entries capture
/// their edges at externalize time). The mark traversal must walk the same
/// universe the sweep and the reactivate phase operate on — a dependency
/// that was demoted out of the heap would otherwise be marked, but never
/// found when reactivation looks for it.
fn dependency_edges(state: &State, id: ContextItemId) -> Option<&[DependencyEdge]> {
    if let Some(item) = state.items.iter().find(|item| item.id == id) {
        return Some(&item.dependencies);
    }
    if let Some(item) = state.eviction_buffer.iter().find(|item| item.id == id) {
        return Some(&item.dependencies);
    }
    state
        .external
        .get(id)
        .map(|entry| entry.dependencies.as_slice())
}

/// Whether the item's authoritative scope membership chain (up to the root)
/// contains `target_id` without crossing a closed scope. Open tool frames
/// under the active focus are included; closed frames drop out.
fn in_scope_chain(state: &State, item: &ContextItem, target_id: ScopeId) -> bool {
    let mut current = item.scope_id;
    while let Some(sid) = current {
        if sid == target_id {
            return true;
        }
        let Some(scope) = state.scopes.by_id(sid) else {
            return false;
        };
        if scope.state == ScopeState::Closed {
            return false;
        }
        current = scope.parent;
    }
    false
}

/// Whether a Working item is ordinary dialogue that outlived its shelf
/// life inside the *open* focus episode. Related messages share tokens, so
/// the score floor keeps them Active forever and the focus-scope root
/// would otherwise accumulate every turn of a long episode (a 500-turn
/// episode would hold ~500 messages). An item that carries no promotable
/// outcome (decision / finding / constraint / open-loop / artifact or
/// evidence ref), is not hot right now, is not model-directed, and is
/// older than the same staleness window residency uses (ttl x 4) leaves
/// the heap even though it is marked — it stays semantically Live in the
/// reversible buffer, so this is aging, not death. A hot item is exempt:
/// "entities are hot again" is a fresh causal reason, so hot ordinary
/// dialogue stays (or comes back).
fn aged_ordinary_dialogue(
    item: &ContextItem,
    config: &SimpleContextConfig,
    turn: u64,
    hot: bool,
) -> bool {
    item.retention == ContextRetention::Working
        && !crate::scope::retention_or_tag_promotable(item.retention, &item.tags)
        && !item.keep_alive
        && !item.lease_until_turn.is_some_and(|until| turn <= until)
        && !hot
        && turn.saturating_sub(item.created_turn) > config.turn_ttl_ticks * 4
}

/// Whether an unmarked item is an eviction candidate this pass. GC only
/// evicts what the semantic machine already demoted (or killed), so GC
/// never fights the policy: active items stay until attention cools them,
/// and semantically dead items leave the heap unconditionally.
fn eviction_candidate(
    item: &ContextItem,
    config: &SimpleContextConfig,
    turn: u64,
    generation: u32,
) -> bool {
    // Semantic death is terminal: leave the heap now (into the reversible
    // buffer, then the store) instead of lingering in checkpoints forever.
    if item.semantic.is_dead() {
        return true;
    }
    // A consumed ephemeral observation leaves attention immediately; it
    // stays semantically Live in the buffer so a hot-entity match can
    // recall it.
    if item.attention == AttentionState::Archived
        && item.retention == ContextRetention::Ephemeral
        && item.scope == ContextScope::Turn
    {
        return true;
    }
    match item.attention {
        // Never evict what the policy keeps active.
        AttentionState::Active => false,
        AttentionState::Cooling | AttentionState::Archived => {
            // TTL age is measured in user turns, never event ticks: a
            // preview or a burst of unrelated events must not age items.
            let age = turn.saturating_sub(item.created_turn);
            // Long past every TTL, or old enough in generations.
            if item.retention != ContextRetention::Durable && age > config.turn_ttl_ticks * 4 {
                return true;
            }
            generation >= config.gc_max_generation
        }
    }
}

fn eviction_reason(
    item: &ContextItem,
    config: &SimpleContextConfig,
    turn: u64,
    generation: u32,
) -> String {
    if item.semantic.is_dead() {
        return format!(
            "semantically dead ({:?}); evicted to reversible buffer (generation {generation})",
            item.semantic
        );
    }
    if item.attention == AttentionState::Archived
        && item.retention == ContextRetention::Ephemeral
        && item.scope == ContextScope::Turn
    {
        return format!(
            "ephemeral observation consumed; evicted to reversible buffer (generation {generation})"
        );
    }
    match item.attention {
        AttentionState::Cooling | AttentionState::Archived => {
            let age = turn.saturating_sub(item.created_turn);
            if item.retention != ContextRetention::Durable && age > config.turn_ttl_ticks * 4 {
                format!(
                    "stale: age {age} turns > ttl x4 = {}; not reachable from roots",
                    config.turn_ttl_ticks * 4
                )
            } else {
                format!(
                    "survived {generation} GC passes without root reachability (max {})",
                    config.gc_max_generation
                )
            }
        }
        AttentionState::Active => {
            format!("unreachable from roots despite active state (generation {generation})")
        }
    }
}

/// Bring evicted items back when they are relevant again: a pin, hot
/// entities in the working set, or a score that clears the active threshold.
/// Newest evictions first, bounded per pass. Items evicted by the current
/// pass are skipped so eviction stays effective, and semantically dead
/// items (superseded decisions, verified-fixed errors, tombstoned) never
/// resurrect — their labels exclude them from the model, so an Active +
/// Resident revival would be a state-space inconsistency. Cold store
/// entries get the same second chance via hot-entity recall — but only
/// their ids are collected here; the content read happens in the IO phase
/// without the state lock.
fn reactivate(
    state: &mut State,
    config: &SimpleContextConfig,
    now_tick: u64,
    plan: &mut GcPlan,
    marked: &HashSet<ContextItemId>,
) {
    let focus = state.focus.clone();
    let hot_entities = state.hot_entities.clone();
    let mut remaining = config.gc_reactivate_per_pass;

    // Warm buffer entries (content still in memory).
    let mut index = state.eviction_buffer.len();
    while index > 0 && remaining > 0 {
        index -= 1;
        let item = &state.eviction_buffer[index];
        if item.evicted_at_tick == Some(now_tick) {
            continue;
        }
        // A member of a closed scope (a rotated episode, a closed tool
        // frame) may come back only for a *new causal reason* — a hot
        // entity or a model directive — never for the residency score
        // floor, which is what kept it Active across turns.
        let scope_closed = item.scope_id.is_some_and(|sid| {
            state
                .scopes
                .by_id(sid)
                .is_none_or(|scope| scope.state == ScopeState::Closed)
        });
        if !item.semantic.is_live() {
            // Semantic death is terminal: a superseded decision, a
            // verified-fixed error or a tombstoned item stays evicted
            // however hot it looks.
            continue;
        }
        // Aged ordinary dialogue stays out of the working set: its score
        // floor is exactly what the aging rule exists to bound, so a high
        // score (or its focus-scope membership) must not bounce it back
        // and forth across the heap/buffer boundary every GC pass. Only a
        // hot entity right now (a fresh causal reason, checked by
        // `aged_ordinary_dialogue` itself) exempts it. An anchor root
        // claim exempts it the same way — task authority outranks the
        // aging heuristic (computed below and re-checked here).
        let anchor_rooted = state.anchor_roots.iter().any(|claim| {
            claim.strength.requires_residency()
                && crate::engine::anchor_claim_matches_item(claim, item)
        });
        let hot_now = !hot_entities.is_empty() && entities_match(&item.entities, &hot_entities);
        if !anchor_rooted && aged_ordinary_dialogue(item, config, state.turn, hot_now) {
            continue;
        }
        let live_root_dep = marked.contains(&item.id);
        let Some(reason) = reactivation_reason(
            item,
            &ReactivationInput {
                config,
                focus: focus.as_ref(),
                hot_entities: &hot_entities,
                current_turn: state.turn,
                guard: RecallGuard {
                    scope_closed,
                    completed_task: task_completed(state, item.task_id),
                },
                live_root_dep,
                anchor_rooted,
            },
        ) else {
            continue;
        };
        let mut item = state.eviction_buffer.remove(index);
        item.attention = AttentionState::Active;
        item.relevance = item.relevance.max(0.5);
        item.residency = ContextResidency::Resident;
        item.gc_generation = 0;
        item.evicted_at_tick = None;
        item.last_access_tick = now_tick;
        crate::ledger::record(
            state,
            item.id,
            LifecycleAxis::Gc,
            "Warm",
            "Resident",
            reason.clone(),
            "gc",
            None,
        );
        plan.buffer_reactivations.push(ContextReactivation {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            reactivated_at_tick: now_tick,
            reason,
        });
        plan.reactivated += 1;
        if anchor_rooted {
            plan.anchor_roots_protected += 1;
        }
        state.gc_reactivated_total += 1;
        // The heap push indexes the item at its slot in the same step.
        state.items.push(item);
        remaining -= 1;
    }

    // Cold store entries: content lives in the store; recall is earned by
    // a marked dependency (required evidence — the root that depends on it
    // is live) or a hot-entity match (no content in memory, so no
    // score-based fallback). The entity filter runs on the in-memory entry
    // signature first, so with thousands of Cold entries only the matching
    // ids are read in the IO phase. Exact matches come from the map's
    // entity index (O(bucket) per hot entity instead of a full scan);
    // substring-tolerant overlaps — hot `AuthService.rs` vs an entry
    // entity `src/auth/AuthService.rs` — cannot be indexed with exact
    // keys, so a residual scan covers the entries the index did not
    // already propose. Coverage is preserved; the common exact-match case
    // is fast. Skip the whole pass when no store directory exists yet.
    if remaining > 0
        && store::store_ready(config)
        && (!hot_entities.is_empty() || !marked.is_empty() || !state.anchor_roots.is_empty())
    {
        let mut covered: HashSet<ContextItemId> = HashSet::new();
        // Anchor root claims first: a Cold entry a ResidentRequired /
        // PromptRequired claim targets must be recalled — task authority
        // says it belongs in the working set, regardless of hot entities.
        if !state.anchor_roots.is_empty() {
            for entry in state.external.iter() {
                if remaining == 0 {
                    break;
                }
                let claimed = state.anchor_roots.iter().any(|claim| {
                    claim.strength.requires_residency()
                        && crate::engine::anchor_claim_matches_entry(claim, entry)
                });
                if claimed
                    && entry.semantic.is_live()
                    && store::recallable(entry)
                    && covered.insert(entry.item_id)
                {
                    plan.recall_candidates.push(entry.item_id);
                    plan.anchor_roots_protected += 1;
                    remaining -= 1;
                }
            }
        }
        // Marked dependencies next: a Cold entry that a live root depends
        // on is recalled even when no hot entity names it.
        for entry in state.external.iter() {
            if remaining == 0 {
                break;
            }
            if marked.contains(&entry.item_id)
                && entry.semantic.is_live()
                && store::recallable(entry)
                && covered.insert(entry.item_id)
            {
                plan.recall_candidates.push(entry.item_id);
                remaining -= 1;
            }
        }
        if remaining > 0 && !hot_entities.is_empty() {
            for hot in &hot_entities {
                if remaining == 0 {
                    break;
                }
                for id in state.catalog.ids_for_entity(hot) {
                    if remaining == 0 {
                        break;
                    }
                    if state.catalog.location(*id)
                        != Some(crate::index::catalog::CatalogLocation::Stored)
                    {
                        continue;
                    }
                    if !covered.insert(*id) {
                        continue;
                    }
                    let Some(entry) = state.external.get(*id) else {
                        continue;
                    };
                    if entry.semantic.is_live()
                        && store::recallable(entry)
                        && !task_completed(state, entry.task_id)
                        && entities_match(&entry.entities, &hot_entities)
                    {
                        plan.recall_candidates.push(*id);
                        remaining -= 1;
                    }
                }
            }
            if remaining > 0 {
                for entry in state.external.iter() {
                    if remaining == 0 {
                        break;
                    }
                    if covered.contains(&entry.item_id) {
                        continue;
                    }
                    if entry.semantic.is_live()
                        && store::recallable(entry)
                        && !task_completed(state, entry.task_id)
                        && entities_match(&entry.entities, &hot_entities)
                    {
                        plan.recall_candidates.push(entry.item_id);
                        remaining -= 1;
                    }
                }
            }
        }
    }
}

/// Whether the item's task has completed: its Task scope is closed. A
/// completed task's records may return to the working set only for an
/// explicit reason (pin, model hint/lease), never for automatic hot-entity
/// recall.
fn task_completed(state: &State, task_id: Option<TaskId>) -> bool {
    task_id.is_some_and(|tid| {
        state.scopes.iter().any(|scope| {
            scope.kind == ScopeKind::Task
                && scope.task_id == Some(tid)
                && scope.state == ScopeState::Closed
        })
    })
}

/// What a recall candidate may come back for. A member of a closed scope
/// may return only for a fresh causal reason — a hot entity or a model
/// directive — never for the residency score floor. A completed task's
/// record needs an explicit reason (pin/hint/lease); automatic hot-entity
/// recall is forbidden.
#[derive(Clone, Copy)]
struct RecallGuard {
    scope_closed: bool,
    completed_task: bool,
}

/// Everything a recall decision needs beyond the candidate item itself,
/// grouped so the reason function stays small and single-purpose.
struct ReactivationInput<'a> {
    config: &'a SimpleContextConfig,
    focus: Option<&'a FocusState>,
    hot_entities: &'a [String],
    current_turn: u64,
    guard: RecallGuard,
    /// The item was marked during the mark phase because a live root
    /// depends on it.
    live_root_dep: bool,
    /// The item is targeted by a ResidentRequired/PromptRequired anchor
    /// root claim — task authority says it must be resident.
    anchor_rooted: bool,
}

/// Why an evicted item earns a second chance. `None` keeps it in the buffer.
/// Task membership alone is deliberately not enough: within an active task
/// the semantic machine already keeps working items; an evicted item must
/// show fresh relevance (hot entities or a high score) to come back.
fn reactivation_reason(item: &ContextItem, input: &ReactivationInput) -> Option<String> {
    // A marked dependency is required evidence: the root that depends on
    // it is live right now, so the item returns regardless of closed-scope
    // or completed-task guards. This closes the mark/reactivate universe
    // gap — the mark phase walks the buffer and the store, and this pass
    // honors those marks.
    if input.live_root_dep {
        return Some(
            "dependency of a marked root (reachable through dependency edges)".to_string(),
        );
    }
    // An anchor root claim is the same kind of authority: the active task's
    // TaskAnchor says this item must stay resident, so a claim-targeted
    // evicted item re-enters the heap (semantic death stays terminal — the
    // caller filters dead items before this decision).
    if input.anchor_rooted {
        return Some("protected by an anchor root claim (task authority)".to_string());
    }
    if item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned {
        return Some("explicitly pinned again".to_string());
    }
    // Model-directed protection brings an evicted item back: the hint or
    // lease is a root claim, so the buffer item re-enters the heap.
    if item.keep_alive {
        return Some("kept alive by a model gc_hint".to_string());
    }
    if let Some(until) = item.lease_until_turn
        && input.current_turn <= until
    {
        return Some(format!("leased by the model until turn {until}"));
    }
    if !input.hot_entities.is_empty() && entities_match(&item.entities, input.hot_entities) {
        if input.guard.completed_task {
            // Automatic recall of a completed task's record is forbidden
            // without a new explicit reason: the hot set alone is
            // not enough to bring finished work back as current truth.
            return None;
        }
        return Some("entities are hot again in the working set".to_string());
    }
    // The score is the fallback: a genuinely high-value item (importance,
    // retention, affinity) may still be worth reactivating even without a
    // root match — explainable, not learned. Closed-scope members are
    // excluded: their score floor is what kept them resident across turns.
    // A completed task's record is excluded the same way: only an explicit
    // reason (pin, model hint/lease, marked dependency) brings finished
    // work back, never the residency score floor.
    if !input.guard.scope_closed && !input.guard.completed_task {
        let breakdown =
            score_item_with_breakdown(item, input.focus, input.hot_entities, input.current_turn);
        if breakdown.total >= input.config.active_threshold {
            return Some(format!(
                "score {:.2} >= active threshold {:.2}",
                breakdown.total, input.config.active_threshold
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests;
