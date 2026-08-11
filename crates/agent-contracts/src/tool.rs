use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AgentError, AgentResult, CancellationToken, ContextAction, ContextItemId, ContextKind,
    ContextScope, RunId, TaskId, TurnId,
};

/// Hard cap on the model-facing `model_content` of a tool result (chars).
/// Producers may choose smaller limits; none may make the model-facing
/// result larger. Matches the reference context engine's per-item ceiling.
pub const MAX_TOOL_MODEL_CONTENT_CHARS: usize = 16_000;

/// Hard cap on the `summary` field of a tool result (chars).
pub const MAX_TOOL_SUMMARY_CHARS: usize = 2_000;

/// Hard cap on the serialized size of the `metadata` field of a tool
/// result (bytes of JSON). Oversized metadata is replaced by a bounded
/// marker that keeps the decoded total honest.
pub const MAX_TOOL_METADATA_BYTES: usize = 8_000;

/// Decoded-total cap: the model-facing view (`summary` + `model_content` +
/// serialized `metadata`) must stay under this many chars even when each
/// field individually fits. The broker trims `model_content` first.
pub const MAX_TOOL_OUTPUT_TOTAL_CHARS: usize = 24_000;

/// Cap on `context.search` limit enforced in execution (the JSON schema
/// advertises the same maximum; execution is authoritative).
pub const CONTEXT_SEARCH_MAX_LIMIT: usize = 50;

/// A trusted output broker: bounds every model-facing field of a tool
/// output and spills oversized content to an artifact once, returning a
/// bounded preview plus a reference. The kernel applies it before a
/// `ToolOutcome` reaches the actor, so a producer that did not spill cannot
/// lose the truncated middle and no field can blow past its cap.
///
/// `budget` is the declaring tool's own model-content cap (`ToolSpec.
/// output_budget`): `Some(n)` bounds `model_content` at `n` chars (never
/// above the global cap), `None` falls back to the global cap. Producers
/// may choose smaller limits; none may make the model-facing result larger.
#[async_trait]
pub trait OutputBroker: Send + Sync {
    /// Bound `output` under the run's artifact store. `run_id` selects the
    /// artifact directory so spills land with the run's other artifacts.
    async fn bound(&self, run_id: RunId, budget: Option<usize>, output: ToolOutput) -> ToolOutput;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ToolRisk {
    ReadOnly,
    WorkspaceWrite,
    ProcessExecution,
}

/// The normalized side effect one tool call intends to perform, derived from
/// the validated arguments — never from the tool's self-declared risk
/// alone. Approval/policy matches this concrete intent, and the executor
/// proves the actual effect fits it before commit. It is a conservative
/// upper bound by design: a workspace write carries its target path and a
/// byte estimate of the content, a process run its lexical command prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectIntent {
    /// A read-only call; nothing to authorize.
    ReadOnly,
    /// A workspace write: the workspace-relative target path and a
    /// conservative byte estimate of the content being written.
    WorkspaceWrite { path: String, content_bytes: u64 },
    /// A process run: the lexical command (whitespace-separated tokens).
    ProcessRun { command: String },
}

impl EffectIntent {
    /// The effect class the intent belongs to — the bridge to the legacy
    /// `ToolRisk` grant vocabulary until grants move to typed permissions.
    pub fn risk(&self) -> ToolRisk {
        match self {
            Self::ReadOnly => ToolRisk::ReadOnly,
            Self::WorkspaceWrite { .. } => ToolRisk::WorkspaceWrite,
            Self::ProcessRun { .. } => ToolRisk::ProcessExecution,
        }
    }
}

