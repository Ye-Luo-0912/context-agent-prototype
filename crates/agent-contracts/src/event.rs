use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    AgentResult, CompactionReason, ContextConsumptionAck, ContextDiagnostics, ContextGcReport,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextMaterializationMisses,
    ContextSelection, ContextStateTransition, EffectAckDebt, OperationId, OperationSnapshot, RunId,
    RuntimeFailureClass, RuntimeInputEnvelope, ScopeId, StorageGcReport, TaskId, ToolCall,
    ToolLeaseReconcileReport, ToolOutput, ToolSurfacePlanReport, ToolSurfaceRequirement, TurnId,
};

/// Cap on `RuntimeEvent::Pinned` content: a pinned constraint is model-facing
/// event text and shares the anchor text bound of other task-owned strings.
/// Oversized pins are rejected before the engine or the journal sees them.
pub const MAX_PINNED_CONTENT_CHARS: usize = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEventEnvelope {
    pub run_id: RunId,
    /// Cursor in the durable event journal for this run. Journaled events
    /// advance it exactly once and are therefore contiguous from 1.
    /// `ModelDelta` and `ModelRetrying` are live-only: they repeat the cursor
    /// of the preceding durable event and never consume a durable sequence
    /// number. Live consumers use their turn/operation/generation identity
    /// as the supersession fence, not this cursor as a delivery counter.
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

/// The runtime transaction whose audit prefix one explicit durable barrier
/// commits. Recovery keys off this marker itself rather than inferring
/// durability from unrelated lifecycle event names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCommitKind {
    /// Durable marker that opts this run into explicit-barrier replay. It
    /// commits no model turn, but prevents a partially appended first turn
    /// from being mistaken for a legacy `TurnCompleted`-only trace.
    RunStart,
    Turn,
    TaskCompletion,
}

/// Authority split of one task-anchor patch. Autonomous patches touch only
/// host/runtime-ingested fields (interpretation, plan, open loops, completion
/// policy/criteria and refs) and apply without confirmation; boundary patches
/// touch user authority (goal, constraints/waiver) and must clear the approval
/// gate first. Model-routable `task.manage` cannot submit completion authority.
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

/// Runtime decision boundary that caused optional schema leases to be
/// reconciled. This is lifecycle provenance, not a timeout or planning hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLeaseBoundary {
    /// The first model request for a newly applied user directive. Ephemeral
    /// leases from an aborted/older directive cannot cross this boundary.
    DirectiveStart,
    /// A successful model decision consumed the preceding results. Tools the
    /// decision calls are rooted until their results reach the next decision.
    ModelDecision,
}

/// Lifecycle of one bounded, workspace-version-bound negative execution
/// fact. These events contain identities only and are sufficient to audit why
/// a speculative miss was remembered, reused, invalidated or promoted into a
/// task obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NegativeFactEventKind {
    #[default]
    Recorded,
    Reused,
    Invalidated,
    Promoted,
    Resolved,
}

/// Lifecycle of one exact verification PASS receipt. A recorded receipt is
/// reusable only while every identity carried by its event remains current;
/// reuse is a no-dispatch terminal tool result, not a synthetic model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPassEventKind {
    #[default]
    Recorded,
    Reused,
}

/// How a reused PASS relates to the request it satisfied. `Exact` is the
/// landed same-recipe identity; `DomainEquivalent` marks a sibling recipe
/// from one host-declared coverage class. The domain id is a bounded
/// host-chosen label, never command text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerificationPassEquivalence {
    #[default]
    Exact,
    DomainEquivalent {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        domain_id: String,
    },
}

