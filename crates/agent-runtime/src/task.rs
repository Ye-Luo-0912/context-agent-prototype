//! The task manager: long-lived execution entities, separate from focus.
//!
//! A *task* is the unit of work the agent keeps returning to (its scopes
//! suspend and resume), while *focus* is the attention inside the current
//! task. `/focus A` then `/focus B` then `/focus A` resumes task A instead
//! of minting a third task, because the task identity is stable and the
//! context engine keys scope suspension on it.
//!
//! Task transitions are two-phase (`prepare_*` then `commit`): the caller
//! applies the external transition first (the context engine's focus/scope
//! change) and only commits the `TaskManager` mutation once that succeeded,
//! so the runtime's task table can never diverge from the engine's task
//! scopes. A prepared-but-uncommitted transition is simply discarded.

use agent_contracts::{
    AgentError, AgentResult, MAX_TASK_TOOL_REQUIREMENTS, MAX_TOOL_REQUIREMENT_NAME_CHARS,
    MAX_TOOL_REQUIREMENT_REASON_CHARS, TaskId, ToolSurfaceRequirement,
};

/// Lifecycle of a task. `Suspended` tasks keep their scopes in the engine
/// and resume on activation; `Completed` tasks are closed for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Active,
    Suspended,
    Completed,
}

/// One long-lived task the runtime knows about.
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: TaskId,
    pub goal: String,
    pub status: TaskStatus,
    pub created_at_ms: u64,
    pub last_active_ms: u64,
    /// Exact tool demand owned by this task. This is declarative demand only:
    /// it neither enables a capability nor grants effect authority.
    pub tool_requirements: TaskToolRequirementSet,
}

/// The bounded, revisioned tool-requirement slice of a TaskAnchor.
///
/// `entries` is always canonical: strictly sorted by exact tool name, with no
/// duplicate names. The whole set is replaced through a compare-and-swap
/// transaction so concurrent/stale writers cannot silently merge intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskToolRequirementSet {
    pub revision: u64,
    pub entries: Vec<ToolSurfaceRequirement>,
}

/// A serializable snapshot for the UI (`/tasks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    pub id: TaskId,
    pub goal: String,
    pub status: TaskStatus,
    /// CAS base callers must use for the next whole-set requirement update.
    pub tool_requirement_revision: u64,
    /// Bounded summary only; requirement content is audited by the change
    /// event and persisted in RuntimeCheckpoint.
    pub tool_requirement_count: usize,
}

/// A pending task-state transition produced by `TaskManager::prepare_*`.
/// Nothing is mutated until `commit` runs, and `commit` must only run after
/// the external transition (the engine's focus/scope change) succeeded.
#[must_use]
pub struct TaskTxn {
    plan: TaskPlan,
}

enum TaskPlan {
    /// A brand-new task becomes active (the previous active one suspends).
    Create {
        target: TaskId,
        goal: String,
        prev_active: Option<TaskId>,
    },
    /// An existing task becomes active (the previous active one suspends).
    Activate {
        target: TaskId,
        prev_active: Option<TaskId>,
    },
    /// The active task suspends without completing.
    Suspend { active: TaskId },
    /// The active task completes (and leaves the active slot).
    Complete { active: TaskId },
    /// Atomically replace one task's complete, normalized tool-demand set.
    ReplaceToolRequirements {
        target: TaskId,
        replacement: TaskToolRequirementSet,
    },
}

#[derive(Default)]
pub struct TaskManager {
    tasks: Vec<TaskRecord>,
    active: Option<TaskId>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently active task, if any.
    pub fn active(&self) -> Option<TaskId> {
        self.active
    }

    /// Look a task up by id.
    pub fn get(&self, id: TaskId) -> Option<&TaskRecord> {
        self.tasks.iter().find(|task| task.id == id)
    }

