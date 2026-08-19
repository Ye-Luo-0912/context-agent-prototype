//! Resource facts, verification facts, and failures. Checkpointable.

use std::path::Path;

use agent_contracts::{
    MAX_TASK_ANCHOR_ITEM_CHARS, ResourceFreshness, TaskProgressView, ToolOutput,
    ToolResultDisposition, TurnFrame, TurnFrameStep,
};

pub(crate) const MAX_RESUME_FILES: usize = 32;
pub(super) const MAX_RESUME_FAILURES: usize = 8;
pub(super) const MAX_REVALIDATE_PER_ROUND: usize = 8;

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
    /// A Known digest change or revalidate mismatch since the last user
    /// turn. Cleared on `on_user_turn` so a later note turn does not
    /// inherit NeedVerify from an earlier edit.
    #[serde(default)]
    pub source_changed: bool,
    /// An Unknown footprint is awaiting identity revalidation. PASS is
    /// already omitted via `workspace_revision`; NeedVerify stays false
    /// unless `source_changed` or a failed verification is open.
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
    Current,
    Stale,
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
    /// New user directive: TurnIntent is replaced by the caller; this only
    /// clears per-turn source-change so NeedVerify does not leak into a
    /// later note turn.
    pub fn on_user_turn(&mut self) {
        self.verification.source_changed = false;
    }

    /// Prefer verification tools only when an obligation is actually due
    /// for this turn. Epoch-stale PASS (Unknown `__pycache__`) is not due
    /// once identity revalidation has not shown a source change.
    pub fn verification_due(&self) -> bool {
        self.verification.source_changed
            || self.verification.failed_open
            || self.current_verifications().any(|row| !row.ok)
    }

    pub fn has_failures(&self) -> bool {
        !self.failed_commands.is_empty()
    }

    pub fn fact_for(&self, path: &str) -> Option<&ResourceFact> {
        self.checked_files.iter().find(|row| row.path == path)
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
                .filter(|row| row.freshness != ResourceFreshness::Missing)
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

    pub(super) fn stale_current_verification(&mut self) {
        self.verification.source_changed = true;
        self.verification.state = VerificationState::Stale;
    }

    pub(super) fn upsert_file(&mut self, path: &str, digest: String, turn: u64) {
        let path = bound_item(path);
        let digest = bound_item(&digest);
        let digest_changed = self.checked_files.iter().any(|row| {
            row.path == path && !row.digest.is_empty() && !digest.is_empty() && row.digest != digest
        });
        if digest_changed {
            self.stale_current_verification();
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
