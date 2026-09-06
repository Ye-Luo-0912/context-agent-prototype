//! Shared long-task entry used by TUI `/work` and headless `--work`.
//! Composes existing RuntimeHandle paths only: no second orchestrator.

use agent_contracts::{ToolSurfaceDemand, ToolSurfaceRequirement};
use agent_runtime::{RuntimeHandle, TaskStatus};

/// Start (or resume) a long task the same way `/work` does: `set_focus`
/// creates or resumes the task while idle, an empty tool-requirement set
/// gains a PreferSurface demand for `task.manage`, and the goal is delivered
/// once through the normal user-message path.
///
/// Returns an optional warning when `task.manage` could not be attached;
/// the user message is still delivered in that case.
pub async fn start_long_task(
    handle: &RuntimeHandle,
    goal: String,
) -> anyhow::Result<Option<String>> {
    handle.set_focus(goal.clone()).await?;
    let tasks = handle.list_tasks().await?;
    let mut warning = None;
    let task = tasks
        .iter()
        .find(|task| task.goal == goal && matches!(task.status, TaskStatus::Active))
        .or_else(|| {
            tasks
                .iter()
                .find(|task| matches!(task.status, TaskStatus::Active))
        });
    if let Some(task) = task
        && task.tool_requirement_count == 0
    {
        // Fill only an empty requirement set: a blind whole-set replace
        // would drop someone else's entries. A failed attach still delivers
        // the goal; task.manage stays capability.manage-loadable either way.
        if let Err(error) = handle
            .replace_task_tool_requirements(
                task.id,
                task.tool_requirement_revision,
                vec![ToolSurfaceRequirement {
                    tool_name: "task.manage".into(),
                    demand: ToolSurfaceDemand::PreferSurface,
                    reason: "long-task checklist".into(),
                }],
            )
            .await
        {
            warning = Some(format!("work: task.manage not attached: {error}"));
        }
    }
    handle.user_message(goal).await?;
    Ok(warning)
}
