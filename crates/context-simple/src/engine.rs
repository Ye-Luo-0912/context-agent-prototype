use agent_contracts::{
    AgentError, AgentResult, ContextAction, ContextConsumptionAck, ContextDiagnostics,
    ContextEngine, ContextGcReport, ContextIngress, ContextItem, ContextItemId, ContextItemSummary,
    ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextRetention, ContextScope, ContextStateTransition, CoreLabel, FocusState, Label,
    MaterializedContext, ScopeId, ScopeKind, ScopeState, StoreReconcileReport,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::checkpoint;
use crate::diagnostics;
use crate::gc::{full, minor, reachability};
use crate::heap::external_summary;
use crate::index::{dependency, entity};
use crate::item;
use crate::materializer;
use crate::scope;
use crate::store;

/// 一条 anchor 根声明的 item_ref 是否匹配给定 id + entity 签名：精确
/// id、`context://run/<id>` uri，或精确 entity 名。TaskAnchor 声明的是
/// 引用（不嵌入 body），engine 只按这三类键解析它指向谁。
pub(crate) fn anchor_ref_matches(item_ref: &str, id: ContextItemId, entities: &[String]) -> bool {
    item_ref == id.to_string()
        || item_ref == format!("context://run/{id}")
        || entities.iter().any(|entity| entity == item_ref)
}

/// 一条 anchor 根声明是否匹配某个 resident 条目（mark 与 materialize
/// 共用同一解析，保证 GC 根与强制入帧看到同一目标集）。
pub(crate) fn anchor_claim_matches_item(
    claim: &agent_contracts::AnchorRootClaim,
    item: &ContextItem,
) -> bool {
    anchor_ref_matches(&claim.item_ref, item.id, &item.entities)
}

/// 一条 anchor 根声明是否匹配某个外部映射条目（Cold/External 的召回与
/// Storage GC 保护共用同一解析）。
pub(crate) fn anchor_claim_matches_entry(
    claim: &agent_contracts::AnchorRootClaim,
    entry: &agent_contracts::ExternalizedContext,
) -> bool {
    anchor_ref_matches(&claim.item_ref, entry.item_id, &entry.entities)
}

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
    /// Cap on the in-engine lifecycle ledger buffer. The ledger is bounded
    /// (oldest rows drop) and is exported to a JSONL artifact on demand —
    /// never written on the context hot path.
    pub max_ledger_records: usize,
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
            max_ledger_records: 4096,
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
#[derive(Debug, Default)]
pub(crate) struct PendingMaterialization {
    id: u64,
    item_ids: std::collections::HashSet<ContextItemId>,
    external_item_ids: std::collections::HashSet<ContextItemId>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// Monotonic event-sequence clock: advances on every state-changing
    /// operation (ingest/maintain/GC/reconcile/ack/scope ops). Never
    /// advances on `materialize` — a preview is a read and must not age
    /// TTLs or recency. `alias = "tick"` keeps pre-separation checkpoints
    /// loadable. TTL rules name their clock explicitly; this one orders
    /// events and measures event-distance, not age.
    #[serde(default, alias = "tick")]
    pub(crate) event_seq: u64,
    /// User-turn clock: advances once per user message. Rules measuring
    /// age in user turns (ephemeral TTL, staleness) read this.
    pub(crate) turn: u64,
    pub(crate) tool_round: u64,
    /// Monotonic identity of the last materialization preview. Persisted so
    /// checkpoint/restore cannot reuse an id inside one engine lifetime.
    #[serde(default)]
    pub(crate) materialization_revision: u64,
    /// Single actor-owned preview awaiting a successful model-consumption
    /// acknowledgement. Ephemeral: checkpoints are taken only at safe points
    /// and must never resume an in-flight provider request.
    #[serde(skip)]
    pub(crate) pending_materialization: Option<PendingMaterialization>,
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
    /// Canonical `item_id -> location` directory plus query indexes shared
    /// by GC recall and `context.search`. Derived from the three body stores
    /// and skipped in checkpoints; restore rebuilds it.
    #[serde(skip)]
    pub(crate) catalog: crate::index::catalog::ContextCatalog,
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
    /// 分级检索戳累计（进 diagnostics / checkpoint）。
    #[serde(default)]
    pub(crate) access_search_hits: u64,
    #[serde(default)]
    pub(crate) access_inspects: u64,
    #[serde(default)]
    pub(crate) access_fetches: u64,
    #[serde(default)]
    pub(crate) access_admits: u64,
    #[serde(default)]
    pub(crate) access_consumption_acks: u64,
    /// Directives counted against the per-turn admit cap. Reset by the
    /// next user message (turn boundary); a turn whose admits are refused
    /// keeps the count so the model learns the cap from refusals.
    #[serde(default)]
    pub(crate) admits_this_turn: usize,
    /// Directives counted against the per-turn derive cap (same lifecycle
    /// as `admits_this_turn`).
    #[serde(default)]
    pub(crate) derives_this_turn: usize,
    /// 本回合已对相同检索指纹执行过 search 强化的次数。回合边界清零。
    /// 不进 checkpoint：恢复后最多再给一次 search 强化，不会变成 pin。
    #[serde(skip)]
    pub(crate) search_query_stamps_this_turn: std::collections::HashMap<u64, u32>,
    /// The active task's typed root claims, projected from its TaskAnchor by
    /// the runtime via `ContextAction::AnchorRoots`. Consumed by GC
    /// (`ResidentRequired`/`PromptRequired` protect the heap) and Storage GC
    /// (`StorageRequired` protects the store). Replacement-only, bounded by
    /// `MAX_ANCHOR_ROOT_CLAIMS`; the engine never owns task authority.
    #[serde(default)]
    pub(crate) anchor_roots: Vec<agent_contracts::AnchorRootClaim>,
    /// Bounded in-engine lifecycle ledger: every item transition on any
    /// axis (attention/semantic/residency/gc) with cause, trigger, turn and
    /// related id. Oldest rows drop past the cap; export to a JSONL
    /// artifact is explicit and never on the hot path.
    #[serde(default)]
    pub(crate) ledger: Vec<agent_contracts::ContextLifecycleRecord>,
    /// Per-item revision counter backing `ContextLifecycleRecord::revision`.
    #[serde(default)]
    pub(crate) ledger_revisions: std::collections::HashMap<ContextItemId, u64>,
    /// Ledger buffer cap, copied from the config at construction so every
    /// record site only needs `&mut State`.
    #[serde(default)]
    pub(crate) ledger_cap: usize,
}

impl State {
    /// Rebuild the catalog when the body stores or event clock moved.
    /// Search and GC recall consume this directory; callers must sync
    /// before reading it.
    pub(crate) fn sync_catalog(&mut self) {
        self.catalog.sync(
            &self.items[..],
            &self.eviction_buffer,
            &self.external[..],
            self.event_seq,
        );
    }

    /// 活跃任务里每个最近文件的最新成功观察。已完成任务或没有焦点时为空：
    /// 文件正文根不能把上一个任务的正文带进下一个任务。
    pub(crate) fn latest_file_body_ids(&self) -> std::collections::HashSet<ContextItemId> {
        let Some(task) = self.focus.as_ref().map(|focus| focus.task_id) else {
            return std::collections::HashSet::new();
        };
        let completed = self.scopes.iter().any(|scope| {
            scope.kind == ScopeKind::Task
                && scope.task_id == Some(task)
                && scope.state == ScopeState::Closed
        });
        if completed {
            return std::collections::HashSet::new();
        }
        entity::latest_file_body_ids(self.items.iter(), Some(task))
    }
}

pub struct SimpleContextEngine {
    pub(crate) config: SimpleContextConfig,
    pub(crate) state: Mutex<State>,
    /// Serializes the multi-phase and whole-state operations. GC, storage
    /// GC, store reconcile, checkpoint and restore each span several state
    /// lock acquisitions (deliberately releasing the state lock across disk
    /// IO); the gate keeps them from interleaving, so a plan computed
    /// against one state can never be committed against a state another
    /// operation replaced in between. Single-phase operations (ingest,
    /// maintain, materialize, ...) are atomic under the state lock alone
    /// and never take the gate — lock order is always gate, then state.
    pub(crate) op_gate: Mutex<()>,
}

impl SimpleContextEngine {
    pub fn new(config: SimpleContextConfig) -> Self {
        let state = State {
            ledger_cap: config.max_ledger_records,
            ..State::default()
        };
        Self {
            config,
            state: Mutex::new(state),
            op_gate: Mutex::new(()),
        }
    }

    /// Export the bounded in-engine lifecycle ledger to a JSONL artifact
    /// (one `ContextLifecycleRecord` per line) and clear the buffer. This
    /// is an explicit, off-hot-path operation; a crashed export never
    /// truncates the previous artifact (temp file + rename).
    pub async fn export_ledger(&self, path: &std::path::Path) -> AgentResult<usize> {
        let rows: Vec<agent_contracts::ContextLifecycleRecord> = {
            let mut state = self.state.lock().await;
            std::mem::take(&mut state.ledger)
        };
        if rows.is_empty() {
            return Ok(0);
        }
        let text = crate::ledger::encode(&rows);
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, text).map_err(|error| {
            agent_contracts::AgentError::Storage(format!("write ledger artifact: {error}"))
        })?;
        std::fs::rename(&tmp, path).map_err(|error| {
            agent_contracts::AgentError::Storage(format!("commit ledger artifact: {error}"))
        })?;
        Ok(rows.len())
    }
}

