//! Actor-owned `ResumePoint`: a bounded operational cache bound to
//! `task_id + anchor_revision + workspace_revision`. `TaskAnchor` remains
//! the only task authority. Verification is a stamped fact, orthogonal to
//! whether the producing operation mutated the workspace.

use agent_contracts::{
    MAX_TASK_ANCHOR_ITEM_CHARS, TaskProgressView, ToolOutput, ToolResultDisposition, TurnFrame,
    TurnFrameStep,
};

const MAX_RESUME_FILES: usize = 32;
const MAX_RESUME_FAILURES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResumePoint {
    pub anchor_revision: u64,
    /// Monotonic world clock. Bumped on any may-mutate observation.
    /// Verification facts bind to this value and do not auto-promote.
    #[serde(default)]
    pub workspace_revision: u64,
    pub checked_files: Vec<CheckedFileFact>,
    pub verifications: Vec<VerificationFact>,
    pub failed_commands: Vec<FailedCommandFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckedFileFact {
    pub path: String,
    pub digest: String,
    pub turn: u64,
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

impl ResumePoint {
    pub fn observe_tool(&mut self, output: &ToolOutput, anchor_revision: u64, turn: u64) {
        self.anchor_revision = anchor_revision;
        let touches = output.resource_touches();
        if output.may_mutate_workspace() {
            self.workspace_revision = self.workspace_revision.saturating_add(1);
            // Pathless mutation (shell/process without a ResourceTouch)
            // may have changed any file; drop stale path@revision facts.
            // A stamped write/patch updates only the touched paths.
            if touches.is_empty() {
                self.checked_files.clear();
            }
        }
        let identity = operation_identity(output);
        if output.ok {
            for touch in &touches {
                self.upsert_file(
                    &touch.path,
                    touch.revision.clone().unwrap_or_default(),
                    turn,
                );
            }
            self.failed_commands
                .retain(|row| !same_operation(row, &identity));
            if output.is_verification() {
                self.push_verification(output.summary.clone(), true, turn);
            }
        } else if let Some(touch) = touches.first() {
            self.push_failure(
                &identity,
                format!("failed observation {}", touch.path),
                turn,
            );
        } else if is_command_tool(&output.tool_name) {
            self.push_failure(&identity, output.summary.clone(), turn);
        }
        self.cap();
    }

    /// Prompt-only view of this cache plus the open turn's persistable
    /// tool results. The stored `ResumePoint` is unchanged; durable
    /// `observe_tool` still runs after the turn commit barrier.
    pub(crate) fn project_from_turn(
        &self,
        turn: &TurnFrame,
        anchor_revision: u64,
        turn_number: u64,
    ) -> TaskProgressView {
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
        projected.view()
    }

    fn upsert_file(&mut self, path: &str, digest: String, turn: u64) {
        let path = bound_item(path);
        if let Some(existing) = self.checked_files.iter_mut().find(|row| row.path == path) {
            existing.digest = bound_item(&digest);
            existing.turn = turn;
            return;
        }
        self.checked_files.push(CheckedFileFact {
            path,
            digest: bound_item(&digest),
            turn,
        });
    }

    fn push_failure(&mut self, identity: &OperationIdentity, summary: String, turn: u64) {
        self.failed_commands
            .retain(|row| !same_operation(row, identity));
        self.failed_commands.push(FailedCommandFact {
            tool_name: bound_item(&identity.tool_name),
            target: bound_item(&identity.target),
            summary: bound_item(&summary),
            turn,
        });
    }

    fn push_verification(&mut self, summary: String, ok: bool, turn: u64) {
        self.verifications.push(VerificationFact {
            summary: bound_item(&summary),
            ok,
            turn,
            anchor_revision: self.anchor_revision,
            workspace_revision: self.workspace_revision,
        });
    }

    fn cap(&mut self) {
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

    fn current_verifications(&self) -> impl Iterator<Item = &VerificationFact> {
        self.verifications.iter().filter(|row| {
            row.anchor_revision == self.anchor_revision
                && row.workspace_revision == self.workspace_revision
        })
    }

    pub fn view(&self) -> TaskProgressView {
        TaskProgressView {
            anchor_revision: self.anchor_revision,
            workspace_revision: self.workspace_revision,
            checked_files: self
                .checked_files
                .iter()
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
}

struct OperationIdentity {
    tool_name: String,
    target: String,
}

fn operation_identity(output: &ToolOutput) -> OperationIdentity {
    let target = output.operation_target().unwrap_or("").to_string();
    OperationIdentity {
        tool_name: output.tool_name.clone(),
        target,
    }
}

fn same_operation(row: &FailedCommandFact, identity: &OperationIdentity) -> bool {
    row.tool_name == identity.tool_name && row.target == identity.target
}

pub(crate) fn validate_resume(resume: &ResumePoint) -> Result<(), String> {
    if resume.checked_files.len() > MAX_RESUME_FILES
        || resume.verifications.len() > MAX_RESUME_FAILURES
        || resume.failed_commands.len() > MAX_RESUME_FAILURES
    {
        return Err("resume list exceeds its cap".into());
    }
    Ok(())
}

fn is_command_tool(name: &str) -> bool {
    name == "shell.exec" || name == "process.run" || name.starts_with("git.")
}

fn bound_item(text: &str) -> String {
    text.chars().take(MAX_TASK_ANCHOR_ITEM_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ToolOutput, ToolResultDisposition, TurnFrame};
    use serde_json::json;

    fn output(name: &str, ok: bool, summary: &str) -> ToolOutput {
        ToolOutput {
            call_id: "c".into(),
            tool_name: name.into(),
            ok,
            summary: summary.into(),
            model_content: summary.into(),
            artifact_ref: None,
            metadata: json!({
                "path": "src/auth.rs",
                "revision": "abc123",
            }),
        }
    }

    #[test]
    fn later_file_observation_replaces_the_stale_digest() {
        let mut resume = ResumePoint::default();
        resume.observe_tool(&output("fs.read", true, "read auth"), 1, 3);
        let mut newer = output("fs.read", true, "read auth");
        newer.metadata = json!({"path": "src/auth.rs", "revision": "def456"});
        resume.observe_tool(&newer, 1, 4);
        assert_eq!(resume.checked_files.len(), 1);
        assert_eq!(resume.checked_files[0].digest, "def456");
    }

    #[test]
    fn failed_file_observation_is_not_checked() {
        let mut resume = ResumePoint::default();
        resume.observe_tool(&output("fs.read", false, "missing"), 1, 2);
        assert!(resume.checked_files.is_empty());
        assert_eq!(resume.failed_commands.len(), 1);
        assert_eq!(resume.failed_commands[0].target, "src/auth.rs");
    }

    #[test]
    fn success_clears_only_the_matching_failed_command() {
        let mut resume = ResumePoint::default();
        let mut fail = output("shell.exec", false, "exit 1");
        fail.metadata = json!({"command": "cargo test"});
        resume.observe_tool(&fail, 1, 2);
        let mut other = output("shell.exec", true, "exit 0");
        other.metadata = json!({"command": "dir"});
        resume.observe_tool(&other, 1, 3);
        assert_eq!(resume.failed_commands.len(), 1);
        assert_eq!(resume.failed_commands[0].target, "cargo test");
        assert!(resume.view().verifications.is_empty());
        let mut ok = output("shell.exec", true, "exit 0");
        ok.metadata = json!({"command": "cargo test", "verification": true});
        resume.observe_tool(&ok, 1, 4);
        assert!(resume.failed_commands.is_empty());
        assert!(
            resume
                .view()
                .verifications
                .last()
                .unwrap()
                .starts_with("ok:")
        );
    }

    #[test]
    fn ls_is_not_a_verification_and_still_mutates() {
        let mut resume = ResumePoint::default();
        resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
        let mut ls = output("shell.exec", true, "exit 0");
        ls.metadata = json!({"command": "ls"});
        resume.observe_tool(&ls, 1, 2);
        assert!(resume.verifications.is_empty());
        assert_eq!(resume.workspace_revision, 1);
        assert!(resume.checked_files.is_empty());
    }

    #[test]
    fn cargo_test_command_is_not_a_verification_without_typed_metadata() {
        let mut resume = ResumePoint::default();
        let mut test = output("shell.exec", true, "exit 0");
        test.metadata = json!({"command": "cargo test -p agent-runtime"});
        resume.observe_tool(&test, 1, 1);
        assert!(resume.verifications.is_empty());
        assert_eq!(resume.workspace_revision, 1);
    }

    #[test]
    fn typed_verification_does_not_keep_an_old_pass_after_mutation() {
        let mut resume = ResumePoint::default();
        let mut test = output("shell.exec", true, "exit 0");
        test.metadata = json!({"command": "cargo test", "verification": true});
        resume.observe_tool(&test, 1, 1);
        assert_eq!(resume.view().verifications.len(), 1);

        let mut write = output("fs.write", true, "wrote");
        write.metadata = json!({"path": "src/auth.rs", "revision": "zzz"});
        resume.observe_tool(&write, 1, 2);
        assert!(
            resume.view().verifications.is_empty(),
            "old PASS must not cover a later workspace revision: {:?}",
            resume.view().verifications
        );
        assert_eq!(resume.workspace_revision, 2);
        assert_eq!(resume.verifications.len(), 1);
    }

    #[test]
    fn anchor_revision_change_does_not_promote_old_pass() {
        let mut resume = ResumePoint::default();
        let mut test = output("shell.exec", true, "exit 0");
        test.metadata = json!({"command": "cargo test", "verification": true});
        resume.observe_tool(&test, 1, 1);
        resume.observe_tool(&output("fs.read", true, "read auth"), 2, 2);
        assert!(resume.view().verifications.is_empty());
        assert_eq!(resume.anchor_revision, 2);
    }

    #[test]
    fn thousands_of_file_observations_stay_capped() {
        let mut resume = ResumePoint::default();
        for index in 0..2000 {
            let mut row = output("fs.read", true, "read");
            row.metadata =
                json!({"path": format!("src/f{index}.rs"), "revision": format!("{index}")});
            resume.observe_tool(&row, 1, index);
        }
        assert_eq!(resume.checked_files.len(), MAX_RESUME_FILES);
        validate_resume(&resume).unwrap();
    }

    #[test]
    fn patch_files_array_updates_checked_paths_without_wiping_others() {
        let mut resume = ResumePoint::default();
        resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
        let mut patch = output("edit.patch", true, "applied");
        patch.metadata = json!({
            "files": [
                {"path": "src/billing.rs", "revision": "bb"},
                {"path": "src/auth.rs", "revision": "aa2"}
            ]
        });
        resume.observe_tool(&patch, 1, 2);
        assert_eq!(resume.workspace_revision, 1);
        assert_eq!(resume.checked_files.len(), 2);
        let auth = resume
            .checked_files
            .iter()
            .find(|row| row.path == "src/auth.rs")
            .expect("auth stays checked");
        assert_eq!(auth.digest, "aa2");
        let billing = resume
            .checked_files
            .iter()
            .find(|row| row.path == "src/billing.rs")
            .expect("patch stamps billing");
        assert_eq!(billing.digest, "bb");
        let view = resume.view();
        assert!(
            view.checked_files
                .iter()
                .any(|row| row == "src/auth.rs@aa2")
        );
        assert!(
            view.checked_files
                .iter()
                .any(|row| row == "src/billing.rs@bb")
        );
    }

    #[test]
    fn open_turn_projection_shows_path_revision_without_persisting() {
        let mut resume = ResumePoint::default();
        resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
        let mut patch = output("edit.patch", true, "applied");
        patch.metadata = json!({
            "files": [
                {"path": "src/billing.rs", "revision": "bb"},
                {"path": "src/auth.rs", "revision": "aa2"}
            ]
        });
        let mut turn = TurnFrame::new("continue");
        turn.push_tool_result(patch, None);
        let before = resume.clone();
        let view = resume.project_from_turn(&turn, 1, 2);
        assert_eq!(resume, before, "projection must not persist");
        assert_eq!(resume.workspace_revision, 0);
        assert_eq!(view.workspace_revision, 1);
        assert!(
            view.checked_files
                .iter()
                .any(|row| row == "src/auth.rs@aa2")
        );
        assert!(
            view.checked_files
                .iter()
                .any(|row| row == "src/billing.rs@bb")
        );
    }

    #[test]
    fn open_turn_projection_skips_transient_results() {
        let resume = ResumePoint::default();
        let mut fetch = output("context.fetch", true, "body");
        fetch.metadata = json!({"path": "src/secret.rs", "revision": "ss"});
        let mut turn = TurnFrame::new("continue");
        turn.push_tool_result_with(fetch, None, ToolResultDisposition::TransientNoPersist);
        let view = resume.project_from_turn(&turn, 1, 2);
        assert!(view.is_empty());
        assert!(resume.checked_files.is_empty());
    }

    #[test]
    fn open_turn_pathless_mutation_clears_projected_checked_files() {
        let mut resume = ResumePoint::default();
        resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
        let mut ls = output("shell.exec", true, "listed");
        ls.metadata = json!({"command": "ls", "mutates_workspace": true});
        let mut turn = TurnFrame::new("continue");
        turn.push_tool_result(ls, None);
        let view = resume.project_from_turn(&turn, 1, 2);
        assert!(view.checked_files.is_empty());
        assert_eq!(resume.checked_files.len(), 1);
    }

    #[test]
    fn dead_fields_are_ignored_on_old_checkpoints() {
        let value = json!({
            "anchor_revision": 3,
            "objective": "legacy",
            "blockers": ["x"],
            "next_actions": ["y"],
            "last_cursor": "shell.exec",
            "workspace_facts_stale": true,
            "checked_files": [],
            "verifications": [],
            "failed_commands": []
        });
        let resume: ResumePoint = serde_json::from_value(value).unwrap();
        assert_eq!(resume.anchor_revision, 3);
        assert!(resume.view().is_empty());
    }
}
