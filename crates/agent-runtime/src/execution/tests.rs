//! Algorithm tests for ExecutionState (formerly ResumePoint).

use super::state::{
    MAX_RESUME_FILES, MAX_REVALIDATE_PER_ROUND, VerificationCause, VerificationCoverage,
    VerificationState,
};
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

/// A refused mutation observed the file it refused to write: the trusted
/// path+revision stamp is a real observation (MOD-OBS-01).
fn refused_edit(path: &str, revision: &str, class: &str) -> ToolOutput {
    let mut refusal = output("edit.replace", false, "edit refused");
    refusal.metadata = json!({
        "path": path,
        "revision": revision,
        "failure_class": class,
    });
    refusal
}

#[test]
fn refused_mutation_stamps_the_observed_fact_without_bumping_the_world() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(
        &refused_edit("src/auth.rs", "abc123", "stale_revision"),
        1,
        2,
    );
    // The observation is trusted world truth: the fact table knows
    // src/auth.rs@abc123 is Fresh.
    assert_eq!(resume.checked_files.len(), 1);
    assert_eq!(resume.checked_files[0].path, "src/auth.rs");
    assert_eq!(resume.checked_files[0].digest, "abc123");
    assert_eq!(resume.checked_files[0].freshness, ResourceFreshness::Fresh);
    assert_eq!(
        resume.checked_files[0].provenance,
        ResourceProvenance::MutationRefusal
    );
    // The mutation did not apply: the world revision must not advance.
    assert_eq!(resume.workspace_revision, 0);
    // The failure itself is still recorded.
    assert_eq!(resume.failed_commands.len(), 1);
}

#[test]
fn refusal_observation_resolves_needs_revalidation_without_a_reread() {
    // MOD-OBS-01 headline: after an unknown mutation, a stale_revision
    // refusal already carries the current revision — NeedsRevalidation
    // resolves without the model burning an fs.read.
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut unknown = output("shell.exec", true, "exit 0");
    unknown.metadata = json!({"command": "cargo build"});
    resume.observe_tool(&unknown, 1, 2);
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::NeedsRevalidation
    );

    resume.observe_tool(
        &refused_edit("src/auth.rs", "abc123", "stale_revision"),
        1,
        3,
    );
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::Fresh,
        "the refusal's revision stamp revalidates the fact"
    );
    assert!(
        resume
            .view()
            .checked_files
            .contains(&"src/auth.rs@abc123".to_string())
    );
}

#[test]
fn refusal_observation_of_a_changed_digest_marks_the_source_changed() {
    // The file really moved on: the observation updates the digest, the
    // source-changed flag is set, and a verification obligation now
    // exists (Pending — there is no prior PASS evidence to mark Stale).
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    resume.observe_tool(
        &refused_edit("src/auth.rs", "def456", "stale_revision"),
        1,
        2,
    );
    assert_eq!(resume.checked_files[0].digest, "def456");
    assert!(resume.verification.source_changed);
    assert_eq!(resume.validity(), VerificationState::Pending);
}

#[test]
fn failed_process_execution_still_bumps_the_world_revision() {
    // process_exit may have partial side effects: it stays conservative
    // (Unknown footprint) and must not be deduplicated away.
    let mut resume = ExecutionState::default();
    let mut failed = output("shell.exec", false, "exit 1");
    failed.metadata = json!({"command": "cargo test", "failure_class": "process_exit"});
    resume.observe_tool(&failed, 1, 1);
    assert_eq!(resume.workspace_revision, 1);
    assert_eq!(
        resume.checked_files.len(),
        0,
        "a failed process produced no trusted resource observation"
    );
}

#[test]
fn repeated_identical_refusals_surface_the_stall_warning() {
    // MOD-PROG-01: the fact is already known at the stamped revision, so
    // each identical refusal is provably no-progress; three in a row
    // surface EXECUTION STALL.
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    for turn in 2..=3 {
        resume.observe_tool(
            &refused_edit("src/auth.rs", "abc123", "stale_revision"),
            1,
            turn,
        );
        assert!(
            resume.view().stall_warning.is_none(),
            "no warning before the threshold (turn {turn})"
        );
    }
    resume.observe_tool(
        &refused_edit("src/auth.rs", "abc123", "stale_revision"),
        1,
        4,
    );
    let warning = resume.view().stall_warning.expect("stall at 3 consecutive");
    assert!(warning.contains("EXECUTION STALL"));
    assert!(warning.contains("edit.replace"));
    assert!(warning.contains("stale_revision"));
}

#[test]
fn progress_resets_the_stall_counter() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    resume.observe_tool(
        &refused_edit("src/auth.rs", "abc123", "stale_revision"),
        1,
        2,
    );
    resume.observe_tool(
        &refused_edit("src/auth.rs", "abc123", "stale_revision"),
        1,
        3,
    );
    assert_eq!(resume.stall.consecutive_no_progress, 2);
    // A successful write moves the world: progress resets the counter.
    let mut write = output("fs.write", true, "wrote");
    write.metadata = json!({"path": "src/auth.rs", "revision": "def456"});
    resume.observe_tool(&write, 1, 4);
    assert_eq!(resume.stall.consecutive_no_progress, 0);
    // The next refusal (against the new revision) starts counting again.
    resume.observe_tool(
        &refused_edit("src/auth.rs", "def456", "stale_revision"),
        1,
        5,
    );
    assert_eq!(resume.stall.consecutive_no_progress, 1);
    assert!(resume.view().stall_warning.is_none());
}

