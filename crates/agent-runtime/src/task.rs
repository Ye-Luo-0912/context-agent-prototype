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
    AgentError, AgentResult, AnchorPatchKind, ArtifactLocator, CompletionProposal,
    MAX_COMPLETION_ARTIFACTS, MAX_COMPLETION_REF_CHARS, MAX_COMPLETION_SUMMARY_CHARS,
    MAX_TASK_ANCHOR_CHANGED_FIELDS, MAX_TASK_ANCHOR_CLAIMS, MAX_TASK_ANCHOR_ITEM_CHARS,
    MAX_TASK_ANCHOR_LIST_ITEMS, MAX_TASK_ANCHOR_TEXT_CHARS, MAX_TASK_TOOL_REQUIREMENTS,
    MAX_TOOL_REQUIREMENT_NAME_CHARS, MAX_TOOL_REQUIREMENT_REASON_CHARS, TaskId,
    ToolSurfaceRequirement,
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
    /// The task's authoritative anchor: goal interpretation, constraints,
    /// acceptance criteria, plan, open loops and typed root claims. This is
    /// task authority, not a scored ContextItem.
    pub anchor: TaskAnchor,
    /// Operational execution state bound to this task's current anchor
    /// revision. Checkpointed as `resume`. Not a second authority: the
    /// assembler projects a `TaskProgressView` and never scores it as a
    /// heap item.
    pub resume: crate::execution::ExecutionState,
    /// Current user-turn directive. Replaced on every user input; never
    /// written into `TaskAnchor` and never bumps `anchor_revision`.
    pub turn_intent: String,
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

/// The actor-owned, bounded, versioned anchor of one task.
///
/// The anchor is task authority: it lives with the `TaskManager`, never as a
/// scored `ContextItem`, so a replaceable context policy cannot collect or
/// rewrite it, and task state is never duplicated across orchestrators. The
/// full anchor is persisted in `RuntimeCheckpoint`; events carry only bounded
/// summaries. Every field is capped (see the `MAX_TASK_ANCHOR_*` bounds), and
/// the whole anchor is replaced through compare-and-swap so stale writers
/// cannot silently merge intent.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TaskAnchor {
    /// Monotonic per-task revision. An anchor at revision 0 is the initial
    /// empty anchor (goal only, stamped from the task goal at creation);
    /// every CAS replacement bumps it.
    pub revision: u64,
    /// The user-given origin the task was created with. This is task
    /// identity / historical origin, not a perpetual current instruction:
    /// temporal wording (`yet`, `for now`, `first`) does not outrank the
    /// current user turn. Immutable in practice (the task identity keys on
    /// it) and carried so restore can re-derive the focus goal without
    /// replaying the transcript.
    pub original_goal: String,
    /// The runtime's current interpretation of the goal, which may have
    /// evolved as constraints and findings landed.
    pub current_interpretation: String,
    /// Hard user-authority constraints the task must respect.
    pub constraints: Vec<String>,
    /// Acceptance criteria the completion outcome is measured against.
    pub acceptance_criteria: Vec<String>,
    /// Ordered plan progress: what has been done and what is next.
    pub plan_progress: Vec<String>,
    /// Open loops the task is still working: unresolved questions,
    /// pending verifications, follow-ups.
    pub open_loops: Vec<String>,
    /// Typed root claims that must stay resident (or recallable) while this
    /// task is active.
    pub working_refs: Vec<ContextRootClaim>,
    /// Typed retention claims that must survive in storage but are not
    /// automatic prompt/residency roots for unrelated tasks.
    pub evidence_refs: Vec<ContextRootClaim>,
}

/// A bounded, field-level patch to one task's anchor. `None` fields are
/// left untouched; the patch applies through one CAS against
/// `base_revision`, so concurrent writers cannot merge intent. Field names
/// mirror the `TaskAnchor` serde names, so the changed-fields audit on
/// `TaskAnchorChanged` matches whole-anchor replacements.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AnchorPatch {
    /// `TaskAnchor.original_goal` — boundary: user authority.
    pub original_goal: Option<String>,
    /// `TaskAnchor.constraints` — boundary: scope/waiver authority.
    pub constraints: Option<Vec<String>>,
    /// `TaskAnchor.current_interpretation` — autonomous.
    pub current_interpretation: Option<String>,
    /// `TaskAnchor.acceptance_criteria` — autonomous.
    pub acceptance_criteria: Option<Vec<String>>,
    /// `TaskAnchor.plan_progress` — autonomous.
    pub plan_progress: Option<Vec<String>>,
    /// `TaskAnchor.open_loops` — autonomous.
    pub open_loops: Option<Vec<String>>,
    /// `TaskAnchor.working_refs` — autonomous.
    pub working_refs: Option<Vec<ContextRootClaim>>,
    /// `TaskAnchor.evidence_refs` — autonomous.
    pub evidence_refs: Option<Vec<ContextRootClaim>>,
}

impl AnchorPatch {
    /// The patch's authority kind: boundary when any goal/constraint field
    /// moves (user authority), autonomous otherwise (runtime-evolvable
    /// interpretation/plan/open-loop/criteria/ref fields).
    pub fn kind(&self) -> AnchorPatchKind {
        if self.original_goal.is_some() || self.constraints.is_some() {
            AnchorPatchKind::Boundary
        } else {
            AnchorPatchKind::Autonomous
        }
    }

