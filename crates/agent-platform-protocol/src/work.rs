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
    ApprovalDecision, ContextItemSummary, MAX_COMPLETION_ARTIFACTS, MAX_COMPLETION_REF_CHARS,
    MAX_COMPLETION_SUMMARY_CHARS, RunId, TaskAnchorView, TaskId, TurnCancelAck,
};

/// EXEC-8: a final-output digest is a hex string over SHA-256 — 128 chars is
/// a generous opaque bound.
pub const MAX_COMPLETION_DIGEST_CHARS: usize = 128;
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
/// F5: in-task steering/correction — distinct from [`WORK_SUBMIT`]. Steering
/// lands on the task the run is already on and never creates one.
pub const WORK_STEER: &str = "steer";
/// F5: activate an existing task through the same RuntimeActor (no second
/// task table, no client-side focus authority).
pub const WORK_ACTIVATE: &str = "activate";
/// F5: suspend a task without completing it.
pub const WORK_SUSPEND: &str = "suspend";
/// F5: capture one FORMAL cross-plane runtime checkpoint (actor + context +
/// capability planes) into the run's checkpoint store.
pub const WORK_CHECKPOINT: &str = "checkpoint";
/// F5: restore one verified checkpoint artifact through the full cross-plane
/// transaction.
pub const WORK_RESTORE: &str = "restore";
pub const WORK_SNAPSHOT: &str = "snapshot";
pub const WORK_SUBSCRIBE: &str = "subscribe";
pub const WORK_EVENT: &str = "event";
// B3 read-only operations (run-scoped; never start a model round).
pub const WORK_TASK_DETAIL: &str = "task_detail";
/// EXEC-8 (R2-09): read-only cold lookup of one completed task's outcome.
pub const WORK_TASK_COMPLETION: &str = "task_completion";
pub const WORK_CHANGES: &str = "changes";
pub const WORK_ARTIFACT: &str = "artifact";
pub const WORK_CONTEXT: &str = "context";
/// PLATFORM-1 (F06): exact-request submission receipt query. Run-scoped and
/// read-only; the caller names its own `client_request_id` instead of
/// inferring admission from a matching goal.
pub const WORK_SUBMIT_RESULT: &str = "submit_result";
pub const APPROVAL_NAMESPACE: &str = "approval";
pub const APPROVAL_RESPOND: &str = "respond";

/// Matches the runtime's task-anchor text cap so a legal goal never trips a
/// protocol bound first. Multi-line development tasks are legal input
/// (M17-N3/F11), so the cap is generous; the hard backstop is the byte
/// bound mirroring the runtime's own input cap.
pub const MAX_WORK_GOAL_CHARS: usize = 200_000;
pub const MAX_CLIENT_REQUEST_ID_BYTES: usize = 128;
/// PLATFORM-1 (F06): bound on the recorded-payload digest returned by the
/// exact-request query's conflict classification (hex sha-256 fits with room).
pub const MAX_SUBMIT_PAYLOAD_DIGEST_BYTES: usize = 128;
/// PLATFORM-3: bound on the workspace root a snapshot names. Long paths are
/// legal (Unix allows 4 KiB), control characters are not.
pub const MAX_SNAPSHOT_WORKSPACE_ROOT_BYTES: usize = 4096;
/// Matches the runtime's resumable task-record cap.
pub const MAX_SNAPSHOT_TASKS: usize = 256;
pub const MAX_SNAPSHOT_PENDING_APPROVALS: usize = 16;
/// EXEC-6: bounded degrade facts ride the snapshot; the cap mirrors the
/// protected-run/lineage bound it reports about.
pub const MAX_SNAPSHOT_DEGRADED_RUNS: usize = 64;
pub const MAX_SNAPSHOT_DEGRADED_RUN_ID_BYTES: usize = 128;
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

    /// F5: the in-task steering route.
    pub fn work_steer() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_STEER.to_owned(),
        }
    }

    pub fn is_work_steer(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_STEER
    }

    /// F5: activate an existing task.
    pub fn work_activate() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_ACTIVATE.to_owned(),
        }
    }

    pub fn is_work_activate(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_ACTIVATE
    }

    /// F5: suspend the active task without completing it.
    pub fn work_suspend() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_SUSPEND.to_owned(),
        }
    }

    pub fn is_work_suspend(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_SUSPEND
    }

    /// F5: capture one formal cross-plane checkpoint.
    pub fn work_checkpoint() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_CHECKPOINT.to_owned(),
        }
    }

    pub fn is_work_checkpoint(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_CHECKPOINT
    }

    /// F5: restore one verified checkpoint artifact.
    pub fn work_restore() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_RESTORE.to_owned(),
        }
    }

    pub fn is_work_restore(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_RESTORE
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

    /// EXEC-8 (R2-09): read-only route — one completed task's outcome from
    /// the hot window or the durable journal. Run-scoped, read-only.
    pub fn work_task_completion() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_TASK_COMPLETION.to_owned(),
        }
    }

    pub fn is_work_task_completion(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_TASK_COMPLETION
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

    /// PLATFORM-1 (F06) read-only route: the exact-request submission receipt
    /// query. Run-scoped and read-only.
    pub fn work_submit_result() -> Self {
        Self {
            namespace: WORK_NAMESPACE.to_owned(),
            operation: WORK_SUBMIT_RESULT.to_owned(),
        }
    }

    pub fn is_work_submit_result(&self) -> bool {
        self.namespace == WORK_NAMESPACE && self.operation == WORK_SUBMIT_RESULT
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

/// Continue the active task's stored directive.
///
/// F5: `expected_task_id` makes "continue" precise for a multi-entry client.
/// Omitted (the historical empty body) the run's own active task is continued;
/// named, the runtime compares it against the live active task inside the
/// actor and starts NO turn on a mismatch. The comparison never happens in the
/// client, so a stale snapshot cannot continue somebody else's task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContinueRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_task_id: Option<TaskId>,
}

impl WorkContinueRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_optional_task_id("work.continue.expected_task_id", self.expected_task_id)
    }
}

/// Whether the continuation actually started a turn. `Rejected` only occurs
/// for a request that named an expectation, so it never reaches a client that
/// did not opt in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkContinueDisposition {
    #[default]
    Continued,
    /// The named `expected_task_id` is not the live active task; nothing was
    /// continued and no directive was re-read.
    ExpectedTaskMismatch,
}

