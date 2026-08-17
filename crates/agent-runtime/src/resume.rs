//! Actor-owned `ResumePoint`: operational facts bound to `task_id +
//! anchor_revision`. `TaskAnchor` remains the only task authority.

use agent_contracts::{
    MAX_TASK_ANCHOR_ITEM_CHARS, MAX_TASK_ANCHOR_LIST_ITEMS, TaskProgressView, ToolOutput,
};

const MAX_RESUME_FILES: usize = 32;
const MAX_RESUME_FAILURES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResumePoint {
    pub anchor_revision: u64,
    pub objective: String,
    pub blockers: Vec<String>,
    pub next_actions: Vec<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FailedCommandFact {
    pub tool_name: String,
    pub summary: String,
    pub turn: u64,
}

impl ResumePoint {
    pub fn observe_tool(&mut self, output: &ToolOutput, anchor_revision: u64, turn: u64) {
        self.anchor_revision = anchor_revision;
        if let Some(path) = output.file_path() {
            let digest = output.file_revision().unwrap_or("").to_string();
            self.upsert_file(path, digest, turn);
        }
        if output.ok {
            self.failed_commands
                .retain(|row| row.tool_name != output.tool_name);
            if is_verification_tool(&output.tool_name) {
                self.push_verification(output.summary.clone(), true, turn);
            }
        } else if is_command_tool(&output.tool_name) {
            self.push_failure(&output.tool_name, output.summary.clone(), turn);
        }
        self.cap();
    }

    pub fn set_objective(&mut self, text: impl Into<String>, anchor_revision: u64) {
        self.anchor_revision = anchor_revision;
        self.objective = bound_item(&text.into());
        self.cap();
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

    fn push_failure(&mut self, tool_name: &str, summary: String, turn: u64) {
        self.failed_commands
            .retain(|row| row.tool_name != tool_name);
        self.failed_commands.push(FailedCommandFact {
            tool_name: bound_item(tool_name),
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
        self.objective = bound_item(&self.objective);
        cap_list(&mut self.blockers);
        cap_list(&mut self.next_actions);
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

    pub fn view(&self) -> TaskProgressView {
        TaskProgressView {
            anchor_revision: self.anchor_revision,
            objective: self.objective.clone(),
            blockers: self.blockers.clone(),
            next_actions: self.next_actions.clone(),
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
                .map(|row| format!("{}:{}", row.tool_name, row.summary))
                .collect(),
        }
    }
}

pub(crate) fn validate_resume(resume: &ResumePoint) -> Result<(), String> {
    if resume.objective.chars().count() > MAX_TASK_ANCHOR_ITEM_CHARS {
        return Err("resume objective exceeds the item cap".into());
    }
    if resume.blockers.len() > MAX_TASK_ANCHOR_LIST_ITEMS
        || resume.next_actions.len() > MAX_TASK_ANCHOR_LIST_ITEMS
        || resume.checked_files.len() > MAX_RESUME_FILES
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

fn is_verification_tool(name: &str) -> bool {
    name == "shell.exec" || name == "process.run"
}

fn bound_item(text: &str) -> String {
    text.chars().take(MAX_TASK_ANCHOR_ITEM_CHARS).collect()
}

fn cap_list(items: &mut Vec<String>) {
    for item in items.iter_mut() {
        *item = bound_item(item);
    }
    if items.len() > MAX_TASK_ANCHOR_LIST_ITEMS {
        items.truncate(MAX_TASK_ANCHOR_LIST_ITEMS);
    }
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
    fn success_clears_the_matching_failed_command() {
        let mut resume = ResumePoint::default();
        let mut fail = output("shell.exec", false, "exit 1");
        fail.metadata = json!({});
        resume.observe_tool(&fail, 1, 2);
        assert_eq!(resume.failed_commands.len(), 1);
        let mut ok = output("shell.exec", true, "exit 0");
        ok.metadata = json!({});
        resume.observe_tool(&ok, 1, 3);
        assert!(resume.failed_commands.is_empty());
        assert!(resume.verifications.last().unwrap().ok);
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