    /// Produce the candidate replacement anchor. Untouched fields keep
    /// their current values, so a field-level patch never resets sibling
    /// fields the way a whole-anchor replacement can.
    pub fn apply_to(&self, anchor: &TaskAnchor) -> TaskAnchor {
        let mut next = anchor.clone();
        if let Some(value) = &self.original_goal {
            next.original_goal = value.clone();
        }
        if let Some(value) = &self.constraints {
            next.constraints = value.clone();
        }
        if let Some(value) = &self.current_interpretation {
            next.current_interpretation = value.clone();
        }
        if let Some(value) = &self.acceptance_criteria {
            next.acceptance_criteria = value.clone();
        }
        if let Some(value) = &self.plan_progress {
            next.plan_progress = value.clone();
        }
        if let Some(value) = &self.open_loops {
            next.open_loops = value.clone();
        }
        if let Some(value) = &self.working_refs {
            next.working_refs = value.clone();
        }
        if let Some(value) = &self.evidence_refs {
            next.evidence_refs = value.clone();
        }
        next
    }
}

/// One typed root claim inside a `TaskAnchor`. The role says *why* the claim
/// exists; the strength says *how strongly* the context policy must hold it.
/// The ref names a context item id, artifact ref, or exact entity name — the
/// anchor does not embed item bodies.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextRootClaim {
    pub item_ref: String,
    pub role: RootClaimRole,
    pub strength: RootClaimStrength,
    /// Which anchor field the claim came from (for provenance/audit).
    pub source_field_id: String,
}

/// Why a root claim exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootClaimRole {
    ConstraintSource,
    AcceptanceEvidence,
    ActiveDecision,
    OpenLoopEvidence,
    WorkingArtifact,
    Verification,
}

/// How strongly the context policy must hold a root claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootClaimStrength {
    /// The claim must be in the model prompt for this task.
    PromptRequired,
    /// The claim must stay resident in the working set.
    ResidentRequired,
    /// The claim must survive in storage (never permanently deleted).
    StorageRequired,
    /// The claim is recallable on demand; no residency guarantee.
    Recallable,
}

/// 把任务锚的根声明投影为上下文策略消费的有界集合。任务权威留在
/// `TaskManager`；GC/materialization 只看到这条投影（有界、按强度原样
/// 映射、来源字段与锚修订保留），从不复制锚本身。working_refs 与
/// evidence_refs 都投影；超出 `MAX_ANCHOR_ROOT_CLAIMS` 的尾部截断
/// （TaskAnchor 本身已被 `MAX_TASK_ANCHOR_CLAIMS` 限制，双上限下投影
/// 不会膨胀）。三类强度保持独立：PromptRequired 进帧，ResidentRequired
/// 保 residency，StorageRequired 只挡永久删除。
pub fn anchor_root_claims(anchor: &TaskAnchor) -> Vec<agent_contracts::AnchorRootClaim> {
    let mut claims = Vec::with_capacity(
        anchor
            .working_refs
            .len()
            .saturating_add(anchor.evidence_refs.len()),
    );
    for claim in anchor
        .working_refs
        .iter()
        .chain(anchor.evidence_refs.iter())
    {
        claims.push(agent_contracts::AnchorRootClaim {
            item_ref: claim.item_ref.clone(),
            strength: match claim.strength {
                RootClaimStrength::PromptRequired => {
                    agent_contracts::AnchorRootStrength::PromptRequired
                }
                RootClaimStrength::ResidentRequired => {
                    agent_contracts::AnchorRootStrength::ResidentRequired
                }
                RootClaimStrength::StorageRequired => {
                    agent_contracts::AnchorRootStrength::StorageRequired
                }
                RootClaimStrength::Recallable => agent_contracts::AnchorRootStrength::Recallable,
            },
            source_field_id: claim.source_field_id.clone(),
            anchor_revision: anchor.revision,
            reason: root_reason_for_role(claim.role),
        });
    }
    claims.truncate(agent_contracts::MAX_ANCHOR_ROOT_CLAIMS);
    claims
}

/// Bounded prompt projection of the active TaskAnchor. Raw working/evidence
/// refs stay on `anchor_root_claims`; this view is the contract the model
/// sees, not a heap item and not a copy of task authority.
pub fn task_anchor_view(anchor: &TaskAnchor) -> agent_contracts::TaskAnchorView {
    agent_contracts::TaskAnchorView {
        revision: anchor.revision,
        original_goal: anchor.original_goal.clone(),
        current_interpretation: anchor.current_interpretation.clone(),
        constraints: anchor.constraints.clone(),
        acceptance_criteria: anchor.acceptance_criteria.clone(),
        plan_progress: anchor.plan_progress.clone(),
        open_loops: anchor.open_loops.clone(),
    }
}

fn root_reason_for_role(role: RootClaimRole) -> agent_contracts::RootReason {
    match role {
        RootClaimRole::ConstraintSource => agent_contracts::RootReason::HardConstraint,
        RootClaimRole::AcceptanceEvidence => agent_contracts::RootReason::CompletionEvidence,
        RootClaimRole::ActiveDecision => agent_contracts::RootReason::TaskAnchor,
        RootClaimRole::OpenLoopEvidence => agent_contracts::RootReason::OpenLoop,
        RootClaimRole::WorkingArtifact => agent_contracts::RootReason::CurrentEpisode,
        RootClaimRole::Verification => agent_contracts::RootReason::CompletionEvidence,
    }
}