impl WorkContinueDisposition {
    fn is_continued(&self) -> bool {
        matches!(self, Self::Continued)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContinueResponse {
    /// The task the directive continues under. `None` only on a rejection with
    /// no active task at all.
    #[serde(default)]
    pub task_id: Option<TaskId>,
    /// Absent (and therefore byte-compatible with the historical response)
    /// whenever the continuation happened.
    #[serde(default, skip_serializing_if = "WorkContinueDisposition::is_continued")]
    pub disposition: WorkContinueDisposition,
    /// F5: the live active task when the expectation did not match, so the
    /// caller can re-target instead of retrying blindly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<TaskId>,
}

impl WorkContinueResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_optional_task_id("work.continue.task_id", self.task_id)?;
        validate_optional_task_id("work.continue.active_task_id", self.active_task_id)?;
        match self.disposition {
            WorkContinueDisposition::Continued => {
                if self.task_id.is_none() {
                    return Err(ValidationError::new(
                        "work.continue.task_id",
                        "a continued turn must name its task",
                    ));
                }
                if self.active_task_id.is_some() {
                    return Err(ValidationError::new(
                        "work.continue.active_task_id",
                        "only a rejection reports the live active task",
                    ));
                }
            }
            WorkContinueDisposition::ExpectedTaskMismatch => {
                if self.task_id.is_some() {
                    return Err(ValidationError::new(
                        "work.continue.task_id",
                        "a rejected continuation must not name a continued task",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// F5: the run/turn identity a precise cancel or steer names. A nil UUID is
/// never a wildcard — omit the field instead.
fn validate_optional_task_id(field: &'static str, task_id: Option<TaskId>) -> ValidationResult<()> {
    match task_id {
        Some(task_id) if task_id.0.is_nil() => {
            Err(ValidationError::new(field, "must not be a nil UUID"))
        }
        _ => Ok(()),
    }
}

fn validate_optional_turn_id(
    field: &'static str,
    turn_id: Option<agent_contracts::TurnId>,
) -> ValidationResult<()> {
    match turn_id {
        Some(turn_id) if turn_id.0.is_nil() => {
            Err(ValidationError::new(field, "must not be a nil UUID"))
        }
        _ => Ok(()),
    }
}

/// Cancel the run's current in-flight turn. Task completion and child-process
/// cleanup are separate facts and are not claimed here; they surface through
/// the durable `TurnCancelled` acknowledgement and supervision truth.
///
/// F5: naming `expected_task_id` / `expected_turn_id` turns "cancel whatever is
/// current" into "cancel exactly the turn I observed". The runtime compares the
/// expectation against the live turn in the same serialized step that would
/// cancel it, so a slow client can never kill the successor of the turn it saw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCancelRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_turn_id: Option<agent_contracts::TurnId>,
}

impl WorkCancelRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_optional_task_id("work.cancel.expected_task_id", self.expected_task_id)?;
        validate_optional_turn_id("work.cancel.expected_turn_id", self.expected_turn_id)
    }
}

/// F5: the live turn identity reported when a precise cancel did not match.
/// Its presence is the proof that NOTHING was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTurnIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<agent_contracts::TurnId>,
}

impl WorkTurnIdentity {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_optional_task_id("work.cancel.identity_mismatch.task_id", self.task_id)?;
        validate_optional_turn_id("work.cancel.identity_mismatch.turn_id", self.turn_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCancelResponse {
    /// Core's exact post-cancellation truth. `Cancelled` proves the durable
    /// barrier; `NoActiveTurn` is a fact, not a failure.
    pub ack: TurnCancelAck,
    /// F5: present only when the request named an expectation that did not
    /// match. Then `ack` is `NoActiveTurn` because this call cancelled nothing,
    /// and these are the identities that are actually live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_mismatch: Option<WorkTurnIdentity>,
}

impl WorkCancelResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if let Some(mismatch) = &self.identity_mismatch {
            mismatch.validate()?;
            // A mismatch means this request did not cancel anything. Reporting
            // a cancellation ack beside it would claim a stop that never
            // happened for the turn the caller asked about.
            if !matches!(self.ack, TurnCancelAck::NoActiveTurn) {
                return Err(ValidationError::new(
                    "work.cancel.identity_mismatch",
                    "an unmatched expectation cancels nothing and must not carry a cancelled ack",
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// F5 in-task steering and task lifecycle.
//
// `submit` starts NEW work; `steer` corrects work already running. Keeping
// them as separate routes is the point: a correction must never silently
// become a new task, and a new task must never be mistaken for a correction.
// `activate`/`suspend` drive the SAME RuntimeActor that owns the task table —
// there is no second task authority anywhere in this contract.
// ---------------------------------------------------------------------------

/// Char bound on one steering instruction. Corrections are ordinary user
/// instructions, so they share the submitted-goal budget.
pub const MAX_STEER_INSTRUCTION_CHARS: usize = MAX_WORK_GOAL_CHARS;

/// One in-task correction. `expected_task_id` is the caller's target
/// assertion: omit it to steer whatever the run is on, name it to refuse
/// unless the runtime is on exactly that task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSteerRequest {
    pub instruction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_task_id: Option<TaskId>,
}

impl WorkSteerRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_text(
            "work.steer.instruction",
            &self.instruction,
            MAX_STEER_INSTRUCTION_CHARS,
        )?;
        validate_optional_task_id("work.steer.expected_task_id", self.expected_task_id)
    }
}

/// What the runtime did with the correction. All three are receipts, not
/// errors: `Rejected` is a fact about the runtime's identity or capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSteerDisposition {
    /// Admitted and carried by a fresh turn.
    Applied,
    /// Admitted into the running turn's single correction slot — accepted, not
    /// yet executed.
    Queued,
    /// Nothing was admitted; see `rejection`.
    Rejected,
}

/// Why a correction was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSteerRejection {
    /// The caller named a task the runtime is not on.
    ExpectedTaskMismatch,
    /// Nothing is active. Steering never creates a task — submit does.
    NoActiveTask,
    /// The running turn's correction slot is already taken.
    QueueFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSteerResponse {
    pub disposition: WorkSteerDisposition,
    /// The task the correction landed on (absent on a rejection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<WorkSteerRejection>,
    /// The live active task, so a refused caller can re-target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<TaskId>,
}

impl WorkSteerResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_optional_task_id("work.steer.task_id", self.task_id)?;
        validate_optional_task_id("work.steer.active_task_id", self.active_task_id)?;
        match self.disposition {
            WorkSteerDisposition::Applied | WorkSteerDisposition::Queued => {
                if self.task_id.is_none() {
                    return Err(ValidationError::new(
                        "work.steer.task_id",
                        "an admitted correction must name the task it landed on",
                    ));
                }
                if self.rejection.is_some() {
                    return Err(ValidationError::new(
                        "work.steer.rejection",
                        "an admitted correction must not carry a rejection reason",
                    ));
                }
            }
            WorkSteerDisposition::Rejected => {
                if self.rejection.is_none() {
                    return Err(ValidationError::new(
                        "work.steer.rejection",
                        "a rejected correction must name its typed reason",
                    ));
                }
                if self.task_id.is_some() {
                    return Err(ValidationError::new(
                        "work.steer.task_id",
                        "a rejected correction landed on no task",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Activate an existing task by id. The id IS the expectation: an unknown or
/// completed task is refused rather than resolved to "whatever is current".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkActivateRequest {
    pub task_id: TaskId,
}

impl WorkActivateRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.activate.task_id",
                "must not be a nil UUID",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkActivateResponse {
    pub task_id: TaskId,
    /// The task this activation displaced, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previously_active: Option<TaskId>,
    /// The run was already on this task: nothing moved, no focus transition.
    pub already_active: bool,
}

impl WorkActivateResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.activate.task_id",
                "must not be a nil UUID",
            ));
        }
        validate_optional_task_id("work.activate.previously_active", self.previously_active)
    }
}

/// Suspend a task without completing it. Suspension is not completion and this
/// response never claims otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSuspendRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_task_id: Option<TaskId>,
}

impl WorkSuspendRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_optional_task_id("work.suspend.expected_task_id", self.expected_task_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSuspendDisposition {
    Suspended,
    /// Nothing was active — a fact, not a failure.
    NoActiveTask,
    /// The named task is not the live active task; nothing was suspended.
    ExpectedTaskMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSuspendResponse {
    pub disposition: WorkSuspendDisposition,
    /// The task that was suspended (only on `Suspended`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// The live active task on a mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<TaskId>,
}

impl WorkSuspendResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_optional_task_id("work.suspend.task_id", self.task_id)?;
        validate_optional_task_id("work.suspend.active_task_id", self.active_task_id)?;
        match self.disposition {
            WorkSuspendDisposition::Suspended if self.task_id.is_none() => Err(
                ValidationError::new(
                    "work.suspend.task_id",
                    "a suspension must name the task it suspended",
                ),
            ),
            WorkSuspendDisposition::Suspended => Ok(()),
            _ if self.task_id.is_some() => Err(ValidationError::new(
                "work.suspend.task_id",
                "nothing was suspended, so no task may be named",
            )),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// F5 formal checkpoint capture / restore.
//
// These routes go through the runtime's cross-plane transaction (actor state +
// context state + host capability plane + the durable Core authority marker).
// An actor-only dump is NOT a checkpoint and is deliberately unreachable from
// here. Artifacts are named inside the run's own checkpoint store — never an
// arbitrary filesystem path — and a restore verifies the envelope before it
// touches any state.
// ---------------------------------------------------------------------------

/// Bound on a checkpoint artifact name (a store-relative file name, not a path).
pub const MAX_CHECKPOINT_ARTIFACT_NAME_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCheckpointRequest {}

impl WorkCheckpointRequest {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCheckpointResponse {
    /// The store artifact the capture landed in; the same name `work.restore`
    /// accepts.
    pub artifact: String,
    pub payload_bytes: u64,
    pub checkpoint_version: u32,
    /// The run that produced the artifact.
    pub run_id: RunId,
    /// Task rows the artifact carries (a size fact, not a completion claim).
    pub tasks: u32,
}

impl WorkCheckpointResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_checkpoint_artifact_name("work.checkpoint.artifact", &self.artifact)?;
        if self.run_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.checkpoint.run_id",
                "must not be a nil UUID",
            ));
        }
        Ok(())
    }
}

/// Restore one checkpoint. `artifact` names a file in the run's own checkpoint
/// store; omitting it restores the newest artifact that fully verifies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkRestoreRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

impl WorkRestoreRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        match &self.artifact {
            Some(artifact) => {
                validate_checkpoint_artifact_name("work.restore.artifact", artifact)
            }
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkRestoreResponse {
    /// The artifact that was actually restored (resolved, when the request
    /// asked for the newest verifiable one).
    pub artifact: String,
    pub checkpoint_version: u32,
    /// The run the restored checkpoint was captured under. The serving run id
    /// stays what `work.snapshot` reports.
    pub restored_run_id: RunId,
    /// EXEC-6: predecessor runs whose sealed references this restore could not
    /// admit. Restore succeeded; some earlier evidence may be unreadable.
    #[serde(default)]
    pub evidence_degraded: Vec<String>,
}

impl WorkRestoreResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_checkpoint_artifact_name("work.restore.artifact", &self.artifact)?;
        if self.restored_run_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.restore.restored_run_id",
                "must not be a nil UUID",
            ));
        }
        if self.evidence_degraded.len() > MAX_SNAPSHOT_DEGRADED_RUNS {
            return Err(ValidationError::new(
                "work.restore.evidence_degraded",
                format!(
                    "carries {} entries, above the {MAX_SNAPSHOT_DEGRADED_RUNS} entry bound",
                    self.evidence_degraded.len()
                ),
            ));
        }
        for run in &self.evidence_degraded {
            validate_opaque(
                "work.restore.evidence_degraded",
                run,
                MAX_SNAPSHOT_DEGRADED_RUN_ID_BYTES,
            )?;
        }
        Ok(())
    }
}

