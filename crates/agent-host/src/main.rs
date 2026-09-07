//! The `agent-host` binary: one workspace, one local Platform host.
//!
//! Composition mirrors the TUI's (same kernel, tools, approval and context
//! choices) but replaces the terminal UI with the local IPC server. The
//! process stays in the foreground owning its workspace; clients attach and
//! detach freely. `--read-only` serves snapshot/subscribe-only sessions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, ModelSelection, build_context_engine,
    compose, try_model_from_env,
};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate, TaskApprovalGate};
use agent_host::{
    HostPlane, HostServer, LocalEndpoint, SingleInstance, negotiated_profile,
    session_schema_digest_hex,
};
use agent_runtime::WorkControlSessionRegistry;
use agent_storage::FileEventJournal;
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use anyhow::Context as _;
use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

struct Args {
    workdir: Option<std::path::PathBuf>,
    pipe: Option<String>,
    socket: Option<std::path::PathBuf>,
    read_only: bool,
    restore_latest: bool,
    context_policy: Option<String>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        workdir: None,
        pipe: None,
        socket: None,
        read_only: false,
        restore_latest: false,
        context_policy: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workdir" => {
                args.workdir = Some(iter.next().context("--workdir needs a path")?.into())
            }
            "--pipe" => args.pipe = Some(iter.next().context("--pipe needs a name")?),
            "--socket" => args.socket = Some(iter.next().context("--socket needs a path")?.into()),
            "--read-only" => args.read_only = true,
            "--restore-latest" => args.restore_latest = true,
            "--context-policy" => {
                args.context_policy = Some(iter.next().context("--context-policy needs a value")?)
            }
            other => anyhow::bail!(
                "unknown argument {other:?}; see --help in the TUI for the full CLI story"
            ),
        }
    }
    if args.pipe.is_some() && args.socket.is_some() {
        anyhow::bail!("choose one endpoint: --pipe (Windows) or --socket (Unix)");
    }
    Ok(args)
}

fn resolve_endpoint(args: &Args) -> LocalEndpoint {
    match (&args.pipe, &args.socket) {
        (Some(name), _) => LocalEndpoint::NamedPipe(name.clone()),
        (None, Some(path)) => LocalEndpoint::UnixSocket(path.clone()),
        (None, None) => {
            if cfg!(windows) {
                LocalEndpoint::NamedPipe(agent_compose_endpoint_default())
            } else {
                LocalEndpoint::UnixSocket(default_socket_path())
            }
        }
    }
}

fn agent_compose_endpoint_default() -> String {
    // Matches the .NET client's AgentTransports.DefaultPipeName.
    "focus-agent.platform.v1".to_string()
}

fn default_socket_path() -> std::path::PathBuf {
    // Matches the .NET client's AgentTransports.DefaultSocketPath.
    std::path::PathBuf::from("/tmp/focus-agent-platform-v1.sock")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The re-entered host-death watchdog (PROCESS-01) must not run normal
    // startup; because this binary dispatches on the marker, arming the
    // Unix containment for the tool dispatcher below is legitimate.
    if agent_process::watchdog::run_if_armed_and_exit() {
        return Ok(());
    }
    real_main().await
}

