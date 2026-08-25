//! Portable disk-full fixtures through the feature-gated storage seam.
//!
//! Real volume exhaustion cannot be reproduced on CI, so these fixtures
//! arm the injected plan at each of the three durability-relevant writes
//! and pin the fail-closed aftermath: a refused intent leaves nothing, a
//! truncated stage cleans itself up behind a rolled-back intent, and a
//! refused committed record stays honestly "applied but not durably
//! acknowledged" with reopen classifying by hash evidence.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_contracts::{
    ArgumentDigest, EffectId, OperationEffectContext, OperationId, RunId, ToolOperationIdentity,
    TurnId,
};
use agent_workspace::{Workspace, WorkspaceEffectRecovery};

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
            call_id: "call-enospc".into(),
            tool_name: "fs.write".into(),
            argument_digest: ArgumentDigest::sha256_bytes(b"enospc"),
        },
        effect_id: EffectId::new(),
    }
}

/// Run the probe in an enospc mode; it must exit cleanly because nothing
/// abrupt happens — only a deterministic write refusal.
fn run_probe(root: &Path, mode: &str, context: &OperationEffectContext) {
    let program = crash_probe().expect("crash_probe builds with this package");
    let output = Command::new(program)
        .arg(root)
        .arg(mode)
        .env(
            "WORKSPACE_CRASH_CONTEXT",
            serde_json::to_string(context).expect("serialize the fault context"),
        )
        .output()
        .expect("run the crash_probe helper");
    assert!(
        output.status.success(),
        "an enospc probe exits cleanly (stderr: {})",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn no_staged_temp_left(root: &Path) -> bool {
    !std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
}

#[tokio::test]
async fn a_full_disk_during_the_intent_append_leaves_nothing_behind() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
    let context = crash_context();

    run_probe(directory.path(), "enospc_intent", &context);

    // No intent frame may exist: the refusal fired before the append.
    let authority = std::fs::read_to_string(
        directory
            .path()
            .join(".focus-agent/authority/workspace-effects.jsonl"),
    )
    .expect("the journal file itself still opens");
    assert!(
        authority.trim().is_empty(),
        "a refused intent append must not leave a frame: {authority}"
    );

    let reopened = Workspace::open(directory.path()).await.unwrap();
    assert!(matches!(
        reopened.reconcile_effect(&context).unwrap(),
        WorkspaceEffectRecovery::NotManaged
    ));
    assert_eq!(
        std::fs::read(directory.path().join("value.txt")).unwrap(),
        b"old",
        "a refused prepare never touches the live target"
    );
    assert!(no_staged_temp_left(directory.path()));
}

#[tokio::test]
async fn a_truncated_stage_rolls_back_and_cleans_its_own_temp() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
    let context = crash_context();

    // The probe's stage budget is 2 bytes against 3 content bytes, so the
    // temp exists truncated when the failure fires. The prepare path owns
    // the exclusive handle and must clean it before rolling the intent
    // back — no recovery ambiguity is allowed for its own staged file.
    run_probe(directory.path(), "enospc_stage", &context);

    assert_eq!(
        std::fs::read(directory.path().join("value.txt")).unwrap(),
        b"old",
        "a failed stage never touches the live target"
    );
    assert!(no_staged_temp_left(directory.path()));

    // The durable trail is exactly Prepared → RolledBack: both frames
    // carry bounded byte accounting, so reopen settles NotApplied without
    // needing to trust any in-memory state from the writer process.
    let authority = std::fs::read_to_string(
        directory
            .path()
            .join(".focus-agent/authority/workspace-effects.jsonl"),
    )
    .unwrap();
    let transitions: Vec<serde_json::Value> = authority
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .map(|frame: serde_json::Value| frame["record"]["transition"].clone())
        .collect();
    assert_eq!(transitions.len(), 2, "{transitions:?}");
    assert_eq!(transitions[0]["kind"], "prepared");
    assert_eq!(transitions[1]["kind"], "rolled_back");

    let reopened = Workspace::open(directory.path()).await.unwrap();
    match reopened.reconcile_effect(&context).unwrap() {
        WorkspaceEffectRecovery::NotApplied { tx_ids } => assert_eq!(tx_ids.len(), 1),
        other => panic!("unexpected recovery after a truncated stage: {other:?}"),
    }
}

#[tokio::test]
async fn a_refused_committed_record_recovers_as_applied_but_incomplete() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("value.txt"), b"old").unwrap();
    let context = crash_context();

    run_probe(directory.path(), "enospc_commit", &context);

    // The replace landed before the refusal, so the live target carries
    // the new bytes while the journal ends at the prepared frame.
    assert_eq!(
        std::fs::read(directory.path().join("value.txt")).unwrap(),
        b"new",
        "the atomic replace precedes the committed-record append"
    );
    let authority = std::fs::read_to_string(
        directory
            .path()
            .join(".focus-agent/authority/workspace-effects.jsonl"),
    )
    .unwrap();
    let last_frame = authority.lines().last().unwrap();
    let frame: serde_json::Value = serde_json::from_str(last_frame).unwrap();
    assert_eq!(frame["record"]["transition"]["kind"], "prepared");

    // Reopen classifies by hash evidence: target == after_hash means the
    // effect applied, but without a committed record it is not durably
    // acknowledged — Applied { complete: false }, never NotApplied.
    let reopened = Workspace::open(directory.path()).await.unwrap();
    match reopened.reconcile_effect(&context).unwrap() {
        WorkspaceEffectRecovery::Applied { tx_ids, complete } => {
            assert_eq!(tx_ids.len(), 1);
            assert!(!complete, "no committed record exists to acknowledge");
        }
        other => panic!("unexpected recovery after a refused commit record: {other:?}"),
    }

    // The classification is stable across reopens: hash evidence does not
    // decay, and recovery never invents the missing acknowledgement.
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