/// A checkpoint artifact is a NAME inside the run's store, never a path: any
/// separator, parent reference or root marker is refused here so a wire string
/// can never reach outside the store directory.
fn validate_checkpoint_artifact_name(field: &'static str, artifact: &str) -> ValidationResult<()> {
    validate_opaque(field, artifact, MAX_CHECKPOINT_ARTIFACT_NAME_BYTES)?;
    if artifact.contains('/')
        || artifact.contains('\\')
        || artifact.contains(':')
        || artifact == "."
        || artifact == ".."
    {
        return Err(ValidationError::new(
            field,
            "must be a store artifact name, not a path",
        ));
    }
    Ok(())
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
///
/// PLATFORM-3: every snapshot names the run and workspace that produced it,
/// so a client that reconnected — perhaps to a different host incarnation on
/// a shared default endpoint — can tell "this run says X" from "a previous
/// run said X" and can verify the endpoint is bound to the workspace it
/// meant to reach. These are facts about identity, never a completion claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSnapshotResponse {
    pub run_started: bool,
    pub run_completed: bool,
    /// Latest durable event sequence visible to this run.
    pub watermark: u64,
    /// The run whose state this snapshot projects. A client that saw a
    /// different run id before reconnect must rebuild instead of splicing.
    pub run_id: RunId,
    /// The workspace root the answering host is bound to, in the host's
    /// canonical form when the path resolves. A client connecting over a
    /// shared default endpoint verifies this is the workspace it meant.
    pub workspace_root: String,
    pub focus: Option<FocusSnapshot>,
    #[serde(default)]
    pub tasks: Vec<TaskSnapshotEntry>,
    #[serde(default)]
    pub pending_approvals: Vec<PendingApprovalSnapshot>,
    pub resync_required: bool,
    /// EXEC-6 (R2-02): the predecessor runs whose sealed references the last
    /// restore could not admit into this run's lineage. A re-obtainable,
    /// bounded fact (not a fire-once event): an empty list means nothing is
    /// degraded. Absent on older servers — decodes as empty.
    #[serde(default)]
    pub restore_evidence_degraded: Vec<String>,
    /// F5: the run configuration actually in effect, so a headless client can
    /// answer "what is this host configured to do?" without a GUI and without
    /// re-deriving the host's own CLI. Absent on older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_config: Option<WorkRunConfig>,
    /// F5 (CTX-8 接线): the engine's store-outage backpressure as last
    /// observed. Absent means the server does not report it; `active: false`
    /// means a clean pass has lifted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_backpressure: Option<WorkStoreBackpressure>,
    /// F5: whether the active task's retained directive can be continued right
    /// now, with the typed reason when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_readiness: Option<WorkContinueReadiness>,
}

