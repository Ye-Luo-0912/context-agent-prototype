//! Typed, transport-independent work-submission routes (C0 platform v1).
//!
//! These routes are *run-scoped*: unlike operation query/cancel they target
//! the run and its task/turn plane, not one tool operation, so their
//! envelopes must not carry a [`WorkIdentity`]. Submission identity is the
//! client's own `client_request_id` plus the submitted content; run binding
//! is the authenticated session, never a wire string.
//!
//! Four lifecycle facts stay distinct and no response conflates them:
//! **admission** (the actor accepted the request), **application** (focus and
//! the first input were applied atomically), **task completion** (durable,
//! operator-owned) and **cleanup confirmation** (supervision truth). A
//! success response here proves the first two only.

use agent_contracts::{
    ApprovalDecision, ContextItemSummary, TaskAnchorView, TaskId, TurnCancelAck,
};
use serde::{Deserialize, Serialize};

use crate::{
    EnvelopeKind, NegotiatedContractProfile, PlatformEnvelope, PlatformResponse, Route,
    ValidationError, ValidationResult,
    validation::{validate_identifier, validate_opaque},
};

pub const WORK_NAMESPACE: &str = "work";
pub const WORK_SUBMIT: &str = "submit";
pub const WORK_CONTINUE: &str = "continue";
pub const WORK_CANCEL: &str = "cancel";
pub const WORK_SNAPSHOT: &str = "snapshot";
pub const WORK_SUBSCRIBE: &str = "subscribe";
pub const WORK_EVENT: &str = "event";
// B3 read-only operations (run-scoped; never start a model round).
pub const WORK_TASK_DETAIL: &str = "task_detail";
pub const WORK_CHANGES: &str = "changes";
pub const WORK_ARTIFACT: &str = "artifact";
pub const WORK_CONTEXT: &str = "context";
pub const APPROVAL_NAMESPACE: &str = "approval";
pub const APPROVAL_RESPOND: &str = "respond";

/// Matches the runtime's task-anchor text cap so a legal goal never trips a
/// protocol bound first. Multi-line development tasks are legal input
/// (M17-N3/F11), so the cap is generous; the hard backstop is the byte
/// bound mirroring the runtime's own input cap.
pub const MAX_WORK_GOAL_CHARS: usize = 200_000;
pub const MAX_CLIENT_REQUEST_ID_BYTES: usize = 128;
/// Matches the runtime's resumable task-record cap.
pub const MAX_SNAPSHOT_TASKS: usize = 256;
pub const MAX_SNAPSHOT_PENDING_APPROVALS: usize = 16;
pub const MAX_SNAPSHOT_GOAL_CHARS: usize = MAX_WORK_GOAL_CHARS;
/// Byte backstop mirroring `agent_contracts::input::USER_INPUT_REPLAY_MAX_BYTES`:
/// the same total budget the runtime applies to a submitted instruction, so
/// an oversized goal is refused at validation instead of after admission.
pub const MAX_WORK_GOAL_BYTES: usize = agent_contracts::input::USER_INPUT_REPLAY_MAX_BYTES;
pub const MAX_SNAPSHOT_CALL_NAME_BYTES: usize = 128;
/// Char bound on the operator-facing target summary of one pending
/// approval (F12). It is a display projection of the call's own structured
/// arguments, never an authority surface, so it stays small.
pub const MAX_SNAPSHOT_APPROVAL_TARGET_CHARS: usize = 256;
/// The most recent durable sequence a subscribe request may still ask to
/// replay from; older cursors get `resync_required` instead of a replay.
pub const MAX_REPLAY_WINDOW_EVENTS: u64 = 4_096;

// ---------------------------------------------------------------------------
// B3 read-only routes: task detail / change journal / artifact bytes /
// context summary. All four are run-scoped reads — they never start a model
// round, never mutate state and never consult Core's approval gate.
// ---------------------------------------------------------------------------

/// Upper bound on one `work.changes` listing.
pub const MAX_CHANGES_LIMIT: usize = 256;
/// Opaque change-journal transaction id bound (mirrors the workspace
/// journal's own opaque ids).
pub const MAX_CHANGE_TX_ID_BYTES: usize = 64;
/// Char bound on one display field of a change record (path/tool/action/
/// reason/entry identity/hash). The journal caps bytes; this mirrors the
/// same intent as characters so an oversized field fails closed.
pub const MAX_CHANGE_FIELD_CHARS: usize = 512;
/// Hard ceiling for one `work.artifact` body read.
pub const MAX_ARTIFACT_READ_BYTES: u32 = 64 * 1024;
/// Default body read when the caller does not ask for a specific bound.
pub const DEFAULT_ARTIFACT_READ_BYTES: u32 = 32 * 1024;
/// Upper bound on one `work.context` listing.
pub const MAX_CONTEXT_ITEMS: usize = 512;

impl Route {
    pub fn work_submit() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_SUBMIT.to_owned(),
        }
    }

    pub fn work_continue() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_CONTINUE.to_owned(),
        }
    }

    pub fn work_cancel() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_CANCEL.to_owned(),
        }
    }

    pub fn work_snapshot() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_SNAPSHOT.to_owned(),
        }
    }

    pub fn work_subscribe() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_SUBSCRIBE.to_owned(),
        }
    }

    pub fn approval_respond() -> Self {
        Self {
            namespace: APPROVAL_NAMESPACE.to_owned(),
            operation: APPROVAL_RESPOND.to_owned(),
        }
    }

    pub fn is_work_submit(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_SUBMIT
    }

    pub fn is_work_continue(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_CONTINUE
    }

    pub fn is_work_cancel(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_CANCEL
    }

    pub fn is_work_snapshot(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_SNAPSHOT
    }

    pub fn is_work_subscribe(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_SUBSCRIBE
    }

    /// B3 read-only route: one task's full anchor (plan/acceptance/open
    /// loops). Run-scoped and read-only.
    pub fn work_task_detail() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_TASK_DETAIL.to_owned(),
        }
    }

    pub fn is_work_task_detail(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_TASK_DETAIL
    }

    /// B3 read-only route: the workspace change journal (review surface).
    /// Run-scoped and read-only.
    pub fn work_changes() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_CHANGES.to_owned(),
        }
    }

    pub fn is_work_changes(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_CHANGES
    }

    /// B3 read-only route: one run-scoped artifact's bounded body. Run-scoped
    /// and read-only.
    pub fn work_artifact() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_ARTIFACT.to_owned(),
        }
    }

    pub fn is_work_artifact(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_ARTIFACT
    }

    /// B3 read-only route: the context engine's bounded item summary.
    /// Run-scoped and read-only.
    pub fn work_context() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_CONTEXT.to_owned(),
        }
    }

    pub fn is_work_context(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_CONTEXT
    }

    pub fn work_event() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_EVENT.to_owned(),
        }
    }

    pub fn is_work_event(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_EVENT
    }

    pub fn is_approval_respond(&self) -> bool {
        self.namespace == APPROVAL_NAMESPACE && self.operation == APPROVAL_RESPOND
    }

    /// Run-scoped routes carry session-bound run identity and must not carry
    /// a tool-operation `work` identity. The set is closed; every other
    /// namespace still requires `work` on non-liveness messages.
    pub fn is_run_scoped(&self) -> bool {
        self.namespace == WORK_NAMESPACE || self.namespace == APPROVAL_NAMESPACE
    }
}

