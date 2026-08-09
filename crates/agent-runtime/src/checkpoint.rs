//! `RuntimeCheckpoint`: the complete runtime snapshot.
//!
//! `ContextEngine::checkpoint` alone is not a full checkpoint since the
//! actor gained its own state: the engine captures items, the scope tree,
//! focus and GC state, but not the runtime's `TaskManager` or the dynamic
//! capability surface. Restoring only the engine state resurrects task
//! scopes the runtime knows nothing about. This type bundles both planes:
//!
//! ```text
//! RuntimeCheckpoint
//!   ├─ version
//!   ├─ run metadata (run id, creation time)
//!   ├─ TaskManager snapshot (task table + active task)
//!   ├─ current TaskId (the actor's belief, kept in sync with the engine)
//!   ├─ context checkpoint (the engine's own JSON state)
//!   └─ capability surface state (activation + loaded per capability)
//! ```
//!
//! Tool lifecycle and context-store generation are deliberately optional
//! today: tool lifecycle is derived from the catalog + loaded flags, and
//! store files are durable on disk (only the in-memory `external` list is
//! part of the engine checkpoint).

use std::collections::HashSet;

use agent_contracts::{AgentError, AgentResult, RunId, TaskId};
use serde::{Deserialize, Serialize};

use crate::task::{TaskManager, TaskRecord, TaskStatus};

/// Bump when the checkpoint shape changes; restore rejects mismatches.
pub const RUNTIME_CHECKPOINT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    pub version: u32,
    pub run_metadata: RunMetadata,
    pub tasks: TaskManagerSnapshot,
    /// The task the actor believes is current. Restored so the runtime and
    /// the engine agree on which scopes are active.
    pub current_task_id: Option<TaskId>,
    /// The context engine's own checkpoint (items, scope tree, focus, GC
    /// state, external store entries).
    pub context: serde_json::Value,
    /// Dynamic capability surface state, applied by the host on restore.
    #[serde(default)]
    pub capabilities: Vec<CapabilitySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetadata {
    pub run_id: RunId,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskManagerSnapshot {
    pub tasks: Vec<TaskRecordSnapshot>,
    pub active: Option<TaskId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecordSnapshot {
    pub id: TaskId,
    pub goal: String,
    pub status: TaskStatus,
    pub created_at_ms: u64,
    pub last_active_ms: u64,
}

/// One dynamic capability's surface state: its activation and whether its
/// tools are on the model surface. Restoring re-applies the flags so a
/// restarted run offers exactly the tools the checkpoint left loaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySnapshot {
    pub id: String,
    pub activation: agent_contracts::CapabilityActivation,
    pub loaded: bool,
}

impl RuntimeCheckpoint {
    /// Validate the redundant task authority fields before any restore-side
    /// mutation. A checkpoint is untrusted input: `tasks.active`, the
    /// actor's `current_task_id`, and the record carrying `Active` must name
    /// exactly the same task.
    pub(crate) fn validate(&self) -> AgentResult<()> {
        if self.version != RUNTIME_CHECKPOINT_VERSION {
            return Err(AgentError::InvalidRequest(format!(
                "checkpoint version {} is not supported (expected {})",
                self.version, RUNTIME_CHECKPOINT_VERSION
            )));
        }

        if self.tasks.active != self.current_task_id {
            return Err(AgentError::InvalidRequest(
                "checkpoint task authority is inconsistent: tasks.active and current_task_id differ"
                    .into(),
            ));
        }

        let mut task_ids = HashSet::new();
        let mut active_records = Vec::new();
        for task in &self.tasks.tasks {
            if !task_ids.insert(task.id) {
                return Err(AgentError::InvalidRequest(format!(
                    "checkpoint contains duplicate task id {}",
                    task.id
                )));
            }
            if task.status == TaskStatus::Active {
                active_records.push(task.id);
            }
        }

        match self.current_task_id {
            Some(current) if active_records.as_slice() != [current] => {
                return Err(AgentError::InvalidRequest(format!(
                    "checkpoint current task {current} must be the only active task record"
                )));
            }
            None if !active_records.is_empty() => {
                return Err(AgentError::InvalidRequest(
                    "checkpoint has an active task record but no current task".into(),
                ));
            }
            _ => {}
        }

        let mut capability_ids = HashSet::new();
        for capability in &self.capabilities {
            if !capability_ids.insert(capability.id.as_str()) {
                return Err(AgentError::InvalidRequest(format!(
                    "checkpoint contains duplicate capability id '{}'",
                    capability.id
                )));
            }
        }
        Ok(())
    }
}

impl TaskManagerSnapshot {
    pub(crate) fn from_manager(tasks: &TaskManager) -> Self {
        Self {
            tasks: tasks
                .list_records()
                .iter()
                .map(|task| TaskRecordSnapshot {
                    id: task.id,
                    goal: task.goal.clone(),
                    status: task.status,
                    created_at_ms: task.created_at_ms,
                    last_active_ms: task.last_active_ms,
                })
                .collect(),
            active: tasks.active(),
        }
    }
}

impl From<TaskRecordSnapshot> for TaskRecord {
    fn from(snapshot: TaskRecordSnapshot) -> Self {
        Self {
            id: snapshot.id,
            goal: snapshot.goal,
            status: snapshot.status,
            created_at_ms: snapshot.created_at_ms,
            last_active_ms: snapshot.last_active_ms,
        }
    }
}