/// F5: the validated run configuration a long-flow client needs to see. Every
/// field is the value in force for THIS run, reported by the host that applied
/// it — not an echo of what a client asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkRunConfig {
    /// `append` | `rolling` | `dynamic` | `service`.
    pub context_policy: String,
    /// The finite per-turn MODEL-round budget the kernel enforces. Counts
    /// model decision rounds, not tool calls.
    pub max_model_rounds: u32,
    /// Where that budget came from: `cli` when the operator set it explicitly,
    /// `kernel_default` otherwise.
    pub max_model_rounds_source: String,
    /// Compaction calls allowed per maintenance pass.
    pub maintenance_max_calls_per_maintain: u32,
    /// Compaction token ceiling per maintenance pass. `None` means unbounded —
    /// reported honestly instead of as a large number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_max_tokens_per_maintain: Option<u64>,
    /// The independent maintenance transport's time bound, when one is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_timeout_secs: Option<u64>,
    /// The provider profile digest in force; absent in demo/mock mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_digest: Option<String>,
    /// The provider prompt-cache mode in force, when the provider reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_mode: Option<String>,
    /// Whether this host serves observation-only sessions.
    pub read_only: bool,
}

/// Bound on one opaque configuration string (policy name, digest, mode).
pub const MAX_RUN_CONFIG_FIELD_BYTES: usize = 128;

impl WorkRunConfig {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_identifier(
            "work.snapshot.effective_config.context_policy",
            &self.context_policy,
            MAX_RUN_CONFIG_FIELD_BYTES,
        )?;
        validate_identifier(
            "work.snapshot.effective_config.max_model_rounds_source",
            &self.max_model_rounds_source,
            MAX_RUN_CONFIG_FIELD_BYTES,
        )?;
        // A zero round budget could never complete a turn; a host reporting it
        // is misconfigured, and silently accepting it would hide that.
        if self.max_model_rounds == 0 {
            return Err(ValidationError::new(
                "work.snapshot.effective_config.max_model_rounds",
                "must be at least one model round",
            ));
        }
        if let Some(digest) = &self.provider_profile_digest {
            validate_opaque(
                "work.snapshot.effective_config.provider_profile_digest",
                digest,
                MAX_RUN_CONFIG_FIELD_BYTES,
            )?;
        }
        if let Some(mode) = &self.prompt_cache_mode {
            validate_identifier(
                "work.snapshot.effective_config.prompt_cache_mode",
                mode,
                MAX_RUN_CONFIG_FIELD_BYTES,
            )?;
        }
        Ok(())
    }
}

/// F5: the store-outage backpressure fact carried on the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkStoreBackpressure {
    /// The last boundary pass hit the retry-list cap: NEW body production is
    /// throttled while every control/query channel stays live.
    pub active: bool,
    pub externalize_deferred: u64,
    pub store_io_failures: u64,
}

impl WorkStoreBackpressure {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

/// F5: why `work.continue` would (or would not) start a turn right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkContinueReason {
    Ready,
    NoActiveTask,
    TurnRunning,
    RecoveryRequired,
    CleanupInFlight,
    /// The active task holds no directive body to replay.
    NoRetainedDirective,
    /// A legacy record sits exactly at the old anchor cap, where a complete
    /// and a truncated instruction are indistinguishable: continuation refuses
    /// rather than replay a prefix.
    DirectiveMayBeTruncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContinueReadiness {
    pub can_continue: bool,
    pub reason: WorkContinueReason,
}

impl WorkContinueReadiness {
    pub fn validate(&self) -> ValidationResult<()> {
        // The boolean is a projection of the reason, never an independent
        // claim: a client must not be able to read "ready" beside a blocker.
        if self.can_continue != matches!(self.reason, WorkContinueReason::Ready) {
            return Err(ValidationError::new(
                "work.snapshot.continue_readiness",
                "can_continue must agree with the typed reason",
            ));
        }
        Ok(())
    }
}

impl WorkSnapshotResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        // PLATFORM-3: the identity facts are mandatory — a snapshot that
        // cannot say which run and workspace produced it is not a valid
        // projection, because a reconnecting client cannot tell whose facts
        // it is reading.
        if self.run_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.snapshot.run_id",
                "must not be a nil UUID",
            ));
        }
        validate_opaque(
            "work.snapshot.workspace_root",
            &self.workspace_root,
            MAX_SNAPSHOT_WORKSPACE_ROOT_BYTES,
        )?;
        if self.restore_evidence_degraded.len() > MAX_SNAPSHOT_DEGRADED_RUNS {
            return Err(ValidationError::new(
                "work.snapshot.restore_evidence_degraded",
                format!(
                    "carries {} entries, above the {MAX_SNAPSHOT_DEGRADED_RUNS} entry bound",
                    self.restore_evidence_degraded.len()
                ),
            ));
        }
        for run in &self.restore_evidence_degraded {
            validate_opaque(
                "work.snapshot.restore_evidence_degraded",
                run,
                MAX_SNAPSHOT_DEGRADED_RUN_ID_BYTES,
            )?;
        }
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
        if let Some(config) = &self.effective_config {
            config.validate()?;
        }
        if let Some(backpressure) = &self.store_backpressure {
            backpressure.validate()?;
        }
        if let Some(readiness) = &self.continue_readiness {
            readiness.validate()?;
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

// ---------------------------------------------------------------------------
// EXEC-8 (R2-09): cold completion lookup. Read-only and run-scoped; the
// answer is typed evidence — a retired record read back from the durable
// journal, an honestly bounded unknown, or the hot-table pointer.
// ---------------------------------------------------------------------------

/// The bounded facts of one retired completion. Mirrors the runtime's
/// `TaskCompletionLookup::Retired` payload; legacy completions carry empty
/// artifacts and no digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkCompletionFact {
    /// The task row is still in the hot table — `task_detail` covers it.
    Hot,
    /// Retired from the hot window; the durable journal carried the
    /// completion.
    Retired {
        summary: String,
        anchor_revision: u64,
        artifacts: Vec<String>,
        final_output_digest: Option<String>,
    },
    /// The bounded journal window could not reach this task's era — an
    /// honestly bounded unknown, never "does not exist".
    BeyondJournalWindow,
    /// The window covered this task's era and nothing was journaled.
    Unknown,
}

