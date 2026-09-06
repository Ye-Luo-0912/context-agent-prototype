//! M16-04 product-config save → end process → restore → continue. The
//! approval stack is the TUI shape (read-only base policy + standing
//! grants — NOT permissive), the dispatcher is capability-aware, the
//! effect-broker journal persists across the restart, and the supervision
//! ledger is reconciled at startup. The model is scripted; everything else
//! is the real product composition.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    RuntimeFailureClass, StandingGrant, ToolCall,
};
use agent_core::{PolicyApprovalGate, TaskApprovalGate};
use agent_workspace::Workspace;
use serde_json::json;

/// Writes one scripted file per model call: call N writes file N+1.
struct RestoreModel {
    step: AtomicUsize,
}

impl RestoreModel {
    fn new(start: usize) -> Self {
        Self {
            step: AtomicUsize::new(start),
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for RestoreModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 4096,
            context_window: None,
        }
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        // One scripted write per segment, then a plain final so the
        // continued turn ends in a TurnCompleted rather than running to
        // the default round budget.
        let tool_call = match step {
            0 => ("file_a.txt", "segment one"),
            1 => ("file_b.txt", "segment two"),
            _ => {
                return Ok(ModelOutput {
                    content: "[scripted] segment delivered".into(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                });
            }
        };
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("write-{step}"),
                name: "fs.write".into(),
                arguments: json!({
                    "path": tool_call.0,
                    "content": tool_call.1,
                }),
            }],
            usage: Default::default(),
        })
    }
}

fn standing_grant(id: &str, prefix: &str) -> String {
    serde_json::json!({
        "id": id,
        "risk": "WorkspaceWrite",
        "target": { "workspace_path_prefix": prefix },
        "constraint": {},
        "expires_at_ms": u64::MAX
    })
    .to_string()
}

async fn product_config(
    root: &std::path::Path,
    model: Arc<dyn ModelTransport>,
    max_tool_rounds: Option<usize>,
) -> anyhow::Result<ComposeConfig> {
    let workspace = Workspace::open(root).await?;
    let journal = Arc::new(
        agent_storage::FileEventJournal::open(workspace.state_dir().join("traces")).await?,
    );
    let recipes = Arc::new(tool_runtime::VerificationRecipes::discover(&workspace)?);
    let host_policies = Arc::new(
        agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
            .map_err(anyhow::Error::msg)?,
    );
    // TUI-shaped approval: read-only base policy; writes pass only through
    // standing grants. Not permissive.
    let task_gate = Arc::new(
        TaskApprovalGate::new(Arc::new(PolicyApprovalGate::read_only()))
            .with_host_policies(host_policies.clone()),
    );
    for grant in [
        standing_grant("grant-a", "file_a.txt"),
        standing_grant("grant-b", "file_b.txt"),
    ] {
        let grant: StandingGrant = serde_json::from_str(&grant)?;
        task_gate.grant(grant).await?;
    }
    let context_engine = agent_compose::build_context_engine(
        agent_compose::ContextPolicy::Dynamic,
        workspace.state_dir(),
        None,
    )
    .await?;
    let base_tools = Arc::new(
        tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            Default::default(),
            (*recipes).clone(),
        ),
    );
    let reservation = workspace
        .state_dir()
        .join("authority")
        .join("broker-reservations.jsonl");
    Ok(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model,
        approval: task_gate,
        base_tools,
        capability_aware: true,
        journal: Some(journal),
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: Some(reservation),
        verification_recipes: Some(recipes.clone()),
        project_proof_refresh: !recipes.is_empty(),
    })
}

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    matches: impl Fn(&RuntimeEvent) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if events
            .try_recv()
            .is_ok_and(|envelope| matches(&envelope.event))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runtime never {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Save → end process → restore → continue, across a real workspace: the
/// first segment writes file A under a 1-round budget and stops; a fresh
/// composition restores the checkpoint and /continue writes file B. Both
/// files land exactly once. This is the M16-04 product-config walkthrough.
#[tokio::test]
async fn product_save_restore_continue_across_two_segments() -> anyhow::Result<()> {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let checkpoint_path = root.join("state").join("checkpoints").join("m16.json");

    // ---- Session 1: segment one under a 1-round budget. ----
    let config = product_config(&root, Arc::new(RestoreModel::new(0)), Some(1)).await?;
    let composed = compose(config).await?;
    let mut events = composed.subscribe();
    let handle = composed.handle().clone();
    handle.start().await.unwrap();
    handle.set_focus("write both files".into()).await?;
    handle.user_message("write both files".into()).await?;
    wait_for(
        &mut events,
        |event| {
            matches!(
                event,
                RuntimeEvent::Failure {
                    class: RuntimeFailureClass::RoundBudget,
                    ..
                }
            )
        },
        "stop at the round budget",
    )
    .await;
    assert!(
        std::fs::read_to_string(root.join("file_a.txt"))
            .unwrap()
            .contains("segment one"),
        "segment one must land before the budget stop"
    );
    assert!(
        !root.join("file_b.txt").exists(),
        "segment two must not run before /continue"
    );
    let checkpoint = composed.instance.checkpoint().await?;
    std::fs::create_dir_all(checkpoint_path.parent().unwrap()).unwrap();
    std::fs::write(&checkpoint_path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    composed.shutdown().await?;

    // ---- Session 2: restore into a fresh composition, then continue. ----
    let config = product_config(&root, Arc::new(RestoreModel::new(1)), None).await?;
    let composed = compose(config).await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    let bytes = std::fs::read(&checkpoint_path).unwrap();
    let checkpoint: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&bytes).unwrap();
    checkpoint.validate()?;
    composed.instance.restore(checkpoint).await?;
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;

    composed.handle().continue_active_task().await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if root.join("file_b.txt").exists() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "/continue never wrote file B"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Segment two runs to its own plain-text end under the default budget.
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::TurnCompleted),
        "finish the continued segment",
    )
    .await;

    // Each file was written exactly once by its own segment.
    assert_eq!(
        std::fs::read_to_string(root.join("file_a.txt")).unwrap(),
        "segment one",
        "file A must be untouched by segment two"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("file_b.txt")).unwrap(),
        "segment two"
    );
    composed.shutdown().await?;
    Ok(())
}

