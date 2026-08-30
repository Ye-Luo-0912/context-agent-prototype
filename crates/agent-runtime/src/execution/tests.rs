//! Algorithm tests for ExecutionState (formerly ResumePoint).

use super::state::{
    MAX_NEGATIVE_FACTS, MAX_OBLIGATIONS, MAX_RESUME_FAILURES, MAX_RESUME_FILES,
    MAX_REVALIDATE_PER_ROUND, MAX_VERIFICATION_SOURCES, VerificationCause, VerificationCoverage,
    VerificationState,
};
use super::*;
use agent_contracts::{
    MAX_TASK_ANCHOR_ITEM_CHARS, NegativeFactEventKind, ResourceFreshness, ResourceVersionOracle,
    SettlementLabel, ToolExecutionAttribution, ToolExecutionFacts, ToolExecutionPurpose,
    ToolFailureDomain, ToolOutput, ToolResultDisposition, TurnFrame, VerificationReuse,
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
/// path+revision stamp is a real observation.
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
    // headline: after an unknown mutation, a stale_revision
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
    // the fact is already known at the stamped revision, so
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
    // 换目标让逐签名计数器各自为政，而拒绝时观察到新文件属于 Evidence
    // 进展，同样会清掉聚类。
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    resume.observe_tool(&refused_edit("src/a.rs", "rev-a", "stale_revision"), 1, 2);
    resume.observe_tool(&refused_edit("src/b.rs", "rev-b", "stale_revision"), 1, 3);
    resume.observe_tool(&refused_edit("src/a.rs", "rev-a", "stale_revision"), 1, 4);
    assert!(resume.view().stall_warning.is_none());
    assert_eq!(resume.stall.consecutive_no_progress, 1);
    assert_eq!(resume.stall.target, "src/a.rs");
}

fn failed_read(path: &str, class: &str) -> ToolOutput {
    let mut failure = output("fs.read", false, "read refused");
    failure.metadata = json!({
        "path": path,
        "failure_class": class,
    });
    failure
}

#[test]
fn invented_path_streak_across_spellings_surfaces_the_cluster_stall() {
    // 虚构路径连击每次换拼写，任何单一签名都积不起来；类别聚类看到的
    // 是两个不同目标在同一世界里以同一方式失败。
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read protocol"), 1, 1);
    resume.observe_tool(&failed_read("src/lib.rs", "path_not_found"), 1, 2);
    assert!(
        resume.view().stall_warning.is_none(),
        "one failure is not yet a cluster"
    );
    resume.observe_tool(&failed_read("src/main.rs", "path_not_found"), 1, 3);
    let warning = resume
        .view()
        .stall_warning
        .expect("two distinct targets failed with the same class");
    assert!(warning.contains("EXECUTION STALL"));
    assert!(warning.contains("fs.read"));
    assert!(warning.contains("path_not_found"));
}

#[test]
fn failure_cluster_needs_the_same_failure_class() {
    // 类别不同就是不同的证据：混在一起不能制造连击。
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    resume.observe_tool(&failed_read("src/a.rs", "path_not_found"), 1, 2);
    resume.observe_tool(&failed_read("src/b.rs", "ambiguous_match"), 1, 3);
    assert!(resume.view().stall_warning.is_none());
}

