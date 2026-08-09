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

use agent_contracts::TaskId;

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
}

/// A serializable snapshot for the UI (`/tasks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    pub id: TaskId,
    pub goal: String,
    pub status: TaskStatus,
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
            })
            .collect()
    }
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

    fn create(tasks: &mut TaskManager, goal: &str) -> TaskId {
        let (txn, id) = tasks.prepare_create(goal);
        tasks.commit(txn);
        id
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
}
