//! Task-completion summary insert and episode-rotation distillation.
//! `TaskCompleted` stores the validated CompletionRecord summary verbatim;
//! only episode rotation may call the LLM compactor.

use agent_contracts::{
    CompactionReason, ContextItemId, ContextKind, ContextRetention, ContextScope, CoreLabel,
    DependencyEdge, DependencyKind, ScopeId, ScopeKind, ScopeState, TaskId,
    bound_compaction_source,
};

use crate::engine::{SimpleContextConfig, State};
use crate::index::dependency;
use crate::item;

pub(crate) struct DistillJob {
    pub(crate) task_id: Option<TaskId>,
    pub(crate) summary_scope_id: Option<ScopeId>,
    pub(crate) fallback: String,
    pub(crate) source: String,
    pub(crate) source_ids: Vec<ContextItemId>,
    pub(crate) source_label: &'static str,
    pub(crate) reason: CompactionReason,
}

const MAX_DISTILL_SOURCES: usize = 8;
const TASK_SUMMARY_SOURCE: &str = "task-summary";
const EPISODE_DERIVED_SOURCE: &str = "episode-derived";

/// Plan a sourced distill of the *closing* focus episode. Called before
/// `close_focus_episode` so membership still uses the open focus scope's
/// `opened_tick`. Raw bodies stay; the compact result becomes a Durable
/// task-scope card. `None` when there is nothing to distill.
pub(crate) fn plan_episode_distill(
    state: &State,
    config: &SimpleContextConfig,
) -> Option<DistillJob> {
    let task = state.focus.as_ref()?.task_id;
    let opened_tick = state
        .scopes
        .iter()
        .find(|scope| {
            scope.kind == ScopeKind::Focus
                && scope.task_id == Some(task)
                && scope.state != ScopeState::Closed
        })
        .map(|scope| scope.opened_tick)?;
    let members: Vec<(ContextItemId, &str)> = state
        .items
        .iter()
        .filter(|item| {
            item.task_id == Some(task)
                && item.created_tick >= opened_tick
                && item.semantic.is_live()
                && item.source.as_deref() != Some(EPISODE_DERIVED_SOURCE)
        })
        .map(|item| (item.id, item.content.as_str()))
        .collect();
    if members.is_empty() {
        return None;
    }
    // Short episodes with no durable semantic payload are not worth a
    // provider round: raw tool dumps stay retrievable without an LLM card.
    // Long episodes may still rotate; generation count is not a distill
    // trigger. Ablation-only `force_episode_llm_distill` overrides this.
    if !config.force_episode_llm_distill && !episode_worth_llm_distill(state, opened_tick, task) {
        return None;
    }
    let start = members.len().saturating_sub(MAX_DISTILL_SOURCES);
    let mut source = String::new();
    let mut source_ids = Vec::new();
    for (id, content) in &members[start..] {
        source_ids.push(*id);
        source.push_str(content);
        source.push('\n');
    }
    let source = bound_compaction_source(&source);
    let summary_scope_id = state
        .scopes
        .iter()
        .find(|scope| {
            scope.kind == ScopeKind::Task
                && scope.task_id == Some(task)
                && scope.state != ScopeState::Closed
        })
        .map(|scope| scope.id);
    Some(DistillJob {
        task_id: Some(task),
        summary_scope_id,
        fallback: format!("[episode] {source}"),
        source,
        source_ids,
        source_label: EPISODE_DERIVED_SOURCE,
        reason: CompactionReason::EpisodeRotation,
    })
}

fn episode_worth_llm_distill(state: &State, opened_tick: u64, task: TaskId) -> bool {
    state
        .items
        .iter()
        .any(|item| item_carries_semantic_delta(item, opened_tick, task))
}

fn item_carries_semantic_delta(
    item: &agent_contracts::ContextItem,
    opened_tick: u64,
    task: TaskId,
) -> bool {
    if item.task_id != Some(task) || item.created_tick < opened_tick {
        return false;
    }
    if matches!(
        item.semantic,
        agent_contracts::SemanticState::VerifiedFixed { .. }
    ) {
        return true;
    }
    if !item.semantic.is_live() {
        return false;
    }
    if item.kind == ContextKind::Error {
        return true;
    }
    if item.tags.iter().any(|tag| {
        tag.is_core(CoreLabel::Decision)
            || tag.is_core(CoreLabel::Finding)
            || tag.is_core(CoreLabel::Constraint)
            || tag.is_core(CoreLabel::OpenLoop)
    }) {
        return true;
    }
    matches!(
        item.retention,
        ContextRetention::Durable | ContextRetention::Pinned
    ) && matches!(
        item.kind,
        ContextKind::Constraint | ContextKind::Decision | ContextKind::Summary | ContextKind::Goal
    )
}

pub(crate) fn insert_derived_summary(
    state: &mut State,
    config: &SimpleContextConfig,
    task_id: Option<TaskId>,
    summary_scope_id: Option<ScopeId>,
    content: String,
    source_ids: &[ContextItemId],
    source_label: &str,
) -> ContextItemId {
    let mut item = item::make_item(
        state,
        config,
        content,
        ContextKind::Summary,
        ContextScope::Session,
        ContextRetention::Durable,
        0.84,
        Some(source_label.to_string()),
    );
    item.task_id = task_id;
    if let Some(scope_id) = summary_scope_id {
        item.scope_id = Some(scope_id);
    }
    for source_id in source_ids {
        item.dependencies.push(DependencyEdge {
            target: *source_id,
            kind: DependencyKind::DerivedFrom,
        });
    }
    let id = dependency::push_linked(state, config, item);
    if source_label == EPISODE_DERIVED_SOURCE {
        queue_prior_episode_cards(state, task_id, id);
    }
    id
}

pub(crate) fn insert_task_summary(
    state: &mut State,
    config: &SimpleContextConfig,
    completed_task: Option<TaskId>,
    summary_scope_id: Option<ScopeId>,
    content: String,
    source_ids: &[ContextItemId],
) {
    insert_derived_summary(
        state,
        config,
        completed_task,
        summary_scope_id,
        content,
        source_ids,
        TASK_SUMMARY_SOURCE,
    );
}

/// One live episode card per task: a newer rotation supersedes the previous
/// card wherever its body sits. Terminal semantic death is drained by the
/// next maintain pass so the transition is observable. Raw episode bodies
/// stay retrievable; only the derived card is superseded.
fn queue_prior_episode_cards(state: &mut State, task: Option<TaskId>, by_id: ContextItemId) {
    let Some(task) = task else {
        return;
    };
    let reason = "episode rotated, prior episode card superseded".to_string();
    let mut old = Vec::new();
    for item in state.items.iter() {
        if item.id != by_id
            && item.task_id == Some(task)
            && item.source.as_deref() == Some(EPISODE_DERIVED_SOURCE)
            && item.semantic.is_live()
        {
            old.push(item.id);
        }
    }
    for item in &state.eviction_buffer {
        if item.id != by_id
            && item.task_id == Some(task)
            && item.source.as_deref() == Some(EPISODE_DERIVED_SOURCE)
            && item.semantic.is_live()
        {
            old.push(item.id);
        }
    }
    for entry in state.external.iter() {
        if entry.item_id != by_id
            && entry.task_id == Some(task)
            && entry.source.as_deref() == Some(EPISODE_DERIVED_SOURCE)
            && entry.semantic.is_live()
        {
            old.push(entry.item_id);
        }
    }
    for id in old {
        state
            .pending_supersessions
            .push((id, by_id, reason.clone()));
    }
}