#[test]
fn stall_signature_change_resets_the_counter() {
    // Alternating targets never accumulate: each signature counts alone.
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    resume.observe_tool(&refused_edit("src/a.rs", "rev-a", "stale_revision"), 1, 2);
    resume.observe_tool(&refused_edit("src/b.rs", "rev-b", "stale_revision"), 1, 3);
    resume.observe_tool(&refused_edit("src/a.rs", "rev-a", "stale_revision"), 1, 4);
    assert!(resume.view().stall_warning.is_none());
    assert_eq!(resume.stall.consecutive_no_progress, 1);
    assert_eq!(resume.stall.target, "src/a.rs");
}

#[test]
fn duplicate_no_progress_refusal_counts_toward_the_stall_counter() {
    // The runtime's own dedup refusal (nothing executed, no fresh
    // observation) is honest NoProgress: it strengthens the stall signal
    // instead of looping forever at zero cost.
    let mut resume = ExecutionState::default();
    let mut duplicate = output("edit.replace", false, "duplicate");
    duplicate.metadata = json!({
        "path": "src/auth.rs",
        "failure_class": "duplicate_no_progress",
        "executed": false,
    });
    for turn in 1..=3 {
        resume.observe_tool(&duplicate, 1, turn);
    }
    let warning = resume
        .view()
        .stall_warning
        .expect("dedup refusals must accumulate toward the stall");
    assert!(warning.contains("duplicate_no_progress"));
    // Nothing executed: the world never moved.
    assert_eq!(resume.workspace_revision, 0);
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
        resume.view().checked_files.is_empty(),
        "NeedsRevalidation must not render as Checked: {:?}",
        resume.view().checked_files
    );
    assert!(!resume.verification_due_now(""));
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
        view.checked_files.is_empty(),
        "NeedsRevalidation must not enter Checked: {view:?}"
    );
    assert_eq!(resume.checked_files.len(), 1);
    assert_eq!(
        resume.checked_files[0].freshness,
        ResourceFreshness::Fresh,
        "projection must not persist"
    );
    let projected = resume.apply_open_turn(&turn, 1, 2);
    assert_eq!(
        projected.checked_files[0].freshness,
        ResourceFreshness::NeedsRevalidation
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
    assert!(!resume.verification_due_now(""));
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
    assert!(resume.has_unmet_obligation());
    assert!(
        !resume.verification_due_now(""),
        "obligation is not automatically due"
    );
    assert!(resume.verification_due_now("src/auth.rs"));
    assert!(resume.verification_due_now("run the tests"));
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
    assert!(!resume.verification_due_now(""));
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
    assert!(resume.has_unmet_obligation());
    assert!(
        !resume.verification_due_now("Append to src/scratch.md"),
        "a note turn must not Prefer-verify"
    );
    assert!(resume.verification_due_now("src/auth.rs"));
    assert!(resume.verification_due_now("run the tests"));
}

#[tokio::test]
async fn unknown_after_current_pass_stales_workspace_coverage_even_when_tracked_hashes_match() {
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    assert_eq!(
        resume.verification.coverage,
        VerificationCoverage::Workspace
    );
    let mut py = output("shell.exec", true, "exit 0");
    py.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&py, 1, 2);
    assert_eq!(resume.verification.state, VerificationState::Stale);
    assert!(resume.verification.unknown_pending);
    assert!(
        !resume.verification_due_now(""),
        "identity not known-changed"
    );
    let mut map = std::collections::HashMap::new();
    map.insert("src/auth.rs".into(), Some("abc123".into()));
    resume.revalidate(&MapOracle(map), "").await;
    assert_eq!(resume.checked_files[0].freshness, ResourceFreshness::Fresh);
    assert!(
        resume.verification.unknown_pending,
        "Workspace coverage cannot recover from tracked-file identity"
    );
    assert_eq!(resume.validity(), VerificationState::Stale);
    assert!(resume.has_unmet_obligation());
    assert!(
        !resume.verification_due_now(""),
        "obligation is not automatically due"
    );
    assert!(resume.verification_due_now("run the tests"));
    assert!(resume.verification_due_now("complete the task"));
}

#[tokio::test]
async fn unknown_after_resources_coverage_recovers_when_covered_hashes_match() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut write = output("fs.write", true, "wrote");
    write.metadata = json!({"path": "src/auth.rs", "revision": "zzz"});
    resume.observe_tool(&write, 1, 2);
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 3);
    assert_eq!(
        resume.verification.coverage,
        VerificationCoverage::Resources(vec!["src/auth.rs".into()])
    );
    assert_eq!(resume.validity(), VerificationState::Current);
    let mut py = output("shell.exec", true, "exit 0");
    py.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&py, 1, 4);
    assert_eq!(resume.validity(), VerificationState::Stale);
    let mut map = std::collections::HashMap::new();
    map.insert("src/auth.rs".into(), Some("zzz".into()));
    resume.revalidate(&MapOracle(map), "").await;
    assert!(!resume.verification.unknown_pending);
    assert_eq!(resume.validity(), VerificationState::Current);
    assert!(!resume.has_unmet_obligation());
    assert!(!resume.verification_due_now(""));
}

