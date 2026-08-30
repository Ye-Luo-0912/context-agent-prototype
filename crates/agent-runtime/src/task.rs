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

/// Whether model-authored completion needs criterion-addressed evidence.
///
/// Tasks begin conservatively in `OperatorClosureOnly`: an ordinary final is
/// still allowed, but only an explicit operator can durably close the task.
/// A trusted host may move the anchor to `EvidenceRequired` while atomically
/// supplying non-empty criteria. The model-routable `task.manage` surface has
/// no field for either value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCompletionPolicy {
    #[default]
    OperatorClosureOnly,
    EvidenceRequired,
}

/// One stable, bounded acceptance criterion and the host-declared proof
/// domain declaration that may satisfy it. Its identity is
/// `(TaskAnchor.verification_revision, index, domain declaration)`; changing
/// content, order or declaration advances the verification revision and
/// invalidates old receipts. Defaulted legacy declaration fields remain
/// readable but can never authorize evidence-backed completion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AcceptanceCriterion {
    pub description: String,
    pub coverage_domain: String,
    pub domain_declaration_revision: u64,
    pub domain_source_digest: String,
}

impl<'de> serde::Deserialize<'de> for AcceptanceCriterion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Structured {
                description: String,
                coverage_domain: String,
                #[serde(default)]
                domain_declaration_revision: u64,
                #[serde(default)]
                domain_source_digest: String,
            },
            Legacy(String),
        }

        Ok(match Wire::deserialize(deserializer)? {
            Wire::Structured {
                description,
                coverage_domain,
                domain_declaration_revision,
                domain_source_digest,
            } => Self {
                description,
                coverage_domain,
                domain_declaration_revision,
                domain_source_digest,
            },
            // Legacy string criteria are preserved for operator visibility,
            // but the empty domain can never mint or satisfy a receipt.
            Wire::Legacy(description) => Self {
                description,
                coverage_domain: String::new(),
                domain_declaration_revision: 0,
                domain_source_digest: String::new(),
            },
        })
    }
}

impl AcceptanceCriterion {
    pub fn new(description: impl Into<String>, coverage_domain: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            coverage_domain: coverage_domain.into(),
            domain_declaration_revision: 0,
            domain_source_digest: String::new(),
        }
    }

    /// Bind a criterion to the exact current host declaration projection.
    /// Callers still pass the resulting anchor through Runtime validation;
    /// an invalid hand-built projection therefore remains fail-closed.
    pub fn declared(
        description: impl Into<String>,
        declaration: &agent_contracts::VerificationCoverageDeclaration,
    ) -> Self {
        Self {
            description: description.into(),
            coverage_domain: declaration.domain_id.clone(),
            domain_declaration_revision: declaration.declaration_revision,
            domain_source_digest: declaration.source_digest.clone(),
        }
    }

    /// Bounded model-visible line: the description plus the host-declared
    /// coverage domain that can satisfy it, when one is required. The
    /// declaration revision/digest stay host-side; the model only needs to
    /// pick a recipe class that can prove the requirement.
    pub fn view_line(&self) -> String {
        if self.coverage_domain.is_empty() {
            self.description.clone()
        } else {
            format!(
                "{} (requires domain {})",
                self.description, self.coverage_domain
            )
        }
    }
}

impl From<String> for AcceptanceCriterion {
    fn from(description: String) -> Self {
        Self {
            description,
            coverage_domain: String::new(),
            domain_declaration_revision: 0,
            domain_source_digest: String::new(),
        }
    }
}

impl From<&str> for AcceptanceCriterion {
    fn from(description: &str) -> Self {
        description.to_string().into()
    }
}

/// One actor-minted, criterion-addressed acceptance receipt. Every identity
/// required to prove currentness is retained directly; free-form completion
/// text and pre-dispatch attribution can never establish coverage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AcceptanceCoverage {
    #[serde(default)]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub verification_revision: u64,
    /// Index into `TaskAnchor.acceptance_criteria` (stable within the
    /// verification revision).
    pub criterion_index: u32,
    #[serde(default)]
    pub coverage_domain: String,
    #[serde(default)]
    pub domain_declaration_revision: u64,
    #[serde(default)]
    pub domain_source_digest: String,
    #[serde(default)]
    pub directive_revision: u64,
    #[serde(default)]
    pub workspace_revision: u64,
    /// `VerificationFact.verification_identity` the receipt links to.
    pub verification_identity: String,
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
    /// Independent verification basis revision. Goal, constraints and
    /// completion authority (policy / criteria) bump it; progress-only
    /// patches keep it stable so a Current verifier stays current through
    /// plan updates. This proof boundary is independent of approval kind.
    #[serde(default)]
    pub verification_revision: u64,
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
    /// Explicit task completion authority. The default is fail-closed for
    /// model completion until a host supplies structured criteria.
    #[serde(default)]
    pub completion_policy: TaskCompletionPolicy,
    /// Acceptance criteria the completion outcome is measured against, each
    /// paired with the host-declared verifier domain that can prove it.
    #[serde(default)]
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    /// Actor-minted acceptance receipts. Generic anchor replacement/patch
    /// APIs preserve or invalidate these; they cannot add them.
    #[serde(default)]
    pub acceptance_coverage: Vec<AcceptanceCoverage>,
    /// Ordered plan progress: what has been done and what is next.
    pub plan_progress: Vec<String>,
    /// Open loops the task is still working: unresolved questions,
    /// pending verifications, follow-ups.
    pub open_loops: Vec<String>,
    /// The single replaceable next-action guidance proposed through
    /// `task.manage`. Replaceable guidance, not a planner: the model still
    /// decides every call and may revise it after new evidence.
    #[serde(default)]
    pub next_action: String,
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
    /// `TaskAnchor.completion_policy` — host/operator authority.
    pub completion_policy: Option<TaskCompletionPolicy>,
    /// `TaskAnchor.acceptance_criteria` — host/operator authority.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterion>>,
    /// Reserved for checkpoint/API compatibility. Generic callers cannot
    /// mint or replace receipts; Runtime uses a dedicated post-observation
    /// transaction.
    pub acceptance_coverage: Option<Vec<AcceptanceCoverage>>,
    /// `TaskAnchor.plan_progress` — autonomous.
    pub plan_progress: Option<Vec<String>>,
    /// `TaskAnchor.open_loops` — autonomous.
    pub open_loops: Option<Vec<String>>,
    /// `TaskAnchor.next_action` — autonomous.
    pub next_action: Option<String>,
    /// `TaskAnchor.working_refs` — autonomous.
    pub working_refs: Option<Vec<ContextRootClaim>>,
    /// `TaskAnchor.evidence_refs` — autonomous.
    pub evidence_refs: Option<Vec<ContextRootClaim>>,
}

impl AnchorPatch {
    /// The patch's approval kind: only goal/constraint changes require the
    /// user boundary gate. Completion authority is host-ingested and still
    /// invalidates proof, but `task.manage` cannot submit it.
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
        if let Some(value) = self.completion_policy {
            next.completion_policy = value;
        }
        if let Some(value) = &self.current_interpretation {
            next.current_interpretation = value.clone();
        }
        if let Some(value) = &self.acceptance_criteria {
            next.acceptance_criteria = value.clone();
        }
        if let Some(value) = &self.acceptance_coverage {
            next.acceptance_coverage = value.clone();
        }
        if let Some(value) = &self.plan_progress {
            next.plan_progress = value.clone();
        }
        if let Some(value) = &self.open_loops {
            next.open_loops = value.clone();
        }
        if let Some(value) = &self.next_action {
            next.next_action = value.clone();
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
        acceptance_criteria: anchor
            .acceptance_criteria
            .iter()
            .map(|criterion| criterion.view_line())
            .collect(),
        plan_progress: anchor.plan_progress.clone(),
        open_loops: anchor.open_loops.clone(),
        next_action: anchor.next_action.clone(),
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

/// How the completion relates to the verification obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionVerificationStatus {
    #[default]
    Unverified,
    Current,
    Failed,
}

/// Trusted source asking Runtime to close a task.
///
/// A model proposal is a claim of successful completion and therefore needs
/// current semantic evidence. An explicit operator command may acknowledge
/// incomplete evidence, but it never bypasses task identity or commit safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionIntent {
    ModelProposal,
    ExplicitOperator,
}

/// Task-authority identity used by completion decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskStateBasis {
    pub task_id: TaskId,
    pub anchor_revision: u64,
}

/// Evidence identity used by completion decisions.
///
/// This is deliberately separate from [`TaskStateBasis`]: progress-only
/// anchor updates advance the task CAS revision without invalidating proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerificationBasis {
    pub task_id: TaskId,
    pub verification_revision: u64,
    pub directive_revision: u64,
    pub workspace_revision: u64,
}

/// Why a task is not a verified completion candidate. Variants carry only
/// bounded scalar data so the same values can be retained on an explicit
/// operator override without copying prompt or tool bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionBlocker {
    NoActiveTask,
    TaskIdentityMismatch,
    TaskNotActive,
    TaskStateStale,
    VerificationBasisStale,
    VerificationNotCurrent,
    OperatorClosureOnly,
    AcceptanceUndeclared,
    AcceptanceDeclarationStale { remaining: u32 },
    AcceptanceUncovered { remaining: u32 },
    OpenLoops { remaining: u32 },
    NextActionPending,
    RequiredContextUnavailable { remaining: u32 },
    ExecutionObligations { remaining: u32 },
    FailedCommands { remaining: u32 },
    RecoveryRequired,
    CancelCleanupPending,
    OperationInFlight,
    PendingToolWork,
}

impl CompletionBlocker {
    fn blocks_commit(self) -> bool {
        matches!(
            self,
            Self::NoActiveTask
                | Self::TaskIdentityMismatch
                | Self::TaskNotActive
                | Self::TaskStateStale
                | Self::RecoveryRequired
                | Self::CancelCleanupPending
                | Self::OperationInFlight
                | Self::PendingToolWork
        )
    }

