//! Resource facts, verification facts, and failures. Checkpointable.

use std::path::Path;

use agent_contracts::{
    ArtifactLocator, ContentDigest, EvidenceValidity, ExecutionEvidence, FrontierDelta,
    MAX_FOREGROUND_RESOURCES, MAX_TASK_ANCHOR_ITEM_CHARS, NegativeFactEventKind, ResourceFreshness,
    ResourceKey, TaskProgressView, ToolExecutionAttribution, ToolExecutionPurpose,
    ToolFailureClass, ToolFailureDomain, ToolOutput, VerificationPassEventKind,
    path_exactly_in_directive,
};
#[cfg(test)]
use agent_contracts::{ToolResultDisposition, TurnFrame, TurnFrameStep};

pub(crate) const MAX_RESUME_FILES: usize = 32;
pub(super) const MAX_RESUME_FAILURES: usize = 8;
pub(super) const MAX_REVALIDATE_PER_ROUND: usize = 8;
pub(super) const MAX_COVERAGE_PATHS: usize = 8;
/// Consecutive identical no-progress rounds before the runtime tells the
/// model its repeated behavior is not moving the world.
pub(super) const STALL_THRESHOLD: u32 = 3;
/// 连续同类失败命中的不同目标数达到该值即上报聚类提示：换拼写的
/// 连击在任一单独签名上永远到不了 [`STALL_THRESHOLD`]。
pub(super) const STALL_CLUSTER_DISTINCT_TARGETS: u32 = 2;
/// 连续无前沿推进的动作数达到该值即给收敛 advisory（软提示，不阻断）。
pub(super) const FRONTIER_ADVISORY_THRESHOLD: u32 = 5;
/// 前沿证据行数上限（最新在前）。
const MAX_EVIDENCE_ROWS: usize = 16;
/// recent_deltas 环形缓冲长度上限。
const MAX_RECENT_DELTAS: usize = 8;
/// 单条证据 outcome / 参数摘要的字符上限。
const EVIDENCE_TEXT_CHARS: usize = 80;
/// 义务账本上限（最旧淘汰）与单义务目标数上限。
pub(super) const MAX_OBLIGATIONS: usize = 8;
const MAX_OBLIGATION_TARGETS: usize = 8;
/// Speculative negative path observations and trusted verifier sources are
/// operational identities, never transcript history. Both tables are small
/// and checkpoint-validated.
pub(super) const MAX_NEGATIVE_FACTS: usize = 8;
pub(super) const MAX_VERIFICATION_SOURCES: usize = 4;

/// 一轮工具观察的确定性前沿分类结果，随 `ExecutionFrontier` 事件上报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierObservation {
    pub delta: FrontierDelta,
    pub actions_since_frontier_advance: u32,
    pub evidence_revision: u64,
    pub invalidated: u64,
    /// 本次观察产生的义务账目事件（有界）。
    pub obligation_events: Vec<agent_contracts::ExecutionObligationEvent>,
    /// Bounded lifecycle transitions for speculative negative facts.
    pub negative_fact_events: Vec<NegativeFactTransition>,
    /// Body-free lifecycle transitions for exact verification PASS receipts.
    pub verification_pass_events: Vec<VerificationPassTransition>,
}

/// [`ExecutionState::record_observation_evidence`] 的三值结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObservationEvidence {
    /// 新证据或证据内容变化。
    Advanced,
    /// A dormant row became current again with the same arguments and
    /// semantic outcome. Currentness improved; knowledge did not.
    Reconfirmed,
    /// 同 key 同 validity 同结果的重复观察。
    Repeated,
    /// 该输出不产生前沿证据（无可键化路径且非命令成功）。
    None,
}

/// Semantic effect of stamping one resource identity. Returning from
/// NeedsRevalidation to the same digest repairs currentness but is not a new
/// world fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResourceObservation {
    None,
    Reconfirmed,
    Advanced,
}

impl ResourceObservation {
    pub(super) fn merge(&mut self, other: Self) {
        if matches!(other, Self::Advanced)
            || matches!(other, Self::Reconfirmed) && matches!(self, Self::None)
        {
            *self = other;
        }
    }
}

/// Consecutive no-progress counter for one operation signature
/// (tool + target + failure class). Reset by any progress or by a
/// signature change.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StallState {
    #[serde(default)]
    pub consecutive_no_progress: u32,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub failure: Option<ToolFailureClass>,
}

/// 同类失败跨不同目标的连击：逐签名的计数器看不见"每次换一个拼写"
/// 的虚构路径连击。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FailureCluster {
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub failure: Option<ToolFailureClass>,
    /// 本轮聚类里真实尝试过的目标（有界去重，按首次顺序）。
    /// 数量取 len；A→B→A 只算两个，不再按变化次数虚增。
    #[serde(default)]
    pub tried_targets: Vec<String>,
}

/// Bounded operational cache bound to `task_id + anchor_revision +
/// workspace_revision`. Serialized as `resume` on `TaskRecord`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecutionState {
    pub anchor_revision: u64,
    /// Monotonic user-directive clock. Exact PASS reuse is deliberately
    /// limited to one directive so an explicit later request to rerun a
    /// verifier is never silently served by an older result.
    #[serde(default)]
    pub directive_revision: u64,
    /// Whether the current directive already has an exact task-rooted
    /// resource identity. Once true, unrelated catalog/workspace discoveries
    /// remain available as evidence but no longer reset the task-convergence
    /// frontier merely because their evidence key is novel.
    #[serde(default)]
    pub directive_has_rooted_evidence: bool,
    /// Monotonic world clock. Bumped on any may-mutate observation.
    /// Verification facts bind to this value and do not auto-promote.
    #[serde(default)]
    pub workspace_revision: u64,
    pub checked_files: Vec<ResourceFact>,
    pub verifications: Vec<VerificationFact>,
    pub failed_commands: Vec<FailedCommandFact>,
    #[serde(default)]
    pub verification: VerificationObligation,
    #[serde(default)]
    pub stall: StallState,
    #[serde(default)]
    pub failure_cluster: FailureCluster,
    /// 证据前沿：成功只读观察与成功命令运行的有界记录（最新在前）。
    /// 只存身份/结果/版本，不存正文。
    #[serde(default)]
    pub evidence: Vec<ExecutionEvidence>,
    /// 收敛账目：无推进动作连击、证据版本与最近 delta 环形缓冲。
    #[serde(default)]
    pub convergence: ConvergenceState,
    /// 最近一次观察的模型轮号，Turn 有效性的判定基准。
    #[serde(default)]
    pub last_turn: u64,
    /// 义务账本：只能由 typed 失效事实产生；无关推进不清除，
    /// 仅前置变化或同类成功解除。
    #[serde(default)]
    pub obligations: Vec<ExecutionObligation>,
    /// Trusted path misses that were speculative rather than rooted in task
    /// authority. They are bound to `workspace_revision`, retain no body,
    /// and may be reused only after a live Workspace absence check.
    #[serde(default)]
    pub negative_facts: Vec<NegativeExecutionFact>,
    /// Exact host-attributed verifier identities for this task anchor. They
    /// become schema roots only while verification is due.
    #[serde(default)]
    pub verification_sources: Vec<VerificationSourceLease>,
    /// LONG-TASK advisory (default off upstream): the last actually offered
    /// completion-opportunity key. Body-free and bounded, so unchanged reads
    /// and progress-only anchor edits cannot re-arm the same hint.
    #[serde(default)]
    pub last_offered_opportunity: Option<String>,
}

/// 一条已证明存在的执行 blocker（lineage 模型）。
/// `scope_key` 是稳定 lineage 身份（ExecutableResolution = 解析上下文
/// digest：cwd + effective PATH + 规则版本；EditTarget/ResourcePath =
/// 路径；ProjectMarker = 目标身份），跨 epoch 不变。`precondition` 是
/// 当前 epoch 的前置指纹（ExecutableResolution = resolution_fingerprint，
/// 覆盖完整有界目录状态；EditTarget = path@被拒revision；其余 = 目标
/// 路径）。世界推进只推进 epoch、不解除义务——PreconditionChanged ≠
/// ObligationResolved；只有 blocker 特定的证明（同 scope 同指纹的
/// 成功，或目标以新身份落地）才解除。`attempts` 计当前 epoch 内失败，
/// `total_attempts` 计整个 lineage 累计。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecutionObligation {
    pub domain: ToolFailureDomain,
    #[serde(default)]
    pub scope_key: String,
    pub precondition: String,
    pub attempts: u32,
    #[serde(default)]
    pub epoch: u32,
    #[serde(default)]
    pub total_attempts: u32,
    pub tried_targets: Vec<String>,
    #[serde(default)]
    pub opened_at_evidence_revision: u64,
    /// The exact tool whose failed execution opened this obligation, stamped
    /// by the trusted recording path from pre-dispatch truth. Empty on rows
    /// that predate the field; drives obligation-scoped lease provenance.
    #[serde(default)]
    pub source_tool_name: String,
}

