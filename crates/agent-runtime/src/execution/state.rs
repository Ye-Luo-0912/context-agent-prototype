//! Resource facts, verification facts, and failures. Checkpointable.

use std::path::Path;

use agent_contracts::{
    MAX_FOREGROUND_RESOURCES, MAX_TASK_ANCHOR_ITEM_CHARS, ResourceFreshness, ResourceKey,
    TaskProgressView, ToolOutput, ToolResultDisposition, TurnFrame, TurnFrameStep,
    path_exactly_in_directive,
};

pub(crate) const MAX_RESUME_FILES: usize = 32;
pub(super) const MAX_RESUME_FAILURES: usize = 8;
pub(super) const MAX_REVALIDATE_PER_ROUND: usize = 8;
pub(super) const MAX_COVERAGE_PATHS: usize = 8;

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
    /// this round should not Prefer-verify.
    pub fn has_unmet_obligation(&self) -> bool {
        self.verification.failed_open
            || self.verification.source_changed
            || matches!(
                self.verification.state,
                VerificationState::Pending | VerificationState::Stale | VerificationState::Failed
            )
            || matches!(
                self.verification.cause,
                VerificationCause::SourceChanged
                    | VerificationCause::SpecChanged
                    | VerificationCause::FailureRepair
            )
            || self.current_verifications().any(|row| !row.ok)
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
        if self.verification.failed_open || self.current_verifications().any(|row| !row.ok) {
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
            VerificationCoverage::Workspace | VerificationCoverage::Unspecified => false,
        }
    }

    pub fn mark_spec_changed(&mut self) {
        if self.verification.state != VerificationState::Failed {
            self.verification.cause = VerificationCause::SpecChanged;
            if self.verification.state == VerificationState::Current {
                self.verification.state = VerificationState::Stale;
            } else if self.verification.state == VerificationState::NotRun {
                self.verification.state = VerificationState::Pending;
            }
        }
        self.verification.coverage = VerificationCoverage::Workspace;
    }

    pub(super) fn mark_source_changed(&mut self, path: &str) {
        self.verification.source_changed = true;
        if !matches!(self.verification.state, VerificationState::Failed) {
            self.verification.cause = VerificationCause::SourceChanged;
            self.verification.state = if matches!(
                self.verification.state,
                VerificationState::Current | VerificationState::Stale
            ) {
                VerificationState::Stale
            } else {
                VerificationState::Pending
            };
        }
        self.add_coverage_path(path);
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

    /// Prompt-only view of this cache plus the open turn's persistable
    /// tool results. The stored state is unchanged; durable `observe_tool`
    /// still runs after the turn commit barrier.
    pub(crate) fn project_from_turn(
        &self,
        turn: &TurnFrame,
        anchor_revision: u64,
        turn_number: u64,
    ) -> TaskProgressView {
        self.apply_open_turn(turn, anchor_revision, turn_number)
            .view()
    }

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
        }
    }

    pub(super) fn mark_facts_needs_revalidation(&mut self) {
        for fact in &mut self.checked_files {
            if fact.freshness != ResourceFreshness::Missing {
                fact.freshness = ResourceFreshness::NeedsRevalidation;
            }
        }
    }

    pub(super) fn upsert_file(&mut self, path: &str, digest: String, turn: u64) {
        let path = bound_item(path);
        let digest = bound_item(&digest);
        let digest_changed = self.checked_files.iter().any(|row| {
            row.path == path && !row.digest.is_empty() && !digest.is_empty() && row.digest != digest
        });
        if digest_changed {
            self.mark_source_changed(&path);
        }
        if let Some(existing) = self.checked_files.iter_mut().find(|row| row.path == path) {
            existing.digest = digest;
            existing.turn = turn;
            existing.freshness = ResourceFreshness::Fresh;
            return;
        }
        self.checked_files.push(ResourceFact {
            path,
            digest,
            freshness: ResourceFreshness::Fresh,
            turn,
        });
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

    pub(super) fn push_verification(&mut self, summary: String, ok: bool, turn: u64) {
        self.verifications.push(VerificationFact {
            summary: bound_item(&summary),
            ok,
            turn,
            anchor_revision: self.anchor_revision,
            workspace_revision: self.workspace_revision,
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
