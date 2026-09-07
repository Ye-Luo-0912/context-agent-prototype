//! Thin TUI adapter over the shared long-task entry (P1). The actual flow —
//! task create/resume, the `task.manage` attach and the first user message —
//! is one atomic actor command in `agent_runtime::work`; see there for the
//! acceptance-receipt and idempotent-retry semantics.

use agent_runtime::{RuntimeHandle, WorkSubmission};

/// Start (or resume) a long task through the atomic `start_work` command.
/// Returns the bounded `task.manage` attach notice, when the checklist
/// surface could not be attached; the goal is still applied in that case.
pub async fn start_long_task(
    handle: &RuntimeHandle,
    goal: String,
) -> anyhow::Result<WorkSubmission> {
    let submission = agent_runtime::work::start_long_task(handle, goal).await?;
    Ok(submission)
}