#[test]
fn world_progress_clears_the_failure_cluster() {
    // 两次失败之间出现真实观察，说明模型在探索而不是打转：聚类重新
    // 开始计数。
    let mut resume = ExecutionState::default();
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    resume.observe_tool(&failed_read("src/lib.rs", "path_not_found"), 1, 2);
    resume.observe_tool(&output("fs.write", true, "wrote"), 1, 3);
    resume.observe_tool(&failed_read("src/main.rs", "path_not_found"), 1, 4);
    assert!(resume.view().stall_warning.is_none());
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
    resume.observe_tool_with_digest(&fail, 1, 2, "cargo-test-digest");
    let mut other = output("shell.exec", true, "exit 0");
    other.metadata = json!({"command": "dir"});
    resume.observe_tool_with_digest(&other, 1, 3, "dir-digest");
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(resume.failed_commands[0].target, "cargo test");
    assert!(resume.view().verifications.is_empty());
    let mut ok = output("shell.exec", true, "exit 0");
    ok.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool_with_digest(&ok, 1, 4, "cargo-test-digest");
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
fn runtime_argument_digest_not_command_text_controls_exact_failure_resolution() {
    let mut resume = ExecutionState::default();
    let mut failure = output("shell.exec", false, "exit 1");
    failure.metadata = json!({"command": "cargo test"});
    resume.observe_tool_with_digest(&failure, 1, 1, "argument-digest-a");
    assert_eq!(resume.failed_commands.len(), 1);

    let mut success = output("shell.exec", true, "exit 0");
    success.metadata = json!({"command": "cargo test"});
    resume.observe_tool_with_digest(&success, 1, 2, "argument-digest-b");
    assert_eq!(
        resume.failed_commands.len(),
        1,
        "identical display command with different runtime arguments is unrelated"
    );

    resume.observe_tool_with_digest(&success, 1, 3, "argument-digest-a");
    assert!(
        resume.failed_commands.is_empty(),
        "the exact runtime argument identity resolves its own blocker"
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
    resume.verification.spec_revision = 1;
    let mut test = output("shell.exec", true, "exit 0");
    test.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&test, 1, 1);
    // Progress-only anchor bump keeps the Current verifier visible
    // because the verification basis is spec_revision, not the whole
    // anchor revision.
    resume.observe_tool(&output("fs.read", true, "read auth"), 2, 2);
    assert!(
        !resume.view().verifications.is_empty(),
        "progress-only anchor change must not hide Current verification"
    );
    assert_eq!(resume.anchor_revision, 2);
    assert_eq!(resume.verification.spec_revision, 1);
    // Authority change (spec bump) hides the old PASS.
    resume.mark_spec_changed();
    resume.verification.spec_revision = 2;
    resume.anchor_revision = 2;
    assert!(resume.view().verifications.is_empty());
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
    turn.push_tool_result(patch, None, ToolExecutionFacts::empty());
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
    turn.push_tool_result_with(
        fetch,
        None,
        ToolResultDisposition::TransientNoPersist,
        ToolExecutionFacts::empty(),
    );
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
    turn.push_tool_result(ls, None, ToolExecutionFacts::empty());
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

#[test]
fn legacy_failed_command_restores_fail_closed_without_typed_identity() {
    let value = json!({
        "anchor_revision": 3,
        "checked_files": [],
        "verifications": [],
        "failed_commands": [{
            "tool_name": "shell.exec",
            "target": "cargo test",
            "summary": "exit 1",
            "turn": 2
        }]
    });
    let mut resume: ExecutionState = serde_json::from_value(value).unwrap();
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(
        resume.failed_commands[0].domain,
        ToolFailureDomain::NonDeterministic
    );
    assert!(resume.failed_commands[0].argument_digest.is_empty());
    assert!(resume.failed_commands[0].scope_key.is_empty());
    assert!(resume.failed_commands[0].precondition.is_empty());

    let mut success = output("shell.exec", true, "exit 0");
    success.metadata = json!({"command": "cargo test"});
    resume.observe_tool_with_digest(&success, 3, 3, "new-runtime-digest");
    assert_eq!(
        resume.failed_commands.len(),
        1,
        "legacy display text must not become equivalence authority"
    );
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

    resume.on_user_turn("run the tests");
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

#[tokio::test]
async fn checkpoint_body_demand_gets_existing_revalidation_quota_first() {
    let mut resume = ExecutionState::default();
    for index in 0..20 {
        let mut row = output("fs.read", true, "read");
        row.metadata = json!({
            "path": format!("src/f{index}.rs"),
            "revision": format!("rev{index}")
        });
        resume.observe_tool(&row, 1, index);
    }
    let mut process = output("process.run", true, "ran");
    process.metadata = json!({"argv": "compiler"});
    resume.observe_tool(&process, 1, 21);

    let mut map = std::collections::HashMap::new();
    for index in 0..20 {
        map.insert(format!("src/f{index}.rs"), Some(format!("rev{index}")));
    }
    resume
        .revalidate_with_priority(&MapOracle(map), "", &["src/f0.rs@rev0".into()])
        .await;
    assert_eq!(
        resume
            .checked_files
            .iter()
            .find(|row| row.path == "src/f0.rs")
            .expect("priority fact")
            .freshness,
        ResourceFreshness::Fresh,
        "a checkpoint-spilled body must not lose to pure recency"
    );
    assert_eq!(
        resume
            .checked_files
            .iter()
            .filter(|row| row.freshness == ResourceFreshness::Fresh)
            .count(),
        MAX_REVALIDATE_PER_ROUND,
        "priority changes order, never expands the bounded quota"
    );
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

// ---- Evidence Frontier / ConvergenceState----

fn pathless_command(name: &str, ok: bool, command: &str, summary: &str) -> ToolOutput {
    ToolOutput {
        call_id: "c".into(),
        tool_name: name.into(),
        ok,
        summary: summary.into(),
        model_content: summary.into(),
        artifact_ref: None,
        metadata: json!({ "command": command }),
    }
}

fn read_output(path: &str, revision: &str) -> ToolOutput {
    let mut out = output("fs.read", true, "read");
    out.metadata = json!({ "path": path, "revision": revision });
    out
}

fn git_status() -> ToolOutput {
    let mut out = output("git.status", true, "on branch main, clean");
    out.metadata = json!({});
    out
}

#[test]
fn git_status_repeat_at_same_revision_is_redundant_evidence() {
    let mut resume = ExecutionState::default();
    let first = resume.observe_tool(&git_status(), 1, 1);
    assert_eq!(
        first.delta,
        agent_contracts::FrontierDelta::EvidenceAdvanced
    );
    assert_eq!(first.actions_since_frontier_advance, 0);
    assert_eq!(resume.evidence.len(), 1);
    assert_eq!(resume.evidence[0].key, "git.status");

    let second = resume.observe_tool(&git_status(), 1, 2);
    assert_eq!(
        second.delta,
        agent_contracts::FrontierDelta::RedundantEvidence
    );
    assert_eq!(second.actions_since_frontier_advance, 1);
    // 重复不新增行，只保留原证据。
    assert_eq!(resume.evidence.len(), 1);
}

#[test]
fn targeted_directive_keeps_unrooted_novel_evidence_off_the_task_frontier() {
    let mut state = ExecutionState::default();
    state.observe_tool(&read_output("src/auth.rs", "rev-a"), 1, 1);
    state.on_user_turn("Refactor src/auth.rs without changing behavior");
    assert!(state.directive_has_rooted_evidence);

    let unrooted = RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Observe,
            Vec::<String>::new(),
            agent_contracts::VerificationReuse::None,
        ),
        rooted_targets: Vec::new(),
    };
    let observation = state.observe_tool_attributed(&git_status(), 1, 2, "git-status-a", &unrooted);
    assert_eq!(
        observation.delta,
        agent_contracts::FrontierDelta::NoProgress
    );
    assert_eq!(observation.actions_since_frontier_advance, 1);
    assert!(
        state.evidence.iter().any(|row| row.key == "git.status"),
        "unrelated evidence remains available even when it is not task progress"
    );

    state.on_user_turn("Survey the workspace broadly");
    assert!(!state.directive_has_rooted_evidence);
    let diff = output("git.diff", true, "new workspace diff");
    let observation = state.observe_tool_attributed(&diff, 1, 3, "git-diff-a", &unrooted);
    assert_eq!(
        observation.delta,
        agent_contracts::FrontierDelta::EvidenceAdvanced,
        "open-ended directives retain broad exploration semantics"
    );
}

#[test]
fn redundant_round_does_not_clear_active_failure_cluster() {
    let mut resume = ExecutionState::default();
    // 先建立已知证据，后面的同版本重读才是冗余而非新证据。
    resume.observe_tool(&git_status(), 1, 1);
    let miss_a = {
        let mut out = pathless_command("process.run", false, "protocol_tests.exe", "not found");
        out.metadata["failure_class"] = json!("command_unavailable");
        out
    };
    resume.observe_tool(&miss_a, 1, 2);
    assert_eq!(resume.failure_cluster.tried_targets.len(), 1);
    let miss_b = {
        let mut out = pathless_command("process.run", false, ".\\protocol_tests.exe", "not found");
        out.metadata["failure_class"] = json!("command_unavailable");
        out
    };
    resume.observe_tool(&miss_b, 1, 3);
    assert_eq!(resume.failure_cluster.tried_targets.len(), 2);

    // 冗余观察不清聚类：换拼写的连击不能靠一次重读洗掉。
    resume.observe_tool(&git_status(), 1, 4);
    assert_eq!(
        resume.convergence.actions_since_frontier_advance, 3,
        "redundant round adds debt without clearing it"
    );
    assert_eq!(resume.failure_cluster.tried_targets.len(), 2);

    // 真正的新证据（新文件身份）才清账。
    let fresh = resume.observe_tool(&read_output("src/other.rs", "r1"), 1, 5);
    assert_eq!(
        fresh.delta,
        agent_contracts::FrontierDelta::EvidenceAdvanced
    );
    assert_eq!(fresh.actions_since_frontier_advance, 0);
    assert!(resume.failure_cluster.tried_targets.is_empty());
}

#[test]
fn fs_read_same_digest_is_redundant_and_new_digest_advances() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&read_output("src/auth.rs", "abc123"), 1, 1);
    let repeat = resume.observe_tool(&read_output("src/auth.rs", "abc123"), 1, 2);
    assert_eq!(
        repeat.delta,
        agent_contracts::FrontierDelta::RedundantEvidence
    );

    let changed = resume.observe_tool(&read_output("src/auth.rs", "def456"), 1, 3);
    assert_eq!(
        changed.delta,
        agent_contracts::FrontierDelta::EvidenceAdvanced
    );
    assert_eq!(resume.evidence.len(), 1);
}

#[test]
fn known_edit_advances_world_and_invalidates_revision_bound_evidence() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&git_status(), 1, 1);
    let edit = {
        let mut out = output("edit.replace", true, "edited");
        out.metadata = json!({ "path": "src/auth.rs", "revision": "after1" });
        out
    };
    let observation = resume.observe_tool(&edit, 1, 2);
    assert_eq!(
        observation.delta,
        agent_contracts::FrontierDelta::ObservedWorldChange
    );
    assert_eq!(observation.invalidated, 1, "git.status@rev0 expired");
    assert_eq!(resume.evidence.len(), 1);
    assert!(!resume.evidence[0].current, "expired row must not project");
    assert!(resume.view().operational_evidence.is_empty());
    assert_eq!(observation.actions_since_frontier_advance, 0);
}

