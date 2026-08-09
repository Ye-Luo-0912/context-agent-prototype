use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AgentError, AgentResult, CancellationToken, ContextAction, ContextItemId, ContextKind,
    ContextScope, RunId, TaskId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolRisk {
    ReadOnly,
    WorkspaceWrite,
    ProcessExecution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: ToolRisk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub summary: String,
    /// Bounded content intended for the next model turn.
    pub model_content: String,
    /// Raw/large output should live here instead of in the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRequest {
    pub run_id: RunId,
    pub call: ToolCall,
    /// Cooperative cancellation handle for this execution (kill long-running
    /// processes, abort expensive searches). Not serialized.
    #[serde(skip)]
    pub cancel: CancellationToken,
}

/// A side effect a tool prepared but did not yet apply.
///
/// The tool's *computation* (reading, diffing, staging a temp file) happens
/// inside execution; the *side-effect commit* is owned by the runtime. The
/// actor validates the operation against the generation fence and only then
/// calls `commit`; a stale operation (cancelled or superseded) must call
/// `rollback` so the staged effect never lands. This is what makes tool
/// cancellation safe: the actor can stop a stale mutation before it touches
/// the filesystem, the git index, an outbox or an external API.
#[async_trait::async_trait]
pub trait Effect: Send + Sync {
    /// Human-readable description for events and logs.
    fn describe(&self) -> String;
    /// Apply the prepared effect (atomic rename, outbox send, ...). The
    /// journal must reflect the outcome either way. The failure kind is
    /// structured: `NotApplied` leaves the world unchanged, while
    /// `AppliedButDurabilityFailed` means the effect landed but its record
    /// could not be persisted — the runtime must treat that as a
    /// degraded/recovery state, never as "nothing happened".
    async fn commit(self: Box<Self>) -> Result<(), EffectCommitError>;
    /// Undo the preparation: the effect must not land.
    async fn rollback(self: Box<Self>, reason: &str);
}

/// Why an effect commit failed. The distinction is load-bearing: after a
/// `NotApplied` failure the world is unchanged (tell the model "nothing
/// happened"); after `AppliedButDurabilityFailed` the side effect already
/// landed but its journal record could not be persisted — the runtime must
/// surface a degraded/recovery state instead of claiming the mutation never
/// happened.
#[derive(Debug)]
pub enum EffectCommitError {
    /// The effect did not land; there is nothing to recover.
    NotApplied(AgentError),
    /// The effect landed but its durability record (journal) failed. The
    /// filesystem and the journal now disagree.
    AppliedButDurabilityFailed(AgentError),
}

impl std::fmt::Display for EffectCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApplied(error) => write!(f, "effect not applied: {error}"),
            Self::AppliedButDurabilityFailed(error) => {
                write!(f, "effect applied but its journal record failed: {error}")
            }
        }
    }
}

impl std::error::Error for EffectCommitError {}

/// A directive a tool attaches to its output asking the runtime to change
/// runtime-owned state (a context `gc_hint` / `tag` / `lease` / `collect`).
/// Unlike a plain `ToolOutput` field — which any tool, including a
/// capability, could set — a `RuntimeDirective` is a distinct
/// `ToolOutcome` variant. The dispatcher only lets trusted tools and
/// capabilities holding `RUNTIME_CONTEXT_CONTROL` produce it, so an
/// arbitrary capability cannot forge context-control requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeDirective {
    Context(ContextAction),
}

/// Permission a capability's manifest must declare to attach context
/// directives to its outputs. The dispatcher checks it before a directive
/// from a capability reaches the actor.
pub const RUNTIME_CONTEXT_CONTROL: &str = "runtime:context-control";

/// What a tool execution produced: either a plain bounded output (a value —
/// reads, searches, already-applied behavior like a spawned process), an
/// output plus a staged side effect the runtime must commit (or roll back)
/// after validating the operation is still current, an output plus a
/// runtime directive (context control) the actor executes at commit time,
/// or an output plus an engine query the runtime resolves against the
/// context engine (tools never touch the engine — invariant 3).
pub enum ToolOutcome {
    /// The execution produced only an output; there is nothing to commit.
    Value(ToolOutput),
    /// The computation finished and a side effect is staged. `output` is
    /// what the model sees after the runtime commits the effect.
    PreparedEffect {
        output: ToolOutput,
        effect: Box<dyn Effect>,
    },
    /// The computation finished and the tool asks the runtime to change
    /// runtime-owned state. Executed at operation-commit time, right after
    /// any staged effect, so "manual collect now" is actually now.
    RuntimeDirective {
        output: ToolOutput,
        directive: RuntimeDirective,
    },
    /// The computation finished and the tool asks the runtime to resolve a
    /// read-only query against the context engine (search/inspect/fetch
    /// over externalized refs). The placeholder `output` becomes the final
    /// tool output once the engine answers.
    EngineQuery {
        output: ToolOutput,
        query: EngineQuery,
    },
}

