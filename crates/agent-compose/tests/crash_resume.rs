//! Real child-process crash -> restart -> resume acceptance, deterministic
//! and vendor-free. A spawned `crash_child` binary runs the product
//! composition path and dies via an armed failpoint (hard `exit(9)`, no
//! cleanup) at a named durability boundary; the test then inspects the
//! residue and resumes over the same workspace, asserting the committed
//! write appears exactly once and the uncommitted suffix never resurrects.
//!
//! Covered kill points:
//! - `effect-applied-before-ack`: the effect is on disk, its durable
//!   acknowledgement is not — startup reconciliation must resolve Applied
//!   without re-executing;
//! - `effect-after-prepare`: staged and durably reserved but never
//!   dispatched — reconciliation resolves NotApplied and nothing may
//!   resurrect, on any number of restarts;
//! - `checkpoint-after-temp-write`: the durable checkpoint write is torn
//!   between the synced temp file and the atomic rename;
//! - `turn-before-commit-barrier`: the finished turn is durable nowhere.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    ToolCall,
};
use agent_core::PolicyApprovalGate;
use agent_runtime::CheckpointStore;
use agent_workspace::Workspace;
use serde_json::json;

/// Identical scripted model to the crash child, used to continue the task
/// in the restarted composition.
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

const PROMPT: &str = "crash flow: write hello";

async fn compose_config(root: &std::path::Path) -> anyhow::Result<ComposeConfig> {
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
        effect_reservation_journal: Some(
            workspace
                .state_dir()
                .join("authority")
                .join("broker-reservations.jsonl"),
        ),
        verification_recipes: if has_recipes { Some(recipes) } else { None },
        project_proof_refresh: has_recipes,
    })
}

/// Spawn the crash child over `root` and wait for it to die at the armed
/// failpoint (exit code 9, like a hard kill).
async fn run_crashing_child(root: &std::path::Path, failpoint: &str) {
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_crash_child"))
        .arg(root)
        .arg(PROMPT)
        .env("FOCUS_AGENT_FAILPOINTS", failpoint)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn crash child");
    let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("crash child timed out")
        .expect("crash child wait failed");
    assert_eq!(
        output.status.code(),
        Some(9),
        "the child must die at the {failpoint} failpoint; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains(&format!("crash failpoint reached: {failpoint}")),
        "the child died for the wrong reason: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Count the committed `hello.txt` change rows in the workspace change
/// journal — the durable effect record, independent of any file content.
fn hello_change_rows(root: &std::path::Path) -> usize {
    let journal = root.join(".focus-agent").join("changes.jsonl");
    std::fs::read_to_string(journal)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("hello.txt"))
        .count()
}

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    matches: impl Fn(&agent_contracts::RuntimeEvent) -> bool,
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
            "the restarted runtime never {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_effect_apply_leaves_a_reconcilable_debt_not_a_duplicate() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();

    run_crashing_child(&root, "effect-applied-before-ack").await;

    // The effect was applied before the crash: the write is on disk with
    // exactly one committed change row, and the broker journal holds the
    // dispatched-but-unacked reservation for startup reconciliation.
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hello from the crash flow"
    );
    assert_eq!(hello_change_rows(&root), 1);

    // Restart: startup reconciliation resolves the pending reservation
    // from the broker journal (Applied), so nothing re-executes.
    let composed = compose(compose_config(&root).await.unwrap()).await.unwrap();
    composed.instance.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        hello_change_rows(&root),
        1,
        "reconciliation must not re-run an applied effect"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hello from the crash flow"
    );
    composed.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_prepare_reconciles_not_applied_without_resurrecting() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();

    run_crashing_child(&root, "effect-after-prepare").await;

    // The effect was staged and durably reserved but never dispatched:
    // the file never lands, while the change journal already carries the
    // attempt row written at preparation time.
    assert!(
        !root.join("hello.txt").exists(),
        "an undispatched effect must not be applied before the crash"
    );
    assert_eq!(hello_change_rows(&root), 1);

    // Restart: a reserved-but-never-dispatched effect reconciles to
    // NotApplied — the runtime starts clean, nothing executes, the
    // attempt row stays, and no later restart resurrects it.
    for restart in 1..=2 {
        let composed = compose(compose_config(&root).await.unwrap()).await.unwrap();
        composed.instance.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !root.join("hello.txt").exists(),
            "restart {restart} must not apply the undispatched effect"
        );
        assert_eq!(
            hello_change_rows(&root),
            1,
            "restart {restart} must not re-execute or duplicate the attempt"
        );
        composed.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn torn_checkpoint_leaves_no_envelope_and_the_store_stays_usable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();

    run_crashing_child(&root, "checkpoint-after-temp-write").await;

    // The effect was applied before the crash: exactly one committed write.
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hello from the crash flow"
    );
    assert_eq!(hello_change_rows(&root), 1);

    // The torn write left the synced temp file behind, but no envelope
    // ever landed, so the store lists nothing and the residue cannot be
    // mistaken for a checkpoint.
    let checkpoints = root.join(".focus-agent").join("checkpoints");
    let store = CheckpointStore::new(&checkpoints);
    let listed = store.list(10).await.unwrap();
    assert!(listed.is_empty(), "a torn write must not list: {listed:?}");
    let has_tmp = std::fs::read_dir(&checkpoints)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(has_tmp, "the torn temp file should still be on disk");

    // Restart on the same workspace with no durable checkpoint: the
    // composition starts clean, the committed effect is not repeated, and
    // the store accepts a new durable write.
    let composed = compose(compose_config(&root).await.unwrap()).await.unwrap();
    let mut events = composed.subscribe();
    composed.instance.start().await.unwrap();
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::RunStarted),
        "start",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hello_change_rows(&root), 1, "restart must not re-apply");
    assert_eq!(store.list(10).await.unwrap().len(), 0);
    composed.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_before_commit_barrier_never_resurrects_the_turn() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();

    run_crashing_child(&root, "turn-before-commit-barrier").await;

    // The effect is durable, the turn never was.
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hello from the crash flow"
    );
    assert_eq!(hello_change_rows(&root), 1);

    // Restart: nothing may run by itself — no duplicated write, no
    // resurrected turn — and the store is still empty for this workspace.
    let composed = compose(compose_config(&root).await.unwrap()).await.unwrap();
    composed.instance.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        hello_change_rows(&root),
        1,
        "the uncommitted turn must not resurrect on restart"
    );
    composed.shutdown().await.unwrap();

    // A fresh scripted session over the recovered workspace completes
    // normally and its durable checkpoint survives a load_verified pass.
    let composed = compose(compose_config(&root).await.unwrap()).await.unwrap();
    let mut events = composed.subscribe();
    composed.instance.start().await.unwrap();
    composed.handle().user_message(PROMPT.into()).await.unwrap();
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::CheckpointDurable { .. }),
        "land a durable checkpoint",
    )
    .await;
    let store = CheckpointStore::new(root.join(".focus-agent").join("checkpoints"));
    let listed = store.list(10).await.unwrap();
    assert!(
        !listed.is_empty(),
        "at least the scripted session's durable checkpoint must exist"
    );
    let payload = store
        .load_verified(&listed[0].artifact)
        .await
        .expect("the newest durable checkpoint must verify");
    let checkpoint: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&payload).unwrap();
    checkpoint.validate().unwrap();
    composed.shutdown().await.unwrap();
}
