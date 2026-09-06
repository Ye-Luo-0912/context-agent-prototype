//! Product-wiring acceptance for the F1/F2/F3 route slices, deterministic
//! and vendor-free: the `/work` composition (set_focus -> an empty tool
//! requirement set gaining `task.manage` -> one user message), the model
//! recording plan progress through the real `task.manage` tool, the
//! read-only plan view, the explicit round-budget stop, and `/continue`
//! restarting the stored directive — including across a checkpoint
//! restore into a fresh composition.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    RuntimeFailureClass, ToolCall,
};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;

/// A scripted model with a deterministic per-call script:
/// 1. segment 1 round 1: record plan progress through `task.manage`;
/// 2. segment 1 round 2: another tool call — round 3 would then be
///    requested and is refused by the explicit round budget;
/// 3. segment 2 round 1 (after `/continue`): a real workspace write;
/// 4. segment 2 round 2: plain text; any further calls: plain text.
struct RouteFlowModel {
    step: AtomicUsize,
}

impl RouteFlowModel {
    fn new() -> Self {
        Self {
            step: AtomicUsize::new(0),
        }
    }

    fn take_step(&self) -> usize {
        self.step.fetch_add(1, Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ModelTransport for RouteFlowModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 4096,
            context_window: None,
        }
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let plain = |content: &str| {
            Ok(ModelOutput {
                content: content.into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        };
        match self.take_step() {
            0 => Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "route-plan-1".into(),
                    name: "task.manage".into(),
                    arguments: json!({
                        "base_anchor_revision": 0,
                        "plan_progress": [
                            "[x] locate the config reader",
                            "[-] add the target key",
                            "[ ] run related checks"
                        ],
                        "open_loops": ["confirm the old default is unchanged"],
                        "next_action": "edit the parser and add a regression"
                    }),
                }],
                usage: Default::default(),
            }),
            // Another tool call keeps the turn alive: round 3 would be
            // requested and is refused by the explicit budget.
            1 => Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "route-write-0".into(),
                    name: "fs.write".into(),
                    arguments: json!({
                        "path": "seg1.txt",
                        "content": "written in segment 1",
                    }),
                }],
                usage: Default::default(),
            }),
            2 => Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "route-write-1".into(),
                    name: "fs.write".into(),
                    arguments: json!({
                        "path": "continued.txt",
                        "content": "written after /continue",
                    }),
                }],
                usage: Default::default(),
            }),
            3 => plain("[scripted] segment 2 ends without completion"),
            _ => plain("[scripted] idle"),
        }
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
    let has_recipes = !recipes.is_empty();
    let host_policies = Arc::new(
        agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
            .map_err(anyhow::Error::msg)?,
    );
    Ok(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine: Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        model,
        approval: Arc::new(PolicyApprovalGate::permissive()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: false,
        journal: Some(journal),
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        // The explicit finite per-turn budget a long task sets (--max-rounds).
        max_tool_rounds: Some(2),
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
async fn work_plan_budget_and_continue_flow_through_the_product_composition() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let checkpoint_path = root.join("state").join("checkpoints").join("route.json");

    // ---- Session 1: the /work composition starts the long task.
    let composed = compose(
        compose_config(&root, Arc::new(RouteFlowModel::new()))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let mut events = composed.subscribe();
    composed.instance.start().await.unwrap();
    let handle = composed.handle().clone();

    // /work step 1: set_focus creates (or resumes) the task while idle.
    handle
        .set_focus("migrate the config module".into())
        .await
        .unwrap();
    // /work step 2: the empty requirement set gains task.manage.
    let tasks = handle.list_tasks().await.unwrap();
    let task = tasks
        .iter()
        .find(|task| task.goal == "migrate the config module")
        .expect("set_focus must have created the task");
    assert_eq!(task.tool_requirement_count, 0);
    handle
        .replace_task_tool_requirements(
            task.id,
            task.tool_requirement_revision,
            vec![agent_contracts::ToolSurfaceRequirement {
                tool_name: "task.manage".into(),
                demand: agent_contracts::ToolSurfaceDemand::PreferSurface,
                reason: "long-task checklist".into(),
            }],
        )
        .await
        .unwrap();
    // /work step 3: the goal is delivered once through the normal path.
    handle
        .user_message("migrate the config module".into())
        .await
        .unwrap();

    // The model records plan progress through the real task.manage tool.
    wait_for(
        &mut events,
        |event| {
            matches!(
                event,
                RuntimeEvent::TaskProgressUpdated { accepted: true, .. }
            )
        },
        "accept the scripted task.manage proposal",
    )
    .await;

    // /plan reads the checklist back through the read-only view.
    let view = handle.task_plan_view().await.unwrap().expect("active task");
    assert_eq!(view.plan_progress.len(), 3, "{view:?}");
    assert!(view.plan_progress[0].starts_with("[x] locate"));
    assert_eq!(view.next_action, "edit the parser and add a regression");

    // With a two-round budget the turn stops as a deliberate refusal, not
    // a fault — and never completes by itself.
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

    // /continue starts a new segment on the same stored directive: no new
    // instruction identity, and the segment performs real new work.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match handle.continue_active_task().await {
            Ok(()) => break,
            Err(error) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the runtime never accepted /continue: {error}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::TaskContinuationStarted { .. }),
        "start the continuation segment",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !root.join("continued.txt").exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the continuation segment never wrote continued.txt"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        std::fs::read_to_string(root.join("continued.txt")).unwrap(),
        "written after /continue"
    );

    // ---- Session 2: checkpoint, restart, restore, and /continue again —
    // the checklist and the stored directive survive the restart.
    let checkpoint = composed.instance.checkpoint().await.unwrap();
    std::fs::create_dir_all(checkpoint_path.parent().unwrap()).unwrap();
    std::fs::write(&checkpoint_path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    composed.shutdown().await.unwrap();

    let composed = compose(
        compose_config(&root, Arc::new(RouteFlowModel::new()))
            .await
            .unwrap(),
    )
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

    // The checklist survives the roundtrip with the task.
    let view = composed
        .handle()
        .task_plan_view()
        .await
        .unwrap()
        .expect("the restored task is active");
    assert_eq!(view.plan_progress.len(), 3, "{view:?}");

    composed.handle().continue_active_task().await.unwrap();
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::TaskContinuationStarted { .. }),
        "start the post-restore continuation",
    )
    .await;
    composed.shutdown().await.unwrap();
}