    /// Plan to make `goal` the active task. A non-completed task with the
    /// same goal is resumed instead — the `/focus A -> /focus B ->
    /// /focus A` sequence must come back to task A, not spawn task C. A
    /// fresh task id is minted here only when no match exists, and it is
    /// discarded if the transition is never committed.
    pub fn prepare_create(&self, goal: &str) -> (TaskTxn, TaskId) {
        let existing = self
            .tasks
            .iter()
            .find(|task| task.goal == goal && task.status != TaskStatus::Completed)
            .map(|task| task.id);
        match existing {
            Some(id) => (
                TaskTxn {
                    plan: TaskPlan::Activate {
                        target: id,
                        prev_active: self.active.filter(|active| *active != id),
                    },
                },
                id,
            ),
            None => {
                let id = TaskId::new();
                (
                    TaskTxn {
                        plan: TaskPlan::Create {
                            target: id,
                            goal: goal.to_string(),
                            prev_active: self.active,
                        },
                    },
                    id,
                )
            }
        }
    }

    /// Plan to activate an existing task (suspending the currently active
    /// one). `None` for unknown or completed ids so the caller can surface
    /// the error before anything changes.
    pub fn prepare_activate(&self, id: TaskId) -> Option<TaskTxn> {
        let known = self
            .tasks
            .iter()
            .any(|task| task.id == id && task.status != TaskStatus::Completed);
        known.then(|| TaskTxn {
            plan: TaskPlan::Activate {
                target: id,
                prev_active: self.active.filter(|active| *active != id),
            },
        })
    }

    /// Plan to suspend the active task. `None` when nothing is active.
    pub fn prepare_suspend(&self) -> Option<TaskTxn> {
        self.active.map(|active| TaskTxn {
            plan: TaskPlan::Suspend { active },
        })
    }

    /// Plan to complete the active task. `None` when nothing is active.
    pub fn prepare_complete(&self) -> Option<TaskTxn> {
        self.active.map(|active| TaskTxn {
            plan: TaskPlan::Complete { active },
        })
    }

    /// Plan a bounded whole-set CAS replacement of a task's tool demand.
    ///
    /// The supplied `base_revision` must match the task's current revision.
    /// Entries are validated and sorted by exact tool name before comparison.
    /// Replacing a set with an equivalent set is idempotent and does not bump
    /// the revision. Completed tasks are immutable.
    pub fn prepare_replace_tool_requirements(
        &self,
        task_id: TaskId,
        base_revision: u64,
        entries: Vec<ToolSurfaceRequirement>,
    ) -> AgentResult<(TaskTxn, u64)> {
        let task = self.get(task_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("task {task_id} is not registered"))
        })?;
        if task.status == TaskStatus::Completed {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} is completed and its tool requirements are immutable"
            )));
        }
        if task.tool_requirements.revision != base_revision {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} tool-requirement revision mismatch: expected {}, got {base_revision}",
                task.tool_requirements.revision
            )));
        }

        let entries = normalize_tool_requirements(entries)?;
        let revision = if entries == task.tool_requirements.entries {
            base_revision
        } else {
            base_revision.checked_add(1).ok_or_else(|| {
                AgentError::InvalidRequest(format!(
                    "task {task_id} tool-requirement revision is exhausted"
                ))
            })?
        };
        Ok((
            TaskTxn {
                plan: TaskPlan::ReplaceToolRequirements {
                    target: task_id,
                    replacement: TaskToolRequirementSet { revision, entries },
                },
            },
            revision,
        ))
    }

    /// Apply a prepared transition. Call only after the external transition
    /// (the engine's `set_focus` / `clear_focus` / task completion) has
    /// succeeded, so the task table and the engine's scopes stay in sync.
    pub fn commit(&mut self, txn: TaskTxn) {
        match txn.plan {
            TaskPlan::Create {
                target,
                goal,
                prev_active,
            } => {
                self.suspend_previous(prev_active);
                let now = now_ms();
                self.tasks.push(TaskRecord {
                    id: target,
                    goal,
                    status: TaskStatus::Active,
                    created_at_ms: now,
                    last_active_ms: now,
                    tool_requirements: TaskToolRequirementSet::default(),
                });
                self.active = Some(target);
            }
            TaskPlan::Activate {
                target,
                prev_active,
            } => {
                self.suspend_previous(prev_active);
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == target) {
                    task.status = TaskStatus::Active;
                    task.last_active_ms = now_ms();
                }
                self.active = Some(target);
            }
            TaskPlan::Suspend { active } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == active) {
                    task.status = TaskStatus::Suspended;
                }
                self.active = None;
            }
            TaskPlan::Complete { active } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == active) {
                    task.status = TaskStatus::Completed;
                }
                self.active = None;
            }
            TaskPlan::ReplaceToolRequirements {
                target,
                replacement,
            } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == target) {
                    task.tool_requirements = replacement;
                }
            }
        }
    }

    fn suspend_previous(&mut self, previous: Option<TaskId>) {
        if let Some(id) = previous
            && let Some(task) = self.tasks.iter_mut().find(|task| task.id == id)
            && task.status != TaskStatus::Completed
        {
            task.status = TaskStatus::Suspended;
        }
    }

    /// The active task's goal, if any (used when re-focusing on activation).
    pub fn active_goal(&self) -> Option<&str> {
        self.active
            .and_then(|id| self.tasks.iter().find(|task| task.id == id))
            .map(|task| task.goal.as_str())
    }

    /// Every task record, in creation order (used by checkpoints).
    pub fn list_records(&self) -> &[TaskRecord] {
        &self.tasks
    }

    /// Replace the whole task table from a checkpoint snapshot. Used by
    /// restore: the engine's task scopes were restored from its own
    /// checkpoint, and this brings the runtime's view back in sync.
    pub fn restore(&mut self, snapshot: crate::checkpoint::TaskManagerSnapshot) {
        self.tasks = snapshot.tasks.into_iter().map(TaskRecord::from).collect();
        self.active = snapshot.active;
    }

    /// Snapshot for the UI.
    pub fn list(&self) -> Vec<TaskInfo> {
        self.tasks
            .iter()
            .map(|task| TaskInfo {
                id: task.id,
                goal: task.goal.clone(),
                status: task.status,
                tool_requirement_revision: task.tool_requirements.revision,
                tool_requirement_count: task.tool_requirements.entries.len(),
            })
            .collect()
    }
}

