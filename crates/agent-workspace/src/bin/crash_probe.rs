//! Abrupt-death probe for workspace effect recovery fixtures.
//!
//! Spawned only by agent-workspace integration tests: it stages Core-
//! identified mutations against a shared workspace root according to the
//! requested mode, then dies through `abort()`. No cleanup runs and no
//! destructor unwinds — the durable journal frames are all that survives,
//! which is exactly what a power loss or `kill -9` leaves behind.

use std::path::Path;

use agent_contracts::{EffectReceipt, OperationEffectContext};
use agent_workspace::Workspace;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        panic!("usage: crash_probe <workspace-root> <prepare|commit|partial>");
    };
    let Some(mode) = args.next() else {
        panic!("usage: crash_probe <workspace-root> <prepare|commit|partial>");
    };
    let context_json = std::env::var("WORKSPACE_CRASH_CONTEXT")
        .expect("WORKSPACE_CRASH_CONTEXT must carry the serialized effect context");
    let context: OperationEffectContext =
        serde_json::from_str(&context_json).expect("deserialize the crash context");

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async move {
            let workspace = Workspace::open(Path::new(&root))
                .await
                .expect("open the shared workspace");
            match mode.as_str() {
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
