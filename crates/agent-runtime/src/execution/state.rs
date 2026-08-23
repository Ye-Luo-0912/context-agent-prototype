//! Resource facts, verification facts, and failures. Checkpointable.

use std::path::Path;

use agent_contracts::{
    MAX_FOREGROUND_RESOURCES, MAX_TASK_ANCHOR_ITEM_CHARS, EvidenceValidity, ExecutionEvidence,
    FrontierDelta, ResourceFreshness, ResourceKey, TaskProgressView, ToolFailureClass, ToolOutput,
    path_exactly_in_directive,
};
#[cfg(test)]
use agent_contracts::{ToolResultDisposition, TurnFrame, TurnFrameStep};

pub(crate) const MAX_RESUME_FILES: usize = 32;
pub(super) const MAX_RESUME_FAILURES: usize = 8;
pub(super) const MAX_REVALIDATE_PER_ROUND: usize = 8;
pub(super) const MAX_COVERAGE_PATHS: usize = 8;
/// Consecutive identical no-progress rounds before the runtime tells the
/// model its repeated behavior is not moving the world (MOD-PROG-01).
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

/// 一轮工具观察的确定性前沿分类结果，随 `ExecutionFrontier` 事件上报。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontierObservation {
    pub delta: FrontierDelta,
    pub actions_since_frontier_advance: u32,
    pub evidence_revision: u64,
    pub invalidated: u64,
}

/// [`ExecutionState::record_observation_evidence`] 的三值结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObservationEvidence {
    /// 新证据或证据内容变化。
    Advanced,
    /// 同 key 同 validity 同结果的重复观察。
    Repeated,
    /// 该输出不产生前沿证据（无可键化路径且非命令成功）。
    None,
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
/// fact is believed (MOD-OBS-01: effect, observation, and attention are
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
    /// Artifact locator of the verification output, when the tool retained one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
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

pub(super) struct OperationIdentity {
    pub tool_name: String,
    pub target: String,
}

impl ExecutionState {
    /// New user directive: TurnIntent is replaced by the caller. Verification
    /// obligation is not wiped here — whether Verify is due this round is
    /// [`Self::verification_due_now`].
    pub fn on_user_turn(&mut self) {}

