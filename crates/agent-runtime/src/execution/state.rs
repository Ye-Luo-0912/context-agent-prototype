//! Resource facts, verification facts, and failures. Checkpointable.

use std::path::Path;

use agent_contracts::{
    MAX_FOREGROUND_RESOURCES, MAX_TASK_ANCHOR_ITEM_CHARS, ResourceFreshness, ResourceKey,
    TaskProgressView, ToolFailureClass, ToolOutput, path_exactly_in_directive,
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

/// Deterministic progress classification of one tool result. Not a
/// planner: it only states what the world can prove changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RoundProgress {
    /// A successful mutation changed the world.
    Meaningful,
    /// New or updated facts/evidence without a world change.
    Evidence,
    /// A previously failed operation now succeeds (or a failure row
    /// cleared) without new facts.
    Control,
    /// Nothing above: the round produced no provable change.
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
            stall_warning: self.stall_warning(),
        }
    }

    /// Bounded deterministic stall message when the same operation
    /// signature produced no world progress for
    /// [`STALL_THRESHOLD`] consecutive rounds. Advisory only: the model
    /// still chooses the next action.
    pub(super) fn stall_warning(&self) -> Option<String> {
        if self.stall.consecutive_no_progress < STALL_THRESHOLD {
            return None;
        }
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
        Some(format!(
            "EXECUTION STALL: {} on {} repeated {} time(s) without world progress (last failure: {}). Choose another strategy or finish with the current state.",
            self.stall.tool, target, self.stall.consecutive_no_progress, failure
        ))
    }

    /// MOD-PROG-01 stall accounting: any progress resets the counter; a
    /// no-progress round increments it when the operation signature
    /// (tool + target + failure class) repeats.
    pub(super) fn update_stall(
        &mut self,
        identity: &OperationIdentity,
        failure: Option<ToolFailureClass>,
        progress: RoundProgress,
    ) {
        if progress != RoundProgress::None {
            self.stall = StallState::default();
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

pub(super) fn bound_item(text: &str) -> String {
    text.chars().take(MAX_TASK_ANCHOR_ITEM_CHARS).collect()
}
