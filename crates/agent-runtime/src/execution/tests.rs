//! Algorithm tests for ExecutionState (formerly ResumePoint).

use super::state::{MAX_RESUME_FILES, VerificationState};
use super::*;
use agent_contracts::{
    ResourceFreshness, ResourceVersionOracle, ToolOutput, ToolResultDisposition, TurnFrame,
};
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
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 3);
    let mut newer = output("fs.read", true, "read auth");
    newer.metadata = json!({"path": "src/auth.rs", "revision": "def456"});
    resume.observe_tool(&newer, 1, 4);
    assert_eq!(resume.checked_files.len(), 1);
    assert_eq!(resume.checked_files[0].digest, "def456");
}

#[test]
fn failed_file_observation_is_not_checked() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", false, "missing"), 1, 2);
    assert!(resume.checked_files.is_empty());
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(resume.failed_commands[0].target, "src/auth.rs");
}

#[test]
fn success_clears_only_the_matching_failed_command() {
    let mut resume = ExecutionState::default();
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
fn ls_is_not_a_verification_and_keeps_resource_facts() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut ls = output("shell.exec", true, "exit 0");
    ls.metadata = json!({"command": "ls"});
    resume.observe_tool(&ls, 1, 2);
    assert!(resume.verifications.is_empty());
    assert_eq!(resume.workspace_revision, 1);
    assert_eq!(resume.checked_files.len(), 1);
    assert_eq!(resume.checked_files[0].path, "src/auth.rs");
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::NeedsRevalidation
    );
    assert!(
        resume
            .view()
            .checked_files
            .iter()
            .any(|row| row == "src/auth.rs@abc123")
    );
    assert!(!resume.verification_due());
}

#[test]
fn cargo_test_command_is_not_a_verification_without_typed_metadata() {
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test -p agent-runtime"});
    resume.observe_tool(&test, 1, 1);
    assert!(resume.verifications.is_empty());
    assert_eq!(resume.workspace_revision, 1);
}

#[test]
fn typed_verification_does_not_keep_an_old_pass_after_mutation() {
    let mut resume = ExecutionState::default();
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
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    resume.observe_tool(&output("fs.read", true, "read auth"), 2, 2);
    assert!(resume.view().verifications.is_empty());
    assert_eq!(resume.anchor_revision, 2);
}

#[test]
fn thousands_of_file_observations_stay_capped() {
    let mut resume = ExecutionState::default();
    for index in 0..2000 {
        let mut row = output("fs.read", true, "read");
        row.metadata = json!({"path": format!("src/f{index}.rs"), "revision": format!("{index}")});
        resume.observe_tool(&row, 1, index);
    }
    assert_eq!(resume.checked_files.len(), MAX_RESUME_FILES);
    validate_resume(&resume).unwrap();
}

#[test]
fn patch_files_array_updates_checked_paths_without_wiping_others() {
    let mut resume = ExecutionState::default();
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
    let mut resume = ExecutionState::default();
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
    let resume = ExecutionState::default();
    let mut fetch = output("context.fetch", true, "body");
    fetch.metadata = json!({"path": "src/secret.rs", "revision": "ss"});
    let mut turn = TurnFrame::new("continue");
    turn.push_tool_result_with(fetch, None, ToolResultDisposition::TransientNoPersist);
    let view = resume.project_from_turn(&turn, 1, 2);
    assert!(view.is_empty());
    assert!(resume.checked_files.is_empty());
}

#[test]
fn open_turn_unknown_mutation_keeps_projected_checked_files() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut ls = output("shell.exec", true, "listed");
    ls.metadata = json!({"command": "ls", "mutates_workspace": true});
    let mut turn = TurnFrame::new("continue");
    turn.push_tool_result(ls, None);
    let view = resume.project_from_turn(&turn, 1, 2);
    assert!(
        view.checked_files
            .iter()
            .any(|row| row == "src/auth.rs@abc123"),
        "Unknown mutation must not wipe identity: {view:?}"
    );
    assert_eq!(resume.checked_files.len(), 1);
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::Fresh,
        "projection must not persist"
    );
    assert_eq!(view.workspace_revision, 1);
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
    let resume: ExecutionState = serde_json::from_value(value).unwrap();
    assert_eq!(resume.anchor_revision, 3);
    assert!(resume.view().is_empty());
}

struct MapOracle(std::collections::HashMap<String, Option<String>>);

#[async_trait::async_trait]
impl ResourceVersionOracle for MapOracle {
    async fn revision(&self, key: &str) -> agent_contracts::AgentResult<Option<String>> {
        Ok(self.0.get(key).cloned().flatten())
    }
}