impl ExecutionObligation {
    /// 旧 checkpoint 行没有 total_attempts（反序列化为 0）；显示取两者
    /// 较大值即可兼容，无需迁移。
    pub fn effective_total(&self) -> u32 {
        self.total_attempts.max(self.attempts)
    }
}

/// 收敛状态。`evidence_revision` 在前沿内容变化（新证据/失效）时单调
/// 递增；`actions_since_frontier_advance` 只被可证明推进的 delta 清零。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConvergenceState {
    #[serde(default)]
    pub evidence_revision: u64,
    #[serde(default)]
    pub actions_since_frontier_advance: u32,
    /// 最近 delta，最旧在前，有界环形。
    #[serde(default)]
    pub recent_deltas: Vec<FrontierDelta>,
}

/// How one [`ResourceFact`] was last observed. Observability only: it
/// never gates freshness, authority, or selection — it explains why the
/// fact is believed (effect, observation, and attention are
/// separate truths).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceProvenance {
    /// Observed by a successful read-only tool (or runtime revalidation).
    #[default]
    Read,
    /// Observed by a successful mutation's own result stamp.
    MutationResult,
    /// Observed by a refused mutation: the tool read the target to
    /// refuse it, so the stamped path+revision is trusted world truth
    /// even though the write did not apply.
    MutationRefusal,
    /// Observed by a verification result.
    Verification,
}

/// One bounded operational fact about a workspace path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResourceFact {
    pub path: String,
    pub digest: String,
    #[serde(default)]
    pub freshness: ResourceFreshness,
    pub turn: u64,
    #[serde(default)]
    pub provenance: ResourceProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VerificationObligation {
    #[serde(default)]
    pub spec_revision: u64,
    #[serde(default)]
    pub state: VerificationState,
    /// Why this obligation exists. Independent of whether it is due now.
    #[serde(default)]
    pub cause: VerificationCause,
    #[serde(default)]
    pub coverage: VerificationCoverage,
    /// A Known digest change since the last successful verification.
    /// Persists across user turns; `verification_due_now` decides whether
    /// to surface Verify this round.
    #[serde(default)]
    pub source_changed: bool,
    /// An Unknown footprint is awaiting identity revalidation. PASS is
    /// already omitted via `workspace_revision`.
    #[serde(default)]
    pub unknown_pending: bool,
    /// Typed verification failed and has not been succeeded since.
    /// Survives user turns ("still fixing").
    #[serde(default)]
    pub failed_open: bool,
    /// When true, completion is refused unless [`ExecutionState::validity`]
    /// is [`VerificationState::Current`]. Default false: do not force a
    /// test run on every task.
    #[serde(default)]
    pub required_for_completion: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    #[default]
    NotRun,
    Pending,
    Current,
    Stale,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCause {
    #[default]
    None,
    SourceChanged,
    UserRequested,
    FailureRepair,
    AcceptanceGate,
    SpecChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCoverage {
    #[default]
    Unspecified,
    Workspace,
    Resources(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VerificationFact {
    pub summary: String,
    pub ok: bool,
    pub turn: u64,
    #[serde(default)]
    pub anchor_revision: u64,
    #[serde(default)]
    pub workspace_revision: u64,
    /// Exact verifier provenance is populated only from trusted pre-dispatch
    /// attribution. Empty fields keep legacy/non-exact evidence fail-closed.
    #[serde(default)]
    pub source_tool_name: String,
    #[serde(default)]
    pub argument_digest: String,
    #[serde(default)]
    pub verification_identity: String,
    #[serde(default)]
    pub directive_revision: u64,
    /// Artifact locator of the verification output, when the tool retained one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    /// Host-resolved provenance of the exact recipe that produced this PASS,
    /// when the dispatcher captured one. Legacy and non-exact facts keep
    /// `None` and never join domain-equivalent reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe_provenance: Option<agent_contracts::VerificationRecipeProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FailedCommandFact {
    pub tool_name: String,
    #[serde(default)]
    pub target: String,
    pub summary: String,
    pub turn: u64,
}

/// One speculative, revision-bound negative path observation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NegativeExecutionFact {
    pub tool_name: String,
    pub target: String,
    pub argument_digest: String,
    pub failure: ToolFailureClass,
    pub workspace_revision: u64,
    pub turn: u64,
}

/// Provenance lease for a verifier explicitly attributed by the trusted host
/// before dispatch. Workspace changes do not erase the source (they are why a
/// rerun becomes due); an anchor revision change does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VerificationSourceLease {
    pub tool_name: String,
    pub argument_digest: String,
    pub anchor_revision: u64,
}

/// Runtime combines host attribution with current task authority. The host
/// names what a call does; only Runtime can say which targets are task-rooted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeExecutionAttribution {
    pub host: ToolExecutionAttribution,
    pub rooted_targets: Vec<String>,
}

impl RuntimeExecutionAttribution {
    pub fn target_is_rooted(&self, target: &str) -> bool {
        let target = agent_contracts::normalize_resource_path(target);
        self.rooted_targets.iter().any(|rooted| rooted == &target)
    }

    pub fn reusable_verification(&self) -> bool {
        self.host.reusable_verification()
    }

    pub fn exact_verification_identity(&self) -> Option<&str> {
        self.host.exact_verification_identity()
    }

    pub fn verification_recipe(&self) -> Option<&agent_contracts::VerificationRecipeProvenance> {
        self.host.verification_recipe()
    }
}

/// Body-free transition emitted through [`FrontierObservation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegativeFactTransition {
    pub kind: NegativeFactEventKind,
    pub tool_name: String,
    pub target: String,
    pub failure: ToolFailureClass,
    pub workspace_revision: u64,
}

/// Body-free transition for an exact verification PASS receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationPassTransition {
    pub kind: VerificationPassEventKind,
    pub tool_name: String,
    pub argument_digest: String,
    pub verification_identity: String,
    pub anchor_revision: u64,
    pub directive_revision: u64,
    pub workspace_revision: u64,
}

pub(super) struct OperationIdentity {
    pub tool_name: String,
    pub target: String,
}

impl ExecutionState {
    /// New user directive: TurnIntent is replaced by the caller. Verification
    /// obligation is not wiped here — whether Verify is due this round is
    /// [`Self::verification_due_now`].
    pub fn on_user_turn(&mut self, message: &str) {
        self.directive_revision = self.directive_revision.saturating_add(1);
        self.directive_has_rooted_evidence = self.checked_files.iter().any(|row| {
            row.freshness == ResourceFreshness::Fresh
                && path_exactly_in_directive(message, &row.path)
        });
    }

    /// Persistent obligation: something still needs a verification, even if
    /// this round should not Prefer-verify. Derived from
    /// [`Self::validity`], not from independently toggled bools.
    pub fn has_unmet_obligation(&self) -> bool {
        matches!(
            self.validity(),
            VerificationState::Pending | VerificationState::Stale | VerificationState::Failed
        ) || self.last_evidence().is_some_and(|row| !row.ok)
    }

    /// How many failure-obligation rows are still open. The ledger only
    /// drains through typed resolution evidence, so any positive count
    /// blocks a successful completion gate.
    pub fn open_obligation_count(&self) -> usize {
        self.obligations.len()
    }

    /// Latest verification evidence, including epoch-stale rows.
    /// `workspace_revision` omits old PASS from the prompt view; validity
    /// still needs the last result.
    pub(crate) fn last_evidence(&self) -> Option<&VerificationFact> {
        self.verifications.last()
    }

    /// Derived validity. Stored `verification.state` is refreshed after
    /// mutations so checkpoints stay consistent with this function.
    pub fn validity(&self) -> VerificationState {
        if self.verification.failed_open || self.last_evidence().is_some_and(|ev| !ev.ok) {
            return VerificationState::Failed;
        }
        let Some(last) = self.last_evidence() else {
            return if self.verification.cause != VerificationCause::None
                || self.verification.required_for_completion
            {
                VerificationState::Pending
            } else {
                VerificationState::NotRun
            };
        };
        if self.verification.source_changed
            || self.unknown_blocks_current()
            || self.verification.cause == VerificationCause::SpecChanged
            || last.anchor_revision != self.verification.spec_revision
        {
            // The last row is bound to a different verification basis than
            // the current one. Authority movement is normally surfaced
            // through `SpecChanged`, but this binding check keeps every
            // consumer honest if the basis moves without that side effect.
            return VerificationState::Stale;
        }
        VerificationState::Current
    }