/// One immutable, typed task completion outcome.
///
/// A completed task owns exactly one committed `CompletionRecord`: it is the
/// authoritative result (task identity, the anchor revision the outcome was
/// measured against, the bounded summary, and optional refs to the exact
/// final output body and its digest). The record lives with the
/// `TaskManager` and is persisted in `RuntimeCheckpoint`; `TaskCompleted`
/// events carry only the bounded summary. Every field is capped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CompletionRecord {
    pub task_id: TaskId,
    /// The anchor revision at completion time: the outcome is measured
    /// against exactly that authority state.
    pub anchor_revision: u64,
    /// Bounded completion summary (the `/done` text or a derived one).
    pub summary: String,
    pub completed_at_ms: u64,
    /// Ref to the exact final output body (artifact path/uri), if retained.
    pub final_output_ref: Option<String>,
    /// Hex digest of the final output body, for byte-for-byte verification
    /// after overflow, restart or Storage GC.
    pub final_output_digest: Option<String>,
    /// Bounded artifact/effect refs the completion produced.
    pub artifacts: Vec<String>,
}

/// Validate a structured completion proposal from `task.complete` before
/// the runtime accepts it: a non-empty, bounded summary, a bounded number
/// of artifact refs, each a genuine `artifact://` reference of bounded
/// length. The same caps that guard a persisted `CompletionRecord` guard
/// the proposal, so a committed record can never exceed them.
pub(crate) fn validate_completion_proposal(
    proposal: &CompletionProposal,
    workspace: Option<&agent_workspace::Workspace>,
    run_id: agent_contracts::RunId,
) -> AgentResult<()> {
    if proposal.summary.trim().is_empty() {
        return Err(AgentError::InvalidRequest(
            "completion summary must not be empty".into(),
        ));
    }
    if proposal.summary.chars().count() > MAX_COMPLETION_SUMMARY_CHARS {
        return Err(AgentError::InvalidRequest(format!(
            "completion summary has {} chars, above the {MAX_COMPLETION_SUMMARY_CHARS} cap",
            proposal.summary.chars().count()
        )));
    }
    if proposal.artifacts.len() > MAX_COMPLETION_ARTIFACTS {
        return Err(AgentError::InvalidRequest(format!(
            "completion proposal carries {} artifacts, above the {MAX_COMPLETION_ARTIFACTS} cap",
            proposal.artifacts.len()
        )));
    }
    for artifact in &proposal.artifacts {
        let locator = ArtifactLocator::parse_sealed(artifact)?;
        if artifact.chars().count() > MAX_COMPLETION_REF_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "completion artifact ref has {} chars, above the {MAX_COMPLETION_REF_CHARS} cap",
                artifact.chars().count()
            )));
        }
        let Some(workspace) = workspace else {
            return Err(AgentError::InvalidRequest(
                "completion artifacts require a trusted artifact workspace".into(),
            ));
        };
        locator.ensure_run(run_id)?;
        workspace.artifact_relative_path_for_run(artifact, run_id)?;
    }
    Ok(())
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
    /// CAS base callers must use for the next whole-anchor replacement.
    pub anchor_revision: u64,
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
    /// The active task completes (and leaves the active slot). The typed
    /// outcome is committed atomically with the status flip: a completed
    /// task owns exactly one `CompletionRecord`.
    Complete {
        active: TaskId,
        completion: CompletionRecord,
    },
    /// Atomically replace one task's complete, normalized tool-demand set.
    ReplaceToolRequirements {
        target: TaskId,
        replacement: TaskToolRequirementSet,
    },
    /// Atomically replace one task's whole anchor (bounded, versioned).
    ReplaceAnchor {
        target: TaskId,
        replacement: TaskAnchor,
    },
}

#[derive(Default)]
pub struct TaskManager {
    tasks: Vec<TaskRecord>,
    active: Option<TaskId>,
    /// One immutable outcome per completed task, in completion order. This
    /// is the authoritative task-catalog result, persisted in checkpoints.
    completed: Vec<CompletionRecord>,
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

    pub(crate) fn get_mut(&mut self, id: TaskId) -> Option<&mut TaskRecord> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    /// Replace the current-turn directive. Does not touch `TaskAnchor` or
    /// bump `anchor_revision`. Verification obligation is not wiped; due
    /// this round is computed from TurnIntent + ledger.
    pub fn on_user_turn(&mut self, text: &str) {
        let intent: String = text.chars().take(MAX_TASK_ANCHOR_TEXT_CHARS).collect();
        let Some(id) = self.active else {
            return;
        };
        let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        if task.status == TaskStatus::Completed {
            return;
        }
        task.turn_intent = intent;
        task.resume.on_user_turn();
    }