/// One long-task submission. `client_request_id` is the caller's logical
/// submission key: the same id with the same goal is an idempotent retry that
/// returns the original admission; the same id with a different goal is a
/// conflict and is rejected. The id is process-scoped — it is not durable and
/// after a host restart the same id is simply unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSubmitRequest {
    pub goal: String,
    pub client_request_id: String,
}

impl WorkSubmitRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_text("work.submit.goal", &self.goal, MAX_WORK_GOAL_CHARS)?;
        validate_opaque(
            "work.submit.client_request_id",
            &self.client_request_id,
            MAX_CLIENT_REQUEST_ID_BYTES,
        )
    }
}

/// Whether admission created a new submission or found the same
/// `client_request_id` already admitted with identical content. Both are
/// successes; neither claims task completion or cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSubmitDisposition {
    Accepted,
    AlreadyAccepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSubmitResponse {
    pub disposition: WorkSubmitDisposition,
    /// The focused task this goal was atomically applied to.
    pub task_id: TaskId,
}

impl WorkSubmitResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.submit.task_id",
                "must not be a nil UUID",
            ));
        }
        Ok(())
    }
}

/// Continue the active task's stored directive. The body is empty; the task
/// is whatever the session's run currently focuses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContinueRequest {}

impl WorkContinueRequest {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContinueResponse {
    pub task_id: TaskId,
}

impl WorkContinueResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.continue.task_id",
                "must not be a nil UUID",
            ));
        }
        Ok(())
    }
}

/// Cancel the run's current in-flight turn. Task completion and child-process
/// cleanup are separate facts and are not claimed here; they surface through
/// the durable `TurnCancelled` acknowledgement and supervision truth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCancelRequest {}

impl WorkCancelRequest {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCancelResponse {
    /// Core's exact post-cancellation truth. `Cancelled` proves the durable
    /// barrier; `NoActiveTurn` is a fact, not a failure.
    pub ack: TurnCancelAck,
}

impl WorkCancelResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSnapshotRequest {}

impl WorkSnapshotRequest {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSnapshotStatus {
    Active,
    Suspended,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSnapshotEntry {
    pub task_id: TaskId,
    pub goal: String,
    pub status: TaskSnapshotStatus,
    /// This task's own anchor revision. Revisions are per-task and never
    /// compared or maxed across tasks.
    pub anchor_revision: u64,
    pub tool_requirement_revision: u64,
    pub tool_requirement_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusSnapshot {
    pub task_id: TaskId,
    pub goal: String,
    pub anchor_revision: u64,
}

/// The gate's declared risk for one pending approval (F12), mirrored from
/// the matched `ToolSpec`'s own risk. It is the fact the interactive gate
/// acted on, shown to the operator; it is not itself a permission and the
/// gate's matching stays the sole authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRisk {
    ReadOnly,
    WorkspaceWrite,
    ProcessExecution,
}

impl ApprovalRisk {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::ProcessExecution => "process_execution",
        }
    }
}

/// One approval awaiting a decision. `request_id` is the approval-gate key;
/// responding is bound to the authenticated session server-side. `risk` is
/// the gate's own declared risk; `target_summary` is a bounded operator
/// display projection of the call's structured arguments (workspace path or
/// argv/command), `None` when the arguments carry none of the well-known
/// keys — a client then shows "unavailable" instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingApprovalSnapshot {
    pub request_id: String,
    pub call_name: String,
    pub risk: ApprovalRisk,
    #[serde(default)]
    pub target_summary: Option<String>,
}

/// One consistent typed snapshot. `watermark` is the durable event sequence
/// the state reflects; a client whose live stream is behind must treat
/// `resync_required` as "your stream has a hole, rebuild from this snapshot"
/// rather than splicing events into a stale projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSnapshotResponse {
    pub run_started: bool,
    pub run_completed: bool,
    /// Latest durable event sequence visible to this run.
    pub watermark: u64,
    pub focus: Option<FocusSnapshot>,
    #[serde(default)]
    pub tasks: Vec<TaskSnapshotEntry>,
    #[serde(default)]
    pub pending_approvals: Vec<PendingApprovalSnapshot>,
    pub resync_required: bool,
}

impl WorkSnapshotResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.tasks.len() > MAX_SNAPSHOT_TASKS {
            return Err(ValidationError::new(
                "work.snapshot.tasks",
                format!(
                    "contains {} entries, above the {MAX_SNAPSHOT_TASKS} entry bound",
                    self.tasks.len()
                ),
            ));
        }
        if self.pending_approvals.len() > MAX_SNAPSHOT_PENDING_APPROVALS {
            return Err(ValidationError::new(
                "work.snapshot.pending_approvals",
                format!(
                    "contains {} entries, above the {MAX_SNAPSHOT_PENDING_APPROVALS} entry bound",
                    self.pending_approvals.len()
                ),
            ));
        }
        for task in &self.tasks {
            validate_text(
                "work.snapshot.task.goal",
                &task.goal,
                MAX_SNAPSHOT_GOAL_CHARS,
            )?;
            if task.task_id.0.is_nil() {
                return Err(ValidationError::new(
                    "work.snapshot.task.task_id",
                    "must not be a nil UUID",
                ));
            }
        }
        for approval in &self.pending_approvals {
            validate_opaque(
                "work.snapshot.approval.request_id",
                &approval.request_id,
                MAX_CLIENT_REQUEST_ID_BYTES,
            )?;
            validate_identifier(
                "work.snapshot.approval.call_name",
                &approval.call_name,
                MAX_SNAPSHOT_CALL_NAME_BYTES,
            )?;
            if let Some(target) = &approval.target_summary {
                validate_text(
                    "work.snapshot.approval.target_summary",
                    target,
                    MAX_SNAPSHOT_APPROVAL_TARGET_CHARS,
                )?;
            }
        }
        if let Some(focus) = &self.focus {
            validate_text(
                "work.snapshot.focus.goal",
                &focus.goal,
                MAX_SNAPSHOT_GOAL_CHARS,
            )?;
            if focus.task_id.0.is_nil() {
                return Err(ValidationError::new(
                    "work.snapshot.focus.task_id",
                    "must not be a nil UUID",
                ));
            }
        }
        Ok(())
    }
}