/// Lifecycle of one derived completion-opportunity observation. The
/// opportunity is advisory: it never commits a task by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionOpportunityDisposition {
    /// The derivation ran and the task is not closure-ready; `reason`
    /// names the blocking class.
    #[default]
    NotReady,
    /// The opportunity became eligible and Runtime leased `task.complete`
    /// onto the next decision surface.
    Offered,
    /// The model called `task.complete` while (or after) an offer was live.
    Called,
    /// The leased decision ended without calling; the lease is spent.
    Ignored,
    /// The proposal was refused by the acceptance gate.
    Refused,
    /// A typed CompletionRecord was committed for the task.
    Completed,
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
    /// Bounded final-frame degradation report. Required misses participate
    /// in completion readiness; optional misses are observability only. The
    /// event carries identities and reasons, never context bodies.
    ContextDegraded {
        turn_id: TurnId,
        #[serde(default)]
        model_round: usize,
        #[serde(default)]
        materialization_id: u64,
        #[serde(default)]
        required_misses: ContextMaterializationMisses,
        #[serde(default)]
        optional_misses: ContextMaterializationMisses,
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
    /// One bounded compaction pass. Cost accounting sums these events
    /// instead of guessing from diagnostics snapshots or maintain deltas.
    ContextCompacted {
        reason: CompactionReason,
        input_tokens: u64,
        output_tokens: u64,
        source_items: usize,
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
    /// One body-free account of optional schema residency reconciled from
    /// typed roots. Emitted only when a dispatcher had optional loaded rows
    /// to examine; the report's totals remain exact if its name sample is
    /// truncated.
    ToolLeasesReconciled {
        turn_id: TurnId,
        model_round: usize,
        boundary: ToolLeaseBoundary,
        report: ToolLeaseReconcileReport,
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
        #[serde(default)]
        prompt_layers: crate::PromptLayerCosts,
        /// Bounded TurnFrame projection accounting. Old traces default to
        /// zero; receipt text never enters the durable event stream.
        #[serde(default)]
        turn_checkpoint: crate::TurnCheckpointStats,
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
        /// Top-level trusted execution facts for this finished tool,
        /// versioned on the event wire. Replay prefers them over the
        /// output metadata fallback; older events without the field keep
        /// deriving from the reserved metadata key.
        #[serde(default)]
        facts: Option<crate::execution_facts::ExecutionFactsEnvelope>,
    },
    /// One model-requested tool batch settled in the actor. This body-free
    /// accounting is orthogonal to Context persistence: transient catalog /
    /// context reads and no-dispatch refusals still terminate actions here.
    /// `missing_terminal` and `unexpected_terminal` are hard integrity
    /// signals and should remain zero on a normally settled batch.
    ExecutionBatchSettled {
        turn_id: TurnId,
        model_round: usize,
        requested: usize,
        terminal: usize,
        spawned: usize,
        refused: usize,
        reused: usize,
        persist_observation: usize,
        transient_no_persist: usize,
        access_event_only: usize,
        succeeded: usize,
        failed: usize,
        known_mutation_results: usize,
        typed_verification_results: usize,
        unknown_invalidations: usize,
        completion_proposals: usize,
        outcome_advances: usize,
        no_outcome_results: usize,
        missing_terminal: usize,
        unexpected_terminal: usize,
    },
    /// 证据前沿账目：每个持久化工具观察一条。收敛指标（前沿推进数 /
    /// 冗余证据调用 / 无推进动作连击 / 证据失效数）从这里确定性聚合；
    /// 字段全部有界，不含任何工具正文。
    ExecutionFrontier {
        #[serde(default)]
        delta: crate::FrontierDelta,
        #[serde(default)]
        actions_since_frontier_advance: u32,
        #[serde(default)]
        evidence_revision: u64,
        /// 本轮因 world revision 推进而失效的前沿证据条数。
        #[serde(default)]
        invalidated: u64,
        /// 派生结算标签；仅在标签变化时填充，供结算后尾部
        /// rounds/calls 从事件流重建。
        #[serde(default)]
        settlement: Option<crate::SettlementLabel>,
    },
    /// 当轮正文缓存账目，每次模型输入组装出一条增量。
    /// eligible/hit/miss 为本次组装真实 checkpoint demand / 回注 /
    /// 未回注行数（仍在 retained tail 的缓存行不计 demand）；
    /// invalidated 为物理丢弃条数（Known mutation / LRU 挤出），
    /// suspended 为 Unknown footprint 挂起（休眠保留）的条数，
    /// oversize 为因超限拒缓存的条数；restored_body_tokens 为本次
    /// 回注正文的近似 token。恢复率由此可从事件流独立验证。
    ProtocolBodyCacheStats {
        #[serde(default)]
        eligible: u64,
        #[serde(default)]
        hit: u64,
        #[serde(default)]
        miss: u64,
        #[serde(default)]
        invalidated: u64,
        #[serde(default)]
        suspended: u64,
        #[serde(default)]
        oversize: u64,
        #[serde(default)]
        restored_body_tokens: u64,
    },
    /// 义务账本生命周期事件（typed、有界，不含任何工具
    /// 正文）。kind ∈ opened / attempted / precondition_changed /
    /// resolved / dropped / overflowed；scope_digest 是稳定的血统身份
    /// （ExecutableResolution = 解析上下文 digest），epoch 是前置指纹
    /// 的代数。收敛报告由此验证 max_attempts_per_epoch /
    /// max_total_attempts_per_lineage 等指标。
    ExecutionObligation {
        #[serde(default)]
        kind: crate::ObligationEventKind,
        #[serde(default)]
        domain: crate::ToolFailureDomain,
        #[serde(default)]
        scope_digest: String,
        #[serde(default)]
        epoch: u32,
        #[serde(default)]
        attempts_in_epoch: u32,
        #[serde(default)]
        total_attempts: u32,
    },
    /// A speculative, trusted path miss changed lifecycle. The event is
    /// body-free and revision-bound; it is not a transcript message or a
    /// task obligation by itself.
    ExecutionNegativeFact {
        #[serde(default)]
        kind: NegativeFactEventKind,
        tool_name: String,
        target: String,
        failure: crate::ToolFailureClass,
        #[serde(default)]
        workspace_revision: u64,
    },
    /// A trusted exact verifier recorded or reused a PASS under the same
    /// bounded task/directive/world/recipe identity. The event carries no
    /// verification body or command arguments.
    ExecutionVerificationPass {
        #[serde(default)]
        kind: VerificationPassEventKind,
        /// How the reuse relates to the skipped dispatch. Old events
        /// deserialize as `Exact`.
        #[serde(default)]
        equivalence: VerificationPassEquivalence,
        tool_name: String,
        argument_digest: String,
        /// SHA-256 digest of the host recipe/profile/policy/environment
        /// identity material; raw host environment data never enters events.
        verification_identity: String,
        #[serde(default)]
        anchor_revision: u64,
        #[serde(default)]
        directive_revision: u64,
        #[serde(default)]
        workspace_revision: u64,
    },
    /// Runtime minted criterion-addressed acceptance receipts from one
    /// already-observed trusted PASS. This event is body-free and bounded;
    /// its identities are sufficient to audit the matching decision and the
    /// corresponding TaskAnchor checkpoint state.
    AcceptanceReceiptsRecorded {
        task_id: TaskId,
        anchor_revision: u64,
        verification_revision: u64,
        criterion_indices: Vec<u32>,
        coverage_domain: String,
        domain_declaration_revision: u64,
        #[serde(default)]
        domain_source_digest: String,
        directive_revision: u64,
        workspace_revision: u64,
        verification_identity: String,
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
    /// A typed execution failure. Consumers use `class` for policy and keep
    /// `message` only as bounded diagnostics; provider/runtime attribution
    /// must never be reconstructed from message text.
    Failure {
        class: RuntimeFailureClass,
        #[serde(default)]
        retryable: bool,
        message: String,
    },
    /// A retryable provider attempt ended before emitting a usable result.
    /// This live-only progress signal keeps outer watchdogs honest while the
    /// transport performs its bounded retry policy.
    ModelRetrying {
        turn_id: TurnId,
        operation_id: OperationId,
        generation: u64,
        attempt: u32,
        delay_ms: u64,
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
    /// A `task.manage` progress proposal settled. On success this follows
    /// the matching `TaskAnchorChanged`; on refusal the task state is
    /// unchanged and `reason` names the refusal class (for example a stale
    /// base revision), so eval can prove CAS outcomes from JSONL alone.
    TaskProgressUpdated {
        task_id: TaskId,
        accepted: bool,
        /// Resulting anchor revision on success; unchanged revision on an
        /// idempotent no-op.
        #[serde(default)]
        anchor_revision: u64,
        #[serde(default)]
        changed_fields: Vec<String>,
        #[serde(default)]
        reason: String,
    },
    /// One body-free completion-opportunity observation (advisory, derived;
    /// never completion authority). `key` is the stable opportunity identity
    /// (empty for `not_ready`), `reason` carries the typed blocker class or
    /// the lease outcome. Emitted only while the host switch is enabled.
    CompletionOpportunity {
        disposition: CompletionOpportunityDisposition,
        task_id: TaskId,
        #[serde(default)]
        key: String,
        #[serde(default)]
        anchor_revision: u64,
        #[serde(default)]
        reason: String,
    },
    /// A fully settled batch carried checkpoint debt, so the runtime
    /// installed the bounded `ExecutionState` into the task resume and
    /// scheduled one atomic checkpoint write. `debt` names the coalesced
    /// reasons (anchor change, durable workspace mutation, verification
    /// change); read-only exploration never produces this event.
    TaskResumeCommitted {
        task_id: TaskId,
        #[serde(default)]
        anchor_revision: u64,
        #[serde(default)]
        debt: Vec<String>,
        /// Actor-owned monotonic snapshot sequence allocated for the frozen
        /// snapshot. Ordering authority for durability — independent of
        /// task-anchor revisions, so same-anchor snapshots and cross-task
        /// switches never alias.
        #[serde(default)]
        sequence: u64,
    },
    /// The scheduled background checkpoint write landed durably. Only after
    /// this event may the run be described as safely resumable up to the
    /// write's sequence. `sequence` is the actor-owned snapshot identity the
    /// artifact was captured under; `revision` remains the legacy anchor
    /// revision for observability only and carries no ordering meaning.
    /// `checksum` pins the artifact contents.
    CheckpointDurable {
        #[serde(default)]
        bytes: u64,
        /// Bounded file name of the atomic checkpoint artifact.
        #[serde(default)]
        artifact: String,
        /// Legacy anchor revision at capture; superseded by `sequence`.
        #[serde(default)]
        revision: u64,
        /// Bounded sha256 hex of the stored envelope, when recorded.
        #[serde(default)]
        checksum: String,
        /// Snapshot sequence this acknowledgement covers.
        #[serde(default)]
        sequence: u64,
        /// Capability-surface generation the captured plane was verified
        /// stable against. Zero when the composition runs without a
        /// capability registry.
        #[serde(default)]
        capability_generation: u64,
    },
    /// The background checkpoint write failed. Accrued debt stays visible
    /// and retryable at the next safe point; nothing may claim safe
    /// resumability from a run whose last write ended here.
    CheckpointWriteFailed {
        #[serde(default)]
        reason: String,
    },
    /// `continue_active_task` started a fresh active turn from the same
    /// task id, current directive, anchor and resume state. No new user
    /// instruction was minted and no directive identity changed.
    TaskContinuationStarted {
        task_id: TaskId,
        #[serde(default)]
        anchor_revision: u64,
    },
    /// Explicit commit marker appended in the same durable batch as the
    /// lifecycle events it covers. `checkpoint_sequence` is present for a
    /// terminal task commit whose task/context planes were first frozen in
    /// one atomic runtime checkpoint.
    RuntimeCommitBarrier {
        kind: RuntimeCommitKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint_sequence: Option<u64>,
    },
    /// The turn fully committed its model result and every mandatory context
    /// write. It is lifecycle audit inside the same durable batch as the
    /// explicit `RuntimeCommitBarrier`; recovery keys off that marker.
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
    /// A model-authored completion proposal was accepted by the operation
    /// gate, but its deferred terminal transaction did not commit. The task
    /// remains active and the bounded failure is also checkpointed in its
    /// resume projection so a later decision can correct or retry it.
    CompletionCommitFailed {
        task_id: TaskId,
        #[serde(default)]
        retryable: bool,
        reason: String,
    },
    /// The runtime detected state it cannot reconcile by itself (a failed
    /// turn commit, a journal/effect disagreement). Operators and future
    /// crash-recovery machinery must intervene before normal operation
    /// resumes with full consistency guarantees.
    RecoveryRequired,
    /// A committed effect's typed settlement could not be durably
    /// acknowledged. The receipt already happened (an applied effect is
    /// never rolled back); this debt names the broker reservation that must
    /// be reconciled before later mutation or completion proceeds.
    EffectAckDebt {
        debt: EffectAckDebt,
    },
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
    /// did not report them. `attempts`/`retries` come from the transport;
    /// failed attempts usually have no usage, so tokens are a lower bound
    /// whenever `retries > 0`.
    ModelUsed {
        input_tokens: u64,
        output_tokens: u64,
        /// Provider-reported prefix-cached input tokens for this round;
        /// `0` when the provider did not report cache details.
        #[serde(default)]
        cached_input_tokens: u64,
        #[serde(default)]
        attempts: u32,
        #[serde(default)]
        retries: u32,
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

/// Emit `ContextCompacted` rows then the `ContextMaintained` audit.
pub fn context_maintenance_events(
    trigger: ContextMaintenanceTrigger,
    report: ContextMaintenanceReport,
) -> Vec<RuntimeEvent> {
    let mut events: Vec<RuntimeEvent> = report
        .compactions
        .iter()
        .map(|compaction| RuntimeEvent::ContextCompacted {
            reason: compaction.reason,
            input_tokens: compaction.input_tokens,
            output_tokens: compaction.output_tokens,
            source_items: compaction.source_items,
        })
        .collect();
    events.push(RuntimeEvent::ContextMaintained { trigger, report });
    events
}

#[async_trait]
pub trait EventJournal: Send + Sync {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()>;

    async fn flush(&self) -> AgentResult<()> {
        Ok(())
    }
}
