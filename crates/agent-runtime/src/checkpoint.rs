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
//!   ├─ focus and last-issued surface revisions
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

use crate::task::{
    TaskManager, TaskRecord, TaskStatus, TaskToolRequirementSet, validate_tool_requirement_set,
};

/// Bump when the checkpoint shape changes; restore rejects mismatches.
pub const RUNTIME_CHECKPOINT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    pub version: u32,
    pub run_metadata: RunMetadata,
    pub tasks: TaskManagerSnapshot,
    /// The task the actor believes is current. Restored so the runtime and
    /// the engine agree on which scopes are active.
    pub current_task_id: Option<TaskId>,
    /// Revision of runtime-owned focus inputs used to fence derived round
    /// surfaces. Defaults only so a v1 payload can deserialize and receive
    /// the explicit unsupported-version error from `validate`.
    #[serde(default)]
    pub focus_revision: u64,
    /// Last issued round-surface identity. Persisting it prevents revision
    /// reuse after restore.
    #[serde(default)]
    pub last_surface_revision: u64,
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
    /// Defaults only to permit explicit rejection of legacy v1 payloads;
    /// there is intentionally no v1-to-v2 migration.
    #[serde(default)]
    pub tool_requirements: TaskToolRequirementSet,
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
                "checkpoint version {} is not supported (expected {}); automatic migration is not available",
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
            validate_tool_requirement_set(&task.tool_requirements).map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "checkpoint task {} has invalid tool requirements: {error}",
                    task.id
                ))
            })?;
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
                    tool_requirements: task.tool_requirements.clone(),
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
            tool_requirements: snapshot.tool_requirements,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ToolSurfaceDemand, ToolSurfaceRequirement};

    fn requirement(name: &str) -> ToolSurfaceRequirement {
        ToolSurfaceRequirement {
            tool_name: name.into(),
            demand: ToolSurfaceDemand::MustSurface,
            reason: "needed by the active task".into(),
        }
    }

    fn task_manager_with_requirements() -> TaskManager {
        let mut tasks = TaskManager::new();
        let (create, task_id) = tasks.prepare_create("finish the runtime");
        tasks.commit(create);
        let (replace, revision) = tasks
            .prepare_replace_tool_requirements(
                task_id,
                0,
                vec![requirement("search.grep"), requirement("fs.read")],
            )
            .unwrap();
        assert_eq!(revision, 1);
        tasks.commit(replace);
        tasks
    }

    fn checkpoint(tasks: &TaskManager) -> RuntimeCheckpoint {
        RuntimeCheckpoint {
            version: RUNTIME_CHECKPOINT_VERSION,
            run_metadata: RunMetadata {
                run_id: RunId::new(),
                created_at_ms: 1,
            },
            tasks: TaskManagerSnapshot::from_manager(tasks),
            current_task_id: tasks.active(),
            focus_revision: 7,
            last_surface_revision: 11,
            context: serde_json::json!({}),
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn v2_round_trip_preserves_task_and_surface_revisions() {
        let checkpoint = checkpoint(&task_manager_with_requirements());
        checkpoint.validate().unwrap();

        let encoded = serde_json::to_vec(&checkpoint).unwrap();
        let decoded: RuntimeCheckpoint = serde_json::from_slice(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.focus_revision, 7);
        assert_eq!(decoded.last_surface_revision, 11);
        let requirements = &decoded.tasks.tasks[0].tool_requirements;
        assert_eq!(requirements.revision, 1);
        assert_eq!(requirements.entries[0].tool_name, "fs.read");
        assert_eq!(requirements.entries[1].tool_name, "search.grep");
    }

    #[test]
    fn legacy_v1_deserializes_only_to_receive_an_explicit_version_rejection() {
        let mut value =
            serde_json::to_value(checkpoint(&task_manager_with_requirements())).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("version".into(), serde_json::json!(1));
        object.remove("focus_revision");
        object.remove("last_surface_revision");
        for task in object["tasks"]["tasks"].as_array_mut().unwrap() {
            task.as_object_mut().unwrap().remove("tool_requirements");
        }

        let decoded: RuntimeCheckpoint = serde_json::from_value(value).unwrap();
        let error = decoded.validate().unwrap_err().to_string();
        assert!(error.contains("checkpoint version 1 is not supported"));
    }

    #[test]
    fn checkpoint_rejects_noncanonical_or_impossible_requirement_sets() {
        let tasks = task_manager_with_requirements();
        let mut unsorted = checkpoint(&tasks);
        unsorted.tasks.tasks[0].tool_requirements.entries.reverse();
        assert!(matches!(
            unsorted.validate(),
            Err(AgentError::InvalidRequest(_))
        ));

        let mut duplicate = checkpoint(&tasks);
        duplicate.tasks.tasks[0]
            .tool_requirements
            .entries
            .push(requirement("fs.read"));
        assert!(matches!(
            duplicate.validate(),
            Err(AgentError::InvalidRequest(_))
        ));

        let mut zero_with_entries = checkpoint(&tasks);
        zero_with_entries.tasks.tasks[0].tool_requirements.revision = 0;
        assert!(matches!(
            zero_with_entries.validate(),
            Err(AgentError::InvalidRequest(_))
        ));
    }
}