#[test]
fn unknown_footprint_never_claims_progress_but_accumulates_debt() {
    let mut resume = ExecutionState::default();
    let first = resume.observe_tool(
        &pathless_command("process.run", true, "cargo build", "compiled"),
        1,
        1,
    );
    assert_eq!(
        first.delta,
        agent_contracts::FrontierDelta::WorldInvalidatedUnknown,
        "unknown footprint never claims provable progress"
    );
    assert_eq!(first.actions_since_frontier_advance, 1);

    let second = resume.observe_tool(
        &pathless_command("process.run", true, "cargo build", "compiled"),
        1,
        2,
    );
    // 每个未知足迹轮都推进世界时钟，重复命令构不成"同版本重复"；
    // 债务持续累计，advisory 阈值负责软性压制。
    assert_eq!(
        second.delta,
        agent_contracts::FrontierDelta::WorldInvalidatedUnknown
    );
    assert_eq!(second.actions_since_frontier_advance, 2);
}

#[test]
fn frontier_advisory_fires_after_threshold_and_view_rows_are_typed() {
    let mut resume = ExecutionState::default();
    for turn in 1..6 {
        resume.observe_tool(&git_status(), 1, turn);
    }
    assert_eq!(
        resume.convergence.actions_since_frontier_advance, 4,
        "first observation advanced; four repeats since"
    );
    assert!(resume.frontier_warning().is_none());
    resume.observe_tool(&git_status(), 1, 6);
    let warning = resume.frontier_warning().expect("advisory at threshold");
    assert!(warning.starts_with("EXECUTION FRONTIER UNCHANGED"));
    assert!(warning.contains("recent deltas"));

    let view = resume.view();
    assert_eq!(view.operational_evidence.len(), 1);
    assert!(
        view.operational_evidence[0].starts_with("git.status: on branch main, clean @ world=0"),
        "typed row only, got {}",
        view.operational_evidence[0]
    );
    assert_eq!(view.frontier_warning.as_deref(), Some(warning.as_str()));
}

#[test]
fn evidence_vec_and_delta_ring_stay_bounded() {
    let mut resume = ExecutionState::default();
    for index in 0..24 {
        resume.observe_tool(&read_output(&format!("src/f{index}.rs"), "r"), 1, index + 1);
    }
    assert_eq!(resume.evidence.len(), 16);
    assert_eq!(resume.convergence.recent_deltas.len(), 8);
}

#[test]
fn repeated_identical_verification_pass_is_redundant_not_progress() {
    let mut resume = ExecutionState::default();
    let verify = || {
        let mut out = output("shell.exec", true, "tests passed");
        out.metadata = json!({ "command": "cargo test", "verification": true });
        out
    };
    let first = resume.observe_tool(&verify(), 1, 1);
    assert_eq!(
        first.delta,
        agent_contracts::FrontierDelta::EvidenceAdvanced
    );
    let second = resume.observe_tool(&verify(), 1, 2);
    assert_eq!(
        second.delta,
        agent_contracts::FrontierDelta::RedundantEvidence
    );
}

// ---- Obligation ledger + execution evidence（第二轮评审）----

fn missing_program(argv0: &str, fingerprint: &str) -> ToolOutput {
    missing_program_in_scope(argv0, "scope-a", fingerprint)
}

fn missing_program_in_scope(argv0: &str, scope: &str, fingerprint: &str) -> ToolOutput {
    let mut out = pathless_command("process.run", false, argv0, "program not found");
    out.metadata = json!({
        // 与真实 process.run 一致：argv 是 join 过的字符串；resolver
        // 在 preflight 统一盖章 scope 与 epoch 指纹。
        "argv": argv0,
        "cwd": ".",
        "resolution_scope_key": scope,
        "resolution_fingerprint": fingerprint,
        "failure_class": "path_not_found",
    });
    out
}

/// 成功的命令运行同样携带解析身份（matched-success 的依据）。
fn resolved_command(argv0: &str, scope: &str, fingerprint: &str) -> ToolOutput {
    let mut out = pathless_command("process.run", true, argv0, "ran");
    out.metadata = json!({
        "argv": argv0,
        "cwd": ".",
        "resolution_scope_key": scope,
        "resolution_fingerprint": fingerprint,
    });
    out
}

#[test]
fn obligation_source_tools_track_live_rows_only() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&missing_program("prog_a", "fp-1"), 1, 1);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.obligations[0].source_tool_name, "process.run");
    assert_eq!(resume.obligation_source_tools(), vec!["process.run"]);

    // A fingerprint-matched success resolves the row and releases its
    // provenance by construction — no separate lease bookkeeping exists.
    resume.observe_tool(&resolved_command("prog_a", "scope-a", "fp-1"), 1, 2);
    assert!(resume.obligations.is_empty());
    assert!(resume.obligation_source_tools().is_empty());
}

#[test]
fn executable_obligation_opens_and_survives_unrelated_progress() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&missing_program("prog_a", "fp-1"), 1, 1);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.obligations[0].attempts, 1);

    // 无关进展 #1：新文件证据。全局债务清零，义务纹丝不动。
    resume.observe_tool(&read_output("src/new.rs", "r1"), 1, 2);
    // 无关进展 #2：已知 mutation 推进世界。
    let mut edit = output("edit.replace", true, "edited other");
    edit.metadata = json!({ "path": "src/other.rs", "revision": "v2" });
    resume.observe_tool(&edit, 1, 3);

    assert_eq!(
        resume.obligations.len(),
        1,
        "unrelated progress must not resolve a blocker"
    );
    assert_eq!(
        resume.convergence.actions_since_frontier_advance, 0,
        "global debt still resets on real advances"
    );
    let view = resume.view();
    assert_eq!(view.unresolved_blockers.len(), 1);
    assert!(view.unresolved_blockers[0].contains("executable_resolution"));
}

#[test]
fn same_fingerprint_accumulates_and_new_fingerprint_advances_the_epoch() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&missing_program("prog_a", "fp-1"), 1, 1);
    resume.observe_tool(&missing_program("prog_b", "fp-1"), 1, 2);
    assert_eq!(resume.obligations[0].attempts, 2);
    assert_eq!(resume.obligations[0].total_attempts, 2);
    assert_eq!(resume.obligations[0].tried_targets.len(), 2);

    // 前置变化（build 完成 / PATH 改变）→ epoch 推进，血统与累计
    // 账目保持：Runtime 不忘掉这个方向已经浪费过两次。
    resume.observe_tool(&missing_program("prog_c", "fp-2"), 1, 3);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.obligations[0].precondition, "fp-2");
    assert_eq!(resume.obligations[0].epoch, 2);
    assert_eq!(resume.obligations[0].attempts, 1);
    assert_eq!(resume.obligations[0].total_attempts, 3);
}

