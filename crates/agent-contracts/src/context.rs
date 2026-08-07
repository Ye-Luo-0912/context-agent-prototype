use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentResult, ContextItemId, ScopeId, TaskId, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextKind {
    Goal,
    Constraint,
    Decision,
    UserMessage,
    AssistantMessage,
    ToolObservation,
    FileObservation,
    Error,
    Summary,
    Note,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextScope {
    Message,
    Turn,
    Task,
    Session,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextRetention {
    Ephemeral,
    Working,
    Durable,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextState {
    Active,
    Cooling,
    Archived,
    Dropped,
}

/// Where an item physically lives. `Resident` means the item sits in the
/// model-visible heap; `Evicted` means it was moved to the bounded,
/// reversible eviction buffer by GC and can be reactivated when it becomes
/// relevant again. This is the GC dimension, orthogonal to the semantic
/// `ContextState` (Active/Cooling/Archived/Dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ContextResidency {
    #[default]
    Resident,
    Evicted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: ContextItemId,
    pub task_id: Option<TaskId>,
    /// The scope this item was produced in, from the runtime-driven scope
    /// tree. Authoritative membership: a closed scope promotes/evicts the
    /// items carrying its id. `None` only for items created before scope
    /// tracking existed (e.g. restored old checkpoints).
    #[serde(default)]
    pub scope_id: Option<ScopeId>,
    pub content: String,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub retention: ContextRetention,
    pub state: ContextState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub last_access_tick: u64,
    pub access_count: u32,
    #[serde(default)]
    pub created_turn: u64,
    #[serde(default)]
    pub last_access_turn: u64,
    #[serde(default)]
    pub dependencies: Vec<ContextItemId>,
    /// Typed labels: core content labels, lifecycle markers and extension
    /// namespaces. Promotion and GC decide membership by these instead of
    /// raw string matching.
    #[serde(default)]
    pub tags: Vec<crate::label::Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// GC dimension: whether the item is in the model-visible heap or in the
    /// reversible eviction buffer. Set by the GC pass, not by the semantic
    /// residency machine.
    #[serde(default)]
    pub residency: ContextResidency,
    /// How many full GC passes the item survived without being a root. Root
    /// reachability resets it to 0; exceeding the configured cap makes an
    /// unmarked item an eviction candidate. The generational dimension of GC.
    #[serde(default)]
    pub gc_generation: u32,
    /// Tick of the GC pass that moved this item into the reversible eviction
    /// buffer. `None` while resident. Reactivation skips items evicted by
    /// the current pass, so an item cannot bounce out and back in one GC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_at_tick: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusState {
    pub task_id: TaskId,
    pub goal: String,
    pub current_query: String,
    pub phase: String,
    #[serde(default)]
    pub active_entities: Vec<String>,
    pub generation: u64,
}

impl FocusState {
    pub fn new(goal: impl Into<String>) -> Self {
        let goal = goal.into();
        Self {
            task_id: TaskId::new(),
            current_query: goal.clone(),
            goal,
            phase: "working".to_string(),
            active_entities: Vec::new(),
            generation: 0,
        }
    }
}

/// Runtime scope entity: a container that owns items for one period of
/// execution. Scopes form a tree — Session -> Task -> Focus -> Tool — and
/// each carries its own lifecycle (open/active/suspended/closed). Closing a
/// scope promotes its durable outcomes to the parent and releases the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeKind {
    Session,
    Task,
    Focus,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeState {
    Open,
    Active,
    Suspended,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub id: ScopeId,
    pub parent: Option<ScopeId>,
    pub kind: ScopeKind,
    pub state: ScopeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub opened_tick: u64,
    pub last_active_tick: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_tick: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextIngress {
    UserMessage {
        content: String,
    },
    AssistantMessage {
        content: String,
    },
    ToolObservation {
        output: ToolOutput,
        /// The tool scope the observation belongs to, opened by the runtime
        /// at tool start. `None` when the caller has no scope tracking
        /// (replay, standalone use).
        #[serde(default)]
        scope_id: Option<ScopeId>,
    },
    FocusChanged {
        focus: FocusState,
    },
    Pin {
        content: String,
        kind: ContextKind,
    },
    TaskCompleted {
        task_id: Option<TaskId>,
        summary: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ContextMaintenanceTrigger {
    #[default]
    UserInput,
    BeforeModel,
    AfterModel,
    AfterTool,
    FocusChanged,
    TaskCompleted,
    Checkpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextQuery {
    pub current_input: String,
    pub budget_tokens: usize,
    #[serde(default)]
    pub hints: ContextHints,
}

/// Runtime knobs for one materialization. Kept open so later policy work can
/// add per-request guidance without breaking the contract.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ContextHints {
    /// Hard cap on how many items the engine may select. Dependency
    /// expansion still respects it; `None` means the budget alone decides.
    pub max_selected_items: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub importance: f32,
    pub focus_match: f32,
    pub recency: f32,
    pub access: f32,
    pub scope_bonus: f32,
    pub retention_bonus: f32,
    /// Reward for an item whose entities are hot in the current working
    /// set (user message + recent tool observations).
    #[serde(default)]
    pub entity_affinity: f32,
    pub total: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSelection {
    pub item_id: ContextItemId,
    pub score: f32,
    pub approx_tokens: usize,
    pub reason: String,
    #[serde(default)]
    pub breakdown: ScoreBreakdown,
}

/// A single observed lifecycle state transition produced by one maintenance pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextStateTransition {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub from: ContextState,
    pub to: ContextState,
    pub turn: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextDiagnostics {
    pub total_items: usize,
    pub active_items: usize,
    pub cooling_items: usize,
    pub archived_items: usize,
    pub dropped_items: usize,
    pub approx_active_tokens: usize,
    pub focus_generation: u64,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub tool_round: u64,
    #[serde(default)]
    pub open_scopes: usize,
    #[serde(default)]
    pub active_scopes: usize,
    #[serde(default)]
    pub suspended_scopes: usize,
    #[serde(default)]
    pub closed_scopes: usize,
    /// GC dimension: items currently in the model-visible heap.
    #[serde(default)]
    pub resident_items: usize,
    /// GC dimension: items sitting in the reversible eviction buffer.
    #[serde(default)]
    pub evicted_items: usize,
    /// Cumulative evictions / reactivations since the engine started, so a
    /// run's GC behavior is explainable without replaying every report.
    #[serde(default)]
    pub gc_evicted_total: u64,
    #[serde(default)]
    pub gc_reactivated_total: u64,
}

/// One structured entry of the materialized working set. The engine returns
/// content, never rendered prompt text; the runtime's prompt assembler
/// decides how the entry is presented to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedItem {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub state: ContextState,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The structured result of one `ContextEngine::materialize` call: the
/// focus, the selected working set and its selections/diagnostics. Prompt
/// rendering is deliberately absent — that is the prompt assembler's job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedContext {
    pub focus: Option<FocusState>,
    pub items: Vec<MaterializedItem>,
    pub selected: Vec<ContextSelection>,
    pub approx_tokens: usize,
    pub diagnostics: ContextDiagnostics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextMaintenanceReport {
    pub promoted: usize,
    pub cooled: usize,
    pub archived: usize,
    pub dropped: usize,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub transitions: Vec<ContextStateTransition>,
    pub diagnostics: ContextDiagnostics,
}

/// One reversible eviction produced by a full GC pass: the item left the
/// model-visible heap for the bounded eviction buffer, with the reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEviction {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    /// How many GC passes the item survived without being a root.
    pub generation: u32,
    pub evicted_at_tick: u64,
    /// Why this item was evicted (not reachable from roots, semantically
    /// dropped, stale, ...) — every eviction must be explainable.
    pub reason: String,
}

/// One reactivation produced by a full GC pass: an evicted item became
/// relevant again (hot entities, focus match, pin, task) and re-entered the
/// heap, with the reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextReactivation {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub reactivated_at_tick: u64,
    /// Why this item came back — every reactivation must be explainable.
    pub reason: String,
}

/// The result of one full `ContextEngine::gc` pass: mark roots, sweep
/// unmarked items into the reversible eviction buffer, reactivate items
/// that became relevant again, purge only when the buffer overflows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextGcReport {
    /// Items marked as roots this pass (pins, active scopes, hot entities,
    /// reachable dependencies).
    pub marked_roots: usize,
    /// Items left in the model-visible heap after the pass.
    pub resident: usize,
    /// Items moved to the reversible eviction buffer this pass.
    pub evicted: usize,
    /// Items moved back from the buffer into the heap this pass.
    pub reactivated: usize,
    /// Items permanently removed because the eviction buffer overflowed.
    /// The only irreversible GC action, and it is bounded and counted.
    pub purged: usize,
    #[serde(default)]
    pub evictions: Vec<ContextEviction>,
    #[serde(default)]
    pub reactivations: Vec<ContextReactivation>,
    pub diagnostics: ContextDiagnostics,
}

/// A bounded, UI/replay-friendly projection of one context item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItemSummary {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    #[serde(default)]
    pub scope_id: Option<ScopeId>,
    pub state: ContextState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub created_turn: u64,
    pub last_access_turn: u64,
    pub access_count: u32,
    /// Ids of prior items this item explicitly depends on (shared entities).
    #[serde(default)]
    pub dependencies: Vec<ContextItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[async_trait]
pub trait ContextEngine: Send + Sync {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()>;

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport>;

    /// Run a full GC pass: mark roots (pins, active scopes, hot entities,
    /// reachable dependencies), sweep unmarked items into the bounded
    /// reversible eviction buffer and reactivate items that became relevant
    /// again. The report explains every eviction and reactivation. The
    /// default implementation does nothing, so engines without a GC pass
    /// (baselines, adapters) keep working unchanged.
    async fn gc(&self) -> AgentResult<ContextGcReport> {
        Ok(ContextGcReport::default())
    }

    /// Materialize the working set for one model request: score, budget-pack
    /// and expand the selection, returning structured items. The runtime
    /// turns them into prompt text via its prompt assembler.
    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext>;

    /// Open a fresh scope under `parent` (or the current active scope when
    /// `parent` is `None`) and make it the active scope. The runtime drives
    /// tool scopes this way — a scope opens when its tool starts, not when
    /// the observation is later persisted. Items created while the scope is
    /// active carry its id.
    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId>;

    /// Close a scope: mark it closed, promote its durable members to the
    /// nearest open ancestor, reactivate the parent, and record the
    /// transitions the close produced.
    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>>;

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics>;

    /// Bounded projection of live items, oldest first, capped at `limit`.
    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>>;

    /// Export the current runtime state (separate from the event journal).
    async fn checkpoint(&self) -> AgentResult<serde_json::Value>;

    /// Replace runtime state from a previously exported checkpoint.
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()>;
}