/// Derive the conservative `EffectIntent` upper bound from validated tool
/// arguments. This is the shared normalization both the legacy
/// standing-grant gate and the v2 lease/`AuthorityGate` path use:
/// approval matches the concrete intent, never the tool name. Fail-closed:
/// a missing argument yields the empty/zero bound, which can never match a
/// grant (an empty path has no prefix, an empty command has no tokens).
pub fn derive_effect_intent(call: &ToolCall, spec: &ToolSpec) -> EffectIntent {
    match spec.risk {
        ToolRisk::ReadOnly => EffectIntent::ReadOnly,
        ToolRisk::WorkspaceWrite => {
            let path = call
                .arguments
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let content_bytes = call
                .arguments
                .get("content")
                .or_else(|| call.arguments.get("new"))
                .and_then(|value| value.as_str())
                .map(|content| content.len() as u64)
                .unwrap_or(0);
            EffectIntent::WorkspaceWrite {
                path,
                content_bytes,
            }
        }
        ToolRisk::ProcessExecution => {
            let command = call
                .arguments
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            EffectIntent::ProcessRun { command }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: ToolRisk,
    /// Declared model-content cap for this tool's results, in chars. A
    /// tool that produces verbose output declares a smaller budget so the
    /// broker spills sooner; `None` uses the global
    /// `MAX_TOOL_MODEL_CONTENT_CHARS`. Execution is authoritative — the
    /// broker enforces `min(budget, global)` regardless of the declared
    /// value, so a declaration can never exceed the hard cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_budget: Option<usize>,
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

/// What happens to a tool result after the turn: whether it becomes a new
/// long-term observation or stays transient.
///
/// Context retrieval (`context.search` / `context.inspect` / `context.fetch`)
/// is a transient store read: it must make evidence visible to the current
/// turn without duplicating it under a new observation id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolResultDisposition {
    /// Persist the result as a new `ToolObservation` (default).
    #[default]
    PersistObservation,
    /// The result is visible to the current turn only; it is not persisted
    /// as a new observation. Access stamps (last_access, GC epoch) are still
    /// recorded by the engine where the read happened.
    TransientNoPersist,
    /// The result is visible to the current turn only and is not persisted
    /// as a new observation — but the referenced item itself receives a
    /// lifecycle/access event (`context.admit`: the item re-enters the
    /// working set under its original id, one transition). Persisting the
    /// directive result would duplicate the admitted item under a new id,
    /// so the event *is* the record.
    AccessEventOnly,
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
    /// Apply the prepared effect (atomic rename, outbox send, ...) and
    /// return the ACI v2 receipt: what happened to the world, how durably
    /// it is recorded, and the evidence reference. `NotApplied` leaves the
    /// world unchanged; `Applied` with `DurabilityFailed` means the effect
    /// landed but its record could not be persisted — the runtime must
    /// treat that as a degraded/recovery state, never as "nothing
    /// happened"; `Unknown` is for remote operations whose applied state
    /// can never be learned back.
    async fn commit(self: Box<Self>) -> EffectReceipt;
    /// Undo the preparation: the effect must not land.
    async fn rollback(self: Box<Self>, reason: &str);
}

/// How durably an applied effect is recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EffectDurability {
    /// Fully recorded; the world and the journal agree.
    Durable,
    /// The effect landed but its record could not be persisted; the world
    /// and the journal disagree. Recovery is required.
    DurabilityFailed(String),
}

/// The outcome of committing a staged effect (ACI v2, compatibility order
/// step 5): what happened to the world, how durably it is recorded, and
/// the evidence reference. `EffectCommitError` semantics are preserved —
/// `NotApplied` and `AppliedButDurabilityFailed` map one-to-one — so the
/// journal format and the model-facing messages stay the same; the receipt
/// just carries them in one typed, serializable, evidence-bearing shape.
/// Errors travel as message strings (receipts are result envelopes for
/// events and logs, not error objects).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EffectReceipt {
    /// The effect did not land; there is nothing to recover.
    NotApplied { error: String },
    /// The effect landed. `evidence` is a stable change id / reference for
    /// review or recovery (e.g. the workspace mutation transaction id).
    Applied {
        durability: EffectDurability,
        evidence: Option<String>,
    },
    /// The applied state is unknowable (a remote operation whose result
    /// never returned). Never blindly retried without an idempotency key.
    Unknown { error: String },
}

impl EffectReceipt {
    /// A short human-readable summary for logs and model-facing messages.
    pub fn summary(&self) -> String {
        match self {
            Self::NotApplied { error } => format!("effect not applied: {error}"),
            Self::Applied {
                durability: EffectDurability::Durable,
                ..
            } => "effect applied".to_string(),
            Self::Applied {
                durability: EffectDurability::DurabilityFailed(error),
                ..
            } => format!("effect applied but its journal record failed: {error}"),
            Self::Unknown { error } => format!("effect applied state unknown: {error}"),
        }
    }
}

impl From<EffectCommitError> for EffectReceipt {
    fn from(error: EffectCommitError) -> Self {
        match error {
            EffectCommitError::NotApplied(error) => Self::NotApplied {
                error: error.to_string(),
            },
            EffectCommitError::AppliedButDurabilityFailed(error) => Self::Applied {
                durability: EffectDurability::DurabilityFailed(error.to_string()),
                evidence: None,
            },
        }
    }
}

/// Several effects committed in order as one operation. Used by the
/// process-capability adapter when a child declares more than one wire
/// effect: each is staged through its own confined handle, and this
/// composite commits them one after the other behind the generation fence.
/// Each sub-effect is itself atomic; a mid-list failure stops the rest and
/// reports the failing receipt — effects already committed stay committed
/// (they are separate atomic operations, not one transaction), and their
/// evidence references are aggregated into the final receipt.
#[async_trait::async_trait]
impl Effect for Vec<Box<dyn Effect>> {
    fn describe(&self) -> String {
        format!("composite of {} staged effects", self.len())
    }

