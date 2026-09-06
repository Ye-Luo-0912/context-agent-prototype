use std::cmp::Ordering;
use std::collections::HashSet;

use std::path::Path;

use agent_contracts::{
    AttentionState, CONTEXT_CONSUMPTION_ACK_ITEM_CAP, CONTEXT_MAP_VIEW_CAP, ContextItem,
    ContextItemId, ContextMapView, ContextMaterializationIdentity, ContextMaterializationMiss,
    ContextMaterializationMissReason, ContextMaterializationMisses, ContextQuery, ContextRetention,
    ContextSelection, MAX_FOREGROUND_RESOURCES, MAX_FOREGROUND_TOKENS, MaterializedContext,
    MaterializedItem, ScopeId, ScopeKind, ScopeState, ScoreBreakdown, checked_files_cover_path,
    normalize_resource_path,
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
/// ... | path=...]`). Charged only when another layer of the same request
/// already carries this exact file body.
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
        let tokens = packed_item_tokens(item, &query.hints.visible_body_identities);
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
                &query.hints.visible_body_identities,
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

    // Then the scored candidates fill the rest of the frame. An item the
    // prompt-required pass already selected (a non-Pinned anchor claim) must
    // not be picked — and budget-charged — a second time here.
    for (index, breakdown, tokens) in candidates {
        let item = &state.items[index];
        if item.retention == ContextRetention::Pinned || selected_indices.contains(&index) {
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
                &query.hints.visible_body_identities,
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
                let tokens = packed_item_tokens(dep, &query.hints.visible_body_identities);
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
        if prices_as_file_body_descriptor(item, &query.hints.visible_body_identities) {
            state.selected_descriptor_paths.insert(path);
        } else {
            state.selected_body_paths.insert(path);
        }
    }
    let items: Vec<MaterializedItem> = selected_indices
        .iter()
        .map(|index| {
            let item = &state.items[*index];
            let content =
                if prices_as_file_body_descriptor(item, &query.hints.visible_body_identities) {
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
                // Selected bodies are full (never preview-clipped);
                // foreground clipping is the only partial projection.
                partial_body: false,
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
        required_item_ids: Vec::new(),
        required_misses: ContextMaterializationMisses::default(),
        optional_misses: ContextMaterializationMisses::default(),
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
        if ranked.len() >= MAX_EXTERNAL_REFS {
            break;
        }
        let Some(path) = checked_row_lookup_path(row) else {
            continue;
        };
        if !seen_checked_paths.insert(path.clone()) {
            continue;
        }
        push_store_entity_hits(state, &path, &mut seen, &mut ranked);
    }
    for item in &state.eviction_buffer {
        if ranked.len() >= MAX_EXTERNAL_REFS {
            break;
        }
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
        if ranked.len() >= MAX_EXTERNAL_REFS {
            break;
        }
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
    // The 32-row view cap is applied during collection: a hot entity with a
    // huge bucket must not clone and stage every descriptor before the limit
    // cuts in.
    for id in state.external.ids_for_entity(entity) {
        if ranked.len() >= MAX_EXTERNAL_REFS {
            return;
        }
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
    visible_body_identities: &[String],
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
    if prices_as_file_body_descriptor(item, visible_body_identities) {
        reason.push_str("; body omitted, exact body already visible");
    }
    reason
}

fn prices_as_file_body_descriptor(item: &ContextItem, visible_body_identities: &[String]) -> bool {
    if item.kind == agent_contracts::ContextKind::Error
        || visible_body_identities.is_empty()
        || !is_file_body_observation(item)
    {
        return false;
    }
    let Some(path) = observation_file_path(item) else {
        return false;
    };
    agent_contracts::visible_body_identities_cover(
        visible_body_identities,
        path,
        item.file_revision.as_deref(),
    )
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

fn packed_item_tokens(item: &ContextItem, visible_body_identities: &[String]) -> usize {
    if prices_as_file_body_descriptor(item, visible_body_identities) {
        approx_tokens(&file_body_descriptor_content(item))
            .saturating_add(FILE_BODY_DESCRIPTOR_FRAME_TOKENS)
    } else {
        approx_tokens(&item.content)
    }
}

/// One current-directive file body to project. Memory sources are cloned
/// under the state lock; store ids are read after the lock is dropped.
pub(crate) enum ForegroundPlanItem {
    Ready {
        item: Box<ContextItem>,
        identity: ContextMaterializationIdentity,
    },
    Store {
        item_id: ContextItemId,
        checksum: Option<String>,
        identity: ContextMaterializationIdentity,
    },
}

pub(crate) struct ForegroundPlan {
    pub(crate) items: Vec<ForegroundPlanItem>,
    pub(crate) misses: ContextMaterializationMisses,
}

pub(crate) fn plan_foreground(
    state: &State,
    query: &ContextQuery,
    selected: &[MaterializedItem],
) -> ForegroundPlan {
    let mut plan = Vec::new();
    let mut misses = ContextMaterializationMisses::default();
    for key in &query.hints.foreground_resources {
        if plan.len() >= MAX_FOREGROUND_RESOURCES {
            misses.push(context_miss(
                resource_identity(key),
                ContextMaterializationMissReason::BudgetExcluded,
            ));
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
        let identity = resource_identity(key);
        if let Some(item) = best_live_file_body(state.items.iter(), &path, revision) {
            plan.push(ForegroundPlanItem::Ready {
                item: Box::new(item.clone()),
                identity,
            });
            continue;
        }
        if let Some(item) = best_live_file_body(state.eviction_buffer.iter(), &path, revision) {
            plan.push(ForegroundPlanItem::Ready {
                item: Box::new(item.clone()),
                identity,
            });
            continue;
        }
        if let Some(entry) = best_stored_file_body(state, &path, revision) {
            plan.push(ForegroundPlanItem::Store {
                item_id: entry.item_id,
                checksum: entry.blob_checksum.clone(),
                identity,
            });
        } else {
            misses.push(context_miss(
                identity,
                ContextMaterializationMissReason::Missing,
            ));
        }
    }
    ForegroundPlan {
        items: plan,
        misses,
    }
}

pub(crate) async fn realize_foreground(
    plan: ForegroundPlan,
    dir: &Path,
) -> (Vec<MaterializedItem>, ContextMaterializationMisses) {
    let mut out = Vec::new();
    let mut used = 0usize;
    let mut misses = plan.misses;
    for source in plan.items {
        if out.len() >= MAX_FOREGROUND_RESOURCES || used >= MAX_FOREGROUND_TOKENS {
            let identity = match source {
                ForegroundPlanItem::Ready { identity, .. }
                | ForegroundPlanItem::Store { identity, .. } => identity,
            };
            misses.push(context_miss(
                identity,
                ContextMaterializationMissReason::BudgetExcluded,
            ));
            break;
        }
        let (item, identity) = match source {
            ForegroundPlanItem::Ready { item, identity } => (*item, identity),
            ForegroundPlanItem::Store {
                item_id,
                checksum,
                identity,
            } => match store::read_item_checked_async(dir, item_id, checksum.as_deref()).await {
                Ok(item) => (item, identity),
                Err(failure) => {
                    misses.push(context_miss(identity, store_miss_reason(failure)));
                    continue;
                }
            },
        };
        if !item.semantic.is_live() || !is_file_body_observation(&item) {
            misses.push(context_miss(
                identity,
                ContextMaterializationMissReason::PolicyExcluded,
            ));
            continue;
        }
        let remaining = MAX_FOREGROUND_TOKENS.saturating_sub(used);
        let (content, clipped) = clip_to_token_budget(&item.content, remaining);
        if content.is_empty() {
            misses.push(context_miss(
                identity,
                ContextMaterializationMissReason::BudgetExcluded,
            ));
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
            // A clipped foreground body is an explicit partial projection:
            // it keeps its identity for display but must never be treated
            // as the full revision by required claims or downstream ledgers.
            partial_body: clipped,
        });
    }
    (out, misses)
}

fn resource_identity(key: &agent_contracts::ResourceKey) -> ContextMaterializationIdentity {
    let path = normalize_resource_path(&key.path);
    let item_ref = match key
        .revision
        .as_deref()
        .map(str::trim)
        .filter(|revision| !revision.is_empty())
    {
        Some(revision) => format!("{path}@{revision}"),
        None => path,
    };
    ContextMaterializationIdentity::new(item_ref, None, "foreground_resources", 0)
}

fn context_miss(
    identity: ContextMaterializationIdentity,
    reason: ContextMaterializationMissReason,
) -> ContextMaterializationMiss {
    ContextMaterializationMiss { identity, reason }
}

fn store_miss_reason(failure: store::StoreReadFailure) -> ContextMaterializationMissReason {
    match failure {
        store::StoreReadFailure::Missing => ContextMaterializationMissReason::Missing,
        store::StoreReadFailure::Corrupt => ContextMaterializationMissReason::Corrupt,
        store::StoreReadFailure::IoFailed => ContextMaterializationMissReason::IoFailed,
    }
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

fn best_stored_file_body<'a>(
    state: &'a State,
    path: &str,
    revision: Option<&str>,
) -> Option<&'a agent_contracts::ExternalizedContext> {
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
}

enum RequiredPlanSource {
    Ready(Box<ContextItem>),
    Store {
        item_id: ContextItemId,
        checksum: Option<String>,
    },
}

struct RequiredPlanItem {
    identity: ContextMaterializationIdentity,
    source: RequiredPlanSource,
}

pub(crate) struct RequiredPlan {
    items: Vec<RequiredPlanItem>,
    misses: ContextMaterializationMisses,
}

#[cfg(test)]
impl RequiredPlan {
    pub(crate) fn body_count(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn store_read_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(&item.source, RequiredPlanSource::Store { .. }))
            .count()
    }

    pub(crate) fn miss_count(&self) -> u32 {
        self.misses.total()
    }
}

const MAX_REQUIRED_PLAN_OBSERVATIONS: usize =
    CONTEXT_CONSUMPTION_ACK_ITEM_CAP + agent_contracts::CONTEXT_MATERIALIZATION_MISS_CAP;

pub(crate) struct RequiredBody {
    item: ContextItem,
    identity: ContextMaterializationIdentity,
}

/// Plan every mandatory body under the state lock. This does not change
/// residency or scoring: it only makes the already-defined Pinned and
/// PromptRequired contract explicit when its body lives outside the heap.
pub(crate) fn plan_required(state: &State, query: &ContextQuery) -> RequiredPlan {
    let mut items = Vec::new();
    let mut misses = ContextMaterializationMisses::default();
    let mut seen = HashSet::new();

    // Pinned retention keeps its existing priority ahead of anchor roots.
    for item in state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        .filter(|item| item.retention == ContextRetention::Pinned)
    {
        let identity = pinned_identity(item.id);
        if !plan_memory_required(
            state,
            query,
            item,
            identity,
            &mut seen,
            &mut items,
            &mut misses,
        ) {
            break;
        }
    }
    for item_id in state.external.pinned_ids().iter().copied() {
        let Some(entry) = state.external.get(item_id) else {
            continue;
        };
        let identity = pinned_identity(entry.item_id);
        if !plan_store_required(entry, identity, &mut seen, &mut items, &mut misses) {
            break;
        }
    }

    for claim in query
        .hints
        .anchor_roots
        .iter()
        .filter(|claim| claim.strength.requires_prompt())
        .take(agent_contracts::MAX_ANCHOR_ROOT_CLAIMS)
    {
        let mut matched = false;
        let mut bounded_out = false;

        // Direct id/uri lookup is O(1) in resident and stored catalogs.
        if let Ok(id) = ContextItemId::parse_ref(&claim.item_ref) {
            if let Some(index) = state.items.indexes().get(id) {
                matched = true;
                let item = &state.items[index];
                bounded_out = !plan_memory_required(
                    state,
                    query,
                    item,
                    claim_identity(claim, Some(id)),
                    &mut seen,
                    &mut items,
                    &mut misses,
                );
            } else if let Some(item) = state.eviction_buffer.iter().find(|item| item.id == id) {
                matched = true;
                bounded_out = !plan_memory_required(
                    state,
                    query,
                    item,
                    claim_identity(claim, Some(id)),
                    &mut seen,
                    &mut items,
                    &mut misses,
                );
            } else if let Some(entry) = state.external.get(id) {
                matched = true;
                bounded_out = !plan_store_required(
                    entry,
                    claim_identity(claim, Some(id)),
                    &mut seen,
                    &mut items,
                    &mut misses,
                );
            }
        }

        // Exact entity buckets preserve the established anchor matching
        // semantics without scanning a long resident/store catalog.
        for id in state.items.indexes().ids_for_entity(&claim.item_ref) {
            if bounded_out {
                break;
            }
            let Some(index) = state.items.indexes().get(*id) else {
                continue;
            };
            matched = true;
            let item = &state.items[index];
            let identity = claim_identity(claim, Some(item.id));
            bounded_out = !plan_memory_required(
                state,
                query,
                item,
                identity,
                &mut seen,
                &mut items,
                &mut misses,
            );
        }
        for item in state
            .eviction_buffer
            .iter()
            .filter(|item| crate::engine::anchor_claim_matches_item(claim, item))
        {
            if bounded_out {
                break;
            }
            matched = true;
            bounded_out = !plan_memory_required(
                state,
                query,
                item,
                claim_identity(claim, Some(item.id)),
                &mut seen,
                &mut items,
                &mut misses,
            );
        }
        for id in state.external.ids_for_entity(&claim.item_ref) {
            if bounded_out {
                break;
            }
            let Some(entry) = state.external.get(*id) else {
                continue;
            };
            matched = true;
            bounded_out = !plan_store_required(
                entry,
                claim_identity(claim, Some(entry.item_id)),
                &mut seen,
                &mut items,
                &mut misses,
            );
        }
        if bounded_out {
            break;
        }
        if !matched {
            misses.push(context_miss(
                claim_identity(claim, None),
                ContextMaterializationMissReason::Missing,
            ));
        }
    }

    RequiredPlan { items, misses }
}

fn plan_memory_required(
    state: &State,
    query: &ContextQuery,
    item: &ContextItem,
    identity: ContextMaterializationIdentity,
    seen: &mut HashSet<ContextItemId>,
    items: &mut Vec<RequiredPlanItem>,
    misses: &mut ContextMaterializationMisses,
) -> bool {
    if seen.contains(&item.id) {
        return true;
    }
    if seen.len() >= MAX_REQUIRED_PLAN_OBSERVATIONS {
        push_required_plan_overflow(misses);
        return false;
    }
    seen.insert(item.id);
    if is_excluded(item) {
        misses.push(context_miss(
            identity,
            ContextMaterializationMissReason::PolicyExcluded,
        ));
        return true;
    }
    // The current user message is already carried by the fixed TurnFrame;
    // duplicating it as historical context is neither necessary nor a miss.
    if item.kind == agent_contracts::ContextKind::UserMessage
        && state.turn > 0
        && item.created_turn == state.turn
        && item.content == query.current_input
    {
        return true;
    }
    if items.len() >= CONTEXT_CONSUMPTION_ACK_ITEM_CAP {
        misses.push(context_miss(
            identity,
            ContextMaterializationMissReason::BudgetExcluded,
        ));
        return true;
    }
    items.push(RequiredPlanItem {
        identity,
        source: RequiredPlanSource::Ready(Box::new(item.clone())),
    });
    true
}

fn plan_store_required(
    entry: &agent_contracts::ExternalizedContext,
    identity: ContextMaterializationIdentity,
    seen: &mut HashSet<ContextItemId>,
    items: &mut Vec<RequiredPlanItem>,
    misses: &mut ContextMaterializationMisses,
) -> bool {
    if seen.contains(&entry.item_id) {
        return true;
    }
    if seen.len() >= MAX_REQUIRED_PLAN_OBSERVATIONS {
        push_required_plan_overflow(misses);
        return false;
    }
    seen.insert(entry.item_id);
    let legacy_excluded = entry.tags.iter().any(|tag| {
        tag.is_lifecycle(agent_contracts::LifecycleLabel::Superseded)
            || tag.is_lifecycle(agent_contracts::LifecycleLabel::VerifiedFixed)
    });
    if !entry.semantic.is_live() || legacy_excluded {
        misses.push(context_miss(
            identity,
            ContextMaterializationMissReason::PolicyExcluded,
        ));
        return true;
    }
    if items.len() >= CONTEXT_CONSUMPTION_ACK_ITEM_CAP {
        misses.push(context_miss(
            identity,
            ContextMaterializationMissReason::BudgetExcluded,
        ));
        return true;
    }
    items.push(RequiredPlanItem {
        identity,
        source: RequiredPlanSource::Store {
            item_id: entry.item_id,
            checksum: entry.blob_checksum.clone(),
        },
    });
    true
}

fn push_required_plan_overflow(misses: &mut ContextMaterializationMisses) {
    misses.push(context_miss(
        ContextMaterializationIdentity::new(
            "context://required-set-overflow",
            None,
            "materialization",
            0,
        ),
        ContextMaterializationMissReason::BudgetExcluded,
    ));
}

fn pinned_identity(item_id: ContextItemId) -> ContextMaterializationIdentity {
    ContextMaterializationIdentity::new(
        format!("context://run/{item_id}"),
        Some(item_id),
        "retention:pinned",
        0,
    )
}

fn claim_identity(
    claim: &agent_contracts::AnchorRootClaim,
    item_id: Option<ContextItemId>,
) -> ContextMaterializationIdentity {
    ContextMaterializationIdentity::new(
        claim.item_ref.clone(),
        item_id,
        claim.source_field_id.clone(),
        claim.anchor_revision,
    )
}

/// Execute required store reads outside the state lock with the same bounded
/// concurrency as GC. Result order is restored by plan ordinal so events and
/// packing stay deterministic even when disk completion order differs.
pub(crate) async fn realize_required(
    plan: RequiredPlan,
    dir: &Path,
) -> (Vec<RequiredBody>, ContextMaterializationMisses) {
    let mut slots: Vec<(
        ContextMaterializationIdentity,
        Option<Result<ContextItem, store::StoreReadFailure>>,
    )> = Vec::with_capacity(plan.items.len());
    let mut jobs = std::collections::VecDeque::new();
    for (ordinal, item) in plan.items.into_iter().enumerate() {
        match item.source {
            RequiredPlanSource::Ready(body) => {
                slots.push((item.identity, Some(Ok(*body))));
            }
            RequiredPlanSource::Store { item_id, checksum } => {
                slots.push((item.identity, None));
                jobs.push_back((ordinal, item_id, checksum));
            }
        }
    }

    let mut reads = tokio::task::JoinSet::new();
    while !jobs.is_empty() || !reads.is_empty() {
        while reads.len() < store::MAX_STORE_IO_CONCURRENCY {
            let Some((ordinal, item_id, checksum)) = jobs.pop_front() else {
                break;
            };
            let dir = dir.to_path_buf();
            reads.spawn(async move {
                let result =
                    store::read_item_checked_async(&dir, item_id, checksum.as_deref()).await;
                (ordinal, result)
            });
        }
        if let Some(Ok((ordinal, result))) = reads.join_next().await
            && let Some(slot) = slots.get_mut(ordinal)
        {
            slot.1 = Some(result);
        }
    }

    let mut bodies = Vec::new();
    let mut misses = plan.misses;
    for (identity, result) in slots {
        match result {
            Some(Ok(item)) if item.semantic.is_live() && !is_excluded(&item) => {
                bodies.push(RequiredBody { item, identity });
            }
            Some(Ok(_)) => misses.push(context_miss(
                identity,
                ContextMaterializationMissReason::PolicyExcluded,
            )),
            Some(Err(failure)) => misses.push(context_miss(identity, store_miss_reason(failure))),
            None => misses.push(context_miss(
                identity,
                ContextMaterializationMissReason::IoFailed,
            )),
        }
    }
    (bodies, misses)
}

/// Overlay mandatory bodies onto the unchanged scored selection. Optional
/// selections are displaced only when needed to uphold a pre-existing
/// Pinned/PromptRequired contract; scoring thresholds and GC state do not
/// move. Anything that still cannot fit becomes an explicit hard miss.
pub(crate) fn apply_required(
    materialized: &mut MaterializedContext,
    query: &ContextQuery,
    bodies: Vec<RequiredBody>,
    mut misses: ContextMaterializationMisses,
) {
    for required in bodies {
        let item_id = required.item.id;
        if materialized.required_item_ids.len() >= CONTEXT_CONSUMPTION_ACK_ITEM_CAP {
            misses.push(context_miss(
                required.identity,
                ContextMaterializationMissReason::BudgetExcluded,
            ));
            continue;
        }

        let already_visible = materialized
            .items
            .iter()
            .chain(materialized.foreground.iter())
            .any(|item| item.item_id == item_id && !item.partial_body);
        if already_visible {
            materialized.required_item_ids.push(item_id);
            remove_external_descriptor(materialized, item_id);
            continue;
        }

        let tokens = packed_item_tokens(&required.item, &query.hints.visible_body_identities);
        let max_items = query.hints.max_selected_items.unwrap_or(usize::MAX);
        let Some(evictions) =
            plan_required_evictions(materialized, tokens, query.budget_tokens, max_items)
        else {
            misses.push(context_miss(
                required.identity,
                ContextMaterializationMissReason::BudgetExcluded,
            ));
            continue;
        };
        for item_id in evictions {
            drop_optional_item(materialized, item_id);
        }

        let content =
            if prices_as_file_body_descriptor(&required.item, &query.hints.visible_body_identities)
            {
                file_body_descriptor_content(&required.item)
            } else {
                required.item.content.clone()
            };
        materialized.items.push(MaterializedItem {
            item_id,
            kind: required.item.kind,
            scope: required.item.scope,
            attention: required.item.attention,
            semantic: required.item.semantic,
            retention: required.item.retention,
            content,
            source: required.item.source.clone(),
            file_path: required.item.file_path.clone(),
            file_revision: required.item.file_revision.clone(),
            // Required bodies are always embedded in full; a partial
            // foreground copy was already rejected above.
            partial_body: false,
        });
        materialized.selected.push(ContextSelection {
            item_id,
            score: 0.0,
            approx_tokens: tokens,
            reason: "required context body included by Pinned/PromptRequired contract".into(),
            breakdown: ScoreBreakdown::default(),
            kind: Some(required.item.kind),
            source: required.item.source.clone(),
            reactivated: false,
        });
        materialized.required_item_ids.push(item_id);
        remove_external_descriptor(materialized, item_id);
    }

    materialized.approx_tokens = selected_item_tokens(materialized)
        + materialized
            .external
            .iter()
            .map(external_ref_tokens)
            .sum::<usize>();
    materialized.required_misses = misses;
}

fn packed_selected_tokens(materialized: &MaterializedContext) -> usize {
    materialized
        .selected
        .iter()
        .map(|selection| selection.approx_tokens)
        .sum()
}

fn selected_item_tokens(materialized: &MaterializedContext) -> usize {
    materialized
        .items
        .iter()
        .map(|item| approx_tokens(&item.content))
        .sum()
}

/// Plan displacement without mutating the current frame. Required bodies are
/// applied one at a time; if this body cannot fit even after every eligible
/// optional item is removed, the existing visible set must remain intact.
/// This makes each required-body overlay transactional while preserving the
/// established largest-first displacement order.
fn plan_required_evictions(
    materialized: &MaterializedContext,
    required_tokens: usize,
    budget_tokens: usize,
    max_items: usize,
) -> Option<Vec<ContextItemId>> {
    let mut packed_tokens = packed_selected_tokens(materialized);
    let mut item_count = materialized.items.len();
    if item_count < max_items && packed_tokens.saturating_add(required_tokens) <= budget_tokens {
        return Some(Vec::new());
    }

    let mut optional: Vec<(ContextItemId, usize, usize)> = materialized
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.retention != ContextRetention::Pinned
                && !materialized.required_item_ids.contains(&item.item_id)
        })
        .map(|(ordinal, item)| {
            let tokens = materialized
                .selected
                .iter()
                .find(|selection| selection.item_id == item.item_id)
                .map(|selection| selection.approx_tokens)
                .unwrap_or_else(|| approx_tokens(&item.content));
            (item.item_id, tokens, ordinal)
        })
        .collect();
    optional.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.2.cmp(&right.2)));

    let mut evictions = Vec::new();
    for (item_id, tokens, _) in optional {
        packed_tokens = packed_tokens.saturating_sub(tokens);
        item_count = item_count.saturating_sub(1);
        evictions.push(item_id);
        if item_count < max_items && packed_tokens.saturating_add(required_tokens) <= budget_tokens
        {
            return Some(evictions);
        }
    }
    None
}

fn drop_optional_item(materialized: &mut MaterializedContext, item_id: ContextItemId) {
    materialized.items.retain(|item| item.item_id != item_id);
    materialized
        .selected
        .retain(|selection| selection.item_id != item_id);
}

fn remove_external_descriptor(materialized: &mut MaterializedContext, item_id: ContextItemId) {
    if materialized
        .external
        .iter()
        .all(|entry| entry.item_id != item_id)
    {
        return;
    }
    materialized.external = ContextMapView::new(
        materialized
            .external
            .iter()
            .filter(|entry| entry.item_id != item_id)
            .cloned()
            .collect(),
    );
}

/// Clip `text` to a token budget, returning the prefix plus whether any
/// content was cut. A cut body is a partial projection; callers must mark
/// it so it cannot stand in for the full revision.
fn clip_to_token_budget(text: &str, max_tokens: usize) -> (String, bool) {
    if max_tokens == 0 {
        return (String::new(), true);
    }
    if approx_tokens(text) <= max_tokens {
        return (text.to_string(), false);
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
    (chars[..lo].iter().collect(), true)
}
