use std::collections::HashSet;

use agent_contracts::{
    AttentionState, ContextEviction, ContextGcReport, ContextItem, ContextItemId,
    ContextReactivation, ContextRef, ContextResidency, ContextRetention, ContextScope, FocusState,
    ScopeId, ScopeKind, ScopeState,
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
    /// first; removed from the buffer by the plan).
    pub(crate) externalize: Vec<ContextItem>,
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
}

/// The store IO outcomes, applied by the commit under a fresh lock.
pub(crate) struct GcIoResult {
    /// (item, reference) successfully written to the store.
    pub(crate) externalized: Vec<(ContextItem, ContextRef)>,
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
) -> Option<GcPlan> {
    if !config.gc_enabled || state.items.is_empty() && state.eviction_buffer.is_empty() {
        return None;
    }

    // One full GC generation. External aging and TTLs count generations —
    // only this counter advances on a real pass, unlike `tick` which also
    // grows on ingest/maintain/materialize.
    state.gc_epoch += 1;
    let gc_epoch = state.gc_epoch;

    let mut plan = GcPlan {
        externalize: Vec::new(),
        recall_candidates: Vec::new(),
        evictions: Vec::new(),
        buffer_reactivations: Vec::new(),
        marked_roots: 0,
        evicted: 0,
        reactivated: 0,
        aged_external: 0,
    };

    // ----- Mark phase: the root set --------------------------------
    // Roots are the current attention: pins, members of the active focus
    // scope (including open tool frames under it), durable task
    // constraints, durable session memory, items whose entities are hot,
    // plus a bounded slice of their dependencies. The task id alone is a
    // boundary, not a root — a long task's cooled history is not protected
    // just because it shares the task id.
    let focus = state.focus.clone();
    let hot_entities = state.hot_entities.clone();
    let marked = mark_roots(state, config, focus.as_ref(), &hot_entities);

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
        let consumed_ephemeral = item.attention == AttentionState::Archived
            && item.retention == ContextRetention::Ephemeral
            && item.scope == ContextScope::Turn;
        let alive_root = item.semantic.is_live()
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
        if eviction_candidate(&item, config, now_tick, generation) {
            let reason = eviction_reason(&item, config, now_tick, generation);
            plan.evictions.push(ContextEviction {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                generation,
                evicted_at_tick: now_tick,
                reason,
            });
            plan.evicted += 1;
            state.gc_evicted_total += 1;
            let mut evicted = item;
            evicted.residency = ContextResidency::Warm;
            evicted.evicted_at_tick = Some(now_tick);
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
    // chance on hot entities / a high score.
    reactivate(state, config, now_tick, &mut plan);

    // ----- Externalize phase: the buffer is bounded -----------------
    // Context GC never purges: overflow writes the item to the context
    // store and keeps only a lightweight entry (Cold), which later ages to
    // External. The *decision* is taken here; the writes happen in the IO
    // phase so the lock is not held across disk IO. Only Storage GC may
    // delete store files.
    while state.eviction_buffer.len() > config.gc_buffer_capacity {
        let item = state.eviction_buffer.remove(0);
        plan.externalize.push(item);
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
pub(crate) async fn run_store_io(config: &SimpleContextConfig, plan: &mut GcPlan) -> GcIoResult {
    let dir = store::store_dir(config);
    let mut io = GcIoResult {
        externalized: Vec::new(),
        externalize_failed: Vec::new(),
        recalled: Vec::new(),
    };
    // Write overflow items. On the first failure the failed item and every
    // remaining candidate go back to the buffer (the store-unavailable
    // fallback, applied by the commit).
    let pending = std::mem::take(&mut plan.externalize);
    let mut pending = pending.into_iter();
    while let Some(item) = pending.next() {
        match store::externalize_async(&dir, &item).await {
            Ok(context_ref) => io.externalized.push((item, context_ref)),
            Err(_) => {
                io.externalize_failed.push(item);
                io.externalize_failed.extend(pending);
                break;
            }
        }
    }
    // Recall reads: only entries whose entities matched are read; failed
    // reads leave the entry in the map for a later pass.
    for item_id in plan.recall_candidates.drain(..) {
        if let Some(item) = store::read_item_async(&dir, item_id).await {
            io.recalled.push(item);
        }
    }
    io
}

/// One full GC pass, phase 3 (commit, under a fresh state lock): apply the
/// IO results — externalized entries join the map, recalled items re-enter
/// the heap, failed writes return to the buffer — and assemble the report.
pub(crate) fn commit_full_gc(
    state: &mut State,
    now_tick: u64,
    plan: GcPlan,
    io: GcIoResult,
) -> ContextGcReport {
    // The buffer: failed/undone writes come back at the front, so the
    // overflow retries on the next pass (order preserved, oldest first).
    let mut buffer = io.externalize_failed;
    buffer.append(&mut state.eviction_buffer);
    state.eviction_buffer = buffer;

    // The store map: successful writes become Cold entries...
    let externalized_count = io.externalized.len();
    for (item, context_ref) in io.externalized {
        state.gc_externalized_total += 1;
        state.external.push(store::to_external_entry(
            &item,
            context_ref,
            now_tick,
            state.gc_epoch,
        ));
    }
    // ...and successfully recalled entries leave the map: their content is
    // resident again, so keeping the reference would duplicate it.
    let recalled_ids: HashSet<ContextItemId> = io.recalled.iter().map(|item| item.id).collect();
    if !recalled_ids.is_empty() {
        state
            .external
            .retain(|entry| !recalled_ids.contains(&entry.item_id));
    }

    // Recalled items re-enter the heap as active residents, exactly like a
    // warm-buffer reactivation.
    let mut recalled_reactivations: Vec<ContextReactivation> = Vec::new();
    for mut item in io.recalled {
        let reason = "entities are hot again in the working set (recalled from the context store)"
            .to_string();
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
        diagnostics: diagnostics::compute(state),
        ..ContextGcReport::default()
    };
    report.evictions = plan.evictions;
    let mut reactivations = plan.buffer_reactivations;
    reactivations.extend(recalled_reactivations);
    report.reactivations = reactivations;
    report
}

/// Mark the root set: pins, members of the active focus scope tree, durable
/// task constraints, durable session memory, hot-entity matches, and a
/// bounded transitive slice of their dependencies.
fn mark_roots(
    state: &State,
    config: &SimpleContextConfig,
    focus: Option<&FocusState>,
    hot_entities: &[String],
) -> Vec<ContextItemId> {
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
        let durable_session_memory =
            item.retention == ContextRetention::Durable && item.scope == ContextScope::Session;
        let hot = !hot_entities.is_empty() && entities_match(&item.entities, hot_entities);
        // Model/operator-directed protection (`context.gc_hint` /
        // `context.lease`): the model asked for this item to stay, so GC
        // treats it as a root until the hint is cleared or the lease runs
        // out. Explainable like every other root: "kept because the model
        // leased it until turn N / set keep_alive".
        let model_directed_root = item.keep_alive
            || item
                .lease_until_turn
                .is_some_and(|until| state.turn <= until);
        if is_pin
            || in_active_focus_scope
            || durable_task_constraint
            || legacy_active_task_member
            || durable_session_memory
            || hot
            || model_directed_root
        {
            marked.push(item.id);
        }
    }

    // Reachability through dependency edges, bounded: a root pulls in the
    // items it *depends on*, so the evidence behind a working item stays
    // protected. The traversal follows `item.dependencies` (new -> old)
    // outward from the roots; dependents of a root are not protected — a
    // root's descendants carry no evidence the working set relies on.
    if config.dependency_expansion && !marked.is_empty() {
        let mut seen: HashSet<ContextItemId> = marked.iter().copied().collect();
        let mut queue: Vec<ContextItemId> = marked.clone();
        let mut added = 0usize;
        while let Some(id) = queue.pop() {
            if added >= MAX_MARKED_DEPENDENCIES {
                break;
            }
            let Some(item) = state.items.iter().find(|item| item.id == id) else {
                continue;
            };
            for dependency in &item.dependencies {
                if seen.insert(*dependency) {
                    marked.push(*dependency);
                    queue.push(*dependency);
                    added += 1;
                }
            }
        }
    }
    marked
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
        let Some(scope) = state.scopes.iter().find(|scope| scope.id == sid) else {
            return false;
        };
        if scope.state == ScopeState::Closed {
            return false;
        }
        current = scope.parent;
    }
    false
}

/// Whether an unmarked item is an eviction candidate this pass. GC only
/// evicts what the semantic machine already demoted (or killed), so GC
/// never fights the policy: active items stay until attention cools them,
/// and semantically dead items leave the heap unconditionally.
fn eviction_candidate(
    item: &ContextItem,
    config: &SimpleContextConfig,
    now_tick: u64,
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
            let age = now_tick.saturating_sub(item.created_tick);
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
    now_tick: u64,
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
            let age = now_tick.saturating_sub(item.created_tick);
            if item.retention != ContextRetention::Durable && age > config.turn_ttl_ticks * 4 {
                format!(
                    "stale: age {age} > ttl x4 = {}; not reachable from roots",
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
fn reactivate(state: &mut State, config: &SimpleContextConfig, now_tick: u64, plan: &mut GcPlan) {
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
        if !item.semantic.is_live() {
            // Semantic death is terminal: a superseded decision, a
            // verified-fixed error or a tombstoned item stays evicted
            // however hot it looks.
            continue;
        }
        let Some(reason) = reactivation_reason(
            item,
            config,
            focus.as_ref(),
            &hot_entities,
            now_tick,
            state.turn,
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
        plan.buffer_reactivations.push(ContextReactivation {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            reactivated_at_tick: now_tick,
            reason,
        });
        plan.reactivated += 1;
        state.gc_reactivated_total += 1;
        // The heap push indexes the item at its slot in the same step.
        state.items.push(item);
        remaining -= 1;
    }

    // Cold store entries: content lives in the store; only hot-entity
    // matches earn a recall (no content, so no score-based fallback). The
    // entity filter runs on the in-memory entry signature first, so with
    // thousands of Cold entries only the matching ids are read in the IO
    // phase. Exact matches come from the map's entity index (O(bucket) per
    // hot entity instead of a full scan); substring-tolerant overlaps —
    // hot `AuthService.rs` vs an entry entity `src/auth/AuthService.rs` —
    // cannot be indexed with exact keys, so a residual scan covers the
    // entries the index did not already propose. Coverage is preserved;
    // the common exact-match case is fast. Skip the whole pass when no
    // store directory exists yet.
    if remaining > 0 && !hot_entities.is_empty() && store::store_ready(config) {
        let mut covered: HashSet<ContextItemId> = HashSet::new();
        for hot in &hot_entities {
            if remaining == 0 {
                break;
            }
            for id in state.external.ids_for_entity(hot) {
                if remaining == 0 {
                    break;
                }
                if !covered.insert(*id) {
                    continue;
                }
                let Some(entry) = state.external.get(*id) else {
                    continue;
                };
                if entry.semantic.is_live()
                    && store::recallable(entry)
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
                    && entities_match(&entry.entities, &hot_entities)
                {
                    plan.recall_candidates.push(entry.item_id);
                    remaining -= 1;
                }
            }
        }
    }
}

/// Why an evicted item earns a second chance. `None` keeps it in the buffer.
/// Task membership alone is deliberately not enough: within an active task
/// the semantic machine already keeps working items; an evicted item must
/// show fresh relevance (hot entities or a high score) to come back.
fn reactivation_reason(
    item: &ContextItem,
    config: &SimpleContextConfig,
    focus: Option<&FocusState>,
    hot_entities: &[String],
    now_tick: u64,
    current_turn: u64,
) -> Option<String> {
    if item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned {
        return Some("explicitly pinned again".to_string());
    }
    // Model-directed protection brings an evicted item back: the hint or
    // lease is a root claim, so the buffer item re-enters the heap.
    if item.keep_alive {
        return Some("kept alive by a model gc_hint".to_string());
    }
    if let Some(until) = item.lease_until_turn
        && current_turn <= until
    {
        return Some(format!("leased by the model until turn {until}"));
    }
    if !hot_entities.is_empty() && entities_match(&item.entities, hot_entities) {
        return Some("entities are hot again in the working set".to_string());
    }
    // The score is the fallback: a genuinely high-value item (importance,
    // retention, affinity) may still be worth reactivating even without a
    // root match — explainable, not learned.
    let breakdown = score_item_with_breakdown(item, focus, hot_entities, now_tick);
    if breakdown.total >= config.active_threshold {
        return Some(format!(
            "score {:.2} >= active threshold {:.2}",
            breakdown.total, config.active_threshold
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SimpleContextEngine;
    use crate::index::entity::extract_entities;
    use agent_contracts::{
        ContextEngine, ContextIngress, ContextItemId, ContextKind, ContextMaintenanceTrigger,
        ContextRetention, TaskId, ToolOutput,
    };
    use serde_json::json;

    #[tokio::test]
    async fn gc_evicts_consumed_ephemeral_observations_with_a_reason() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();
        // A successful observation is ephemeral and leaves attention after
        // the turn (consumed, not tombstoned — it stays recallable).
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: "tests passed in AuthService.rs".into(),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();

        let before = engine.diagnostics().await.unwrap();
        assert!(before.archived_items >= 1, "ephemeral observation consumed");

        let report = engine.gc().await.unwrap();
        assert!(
            report.evicted >= 1,
            "gc must evict the consumed observation"
        );
        assert!(
            report
                .evictions
                .iter()
                .any(|e| e.reason.contains("observation consumed")),
            "eviction must be explainable, got: {:?}",
            report.evictions
        );

        let after = engine.diagnostics().await.unwrap();
        assert_eq!(
            after.warm_items, 1,
            "the consumed observation leaves the heap for the reversible buffer"
        );
        assert_eq!(after.total_items, 1, "only the user message stays resident");
    }

    #[tokio::test]
    async fn gc_marks_roots_and_evicts_stale_archived_items_by_generation() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();

        // Archive a cold item from another task, outside the active focus
        // scope tree: unmarked, past the generation cap, entities not hot.
        {
            let mut state = engine.state.lock().await;
            for item in &mut state.items {
                if item.kind == ContextKind::UserMessage {
                    item.task_id = Some(TaskId::new());
                    item.scope_id = None; // no focus-scope membership
                    item.content = "fix CacheStore.rs".into();
                    item.entities = extract_entities(&item.content);
                    item.attention = AttentionState::Archived;
                    item.relevance = 0.0;
                    item.gc_generation = 99; // already past the cap
                }
            }
        }

        let report = engine.gc().await.unwrap();
        assert_eq!(report.marked_roots, 0, "no roots in the test heap");
        assert_eq!(report.evicted, 1, "the cold archived item is evicted");
        assert!(
            report.evictions[0].reason.contains("GC passes"),
            "generational reason expected, got: {}",
            report.evictions[0].reason
        );
    }

    #[tokio::test]
    async fn gc_reactivates_warm_items_whose_entities_become_hot_again() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        // Round 1: the agent works on AuthService.rs; the successful
        // observation drops after the turn and gc evicts it.
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: "touched AuthService.rs".into(),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        let report = engine.gc().await.unwrap();
        assert!(report.evicted >= 1, "something must be evicted first");

        // Round 2: the user asks about AuthService.rs again — its entities
        // are hot, so the next gc reactivates the evicted observations.
        engine
            .ingest(ContextIngress::UserMessage {
                content: "what did we change in AuthService.rs?".into(),
            })
            .await
            .unwrap();
        let report = engine.gc().await.unwrap();
        assert!(report.reactivated >= 1, "evicted items must come back");
        assert!(
            report
                .reactivations
                .iter()
                .any(|r| r.reason.contains("hot again")),
            "reactivation must be explainable, got: {:?}",
            report.reactivations
        );

        let diagnostics = engine.diagnostics().await.unwrap();
        assert_eq!(
            diagnostics.gc_reactivated_total as usize, report.reactivated,
            "cumulative counter matches"
        );
    }

    #[tokio::test]
    async fn gc_buffer_overflow_externalizes_instead_of_purging() {
        let store = tempfile::tempdir().unwrap();
        let config = SimpleContextConfig {
            gc_buffer_capacity: 2,
            context_store_dir: Some(store.path().to_path_buf()),
            ..SimpleContextConfig::default()
        };
        let engine = SimpleContextEngine::new(config);

        // Three turns on distinct files: each successful observation is
        // consumed, evicted, and stays evicted (its entities are not hot
        // again).
        let mut last = None;
        for i in 0..3 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("task round {i} in File{i}.rs"),
                })
                .await
                .unwrap();
            engine
                .ingest(ContextIngress::ToolObservation {
                    output: ToolOutput {
                        call_id: i.to_string(),
                        tool_name: "shell.exec".into(),
                        ok: true,
                        summary: "ok".into(),
                        model_content: format!("touched File{i}.rs round {i}"),
                        artifact_ref: None,
                        metadata: json!({}),
                    },
                    scope_id: None,
                })
                .await
                .unwrap();
            engine
                .maintain(ContextMaintenanceTrigger::AfterModel)
                .await
                .unwrap();
            last = Some(engine.gc().await.unwrap());
        }

        let last = last.expect("three gc passes ran");
        assert_eq!(
            last.externalized, 1,
            "overflow externalizes the oldest eviction instead of purging"
        );
        let diagnostics = engine.diagnostics().await.unwrap();
        assert_eq!(diagnostics.warm_items, 2, "buffer stays bounded");
        assert_eq!(diagnostics.cold_items, 1, "the externalized item is Cold");
        // The store keeps the full content: nothing was deleted.
        let files = std::fs::read_dir(store.path()).unwrap().count();
        assert_eq!(files, 1, "the externalized item's content lives on disk");
    }

    #[tokio::test]
    async fn gc_generation_increments_for_survivors_and_evicts_at_the_cap() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();
        // A cold, unmarked working item from another task: Cooling, entities
        // not hot, so nothing marks it and the generational counter climbs.
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: false,
                    summary: "fail".into(),
                    model_content: "error in CacheStore.rs:42".into(),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
        {
            let mut state = engine.state.lock().await;
            for item in &mut state.items {
                if item.kind == ContextKind::Error {
                    // Cold and unmarked: another task, outside the active
                    // focus scope tree, entities outside the hot set (the
                    // error itself contributed CacheStore.rs to the hot set
                    // at ingest, so the content must change).
                    item.task_id = Some(TaskId::new());
                    item.scope_id = None;
                    item.content = "error in TempStore.rs:7".into();
                    item.entities = extract_entities(&item.content);
                    item.attention = AttentionState::Cooling;
                }
            }
        }

        let mut evicted_at_cap = None;
        for pass in 0..4 {
            let report = engine.gc().await.unwrap();
            if report
                .evictions
                .iter()
                .any(|e| e.reason.contains("GC passes"))
            {
                evicted_at_cap = Some((pass, report.evictions[0].generation));
            }
        }
        let (pass, generation) = evicted_at_cap.expect("the cooling item is evicted at the cap");
        assert_eq!(generation, 3, "eviction happens once generation 3 >= max 3");
        assert_eq!(pass, 3, "it takes the full generational ladder to evict");
    }

    #[tokio::test]
    async fn gc_protects_dependencies_of_roots_forward_along_the_edges() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        // A: an old decision; B: the current finding that depends on A.
        engine
            .ingest(ContextIngress::UserMessage {
                content: "use AuthService.rs as the auth layer".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: "touched AuthService.rs".into(),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
        let (a_id, b_id) = {
            let state = engine.state.lock().await;
            let a = state
                .items
                .iter()
                .find(|item| item.kind == ContextKind::UserMessage)
                .expect("decision item");
            let b = state
                .items
                .iter()
                .find(|item| item.kind == ContextKind::ToolObservation)
                .expect("finding item");
            assert!(
                b.dependencies.contains(&a.id),
                "the finding must depend on the decision it builds on"
            );
            (a.id, b.id)
        };
        {
            let mut state = engine.state.lock().await;
            for item in &mut state.items {
                if item.id == a_id {
                    // Cold and unmarked: another task, outside the focus
                    // scope tree, entities outside the hot set, past the
                    // generation cap — only the dependency edge from the
                    // root can protect it now.
                    item.task_id = Some(TaskId::new());
                    item.scope_id = None;
                    item.content = "use OldStore.rs instead".into();
                    item.entities = extract_entities(&item.content);
                    item.attention = AttentionState::Archived;
                    item.relevance = 0.0;
                    item.gc_generation = 99;
                }
                if item.id == b_id {
                    // The root: pinned, so nothing else can protect A.
                    item.retention = ContextRetention::Pinned;
                }
            }
        }

        let report = engine.gc().await.unwrap();
        assert!(
            report.marked_roots >= 2,
            "the root and its dependency must be marked, got {:?}",
            report.evictions
        );
        assert!(
            !report.evictions.iter().any(|e| e.item_id == a_id),
            "the dependency of a root must be protected by the forward edge, got: {:?}",
            report.evictions
        );
        let diagnostics = engine.diagnostics().await.unwrap();
        assert!(
            state_has(&engine, a_id).await,
            "the old decision must still be resident; diagnostics: {diagnostics:?}"
        );
    }

    #[tokio::test]
    async fn gc_never_resurrects_superseded_items() {
        // Dependency expansion is off so the (correctly protected) old
        // decision is not rooted through the new decision's dependency edge;
        // this test isolates the reactivation exclusion.
        let engine = SimpleContextEngine::new(SimpleContextConfig {
            dependency_expansion: false,
            ..SimpleContextConfig::default()
        });
        // A decision, then a newer decision supersedes it.
        engine
            .ingest(ContextIngress::UserMessage {
                content: "use AuthService.rs as the auth layer".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::UserMessage {
                content: "use AuthService.rs instead".into(),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        let old_id = {
            let state = engine.state.lock().await;
            state
                .items
                .iter()
                .find(|item| item.content.contains("as the auth layer"))
                .expect("the superseded decision")
                .id
        };
        {
            let mut state = engine.state.lock().await;
            for item in &mut state.items {
                if item.id == old_id {
                    assert!(
                        matches!(
                            item.semantic,
                            agent_contracts::SemanticState::Superseded { .. }
                        ),
                        "the older decision must be superseded, got {:?}",
                        item.semantic
                    );
                    // Evictable: past the generation cap, another task,
                    // outside the focus scope tree, and its entities leave
                    // the hot set so nothing roots it.
                    item.task_id = Some(TaskId::new());
                    item.scope_id = None;
                    item.content = "use OldStore.rs instead".into();
                    item.entities = extract_entities(&item.content);
                    item.gc_generation = 99;
                }
            }
        }
        let first = engine.gc().await.unwrap();
        assert!(
            first.evictions.iter().any(|e| e.item_id == old_id),
            "the superseded item must be evicted first"
        );

        // Its old entities become hot again — but semantic death is
        // terminal: the item must stay in the reversible buffer.
        {
            let mut state = engine.state.lock().await;
            for item in &mut state.items {
                if item.id == old_id {
                    item.entities = extract_entities("AuthService.rs");
                }
            }
        }
        engine
            .ingest(ContextIngress::UserMessage {
                content: "what about AuthService.rs?".into(),
            })
            .await
            .unwrap();
        let second = engine.gc().await.unwrap();
        assert!(
            !second.reactivations.iter().any(|r| r.item_id == old_id),
            "a superseded item must never resurrect, got: {:?}",
            second.reactivations
        );
        let diagnostics = engine.diagnostics().await.unwrap();
        assert_eq!(
            diagnostics.warm_items, 1,
            "the superseded item stays in the buffer: {diagnostics:?}"
        );
    }

    /// Whether the engine still holds the item in its resident heap.
    async fn state_has(engine: &SimpleContextEngine, id: ContextItemId) -> bool {
        engine
            .state
            .lock()
            .await
            .items
            .iter()
            .any(|item| item.id == id)
    }
}
