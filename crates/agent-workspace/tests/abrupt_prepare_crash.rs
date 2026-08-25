//! Abrupt-process fixtures for Core-managed workspace prepare seams.
//!
//! The in-module journal tests simulate a crash by dropping the Workspace
//! handle. Here the writer is a separate OS process killed through
//! `abort()` with no cleanup path at all, so recovery must stand on the
//! durable journal alone: classification, temp cleanup, and staged-byte
//! accounting all have to work after a death no Drop could prepare for.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_contracts::{
    ArgumentDigest, EffectId, OperationEffectContext, OperationId, RunId, ToolOperationIdentity,
    TurnId,
};
use agent_workspace::{Workspace, WorkspaceEffectRecovery};

/// Locate the `crash_probe` helper binary. It is a bin target of this
/// package, so cargo places it beside the `deps/` directory this test
/// binary runs from; the generic probe covers both layouts.
fn crash_probe() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "crash_probe.exe"
    } else {
        "crash_probe"
    };
    let current = std::env::current_exe().ok()?;
    agent_process::probe_siblings(&current, name)
}

fn crash_context() -> OperationEffectContext {
    OperationEffectContext {
        identity: ToolOperationIdentity {
            run_id: RunId::new(),
            task_id: None,
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id: OperationId::new(),
            generation: 1,
            call_id: "call-abrupt".into(),
            tool_name: "fs.write".into(),
            argument_digest: ArgumentDigest::sha256_bytes(b"abrupt"),
        },
        effect_id: EffectId::new(),
    }
}

fn kill_probe_during(root: &Path, mode: &str, context: &OperationEffectContext) {
    let program = crash_probe().expect("crash_probe builds with this package");
    let output = Command::new(program)
        .arg(root)
        .arg(mode)
        .env(
            "WORKSPACE_CRASH_CONTEXT",
            serde_json::to_string(context).expect("serialize the crash context"),
        )
        .output()
        .expect("spawn crash_probe");
    assert!(
        !output.status.success(),
        "an abort() probe never exits cleanly (stderr: {})",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn abrupt_kill_after_prepare_recovers_not_applied_and_cleans_the_temp() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
    let context = crash_context();

    kill_probe_during(directory.path(), "prepare", &context);

    // The staged-byte accounting survived the death: the v2 frame carries
    // both lengths and both content hashes even though the writer died.
    // This raw read happens before any parent Workspace::open, because an
    // open workspace holds an exclusive lock on this same journal.
    let authority = std::fs::read_to_string(
        directory
            .path()
            .join(".focus-agent/authority/workspace-effects.jsonl"),
    )
    .unwrap();
    let frame: serde_json::Value = serde_json::from_str(authority.lines().next().unwrap()).unwrap();
    assert_eq!(frame["record"]["transition"]["bytes_before"], 3);
    assert_eq!(frame["record"]["transition"]["bytes_after"], 3);
    assert_eq!(
        frame["record"]["transition"]["before_hash"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(
        frame["record"]["transition"]["after_hash"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    let reopened = Workspace::open(directory.path()).await.unwrap();
    match reopened.reconcile_effect(&context).unwrap() {
        WorkspaceEffectRecovery::NotApplied { tx_ids } => assert_eq!(tx_ids.len(), 1),
        other => panic!("unexpected recovery after an abrupt prepare kill: {other:?}"),
    }
    assert_eq!(
        std::fs::read(directory.path().join("value.txt")).unwrap(),
        b"old",
        "a killed prepare must not touch the live target"
    );
    let leaked_temp = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(
        !leaked_temp,
        "recovery must clean the stage a killed process left behind"
    );

    drop(reopened);
    let again = Workspace::open(directory.path()).await.unwrap();
    assert!(matches!(
        again.reconcile_effect(&context).unwrap(),
        WorkspaceEffectRecovery::NotApplied { .. }
    ));
}

#[tokio::test]
async fn abrupt_kill_right_after_commit_recovers_durable_applied_content() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
    let context = crash_context();

    kill_probe_during(directory.path(), "commit", &context);

    let reopened = Workspace::open(directory.path()).await.unwrap();
    match reopened.reconcile_effect(&context).unwrap() {
        WorkspaceEffectRecovery::Applied { tx_ids, complete } => {
            assert_eq!(tx_ids.len(), 1);
            assert!(complete, "a committed single-target tx is complete");
        }
        other => panic!("unexpected recovery after an abrupt post-commit kill: {other:?}"),
    }
    assert_eq!(
        std::fs::read(directory.path().join("value.txt")).unwrap(),
        b"new",
        "the committed content must survive the kill"
    );

    drop(reopened);
    let again = Workspace::open(directory.path()).await.unwrap();
    assert!(matches!(
        again.reconcile_effect(&context).unwrap(),
        WorkspaceEffectRecovery::Applied { .. }
    ));
}

#[tokio::test]
async fn abrupt_kill_mid_batch_reports_applied_but_incomplete_and_cleans_the_remainder() {
    let directory = tempfile::tempdir().unwrap();
    let context = crash_context();

    kill_probe_during(directory.path(), "partial", &context);

    let reopened = Workspace::open(directory.path()).await.unwrap();
    match reopened.reconcile_effect(&context).unwrap() {
        WorkspaceEffectRecovery::Applied { tx_ids, complete } => {
            assert_eq!(tx_ids.len(), 2);
            assert!(
                !complete,
                "a batch whose second target never committed is incomplete"
            );
        }
        other => panic!("unexpected recovery after a mid-batch kill: {other:?}"),
    }
    assert_eq!(
        std::fs::read(directory.path().join("one.txt")).unwrap(),
        b"one",
        "the landed first target keeps its committed content"
    );
    assert!(
        !directory.path().join("second.txt").exists(),
        "the killed second stage must never surface as a target"
    );
    let leaked_temp = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(
        !leaked_temp,
        "recovery must clean the remainder a killed batch left behind"
    );

    drop(reopened);
    let again = Workspace::open(directory.path()).await.unwrap();
    assert!(matches!(
        again.reconcile_effect(&context).unwrap(),
        WorkspaceEffectRecovery::Applied {
            complete: false,
            ..
        }
    ));
}