    async fn commit(self: Box<Self>) -> EffectReceipt {
        let mut evidence: Vec<String> = Vec::new();
        for effect in (*self).into_iter() {
            match effect.commit().await {
                EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence: Some(id),
                } => {
                    evidence.push(id);
                }
                EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence: None,
                } => {}
                receipt => {
                    // The failing effect stops the list. Its own receipt
                    // carries its error; the already-committed evidence is
                    // attached so recovery knows what landed.
                    return match receipt {
                        EffectReceipt::Applied {
                            durability,
                            evidence: own,
                        } => EffectReceipt::Applied {
                            durability,
                            evidence: Some(evidence_ids(own, &evidence)),
                        },
                        other => other,
                    };
                }
            }
        }
        EffectReceipt::Applied {
            durability: EffectDurability::Durable,
            evidence: Some(evidence.join(",")),
        }
    }

    async fn rollback(self: Box<Self>, reason: &str) {
        for effect in (*self).into_iter() {
            effect.rollback(reason).await;
        }
    }
}

/// Combine a sub-effect's own evidence with the already-committed evidence
/// of earlier sub-effects.
fn evidence_ids(own: Option<String>, committed: &[String]) -> String {
    let mut ids: Vec<String> = committed.to_vec();
    if let Some(id) = own {
        ids.push(id);
    }
    ids.join(",")
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

/// Maximum number of exact tool requirements one task may retain. This is a
/// runtime resource-policy bound, not a model-context packing hint.
pub const MAX_TASK_TOOL_REQUIREMENTS: usize = 32;
/// Defensive bounds for task-owned requirement metadata and event rows.
pub const MAX_TOOL_REQUIREMENT_NAME_CHARS: usize = 96;
pub const MAX_TOOL_REQUIREMENT_REASON_CHARS: usize = 160;
/// Defensive bounds for the actor-owned TaskAnchor contract.
/// Each free-text anchor field is capped; each list (constraints, criteria,
/// plan steps, open loops) and each typed root-claim list is capped in
/// length and per-entry size. An event row names at most this many changed
/// fields so the audit event stays bounded regardless of anchor size.
pub const MAX_TASK_ANCHOR_TEXT_CHARS: usize = 2_000;
pub const MAX_TASK_ANCHOR_LIST_ITEMS: usize = 32;
pub const MAX_TASK_ANCHOR_ITEM_CHARS: usize = 200;
pub const MAX_TASK_ANCHOR_CLAIMS: usize = 32;
pub const MAX_TASK_ANCHOR_CHANGED_FIELDS: usize = 8;
/// Defensive bounds for one typed CompletionRecord.
pub const MAX_COMPLETION_SUMMARY_CHARS: usize = 2_000;
pub const MAX_COMPLETION_REF_CHARS: usize = 256;
pub const MAX_COMPLETION_ARTIFACTS: usize = 32;
pub const MAX_TOOL_SURFACE_REPORT_SELECTED: usize = 32;
pub const MAX_TOOL_SURFACE_REPORT_OMITTED: usize = 32;
pub const MAX_TOOL_SURFACE_REPORT_BLOCKED: usize = 32;
/// UTF-8 byte cap for every tool name copied into a round-plan event row.
pub const MAX_TOOL_SURFACE_REPORT_NAME_BYTES: usize = 64;
/// Hard wire cap for one serialized round-surface report event. Covers the
/// worst-case fixed overhead of every row at all row/name maxima, including
/// the per-row `origin` provenance field.
pub const MAX_TOOL_SURFACE_REPORT_WIRE_BYTES: usize = 18 * 1024;

/// A task's demand for one exact tool id. This is orthogonal to catalog
/// lifecycle and authority: it neither enables a capability nor grants an
/// approval/effect permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceDemand {
    KeepReady,
    PreferSurface,
    MustSurface,
}

impl ToolSurfaceDemand {
    /// Explicit priority for conflict resolution and deterministic reports.
    pub const fn rank(self) -> u8 {
        match self {
            Self::KeepReady => 1,
            Self::PreferSurface => 2,
            Self::MustSurface => 3,
        }
    }
}

/// One exact, bounded task-owned tool requirement. `reason` is explanatory
/// metadata only; it never enters a provider request or changes authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceRequirement {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceOmissionReason {
    KeepReady,
    SchemaBudget,
    ProviderInputBudget,
    Unavailable,
}