#[tokio::test]
async fn unknown_shell_then_same_hash_is_fresh_again() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut ls = output("shell.exec", true, "exit 0");
    ls.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&ls, 1, 2);
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::NeedsRevalidation
    );
    let mut map = std::collections::HashMap::new();
    map.insert("src/auth.rs".into(), Some("abc123".into()));
    resume
        .revalidate(&MapOracle(map), "append scratch.md")
        .await;
    assert_eq!(resume.checked_files[0].freshness, ResourceFreshness::Fresh);
    assert_eq!(resume.checked_files[0].digest, "abc123");
    assert!(!resume.verification_due());
    assert!(
        resume
            .view()
            .checked_files
            .iter()
            .any(|row| row == "src/auth.rs@abc123")
    );
}

#[tokio::test]
async fn revalidate_changed_hash_marks_verification_stale() {
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    assert_eq!(resume.verification.state, VerificationState::Current);
    let mut ls = output("shell.exec", true, "exit 0");
    ls.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&ls, 1, 2);
    let mut map = std::collections::HashMap::new();
    map.insert("src/auth.rs".into(), Some("changed".into()));
    resume.revalidate(&MapOracle(map), "src/auth.rs").await;
    assert_eq!(resume.checked_files[0].digest, "changed");
    assert_eq!(resume.checked_files[0].freshness, ResourceFreshness::Fresh);
    assert_eq!(resume.verification.state, VerificationState::Stale);
    assert!(resume.verification_due());
}

#[tokio::test]
async fn revalidate_missing_path_drops_from_checked_view() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut ls = output("shell.exec", true, "exit 0");
    ls.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&ls, 1, 2);
    let mut map = std::collections::HashMap::new();
    map.insert("src/auth.rs".into(), None);
    resume.revalidate(&MapOracle(map), "").await;
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::Missing
    );
    assert!(resume.view().checked_files.is_empty());
}

#[test]
fn known_write_of_a_new_file_does_not_stale_current_verification() {
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    assert_eq!(resume.verification.state, VerificationState::Current);
    let mut write = output("fs.write", true, "wrote");
    write.metadata = json!({"path": "src/scratch.md", "revision": "note1"});
    resume.observe_tool(&write, 1, 2);
    assert_eq!(resume.verification.state, VerificationState::Current);
    assert!(!resume.verification_due());
    assert_eq!(resume.checked_files.len(), 2);
}

#[test]
fn known_write_of_an_existing_digest_stales_verification() {
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut write = output("fs.write", true, "wrote");
    write.metadata = json!({"path": "src/auth.rs", "revision": "zzz"});
    resume.observe_tool(&write, 1, 2);
    assert_eq!(resume.verification.state, VerificationState::Stale);
    assert!(resume.verification_due());
}

#[tokio::test]
async fn unknown_after_current_pass_stales_fact_but_same_hash_is_not_due() {
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut py = output("shell.exec", true, "exit 0");
    py.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&py, 1, 2);
    assert_eq!(resume.verification.state, VerificationState::Stale);
    assert!(resume.verification.unknown_pending);
    assert!(!resume.verification_due(), "identity not known-changed");
    let mut map = std::collections::HashMap::new();
    map.insert("src/auth.rs".into(), Some("abc123".into()));
    resume.revalidate(&MapOracle(map), "").await;
    assert_eq!(resume.checked_files[0].freshness, ResourceFreshness::Fresh);
    assert!(!resume.verification.unknown_pending);
    assert!(!resume.verification_due());
}

#[tokio::test]
async fn recall_after_fix_note_turn_does_not_inherit_need_verify() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut edit = output("fs.write", true, "fixed");
    edit.metadata = json!({"path": "src/auth.rs", "revision": "revB"});
    resume.observe_tool(&edit, 1, 1);
    assert!(resume.verification_due());
    let mut py = output("shell.exec", true, "exit 0");
    py.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&py, 1, 1);
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::NeedsRevalidation
    );
    let mut map = std::collections::HashMap::new();
    map.insert("src/auth.rs".into(), Some("revB".into()));
    resume.revalidate(&MapOracle(map), "fix util").await;
    assert_eq!(resume.checked_files[0].digest, "revB");
    assert_eq!(resume.checked_files[0].freshness, ResourceFreshness::Fresh);
    assert!(
        resume.verification_due(),
        "same turn still saw a Known edit"
    );

    resume.on_user_turn();
    assert!(
        !resume.verification_due(),
        "T2 note turn must not inherit NeedVerify(util)"
    );
    assert!(
        resume
            .view()
            .checked_files
            .iter()
            .any(|row| row == "src/auth.rs@revB")
    );
}
