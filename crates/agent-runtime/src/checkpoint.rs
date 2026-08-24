//! `RuntimeCheckpoint`: the actor/context/capability snapshot plus a durable
//! Core-authority checkpoint marker.
//!
//! `ContextEngine::checkpoint` alone is not a full checkpoint since the
//! actor gained its own state: the engine captures items, the scope tree,
//! focus and GC state, but not the runtime's `TaskManager`, dynamic
//! capability surface, or Core operation authority. Restoring only the
//! engine state resurrects task scopes the runtime knows nothing about.
//! This type bundles the actor-owned planes and references — but never
//! embeds or rewinds — the durable Core authority journal:
//!
//! ```text
//! RuntimeCheckpoint
//!   ├─ version
//!   ├─ run metadata (run id, creation time)
//!   ├─ TaskManager snapshot (task table + active task)
//!   ├─ current TaskId (the actor's belief, kept in sync with the engine)
//!   ├─ focus and last-issued surface revisions
//!   ├─ context checkpoint (the engine's own JSON state)
//!   ├─ capability surface state (activation + loaded tools per capability)
//!   └─ authority marker (journal lineage + verified durable prefix)
//! ```
//!
//! Tool lifecycle and context-store generation are deliberately optional
//! today: tool lifecycle is derived from the catalog + loaded flags, and
//! store files are durable on disk (only the in-memory `external` list is
//! part of the engine checkpoint).

use std::collections::HashSet;

use agent_contracts::{
    AgentError, AgentResult, AuthorityCheckpointMarker, MAX_COMPLETION_ARTIFACTS,
    MAX_COMPLETION_REF_CHARS, MAX_COMPLETION_SUMMARY_CHARS, RunId, TaskId,
};
use serde::{Deserialize, Serialize};

use crate::task::{
    CompletionRecord, TaskAnchor, TaskManager, TaskRecord, TaskStatus, TaskToolRequirementSet,
    validate_anchor, validate_tool_requirement_set,
};

/// Bounded reasons why a fully settled batch owes a durable resume
/// checkpoint. Reasons coalesce into one candidate snapshot; read-only
/// exploration accrues nothing and never forces a synchronous write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointDebtReason {
    TaskAnchorChanged,
    DurableWorkspaceMutation,
    VerificationChanged,
}

impl CheckpointDebtReason {
    pub fn name(self) -> &'static str {
        match self {
            Self::TaskAnchorChanged => "task_anchor_changed",
            Self::DurableWorkspaceMutation => "durable_workspace_mutation",
            Self::VerificationChanged => "verification_changed",
        }
    }
}

/// Acknowledgement record of one durably written safe-point artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCheckpoint {
    /// File name inside the store directory.
    pub artifact: String,
    /// Total stored byte length (envelope incl. payload).
    pub bytes: u64,
    /// Sha256 hex over the raw checkpoint payload bytes.
    pub checksum: String,
}

const CHECKPOINT_ENVELOPE_FORMAT: &str = "runtime-checkpoint-envelope-v1";

/// Actor-owned atomic checkpoint artifact store under the workspace state
/// directory. A write lands as a unique temp file renamed onto its final
/// name inside the same directory, so a reader never observes a partial
/// checkpoint. The stored form is one header line (format + payload
/// checksum + length) followed by the raw checkpoint payload bytes; load
/// verifies both before returning anything, so corruption or truncation is
/// refused instead of silently restored. File data crosses an OS sync
/// barrier before the rename; parent-directory sync is attempted on a
/// best-effort basis because Windows does not expose it portably — that
/// platform limitation stays explicit rather than claimed away.
pub struct CheckpointStore {
    dir: std::path::PathBuf,
}