    /// Whether Runtime lacks a safe model-owned resolver for this blocker.
    /// Repair projection must stop here instead of inventing a tool call.
    pub(crate) fn requires_operator_repair(self) -> bool {
        matches!(
            self,
            Self::NoActiveTask
                | Self::TaskIdentityMismatch
                | Self::TaskNotActive
                | Self::TaskStateStale
                | Self::VerificationBasisStale
                | Self::OperatorClosureOnly
                | Self::AcceptanceUndeclared
                | Self::AcceptanceDeclarationStale { .. }
                | Self::RequiredContextUnavailable { .. }
                | Self::RecoveryRequired
                | Self::CancelCleanupPending
                | Self::OperationInFlight
                | Self::PendingToolWork
        )
    }

    pub(crate) fn summary(self) -> String {
        match self {
            Self::NoActiveTask => "no active task".into(),
            Self::TaskIdentityMismatch => {
                "actor and task manager disagree on the active task".into()
            }
            Self::TaskNotActive => "the selected task is not active".into(),
            Self::TaskStateStale => "execution is bound to an older task-state revision".into(),
            Self::VerificationBasisStale => {
                "execution is bound to an older verification revision".into()
            }
            Self::VerificationNotCurrent => "trusted verification is not current".into(),
            Self::OperatorClosureOnly => {
                "task policy permits durable closure only by an explicit operator".into()
            }
            Self::AcceptanceUndeclared => "no acceptance criteria are declared".into(),
            Self::AcceptanceDeclarationStale { remaining } => format!(
                "{remaining} acceptance criterion/criteria are not bound to the current host declaration"
            ),
            Self::AcceptanceUncovered { remaining } => {
                format!("{remaining} acceptance criterion/criteria lack current coverage")
            }
            Self::OpenLoops { remaining } => format!("{remaining} explicit open loop(s) remain"),
            Self::NextActionPending => "a concrete next action remains".into(),
            Self::RequiredContextUnavailable { remaining } => {
                format!("{remaining} required context item(s) are unavailable")
            }
            Self::ExecutionObligations { remaining } => {
                format!("{remaining} unresolved execution obligation(s) remain")
            }
            Self::FailedCommands { remaining } => {
                format!("{remaining} unresolved failed command(s) remain")
            }
            Self::RecoveryRequired => "runtime recovery is required".into(),
            Self::CancelCleanupPending => "a cancelled operation is still unsettled".into(),
            Self::OperationInFlight => "an operation is still in flight".into(),
            Self::PendingToolWork => "tool work is still pending".into(),
        }
    }
}

/// Runtime-owned safety inputs which cannot be derived from task/execution
/// state alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CompletionSafety {
    pub recovery_required: bool,
    pub cancel_cleanup_pending: bool,
    pub operation_in_flight: bool,
    pub pending_tool_work: bool,
    pub required_context_misses: u32,
}

/// One pure, bounded completion decision shared by settlement, proposals and
/// the durable commit safe point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReadiness {
    pub intent: CompletionIntent,
    pub task_state_basis: Option<TaskStateBasis>,
    pub verification_basis: Option<VerificationBasis>,
    pub task_state_current: bool,
    pub commit_safe: bool,
    pub verified_ready: bool,
    blockers: Vec<CompletionBlocker>,
}

impl CompletionReadiness {
    pub fn allows_completion(&self) -> bool {
        self.commit_safe
            && (self.verified_ready || self.intent == CompletionIntent::ExplicitOperator)
    }

    pub fn settled_candidate(&self) -> bool {
        self.commit_safe && self.verified_ready
    }

    pub fn applicable_blockers(&self) -> Vec<CompletionBlocker> {
        self.blockers
            .iter()
            .copied()
            .filter(|blocker| {
                self.intent == CompletionIntent::ModelProposal || blocker.blocks_commit()
            })
            .collect()
    }

    pub fn refusal(&self) -> AgentError {
        let reasons = self
            .applicable_blockers()
            .into_iter()
            .map(CompletionBlocker::summary)
            .collect::<Vec<_>>()
            .join("; ");
        AgentError::InvalidRequest(if reasons.is_empty() {
            "completion is not currently authorized".into()
        } else {
            format!("completion is not ready: {reasons}")
        })
    }

    pub(crate) fn disposition(&self) -> Option<CompletionDisposition> {
        if !self.allows_completion() {
            return None;
        }
        Some(if self.verified_ready {
            CompletionDisposition::Verified
        } else {
            CompletionDisposition::OperatorOverride
        })
    }

    pub(crate) fn override_reasons(&self) -> Vec<CompletionBlocker> {
        if self.disposition() != Some(CompletionDisposition::OperatorOverride) {
            return Vec::new();
        }
        self.blockers
            .iter()
            .copied()
            .filter(|blocker| !blocker.blocks_commit())
            .collect()
    }
}

/// How Runtime authorized a durable completion record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionDisposition {
    /// Record written before the disposition field existed.
    #[default]
    LegacyUnclassified,
    Verified,
    OperatorOverride,
}

/// Hard bound on semantic blockers retained by an operator override.
pub const MAX_COMPLETION_BLOCKERS: usize = 16;

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
    /// Derived from ExecutionState at commit. Default Unverified for
    /// records written before this field existed.
    #[serde(default)]
    pub verification_status: CompletionVerificationStatus,
    /// Evidence locators from the last verification, when status is
    /// Current or Failed. Capped like `artifacts`.
    #[serde(default)]
    pub verification_refs: Vec<String>,
    /// Whether completion was backed by the full verified-readiness join or
    /// explicitly acknowledged by the operator despite semantic blockers.
    #[serde(default)]
    pub disposition: CompletionDisposition,
    /// Semantic blockers acknowledged by an operator override. Empty for a
    /// verified completion and bounded independently of prompt/tool content.
    #[serde(default)]
    pub unmet_reasons: Vec<CompletionBlocker>,
}

/// Caller-supplied fields for preparing one immutable completion record.
/// Runtime supplies task identity, anchor revision and completion time from
/// the active authority state at preparation time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionRecordDraft {
    pub summary: String,
    pub final_output_ref: Option<String>,
    pub final_output_digest: Option<String>,
    pub artifacts: Vec<String>,
    pub verification_status: CompletionVerificationStatus,
    pub verification_refs: Vec<String>,
    pub disposition: CompletionDisposition,
    pub unmet_reasons: Vec<CompletionBlocker>,
}

/// Derive the single completion decision for the current task and execution
/// projection. The function owns no actor state and performs no mutation.
pub(crate) fn derive_completion_readiness(
    intent: CompletionIntent,
    actor_task_id: Option<TaskId>,
    active_task: Option<&TaskRecord>,
    execution: Option<&crate::execution::ExecutionState>,
    safety: CompletionSafety,
    current_declarations: &[agent_contracts::VerificationCoverageDeclaration],
) -> CompletionReadiness {
    let mut blockers = Vec::with_capacity(MAX_COMPLETION_BLOCKERS);
    let mut push = |blocker| {
        if blockers.len() < MAX_COMPLETION_BLOCKERS && !blockers.contains(&blocker) {
            blockers.push(blocker);
        }
    };

    let task_state_basis = active_task.map(|task| TaskStateBasis {
        task_id: task.id,
        anchor_revision: task.anchor.revision,
    });
    let verification_basis =
        active_task
            .zip(execution)
            .map(|(task, execution)| VerificationBasis {
                task_id: task.id,
                verification_revision: task.anchor.verification_revision,
                directive_revision: execution.directive_revision,
                workspace_revision: execution.workspace_revision,
            });

    match active_task {
        None => push(CompletionBlocker::NoActiveTask),
        Some(task) => {
            if actor_task_id != Some(task.id) {
                push(CompletionBlocker::TaskIdentityMismatch);
            }
            if task.status != TaskStatus::Active {
                push(CompletionBlocker::TaskNotActive);
            }
            let Some(execution) = execution else {
                push(CompletionBlocker::TaskStateStale);
                return finish_completion_readiness(
                    intent,
                    task_state_basis,
                    verification_basis,
                    blockers,
                    safety,
                );
            };
            if execution.anchor_revision != task.anchor.revision {
                push(CompletionBlocker::TaskStateStale);
            }
            if execution.verification.spec_revision != task.anchor.verification_revision {
                push(CompletionBlocker::VerificationBasisStale);
            }

            let trusted = verification_basis
                .as_ref()
                .and_then(|basis| current_trusted_verification(execution, basis));
            if trusted.is_none() {
                push(CompletionBlocker::VerificationNotCurrent);
            }

            if task.anchor.acceptance_criteria.is_empty() {
                push(CompletionBlocker::AcceptanceUndeclared);
            }
            if task.anchor.completion_policy == TaskCompletionPolicy::OperatorClosureOnly {
                push(CompletionBlocker::OperatorClosureOnly);
            } else if !task.anchor.acceptance_criteria.is_empty() {
                let declarations_current = declarations_are_canonical(current_declarations);
                let stale_declarations = task
                    .anchor
                    .acceptance_criteria
                    .iter()
                    .filter(|criterion| {
                        !declarations_current
                            || !criterion_matches_current_declaration(
                                criterion,
                                current_declarations,
                            )
                    })
                    .count();
                if stale_declarations > 0 {
                    push(CompletionBlocker::AcceptanceDeclarationStale {
                        remaining: stale_declarations.min(u32::MAX as usize) as u32,
                    });
                }
                let uncovered = task
                    .anchor
                    .acceptance_criteria
                    .iter()
                    .enumerate()
                    .filter(|(index, criterion)| {
                        !task.anchor.acceptance_coverage.iter().any(|receipt| {
                            receipt.criterion_index as usize == *index
                                && receipt.coverage_domain == criterion.coverage_domain
                                && receipt.domain_declaration_revision
                                    == criterion.domain_declaration_revision
                                && receipt.domain_source_digest == criterion.domain_source_digest
                                && declarations_current
                                && criterion_matches_current_declaration(
                                    criterion,
                                    current_declarations,
                                )
                                && verification_basis.as_ref().is_some_and(|basis| {
                                    acceptance_receipt_fact(execution, task, basis, receipt)
                                        .is_some()
                                })
                        })
                    })
                    .count();
                if uncovered > 0 {
                    push(CompletionBlocker::AcceptanceUncovered {
                        remaining: uncovered.min(u32::MAX as usize) as u32,
                    });
                }
            }
            if !task.anchor.open_loops.is_empty() {
                push(CompletionBlocker::OpenLoops {
                    remaining: task.anchor.open_loops.len().min(u32::MAX as usize) as u32,
                });
            }
            if !task.anchor.next_action.trim().is_empty() {
                push(CompletionBlocker::NextActionPending);
            }
            let obligations = execution.open_obligation_count();
            if obligations > 0 {
                push(CompletionBlocker::ExecutionObligations {
                    remaining: obligations.min(u32::MAX as usize) as u32,
                });
            }
            let failed_commands = execution.unresolved_failed_command_count();
            if failed_commands > 0 {
                push(CompletionBlocker::FailedCommands {
                    remaining: failed_commands.min(u32::MAX as usize) as u32,
                });
            }
        }
    }

    finish_completion_readiness(
        intent,
        task_state_basis,
        verification_basis,
        blockers,
        safety,
    )
}

