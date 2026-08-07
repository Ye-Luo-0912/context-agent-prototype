use std::cmp::Ordering;

use agent_contracts::{
    ContextBuildRequest, ContextItem, ContextItemId, ContextRetention, ContextSelection,
    ContextSnapshot, ContextState, ModelMessage, ScoreBreakdown,
};

use crate::diagnostics;
use crate::engine::{SimpleContextConfig, State};
use crate::gc::reachability::is_excluded;
use crate::heap::find_index;
use crate::item::{approx_tokens, short_id};
use crate::policy::score_item_with_breakdown;

/// Per-snapshot cap on items pulled in by dependency expansion.
const MAX_EXPANSION_ITEMS: usize = 8;
/// Token reserve carved out of the model budget so dependency expansion
/// can follow selected items without blowing the budget.
const EXPANSION_RESERVE_TOKENS: usize = 1024;

/// Materialize the Context Frame for one model request: score the heap,
/// pack the best candidates into the budget, expand along explicit
/// dependency edges within a reserved slice, and render the result as
/// messages. The runtime owns the turn frame; this only covers the
/// long-term working set.
pub(crate) fn build_snapshot(
    state: &mut State,
    config: &SimpleContextConfig,
    request: &ContextBuildRequest,
) -> ContextSnapshot {
    let now_tick = state.tick;
    let focus = state.focus.clone();

    let mut candidates: Vec<(usize, ScoreBreakdown, usize)> = state
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.state != ContextState::Dropped
                && !(item.kind == agent_contracts::ContextKind::UserMessage
                    && item.content == request.current_input)
                && !is_excluded(item)
        })
        .map(|(index, item)| {
            let breakdown =
                score_item_with_breakdown(item, focus.as_ref(), &state.hot_entities, now_tick);
            let tokens = approx_tokens(&item.content);
            (index, breakdown, tokens)
        })
        .collect();

    candidates.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(Ordering::Equal));

    let fixed_tokens = approx_tokens(&request.system_prompt)
        + approx_tokens(&request.current_input)
        + focus
            .as_ref()
            .map(|f| approx_tokens(&f.goal) + approx_tokens(&f.current_query))
            .unwrap_or_default();
    // A small slice of the budget is reserved for dependency expansion so
    // traceability items can follow the working set without letting the
    // snapshot exceed the budget.
    let total_budget = request.budget_tokens.saturating_sub(fixed_tokens);
    let expansion_reserve = EXPANSION_RESERVE_TOKENS.min(total_budget);
    let mut remaining = total_budget - expansion_reserve;
    let mut selected_indices = Vec::new();
    let mut selections = Vec::new();

    for (index, breakdown, tokens) in candidates {
        let item = &state.items[index];
        if item.state == ContextState::Archived && breakdown.total < config.active_threshold {
            continue;
        }
        if tokens > remaining && item.retention != ContextRetention::Pinned {
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
                let Some(dep_index) = find_index(&state.items, dep_id) else {
                    continue;
                };
                let dep = &state.items[dep_index];
                if dep.state == ContextState::Dropped || is_excluded(dep) {
                    continue;
                }
                let breakdown =
                    score_item_with_breakdown(dep, focus.as_ref(), &state.hot_entities, now_tick);
                if dep.state == ContextState::Archived && breakdown.total < config.active_threshold
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
            if seen.contains(&dep_index) {
                continue;
            }
            seen.push(dep_index);
            let item = &state.items[dep_index];
            if item.retention != ContextRetention::Pinned && tokens > expansion_budget {
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
    let mut working_context = String::new();
    for index in &selected_indices {
        let item = &mut state.items[*index];
        item.last_access_tick = now_tick;
        item.last_access_turn = turn;
        item.access_count = item.access_count.saturating_add(1);
        working_context.push_str(&format!(
            "\n[{:?} | {:?} | {:?}]\n{}\n",
            item.kind, item.scope, item.state, item.content
        ));
    }

    let mut messages = vec![ModelMessage::system(request.system_prompt.clone())];
    if let Some(focus) = &focus {
        messages.push(ModelMessage::system(format!(
            "CURRENT FOCUS\nGoal: {}\nPhase: {}\nCurrent query: {}\nActive entities: {}",
            focus.goal,
            focus.phase,
            focus.current_query,
            if focus.active_entities.is_empty() {
                "(none)".to_string()
            } else {
                focus.active_entities.join(", ")
            }
        )));
    }
    if !working_context.is_empty() {
        messages.push(ModelMessage::system(format!(
            "SELECTED WORKING CONTEXT\nOnly use these prior items when they remain relevant to the current focus.\n{}",
            working_context
        )));
    }
    messages.push(ModelMessage::user(request.current_input.clone()));

    let approx_tokens_total = messages.iter().map(|m| approx_tokens(&m.content)).sum();
    let diagnostics = diagnostics::compute(state);

    ContextSnapshot {
        messages,
        selected: selections,
        approx_tokens: approx_tokens_total,
        diagnostics,
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
