use agent_contracts::{
    AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItem, ContextItemId,
    ContextItemSummary, ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, ContextRetention, ContextScope, ContextStateTransition, CoreLabel, FocusState,
    Label, MaterializedContext, Scope, ScopeId, ScopeKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::checkpoint;
use crate::diagnostics;
use crate::gc::{minor, reachability};
use crate::heap;
use crate::index::{dependency, entity};
use crate::item;
use crate::materializer;
use crate::scope;

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
    /// affinity, no dependency graph. Kept for A/B/C comparison so the policy
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

/// Mutable runtime state of the engine, kept behind a lock. The heap, the
/// focus, the hot-entity set and the pending lifecycle intents all live here;
/// modules read and mutate it through `pub(crate)` access.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    pub(crate) tick: u64,
    pub(crate) turn: u64,
    pub(crate) tool_round: u64,
    pub(crate) focus: Option<FocusState>,
    pub(crate) items: Vec<ContextItem>,
    /// (item_id, reason) queued by ingest, drained by maintenance so the
    /// resulting state change is recorded as a lifecycle transition.
    #[serde(default)]
    pub(crate) pending_supersessions: Vec<(ContextItemId, String)>,
    /// (item_id, reason) queued by ingest for verified-fixed errors.
    #[serde(default)]
    pub(crate) pending_verifications: Vec<(ContextItemId, String)>,
    /// Entities named by the last user message or touched by recent tool
    /// observations. Reset on user message / focus change, extended by tools.
    #[serde(default)]
    pub(crate) hot_entities: Vec<String>,
    /// Runtime scope tree: one session scope, one task scope per task, one
    /// focus scope per task while it runs, one tool scope per tool call.
    #[serde(default)]
    pub(crate) scopes: Vec<Scope>,
    /// Deepest scope currently receiving attention (tool > focus > task).
    #[serde(default)]
    pub(crate) active_scope_id: Option<ScopeId>,
    /// Scopes queued for close by ingest (task completion, tool result
    /// consumed); drained by maintenance so promotion/eviction is recorded.
    #[serde(default)]
    pub(crate) pending_closed_scopes: Vec<ScopeId>,
}

pub struct SimpleContextEngine {
    pub(crate) config: SimpleContextConfig,
    pub(crate) state: Mutex<State>,
}

impl SimpleContextEngine {
    pub fn new(config: SimpleContextConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State::default()),
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
                // A new user message starts a new turn and resets the tool
                // round counter; the hot entity set is reset to the new
                // instruction.
                state.turn += 1;
                state.tool_round = 0;
                state.hot_entities = entity::extract_entities(&content);
                if let Some(focus) = state.focus.as_mut() {
                    focus.current_query = content.clone();
                    focus.active_entities = entity::extract_entities(&content);
                    focus.generation += 1;
                } else {
                    let mut focus = FocusState::new(content.clone());
                    focus.active_entities = entity::extract_entities(&content);
                    state.focus = Some(focus);
                }
                // The user message opens (or touches) the task and focus
                // scopes of the current work.
                scope::open_focus_scope(&mut state);

                let mut item = item::make_item(
                    &state,
                    &self.config,
                    content.clone(),
                    ContextKind::UserMessage,
                    ContextScope::Task,
                    ContextRetention::Working,
                    0.62,
                    Some("user".to_string()),
                );
                if self.config.supersession && reachability::classify_decision(&content) {
                    // Decisions are promoted and tracked so later decisions
                    // can supersede them.
                    item.tags.push(Label::core(CoreLabel::Decision));
                    item.importance = 0.72;
                }
                let item_id = dependency::push_linked(&mut state, &self.config, item);