fn declarations_are_canonical(
    declarations: &[agent_contracts::VerificationCoverageDeclaration],
) -> bool {
    declarations.len() <= agent_contracts::MAX_VERIFICATION_COVERAGE_DECLARATIONS
        && declarations
            .iter()
            .all(agent_contracts::VerificationCoverageDeclaration::is_valid)
        && declarations
            .windows(2)
            .all(|pair| pair[0].domain_id < pair[1].domain_id)
}

fn criterion_matches_current_declaration(
    criterion: &AcceptanceCriterion,
    declarations: &[agent_contracts::VerificationCoverageDeclaration],
) -> bool {
    if criterion.domain_declaration_revision == 0
        || criterion
            .domain_source_digest
            .parse::<agent_contracts::ContentDigest>()
            .is_err()
    {
        return false;
    }
    declarations
        .binary_search_by(|declaration| {
            declaration
                .domain_id
                .as_str()
                .cmp(criterion.coverage_domain.as_str())
        })
        .ok()
        .map(|index| &declarations[index])
        .is_some_and(|declaration| {
            declaration.declaration_revision == criterion.domain_declaration_revision
                && declaration.source_digest == criterion.domain_source_digest
        })
}

fn finish_completion_readiness(
    intent: CompletionIntent,
    task_state_basis: Option<TaskStateBasis>,
    verification_basis: Option<VerificationBasis>,
    mut blockers: Vec<CompletionBlocker>,
    safety: CompletionSafety,
) -> CompletionReadiness {
    for blocker in [
        safety
            .recovery_required
            .then_some(CompletionBlocker::RecoveryRequired),
        safety
            .cancel_cleanup_pending
            .then_some(CompletionBlocker::CancelCleanupPending),
        safety
            .operation_in_flight
            .then_some(CompletionBlocker::OperationInFlight),
        safety
            .pending_tool_work
            .then_some(CompletionBlocker::PendingToolWork),
    ]
    .into_iter()
    .flatten()
    {
        if blockers.len() < MAX_COMPLETION_BLOCKERS && !blockers.contains(&blocker) {
            blockers.push(blocker);
        }
    }
    if safety.required_context_misses > 0 && blockers.len() < MAX_COMPLETION_BLOCKERS {
        blockers.push(CompletionBlocker::RequiredContextUnavailable {
            remaining: safety.required_context_misses,
        });
    }
    let task_state_current = task_state_basis.is_some()
        && !blockers.iter().copied().any(|blocker| {
            matches!(
                blocker,
                CompletionBlocker::NoActiveTask
                    | CompletionBlocker::TaskIdentityMismatch
                    | CompletionBlocker::TaskNotActive
                    | CompletionBlocker::TaskStateStale
            )
        });
    let commit_safe = task_state_current
        && !blockers
            .iter()
            .copied()
            .any(CompletionBlocker::blocks_commit);
    let verified_ready = !blockers
        .iter()
        .copied()
        .any(|blocker| !blocker.blocks_commit());
    CompletionReadiness {
        intent,
        task_state_basis,
        verification_basis,
        task_state_current,
        commit_safe,
        verified_ready,
        blockers,
    }
}

fn current_trusted_verification<'a>(
    state: &'a crate::execution::ExecutionState,
    basis: &VerificationBasis,
) -> Option<&'a crate::execution::VerificationFact> {
    if state.verification.spec_revision != basis.verification_revision
        || state.validity() != crate::execution::VerificationState::Current
    {
        return None;
    }
    let trusted_identity = state.trusted_verification_identity()?;
    state.verifications.iter().rev().find(|fact| {
        fact.ok
            && !fact.source_tool_name.is_empty()
            && fact.verification_identity == trusted_identity
            && fact.anchor_revision == basis.verification_revision
            && fact.directive_revision == basis.directive_revision
            && fact.workspace_revision == basis.workspace_revision
    })
}

/// Resolve one acceptance receipt to the exact current trusted PASS it
/// names. Unlike the single latest verification used for completion-summary
/// provenance, criterion receipts may legitimately point at different PASS
/// facts from the same task/directive/world basis.
pub(crate) fn acceptance_receipt_fact<'a>(
    state: &'a crate::execution::ExecutionState,
    task: &TaskRecord,
    basis: &VerificationBasis,
    receipt: &AcceptanceCoverage,
) -> Option<&'a crate::execution::VerificationFact> {
    if state.verification.spec_revision != basis.verification_revision
        || state.validity() != crate::execution::VerificationState::Current
        || receipt.task_id != Some(task.id)
        || receipt.verification_revision != basis.verification_revision
        || receipt.directive_revision != basis.directive_revision
        || receipt.workspace_revision != basis.workspace_revision
        || receipt.domain_declaration_revision == 0
        || receipt
            .domain_source_digest
            .parse::<agent_contracts::ContentDigest>()
            .is_err()
    {
        return None;
    }
    state.verifications.iter().rev().find(|fact| {
        fact.ok
            && !fact.source_tool_name.is_empty()
            && fact.anchor_revision == basis.verification_revision
            && fact.directive_revision == basis.directive_revision
            && fact.workspace_revision == basis.workspace_revision
            && fact.verification_identity == receipt.verification_identity
            && fact.recipe_provenance.as_ref().is_some_and(|provenance| {
                provenance.coverage_domain.as_deref() == Some(receipt.coverage_domain.as_str())
                    && provenance.domain_declaration_revision
                        == Some(receipt.domain_declaration_revision)
                    && provenance.domain_source_digest == receipt.domain_source_digest
            })
    })
}

