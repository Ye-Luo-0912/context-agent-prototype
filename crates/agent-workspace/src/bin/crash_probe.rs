//! Abrupt-death, lock-contention, and injected-disk-full probe for
//! workspace effect fixtures.
//!
//! Spawned only by agent-workspace integration tests. Mutating modes stage
//! Core-identified mutations against a shared workspace root according to
//! the requested mode, then die through `abort()` — no cleanup runs and no
//! destructor unwinds, so the durable journal frames are all that survives,
//! which is exactly what a power loss or `kill -9` leaves behind. The
//! non-mutating `hold` mode keeps the exclusive journal lock alive until a
//! release signal appears, then exits cleanly, letting tests exercise real
//! cross-process contention against the bounded retry window. The
//! `enospc_*` modes arm the feature-gated storage-full seam at one of the
//! three durability-relevant writes and exit cleanly: nothing abrupt
//! happens, so the classification a fixture asserts lives entirely in the
//! durable journal and filesystem the child leaves behind.

use std::path::Path;
use std::time::Duration;

use agent_contracts::{EffectReceipt, OperationEffectContext};
use agent_workspace::Workspace;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        panic!(
            "usage: crash_probe <workspace-root> <prepare|commit|partial|hold|enospc_intent|enospc_stage|enospc_commit>"
        );
    };
    let Some(mode) = args.next() else {
        panic!(
            "usage: crash_probe <workspace-root> <prepare|commit|partial|hold|enospc_intent|enospc_stage|enospc_commit>"
        );
    };

    match mode.as_str() {
        "hold" => hold_until_released(&root),
        "prepare" | "commit" | "partial" => mutate_then_abort(&root, &mode),
        #[cfg(feature = "test-faults")]
        "enospc_intent" | "enospc_stage" | "enospc_commit" => {
            mutate_with_injected_full_disk(&root, &mode)
        }
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

/// Fallible staging for modes whose injected fault is expected to refuse.
#[cfg(feature = "test-faults")]
async fn try_stage(
    workspace: &Workspace,
    context: &OperationEffectContext,
    relative: &str,
    content: &[u8],
) -> agent_contracts::AgentResult<agent_workspace::PreparedMutation> {
    let mutation = workspace
        .begin_mutation("fs.write", "write", relative)
        .await?;
    mutation
        .prepare_with_effect_context(content, context.clone())
        .await
}

/// Arm one storage-full fault point, run the matching mutation step, and
/// assert the injected refusal surfaced as an honest typed outcome. The
/// process exits cleanly so the parent can classify the aftermath.
#[cfg(feature = "test-faults")]
fn mutate_with_injected_full_disk(root: &str, mode: &str) {
    use agent_contracts::EffectDurability;
    use agent_workspace::StorageFaultPlan;

    let context_json = std::env::var("WORKSPACE_CRASH_CONTEXT")
        .expect("WORKSPACE_CRASH_CONTEXT must carry the serialized effect context");
    let context: OperationEffectContext =
        serde_json::from_str(&context_json).expect("deserialize the fault context");

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async move {
            let workspace = Workspace::open(Path::new(root))
                .await
                .expect("open the shared workspace");
            workspace.arm_storage_faults(Some(match mode {
                "enospc_intent" => StorageFaultPlan {
                    refuse_prepare_intent_append: true,
                    ..StorageFaultPlan::default()
                },
                "enospc_stage" => StorageFaultPlan {
                    stage_write_budget_bytes: Some(2),
                    ..StorageFaultPlan::default()
                },
                "enospc_commit" => StorageFaultPlan {
                    refuse_commit_record_append: true,
                    ..StorageFaultPlan::default()
                },
                other => panic!("unknown storage fault mode: {other}"),
            }));

            match mode {
                "enospc_intent" => match try_stage(&workspace, &context, "value.txt", b"new").await
                {
                    Ok(_) => panic!("an armed intent-append fault must refuse prepare"),
                    Err(error) => assert!(
                        error.to_string().contains("storage full"),
                        "unexpected refusal: {error}"
                    ),
                },
                "enospc_stage" => match try_stage(&workspace, &context, "value.txt", b"new").await
                {
                    Ok(_) => panic!("an armed stage-write fault must fail prepare"),
                    Err(error) => assert!(
                        error.to_string().contains("storage full"),
                        "unexpected refusal: {error}"
                    ),
                },
                "enospc_commit" => {
                    let prepared = stage(&workspace, &context, "value.txt", b"new").await;
                    let receipt = prepared.commit().await;
                    assert!(
                        matches!(
                            receipt,
                            EffectReceipt::Applied {
                                durability: EffectDurability::DurabilityFailed(_),
                                ..
                            }
                        ),
                        "a refused committed-record append must stay applied but not durably acknowledged"
                    );
                }
                other => panic!("unknown storage fault mode: {other}"),
            }
        });
}
