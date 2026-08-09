use agent_contracts::{
    AgentError, AgentResult, ContextAction, ContextDiagnostics, ContextEngine, ContextGcReport,
    ContextIngress, ContextItem, ContextItemId, ContextItemSummary, ContextKind,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextRetention,
    ContextScope, ContextStateTransition, CoreLabel, FocusState, Label, MaterializedContext,
    ScopeId, ScopeKind, ScopeState,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::checkpoint;
use crate::diagnostics;
use crate::gc::{full, minor, reachability};
use crate::heap;
use crate::index::{dependency, entity};
use crate::item;
use crate::materializer;
use crate::scope;
use crate::store;

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
    /// Run the full GC pass (mark roots, sweep, reversible eviction) when
    /// `ContextEngine::gc` is invoked.
    pub gc_enabled: bool,
    /// Items surviving this many full passes without root reachability are
    /// eviction candidates (the generational dimension of GC).
    pub gc_max_generation: u32,
    /// Cap on the reversible eviction buffer; overflow no longer purges —
    /// items are externalized to the context store instead.
    pub gc_buffer_capacity: usize,
    /// Max items reactivated per GC pass (newest first).
    pub gc_reactivate_per_pass: usize,
    /// Directory of the external context store: eviction-buffer overflow
    /// writes full items here and keeps only a lightweight `ContextRef`
    /// entry. The composition root injects `workspace.state_dir()/context-store`
    /// so runtime state never scatters under a CWD; `None` falls back to an
    /// OS temp dir scoped to this process (never a CWD-relative path).
    pub context_store_dir: Option<std::path::PathBuf>,
    /// Full GC passes an externalized (`Cold`) entry may sit in memory
    /// before it ages to `External` (only the store retains it). The unit
    /// is *generations*: only a full GC increments `State::gc_epoch`, so
    /// the TTL counts real passes, not the tick counter (which also grows
    /// on ingest/maintain/materialize).
    pub gc_external_ttl_generations: u32,
    /// Storage GC only deletes store entries whose semantic lifecycle ended
    /// at least this many ticks ago and that nothing references.
    pub storage_ttl_ticks: u64,
    /// Cap on how many items may carry `keep_alive` at once. Model hints are
    /// hints: a runaway `gc_hint keep=true` must not root the whole heap.
    pub max_keep_alive_items: usize,
    /// Cap on lease turns per directive. A lease is bounded, not permanent —
    /// the model cannot lease an item "forever" with one call.
    pub max_lease_turns: u32,
    /// Cap on leased items per task (count). A task cannot lease its whole
    /// history into roots.
    pub max_leased_items_per_task: usize,
    /// Cap on total content tokens leased per task. Count + tokens together
    /// bound both the number and the weight of model-protected items.
    pub max_leased_tokens_per_task: usize,
    /// Cap on `context.admit` calls per turn: admit re-enters items into
    /// the working set, so a runaway admit loop must not grow the heap
    /// without bound between GC passes.
    pub max_admits_per_turn: usize,
    /// Cap on `context.derive` calls per turn: each derive persists a new
    /// observation, so the model cannot mint derived items without bound.
    pub max_derived_items_per_turn: usize,
    /// Minimum token overlap between a new user instruction and the current
    /// episode's query for the instruction to count as a continuation of the
    /// same episode. Below this, and when the message carries real
    /// information (entities or length), the focus episode rotates: durable
    /// outcomes promote to the task scope, ordinary dialogue is evicted.
    pub episode_rotate_threshold: f32,
    /// Hard cap on user turns per focus episode. Even when every message is
    /// a semantic continuation, the episode rotates at this budget so a
    /// pathological single-episode run cannot grow the working set without
    /// bound.
    pub episode_max_user_turns: usize,
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
            gc_enabled: true,
            gc_max_generation: 3,
            gc_buffer_capacity: 256,
            gc_reactivate_per_pass: 8,
            context_store_dir: None,
            gc_external_ttl_generations: 4,
            storage_ttl_ticks: 40,
            max_keep_alive_items: 16,
            max_lease_turns: 32,
            max_leased_items_per_task: 16,
            max_leased_tokens_per_task: 4096,
            max_admits_per_turn: 8,
            max_derived_items_per_turn: 8,
            episode_rotate_threshold: 0.15,
            episode_max_user_turns: 500,
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

/// Mutable runtime state of the engine, kept behind a lock. The heap (with
/// its secondary indexes bound to it), the focus, the hot-entity set and
/// the pending lifecycle intents all live here; modules read and mutate it
/// through `pub(crate)` access.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    pub(crate) tick: u64,
    pub(crate) turn: u64,
    pub(crate) tool_round: u64,
    pub(crate) focus: Option<FocusState>,
    /// The heap owns its slot/entity/scope indexes: structural mutations
    /// go through `ContextHeap` methods so the indexes cannot drift.
    pub(crate) items: crate::index::heap::ContextHeap,
    /// (item_id, by_id, reason) queued by ingest for superseded decisions,
    /// drained by maintenance so the resulting semantic state change is
    /// recorded as a lifecycle transition.
    #[serde(default)]
    pub(crate) pending_supersessions: Vec<(ContextItemId, ContextItemId, String)>,
    /// (item_id, by_id, reason) queued by ingest for verified-fixed errors.
    #[serde(default)]
    pub(crate) pending_verifications: Vec<(ContextItemId, ContextItemId, String)>,
    /// Lifecycle transitions already applied by ingest (focus episode
    /// rotation). They are surfaced by the next maintenance report so the
    /// rotation is observable as bounded runtime events.
    #[serde(default)]
    pub(crate) pending_ingest_transitions: Vec<ContextStateTransition>,
    /// Entities named by the last user message or touched by recent tool
    /// observations. Reset on user message / focus change, extended by tools.
    #[serde(default)]
    pub(crate) hot_entities: Vec<String>,
    /// Runtime scope tree: one session scope, one task scope per task, one
    /// focus scope per task while it runs, one tool scope per tool call.
    /// The tree owns its id index: `push`/`by_id`/`index_of` keep lookups
    /// O(1) and structural mutations cannot drift the index.
    #[serde(default)]
    pub(crate) scopes: crate::scope_tree::ScopeTree,
    /// Deepest scope currently receiving attention (tool > focus > task).
    #[serde(default)]
    pub(crate) active_scope_id: Option<ScopeId>,
    /// Scopes queued for close by ingest (task completion, tool result
    /// consumed); drained by maintenance so promotion/eviction is recorded.
    #[serde(default)]
    pub(crate) pending_closed_scopes: Vec<ScopeId>,
    /// Items evicted by the full GC pass. Bounded by
    /// `gc_buffer_capacity`; eviction is reversible — items re-enter the
    /// heap when they become roots again. Overflow externalizes to the
    /// context store (never purged).
    #[serde(default)]
    pub(crate) eviction_buffer: Vec<ContextItem>,
    /// The external context map: lightweight entries for items whose content
    /// lives in the context store. `Cold` entries can still be recalled by
    /// hot-entity matches; `External` entries only exist as references. The
    /// map owns its id/entity indexes: structural mutations (push, retain,
    /// replace) go through `ExternalMap` methods so the indexes cannot drift.
    #[serde(default)]
    pub(crate) external: crate::index::external::ExternalMap,
    /// Counts full GC passes only. External-entry aging (Cold -> External)
    /// and TTLs compare this epoch, never the tick counter — the tick also
    /// advances on ingest/maintain/materialize, so a pass-based TTL must
    /// not drift with unrelated runtime activity.
    #[serde(default)]
    pub(crate) gc_epoch: u64,
    /// Cumulative GC counters, so diagnostics explain a run's eviction and
    /// reactivation behavior without replaying every report.
    #[serde(default)]
    pub(crate) gc_evicted_total: u64,
    #[serde(default)]
    pub(crate) gc_reactivated_total: u64,
    #[serde(default)]
    pub(crate) gc_externalized_total: u64,
    #[serde(default)]
    pub(crate) gc_storage_deleted_total: u64,
    /// Directives counted against the per-turn admit cap. Reset by the
    /// next user message (turn boundary); a turn whose admits are refused
    /// keeps the count so the model learns the cap from refusals.
    #[serde(default)]
    pub(crate) admits_this_turn: usize,
    /// Directives counted against the per-turn derive cap (same lifecycle
    /// as `admits_this_turn`).
    #[serde(default)]
    pub(crate) derives_this_turn: usize,
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
                // Per-turn directive quotas reset at the turn boundary: the
                // admit/derive caps are per user turn, not per process run.
                state.admits_this_turn = 0;
                state.derives_this_turn = 0;
                // Episode rotation: the working set is bounded by the current
                // episode plus unresolved semantic state, not by task turns.
                // A new instruction that is semantically distant from the
                // current episode (below the token-overlap threshold, and
                // informative enough to be a phase change rather than a
                // continuation token), or an episode that exhausted its turn
                // budget, closes the focus episode: durable outcomes promote
                // to the task scope, ordinary dialogue leaves the working
                // set. The transitions are applied here and surfaced by the
                // next maintenance report.
                if needs_episode_rotation(&state, &self.config, &content) {
                    let transitions = scope::close_focus_episode(&mut state);
                    state.pending_ingest_transitions.extend(transitions);
                }
                state.hot_entities = entity::extract_entities(&content);
                if let Some(focus) = state.focus.as_mut() {
                    focus.current_query = content.clone();
                    focus.active_entities = entity::extract_entities(&content);
                    focus.generation += 1;
                }
                // A user message with no focus is a session-level message:
                // the engine never mints a `TaskId` (task identity is
                // runtime-owned, established via `FocusChanged`), so no
                // focus is invented here — the item lands in the session
                // scope and stays selectable while focus is absent.
                let has_focus = state.focus.is_some();
                // The user message opens (or touches) the task and focus
                // scopes of the current work; without a focus this falls
                // back to the session scope.
                scope::open_focus_scope(&mut state);

                let mut item = item::make_item(
                    &state,
                    &self.config,
                    content.clone(),
                    ContextKind::UserMessage,
                    if has_focus {
                        ContextScope::Task
                    } else {
                        ContextScope::Session
                    },
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
                        item_id,
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
                let mut item = item::make_item(
                    &state,
                    &self.config,
                    content.clone(),
                    kind,
                    ContextScope::Turn,
                    retention,
                    if ok { 0.58 } else { 0.82 },
                    Some(format!("tool:{}", output.tool_name)),
                );
                // The runtime opened the tool scope at tool start; the
                // observation is tagged with that frame even though it is
                // persisted at turn end.
                if let Some(tool_scope_id) = scope_id {
                    item.scope_id = Some(tool_scope_id);
                }
                // The observation itself is the `by` of the intents it
                // queues: verification (success) or recurrence supersession
                // (failure). It must exist with its id before queueing so
                // the semantic state can name it, but it is pushed to the
                // heap only after queueing so intents never see it.
                let observation_id = item.id;
                if self.config.error_verification && !ok {
                    reachability::queue_error_recurrence(
                        &mut state,
                        &content,
                        round,
                        observation_id,
                    );
                }
                if self.config.error_verification && ok {
                    reachability::queue_error_verifications(
                        &mut state,
                        &content,
                        &format!("error verified fixed by successful tool result (round {round})"),
                        observation_id,
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
            ContextIngress::FocusCleared => {
                // Suspend (not complete) the active task: its scopes stay
                // open so a later FocusChanged with the same task id
                // resumes them; focus returns to None until then.
                let task_id = state.focus.as_ref().map(|focus| focus.task_id);
                state.focus = None;
                state.hot_entities.clear();
                if let Some(task_id) = task_id {
                    for scope in state.scopes.iter_mut() {
                        if scope.task_id == Some(task_id) && scope.state == ScopeState::Active {
                            scope.state = ScopeState::Suspended;
                        }
                    }
                }
                if let Some(session) = state
                    .scopes
                    .iter()
                    .find(|scope| scope.kind == ScopeKind::Session)
                {
                    state.active_scope_id = Some(session.id);
                }
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
                    // Model hints are per-task: when the task completes its
                    // keep_alive and lease protections expire in *every* body
                    // location, so a completed task cannot keep rooting items
                    // forever. A keep-alive item is normally a GC
                    // root and stays in the heap, but a warm-buffer item from
                    // an older checkpoint must not retain the protection.
                    for item in &mut state.items {
                        if item.task_id == Some(completed_task) {
                            item.keep_alive = false;
                            item.lease_until_turn = None;
                        }
                    }
                    for item in &mut state.eviction_buffer {
                        if item.task_id == Some(completed_task) {
                            item.keep_alive = false;
                            item.lease_until_turn = None;
                        }
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
            ContextIngress::ContextDirective { action } => {
                // Admit of an externalized item reads its content back from
                // the context store. Plan under the lock, read outside it,
                // re-apply under a fresh lock — the state lock is never held
                // across disk IO (same phases as the GC's store step).
                let read_plan = match &action {
                    ContextAction::Admit { item_id, .. } => {
                        match crate::directive::plan_admit(&state, *item_id) {
                            crate::directive::AdmitPlan::Refused(reason) => {
                                return Err(AgentError::InvalidRequest(reason));
                            }
                            crate::directive::AdmitPlan::ReadExternal(id) => Some(id),
                            crate::directive::AdmitPlan::InMemory
                            | crate::directive::AdmitPlan::Missing => None,
                        }
                    }
                    _ => None,
                };
                let external_read = match read_plan {
                    Some(item_id) => {
                        let dir = crate::store::store_dir(&self.config);
                        Some((item_id, crate::store::read_item_async(&dir, item_id).await))
                    }
                    None => None,
                };
                if let Some(reason) =
                    apply_directive(&mut state, &self.config, action, external_read)
                {
                    // A quota refused the directive: surface it so the model
                    // (which believes the hint/lease was granted) learns it
                    // was not.
                    return Err(AgentError::InvalidRequest(reason));
                }
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

    async fn gc(&self) -> AgentResult<ContextGcReport> {
        // Three phases so the state lock is not held across disk IO:
        // 1. plan under the lock (mark/sweep/reactivate/age — in memory);
        // 2. store writes and recall reads without the lock;
        // 3. commit under a fresh lock (external entries, recalled items,
        //    failed-write buffer returns, diagnostics).
        let mut state = self.state.lock().await;
        state.tick += 1;
        let now_tick = state.tick;
        let Some(mut plan) = full::plan_full_gc(&mut state, &self.config, now_tick) else {
            return Ok(ContextGcReport {
                resident: state.items.len(),
                diagnostics: diagnostics::compute(&state),
                ..ContextGcReport::default()
            });
        };
        drop(state);
        let io = full::run_store_io(&self.config, &mut plan).await;
        let mut state = self.state.lock().await;
        Ok(full::commit_full_gc(&mut state, now_tick, plan, io))
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

    async fn search_external(
        &self,
        query: agent_contracts::ContextSearchQuery,
    ) -> AgentResult<Vec<agent_contracts::ExternalizedContext>> {
        let state = self.state.lock().await;
        Ok(crate::store::search_entries(&state.external, &query))
    }

    async fn inspect_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<agent_contracts::ExternalizedContext>> {
        let state = self.state.lock().await;
        Ok(state
            .external
            .get(item_id)
            .filter(|entry| crate::store::externally_retrievable(entry))
            .cloned())
    }

    async fn fetch_external(&self, item_id: ContextItemId) -> AgentResult<Option<ContextItem>> {
        // Membership and the access-stamp inputs under the lock; the disk
        // read happens *outside* it — sync store IO must never stall the
        // context hot path.
        let dir = crate::store::store_dir(&self.config);
        let (retrievable, now_tick, gc_epoch) = {
            let state = self.state.lock().await;
            // O(1) id-index membership instead of a linear scan: the
            // model's retrieval loop calls this per item.
            let retrievable = state
                .external
                .get(item_id)
                .is_some_and(crate::store::externally_retrievable);
            (retrievable, state.tick, state.gc_epoch)
        };
        if !retrievable {
            return Ok(None);
        }
        let item = crate::store::read_item_async(&dir, item_id).await;
        if item.is_some() {
            // A deliberate pull stamps recency and the GC generation on the
            // entry, so ranking and Cold -> External aging stay honest — the
            // item was used, it is not an untouched stale reference.
            let mut state = self.state.lock().await;
            if let Some(entry) = state.external.get_mut(item_id) {
                // Re-check after IO: a concurrent lifecycle transition may
                // have made the entry terminal while the file was read.
                if !crate::store::externally_retrievable(entry) {
                    return Ok(None);
                }
                entry.last_access_tick = now_tick;
                entry.last_access_gc_epoch = Some(gc_epoch);
            } else {
                return Ok(None);
            }
        }
        Ok(item)
    }

    async fn checkpoint(&self) -> AgentResult<Value> {
        let state = self.state.lock().await;
        checkpoint::serialize(&state)
    }

    async fn restore(&self, data: Value) -> AgentResult<()> {
        let mut state = self.state.lock().await;
        *state = checkpoint::deserialize(data)?;
        // Old checkpoints predate the entity signature cache; backfill it
        // once so restored items keep scoring and dependency behavior. The
        // heap method re-indexes each backfilled signature, so the entity
        // index stays consistent without a wholesale rebuild.
        let backfills: Vec<(usize, Vec<String>)> = state
            .items
            .items_mut()
            .iter_mut()
            .enumerate()
            .filter(|(_, item)| item.entities.is_empty())
            .map(|(index, item)| (index, entity::extract_entities(&item.content)))
            .collect();
        for (index, entities) in backfills {
            state.items.update_entities(index, entities);
        }
        // Pre-split checkpoints expressed semantic death as lifecycle
        // labels; migrate them to the SemanticState dimension so GC and
        // the materializer treat restored items like live ones. Semantic
        // state is not indexed, so the raw mutable slice is safe here.
        for item in state.items.items_mut() {
            if item.semantic.is_live() {
                if item
                    .tags
                    .iter()
                    .any(|tag| tag.is_lifecycle(agent_contracts::LifecycleLabel::Superseded))
                {
                    item.semantic = agent_contracts::SemanticState::Superseded { by: None };
                } else if item
                    .tags
                    .iter()
                    .any(|tag| tag.is_lifecycle(agent_contracts::LifecycleLabel::VerifiedFixed))
                {
                    item.semantic = agent_contracts::SemanticState::VerifiedFixed { by: None };
                }
            }
        }
        Ok(())
    }

    async fn storage_gc(&self) -> AgentResult<agent_contracts::StorageGcReport> {
        // Plan under the lock, delete outside it, commit under a fresh
        // lock — the state lock is never held across disk IO.
        let plan = {
            let mut state = self.state.lock().await;
            state.tick += 1;
            let now_tick = state.tick;
            store::plan_storage_gc(&state, &self.config, now_tick)
        };
        let dir = store::store_dir(&self.config);
        let io = store::run_storage_io(&dir, &plan).await;
        let mut state = self.state.lock().await;
        Ok(store::commit_storage_gc(&mut state, plan, io))
    }
}

/// Apply one model/operator context directive to the current state. Every
/// directive targets an existing item — in the heap or the reversible
/// eviction buffer (a hint/lease on an evicted item is what brings it
/// back on the next GC pass); a stale `item_id` (already externalized or
/// superseded) is a silent no-op. GC reads the resulting fields, so every
/// "kept because ..." is explainable in the eviction reasons.
///
/// Returns `Some(reason)` when the directive was refused by a quota:
/// `keep_alive` and leases are bounded so the model cannot root the whole
/// heap. A refused directive leaves the item unchanged — the caller
/// surfaces the reason to the model.
/// Whether the next user message should rotate the focus episode. Two
/// signals, both explainable:
/// - a semantic boundary: the new instruction shares almost no tokens with
///   the current episode's query AND carries real information (entities or
///   enough length) — a bare continuation token ("continue", "ok", "next")
///   does not rotate;
/// - the turn budget: even perfectly related instructions rotate once the
///   episode exceeded `episode_max_user_turns` user turns.
fn needs_episode_rotation(state: &State, config: &SimpleContextConfig, content: &str) -> bool {
    let Some(focus) = state.focus.as_ref() else {
        return false;
    };
    if focus.generation >= config.episode_max_user_turns as u64 {
        return true;
    }
    let overlap = crate::policy::lexical_overlap(content, &focus.current_query);
    // Continuation tokens carry no entities and are short; only a message
    // with real entities or a genuinely long body can signal a phase
    // change. The overlap check already covers entity-sharing continuations
    // ("keep fixing AuthService.rs" shares the entity, so overlap is high).
    let informative =
        !entity::extract_entities(content).is_empty() || content.chars().count() >= 12;
    overlap < config.episode_rotate_threshold && informative
}

fn apply_directive(
    state: &mut State,
    config: &SimpleContextConfig,
    action: ContextAction,
    external_read: Option<(ContextItemId, Option<ContextItem>)>,
) -> Option<String> {
    let target_id = directive_item_id(&action);

    // Admit and Derive have their own quota checks and mutation logic (the
    // admit path may need the store read planned by `ingest`); dispatch
    // them before the in-memory directive machinery.
    match &action {
        ContextAction::Admit { item_id, reason } => {
            return crate::directive::apply_admit(state, config, *item_id, reason, external_read);
        }
        ContextAction::Derive { item_id, fact, .. } => {
            return crate::directive::apply_derive(state, config, *item_id, fact.clone());
        }
        _ => {}
    }

    // Quota checks run on read-only views first; the mutation happens after,
    // so the checks never contend with the mutable borrow.
    let refusal = match &action {
        // `keep=false` always applies (releasing cannot exceed a cap);
        // `keep=true` is bounded by the keep-alive quota.
        ContextAction::GcHint {
            keep_alive: true, ..
        } => {
            // Quotas are global across body locations: a keep_alive item in
            // the warm buffer still consumes the cap.
            let kept = state
                .items
                .iter()
                .chain(&state.eviction_buffer)
                .filter(|item| item.keep_alive)
                .count();
            (kept >= config.max_keep_alive_items).then(|| {
                format!(
                    "gc_hint refused: {kept} items are already keep_alive (cap {})",
                    config.max_keep_alive_items
                )
            })
        }
        ContextAction::Lease { .. } => {
            let target = state
                .items
                .iter()
                .find(|item| item.id == target_id)
                .or_else(|| {
                    state
                        .eviction_buffer
                        .iter()
                        .find(|item| item.id == target_id)
                });
            match target {
                // Stale target: silent no-op, same as the mutation path.
                None => None,
                Some(item) => {
                    // A lease is bounded per directive and per task: the
                    // model cannot lease an item forever, nor lease a task's
                    // whole history into roots.
                    let task = item.task_id;
                    // Renewing an item that is already leased adds no new
                    // leased item or tokens, so it never trips the quota.
                    let already_leased = item
                        .lease_until_turn
                        .is_some_and(|until| until >= state.turn);
                    // Leased-item accounting is global across body locations:
                    // a leased item in the warm buffer still counts against
                    // the task cap.
                    let (leased, leased_tokens) = state
                        .items
                        .iter()
                        .chain(&state.eviction_buffer)
                        .filter(|other| {
                            other
                                .lease_until_turn
                                .is_some_and(|until| until >= state.turn)
                                && other.task_id == task
                        })
                        .fold((0usize, 0usize), |(count, tokens), other| {
                            (
                                count + 1,
                                tokens + crate::item::approx_tokens(&other.content),
                            )
                        });
                    let added = usize::from(!already_leased);
                    let added_tokens = if already_leased {
                        0
                    } else {
                        crate::item::approx_tokens(&item.content)
                    };
                    if leased.saturating_add(added) > config.max_leased_items_per_task {
                        Some(format!(
                            "lease refused: task would lease {} items (cap {})",
                            leased.saturating_add(added),
                            config.max_leased_items_per_task
                        ))
                    } else if leased_tokens.saturating_add(added_tokens)
                        > config.max_leased_tokens_per_task
                    {
                        Some(format!(
                            "lease refused: task would lease {} tokens (cap {})",
                            leased_tokens.saturating_add(added_tokens),
                            config.max_leased_tokens_per_task
                        ))
                    } else {
                        None
                    }
                }
            }
        }
        _ => None,
    };
    if let Some(reason) = refusal {
        return Some(reason);
    }

    let mut target = state
        .items
        .iter_mut()
        .chain(state.eviction_buffer.iter_mut())
        .find(|item| item.id == target_id);
    if let Some(item) = target.as_mut() {
        match action {
            ContextAction::GcHint { keep_alive, .. } => {
                item.keep_alive = keep_alive;
            }
            ContextAction::Tag { tag, .. } => {
                let label = Label::extension(tag);
                if !item.tags.contains(&label) {
                    item.tags.push(label);
                }
            }
            ContextAction::Lease { turns, .. } => {
                let turns = turns.min(config.max_lease_turns);
                item.lease_until_turn = Some(state.turn.saturating_add(turns as u64));
            }
            // The runtime owns the GC pass; `context.collect` never arrives
            // as an ingest directive (the actor calls `ContextEngine::gc`).
            ContextAction::Collect => {}
            // Admit/Derive are dispatched above and never reach the
            // in-memory directive loop.
            ContextAction::Admit { .. } | ContextAction::Derive { .. } => unreachable!(),
        }
    }
    None
}

fn directive_item_id(action: &ContextAction) -> ContextItemId {
    match action {
        ContextAction::GcHint { item_id, .. }
        | ContextAction::Tag { item_id, .. }
        | ContextAction::Lease { item_id, .. }
        | ContextAction::Admit { item_id, .. }
        | ContextAction::Derive { item_id, .. } => *item_id,
        ContextAction::Collect => ContextItemId::new(),
    }
}