impl WorkCompletionFact {
    pub fn validate(&self) -> ValidationResult<()> {
        match self {
            Self::Hot | Self::BeyondJournalWindow | Self::Unknown => Ok(()),
            Self::Retired {
                summary,
                anchor_revision: _,
                artifacts,
                final_output_digest,
            } => {
                validate_text(
                    "work.task_completion.summary",
                    summary,
                    MAX_COMPLETION_SUMMARY_CHARS,
                )?;
                if artifacts.len() > MAX_COMPLETION_ARTIFACTS + 1 {
                    return Err(ValidationError::new(
                        "work.task_completion.artifacts",
                        format!(
                            "carries {} references, above the {} bound",
                            artifacts.len(),
                            MAX_COMPLETION_ARTIFACTS + 1
                        ),
                    ));
                }
                for artifact in artifacts {
                    validate_text(
                        "work.task_completion.artifact",
                        artifact,
                        MAX_COMPLETION_REF_CHARS,
                    )?;
                }
                if let Some(digest) = final_output_digest {
                    validate_text(
                        "work.task_completion.final_output_digest",
                        digest,
                        MAX_COMPLETION_DIGEST_CHARS,
                    )?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskCompletionRequest {
    pub task_id: TaskId,
}

impl WorkTaskCompletionRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.task_completion.task_id",
                "must not be a nil UUID",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskCompletionResponse {
    pub task_id: TaskId,
    pub fact: WorkCompletionFact,
}

impl WorkTaskCompletionResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.task_id.0.is_nil() {
            return Err(ValidationError::new(
                "work.task_completion.task_id",
                "must not be a nil UUID",
            ));
        }
        self.fact.validate()
    }
}

// ---------------------------------------------------------------------------
// PLATFORM-1 (F06): exact-request submission receipt query.
// ---------------------------------------------------------------------------

/// Ask whether THIS caller's submission was admitted. The caller names its own
/// `client_request_id`; the query never matches on goal text, and the
/// envelope's `message_id` is never used as (nor conflated with) the request
/// identity. Run-scoped and read-only.
///
/// `payload_digest` is optional and is only a comparison token the caller
/// computes over its OWN payload with [`submission_payload_digest`]'s
/// documented preimage — it is never authority and the server never echoes
/// goal text back. When present, the answer can distinguish "this exact
/// payload was admitted" from "a different payload holds this id"
/// (`KnownRejected`). When absent, the answer can only speak to identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSubmitResultRequest {
    pub client_request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_digest: Option<String>,
}

impl WorkSubmitResultRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_opaque(
            "work.submit_result.client_request_id",
            &self.client_request_id,
            MAX_CLIENT_REQUEST_ID_BYTES,
        )?;
        if let Some(digest) = &self.payload_digest {
            validate_opaque(
                "work.submit_result.payload_digest",
                digest,
                MAX_SUBMIT_PAYLOAD_DIGEST_BYTES,
            )?;
        }
        Ok(())
    }
}

/// The documented preimage for the submission payload digest, so a Rust host
/// and a .NET client hash identically. Exposed as a constant so both sides
/// derive it from one place instead of copying a literal.
pub const SUBMIT_PAYLOAD_DIGEST_DOMAIN: &str = "focus-agent.platform.work.submit-payload.v1";

/// Compute the submission payload digest over a goal exactly as the runtime
/// does: domain-separated SHA-256, lowercase hex. This is the comparison token
/// the exact-request query accepts; it is not an authority surface.
pub fn submission_payload_digest(goal: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(SUBMIT_PAYLOAD_DIGEST_DOMAIN.as_bytes());
    hasher.update([0u8]);
    hasher.update(goal.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// What the ledger can testify about one `client_request_id`. Every variant is
/// a fact, not an instruction: `Unknown` and `Expired` mean the runtime cannot
/// prove either admission or non-admission, so a client must not read them as
/// "not executed" and must not auto-resend a side-effecting request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSubmitResultDisposition {
    /// This run admitted the id and applied the goal.
    Accepted,
    /// The id was already admitted for the identical payload; no second
    /// execution happened. The original admission is the authority.
    AlreadyAccepted,
    /// The id was seen, but with a different payload. The current request was
    /// never admitted; the recorded one was. Terminal — reusing the id will
    /// never succeed.
    KnownRejected,
    /// The runtime has no evidence for this id: it was never seen, or the
    /// bounded process-lifetime window already evicted it. Not proof of
    /// non-execution.
    Unknown,
    /// The id belonged to an earlier host/run epoch and this run has no
    /// carried-over evidence. Not proof of non-execution.
    Expired,
}

impl WorkSubmitResultDisposition {
    /// Does this disposition establish that the exact request was admitted?
    pub fn is_admitted(self) -> bool {
        matches!(self, Self::Accepted | Self::AlreadyAccepted)
    }

    /// Does this disposition leave the outcome genuinely undetermined?
    pub fn is_indeterminate(self) -> bool {
        matches!(self, Self::Unknown | Self::Expired)
    }
}

/// The receipt query's answer, bound to the epoch and run that produced it so
/// a client can tell "this run says X" from "a previous run said X".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSubmitResultResponse {
    /// The run whose ledger answered. A client that reconnects to a different
    /// run must re-query instead of trusting a stale answer.
    pub run_id: RunId,
    /// Echo of the queried key, so a late response can never be applied to a
    /// different request.
    pub client_request_id: String,
    pub disposition: WorkSubmitResultDisposition,
    /// Present only when the disposition is `Accepted` / `AlreadyAccepted` /
    /// `KnownRejected`: the task the recorded admission bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Present only when the disposition is `KnownRejected`: a stable digest of
    /// the payload the id was originally admitted for. Lets a client show
    /// "same id, different content" without shipping the old goal text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_payload_digest: Option<String>,
}

impl WorkSubmitResultResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_opaque(
            "work.submit_result.client_request_id",
            &self.client_request_id,
            MAX_CLIENT_REQUEST_ID_BYTES,
        )?;
        // A disposition that claims a recorded admission must name its task;
        // an indeterminate one must not pretend to.
        match self.disposition {
            WorkSubmitResultDisposition::Accepted
            | WorkSubmitResultDisposition::AlreadyAccepted
            | WorkSubmitResultDisposition::KnownRejected => {
                let Some(task_id) = self.task_id else {
                    return Err(ValidationError::new(
                        "work.submit_result.task_id",
                        "a recorded disposition must name its task",
                    ));
                };
                if task_id.0.is_nil() {
                    return Err(ValidationError::new(
                        "work.submit_result.task_id",
                        "must not be a nil UUID",
                    ));
                }
            }
            WorkSubmitResultDisposition::Unknown | WorkSubmitResultDisposition::Expired => {
                if self.task_id.is_some() {
                    return Err(ValidationError::new(
                        "work.submit_result.task_id",
                        "an indeterminate disposition must not name a task",
                    ));
                }
            }
        }
        // The rejected-payload digest only exists on a conflict.
        if self.disposition == WorkSubmitResultDisposition::KnownRejected {
            validate_opaque(
                "work.submit_result.accepted_payload_digest",
                self.accepted_payload_digest.as_deref().unwrap_or_default(),
                MAX_SUBMIT_PAYLOAD_DIGEST_BYTES,
            )?;
        } else if self.accepted_payload_digest.is_some() {
            return Err(ValidationError::new(
                "work.submit_result.accepted_payload_digest",
                "only a rejected conflict carries the recorded payload digest",
            ));
        }
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
        if let Some(limit) = self.limit
            && (limit == 0 || limit > MAX_CHANGES_LIMIT)
        {
            return Err(ValidationError::new(
                "work.changes.limit",
                format!("must be in 1..={MAX_CHANGES_LIMIT}"),
            ));
        }
        if let Some(after_tx) = &self.after_tx {
            validate_opaque("work.changes.after_tx", after_tx, MAX_CHANGE_TX_ID_BYTES)?;
        }
        Ok(())
    }
}

/// One mirrored workspace journal record. `old_content` itself still never
/// travels as a protocol field: the platform spills a captured before-body
/// into the run's sealed artifact store and hands back a *reference*
/// (PLATFORM-2), so the review chain locates the actual content through the
/// same bounded, digest-verified paging every other read uses.
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
        /// Run-scoped `artifact://` reference to the captured before-body,
        /// readable with the paged `work.artifact` route. Absent when the
        /// journal captured no content (oversized capture or empty file).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_content_artifact: Option<String>,
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
                old_content_artifact,
                ..
            } => {
                if let Some(reference) = old_content_artifact {
                    validate_opaque(
                        "work.changes.old_content_artifact",
                        reference,
                        agent_contracts::MAX_ARTIFACT_REFERENCE_BYTES,
                    )?;
                }
                (
                    tx_id,
                    vec![
                        ("work.changes.tool", tool),
                        ("work.changes.path", path),
                        ("work.changes.action", action),
                        ("work.changes.before_hash", before_hash),
                        ("work.changes.after_hash", after_hash),
                    ],
                )
            }
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