async fn real_main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let root = args
        .workdir
        .clone()
        .unwrap_or(std::env::current_dir().context("current directory")?);
    let policy = match &args.context_policy {
        Some(raw) => ContextPolicy::from_str_checked(raw)?,
        None => ContextPolicy::Rolling,
    };

    let (model, provider_profile_digest) = match try_model_from_env()? {
        ModelSelection::Mock(mock) => {
            eprintln!("host: demo mode (AGENT_DEMO=1) selected the mock transport");
            (mock, None)
        }
        ModelSelection::Provider(provider, profile) => {
            eprintln!("host: {}", profile.banner());
            let digest = profile.digest();
            (provider, Some(digest))
        }
    };

    let workspace = Workspace::open(&root).await?;
    let single = SingleInstance::acquire(workspace.state_dir())?;
    eprintln!("host: workspace {}", root.display());

    let journal = Arc::new(FileEventJournal::open(workspace.state_dir().join("traces")).await?);
    let context_engine =
        build_context_engine(policy, workspace.state_dir(), Some(model.clone())).await?;
    let verification_recipes = Arc::new(VerificationRecipes::discover(&workspace)?);
    let host_policies = Arc::new(
        HostToolPolicyRegistry::with_builtins_and_verification(&verification_recipes)
            .map_err(anyhow::Error::msg)?,
    );

    // The approval plane is shared: compose wires the task gate into Core,
    // the work-control router answers pending requests through the same
    // interactive gate. In read-only mode Core stays policy-gated and the
    // router's grant denies approval responses entirely.
    let (approval, broker, gate) = if args.read_only {
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        (
            Arc::new(PolicyApprovalGate::read_only()) as Arc<dyn agent_contracts::ApprovalGate>,
            broker,
            gate,
        )
    } else {
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        let task_gate =
            Arc::new(TaskApprovalGate::new(gate.clone()).with_host_policies(host_policies.clone()));
        (
            task_gate.clone() as Arc<dyn agent_contracts::ApprovalGate>,
            broker,
            gate,
        )
    };

    let base_tools = Arc::new(
        BuiltinToolDispatcher::with_config_recipes_and_host_death_watchdog(
            workspace.clone(),
            Default::default(),
            (*verification_recipes).clone(),
            true,
        ),
    );
    let artifact_store = Arc::new(workspace.clone());
    let output_broker = Arc::new(WorkspaceOutputBroker::new(workspace.clone().into()));

    let composed = compose(ComposeConfig {
        provider_profile_digest,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model,
        approval,
        base_tools,
        capability_aware: true,
        journal: Some(journal),
        artifact_store: Some(artifact_store),
        output_broker: Some(output_broker),
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
        verification_recipes: Some(verification_recipes),
        project_proof_refresh: false,
        host_death_watchdog: true,
        mcp_servers: Vec::new(),
        plugins: None,
    })
    .await?;

    // Subscribe before start, exactly like every other composition root.
    let _run_events = composed.subscribe();
    composed.instance.start().await?;

    if args.restore_latest {
        let resolved = resolve_latest_checkpoint(&workspace.state_dir().join("checkpoints"))?;
        let checkpoint: agent_runtime::checkpoint::RuntimeCheckpoint =
            serde_json::from_str(&std::fs::read_to_string(&resolved)?)
                .with_context(|| format!("parsing {}", resolved.display()))?;
        composed.instance.restore(checkpoint).await?;
        eprintln!("host: restored {}", resolved.display());
    }

    let handle = composed.handle().clone();
    let registry = WorkControlSessionRegistry::new(handle.run_id());
    let profile = negotiated_profile()?;
    eprintln!(
        "host: negotiated profile schema digest {}",
        session_schema_digest_hex()
    );

    let plane = HostPlane {
        profile,
        handle,
        broker,
        gate,
        registry,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let endpoint = resolve_endpoint(&args);
    let server = HostServer {
        endpoint: endpoint.clone(),
        read_only: args.read_only,
        stop: Arc::clone(&stop),
    };

    // The accept loop blocks this thread; ctrl-c stops it through the
    // server's cooperative stop switch and releases the single-instance lock.
    let runtime_handle = tokio::runtime::Handle::current();
    let serve_thread = std::thread::spawn(move || server.serve(plane, runtime_handle));

    let _ = tokio::signal::ctrl_c().await;
    eprintln!("host: shutting down");
    composed.shutdown().await?;
    // Set the stop flag, then poke the endpoint once so the parked accept
    // loop wakes, observes the flag, and exits `Ok` on its own.
    stop.store(true, Ordering::SeqCst);
    wake_endpoint(&endpoint);
    let serve_result = serve_thread
        .join()
        .map_err(|_| anyhow::anyhow!("host serve thread panicked"))?;
    serve_result?;
    drop(single);
    Ok(())
}

/// Opens one throwaway local connection so a parked accept loop can wake up
/// and observe the stop flag. Best effort: if the endpoint is already gone,
/// the serve thread has exited and the join above reports its result.
#[cfg(windows)]
fn wake_endpoint(endpoint: &LocalEndpoint) {
    if let LocalEndpoint::NamedPipe(name) = endpoint {
        let _ = std::fs::File::open(format!(r"\\.\pipe\{name}"));
    }
}

#[cfg(unix)]
fn wake_endpoint(endpoint: &LocalEndpoint) {
    if let LocalEndpoint::UnixSocket(path) = endpoint {
        let _ = std::os::unix::net::UnixStream::connect(path);
    }
}

fn resolve_latest_checkpoint(dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    candidates.sort();
    candidates
        .pop()
        .context("no checkpoints exist to restore from")
}
