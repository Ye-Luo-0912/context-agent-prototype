use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentResult, ContextItemId, Label, ScopeId, TaskId, ToolOutput};

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

/// The attention dimension of an item: how present it is in the current
/// working set. Owned by the per-event residency machine (`maintain`), not
/// by GC. Semantic death lives in `SemanticState`, physical placement in
/// `ContextResidency` — attention alone never means an item is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionState {
    Active,
    Cooling,
    Archived,
}

/// The semantic dimension of an item: is it still *true* in the run's
/// decision/error history, or has a newer item replaced it? `Live` items can
/// be recalled when attention or GC brings them back; every non-Live state
/// is terminal — the materializer excludes them and Context GC never
/// resurrects them. `Tombstoned` replaces the old `Dropped` overload (which
/// mixed ephemeral attention loss with semantic death).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SemanticState {
    #[default]
    Live,
    /// A newer decision superseded this one.
    Superseded {
        /// The decision that replaced it; `None` only for items migrated
        /// from pre-semantic checkpoints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<ContextItemId>,
    },
    /// A successful tool result verified the error fixed.
    VerifiedFixed {
        /// The observation that verified it; `None` for migrated items.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<ContextItemId>,
    },
    /// The item's information lifecycle ended (TTL/staleness). Permanently
    /// dead from the runtime's perspective; only Storage GC may delete it.
    Tombstoned,
}

impl SemanticState {
    /// Live items can be recalled by attention or GC.
    pub fn is_live(self) -> bool {
        matches!(self, SemanticState::Live)
    }

    /// Superseded, verified-fixed and tombstoned items are semantically dead:
    /// excluded from the model and never resurrected by Context GC.
    pub fn is_dead(self) -> bool {
        !self.is_live()
    }
}

/// Where an item physically lives. `Resident` means the item sits in the
/// model-visible heap; `Warm` means it was moved to the bounded, reversible
/// eviction buffer by GC and can be reactivated when it becomes relevant
/// again; `Cold` means the buffer overflowed and the item's content now
/// lives in the external context store (a lightweight entry with a
/// `ContextRef` stays visible); `External` means the entry has aged out of
/// the working set entirely and only the store retains it. This is the GC
/// dimension, orthogonal to `AttentionState` and `SemanticState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ContextResidency {
    #[default]
    Resident,
    Warm,
    Cold,
    External,
}

/// Named generational stage of an item, derived from how many full GC
/// passes it survived without being a root. Nursery items are young, Stable
/// items have outlived the generational ladder. The underlying pass count
/// stays observable as `ContextItem::gc_generation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Generation {
    Nursery,
    Working,
    Stable,
}

impl Generation {
    pub fn of(passes: u32) -> Self {
        match passes {
            0..=1 => Generation::Nursery,
            2 => Generation::Working,
            _ => Generation::Stable,
        }
    }
}

/// The semantics of one explicit dependency edge. The dependency graph is
/// typed so GC reachability, supersession and future policies can
/// distinguish *why* an item is referenced instead of treating every edge
/// alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DependencyKind {
    /// The item was created with an entity overlap with the target: the
    /// default link edge recorded at ingest (new item -> prior item).
    #[default]
    #[serde(rename = "shares_entities")]
    SharesEntities,
    /// The item is a fact the model derived from the target ref
    /// (`context.derive`): a new item with its own id, explicitly linked
    /// back to the ref it came from, so traceability survives storage GC.
    #[serde(rename = "derived_from")]
    DerivedFrom,
}

/// One explicit dependency edge: the item depends on `target` for the
/// given reason. Deserializes from both the typed form
/// (`{"target": "...", "kind": "shares_entities"}`) and the pre-typed
/// form (`"<uuid>"`, meaning `SharesEntities`), so checkpoints written
/// before the graph was typed keep loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DependencyEdge {
    pub target: ContextItemId,
    #[serde(default)]
    pub kind: DependencyKind,
}

impl DependencyEdge {
    pub fn shares(target: ContextItemId) -> Self {
        Self {
            target,
            kind: DependencyKind::SharesEntities,
        }
    }
}

