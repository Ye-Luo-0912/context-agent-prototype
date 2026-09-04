//! Product save -> kill -> restart -> resume acceptance, deterministic and
//! vendor-free. One scripted model turn performs a real workspace write
//! through the production tool surface; the runtime checkpoint is
//! persisted to the state dir; a fresh composition restores it; the
//! written file exists exactly once across the restart.

use std::sync::Arc;
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    ToolCall,
};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;

/// One scripted demo-model turn: emit one `fs.write`, then a plain text
/// completion once the tool result is in the turn frame.
struct ScriptedWriteModel;

#[async_trait::async_trait]
impl ModelTransport for ScriptedWriteModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 4096,
            context_window: None,
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let has_tool_result = request
            .messages
            .iter()
            .any(|message| message.role == agent_contracts::ModelRole::Tool);
        if !has_tool_result {
            return Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "flow-write-1".into(),
                    name: "fs.write".into(),
                    arguments: json!({
                        "path": "hello.txt",
                        "content": "hello from the product flow",
                    }),
                }],
                usage: Default::default(),
            });
        }
        Ok(ModelOutput {
            content: "[scripted] wrote hello.txt - flow complete.".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

async fn compose_config(
    root: &std::path::Path,
    model: Arc<dyn ModelTransport>,
) -> anyhow::Result<ComposeConfig> {
    let workspace = Workspace::open(root).await?;
    let journal = Arc::new(
        agent_storage::FileEventJournal::open(workspace.state_dir().join("traces")).await?,
    );
    let recipes = Arc::new(tool_runtime::VerificationRecipes::discover(&workspace)?);
    // A bare workspace discovers no recipes: proof refresh degrades to off
    // instead of failing the boot (a product root must start anywhere).
    let has_recipes = !recipes.is_empty();
    let host_policies = Arc::new(
        agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
            .map_err(anyhow::Error::msg)?,
    );
    Ok(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        // The shadow frame compiler runs for real in this acceptance flow:
        // one manifest per model round must reach the event stream.
        shadow_context_frame: true,
        workspace: workspace.clone(),
        context_engine: Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        model,
        approval: Arc::new(PolicyApprovalGate::permissive()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: false,
        journal: Some(journal),
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: None,
        verification_recipes: if has_recipes { Some(recipes) } else { None },
        project_proof_refresh: has_recipes,
    })
}

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    matches: impl Fn(&agent_contracts::RuntimeEvent) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
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

#[tokio::test]
async fn save_restart_resume_writes_exactly_once() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let checkpoint_path = root.join("state").join("checkpoints").join("flow.json");

    // ---- First session: the scripted turn writes hello.txt through the
    // production tool surface, then the runtime checkpoints.
    let model: Arc<dyn ModelTransport> = Arc::new(ScriptedWriteModel);
    let composed = compose(compose_config(&root, model.clone()).await.unwrap())
        .await
        .unwrap();
    let mut events = composed.subscribe();
    composed.instance.start().await.unwrap();
    composed
        .handle()
        .user_message("demo: write hello".into())
        .await
        .unwrap();
    // The turn finishes through two model rounds; the shadow Context Frame
    // manifest is emitted during each round's preparation, so track it in
    // the same drain instead of after the turn event.
    let mut shadow_manifest_seen = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ContextFrameShadow { .. } => shadow_manifest_seen = true,
                RuntimeEvent::TurnCompleted => break,
                _ => {}
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runtime never finished the scripted turn"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        shadow_manifest_seen,
        "the shadow frame manifest must reach the event stream"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hello from the product flow"
    );
    let checkpoint = composed.instance.checkpoint().await.unwrap();
    std::fs::create_dir_all(checkpoint_path.parent().unwrap()).unwrap();
    std::fs::write(&checkpoint_path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    composed.shutdown().await.unwrap();

    // ---- Restart: a fresh composition over the same workspace restores
    // the persisted checkpoint; the written file exists exactly once.
    let model: Arc<dyn ModelTransport> = Arc::new(ScriptedWriteModel);
    let composed = compose(compose_config(&root, model.clone()).await.unwrap())
        .await
        .unwrap();
    let mut events = composed.subscribe();
    composed.instance.start().await.unwrap();
    let bytes = std::fs::read(&checkpoint_path).unwrap();
    let checkpoint: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&bytes).unwrap();
    checkpoint.validate().unwrap();
    composed.instance.restore(checkpoint).await.unwrap();
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;
    let hello = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() == "hello.txt")
        .count();
    assert_eq!(hello, 1, "the resumed session must not duplicate effects");
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hello from the product flow"
    );
    composed.shutdown().await.unwrap();
}
