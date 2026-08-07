use agent_contracts::{
    ContextEviction, ContextGcReport, ContextItem, ContextItemId, ContextReactivation,
    ContextResidency, ContextRetention, ContextScope, ContextState, FocusState,
};

use crate::diagnostics;
use crate::engine::{SimpleContextConfig, State};
use crate::index::entity::{entities_match, extract_entities};
use crate::policy::score_item_with_breakdown;

/// Cap on transitive dependency expansion during the mark phase: the root
/// set stays a bounded, cheap reachability view.
const MAX_MARKED_DEPENDENCIES: usize = 8;

/// One full GC pass: mark roots, sweep unmarked items into the bounded
/// reversible eviction buffer, reactivate items that became relevant again,
/// purge only when the buffer overflows.
///
/// The three GC dimensions are separated:
/// - `residency` (Resident / Evicted) says where the item physically lives;
/// - `gc_generation` counts how many passes an item survived without being
///   a root (the generational heuristic);
/// - the semantic `ContextState` (Active/Cooling/Archived/Dropped) is owned
///   by the per-event residency machine, not by GC.
///
/// Every eviction and reactivation carries an explicit reason, so the report
/// can explain *why* an item is Resident, Evicted or Reactivated.
pub(crate) fn run_full_gc(
    state: &mut State,
    config: &SimpleContextConfig,
    now_tick: u64,
) -> ContextGcReport {
    let mut report = ContextGcReport {
        resident: state.items.len(),
        ..ContextGcReport::default()
    };

    if !config.gc_enabled || state.items.is_empty() && state.eviction_buffer.is_empty() {
        report.diagnostics = diagnostics::compute(state);
        return report;
    }

    // ----- Mark phase: the root set --------------------------------
    // Roots are the current attention: pins, members of the active
    // task/focus scopes, durable session memory, items whose entities are
    // hot, plus a bounded slice of their dependencies.
    let focus = state.focus.clone();
    let hot_entities = state.hot_entities.clone();
    let marked = mark_roots(state, config, focus.as_ref(), &hot_entities);

    // ----- Sweep phase: unmarked items ------------------------------
    // Roots protect *live* items. A semantically Dropped item is dead no
    // matter how reachable it looks — the residency machine decided it is
    // gone, so GC physically removes it (into the reversible buffer) instead
    // of letting it linger in the heap and checkpoints forever.
    let mut survivors: Vec<ContextItem> = Vec::with_capacity(state.items.len());
    for item in state.items.drain(..) {
        let alive_root = item.state != ContextState::Dropped && marked.contains(&item.id);
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
            report.evictions.push(ContextEviction {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                generation,
                evicted_at_tick: now_tick,
                reason,
            });
            report.evicted += 1;
            state.gc_evicted_total += 1;
            let mut evicted = item;
            evicted.residency = ContextResidency::Evicted;
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
    state.items = survivors;

    // ----- Reactivate phase: evicted items that became relevant -----
    reactivate(state, config, now_tick, &mut report);

    // ----- Purge: the buffer is bounded -----------------------------
    while state.eviction_buffer.len() > config.gc_buffer_capacity {
        state.eviction_buffer.remove(0);
        report.purged += 1;
    }

    report.marked_roots = marked.len();
    report.resident = state.items.len();
    report.diagnostics = diagnostics::compute(state);
    report
}

/// Mark the root set: pins, active-scope members, durable session memory,
/// hot-entity matches, and a bounded transitive slice of their dependencies.
fn mark_roots(
    state: &State,
    config: &SimpleContextConfig,
    focus: Option<&FocusState>,
    hot_entities: &[String],
) -> Vec<ContextItemId> {
    let active_task = focus.map(|f| f.task_id);
    let mut marked: Vec<ContextItemId> = Vec::new();

    for item in &state.items {
        let is_pin =
            item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned;
        let in_active_scope = active_task.is_some_and(|task| item.task_id == Some(task));
        let durable_session_memory =
            item.retention == ContextRetention::Durable && item.scope == ContextScope::Session;
        let hot = !hot_entities.is_empty() && {
            let item_entities = extract_entities(&item.content);
            entities_match(&item_entities, hot_entities)
        };
        if is_pin || in_active_scope || durable_session_memory || hot {
            marked.push(item.id);
        }
    }

    // Reachability through dependency edges, bounded: a working item pulls
    // in the items it depends on, so dependencies of roots are protected too.
    if config.dependency_expansion && !marked.is_empty() {
        let mut added = 0usize;
        let mut changed = true;
        while changed && added < MAX_MARKED_DEPENDENCIES {
            changed = false;
            for item in &state.items {
                if marked.contains(&item.id) {
                    continue;
                }
                let reachable = item.dependencies.iter().any(|dep| marked.contains(dep));
                if reachable {
                    marked.push(item.id);
                    added += 1;
                    changed = true;
                }
            }
        }
    }
    marked
}

/// Whether an unmarked item is an eviction candidate this pass. GC only
/// evicts what the semantic machine already demoted (or dropped), so GC
/// never fights the policy: active items stay until residency cools them.
fn eviction_candidate(
    item: &ContextItem,
    config: &SimpleContextConfig,
    now_tick: u64,
    generation: u32,
) -> bool {
    match item.state {
        // The semantic machine already dropped it: leave the heap now
        // instead of lingering in checkpoints forever.
        ContextState::Dropped => true,
        // Never evict what the policy keeps active.
        ContextState::Active => false,
        ContextState::Cooling | ContextState::Archived => {
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
    match item.state {
        ContextState::Dropped => {
            format!("semantically dropped; evicted to reversible buffer (generation {generation})")
        }
        ContextState::Cooling | ContextState::Archived => {
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
        ContextState::Active => {
            format!("unreachable from roots despite active state (generation {generation})")
        }
    }
}

/// Bring evicted items back when they are relevant again: a pin, hot
/// entities in the working set, or a score that clears the active threshold.
/// Newest evictions first, bounded per pass. Items evicted by the current
/// pass are skipped so eviction stays effective.
fn reactivate(
    state: &mut State,
    config: &SimpleContextConfig,
    now_tick: u64,
    report: &mut ContextGcReport,
) {
    let focus = state.focus.clone();
    let hot_entities = state.hot_entities.clone();
    let mut reactivated: Vec<ContextItem> = Vec::new();
    let mut remaining = config.gc_reactivate_per_pass;
    let mut index = state.eviction_buffer.len();

    while index > 0 && remaining > 0 {
        index -= 1;
        let item = &state.eviction_buffer[index];
        if item.evicted_at_tick == Some(now_tick) {
            continue;
        }
        let Some(reason) =
            reactivation_reason(item, config, focus.as_ref(), &hot_entities, now_tick)
        else {
            continue;
        };
        let mut item = state.eviction_buffer.remove(index);
        item.state = ContextState::Active;
        item.relevance = item.relevance.max(0.5);
        item.residency = ContextResidency::Resident;
        item.gc_generation = 0;
        item.evicted_at_tick = None;
        item.last_access_tick = now_tick;
        report.reactivations.push(ContextReactivation {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            reactivated_at_tick: now_tick,
            reason,
        });
        report.reactivated += 1;
        state.gc_reactivated_total += 1;
        reactivated.push(item);
        remaining -= 1;
    }

    state.items.extend(reactivated);
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
) -> Option<String> {
    if item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned {
        return Some("explicitly pinned again".to_string());
    }
    if !hot_entities.is_empty() {
        let item_entities = extract_entities(&item.content);
        if entities_match(&item_entities, hot_entities) {
            return Some("entities are hot again in the working set".to_string());
        }
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
    use agent_contracts::{
        ContextEngine, ContextIngress, ContextKind, ContextMaintenanceTrigger, TaskId, ToolOutput,
    };
    use serde_json::json;

    #[tokio::test]
    async fn gc_evicts_dropped_items_from_the_heap_with_a_reason() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();
        // A successful observation is ephemeral and drops after the turn.
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
        assert!(before.dropped_items >= 1, "ephemeral observation drops");

        let report = engine.gc().await.unwrap();
        assert!(report.evicted >= 1, "gc must evict the dropped item");
        assert!(
            report
                .evictions
                .iter()
                .any(|e| e.reason.contains("semantically dropped")),
            "eviction must be explainable, got: {:?}",
            report.evictions
        );

        let after = engine.diagnostics().await.unwrap();
        assert_eq!(
            after.evicted_items, before.dropped_items,
            "dropped items leave the heap for the reversible buffer"
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

        // Archive a cold item from another task: unmarked, past the
        // generation cap, entities not hot.
        {
            let mut state = engine.state.lock().await;
            for item in &mut state.items {
                if item.kind == ContextKind::UserMessage {
                    item.task_id = Some(TaskId::new());
                    item.content = "fix CacheStore.rs".into();
                    item.state = ContextState::Archived;
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
    async fn gc_reactivates_evicted_items_whose_entities_become_hot_again() {
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
    async fn gc_buffer_is_bounded_and_purges_oldest() {
        let config = SimpleContextConfig {
            gc_buffer_capacity: 2,
            ..SimpleContextConfig::default()
        };
        let engine = SimpleContextEngine::new(config);

        // Three turns on distinct files: each successful observation drops,
        // is evicted, and stays evicted (its entities are not hot again).
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
        assert_eq!(last.purged, 1, "overflow purges the oldest eviction");
        let diagnostics = engine.diagnostics().await.unwrap();
        assert_eq!(diagnostics.evicted_items, 2, "buffer stays bounded");
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
                    // Cold and unmarked: another task, entities outside the
                    // hot set (the error itself contributed CacheStore.rs to
                    // the hot set at ingest, so the content must change).
                    item.task_id = Some(TaskId::new());
                    item.content = "error in TempStore.rs:7".into();
                    item.state = ContextState::Cooling;
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
}
