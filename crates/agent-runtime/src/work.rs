//! Shared long-task submission — the common application-layer entry every
//! client (TUI `/work`, headless `--work`, and future platform clients)
//! drives. The whole flow (task create/resume focus, the `task.manage`
//! surface attach, the first user message) runs inside one serialized actor
//! command, so two clients can never interleave focus and message across
//! tasks (the F07 misdelivery race), and every admission returns a stable
//! receipt keyed by the caller's `client_request_id`.

use agent_contracts::{AgentResult, RunId, TaskId};

use crate::RuntimeHandle;
use crate::task::TaskInfo;

/// Bounded process-lifetime dedup ledger. A retried `client_request_id` older
/// than this window is simply unknown again — the submission is not durable
/// and no cross-restart exactly-once is promised.
pub const MAX_PENDING_WORK_SUBMISSIONS: usize = 256;

/// Bound shared with the wire contract so a legal id never trips one bound
/// while passing the other.
pub use agent_platform_protocol::MAX_CLIENT_REQUEST_ID_BYTES;

/// Whether the actor newly admitted and applied this submission, or found the
/// same `client_request_id` with the identical goal already admitted. Neither
/// outcome claims task completion; both return the focused task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkSubmissionDisposition {
    Accepted,
    AlreadyAccepted,
}

/// The stable acceptance receipt for one [`start_long_task`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSubmission {
    pub disposition: WorkSubmissionDisposition,
    /// The task the goal was atomically applied to.
    pub task_id: TaskId,
    /// `task.manage` could not be attached to an empty requirement set; the
    /// goal was still applied and the turn still started.
    pub task_manage_notice: Option<String>,
}

/// One admitted submission kept for idempotent-retry matching. Process-local
/// memory, deliberately not checkpoint authority: after a restart the same id
/// is unknown and the caller must query instead of assuming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkSubmissionRecord {
    pub(crate) client_request_id: String,
    pub(crate) goal: String,
    pub(crate) task_id: TaskId,
}

/// Start (or resume) a long task the way `/work` and `--work` do. The actor
/// creates or resumes the task for `goal`, attaches the long-task checklist
/// surface when the requirement set is empty, and applies the goal once
/// through the normal user-message path — all inside one command.
pub async fn start_long_task(handle: &RuntimeHandle, goal: String) -> AgentResult<WorkSubmission> {
    let client_request_id = format!("work-{}", agent_contracts::OperationId::new());
    handle.start_work(goal, client_request_id).await
}

/// One consistent typed status snapshot (P2). Read at a single point in the
/// serialized actor loop, so it can never interleave with a focus change or
/// a submission. `watermark` is the latest durable event sequence the state
/// reflects; a client whose live stream lags behind this value has a hole
/// and must rebuild instead of splicing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatusSnapshot {
    pub run_id: RunId,
    pub serving: bool,
    pub watermark: u64,
    pub focus_task_id: Option<TaskId>,
    pub focus_goal: String,
    /// The focused task's own anchor revision. Revisions are per-task and
    /// are never compared or maxed across tasks (F06).
    pub focus_anchor_revision: u64,
    /// Every task the runtime knows, with per-task revisions.
    pub tasks: Vec<TaskInfo>,
}