impl CheckpointStore {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Write one checkpoint payload atomically and return its artifact
    /// acknowledgement. Unique names keep every successful write
    /// addressable; retention/cleanup stays a host concern.
    pub async fn write_atomic(&self, payload: &[u8]) -> AgentResult<StoredCheckpoint> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| {
                AgentError::InvalidRequest(format!("checkpoint dir unavailable: {error}"))
            })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default();
        let artifact = format!("checkpoint-{now}-{}.json", RunId::new());
        let checksum = sha256_hex(payload);
        let mut stored: Vec<u8> = Vec::with_capacity(payload.len() + 160);
        let header = serde_json::json!({
            "format": CHECKPOINT_ENVELOPE_FORMAT,
            "checksum": checksum,
            "payload_bytes": payload.len(),
        });
        stored.extend_from_slice(header.to_string().as_bytes());
        stored.push(b'\n');
        stored.extend_from_slice(payload);
        let temp = self.dir.join(format!(".{artifact}.tmp"));
        let final_path = self.dir.join(&artifact);
        write_and_sync(&temp, &stored).await.map_err(|error| {
            AgentError::InvalidRequest(format!("checkpoint write failed: {error}"))
        })?;
        tokio::fs::rename(&temp, &final_path)
            .await
            .map_err(|error| {
                AgentError::InvalidRequest(format!("checkpoint rename failed: {error}"))
            })?;
        sync_directory_best_effort(&self.dir).await;
        Ok(StoredCheckpoint {
            artifact,
            bytes: stored.len() as u64,
            checksum,
        })
    }

    /// Load and verify one acknowledged artifact by name. Refuses unknown
    /// files, truncated payloads, checksum mismatches and wrong envelopes
    /// before any bytes reach the caller.
    pub async fn load_verified(&self, artifact: &str) -> AgentResult<Vec<u8>> {
        // The name comes from an ack/event, never from model output; still,
        // refuse anything path-like outright.
        if artifact.is_empty()
            || !artifact
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(AgentError::InvalidRequest(
                "invalid checkpoint artifact name".into(),
            ));
        }
        let path = self.dir.join(artifact);
        let stored = tokio::fs::read(&path).await.map_err(|error| {
            AgentError::InvalidRequest(format!(
                "checkpoint artifact {artifact} unreadable: {error}"
            ))
        })?;
        let split = stored
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| {
                AgentError::InvalidRequest(format!("checkpoint artifact {artifact} has no header"))
            })?;
        let header: serde_json::Value =
            serde_json::from_slice(&stored[..split]).map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "checkpoint artifact {artifact} header malformed: {error}"
                ))
            })?;
        if header["format"] != CHECKPOINT_ENVELOPE_FORMAT {
            return Err(AgentError::InvalidRequest(format!(
                "checkpoint artifact {artifact} is not a {} envelope",
                CHECKPOINT_ENVELOPE_FORMAT
            )));
        }
        let expected_checksum = header["checksum"].as_str().ok_or_else(|| {
            AgentError::InvalidRequest(format!(
                "checkpoint artifact {artifact} header has no checksum"
            ))
        })?;
        let payload_bytes = header["payload_bytes"].as_u64().ok_or_else(|| {
            AgentError::InvalidRequest(format!(
                "checkpoint artifact {artifact} header has no payload length"
            ))
        })? as usize;
        let payload = &stored[split + 1..];
        if payload.len() != payload_bytes {
            return Err(AgentError::InvalidRequest(format!(
                "checkpoint artifact {artifact} is truncated: {} of {payload_bytes} bytes",
                payload.len()
            )));
        }
        if sha256_hex(payload) != expected_checksum {
            return Err(AgentError::InvalidRequest(format!(
                "checkpoint artifact {artifact} fails its checksum"
            )));
        }
        Ok(payload.to_vec())
    }
}

async fn write_and_sync(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    Ok(())
}

