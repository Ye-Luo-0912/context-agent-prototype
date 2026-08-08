use std::cmp::Ordering;
use std::collections::HashSet;

use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextQuery, ContextRetention, ContextSelection,
    MaterializedContext, MaterializedItem, ScopeId, ScopeKind, ScoreBreakdown,
};

use crate::diagnostics;
use crate::engine::{SimpleContextConfig, State};
use crate::gc::reachability::is_excluded;
use crate::item::{approx_tokens, short_id};
use crate::policy::score_item_with_breakdown;

/// Per-snapshot cap on items pulled in by dependency expansion.
const MAX_EXPANSION_ITEMS: usize = 8;
/// Token reserve carved out of the model budget so dependency expansion
/// can follow selected items without blowing the budget.
const EXPANSION_RESERVE_TOKENS: usize = 1024;

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
    // active scope subtree (session + the current task's scopes, including
    // closed tool frames of that task) plus hot-entity matches plus legacy
    // unscoped items. The selection universe is explainable: an item is
    // scoreable when it is a member of the current task's scope lineage, is
    // pinned/durable in the session scope, or its entities are hot.
    state.indexes.ensure_consistent(&state.items);
    let active_task_id = focus.as_ref().map(|f| f.task_id);
    let mut active_scopes: HashSet<ScopeId> = HashSet::new();
    for scope in &state.scopes {
        let in_active_task = scope.task_id.is_some_and(|id| Some(id) == active_task_id);
        if scope.kind == ScopeKind::Session || in_active_task {
            active_scopes.insert(scope.id);
        }
    }

    let mut candidates: Vec<(usize, ScoreBreakdown, usize)> = Vec::new();
    for id in state
        .indexes
        .candidate_ids(&active_scopes, &state.hot_entities)
    {
        let Some(index) = state.indexes.get(id) else {
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

    candidates.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(Ordering::Equal));

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
        let mut expanded: Vec<(usize, ScoreBreakdown, usize, ContextItemId)> = Vec::new();
        for &index in &selected_indices {
            let item = &state.items[index];
            let dependencies = item.dependencies.clone();
            for dep_id in dependencies {
                if selected_ids.contains(&dep_id) {
                    continue;
                }
                let Some(dep_index) = state.indexes.get(dep_id) else {
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
                expanded.push((dep_index, breakdown, tokens, item.id));
            }
        }
        expanded.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(Ordering::Equal));
        let mut seen: Vec<usize> = Vec::new();
        let mut added = 0usize;
        for (dep_index, breakdown, tokens, depends_on) in expanded {
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
            if seen.contains(&dep_index) {
                continue;
            }
            seen.push(dep_index);
            let item = &state.items[dep_index];
            // The expansion slice is a hard bound for every item — pinned
            // dependencies included. Priority is a selection-order concern,
            // not a budget exemption.
            if tokens > expansion_budget {
                continue;
            }
            expansion_budget = expansion_budget.saturating_sub(tokens);
            selected_ids.push(item.id);
            selected_indices.push(dep_index);
            selections.push(ContextSelection {
                item_id: item.id,
                score: breakdown.total,
                approx_tokens: tokens,
                reason: format!("included as dependency of item {}", short_id(&depends_on)),
                breakdown,
            });
            added += 1;
        }
    }

    selected_indices.sort_by_key(|index| state.items[*index].created_tick);
    let turn = state.turn;

    // Access reinforcement happens on every materialization: an item that
    // reached the working set earns a fresh access stamp.
    for index in &selected_indices {
        let item = &mut state.items[*index];
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
        // The lightweight context map: externalized items are visible only
        // as references, never as content — the model sees `context://...`
        // entries and can deliberately pull them with a future context tool.
        external: state.external.clone(),
        selected: selections,
        approx_tokens: approx_tokens_total,
        diagnostics: diagnostics::compute(state),
    }
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