    /// Record a trusted tool fact on the active task's execution state.
    pub fn observe_tool(&mut self, output: &agent_contracts::ToolOutput, turn: u64) {
        let Some(id) = self.active else {
            return;
        };
        let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        if task.status == TaskStatus::Completed {
            return;
        }
        task.resume.observe_tool(output, task.anchor.revision, turn);
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

    /// Plan to complete the active task, committing its typed outcome with
    /// the status flip. `None` when nothing is active. The record captures
    /// the task identity and the anchor revision the outcome is measured
    /// against; the bounded summary comes from the caller (`/done` text or
    /// a `task.complete` proposal). `final_output_ref`/`final_output_digest`
    /// name the exact final output body (if retained) so the outcome stays
    /// byte-for-byte verifiable; `artifacts` are the completion's bounded
    /// `artifact://` refs recorded on the outcome.
    pub fn prepare_complete(
        &self,
        summary: String,
        final_output_ref: Option<String>,
        final_output_digest: Option<String>,
        artifacts: Vec<String>,
    ) -> Option<(TaskTxn, CompletionRecord)> {
        let active = self.active?;
        let anchor_revision = self
            .tasks
            .iter()
            .find(|task| task.id == active)
            .map(|task| task.anchor.revision)
            .unwrap_or_default();
        let record = CompletionRecord {
            task_id: active,
            anchor_revision,
            summary,
            completed_at_ms: now_ms(),
            final_output_ref,
            final_output_digest,
            artifacts,
        };
        Some((
            TaskTxn {
                plan: TaskPlan::Complete {
                    active,
                    completion: record.clone(),
                },
            },
            record,
        ))
    }

    /// Every committed completion outcome, in completion order.
    pub fn completed_records(&self) -> &[CompletionRecord] {
        &self.completed
    }

    /// The committed outcome of one task, if it completed.
    pub fn completion_of(&self, task_id: TaskId) -> Option<&CompletionRecord> {
        self.completed
            .iter()
            .find(|record| record.task_id == task_id)
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

    /// Plan a bounded whole-anchor CAS replacement of a task's authority.
    ///
    /// The supplied `base_revision` must match the task's current anchor
    /// revision. The caller's anchor content is validated and bounded; the
    /// revision field on the supplied anchor is ignored and re-stamped by the
    /// CAS (equivalent anchors are idempotent and do not bump it). Completed
    /// tasks are immutable. Returns the transaction, the resulting revision,
    /// and the capped list of field names whose content moved.
    pub fn prepare_replace_anchor(
        &self,
        task_id: TaskId,
        base_revision: u64,
        mut anchor: TaskAnchor,
    ) -> AgentResult<(TaskTxn, u64, Vec<String>)> {
        let task = self.get(task_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("task {task_id} is not registered"))
        })?;
        if task.status == TaskStatus::Completed {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} is completed and its anchor is immutable"
            )));
        }
        if task.anchor.revision != base_revision {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} anchor revision mismatch: expected {}, got {base_revision}",
                task.anchor.revision
            )));
        }

        normalize_anchor(&mut anchor)?;
        let changed_fields = anchor_changed_fields(&task.anchor, &anchor);
        let revision = if changed_fields.is_empty() {
            base_revision
        } else {
            base_revision.checked_add(1).ok_or_else(|| {
                AgentError::InvalidRequest(format!("task {task_id} anchor revision is exhausted"))
            })?
        };
        anchor.revision = revision;
        Ok((
            TaskTxn {
                plan: TaskPlan::ReplaceAnchor {
                    target: task_id,
                    replacement: anchor,
                },
            },
            revision,
            changed_fields,
        ))
    }

    /// Plan a bounded, field-level CAS patch of one task's anchor.
    ///
    /// The patch is classified before it reaches the task table: patches
    /// touching only runtime-evolvable fields (interpretation, plan, open
    /// loops, criteria, refs) are `Autonomous` and apply directly; patches
    /// touching goal/constraints are `Boundary` and must clear the approval
    /// gate before this transaction is committed. Completed tasks are
    /// immutable and the `base_revision` must still match, exactly like a
    /// whole-anchor replacement. Returns the transaction, the resulting
    /// revision, the capped changed-field list and the authority kind.
    pub fn prepare_patch_anchor(
        &self,
        task_id: TaskId,
        base_revision: u64,
        patch: &AnchorPatch,
    ) -> AgentResult<(TaskTxn, u64, Vec<String>, AnchorPatchKind)> {
        let task = self.get(task_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("task {task_id} is not registered"))
        })?;
        if task.status == TaskStatus::Completed {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} is completed and its anchor is immutable"
            )));
        }
        if task.anchor.revision != base_revision {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} anchor revision mismatch: expected {}, got {base_revision}",
                task.anchor.revision
            )));
        }
        let kind = patch.kind();
        let mut replacement = patch.apply_to(&task.anchor);
        normalize_anchor(&mut replacement)?;
        let changed_fields = anchor_changed_fields(&task.anchor, &replacement);
        let revision = if changed_fields.is_empty() {
            base_revision
        } else {
            base_revision.checked_add(1).ok_or_else(|| {
                AgentError::InvalidRequest(format!("task {task_id} anchor revision is exhausted"))
            })?
        };
        replacement.revision = revision;
        Ok((
            TaskTxn {
                plan: TaskPlan::ReplaceAnchor {
                    target: task_id,
                    replacement,
                },
            },
            revision,
            changed_fields,
            kind,
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
                    goal: goal.clone(),
                    status: TaskStatus::Active,
                    created_at_ms: now,
                    last_active_ms: now,
                    tool_requirements: TaskToolRequirementSet::default(),
                    anchor: TaskAnchor {
                        original_goal: goal,
                        ..TaskAnchor::default()
                    },
                    resume: crate::execution::ExecutionState::default(),
                    turn_intent: String::new(),
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
            TaskPlan::Complete { active, completion } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == active) {
                    task.status = TaskStatus::Completed;
                }
                self.completed.push(completion);
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
            TaskPlan::ReplaceAnchor {
                target,
                replacement,
            } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == target) {
                    if task.anchor.revision != replacement.revision {
                        task.resume.mark_spec_changed();
                    }
                    task.resume.anchor_revision = replacement.revision;
                    task.anchor = replacement;
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
        self.completed = snapshot.completed;
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
                anchor_revision: task.anchor.revision,
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

/// Validate and bound one task-owned anchor before it enters the task table.
/// The revision field is owned by the CAS flow and is left untouched here.
pub(crate) fn normalize_anchor(anchor: &mut TaskAnchor) -> AgentResult<()> {
    check_bounded_text("anchor original_goal", &anchor.original_goal)?;
    check_bounded_text(
        "anchor current_interpretation",
        &anchor.current_interpretation,
    )?;
    for (field, list) in [
        ("constraints", &anchor.constraints),
        ("acceptance_criteria", &anchor.acceptance_criteria),
        ("plan_progress", &anchor.plan_progress),
        ("open_loops", &anchor.open_loops),
    ] {
        check_bounded_list(field, list)?;
    }
    for (field, claims) in [
        ("working_refs", &anchor.working_refs),
        ("evidence_refs", &anchor.evidence_refs),
    ] {
        if claims.len() > MAX_TASK_ANCHOR_CLAIMS {
            return Err(AgentError::InvalidRequest(format!(
                "anchor {field} carries {} claims, above the {MAX_TASK_ANCHOR_CLAIMS} cap",
                claims.len()
            )));
        }
        for claim in claims {
            check_bounded_text(&format!("{field}.item_ref"), &claim.item_ref)?;
            check_bounded_text(&format!("{field}.source_field_id"), &claim.source_field_id)?;
        }
    }
    Ok(())
}

/// The capped list of anchor field names whose content differs from `old`,
/// used by the bounded `TaskAnchorChanged` audit event. `old` and `new` must
/// both already be normalized.
pub(crate) fn anchor_changed_fields(old: &TaskAnchor, new: &TaskAnchor) -> Vec<String> {
    let mut changed = Vec::new();
    let consider = |name: &str, different: bool, changed: &mut Vec<String>| {
        if different && changed.len() < MAX_TASK_ANCHOR_CHANGED_FIELDS {
            changed.push(name.to_string());
        }
    };
    consider(
        "original_goal",
        old.original_goal != new.original_goal,
        &mut changed,
    );
    consider(
        "current_interpretation",
        old.current_interpretation != new.current_interpretation,
        &mut changed,
    );
    consider(
        "constraints",
        old.constraints != new.constraints,
        &mut changed,
    );
    consider(
        "acceptance_criteria",
        old.acceptance_criteria != new.acceptance_criteria,
        &mut changed,
    );
    consider(
        "plan_progress",
        old.plan_progress != new.plan_progress,
        &mut changed,
    );
    consider("open_loops", old.open_loops != new.open_loops, &mut changed);
    consider(
        "working_refs",
        old.working_refs != new.working_refs,
        &mut changed,
    );
    consider(
        "evidence_refs",
        old.evidence_refs != new.evidence_refs,
        &mut changed,
    );
    changed
}

/// The authority split of an anchor change whose moved fields are known:
/// boundary when any goal/constraint field moved (user authority), else
/// autonomous. Used both by whole-anchor replacements (audit labeling) and
/// field-level patches (where `AnchorPatch::kind` classifies the intent and
/// this labels the result consistently).
pub(crate) fn changed_fields_kind(changed_fields: &[String]) -> AnchorPatchKind {
    if changed_fields
        .iter()
        .any(|field| field == "original_goal" || field == "constraints")
    {
        AnchorPatchKind::Boundary
    } else {
        AnchorPatchKind::Autonomous
    }
}

fn check_bounded_text(field: &str, value: &str) -> AgentResult<()> {
    let chars = value.chars().count();
    if chars > MAX_TASK_ANCHOR_TEXT_CHARS {
        return Err(AgentError::InvalidRequest(format!(
            "{field} has {chars} chars, above the {MAX_TASK_ANCHOR_TEXT_CHARS} cap"
        )));
    }
    Ok(())
}

fn check_bounded_list(field: &str, list: &[String]) -> AgentResult<()> {
    if list.len() > MAX_TASK_ANCHOR_LIST_ITEMS {
        return Err(AgentError::InvalidRequest(format!(
            "{field} carries {} entries, above the {MAX_TASK_ANCHOR_LIST_ITEMS} cap",
            list.len()
        )));
    }
    for entry in list {
        let chars = entry.chars().count();
        if chars == 0 {
            return Err(AgentError::InvalidRequest(format!(
                "{field} contains an empty entry"
            )));
        }
        if chars > MAX_TASK_ANCHOR_ITEM_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "{field} entry has {chars} chars, above the {MAX_TASK_ANCHOR_ITEM_CHARS} cap"
            )));
        }
    }
    Ok(())
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