/// Best-effort directory-entry durability. Platforms without portable
/// directory fsync (Windows) simply skip it; the limitation is part of the
/// contract, not hidden behind a fake success.
async fn sync_directory_best_effort(dir: &std::path::Path) {
    if let Ok(file) = tokio::fs::File::open(dir).await {
        let _ = file.sync_all().await;
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    // agent-runtime depends on agent-contracts only for hashing-free
    // types; pull the digest through the workspace-shared sha2 crate.
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Bump when the checkpoint shape changes; restore rejects mismatches.
pub const RUNTIME_CHECKPOINT_VERSION: u32 = 4;

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
    /// A read-only reference to one verified durable prefix of Core's
    /// operation-authority journal. It is absent only for explicitly
    /// ephemeral, in-process compositions with no operation journal.
    /// Restore validates this marker against the live Core before any
    /// mutation and never installs its epoch or journal state.
    #[serde(default)]
    pub authority: Option<AuthorityCheckpointMarker>,
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
    /// One immutable outcome per completed task, in completion order.
    /// Defaults only to permit explicit rejection of legacy v2 payloads.
    #[serde(default)]
    pub completed: Vec<CompletionRecord>,
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
    /// Defaults only to permit explicit rejection of legacy v2 payloads;
    /// there is intentionally no v2-to-v3 migration.
    #[serde(default)]
    pub anchor: TaskAnchor,
    #[serde(default)]
    pub resume: crate::execution::ExecutionState,
    /// Current user-turn directive. Empty on legacy checkpoints.
    #[serde(default)]
    pub turn_intent: String,
}

/// One dynamic capability's activation plus bounded mechanical tool
/// readiness/residency. The checkpoint intentionally does not carry a live
/// host-persistent source: current composition must re-establish that intent,
/// while restored-only rows remain eligible for lease reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySnapshot {
    pub id: String,
    pub activation: agent_contracts::CapabilityActivation,
    /// Legacy whole-capability loaded flag, kept so old checkpoints round
    /// trip. New checkpoints write it as "at least one tool loaded" so
    /// older readers still see a non-empty surface; a non-empty `loaded_tools`
    /// list is the authoritative form.
    #[serde(default)]
    pub loaded: bool,
    /// Per-tool readiness state (authoritative since the per-tool lifecycle).
    /// It includes Loaded and Warm rows so reload remains cheap; empty means
    /// no retained readiness for this capability.
    #[serde(default)]
    pub loaded_tools: Vec<String>,
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

        if let Some(marker) = &self.authority {
            marker.validate().map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "checkpoint has an invalid authority marker: {error}"
                ))
            })?;
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
            validate_anchor(&task.anchor).map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "checkpoint task {} has an invalid anchor: {error}",
                    task.id
                ))
            })?;
            crate::execution::validate_resume(&task.resume).map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "checkpoint task {} has an invalid resume point: {error}",
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

        // A completed task must own exactly one committed completion record,
        // and every record must name a completed task with a matching anchor
        // revision — the outcome is measured against exactly that authority.
        let mut completed_without_record = Vec::new();
        let mut records_by_task = std::collections::HashMap::new();
        for task in &self.tasks.tasks {
            if task.status == TaskStatus::Completed {
                completed_without_record.push(task.id);
            }
        }
        for record in &self.tasks.completed {
            validate_completion_record(record).map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "checkpoint has an invalid completion record: {error}"
                ))
            })?;
            if let Some(position) = completed_without_record
                .iter()
                .position(|id| *id == record.task_id)
            {
                completed_without_record.swap_remove(position);
                if records_by_task.insert(record.task_id, record).is_some() {
                    return Err(AgentError::InvalidRequest(format!(
                        "checkpoint task {} has more than one committed completion record",
                        record.task_id
                    )));
                }
            } else {
                return Err(AgentError::InvalidRequest(format!(
                    "checkpoint completion record names task {} which is not completed",
                    record.task_id
                )));
            }
        }
        if let Some(task_id) = completed_without_record.first() {
            return Err(AgentError::InvalidRequest(format!(
                "checkpoint completed task {task_id} has no committed completion record"
            )));
        }
        for task in &self.tasks.tasks {
            if let Some(record) = records_by_task.get(&task.id)
                && record.anchor_revision != task.anchor.revision
            {
                return Err(AgentError::InvalidRequest(format!(
                    "checkpoint completion record for task {} names anchor revision {}, but the task anchor is at {}",
                    task.id, record.anchor_revision, task.anchor.revision
                )));
            }
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

/// Bound and validate one completion record from a checkpoint. A record is
/// immutable authority: every field is capped and refs are short strings.
pub(crate) fn validate_completion_record(record: &CompletionRecord) -> AgentResult<()> {
    if record.summary.chars().count() > MAX_COMPLETION_SUMMARY_CHARS {
        return Err(AgentError::InvalidRequest(format!(
            "completion summary has {} chars, above the {MAX_COMPLETION_SUMMARY_CHARS} cap",
            record.summary.chars().count()
        )));
    }
    if record.artifacts.len() > MAX_COMPLETION_ARTIFACTS {
        return Err(AgentError::InvalidRequest(format!(
            "completion record carries {} artifacts, above the {MAX_COMPLETION_ARTIFACTS} cap",
            record.artifacts.len()
        )));
    }
    if record.verification_refs.len() > MAX_COMPLETION_ARTIFACTS {
        return Err(AgentError::InvalidRequest(format!(
            "completion record carries {} verification refs, above the {MAX_COMPLETION_ARTIFACTS} cap",
            record.verification_refs.len()
        )));
    }
    for reference in record
        .final_output_ref
        .iter()
        .chain(record.final_output_digest.iter())
        .chain(record.verification_refs.iter())
    {
        if reference.chars().count() > MAX_COMPLETION_REF_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "completion ref has {} chars, above the {MAX_COMPLETION_REF_CHARS} cap",
                reference.chars().count()
            )));
        }
    }
    for artifact in &record.artifacts {
        if artifact.chars().count() > MAX_COMPLETION_REF_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "completion artifact ref has {} chars, above the {MAX_COMPLETION_REF_CHARS} cap",
                artifact.chars().count()
            )));
        }
    }
    Ok(())
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
                    anchor: task.anchor.clone(),
                    resume: task.resume.clone(),
                    turn_intent: task.turn_intent.clone(),
                })
                .collect(),
            active: tasks.active(),
            completed: tasks.completed_records().to_vec(),
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
            anchor: snapshot.anchor,
            resume: snapshot.resume,
            turn_intent: snapshot.turn_intent,
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
            authority: None,
        }
    }

    #[test]
    fn v4_round_trip_preserves_task_surface_and_authority_shape() {
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
        assert!(decoded.authority.is_none());
    }

    #[test]
    fn v4_round_trip_preserves_a_progress_proposal_exactly() {
        let mut tasks = task_manager_with_requirements();
        let task_id = tasks.active().unwrap();
        let base = tasks.get(task_id).unwrap().anchor.revision;
        let (txn, _revision, changed_fields, kind) = tasks
            .prepare_patch_anchor(
                task_id,
                base,
                &crate::task::AnchorPatch {
                    current_interpretation: Some(
                        "retry policy spans config, errors and execution".into(),
                    ),
                    plan_progress: Some(vec!["read the runner".into()]),
                    open_loops: Some(vec!["prove saturation at the delay cap".into()]),
                    next_action: Some("add the fake-sleeper unit test".into()),
                    ..crate::task::AnchorPatch::default()
                },
            )
            .unwrap();
        assert!(matches!(kind, agent_contracts::AnchorPatchKind::Autonomous));
        assert!(changed_fields.contains(&"next_action".to_string()));
        tasks.commit(txn);

        let encoded = serde_json::to_vec(&checkpoint(&tasks)).unwrap();
        let decoded: RuntimeCheckpoint = serde_json::from_slice(&encoded).unwrap();
        decoded.validate().unwrap();
        // The proposal survives restore exactly, next_action included.
        assert_eq!(
            decoded.tasks.tasks[0].anchor,
            tasks.get(task_id).unwrap().anchor
        );
        assert_eq!(
            decoded.tasks.tasks[0].anchor.next_action,
            "add the fake-sleeper unit test"
        );
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
    fn legacy_v3_deserializes_only_to_receive_an_explicit_version_rejection() {
        let mut value =
            serde_json::to_value(checkpoint(&task_manager_with_requirements())).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("version".into(), serde_json::json!(3));
        object.remove("authority");

        let decoded: RuntimeCheckpoint = serde_json::from_value(value).unwrap();
        let error = decoded.validate().unwrap_err().to_string();
        assert!(error.contains("checkpoint version 3 is not supported"));
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