impl ToolSurfaceOmissionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KeepReady => "kept ready outside the prompt",
            Self::SchemaBudget => "round schema budget",
            Self::ProviderInputBudget => "provider input budget",
            Self::Unavailable => "not available at the safe point",
        }
    }
}

/// Why a MustSurface requirement prevented a model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceBlockReason {
    Unavailable,
    SchemaBudget,
    ProviderInputBudget,
}

/// Which authority put one tool into the round surface. Rows with the same
/// `demand` can now answer whether they entered because of Task intent, a
/// fail-closed dispatcher/core policy, or an ordinary catalog load. `Unknown`
/// marks rows predating per-row provenance (old journal events), so no
/// legacy row ever pretends to know its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceOrigin {
    /// Legacy row without provenance information.
    #[default]
    Unknown,
    /// Task-owned requirement (task requirement revision / reason ref).
    TaskRequirement,
    /// Fail-closed dispatcher/core policy (`may_omit` returned false).
    DispatcherRequired,
    /// Loaded optional from the catalog (e.g. `capability.load`).
    CatalogLoadedOptional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceSelection {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    /// Which authority put this tool into consideration.
    #[serde(default)]
    pub origin: ToolSurfaceOrigin,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceOmission {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    /// Which authority put this tool into consideration.
    #[serde(default)]
    pub origin: ToolSurfaceOrigin,
    pub reason: ToolSurfaceOmissionReason,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceBlock {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    pub reason: ToolSurfaceBlockReason,
}

/// Exact source revisions used to derive one round surface. Optional fields
/// stay `None` until the corresponding policy plane exists; zero never
/// pretends to be a real Task/Focus/policy revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceSourceRevisions {
    pub builtin_catalog_generation: u64,
    pub capability_catalog_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_requirement_revision: Option<u64>,
    /// TaskAnchor revision the derived roots came from. None when no task
    /// is active (focus-only rounds have no anchor plane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_policy_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfacePlanStatus {
    Ready,
    Unsatisfiable { reason: ToolSurfaceBlockReason },
}

/// Bounded, schema-free audit record for one round's surface decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfacePlanReport {
    pub turn_id: TurnId,
    pub model_round: usize,
    pub surface_revision: u64,
    pub source_revisions: ToolSurfaceSourceRevisions,
    pub status: ToolSurfacePlanStatus,
    pub selected: Vec<ToolSurfaceSelection>,
    pub selected_total: usize,
    pub omitted: Vec<ToolSurfaceOmission>,
    pub omitted_total: usize,
    pub blocked: Vec<ToolSurfaceBlock>,
    pub blocked_total: usize,
    pub selected_schema_tokens: usize,
    pub mandatory_schema_tokens: usize,
    pub estimated_input_tokens: usize,
    pub input_budget_tokens: usize,
}

/// One immutable view of the tool surface captured at a runtime safe point.
/// A model round publishes exactly one final snapshot and uses it for the
/// budget, prompt and tool-call validation. `generation` remains a legacy
/// catalog display value; `surface_revision` is the unique round identity and
/// `source_revisions` preserves the non-colliding source generations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSurfaceSnapshot {
    pub specs: Vec<ToolSpec>,
    pub generation: u64,
    #[serde(default)]
    pub surface_revision: u64,
    #[serde(default)]
    pub source_revisions: ToolSurfaceSourceRevisions,
    /// Bounded host-only decisions used for precise call rejection. Valid
    /// tool names remain exact up to `MAX_TOOL_REQUIREMENT_NAME_CHARS`; the
    /// tighter event-display cap is applied only when constructing
    /// `ToolSurfacePlanReport`. Full diagnostics never enter the prompt.
    #[serde(default)]
    pub omissions: Vec<ToolSurfaceOmission>,
    #[serde(default)]
    pub omitted_total: usize,
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

/// The merged control surface: one `capability.manage` entry point (op =
/// search/inspect/load/unload) and one `context.manage` entry point (op =
/// gc_hint/tag/lease/collect/search/inspect/fetch) keep the always-visible
/// schema count small — a dozen single-purpose meta-tools would cost more
/// model input than the runtime control they provide.
pub const CAPABILITY_MANAGE: &str = "capability.manage";
pub const CONTEXT_MANAGE: &str = "context.manage";