#[test]
fn only_candidate_family_matched_success_resolves_the_executable_obligation() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&missing_program("app.exe", "fp-1"), 1, 1);
    assert!(!resume.obligations.is_empty());
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(
        resume.failed_commands[0].domain,
        ToolFailureDomain::ExecutableResolution
    );
    assert_eq!(resume.failed_commands[0].scope_key, "scope-a");
    assert_eq!(resume.failed_commands[0].precondition, "fp-1");

    // Another program's successful launch is a different candidate family
    // and cannot clear app.exe, even if the broader resolver world changed.
    resume.observe_tool(&resolved_command("rustc", "scope-b", "fp-2"), 1, 2);
    assert_eq!(
        resume.obligations.len(),
        1,
        "another program launch is not resolution"
    );
    assert_eq!(resume.failed_commands.len(), 1);

    // 另一个 scope 的成功与本 blocker 无关。
    resume.observe_tool(&resolved_command("other.exe", "scope-b", "fp-2"), 1, 3);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.failed_commands.len(), 1);

    // Installing app.exe moves the fingerprint, but a successful launch in
    // the exact app.exe candidate-family scope proves the blocker resolved.
    let resolved = resume.observe_tool(&resolved_command("app.exe", "scope-a", "fp-2"), 1, 4);
    assert!(
        resume.obligations.is_empty(),
        "a candidate-family-matched launch proves resolution works now"
    );
    assert!(resume.failed_commands.is_empty());
    assert!(resolved.obligation_events.iter().any(|event| {
        event.kind == agent_contracts::ObligationEventKind::Resolved
            && event.domain == ToolFailureDomain::ExecutableResolution
            && event.total_attempts == 1
    }));
}

#[test]
fn exact_candidate_family_success_resolves_after_install_epoch_change() {
    let mut resume = ExecutionState::default();
    resume.observe_tool_with_digest(
        &missing_program("app.exe", "fp-1"),
        1,
        1,
        "argument-digest-a",
    );

    // Installation changes the resolver precondition. The same candidate
    // family's successful launch is stronger evidence than that change and
    // resolves both blocker projections immediately.
    resume.observe_tool_with_digest(
        &resolved_command("app.exe", "scope-a", "fp-2"),
        1,
        2,
        "argument-digest-a",
    );
    assert!(resume.obligations.is_empty());
    assert!(resume.failed_commands.is_empty());
}

#[test]
fn executable_resolution_requires_the_exact_program_candidate_family() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(
        &missing_program_in_scope("foo", "scope-foo", "fp-before"),
        1,
        1,
    );

    resume.observe_tool(&resolved_command("bar", "scope-bar", "fp-after"), 1, 2);
    assert_eq!(resume.obligations.len(), 1, "bar cannot resolve foo");
    assert_eq!(resume.failed_commands.len(), 1, "bar cannot clear foo");

    resume.observe_tool(&resolved_command("foo", "scope-foo", "fp-after"), 1, 3);
    assert!(
        resume.obligations.is_empty(),
        "the real foo launch resolves foo"
    );
    assert!(
        resume.failed_commands.is_empty(),
        "both projections resolve together"
    );
}

#[test]
fn unresolved_failure_overflow_survives_directives_and_stays_fail_closed() {
    let mut resume = ExecutionState::default();
    let mut overflow_event = None;
    for index in 0..=MAX_OBLIGATIONS {
        let path = format!("src/missing-{index}.rs");
        let observation =
            resume.observe_tool(&failed_read(&path, "path_not_found"), 1, index as u64);
        overflow_event = observation
            .obligation_events
            .into_iter()
            .find(|event| event.kind == agent_contracts::ObligationEventKind::Overflowed)
            .or(overflow_event);
    }

    assert_eq!(resume.obligations.len(), MAX_OBLIGATIONS);
    assert_eq!(resume.failed_commands.len(), MAX_RESUME_FAILURES);
    assert_eq!(resume.failure_overflow.omitted_obligations, 1);
    assert_eq!(resume.failure_overflow.omitted_failed_commands, 1);
    assert_eq!(resume.open_obligation_count(), MAX_OBLIGATIONS + 1);
    assert_eq!(
        resume.unresolved_failed_command_count(),
        MAX_RESUME_FAILURES + 1
    );
    assert!(
        !resume.execution_ready(),
        "overflow debt must block completion"
    );
    assert_eq!(
        overflow_event.expect("overflow transition").scope_digest,
        format!("src/missing-{MAX_OBLIGATIONS}.rs")
    );
    assert!(
        resume
            .view()
            .unresolved_blockers
            .iter()
            .any(|warning| warning.contains("BLOCKER OVERFLOW"))
    );

    // Exact observations can still retire every identity retained in the
    // bounded hot set. They cannot guess the omitted identity, so the
    // sentinel remains fail-closed.
    for index in 0..MAX_OBLIGATIONS {
        resume.observe_tool(
            &read_output(&format!("src/missing-{index}.rs"), "now-present"),
            1,
            20 + index as u64,
        );
    }
    assert!(resume.obligations.is_empty());
    assert!(resume.failed_commands.is_empty());
    assert_eq!(resume.open_obligation_count(), 1);
    assert!(resume.has_failures());

    // Neither TaskContinuation nor an ordinary incremental user directive is
    // a waiver. The old opaque debt stays bound to its opening epoch and
    // continues to block; only operator override/new-task authority can
    // leave it behind.
    resume.on_user_turn("continue the same task with one more requirement");
    assert_eq!(resume.directive_revision, 1);
    assert_eq!(resume.failure_overflow.directive_revision, 0);
    assert_eq!(resume.open_obligation_count(), 1);
    assert!(resume.has_failures());
    assert!(super::state::validate_execution_state(&resume).is_ok());

    let fresh_task = ExecutionState::default();
    assert_eq!(
        fresh_task.failure_overflow,
        UnresolvedFailureOverflow::default()
    );
    assert_eq!(fresh_task.open_obligation_count(), 0);
}

#[test]
fn typed_resolution_preserves_an_unrelated_failure_domain() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&missing_program("app.exe", "fp-1"), 1, 1);
    resume.observe_tool(&failed_read("src/missing.rs", "path_not_found"), 1, 2);
    assert_eq!(resume.failed_commands.len(), 2);
    assert_eq!(resume.obligations.len(), 2);

    resume.observe_tool(&resolved_command("app.exe", "scope-a", "fp-1"), 1, 3);

    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(
        resume.failed_commands[0].domain,
        ToolFailureDomain::ResourcePath
    );
    assert_eq!(resume.failed_commands[0].scope_key, "src/missing.rs");
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(
        resume.obligations[0].domain,
        ToolFailureDomain::ResourcePath
    );
    assert_eq!(resume.obligations[0].scope_key, "src/missing.rs");
}

#[test]
fn a_new_failure_cannot_resolve_itself_from_a_cached_resource_fact() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&read_output("src/unstable.rs", "r1"), 1, 1);

    // The old fact may be stale in reality. The current failed observation
    // is an attempt in this lineage, not proof that its own blocker vanished.
    resume.observe_tool(&failed_read("src/unstable.rs", "path_not_found"), 1, 2);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.failed_commands.len(), 1);

    // Nor may a later unrelated success reinterpret that cached pre-failure
    // fact as a new observation of the missing path.
    resume.observe_tool(&read_output("src/other.rs", "other-r1"), 1, 3);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.failed_commands.len(), 1);

    resume.observe_tool(&read_output("src/unstable.rs", "r2"), 1, 4);
    assert!(resume.obligations.is_empty());
    assert!(resume.failed_commands.is_empty());
}