/// Subscribe to the run's typed event stream. `replay_after_seq` asks for a
/// bounded replay cursor; when it is outside the retained window the response
/// reports `resync_required` and the caller must rebuild from a snapshot
/// instead of assuming the gap is filled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSubscribeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_after_seq: Option<u64>,
}

impl WorkSubscribeRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSubscribeResponse {
    /// The sequence the live stream starts from (the current watermark).
    pub watermark: u64,
    pub resync_required: bool,
}

impl WorkSubscribeResponse {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

/// Respond to one pending approval. The decision rides the authenticated
/// session; a late or duplicate response returns the current fact
/// (`NoLongerPending`) instead of failing or re-delivering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondRequest {
    pub request_id: String,
    pub decision: ApprovalDecision,
}

impl ApprovalRespondRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_opaque(
            "approval.respond.request_id",
            &self.request_id,
            MAX_CLIENT_REQUEST_ID_BYTES,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRespondOutcome {
    /// The pending waiter received this decision.
    Delivered,
    /// The request was already answered, expired or unknown. This is the
    /// current fact, not an error and not permission to retry blindly.
    NoLongerPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondResponse {
    pub outcome: ApprovalRespondOutcome,
}

impl ApprovalRespondResponse {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

/// One durable event forwarded to a subscribed session (P3 event stream).
/// The payload is the kernel's own `RuntimeEventEnvelope` — run id, durable
/// sequence and event — forwarded verbatim; the host never rewrites events
/// and live-only deltas keep their own cursor semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEventNotification {
    pub envelope: agent_contracts::RuntimeEventEnvelope,
}

impl WorkEventNotification {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// B3 read-only DTOs: task detail (full anchor), change journal listing,
// bounded artifact bytes and context-item summary.
//
// All four are run-scoped reads: they never start a model round, never
// mutate state and never consult Core's approval gate. The workspace is the
// trusted authority behind each; bytes travel base64 because artifacts are
// binary-safe and the wire is JSON.
// ---------------------------------------------------------------------------

/// One task's full anchor, on demand (the snapshot keeps the bounded list,
/// this route carries the plan/acceptance/open-loops projection the GUI
/// actually renders).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskDetailRequest {
    pub task_id: TaskId,
}

impl WorkTaskDetailRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.task_detail.task_id",
                "must not be a nil UUID",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskDetailResponse {
    pub task_id: TaskId,
    pub goal: String,
    pub status: TaskSnapshotStatus,
    /// This task's own anchor revision; never compared across tasks.
    pub anchor_revision: u64,
    /// The task's authoritative anchor projection (plan/acceptance/open
    /// loops), verbatim from the runtime's own assembler.
    pub anchor: TaskAnchorView,
}

impl WorkTaskDetailResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.task_detail.task_id",
                "must not be a nil UUID",
            ));
        }
        validate_text("work.task_detail.goal", &self.goal, MAX_SNAPSHOT_GOAL_CHARS)?;
        Ok(())
    }
}

/// Bounded change-journal listing request. `after_tx` names an exclusive
/// cursor: records are returned newest-first until the cursor, `limit`, or
/// the journal head is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkChangesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tx: Option<String>,
}

impl WorkChangesRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        if let Some(limit) = self.limit {
            if limit == 0 || limit > MAX_CHANGES_LIMIT {
                return Err(ValidationError::new(
                    "work.changes.limit",
                    format!("must be in 1..={MAX_CHANGES_LIMIT}"),
                ));
            }
        }
        if let Some(after_tx) = &self.after_tx {
            validate_opaque("work.changes.after_tx", after_tx, MAX_CHANGE_TX_ID_BYTES)?;
        }
        Ok(())
    }
}

/// One mirrored workspace journal record. `old_content` never travels to the
/// wire: the journal's old-content capture is an internal review aid, kept
/// bounded inside the workspace and not duplicated as a protocol field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeSummary {
    MutationPrepared {
        tx_id: String,
        timestamp_ms: u64,
        tool: String,
        path: String,
        action: String,
        bytes_before: u64,
        bytes_after: u64,
        before_hash: String,
        after_hash: String,
    },
    MutationCommitted {
        tx_id: String,
        timestamp_ms: u64,
    },
    MutationRolledBack {
        tx_id: String,
        timestamp_ms: u64,
        reason: String,
    },
    DirectoryPrepared {
        tx_id: String,
        timestamp_ms: u64,
        tool: String,
        path: String,
    },
    DirectoryCommitted {
        tx_id: String,
        timestamp_ms: u64,
        entry_identity: String,
    },
    DirectoryRolledBack {
        tx_id: String,
        timestamp_ms: u64,
        reason: String,
    },
}