impl<'de> Deserialize<'de> for DependencyEdge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Typed {
                target: ContextItemId,
                #[serde(default)]
                kind: DependencyKind,
            },
            Legacy(ContextItemId),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Typed { target, kind } => Ok(DependencyEdge { target, kind }),
            Repr::Legacy(target) => Ok(DependencyEdge::shares(target)),
        }
    }
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
    /// Attention state (Active/Cooling/Archived), owned by the residency
    /// machine. `alias = "state"` keeps pre-split checkpoints loadable.
    #[serde(alias = "state")]
    pub attention: AttentionState,
    /// Semantic state (Live/Superseded/VerifiedFixed/Tombstoned): terminal
    /// death is expressed here, never in attention.
    #[serde(default)]
    pub semantic: SemanticState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub last_access_tick: u64,
    pub access_count: u32,
    #[serde(default)]
    pub created_turn: u64,
    #[serde(default)]
    pub last_access_turn: u64,
    /// Explicit dependency edges to prior items (typed: why the item
    /// references the target).
    #[serde(default)]
    pub dependencies: Vec<DependencyEdge>,
    /// Typed labels: core content labels, lifecycle markers and extension
    /// namespaces. Promotion and GC decide membership by these instead of
    /// raw string matching.
    #[serde(default)]
    pub tags: Vec<crate::label::Label>,
    /// Model/operator-directed GC hint (`context.gc_hint`): while set, the
    /// item is treated as a root by every GC pass until a later directive
    /// clears it.
    #[serde(default)]
    pub keep_alive: bool,
    /// Model/operator-directed lease (`context.lease`): the item is treated
    /// as a GC root until this turn (inclusive); `None` means no lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until_turn: Option<u64>,
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
    /// Precomputed entity signature of `content` (the same tokens the
    /// policy, dependency linking, supersession and GC all use), so the hot
    /// paths do not re-parse item content on every pass. The engine keeps it
    /// in sync; `serde(default)` keeps old checkpoints and hand-built items
    /// valid (restore backfills items with an empty signature).
    #[serde(default)]
    pub entities: Vec<String>,
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
    /// Build a focus for an *existing* task. The task id comes from the
    /// runtime's `TaskManager` — the context engine must never mint a
    /// `TaskId` (task identity is runtime-owned), which is why this is the
    /// only constructor.
    pub fn for_task(task_id: TaskId, goal: impl Into<String>) -> Self {
        let goal = goal.into();
        Self {
            task_id,
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
    /// A structured context directive from a tool's output (gc hint, tag,
    /// lease). Tools never touch the engine — the runtime routes the
    /// directive here and the engine applies it or ignores it when the
    /// target item is gone.
    ContextDirective {
        action: ContextAction,
    },
    FocusChanged {
        focus: FocusState,
    },
    /// The current focus is cleared: the active task is suspended (not
    /// completed), its scopes stay open for a later resume, and subsequent
    /// turns run without a task until a `FocusChanged` reactivates one.
    FocusCleared,
    Pin {
        content: String,
        kind: ContextKind,
    },
    TaskCompleted {
        task_id: Option<TaskId>,
        summary: String,
    },
}

/// A structured context directive a tool may attach to its output as a
/// `RuntimeDirective` (context control). The runtime routes it to the
/// context engine; the engine applies it or silently ignores it when the
/// target item is gone. Every directive must be explainable in the
/// lifecycle ledger: "item kept alive because ...", "item leased until
/// turn N ...", "item tagged because ...".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextAction {
    /// Keep an item resident across GC passes until a later directive
    /// clears the hint (`context.gc_hint`).
    GcHint {
        item_id: ContextItemId,
        keep_alive: bool,
    },
    /// Attach an extension tag to an item (`context.tag`).
    Tag { item_id: ContextItemId, tag: String },
    /// Protect an item from GC for the next `turns` turns
    /// (`context.lease`).
    Lease { item_id: ContextItemId, turns: u32 },
    /// Run a full GC pass now (`context.collect`). Handled by the runtime
    /// — it owns the GC pass — not by `ContextIngress::ContextDirective`.
    Collect,
    /// Bring an item back into the working set under its *original* item
    /// id (`context.admit`). Unlike `fetch` (transient read only), admit
    /// produces one lifecycle transition: the item re-enters the heap with
    /// the same id, re-stamped into the current task's scope, so it can be
    /// materialized without a later re-fetch. Terminal items (superseded,
    /// verified-fixed, tombstoned) are refused — semantic death never
    /// resurrects.
    Admit {
        item_id: ContextItemId,
        reason: String,
    },
    /// Persist a fact derived from a ref (`context.derive`): a *new* item
    /// with a new id and an explicit `DerivedFrom` edge to the source ref,
    /// so the derived knowledge is traceable but never confuses the source
    /// ref's identity with a copy.
    Derive {
        item_id: ContextItemId,
        fact: String,
        reason: String,
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
    pub from: AttentionState,
    pub to: AttentionState,
    pub turn: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextDiagnostics {
    pub total_items: usize,
    pub active_items: usize,
    pub cooling_items: usize,
    pub archived_items: usize,
    /// Items whose semantic lifecycle ended (TTL/staleness): permanently
    /// dead, never resurrected by Context GC.
    #[serde(default)]
    pub tombstoned_items: usize,
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
    pub warm_items: usize,
    /// GC dimension: items externalized to the context store, still tracked
    /// in memory with a lightweight entry (`Cold`, content in the store).
    #[serde(default)]
    pub cold_items: usize,
    /// GC dimension: externalized entries that aged out of the working set;
    /// only the store retains them (`External`).
    #[serde(default)]
    pub external_items: usize,
    /// Cumulative evictions / reactivations / externalizations since the
    /// engine started, so a run's GC behavior is explainable without
    /// replaying every report.
    #[serde(default)]
    pub gc_evicted_total: u64,
    #[serde(default)]
    pub gc_reactivated_total: u64,
    #[serde(default)]
    pub gc_externalized_total: u64,
    #[serde(default)]
    pub gc_storage_deleted_total: u64,
}

/// One structured entry of the materialized working set. The engine returns
/// content, never rendered prompt text; the runtime's prompt assembler
/// decides how the entry is presented to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedItem {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub attention: AttentionState,
    #[serde(default)]
    pub semantic: SemanticState,
    /// Retention is exposed so the runtime's final budget guard can keep
    /// pinned items while trimming the context frame, and so the model can
    /// see which entries are durable.
    #[serde(default = "default_retention")]
    pub retention: ContextRetention,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Missing `retention` on old wire/checkpoint data means a normal working
/// item, not a pinned one.
fn default_retention() -> ContextRetention {
    ContextRetention::Working
}

/// The structured result of one `ContextEngine::materialize` call: the
/// focus, the selected working set, the lightweight external context map
/// (externalized items visible only by `ContextRef`) and the
/// selections/diagnostics. Prompt rendering is deliberately absent — that
/// is the prompt assembler's job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedContext {
    pub focus: Option<FocusState>,
    pub items: Vec<MaterializedItem>,
    /// The lightweight context map: externalized items the model can only
    /// see as references (`context://...`), never as full content. The
    /// view is bounded by [`CONTEXT_MAP_VIEW_CAP`].
    #[serde(default)]
    pub external: ContextMapView,
    pub selected: Vec<ContextSelection>,
    pub approx_tokens: usize,
    pub diagnostics: ContextDiagnostics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextMaintenanceReport {
    pub promoted: usize,
    pub cooled: usize,
    pub archived: usize,
    /// Items whose semantic lifecycle ended this pass (TTL/staleness).
    #[serde(default)]
    pub tombstoned: usize,
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
/// that became relevant again. Context GC never deletes information: buffer
/// overflow *externalizes* items to the context store (`Cold`), and only
/// the separate, conservative Storage GC may delete store files.
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
    /// Items moved from the buffer to the external context store this pass
    /// (buffer overflow). Their content is not deleted — it lives on under
    /// a `ContextRef`, visible through the lightweight context map.
    #[serde(default)]
    pub externalized: usize,
    /// Warm -> Cold aging in the other direction: externalized entries that
    /// became `External` this pass (only the store retains them).
    #[serde(default)]
    pub aged_external: usize,
    #[serde(default)]
    pub evictions: Vec<ContextEviction>,
    #[serde(default)]
    pub reactivations: Vec<ContextReactivation>,
    pub diagnostics: ContextDiagnostics,
}

/// A reference to an externalized item in the context store. The model only
/// ever sees these lightweight references for `Cold`/`External` items, never
/// their full content — reading an externalized item back is a deliberate
/// operation (e.g. a future context tool), not a passive materialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRef {
    /// Stable, addressable uri, e.g. `context://run/<item-id>`.
    pub uri: String,
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    /// Bounded summary (content truncated at externalization time).
    pub summary: String,
    pub created_tick: u64,
}

/// One entry of the external context map: a lightweight record of an item
/// whose content lives in the context store. `Cold` entries can still be
/// recalled by hot-entity matches (the store is read back); `External`
/// entries only exist as references until a future context tool pulls them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalizedContext {
    pub item_id: ContextItemId,
    /// Task the item belonged to when it was externalized, kept so
    /// deterministic retrieval can filter by task without reading the file.
    #[serde(default)]
    pub task_id: Option<TaskId>,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub retention: ContextRetention,
    pub attention: AttentionState,
    pub semantic: SemanticState,
    pub context_ref: ContextRef,
    pub externalized_at_tick: u64,
    pub last_access_tick: u64,
    pub residency: ContextResidency,
    /// Entity signature captured at externalize time. Recall filters on this
    /// in memory first, so a Cold-recall pass reads only the store files
    /// whose entities actually match — not every externalized entry.
    #[serde(default)]
    pub entities: Vec<String>,
    /// Tags captured at externalize time, so the materialized external view
    /// and future context retrieval can rank by open-loop/decision labels
    /// without reading the store file.
    #[serde(default)]
    pub tags: Vec<Label>,
    /// Dependency edges captured at externalize time, so the Storage GC can
    /// run a reachability closure over resident *and* external entries
    /// instead of checking only single incoming edges from the heap.
    #[serde(default)]
    pub dependencies: Vec<DependencyEdge>,
    /// The `State::gc_epoch` at which this entry was last accessed. Aging
    /// Cold -> External compares *generations* (only full GC increments the
    /// epoch), never ticks — ingest/maintain/materialize also advance the
    /// tick counter and would make a pass-based TTL meaningless. `None` for
    /// entries restored from pre-epoch checkpoints: they are treated as
    /// accessed at the current epoch instead of aging out instantly.
    #[serde(default)]
    pub last_access_gc_epoch: Option<u64>,
}

