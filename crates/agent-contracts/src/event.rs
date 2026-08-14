use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    AgentResult, ContextConsumptionAck, ContextDiagnostics, ContextGcReport,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextSelection, ContextStateTransition,
    OperationId, OperationSnapshot, RunId, RuntimeInputEnvelope, ScopeId, StorageGcReport, TaskId,
    ToolCall, ToolOutput, ToolSurfacePlanReport, ToolSurfaceRequirement, TurnId,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEventEnvelope {
    pub run_id: RunId,
    /// Cursor in the durable event journal for this run. Journaled events
    /// advance it exactly once and are therefore contiguous from 1.
    /// `ModelDelta` is live-only: it repeats the cursor of the preceding
    /// `ModelStarted` and never consumes a durable sequence number. Live
    /// consumers must use the delta's turn/operation/generation identity as
    /// its supersession fence, not treat this cursor as a delivery counter.
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

/// Authority split of one task-anchor patch. Autonomous patches touch only
/// runtime-evolvable fields (interpretation, plan, open loops, criteria,
/// refs) and apply without confirmation; boundary patches touch user
/// authority (goal, constraints/waiver) and must clear the approval gate
/// first. The split is task-anchor policy, so it lives in the contract the
/// runtime and its consumers both see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorPatchKind {
    /// Runtime-evolvable fields only; applied directly, no approval round.
    #[default]
    Autonomous,
    /// Goal / constraints / waiver touched; the patch had to clear the
    /// approval gate before it reached the task table.
    Boundary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    RunStarted,
    /// 用户输入入账。`input.lifecycle` 区分 Applied（已 ingest）与
    /// Rejected（忙/非法，未 ingest）。事件名保留兼容；不是“一律成功”。
    UserMessageAccepted {
        #[serde(flatten)]
        input: RuntimeInputEnvelope,
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
        /// Engine materialization latency in milliseconds (the engine's
        /// own `materialize` call, not the runtime's later rendering
        /// overhead). Default preserves old wire/checkpoint rows.
        #[serde(default)]
        materialize_ms: u64,
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
    /// A conservative Storage GC pass ran at a runtime boundary (task
    /// completion): semantically dead, retention-expired store entries with
    /// no live dependency were permanently deleted. Storage GC is the only
    /// place information is deleted, it never runs on the per-model hot
    /// path, and the report names every deletion reason.
    StorageGc {
        report: StorageGcReport,
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
    /// Core durably admitted one logical tool operation before Runtime made
    /// it executable. The full bounded snapshot is the discovery identity
    /// for authorized Platform observers; Core's operation WAL, not this
    /// event stream, remains the query/recovery authority.
    OperationAccepted {
        snapshot: Box<OperationSnapshot>,
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
    /// bounded audit row: task identity, the resulting revision, the names
    /// of the fields whose content moved (capped), and the authority split
    /// of the patch that moved them. Full anchor content lives in
    /// RuntimeCheckpoint, never in the event stream.
    TaskAnchorChanged {
        task_id: TaskId,
        revision: u64,
        #[serde(default)]
        changed_fields: Vec<String>,
        /// Whether the patch applied autonomously (runtime-evolvable
        /// fields only) or after clearing the approval gate (goal /
        /// constraints / waiver touched). Default preserves old
        /// wire/checkpoint rows.
        #[serde(default)]
        patch_kind: AnchorPatchKind,
    },
    /// The turn fully committed its model result and every mandatory context
    /// write behind the durable event barrier. This is the only successful
    /// turn-commit marker used by crash recovery.
    TurnCompleted,
    /// The runtime fenced an active turn without committing it. This event
    /// has its own durability barrier, but it is deliberately not a
    /// `TurnCompleted`: recovery must never count cancellation as a
    /// successful model/context commit.
    TurnCancelled {
        turn_id: TurnId,
        task_id: Option<TaskId>,
        operation_id: Option<OperationId>,
        cancelled_generation: u64,
        effective_generation: u64,
        reason: crate::TurnCancellationReason,
    },
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
    /// A shadow-mode approval decision (ACI v2 compatibility order step 4):
    /// the v2 intent-derived verdict recorded beside the legacy gate. Only
    /// emitted when a shadow gate is configured; the legacy gate's decision
    /// is the one that runs. `legacy_allowed` records whether the legacy
    /// path allowed the call, so the invariant trace can prove the shadow
    /// gate never grants beyond the legacy gate.
    ShadowDecision {
        call_name: String,
        legacy_allowed: bool,
        shadow: crate::approval::ShadowVerdict,
    },
    /// A short-lived authority lease was minted for one side-effecting
    /// tool call (ACI v2 §6): the legacy path allowed the call, and the
    /// runtime now holds a bounded commit-time authorization for its
    /// operation generation. The audit row carries the lease identity,
    /// the call, the covering grant (when the v2 shadow gate granted the
    /// intent) and the expiry; the full lease (intent, issued instant)
    /// travels with the operation, not the event.
    LeaseIssued {
        lease_id: String,
        call_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant_id: Option<String>,
        expires_at_ms: u64,
    },
    RunCompleted,
}

impl RuntimeEvent {
    /// 短对话入账：预览即正文。回放场景与旧测试用。
    pub fn user_message_accepted(body: &str) -> Self {
        Self::UserMessageAccepted {
            input: RuntimeInputEnvelope::from_preview(body),
        }
    }
}

#[async_trait]
pub trait EventJournal: Send + Sync {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()>;

    async fn flush(&self) -> AgentResult<()> {
        Ok(())
    }
}