/// Validate and canonicalize a whole task-owned requirement set.
pub(crate) fn normalize_tool_requirements(
    mut entries: Vec<ToolSurfaceRequirement>,
) -> AgentResult<Vec<ToolSurfaceRequirement>> {
    if entries.len() > MAX_TASK_TOOL_REQUIREMENTS {
        return Err(AgentError::InvalidRequest(format!(
            "task declares {} tool requirements, above the {MAX_TASK_TOOL_REQUIREMENTS} cap",
            entries.len()
        )));
    }

    for requirement in &entries {
        let name_chars = requirement.tool_name.chars().count();
        if name_chars == 0 || name_chars > MAX_TOOL_REQUIREMENT_NAME_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "tool-requirement name has {name_chars} chars (allowed 1..={MAX_TOOL_REQUIREMENT_NAME_CHARS})"
            )));
        }
        if !requirement.tool_name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | ':')
        }) {
            return Err(AgentError::InvalidRequest(format!(
                "tool-requirement name '{}': only [A-Za-z0-9._:-] are allowed",
                requirement.tool_name
            )));
        }
        let reason_chars = requirement.reason.chars().count();
        if reason_chars > MAX_TOOL_REQUIREMENT_REASON_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "tool requirement '{}' has a {reason_chars}-char reason, above the {MAX_TOOL_REQUIREMENT_REASON_CHARS} cap",
                requirement.tool_name
            )));
        }
    }

    entries.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
    if let Some(duplicate) = entries
        .windows(2)
        .find(|pair| pair[0].tool_name == pair[1].tool_name)
        .map(|pair| pair[0].tool_name.as_str())
    {
        return Err(AgentError::InvalidRequest(format!(
            "task declares tool requirement '{duplicate}' more than once"
        )));
    }
    Ok(entries)
}