/// Cap on the external refs surfaced in one materialized context. The
/// prompt renders refs only, so the bound is about prompt cost: the model
/// should see a handful of pullable refs, not the whole external history.
pub const CONTEXT_MAP_VIEW_CAP: usize = 32;

/// The bounded, model-facing view of the external context map. The engine
/// selects at most [`CONTEXT_MAP_VIEW_CAP`] refs per materialization; the
/// type enforces that bound, so a producer that forgets the cap fails
/// loudly at the type boundary instead of silently growing the prompt.
/// Newtype-serializes transparently as the inner refs, so the wire shape
/// is unchanged from the raw `Vec`.
#[derive(Debug, Clone, Default)]
pub struct ContextMapView(Vec<ExternalizedContext>);

impl ContextMapView {
    /// Build a view from a bounded selection. Panics when `entries`
    /// exceeds [`CONTEXT_MAP_VIEW_CAP`]: the engine's selection policy is
    /// the only local producer and is itself bounded, so an over-cap input
    /// is an internal invariant violation, not a runtime condition.
    pub fn new(entries: Vec<ExternalizedContext>) -> Self {
        assert!(
            entries.len() <= CONTEXT_MAP_VIEW_CAP,
            "external context map view of {} refs exceeds the cap of {CONTEXT_MAP_VIEW_CAP}",
            entries.len()
        );
        Self(entries)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ExternalizedContext> {
        self.0.iter()
    }

    pub fn as_slice(&self) -> &[ExternalizedContext] {
        &self.0
    }
}

impl std::ops::Deref for ContextMapView {
    type Target = [ExternalizedContext];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> IntoIterator for &'a ContextMapView {
    type Item = &'a ExternalizedContext;
    type IntoIter = std::slice::Iter<'a, ExternalizedContext>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Serialize for ContextMapView {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContextMapView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let entries = Vec::<ExternalizedContext>::deserialize(deserializer)?;
        if entries.len() > CONTEXT_MAP_VIEW_CAP {
            return Err(serde::de::Error::custom(format!(
                "external context map view of {} refs exceeds the cap of {CONTEXT_MAP_VIEW_CAP}",
                entries.len()
            )));
        }
        Ok(Self(entries))
    }
}

