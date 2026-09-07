//! M17-B1 acceptance for the composition root's supervision gate: the
//! host-child ledger is reconciled with typed outcomes before the
//! workspace is reused, and an unresolved ledger refuses startup instead
//! of being conflated with "no pending children".

use std::io::Write as _;
use std::process::{Command, Stdio};

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;

/// A plain-text model: the gate under test runs before any model call, so
/// the transport only has to exist.
struct PlainModel;

#[async_trait::async_trait]
impl ModelTransport for PlainModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: false,
            max_output_tokens: 256,
            context_window: None,
        }
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "[scripted] idle".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// A live process this test owns, spawned as a group leader on Unix like
/// every production child (the contract the group kill relies on).
fn spawn_sleeper() -> (std::process::Child, u32) {
    #[cfg(unix)]
    let mut command = {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sleep");
        command.arg("30").process_group(0);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("ping");
        command.args(["-n", "30", "127.0.0.1"]);
        command
    };
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let child = command.spawn().unwrap();
    let pid = child.id();
    (child, pid)
}

fn ledger_path(workspace: &Workspace) -> std::path::PathBuf {
    workspace
        .state_dir()
        .join("authority")
        .join("host-children.jsonl")
}

fn write_ledger(workspace: &Workspace, line: serde_json::Value) {
    let path = ledger_path(workspace);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

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
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine: Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        model: Arc::new(PlainModel),
        approval: Arc::new(PolicyApprovalGate::permissive()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: false,
        journal: Some(journal),
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: false,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: None,
        verification_recipes: if has_recipes { Some(recipes) } else { None },
        project_proof_refresh: has_recipes,
        // Harness composition: no watchdog dispatch in this executable.
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    })
}

use std::sync::Arc;

/// A legacy row (no identity) for a LIVE pid is never trusted with a kill:
/// compose refuses to reuse the workspace and the process is untouched.
#[tokio::test]
async fn compose_refuses_a_workspace_with_an_unresolved_ledger_row() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, pid) = spawn_sleeper();
    {
        let workspace = Workspace::open(dir.path()).await.unwrap();
        write_ledger(&workspace, json!({ "pid": pid, "purpose": "verify.run" }));
    }
    let Err(error) = compose(compose_config(dir.path()).await.unwrap()).await else {
        panic!("an unresolved ledger row must refuse startup")
    };
    assert!(
        error.to_string().contains("unresolved host-child records"),
        "{error}"
    );
    assert!(
        child.try_wait().ok().flatten().is_none(),
        "the gate must not kill a pid it cannot verify"
    );
    let _ = child.kill();
    child.wait().unwrap();
}

/// A corrupted ledger is a typed refusal, never "no pending children".
#[tokio::test]
async fn compose_refuses_a_workspace_with_an_unreadable_ledger() {
    let dir = tempfile::tempdir().unwrap();
    {
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let path = ledger_path(&workspace);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"pid\": not-json\n").unwrap();
    }
    let Err(error) = compose(compose_config(dir.path()).await.unwrap()).await else {
        panic!("an unreadable ledger must refuse startup")
    };
    assert!(
        error.to_string().contains("could not be reconciled"),
        "{error}"
    );
}

/// The positive control: a row whose identity matches a live child is
/// reconciled (killed with confirmed exit) and startup proceeds.
#[tokio::test]
async fn compose_reconciles_a_matching_row_and_proceeds() {
    let dir = tempfile::tempdir().unwrap();
    let (child, pid) = spawn_sleeper();
    let identity_token = agent_process::capture_process_identity(pid)
        .unwrap()
        .identity_token;
    {
        let workspace = Workspace::open(dir.path()).await.unwrap();
        write_ledger(
            &workspace,
            json!({ "pid": pid, "identity": identity_token, "purpose": "process.run" }),
        );
    }
    let composed = compose(compose_config(dir.path()).await.unwrap())
        .await
        .expect("a resolvable ledger must not block startup");
    composed.shutdown().await.unwrap();
    // The reconciled child is confirmed dead (reaped by init here).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while agent_process::process_is_running(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the recorded leftover must have been killed by the startup gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(child);
}