/// Stamp evidence on the completion record without making a second decision.
/// Only an exact trusted PASS on the already-derived verification basis may be
/// recorded as Current.
pub(crate) fn completion_evidence(
    state: &crate::execution::ExecutionState,
    basis: Option<&VerificationBasis>,
) -> (CompletionVerificationStatus, Vec<String>) {
    let current = basis.and_then(|basis| current_trusted_verification(state, basis));
    if let Some(fact) = current {
        return (
            CompletionVerificationStatus::Current,
            fact.evidence_ref.clone().into_iter().collect(),
        );
    }
    let failed = basis.and_then(|basis| {
        state.verifications.iter().rev().find(|fact| {
            !fact.ok
                && fact.anchor_revision == basis.verification_revision
                && fact.directive_revision == basis.directive_revision
                && fact.workspace_revision == basis.workspace_revision
        })
    });
    match failed {
        Some(fact) => (
            CompletionVerificationStatus::Failed,
            fact.evidence_ref.clone().into_iter().collect(),
        ),
        None => (CompletionVerificationStatus::Unverified, Vec::new()),
    }
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
    /// `authority_changed` marks a boundary-class change (goal/constraints):
    /// only those invalidate dependent verification. A progress-only CAS
    /// advances the record revision without staling a Current verifier.
    ReplaceAnchor {
        target: TaskId,
        replacement: TaskAnchor,
        authority_changed: bool,
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
        task.resume.on_user_turn(text);
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

    /// Install a committed turn's execution projection onto the durable
    /// task resume. Cancel / fail / stale paths must not call this.
    pub fn install_resume(&mut self, task_id: TaskId, resume: crate::execution::ExecutionState) {
        let Some(task) = self.get_mut(task_id) else {
            return;
        };
        if task.status == TaskStatus::Completed {
            return;
        }
        task.resume = resume;
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
        draft: CompletionRecordDraft,
    ) -> Option<(TaskTxn, CompletionRecord)> {
        let active = self.active?;
        let anchor_revision = self
            .tasks
            .iter()
            .find(|task| task.id == active)
            .map(|task| task.anchor.revision)
            .unwrap_or_default();
        let CompletionRecordDraft {
            summary,
            final_output_ref,
            final_output_digest,
            artifacts,
            verification_status,
            verification_refs,
            disposition,
            mut unmet_reasons,
        } = draft;
        unmet_reasons.truncate(MAX_COMPLETION_BLOCKERS);
        let record = CompletionRecord {
            task_id: active,
            anchor_revision,
            summary,
            completed_at_ms: now_ms(),
            final_output_ref,
            final_output_digest,
            artifacts,
            verification_status,
            verification_refs,
            disposition,
            unmet_reasons,
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
    /// Build the prospective TERMINAL snapshot for two-phase completion: the
    /// active task flips to `Completed` with its freshly prepared record inside
    /// the snapshot only, `active` clears, and the live manager is untouched.
    /// The durable acknowledgement of this exact shape is what authorizes the
    /// real in-memory transition.
    pub(crate) fn prospective_terminal_snapshot(
        tasks: &TaskManager,
        record: CompletionRecord,
    ) -> Option<crate::checkpoint::TaskManagerSnapshot> {
        let active = tasks.active?;
        let mut task_rows: Vec<crate::checkpoint::TaskRecordSnapshot> =
            Vec::with_capacity(tasks.tasks.len() + 1);
        let mut flipped = false;
        for task in &tasks.tasks {
            let status = if task.id == active {
                flipped = true;
                TaskStatus::Completed
            } else {
                task.status
            };
            task_rows.push(crate::checkpoint::TaskRecordSnapshot {
                id: task.id,
                goal: task.goal.clone(),
                status,
                created_at_ms: task.created_at_ms,
                last_active_ms: task.last_active_ms,
                tool_requirements: task.tool_requirements.clone(),
                anchor: task.anchor.clone(),
                resume: task.resume.clone(),
                turn_intent: task.turn_intent.clone(),
            });
        }
        if !flipped {
            return None;
        }
        let mut completed = tasks.completed_records().to_vec();
        completed.push(record);
        Some(crate::checkpoint::TaskManagerSnapshot {
            tasks: task_rows,
            active: None,
            completed,
        })
    }

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

        // Both revisions are Runtime-owned. Stamp the current bases before
        // receipt validation so arbitrary caller values cannot either reject
        // an otherwise valid replacement or authorize a foreign basis.
        anchor.revision = task.anchor.revision;
        anchor.verification_revision = task.anchor.verification_revision;

        let acceptance_boundary_changed = anchor.completion_policy != task.anchor.completion_policy
            || anchor.acceptance_criteria != task.anchor.acceptance_criteria;
        if acceptance_boundary_changed {
            if !anchor.acceptance_coverage.is_empty()
                && anchor.acceptance_coverage != task.anchor.acceptance_coverage
            {
                return Err(AgentError::InvalidRequest(
                    "acceptance receipts are runtime-owned and cannot be supplied through an authority replacement"
                        .into(),
                ));
            }
            anchor.acceptance_coverage.clear();
        } else if anchor.acceptance_coverage != task.anchor.acceptance_coverage {
            return Err(AgentError::InvalidRequest(
                "acceptance receipts are runtime-owned and cannot be replaced through the generic anchor API"
                    .into(),
            ));
        }
        normalize_anchor(&mut anchor)?;
        let changed_fields = anchor_changed_fields(&task.anchor, &anchor);
        let authority_changed = changed_fields.iter().any(|field| is_authority_field(field));
        let revision = if changed_fields.is_empty() {
            base_revision
        } else {
            base_revision.checked_add(1).ok_or_else(|| {
                AgentError::InvalidRequest(format!("task {task_id} anchor revision is exhausted"))
            })?
        };
        anchor.revision = revision;
        if authority_changed {
            anchor.verification_revision = task
                .anchor
                .verification_revision
                .checked_add(1)
                .ok_or_else(|| {
                    AgentError::InvalidRequest(format!(
                        "task {task_id} verification revision is exhausted"
                    ))
                })?;
        } else {
            anchor.verification_revision = task.anchor.verification_revision;
        }
        Ok((
            TaskTxn {
                plan: TaskPlan::ReplaceAnchor {
                    target: task_id,
                    replacement: anchor,
                    authority_changed,
                },
            },
            revision,
            changed_fields,
        ))
    }

    /// Plan a bounded, field-level CAS patch of one task's anchor.
    ///
    /// The patch is classified before it reaches the task table: patches
    /// touching only host/runtime-ingested fields (including completion
    /// policy/criteria) are `Autonomous` and apply directly; patches touching
    /// goal/constraints are `Boundary` and must clear the approval gate.
    /// Completed tasks are
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
        if patch.acceptance_coverage.is_some() {
            return Err(AgentError::InvalidRequest(
                "acceptance receipts are runtime-owned and cannot be patched through the generic anchor API"
                    .into(),
            ));
        }
        let kind = patch.kind();
        let mut replacement = patch.apply_to(&task.anchor);
        let acceptance_boundary_changed = replacement.completion_policy
            != task.anchor.completion_policy
            || replacement.acceptance_criteria != task.anchor.acceptance_criteria;
        if acceptance_boundary_changed {
            replacement.acceptance_coverage.clear();
        }
        normalize_anchor(&mut replacement)?;
        let changed_fields = anchor_changed_fields(&task.anchor, &replacement);
        // A patch that moves only runtime-evolvable fields advances the
        // record CAS without touching the verification basis; a boundary
        // patch (goal/constraints/completion authority) invalidates proof.
        let authority_changed = kind == AnchorPatchKind::Boundary
            || changed_fields.iter().any(|field| is_authority_field(field));
        let revision = if changed_fields.is_empty() {
            base_revision
        } else {
            base_revision.checked_add(1).ok_or_else(|| {
                AgentError::InvalidRequest(format!("task {task_id} anchor revision is exhausted"))
            })?
        };
        replacement.revision = revision;
        if authority_changed {
            replacement.verification_revision = replacement
                .verification_revision
                .checked_add(1)
                .ok_or_else(|| {
                    AgentError::InvalidRequest(format!(
                        "task {task_id} verification revision is exhausted"
                    ))
                })?;
        } else {
            replacement.verification_revision = task.anchor.verification_revision;
        }
        Ok((
            TaskTxn {
                plan: TaskPlan::ReplaceAnchor {
                    target: task_id,
                    replacement,
                    authority_changed,
                },
            },
            revision,
            changed_fields,
            kind,
        ))
    }

    /// Plan Runtime's sole acceptance-receipt mutation. The caller must have
    /// observed and matched a trusted PASS already; this transaction only
    /// enforces bounded/canonical task authority and advances the record CAS.
    /// It deliberately preserves `verification_revision`, so the receipt does
    /// not stale the PASS that earned it.
    pub(crate) fn prepare_record_acceptance_receipts(
        &self,
        task_id: TaskId,
        base_revision: u64,
        receipts: Vec<AcceptanceCoverage>,
    ) -> AgentResult<(TaskTxn, u64, Vec<String>)> {
        let task = self.get(task_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("task {task_id} is not registered"))
        })?;
        if task.status == TaskStatus::Completed {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} is completed and its acceptance receipts are immutable"
            )));
        }
        if task.anchor.revision != base_revision {
            return Err(AgentError::InvalidRequest(format!(
                "task {task_id} anchor revision mismatch: expected {}, got {base_revision}",
                task.anchor.revision
            )));
        }
        if task.anchor.completion_policy != TaskCompletionPolicy::EvidenceRequired {
            return Err(AgentError::InvalidRequest(
                "operator-closure-only tasks cannot receive acceptance receipts".into(),
            ));
        }
        if receipts
            .iter()
            .any(|receipt| receipt.task_id != Some(task_id))
        {
            return Err(AgentError::InvalidRequest(
                "acceptance receipt task identity does not match the target task".into(),
            ));
        }
        let mut replacement = task.anchor.clone();
        replacement.acceptance_coverage = receipts;
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
        replacement.verification_revision = task.anchor.verification_revision;
        Ok((
            TaskTxn {
                plan: TaskPlan::ReplaceAnchor {
                    target: task_id,
                    replacement,
                    authority_changed: false,
                },
            },
            revision,
            changed_fields,
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
                authority_changed,
            } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == target) {
                    if task.anchor.revision != replacement.revision && authority_changed {
                        // Only a boundary change (goal, constraints or
                        // acceptance criteria) makes a Current verifier
                        // stale. Progress-only CAS keeps the verification
                        // basis untouched.
                        task.resume.mark_spec_changed();
                    }
                    task.resume.anchor_revision = replacement.revision;
                    task.resume.verification.spec_revision = replacement.verification_revision;
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
        ("plan_progress", &anchor.plan_progress),
        ("open_loops", &anchor.open_loops),
    ] {
        check_bounded_list(field, list)?;
    }
    if anchor.acceptance_criteria.len() > MAX_TASK_ANCHOR_LIST_ITEMS {
        return Err(AgentError::InvalidRequest(format!(
            "anchor acceptance_criteria carries {} entries, above the {MAX_TASK_ANCHOR_LIST_ITEMS} cap",
            anchor.acceptance_criteria.len()
        )));
    }
    for criterion in &anchor.acceptance_criteria {
        check_bounded_text("anchor acceptance criterion", &criterion.description)?;
        if anchor.completion_policy == TaskCompletionPolicy::EvidenceRequired
            || !criterion.coverage_domain.is_empty()
        {
            check_coverage_domain(&criterion.coverage_domain)?;
        }
        if !criterion.domain_source_digest.is_empty()
            && criterion
                .domain_source_digest
                .parse::<agent_contracts::ContentDigest>()
                .is_err()
        {
            return Err(AgentError::InvalidRequest(
                "anchor acceptance criterion has an invalid coverage-domain source digest".into(),
            ));
        }
    }
    match anchor.completion_policy {
        TaskCompletionPolicy::EvidenceRequired if anchor.acceptance_criteria.is_empty() => {
            return Err(AgentError::InvalidRequest(
                "evidence-required tasks must declare at least one acceptance criterion".into(),
            ));
        }
        _ => {}
    }
    // Receipts are canonical by criterion and every field is bounded. The
    // task-id equality is checked by the task-aware transaction/checkpoint
    // validator because a standalone anchor has no task identity.
    if anchor.acceptance_coverage.len() > MAX_TASK_ANCHOR_LIST_ITEMS {
        return Err(AgentError::InvalidRequest(format!(
            "anchor acceptance_coverage carries {} claims, above the {MAX_TASK_ANCHOR_LIST_ITEMS} cap",
            anchor.acceptance_coverage.len()
        )));
    }
    anchor
        .acceptance_coverage
        .sort_by_key(|receipt| receipt.criterion_index);
    if anchor
        .acceptance_coverage
        .windows(2)
        .any(|pair| pair[0].criterion_index == pair[1].criterion_index)
    {
        return Err(AgentError::InvalidRequest(
            "anchor acceptance_coverage contains duplicate criterion receipts".into(),
        ));
    }
    for claim in &anchor.acceptance_coverage {
        check_bounded_text(
            "anchor acceptance_coverage identity",
            &claim.verification_identity,
        )?;
        if claim.criterion_index as usize >= anchor.acceptance_criteria.len() {
            return Err(AgentError::InvalidRequest(format!(
                "anchor acceptance_coverage addresses criterion {}, beyond the {} declared criteria",
                claim.criterion_index,
                anchor.acceptance_criteria.len()
            )));
        }
        let criterion = &anchor.acceptance_criteria[claim.criterion_index as usize];
        if anchor.completion_policy == TaskCompletionPolicy::OperatorClosureOnly {
            // Legacy v4 anchors carried string criteria and weak claims.
            // Preserve them for restore/audit but never interpret them as
            // receipts: the explicit policy blocks model completion.
            continue;
        }
        check_coverage_domain(&claim.coverage_domain)?;
        if claim.coverage_domain != criterion.coverage_domain {
            return Err(AgentError::InvalidRequest(format!(
                "anchor acceptance receipt domain '{}' does not match criterion {} domain '{}'",
                claim.coverage_domain, claim.criterion_index, criterion.coverage_domain
            )));
        }
        if claim.verification_revision != anchor.verification_revision {
            return Err(AgentError::InvalidRequest(format!(
                "anchor acceptance receipt for criterion {} names verification revision {}, expected {}",
                claim.criterion_index, claim.verification_revision, anchor.verification_revision
            )));
        }
        if !claim.domain_source_digest.is_empty()
            && claim
                .domain_source_digest
                .parse::<agent_contracts::ContentDigest>()
                .is_err()
        {
            return Err(AgentError::InvalidRequest(
                "anchor acceptance receipt has an invalid coverage-domain source digest".into(),
            ));
        }
        let criterion_fully_bound =
            criterion.domain_declaration_revision > 0 && !criterion.domain_source_digest.is_empty();
        let receipt_fully_bound =
            claim.domain_declaration_revision > 0 && !claim.domain_source_digest.is_empty();
        if criterion_fully_bound
            && receipt_fully_bound
            && (claim.domain_declaration_revision != criterion.domain_declaration_revision
                || claim.domain_source_digest != criterion.domain_source_digest)
        {
            return Err(AgentError::InvalidRequest(format!(
                "anchor acceptance receipt declaration identity does not match criterion {}",
                claim.criterion_index
            )));
        }
        if claim
            .verification_identity
            .parse::<agent_contracts::ContentDigest>()
            .is_err()
        {
            return Err(AgentError::InvalidRequest(
                "anchor acceptance receipt has an invalid verification identity".into(),
            ));
        }
    }
    check_bounded_text("anchor next_action", &anchor.next_action)?;
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

