//! The `agent-host` binary: one workspace, one local Platform host.
//!
//! Composition mirrors the TUI's (same kernel, tools, approval and context
//! choices) but replaces the terminal UI with the local IPC server. The
//! process stays in the foreground owning its workspace; clients attach and
//! detach freely. `--read-only` serves snapshot/subscribe-only sessions.
//! `--restore-latest` resumes the newest verifiable checkpoint store
//! artifact before the server accepts any client.

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
    /// Resume from the newest *verifiable* checkpoint in the workspace
    /// store (envelope checksum, bounds and compatibility verified through
    /// the runtime's own store — never a file-name guess; a refused store
    /// fails startup closed).
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

fn resolve_endpoint(args: &Args, workspace_root: &std::path::Path) -> LocalEndpoint {
    match (&args.pipe, &args.socket) {
        (Some(name), _) => LocalEndpoint::NamedPipe(name.clone()),
        (None, Some(path)) => LocalEndpoint::UnixSocket(path.clone()),
        (None, None) => default_endpoint(workspace_root),
    }
}

#[cfg(windows)]
fn default_endpoint(_workspace_root: &std::path::Path) -> LocalEndpoint {
    // Matches the .NET client's AgentTransports.DefaultPipeName; the pipe
    // name space is additionally guarded by the current-user DACL and the
    // per-connection token check, and FILE_FLAG_FIRST_PIPE_INSTANCE
    // refuses takeover of a live pipe.
    LocalEndpoint::NamedPipe("focus-agent.platform.v1".to_string())
}

#[cfg(unix)]
fn default_endpoint(workspace_root: &std::path::Path) -> LocalEndpoint {
    // Never a fixed global /tmp name: the endpoint is user-private and
    // workspace-scoped. Clients that rely on the old fixed default must
    // pass --socket (or derive the same workspace-scoped path) explicitly.
    LocalEndpoint::UnixSocket(agent_host::default_socket_path_for(workspace_root))
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
        // The same resolution the TUI's `--restore=latest` performs: the
        // newest store artifact that fully verifies (envelope checksum,
        // bounds, version/compat), skipping invalid candidates visibly and
        // failing closed when none verifies.
        let (checkpoint, resolved) = agent_host::resolve_latest_verified_checkpoint(
            &workspace.state_dir().join("checkpoints"),
        )
        .await?;
        // The full cross-plane restore transaction — the same semantics the
        // TUI and every other composition root use. This runs before the
        // IPC server accepts its first connection, so a refused checkpoint
        // exits the host before any work submission or model request can
        // happen; nothing is half-restored into a serving host.
        composed
            .instance
            .restore(checkpoint)
            .await
            .map_err(|error| {
                anyhow::Error::new(error).context(format!(
                    "startup restore from {} failed; the runtime refused the checkpoint before any mutation",
                    resolved.display()
                ))
            })?;
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
    let endpoint = resolve_endpoint(&args, &root);
    let server = HostServer {
        endpoint: endpoint.clone(),
        read_only: args.read_only,
        stop: Arc::clone(&stop),
    };

    // The accept loop blocks this thread; ctrl-c stops it through the
    // server's cooperative stop switch and releases the single-instance lock.
    let runtime_handle = tokio::runtime::Handle::current();
    // The serve thread announces its own exit through this channel, so an
    // early failure (refused endpoint bind, takeover check, accept-loop
    // error) is observed below instead of leaving the process parked on
    // ctrl-c while it already stopped serving.
    let (serve_done_tx, serve_done_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_thread = std::thread::spawn(move || {
        let result = server.serve(plane, runtime_handle);
        let _ = serve_done_tx.send(());
        result
    });

    // Either ctrl-c or the serve thread ending first ends this wait (B2
    // CANCEL-ALL): a host whose serve loop already failed must converge
    // through the same bounded shutdown, not wait for a signal that may
    // never come.
    let _ = tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = serve_done_rx => {},
    };
    eprintln!("host: shutting down");
    // The runtime shutdown result is captured, not propagated inline: a
    // failed shutdown must not skip the transport stop below — the accept
    // loop and every live connection still get their bounded wind-down, and
    // the failure is surfaced only after the transport has actually stopped.
    let composed_shutdown = composed.shutdown().await;
    // Set the stop flag, then poke the endpoint once so the parked accept
    // loop wakes, observes the flag, and exits `Ok` on its own.
    stop.store(true, Ordering::SeqCst);
    wake_endpoint(&endpoint);
    let serve_result = serve_thread
        .join()
        .map_err(|_| anyhow::anyhow!("host serve thread panicked"))?;
    serve_result?;
    drop(single);
    composed_shutdown?;
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
