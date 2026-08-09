use std::cmp::Ordering;
use std::collections::HashSet;

use agent_contracts::{
    AttentionState, CONTEXT_MAP_VIEW_CAP, ContextItem, ContextItemId, ContextMapView, ContextQuery,
    ContextRetention, ContextSelection, MaterializedContext, MaterializedItem, ScopeId, ScopeKind,
    ScopeState, ScoreBreakdown,
};

use crate::diagnostics;
use crate::engine::{SimpleContextConfig, State};
use crate::gc::reachability::is_excluded;
use crate::index::dependency::MAX_DEPENDENCY_EDGES;
use crate::item::{approx_tokens, short_id};
use crate::policy::score_item_with_breakdown;

/// Per-snapshot cap on items pulled in by dependency expansion.
const MAX_EXPANSION_ITEMS: usize = 8;
/// Token reserve carved out of the model budget so dependency expansion
/// can follow selected items without blowing the budget.
const EXPANSION_RESERVE_TOKENS: usize = 1024;

/// One dependency-expansion candidate for the bounded top-K heap: ordered
/// by (score descending, slot ascending) so equal scores pop
/// deterministically. The heap never holds the whole expanded set sorted —
/// it pops exactly the best candidates until the expansion window is full.
struct ExpandedCandidate {
    index: usize,
    score: f32,
    tokens: usize,
    depends_on: ContextItemId,
    breakdown: ScoreBreakdown,
}

impl Ord for ExpandedCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.index.cmp(&self.index))
    }
}

impl PartialOrd for ExpandedCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ExpandedCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ExpandedCandidate {}