/// Deterministic search over the external context map — no vectors. The
/// indexed dimensions of the map (entity signature, kind, scope, task,
/// label, recency) are enough to bring a ref back on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSearchQuery {
    /// Free-text query matched (case-insensitively) against entity
    /// signatures and the entry summary.
    pub query: String,
    /// Optional kind filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ContextKind>,
    /// Optional scope filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ContextScope>,
    /// Optional task filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Cap on returned refs. `0` means the engine default (16).
    pub limit: usize,
}

impl ContextSearchQuery {
    pub fn new(query: impl Into<String>, limit: usize) -> Self {
        Self {
            query: query.into(),
            kind: None,
            scope: None,
            task_id: None,
            limit,
        }
    }
}

/// The result of one `ContextEngine::storage_gc` pass. Storage GC is the
/// only place information is permanently deleted, and it is deliberately
/// conservative: nothing is deleted while a resident/warm item still
/// depends on it, pinned/durable items are never touched, and only items
/// whose semantic lifecycle ended are candidates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageGcReport {
    pub scanned: usize,
    pub deleted: usize,
    /// Store entries that could not be touched because the filesystem
    /// returned a real error (permission, disk). Those entries are *kept* —
    /// an IO failure must never be mistaken for "the file is already gone".
    #[serde(default)]
    pub io_errors: usize,
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// A bounded, UI/replay-friendly projection of one context item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItemSummary {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    #[serde(default)]
    pub scope_id: Option<ScopeId>,
    pub attention: AttentionState,
    #[serde(default)]
    pub semantic: SemanticState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub created_turn: u64,
    pub last_access_turn: u64,
    pub access_count: u32,
    /// Ids of prior items this item explicitly depends on (shared entities).
    #[serde(default)]
    pub dependencies: Vec<ContextItemId>,
    /// Model/operator-directed GC hint (see `ContextItem::keep_alive`).
    #[serde(default)]
    pub keep_alive: bool,
    /// Model/operator-directed lease (see `ContextItem::lease_until_turn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until_turn: Option<u64>,
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

    /// Run the conservative Storage GC: permanently delete context-store
    /// entries whose semantic lifecycle ended and that nothing references
    /// anymore. This is the *only* place information is deleted — Context
    /// GC externalizes, it never purges. Default implementation does
    /// nothing, so engines without a store (baselines, adapters) keep
    /// working unchanged.
    async fn storage_gc(&self) -> AgentResult<StorageGcReport> {
        Ok(StorageGcReport::default())
    }

    /// Bounded projection of live items, oldest first, capped at `limit`.
    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>>;

    /// Deterministic search over externalized refs: matches the query
    /// against entity signatures, kind/scope/task filters and recency,
    /// capped at `query.limit`. The default implementation returns nothing,
    /// so engines without an external store (baselines, adapters) keep
    /// working unchanged.
    async fn search_external(
        &self,
        query: ContextSearchQuery,
    ) -> AgentResult<Vec<ExternalizedContext>> {
        let _ = query;
        Ok(Vec::new())
    }

    /// One externalized entry's metadata by item id. No store read — the
    /// map entry already carries everything the model needs to decide
    /// whether to fetch.
    async fn inspect_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<ExternalizedContext>> {
        let _ = item_id;
        Ok(None)
    }

    /// Pull one externalized item's full content back from the store. The
    /// item stays externalized — this is a deliberate, access-stamped read,
    /// not a reactivation; the caller (the model) decides what to do with
    /// the content and the working set is left untouched.
    async fn fetch_external(&self, item_id: ContextItemId) -> AgentResult<Option<ContextItem>> {
        let _ = item_id;
        Ok(None)
    }

    /// Export the current runtime state (separate from the event journal).
    async fn checkpoint(&self) -> AgentResult<serde_json::Value>;

    /// Replace runtime state from a previously exported checkpoint.
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_entry() -> ExternalizedContext {
        ExternalizedContext {
            item_id: ContextItemId::new(),
            task_id: None,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: AttentionState::Archived,
            semantic: SemanticState::Live,
            context_ref: ContextRef {
                uri: "context://run/x".into(),
                item_id: ContextItemId::new(),
                kind: ContextKind::Note,
                scope: ContextScope::Task,
                summary: "x".into(),
                created_tick: 0,
            },
            externalized_at_tick: 0,
            last_access_tick: 0,
            residency: ContextResidency::Cold,
            entities: Vec::new(),
            tags: Vec::new(),
            dependencies: Vec::new(),
            last_access_gc_epoch: Some(0),
        }
    }

    #[test]
    fn context_map_view_caps_at_the_contract_bound() {
        let entries: Vec<ExternalizedContext> =
            (0..CONTEXT_MAP_VIEW_CAP).map(|_| ref_entry()).collect();
        let view = ContextMapView::new(entries);
        assert_eq!(view.len(), CONTEXT_MAP_VIEW_CAP);
        assert_eq!(view.iter().count(), CONTEXT_MAP_VIEW_CAP);
        assert_eq!(view.as_slice().len(), CONTEXT_MAP_VIEW_CAP);
    }

    #[test]
    #[should_panic(expected = "exceeds the cap")]
    fn context_map_view_rejects_an_over_cap_build() {
        let entries: Vec<ExternalizedContext> =
            (0..CONTEXT_MAP_VIEW_CAP + 1).map(|_| ref_entry()).collect();
        let _ = ContextMapView::new(entries);
    }

    #[test]
    fn context_map_view_serializes_transparently_and_validates_wire_data() {
        let mut entries: Vec<ExternalizedContext> =
            (0..CONTEXT_MAP_VIEW_CAP).map(|_| ref_entry()).collect();
        entries.push(ref_entry());
        // Over-cap wire data is rejected by deserialization (the bound is
        // enforced on both sides of the service boundary), while the local
        // constructor's invariant keeps the in-process shape honest.
        let bytes = serde_json::to_vec(&entries).unwrap();
        let err = serde_json::from_slice::<ContextMapView>(&bytes).unwrap_err();
        assert!(err.to_string().contains("exceeds the cap"));
    }

    #[test]
    fn dependency_edge_roundtrips_and_accepts_the_legacy_id_form() {
        let target = ContextItemId::new();
        let edge = DependencyEdge::shares(target);

        // The typed wire form carries the edge kind...
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("shares_entities"), "{json}");
        let back: DependencyEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(back, edge);

        // ...and the pre-typed form (a bare id string, as written by old
        // checkpoints) still deserializes as a SharesEntities edge.
        let legacy: DependencyEdge = serde_json::from_str(&format!("\"{target}\"")).unwrap();
        assert_eq!(legacy, DependencyEdge::shares(target));
    }

    #[test]
    fn legacy_dependency_arrays_in_checkpoints_still_load() {
        let target = ContextItemId::new();
        let legacy: Vec<DependencyEdge> = serde_json::from_str(&format!("[\"{target}\"]")).unwrap();
        assert_eq!(legacy, vec![DependencyEdge::shares(target)]);
    }
}
