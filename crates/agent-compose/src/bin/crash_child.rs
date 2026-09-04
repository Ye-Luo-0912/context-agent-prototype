//! One scripted crash-child session for the durability acceptance tests.
//!
//! Runs a real composition over the workspace given as argv[1], submits
//! the user message argv[2] (the scripted model performs one real
//! `fs.write`), and waits for the turn to finish — unless an armed crash
//! failpoint (see `agent_runtime::crash`) terminates the process first the
//! way a hard kill would. Nothing here is test-only magic: this is the
//! product composition path with a scripted model.

use std::sync::Arc;
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    ToolCall,
};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use serde_json::json;

/// One scripted turn: emit one `fs.write`, then a plain completion once
/// the tool result is in the turn frame.
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
                    id: "crash-write-1".into(),
                    name: "fs.write".into(),
                    arguments: json!({
                        "path": "hello.txt",
                        "content": "hello from the crash flow",
                    }),
                }],
                usage: Default::default(),
            });
        }
        Ok(ModelOutput {
            content: "[scripted] wrote hello.txt - crash flow turn complete.".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().expect("workspace root required"));
    let prompt = args.next().expect("user message required");

    let workspace = Workspace::open(&root).await?;
    let journal = Arc::new(
        agent_storage::FileEventJournal::open(workspace.state_dir().join("traces")).await?,
    );
    let recipes = Arc::new(tool_runtime::VerificationRecipes::discover(&workspace)?);
    let has_recipes = !recipes.is_empty();
    let host_policies = Arc::new(
        agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
            .map_err(anyhow::Error::msg)?,
    );
    let composed = compose(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine: Arc::new(context_simple::SimpleContextEngine::new(
            context_simple::SimpleContextConfig::default(),
        )),
        model: Arc::new(ScriptedWriteModel),
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
        // Product-real durability: the persistent reservation journal the
        // interactive host defaults to, so a crash leaves reconcilable
        // reservation state rather than none at all.
        effect_reservation_journal: Some(
            workspace
                .state_dir()
                .join("authority")
                .join("broker-reservations.jsonl"),
        ),
        verification_recipes: if has_recipes { Some(recipes) } else { None },
        project_proof_refresh: has_recipes,
    })
    .await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    composed.handle().user_message(prompt).await?;

    // Wait for the scripted turn to finish, or die at an armed failpoint
    // mid-flow. A deadline only exists so a mis-armed test fails with a
    // readable message instead of hanging.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(envelope) = events.try_recv()
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("crash child: the runtime never finished the scripted turn");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Give the post-turn durable checkpoint drain a moment when no
    // failpoint fired, then shut down gracefully.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if let Ok(envelope) = events.try_recv()
            && matches!(envelope.event, RuntimeEvent::CheckpointDurable { .. })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    composed.shutdown().await?;
    Ok(())
}