/// Materialize the working set for one model request: score the heap, pack
/// the best candidates into the budget and expand along explicit dependency
/// edges within a reserved slice. The result is structured items — prompt
/// rendering belongs to the runtime's prompt assembler. The runtime owns
/// the turn frame; this only covers the long-term working set.
pub(crate) fn materialize(
    state: &mut State,
    config: &SimpleContextConfig,
    query: &ContextQuery,
) -> MaterializedContext {
    let now_tick = state.tick;
    let focus = state.focus.clone();

    // Candidate generation via the indexes instead of a full-heap scan: the
    // active scope subtree (session + the active task's open task/focus
    // scopes + open tool frames) plus hot-entity matches plus legacy
    // unscoped items. The selection universe is explainable: an item is
    // scoreable when it is a member of the current task's open scope
    // lineage, is pinned/durable in the session scope, or its entities are
    // hot. A *closed* scope is not a candidate — a closed tool frame's
    // observations re-enter through retention, affinity or dependency
    // edges, not task membership (the same rule the GC mark phase enforces
    // by never crossing a closed scope).
    state.items.ensure_consistent();
    // The external map's length guard: a direct mutation outside the
    // structured methods (restored checkpoints, tests) triggers a rebuild
    // before any indexed query reads it.
    state.external.ensure_consistent();
    // The scope tree's length guard, for the same reason.
    state.scopes.ensure_consistent();
    let active_task_id = focus.as_ref().map(|f| f.task_id);
    let mut active_scopes: HashSet<ScopeId> = HashSet::new();
    for scope in &state.scopes {
        let open = scope.state != ScopeState::Closed;
        let candidate = match scope.kind {
            // The session scope is always a candidate: durable session
            // memory and pins live there; scoring and the budget decide
            // what actually reaches the frame.
            ScopeKind::Session => true,
            // The active task's own task/focus scopes are the working-set
            // container; a suspended or closed task's scopes are not.
            ScopeKind::Task | ScopeKind::Focus => {
                open && scope.task_id.is_some_and(|id| Some(id) == active_task_id)
            }
            // A tool frame is a candidate only while it is open — it is an
            // execution frame, not a task membership claim.
            ScopeKind::Tool => open,
        };
        if candidate {
            active_scopes.insert(scope.id);
        }
    }

    let mut candidates: Vec<(usize, ScoreBreakdown, usize)> = Vec::new();
    for id in state
        .items
        .indexes()
        .candidate_ids(&active_scopes, &state.hot_entities)
    {
        let Some(index) = state.items.indexes().get(id) else {
            continue;
        };
        let item = &state.items[index];
        if !item.semantic.is_live()
            || (item.kind == agent_contracts::ContextKind::UserMessage
                && item.content == query.current_input)
            || is_excluded(item)
        {
            continue;
        }
        let breakdown =
            score_item_with_breakdown(item, focus.as_ref(), &state.hot_entities, now_tick);
        let tokens = approx_tokens(&item.content);
        candidates.push((index, breakdown, tokens));
    }

    // Deterministic candidate order: score descending, slot index as the
    // tie-break (the old unstable sort left equal-score order undefined).
    // When the caller caps the working set, quickselect trims the candidate
    // universe to that bound *before* sorting — the cap also bounds how
    // many items can ever be selected, so trimming cannot change the
    // outcome, and the sort cost drops from O(n log n) to O(n + k log k).
    let by_score = |a: &(usize, ScoreBreakdown, usize), b: &(usize, ScoreBreakdown, usize)| {
        b.1.total
            .partial_cmp(&a.1.total)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    };
    if let Some(max) = query.hints.max_selected_items
        && max < candidates.len()
    {
        candidates.select_nth_unstable_by(max, by_score);
        candidates.truncate(max);
    }
    candidates.sort_by(by_score);

    // The engine owns the focus frame and the selected items; the current
    // input rides in the runtime's turn frame and is charged there, so it is
    // not deducted a second time here.
    let fixed_tokens = focus
        .as_ref()
        .map(|f| approx_tokens(&f.goal) + approx_tokens(&f.current_query))
        .unwrap_or_default();
    // A small slice of the budget is reserved for dependency expansion so
    // traceability items can follow the working set without letting the
    // snapshot exceed the budget.
    let total_budget = query.budget_tokens.saturating_sub(fixed_tokens);
    let expansion_reserve = EXPANSION_RESERVE_TOKENS.min(total_budget);
    let mut remaining = total_budget - expansion_reserve;
    let mut selected_indices = Vec::new();
    let mut selections = Vec::new();

    // Pinned items go first — priority, not exemption: every item, pinned or
    // not, must fit the remaining budget, so the frame is a hard bound.
    for (index, breakdown, tokens) in &candidates {
        let item = &state.items[*index];
        if item.retention != ContextRetention::Pinned {
            continue;
        }
        if let Some(max) = query.hints.max_selected_items
            && selections.len() >= max
        {
            break;
        }
        if item.attention == AttentionState::Archived && breakdown.total < config.active_threshold {
            continue;
        }
        if *tokens > remaining {
            continue;
        }
        remaining -= *tokens;
        selected_indices.push(*index);
        selections.push(ContextSelection {
            item_id: item.id,
            score: breakdown.total,
            approx_tokens: *tokens,
            reason: selection_reason(item, breakdown),
            breakdown: breakdown.clone(),
        });
    }

    // Then the scored candidates fill the rest of the frame.
    for (index, breakdown, tokens) in candidates {
        let item = &state.items[index];
        if item.retention == ContextRetention::Pinned {
            continue;
        }
        if let Some(max) = query.hints.max_selected_items
            && selections.len() >= max
        {
            break;
        }
        if item.attention == AttentionState::Archived && breakdown.total < config.active_threshold {
            continue;
        }
        if tokens > remaining {
            continue;
        }

        remaining = remaining.saturating_sub(tokens);
        selected_indices.push(index);
        selections.push(ContextSelection {
            item_id: item.id,
            score: breakdown.total,
            approx_tokens: tokens,
            reason: selection_reason(item, &breakdown),
            breakdown,
        });
    }

    // Explicit dependency expansion — pull in dependencies of selected items
    // (skip Dropped and excluded items; Archived dependencies only when they
    // still clear the active threshold), best dependencies first, bounded per
    // snapshot, spending only the reserved slice.
    let mut selected_ids: Vec<ContextItemId> = selections
        .iter()
        .map(|selection| selection.item_id)
        .collect();
    if config.dependency_expansion {
        let mut expansion_budget = remaining + expansion_reserve;
        // Bounded top-K: the expansion window is at most MAX_EXPANSION_ITEMS
        // distinct items, so a max-heap pops exactly the best candidates
        // (score descending, slot as the tie-break) instead of sorting the
        // whole expanded set.
        let mut expanded: std::collections::BinaryHeap<ExpandedCandidate> =
            std::collections::BinaryHeap::with_capacity(
                selected_indices.len().saturating_mul(MAX_DEPENDENCY_EDGES),
            );
        for &index in &selected_indices {
            let item = &state.items[index];
            let dependencies = item.dependencies.clone();
            for edge in dependencies {
                let dep_id = edge.target;
                if selected_ids.contains(&dep_id) {
                    continue;
                }
                let Some(dep_index) = state.items.indexes().get(dep_id) else {
                    continue;
                };
                let dep = &state.items[dep_index];
                if dep.semantic.is_dead() || is_excluded(dep) {
                    continue;
                }
                let breakdown =
                    score_item_with_breakdown(dep, focus.as_ref(), &state.hot_entities, now_tick);
                if dep.attention == AttentionState::Archived
                    && breakdown.total < config.active_threshold
                {
                    continue;
                }
                let tokens = approx_tokens(&dep.content);
                expanded.push(ExpandedCandidate {
                    index: dep_index,
                    score: breakdown.total,
                    tokens,
                    depends_on: item.id,
                    breakdown,
                });
            }
        }
        let mut seen: Vec<usize> = Vec::new();
        let mut added = 0usize;
        while let Some(candidate) = expanded.pop() {
            if added >= MAX_EXPANSION_ITEMS {
                break;
            }
            if query
                .hints
                .max_selected_items
                .is_some_and(|max| selections.len() >= max)
            {
                break;
            }
            let dep_index = candidate.index;
            if seen.contains(&dep_index) {
                continue;
            }
            seen.push(dep_index);
            let item = &state.items[dep_index];
            // The expansion slice is a hard bound for every item — pinned
            // dependencies included. Priority is a selection-order concern,
            // not a budget exemption.
            if candidate.tokens > expansion_budget {
                continue;
            }
            expansion_budget = expansion_budget.saturating_sub(candidate.tokens);
            selected_ids.push(item.id);
            selected_indices.push(dep_index);
            selections.push(ContextSelection {
                item_id: item.id,
                score: candidate.score,
                approx_tokens: candidate.tokens,
                reason: format!(
                    "included as dependency of item {}",
                    short_id(&candidate.depends_on)
                ),
                breakdown: candidate.breakdown,
            });
            added += 1;
        }
    }

    selected_indices.sort_by_key(|index| state.items[*index].created_tick);
    let turn = state.turn;

    // Access reinforcement happens on every materialization: an item that
    // reached the working set earns a fresh access stamp. Access stamps are
    // not indexed, so the raw mutable slice is safe here.
    for index in &selected_indices {
        let item = &mut state.items.items_mut()[*index];
        item.last_access_tick = now_tick;
        item.last_access_turn = turn;
        item.access_count = item.access_count.saturating_add(1);
    }

    let items: Vec<MaterializedItem> = selected_indices
        .iter()
        .map(|index| {
            let item = &state.items[*index];
            MaterializedItem {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                attention: item.attention,
                semantic: item.semantic,
                retention: item.retention,
                content: item.content.clone(),
                source: item.source.clone(),
            }
        })
        .collect();

    // The engine-side token share: focus frame + selected items. The system
    // prompt, turn frame and tool schemas are the runtime's share and are
    // charged by the model budget before this snapshot is requested.
    let approx_tokens_total = focus
        .as_ref()
        .map(|f| approx_tokens(&f.goal) + approx_tokens(&f.current_query))
        .unwrap_or_default()
        + items
            .iter()
            .map(|item| approx_tokens(&item.content))
            .sum::<usize>();

    MaterializedContext {
        focus,
        items,
        // The lightweight context map: a *bounded* slice of the external
        // entries, never the whole map. The full `state.external` stays in
        // the engine; cloning it per materialize would grow linearly with
        // the run (10K/100K refs) and the prompt does not use it anyway.
        // The view prefers hot-entity matches, open loops, then recency —
        // the entries the model is most likely to deliberately pull.
        external: external_view(state, &state.hot_entities),
        selected: selections,
        approx_tokens: approx_tokens_total,
        diagnostics: diagnostics::compute(state),
    }
}