/// Check that a checkpoint-owned anchor is bounded and its revision is
/// consistent with the CAS flow: the initial anchor is revision 0 with no
/// evolved fields (only the original goal), so a checkpoint cannot mint a
/// zero-revision anchor that pretends to have evolved content.
pub(crate) fn validate_anchor(anchor: &TaskAnchor) -> AgentResult<()> {
    normalize_anchor(&mut anchor.clone())?;
    let empty_evolved = TaskAnchor {
        original_goal: anchor.original_goal.clone(),
        ..TaskAnchor::default()
    };
    if anchor.revision == 0 && anchor != &empty_evolved {
        return Err(AgentError::InvalidRequest(
            "task anchor at revision 0 must carry only the original goal".into(),
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

/// Hex SHA-256 digest of a final-output body, so a completion outcome stays
/// byte-for-byte verifiable after overflow, restart or Storage GC.
pub(crate) fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

        let (txn, _record) = tasks
            .prepare_complete("done".into(), None, None, Vec::new())
            .expect("a is active");
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
        assert!(
            tasks
                .prepare_complete("done".into(), None, None, Vec::new())
                .is_none()
        );
    }

    #[test]
    fn anchor_patch_kind_classifies_authority() {
        let autonomous = AnchorPatch {
            plan_progress: Some(vec!["step 1".into()]),
            ..AnchorPatch::default()
        };
        assert_eq!(autonomous.kind(), AnchorPatchKind::Autonomous);
        assert_eq!(
            AnchorPatch {
                open_loops: Some(vec!["loop".into()]),
                acceptance_criteria: Some(vec!["crit".into()]),
                ..AnchorPatch::default()
            }
            .kind(),
            AnchorPatchKind::Autonomous
        );

        let boundary = AnchorPatch {
            constraints: Some(vec!["no network".into()]),
            ..AnchorPatch::default()
        };
        assert_eq!(boundary.kind(), AnchorPatchKind::Boundary);
        assert_eq!(
            AnchorPatch {
                original_goal: Some("rewrite".into()),
                plan_progress: Some(vec!["step 1".into()]),
                ..AnchorPatch::default()
            }
            .kind(),
            AnchorPatchKind::Boundary,
            "a patch touching goal authority is boundary even with autonomous fields"
        );
    }

    #[test]
    fn anchor_patch_applies_only_touched_fields() {
        let claim =
            |role: RootClaimRole, strength: RootClaimStrength, field: &str| ContextRootClaim {
                item_ref: "ref".into(),
                role,
                strength,
                source_field_id: field.into(),
            };
        let anchor = TaskAnchor {
            original_goal: "goal".into(),
            current_interpretation: "interpretation".into(),
            constraints: vec!["c1".into()],
            acceptance_criteria: vec!["crit".into()],
            plan_progress: vec!["done".into()],
            open_loops: vec!["loop".into()],
            working_refs: vec![claim(
                RootClaimRole::ActiveDecision,
                RootClaimStrength::ResidentRequired,
                "working_refs",
            )],
            evidence_refs: vec![claim(
                RootClaimRole::AcceptanceEvidence,
                RootClaimStrength::StorageRequired,
                "evidence_refs",
            )],
            revision: 0,
        };
        let next = AnchorPatch {
            plan_progress: Some(vec!["next".into()]),
            ..AnchorPatch::default()
        }
        .apply_to(&anchor);
        assert_eq!(next.original_goal, "goal");
        assert_eq!(next.current_interpretation, "interpretation");
        assert_eq!(next.constraints, vec!["c1".to_string()]);
        assert_eq!(next.acceptance_criteria, vec!["crit".to_string()]);
        assert_eq!(next.plan_progress, vec!["next".to_string()]);
        assert_eq!(next.open_loops, vec!["loop".to_string()]);
        assert_eq!(next.working_refs.len(), 1);
        assert_eq!(next.evidence_refs.len(), 1);
        assert_eq!(next.revision, 0, "the patch does not re-stamp the revision");
    }

    #[test]
    fn prepare_patch_anchor_bumps_revision_and_reports_kind() {
        let mut tasks = TaskManager::new();
        let id = create(&mut tasks, "goal");

        let (txn, revision, changed_fields, kind) = tasks
            .prepare_patch_anchor(
                id,
                0,
                &AnchorPatch {
                    open_loops: Some(vec!["verify".into()]),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        assert_eq!(revision, 1);
        assert_eq!(kind, AnchorPatchKind::Autonomous);
        assert_eq!(changed_fields, vec!["open_loops".to_string()]);
        tasks.commit(txn);

        // Idempotent: the same patch again changes nothing and keeps the
        // revision.
        let (txn, revision, changed_fields, _kind) = tasks
            .prepare_patch_anchor(
                id,
                1,
                &AnchorPatch {
                    open_loops: Some(vec!["verify".into()]),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        assert_eq!(revision, 1);
        assert!(changed_fields.is_empty());
        tasks.commit(txn);

        // Boundary classification is reported for goal/constraint patches.
        let (_txn, revision, changed_fields, kind) = tasks
            .prepare_patch_anchor(
                id,
                1,
                &AnchorPatch {
                    constraints: Some(vec!["no network".into()]),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        assert_eq!(revision, 2);
        assert_eq!(kind, AnchorPatchKind::Boundary);
        assert_eq!(changed_fields, vec!["constraints".to_string()]);
    }

    #[test]
    fn prepare_patch_anchor_rejects_completed_and_stale_revisions() {
        let mut tasks = TaskManager::new();
        let id = create(&mut tasks, "goal");

        let stale = tasks.prepare_patch_anchor(
            id,
            9,
            &AnchorPatch {
                open_loops: Some(vec!["x".into()]),
                ..AnchorPatch::default()
            },
        );
        assert!(
            stale
                .err()
                .expect("a stale base revision must be refused")
                .to_string()
                .contains("revision mismatch"),
            "a stale base revision must be refused"
        );

        let (txn, _record) = tasks
            .prepare_complete("done".into(), None, None, Vec::new())
            .expect("active task completes");
        tasks.commit(txn);
        let closed = tasks.prepare_patch_anchor(
            id,
            1,
            &AnchorPatch {
                open_loops: Some(vec!["x".into()]),
                ..AnchorPatch::default()
            },
        );
        assert!(
            closed
                .err()
                .expect("a completed task's anchor must be immutable")
                .to_string()
                .contains("immutable"),
            "a completed task's anchor must be immutable"
        );
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
        let (txn, _record) = tasks
            .prepare_complete("done".into(), None, None, Vec::new())
            .expect("task is active");
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

    fn evolved_anchor() -> TaskAnchor {
        TaskAnchor {
            revision: 0, // the CAS flow re-stamps this
            original_goal: "task A".into(),
            current_interpretation: "refactor the auth module".into(),
            constraints: vec!["no dependency changes".into()],
            acceptance_criteria: vec!["tests pass".into(), "api unchanged".into()],
            plan_progress: vec!["read the module".into()],
            open_loops: vec!["verify edge cases".into()],
            working_refs: vec![ContextRootClaim {
                item_ref: "item:auth".into(),
                role: RootClaimRole::ActiveDecision,
                strength: RootClaimStrength::ResidentRequired,
                source_field_id: "plan_progress".into(),
            }],
            evidence_refs: Vec::new(),
        }
    }

    #[test]
    fn anchor_is_whole_set_cas_bounded_and_idempotent() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");

        // The initial anchor carries only the original goal at revision 0.
        let initial = tasks.get(task_id).unwrap().anchor.clone();
        assert_eq!(initial.revision, 0);
        assert_eq!(initial.original_goal, "task A");
        assert!(initial.current_interpretation.is_empty());
        assert!(initial.constraints.is_empty());

        let (txn, revision, changed_fields) = tasks
            .prepare_replace_anchor(task_id, 0, evolved_anchor())
            .expect("initial CAS is valid");
        assert_eq!(revision, 1);
        assert_eq!(
            changed_fields,
            vec![
                "current_interpretation",
                "constraints",
                "acceptance_criteria",
                "plan_progress",
                "open_loops",
                "working_refs"
            ]
        );
        assert_eq!(
            tasks.get(task_id).unwrap().anchor.revision,
            0,
            "prepare is not visible before commit"
        );
        tasks.commit(txn);
        let stored = &tasks.get(task_id).unwrap().anchor;
        assert_eq!(stored.revision, 1);
        assert_eq!(stored.current_interpretation, "refactor the auth module");
        assert_eq!(stored.working_refs[0].role, RootClaimRole::ActiveDecision);
        assert_eq!(tasks.list()[0].anchor_revision, 1);

        // Equivalent replacement is idempotent and does not bump revision.
        let (txn, revision, changed_fields) = tasks
            .prepare_replace_anchor(task_id, 1, evolved_anchor())
            .expect("equivalent anchor is valid");
        assert_eq!(revision, 1, "an equivalent anchor must not bump revision");
        assert!(changed_fields.is_empty());
        tasks.commit(txn);
        assert_eq!(tasks.get(task_id).unwrap().anchor.revision, 1);

        // A stale base revision is rejected by CAS.
        let stale = tasks.prepare_replace_anchor(task_id, 0, evolved_anchor());
        assert!(matches!(stale, Err(AgentError::InvalidRequest(_))));
    }

    #[test]
    fn anchor_validation_is_bounded_and_refuses_empty_entries() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");

        let mut too_long = evolved_anchor();
        too_long.current_interpretation = "理".repeat(MAX_TASK_ANCHOR_TEXT_CHARS + 1);
        assert!(matches!(
            tasks.prepare_replace_anchor(task_id, 0, too_long),
            Err(AgentError::InvalidRequest(_))
        ));

        let mut too_many_loops = evolved_anchor();
        too_many_loops.open_loops = (0..=MAX_TASK_ANCHOR_LIST_ITEMS)
            .map(|index| format!("loop {index}"))
            .collect();
        assert!(matches!(
            tasks.prepare_replace_anchor(task_id, 0, too_many_loops),
            Err(AgentError::InvalidRequest(_))
        ));

        let mut empty_entry = evolved_anchor();
        empty_entry.constraints = vec![String::new()];
        assert!(matches!(
            tasks.prepare_replace_anchor(task_id, 0, empty_entry),
            Err(AgentError::InvalidRequest(_))
        ));

        let mut too_many_claims = evolved_anchor();
        too_many_claims.working_refs = (0..=MAX_TASK_ANCHOR_CLAIMS)
            .map(|index| ContextRootClaim {
                item_ref: format!("item:{index}"),
                role: RootClaimRole::WorkingArtifact,
                strength: RootClaimStrength::Recallable,
                source_field_id: "open_loops".into(),
            })
            .collect();
        assert!(matches!(
            tasks.prepare_replace_anchor(task_id, 0, too_many_claims),
            Err(AgentError::InvalidRequest(_))
        ));
    }

    #[test]
    fn completed_task_rejects_anchor_replacement() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");
        let (txn, _record) = tasks
            .prepare_complete("done".into(), None, None, Vec::new())
            .expect("task is active");
        tasks.commit(txn);

        assert!(matches!(
            tasks.prepare_replace_anchor(task_id, 0, evolved_anchor()),
            Err(AgentError::InvalidRequest(_))
        ));
    }

    #[test]
    fn anchor_survives_suspend_and_resume() {
        let mut tasks = TaskManager::new();
        let task_a = create(&mut tasks, "task A");
        let (replace, _, _) = tasks
            .prepare_replace_anchor(task_a, 0, evolved_anchor())
            .unwrap();
        tasks.commit(replace);

        let task_b = create(&mut tasks, "task B");
        assert_eq!(tasks.get(task_a).unwrap().status, TaskStatus::Suspended);
        let activate = tasks.prepare_activate(task_a).unwrap();
        tasks.commit(activate);

        let restored = &tasks.get(task_a).unwrap().anchor;
        assert_eq!(restored.revision, 1);
        assert_eq!(restored.acceptance_criteria.len(), 2);
        assert_eq!(tasks.get(task_b).unwrap().status, TaskStatus::Suspended);
    }

    #[test]
    fn completion_records_are_typed_one_per_completed_task() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");
        let (replace, _, _) = tasks
            .prepare_replace_anchor(task_id, 0, evolved_anchor())
            .unwrap();
        tasks.commit(replace);

        let (txn, record) = tasks
            .prepare_complete("auth refactor shipped".into(), None, None, Vec::new())
            .unwrap();
        assert_eq!(record.task_id, task_id);
        assert_eq!(
            record.anchor_revision, 1,
            "the record names the anchor it was measured against"
        );
        assert_eq!(record.summary, "auth refactor shipped");
        assert!(
            record.final_output_ref.is_none(),
            "output retention is a later stage"
        );
        assert_eq!(
            tasks.completed_records().len(),
            0,
            "prepare is not visible before commit"
        );
        tasks.commit(txn);

        assert_eq!(tasks.completed_records().len(), 1);
        let stored = tasks
            .completion_of(task_id)
            .expect("the task owns its outcome");
        assert_eq!(stored.anchor_revision, 1);
        assert_eq!(stored.summary, "auth refactor shipped");
        assert_eq!(tasks.active(), None);
    }

    #[test]
    fn anchor_root_claims_project_boundedly_with_strength_and_source() {
        use agent_contracts::AnchorRootStrength;
        let anchor = TaskAnchor {
            original_goal: "refactor auth".into(),
            current_interpretation: "split the module".into(),
            working_refs: vec![
                ContextRootClaim {
                    item_ref: "context://run/abc".into(),
                    role: RootClaimRole::ActiveDecision,
                    strength: RootClaimStrength::PromptRequired,
                    source_field_id: "working_refs".into(),
                },
                ContextRootClaim {
                    item_ref: "AuthService.rs".into(),
                    role: RootClaimRole::WorkingArtifact,
                    strength: RootClaimStrength::ResidentRequired,
                    source_field_id: "plan_progress".into(),
                },
            ],
            evidence_refs: vec![ContextRootClaim {
                item_ref: "context://run/evidence".into(),
                role: RootClaimRole::Verification,
                strength: RootClaimStrength::StorageRequired,
                source_field_id: "evidence_refs".into(),
            }],
            ..TaskAnchor::default()
        };
        let claims = anchor_root_claims(&anchor);
        assert_eq!(claims.len(), 3, "working + evidence 全部投影");
        assert_eq!(claims[0].item_ref, "context://run/abc");
        assert_eq!(
            claims[0].strength,
            AnchorRootStrength::PromptRequired,
            "强度原样映射"
        );
        assert_eq!(claims[0].source_field_id, "working_refs");
        assert_eq!(claims[0].anchor_revision, 0);
        assert_eq!(claims[0].reason, agent_contracts::RootReason::TaskAnchor);
        assert_eq!(claims[1].strength, AnchorRootStrength::ResidentRequired);
        assert_eq!(
            claims[1].reason,
            agent_contracts::RootReason::CurrentEpisode
        );
        assert_eq!(claims[2].strength, AnchorRootStrength::StorageRequired);
        assert_eq!(claims[2].source_field_id, "evidence_refs");
        assert_eq!(
            claims[2].reason,
            agent_contracts::RootReason::CompletionEvidence
        );
        let view = task_anchor_view(&anchor);
        assert_eq!(view.revision, 0);
        assert_eq!(view.original_goal, "refactor auth");
        assert_eq!(view.current_interpretation, "split the module");
        assert!(view.constraints.is_empty());
        assert!(view.open_loops.is_empty());
    }

    #[test]
    fn anchor_root_claims_truncate_beyond_the_projection_cap() {
        let mut working = Vec::new();
        for i in 0..(agent_contracts::MAX_ANCHOR_ROOT_CLAIMS + 8) {
            working.push(ContextRootClaim {
                item_ref: format!("item-{i}"),
                role: RootClaimRole::WorkingArtifact,
                strength: RootClaimStrength::Recallable,
                source_field_id: "working_refs".into(),
            });
        }
        let anchor = TaskAnchor {
            original_goal: "x".into(),
            working_refs: working,
            ..TaskAnchor::default()
        };
        let claims = anchor_root_claims(&anchor);
        assert_eq!(
            claims.len(),
            agent_contracts::MAX_ANCHOR_ROOT_CLAIMS,
            "投影必须截断到有界上限"
        );
    }
}