/// A read-only query the runtime resolves against the context engine. Only
/// builtin context tools produce these (a capability cannot emit an engine
/// query through `CapabilityOutcome`), and they carry no side effects — the
/// engine stamps access on fetch so recency ranking stays honest.
#[derive(Debug, Clone)]
pub enum EngineQuery {
    /// Deterministic search over externalized refs (entity/kind/scope/task
    /// filters + recency). `limit` caps the answer.
    SearchExternal {
        query: String,
        kind: Option<ContextKind>,
        scope: Option<ContextScope>,
        task_id: Option<TaskId>,
        limit: usize,
    },
    /// Metadata of one externalized entry by item id (no store read).
    InspectExternal { item_id: ContextItemId },
    /// Full content of one externalized item, pulled back deliberately.
    FetchExternal { item_id: ContextItemId },
}

impl std::fmt::Debug for ToolOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolOutcome::Value(output) => f.debug_tuple("Value").field(output).finish(),
            ToolOutcome::PreparedEffect { output, .. } => f
                .debug_struct("PreparedEffect")
                .field("output", output)
                .field("effect", &"<staged effect>")
                .finish(),
            ToolOutcome::RuntimeDirective { output, directive } => f
                .debug_struct("RuntimeDirective")
                .field("output", output)
                .field("directive", directive)
                .finish(),
            ToolOutcome::EngineQuery { output, query } => f
                .debug_struct("EngineQuery")
                .field("output", output)
                .field("query", query)
                .finish(),
        }
    }
}

/// One immutable view of the tool surface captured at a runtime safe point.
/// A model round captures exactly one snapshot after the tool lifecycle GC
/// and uses it for everything: the budget, the prompt and tool-call
/// validation. `generation` is the catalog generation at capture time, so a
/// round's surface is auditably identifiable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSurfaceSnapshot {
    pub specs: Vec<ToolSpec>,
    pub generation: u64,
}

/// The unified tool/capability lifecycle shared by the builtin catalog and
/// dynamic capabilities. `Available` entries are known but not on the model
/// surface; `Loaded` entries are offered to the model; `Active` is executing
/// right now; `Warm`/`Unloaded` are the idle-cooling steps of the builtin
/// catalog. Disabled/quarantined states map onto `Unloaded` in the current
/// runtime — the maturity ladder (`CapabilityStatus`) carries that policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolLifecycle {
    /// Known to the catalog but not offered to the model.
    Available,
    /// In the active set: its schema is exposed to the model.
    Loaded,
    /// Executing a call right now.
    Active,
    /// Idle; kept only for a fast reload.
    Warm,
    /// Removed from the model surface by the GC or an explicit unload.
    Unloaded,
}

impl ToolLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Loaded => "loaded",
            Self::Active => "active",
            Self::Warm => "warm",
            Self::Unloaded => "unloaded",
        }
    }

    /// Whether the tool's schema is part of the model surface right now.
    pub fn in_surface(self) -> bool {
        matches!(self, Self::Loaded | Self::Active)
    }
}

/// One row of the unified tool catalog (the discovery surface shared by
/// `capability.search` / `capability.inspect`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCatalogEntry {
    pub name: String,
    pub state: ToolLifecycle,
    /// Who owns the tool: `builtin` or the capability id (e.g. `ext:github`).
    pub owner: String,
    pub description: String,
}

/// Always-visible control tools of the unified catalog: the model can
/// discover and change the active set no matter what else is loaded.
pub const CAPABILITY_SEARCH: &str = "capability.search";
pub const CAPABILITY_LOAD: &str = "capability.load";
pub const CAPABILITY_UNLOAD: &str = "capability.unload";
pub const CAPABILITY_INSPECT: &str = "capability.inspect";

#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    /// The current tool surface. MUST be pure: a model round reads it for
    /// the budget, the prompt and tool-call validation, so the surface has
    /// to stay stable within a round.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Capture the tool surface for one model round. The runtime calls this
    /// once per round, right after `gc()`, and threads the snapshot through
    /// budget, prompt and validation so the model always sees — and the
    /// runtime always validates against — the same surface.
    fn snapshot(&self) -> ToolSurfaceSnapshot {
        ToolSurfaceSnapshot {
            specs: self.specs(),
            generation: 0,
        }
    }

    /// Explicit lifecycle maintenance safe point, called by the runtime
    /// once per model round. Dispatchers with mutable tool lifecycles
    /// (idle aging, unloading) run their GC here — never inside `specs()`.
    fn gc(&self) {}

    /// Unified discovery surface: every known tool (builtin and dynamic
    /// capability) with its lifecycle state and owner, for
    /// `capability.search` / `capability.inspect`. Default: no rows, so
    /// dispatchers without a catalog keep working unchanged.
    fn catalog(&self) -> Vec<ToolCatalogEntry> {
        Vec::new()
    }

    /// Unified `capability.load`: put a tool (or the capability owning it)
    /// on the model surface. Default: unsupported.
    fn load_tool(&self, name: &str) -> AgentResult<()> {
        Err(AgentError::InvalidRequest(format!(
            "this tool provider does not support loading '{name}'"
        )))
    }

    /// Unified `capability.unload`: remove a tool from the model surface.
    fn unload_tool(&self, name: &str) -> AgentResult<()> {
        Err(AgentError::InvalidRequest(format!(
            "this tool provider does not support unloading '{name}'"
        )))
    }

    /// Unified `capability.inspect`: the full spec of one tool, if known.
    fn inspect_tool(&self, _name: &str) -> Option<ToolSpec> {
        None
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome>;
}