    /// Persistent obligation: something still needs a verification, even if
    /// this round should not Prefer-verify. Derived from
    /// [`Self::validity`], not from independently toggled bools.
    pub fn has_unmet_obligation(&self) -> bool {
        matches!(
            self.validity(),
            VerificationState::Pending | VerificationState::Stale | VerificationState::Failed
        ) || self.last_evidence().is_some_and(|row| !row.ok)
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
        let Some(_last) = self.last_evidence() else {
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
        {
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

    fn turn_requests_complete(message: &str) -> bool {
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

    pub fn fact_for(&self, path: &str) -> Option<&ResourceFact> {
        self.checked_files.iter().find(|row| row.path == path)
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
                .map(|row| {
                    if row.target.is_empty() {
                        format!("{}:{}", row.tool_name, row.summary)
                    } else {
                        format!("{} {}:{}", row.tool_name, row.target, row.summary)
                    }
                })
                .collect(),
            operational_evidence: self.evidence_rows(),
            stall_warning: self.stall_warning(),
            frontier_warning: self.frontier_warning(),
        }
    }

    /// 类型化证据行，最新在前、有界。只含 key + 结果 + world 版本；
    /// Resource 有效性附带 path@digest 身份。无任何正文。
    fn evidence_rows(&self) -> Vec<String> {
        self.evidence
            .iter()
            .take(6)
            .map(|row| match &row.validity {
                EvidenceValidity::Resource { digest, .. } if !digest.is_empty() => {
                    format!("{}@{}: {} @ world={}", row.key, digest, row.outcome, row.observed_world_revision)
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
        self.convergence.actions_since_frontier_advance =
            self.convergence.actions_since_frontier_advance.saturating_add(1);
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
                    self.failure_cluster.tried_targets.push(identity.target.clone());
                }
            } else {
                self.failure_cluster = FailureCluster {
                    tool: identity.tool_name.clone(),
                    failure: Some(class),
                    tried_targets: vec![identity.target.clone()],
                };
            }
        }
        // 逐签名停滞只在重复行为（NoProgress / RedundantEvidence）下
        // 累计；无失败的未知失效只记债务，不冒充停滞。
        if !matches!(
            delta,
            FrontierDelta::NoProgress | FrontierDelta::RedundantEvidence
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
        let excess = self.convergence.recent_deltas.len().saturating_sub(MAX_RECENT_DELTAS);
        if excess > 0 {
            self.convergence.recent_deltas.drain(0..excess);
        }
    }

    /// world revision 推进后使版本绑定的证据过期。返回失效条数——
    /// 这是"证据因世界变化而死亡"的可解释计数，不是知识损失：事实表
    /// 与事件流仍在。
    pub(super) fn invalidate_stale_evidence(&mut self) -> u64 {
        let revision = self.workspace_revision;
        let before = self.evidence.len();
        self.evidence.retain(|row| match row.validity {
            EvidenceValidity::WorkspaceRevision(at) => at >= revision,
            EvidenceValidity::Turn | EvidenceValidity::Resource { .. } => true,
        });
        let invalidated = (before - self.evidence.len()) as u64;
        if invalidated > 0 {
            self.convergence.evidence_revision = self.convergence.evidence_revision.saturating_add(invalidated);
        }
        invalidated
    }

    /// 成功观察入前沿（评审第 9/15 条）。键化规则：
    /// - `git.status` / `git.diff` / `git.log`：key=工具名，
    ///   validity=`WorkspaceRevision(当前)`；
    /// - 其他带 path 的成功读：key=`工具:path`，
    ///   validity=`Resource{path,digest}`；
    /// - 成功命令运行（未知足迹）：key=`工具:参数摘要`，
    ///   validity=`WorkspaceRevision(当前)`。
    ///
    /// 同 key 同 validity 同结果同参数即重复；否则插入为新证据。
    pub(super) fn record_observation_evidence(
        &mut self,
        output: &ToolOutput,
        turn: u64,
    ) -> ObservationEvidence {
        let target = output.operation_target().unwrap_or("").to_string();
        let is_git_read = matches!(output.tool_name.as_str(), "git.status" | "git.diff" | "git.log");
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
        let argument_digest = bound_evidence_text(&target);
        if let Some(existing) = self
            .evidence
            .iter_mut()
            .find(|row| row.key == key)
        {
            let identical = existing.validity == validity
                && existing.outcome == outcome
                && existing.argument_digest == argument_digest;
            if identical {
                return ObservationEvidence::Repeated;
            }
            existing.outcome = outcome;
            existing.observed_world_revision = self.workspace_revision;
            existing.validity = validity;
            existing.argument_digest = argument_digest;
            existing.turn = turn;
            existing.evidence_ref = output.artifact_ref.clone();
            self.bump_evidence_revision();
            return ObservationEvidence::Advanced;
        }
        self.evidence.insert(
            0,
            ExecutionEvidence {
                key,
                outcome,
                observed_world_revision: self.workspace_revision,
                validity,
                argument_digest,
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

    /// Upsert one resource fact. Returns whether the observation changed
    /// anything (new row, new digest, or freshness improvement) — the
    /// deterministic progress vector consumes that signal.
    pub(super) fn upsert_file(
        &mut self,
        path: &str,
        digest: String,
        turn: u64,
        provenance: ResourceProvenance,
    ) -> bool {
        let path = bound_item(path);
        let digest = bound_item(&digest);
        let digest_changed = self.checked_files.iter().any(|row| {
            row.path == path && !row.digest.is_empty() && !digest.is_empty() && row.digest != digest
        });
        if digest_changed {
            self.mark_source_changed(&path);
        }
        if let Some(existing) = self.checked_files.iter_mut().find(|row| row.path == path) {
            let changed = digest_changed
                || existing.digest != digest
                || existing.freshness != ResourceFreshness::Fresh;
            existing.digest = digest;
            existing.turn = turn;
            existing.freshness = ResourceFreshness::Fresh;
            existing.provenance = provenance;
            return changed;
        }
        self.checked_files.push(ResourceFact {
            path,
            digest,
            freshness: ResourceFreshness::Fresh,
            turn,
            provenance,
        });
        true
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
        summary: String,
        ok: bool,
        turn: u64,
        evidence_ref: Option<String>,
    ) {
        self.verifications.push(VerificationFact {
            summary: bound_item(&summary),
            ok,
            turn,
            anchor_revision: self.anchor_revision,
            workspace_revision: self.workspace_revision,
            evidence_ref: evidence_ref.map(|value| bound_item(&value)),
        });
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
    }

    pub(super) fn current_verifications(&self) -> impl Iterator<Item = &VerificationFact> {
        self.verifications.iter().filter(|row| {
            row.anchor_revision == self.anchor_revision
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
    {
        return Err("resume list exceeds its cap".into());
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
    name == "shell.exec" || name == "process.run" || name.starts_with("git.")
}

/// 证据文本的收紧界：outcome / 参数摘要不需要事实表级别的长度。
fn bound_evidence_text(text: &str) -> String {
    text.chars().take(EVIDENCE_TEXT_CHARS).collect()
}

pub(super) fn bound_item(text: &str) -> String {
    text.chars().take(MAX_TASK_ANCHOR_ITEM_CHARS).collect()
}