/// `capability.search` paging bounds. A large catalog (hundreds of
/// capabilities) must never become context pollution: the model-facing
/// page is capped, and the full listing spills to an artifact.
pub const CAPABILITY_SEARCH_DEFAULT_LIMIT: usize = 20;
pub const CAPABILITY_SEARCH_MAX_LIMIT: usize = 50;

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
            source_revisions: ToolSurfaceSourceRevisions::default(),
            ..ToolSurfaceSnapshot::default()
        }
    }

    /// Whether this tool's schema may be omitted from one model round when
    /// the provider's final input budget is smaller than the captured tool
    /// surface. This is a pure classification query: omitting a schema from
    /// a round must not unload the tool or otherwise change catalog state.
    ///
    /// The default is fail-closed because a dispatcher that cannot
    /// distinguish its core/required tools from optional tools must never
    /// let token pressure silently hide one of them.
    fn may_omit_from_round(&self, _name: &str) -> bool {
        false
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn effect_intent_round_trips_and_maps_to_risk() {
        for intent in [
            EffectIntent::ReadOnly,
            EffectIntent::WorkspaceWrite {
                path: "src/main.rs".into(),
                content_bytes: 42,
            },
            EffectIntent::ProcessRun {
                command: "cargo test".into(),
            },
        ] {
            let value = serde_json::to_value(&intent).unwrap();
            let back: EffectIntent = serde_json::from_value(value).unwrap();
            assert_eq!(back, intent);
        }
        assert_eq!(EffectIntent::ReadOnly.risk(), ToolRisk::ReadOnly);
        assert_eq!(
            EffectIntent::WorkspaceWrite {
                path: "x".into(),
                content_bytes: 1
            }
            .risk(),
            ToolRisk::WorkspaceWrite
        );
        assert_eq!(
            EffectIntent::ProcessRun {
                command: "x".into()
            }
            .risk(),
            ToolRisk::ProcessExecution
        );
    }

    /// Records whether it was committed or rolled back, optionally failing
    /// its own commit — the observable trace for the composite semantics.
    struct RecordingEffect {
        label: &'static str,
        commits: Arc<AtomicUsize>,
        rollbacks: Arc<AtomicUsize>,
        fail_commit: bool,
    }

    #[async_trait::async_trait]
    impl Effect for RecordingEffect {
        fn describe(&self) -> String {
            self.label.into()
        }
        async fn commit(self: Box<Self>) -> EffectReceipt {
            self.commits.fetch_add(1, Ordering::SeqCst);
            if self.fail_commit {
                EffectReceipt::NotApplied {
                    error: "boom".into(),
                }
            } else {
                EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence: Some(self.label.into()),
                }
            }
        }
        async fn rollback(self: Box<Self>, _reason: &str) {
            self.rollbacks.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn composite(effects: Vec<Box<dyn Effect>>) -> Box<dyn Effect> {
        Box::new(effects)
    }

    #[tokio::test]
    async fn composite_effect_commits_every_sub_effect_in_order() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a",
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: false,
            }),
            Box::new(RecordingEffect {
                label: "b",
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: false,
            }),
            Box::new(RecordingEffect {
                label: "c",
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: false,
            }),
        ]);
        let receipt = effect.commit().await;
        assert!(
            matches!(
                &receipt,
                EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence,
                } if evidence.as_deref() == Some("a,b,c")
            ),
            "all three sub-effects commit and their evidence is aggregated: {receipt:?}"
        );
        assert_eq!(commits.load(Ordering::SeqCst), 3);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn composite_effect_stops_at_the_first_failure() {
        // A mid-list failure must stop the rest and report `NotApplied`:
        // effects already committed stay committed (they are separate
        // atomic operations), but nothing after the failure runs — a
        // cancelled operation cannot keep mutating the world.
        let ca = Arc::new(AtomicUsize::new(0));
        let cb = Arc::new(AtomicUsize::new(0));
        let cc = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a",
                commits: ca.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: false,
            }),
            Box::new(RecordingEffect {
                label: "b",
                commits: cb.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: true,
            }),
            Box::new(RecordingEffect {
                label: "c",
                commits: cc.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: false,
            }),
        ]);
        let receipt = effect.commit().await;
        assert!(
            matches!(receipt, EffectReceipt::NotApplied { .. }),
            "the composite must report the failed sub-effect: {receipt:?}"
        );
        assert_eq!(ca.load(Ordering::SeqCst), 1, "'a' committed first");
        assert_eq!(cb.load(Ordering::SeqCst), 1, "'b' attempted and failed");
        assert_eq!(
            cc.load(Ordering::SeqCst),
            0,
            "'c' must never run after the failure"
        );
        assert_eq!(rollbacks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn composite_effect_rolls_back_every_sub_effect() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a",
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: false,
            }),
            Box::new(RecordingEffect {
                label: "b",
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                fail_commit: false,
            }),
        ]);
        effect.rollback("superseded").await;
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 2);
    }
}