#[test]
fn project_marker_blocker_uses_the_typed_marker_not_command_text() {
    let mut resume = ExecutionState::default();
    let mut missing = output("shell.exec", false, "manifest missing");
    missing.metadata = json!({
        "command": "cargo test --workspace",
        "missing_marker": "Cargo.toml",
        "failure_class": "missing_project_marker",
    });
    resume.observe_tool(&missing, 1, 1);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(
        resume.obligations[0].domain,
        ToolFailureDomain::ProjectMarker
    );
    assert_eq!(resume.obligations[0].scope_key, "Cargo.toml");
    assert_eq!(resume.failed_commands[0].scope_key, "Cargo.toml");
    assert_ne!(
        resume.failed_commands[0].scope_key, resume.failed_commands[0].target,
        "display command text is not blocker equivalence authority"
    );

    let mut created = output("fs.write", true, "created manifest");
    created.metadata = json!({"path": "Cargo.toml", "revision": "manifest-v1"});
    resume.observe_tool(&created, 1, 2);
    assert!(resume.obligations.is_empty());
    assert!(resume.failed_commands.is_empty());

    // External creation is equally provable through a current exact read;
    // no Runtime mutation is required to retire the missing-marker fact.
    resume.observe_tool(&missing, 1, 3);
    resume.observe_tool(&read_output("Cargo.toml", "manifest-v2"), 1, 4);
    assert!(resume.obligations.is_empty());
    assert!(resume.failed_commands.is_empty());
}

#[test]
fn edit_target_obligation_resolves_only_at_new_digest() {
    let mut resume = ExecutionState::default();
    let refusal = {
        let mut out = output("edit.replace", false, "stale");
        out.metadata = json!({
            "path": "src/auth.rs",
            "revision": "rOLD",
            "failure_class": "stale_revision",
        });
        out
    };
    resume.observe_tool(&refusal, 1, 1);
    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.obligations[0].precondition, "src/auth.rs@rOLD");

    // 同一文件以旧 digest 变 Fresh：blocker 未变，不清账。
    resume.observe_tool(&read_output("src/auth.rs", "rOLD"), 1, 2);
    assert_eq!(resume.obligations.len(), 1);

    // 文件移动到新身份可以解除。
    resume.observe_tool(&read_output("src/auth.rs", "rNEW"), 1, 3);
    assert!(resume.obligations.is_empty());
}

#[test]
fn changed_edit_refusal_advances_lineage_instead_of_erasing_history() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&refused_edit("src/auth.rs", "rOLD", "stale_revision"), 1, 1);
    resume.observe_tool(
        &refused_edit("src/auth.rs", "rCURRENT", "stale_revision"),
        1,
        2,
    );

    assert_eq!(resume.obligations.len(), 1);
    assert_eq!(resume.obligations[0].epoch, 2);
    assert_eq!(resume.obligations[0].attempts, 1);
    assert_eq!(resume.obligations[0].total_attempts, 2);
    assert_eq!(resume.obligations[0].precondition, "src/auth.rs@rCURRENT");
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(
        resume.failed_commands[0].precondition,
        "src/auth.rs@rCURRENT"
    );
}

#[test]
fn trusted_verification_resolves_only_its_rooted_edit_blocker() {
    let mut state = ExecutionState::default();
    let mut auth_refusal = output("edit.replace", false, "stale auth");
    auth_refusal.metadata = json!({
        "path": "src/auth.rs",
        "revision": "rCURRENT",
        "failure_class": "stale_revision",
    });
    let mut billing_refusal = output("edit.replace", false, "stale billing");
    billing_refusal.metadata = json!({
        "path": "src/billing.rs",
        "revision": "rCURRENT",
        "failure_class": "stale_revision",
    });
    state.observe_tool(&auth_refusal, 1, 1);
    state.observe_tool(&billing_refusal, 1, 2);
    assert_eq!(state.obligations.len(), 2);
    assert_eq!(state.failed_commands.len(), 2);

    // Provider/plugin metadata cannot retire a blocker by declaring itself
    // a verifier. The authority must come from trusted pre-dispatch facts.
    let mut verify = output("verify.run", true, "tests passed");
    verify.metadata = json!({"verification": true});
    state.observe_tool(&verify, 1, 3);
    assert_eq!(
        state.obligations.len(),
        2,
        "legacy producer metadata may record evidence but cannot retire an obligation"
    );
    state.observe_tool_attributed(
        &verify,
        1,
        4,
        "untrusted-verify",
        &RuntimeExecutionAttribution::default(),
    );
    assert_eq!(state.obligations.len(), 2);

    let trusted = RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            ["src/auth.rs".into()],
            VerificationReuse::TaskScoped,
        ),
        rooted_targets: vec!["src/auth.rs".into()],
    };
    let observation = state.observe_tool_attributed(&verify, 1, 5, "trusted-verify", &trusted);
    assert_eq!(state.obligations.len(), 1);
    assert_eq!(state.obligations[0].scope_key, "src/billing.rs");
    assert_eq!(state.failed_commands.len(), 1);
    assert_eq!(state.failed_commands[0].scope_key, "src/billing.rs");
    assert!(observation.obligation_events.iter().any(|event| {
        event.kind == agent_contracts::ObligationEventKind::Resolved
            && event.domain == ToolFailureDomain::EditTarget
    }));
}

#[test]
fn stale_resource_evidence_is_hidden_but_keeps_bounded_fingerprint() {
    // 的原始 bug 场景：edit 之后 Resource 行不得残留 AAA。
    let mut resume = ExecutionState::default();
    resume.observe_tool(&read_output("src/foo.rs", "AAA"), 1, 1);
    assert_eq!(resume.view().operational_evidence.len(), 1);
    let mut edit = output("edit.replace", true, "edited");
    edit.metadata = json!({ "path": "src/foo.rs", "revision": "BBB" });
    resume.observe_tool(&edit, 1, 2);
    assert!(
        resume.view().operational_evidence.is_empty(),
        "evidence for a changed file must not render"
    );
    let dormant = resume
        .evidence
        .iter()
        .find(|row| {
            row.validity
                == agent_contracts::EvidenceValidity::Resource {
                    path: "src/foo.rs".into(),
                    digest: "AAA".into(),
                }
        })
        .expect("bounded semantic fingerprint remains available");
    assert!(!dormant.current, "stale evidence must never project");
}

#[test]
fn identical_read_after_unknown_reconfirms_without_advancing_frontier() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&read_output("src/auth.rs", "r1"), 1, 1);
    let unknown = resume.observe_tool(
        &pathless_command("process.run", true, "rustc --test", "compiled"),
        1,
        2,
    );
    assert_eq!(unknown.invalidated, 1);
    assert!(!resume.evidence[0].current);

    let reconfirmed = resume.observe_tool(&read_output("src/auth.rs", "r1"), 1, 3);
    assert_eq!(
        reconfirmed.delta,
        agent_contracts::FrontierDelta::EvidenceReconfirmed
    );
    assert_eq!(reconfirmed.actions_since_frontier_advance, 2);
    assert!(resume.evidence[0].current);
    assert_eq!(resume.evidence.len(), 1);
}

