//! The task manager: long-lived execution entities, separate from focus.
//!
//! A *task* is the unit of work the agent keeps returning to (its scopes
//! suspend and resume), while *focus* is the attention inside the current
//! task. `/focus A` then `/focus B` then `/focus A` resumes task A instead
//! of minting a third task, because the task identity is stable and the
//! context engine keys scope suspension on it.

use agent_contracts::TaskId;

/// Lifecycle of a task. `Suspended` tasks keep their scopes in the engine
/// and resume on activation; `Completed` tasks are closed for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Create a task and activate it. When a non-completed task with the
    /// same goal already exists, that task is resumed instead — the
    /// `/focus A -> /focus B -> /focus A` sequence must come back to task
    /// A, not spawn task C.
    pub fn create_task(&mut self, goal: String) -> TaskId {
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|task| task.goal == goal && task.status != TaskStatus::Completed)
        {
            task.status = TaskStatus::Active;
            task.last_active_ms = now_ms();
            self.active = Some(task.id);
            return task.id;
        }
        // A new task suspends the previously active one, exactly like a
        // task switch.
        if let Some(previous) = self
            .active
            .and_then(|id| self.tasks.iter_mut().find(|task| task.id == id))
            && previous.status != TaskStatus::Completed
        {
            previous.status = TaskStatus::Suspended;
        }
        let task = TaskRecord {
            id: TaskId::new(),
            goal,
            status: TaskStatus::Active,
            created_at_ms: now_ms(),
            last_active_ms: now_ms(),
        };
        let id = task.id;
        self.active = Some(id);
        self.tasks.push(task);
        id
    }

    /// Activate an existing task (suspending the currently active one).
    /// Unknown ids are rejected so the caller can surface the error.
    pub fn activate_task(&mut self, id: TaskId) -> Option<()> {
        // Reject unknown and completed ids first (the borrow ends here).
        if self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .is_none_or(|task| task.status == TaskStatus::Completed)
        {
            return None;
        }
        // Switching to another task suspends the one that was active.
        if let Some(previous) = self
            .active
            .and_then(|active| self.tasks.iter_mut().find(|task| task.id == active))
            && previous.id != id
            && previous.status != TaskStatus::Completed
        {
            previous.status = TaskStatus::Suspended;
        }
        if let Some(target) = self.tasks.iter_mut().find(|task| task.id == id) {
            target.status = TaskStatus::Active;
            target.last_active_ms = now_ms();
        }
        self.active = Some(id);
        Some(())
    }

    /// Suspend the active task, if any. Returns the suspended task id.
    pub fn suspend_active(&mut self) -> Option<TaskId> {
        let id = self.active?;
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id)
            && task.status != TaskStatus::Completed
        {
            task.status = TaskStatus::Suspended;
        }
        self.active = None;
        Some(id)
    }

    /// Mark a task completed (and clear it from the active slot when it was
    /// current). Returns the completed id.
    pub fn complete_task(&mut self, id: TaskId) -> Option<TaskId> {
        let task = self.tasks.iter_mut().find(|task| task.id == id)?;
        task.status = TaskStatus::Completed;
        if self.active == Some(id) {
            self.active = None;
        }
        Some(id)
    }

    /// The active task's goal, if any (used when re-focusing on activation).
    pub fn active_goal(&self) -> Option<&str> {
        self.active
            .and_then(|id| self.tasks.iter().find(|task| task.id == id))
            .map(|task| task.goal.as_str())
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

    #[test]
    fn refocusing_the_same_goal_resumes_the_same_task() {
        let mut tasks = TaskManager::new();
        let a = tasks.create_task("fix AuthService".into());
        let b = tasks.create_task("write docs".into());
        let again = tasks.create_task("fix AuthService".into());
        assert_eq!(a, again, "same goal resumes the existing task");
        assert_ne!(a, b);
        assert_eq!(tasks.active(), Some(a));
    }

    #[test]
    fn activate_suspends_and_complete_closes() {
        let mut tasks = TaskManager::new();
        let a = tasks.create_task("task A".into());
        let b = tasks.create_task("task B".into());
        assert_eq!(tasks.active(), Some(b));
        assert_eq!(tasks.get(a).map(|t| t.status), Some(TaskStatus::Suspended));

        tasks.activate_task(a).unwrap();
        assert_eq!(tasks.active(), Some(a));
        assert_eq!(tasks.get(b).map(|t| t.status), Some(TaskStatus::Suspended));

        tasks.complete_task(a).unwrap();
        assert_eq!(tasks.get(a).map(|t| t.status), Some(TaskStatus::Completed));
        assert_eq!(tasks.active(), None, "completing the active task clears it");

        // A completed task cannot be re-activated.
        assert!(tasks.activate_task(a).is_none());
    }

    #[test]
    fn suspend_active_clears_the_active_slot() {
        let mut tasks = TaskManager::new();
        let a = tasks.create_task("task A".into());
        assert_eq!(tasks.suspend_active(), Some(a));
        assert_eq!(tasks.active(), None);
        assert_eq!(tasks.get(a).map(|t| t.status), Some(TaskStatus::Suspended));
        assert_eq!(tasks.suspend_active(), None);
    }

    #[test]
    fn unknown_task_ids_are_rejected() {
        let mut tasks = TaskManager::new();
        assert!(tasks.activate_task(TaskId::new()).is_none());
        assert!(tasks.complete_task(TaskId::new()).is_none());
    }
}