/// Read one run-scoped artifact reference with an explicit byte budget and
/// (optionally) a byte offset — PLATFORM-2 (F08): the bounded-paging read.
/// `reference` is a sealed `artifact://` locator; the workspace verifies the
/// run binding and (when the reference carries one) the content digest before
/// the router reads anything. The sealed artifact is immutable, so paging
/// cannot quietly switch versions: every page re-verifies the same identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkArtifactRequest {
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
    /// Byte position where this read starts. Absent/0 reads from the
    /// beginning (the historical prefix read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

impl WorkArtifactRequest {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_opaque(
            "work.artifact.reference",
            &self.reference,
            agent_contracts::MAX_ARTIFACT_REFERENCE_BYTES,
        )?;
        if let Some(max) = self.max_bytes
            && (max == 0 || max > MAX_ARTIFACT_READ_BYTES)
        {
            return Err(ValidationError::new(
                "work.artifact.max_bytes",
                format!("must be in 1..={MAX_ARTIFACT_READ_BYTES}"),
            ));
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
    /// The byte position where `content_base64` starts (0 for the
    /// historical prefix read).
    #[serde(default)]
    pub offset: u64,
    /// `true` only when there are more bytes beyond this window (cut at the
    /// requested budget); a window that reaches the end of the artifact is
    /// never marked truncated.
    pub truncated: bool,
    /// The next byte position to read, present exactly when `truncated` is
    /// true. `truncated == false` (eof) carries no cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
    /// The (possibly bounded) body window starting at `offset`, base64.
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
        let end = self.offset + decoded_len as u64;
        if self.offset > self.size_bytes || end > self.size_bytes {
            return Err(ValidationError::new(
                "work.artifact.size_bytes",
                format!(
                    "window [{offset},{end}) does not fit the artifact of {size} bytes",
                    offset = self.offset,
                    end = end,
                    size = self.size_bytes
                ),
            ));
        }
        if self.truncated != self.next_offset.is_some() {
            return Err(ValidationError::new(
                "work.artifact.next_offset",
                "the continuation cursor must be present exactly when the window is truncated",
            ));
        }
        if self.truncated {
            let next = self.next_offset.expect("checked above");
            if next != end || end >= self.size_bytes {
                return Err(ValidationError::new(
                    "work.artifact.next_offset",
                    format!(
                        "must point at the first unread byte ({end}) of a window that ends before {size}",
                        end = end,
                        size = self.size_bytes
                    ),
                ));
            }
        } else if end != self.size_bytes {
            return Err(ValidationError::new(
                "work.artifact.size_bytes",
                format!(
                    "a non-truncated window must reach the end of the artifact ({size}), not {end}",
                    end = end,
                    size = self.size_bytes
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
        if let Some(limit) = self.limit
            && (limit == 0 || limit as usize > MAX_CONTEXT_ITEMS)
        {
            return Err(ValidationError::new(
                "work.context.limit",
                format!("must be in 1..={MAX_CONTEXT_ITEMS}"),
            ));
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

pub fn validate_work_steer_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSteerRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_steer, |payload| {
        payload.validate()
    })
}

pub fn validate_work_steer_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSteerRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkSteerResponse>>,
) -> ValidationResult<()> {
    validate_work_steer_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())?;
    if let PlatformResponse::Success { value } = &response.payload {
        // A correction that names a target must never be reported as applied to
        // a different task: that is exactly the misdelivery this route exists
        // to prevent.
        if let (Some(expected), Some(landed)) = (request.payload.expected_task_id, value.task_id)
            && expected != landed
        {
            return Err(ValidationError::new(
                "work.steer.task_id",
                "an admitted correction must land on the task the request named",
            ));
        }
    }
    Ok(())
}

pub fn validate_work_activate_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkActivateRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_activate, |payload| {
        payload.validate()
    })
}

pub fn validate_work_activate_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkActivateRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkActivateResponse>>,
) -> ValidationResult<()> {
    validate_work_activate_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())?;
    if let PlatformResponse::Success { value } = &response.payload
        && value.task_id != request.payload.task_id
    {
        return Err(ValidationError::new(
            "work.activate.task_id",
            "must echo the task the request asked to activate",
        ));
    }
    Ok(())
}

pub fn validate_work_suspend_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSuspendRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_suspend, |payload| {
        payload.validate()
    })
}

pub fn validate_work_suspend_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSuspendRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkSuspendResponse>>,
) -> ValidationResult<()> {
    validate_work_suspend_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())?;
    if let PlatformResponse::Success { value } = &response.payload
        && let (Some(expected), Some(suspended)) =
            (request.payload.expected_task_id, value.task_id)
        && expected != suspended
    {
        return Err(ValidationError::new(
            "work.suspend.task_id",
            "must not report suspending a task other than the one named",
        ));
    }
    Ok(())
}

pub fn validate_work_checkpoint_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkCheckpointRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_checkpoint, |payload| {
        payload.validate()
    })
}

pub fn validate_work_checkpoint_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkCheckpointRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkCheckpointResponse>>,
) -> ValidationResult<()> {
    validate_work_checkpoint_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_restore_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkRestoreRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_restore, |payload| {
        payload.validate()
    })
}

pub fn validate_work_restore_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkRestoreRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkRestoreResponse>>,
) -> ValidationResult<()> {
    validate_work_restore_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())?;
    if let PlatformResponse::Success { value } = &response.payload
        && let Some(asked) = &request.payload.artifact
        && asked != &value.artifact
    {
        return Err(ValidationError::new(
            "work.restore.artifact",
            "must name the artifact the request asked to restore",
        ));
    }
    Ok(())
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

pub fn validate_work_task_completion_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkTaskCompletionRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(
        profile,
        request,
        Route::is_work_task_completion,
        |payload| payload.validate(),
    )
}

pub fn validate_work_task_completion_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkTaskCompletionRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkTaskCompletionResponse>>,
) -> ValidationResult<()> {
    validate_work_task_completion_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_task_detail_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkTaskDetailRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkTaskDetailResponse>>,
) -> ValidationResult<()> {
    validate_work_task_detail_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())
}

pub fn validate_work_submit_result_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSubmitResultRequest>,
) -> ValidationResult<()> {
    validate_run_scoped_request(profile, request, Route::is_work_submit_result, |payload| {
        payload.validate()
    })
}