#[test]
fn restore_rejects_oversized_frontier_fields() {
    use super::state::validate_execution_state;
    let mut state = ExecutionState::default();
    for index in 0..20 {
        state.evidence.push(agent_contracts::ExecutionEvidence {
            key: format!("fs.read:f{index}.rs"),
            outcome: "ok".into(),
            observed_world_revision: 1,
            validity: agent_contracts::EvidenceValidity::WorkspaceRevision(1),
            argument_digest: String::new(),
            outcome_digest: String::new(),
            current: true,
            turn: 1,
            evidence_ref: None,
        });
    }
    assert!(validate_execution_state(&state).is_err());

    let mut long_key = ExecutionState::default();
    long_key.evidence.push(agent_contracts::ExecutionEvidence {
        key: "k".repeat(MAX_TASK_ANCHOR_ITEM_CHARS + 1),
        outcome: "ok".into(),
        observed_world_revision: 1,
        validity: agent_contracts::EvidenceValidity::WorkspaceRevision(1),
        argument_digest: String::new(),
        outcome_digest: String::new(),
        current: true,
        turn: 1,
        evidence_ref: None,
    });
    assert!(validate_execution_state(&long_key).is_err());

    let mut negative_overflow = ExecutionState::default();
    for index in 0..=MAX_NEGATIVE_FACTS {
        negative_overflow
            .negative_facts
            .push(NegativeExecutionFact {
                tool_name: "fs.read".into(),
                target: format!("src/guess-{index}.rs"),
                argument_digest: format!("arg-{index}"),
                failure: agent_contracts::ToolFailureClass::PathNotFound,
                workspace_revision: 0,
                turn: 1,
            });
    }
    assert!(validate_execution_state(&negative_overflow).is_err());

    let mut source_overflow = ExecutionState::default();
    for index in 0..=MAX_VERIFICATION_SOURCES {
        source_overflow
            .verification_sources
            .push(VerificationSourceLease {
                tool_name: format!("verify.{index}"),
                argument_digest: format!("arg-{index}"),
                anchor_revision: 1,
            });
    }
    assert!(validate_execution_state(&source_overflow).is_err());

    let mut valid_failure_overflow = ExecutionState {
        directive_revision: 7,
        failure_overflow: UnresolvedFailureOverflow {
            directive_revision: 7,
            omitted_obligations: 2,
            omitted_failed_commands: 3,
        },
        ..ExecutionState::default()
    };
    assert!(validate_execution_state(&valid_failure_overflow).is_ok());
    valid_failure_overflow.failure_overflow.directive_revision = 6;
    assert!(
        validate_execution_state(&valid_failure_overflow).is_ok(),
        "overflow debt remains valid across later directives"
    );
    valid_failure_overflow.failure_overflow.directive_revision = 8;
    assert!(
        validate_execution_state(&valid_failure_overflow).is_err(),
        "an overflow sentinel cannot claim a future directive epoch"
    );
    valid_failure_overflow.failure_overflow.directive_revision = 7;
    valid_failure_overflow.failure_overflow.omitted_obligations = 4;
    assert!(
        validate_execution_state(&valid_failure_overflow).is_ok(),
        "the independently capped projections may omit different counts"
    );
}

fn attributed_path(
    purpose: ToolExecutionPurpose,
    path: &str,
    rooted: bool,
) -> RuntimeExecutionAttribution {
    RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            purpose,
            [path.to_string()],
            VerificationReuse::None,
        ),
        rooted_targets: rooted.then(|| path.to_string()).into_iter().collect(),
    }
}

fn missing_path(path: &str) -> ToolOutput {
    let mut missing = output("fs.read", false, "path not found");
    missing.metadata = json!({
        "path": path,
        "failure_class": "path_not_found",
    });
    missing
}

#[test]
fn speculative_path_miss_records_negative_fact_without_task_obligation() {
    let mut state = ExecutionState::default();
    let attribution = attributed_path(ToolExecutionPurpose::Observe, "src/guess.rs", false);
    let observation =
        state.observe_tool_attributed(&missing_path("src/guess.rs"), 3, 1, "arg-1", &attribution);

    assert!(state.obligations.is_empty());
    assert_eq!(state.negative_facts.len(), 1);
    assert_eq!(state.negative_facts[0].target, "src/guess.rs");
    assert_eq!(state.view().failed_commands.len(), 1);
    assert!(state.view().failed_commands[0].starts_with("known_absent "));
    assert_eq!(
        observation.negative_fact_events[0].kind,
        NegativeFactEventKind::Recorded
    );
    assert!(
        state
            .current_negative_fact("fs.read", &attribution)
            .is_some()
    );
}

#[test]
fn task_rooted_path_miss_stays_an_obligation() {
    let mut state = ExecutionState::default();
    let attribution = attributed_path(ToolExecutionPurpose::Observe, "src/required.rs", true);
    let observation = state.observe_tool_attributed(
        &missing_path("src/required.rs"),
        3,
        1,
        "arg-1",
        &attribution,
    );

    assert!(state.negative_facts.is_empty());
    assert_eq!(state.obligations.len(), 1);
    assert!(observation.negative_fact_events.is_empty());
}

#[test]
fn workspace_mutation_invalidates_speculative_negative_facts() {
    let mut state = ExecutionState::default();
    let attribution = attributed_path(ToolExecutionPurpose::Observe, "src/guess.rs", false);
    state.observe_tool_attributed(&missing_path("src/guess.rs"), 3, 1, "arg-1", &attribution);

    let mut edit = output("edit.replace", true, "updated workspace");
    edit.metadata = json!({"path": "src/other.rs", "revision": "r2"});
    let edit_attribution = attributed_path(ToolExecutionPurpose::Mutate, "src/other.rs", true);
    let observation = state.observe_tool_attributed(&edit, 3, 2, "arg-2", &edit_attribution);

    assert!(state.negative_facts.is_empty());
    assert!(
        observation
            .negative_fact_events
            .iter()
            .any(|event| event.kind == NegativeFactEventKind::Invalidated)
    );
}

#[test]
fn only_trusted_verify_attribution_mints_reusable_verification() {
    let mut state = ExecutionState::default();
    state.verification.spec_revision = 7;
    let mut verify = output("test.verify", true, "tests passed");
    verify.metadata = json!({"verification": true});
    let untrusted = RuntimeExecutionAttribution::default();
    state.observe_tool_attributed(&verify, 7, 1, "arg-u", &untrusted);
    assert!(state.verifications.is_empty());
    assert!(state.verification_source_tools(7).is_empty());

    let trusted = RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            Vec::<String>::new(),
            VerificationReuse::TaskScoped,
        ),
        rooted_targets: Vec::new(),
    };
    state.observe_tool_attributed(&verify, 7, 2, "arg-t", &trusted);
    assert_eq!(state.verifications.len(), 1);
    assert_eq!(state.verification_source_tools(7), vec!["test.verify"]);
    assert!(state.verification_source_tools(8).is_empty());
}

#[test]
fn dispatcher_lane_facts_verify_without_metadata_stamps() {
    let mut state = ExecutionState::default();
    // The output carries no verification metadata at all; the typed facts
    // captured on the dispatcher lane are the trusted claim.
    let verify = output("test.verify", true, "tests passed");
    let claimed = ToolExecutionFacts::empty().with_verification(true);
    state.observe_tool_facts(&verify, 7, 1, "arg-f", &claimed);
    assert_eq!(state.verifications.len(), 1);
    assert!(state.verifications[0].ok);

    // Frames without a stamped claim keep the legacy metadata read.
    let mut stamped = output("test.verify", true, "tests passed");
    stamped.metadata = json!({"verification": true});
    state.observe_tool_with_digest(&stamped, 7, 2, "arg-s");
    assert_eq!(state.verifications.len(), 2);

    // An unstamped claim falls back per value: no metadata, no fact row.
    let plain = output("test.verify", true, "tests passed");
    let empty = ToolExecutionFacts::empty();
    state.observe_tool_facts(&plain, 7, 3, "arg-p", &empty);
    assert_eq!(state.verifications.len(), 2);
}

