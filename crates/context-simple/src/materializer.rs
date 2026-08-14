use std::cmp::Ordering;
use std::collections::HashSet;

use agent_contracts::{
    AnchorRootStrength, AttentionState, CONTEXT_MAP_VIEW_CAP, ContextItem, ContextItemId,
    ContextMapView, ContextQuery, ContextRetention, ContextSelection, MaterializedContext,
    MaterializedItem, ScopeId, ScopeKind, ScopeState, ScoreBreakdown,
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
    // Event sequence orders the preview (created_tick comparisons); recency
    // scoring reads the user-turn clock, so a preview never ages items.
    let turn = state.turn;
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

    let mut candidate_ids = state
        .items
        .indexes()
        .candidate_ids(&active_scopes, &state.hot_entities);
    // 当前任务最近文件的最新正文：工具帧关闭后不再属于 active scopes，
    // 换文件时热实体也对不上旧路径，必须单独纳入候选，否则 Resident
    // 根进不了模型帧。
    let latest_file_bodies = state.latest_file_body_ids();
    if !latest_file_bodies.is_empty() {
        let mut seen: HashSet<ContextItemId> = candidate_ids.iter().copied().collect();
        for id in &latest_file_bodies {
            if seen.insert(*id) {
                candidate_ids.push(*id);
            }
        }
    }
    // Substring residual: the candidate index matches entities *exactly*,
    // but the scorer matches substrings (`hot.contains(entity) ||
    // entity.contains(hot)`), so an item whose signature contains the hot
    // entity as a substring (e.g. `src/auth/AuthService.rs` vs
    // `AuthService.rs`) is not in the exact bucket and would never be
    // scored. The heap is bounded by GC, so one residual pass keeps
    // candidate generation and scoring on the same matching universe —
    // the exact index answers the common case, this pass covers the
    // overlap the index cannot express.
    if !state.hot_entities.is_empty() {
        let seen: HashSet<ContextItemId> = candidate_ids.iter().copied().collect();
        for item in state.items.iter() {
            if seen.contains(&item.id) {
                continue;
            }
            let hot = item.entities.iter().any(|entity| {
                state
                    .hot_entities
                    .iter()
                    .any(|hot| hot.contains(entity) || entity.contains(hot))
            });
            if hot {
                candidate_ids.push(item.id);
            }
        }
    }
    // TaskAnchor 投影的 PromptRequired 声明：任务权威要求这些条目进
    // 模型帧，即使它们不在 active scopes 或热实体集里。与 GC 根共用
    // 同一匹配解析（anchor_claim_matches_item），terminal 语义仍由
    // 下面的 live 过滤挡住，声明不复活死条目。
    let prompt_required_ids: HashSet<ContextItemId> = query
        .hints
        .anchor_roots
        .iter()
        .filter(|claim| claim.strength == AnchorRootStrength::PromptRequired)
        .flat_map(|claim| {
            state
                .items
                .iter()
                .filter(|item| crate::engine::anchor_claim_matches_item(claim, item))
                .map(|item| item.id)
        })
        .collect();
    if !prompt_required_ids.is_empty() {
        let mut seen: HashSet<ContextItemId> = candidate_ids.iter().copied().collect();
        for id in &prompt_required_ids {
            if seen.insert(*id) {
                candidate_ids.push(*id);
            }
        }
    }
    let mut candidates: Vec<(usize, ScoreBreakdown, usize)> = Vec::new();
    for id in candidate_ids {
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
        let breakdown = score_item_with_breakdown(item, focus.as_ref(), &state.hot_entities, turn);
        let tokens = approx_tokens(&item.content);
        candidates.push((index, breakdown, tokens));
    }

    // Deterministic candidate order: score descending, slot index as the
    // tie-break (the old unstable sort left equal-score order undefined).
    // The heap is bounded by GC, so sorting the whole candidate universe is
    // O(heap log heap), not O(total history). The candidate list is *not*
    // pre-trimmed to `max_selected_items`: fit packing runs after scoring,
    // and an oversized top item that cannot fit must not hide a lower-ranked
    // item that does fit — packing's own `selections.len() >= max` checks
    // enforce the cap, so trimming here would only lose candidates.
    let by_score = |a: &(usize, ScoreBreakdown, usize), b: &(usize, ScoreBreakdown, usize)| {
        b.1.total
            .partial_cmp(&a.1.total)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    };
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
    // snapshot exceed the budget. The reserve is only carved out when
    // expansion can actually run — with expansion disabled the whole budget
    // belongs to the working set, and reserving a slice that is never spent
    // would shrink the frame for no reason.
    let total_budget = query.budget_tokens.saturating_sub(fixed_tokens);
    let expansion_reserve = if config.dependency_expansion {
        EXPANSION_RESERVE_TOKENS.min(total_budget)
    } else {
        0
    };
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
        if archived_below_cutoff(item, breakdown, config, &latest_file_bodies) {
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
            reason: selection_reason(item, breakdown, latest_file_bodies.contains(&item.id)),
            breakdown: breakdown.clone(),
        });
    }

    // Anchor prompt-required 声明：优先级在 pinned 之后、打分候选之前。
    // 预算仍是硬约束（放不下的条目跳过，帧不豁免），但不再被 Archived
    // 阈值挡在门外——任务权威要求它进帧，reason 可解释。
    for (index, breakdown, tokens) in &candidates {
        let item = &state.items[*index];
        if !prompt_required_ids.contains(&item.id) || selected_indices.contains(index) {
            continue;
        }
        if let Some(max) = query.hints.max_selected_items
            && selections.len() >= max
        {
            break;
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
            reason: "anchor root requires it in the prompt".to_string(),
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
        if archived_below_cutoff(item, &breakdown, config, &latest_file_bodies) {
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
            reason: selection_reason(item, &breakdown, latest_file_bodies.contains(&item.id)),
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
                    score_item_with_breakdown(dep, focus.as_ref(), &state.hot_entities, turn);
                if archived_below_cutoff(dep, &breakdown, config, &latest_file_bodies) {
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

    // The engine-side token share: focus frame + selected items + the
    // external refs surfaced in this snapshot. The system prompt, turn
    // frame and tool schemas are the runtime's share and are charged by the
    // model budget before this snapshot is requested. Refs are model-visible
    // (uri + summary), so they are charged here with the same measure the
    // selection walked — not free.
    let external = external_view(state, &state.hot_entities);
    let approx_tokens_total = focus
        .as_ref()
        .map(|f| approx_tokens(&f.goal) + approx_tokens(&f.current_query))
        .unwrap_or_default()
        + items
            .iter()
            .map(|item| approx_tokens(&item.content))
            .sum::<usize>()
        + external.iter().map(external_ref_tokens).sum::<usize>();

    MaterializedContext {
        materialization_id: 0,
        focus,
        items,
        // The lightweight context map: a *bounded* slice of the external
        // entries, never the whole map. The full `state.external` stays in
        // the engine; cloning it per materialize would grow linearly with
        // the run (10K/100K refs) and the prompt does not use it anyway.
        // The view prefers hot-entity matches, open loops, then recency —
        // the entries the model is most likely to deliberately pull.
        external,
        selected: selections,
        approx_tokens: approx_tokens_total,
        diagnostics: diagnostics::compute(state),
    }
}

/// Cap on external refs surfaced in one materialized context. The bound is
/// enforced by the [`ContextMapView`] type; this is the selection side that
/// keeps the producer within it.
const MAX_EXTERNAL_REFS: usize = CONTEXT_MAP_VIEW_CAP;

/// Token bound on the external refs surfaced in one materialized context.
/// Refs are model-visible (uri + summary), so the item-count cap alone does
/// not bound the prompt: 32 long summaries could still cost more than the
/// frame allows. The ranked selection is walked in order and stops when the
/// summaries would exceed this bound.
const EXTERNAL_REF_TOKENS: usize = 512;

/// Build a small, deterministic slice of the external map without walking
/// the whole map: hot-entity matches come from the entity index (O(bucket)
/// per hot entity), and the rest is the most-recently-externalized tail —
/// the map stores entries in externalize order, so the tail is a bounded
/// O(1) recency approximation (a `fetch`/`ack` stamps access but does not
/// reorder, so exact global recency would need a second index; the tail
/// keeps the hot path independent of total history). The union is ranked
/// (hot-entity match, open loop, recency), capped to `CONTEXT_MAP_VIEW_CAP`
/// refs and `EXTERNAL_REF_TOKENS` of summary tokens; the bounded
/// `ContextMapView` enforces the cap at the type level.
fn external_view(state: &State, hot_entities: &[String]) -> ContextMapView {
    let mut seen: HashSet<ContextItemId> = HashSet::new();
    let mut ranked: Vec<&agent_contracts::ExternalizedContext> = Vec::new();
    for hot in hot_entities {
        for id in state.external.ids_for_entity(hot) {
            if seen.insert(*id)
                && let Some(entry) = state.external.get(*id)
                && crate::store::externally_retrievable(entry)
            {
                ranked.push(entry);
            }
        }
    }
    let total = state.external.len();
    let tail_start = total.saturating_sub(MAX_EXTERNAL_REFS);
    for entry in &state.external[tail_start..] {
        if seen.insert(entry.item_id) && crate::store::externally_retrievable(entry) {
            ranked.push(entry);
        }
    }
    if ranked.is_empty() {
        return ContextMapView::default();
    }
    // Small bounded set: sort directly (no quickselect needed), then apply
    // the token cap in the same ranked order the model sees. A long summary
    // costs prompt tokens, so the first-ranked refs win and the walk stops
    // once the bound is exhausted.
    ranked.sort_by(|a, b| {
        external_view_key(b, hot_entities)
            .cmp(&external_view_key(a, hot_entities))
            .then_with(|| a.item_id.0.cmp(&b.item_id.0))
    });
    let mut ref_tokens = 0usize;
    let mut capped = Vec::with_capacity(ranked.len());
    for entry in ranked {
        let tokens = external_ref_tokens(entry);
        if ref_tokens.saturating_add(tokens) > EXTERNAL_REF_TOKENS {
            break;
        }
        ref_tokens += tokens;
        capped.push(entry.clone());
    }
    ContextMapView::new(capped)
}

/// The model-visible token cost of one external ref: its uri plus its
/// bounded summary. The materializer charges the same amount against the
/// snapshot's `approx_tokens`, so the refs are no longer free.
fn external_ref_tokens(entry: &agent_contracts::ExternalizedContext) -> usize {
    approx_tokens(&entry.context_ref.uri) + approx_tokens(&entry.context_ref.summary)
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

fn archived_below_cutoff(
    item: &ContextItem,
    breakdown: &ScoreBreakdown,
    config: &SimpleContextConfig,
    latest_file_bodies: &HashSet<ContextItemId>,
) -> bool {
    item.attention == AttentionState::Archived
        && breakdown.total < config.active_threshold
        && !latest_file_bodies.contains(&item.id)
}

fn selection_reason(
    item: &ContextItem,
    breakdown: &ScoreBreakdown,
    latest_file_body: bool,
) -> String {
    if item.retention == ContextRetention::Pinned {
        return "explicitly pinned".to_string();
    }
    if latest_file_body {
        return format!(
            "latest body of a recent file in the active task; working-set score {:.2}; kind={:?}; scope={:?}; importance={:.2} focus={:.2} recency={:.2} access={:.2} scope_bonus={:.2} retention_bonus={:.2} affinity={:.2}",
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
        );
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