/// A read-only final round must still leave a resumable snapshot at the
/// budget stop: the active task and its stored directive live only in the
/// runtime planes, so without a snapshot `--restore=latest` finds nothing
/// and the task cannot be continued across processes. Found by the
/// 2026-09-07 real-binary live walkthrough.
#[tokio::test]
async fn product_budget_stop_after_a_read_only_round_still_lands_a_resumable_checkpoint()
-> anyhow::Result<()> {
    struct ReadThenWriteModel {
        step: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ModelTransport for ReadThenWriteModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: true,
                tool_calls: true,
                max_output_tokens: 4096,
                context_window: None,
            }
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            let step = self.step.fetch_add(1, Ordering::SeqCst);
            match step {
                0 => Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "read-0".into(),
                        name: "fs.read".into(),
                        arguments: json!({"path": "notes.txt"}),
                    }],
                    usage: Default::default(),
                }),
                1 => Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "write-1".into(),
                        name: "fs.write".into(),
                        arguments: json!({
                            "path": "file_b.txt",
                            "content": "resumed content",
                        }),
                    }],
                    usage: Default::default(),
                }),
                _ => Ok(ModelOutput {
                    content: "[scripted] segment delivered".into(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                }),
            }
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("notes.txt"),
        "sections: intro
",
    )
    .unwrap();
    let checkpoint_dir = root.join(".focus-agent").join("checkpoints");

    // ---- Session 1: one read-only round, then the budget stop. ----
    let config = product_config(
        &root,
        Arc::new(ReadThenWriteModel {
            step: AtomicUsize::new(0),
        }),
        Some(1),
    )
    .await?;
    let composed = compose(config).await?;
    let mut events = composed.subscribe();
    let handle = composed.handle().clone();
    handle.start().await?;
    handle.set_focus("read then write".into()).await?;
    handle.user_message("read then write".into()).await?;
    wait_for(
        &mut events,
        |event| {
            matches!(
                event,
                RuntimeEvent::Failure {
                    class: RuntimeFailureClass::RoundBudget,
                    ..
                }
            )
        },
        "stop at the round budget",
    )
    .await;
    assert!(
        !root.join("file_b.txt").exists(),
        "the read-only round must not have written anything"
    );
    // The BudgetStopYield debt forces a resumable snapshot even though
    // nothing was mutated.
    let store = agent_runtime::CheckpointStore::new(checkpoint_dir.clone());
    let rows = store.list(5).await?;
    assert!(
        !rows.is_empty(),
        "the budget stop must land a resumable checkpoint"
    );
    composed.shutdown().await?;

    // ---- Session 2: restore the snapshot, continue, and the write lands. ----
    let config = product_config(
        &root,
        Arc::new(ReadThenWriteModel {
            step: AtomicUsize::new(1),
        }),
        None,
    )
    .await?;
    let composed = compose(config).await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    let stored = rows.first().unwrap();
    let bytes = std::fs::read(checkpoint_dir.join(&stored.artifact))?;
    // Store artifacts are envelope-wrapped: decode through the product
    // decoder, not a raw serde round-trip.
    let checkpoint = agent_runtime::decode_checkpoint_bytes(&bytes)?;
    checkpoint.validate()?;
    composed.instance.restore(checkpoint).await?;
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;

    composed.handle().continue_active_task().await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if root.join("file_b.txt").exists() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "/continue never wrote file B"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        std::fs::read_to_string(root.join("file_b.txt"))?,
        "resumed content",
        "the continued segment must land its write"
    );
    composed.shutdown().await?;
    Ok(())
}