pub fn validate_work_submit_result_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<WorkSubmitResultRequest>,
    response: &PlatformEnvelope<PlatformResponse<WorkSubmitResultResponse>>,
) -> ValidationResult<()> {
    validate_work_submit_result_request(profile, request)?;
    validate_run_scoped_response(profile, request, response, |payload| payload.validate())?;
    if let PlatformResponse::Success { value } = &response.payload {
        // The answer must speak about the request that was actually asked.
        if value.client_request_id != request.payload.client_request_id {
            return Err(ValidationError::new(
                "work.submit_result.client_request_id",
                "must echo the queried client request id",
            ));
        }
    }
    Ok(())
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

    fn run_id() -> RunId {
        RunId::from_str(RUN).unwrap()
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
            run_id: run_id(),
            workspace_root: "/workspaces/alpha".into(),
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
            restore_evidence_degraded: Vec::new(),
            effective_config: None,
            store_backpressure: None,
            continue_readiness: None,
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

    /// PLATFORM-3: a snapshot must name the run and workspace that produced
    /// it. Silent defaults would let a reconnecting client read one host's
    /// facts while believing they are another's, so the identity facts are
    /// mandatory and bounded.
    #[test]
    fn snapshot_identity_facts_are_mandatory_and_bounded() {
        let request = run_scoped_request(Route::work_snapshot(), WorkSnapshotRequest {});
        let snapshot = WorkSnapshotResponse {
            run_started: true,
            run_completed: false,
            watermark: 1,
            run_id: run_id(),
            workspace_root: "/workspaces/alpha".into(),
            focus: None,
            tasks: vec![],
            pending_approvals: vec![],
            resync_required: false,
            restore_evidence_degraded: Vec::new(),
            effective_config: None,
            store_backpressure: None,
            continue_readiness: None,
        };
        validate_work_snapshot_response(
            &profile(),
            &request,
            &response(&request, snapshot.clone()),
        )
        .unwrap();

        // A nil run id is not a run identity.
        let mut anonymous = snapshot.clone();
        anonymous.run_id = RunId::from_str("00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(
            anonymous.validate().unwrap_err().field(),
            "work.snapshot.run_id"
        );

        // An empty or control-bearing workspace root is not an identity.
        let mut unnamed = snapshot.clone();
        unnamed.workspace_root.clear();
        assert_eq!(
            unnamed.validate().unwrap_err().field(),
            "work.snapshot.workspace_root"
        );
        let mut control = snapshot;
        control.workspace_root = "/workspaces/al\u{1}pha".into();
        assert_eq!(
            control.validate().unwrap_err().field(),
            "work.snapshot.workspace_root"
        );

        // Over the path bound is refused; at the bound is legal.
        let mut long = WorkSnapshotResponse {
            run_started: true,
            run_completed: false,
            watermark: 1,
            run_id: run_id(),
            workspace_root: "/".repeat(MAX_SNAPSHOT_WORKSPACE_ROOT_BYTES + 1),
            focus: None,
            tasks: vec![],
            pending_approvals: vec![],
            resync_required: false,
            restore_evidence_degraded: Vec::new(),
            effective_config: None,
            store_backpressure: None,
            continue_readiness: None,
        };
        assert!(long.validate().is_err());
        long.workspace_root = "/".repeat(MAX_SNAPSHOT_WORKSPACE_ROOT_BYTES);
        long.validate().unwrap();
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
            run_id: run_id(),
            workspace_root: "/workspaces/alpha".into(),
            focus: None,
            tasks: vec![],
            pending_approvals: vec![approval(Some(
                "x".repeat(MAX_SNAPSHOT_APPROVAL_TARGET_CHARS + 1),
            ))],
            resync_required: false,
            restore_evidence_degraded: Vec::new(),
            effective_config: None,
            store_backpressure: None,
            continue_readiness: None,
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
        let request = run_scoped_request(Route::work_cancel(), WorkCancelRequest::default());
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
            let success = response(
                &request,
                WorkCancelResponse {
                    ack: ack.clone(),
                    identity_mismatch: None,
                },
            );
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
        let request = run_scoped_request(Route::work_continue(), WorkContinueRequest::default());
        validate_work_continue_request(&profile(), &request).unwrap();

        let success = response(
            &request,
            WorkContinueResponse {
                task_id: Some(task_id()),
                disposition: WorkContinueDisposition::Continued,
                active_task_id: None,
            },
        );
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

    /// PLATFORM-1: the exact-request query answers about the id it was asked
    /// about, and every disposition carries exactly the facts it can prove.
    #[test]
    fn submit_result_query_binds_the_answer_to_the_asked_request() {
        let request = run_scoped_request(
            Route::work_submit_result(),
            WorkSubmitResultRequest {
                client_request_id: "client-7".into(),
                payload_digest: Some(submission_payload_digest("migrate the retry table")),
            },
        );
        validate_work_submit_result_request(&profile(), &request).unwrap();

        // An identity-only query (no payload) is legal too, and then answers
        // only about identity.
        let identity_only = run_scoped_request(
            Route::work_submit_result(),
            WorkSubmitResultRequest {
                client_request_id: "client-7".into(),
                payload_digest: None,
            },
        );
        validate_work_submit_result_request(&profile(), &identity_only).unwrap();

        // Admitted: the id and its task.
        let admitted = response(
            &request,
            WorkSubmitResultResponse {
                run_id: RunId::from_str(RUN).unwrap(),
                client_request_id: "client-7".into(),
                disposition: WorkSubmitResultDisposition::Accepted,
                task_id: Some(task_id()),
                accepted_payload_digest: None,
            },
        );
        validate_work_submit_result_response(&profile(), &request, &admitted).unwrap();

        // Indeterminate: no task, because none can be proven.
        let unknown = response(
            &request,
            WorkSubmitResultResponse {
                run_id: RunId::from_str(RUN).unwrap(),
                client_request_id: "client-7".into(),
                disposition: WorkSubmitResultDisposition::Unknown,
                task_id: None,
                accepted_payload_digest: None,
            },
        );
        validate_work_submit_result_response(&profile(), &request, &unknown).unwrap();

        // Conflict: the recorded task plus the digest it was admitted for.
        let rejected = response(
            &request,
            WorkSubmitResultResponse {
                run_id: RunId::from_str(RUN).unwrap(),
                client_request_id: "client-7".into(),
                disposition: WorkSubmitResultDisposition::KnownRejected,
                task_id: Some(task_id()),
                accepted_payload_digest: Some("ab".repeat(32)),
            },
        );
        validate_work_submit_result_response(&profile(), &request, &rejected).unwrap();

        // The answer must be about the requested id, not some other one.
        let mut mismatched = admitted.clone();
        if let PlatformResponse::Success { value } = &mut mismatched.payload {
            value.client_request_id = "client-8".into();
        }
        let error =
            validate_work_submit_result_response(&profile(), &request, &mismatched).unwrap_err();
        assert_eq!(error.field(), "work.submit_result.client_request_id");

        // An empty id can never be queried.
        let mut empty = request.clone();
        empty.payload.client_request_id.clear();
        assert!(empty.payload.validate().is_err());
    }

    /// The submission payload digest is domain-separated and content-bound, so
    /// the same goal always hashes the same and two different goals never
    /// trivially collide. The golden construction pins the preimage so a .NET
    /// client reproduces it byte-for-byte.
    #[test]
    fn submission_payload_digest_is_stable_and_content_bound() {
        let a = submission_payload_digest("migrate the retry table");
        let b = submission_payload_digest("migrate the retry table");
        let c = submission_payload_digest("migrate the retry tablE");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));

        let mut hasher = sha2::Sha256::new();
        use sha2::Digest as _;
        hasher.update(SUBMIT_PAYLOAD_DIGEST_DOMAIN.as_bytes());
        hasher.update([0u8]);
        hasher.update(b"migrate the retry table");
        let expected: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(a, expected);
    }

    /// A recorded disposition must name its task; an indeterminate one must
    /// not manufacture a task. Silent contradictions are rejected as fields.
    #[test]
    fn submit_result_disposition_and_task_facts_must_agree() {
        let no_task = WorkSubmitResultResponse {
            run_id: RunId::from_str(RUN).unwrap(),
            client_request_id: "client-7".into(),
            disposition: WorkSubmitResultDisposition::Accepted,
            task_id: None,
            accepted_payload_digest: None,
        };
        assert_eq!(
            no_task.validate().unwrap_err().field(),
            "work.submit_result.task_id"
        );

        let invented_task = WorkSubmitResultResponse {
            run_id: RunId::from_str(RUN).unwrap(),
            client_request_id: "client-7".into(),
            disposition: WorkSubmitResultDisposition::Expired,
            task_id: Some(task_id()),
            accepted_payload_digest: None,
        };
        assert_eq!(
            invented_task.validate().unwrap_err().field(),
            "work.submit_result.task_id"
        );

        // A conflict without the recorded digest cannot show "different
        // content"; a non-conflict must not carry one.
        let silent_conflict = WorkSubmitResultResponse {
            run_id: RunId::from_str(RUN).unwrap(),
            client_request_id: "client-7".into(),
            disposition: WorkSubmitResultDisposition::KnownRejected,
            task_id: Some(task_id()),
            accepted_payload_digest: None,
        };
        assert_eq!(
            silent_conflict.validate().unwrap_err().field(),
            "work.submit_result.accepted_payload_digest"
        );
        let stray_digest = WorkSubmitResultResponse {
            run_id: RunId::from_str(RUN).unwrap(),
            client_request_id: "client-7".into(),
            disposition: WorkSubmitResultDisposition::Accepted,
            task_id: Some(task_id()),
            accepted_payload_digest: Some("ab".repeat(32)),
        };
        assert_eq!(
            stray_digest.validate().unwrap_err().field(),
            "work.submit_result.accepted_payload_digest"
        );

        // The disposition helpers keep "admitted" separate from
        // "indeterminate" — the distinction the GUI must not collapse.
        assert!(WorkSubmitResultDisposition::Accepted.is_admitted());
        assert!(WorkSubmitResultDisposition::AlreadyAccepted.is_admitted());
        assert!(!WorkSubmitResultDisposition::KnownRejected.is_admitted());
        assert!(WorkSubmitResultDisposition::Unknown.is_indeterminate());
        assert!(WorkSubmitResultDisposition::Expired.is_indeterminate());
        assert!(!WorkSubmitResultDisposition::Accepted.is_indeterminate());
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
                        old_content_artifact: None,
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
                offset: None,
            },
        );
        validate_work_artifact_request(&profile(), &request).unwrap();

        let mut big_request = request.clone();
        big_request.payload.max_bytes = Some(MAX_ARTIFACT_READ_BYTES + 1);
        assert!(big_request.payload.validate().is_err());

        let mut long_ref = request.clone();
        long_ref.payload.reference = "x".repeat(agent_contracts::MAX_ARTIFACT_REFERENCE_BYTES + 1);
        assert!(long_ref.payload.validate().is_err());

        // A window that reaches the end of the artifact carries no cursor; a
        // truncated window carries the exact next byte position. Both are
        // validated, never assumed. (PLATFORM-2/F08 paging contract.)
        let full = response(
            &request,
            WorkArtifactResponse {
                reference: "artifact://r".into(),
                size_bytes: 5,
                offset: 0,
                truncated: false,
                next_offset: None,
                content_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        );
        validate_work_artifact_response(&profile(), &request, &full).unwrap();

        let truncated = response(
            &request,
            WorkArtifactResponse {
                reference: "artifact://r".into(),
                size_bytes: 100,
                offset: 0,
                truncated: true,
                next_offset: Some(5),
                content_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        );
        validate_work_artifact_response(&profile(), &request, &truncated).unwrap();

        let lies = response(
            &request,
            WorkArtifactResponse {
                reference: "artifact://r".into(),
                size_bytes: 32,
                offset: 0,
                truncated: false,
                next_offset: None,
                content_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        );
        assert!(validate_work_artifact_response(&profile(), &request, &lies).is_err());
    }

    #[test]
    fn artifact_pages_carry_offset_and_continuation_truth() {
        // PLATFORM-2 (F08): a middle page must name where it starts, carry a
        // cursor to the next unread byte, and an eof page must carry none.
        // Every inconsistency below is a rejected answer, not a guess.
        let request = run_scoped_request(
            Route::work_artifact(),
            WorkArtifactRequest {
                reference: "artifact://r".into(),
                max_bytes: Some(8),
                offset: Some(10),
            },
        );
        validate_work_artifact_request(&profile(), &request).unwrap();

        let middle_payload = WorkArtifactResponse {
            reference: "artifact://r".into(),
            size_bytes: 40,
            offset: 10,
            truncated: true,
            next_offset: Some(18),
            content_base64: base64::engine::general_purpose::STANDARD.encode([0u8; 8]),
        };
        let middle_page = response(&request, middle_payload.clone());
        validate_work_artifact_response(&profile(), &request, &middle_page).unwrap();

        // eof page: the window reaches exactly the end; no cursor.
        let eof_payload = WorkArtifactResponse {
            reference: "artifact://r".into(),
            size_bytes: 40,
            offset: 32,
            truncated: false,
            next_offset: None,
            content_base64: base64::engine::general_purpose::STANDARD.encode([0u8; 8]),
        };
        let eof_page = response(&request, eof_payload.clone());
        validate_work_artifact_response(&profile(), &request, &eof_page).unwrap();

        // A cursor that skips bytes (or points backwards) is a lie.
        let mut skipping = middle_payload.clone();
        skipping.next_offset = Some(20);
        assert!(
            validate_work_artifact_response(&profile(), &request, &response(&request, skipping))
                .is_err()
        );

        // A window that overruns the artifact is rejected.
        let mut overrun = eof_payload.clone();
        overrun.offset = 36;
        assert!(
            validate_work_artifact_response(&profile(), &request, &response(&request, overrun))
                .is_err()
        );

        // Truncated with no cursor (and the inverse) is rejected.
        let mut cursorless = middle_payload.clone();
        cursorless.next_offset = None;
        assert!(
            validate_work_artifact_response(&profile(), &request, &response(&request, cursorless))
                .is_err()
        );
        let mut cursor_at_eof = eof_payload.clone();
        cursor_at_eof.next_offset = Some(40);
        cursor_at_eof.truncated = true;
        assert!(
            validate_work_artifact_response(
                &profile(),
                &request,
                &response(&request, cursor_at_eof)
            )
            .is_err()
        );

        // An offset beyond the artifact cannot serve any window.
        let mut beyond = eof_payload;
        beyond.offset = 41;
        assert!(
            validate_work_artifact_response(&profile(), &request, &response(&request, beyond))
                .is_err()
        );
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