    fn unknown_blocks_current(&self) -> bool {
        if !self.verification.unknown_pending {
            return false;
        }
        match &self.verification.coverage {
            VerificationCoverage::Resources(paths) if !paths.is_empty() => {
                !self.covered_resources_identity_confirmed(paths)
            }
            _ => true,
        }
    }

    pub(super) fn covered_resources_identity_confirmed(&self, paths: &[String]) -> bool {
        !self.verification.source_changed
            && paths.iter().all(|path| {
                self.checked_files
                    .iter()
                    .any(|fact| fact.path == *path && fact.freshness == ResourceFreshness::Fresh)
            })
    }

    pub(super) fn refresh_validity(&mut self) {
        self.verification.state = self.validity();
    }

    /// Record the last actually offered completion-opportunity key. The
    /// key is body-free and hard-bounded; recording an offer never touches
    /// verification state.
    pub fn record_opportunity_offer(&mut self, key: String) {
        let mut key = key;
        if key.chars().count() > crate::opportunity::MAX_OPPORTUNITY_KEY_CHARS {
            key = key
                .chars()
                .take(crate::opportunity::MAX_OPPORTUNITY_KEY_CHARS)
                .collect();
        }
        self.last_offered_opportunity = Some(key);
    }

    /// Conservative user-turn signal that this instruction is asking to
    /// verify. Diagnostic-only: four frozen needles, not a dictionary.
    /// Reliable Verify triggers are typed failed verification, a persistent
    /// unmet obligation, the completion gate, or an explicit tool/user
    /// control signal. Do not grow this into
    /// verify/check/test/confirm/validate/ensure.
    pub fn turn_requests_verify(message: &str) -> bool {
        let lower = message.to_ascii_lowercase();
        lower.contains("run the tests")
            || lower.contains("run tests")
            || lower.contains("verify that")
            || lower.contains("check that tests")
    }

    pub(crate) fn turn_requests_complete(message: &str) -> bool {
        let lower = message.to_ascii_lowercase();
        lower.contains("task.complete")
            || lower.contains("complete the task")
            || lower.contains("mark this done")
    }

    /// Prefer verification tools only when an obligation is actually due
    /// for this turn. Epoch-stale PASS (Unknown `__pycache__`) is not due
    /// once identity revalidation has not shown a source change.
    /// Natural-language verify is a soft hint: it never creates an
    /// obligation and is ignored unless one already exists.
    pub fn verification_due_now(&self, turn_intent: &str) -> bool {
        if self.verification.failed_open || self.last_evidence().is_some_and(|row| !row.ok) {
            return true;
        }
        if !self.has_unmet_obligation() {
            return false;
        }
        if Self::turn_requests_complete(turn_intent) {
            return true;
        }
        if self.coverage_mentioned(turn_intent) {
            return true;
        }
        Self::turn_requests_verify(turn_intent)
    }

    fn coverage_mentioned(&self, turn_intent: &str) -> bool {
        match &self.verification.coverage {
            VerificationCoverage::Resources(paths) => paths
                .iter()
                .any(|path| path_mentioned_in_query(turn_intent, path)),
            VerificationCoverage::Workspace | VerificationCoverage::Unspecified => {
                self.verification.source_changed
                    && self
                        .checked_files
                        .iter()
                        .any(|fact| path_mentioned_in_query(turn_intent, &fact.path))
            }
        }
    }

    pub fn mark_spec_changed(&mut self) {
        if self.verification.state != VerificationState::Failed {
            self.verification.cause = VerificationCause::SpecChanged;
        }
        self.verification.coverage = VerificationCoverage::Workspace;
        self.refresh_validity();
    }

    pub(super) fn mark_source_changed(&mut self, path: &str) {
        self.verification.source_changed = true;
        if !matches!(self.verification.state, VerificationState::Failed) {
            self.verification.cause = VerificationCause::SourceChanged;
        }
        self.add_coverage_path(path);
        self.refresh_validity();
    }

    fn add_coverage_path(&mut self, path: &str) {
        if matches!(self.verification.coverage, VerificationCoverage::Workspace) {
            return;
        }
        let path = bound_item(path);
        if path.is_empty() {
            return;
        }
        if let VerificationCoverage::Resources(paths) = &mut self.verification.coverage {
            if !paths.iter().any(|existing| existing == &path) {
                paths.push(path);
                if paths.len() > MAX_COVERAGE_PATHS {
                    let drop = paths.len() - MAX_COVERAGE_PATHS;
                    paths.drain(0..drop);
                }
            }
            return;
        }
        self.verification.coverage = VerificationCoverage::Resources(vec![path]);
    }

    pub fn has_failures(&self) -> bool {
        !self.failed_commands.is_empty()
    }

    /// The verification identity of the latest positive, exactly-attributed
    /// PASS whose basis tuple still matches the current world: task
    /// verification basis, user directive and admitted workspace revision.
    /// This is the evidence side of the execution-ready join. Empty
    /// provenance fields are legacy/non-exact evidence and stay fail-closed.
    pub fn trusted_verification_identity(&self) -> Option<&str> {
        if self.validity() != VerificationState::Current {
            return None;
        }
        let basis = self.verification.spec_revision;
        self.verifications.iter().rev().find(|fact| {
            fact.ok
                && !fact.source_tool_name.is_empty()
                && !fact.verification_identity.is_empty()
                && fact.anchor_revision == basis
                && fact.directive_revision == self.directive_revision
                && fact.workspace_revision == self.workspace_revision
        })
        .map(|fact| fact.verification_identity.as_str())
    }

    /// Execution-local readiness layer of the settlement gate: a current
    /// trusted verification pass binds the full tuple (anchor basis,
    /// directive, workspace) and no obligation row or failed command is
    /// open. The actor still joins in-flight/cancel-cleanup state and the
    /// task-anchor gates on top of this; execution state alone never
    /// claims the whole task is settled.
    pub fn execution_ready(&self) -> bool {
        self.trusted_verification_identity().is_some()
            && self.open_obligation_count() == 0
            && self.failed_commands.is_empty()
    }

    /// Derived settlement label: an evidence-driven decision boundary, not a
    /// policy. Execution-local only: the strongest label this function can
    /// produce is `VerifiedCurrent` (the world is verified and no execution
    /// obligation is open). Whether that rises to `SettledCandidate` is the
    /// actor-owned task-aware join over the anchor epoch, open loops, next
    /// action and acceptance coverage; the pure execution view never names
    /// the whole task settled on its own. Provenance: the label is
    /// recomputed on demand from typed facts, never stored, so a checkpoint
    /// cannot carry a stale computed label.
    pub fn settlement(&self) -> agent_contracts::SettlementLabel {
        use agent_contracts::SettlementLabel;
        if self.trusted_verification_identity().is_none() {
            return if self.has_unmet_obligation() {
                SettlementLabel::VerificationDue
            } else {
                // No exact trusted receipt on this basis (legacy evidence,
                // directive moved, or a bare ok row): fail closed.
                SettlementLabel::Working
            };
        }
        if self.open_obligation_count() > 0 || self.has_failures() {
            SettlementLabel::Working
        } else {
            SettlementLabel::VerifiedCurrent
        }
    }

    pub fn fact_for(&self, path: &str) -> Option<&ResourceFact> {
        self.checked_files.iter().find(|row| row.path == path)
    }

    /// Whether existing Runtime facts already make this path a task
    /// precondition. Textual directive/anchor relevance is combined by the
    /// actor; this method owns only structured execution state.
    pub fn path_is_execution_rooted(&self, path: &str) -> bool {
        let path = agent_contracts::normalize_resource_path(path);
        if path.is_empty() {
            return false;
        }
        if self.checked_files.iter().any(|fact| fact.path == path) {
            return true;
        }
        if matches!(
            &self.verification.coverage,
            VerificationCoverage::Resources(paths) if paths.iter().any(|covered| covered == &path)
        ) {
            return true;
        }
        self.obligations.iter().any(|row| {
            row.scope_key == path
                || row
                    .precondition
                    .split_once('@')
                    .map(|(candidate, _)| candidate == path)
                    .unwrap_or(row.precondition == path)
        })
    }