fn check_coverage_domain(domain: &str) -> AgentResult<()> {
    let chars = domain.chars().count();
    if chars == 0 || chars > MAX_TASK_ANCHOR_ITEM_CHARS {
        return Err(AgentError::InvalidRequest(format!(
            "acceptance coverage domain has {chars} chars (allowed 1..={MAX_TASK_ANCHOR_ITEM_CHARS})"
        )));
    }
    if !domain.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | ':')
    }) {
        return Err(AgentError::InvalidRequest(format!(
            "acceptance coverage domain '{domain}': only [A-Za-z0-9._:-] are allowed"
        )));
    }
    Ok(())
}

/// Verification-authority fields of the anchor: moving any of them
/// invalidates dependent verification. This is deliberately orthogonal to
/// approval classification: host-declared completion authority is not a
/// user goal/constraint waiver. Everything else
/// (interpretation, plan, open loops, next action, refs) advances only the
/// record CAS. Acceptance criteria move the basis too: they are the
/// authoritative verdict the completion outcome is measured against, so a
/// changed criterion requires fresh proof. `task.manage` cannot submit this
/// field; trusted host/operator anchor APIs may ingest it directly.
fn is_authority_field(field: &str) -> bool {
    matches!(
        field,
        "original_goal" | "constraints" | "completion_policy" | "acceptance_criteria"
    )
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
        "completion_policy",
        old.completion_policy != new.completion_policy,
        &mut changed,
    );
    consider(
        "acceptance_criteria",
        old.acceptance_criteria != new.acceptance_criteria,
        &mut changed,
    );
    consider(
        "acceptance_coverage",
        old.acceptance_coverage != new.acceptance_coverage,
        &mut changed,
    );
    consider(
        "plan_progress",
        old.plan_progress != new.plan_progress,
        &mut changed,
    );
    consider("open_loops", old.open_loops != new.open_loops, &mut changed);
    consider(
        "next_action",
        old.next_action != new.next_action,
        &mut changed,
    );
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
/// boundary when any goal/constraint field moved, else
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