#[tokio::test]
async fn recall_after_fix_note_turn_does_not_inherit_need_verify() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    let mut edit = output("fs.write", true, "fixed");
    edit.metadata = json!({"path": "src/auth.rs", "revision": "revB"});
    resume.observe_tool(&edit, 1, 1);
    assert!(resume.has_unmet_obligation());
    assert!(
        !resume.verification_due_now("Append to src/scratch.md"),
        "notes are not due"
    );
    assert!(resume.verification_due_now("src/auth.rs"));
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
        resume.has_unmet_obligation(),
        "Known edit still unmet after same-hash revalidate"
    );

    resume.on_user_turn();
    assert!(
        resume.has_unmet_obligation(),
        "on_user_turn must not wipe the obligation"
    );
    assert!(
        !resume.verification_due_now("Append to src/scratch.md: HDMI is in drawer 3"),
        "T2 note turn must not Prefer-verify"
    );
    assert!(resume.verification_due_now("run the tests"));
    assert!(
        resume
            .view()
            .checked_files
            .iter()
            .any(|row| row == "src/auth.rs@revB")
    );
}

#[tokio::test]
async fn pending_revalidation_past_the_round_cap_is_not_checked() {
    let mut resume = ExecutionState::default();
    for index in 0..20 {
        let mut row = output("fs.read", true, "read");
        row.metadata = json!({
            "path": format!("src/f{index}.rs"),
            "revision": format!("rev{index}")
        });
        resume.observe_tool(&row, 1, index);
    }
    let mut ls = output("shell.exec", true, "listed");
    ls.metadata = json!({"command": "python -c import visit_all"});
    resume.observe_tool(&ls, 1, 20);
    assert_eq!(resume.checked_files.len(), 20);
    assert!(
        resume
            .checked_files
            .iter()
            .all(|row| row.freshness == ResourceFreshness::NeedsRevalidation)
    );
    assert!(resume.view().checked_files.is_empty());

    let mut map = std::collections::HashMap::new();
    for index in 0..20 {
        map.insert(format!("src/f{index}.rs"), Some(format!("rev{index}")));
    }
    resume.revalidate(&MapOracle(map), "").await;
    let fresh = resume
        .checked_files
        .iter()
        .filter(|row| row.freshness == ResourceFreshness::Fresh)
        .count();
    let pending = resume
        .checked_files
        .iter()
        .filter(|row| row.freshness == ResourceFreshness::NeedsRevalidation)
        .count();
    assert_eq!(fresh, MAX_REVALIDATE_PER_ROUND);
    assert_eq!(pending, 20 - MAX_REVALIDATE_PER_ROUND);
    assert_eq!(resume.view().checked_files.len(), MAX_REVALIDATE_PER_ROUND);
}

#[test]
fn spec_change_keeps_obligation_without_due_on_notes() {
    let mut resume = ExecutionState::default();
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    resume.mark_spec_changed();
    resume.anchor_revision = 2;
    assert_eq!(resume.verification.state, VerificationState::Stale);
    assert_eq!(resume.verification.cause, VerificationCause::SpecChanged);
    assert_eq!(
        resume.verification.coverage,
        VerificationCoverage::Workspace
    );
    assert!(resume.has_unmet_obligation());
    assert!(!resume.verification_due_now("Append to src/scratch.md"));
    assert!(resume.verification_due_now("run the tests"));
    assert!(resume.verification_due_now("complete the task"));
}

#[test]
fn nl_verify_without_obligation_is_not_due() {
    let resume = ExecutionState::default();
    assert!(ExecutionState::turn_requests_verify("run the tests"));
    assert!(ExecutionState::turn_requests_verify(
        "check that tests pass"
    ));
    assert!(!resume.verification_due_now("run the tests"));
    assert!(!resume.verification_due_now("verify that"));
}

#[test]
fn foreground_resources_are_exact_mentions_of_known_paths() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 3);
    let mut scratch = output("fs.read", true, "read scratch");
    scratch.metadata = json!({"path": "src/scratch.md", "revision": "r3"});
    resume.observe_tool(&scratch, 1, 4);
    let keys = resume.foreground_resources("Append to src/scratch.md");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].path, "src/scratch.md");
    assert_eq!(keys[0].revision.as_deref(), Some("r3"));
    assert!(
        resume
            .foreground_resources("Append to src/scratch.md")
            .iter()
            .all(|key| key.path != "src/auth.rs")
    );
    resume.checked_files[0].freshness = ResourceFreshness::Missing;
    assert!(
        resume
            .foreground_resources("inspect src/auth.rs")
            .is_empty(),
        "Missing paths are not known resources"
    );
}
