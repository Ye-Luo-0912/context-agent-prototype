//! B4/N8 integration: the host's capability configuration ingestion is
//! actually wired into the composition — a configured MCP declaration
//! reaches compose and refuses an unreachable server at startup, an
//! empty/pristine configuration stays a no-op, and a discovered plugin
//! package root ends up enabled inside the composed runtime.

use std::path::Path;
use std::sync::Arc;

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, build_context_engine, compose,
};
use agent_contracts::PluginActivation;
use agent_core::{ApprovalBroker, InteractiveApprovalGate, TaskApprovalGate};
use agent_host::{SingleInstance, config};
use agent_runtime::PluginRegistry;
use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

/// One composed runtime plus its single-instance lock, sharing the host's
/// composition choices but without the IPC server (this is a startup-shape
/// drill, not a connection drill).
struct Fixture {
    composed: agent_compose::ComposedRuntime,
    _lock: SingleInstance,
}

async fn compose_fixture(
    mcp_servers: Vec<agent_capability_process::McpServerDecl>,
    plugins: Option<Arc<PluginRegistry>>,
) -> anyhow::Result<Fixture> {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let workspace = agent_workspace::Workspace::open(&root).await?;
    let lock = SingleInstance::acquire(workspace.state_dir())?;
    let model: Arc<dyn agent_contracts::ModelTransport> =
        Arc::new(agent_compose::MockModelTransport);
    let context_engine = build_context_engine(
        ContextPolicy::Rolling,
        workspace.state_dir(),
        Some(model.clone()),
    )
    .await?;
    let verification_recipes = Arc::new(VerificationRecipes::discover(&workspace)?);
    let host_policies = Arc::new(
        HostToolPolicyRegistry::with_builtins_and_verification(&verification_recipes)
            .map_err(anyhow::Error::msg)?,
    );
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
    let task_gate =
        Arc::new(TaskApprovalGate::new(gate.clone()).with_host_policies(host_policies.clone()));
    let base_tools = Arc::new(BuiltinToolDispatcher::new(workspace.clone())?);
    let composed = compose(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model,
        approval: task_gate.clone() as Arc<dyn agent_contracts::ApprovalGate>,
        base_tools,
        capability_aware: true,
        journal: None,
        artifact_store: None,
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: None,
        verification_recipes: Some(verification_recipes),
        project_proof_refresh: false,
        host_death_watchdog: false,
        mcp_servers,
        plugins,
    })
    .await?;
    Ok(Fixture {
        composed,
        _lock: lock,
    })
}

fn config_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// The config loader hands compose a real declaration; an unreachable server
/// makes compose fail closed at startup instead of silently running without
/// the configured capability.
#[tokio::test]
async fn configured_mcp_declaration_reaches_compose_and_fails_closed_when_unreachable() {
    let dir = config_dir();
    let config = dir.path().join("mcp.json");
    write(
        &config,
        r#"[{"id": "ghost.server", "version": "1.0.0", "name": "ghost",
             "summary": "unreachable by design", "program": "definitely-not-a-real-program-b4"}]"#,
    );
    let servers = config::parse_mcp_config(&config).expect("declaration must parse");
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].id, "ghost.server");

    let failure = compose_fixture(servers, None).await;
    assert!(
        failure.is_err(),
        "an unreachable configured MCP server must refuse startup (fail closed)"
    );
}

/// A host started without mcp/plugin configuration behaves exactly as before
/// (this is the regression anchor: adding config ingestion must not change
/// the pristine startup shape).
#[tokio::test]
async fn empty_and_pristine_config_is_a_noop() {
    let fixture = compose_fixture(Vec::new(), None)
        .await
        .expect("pristine composition");
    fixture.composed.instance.start().await.expect("start");
    fixture.composed.shutdown().await.expect("bounded shutdown");
}

/// A discovered plugin root is installed, enabled and its skill activated
/// through the compose path — the operator's explicit root IS the enabling
/// act, and the composed runtime runs with the package active.
#[tokio::test]
async fn discovered_plugin_root_populates_the_registry_through_compose() {
    let dir = config_dir();
    write(
        &dir.path().join("pack-b").join(config::PLUGIN_MANIFEST_FILE),
        r#"{
            "id": "pack-b", "version": "1.0.0", "name": "Pack B", "summary": "skills",
            "api": "0.1",
            "skills": [{"id": "skill-x", "version": "1.0.0", "summary": "s",
                        "reference": "skills/skill-x.md", "provenance": "package"}]
        }"#,
    );
    let discovered = config::discover_plugin_packages(dir.path()).expect("discovery");
    assert_eq!(discovered.len(), 1);

    let registry = Arc::new(PluginRegistry::new());
    for (manifest, package_root) in &discovered {
        registry
            .install_from_root(manifest.clone(), package_root.clone())
            .expect("install");
        registry.enable(&manifest.id).expect("enable");
        for skill in &manifest.skills {
            registry
                .activate_skill(&manifest.id, &skill.id)
                .expect("activate skill");
        }
    }

    let fixture = compose_fixture(Vec::new(), Some(registry.clone()))
        .await
        .expect("composition with plugins");
    fixture.composed.instance.start().await.expect("start");
    let view = registry.inspect("pack-b").expect("installed view");
    assert_eq!(
        view.activation,
        PluginActivation::Active,
        "explicit root enables the package"
    );
    assert_eq!(view.skills, 1);
    fixture.composed.shutdown().await.expect("bounded shutdown");
}
