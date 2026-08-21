use std::cmp::Ordering;
use std::collections::HashSet;

use std::path::Path;

use agent_contracts::{
    AttentionState, CONTEXT_MAP_VIEW_CAP, ContextItem, ContextItemId, ContextMapView, ContextQuery,
    ContextRetention, ContextSelection, MAX_FOREGROUND_RESOURCES, MAX_FOREGROUND_TOKENS,
    MaterializedContext, MaterializedItem, ScopeId, ScopeKind, ScopeState, ScoreBreakdown,
    checked_files_cover_path, normalize_resource_path,
};

use crate::diagnostics;
use crate::engine::{SimpleContextConfig, State};
use crate::gc::reachability::is_excluded;
use crate::index::dependency::MAX_DEPENDENCY_EDGES;
use crate::index::entity::{
    entities_match_exact, is_file_body_entry, is_file_body_observation, observation_file_path,
    observation_file_path_entry,
};
use crate::item::{approx_tokens, short_id};
use crate::policy::score_item_with_breakdown;
use crate::store;

/// Per-snapshot cap on items pulled in by dependency expansion.
const MAX_EXPANSION_ITEMS: usize = 8;
/// Token reserve carved from the primary budget only when some candidate
/// actually has a Continuation (prompt-body) edge. Frames with no such
/// edge keep the full working-set budget.
const EXPANSION_RESERVE_TOKENS: usize = 1024;
/// Assembler frame around a raw-evidence identity header (`[ToolObservation |
/// ... | path=...]`). Charged when TASK PROGRESS already names the path so
/// packing does not reserve the omitted body (file text or identity-log stdout).
const FILE_BODY_DESCRIPTOR_FRAME_TOKENS: usize = 48;

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
/// the best candidates into the budget and expand along Continuation
/// edges using leftover budget. The result is structured items — prompt
/// rendering belongs to the runtime's prompt assembler. The runtime owns
/// the turn frame and CURRENT FOCUS; this only covers historical working
/// context.
pub(crate) fn materialize(
    state: &mut State,
    config: &SimpleContextConfig,
    query: &ContextQuery,
) -> MaterializedContext {
    // Event sequence orders the preview (created_tick comparisons); recency
    // scoring reads the user-turn clock, so a preview never ages items.
    let turn = state.turn;
    let engine_focus = state.focus.clone();

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
    let active_task_id = engine_focus.as_ref().map(|f| f.task_id);
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
        .filter(|claim| claim.strength.requires_prompt())
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
                && turn > 0
                && item.created_turn == turn)
            || is_excluded(item)
        {
            continue;
        }
        let breakdown =
            score_item_with_breakdown(item, engine_focus.as_ref(), &state.hot_entities, turn);
        let tokens = packed_item_tokens(item, &query.hints.checked_files);
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

    // Historical working set only: CURRENT FOCUS / TaskAnchor are runtime-
    // owned and charged at assemble time. The current user message is skipped
    // by turn stamp above, not by body equality.
    let total_budget = query.budget_tokens;
    let expansion_reserve = if config.dependency_expansion
        && candidates.iter().any(|(index, _, _)| {
            state.items[*index]
                .dependencies
                .iter()
                .any(|edge| edge.kind.requires_prompt_body())
        }) {
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
        selections.push(selection_record(
            state,
            item,
            breakdown.total,
            *tokens,
            selection_reason(
                item,
                breakdown,
                latest_file_bodies.contains(&item.id),
                &query.hints.checked_files,
            ),
            breakdown.clone(),
        ));
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
        selections.push(selection_record(
            state,
            item,
            breakdown.total,
            *tokens,
            "anchor root requires it in the prompt".to_string(),
            breakdown.clone(),
        ));
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
        selections.push(selection_record(
            state,
            item,
            breakdown.total,
            tokens,
            selection_reason(
                item,
                &breakdown,
                latest_file_bodies.contains(&item.id),
                &query.hints.checked_files,
            ),
            breakdown,
        ));
    }

    // Explicit dependency expansion — pull in Continuation targets of
    // selected items (skip Dropped and excluded items; Archived only when
    // they still clear the active threshold). Affinity and provenance
    // edges do not copy bodies into the prompt.
    let mut selected_ids: Vec<ContextItemId> = selections
        .iter()
        .map(|selection| selection.item_id)
        .collect();
    if config.dependency_expansion
        && selected_indices.iter().any(|&index| {
            state.items[index]
                .dependencies
                .iter()
                .any(|edge| edge.kind.requires_prompt_body())
        })
    {
        // Leftover primary budget plus the Continuation reserve carved above.
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
                // Affinity and provenance are not prompt citations. A
                // SharesEntities overlap or a DerivedFrom card must not
                // pull the target's body back into the working set.
                if !edge.kind.requires_prompt_body() {
                    continue;
                }
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
                let breakdown = score_item_with_breakdown(
                    dep,
                    engine_focus.as_ref(),
                    &state.hot_entities,
                    turn,
                );
                if archived_below_cutoff(dep, &breakdown, config, &latest_file_bodies) {
                    continue;
                }
                let tokens = packed_item_tokens(dep, &query.hints.checked_files);
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
            selections.push(selection_record(
                state,
                item,
                candidate.score,
                candidate.tokens,
                format!(
                    "included as dependency of item {}",
                    short_id(&candidate.depends_on)
                ),
                candidate.breakdown,
            ));
            added += 1;
        }
    }

    selected_indices.sort_by_key(|index| state.items[*index].created_tick);
    state.selected_body_paths.clear();
    state.selected_descriptor_paths.clear();
    state.external_descriptor_paths.clear();
    for &index in &selected_indices {
        let item = &state.items[index];
        let Some(path) = crate::index::entity::observation_file_path(item) else {
            continue;
        };
        let path = normalize_resource_path(path);
        if path.is_empty() {
            continue;
        }
        if prices_as_file_body_descriptor(item, &query.hints.checked_files) {
            state.selected_descriptor_paths.insert(path);
        } else {
            state.selected_body_paths.insert(path);
        }
    }
    let items: Vec<MaterializedItem> = selected_indices
        .iter()
        .map(|index| {
            let item = &state.items[*index];
            let content = if prices_as_file_body_descriptor(item, &query.hints.checked_files) {
                file_body_descriptor_content(item)
            } else {
                item.content.clone()
            };
            MaterializedItem {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                attention: item.attention,
                semantic: item.semantic,
                retention: item.retention,
                content,
                source: item.source.clone(),
                file_path: item.file_path.clone(),
                file_revision: item.file_revision.clone(),
            }
        })
        .collect();

    // Historical working-set tokens only. Focus/TaskAnchor rendering is
    // the runtime assembler's share and is charged after this snapshot.
    let checked_files = merged_checked_files(state, query);
    let external = external_view(state, &state.hot_entities, &checked_files);
    for entry in external.iter() {
        if let Some(path) = entry.file_path.as_deref() {
            let path = normalize_resource_path(path);
            if path.is_empty() {
                continue;
            }
            if state.selected_body_paths.contains(&path)
                || state.selected_descriptor_paths.contains(&path)
            {
                continue;
            }
            state.external_descriptor_paths.insert(path);
        }
    }
    let approx_tokens_total = items
        .iter()
        .map(|item| approx_tokens(&item.content))
        .sum::<usize>()
        + external.iter().map(external_ref_tokens).sum::<usize>();

    for sel in &selections {
        crate::reactivation::mark_selected(state, sel.item_id, sel.approx_tokens);
    }

    MaterializedContext {
        materialization_id: 0,
        focus: None,
        task: None,
        items,
        // The lightweight context map: a *bounded* slice of the external
        // entries, never the whole map. The full `state.external` stays in
        // the engine; cloning it per materialize would grow linearly with
        // the run (10K/100K refs) and the prompt does not use it anyway.
        // The view prefers hot-entity matches, open loops, then recency —
        // plus Warm items that are hot or already Checked, as identity
        // refs so skipped file bodies stay Fetch-able. The full
        // `state.external` stays in the engine.
        external,
        selected: selections,
        approx_tokens: approx_tokens_total,
        diagnostics: diagnostics::compute(state),
        foreground: Vec::new(),
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

/// Build a small, deterministic slice of reachable descriptors without
/// walking the whole store: hot-entity matches come from the store entity
/// index (O(bucket) per hot entity), Checked paths use the same index
/// (O(bucket) per unique path, cap 32), Warm-buffer items that are hot or
/// already Checked are projected as catalog refs (so skipped file bodies
/// stay Fetch/Admit-able), and the rest is the most-recently-externalized
/// tail. The map stores entries in externalize order, so the tail is a
/// bounded O(1) recency approximation. Raw ToolObservation / FileObservation
/// summaries are rewritten to path@revision identity so the prompt does not
/// dump stdout or file text under "refs only".
fn external_view(
    state: &State,
    hot_entities: &[String],
    checked_files: &[String],
) -> ContextMapView {
    let mut seen: HashSet<ContextItemId> = HashSet::new();
    let mut ranked: Vec<agent_contracts::ExternalizedContext> = Vec::new();
    for hot in hot_entities {
        push_store_entity_hits(state, hot, &mut seen, &mut ranked);
    }
    let mut seen_checked_paths = HashSet::new();
    for row in checked_files {
        let Some(path) = checked_row_lookup_path(row) else {
            continue;
        };
        if !seen_checked_paths.insert(path.clone()) {
            continue;
        }
        push_store_entity_hits(state, &path, &mut seen, &mut ranked);
    }
    for item in &state.eviction_buffer {
        if !item.semantic.is_live() || seen.contains(&item.id) {
            continue;
        }
        let hot = !hot_entities.is_empty() && entities_match_exact(&item.entities, hot_entities);
        let checked = observation_file_path(item)
            .is_some_and(|path| checked_files_cover_path(checked_files, path));
        if !(hot || checked) {
            continue;
        }
        let Some(entry) = crate::store::project_search_hit(state, item.id) else {
            continue;
        };
        seen.insert(item.id);
        ranked.push(crate::store::prompt_evidence_descriptor(entry));
    }
    let total = state.external.len();
    let tail_start = total.saturating_sub(MAX_EXTERNAL_REFS);
    for entry in &state.external[tail_start..] {
        if seen.insert(entry.item_id) && crate::store::externally_retrievable(entry) {
            ranked.push(crate::store::prompt_evidence_descriptor(entry.clone()));
        }
    }
    if ranked.is_empty() {
        return ContextMapView::default();
    }
    ranked.sort_by(|a, b| {
        external_view_key(b, hot_entities, checked_files)
            .cmp(&external_view_key(a, hot_entities, checked_files))
            .then_with(|| a.item_id.0.cmp(&b.item_id.0))
    });
    let mut ref_tokens = 0usize;
    let mut capped = Vec::with_capacity(ranked.len());
    for entry in ranked {
        if capped.len() >= MAX_EXTERNAL_REFS {
            break;
        }
        let tokens = external_ref_tokens(&entry);
        if ref_tokens.saturating_add(tokens) > EXTERNAL_REF_TOKENS {
            break;
        }
        ref_tokens += tokens;
        capped.push(entry);
    }
    ContextMapView::new(capped)
}

/// The model-visible token cost of one external ref: its uri plus its
/// bounded summary. The materializer charges the same amount against the
/// snapshot's `approx_tokens`, so the refs are no longer free.
fn external_ref_tokens(entry: &agent_contracts::ExternalizedContext) -> usize {
    approx_tokens(&entry.context_ref.uri) + approx_tokens(&entry.context_ref.summary)
}

/// (hot-entity match, open loop, checked path, recency) — higher sorts first.
/// Open loops stay above Checked so unfinished business is not displaced by
/// identity refs. Checked still beats the recency tail so a skipped Stored
/// file body remains Fetch-able after later overflow.
fn external_view_key(
    entry: &agent_contracts::ExternalizedContext,
    hot_entities: &[String],
    checked_files: &[String],
) -> (u8, u8, u8, u64) {
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
    let checked = u8::from(entry_covers_checked_files(entry, checked_files));
    (hot, open_loop, checked, entry.last_access_tick)
}

fn push_store_entity_hits(
    state: &State,
    entity: &str,
    seen: &mut HashSet<ContextItemId>,
    ranked: &mut Vec<agent_contracts::ExternalizedContext>,
) {
    for id in state.external.ids_for_entity(entity) {
        if seen.insert(*id)
            && let Some(entry) = state.external.get(*id)
            && crate::store::externally_retrievable(entry)
        {
            ranked.push(crate::store::prompt_evidence_descriptor(entry.clone()));
        }
    }
}

/// Hint rows plus the engine's last `CheckedFiles` projection, so a skipped
/// Stored body stays reachable even when this `ContextQuery` omitted the hint.
fn merged_checked_files(state: &State, query: &ContextQuery) -> Vec<String> {
    let mut out = query.hints.checked_files.clone();
    for row in &state.checked_files {
        if !out.iter().any(|existing| existing == row) {
            out.push(row.clone());
        }
    }
    out
}

fn checked_row_lookup_path(row: &str) -> Option<String> {
    let row = normalize_resource_path(row);
    if row.is_empty() {
        return None;
    }
    let path = match row.split_once('@') {
        Some((path, _)) => path,
        None => row.as_str(),
    };
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn entry_covers_checked_files(
    entry: &agent_contracts::ExternalizedContext,
    checked_files: &[String],
) -> bool {
    if checked_files.is_empty() {
        return false;
    }
    if let Some(path) = entry.file_path.as_deref()
        && checked_files_cover_path(checked_files, path)
    {
        return true;
    }
    entry
        .entities
        .iter()
        .any(|entity| checked_files_cover_path(checked_files, entity))
}

fn selection_record(
    state: &State,
    item: &ContextItem,
    score: f32,
    approx_tokens: usize,
    reason: String,
    breakdown: ScoreBreakdown,
) -> ContextSelection {
    ContextSelection {
        item_id: item.id,
        score,
        approx_tokens,
        reason,
        breakdown,
        kind: Some(item.kind),
        source: item.source.clone(),
        reactivated: state.reactivation_traces.contains_key(&item.id),
    }
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
    checked_files: &[String],
) -> String {
    let mut reason = if item.retention == ContextRetention::Pinned {
        "explicitly pinned".to_string()
    } else if latest_file_body {
        format!(
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
        )
    } else {
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
    };
    if prices_as_file_body_descriptor(item, checked_files) {
        reason.push_str("; body omitted, path already checked");
    }
    reason
}

fn prices_as_file_body_descriptor(item: &ContextItem, checked_files: &[String]) -> bool {
    if item.kind == agent_contracts::ContextKind::Error || checked_files.is_empty() {
        return false;
    }
    if !matches!(
        item.kind,
        agent_contracts::ContextKind::ToolObservation
            | agent_contracts::ContextKind::FileObservation
    ) {
        return false;
    }
    let Some(path) = observation_file_path(item) else {
        return false;
    };
    agent_contracts::checked_files_cover_path(checked_files, path)
}

fn file_body_descriptor_content(item: &ContextItem) -> String {
    let path = observation_file_path(item).unwrap_or("");
    match item
        .file_revision
        .as_deref()
        .map(str::trim)
        .filter(|revision| !revision.is_empty())
    {
        Some(revision) => format!("{path}@{revision}"),
        None => path.to_string(),
    }
}

fn packed_item_tokens(item: &ContextItem, checked_files: &[String]) -> usize {
    if prices_as_file_body_descriptor(item, checked_files) {
        approx_tokens(&file_body_descriptor_content(item))
            .saturating_add(FILE_BODY_DESCRIPTOR_FRAME_TOKENS)
    } else {
        approx_tokens(&item.content)
    }
}

/// One current-directive file body to project. Memory sources are cloned
/// under the state lock; store ids are read after the lock is dropped.
pub(crate) enum ForegroundPlanItem {
    Ready(Box<ContextItem>),
    Store(ContextItemId),
}

pub(crate) fn plan_foreground(
    state: &State,
    query: &ContextQuery,
    selected: &[MaterializedItem],
) -> Vec<ForegroundPlanItem> {
    let mut plan = Vec::new();
    for key in &query.hints.foreground_resources {
        if plan.len() >= MAX_FOREGROUND_RESOURCES {
            break;
        }
        let path = normalize_resource_path(&key.path);
        if path.is_empty() {
            continue;
        }
        if selected_includes_file_body(selected, &path) {
            continue;
        }
        let revision = key.revision.as_deref();
        if let Some(item) = best_live_file_body(state.items.iter(), &path, revision) {
            plan.push(ForegroundPlanItem::Ready(Box::new(item.clone())));
            continue;
        }
        if let Some(item) = best_live_file_body(state.eviction_buffer.iter(), &path, revision) {
            plan.push(ForegroundPlanItem::Ready(Box::new(item.clone())));
            continue;
        }
        if let Some(id) = best_stored_file_body(state, &path, revision) {
            plan.push(ForegroundPlanItem::Store(id));
        }
    }
    plan
}

pub(crate) async fn realize_foreground(
    plan: Vec<ForegroundPlanItem>,
    dir: &Path,
) -> Vec<MaterializedItem> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for source in plan {
        if out.len() >= MAX_FOREGROUND_RESOURCES || used >= MAX_FOREGROUND_TOKENS {
            break;
        }
        let Some(item) = (match source {
            ForegroundPlanItem::Ready(item) => Some(*item),
            ForegroundPlanItem::Store(id) => store::read_item_async(dir, id).await,
        }) else {
            continue;
        };
        if !item.semantic.is_live() || !is_file_body_observation(&item) {
            continue;
        }
        let remaining = MAX_FOREGROUND_TOKENS.saturating_sub(used);
        let content = clip_to_token_budget(&item.content, remaining);
        if content.is_empty() {
            continue;
        }
        used = used.saturating_add(approx_tokens(&content));
        out.push(MaterializedItem {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            attention: item.attention,
            semantic: item.semantic,
            retention: item.retention,
            content,
            source: item.source.clone(),
            file_path: item.file_path.clone(),
            file_revision: item.file_revision.clone(),
        });
    }
    out
}

fn selected_includes_file_body(selected: &[MaterializedItem], path: &str) -> bool {
    selected.iter().any(|item| {
        let Some(item_path) = item
            .file_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        if normalize_resource_path(item_path) != path {
            return false;
        }
        let descriptor = match item
            .file_revision
            .as_deref()
            .map(str::trim)
            .filter(|revision| !revision.is_empty())
        {
            Some(revision) => format!("{path}@{revision}"),
            None => path.to_string(),
        };
        item.content != descriptor
    })
}

fn revision_ok(item_revision: Option<&str>, wanted: Option<&str>) -> bool {
    let wanted = wanted.map(str::trim).filter(|value| !value.is_empty());
    let item_revision = item_revision
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (wanted, item_revision) {
        (None, _) | (Some(_), None) => true,
        (Some(wanted), Some(have)) => wanted == have,
    }
}

fn best_live_file_body<'a>(
    items: impl Iterator<Item = &'a ContextItem>,
    path: &str,
    revision: Option<&str>,
) -> Option<&'a ContextItem> {
    items
        .filter(|item| item.semantic.is_live() && is_file_body_observation(item))
        .filter(|item| {
            observation_file_path(item)
                .map(normalize_resource_path)
                .as_deref()
                == Some(path)
        })
        .filter(|item| revision_ok(item.file_revision.as_deref(), revision))
        .max_by_key(|item| (item.created_tick, item.last_access_tick))
}

fn best_stored_file_body(
    state: &State,
    path: &str,
    revision: Option<&str>,
) -> Option<ContextItemId> {
    state
        .external
        .iter()
        .filter(|entry| store::externally_retrievable(entry) && is_file_body_entry(entry))
        .filter(|entry| {
            observation_file_path_entry(entry)
                .map(normalize_resource_path)
                .as_deref()
                == Some(path)
        })
        .filter(|entry| revision_ok(entry.file_revision.as_deref(), revision))
        .max_by_key(|entry| (entry.created_tick, entry.last_access_tick))
        .map(|entry| entry.item_id)
}

fn clip_to_token_budget(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    if approx_tokens(text) <= max_tokens {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let prefix: String = chars[..mid].iter().collect();
        if approx_tokens(&prefix) <= max_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect()
}