/// Cap on external refs surfaced in one materialized context. The bound is
/// enforced by the [`ContextMapView`] type; this is the selection side that
/// keeps the producer within it.
const MAX_EXTERNAL_REFS: usize = CONTEXT_MAP_VIEW_CAP;

/// Build a small, deterministic slice of the external map: hot-entity
/// matches and open loops first, then most-recently-accessed entries, up
/// to `CONTEXT_MAP_VIEW_CAP`. Quickselect keeps this O(n) without cloning
/// the whole map; the bounded `ContextMapView` enforces the cap at the
/// type level.
fn external_view(state: &State, hot_entities: &[String]) -> ContextMapView {
    let mut ranked: Vec<&agent_contracts::ExternalizedContext> = state
        .external
        .iter()
        .filter(|entry| crate::store::externally_retrievable(entry))
        .collect();
    let k = ranked.len().min(MAX_EXTERNAL_REFS);
    if k == 0 {
        return ContextMapView::default();
    }
    ranked.select_nth_unstable_by(k - 1, |a, b| {
        external_view_key(b, hot_entities).cmp(&external_view_key(a, hot_entities))
    });
    ranked.truncate(k);
    // Stable final order: rank, then item id as a deterministic tie-break.
    ranked.sort_by(|a, b| {
        external_view_key(b, hot_entities)
            .cmp(&external_view_key(a, hot_entities))
            .then_with(|| a.item_id.0.cmp(&b.item_id.0))
    });
    ContextMapView::new(ranked.into_iter().cloned().collect())
}