                if self.config.supersession && reachability::classify_decision(&content) {
                    let snippet: String = content.chars().take(60).collect();
                    let turn = state.turn;
                    reachability::queue_decision_supersessions(
                        &mut state,
                        &content,
                        &format!("superseded by decision at turn {turn}: '{snippet}'"),
                        Some(item_id),
                    );
                }
            }
            ContextIngress::AssistantMessage { content } => {
                let item = item::make_item(
                    &state,
                    &self.config,
                    content,
                    ContextKind::AssistantMessage,
                    ContextScope::Task,
                    ContextRetention::Working,
                    0.40,
                    Some("assistant".to_string()),
                );
                dependency::push_linked(&mut state, &self.config, item);
            }
            ContextIngress::ToolObservation { output, scope_id } => {
                state.tool_round += 1;
                let mut content = output.model_content;
                if let Some(artifact_ref) = output.artifact_ref {
                    content.push_str("\nartifact: ");
                    content.push_str(&artifact_ref);
                }
                let ok = output.ok;
                let round = state.tool_round;
                if self.config.error_verification && !ok {
                    reachability::queue_error_recurrence(&mut state, &content, round);
                }
                if self.config.error_verification && ok {
                    reachability::queue_error_verifications(
                        &mut state,
                        &content,
                        &format!("error verified fixed by successful tool result (round {round})"),
                    );
                }
                // Entities the agent actually touched via tools extend the
                // hot set for the rest of this turn.
                if self.config.entity_affinity {
                    entity::merge_hot_entities(
                        &mut state.hot_entities,
                        entity::extract_entities(&content),
                    );
                }
                let kind = if ok {
                    ContextKind::ToolObservation
                } else {
                    ContextKind::Error
                };
                // Failed observations persist as Working until verified or
                // superseded; successful observations stay ephemeral and
                // leave after the model consumes them.
                let retention = if ok {
                    ContextRetention::Ephemeral
                } else {
                    ContextRetention::Working
                };
                let item = item::make_item(
                    &state,
                    &self.config,
                    content,
                    kind,
                    ContextScope::Turn,
                    retention,
                    if ok { 0.58 } else { 0.82 },
                    Some(format!("tool:{}", output.tool_name)),
                );
                // The runtime opened the tool scope at tool start; the
                // observation is tagged with that frame even though it is
                // persisted at turn end.
                let mut item = item;
                if let Some(tool_scope_id) = scope_id {
                    item.scope_id = Some(tool_scope_id);
                }
                dependency::push_linked(&mut state, &self.config, item);
            }
            ContextIngress::FocusChanged { mut focus } => {
                focus.generation += 1;
                // A new focus defines the hot set from its own active entities.
                state.hot_entities = focus.active_entities.clone();
                state.focus = Some(focus);
                // The focus (and its task) scope opens or reactivates.
                scope::open_focus_scope(&mut state);
            }
            ContextIngress::Pin { content, kind } => {
                // A pin is session-level: it guarantees the session scope
                // exists even when no task has started yet.
                scope::ensure_session(&mut state);
                let item = item::make_item(
                    &state,
                    &self.config,
                    content,
                    kind,
                    ContextScope::Pinned,
                    ContextRetention::Pinned,
                    1.0,
                    Some("explicit-pin".to_string()),
                );
                dependency::push_linked(&mut state, &self.config, item);
            }
            ContextIngress::TaskCompleted { task_id, summary } => {
                // Record which task completed; the scope close (promotion of
                // durable outcomes, eviction of the working set) happens in
                // maintain(TaskCompleted) so it is observable.
                let completed_task = task_id.or_else(|| state.focus.as_ref().map(|f| f.task_id));
                if let Some(completed_task) = completed_task {
                    if state.focus.as_ref().map(|f| f.task_id) == Some(completed_task) {
                        state.focus = None;
                    }
                    scope::queue_task_scope_close(&mut state, completed_task);
                }
                let item = item::make_item(
                    &state,
                    &self.config,
                    summary,
                    ContextKind::Summary,
                    ContextScope::Session,
                    ContextRetention::Durable,
                    0.84,
                    Some("task-summary".to_string()),
                );
                dependency::push_linked(&mut state, &self.config, item);
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
        Ok(minor::run_minor(
            &mut state,
            &self.config,
            trigger,
            now_tick,
            turn,
        ))
    }

    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        let mut state = self.state.lock().await;
        state.tick += 1;
        Ok(materializer::materialize(&mut state, &self.config, &query))
    }

    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        let mut state = self.state.lock().await;
        state.tick += 1;
        Ok(scope::open_scope(&mut state, kind, parent))
    }

    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        let mut state = self.state.lock().await;
        state.tick += 1;
        Ok(scope::close_scope(&mut state, scope_id))
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        let state = self.state.lock().await;
        Ok(diagnostics::compute(&state))
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        let state = self.state.lock().await;
        let mut summaries = heap::to_summaries(&state.items);
        summaries.sort_by_key(|summary| summary.created_tick);
        summaries.truncate(limit);
        Ok(summaries)
    }

    async fn checkpoint(&self) -> AgentResult<Value> {
        let state = self.state.lock().await;
        checkpoint::serialize(&state)
    }

    async fn restore(&self, data: Value) -> AgentResult<()> {
        let mut state = self.state.lock().await;
        *state = checkpoint::deserialize(data)?;
        Ok(())
    }
}