fn has_exactly_one_owner(state: &State, item_id: ContextItemId) -> bool {
    // The heap and external map own unique id indexes; the reversible Warm
    // buffer is bounded by config, so checking all three locations is O(1)
    // plus a small bounded scan rather than O(total history). The catalog
    // skips a duplicate on rebuild, so it cannot be the duplicate detector.
    let resident = usize::from(state.items.indexes().get(item_id).is_some());
    let warm = usize::from(state.eviction_buffer.iter().any(|item| item.id == item_id));
    let external = usize::from(state.external.get(item_id).is_some());
    resident + warm + external == 1
}

/// Stamp one consumed identity wherever its body/descriptor currently lives.
/// A successful acknowledgement never changes residency or semantic state;
/// it only records that the model actually saw the final packed projection.
fn stamp_consumed(
    state: &mut State,
    item_id: ContextItemId,
    now_tick: u64,
    turn: u64,
    gc_epoch: u64,
) -> bool {
    crate::access::stamp_consumed(state, item_id, now_tick, turn, gc_epoch)
}

#[async_trait::async_trait]
impl ContextEngine for SimpleContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let mut state = self.state.lock().await;
        state.event_seq += 1;

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
                state.search_query_stamps_this_turn.clear();
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
                if ok {
                    reachability::queue_file_body_supersessions(
                        &mut state,
                        &content,
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
            ContextIngress::WorkingSetSignal { content } => {
                // A mid-turn signal from a tool commit: the entities the
                // tool just touched become hot for the *next* model round,
                // without persisting a body yet (the observation lands at
                // turn end). Bounded merge, no item, no scope change — the
                // signal only extends the hot-entity set.
                if self.config.entity_affinity {
                    entity::merge_hot_entities(
                        &mut state.hot_entities,
                        entity::extract_entities(&content),
                    );
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
                // The summary belongs to the completed task line, not to
                // whatever scope happens to be active right now: capture the
                // completed task's scope *before* the focus/close machinery
                // runs, so a named summary never inherits the current focus
                // identity (a completed task can arrive while another task
                // is focused, and `state.focus` is cleared below before the
                // item is built).
                let summary_scope_id = completed_task
                    .and_then(|task| {
                        state.scopes.iter().find(|scope| {
                            scope.kind == ScopeKind::Task
                                && scope.task_id == Some(task)
                                && scope.state != ScopeState::Closed
                        })
                    })
                    .map(|scope| scope.id)
                    .or_else(|| {
                        state
                            .scopes
                            .iter()
                            .find(|scope| scope.kind == ScopeKind::Session)
                            .map(|scope| scope.id)
                    });
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
                    // External entries carry the same protection fields
                    // (captured at externalize time); a completed task
                    // clears them there too, so no body location can keep
                    // rooting the finished task's records.
                    let mut external = state.external.take_all();
                    for entry in &mut external {
                        if entry.task_id == Some(completed_task) {
                            entry.keep_alive = false;
                            entry.lease_until_turn = None;
                        }
                    }
                    state.external.replace_all(external);
                    scope::queue_task_scope_close(&mut state, completed_task);
                }
                let mut item = item::make_item(
                    &state,
                    &self.config,
                    summary,
                    ContextKind::Summary,
                    ContextScope::Session,
                    ContextRetention::Durable,
                    0.84,
                    Some("task-summary".to_string()),
                );
                // Re-stamp the identity the focus machinery above may have
                // cleared or displaced: the summary belongs to the completed
                // task and its scope, never to the current focus.
                item.task_id = completed_task;
                if let Some(scope_id) = summary_scope_id {
                    item.scope_id = Some(scope_id);
                }
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
        state.event_seq += 1;
        let now_tick = state.event_seq;
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
        // Serialize against the other multi-phase/whole-state operations:
        // the plan computed below must commit against the same state it was
        // planned against, never one a concurrent restore/storage-GC
        // replaced in the meantime.
        let _gate = self.op_gate.lock().await;
        // Four phases so the state lock is not held across disk IO:
        // 1. plan under the lock (mark/sweep/reactivate/age — in memory);
        // 2. store writes and recall reads without the lock;
        // 3. commit under a fresh lock (external entries, recalled items,
        //    failed-write buffer returns, diagnostics);
        // 4. delete recalled blobs after the commit, without the lock — a
        //    blob is removed only once its content is resident again.
        let mut state = self.state.lock().await;
        state.event_seq += 1;
        let now_tick = state.event_seq;
        let turn = state.turn;
        let Some(mut plan) = full::plan_full_gc(&mut state, &self.config, now_tick, turn) else {
            return Ok(ContextGcReport {
                resident: state.items.len(),
                diagnostics: diagnostics::compute(&state),
                ..ContextGcReport::default()
            });
        };
        drop(state);
        let io = full::run_store_io(&self.config, &mut plan).await;
        let mut state = self.state.lock().await;
        let (mut report, blobs_to_delete) = full::commit_full_gc(&mut state, now_tick, plan, io);
        drop(state);
        if !blobs_to_delete.is_empty() {
            let dir = crate::store::store_dir(&self.config);
            let outcomes = crate::store::delete_blobs_async(&dir, &blobs_to_delete).await;
            report.store_blob_delete_errors = outcomes
                .iter()
                .filter(|(_, outcome)| outcome.is_err())
                .count();
        }
        Ok(report)
    }

    async fn reconcile_store(&self) -> AgentResult<StoreReconcileReport> {
        // Same plan/io/commit split as the GC: snapshot the map's owned
        // checksums and the resident ids under the lock, scan + classify
        // the directory without it, then re-own rebuilt blobs under a
        // fresh lock (re-checking that nothing claimed the id meanwhile).
        // The gate keeps this three-phase operation from interleaving with
        // GC/storage-GC/checkpoint/restore, so the re-ownership commit
        // always runs against the state the plan was derived from.
        let _gate = self.op_gate.lock().await;
        let (map_checksums, resident_ids) = {
            let mut state = self.state.lock().await;
            state.event_seq += 1;
            let map_checksums: std::collections::HashMap<_, _> = state
                .external
                .iter()
                .map(|entry| (entry.item_id, entry.blob_checksum.clone()))
                .collect();
            let resident_ids: std::collections::HashSet<_> = state
                .items
                .iter()
                .chain(state.eviction_buffer.iter())
                .map(|item| item.id)
                .collect();
            (map_checksums, resident_ids)
        };
        let dir = crate::store::store_dir(&self.config);
        let io = crate::store::run_reconcile_io(&dir, &map_checksums, &resident_ids).await;
        let mut state = self.state.lock().await;
        let now_tick = state.event_seq;
        let gc_epoch = state.gc_epoch;
        Ok(crate::store::commit_reconcile(
            &mut state, io, now_tick, gc_epoch,
        ))
    }

    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        let mut state = self.state.lock().await;
        // A preview is a read: it must not advance the event-sequence clock,
        // so merely materializing never ages TTLs or recency scores.
        state.materialization_revision =
            state
                .materialization_revision
                .checked_add(1)
                .ok_or_else(|| {
                    AgentError::Internal("context materialization id is exhausted".into())
                })?;
        let materialization_id = state.materialization_revision;
        let mut materialized = materializer::materialize(&mut state, &self.config, &query);
        materialized.materialization_id = materialization_id;
        state.pending_materialization = Some(PendingMaterialization {
            id: materialization_id,
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: materialized
                .external
                .iter()
                .map(|entry| entry.item_id)
                .collect(),
        });
        Ok(materialized)
    }

    async fn acknowledge_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()> {
        ack.validate()?;
        let mut state = self.state.lock().await;
        let pending = state.pending_materialization.as_ref().ok_or_else(|| {
            AgentError::InvalidRequest(
                "context consumption ack has no pending materialization preview".into(),
            )
        })?;
        if pending.id != ack.materialization_id {
            return Err(AgentError::InvalidRequest(format!(
                "context consumption ack references materialization {}, but {} is pending",
                ack.materialization_id, pending.id
            )));
        }
        if ack.item_ids.iter().any(|id| !pending.item_ids.contains(id)) {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains an item outside the referenced preview".into(),
            ));
        }
        if ack
            .external_item_ids
            .iter()
            .any(|id| !pending.external_item_ids.contains(id))
        {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains an external ref outside the referenced preview"
                    .into(),
            ));
        }
        if ack
            .item_ids
            .iter()
            .chain(&ack.external_item_ids)
            .any(|id| !has_exactly_one_owner(&state, *id))
        {
            return Err(AgentError::Context(
                "context consumption ack references an item without exactly one residency owner"
                    .into(),
            ));
        }

        let now_event_seq = state
            .event_seq
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("context event sequence is exhausted".into()))?;
        state.event_seq = now_event_seq;
        let turn = state.turn;
        let gc_epoch = state.gc_epoch;
        for item_id in ack.item_ids.iter().chain(&ack.external_item_ids) {
            // Ownership was validated above while holding the same lock, so
            // stamping is infallible and the acknowledgement commits as one
            // mutation rather than partially reinforcing a prefix.
            debug_assert!(stamp_consumed(
                &mut state,
                *item_id,
                now_event_seq,
                turn,
                gc_epoch
            ));
        }
        state.pending_materialization = None;
        Ok(())
    }

    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        let mut state = self.state.lock().await;
        state.event_seq += 1;
        Ok(scope::open_scope(&mut state, kind, parent))
    }

    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        let mut state = self.state.lock().await;
        state.event_seq += 1;
        Ok(scope::close_scope(&mut state, scope_id))
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        let state = self.state.lock().await;
        Ok(diagnostics::compute(&state))
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        let state = self.state.lock().await;
        // The logical catalog, not just the resident share: the heap, the
        // reversible warm buffer and the external store entries are all
        // known items. External entries project from their descriptor,
        // which carries the authoritative creation clock captured at
        // externalize time.
        //
        // Bounded by construction: `bounded_catalog` keeps only the
        // `limit` smallest created_tick summaries while streaming, so the
        // call's memory stays O(limit) no matter how large the external
        // store grows — a model-driven catalog call must not cost
        // proportional to logical history size (M14 resource policy). The
        // stream order (heap slot, buffer order, externalization order)
        // makes equal ticks deterministic, exactly like the previous
        // stable sort + truncate.
        let summaries = crate::heap::to_summaries(&state.items);
        let summaries = crate::heap::bounded_catalog(
            limit,
            summaries
                .into_iter()
                .chain(crate::heap::to_summaries(&state.eviction_buffer))
                .chain(state.external.iter().map(external_summary)),
        );
        Ok(summaries)
    }

    async fn search_external(
        &self,
        query: agent_contracts::ContextSearchQuery,
    ) -> AgentResult<Vec<agent_contracts::ExternalizedContext>> {
        let mut state = self.state.lock().await;
        state.sync_catalog();
        let hits = crate::store::search_catalog(&state, &query);
        // search 命中是最弱信号：相同查询本回合只强化一次，单条目同一
        // event_seq 冷却，饱和后不再推迟 Cold 老化。terminal 命中已被
        // externally_retrievable 过滤；search 从不覆盖终态语义或 GC 根。
        crate::access::reinforce_search_hits(&mut state, &hits, &query);
        Ok(hits)
    }

    async fn inspect_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<agent_contracts::ExternalizedContext>> {
        let mut state = self.state.lock().await;
        let retrievable = state
            .external
            .get(item_id)
            .is_some_and(crate::store::externally_retrievable);
        if !retrievable {
            return Ok(None);
        }
        // inspect 是故意读取描述符，强于 search。更强信号（fetch/ack）
        // 已经写过时 stamp 拒绝降级，返回值仍是当前权威描述符。
        crate::access::stamp_read(&mut state, item_id, agent_contracts::AccessSignal::Inspect);
        Ok(state.external.get(item_id).cloned())
    }

    async fn fetch_external(&self, item_id: ContextItemId) -> AgentResult<Option<ContextItem>> {
        // Membership and the access-stamp inputs under the lock; the disk
        // read happens *outside* it — sync store IO must never stall the
        // context hot path.
        let dir = crate::store::store_dir(&self.config);
        let retrievable = {
            let state = self.state.lock().await;
            // O(1) id-index membership instead of a linear scan: the
            // model's retrieval loop calls this per item.
            state
                .external
                .get(item_id)
                .is_some_and(crate::store::externally_retrievable)
        };
        if !retrievable {
            return Ok(None);
        }
        let item = crate::store::read_item_async(&dir, item_id).await;
        if item.is_some() {
            // fetch 读到 body，信号强于 inspect/search。更强的 ack 已经
            // 写过时 stamp 拒绝降级。
            let mut state = self.state.lock().await;
            let still_retrievable = state
                .external
                .get(item_id)
                .is_some_and(crate::store::externally_retrievable);
            if !still_retrievable {
                return Ok(None);
            }
            crate::access::stamp_read(&mut state, item_id, agent_contracts::AccessSignal::Fetch);
        }
        Ok(item)
    }

    async fn checkpoint(&self) -> AgentResult<Value> {
        // Serialized with the multi-phase operations so a checkpoint never
        // captures a state torn across a GC/storage-GC commit boundary.
        let _gate = self.op_gate.lock().await;
        let state = self.state.lock().await;
        checkpoint::serialize(&state)
    }

    async fn restore(&self, data: Value) -> AgentResult<()> {
        // Whole-state replacement must not interleave with a multi-phase
        // plan: a GC plan computed before the restore would otherwise
        // commit stale transitions against the restored state.
        let _gate = self.op_gate.lock().await;
        let mut state = self.state.lock().await;
        *state = checkpoint::deserialize(data)?;
        state.sync_catalog();
        // Structural validation before the state becomes live: duplicate
        // ids, cross-location ownership, scope ancestry and item scope
        // references must all hold (see checkpoint::validate).
        checkpoint::validate(&state)?;
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
        // lock — the state lock is never held across disk IO. The gate
        // serializes this with GC/reconcile/checkpoint/restore so the
        // commit always sees the state the plan was derived from.
        let _gate = self.op_gate.lock().await;
        let plan = {
            let mut state = self.state.lock().await;
            state.event_seq += 1;
            let now_tick = state.event_seq;
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
        // The anchor-root projection is a bounded whole-set replacement:
        // task authority stays with the TaskManager, so there is no per-claim
        // mutation to serialize with — the engine only mirrors the current
        // projection for its GC and materialization passes.
        ContextAction::AnchorRoots { roots } => {
            if roots.len() > agent_contracts::MAX_ANCHOR_ROOT_CLAIMS {
                return Some(format!(
                    "anchor roots refused: {} claims exceed the cap of {}",
                    roots.len(),
                    agent_contracts::MAX_ANCHOR_ROOT_CLAIMS
                ));
            }
            state.anchor_roots = roots.clone();
            return None;
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
            // Admit/Derive/AnchorRoots are dispatched above and never reach
            // the in-memory directive loop.
            ContextAction::Admit { .. }
            | ContextAction::Derive { .. }
            | ContextAction::AnchorRoots { .. } => unreachable!(),
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
        ContextAction::Collect | ContextAction::AnchorRoots { .. } => ContextItemId::new(),
    }
}