/// Check that a checkpoint-owned set is both valid and already canonical.
pub(crate) fn validate_tool_requirement_set(
    requirements: &TaskToolRequirementSet,
) -> AgentResult<()> {
    let normalized = normalize_tool_requirements(requirements.entries.clone())?;
    if normalized != requirements.entries {
        return Err(AgentError::InvalidRequest(
            "task tool requirements are not normalized by tool name".into(),
        ));
    }
    if requirements.revision == 0 && !requirements.entries.is_empty() {
        return Err(AgentError::InvalidRequest(
            "task tool requirements at revision 0 must be empty".into(),
        ));
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ToolSurfaceDemand;

    fn create(tasks: &mut TaskManager, goal: &str) -> TaskId {
        let (txn, id) = tasks.prepare_create(goal);
        tasks.commit(txn);
        id
    }

    fn requirement(name: impl Into<String>, demand: ToolSurfaceDemand) -> ToolSurfaceRequirement {
        ToolSurfaceRequirement {
            tool_name: name.into(),
            demand,
            reason: String::new(),
        }
    }

    #[test]
    fn refocusing_the_same_goal_resumes_the_same_task() {
        let mut tasks = TaskManager::new();
        let a = create(&mut tasks, "fix AuthService");
        let b = create(&mut tasks, "write docs");
        let (txn, again) = tasks.prepare_create("fix AuthService");
        assert_eq!(a, again, "same goal resumes the existing task");
        tasks.commit(txn);
        assert_ne!(a, b);
        assert_eq!(tasks.active(), Some(a));
    }

    #[test]
    fn activate_suspends_and_complete_closes() {
        let mut tasks = TaskManager::new();
        let a = create(&mut tasks, "task A");
        let b = create(&mut tasks, "task B");
        assert_eq!(tasks.active(), Some(b));
        assert_eq!(tasks.get(a).map(|t| t.status), Some(TaskStatus::Suspended));

        let txn = tasks.prepare_activate(a).expect("a exists and is open");
        tasks.commit(txn);
        assert_eq!(tasks.active(), Some(a));
        assert_eq!(tasks.get(b).map(|t| t.status), Some(TaskStatus::Suspended));

        let txn = tasks.prepare_complete().expect("a is active");
        tasks.commit(txn);
        assert_eq!(tasks.get(a).map(|t| t.status), Some(TaskStatus::Completed));
        assert_eq!(tasks.active(), None, "completing the active task clears it");

        // A completed task cannot be re-activated.
        assert!(tasks.prepare_activate(a).is_none());
    }

    #[test]
    fn suspend_active_clears_the_active_slot() {
        let mut tasks = TaskManager::new();
        let a = create(&mut tasks, "task A");
        assert_eq!(tasks.active(), Some(a));
        let txn = tasks.prepare_suspend().expect("a is active");
        tasks.commit(txn);
        assert_eq!(tasks.active(), None);
        assert_eq!(tasks.get(a).map(|t| t.status), Some(TaskStatus::Suspended));
        assert!(tasks.prepare_suspend().is_none());
    }

    #[test]
    fn unknown_task_ids_are_rejected() {
        let tasks = TaskManager::new();
        assert!(tasks.prepare_activate(TaskId::new()).is_none());
        assert!(tasks.prepare_complete().is_none());
    }

    #[test]
    fn an_uncommitted_transition_changes_nothing() {
        let mut tasks = TaskManager::new();
        let a = create(&mut tasks, "task A");
        // Prepare a switch to a new task but never commit it: the table
        // must stay exactly as it was (the external transition failed).
        let (_txn, _b) = tasks.prepare_create("task B");
        assert_eq!(tasks.active(), Some(a));
        assert_eq!(tasks.list().len(), 1);
    }

    #[test]
    fn tool_requirements_are_whole_set_cas_normalized_and_idempotent() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");
        let desired = vec![
            requirement("search.grep", ToolSurfaceDemand::PreferSurface),
            requirement("fs.read", ToolSurfaceDemand::MustSurface),
        ];

        let (txn, revision) = tasks
            .prepare_replace_tool_requirements(task_id, 0, desired)
            .expect("initial CAS is valid");
        assert_eq!(revision, 1);
        assert_eq!(
            tasks.get(task_id).unwrap().tool_requirements.revision,
            0,
            "prepare is not visible before commit"
        );
        tasks.commit(txn);
        let stored = &tasks.get(task_id).unwrap().tool_requirements;
        assert_eq!(stored.revision, 1);
        assert_eq!(stored.entries[0].tool_name, "fs.read");
        assert_eq!(stored.entries[1].tool_name, "search.grep");
        let info = &tasks.list()[0];
        assert_eq!(info.tool_requirement_revision, 1);
        assert_eq!(info.tool_requirement_count, 2);

        let equivalent_in_a_different_order = vec![
            requirement("search.grep", ToolSurfaceDemand::PreferSurface),
            requirement("fs.read", ToolSurfaceDemand::MustSurface),
        ];
        let (txn, revision) = tasks
            .prepare_replace_tool_requirements(task_id, 1, equivalent_in_a_different_order)
            .expect("equivalent replacement is valid");
        assert_eq!(revision, 1, "an equivalent set must not bump revision");
        tasks.commit(txn);
        assert_eq!(tasks.get(task_id).unwrap().tool_requirements.revision, 1);

        let stale = tasks.prepare_replace_tool_requirements(task_id, 0, Vec::new());
        assert!(matches!(stale, Err(AgentError::InvalidRequest(_))));
    }

    #[test]
    fn tool_requirement_validation_is_bounded_and_exact_name_unique() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");

        let too_many = (0..=MAX_TASK_TOOL_REQUIREMENTS)
            .map(|index| requirement(format!("tool.{index}"), ToolSurfaceDemand::PreferSurface))
            .collect();
        assert!(matches!(
            tasks.prepare_replace_tool_requirements(task_id, 0, too_many),
            Err(AgentError::InvalidRequest(_))
        ));

        let duplicate = vec![
            requirement("fs.read", ToolSurfaceDemand::MustSurface),
            requirement("fs.read", ToolSurfaceDemand::KeepReady),
        ];
        assert!(matches!(
            tasks.prepare_replace_tool_requirements(task_id, 0, duplicate),
            Err(AgentError::InvalidRequest(_))
        ));

        let invalid_names = [
            String::new(),
            "x".repeat(MAX_TOOL_REQUIREMENT_NAME_CHARS + 1),
            "bad\nname".into(),
            "工具.read".into(),
        ];
        for name in invalid_names {
            assert!(matches!(
                tasks.prepare_replace_tool_requirements(
                    task_id,
                    0,
                    vec![requirement(name, ToolSurfaceDemand::MustSurface)]
                ),
                Err(AgentError::InvalidRequest(_))
            ));
        }

        let mut long_reason = requirement("fs.read", ToolSurfaceDemand::MustSurface);
        long_reason.reason = "理".repeat(MAX_TOOL_REQUIREMENT_REASON_CHARS + 1);
        assert!(matches!(
            tasks.prepare_replace_tool_requirements(task_id, 0, vec![long_reason]),
            Err(AgentError::InvalidRequest(_))
        ));
    }

    #[test]
    fn completed_task_rejects_tool_requirement_replacement() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");
        let txn = tasks.prepare_complete().expect("task is active");
        tasks.commit(txn);

        assert!(matches!(
            tasks.prepare_replace_tool_requirements(task_id, 0, Vec::new()),
            Err(AgentError::InvalidRequest(_))
        ));
    }

    #[test]
    fn suspend_and_resume_preserve_task_owned_tool_requirements() {
        let mut tasks = TaskManager::new();
        let task_a = create(&mut tasks, "task A");
        let (replace, _) = tasks
            .prepare_replace_tool_requirements(
                task_a,
                0,
                vec![requirement("fs.read", ToolSurfaceDemand::KeepReady)],
            )
            .unwrap();
        tasks.commit(replace);

        let task_b = create(&mut tasks, "task B");
        assert_eq!(tasks.get(task_a).unwrap().status, TaskStatus::Suspended);
        let activate = tasks.prepare_activate(task_a).unwrap();
        tasks.commit(activate);

        let restored = &tasks.get(task_a).unwrap().tool_requirements;
        assert_eq!(restored.revision, 1);
        assert_eq!(restored.entries[0].tool_name, "fs.read");
        assert_eq!(tasks.get(task_b).unwrap().status, TaskStatus::Suspended);
    }
}