    /// Source tools of live obligation rows, sorted and deduplicated. This
    /// is a derived view — membership exists exactly while its row does, so
    /// resolution, invalidation and the bounded drop path release the lease
    /// with no separate bookkeeping.
    pub fn obligation_source_tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = self
            .obligations
            .iter()
            .filter(|row| !row.source_tool_name.is_empty())
            .map(|row| row.source_tool_name.clone())
            .collect();
        tools.sort();
        tools.dedup();
        tools
    }

    /// Exact verifier schemas remembered for the current verification basis.
    /// The caller decides whether verification is due; current verification
    /// does not keep schemas resident merely because a source exists.
    pub fn verification_source_tools(&self, verification_revision: u64) -> Vec<String> {
        let mut tools: Vec<String> = self
            .verification_sources
            .iter()
            .filter(|source| source.anchor_revision == verification_revision)
            .map(|source| source.tool_name.clone())
            .collect();
        tools.sort();
        tools.dedup();
        tools
    }

    /// Return a PASS only when the complete equivalence tuple is current:
    /// trusted tool+recipe, exact arguments, task anchor, user directive and
    /// admitted workspace world. There is no TTL or approximate match; any
    /// uncertainty falls through to normal dispatch.
    pub fn current_exact_verification_pass(
        &self,
        tool_name: &str,
        argument_digest: &str,
        verification_revision: u64,
        attribution: &RuntimeExecutionAttribution,
    ) -> Option<VerificationFact> {
        let verification_identity = attribution.exact_verification_identity()?;
        if self.verification.spec_revision != verification_revision
            || self.validity() != VerificationState::Current
        {
            return None;
        }
        self.verifications
            .iter()
            .rev()
            .find(|fact| {
                fact.ok
                    && fact.anchor_revision == verification_revision
                    && fact.directive_revision == self.directive_revision
                    && fact.workspace_revision == self.workspace_revision
                    && fact.source_tool_name == tool_name
                    && fact.argument_digest == argument_digest
                    && fact.verification_identity == verification_identity
            })
            .cloned()
    }

    /// Domain-equivalent PASS lookup: the request and the recorded fact may
    /// come from different recipes, but only when the trusted pre-dispatch
    /// attribution proves both sides sit in one host-declared coverage class
    /// under the same declaration revision, share one class execution
    /// identity, and every exact-current world check still holds. Recipe
    /// class membership itself is judged against the current composition by
    /// the caller, which owns the table; any uncertainty returns `None`.
    pub fn current_domain_verification_pass(
        &self,
        verification_revision: u64,
        attribution: &RuntimeExecutionAttribution,
    ) -> Option<VerificationFact> {
        // The skipped dispatch must itself be exact-capable; a downgrade to
        // TaskScoped never rides a sibling's PASS.
        attribution.exact_verification_identity()?;
        let requested = attribution.verification_recipe()?;
        let domain = requested.coverage_domain.as_deref()?;
        if requested.domain_declaration_revision.is_none()
            || requested.class_identity_digest.is_empty()
        {
            return None;
        }
        if self.verification.spec_revision != verification_revision
            || self.validity() != VerificationState::Current
        {
            return None;
        }
        self.verifications
            .iter()
            .rev()
            .find(|fact| {
                fact.ok
                    && fact.anchor_revision == verification_revision
                    && fact.directive_revision == self.directive_revision
                    && fact.workspace_revision == self.workspace_revision
                    && fact.recipe_provenance.as_ref().is_some_and(|recorded| {
                        recorded.class_identity_digest == requested.class_identity_digest
                            && recorded.coverage_domain.as_deref() == Some(domain)
                            && recorded.domain_declaration_revision
                                == requested.domain_declaration_revision
                    })
            })
            .cloned()
    }

    /// Find a current speculative miss equivalent to this attributed call.
    /// Target identity, not arbitrary arguments, defines equivalence: line
    /// windows and search needles cannot make an absent path appear. The
    /// actor still performs a live Workspace absence check before reuse.
    pub fn current_negative_fact(
        &self,
        tool_name: &str,
        attribution: &RuntimeExecutionAttribution,
    ) -> Option<NegativeExecutionFact> {
        if !matches!(
            attribution.host.purpose,
            ToolExecutionPurpose::Observe | ToolExecutionPurpose::Search
        ) {
            return None;
        }
        attribution.host.targets.iter().find_map(|target| {
            if attribution.target_is_rooted(target) {
                return None;
            }
            self.negative_facts
                .iter()
                .find(|fact| {
                    fact.tool_name == tool_name
                        && fact.target == *target
                        && fact.failure == ToolFailureClass::PathNotFound
                        && fact.workspace_revision == self.workspace_revision
                })
                .cloned()
        })
    }

    /// Remove one negative fact after a live recheck disproves it. The
    /// transition is returned so the actor can journal the decision before
    /// dispatching the formerly-missing path.
    pub fn invalidate_negative_fact(
        &mut self,
        tool_name: &str,
        target: &str,
    ) -> Option<NegativeFactTransition> {
        let target = agent_contracts::normalize_resource_path(target);
        let index = self
            .negative_facts
            .iter()
            .position(|fact| fact.tool_name == tool_name && fact.target == target)?;
        let fact = self.negative_facts.remove(index);
        Some(NegativeFactTransition {
            kind: NegativeFactEventKind::Invalidated,
            tool_name: fact.tool_name,
            target: fact.target,
            failure: fact.failure,
            workspace_revision: self.workspace_revision,
        })
    }

    /// Current directive exact-mentions ∩ known (non-Missing) resource
    /// paths. Recency first, capped. Runtime fills `ContextHints`; the
    /// engine must not re-parse TurnIntent.
    pub fn foreground_resources(&self, turn_intent: &str) -> Vec<ResourceKey> {
        let mut hits: Vec<&ResourceFact> = self
            .checked_files
            .iter()
            .filter(|row| row.freshness != ResourceFreshness::Missing)
            .filter(|row| path_exactly_in_directive(turn_intent, &row.path))
            .collect();
        hits.sort_by(|a, b| b.turn.cmp(&a.turn).then_with(|| a.path.cmp(&b.path)));
        hits.truncate(MAX_FOREGROUND_RESOURCES);
        hits.into_iter()
            .map(|row| ResourceKey {
                path: row.path.clone(),
                revision: (!row.digest.is_empty()).then(|| row.digest.clone()),
            })
            .collect()
    }

    /// Unit-test helper: clone plus replay persistable `TurnFrame` results.
    /// Production uses `ActiveTurn.execution` (observed live, installed
    /// onto `TaskRecord.resume` after the TurnCompleted barrier).
    #[cfg(test)]
    pub(crate) fn project_from_turn(
        &self,
        turn: &TurnFrame,
        anchor_revision: u64,
        turn_number: u64,
    ) -> TaskProgressView {
        self.apply_open_turn(turn, anchor_revision, turn_number)
            .view()
    }

    #[cfg(test)]
    pub(crate) fn apply_open_turn(
        &self,
        turn: &TurnFrame,
        anchor_revision: u64,
        turn_number: u64,
    ) -> Self {
        let mut projected = self.clone();
        for step in &turn.steps {
            let TurnFrameStep::ToolResult {
                output,
                disposition,
                ..
            } = step
            else {
                continue;
            };
            if *disposition != ToolResultDisposition::PersistObservation {
                continue;
            }
            projected.observe_tool(output, anchor_revision, turn_number);
        }
        projected
    }

    pub fn view(&self) -> TaskProgressView {
        TaskProgressView {
            anchor_revision: self.anchor_revision,
            workspace_revision: self.workspace_revision,
            checked_files: self
                .checked_files
                .iter()
                .filter(|row| row.freshness == ResourceFreshness::Fresh)
                .map(|row| format!("{}@{}", row.path, row.digest))
                .collect(),
            verifications: self
                .current_verifications()
                .map(|row| format!("{}:{}", if row.ok { "ok" } else { "fail" }, row.summary))
                .collect(),
            failed_commands: self
                .failed_commands
                .iter()
                // A current speculative miss has a more precise projection
                // below. Do not spend prompt budget on both the generic
                // failure row and its revision-bound known-absence identity.
                .filter(|failure| {
                    !self.negative_facts.iter().any(|negative| {
                        negative.workspace_revision == self.workspace_revision
                            && negative.tool_name == failure.tool_name
                            && negative.target == failure.target
                    })
                })
                .map(|row| {
                    if row.target.is_empty() {
                        format!("{}:{}", row.tool_name, row.summary)
                    } else {
                        format!("{} {}:{}", row.tool_name, row.target, row.summary)
                    }
                })
                .chain(
                    self.negative_facts
                        .iter()
                        .filter(|row| row.workspace_revision == self.workspace_revision)
                        .map(|row| {
                            format!(
                                "known_absent {} {}:{} @ world={}",
                                row.tool_name,
                                row.target,
                                row.failure.as_str(),
                                row.workspace_revision
                            )
                        }),
                )
                .collect(),
            operational_evidence: self.evidence_rows(),
            unresolved_blockers: self.obligation_warnings(),
            stall_warning: self.stall_warning(),
            frontier_warning: self.frontier_warning(),
            completion_opportunity: None,
            // The task-aware settlement fact is filled by the actor only
            // when the projection switch is on and the joined label rises
            // to `SettledCandidate`; execution state never projects it.
            settlement: None,
        }
    }

    /// 类型化证据行，最新在前、有界。只含 key + 结果 + world 版本；
    /// Resource 有效性附带 path@digest 身份。无任何正文。投影前先按
    /// currentness 谓词过滤——过期证据不渲染。
    fn evidence_rows(&self) -> Vec<String> {
        self.evidence
            .iter()
            .filter(|row| self.evidence_is_current(row))
            .take(6)
            .map(|row| match &row.validity {
                EvidenceValidity::Resource { digest, .. } if !digest.is_empty() => {
                    format!(
                        "{}@{}: {} @ world={}",
                        row.key, digest, row.outcome, row.observed_world_revision
                    )
                }
                _ => format!(
                    "{}: {} @ world={}",
                    row.key, row.outcome, row.observed_world_revision
                ),
            })
            .collect()
    }

    /// 收敛 advisory：连续 [`FRONTIER_ADVISORY_THRESHOLD`] 个动作无可证明
    /// 前沿推进即触发。软提示：模型仍自主选择下一步。
    pub(super) fn frontier_warning(&self) -> Option<String> {
        if self.convergence.actions_since_frontier_advance < FRONTIER_ADVISORY_THRESHOLD {
            return None;
        }
        let recent = self
            .convergence
            .recent_deltas
            .iter()
            .map(|delta| delta.token())
            .collect::<Vec<_>>()
            .join(",");
        Some(format!(
            "EXECUTION FRONTIER UNCHANGED: {} action(s) without a provable frontier advance (recent deltas: {}). Re-reading known state or repeating outcomes does not move the task; act on what you know, change strategy, or finish.",
            self.convergence.actions_since_frontier_advance, recent
        ))
    }

    /// 有界的确定性停滞提示。两个检测器共用：同签名重复与跨目标同类
    /// 聚类，先触发者生效。仅建议，模型仍自主选择。
    pub(super) fn stall_warning(&self) -> Option<String> {
        if self.stall.consecutive_no_progress >= STALL_THRESHOLD {
            let target = if self.stall.target.is_empty() {
                "unknown target"
            } else {
                &self.stall.target
            };
            let failure = self
                .stall
                .failure
                .map(|class| class.as_str().to_string())
                .unwrap_or_else(|| "no failure class".into());
            return Some(format!(
                "EXECUTION STALL: {} on {} repeated {} time(s) without world progress (last failure: {}). Choose another strategy or finish with the current state.",
                self.stall.tool, target, self.stall.consecutive_no_progress, failure
            ));
        }
        if self.stall.consecutive_no_progress > 0
            && let Some(failure) = self.failure_cluster.failure
            && self.failure_cluster.tried_targets.len() >= STALL_CLUSTER_DISTINCT_TARGETS as usize
        {
            return Some(format!(
                "EXECUTION STALL: {} hit {} across {} different targets in a row without world progress. Choose another strategy or finish with the current state.",
                self.failure_cluster.tool,
                failure.as_str(),
                self.failure_cluster.tried_targets.len()
            ));
        }
        None
    }

    /// 收敛记账：可证明推进的 delta 清空停滞签名、失败聚类与无推进
    /// 债务；其余 delta 只推进入环形缓冲并累计债务。逐签名停滞与跨
    /// 目标聚类只在重复行为（NoProgress / RedundantEvidence）下累计，
    /// Unknown 失效不冒充停滞也不清账。
    pub(super) fn update_convergence(
        &mut self,
        identity: &OperationIdentity,
        failure: Option<ToolFailureClass>,
        delta: FrontierDelta,
    ) {
        self.push_delta(delta);
        if delta.advances_frontier() {
            self.stall = StallState::default();
            self.failure_cluster = FailureCluster::default();
            self.convergence.actions_since_frontier_advance = 0;
            return;
        }
        self.convergence.actions_since_frontier_advance = self
            .convergence
            .actions_since_frontier_advance
            .saturating_add(1);
        // 失败聚类：任何带失败类别的非推进轮都累计——包括未知足迹的
        // 失败运行，换拼写的连击躲不开聚类。
        if let Some(class) = failure {
            if self.failure_cluster.tool == identity.tool_name
                && self.failure_cluster.failure == Some(class)
            {
                if !self
                    .failure_cluster
                    .tried_targets
                    .iter()
                    .any(|target| target == &identity.target)
                    && self.failure_cluster.tried_targets.len() < 8
                {
                    self.failure_cluster
                        .tried_targets
                        .push(identity.target.clone());
                }
            } else {
                self.failure_cluster = FailureCluster {
                    tool: identity.tool_name.clone(),
                    failure: Some(class),
                    tried_targets: vec![identity.target.clone()],
                };
            }
        }
        // 逐签名停滞只在重复行为（NoProgress / redundant/reconfirmed
        // evidence）下累计；无失败的未知失效只记债务，不冒充停滞。
        if !matches!(
            delta,
            FrontierDelta::NoProgress
                | FrontierDelta::RedundantEvidence
                | FrontierDelta::EvidenceReconfirmed
        ) {
            return;
        }
        if self.stall.tool != identity.tool_name
            || self.stall.target != identity.target
            || self.stall.failure != failure
        {
            self.stall = StallState {
                consecutive_no_progress: 0,
                tool: identity.tool_name.clone(),
                target: identity.target.clone(),
                failure,
            };
        }
        self.stall.consecutive_no_progress = self.stall.consecutive_no_progress.saturating_add(1);
    }

    fn push_delta(&mut self, delta: FrontierDelta) {
        self.convergence.recent_deltas.push(delta);
        let excess = self
            .convergence
            .recent_deltas
            .len()
            .saturating_sub(MAX_RECENT_DELTAS);
        if excess > 0 {
            self.convergence.recent_deltas.drain(0..excess);
        }
    }

    /// World movement marks version-bound evidence non-current. Rows stay in
    /// the same bounded table as semantic fingerprints so an identical
    /// observation can repair currentness without laundering itself into
    /// new evidence. Prompt projection still hides every non-current row.
    pub(super) fn invalidate_stale_evidence(&mut self) -> u64 {
        // 失效与投影共用同一个 currentness 谓词；
        // 先物化事实表快照，避免借用冲突。
        let revision = self.workspace_revision;
        let last_turn = self.last_turn;
        let fresh: std::collections::HashMap<&str, &str> = self
            .checked_files
            .iter()
            .filter(|fact| fact.freshness == ResourceFreshness::Fresh)
            .map(|fact| (fact.path.as_str(), fact.digest.as_str()))
            .collect();
        let mut invalidated = 0u64;
        for row in &mut self.evidence {
            let binding_current = match &row.validity {
                EvidenceValidity::WorkspaceRevision(at) => *at == revision,
                EvidenceValidity::Resource { path, digest } => fresh
                    .get(path.as_str())
                    .is_some_and(|current| *current == digest),
                EvidenceValidity::Turn => row.turn != 0 && row.turn == last_turn,
            };
            if row.current && !binding_current {
                row.current = false;
                invalidated = invalidated.saturating_add(1);
            }
        }
        if invalidated > 0 {
            self.convergence.evidence_revision = self
                .convergence
                .evidence_revision
                .saturating_add(invalidated);
        }
        invalidated
    }

    /// "这条证据现在还能证明为真吗"——唯一裁决点。
    /// WorkspaceRevision 绑定当前世界版本；Resource 要求事实表存在
    /// 同 path@digest 的 Fresh 行；Turn 要求仍是当轮。
    pub(super) fn evidence_is_current(&self, row: &ExecutionEvidence) -> bool {
        if !row.current {
            return false;
        }
        match &row.validity {
            EvidenceValidity::WorkspaceRevision(at) => *at == self.workspace_revision,
            EvidenceValidity::Resource { path, digest } => {
                self.fact_for(path).is_some_and(|fact| {
                    fact.freshness == ResourceFreshness::Fresh && fact.digest == *digest
                })
            }
            EvidenceValidity::Turn => row.turn != 0 && row.turn == self.last_turn,
        }
    }

    /// Any admitted workspace mutation invalidates absence claims. This is
    /// intentionally conservative: a write to a parent or a generated file
    /// can make a previously speculative path exist. Rows are tiny and
    /// bounded, so invalidation is exact and event-visible.
    pub(super) fn invalidate_negative_facts_for_world_change(
        &mut self,
    ) -> Vec<NegativeFactTransition> {
        self.negative_facts
            .drain(..)
            .map(|fact| NegativeFactTransition {
                kind: NegativeFactEventKind::Invalidated,
                tool_name: fact.tool_name,
                target: fact.target,
                failure: fact.failure,
                workspace_revision: self.workspace_revision,
            })
            .collect()
    }

    /// Apply one trusted path observation to the negative-fact table.
    /// Returns whether this miss is speculative and therefore must *not* open
    /// a task obligation, plus body-free lifecycle transitions.
    pub(super) fn observe_negative_fact(
        &mut self,
        output: &ToolOutput,
        argument_digest: &str,
        turn: u64,
        attribution: Option<&RuntimeExecutionAttribution>,
    ) -> (bool, Vec<NegativeFactTransition>) {
        let Some(attribution) = attribution else {
            return (false, Vec::new());
        };
        if !matches!(
            attribution.host.purpose,
            ToolExecutionPurpose::Observe | ToolExecutionPurpose::Search
        ) {
            return (false, Vec::new());
        }
        let Some(target) = output
            .file_path()
            .map(agent_contracts::normalize_resource_path)
            .filter(|target| attribution.host.targets.iter().any(|known| known == target))
        else {
            return (false, Vec::new());
        };
        let mut transitions = Vec::new();
        if output.ok {
            if let Some(index) = self
                .negative_facts
                .iter()
                .position(|fact| fact.tool_name == output.tool_name && fact.target == target)
            {
                let fact = self.negative_facts.remove(index);
                transitions.push(NegativeFactTransition {
                    kind: NegativeFactEventKind::Resolved,
                    tool_name: fact.tool_name,
                    target: fact.target,
                    failure: fact.failure,
                    workspace_revision: self.workspace_revision,
                });
            }
            return (false, transitions);
        }
        if output.failure_class() != Some(ToolFailureClass::PathNotFound) {
            return (false, transitions);
        }
        if attribution.target_is_rooted(&target) {
            if let Some(index) = self
                .negative_facts
                .iter()
                .position(|fact| fact.tool_name == output.tool_name && fact.target == target)
            {
                let fact = self.negative_facts.remove(index);
                transitions.push(NegativeFactTransition {
                    kind: NegativeFactEventKind::Promoted,
                    tool_name: fact.tool_name,
                    target: fact.target,
                    failure: fact.failure,
                    workspace_revision: self.workspace_revision,
                });
            }
            return (false, transitions);
        }

        let argument_digest = bound_item(argument_digest);
        if let Some(existing) = self
            .negative_facts
            .iter_mut()
            .find(|fact| fact.tool_name == output.tool_name && fact.target == target)
        {
            existing.argument_digest = argument_digest;
            existing.workspace_revision = self.workspace_revision;
            existing.turn = turn;
            return (true, transitions);
        }
        self.negative_facts.push(NegativeExecutionFact {
            tool_name: bound_item(&output.tool_name),
            target: target.clone(),
            argument_digest,
            failure: ToolFailureClass::PathNotFound,
            workspace_revision: self.workspace_revision,
            turn,
        });
        transitions.push(NegativeFactTransition {
            kind: NegativeFactEventKind::Recorded,
            tool_name: bound_item(&output.tool_name),
            target,
            failure: ToolFailureClass::PathNotFound,
            workspace_revision: self.workspace_revision,
        });
        (true, transitions)
    }

    /// Remember an exact trusted verifier for this task anchor. Producer
    /// metadata cannot reach this path; only pre-dispatch attribution can.
    pub(super) fn record_verification_source(
        &mut self,
        tool_name: &str,
        argument_digest: &str,
        attribution: Option<&RuntimeExecutionAttribution>,
    ) {
        if !attribution.is_some_and(RuntimeExecutionAttribution::reusable_verification) {
            return;
        }
        let tool_name = bound_item(tool_name);
        let argument_digest = bound_item(argument_digest);
        self.verification_sources
            .retain(|source| source.tool_name != tool_name);
        self.verification_sources.push(VerificationSourceLease {
            tool_name,
            argument_digest,
            anchor_revision: self.verification.spec_revision,
        });
    }

    /// lineage：由 typed 失效事实登记/累计义务。同 scope 的
    /// 行是同一个 blocker 的同一血统——同指纹累计 epoch 内尝试，异
    /// 指纹推进 epoch（PreconditionChanged ≠ Resolved）；不同 scope
    /// 是不同 blocker，互不取代。只有失败输出才开义务。
    pub(super) fn record_obligation(
        &mut self,
        output: &ToolOutput,
        identity: &OperationIdentity,
        speculative_negative: bool,
        events: &mut Vec<agent_contracts::ExecutionObligationEvent>,
    ) {
        if output.ok || speculative_negative {
            return;
        }
        // 注意走 output 的域判定而非裸 class 映射：外壳工具的
        // path_not_found 属于 ExecutableResolution，不是 ResourcePath。
        let domain = output.failure_domain();
        if matches!(domain, ToolFailureDomain::NonDeterministic) {
            return;
        }
        let metadata_str = |key: &str| {
            output
                .metadata
                .get(key)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let touches = output.resource_touches();
        let (scope_key, fingerprint) = match domain {
            // resolver 在 preflight 统一盖章；旧格式失败只有指纹时退化为
            // 单一 scope（scope 为空按匿名血统处理，仍可累计）。
            ToolFailureDomain::ExecutableResolution => (
                metadata_str("resolution_scope_key"),
                metadata_str("resolution_fingerprint"),
            ),
            ToolFailureDomain::EditTarget | ToolFailureDomain::ResourcePath => {
                let Some(touch) = touches.first() else {
                    return;
                };
                let fingerprint = match (domain, touch.revision.as_deref()) {
                    (ToolFailureDomain::EditTarget, Some(revision)) if !revision.is_empty() => {
                        format!("{}@{}", touch.path, revision)
                    }
                    _ => String::new(),
                };
                (touch.path.clone(), fingerprint)
            }
            ToolFailureDomain::ProjectMarker => (
                bound_evidence_text(output.operation_target().unwrap_or_default()),
                String::new(),
            ),
            ToolFailureDomain::NonDeterministic => return,
        };
        let scope_key = bound_evidence_text(&scope_key);
        if scope_key.is_empty() && fingerprint.is_empty() {
            return;
        }
        let target = bound_evidence_text(&identity.target);
        if let Some(row) = self
            .obligations
            .iter_mut()
            .find(|row| row.domain == domain && row.scope_key == scope_key)
        {
            let kind = if row.precondition == fingerprint {
                row.attempts = row.attempts.saturating_add(1);
                agent_contracts::ObligationEventKind::Attempted
            } else {
                // 前置变化：epoch 推进、本 epoch 失败计数从这次失败起算；
                // 血统与累计账目保持——Runtime 不会忘掉这个方向浪费过多少次。
                row.epoch = row.epoch.saturating_add(1);
                row.precondition = fingerprint.clone();
                row.attempts = 1;
                agent_contracts::ObligationEventKind::PreconditionChanged
            };
            row.total_attempts = row.total_attempts.saturating_add(1);
            if !row.tried_targets.iter().any(|t| t == &target)
                && row.tried_targets.len() < MAX_OBLIGATION_TARGETS
            {
                row.tried_targets.push(target);
            }
            events.push(agent_contracts::ExecutionObligationEvent {
                kind,
                domain,
                scope_digest: scope_key,
                epoch: row.epoch,
                attempts_in_epoch: row.attempts,
                total_attempts: row.total_attempts,
            });
            return;
        }
        self.obligations.push(ExecutionObligation {
            domain,
            scope_key,
            precondition: fingerprint.clone(),
            attempts: 1,
            epoch: 1,
            total_attempts: 1,
            tried_targets: vec![target],
            opened_at_evidence_revision: self.convergence.evidence_revision,
            source_tool_name: bound_evidence_text(&output.tool_name),
        });
        let row = self.obligations.last().expect("just pushed");
        events.push(agent_contracts::ExecutionObligationEvent {
            kind: agent_contracts::ObligationEventKind::Opened,
            domain,
            scope_digest: row.scope_key.clone(),
            epoch: row.epoch,
            attempts_in_epoch: row.attempts,
            total_attempts: row.total_attempts,
        });
        let excess = self.obligations.len().saturating_sub(MAX_OBLIGATIONS);
        if excess > 0 {
            for dropped in self.obligations.drain(0..excess) {
                let total = dropped.effective_total();
                events.push(agent_contracts::ExecutionObligationEvent {
                    kind: agent_contracts::ObligationEventKind::Dropped,
                    domain: dropped.domain,
                    scope_digest: dropped.scope_key,
                    epoch: dropped.epoch,
                    attempts_in_epoch: dropped.attempts,
                    total_attempts: total,
                });
            }
        }
    }

    /// 义务只被 blocker 特定的证明解除——ExecutableResolution
    /// 要求同 scope 且同前置指纹的成功（"同类成功"太宽：rustc 编译成功
    /// 不能证明 tests.exe 的解析 blocker 已解决）；世界推进只把 epoch
    /// 推进一格（PreconditionChanged ≠ Resolved）。EditTarget 以新
    /// digest 落地，或被失败之后的可信当前验证 supersede；ResourcePath
    /// 被 Known mutation 触碰或出现 Fresh 事实、ProjectMarker 被触碰——
    /// 这些本身就是 blocker 消失的证明。验证只能来自调用方传入的可信
    /// pre-dispatch attribution，不能由 ToolOutput metadata 自行声明。
    pub(super) fn resolve_obligations(
        &mut self,
        output: &ToolOutput,
        trusted_verification_pass: bool,
        events: &mut Vec<agent_contracts::ExecutionObligationEvent>,
    ) {
        let launch_ok = output.ok && is_command_tool(&output.tool_name);
        let success_scope = if launch_ok {
            output
                .metadata
                .get("resolution_scope_key")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        } else {
            String::new()
        };
        let success_fingerprint = if launch_ok {
            output
                .metadata
                .get("resolution_fingerprint")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        } else {
            String::new()
        };
        let mutated_paths: Vec<String> = match output.mutation_footprint() {
            agent_contracts::MutationFootprint::Known(_) if output.ok => output
                .resource_touches()
                .into_iter()
                .map(|touch| touch.path)
                .collect(),
            _ => Vec::new(),
        };
        // 物化事实表快照，闭包里不再借 self。
        let fresh: std::collections::HashMap<&str, &str> = self
            .checked_files
            .iter()
            .filter(|fact| fact.freshness == ResourceFreshness::Fresh)
            .map(|fact| (fact.path.as_str(), fact.digest.as_str()))
            .collect();

        enum Outcome {
            Keep,
            AdvanceEpoch,
            Resolve,
        }
        let mut kept = Vec::with_capacity(self.obligations.len());
        for mut row in self.obligations.drain(..) {
            let outcome = match row.domain {
                ToolFailureDomain::ExecutableResolution => {
                    if !launch_ok || row.scope_key != success_scope {
                        Outcome::Keep
                    } else if row.scope_key.is_empty() && success_scope.is_empty() {
                        // 旧行/旧输出的退化匹配：指纹一致才认解决。
                        if !success_fingerprint.is_empty()
                            && row.precondition == success_fingerprint
                        {
                            Outcome::Resolve
                        } else {
                            Outcome::Keep
                        }
                    } else if row.precondition == success_fingerprint {
                        Outcome::Resolve
                    } else {
                        Outcome::AdvanceEpoch
                    }
                }
                ToolFailureDomain::EditTarget => {
                    let path = if row.scope_key.is_empty() {
                        row.precondition
                            .split_once('@')
                            .map(|(path, _)| path)
                            .unwrap_or(row.precondition.as_str())
                    } else {
                        row.scope_key.as_str()
                    };
                    let old = row
                        .precondition
                        .rsplit_once('@')
                        .map(|(_, rev)| rev)
                        .unwrap_or_default();
                    if trusted_verification_pass
                        || fresh.get(path).is_some_and(|digest| *digest != old)
                    {
                        Outcome::Resolve
                    } else {
                        Outcome::Keep
                    }
                }
                ToolFailureDomain::ResourcePath => {
                    let touched = mutated_paths.iter().any(|p| p == &row.scope_key)
                        || fresh.contains_key(row.scope_key.as_str());
                    if touched {
                        Outcome::Resolve
                    } else {
                        Outcome::Keep
                    }
                }
                ToolFailureDomain::ProjectMarker => {
                    if mutated_paths.iter().any(|p| p == &row.scope_key) {
                        Outcome::Resolve
                    } else {
                        Outcome::Keep
                    }
                }
                ToolFailureDomain::NonDeterministic => Outcome::Keep,
            };
            match outcome {
                Outcome::Keep => kept.push(row),
                Outcome::AdvanceEpoch => {
                    row.epoch = row.epoch.saturating_add(1);
                    row.attempts = 0;
                    row.precondition = success_fingerprint.clone();
                    events.push(agent_contracts::ExecutionObligationEvent {
                        kind: agent_contracts::ObligationEventKind::PreconditionChanged,
                        domain: row.domain,
                        scope_digest: row.scope_key.clone(),
                        epoch: row.epoch,
                        attempts_in_epoch: row.attempts,
                        total_attempts: row.effective_total(),
                    });
                    kept.push(row);
                }
                Outcome::Resolve => {
                    let total = row.effective_total();
                    events.push(agent_contracts::ExecutionObligationEvent {
                        kind: agent_contracts::ObligationEventKind::Resolved,
                        domain: row.domain,
                        scope_digest: row.scope_key,
                        epoch: row.epoch,
                        attempts_in_epoch: row.attempts,
                        total_attempts: total,
                    });
                }
            }
        }
        self.obligations = kept;
    }

    /// 有界的逐义务警告行（≤2 条）：与全局 advisory 正交，模型仍自主
    /// 选择解法，但"换名字再猜"不再能靠无关进展隐藏。
    fn obligation_warnings(&self) -> Vec<String> {
        self.obligations
            .iter()
            .take(2)
            .map(|row| {
                format!(
                    "UNRESOLVED BLOCKER [{}] epoch={} attempts={} total={} targets={:?} precondition={} — change the preconditions (build/install/create the target), not more identical guesses",
                    row.domain.as_str(),
                    row.epoch,
                    row.attempts,
                    row.effective_total(),
                    row.tried_targets,
                    row.precondition
                )
            })
            .collect()
    }

    /// Store successful observations in the bounded evidence table. A novel
    /// row advances the task frontier only when the current directive has no
    /// exact root yet or trusted Runtime attribution binds the observation to
    /// one; storage and task progress are deliberately separate. Key rules:
    /// - `git.status` / `git.diff` / `git.log`：key=工具名，
    ///   validity=`WorkspaceRevision(当前)`；
    /// - 其他带 path 的成功读：key=`工具:path`，
    ///   validity=`Resource{path,digest}`；
    /// - 成功命令运行（未知足迹）：key=`工具:参数摘要`，
    ///   validity=`WorkspaceRevision(当前)`。
    ///
    /// 同 key 同结果 digest 同参数：current 时是重复，dormant 时只是
    /// reconfirmation；只有语义结果或 coverage/参数改变才是新证据。
    pub(super) fn record_observation_evidence(
        &mut self,
        output: &ToolOutput,
        turn: u64,
        argument_digest: &str,
    ) -> ObservationEvidence {
        let target = output.operation_target().unwrap_or("").to_string();
        let is_git_read = matches!(
            output.tool_name.as_str(),
            "git.status" | "git.diff" | "git.log"
        );
        let touches = output.resource_touches();
        let (key, validity) = if is_git_read {
            (
                output.tool_name.clone(),
                EvidenceValidity::WorkspaceRevision(self.workspace_revision),
            )
        } else if let Some(touch) = touches.first() {
            let digest = touch.revision.clone().unwrap_or_default();
            (
                format!("{}:{}", output.tool_name, bound_item(&touch.path)),
                EvidenceValidity::Resource {
                    path: touch.path.clone(),
                    digest,
                },
            )
        } else if is_command_tool(&output.tool_name) && !target.is_empty() {
            (
                format!("{}:{}", output.tool_name, bound_item(&target)),
                EvidenceValidity::WorkspaceRevision(self.workspace_revision),
            )
        } else {
            return ObservationEvidence::None;
        };
        let outcome = bound_evidence_text(output.summary.trim());
        let outcome_digest = observation_outcome_digest(output);
        // Runtime 传入的真 ArgumentDigest（OperationCompletion 计算）；
        // 空串时退化为参数摘要，保持旧轨迹可用。
        let argument_digest = if argument_digest.is_empty() {
            bound_evidence_text(&target)
        } else {
            bound_evidence_text(argument_digest)
        };
        if let Some(existing) = self.evidence.iter_mut().find(|row| row.key == key) {
            let same_semantic = !existing.outcome_digest.is_empty()
                && existing.outcome_digest == outcome_digest
                && existing.argument_digest == argument_digest;
            if same_semantic && existing.current && existing.validity == validity {
                return ObservationEvidence::Repeated;
            }
            let reconfirmed = same_semantic && !existing.current;
            existing.outcome = outcome;
            existing.outcome_digest = outcome_digest;
            existing.observed_world_revision = self.workspace_revision;
            existing.validity = validity;
            existing.argument_digest = argument_digest;
            existing.current = true;
            existing.turn = turn;
            existing.evidence_ref = output.artifact_ref.clone();
            self.bump_evidence_revision();
            return if reconfirmed {
                ObservationEvidence::Reconfirmed
            } else {
                ObservationEvidence::Advanced
            };
        }
        self.evidence.insert(
            0,
            ExecutionEvidence {
                key,
                outcome,
                observed_world_revision: self.workspace_revision,
                validity,
                argument_digest,
                outcome_digest,
                current: true,
                turn,
                evidence_ref: output.artifact_ref.clone(),
            },
        );
        if self.evidence.len() > MAX_EVIDENCE_ROWS {
            self.evidence.truncate(MAX_EVIDENCE_ROWS);
        }
        self.bump_evidence_revision();
        ObservationEvidence::Advanced
    }

    fn bump_evidence_revision(&mut self) {
        self.convergence.evidence_revision = self.convergence.evidence_revision.saturating_add(1);
    }

    pub(super) fn mark_facts_needs_revalidation(&mut self) {
        for fact in &mut self.checked_files {
            if fact.freshness != ResourceFreshness::Missing {
                fact.freshness = ResourceFreshness::NeedsRevalidation;
            }
        }
    }

    /// Upsert one resource fact while keeping semantic change orthogonal to
    /// currentness repair.
    pub(super) fn upsert_file(
        &mut self,
        path: &str,
        digest: String,
        turn: u64,
        provenance: ResourceProvenance,
    ) -> ResourceObservation {
        let path = bound_item(path);
        let digest = bound_item(&digest);
        let digest_changed = self.checked_files.iter().any(|row| {
            row.path == path && !row.digest.is_empty() && !digest.is_empty() && row.digest != digest
        });
        if digest_changed {
            self.mark_source_changed(&path);
        }
        if let Some(existing) = self.checked_files.iter_mut().find(|row| row.path == path) {
            let semantic_changed = digest_changed || existing.digest != digest;
            let currentness_repaired =
                !semantic_changed && existing.freshness != ResourceFreshness::Fresh;
            existing.digest = digest;
            existing.turn = turn;
            existing.freshness = ResourceFreshness::Fresh;
            existing.provenance = provenance;
            return if semantic_changed {
                ResourceObservation::Advanced
            } else if currentness_repaired {
                ResourceObservation::Reconfirmed
            } else {
                ResourceObservation::None
            };
        }
        self.checked_files.push(ResourceFact {
            path,
            digest,
            freshness: ResourceFreshness::Fresh,
            turn,
            provenance,
        });
        ResourceObservation::Advanced
    }

    pub(super) fn push_failure(
        &mut self,
        identity: &OperationIdentity,
        summary: String,
        turn: u64,
    ) {
        self.failed_commands
            .retain(|row| !same_operation(row, identity));
        self.failed_commands.push(FailedCommandFact {
            tool_name: bound_item(&identity.tool_name),
            target: bound_item(&identity.target),
            summary: bound_item(&summary),
            turn,
        });
    }

    pub(super) fn push_verification(
        &mut self,
        output: &ToolOutput,
        argument_digest: &str,
        attribution: Option<&RuntimeExecutionAttribution>,
        turn: u64,
    ) -> Option<VerificationPassTransition> {
        let verification_identity = attribution
            .and_then(RuntimeExecutionAttribution::exact_verification_identity)
            .map(bound_item)
            .unwrap_or_default();
        let source_tool_name = if verification_identity.is_empty() {
            String::new()
        } else {
            bound_item(&output.tool_name)
        };
        let argument_digest = if verification_identity.is_empty() {
            String::new()
        } else {
            bound_item(argument_digest)
        };
        // Store the verification basis, not the whole anchor revision, so
        // progress-only CAS does not invalidate a Current verifier.
        let basis = self.verification.spec_revision;
        self.verifications.push(VerificationFact {
            summary: bound_item(&output.summary),
            ok: output.ok,
            turn,
            anchor_revision: basis,
            workspace_revision: self.workspace_revision,
            source_tool_name: source_tool_name.clone(),
            argument_digest: argument_digest.clone(),
            verification_identity: verification_identity.clone(),
            directive_revision: self.directive_revision,
            evidence_ref: output.artifact_ref.as_ref().map(|value| bound_item(value)),
            recipe_provenance: attribution
                .and_then(|attribution| attribution.host.verification_recipe.clone()),
        });
        (output.ok && !verification_identity.is_empty()).then_some(VerificationPassTransition {
            kind: VerificationPassEventKind::Recorded,
            tool_name: source_tool_name,
            argument_digest,
            verification_identity,
            anchor_revision: basis,
            directive_revision: self.directive_revision,
            workspace_revision: self.workspace_revision,
        })
    }

    pub(super) fn cap(&mut self) {
        if self.checked_files.len() > MAX_RESUME_FILES {
            let drop = self.checked_files.len() - MAX_RESUME_FILES;
            self.checked_files.drain(0..drop);
        }
        if self.verifications.len() > MAX_RESUME_FAILURES {
            let drop = self.verifications.len() - MAX_RESUME_FAILURES;
            self.verifications.drain(0..drop);
        }
        if self.failed_commands.len() > MAX_RESUME_FAILURES {
            let drop = self.failed_commands.len() - MAX_RESUME_FAILURES;
            self.failed_commands.drain(0..drop);
        }
        if self.negative_facts.len() > MAX_NEGATIVE_FACTS {
            let drop = self.negative_facts.len() - MAX_NEGATIVE_FACTS;
            self.negative_facts.drain(0..drop);
        }
        if self.verification_sources.len() > MAX_VERIFICATION_SOURCES {
            let drop = self.verification_sources.len() - MAX_VERIFICATION_SOURCES;
            self.verification_sources.drain(0..drop);
        }
    }

    pub(super) fn current_verifications(&self) -> impl Iterator<Item = &VerificationFact> {
        self.verifications.iter().filter(|row| {
            row.anchor_revision == self.verification.spec_revision
                && row.workspace_revision == self.workspace_revision
        })
    }
}