#[test]
fn domain_equivalent_pass_requires_matching_class_identity_and_declaration() {
    fn attribution_for(
        recipe_id: &str,
        class: &str,
        declaration_revision: u64,
    ) -> RuntimeExecutionAttribution {
        RuntimeExecutionAttribution {
            host: agent_contracts::ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                Vec::<String>::new(),
                VerificationReuse::ExactCurrentWorld,
            )
            .with_verification_identity_material(format!("recipe:{recipe_id}|env:e").as_str())
            .with_verification_recipe(agent_contracts::VerificationRecipeProvenance {
                recipe_id: recipe_id.into(),
                recipe_revision: "rev-1".into(),
                coverage_domain: Some("workspace-tests".into()),
                domain_declaration_revision: Some(declaration_revision),
                domain_source_digest: agent_contracts::ContentDigest::sha256_bytes(
                    b"workspace-tests/source",
                )
                .to_string(),
                class_identity_digest: class.into(),
            }),
            rooted_targets: Vec::new(),
        }
    }

    let mut state = ExecutionState::default();
    state.verification.spec_revision = 7;
    let verify = output("test.verify", true, "tests passed");
    state.observe_tool_attributed(
        &verify,
        7,
        1,
        "arg-a",
        &attribution_for("verify.a", "class-1", 3),
    );

    // A sibling recipe from the same declared class and shared execution
    // identity satisfies the due verification without a dispatch.
    let sibling =
        state.current_domain_verification_pass(7, &attribution_for("verify.b", "class-1", 3));
    assert!(sibling.is_some());
    assert_eq!(
        sibling.unwrap().recipe_provenance.unwrap().recipe_id,
        "verify.a"
    );

    // Declaration revision bump invalidates older facts.
    assert!(
        state
            .current_domain_verification_pass(7, &attribution_for("verify.b", "class-1", 4))
            .is_none()
    );
    // Recomposition under the same numeric revision is fenced by the stable
    // source digest too.
    let mut recomposed = attribution_for("verify.b", "class-1", 3);
    recomposed
        .host
        .verification_recipe
        .as_mut()
        .unwrap()
        .domain_source_digest =
        agent_contracts::ContentDigest::sha256_bytes(b"recomposed/source").to_string();
    assert!(
        state
            .current_domain_verification_pass(7, &recomposed)
            .is_none()
    );
    // Class execution-identity drift invalidates.
    assert!(
        state
            .current_domain_verification_pass(7, &attribution_for("verify.b", "class-2", 3))
            .is_none()
    );
    // A request whose host resolved no coverage domain fails closed.
    let no_domain = RuntimeExecutionAttribution {
        host: agent_contracts::ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            Vec::<String>::new(),
            VerificationReuse::ExactCurrentWorld,
        )
        .with_verification_identity_material("recipe:b|env:e"),
        rooted_targets: Vec::new(),
    };
    assert!(
        state
            .current_domain_verification_pass(7, &no_domain)
            .is_none()
    );
}

#[test]
fn exact_verification_pass_reuse_requires_the_complete_current_identity() {
    let mut state = ExecutionState::default();
    state.verification.spec_revision = 7;
    state.on_user_turn("verify current state");
    let verify = output("test.verify", true, "tests passed");
    let exact = RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            Vec::<String>::new(),
            VerificationReuse::ExactCurrentWorld,
        )
        .with_verification_identity_material("test-runner:v2|policy:p3|env:win11"),
        rooted_targets: Vec::new(),
    };

    let observation = state.observe_tool_attributed(&verify, 7, 1, "arg-a", &exact);
    assert_eq!(observation.verification_pass_events.len(), 1);
    assert_eq!(
        observation.verification_pass_events[0].kind,
        agent_contracts::VerificationPassEventKind::Recorded
    );
    assert!(
        state
            .current_exact_verification_pass("test.verify", "arg-a", 7, &exact)
            .is_some()
    );
    let verification_count = state.verifications.len();
    let reused = state.observe_reused_verification(&verify, 7, 2);
    assert_eq!(
        reused.delta,
        agent_contracts::FrontierDelta::RedundantEvidence
    );
    assert_eq!(state.verifications.len(), verification_count);
    assert!(
        state
            .current_exact_verification_pass("test.verify", "arg-b", 7, &exact)
            .is_none()
    );

    let changed_environment = RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            Vec::<String>::new(),
            VerificationReuse::ExactCurrentWorld,
        )
        .with_verification_identity_material("test-runner:v2|policy:p3|env:linux"),
        rooted_targets: Vec::new(),
    };
    assert!(
        state
            .current_exact_verification_pass("test.verify", "arg-a", 7, &changed_environment)
            .is_none()
    );

    state.on_user_turn("verify current state again");
    assert!(
        state
            .current_exact_verification_pass("test.verify", "arg-a", 7, &exact)
            .is_none(),
        "a later user directive must be able to request a real rerun"
    );

    state.observe_tool_attributed(&verify, 7, 2, "arg-a", &exact);
    let mut edit = output("edit.replace", true, "updated workspace");
    edit.metadata = json!({"path": "src/other.rs", "revision": "r2"});
    let edit_attribution = attributed_path(ToolExecutionPurpose::Mutate, "src/other.rs", true);
    state.observe_tool_attributed(&edit, 7, 3, "edit-a", &edit_attribution);
    assert!(
        state
            .current_exact_verification_pass("test.verify", "arg-a", 7, &exact)
            .is_none(),
        "an admitted workspace revision change must force real verification"
    );
}

fn verified_ok() -> ToolOutput {
    let mut out = output("shell.exec", true, "exit 0");
    // A verifier that names its covered resource has a Known footprint, so
    // the pass admits the current world instead of re-arming Unknown
    // pending. The touched revision must match the fact it admits.
    out.metadata = json!({
        "command": "cargo test",
        "verification": true,
        "path": "src/auth.rs",
        "revision": "v2",
    });
    out
}

fn write_of(path: &str, revision: &str) -> ToolOutput {
    let mut out = output("fs.write", true, "wrote");
    out.metadata = json!({"path": path, "revision": revision});
    out
}

#[test]
fn settlement_is_working_without_covered_mutation() {
    let mut resume = ExecutionState::default();
    assert_eq!(resume.settlement(), SettlementLabel::Working);
    assert!(!resume.execution_ready());
    assert!(resume.view().settlement.is_none());

    // Read-only exploration admits no mutation; the task has not settled.
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    assert_eq!(resume.settlement(), SettlementLabel::Working);
    assert!(!resume.execution_ready());
    assert!(resume.view().settlement.is_none());
}

/// Exact host-attributed verifier for the settlement tests. Without an
/// exact verification identity the execution-ready gate fails closed, so
/// every positive must go through this attribution.
fn observed_exact_pass(resume: &mut ExecutionState, anchor_revision: u64, turn: u64) {
    let attribution = RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            Vec::<String>::new(),
            VerificationReuse::ExactCurrentWorld,
        )
        .with_verification_identity_material("test-runner:v1|env:win"),
        rooted_targets: Vec::new(),
    };
    resume.observe_tool_attributed(
        &verified_ok(),
        anchor_revision,
        turn,
        "arg-settle",
        &attribution,
    );
}

#[test]
fn mutation_then_current_verification_is_verified_current() {
    let mut resume = ExecutionState::default();
    // A covered mutation needs a tracked path first: a write to a known
    // digest marks the source changed and makes verification due.
    resume.observe_tool(&output("fs.read", true, "read auth"), 1, 1);
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 2);
    assert_eq!(resume.settlement(), SettlementLabel::VerificationDue);
    assert!(!resume.execution_ready());

    observed_exact_pass(&mut resume, 1, 3);
    assert_eq!(resume.validity(), VerificationState::Current);
    // Execution-local top is VerifiedCurrent: the world is verified, but
    // whether the whole task rises to a candidate is the actor-owned join.
    assert_eq!(resume.settlement(), SettlementLabel::VerifiedCurrent);
    assert!(resume.execution_ready());
    assert!(
        resume.view().settlement.is_none(),
        "execution state never projects the settlement fact"
    );
}

