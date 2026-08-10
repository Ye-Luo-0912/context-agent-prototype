use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    AgentResult, ContextConsumptionAck, ContextDiagnostics, ContextGcReport,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextSelection, ContextStateTransition,
    OperationId, RunId, ScopeId, TaskId, ToolCall, ToolOutput, ToolSurfacePlanReport,
    ToolSurfaceRequirement, TurnId,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEventEnvelope {
    pub run_id: RunId,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub event: RuntimeEvent,
}

/// Old / restored / effective values of one runtime revision across a
/// restore. `effective` is what the live runtime uses after rebase — it
/// never moves backwards, so an old checkpoint cannot alias a surface
/// prepared before the restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreRevision {
    /// Value the live runtime had before the restore.
    pub old: u64,
    /// Value recorded in the checkpoint.
    pub restored: u64,
    /// Value in effect after the rebase (max of the two, plus the restore
    /// epoch bump where the runtime owns the revision).
    pub effective: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    RunStarted,
    UserMessageAccepted {
        content: String,
    },
    FocusChanged {
        task_id: TaskId,
        goal: String,
    },
    FocusCleared,
    /// The runtime-owned tool-requirement slice of a Task changed through a
    /// bounded whole-set CAS update. Requirements describe task demand only;
    /// they do not enable capabilities or grant effect authority.
    TaskToolRequirementsChanged {
        task_id: TaskId,
        revision: u64,
        requirements: Vec<ToolSurfaceRequirement>,
    },
    Pinned {
        content: String,
    },
    ContextPrepared {
        diagnostics: ContextDiagnostics,
        #[serde(default)]
        selected: Vec<ContextSelection>,
    },
    /// A successful model operation consumed exactly this bounded subset of
    /// one materialization preview. Failed/cancelled/refused/stale operations
    /// emit no acknowledgement and receive no access reinforcement.
    ContextConsumed {
        ack: ContextConsumptionAck,
    },
    ContextMaintained {
        #[serde(default)]
        trigger: ContextMaintenanceTrigger,
        report: ContextMaintenanceReport,
    },
    /// A full GC pass ran: roots were marked, unmarked items were evicted to
    /// the reversible buffer and/or reactivated. The report explains every
    /// eviction and reactivation.
    ContextGc {
        report: ContextGcReport,
    },
    /// One bounded, schema-free account of the final round surface decision.
    /// A Ready report is emitted before ModelStarted; an Unsatisfiable report
    /// means the provider was not called.
    ToolSurfacePlanned {
        report: ToolSurfacePlanReport,
    },
    /// A model round started. Carries the operation identity so live
    /// consumers (the UI's run-state aggregator) can fence streamed deltas:
    /// a delta whose turn/operation/generation no longer matches the
    /// current round belongs to a superseded turn and must be dropped.
    ModelStarted {
        turn_id: TurnId,
        operation_id: OperationId,
        generation: u64,
        #[serde(default)]
        surface_revision: u64,
        #[serde(default)]
        model_round: usize,
    },
    /// Live streamed text delta. Never journaled (the final `AssistantMessage`
    /// carries the complete content); only forwarded to live subscribers.
    /// The operation identity is the fence: a late delta from a cancelled
    /// turn must not be rendered into the next turn's transcript.
    ModelDelta {
        turn_id: TurnId,
        operation_id: OperationId,
        generation: u64,
        delta: String,
    },
    AssistantMessage {
        content: String,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        output: ToolOutput,
    },
    /// A tool frame closed: the runtime published the lifecycle transitions
    /// the close produced (durable outcomes promoted out of the tool frame),
    /// so a tool scope close is an auditable result instead of a silent
    /// discard. A failed close is reported as an `Error` event instead.
    ToolScopeClosed {
        scope_id: ScopeId,
        #[serde(default)]
        transitions: Vec<ContextStateTransition>,
    },
    Diagnostics {
        diagnostics: ContextDiagnostics,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
    /// One task completed: the runtime committed a typed CompletionRecord
    /// for it. Carries the task/result identity (task id and the anchor
    /// revision the outcome was measured against) plus the bounded summary;
    /// the full record lives in the runtime's task catalog, not the event.
    TaskCompleted {
        task_id: TaskId,
        anchor_revision: u64,
        summary: String,
    },
    /// A task's anchor was replaced through whole-set CAS. The event is the
    /// bounded audit row: task identity, the resulting revision, and the
    /// names of the fields whose content moved (capped). Full anchor content
    /// lives in RuntimeCheckpoint, never in the event stream.
    TaskAnchorChanged {
        task_id: TaskId,
        revision: u64,
        #[serde(default)]
        changed_fields: Vec<String>,
    },
    TurnCompleted,
    /// A mandatory turn-commit step failed: the model answered, but the
    /// runtime did not durably commit the turn (observation ingest,
    /// maintenance, GC or a journal event failed). The turn is NOT
    /// completed; `phase` names the exact step recovery must look at. The
    /// runtime drops the turn frame on the first failure — later writes
    /// would compound the inconsistency.
    TurnCommitFailed {
        phase: String,
        message: String,
    },
    /// The runtime detected state it cannot reconcile by itself (a failed
    /// turn commit, a journal/effect disagreement). Operators and future
    /// crash-recovery machinery must intervene before normal operation
    /// resumes with full consistency guarantees.
    RecoveryRequired,
    /// A restore committed: context + task authority were transactionally
    /// restored, and the host re-applied the checkpoint's capability flags.
    /// This is the bounded audit record of that commit. If it cannot be
    /// journaled, the runtime keeps the restored state but demands
    /// recovery — a restore must not outrun its own audit event.
    RuntimeRestored {
        checkpoint_version: u32,
        /// Run that produced the checkpoint (the restored run identity).
        restored_run_id: RunId,
        /// Run that restored it (the live run; equal to `restored_run_id`
        /// for an in-process round-trip).
        current_run_id: RunId,
        focus_revision: RestoreRevision,
        surface_revision: RestoreRevision,
        /// How many task tool-requirement revisions were rebased past the
        /// live high-water mark so they cannot move backwards.
        rebased_tasks: usize,
        /// Capped sample of the rebased task ids (artifact spill carries
        /// the full detail).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rebased_task_sample: Vec<TaskId>,
        /// Whether the checkpoint carried capability surface state that
        /// the host re-applied.
        capabilities_applied: bool,
    },
    /// A model round completed and the provider reported usage. Emitted at
    /// turn-commit time, so live consumers (the eval harness, a token meter)
    /// can measure the true cost of a turn without parsing provider
    /// internals. `input_tokens`/`output_tokens` are `0` when the provider
    /// did not report them.
    ModelUsed {
        input_tokens: u64,
        output_tokens: u64,
    },
    RunCompleted,
}

#[async_trait]
pub trait EventJournal: Send + Sync {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()>;

    async fn flush(&self) -> AgentResult<()> {
        Ok(())
    }
}