pub(super) fn operation_identity(output: &ToolOutput) -> OperationIdentity {
    let target = output.operation_target().unwrap_or("").to_string();
    OperationIdentity {
        tool_name: output.tool_name.clone(),
        target,
    }
}

pub(super) fn same_operation(row: &FailedCommandFact, identity: &OperationIdentity) -> bool {
    row.tool_name == identity.tool_name && row.target == identity.target
}

pub(crate) fn validate_execution_state(state: &ExecutionState) -> Result<(), String> {
    if state.checked_files.len() > MAX_RESUME_FILES
        || state.verifications.len() > MAX_RESUME_FAILURES
        || state.failed_commands.len() > MAX_RESUME_FAILURES
        || state.negative_facts.len() > MAX_NEGATIVE_FACTS
        || state.verification_sources.len() > MAX_VERIFICATION_SOURCES
    {
        return Err("resume list exceeds its cap".into());
    }
    // restore 契约不得假定 checkpoint 由当前 Runtime
    // 生成且未损坏——新增字段同样受界。
    if state.evidence.len() > MAX_EVIDENCE_ROWS
        || state.convergence.recent_deltas.len() > MAX_RECENT_DELTAS
        || state.failure_cluster.tried_targets.len() > 8
        || state.obligations.len() > MAX_OBLIGATIONS
    {
        return Err("execution frontier exceeds its cap".into());
    }
    for row in &state.evidence {
        if row.key.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.outcome.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.argument_digest.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.outcome_digest.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
        {
            return Err("evidence row exceeds its text bound".into());
        }
    }
    for row in &state.verifications {
        if row.summary.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.source_tool_name.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.argument_digest.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.verification_identity.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || !row.verification_identity.is_empty()
                && row
                    .verification_identity
                    .parse::<agent_contracts::ContentDigest>()
                    .is_err()
            || row
                .evidence_ref
                .as_ref()
                .is_some_and(|value| value.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS)
        {
            return Err("verification fact exceeds its text bound".into());
        }
    }
    for row in &state.negative_facts {
        if row.tool_name.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.target.chars().count() > agent_contracts::MAX_RESOURCE_PATH_CHARS
            || row.argument_digest.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
        {
            return Err("negative fact exceeds its text bound".into());
        }
    }
    for row in &state.verification_sources {
        if row.tool_name.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
            || row.argument_digest.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS
        {
            return Err("verification source exceeds its text bound".into());
        }
    }
    Ok(())
}

pub(super) fn path_mentioned_in_query(query: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let query = query.replace('\\', "/");
    if query.contains(path) {
        return true;
    }
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| query.contains(name))
}

pub(super) fn is_command_tool(name: &str) -> bool {
    name == "shell.exec"
        || name == "process.run"
        || name == "verify.run"
        || name.starts_with("git.")
}

/// 证据文本的收紧界：outcome / 参数摘要不需要事实表级别的长度。
fn bound_evidence_text(text: &str) -> String {
    text.chars().take(EVIDENCE_TEXT_CHARS).collect()
}

fn observation_outcome_digest(output: &ToolOutput) -> String {
    if let Some(digest) = output
        .artifact_ref
        .as_deref()
        .and_then(|reference| ArtifactLocator::parse_sealed(reference).ok())
        .and_then(|locator| locator.digest())
    {
        return digest.to_string();
    }
    let bytes = if output.model_content.is_empty() {
        output.summary.as_bytes()
    } else {
        output.model_content.as_bytes()
    };
    ContentDigest::sha256_bytes(bytes).to_string()
}

pub(super) fn bound_item(text: &str) -> String {
    text.chars().take(MAX_TASK_ANCHOR_ITEM_CHARS).collect()
}
