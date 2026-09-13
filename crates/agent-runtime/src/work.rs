//! Shared long-task submission — the common application-layer entry every
//! client (TUI `/work`, headless `--work`, and future platform clients)
//! drives. The whole flow (task create/resume focus, the `task.manage`
//! surface attach, the first user message) runs inside one serialized actor
//! command, so two clients can never interleave focus and message across
//! tasks (the F07 misdelivery race), and every admission returns a stable
//! receipt keyed by the caller's `client_request_id`.

use agent_contracts::{AgentResult, RunId, TaskAnchorView, TaskId};

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
    /// Stable digest of the payload this id was admitted for. Kept so a
    /// conflicting retry can be classified as "same id, different content"
    /// without shipping or comparing the original goal text, and so the
    /// conflict answer stays truthful after the goal itself is no longer the
    /// evaluator (F06: goal text is never the idempotency key).
    pub(crate) payload_digest: String,
}

/// Stable, bounded digest of one submission payload. The identity of a
/// submission is its content, not its goal text as an idempotency key; the
/// digest is what a conflict comparison uses. Delegates to the shared wire
/// implementation so the host and its clients hash identically.
pub(crate) use agent_platform_protocol::submission_payload_digest;

/// What the runtime's own ledger can testify about one `client_request_id`
/// (PLATFORM-1 / F06). Every variant is a fact about evidence, never an
/// instruction: `Unknown` proves nothing in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkSubmissionQuery {
    /// This run recorded the id. `matches` is whether the caller's own
    /// payload digest (when it supplied one) equals the recorded one; `None`
    /// means the caller named no payload, so only identity is proven.
    Recorded {
        task_id: TaskId,
        payload_digest: String,
        matches: Option<bool>,
    },
    /// No evidence in this run's bounded ledger — never seen, or already
    /// evicted. Not proof of non-execution.
    Unknown,
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
    /// EXEC-2: the task table's bounded hot-state facts (counts and
    /// retirement counters), so "is growth still bounded?" is observable.
    pub task_hot_state: crate::task::TaskHotStateSummary,
    /// EXEC-6 (R2-02): the predecessor runs whose sealed references the last
    /// restore could not admit into this run's lineage (typed degradation).
    /// A re-obtainable fact, not a fire-once event: empty means nothing is
    /// degraded. Bounded to the lineage cap.
    pub restore_evidence_degraded: Vec<String>,
    /// CTX-8 接线 (R3-08): the engine's store-outage backpressure, observed
    /// at the last boundary pass and re-served here until the store
    /// recovers and a clean pass lands. A failing store keeps its items
    /// owned (pending externalize), so the honest runtime response is to
    /// limit NEW body production while every control/query channel stays
    /// live; `Some(false)`/`None` mean no backpressure is observed.
    pub store_backpressure: Option<crate::work::StoreBackpressure>,
}

/// CTX-8 接线 (R3-08): the observation itself. `active == true` means the
/// last boundary pass hit the retry-list cap (the store is failing and new
/// body production is throttled); `active == false` means a clean pass ran
/// after the outage — the throttle is lifted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreBackpressure {
    pub active: bool,
    pub externalize_deferred: u64,
    pub store_io_failures: u64,
}

/// EXEC-8 (R2-09): what the runtime can testify about one task's COMPLETED
/// outcome, hot or cold. A task that left the bounded hot window stays
/// reviewable from the durable journal; every answer is a fact about
/// evidence, never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCompletionLookup {
    /// The task row is still in the hot table — `TaskDetail` (and the
    /// in-memory completion record) covers it.
    Hot,
    /// Retired from the hot window; the durable journal carried its
    /// completion. `artifacts` are the sealed evidence references the
    /// record carried (bounded, same set the hot record held).
    Retired {
        summary: String,
        anchor_revision: u64,
        artifacts: Vec<String>,
        final_output_digest: Option<String>,
    },
    /// The bounded journal window could not reach this task's era — an
    /// honestly bounded unknown, not "does not exist".
    BeyondJournalWindow,
    /// The window covered this task's era and nothing was journaled.
    Unknown,
}

/// EXEC-8 残余 (R3-11): the pure cold half of a completion lookup over a
/// given set of journal partitions (current run first, then restored
/// ancestors newest-first). The newest matching fact wins; an incomplete
/// window answers BeyondJournalWindow, a backend without read support is
/// an honestly bounded unknown, and a corrupted row fails closed. Pure
/// reads: no actor state, no model, no tools, no checkpoint write.
pub(crate) async fn cold_completion_lookup_in(
    journal: &Option<std::sync::Arc<dyn agent_contracts::EventJournal>>,
    runs: &[RunId],
    task_id: TaskId,
) -> AgentResult<TaskCompletionLookup> {
    const LOOKUP_WINDOW_ROWS: usize = 4096;
    let Some(journal) = journal else {
        return Ok(TaskCompletionLookup::BeyondJournalWindow);
    };
    let mut found = None;
    let mut all_complete = true;
    for run in runs {
        let (envelopes, window_complete) = match journal.read_tail(*run, LOOKUP_WINDOW_ROWS).await {
            // A backend without read support: honestly bounded, never
            // "does not exist".
            Ok(None) => {
                all_complete = false;
                continue;
            }
            Ok(Some(pair)) => pair,
            // A corrupted journal row fails closed as a typed error —
            // never as "unknown", which would read as "never happened".
            Err(error) => return Err(error),
        };
        all_complete &= window_complete;
        for envelope in &envelopes {
            if let agent_contracts::RuntimeEvent::TaskCompleted {
                task_id: completed,
                anchor_revision,
                summary,
                artifacts,
                final_output_digest,
            } = &envelope.event
                && *completed == task_id
            {
                found = Some((
                    summary.clone(),
                    *anchor_revision,
                    artifacts.clone(),
                    final_output_digest.clone(),
                ));
            }
        }
    }
    match found {
        Some((summary, anchor_revision, artifacts, final_output_digest)) => {
            Ok(TaskCompletionLookup::Retired {
                summary,
                anchor_revision,
                artifacts,
                final_output_digest,
            })
        }
        None if all_complete => Ok(TaskCompletionLookup::Unknown),
        None => Ok(TaskCompletionLookup::BeyondJournalWindow),
    }
}

/// One task's full read-only detail (B3): its identity facts plus the
/// complete anchor projection the GUI renders as the task card. Assembled in
/// one serialized actor step; reading never mutates state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDetailSnapshot {
    pub task: TaskInfo,
    pub anchor: TaskAnchorView,
}
