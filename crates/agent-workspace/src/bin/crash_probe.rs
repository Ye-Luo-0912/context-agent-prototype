//! Abrupt-death probe for workspace effect recovery fixtures.
//!
//! Spawned only by agent-workspace integration tests: it stages one
//! Core-identified mutation against a shared workspace root, optionally
//! commits it, then dies through `abort()`. No cleanup runs and no
//! destructor unwinds — the durable journal frames are all that survives,
//! which is exactly what a power loss or `kill -9` leaves behind.

use std::path::Path;

use agent_contracts::{EffectReceipt, OperationEffectContext};
use agent_workspace::Workspace;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        panic!("usage: crash_probe <workspace-root> <prepare|commit>");
    };
    let Some(mode) = args.next() else {
        panic!("usage: crash_probe <workspace-root> <prepare|commit>");
    };
    let context_json = std::env::var("WORKSPACE_CRASH_CONTEXT")
        .expect("WORKSPACE_CRASH_CONTEXT must carry the serialized effect context");
    let context: OperationEffectContext =
        serde_json::from_str(&context_json).expect("deserialize the crash context");

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            let workspace = Workspace::open(Path::new(&root))
                .await
                .expect("open the shared workspace");
            let mutation = workspace
                .begin_mutation("fs.write", "write", "value.txt")
                .await
                .expect("begin the staged mutation");
            let prepared = mutation
                .prepare_with_effect_context(b"new", context.clone())
                .await
                .expect("stage the write");
            match mode.as_str() {
                // Forget instead of drop: an abrupt kill never rolls back.
                "prepare" => std::mem::forget(prepared),
                "commit" => assert!(
                    matches!(prepared.commit().await, EffectReceipt::Applied { .. }),
                    "probe commit must apply before the abrupt exit"
                ),
                other => panic!("unknown crash_probe mode: {other}"),
            }
        });

    std::process::abort();
}
