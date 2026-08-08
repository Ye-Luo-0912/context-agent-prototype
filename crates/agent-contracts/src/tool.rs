use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentError, AgentResult, CancellationToken, RunId};

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

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput>;
}
