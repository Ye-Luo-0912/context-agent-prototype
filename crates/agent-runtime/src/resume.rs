//! Actor-owned `ResumePoint`: a bounded operational cache bound to
//! `task_id + anchor_revision`. `TaskAnchor` remains the only task
//! authority. Objective / blockers / next-actions are not writable here.

use agent_contracts::{
    MAX_TASK_ANCHOR_ITEM_CHARS, MAX_TASK_ANCHOR_LIST_ITEMS, TaskProgressView, ToolOutput,
};

const MAX_RESUME_FILES: usize = 32;
const MAX_RESUME_FAILURES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResumePoint {
    pub anchor_revision: u64,
    /// Legacy checkpoint fields. Never written; TaskAnchor owns goal state.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub objective: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<String>,
    pub checked_files: Vec<CheckedFileFact>,
    pub verifications: Vec<VerificationFact>,
    pub failed_commands: Vec<FailedCommandFact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cursor: Option<String>,
    #[serde(default)]
    pub workspace_facts_stale: bool,
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
        let identity = operation_identity(output);
        if output.ok {
            if let Some(path) = output.file_path() {
                let digest = output.file_revision().unwrap_or("").to_string();
                self.upsert_file(path, digest, turn);
            } else if is_generic_mutator(&output.tool_name) && !output.is_verification() {
                self.invalidate_workspace_facts();
            }
            self.failed_commands
                .retain(|row| !same_operation(row, &identity));
            if output.is_verification() {
                self.push_verification(output.summary.clone(), true, turn);
            }
            self.last_cursor = Some(bound_item(&identity.cursor));
        } else {
            if let Some(path) = output.file_path() {
                self.push_failure(&identity, format!("failed observation {path}"), turn);
            } else if is_command_tool(&output.tool_name) {
                self.push_failure(&identity, output.summary.clone(), turn);
            }
        }
        self.cap();
    }

    fn invalidate_workspace_facts(&mut self) {
        self.workspace_facts_stale = true;
        self.checked_files.clear();
    }

    fn upsert_file(&mut self, path: &str, digest: String, turn: u64) {
        let path = bound_item(path);
        self.workspace_facts_stale = false;
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
        if let Some(cursor) = &self.last_cursor {
            self.last_cursor = Some(bound_item(cursor));
        }
    }

    pub fn view(&self) -> TaskProgressView {
        TaskProgressView {
            anchor_revision: self.anchor_revision,
            objective: String::new(),
            blockers: Vec::new(),
            next_actions: Vec::new(),
            checked_files: self
                .checked_files
                .iter()
                .map(|row| format!("{}@{}", row.path, row.digest))
                .collect(),
            verifications: self
                .verifications
                .iter()
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
    cursor: String,
}

fn operation_identity(output: &ToolOutput) -> OperationIdentity {
    let target = output.operation_target().unwrap_or("").to_string();
    let cursor = if target.is_empty() {
        output.tool_name.clone()
    } else {
        format!("{} {target}", output.tool_name)
    };
    OperationIdentity {
        tool_name: output.tool_name.clone(),
        target,
        cursor,
    }
}

fn same_operation(row: &FailedCommandFact, identity: &OperationIdentity) -> bool {
    row.tool_name == identity.tool_name && row.target == identity.target
}

pub(crate) fn validate_resume(resume: &ResumePoint) -> Result<(), String> {
    if resume.checked_files.len() > MAX_RESUME_FILES
        || resume.verifications.len() > MAX_RESUME_FAILURES
        || resume.failed_commands.len() > MAX_RESUME_FAILURES
        || resume.blockers.len() > MAX_TASK_ANCHOR_LIST_ITEMS
        || resume.next_actions.len() > MAX_TASK_ANCHOR_LIST_ITEMS
    {
        return Err("resume list exceeds its cap".into());
    }
    Ok(())
}

fn is_command_tool(name: &str) -> bool {
    name == "shell.exec" || name == "process.run" || name.starts_with("git.")
}

fn is_generic_mutator(name: &str) -> bool {
    name == "shell.exec" || name == "process.run"
}

fn bound_item(text: &str) -> String {
    text.chars().take(MAX_TASK_ANCHOR_ITEM_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ToolOutput;
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
        assert!(resume.verifications.is_empty());
        let mut ok = output("shell.exec", true, "exit 0");
        ok.metadata = json!({"command": "cargo test", "verification": true});
        resume.observe_tool(&ok, 1, 4);
        assert!(resume.failed_commands.is_empty());
        assert!(resume.verifications.last().unwrap().ok);
    }

    #[test]
    fn ls_is_not_a_verification() {
        let mut resume = ResumePoint::default();
        let mut ls = output("shell.exec", true, "exit 0");
        ls.metadata = json!({"command": "ls"});
        resume.observe_tool(&ls, 1, 1);
        assert!(resume.verifications.is_empty());
        assert!(resume.workspace_facts_stale);
        assert!(resume.checked_files.is_empty());
    }

    #[test]
    fn cargo_test_command_is_a_verification() {
        let mut resume = ResumePoint::default();
        let mut test = output("shell.exec", true, "exit 0");
        test.metadata = json!({"command": "cargo test -p agent-runtime"});
        resume.observe_tool(&test, 1, 1);
        assert_eq!(resume.verifications.len(), 1);
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
}
