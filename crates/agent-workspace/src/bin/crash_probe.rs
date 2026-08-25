//! Abrupt-death and lock-contention probe for workspace effect fixtures.
//!
//! Spawned only by agent-workspace integration tests. Mutating modes stage
//! Core-identified mutations against a shared workspace root according to
//! the requested mode, then die through `abort()` — no cleanup runs and no
//! destructor unwinds, so the durable journal frames are all that survives,
//! which is exactly what a power loss or `kill -9` leaves behind. The
//! non-mutating `hold` mode keeps the exclusive journal lock alive until a
//! release signal appears, then exits cleanly, letting tests exercise real
//! cross-process contention against the bounded retry window.

use std::path::Path;
use std::time::Duration;

use agent_contracts::{EffectReceipt, OperationEffectContext};
use agent_workspace::Workspace;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        panic!("usage: crash_probe <workspace-root> <prepare|commit|partial|hold>");
    };
    let Some(mode) = args.next() else {
        panic!("usage: crash_probe <workspace-root> <prepare|commit|partial|hold>");
    };

    match mode.as_str() {
        "hold" => hold_until_released(&root),
        "prepare" | "commit" | "partial" => mutate_then_abort(&root, &mode),
        other => panic!("unknown crash_probe mode: {other}"),
    }

    // Only the clean `hold` path reaches this; mutating paths abort inside.
}

fn hold_until_released(root: &str) {
    let held_flag = std::env::var("WORKSPACE_CRASH_HELD").ok();
    let release_flag = std::env::var("WORKSPACE_CRASH_RELEASE")
        .expect("WORKSPACE_CRASH_RELEASE must name the release signal file");

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            let workspace = Workspace::open(Path::new(root))
                .await
                .expect("open the shared workspace to hold its journal lock");
            // Announce that this process now owns the lock so the test can
            // sequence its own attempt deterministically.
            if let Some(held) = held_flag {
                std::fs::write(&held, b"held").expect("write the held flag");
            }
            while !Path::new(&release_flag).exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(workspace);
        });
}

fn mutate_then_abort(root: &str, mode: &str) {
    let context_json = std::env::var("WORKSPACE_CRASH_CONTEXT")
        .expect("WORKSPACE_CRASH_CONTEXT must carry the serialized effect context");
    let context: OperationEffectContext =
        serde_json::from_str(&context_json).expect("deserialize the crash context");

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async move {
            let workspace = Workspace::open(Path::new(root))
                .await
                .expect("open the shared workspace");
            match mode {
                "prepare" => {
                    let prepared = stage(&workspace, &context, "value.txt", b"new").await;
                    // Forget instead of drop: an abrupt kill never rolls back.
                    std::mem::forget(prepared);
                }
                "commit" => {
                    let prepared = stage(&workspace, &context, "value.txt", b"new").await;
                    assert!(
                        matches!(prepared.commit().await, EffectReceipt::Applied { .. }),
                        "probe commit must apply before the abrupt exit"
                    );
                }
                // Land the first target of a two-target batch, then die
                // before the second commits: recovery must see the batch as
                // applied-but-incomplete and clean the surviving remainder.
                "partial" => {
                    let first = stage(&workspace, &context, "one.txt", b"one").await;
                    assert!(
                        matches!(first.commit().await, EffectReceipt::Applied { .. }),
                        "the first batch target must land before the kill"
                    );
                    let second = stage(&workspace, &context, "second.txt", b"two").await;
                    std::mem::forget(second);
                }
                other => panic!("unknown crash_probe mode: {other}"),
            }
        });

    std::process::abort();
}

async fn stage(
    workspace: &Workspace,
    context: &OperationEffectContext,
    relative: &str,
    content: &[u8],
) -> agent_workspace::PreparedMutation {
    let mutation = workspace
        .begin_mutation("fs.write", "write", relative)
        .await
        .expect("begin the staged mutation");
    mutation
        .prepare_with_effect_context(content, context.clone())
        .await
        .expect("stage the write")
}