impl ChangeSummary {
    /// Validate one change's display fields against the shared bounded
    /// budget. Byte counts and timestamps are facts, not text.
    pub fn validate(&self) -> ValidationResult<()> {
        let (tx_id, texts): (&str, Vec<(&'static str, &str)>) = match self {
            Self::MutationPrepared {
                tx_id,
                tool,
                path,
                action,
                before_hash,
                after_hash,
                ..
            } => (
                tx_id,
                vec![
                    ("work.changes.tool", tool),
                    ("work.changes.path", path),
                    ("work.changes.action", action),
                    ("work.changes.before_hash", before_hash),
                    ("work.changes.after_hash", after_hash),
                ],
            ),
            Self::MutationCommitted { tx_id, .. } => (tx_id, Vec::new()),
            Self::MutationRolledBack { tx_id, reason, .. } => {
                (tx_id, vec![("work.changes.reason", reason)])
            }
            Self::DirectoryPrepared {
                tx_id, tool, path, ..
            } => (
                tx_id,
                vec![("work.changes.tool", tool), ("work.changes.path", path)],
            ),
            Self::DirectoryCommitted {
                tx_id,
                entry_identity,
                ..
            } => (tx_id, vec![("work.changes.entry_identity", entry_identity)]),
            Self::DirectoryRolledBack { tx_id, reason, .. } => {
                (tx_id, vec![("work.changes.reason", reason)])
            }
        };
        validate_opaque("work.changes.tx_id", tx_id, MAX_CHANGE_TX_ID_BYTES)?;
        for (field, text) in texts {
            validate_text(field, text, MAX_CHANGE_FIELD_CHARS)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkChangesResponse {
    #[serde(default)]
    pub changes: Vec<ChangeSummary>,
}

impl WorkChangesResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.changes.len() > MAX_CHANGES_LIMIT {
            return Err(ValidationError::new(
                "work.changes.changes",
                format!(
                    "contains {} entries, above the {MAX_CHANGES_LIMIT} entry bound",
                    self.changes.len()
                ),
            ));
        }
        for change in &self.changes {
            change.validate()?;
        }
        Ok(())
    }
}

/// Read one run-scoped artifact reference with an explicit byte budget.
/// `reference` is a sealed `artifact://` locator; the workspace verifies the
/// run binding and (when the reference carries one) the content digest before
/// the router reads anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkArtifactRequest {
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
}

impl WorkArtifactRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_opaque(
            "work.artifact.reference",
            &self.reference,
            agent_contracts::MAX_ARTIFACT_REFERENCE_BYTES,
        )?;
        if let Some(max) = self.max_bytes {
            if max == 0 || max > MAX_ARTIFACT_READ_BYTES {
                return Err(ValidationError::new(
                    "work.artifact.max_bytes",
                    format!("must be in 1..={MAX_ARTIFACT_READ_BYTES}"),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkArtifactResponse {
    /// The canonical reference spelling (verify-sealed when the locator
    /// carries a digest).
    pub reference: String,
    /// The artifact's on-disk byte length, before any route-side bound.
    pub size_bytes: u64,
    /// `true` only when the returned body is a prefix, cut at the requested
    /// budget; a body that fits fully is never marked truncated.
    pub truncated: bool,
    /// The (possibly truncated) body, base64-encoded.
    pub content_base64: String,
}

impl WorkArtifactResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_opaque(
            "work.artifact.reference",
            &self.reference,
            agent_contracts::MAX_ARTIFACT_REFERENCE_BYTES,
        )?;
        let decoded_len = match base64_len(&self.content_base64) {
            Some(len) => len,
            None => {
                return Err(ValidationError::new(
                    "work.artifact.content_base64",
                    "is not a valid base64 body",
                ));
            }
        };
        let truncated = self.truncated;
        let consistent = if truncated {
            decoded_len < self.size_bytes as usize
        } else {
            (decoded_len as u64) == self.size_bytes
        };
        if !consistent {
            return Err(ValidationError::new(
                "work.artifact.size_bytes",
                format!(
                    "size {size} and truncated {truncated} disagree with {decoded} decoded bytes",
                    size = self.size_bytes,
                    decoded = decoded_len
                ),
            ));
        }
        Ok(())
    }
}

/// Bounded context-engine summary read (the GUI's read-only Context panel).
/// Never starts a model round and never mutates the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContextRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl WorkContextRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        if let Some(limit) = self.limit {
            if limit == 0 || limit as usize > MAX_CONTEXT_ITEMS {
                return Err(ValidationError::new(
                    "work.context.limit",
                    format!("must be in 1..={MAX_CONTEXT_ITEMS}"),
                ));
            }
        }
        Ok(())
    }
}

// `ContextItemSummary` deliberately carries no `PartialEq` (it mirrors the
// engine's own computation-heavy summary); the response itself only needs
// the wire derives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContextResponse {
    #[serde(default)]
    pub items: Vec<ContextItemSummary>,
}

impl WorkContextResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.items.len() > MAX_CONTEXT_ITEMS {
            return Err(ValidationError::new(
                "work.context.items",
                format!(
                    "contains {} entries, above the {MAX_CONTEXT_ITEMS} entry bound",
                    self.items.len()
                ),
            ));
        }
        Ok(())
    }
}

/// Decoded length of standard (non-URL-safe) base64 text, `None` when the
/// text is not valid padded base64.
fn base64_len(text: &str) -> Option<usize> {
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(text) {
        Ok(bytes) => Some(bytes.len()),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Shared default-endpoint derivation (N4).
//
// The host binds its local endpoint and local clients derive the same
// default from ONE rule, so a client that did not watch the host's startup
// log still knocks on the right door. The suffix is a pure digest over the
// workspace path *as given* (no canonicalization here — the host may
// canonicalize first, but the wire-facing rule must stay exactly
// reproducible cross-language); the UDS default places it under the user's
// runtime directory when the platform provides one, else the temp dir.
// ---------------------------------------------------------------------------

/// The per-workspace endpoint discriminator: 16 hex chars of the SHA-256
/// digest over the workspace root's bytes as given. One workspace always
/// resolves to the same suffix and two workspaces never share one; no
/// default endpoint is a fixed global name.
pub fn workspace_endpoint_suffix(workspace_root: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(workspace_root.as_os_str().as_encoded_bytes());
    let mut suffix = String::with_capacity(16);
    for byte in &digest[..8] {
        suffix.push_str(&format!("{byte:02x}"));
    }
    suffix
}

/// The default UDS endpoint for one workspace: the user's runtime directory
/// when the platform provides one, else the temp dir — plus the workspace
/// discriminator, so the path is user-private, workspace-scoped, and never
/// a fixed global name in `/tmp`.
#[cfg(unix)]
pub fn default_socket_path_for(workspace_root: &std::path::Path) -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!(
        "focus-agent-platform-{}.sock",
        workspace_endpoint_suffix(workspace_root)
    ))
}

/// Free human-readable text (goals, task summaries). The runtime caps these
/// by character count, so the protocol bound is characters too — a legal
/// CJK goal must not trip the protocol first — with a byte backstop that
/// mirrors the runtime's input cap. Control characters are rejected EXCEPT
/// the three that multi-line development input legitimately contains
/// (LF, CR, TAB — M17-N3/F11): a pasted multi-line task must not be
/// refused at the door.
fn validate_text(field: &'static str, value: &str, max_chars: usize) -> ValidationResult<()> {
    if value.is_empty() {
        return Err(ValidationError::new(field, "must not be empty"));
    }
    let chars = value.chars().count();
    if chars > max_chars {
        return Err(ValidationError::new(
            field,
            format!("is {chars} chars, above the {max_chars} char bound"),
        ));
    }
    if value.len() > MAX_WORK_GOAL_BYTES {
        return Err(ValidationError::new(
            field,
            format!(
                "is {} bytes, above the {MAX_WORK_GOAL_BYTES} byte bound",
                value.len()
            ),
        ));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(ValidationError::new(
            field,
            "must not contain control characters (LF/CR/TAB are allowed)",
        ));
    }
    Ok(())
}