#[test]
fn mutation_after_verification_returns_to_due() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    observed_exact_pass(&mut resume, 1, 2);
    assert_eq!(resume.settlement(), SettlementLabel::VerifiedCurrent);

    resume.observe_tool(&write_of("src/auth.rs", "v3"), 1, 3);
    assert_eq!(resume.validity(), VerificationState::Stale);
    assert_eq!(resume.settlement(), SettlementLabel::VerificationDue);
    assert!(!resume.execution_ready());
}

#[test]
fn failed_verification_is_due_not_settled() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    let mut fail = output("shell.exec", false, "tests failed");
    fail.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&fail, 1, 2);
    assert_eq!(resume.settlement(), SettlementLabel::VerificationDue);
    assert!(!resume.execution_ready());
}

#[test]
fn open_obligation_with_current_verification_is_working() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    observed_exact_pass(&mut resume, 1, 2);
    resume.observe_tool(&missing_program("prog_a", "fp-1"), 1, 3);
    assert_eq!(resume.settlement(), SettlementLabel::Working);
    assert!(!resume.execution_ready());

    // A typed resolution drains the ledger; the resolving command is itself
    // a world change, so settlement needs a fresh verification on top.
    resume.observe_tool(&resolved_command("prog_a", "scope-a", "fp-1"), 1, 4);
    observed_exact_pass(&mut resume, 1, 5);
    assert_eq!(resume.settlement(), SettlementLabel::VerifiedCurrent);
    assert!(resume.execution_ready());
}

#[test]
fn open_obligation_without_current_verification_is_working() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&missing_program("prog_a", "fp-1"), 1, 1);
    assert_eq!(resume.settlement(), SettlementLabel::Working);
    assert!(!resume.execution_ready());
}

#[test]
fn verified_current_survives_progress_only_anchor_change() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    observed_exact_pass(&mut resume, 1, 2);
    // Progress-only anchor advancement keeps the verification basis: the
    // current verifier stays current (basis == 0 on both sides).
    resume.anchor_revision = 5;
    assert_eq!(resume.settlement(), SettlementLabel::VerifiedCurrent);
    assert!(resume.execution_ready());

    // A boundary change to the verification basis reopens the obligation.
    resume.verification.spec_revision = 2;
    assert_eq!(resume.settlement(), SettlementLabel::VerificationDue);
    assert!(!resume.execution_ready());
}

#[test]
fn execution_ready_matches_verified_current_label() {
    let mut resume = ExecutionState::default();
    assert_eq!(
        resume.execution_ready(),
        resume.settlement() == SettlementLabel::VerifiedCurrent
    );
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    assert_eq!(
        resume.execution_ready(),
        resume.settlement() == SettlementLabel::VerifiedCurrent
    );

    observed_exact_pass(&mut resume, 1, 2);
    assert!(resume.execution_ready());
    assert_eq!(resume.settlement(), SettlementLabel::VerifiedCurrent);

    // Every reopening input must flip readiness and the label together.
    resume.observe_tool(&missing_program("prog_a", "fp-1"), 1, 3);
    assert_eq!(
        resume.execution_ready(),
        resume.settlement() == SettlementLabel::VerifiedCurrent
    );
    assert!(!resume.execution_ready());
    assert_eq!(resume.settlement(), SettlementLabel::Working);
}

/// Untrusted verifier: the same reuse class as `observed_exact_pass` but
/// without exact identity material, so the pass can never bind the world.
fn observed_untrusted_pass(resume: &mut ExecutionState, anchor_revision: u64, turn: u64) {
    let attribution = RuntimeExecutionAttribution {
        host: ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            Vec::<String>::new(),
            VerificationReuse::ExactCurrentWorld,
        ),
        rooted_targets: Vec::new(),
    };
    resume.observe_tool_attributed(
        &verified_ok(),
        anchor_revision,
        turn,
        "arg-settle",
        &attribution,
    );
}

/// A failed shell command: records a failed command whose Unknown-side
/// footprint bumps the world revision, so readiness must reopen.
fn failed_shell(command: &str) -> ToolOutput {
    let mut fail = output("shell.exec", false, "command failed");
    fail.metadata = json!({"command": command});
    fail
}

#[test]
fn trusted_verification_pass_keeps_an_unrelated_failure_blocker() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    resume.observe_tool(&failed_shell("cargo build"), 1, 2);
    assert_eq!(resume.failed_commands.len(), 1);
    assert!(!resume.execution_ready());

    // A trusted verification PASS is evidence for its declared verifier,
    // not a universal eraser for an unrelated shell failure.
    observed_exact_pass(&mut resume, 1, 3);
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(resume.settlement(), SettlementLabel::Working);
    assert!(!resume.execution_ready());
}

#[test]
fn untrusted_verification_pass_keeps_failed_attempts_open() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    resume.observe_tool(&failed_shell("cargo build"), 1, 2);
    assert_eq!(resume.failed_commands.len(), 1);

    // A PASS without exact identity cannot bind the world tuple: the
    // failure history stays open and readiness fails closed.
    observed_untrusted_pass(&mut resume, 1, 3);
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(resume.settlement(), SettlementLabel::Working);
    assert!(!resume.execution_ready());
}

#[test]
fn failed_verification_does_not_resolve_failure_history() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    resume.observe_tool(&failed_shell("cargo build"), 1, 2);
    let mut fail = output("shell.exec", false, "tests failed");
    fail.metadata = json!({"command": "cargo test", "verification": true});
    resume.observe_tool(&fail, 1, 3);
    assert!(!resume.failed_commands.is_empty());
    assert_eq!(resume.settlement(), SettlementLabel::VerificationDue);
    assert!(!resume.execution_ready());
}

#[test]
fn failure_after_trusted_verification_reblocks_readiness() {
    let mut resume = ExecutionState::default();
    resume.observe_tool(&write_of("src/auth.rs", "v2"), 1, 1);
    observed_exact_pass(&mut resume, 1, 2);
    assert!(resume.execution_ready());

    // A new failure after the trusted verification reopens the fail-closed
    // gate. Another unrelated PASS must not erase it.
    resume.observe_tool_with_digest(&failed_shell("cargo build"), 1, 3, "cargo-build-digest");
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(resume.settlement(), SettlementLabel::VerificationDue);
    assert!(!resume.execution_ready());

    observed_exact_pass(&mut resume, 1, 4);
    assert_eq!(resume.failed_commands.len(), 1);
    assert_eq!(resume.settlement(), SettlementLabel::Working);
    assert!(!resume.execution_ready());

    // The exact failed operation succeeds, which resolves its own blocker.
    // That command may have mutated the workspace, so a fresh verifier is
    // still required before readiness returns.
    let mut build_ok = output("shell.exec", true, "build succeeded");
    build_ok.metadata = json!({"command": "cargo build"});
    resume.observe_tool_with_digest(&build_ok, 1, 5, "cargo-build-digest");
    assert!(resume.failed_commands.is_empty());
    assert!(!resume.execution_ready());
    observed_exact_pass(&mut resume, 1, 6);
    assert_eq!(resume.settlement(), SettlementLabel::VerifiedCurrent);
    assert!(resume.execution_ready());
}
