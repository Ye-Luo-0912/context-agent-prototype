use std::cmp::Ordering;

use agent_contracts::{
    AgentResult, ContextBuildRequest, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItem, ContextItemId, ContextItemSummary, ContextKind, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextRetention, ContextScope, ContextSelection, ContextSnapshot,
    ContextState, ContextStateTransition, FocusState, ModelMessage, ScoreBreakdown, TaskId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::scoring::{extract_entities, score_item_with_breakdown};

/// Per-item cap on explicit dependency edges recorded at ingest.
const MAX_DEPENDENCY_EDGES: usize = 8;
/// Per-snapshot cap on items pulled in by dependency expansion.
const MAX_EXPANSION_ITEMS: usize = 8;
/// Token reserve carved out of the model budget so dependency expansion
/// can follow selected items without blowing the budget.
const EXPANSION_RESERVE_TOKENS: usize = 1024;
/// Cap for the hot-entity set, matching `extract_entities`'s per-text cap.
const MAX_HOT_ENTITIES: usize = 24;

#[derive(Debug, Clone)]
pub struct SimpleContextConfig {
    pub active_threshold: f32,
    pub archive_threshold: f32,
    pub turn_ttl_ticks: u64,
    pub max_item_chars: usize,
    /// Detect superseding decisions and archive the superseded ones.
    pub supersession: bool,
    /// Error -> fix -> verified lifecycle (errors persist until a
    /// successful result on the same entities verifies the fix).
    pub error_verification: bool,
    /// Reward items whose entity signature is hot (last user message +
    /// recent tool observations).
    pub entity_affinity: bool,
    /// Record explicit dependency edges between items sharing entities
    /// and expand the working set with dependencies of selected items.
    pub dependency_expansion: bool,
}

impl Default for SimpleContextConfig {
    fn default() -> Self {
        Self {
            active_threshold: 0.58,
            archive_threshold: 0.24,
            turn_ttl_ticks: 5,
            max_item_chars: 16_000,
            supersession: true,
            error_verification: true,
            entity_affinity: true,
            dependency_expansion: true,
        }
    }
}

impl SimpleContextConfig {
    /// The baseline policy: no supersession, no error verification, no entity
    /// affinity, no dependency graph. Kept for A/B/C comparison so the P4
    /// delta is measurable.
    pub fn baseline_v0() -> Self {
        Self {
            supersession: false,
            error_verification: false,
            entity_affinity: false,
            dependency_expansion: false,
            ..Self::default()
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    tick: u64,
    turn: u64,
    tool_round: u64,
    focus: Option<FocusState>,
    completed_task_id: Option<TaskId>,
    items: Vec<ContextItem>,
    /// (item_id, reason) queued by ingest, drained by maintain so the
    /// resulting state change is recorded as a lifecycle transition.
    #[serde(default)]
    pending_supersessions: Vec<(ContextItemId, String)>,
    /// (item_id, reason) queued by ingest for verified-fixed errors.
    #[serde(default)]
    pending_verifications: Vec<(ContextItemId, String)>,
    /// Entities named by the last user message or touched by recent tool
    /// observations. Reset on user message / focus change, extended by tools.
    #[serde(default)]
    hot_entities: Vec<String>,
}

pub struct SimpleContextEngine {
    config: SimpleContextConfig,
    state: Mutex<State>,
}

impl SimpleContextEngine {
    pub fn new(config: SimpleContextConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State::default()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_item(
        &self,
        state: &State,
        content: String,
        kind: ContextKind,
        scope: ContextScope,
        retention: ContextRetention,
        importance: f32,
        source: Option<String>,
    ) -> ContextItem {
        let content = truncate_chars(content, self.config.max_item_chars);
        ContextItem {
            id: ContextItemId::new(),
            task_id: state.focus.as_ref().map(|f| f.task_id),
            content,
            kind,
            scope,
            retention,
            state: ContextState::Active,
            importance,
            relevance: 0.5,
            created_tick: state.tick,
            last_access_tick: state.tick,
            access_count: 0,
            created_turn: state.turn,
            last_access_turn: state.turn,
            dependencies: Vec::new(),
            tags: Vec::new(),
            source,
        }
    }

    fn diagnostics_locked(&self, state: &State) -> ContextDiagnostics {
        let mut diagnostics = ContextDiagnostics {
            total_items: state.items.len(),
            focus_generation: state.focus.as_ref().map_or(0, |f| f.generation),
            turn: state.turn,
            tool_round: state.tool_round,
            ..ContextDiagnostics::default()
        };

        for item in &state.items {
            match item.state {
                ContextState::Active => diagnostics.active_items += 1,
                ContextState::Cooling => diagnostics.cooling_items += 1,
                ContextState::Archived => diagnostics.archived_items += 1,
                ContextState::Dropped => diagnostics.dropped_items += 1,
            }

            if item.state == ContextState::Active {
                diagnostics.approx_active_tokens += approx_tokens(&item.content);
            }
        }

        diagnostics
    }

    /// Link a fresh item to prior non-dropped items that share at least one
    /// entity (new item depends on prior), bounded per item, then store it.
    /// Gated by `dependency_expansion` so `baseline_v0()` records no edges.
    fn push_linked(&self, state: &mut State, mut item: ContextItem) -> ContextItemId {
        if self.config.dependency_expansion {
            let entities = extract_entities(&item.content);
            if !entities.is_empty() {
                let mut edges = 0usize;
                for prior in state.items.iter().rev() {
                    if prior.state == ContextState::Dropped {
                        continue;
                    }
                    let prior_entities = extract_entities(&prior.content);
                    if prior_entities.is_empty() {
                        continue;
                    }
                    if entities.iter().any(|entity| {
                        prior_entities
                            .iter()
                            .any(|prior| prior.contains(entity) || entity.contains(prior))
                    }) {
                        item.dependencies.push(prior.id);
                        edges += 1;
                        if edges >= MAX_DEPENDENCY_EDGES {
                            break;
                        }
                    }
                }
            }
        }
        let id = item.id;
        state.items.push(item);
        id
    }

    /// Merge tool-touched entities into the hot set: most recent first,
    /// deduplicated, bounded.
    fn merge_hot_entities(hot: &mut Vec<String>, entities: Vec<String>) {
        for entity in entities {
            if let Some(position) = hot.iter().position(|existing| existing == &entity) {
                hot.remove(position);
            }
            hot.insert(0, entity);
        }
        hot.truncate(MAX_HOT_ENTITIES);
    }

    /// A user message reads as a decision when it carries a directive verb
    /// ("use X", "switch to Y", "revert", "drop Z", ...). Explicit, keyword
    /// based, explainable — no learned scoring.
    fn classify_decision(text: &str) -> bool {
        const KEYWORDS: &[&str] = &[
            "use ",
            "switch",
            "revert",
            "drop ",
            "adopt",
            "prefer",
            "instead of",
            "no, ",
            "actually ",
            "replace ",
            "remove ",
        ];
        let lower = text.to_lowercase();
        KEYWORDS.iter().any(|keyword| lower.contains(keyword))
    }

    /// Queue supersession for every non-dropped decision item that shares an
    /// entity with the incoming decision (except `except_id`, the new item).
    fn queue_decision_supersessions(
        state: &mut State,
        content: &str,
        reason_prefix: &str,
        except_id: Option<ContextItemId>,
    ) {
        let entities = extract_entities(content);
        if entities.is_empty() {
            return;
        }
        for item in &mut state.items {
            if Some(item.id) == except_id {
                continue;
            }
            let is_decision =
                item.kind == ContextKind::Decision || item.tags.iter().any(|tag| tag == "decision");
            if !is_decision || item.state == ContextState::Dropped {
                continue;
            }
            if item.tags.iter().any(|tag| tag == "superseded") {
                continue;
            }
            let prior_entities = extract_entities(&item.content);
            if entities.iter().any(|entity| {
                prior_entities
                    .iter()
                    .any(|prior| prior.contains(entity) || entity.contains(prior))
            }) {
                let snippet: String = item.content.chars().take(60).collect();
                state
                    .pending_supersessions
                    .push((item.id, format!("{reason_prefix}: '{snippet}'")));
            }
        }
    }

    /// Queue verification for every non-dropped, unverified error item that
    /// shares an entity with a successful observation.
    fn queue_error_verifications(state: &mut State, content: &str, reason: &str) {
        let entities = extract_entities(content);
        if entities.is_empty() {
            return;
        }
        for item in &mut state.items {
            if item.kind != ContextKind::Error || item.state == ContextState::Dropped {
                continue;
            }
            if item
                .tags
                .iter()
                .any(|tag| tag == "verified-fixed" || tag == "superseded")
            {
                continue;
            }
            let prior_entities = extract_entities(&item.content);
            if entities.iter().any(|entity| {
                prior_entities
                    .iter()
                    .any(|prior| prior.contains(entity) || entity.contains(prior))
            }) {
                state
                    .pending_verifications
                    .push((item.id, reason.to_string()));
            }
        }
    }

    /// Queue recurrence-supersession for every non-dropped error item that
    /// shares an entity with a new failure: one live error per failure site,
    /// the latest one.
    fn queue_error_recurrence(state: &mut State, content: &str, round: u64) {
        let entities = extract_entities(content);
        if entities.is_empty() {
            return;
        }
        for item in &mut state.items {
            if item.kind != ContextKind::Error || item.state == ContextState::Dropped {
                continue;
            }
            if item
                .tags
                .iter()
                .any(|tag| tag == "verified-fixed" || tag == "superseded")
            {
                continue;
            }
            let prior_entities = extract_entities(&item.content);
            if entities.iter().any(|entity| {
                prior_entities
                    .iter()
                    .any(|prior| prior.contains(entity) || entity.contains(prior))
            }) {
                state.pending_supersessions.push((
                    item.id,
                    format!(
                        "recurring failure supersedes earlier error (round {round}, same entities)"
                    ),
                ));
            }
        }
    }
}

#[async_trait::async_trait]
impl ContextEngine for SimpleContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let mut state = self.state.lock().await;
        state.tick += 1;

        match ingress {
            ContextIngress::UserMessage { content } => {
                // A new user message starts a new turn and resets the tool round counter.
                state.turn += 1;
                state.tool_round = 0;
                // The hot entity set is reset to the new instruction: the
                // agent is now working on these names (P4).
                state.hot_entities = extract_entities(&content);
                if let Some(focus) = state.focus.as_mut() {
                    focus.current_query = content.clone();
                    focus.active_entities = extract_entities(&content);
                    focus.generation += 1;
                } else {
                    let mut focus = FocusState::new(content.clone());
                    focus.active_entities = extract_entities(&content);
                    state.focus = Some(focus);
                }

                let mut item = self.make_item(
                    &state,
                    content.clone(),
                    ContextKind::UserMessage,
                    ContextScope::Task,
                    ContextRetention::Working,
                    0.62,
                    Some("user".to_string()),
                );
                if self.config.supersession && Self::classify_decision(&content) {
                    // Decisions are promoted and tracked so later decisions
                    // can supersede them (P4).
                    item.tags.push("decision".into());
                    item.importance = 0.72;
                }
                let item_id = self.push_linked(&mut state, item);

                if self.config.supersession && Self::classify_decision(&content) {
                    let snippet: String = content.chars().take(60).collect();
                    let turn = state.turn;
                    Self::queue_decision_supersessions(
                        &mut state,
                        &content,
                        &format!("superseded by decision at turn {turn}: '{snippet}'"),
                        Some(item_id),
                    );
                }
            }
            ContextIngress::AssistantMessage { content } => {
                let item = self.make_item(
                    &state,
                    content,
                    ContextKind::AssistantMessage,
                    ContextScope::Task,
                    ContextRetention::Working,
                    0.40,
                    Some("assistant".to_string()),
                );
                self.push_linked(&mut state, item);
            }
            ContextIngress::ToolObservation { output } => {
                state.tool_round += 1;
                let mut content = output.model_content;
                if let Some(artifact_ref) = output.artifact_ref {
                    content.push_str("\nartifact: ");
                    content.push_str(&artifact_ref);
                }
                let ok = output.ok;
                let round = state.tool_round;
                if self.config.error_verification && !ok {
                    Self::queue_error_recurrence(&mut state, &content, round);
                }
                if self.config.error_verification && ok {
                    Self::queue_error_verifications(
                        &mut state,
                        &content,
                        &format!("error verified fixed by successful tool result (round {round})"),
                    );
                }
                // Entities the agent actually touched via tools extend the hot
                // set for the rest of this turn (P4).
                if self.config.entity_affinity {
                    Self::merge_hot_entities(&mut state.hot_entities, extract_entities(&content));
                }
                let kind = if ok {
                    ContextKind::ToolObservation
                } else {
                    ContextKind::Error
                };
                // Failed observations persist as Working until verified or
                // superseded (P4); successful observations stay ephemeral and
                // leave after the model consumes them.
                let retention = if ok {
                    ContextRetention::Ephemeral
                } else {
                    ContextRetention::Working
                };
                let item = self.make_item(
                    &state,
                    content,
                    kind,
                    ContextScope::Turn,
                    retention,
                    if ok { 0.58 } else { 0.82 },
                    Some(format!("tool:{}", output.tool_name)),
                );
                self.push_linked(&mut state, item);
            }
            ContextIngress::FocusChanged { mut focus } => {
                focus.generation += 1;
                // A new focus defines the hot set from its own active entities.
                state.hot_entities = focus.active_entities.clone();
                state.focus = Some(focus);
            }
            ContextIngress::Pin { content, kind } => {
                let item = self.make_item(
                    &state,
                    content,
                    kind,
                    ContextScope::Pinned,
                    ContextRetention::Pinned,
                    1.0,
                    Some("explicit-pin".to_string()),
                );
                self.push_linked(&mut state, item);
            }
            ContextIngress::TaskCompleted { task_id, summary } => {
                // Record which task completed; the archiving itself happens in
                // maintain(TaskCompleted) so the transition is observable.
                let completed_task = task_id.or_else(|| state.focus.as_ref().map(|f| f.task_id));
                if let Some(completed_task) = completed_task {
                    state.completed_task_id = Some(completed_task);
                    if state.focus.as_ref().map(|f| f.task_id) == Some(completed_task) {
                        state.focus = None;
                    }
                }
                let item = self.make_item(
                    &state,
                    summary,
                    ContextKind::Summary,
                    ContextScope::Session,
                    ContextRetention::Durable,
                    0.84,
                    Some("task-summary".to_string()),
                );
                self.push_linked(&mut state, item);
            }
        }

        Ok(())
    }

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        let mut state = self.state.lock().await;
        state.tick += 1;
        let now_tick = state.tick;
        let turn = state.turn;
        let focus = state.focus.clone();
        let mut report = ContextMaintenanceReport {
            turn,
            ..ContextMaintenanceReport::default()
        };

        // Task completion: the completed task's working set leaves active
        // attention. Done here (not in ingest) so the archival is recorded as a
        // lifecycle transition.
        if matches!(trigger, ContextMaintenanceTrigger::TaskCompleted)
            && let Some(completed) = state.completed_task_id.take()
        {
            for item in &mut state.items {
                if item.task_id == Some(completed)
                    && item.retention != ContextRetention::Pinned
                    && item.state != ContextState::Dropped
                    && item.state != ContextState::Archived
                {
                    report.archived += 1;
                    report.transitions.push(ContextStateTransition {
                        item_id: item.id,
                        kind: item.kind,
                        scope: item.scope,
                        from: item.state,
                        to: ContextState::Archived,
                        turn,
                        reason: "task completed: working set archived".to_string(),
                    });
                    item.state = ContextState::Archived;
                }
            }
        }

        // Supersession and verification intents recorded by ingest become
        // observable state changes here, with explainable reasons.
        let supersessions = std::mem::take(&mut state.pending_supersessions);
        for (item_id, reason) in supersessions {
            let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) else {
                continue;
            };
            if item.state == ContextState::Dropped {
                continue;
            }
            if item.state != ContextState::Archived {
                report.archived += 1;
                report.transitions.push(ContextStateTransition {
                    item_id: item.id,
                    kind: item.kind,
                    scope: item.scope,
                    from: item.state,
                    to: ContextState::Archived,
                    turn,
                    reason: reason.clone(),
                });
            }
            item.state = ContextState::Archived;
            item.relevance = 0.0;
            item.tags.push("superseded".into());
        }

        let verifications = std::mem::take(&mut state.pending_verifications);
        for (item_id, reason) in verifications {
            let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) else {
                continue;
            };
            if item.state == ContextState::Dropped {
                continue;
            }
            if item.state != ContextState::Archived {
                report.archived += 1;
                report.transitions.push(ContextStateTransition {
                    item_id: item.id,
                    kind: item.kind,
                    scope: item.scope,
                    from: item.state,
                    to: ContextState::Archived,
                    turn,
                    reason: reason.clone(),
                });
            }
            item.state = ContextState::Archived;
            item.relevance = 0.0;
            item.tags.push("verified-fixed".into());
        }

        // Clone the bounded hot set once: `state` is a MutexGuard, so the
        // borrow checker cannot split fields through Deref while the loop
        // mutates items. The set is capped at 24 entries and only changes on
        // ingest, so this is a tiny, semantically identical copy.
        let hot_entities = state.hot_entities.clone();

        for item in &mut state.items {
            let old_state = item.state;

            if item.retention == ContextRetention::Pinned || item.scope == ContextScope::Pinned {
                item.state = ContextState::Active;
                item.relevance = 1.0;
                continue;
            }

            // Superseded decisions and verified-fixed errors never re-enter
            // active attention (P4).
            if item
                .tags
                .iter()
                .any(|tag| tag == "superseded" || tag == "verified-fixed")
            {
                item.state = ContextState::Archived;
                item.relevance = 0.0;
                continue;
            }

            let age = now_tick.saturating_sub(item.created_tick);
            let should_drop_ephemeral = item.retention == ContextRetention::Ephemeral
                && item.scope == ContextScope::Turn
                && matches!(trigger, ContextMaintenanceTrigger::AfterModel)
                && age >= 1;
            let ttl_expired =
                item.retention == ContextRetention::Ephemeral && age > self.config.turn_ttl_ticks;

            let (new_state, reason) = if should_drop_ephemeral {
                (
                    ContextState::Dropped,
                    format!(
                        "ephemeral {:?} observation dropped after model turn {}",
                        item.kind, turn
                    ),
                )
            } else if ttl_expired {
                (
                    ContextState::Dropped,
                    format!(
                        "ephemeral TTL expired (age {age} > {} ticks)",
                        self.config.turn_ttl_ticks
                    ),
                )
            } else {
                let breakdown =
                    score_item_with_breakdown(item, focus.as_ref(), &hot_entities, now_tick);
                item.relevance = breakdown.total.min(1.0);

                // Items belonging to a task other than the active focus (or to
                // a completed task after focus is cleared) must not linger in
                // active attention: cap them at Archived unless the current
                // focus strongly reactivates them.
                let stale_task = match (item.task_id, focus.as_ref()) {
                    (Some(item_task), Some(active)) => item_task != active.task_id,
                    (Some(_), None) => true,
                    _ => false,
                };

                let next = if stale_task && breakdown.total < self.config.active_threshold {
                    ContextState::Archived
                } else if breakdown.total >= self.config.active_threshold {
                    ContextState::Active
                } else if breakdown.total >= self.config.archive_threshold {
                    ContextState::Cooling
                } else if item.retention == ContextRetention::Durable {
                    ContextState::Archived
                } else if age > self.config.turn_ttl_ticks * 4 {
                    ContextState::Dropped
                } else {
                    ContextState::Archived
                };

                let mut reason = transition_reason(
                    old_state,
                    next,
                    &breakdown,
                    self.config.active_threshold,
                    self.config.archive_threshold,
                    age,
                    self.config.turn_ttl_ticks,
                );
                if stale_task
                    && next == ContextState::Archived
                    && old_state != ContextState::Archived
                {
                    reason = format!(
                        "task no longer active: archived (score {:.2} < active threshold {:.2})",
                        breakdown.total, self.config.active_threshold
                    );
                }
                (next, reason)
            };

            item.state = new_state;

            if item.state != old_state {
                match item.state {
                    ContextState::Active => report.promoted += 1,
                    ContextState::Cooling => report.cooled += 1,
                    ContextState::Archived => report.archived += 1,
                    ContextState::Dropped => report.dropped += 1,
                }
                report.transitions.push(ContextStateTransition {
                    item_id: item.id,
                    kind: item.kind,
                    scope: item.scope,
                    from: old_state,
                    to: item.state,
                    turn,
                    reason,
                });
            }
        }

        report.diagnostics = self.diagnostics_locked(&state);
        Ok(report)
    }

    async fn build_snapshot(&self, request: ContextBuildRequest) -> AgentResult<ContextSnapshot> {
        let mut state = self.state.lock().await;
        state.tick += 1;
        let now_tick = state.tick;
        let focus = state.focus.clone();

        let mut candidates: Vec<(usize, ScoreBreakdown, usize)> = state
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.state != ContextState::Dropped
                    && !(item.kind == ContextKind::UserMessage
                        && item.content == request.current_input)
                    // Superseded decisions and verified-fixed errors never
                    // re-enter a model request, whatever their score.
                    && !item
                        .tags
                        .iter()
                        .any(|tag| tag == "superseded" || tag == "verified-fixed")
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
        // A small slice of the budget is reserved for dependency expansion
        // so traceability items can follow the working set without letting the
        // snapshot exceed the budget.
        let total_budget = request.budget_tokens.saturating_sub(fixed_tokens);
        let expansion_reserve = EXPANSION_RESERVE_TOKENS.min(total_budget);
        let mut remaining = total_budget - expansion_reserve;
        let mut selected_indices = Vec::new();
        let mut selections = Vec::new();

        for (index, breakdown, tokens) in candidates {
            let item = &state.items[index];
            if item.state == ContextState::Archived
                && breakdown.total < self.config.active_threshold
            {
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

        // Explicit dependency expansion — pull in dependencies of selected
        // items (skip Dropped and superseded/verified-fixed; Archived items
        // only when they still clear the active threshold), best dependencies
        // first, bounded per snapshot, spending only the reserved slice.
        let mut selected_ids: Vec<ContextItemId> = selections
            .iter()
            .map(|selection| selection.item_id)
            .collect();
        if self.config.dependency_expansion {
            let mut expansion_budget = remaining + expansion_reserve;
            let mut expanded: Vec<(usize, ScoreBreakdown, usize, ContextItemId)> = Vec::new();
            for &index in &selected_indices {
                let item = &state.items[index];
                let dependencies = item.dependencies.clone();
                for dep_id in dependencies {
                    if selected_ids.contains(&dep_id) {
                        continue;
                    }
                    let Some(dep_index) = state.items.iter().position(|i| i.id == dep_id) else {
                        continue;
                    };
                    let dep = &state.items[dep_index];
                    if dep.state == ContextState::Dropped {
                        continue;
                    }
                    if dep
                        .tags
                        .iter()
                        .any(|tag| tag == "superseded" || tag == "verified-fixed")
                    {
                        continue;
                    }
                    let breakdown = score_item_with_breakdown(
                        dep,
                        focus.as_ref(),
                        &state.hot_entities,
                        now_tick,
                    );
                    if dep.state == ContextState::Archived
                        && breakdown.total < self.config.active_threshold
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

        let mut messages = vec![ModelMessage::system(request.system_prompt)];
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
        messages.push(ModelMessage::user(request.current_input));

        let approx_tokens = messages.iter().map(|m| approx_tokens(&m.content)).sum();
        let diagnostics = self.diagnostics_locked(&state);

        Ok(ContextSnapshot {
            messages,
            selected: selections,
            approx_tokens,
            diagnostics,
        })
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        let state = self.state.lock().await;
        Ok(self.diagnostics_locked(&state))
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        let state = self.state.lock().await;
        let mut summaries: Vec<ContextItemSummary> = state
            .items
            .iter()
            .map(|item| ContextItemSummary {
                id: item.id,
                kind: item.kind,
                scope: item.scope,
                state: item.state,
                importance: item.importance,
                relevance: item.relevance,
                created_tick: item.created_tick,
                created_turn: item.created_turn,
                last_access_turn: item.last_access_turn,
                access_count: item.access_count,
                dependencies: item.dependencies.clone(),
                source: item.source.clone(),
            })
            .collect();
        summaries.sort_by_key(|summary| summary.created_tick);
        summaries.truncate(limit);
        Ok(summaries)
    }

    async fn checkpoint(&self) -> AgentResult<Value> {
        let state = self.state.lock().await;
        serde_json::to_value(&*state)
            .map_err(|e| agent_contracts::AgentError::Context(format!("checkpoint serialize: {e}")))
    }

    async fn restore(&self, data: Value) -> AgentResult<()> {
        let mut state = self.state.lock().await;
        *state = serde_json::from_value(data).map_err(|e| {
            agent_contracts::AgentError::Context(format!("checkpoint restore: {e}"))
        })?;
        Ok(())
    }
}

fn transition_reason(
    from: ContextState,
    to: ContextState,
    breakdown: &ScoreBreakdown,
    active_threshold: f32,
    archive_threshold: f32,
    age: u64,
    turn_ttl_ticks: u64,
) -> String {
    match (from, to) {
        (_, ContextState::Active) => format!(
            "reactivated: score {:.2} >= active threshold {active_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Active, ContextState::Cooling) => format!(
            "decayed: score {:.2} below active threshold {active_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Active, ContextState::Archived) => format!(
            "archived: score {:.2} below archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Cooling, ContextState::Archived) => format!(
            "archived: score {:.2} below archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (ContextState::Archived, ContextState::Cooling) => format!(
            "renewed: score {:.2} >= archive threshold {archive_threshold:.2}",
            breakdown.total
        ),
        (_, ContextState::Dropped) => format!(
            "dropped: stale (age {age} > ttl x4 = {})",
            turn_ttl_ticks * 4
        ),
        (from, to) => format!("state {from:?} -> {to:?}"),
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

fn short_id(id: &ContextItemId) -> String {
    id.to_string().chars().take(8).collect()
}

fn approx_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else if !ch.is_whitespace() {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

fn truncate_chars(mut text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    text = text.chars().take(max_chars).collect();
    text.push_str("\n...[truncated by context engine]");
    text
}

#[cfg(test)]
mod tests {
    use agent_contracts::{
        ContextBuildRequest, ContextEngine, ContextIngress, ContextKind, ContextMaintenanceTrigger,
        ContextState, ToolOutput,
    };

    use super::*;

    #[tokio::test]
    async fn successful_observation_is_ephemeral_but_failure_persists_until_verified() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();

        // Round 1: failure — persists (Working) so a later fix can be verified.
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: false,
                    summary: "test failed".into(),
                    model_content: "error in AuthService.rs:42".into(),
                    artifact_ref: Some("artifact://run/test.log".into()),
                    metadata: serde_json::Value::Null,
                },
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterTool)
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        let diagnostics = engine.diagnostics().await.unwrap();
        assert_eq!(
            diagnostics.dropped_items, 0,
            "a failed observation must persist until verified"
        );

        // Round 2: success on the same entity verifies the fix.
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "2".into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "tests passed".into(),
                    model_content: "tests passed in AuthService.rs".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterTool)
            .await
            .unwrap();
        assert!(
            report
                .transitions
                .iter()
                .any(|t| t.reason.contains("verified fixed")),
            "the error must be archived with a verification reason, got: {:?}",
            report
                .transitions
                .iter()
                .map(|t| &t.reason)
                .collect::<Vec<_>>()
        );

        // The successful observation itself stays ephemeral and leaves after
        // the model turn.
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        let after = engine.diagnostics().await.unwrap();
        assert!(after.dropped_items >= 1, "successful observation drops");
        assert!(after.archived_items >= 1, "verified error stays archived");
    }

    #[tokio::test]
    async fn pinned_context_survives_maintenance() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::Pin {
                content: "Never edit generated files".into(),
                kind: ContextKind::Constraint,
            })
            .await
            .unwrap();

        for _ in 0..20 {
            engine
                .maintain(ContextMaintenanceTrigger::AfterModel)
                .await
                .unwrap();
        }

        let snapshot = engine
            .build_snapshot(ContextBuildRequest {
                system_prompt: "test".into(),
                current_input: "continue".into(),
                budget_tokens: 4096,
            })
            .await
            .unwrap();

        assert!(
            snapshot
                .messages
                .iter()
                .any(|m| m.content.contains("Never edit generated files"))
        );
    }

    #[tokio::test]
    async fn maintenance_records_transitions_with_reasons() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "run tests".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "tests ok".into(),
                    model_content: "3 passed, 0 failed".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            })
            .await
            .unwrap();

        // First maintenance (AfterTool) must not drop the fresh observation
        // (the user message may decay to Cooling; that is normal, not a drop).
        let after_tool = engine
            .maintain(ContextMaintenanceTrigger::AfterTool)
            .await
            .unwrap();
        assert!(
            !after_tool
                .transitions
                .iter()
                .any(|t| t.to == ContextState::Dropped),
            "fresh observation must not be dropped at AfterTool: {:?}",
            after_tool.transitions
        );

        // AfterModel with age >= 1 drops the ephemeral turn observation.
        let after_model = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        let drop = after_model
            .transitions
            .iter()
            .find(|t| t.to == ContextState::Dropped);
        assert!(
            drop.is_some(),
            "expected a drop transition, got: {:?}",
            after_model.transitions
        );
        let drop = drop.unwrap();
        assert_eq!(drop.kind, ContextKind::ToolObservation);
        assert_eq!(drop.turn, 1);
        assert!(
            drop.reason.contains("after model turn"),
            "unexpected reason: {}",
            drop.reason
        );
        assert_eq!(after_model.turn, 1);
    }

    #[tokio::test]
    async fn checkpoint_restore_roundtrip() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "refactor AuthService".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::Pin {
                content: "never touch generated files".into(),
                kind: ContextKind::Constraint,
            })
            .await
            .unwrap();

        let before = engine.diagnostics().await.unwrap();
        let snapshot_before = engine
            .build_snapshot(ContextBuildRequest {
                system_prompt: "s".into(),
                current_input: "refactor AuthService".into(),
                budget_tokens: 8192,
            })
            .await
            .unwrap();
        let consumed_ids: Vec<_> = snapshot_before
            .selected
            .iter()
            .map(|selection| selection.item_id)
            .collect();
        assert!(!consumed_ids.is_empty());

        let checkpoint = engine.checkpoint().await.unwrap();

        let restored = SimpleContextEngine::new(SimpleContextConfig::default());
        restored.restore(checkpoint).await.unwrap();

        let after = restored.diagnostics().await.unwrap();
        assert_eq!(before.total_items, after.total_items);
        assert_eq!(before.turn, after.turn);

        // Access counters survived the round-trip: the same items were consumed.
        let summaries = restored.inspect(usize::MAX).await.unwrap();
        for summary in &summaries {
            if consumed_ids.contains(&summary.id) {
                assert!(
                    summary.access_count >= 1,
                    "consumed item lost access count: {:?}",
                    summary
                );
            }
        }

        // The restored engine remains live.
        restored
            .ingest(ContextIngress::UserMessage {
                content: "continue".into(),
            })
            .await
            .unwrap();
        let grown = restored.diagnostics().await.unwrap();
        assert_eq!(grown.total_items, after.total_items + 1);
    }

    #[tokio::test]
    async fn inspect_is_bounded_and_oldest_first() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        for i in 0..5 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("message {i}"),
                })
                .await
                .unwrap();
        }
        let summaries = engine.inspect(3).await.unwrap();
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].created_turn, 1);
        assert_eq!(summaries[2].created_turn, 3);
    }

    #[tokio::test]
    async fn completed_task_working_set_is_archived_and_stays_out() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "refactor auth module".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::TaskCompleted {
                task_id: None,
                summary: "auth refactor done".into(),
            })
            .await
            .unwrap();

        // Archival happens during maintain(TaskCompleted) and is observable.
        let report = engine
            .maintain(ContextMaintenanceTrigger::TaskCompleted)
            .await
            .unwrap();
        let archive = report
            .transitions
            .iter()
            .find(|t| t.to == ContextState::Archived);
        assert!(
            archive.is_some(),
            "expected an archived transition, got: {:?}",
            report.transitions
        );
        assert!(
            archive.unwrap().reason.contains("task completed"),
            "unexpected reason: {}",
            archive.unwrap().reason
        );

        // A new task must not drag the completed task's details back into the
        // working set: they stay Archived (score below active threshold).
        engine
            .ingest(ContextIngress::UserMessage {
                content: "task two: add tests".into(),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::UserInput)
            .await
            .unwrap();
        let snapshot = engine
            .build_snapshot(ContextBuildRequest {
                system_prompt: "s".into(),
                current_input: "task two: add tests".into(),
                budget_tokens: 8192,
            })
            .await
            .unwrap();
        assert!(
            !snapshot
                .messages
                .iter()
                .any(|m| m.content.contains("refactor auth module")),
            "completed task details leaked into the new task's working set"
        );
    }

    #[tokio::test]
    async fn later_decision_supersedes_earlier_decision() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "use TOML for config".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::UserMessage {
                content: "switch to YAML instead of TOML".into(),
            })
            .await
            .unwrap();

        let report = engine
            .maintain(ContextMaintenanceTrigger::UserInput)
            .await
            .unwrap();
        let supersession = report
            .transitions
            .iter()
            .find(|t| t.reason.contains("superseded by decision"));
        assert!(
            supersession.is_some(),
            "the earlier decision must be superseded, got: {:?}",
            report
                .transitions
                .iter()
                .map(|t| &t.reason)
                .collect::<Vec<_>>()
        );

        // The superseded decision never re-enters the working set (the focus
        // goal may still carry its text — the goal is set once and is the
        // task statement, not the superseded item).
        let snapshot = engine
            .build_snapshot(ContextBuildRequest {
                system_prompt: "s".into(),
                current_input: "continue".into(),
                budget_tokens: 8192,
            })
            .await
            .unwrap();
        let working = snapshot
            .messages
            .iter()
            .find(|m| m.content.starts_with("SELECTED WORKING CONTEXT"))
            .map(|m| m.content.as_str())
            .unwrap_or_default();
        assert!(
            !working.contains("use TOML for config"),
            "superseded decision leaked back into the working context"
        );
    }

    #[tokio::test]
    async fn recurring_failure_supersedes_prior_error() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix the build".into(),
            })
            .await
            .unwrap();
        let mut recurrences = 0usize;
        for round in 1..=3 {
            engine
                .ingest(ContextIngress::ToolObservation {
                    output: ToolOutput {
                        call_id: format!("r{round}"),
                        tool_name: "shell.exec".into(),
                        ok: false,
                        summary: format!("round {round} failed"),
                        model_content: "error in Build.kt (module build failed)".into(),
                        artifact_ref: None,
                        metadata: serde_json::Value::Null,
                    },
                })
                .await
                .unwrap();
            let report = engine
                .maintain(ContextMaintenanceTrigger::AfterTool)
                .await
                .unwrap();
            recurrences += report
                .transitions
                .iter()
                .filter(|t| t.reason.contains("recurring failure supersedes"))
                .count();
        }

        // Two of the three failures were superseded by the next recurrence;
        // exactly one error stays live.
        assert_eq!(recurrences, 2, "two earlier errors superseded");

        let items = engine.inspect(usize::MAX).await.unwrap();
        let live_errors = items
            .iter()
            .filter(|item| item.kind == ContextKind::Error && item.state != ContextState::Archived)
            .count();
        assert_eq!(
            live_errors, 1,
            "one live error per failure site, got {live_errors}"
        );
    }

    #[test]
    fn baseline_v0_turns_off_every_p4_policy() {
        let v0 = SimpleContextConfig::baseline_v0();
        assert!(!v0.supersession);
        assert!(!v0.error_verification);
        assert!(!v0.entity_affinity);
        assert!(!v0.dependency_expansion);
        // and the defaults keep them on
        let on = SimpleContextConfig::default();
        assert!(on.supersession && on.error_verification);
        assert!(on.entity_affinity && on.dependency_expansion);
    }

    #[tokio::test]
    async fn hot_entities_follow_user_then_tool_then_reset() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());

        // A user message defines the hot set from its own entities.
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();
        {
            let state = engine.state.lock().await;
            assert!(
                state.hot_entities.contains(&"AuthService.rs".to_string()),
                "user message entities must seed the hot set"
            );
        }

        // A tool observation touching a new file extends it (newest first).
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: "CacheStore.rs is hot now".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            })
            .await
            .unwrap();
        {
            let state = engine.state.lock().await;
            assert_eq!(
                state.hot_entities.first().map(String::as_str),
                Some("CacheStore.rs"),
                "most recently touched entity must lead"
            );
            assert!(state.hot_entities.contains(&"AuthService.rs".to_string()));
        }

        // The next user message resets the hot set.
        engine
            .ingest(ContextIngress::UserMessage {
                content: "unrelated plain words".into(),
            })
            .await
            .unwrap();
        {
            let state = engine.state.lock().await;
            assert!(
                state.hot_entities.is_empty(),
                "a new user message must reset the hot set"
            );
        }
    }

    #[tokio::test]
    async fn ingest_links_items_sharing_entities() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
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
                    model_content: "tests passed in AuthService.rs".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            })
            .await
            .unwrap();

        let summaries = engine.inspect(usize::MAX).await.unwrap();
        let user = summaries
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("user message item");
        let tool = summaries
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("tool observation item");

        assert!(
            user.dependencies.is_empty(),
            "first item has nothing to depend on"
        );
        assert!(
            tool.dependencies.contains(&user.id),
            "the tool observation must depend on the prior user message sharing its entity"
        );
    }

    #[tokio::test]
    async fn dependency_expansion_pulls_in_dependencies_within_reserved_budget() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        {
            let mut state = engine.state.lock().await;
            // A high-value hub and a bulky, low-score dependency of it.
            let hub_id = ContextItemId::new();
            let dep_id = ContextItemId::new();
            let hub = ContextItem {
                id: hub_id,
                task_id: None,
                content: "hub data ".repeat(400), // ~2000 tokens
                kind: ContextKind::UserMessage,
                scope: ContextScope::Task,
                retention: ContextRetention::Working,
                state: ContextState::Active,
                importance: 1.0,
                relevance: 0.5,
                created_tick: 1,
                last_access_tick: 1,
                access_count: 0,
                created_turn: 1,
                last_access_turn: 1,
                dependencies: vec![dep_id],
                tags: Vec::new(),
                source: None,
            };
            let dep = ContextItem {
                id: dep_id,
                task_id: None,
                content: "dependency detail ".repeat(600), // ~10800 chars, ~2700 tokens
                kind: ContextKind::FileObservation,
                scope: ContextScope::Turn,
                retention: ContextRetention::Working,
                state: ContextState::Active,
                importance: 0.0,
                relevance: 0.0,
                created_tick: 2,
                last_access_tick: 2,
                access_count: 0,
                created_turn: 1,
                last_access_turn: 1,
                dependencies: Vec::new(),
                tags: Vec::new(),
                source: None,
            };
            state.items.push(hub);
            state.items.push(dep);
        }

        let snapshot = engine
            .build_snapshot(ContextBuildRequest {
                system_prompt: "s".into(),
                current_input: "go".into(),
                budget_tokens: 4096,
            })
            .await
            .unwrap();

        let expansion = snapshot
            .selected
            .iter()
            .find(|selection| selection.reason.contains("included as dependency of item"));
        assert!(
            expansion.is_some(),
            "the low-score dependency must be pulled in by expansion, got: {:?}",
            snapshot
                .selected
                .iter()
                .map(|selection| &selection.reason)
                .collect::<Vec<_>>()
        );
        assert_eq!(snapshot.selected.len(), 2, "hub + its dependency");
        assert!(
            snapshot.approx_tokens <= 4096,
            "expansion must never blow the budget, got {}",
            snapshot.approx_tokens
        );
    }

    #[tokio::test]
    async fn dependency_expansion_can_be_disabled() {
        let engine = SimpleContextEngine::new(SimpleContextConfig {
            dependency_expansion: false,
            ..SimpleContextConfig::default()
        });
        {
            let mut state = engine.state.lock().await;
            let hub_id = ContextItemId::new();
            let dep_id = ContextItemId::new();
            let hub = ContextItem {
                id: hub_id,
                task_id: None,
                content: "hub data ".repeat(400),
                kind: ContextKind::UserMessage,
                scope: ContextScope::Task,
                retention: ContextRetention::Working,
                state: ContextState::Active,
                importance: 1.0,
                relevance: 0.5,
                created_tick: 1,
                last_access_tick: 1,
                access_count: 0,
                created_turn: 1,
                last_access_turn: 1,
                dependencies: vec![dep_id],
                tags: Vec::new(),
                source: None,
            };
            let dep = ContextItem {
                id: dep_id,
                task_id: None,
                content: "dependency detail ".repeat(1200),
                kind: ContextKind::FileObservation,
                scope: ContextScope::Turn,
                retention: ContextRetention::Working,
                state: ContextState::Active,
                importance: 0.0,
                relevance: 0.0,
                created_tick: 2,
                last_access_tick: 2,
                access_count: 0,
                created_turn: 1,
                last_access_turn: 1,
                dependencies: Vec::new(),
                tags: Vec::new(),
                source: None,
            };
            state.items.push(hub);
            state.items.push(dep);
        }

        let snapshot = engine
            .build_snapshot(ContextBuildRequest {
                system_prompt: "s".into(),
                current_input: "go".into(),
                budget_tokens: 4096,
            })
            .await
            .unwrap();

        assert!(
            !snapshot
                .selected
                .iter()
                .any(|selection| selection.reason.contains("included as dependency")),
            "with dependency_expansion off the dependency must stay out"
        );
        assert_eq!(snapshot.selected.len(), 1, "only the hub is selected");
    }

    #[tokio::test]
    async fn archived_dependency_below_threshold_stays_out() {
        // Expansion must respect the same active-threshold gate as primary
        // selection: an archived dependency with a cold score is not pulled in.
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        {
            let mut state = engine.state.lock().await;
            let hub_id = ContextItemId::new();
            let dep_id = ContextItemId::new();
            let hub = ContextItem {
                id: hub_id,
                task_id: None,
                content: "hub data ".repeat(400),
                kind: ContextKind::UserMessage,
                scope: ContextScope::Task,
                retention: ContextRetention::Working,
                state: ContextState::Active,
                importance: 1.0,
                relevance: 0.5,
                created_tick: 1,
                last_access_tick: 1,
                access_count: 0,
                created_turn: 1,
                last_access_turn: 1,
                dependencies: vec![dep_id],
                tags: Vec::new(),
                source: None,
            };
            let dep = ContextItem {
                id: dep_id,
                task_id: None,
                content: "stale dependency".into(),
                kind: ContextKind::FileObservation,
                scope: ContextScope::Turn,
                retention: ContextRetention::Working,
                state: ContextState::Archived,
                importance: 0.0,
                relevance: 0.0,
                created_tick: 2,
                last_access_tick: 2,
                access_count: 0,
                created_turn: 1,
                last_access_turn: 1,
                dependencies: Vec::new(),
                tags: Vec::new(),
                source: None,
            };
            state.items.push(hub);
            state.items.push(dep);
        }

        let snapshot = engine
            .build_snapshot(ContextBuildRequest {
                system_prompt: "s".into(),
                current_input: "go".into(),
                budget_tokens: 4096,
            })
            .await
            .unwrap();

        assert!(
            !snapshot
                .selected
                .iter()
                .any(|selection| selection.reason.contains("included as dependency")),
            "a cold archived dependency must not be resurrected by expansion"
        );
    }
}