/// Route-kind guard shared by every run-scoped request validator.
fn validate_run_scoped_request<P>(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<P>,
    route_matches: fn(&Route) -> bool,
    payload: fn(&P) -> ValidationResult<()>,
) -> ValidationResult<()> {
    request.validate(profile)?;
    if request.kind != EnvelopeKind::Request {
        return Err(ValidationError::new(
            "envelope.kind",
            "run-scoped work control requires a request envelope",
        ));
    }
    if !route_matches(&request.route) {
        return Err(ValidationError::new(
            "envelope.route",
            "does not match the run-scoped payload",
        ));
    }
    if request.work.is_some() {
        return Err(ValidationError::new(
            "envelope.work",
            "run-scoped routes must not carry tool-operation work identity",
        ));
    }
    payload(&request.payload)
}

/// Route-kind guard for run-scoped responses: pairing plus payload truth.
fn validate_run_scoped_response<RequestPayload, ResponsePayload>(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<RequestPayload>,
    response: &PlatformEnvelope<PlatformResponse<ResponsePayload>>,
    payload: fn(&ResponsePayload) -> ValidationResult<()>,
) -> ValidationResult<()> {
    crate::validate_response_pair(profile, request, response)?;
    if let PlatformResponse::Success { value } = &response.payload {
        payload(value)?;
    }
    Ok(())
}

pub fn validate_work_submit_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSubmitRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_submit, |payload| {
        payload.validate()
    })
}

pub fn validate_work_submit_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSubmitRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkSubmitResponse>>,
) -> ValidationResult<()> {
    validate_work_submit_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_continue_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkContinueRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_continue, |payload| {
        payload.validate()
    })
}

pub fn validate_work_continue_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkContinueRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkContinueResponse>>,
) -> ValidationResult<()> {
    validate_work_continue_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_cancel_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkCancelRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_cancel, |payload| {
        payload.validate()
    })
}

pub fn validate_work_cancel_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkCancelRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkCancelResponse>>,
) -> ValidationResult<()> {
    validate_work_cancel_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_snapshot_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSnapshotRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_snapshot, |payload| {
        payload.validate()
    })
}

pub fn validate_work_snapshot_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSnapshotRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkSnapshotResponse>>,
) -> ValidationResult<()> {
    validate_work_snapshot_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_subscribe_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSubscribeRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_subscribe, |payload| {
        payload.validate()
    })
}

pub fn validate_work_subscribe_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSubscribeRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkSubscribeResponse>>,
) -> ValidationResult<()> {
    validate_work_subscribe_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_approval_respond_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<ApprovalRespondRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_approval_respond, |payload| {
        payload.validate()
    })
}

pub fn validate_approval_respond_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<ApprovalRespondRequest>,
    response: &PlatformEnvelope<PlatformResponse<ApprovalRespondResponse>>,
) -> ValidationResult<()> {
    validate_approval_respond_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_task_detail_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkTaskDetailRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_task_detail, |payload| {
        payload.validate()
    })
}

pub fn validate_work_task_detail_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkTaskDetailRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkTaskDetailResponse>>,
) -> ValidationResult<()> {
    validate_work_task_detail_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_changes_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkChangesRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_changes, |payload| {
        payload.validate()
    })
}

pub fn validate_work_changes_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkChangesRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkChangesResponse>>,
) -> ValidationResult<()> {
    validate_work_changes_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_artifact_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkArtifactRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_artifact, |payload| {
        payload.validate()
    })
}

pub fn validate_work_artifact_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkArtifactRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkArtifactResponse>>,
) -> ValidationResult<()> {
    validate_work_artifact_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_context_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkContextRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_context, |payload| {
        payload.validate()
    })
}