/// (hot-entity match, open loop, recency) — higher sorts first. Open loops
/// are tagged items whose decision is still pending; surfacing them keeps
/// the model's unfinished business visible even after externalization.
fn external_view_key(
    entry: &agent_contracts::ExternalizedContext,
    hot_entities: &[String],
) -> (u8, u8, u64) {
    let hot = u8::from(
        hot_entities
            .iter()
            .any(|hot| entry.entities.iter().any(|e| e == hot)),
    );
    let open_loop = u8::from(
        entry
            .tags
            .iter()
            .any(|tag| tag.is_core(agent_contracts::CoreLabel::OpenLoop)),
    );
    (hot, open_loop, entry.last_access_tick)
}

fn selection_reason(item: &ContextItem, breakdown: &ScoreBreakdown) -> String {
    if item.retention == ContextRetention::Pinned {
        return "explicitly pinned".to_string();
    }
    format!(
        "working-set score {:.2}; kind={:?}; scope={:?}; importance={:.2} focus={:.2} recency={:.2} access={:.2} scope_bonus={:.2} retention_bonus={:.2} affinity={:.2}",
        breakdown.total,
        item.kind,
        item.scope,
        breakdown.importance,
        breakdown.focus_match,
        breakdown.recency,
        breakdown.access,
        breakdown.scope_bonus,
        breakdown.retention_bonus,
        breakdown.entity_affinity,
    )
}