/// The bounded, model-neutral settlement fact projected only when the
/// task-aware join has risen to `SettledCandidate`. It states the derived
/// readiness facts and preserves the model's own choice of ordinary final,
/// durable `task.complete`, or concrete continuation; it never instructs
/// the model to stop and never auto-closes the task.
pub(crate) const SETTLED_CANDIDATE_PROMPT_LINE: &str = "TASK SETTLED: every acceptance criterion \
     is covered by a current trusted verification, the task epoch matches, and no open loop or \
     next action remains. You may give an ordinary final answer, call task.complete for durable \
     closure, or continue with concrete remaining work.";

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
pub(crate) fn validate_anchor(task_id: TaskId, anchor: &TaskAnchor) -> AgentResult<()> {
    normalize_anchor(&mut anchor.clone())?;
    if anchor.completion_policy == TaskCompletionPolicy::EvidenceRequired
        && anchor
            .acceptance_coverage
            .iter()
            .any(|receipt| receipt.task_id != Some(task_id))
    {
        return Err(AgentError::InvalidRequest(
            "task anchor contains an acceptance receipt for a different task".into(),
        ));
    }
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

    #[test]
    fn criterion_view_line_names_the_required_domain_when_declared() {
        let plain = AcceptanceCriterion {
            description: "behaves correctly".into(),
            coverage_domain: String::new(),
            domain_declaration_revision: 0,
            domain_source_digest: String::new(),
        };
        assert_eq!(plain.view_line(), "behaves correctly");
        let declaration = agent_contracts::VerificationCoverageDeclaration {
            domain_id: "saturation-boundary".into(),
            declaration_revision: 1,
            source_digest: "a1b2c3".into(),
        };
        let declared =
            AcceptanceCriterion::declared("large attempts saturate at max_delay", &declaration);
        assert_eq!(
            declared.view_line(),
            "large attempts saturate at max_delay (requires domain saturation-boundary)"
        );
    }

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
            .prepare_complete(CompletionRecordDraft {
                summary: "done".into(),
                ..CompletionRecordDraft::default()
            })
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
                .prepare_complete(CompletionRecordDraft {
                    summary: "done".into(),
                    ..CompletionRecordDraft::default()
                })
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
            AnchorPatchKind::Autonomous,
            "trusted completion authority invalidates proof without requiring an approval round"
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
            completion_policy: TaskCompletionPolicy::OperatorClosureOnly,
            acceptance_criteria: vec!["crit".into()],
            acceptance_coverage: Vec::new(),
            plan_progress: vec!["done".into()],
            open_loops: vec!["loop".into()],
            next_action: "next step".into(),
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
            verification_revision: 0,
        };
        let next = AnchorPatch {
            plan_progress: Some(vec!["next".into()]),
            ..AnchorPatch::default()
        }
        .apply_to(&anchor);
        assert_eq!(next.original_goal, "goal");
        assert_eq!(next.current_interpretation, "interpretation");
        assert_eq!(next.constraints, vec!["c1".to_string()]);
        assert_eq!(next.acceptance_criteria[0].description, "crit");
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
            .prepare_complete(CompletionRecordDraft {
                summary: "done".into(),
                ..CompletionRecordDraft::default()
            })
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
    fn progress_only_cas_keeps_current_verification_and_boundary_change_stales_it() {
        use crate::execution::{VerificationCause, VerificationState};

        let mut tasks = TaskManager::new();
        let id = create(&mut tasks, "goal");

        // A passing verification command makes the resume verifier Current.
        let test = agent_contracts::ToolOutput {
            call_id: "c".into(),
            tool_name: "shell.exec".into(),
            ok: true,
            summary: "ok".into(),
            model_content: "exit 0".into(),
            artifact_ref: None,
            metadata: serde_json::json!({"command": "cargo test", "verification": true}),
        };
        tasks.observe_tool(&test, 1);
        assert_eq!(
            tasks.get(id).unwrap().resume.verification.state,
            VerificationState::Current,
            "the fixture itself must produce a Current verifier"
        );

        // A progress-only patch advances the record CAS but leaves the
        // verification basis untouched.
        let (txn, revision, _, kind) = tasks
            .prepare_patch_anchor(
                id,
                0,
                &AnchorPatch {
                    next_action: Some("record progress".into()),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        assert_eq!(kind, AnchorPatchKind::Autonomous);
        tasks.commit(txn);
        let task = tasks.get(id).unwrap();
        assert_eq!(task.anchor.revision, 1);
        assert_eq!(revision, 1);
        assert_eq!(
            task.resume.verification.state,
            VerificationState::Current,
            "progress-only CAS must not stale a Current verifier"
        );
        assert_eq!(
            task.resume.verification.cause,
            VerificationCause::None,
            "no stale cause may be recorded for a progress-only CAS"
        );
        assert_eq!(
            task.resume.anchor_revision, 1,
            "the CAS fence must still advance so a stale write is refused"
        );
        // The stale base revision is still rejected after the progress bump.
        let stale_write = tasks.prepare_patch_anchor(
            id,
            0,
            &AnchorPatch {
                open_loops: Some(vec!["x".into()]),
                ..AnchorPatch::default()
            },
        );
        assert!(
            stale_write.is_err(),
            "CAS fence must survive the decoupling"
        );

        // A boundary patch (goal change) does invalidate the verifier.
        let (txn, _, _, kind) = tasks
            .prepare_patch_anchor(
                id,
                1,
                &AnchorPatch {
                    original_goal: Some("changed goal".into()),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        assert_eq!(kind, AnchorPatchKind::Boundary);
        tasks.commit(txn);
        let task = tasks.get(id).unwrap();
        assert_eq!(task.anchor.revision, 2);
        assert_eq!(
            task.resume.verification.state,
            VerificationState::Stale,
            "a goal change must stale the dependent verification"
        );
        assert_eq!(
            task.resume.verification.cause,
            VerificationCause::SpecChanged
        );
    }

    #[test]
    fn whole_anchor_replace_matches_authority_semantics_of_patches() {
        use crate::execution::VerificationState;

        let mut tasks = TaskManager::new();
        let id = create(&mut tasks, "task A");
        let test = agent_contracts::ToolOutput {
            call_id: "c".into(),
            tool_name: "shell.exec".into(),
            ok: true,
            summary: "ok".into(),
            model_content: "exit 0".into(),
            artifact_ref: None,
            metadata: serde_json::json!({"command": "cargo test", "verification": true}),
        };
        tasks.observe_tool(&test, 1);
        assert_eq!(
            tasks.get(id).unwrap().resume.verification.state,
            VerificationState::Current
        );

        // Replace with an anchor whose authority fields are identical:
        // only runtime fields move, so verification stays Current.
        let mut evolved = evolved_anchor();
        evolved.original_goal = "task A".into();
        evolved.constraints.clear();
        evolved.acceptance_criteria.clear();
        let (txn, revision, changed) = tasks
            .prepare_replace_anchor(id, 0, evolved)
            .expect("initial CAS is valid");
        assert!(
            !changed.iter().any(|field| {
                field == "original_goal" || field == "constraints" || field == "acceptance_criteria"
            }),
            "fixture must not touch authority fields, got {changed:?}"
        );
        tasks.commit(txn);
        let task = tasks.get(id).unwrap();
        assert_eq!(task.anchor.revision, revision);
        assert_eq!(
            task.resume.verification.state,
            VerificationState::Current,
            "authority-preserving replace must keep the verifier Current"
        );

        // Replacing across a goal change stales it.
        let mut moved = tasks.get(id).unwrap().anchor.clone();
        moved.original_goal = "task B".into();
        let (txn, _, _) = tasks
            .prepare_replace_anchor(id, revision, moved)
            .expect("revision matches");
        tasks.commit(txn);
        assert_eq!(
            tasks.get(id).unwrap().resume.verification.state,
            VerificationState::Stale,
            "an authority-moving replace must stale the verifier"
        );
    }

    #[test]
    fn verification_basis_consumers_agree_through_progress_boundary_and_restore() {
        use crate::checkpoint::{TaskManagerSnapshot, TaskRecordSnapshot};
        use crate::execution::{
            ExecutionState, ResourceFact, ResourceProvenance, RuntimeExecutionAttribution,
            VerificationState,
        };
        use crate::opportunity::derive_completion_opportunity;
        use agent_contracts::{
            ResourceFreshness, ToolExecutionAttribution, ToolExecutionPurpose, VerificationReuse,
        };
        use serde_json::json;

        let mut tasks = TaskManager::new();
        let id = create(&mut tasks, "goal");

        // One committed world: a durable mutation stamp plus a trusted
        // exact-verification PASS. The whole-record CAS revision and the
        // verification basis start at 0 and stay bound together.
        let mut resume = ExecutionState {
            directive_revision: 1,
            workspace_revision: 1,
            ..ExecutionState::default()
        };
        resume.checked_files.push(ResourceFact {
            path: "src/lib.rs".into(),
            digest: "deadbeef".into(),
            freshness: ResourceFreshness::Fresh,
            turn: 1,
            provenance: ResourceProvenance::MutationResult,
        });
        let exact = RuntimeExecutionAttribution {
            host: ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                Vec::<String>::new(),
                VerificationReuse::ExactCurrentWorld,
            )
            .with_verification_identity_material("test-runner:v2|env:win"),
            rooted_targets: Vec::new(),
        };
        let verify = agent_contracts::ToolOutput {
            call_id: "v".into(),
            tool_name: "test.verify".into(),
            ok: true,
            summary: "ok".into(),
            model_content: "tests passed".into(),
            artifact_ref: None,
            metadata: json!({"verification": true}),
        };
        resume.observe_tool_attributed(&verify, 0, 1, "arg-verify", &exact);
        tasks.install_resume(id, resume);
        assert_eq!(
            tasks.get(id).unwrap().resume.validity(),
            VerificationState::Current
        );

        // Progress-only CAS: the record revision advances, the verification
        // basis does not, and every consumer agrees the PASS is still
        // current — ActiveTurn validity, completion, exact reuse and the
        // derived closure opportunity all accept the same fact.
        let (txn, revision, _, kind) = tasks
            .prepare_patch_anchor(
                id,
                0,
                &AnchorPatch {
                    next_action: Some("continue".into()),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        assert_eq!(kind, AnchorPatchKind::Autonomous);
        tasks.commit(txn);
        let task = tasks.get(id).unwrap();
        assert_eq!(task.anchor.revision, revision);
        assert_eq!(task.anchor.verification_revision, 0);
        assert_eq!(task.resume.verification.spec_revision, 0);
        assert_eq!(task.resume.validity(), VerificationState::Current);
        let basis = VerificationBasis {
            task_id: id,
            verification_revision: task.anchor.verification_revision,
            directive_revision: task.resume.directive_revision,
            workspace_revision: task.resume.workspace_revision,
        };
        assert!(
            matches!(
                completion_evidence(&task.resume, Some(&basis)).0,
                CompletionVerificationStatus::Current
            ),
            "progress-only CAS must keep completion verification Current"
        );
        assert!(
            task.resume
                .current_exact_verification_pass("test.verify", "arg-verify", 0, &exact)
                .is_some(),
            "progress-only CAS must keep exact reuse on the same basis"
        );
        let decision =
            derive_completion_opportunity(id, &task.anchor, &task.resume, false, false, false);
        assert!(
            decision.ready.is_some(),
            "progress-only CAS must keep the derived opportunity eligible"
        );

        // A trusted host acceptance-criteria change moves the proof basis.
        // Every consumer agrees the old PASS is stale; model-routable
        // `task.manage` cannot submit this field, so no approval round is
        // required merely to ingest the host declaration.
        let (txn, revision, _, kind) = tasks
            .prepare_patch_anchor(
                id,
                1,
                &AnchorPatch {
                    acceptance_criteria: Some(vec!["api unchanged".into()]),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        assert_eq!(
            kind,
            AnchorPatchKind::Autonomous,
            "host completion authority is not a user goal/constraint boundary"
        );
        tasks.commit(txn);
        let task = tasks.get(id).unwrap();
        assert_eq!(task.anchor.revision, revision);
        assert_eq!(task.anchor.verification_revision, 1);
        assert_eq!(task.resume.verification.spec_revision, 1);
        assert_eq!(task.resume.validity(), VerificationState::Stale);
        let basis = VerificationBasis {
            task_id: id,
            verification_revision: task.anchor.verification_revision,
            directive_revision: task.resume.directive_revision,
            workspace_revision: task.resume.workspace_revision,
        };
        assert!(
            matches!(
                completion_evidence(&task.resume, Some(&basis)).0,
                CompletionVerificationStatus::Unverified
            ),
            "a moved basis must downgrade completion verification"
        );
        assert!(
            task.resume
                .current_exact_verification_pass("test.verify", "arg-verify", 1, &exact)
                .is_none(),
            "a moved basis must refuse exact reuse of the old PASS"
        );
        let decision =
            derive_completion_opportunity(id, &task.anchor, &task.resume, false, false, false);
        assert!(
            decision.ready.is_none(),
            "a moved basis must block the derived opportunity"
        );

        // Checkpoint round-trip: the anchor/revision binding survives
        // serialization and restore, and the restored consumers agree with
        // the live ones.
        let task = tasks.get(id).unwrap();
        let snapshot = TaskManagerSnapshot {
            tasks: vec![TaskRecordSnapshot {
                id,
                goal: task.goal.clone(),
                status: TaskStatus::Active,
                created_at_ms: 0,
                last_active_ms: 0,
                tool_requirements: task.tool_requirements.clone(),
                anchor: task.anchor.clone(),
                resume: task.resume.clone(),
                turn_intent: String::new(),
            }],
            active: Some(id),
            completed: Vec::new(),
        };
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let mut restored = TaskManager::new();
        restored.restore(serde_json::from_str(&encoded).unwrap());
        let task = restored.get(id).unwrap();
        assert_eq!(task.anchor.verification_revision, 1);
        assert_eq!(task.resume.verification.spec_revision, 1);
        assert_eq!(task.resume.validity(), VerificationState::Stale);
        assert!(
            task.resume
                .current_exact_verification_pass("test.verify", "arg-verify", 1, &exact)
                .is_none()
        );
        let decision =
            derive_completion_opportunity(id, &task.anchor, &task.resume, false, false, false);
        assert!(decision.ready.is_none());
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
            .prepare_complete(CompletionRecordDraft {
                summary: "done".into(),
                ..CompletionRecordDraft::default()
            })
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
            verification_revision: 0,
            original_goal: "task A".into(),
            current_interpretation: "refactor the auth module".into(),
            constraints: vec!["no dependency changes".into()],
            completion_policy: TaskCompletionPolicy::OperatorClosureOnly,
            acceptance_criteria: vec!["tests pass".into(), "api unchanged".into()],
            acceptance_coverage: Vec::new(),
            plan_progress: vec!["read the module".into()],
            open_loops: vec!["verify edge cases".into()],
            next_action: "patch the second caller".into(),
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
                "next_action",
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
    fn generic_anchor_updates_cannot_mint_receipts_and_boundary_change_clears_them() {
        let mut tasks = TaskManager::new();
        let task_id = create(&mut tasks, "task A");
        let (authority, revision, _, _) = tasks
            .prepare_patch_anchor(
                task_id,
                0,
                &AnchorPatch {
                    completion_policy: Some(TaskCompletionPolicy::EvidenceRequired),
                    acceptance_criteria: Some(vec![AcceptanceCriterion::new(
                        "workspace tests pass",
                        "workspace-tests",
                    )]),
                    ..AnchorPatch::default()
                },
            )
            .unwrap();
        tasks.commit(authority);
        assert_eq!(revision, 1);
        let receipt = AcceptanceCoverage {
            task_id: Some(task_id),
            verification_revision: 1,
            criterion_index: 0,
            coverage_domain: "workspace-tests".into(),
            domain_declaration_revision: 1,
            domain_source_digest: agent_contracts::ContentDigest::sha256_bytes(
                b"workspace-tests/source",
            )
            .to_string(),
            directive_revision: 1,
            workspace_revision: 2,
            verification_identity: agent_contracts::ContentDigest::sha256_bytes(b"verifier")
                .to_string(),
        };
        let (record, receipt_revision, _) = tasks
            .prepare_record_acceptance_receipts(task_id, revision, vec![receipt.clone()])
            .unwrap();
        tasks.commit(record);
        assert_eq!(receipt_revision, 2);

        let mut fabricated = tasks.get(task_id).unwrap().anchor.clone();
        fabricated.acceptance_coverage[0].workspace_revision += 1;
        assert!(matches!(
            tasks.prepare_replace_anchor(task_id, receipt_revision, fabricated),
            Err(AgentError::InvalidRequest(_))
        ));
        assert!(matches!(
            tasks.prepare_patch_anchor(
                task_id,
                receipt_revision,
                &AnchorPatch {
                    acceptance_coverage: Some(vec![receipt]),
                    ..AnchorPatch::default()
                }
            ),
            Err(AgentError::InvalidRequest(_))
        ));

        // A legitimate authority replacement need not echo Runtime-owned
        // receipts. Empty input is accepted and the old proof is cleared.
        let mut changed = tasks.get(task_id).unwrap().anchor.clone();
        changed.acceptance_criteria.push(AcceptanceCriterion::new(
            "public API remains compatible",
            "api-contract",
        ));
        changed.acceptance_coverage.clear();
        let (boundary, next_revision, _) = tasks
            .prepare_replace_anchor(task_id, receipt_revision, changed)
            .unwrap();
        tasks.commit(boundary);
        let anchor = &tasks.get(task_id).unwrap().anchor;
        assert_eq!(next_revision, receipt_revision + 1);
        assert!(anchor.acceptance_coverage.is_empty());
        assert_eq!(anchor.verification_revision, 2);
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
            .prepare_complete(CompletionRecordDraft {
                summary: "done".into(),
                ..CompletionRecordDraft::default()
            })
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
            .prepare_complete(CompletionRecordDraft {
                summary: "auth refactor shipped".into(),
                ..CompletionRecordDraft::default()
            })
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

    #[test]
    fn exact_current_completion_evidence_attaches_refs_and_stale_does_not() {
        let (mut state, _) = settled_execution();
        state.verifications[0].evidence_ref = Some("artifact://v1/run/owner/digest".into());
        let basis = VerificationBasis {
            task_id: TaskId::new(),
            verification_revision: 1,
            directive_revision: 2,
            workspace_revision: 3,
        };
        let (status, refs) = completion_evidence(&state, Some(&basis));
        assert_eq!(status, CompletionVerificationStatus::Current);
        assert_eq!(refs, vec!["artifact://v1/run/owner/digest"]);

        state.verification.source_changed = true;
        let (status, refs) = completion_evidence(&state, Some(&basis));
        assert_eq!(status, CompletionVerificationStatus::Unverified);
        assert!(refs.is_empty());
    }

    #[test]
    fn evidence_snapshot_never_acts_as_a_completion_gate() {
        let state = crate::execution::ExecutionState::default();
        assert_eq!(
            completion_evidence(&state, None),
            (CompletionVerificationStatus::Unverified, Vec::new())
        );
    }

    /// An execution whose fact ledger looks exactly like a current trusted
    /// verification world: one durable mutation and one exact attributed
    /// PASS whose basis tuple matches every revision. Returns the identity
    /// the anchor must claim.
    fn test_declaration() -> agent_contracts::VerificationCoverageDeclaration {
        agent_contracts::VerificationCoverageDeclaration {
            domain_id: "workspace-tests".into(),
            declaration_revision: 1,
            source_digest: agent_contracts::ContentDigest::sha256_bytes(b"workspace-tests/v1")
                .to_string(),
        }
    }

    fn settled_execution() -> (crate::execution::ExecutionState, String) {
        let identity = agent_contracts::ContentDigest::sha256_bytes(b"test verifier").to_string();
        let mut execution = crate::execution::ExecutionState {
            anchor_revision: 1,
            directive_revision: 2,
            workspace_revision: 3,
            ..crate::execution::ExecutionState::default()
        };
        execution.verification.spec_revision = 1;
        execution
            .checked_files
            .push(crate::execution::ResourceFact {
                path: "src/lib.rs".into(),
                digest: "deadbeef".into(),
                freshness: agent_contracts::ResourceFreshness::Fresh,
                turn: 1,
                provenance: crate::execution::ResourceProvenance::MutationResult,
            });
        execution
            .verifications
            .push(crate::execution::VerificationFact {
                summary: "tests pass".into(),
                ok: true,
                turn: 1,
                anchor_revision: 1,
                workspace_revision: 3,
                source_tool_name: "test.verify".into(),
                argument_digest: "arg-a".into(),
                verification_identity: identity.clone(),
                directive_revision: 2,
                evidence_ref: None,
                recipe_provenance: Some(agent_contracts::VerificationRecipeProvenance {
                    recipe_id: "test.verify".into(),
                    recipe_revision: "v1".into(),
                    coverage_domain: Some("workspace-tests".into()),
                    domain_declaration_revision: Some(1),
                    domain_source_digest: test_declaration().source_digest,
                    class_identity_digest: "class-a".into(),
                }),
            });
        (execution, identity)
    }

    fn settled_anchor(revision: u64) -> TaskAnchor {
        let identity = agent_contracts::ContentDigest::sha256_bytes(b"test verifier").to_string();
        TaskAnchor {
            original_goal: "close the loop".into(),
            revision,
            verification_revision: 1,
            completion_policy: TaskCompletionPolicy::EvidenceRequired,
            acceptance_criteria: vec![AcceptanceCriterion::declared(
                "tests pass",
                &test_declaration(),
            )],
            acceptance_coverage: vec![AcceptanceCoverage {
                task_id: None,
                verification_revision: 1,
                criterion_index: 0,
                coverage_domain: "workspace-tests".into(),
                domain_declaration_revision: 1,
                domain_source_digest: test_declaration().source_digest,
                directive_revision: 2,
                workspace_revision: 3,
                verification_identity: identity,
            }],
            ..TaskAnchor::default()
        }
    }

    fn readiness(
        intent: CompletionIntent,
        anchor: TaskAnchor,
        execution: crate::execution::ExecutionState,
        safety: CompletionSafety,
    ) -> CompletionReadiness {
        readiness_with_declarations(intent, anchor, execution, safety, &[test_declaration()])
    }

    fn readiness_with_declarations(
        intent: CompletionIntent,
        mut anchor: TaskAnchor,
        execution: crate::execution::ExecutionState,
        safety: CompletionSafety,
        declarations: &[agent_contracts::VerificationCoverageDeclaration],
    ) -> CompletionReadiness {
        let mut tasks = TaskManager::new();
        let (txn, id) = tasks.prepare_create("close the loop");
        tasks.commit(txn);
        for receipt in &mut anchor.acceptance_coverage {
            receipt.task_id = Some(id);
        }
        let task = tasks.get_mut(id).unwrap();
        task.anchor = anchor;
        task.resume = execution;
        let task = tasks.get(id).unwrap();
        derive_completion_readiness(
            intent,
            Some(id),
            Some(task),
            Some(&task.resume),
            safety,
            declarations,
        )
    }

    #[test]
    fn completion_readiness_accepts_the_fully_settled_task() {
        let (execution, _) = settled_execution();
        let anchor = settled_anchor(1);
        let decision = readiness(
            CompletionIntent::ModelProposal,
            anchor,
            execution,
            CompletionSafety::default(),
        );
        assert!(decision.task_state_current);
        assert!(decision.commit_safe);
        assert!(decision.verified_ready);
        assert!(decision.allows_completion());
        assert_eq!(
            decision.disposition(),
            Some(CompletionDisposition::Verified)
        );
    }

    #[test]
    fn completion_readiness_fences_current_table_revision_and_source_recomposition() {
        let (execution, _) = settled_execution();
        let anchor = settled_anchor(1);
        for current in [
            agent_contracts::VerificationCoverageDeclaration {
                declaration_revision: 2,
                ..test_declaration()
            },
            agent_contracts::VerificationCoverageDeclaration {
                source_digest: agent_contracts::ContentDigest::sha256_bytes(
                    b"same-revision-different-host-table",
                )
                .to_string(),
                ..test_declaration()
            },
        ] {
            let decision = readiness_with_declarations(
                CompletionIntent::ModelProposal,
                anchor.clone(),
                execution.clone(),
                CompletionSafety::default(),
                &[current],
            );
            assert!(!decision.allows_completion());
            assert!(
                decision
                    .applicable_blockers()
                    .iter()
                    .any(|blocker| matches!(
                        blocker,
                        CompletionBlocker::AcceptanceDeclarationStale { remaining: 1 }
                    ))
            );
        }
    }

    #[test]
    fn receipt_requires_fact_provenance_to_share_the_declaration_identity() {
        let (mut execution, _) = settled_execution();
        execution.verifications[0]
            .recipe_provenance
            .as_mut()
            .unwrap()
            .domain_source_digest =
            agent_contracts::ContentDigest::sha256_bytes(b"foreign-fact-source").to_string();
        let decision = readiness(
            CompletionIntent::ModelProposal,
            settled_anchor(1),
            execution,
            CompletionSafety::default(),
        );
        assert!(!decision.allows_completion());
        assert!(
            decision
                .applicable_blockers()
                .iter()
                .any(|blocker| matches!(
                    blocker,
                    CompletionBlocker::AcceptanceUncovered { remaining: 1 }
                ))
        );
    }

    #[test]
    fn restored_anchor_cannot_follow_a_recomposed_host_table() {
        let (execution, _) = settled_execution();
        let persisted = serde_json::to_vec(&settled_anchor(1)).unwrap();
        let restored: TaskAnchor = serde_json::from_slice(&persisted).unwrap();
        let recomposed = agent_contracts::VerificationCoverageDeclaration {
            source_digest: agent_contracts::ContentDigest::sha256_bytes(
                b"recomposed-table-under-revision-one",
            )
            .to_string(),
            ..test_declaration()
        };
        assert!(
            !readiness_with_declarations(
                CompletionIntent::ModelProposal,
                restored,
                execution,
                CompletionSafety::default(),
                &[recomposed],
            )
            .allows_completion(),
            "persisted receipts must be inert under a differently composed current host table"
        );
    }

    #[test]
    fn legacy_unbound_criterion_deserializes_but_fails_closed() {
        let (execution, _) = settled_execution();
        let mut value = serde_json::to_value(settled_anchor(1)).unwrap();
        let criterion = value["acceptance_criteria"][0].as_object_mut().unwrap();
        criterion.remove("domain_declaration_revision");
        criterion.remove("domain_source_digest");
        let receipt = value["acceptance_coverage"][0].as_object_mut().unwrap();
        receipt.remove("domain_source_digest");
        let restored: TaskAnchor = serde_json::from_value(value).unwrap();
        assert_eq!(
            restored.acceptance_criteria[0].domain_declaration_revision,
            0
        );
        assert!(
            restored.acceptance_criteria[0]
                .domain_source_digest
                .is_empty()
        );
        let decision = readiness(
            CompletionIntent::ModelProposal,
            restored,
            execution,
            CompletionSafety::default(),
        );
        assert!(!decision.allows_completion());
        assert!(
            decision
                .applicable_blockers()
                .iter()
                .any(|blocker| matches!(
                    blocker,
                    CompletionBlocker::AcceptanceDeclarationStale { remaining: 1 }
                ))
        );
    }

    #[test]
    fn unbound_convenience_constructor_is_deliberately_fail_closed() {
        let (execution, _) = settled_execution();
        let mut anchor = settled_anchor(1);
        anchor.acceptance_criteria[0] =
            AcceptanceCriterion::new("tests pass", test_declaration().domain_id);
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor,
                execution,
                CompletionSafety::default(),
            )
            .allows_completion()
        );
    }

    #[test]
    fn model_proposal_fails_closed_without_declared_coverage() {
        let (execution, _) = settled_execution();
        let mut anchor = settled_anchor(1);
        anchor.acceptance_criteria.clear();
        anchor.acceptance_coverage.clear();
        anchor.completion_policy = TaskCompletionPolicy::OperatorClosureOnly;
        let decision = readiness(
            CompletionIntent::ModelProposal,
            anchor,
            execution,
            CompletionSafety::default(),
        );
        assert!(!decision.verified_ready);
        assert!(!decision.allows_completion());
        assert!(
            decision
                .applicable_blockers()
                .contains(&CompletionBlocker::AcceptanceUndeclared)
        );
        assert!(decision.refusal().to_string().contains("explicit operator"));
    }

    #[test]
    fn evidence_required_without_criteria_has_an_explicit_undeclared_blocker() {
        let (execution, _) = settled_execution();
        let mut anchor = settled_anchor(1);
        anchor.completion_policy = TaskCompletionPolicy::EvidenceRequired;
        anchor.acceptance_criteria.clear();
        anchor.acceptance_coverage.clear();
        let decision = readiness(
            CompletionIntent::ModelProposal,
            anchor,
            execution,
            CompletionSafety::default(),
        );
        assert!(!decision.allows_completion());
        assert_eq!(
            decision.applicable_blockers(),
            vec![CompletionBlocker::AcceptanceUndeclared]
        );
    }

    #[test]
    fn completion_readiness_requires_a_live_claim_for_every_criterion() {
        let (execution, _) = settled_execution();
        let mut anchor = settled_anchor(1);
        anchor
            .acceptance_criteria
            .push(AcceptanceCriterion::declared(
                "no regressions",
                &test_declaration(),
            ));
        let decision = readiness(
            CompletionIntent::ModelProposal,
            anchor.clone(),
            execution.clone(),
            CompletionSafety::default(),
        );
        assert!(!decision.allows_completion());
        anchor.acceptance_coverage.push(AcceptanceCoverage {
            task_id: None,
            verification_revision: 1,
            criterion_index: 1,
            coverage_domain: "workspace-tests".into(),
            domain_declaration_revision: 1,
            domain_source_digest: test_declaration().source_digest,
            directive_revision: 2,
            workspace_revision: 3,
            verification_identity: agent_contracts::ContentDigest::sha256_bytes(b"test verifier")
                .to_string(),
        });
        assert!(
            readiness(
                CompletionIntent::ModelProposal,
                anchor,
                execution,
                CompletionSafety::default(),
            )
            .allows_completion()
        );
    }

    #[test]
    fn completion_readiness_rejects_stale_or_fabricated_identity() {
        let (execution, _) = settled_execution();
        let mut anchor = settled_anchor(1);
        anchor.acceptance_coverage[0].verification_identity = "other-identity".into();
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor.clone(),
                execution.clone(),
                CompletionSafety::default(),
            )
            .allows_completion(),
            "a claim that does not resolve to the current trusted pass covers nothing"
        );

        // A new directive moves the world: the old PASS no longer binds.
        anchor.acceptance_coverage[0].verification_identity = "identity-a".into();
        let mut moved = execution.clone();
        moved.directive_revision = 3;
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor,
                moved,
                CompletionSafety::default(),
            )
            .allows_completion(),
            "a new directive must invalidate readiness immediately"
        );
    }

    #[test]
    fn completion_readiness_rejects_open_loops_and_next_action() {
        let (execution, _) = settled_execution();
        let mut anchor = settled_anchor(1);
        anchor.open_loops.push("verify edge cases".into());
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor.clone(),
                execution.clone(),
                CompletionSafety::default(),
            )
            .allows_completion()
        );
        anchor.open_loops.clear();
        anchor.next_action = "write the missing test".into();
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor,
                execution,
                CompletionSafety::default(),
            )
            .allows_completion()
        );
    }

    #[test]
    fn task_state_and_verification_bases_move_independently() {
        let (mut execution, _) = settled_execution();
        let anchor = settled_anchor(2);
        execution.anchor_revision = 2;
        let decision = readiness(
            CompletionIntent::ModelProposal,
            anchor.clone(),
            execution.clone(),
            CompletionSafety::default(),
        );
        assert!(
            decision.allows_completion(),
            "progress-only task CAS must preserve verification basis"
        );

        execution.anchor_revision = 1;
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor,
                execution,
                CompletionSafety::default(),
            )
            .commit_safe,
            "stale task-state basis blocks every intent"
        );
    }

    #[test]
    fn completion_readiness_rejects_open_obligation_and_failed_command() {
        let (execution, _) = settled_execution();
        let anchor = settled_anchor(1);
        let mut with_obligation = execution.clone();
        with_obligation
            .obligations
            .push(crate::execution::ExecutionObligation {
                domain: agent_contracts::ToolFailureDomain::ExecutableResolution,
                scope_key: "scope-a".into(),
                precondition: "fp-1".into(),
                attempts: 1,
                epoch: 0,
                total_attempts: 1,
                tried_targets: Vec::new(),
                opened_at_evidence_revision: 1,
                source_tool_name: String::new(),
            });
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor.clone(),
                with_obligation,
                CompletionSafety::default(),
            )
            .allows_completion()
        );

        let mut with_failure = execution.clone();
        with_failure
            .failed_commands
            .push(crate::execution::FailedCommandFact {
                tool_name: "test.run".into(),
                target: "pkg".into(),
                summary: "crash".into(),
                turn: 1,
                ..crate::execution::FailedCommandFact::default()
            });
        assert!(
            !readiness(
                CompletionIntent::ModelProposal,
                anchor,
                with_failure,
                CompletionSafety::default(),
            )
            .allows_completion()
        );

        let mut with_overflow = execution;
        with_overflow.failure_overflow = crate::execution::UnresolvedFailureOverflow {
            directive_revision: with_overflow.directive_revision,
            omitted_obligations: 3,
            omitted_failed_commands: 4,
        };
        let decision = readiness(
            CompletionIntent::ModelProposal,
            settled_anchor(1),
            with_overflow.clone(),
            CompletionSafety::default(),
        );
        assert!(!decision.allows_completion());
        assert!(
            decision
                .applicable_blockers()
                .contains(&CompletionBlocker::ExecutionObligations { remaining: 3 })
        );
        assert!(
            decision
                .applicable_blockers()
                .contains(&CompletionBlocker::FailedCommands { remaining: 4 })
        );

        let operator = readiness(
            CompletionIntent::ExplicitOperator,
            settled_anchor(1),
            with_overflow,
            CompletionSafety::default(),
        );
        assert!(
            operator.allows_completion(),
            "only explicit operator authority may override opaque overflow debt"
        );
        assert!(
            operator
                .override_reasons()
                .contains(&CompletionBlocker::ExecutionObligations { remaining: 3 })
        );
    }

    #[test]
    fn explicit_operator_bypasses_semantic_blockers_but_not_commit_safety() {
        let (execution, _) = settled_execution();
        let mut anchor = settled_anchor(1);
        anchor.open_loops.push("operator accepted risk".into());
        let override_decision = readiness(
            CompletionIntent::ExplicitOperator,
            anchor.clone(),
            execution.clone(),
            CompletionSafety::default(),
        );
        assert!(override_decision.allows_completion());
        assert!(!override_decision.verified_ready);
        assert_eq!(
            override_decision.disposition(),
            Some(CompletionDisposition::OperatorOverride)
        );
        assert_eq!(
            override_decision.override_reasons(),
            vec![CompletionBlocker::OpenLoops { remaining: 1 }]
        );

        let fenced = readiness(
            CompletionIntent::ExplicitOperator,
            anchor,
            execution,
            CompletionSafety {
                recovery_required: true,
                ..CompletionSafety::default()
            },
        );
        assert!(!fenced.allows_completion());
        assert!(!fenced.commit_safe);
    }

    #[test]
    fn latest_failure_or_source_change_cannot_reanimate_an_old_pass() {
        let (mut execution, _) = settled_execution();
        let anchor = settled_anchor(1);
        execution.verification.source_changed = true;
        let decision = readiness(
            CompletionIntent::ModelProposal,
            anchor,
            execution,
            CompletionSafety::default(),
        );
        assert!(!decision.verified_ready);
        assert!(!decision.allows_completion());
    }
}