pub fn validate_work_context_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkContextRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkContextResponse>>,
) -> ValidationResult<()> {
    validate_work_context_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use agent_contracts::{RunId, TurnId};
    use base64::Engine;
    use serde_json::json;

    use super::*;
    use crate::{
        ActiveFeatures, Causality, MessageId, ProtocolIdentity, ProtocolVersion, RequestId,
        SchemaDigest, WorkIdentity,
    };

    const MESSAGE_1: &str = "00000000-0000-4000-8000-000000000001";
    const MESSAGE_2: &str = "00000000-0000-4000-8000-000000000002";
    const REQUEST_1: &str = "00000000-0000-4000-8000-000000000011";
    const RUN: &str = "00000000-0000-4000-8000-000000000021";
    const TASK: &str = "00000000-0000-4000-8000-000000000022";

    fn protocol() -> ProtocolIdentity {
        ProtocolIdentity {
            name: "focus-agent.platform".into(),
            version: ProtocolVersion { major: 1, minor: 0 },
            active_features: ActiveFeatures::default(),
            schema_digest: SchemaDigest::from_bytes([0x11; 32]),
        }
    }

    fn profile() -> NegotiatedContractProfile {
        let protocol = protocol();
        NegotiatedContractProfile::new(
            protocol.name,
            protocol.version,
            protocol.active_features,
            protocol.schema_digest,
        )
        .unwrap()
    }

    /// A run-scoped request envelope: no `work` identity by contract.
    fn run_scoped_request<P>(route: Route, payload: P) -> PlatformEnvelope<P> {
        let message_id = MessageId::from_str(MESSAGE_1).unwrap();
        PlatformEnvelope {
            protocol: protocol(),
            message_id,
            request_id: Some(RequestId::from_str(REQUEST_1).unwrap()),
            kind: EnvelopeKind::Request,
            route,
            work: None,
            causality: Causality::root(message_id),
            payload,
        }
    }

    fn response<RequestPayload, ResponsePayload>(
        request: &PlatformEnvelope<RequestPayload>,
        value: ResponsePayload,
    ) -> PlatformEnvelope<PlatformResponse<ResponsePayload>> {
        PlatformEnvelope {
            protocol: request.protocol.clone(),
            message_id: MessageId::from_str(MESSAGE_2).unwrap(),
            request_id: request.request_id,
            kind: EnvelopeKind::Response,
            route: request.route.clone(),
            work: None,
            causality: Causality::caused_by(request.causality.correlation_id, request.message_id),
            payload: PlatformResponse::Success { value },
        }
    }

    fn submit_request() -> PlatformEnvelope<WorkSubmitRequest> {
        run_scoped_request(
            Route::work_submit(),
            WorkSubmitRequest {
                goal: "migrate the retry table".into(),
                client_request_id: "client-1".into(),
            },
        )
    }

    fn task_id() -> TaskId {
        TaskId::from_str(TASK).unwrap()
    }

    #[test]
    fn work_submit_request_has_exact_golden_shape() {
        let request = submit_request();
        validate_work_submit_request(&profile(), &request).unwrap();

        let digest_11 = "11".repeat(32);
        let expected = format!(
            "{{\"protocol\":{{\"name\":\"focus-agent.platform\",\"version\":{{\"major\":1,\"minor\":0}},\"active_features\":[],\"schema_digest\":\"{digest_11}\"}},\"message_id\":\"{MESSAGE_1}\",\"request_id\":\"{REQUEST_1}\",\"kind\":\"request\",\"route\":{{\"namespace\":\"work\",\"operation\":\"submit\"}},\"causality\":{{\"correlation_id\":\"{MESSAGE_1}\"}},\"payload\":{{\"goal\":\"migrate the retry table\",\"client_request_id\":\"client-1\"}}}}"
        );
        assert_eq!(serde_json::to_string(&request).unwrap(), expected);
        assert_eq!(
            serde_json::from_str::<PlatformEnvelope<WorkSubmitRequest>>(&expected).unwrap(),
            request
        );

        let success = response(
            &request,
            WorkSubmitResponse {
                disposition: WorkSubmitDisposition::Accepted,
                task_id: task_id(),
            },
        );
        validate_work_submit_response(&profile(), &request, &success).unwrap();
        let encoded = serde_json::to_string(&success.payload).unwrap();
        assert!(encoded.contains("\"disposition\":\"accepted\""));
        assert!(encoded.contains(&format!("\"task_id\":\"{TASK}\"")));
    }

    #[test]
    fn submit_bounds_and_conflict_inputs_fail_closed() {
        let mut oversized = submit_request();
        oversized.payload.goal = "好".repeat(MAX_WORK_GOAL_CHARS + 1);
        assert!(oversized.payload.validate().is_err());

        let mut byte_oversized = submit_request();
        // CJK triples the UTF-8 cost: under the char bound, over the byte
        // backstop that mirrors the runtime's input cap (M17-N3/F11).
        byte_oversized.payload.goal = "好".repeat(MAX_WORK_GOAL_BYTES / 3 + 1);
        assert!(byte_oversized.payload.validate().is_err());

        let mut long_id = submit_request();
        long_id.payload.client_request_id = "x".repeat(MAX_CLIENT_REQUEST_ID_BYTES + 1);
        assert!(long_id.payload.validate().is_err());

        let mut empty_id = submit_request();
        empty_id.payload.client_request_id.clear();
        assert!(empty_id.payload.validate().is_err());
    }

    /// M17-N3/F11: a pasted multi-line development task is legal input —
    /// LF/CR/TAB pass validation while every other control character still
    /// fails it.
    #[test]
    fn multi_line_goals_are_legal_but_other_controls_fail() {
        let mut multi_line = submit_request();
        multi_line.payload.goal =
            "fix the retry table:\n- first repro\n\t- then patch\r\nand add a regression"
                .to_owned();
        assert!(multi_line.payload.validate().is_ok());

        let mut other_control = submit_request();
        other_control.payload.goal = "bad\u{1}control".to_owned();
        assert!(other_control.payload.validate().is_err());
    }

    #[test]
    fn run_scoped_envelopes_reject_work_identity_and_kind_drift() {
        let mut carrying_work = submit_request();
        carrying_work.work = Some(WorkIdentity {
            run_id: RunId::from_str(RUN).unwrap(),
            task_id: None,
            turn_id: None,
            scope_id: None,
            operation_id: agent_contracts::OperationId::new(),
            generation: 1,
            attempt: crate::Attempt::new(1).unwrap(),
            call_id: None,
            effect_id: None,
            argument_digest: agent_contracts::ArgumentDigest::from_bytes([0x22; 32]),
            deadline_remaining_ms: crate::DeadlineRemainingMs::new(1_000).unwrap(),
            authority_ref: None,
        });
        let error = validate_work_submit_request(&profile(), &carrying_work).unwrap_err();
        assert_eq!(error.field(), "envelope.work");

        let mut response_kind = submit_request();
        response_kind.kind = EnvelopeKind::Response;
        assert!(validate_work_submit_request(&profile(), &response_kind).is_err());

        let mut wrong_route = submit_request();
        wrong_route.route = Route::work_snapshot();
        assert!(validate_work_submit_request(&profile(), &wrong_route).is_err());

        let mut unknown_namespace = submit_request();
        unknown_namespace.route = Route::new("tool", "invoke").unwrap();
        // Non-run-scoped routes still require work (fail closed default).
        assert!(unknown_namespace.validate(&profile()).is_err());
    }

    #[test]
    fn snapshot_response_is_bounded_and_self_validating() {
        let request = run_scoped_request(Route::work_snapshot(), WorkSnapshotRequest {});
        let mut snapshot = WorkSnapshotResponse {
            run_started: true,
            run_completed: false,
            watermark: 41,
            focus: Some(FocusSnapshot {
                task_id: task_id(),
                goal: "migrate".into(),
                anchor_revision: 1,
            }),
            tasks: vec![TaskSnapshotEntry {
                task_id: task_id(),
                goal: "migrate".into(),
                status: TaskSnapshotStatus::Active,
                anchor_revision: 1,
                tool_requirement_revision: 2,
                tool_requirement_count: 1,
            }],
            pending_approvals: vec![PendingApprovalSnapshot {
                request_id: "approval-1".into(),
                call_name: "fs.write".into(),
                risk: ApprovalRisk::WorkspaceWrite,
                target_summary: Some("docs/plan.md".into()),
            }],
            resync_required: false,
        };
        let success = response(&request, snapshot.clone());
        validate_work_snapshot_response(&profile(), &request, &success).unwrap();

        snapshot.tasks = vec![
            TaskSnapshotEntry {
                task_id: task_id(),
                goal: "g".into(),
                status: TaskSnapshotStatus::Suspended,
                anchor_revision: 0,
                tool_requirement_revision: 0,
                tool_requirement_count: 0,
            };
            MAX_SNAPSHOT_TASKS + 1
        ];
        assert!(snapshot.validate().is_err());

        snapshot.tasks.clear();
        snapshot.pending_approvals = vec![
            PendingApprovalSnapshot {
                request_id: "a".into(),
                call_name: "fs.write".into(),
                risk: ApprovalRisk::WorkspaceWrite,
                target_summary: None,
            };
            MAX_SNAPSHOT_PENDING_APPROVALS + 1
        ];
        assert!(snapshot.validate().is_err());
    }

    /// F12: an approval's target summary is a bounded display projection.
    /// Oversized or control-bearing summaries fail validation; the summary
    /// travels on the wire exactly as encoded (snake_case risk).
    #[test]
    fn approval_target_summary_is_bounded_display_text() {
        fn approval(target: Option<String>) -> PendingApprovalSnapshot {
            PendingApprovalSnapshot {
                request_id: "approval-1".into(),
                call_name: "fs.write".into(),
                risk: ApprovalRisk::WorkspaceWrite,
                target_summary: target,
            }
        }

        let mut snapshot = WorkSnapshotResponse {
            run_started: true,
            run_completed: false,
            watermark: 1,
            focus: None,
            tasks: vec![],
            pending_approvals: vec![approval(Some(
                "x".repeat(MAX_SNAPSHOT_APPROVAL_TARGET_CHARS + 1),
            ))],
            resync_required: false,
        };
        assert!(snapshot.validate().is_err());

        snapshot.pending_approvals = vec![approval(Some("bad\u{1}control".into()))];
        assert!(snapshot.validate().is_err());

        snapshot.pending_approvals = vec![approval(Some("src/main.rs".into()))];
        snapshot.validate().unwrap();

        // The risk tag is snake_case on the wire (mirrored by the .NET
        // client's enum naming policy).
        let encoded = serde_json::to_string(&approval(None)).unwrap();
        assert!(encoded.contains("\"risk\":\"workspace_write\""));
        assert!(encoded.contains("\"target_summary\":null"));
    }

    #[test]
    fn cancel_ack_round_trips_both_truths() {
        let request = run_scoped_request(Route::work_cancel(), WorkCancelRequest {});
        for ack in [
            TurnCancelAck::NoActiveTurn,
            TurnCancelAck::Cancelled {
                turn_id: TurnId::new(),
                task_id: Some(task_id()),
                operation_id: None,
                cancelled_generation: 3,
                effective_generation: 4,
            },
        ] {
            let success = response(&request, WorkCancelResponse { ack: ack.clone() });
            validate_work_cancel_response(&profile(), &request, &success).unwrap();
            let decoded: PlatformResponse<WorkCancelResponse> =
                serde_json::from_str(&serde_json::to_string(&success.payload).unwrap()).unwrap();
            let PlatformResponse::Success { value } = decoded else {
                unreachable!()
            };
            assert_eq!(value.ack, ack);
        }
    }

    #[test]
    fn subscribe_and_approval_dto_shapes_are_bounded() {
        let subscribe = run_scoped_request(
            Route::work_subscribe(),
            WorkSubscribeRequest {
                replay_after_seq: Some(MAX_REPLAY_WINDOW_EVENTS),
            },
        );
        validate_work_subscribe_request(&profile(), &subscribe).unwrap();
        let accepted = response(
            &subscribe,
            WorkSubscribeResponse {
                watermark: MAX_REPLAY_WINDOW_EVENTS,
                resync_required: false,
            },
        );
        validate_work_subscribe_response(&profile(), &subscribe, &accepted).unwrap();

        let approval = run_scoped_request(
            Route::approval_respond(),
            ApprovalRespondRequest {
                request_id: "approval-1".into(),
                decision: agent_contracts::ApprovalDecision::Deny,
            },
        );
        validate_approval_respond_request(&profile(), &approval).unwrap();
        let delivered = response(
            &approval,
            ApprovalRespondResponse {
                outcome: ApprovalRespondOutcome::Delivered,
            },
        );
        validate_approval_respond_response(&profile(), &approval, &delivered).unwrap();
        let late = response(
            &approval,
            ApprovalRespondResponse {
                outcome: ApprovalRespondOutcome::NoLongerPending,
            },
        );
        validate_approval_respond_response(&profile(), &approval, &late).unwrap();
        let encoded = serde_json::to_value(&late.payload).unwrap();
        assert_eq!(encoded["value"]["outcome"], json!("no_longer_pending"));

        let mut long_request_id = run_scoped_request(
            Route::approval_respond(),
            ApprovalRespondRequest {
                request_id: "x".repeat(MAX_CLIENT_REQUEST_ID_BYTES + 1),
                decision: agent_contracts::ApprovalDecision::Allow,
            },
        );
        assert!(long_request_id.payload.validate().is_err());
        long_request_id.payload.request_id = "ok".into();
        validate_approval_respond_request(&profile(), &long_request_id).unwrap();
    }

    #[test]
    fn continue_route_has_empty_body_and_validated_task() {
        let request = run_scoped_request(Route::work_continue(), WorkContinueRequest {});
        validate_work_continue_request(&profile(), &request).unwrap();

        let success = response(&request, WorkContinueResponse { task_id: task_id() });
        validate_work_continue_response(&profile(), &request, &success).unwrap();

        // Unknown fields stay rejected on every run-scoped body.
        assert!(serde_json::from_value::<WorkContinueRequest>(json!({"extra": true})).is_err());
        assert!(
            serde_json::from_value::<WorkSubmitRequest>(
                json!({"goal": "g", "client_request_id": "c", "extra": 1})
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // B3 read-only routes.
    // -----------------------------------------------------------------------

    fn anchor() -> TaskAnchorView {
        TaskAnchorView {
            revision: 1,
            original_goal: "migrate".into(),
            current_interpretation: "interpreted".into(),
            constraints: vec!["bounded".into()],
            acceptance_criteria: vec!["green".into()],
            plan_progress: vec!["step 1".into()],
            open_loops: vec!["verify".into()],
            next_action: "continue".into(),
        }
    }

    #[test]
    fn task_detail_golden_shape_round_trips() {
        let request = run_scoped_request(
            Route::work_task_detail(),
            WorkTaskDetailRequest { task_id: task_id() },
        );
        validate_work_task_detail_request(&profile(), &request).unwrap();
        let detail = response(
            &request,
            WorkTaskDetailResponse {
                task_id: task_id(),
                goal: "migrate".into(),
                status: TaskSnapshotStatus::Active,
                anchor_revision: 1,
                anchor: anchor(),
            },
        );
        validate_work_task_detail_response(&profile(), &request, &detail).unwrap();

        let mut nil_task = request.clone();
        nil_task.payload.task_id =
            TaskId::from_str("00000000-0000-0000-0000-000000000000").unwrap();
        assert!(nil_task.payload.validate().is_err());

        // A task-detail response whose anchor is absent fails validation too:
        // the anchor is the payload of this route, never optional.
        let mut empty_goal = request;
        empty_goal.payload.task_id = task_id();
        let mut bad = detail.clone();
        if let PlatformResponse::Success { value } = &mut bad.payload {
            value.goal.clear();
        }
        assert!(validate_work_task_detail_response(&profile(), &empty_goal, &bad).is_err());
    }

    #[test]
    fn changes_listing_round_trips_and_stays_bounded() {
        let request = run_scoped_request(
            Route::work_changes(),
            WorkChangesRequest {
                limit: Some(8),
                after_tx: Some("tx-9".into()),
            },
        );
        validate_work_changes_request(&profile(), &request).unwrap();
        let listed = response(
            &request,
            WorkChangesResponse {
                changes: vec![
                    ChangeSummary::MutationPrepared {
                        tx_id: "tx-3".into(),
                        timestamp_ms: 30,
                        tool: "fs.write".into(),
                        path: "docs/plan.md".into(),
                        action: "overwrite".into(),
                        bytes_before: 10,
                        bytes_after: 20,
                        before_hash: "a1".into(),
                        after_hash: "b2".into(),
                    },
                    ChangeSummary::MutationCommitted {
                        tx_id: "tx-3".into(),
                        timestamp_ms: 31,
                    },
                ],
            },
        );
        validate_work_changes_response(&profile(), &request, &listed).unwrap();

        let encoded = serde_json::to_string(&listed.payload).unwrap();
        assert!(encoded.contains("\"kind\":\"mutation_prepared\""));
        assert!(
            !encoded.contains("old_content"),
            "the journal's internal old-content capture must never reach the wire"
        );

        let mut over_limit = listed.clone();
        if let PlatformResponse::Success { value } = &mut over_limit.payload {
            value.changes = vec![
                ChangeSummary::MutationCommitted {
                    tx_id: "t".into(),
                    timestamp_ms: 1,
                };
                MAX_CHANGES_LIMIT + 1
            ];
        }
        assert!(validate_work_changes_response(&profile(), &request, &over_limit).is_err());

        // Unknown fields on a change variant never leak to serialization (the
        // wire mirror has no old_content member); serde itself is lenient
        // when decoding internally-tagged variants, so the guard is: reading
        // an old_content-bearing line must round-trip it AWAY.
        let with_old_content: ChangeSummary = serde_json::from_value(json!({
            "kind": "mutation_prepared",
            "tx_id": "t",
            "timestamp_ms": 1,
            "tool": "fs.write",
            "path": "a.txt",
            "action": "overwrite",
            "bytes_before": 0,
            "bytes_after": 1,
            "before_hash": "h",
            "after_hash": "h2",
            "old_content": "stale"
        }))
        .unwrap();
        assert!(
            !serde_json::to_string(&with_old_content)
                .unwrap()
                .contains("old_content"),
            "the journal's internal old-content capture must never reach the wire"
        );
    }

    #[test]
    fn artifacts_validate_reference_and_truncation_truth() {
        let request = run_scoped_request(
            Route::work_artifact(),
            WorkArtifactRequest {
                reference: "artifact://.focus-agent/artifacts/r/proof/aa".into(),
                max_bytes: Some(32),
            },
        );
        validate_work_artifact_request(&profile(), &request).unwrap();

        let mut big_request = request.clone();
        big_request.payload.max_bytes = Some(MAX_ARTIFACT_READ_BYTES + 1);
        assert!(big_request.payload.validate().is_err());

        let mut long_ref = request.clone();
        long_ref.payload.reference = "x".repeat(agent_contracts::MAX_ARTIFACT_REFERENCE_BYTES + 1);
        assert!(long_ref.payload.validate().is_err());

        // A truncated body must be strictly shorter than size_bytes; a full
        // body must equal it. Both are validated, never assumed.
        let full = response(
            &request,
            WorkArtifactResponse {
                reference: "artifact://r".into(),
                size_bytes: 5,
                truncated: false,
                content_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        );
        validate_work_artifact_response(&profile(), &request, &full).unwrap();

        let truncated = response(
            &request,
            WorkArtifactResponse {
                reference: "artifact://r".into(),
                size_bytes: 100,
                truncated: true,
                content_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        );
        validate_work_artifact_response(&profile(), &request, &truncated).unwrap();

        let lies = response(
            &request,
            WorkArtifactResponse {
                reference: "artifact://r".into(),
                size_bytes: 32,
                truncated: false,
                content_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        );
        assert!(validate_work_artifact_response(&profile(), &request, &lies).is_err());
    }

    #[test]
    fn context_listing_round_trips_a_mirrored_item_summary() {
        let request =
            run_scoped_request(Route::work_context(), WorkContextRequest { limit: Some(4) });
        validate_work_context_request(&profile(), &request).unwrap();
        let item: ContextItemSummary = serde_json::from_value(json!({
            "id": "9f823fc0-8ed4-4d18-a02d-2d75691f7f01",
            "kind": "Goal",
            "scope": "Task",
            "attention": "Active",
            "semantic": "Live",
            "importance": 0.5,
            "relevance": 0.3,
            "created_tick": 1,
            "created_turn": 1,
            "last_access_turn": 1,
            "access_count": 1,
            "dependencies": [],
            "keep_alive": false,
        }))
        .unwrap();
        let listed = response(
            &request,
            WorkContextResponse {
                items: vec![item.clone()],
            },
        );
        validate_work_context_response(&profile(), &request, &listed).unwrap();

        let mut bullet = request.clone();
        bullet.payload.limit = Some((MAX_CONTEXT_ITEMS + 1) as u32);
        assert!(bullet.payload.validate().is_err());
        bullet.payload.limit = Some(0);
        assert!(bullet.payload.validate().is_err());

        let mut over = listed;
        if let PlatformResponse::Success { value } = &mut over.payload {
            value.items = vec![item; MAX_CONTEXT_ITEMS + 1];
        }
        assert!(validate_work_context_response(&profile(), &request, &over).is_err());
    }

    #[test]
    fn read_only_routes_reject_work_identity_like_every_run_scoped_route() {
        let mut request = run_scoped_request(
            Route::work_task_detail(),
            WorkTaskDetailRequest { task_id: task_id() },
        );
        request.work = Some(WorkIdentity {
            run_id: RunId::from_str(RUN).unwrap(),
            task_id: None,
            turn_id: None,
            scope_id: None,
            operation_id: agent_contracts::OperationId::new(),
            generation: 1,
            attempt: crate::Attempt::new(1).unwrap(),
            call_id: None,
            effect_id: None,
            argument_digest: agent_contracts::ArgumentDigest::from_bytes([0x22; 32]),
            deadline_remaining_ms: crate::DeadlineRemainingMs::new(1_000).unwrap(),
            authority_ref: None,
        });
        let error = validate_work_task_detail_request(&profile(), &request).unwrap_err();
        assert_eq!(error.field(), "envelope.work");
    }
}
