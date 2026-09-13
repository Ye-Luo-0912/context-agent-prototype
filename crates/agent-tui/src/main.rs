mod args;
mod cli;
mod doctor;
mod session;
mod state;
mod ui;
mod work;

use std::{io, sync::Arc};

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, build_context_engine, compose,
    maintenance_budget_from_env, try_maintenance_transport_from_env, try_model_from_env,
};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate, TaskApprovalGate};
use agent_storage::FileEventJournal;
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use anyhow::Context;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use session::{
    InteractiveHandle, TerminalSink, TerminalSource, load_runtime_checkpoint,
    resolve_latest_checkpoint, run_session,
};
use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

fn main() -> anyhow::Result<()> {
    // The re-entered host-death watchdog (PROCESS-01) must not run normal
    // startup: it lives only to outlive a crashed host and kill the
    // watched process group.
    if agent_process::watchdog::run_if_armed_and_exit() {
        return Ok(());
    }
    real_main()
}

#[tokio::main]
async fn real_main() -> anyhow::Result<()> {
    let mut args = args::parse_args(std::env::args().skip(1))?;
    if args.help {
        args::print_usage();
        return Ok(());
    }
    if let Some(raw) = args.prompt.take() {
        args.prompt = Some(cli::resolve_prompt(&raw)?);
    }
    args.grant_args = cli::collect_grants(&args.grant_files, &args.grant_args)?;
    let read_only = args.read_only;
    let restore_arg = args.restore_arg.clone();
    let grant_args = args.grant_args.clone();
    let max_rounds = args.max_rounds;
    let defer_proof = args.defer_proof;
    let effect_reservation_journal = args.effect_reservation_journal.clone();
    // An invalid checkpoint must fail before the workspace, host or any
    // state is touched: read, parse and validate an explicit path up
    // front. `--restore=latest` resolves after the workspace opens, then
    // validates the same way before anything starts.
    let restore_latest = matches!(&restore_arg, Some(path) if path.as_os_str() == "latest");
    let restore_checkpoint = match (&restore_arg, restore_latest) {
        (Some(path), false) => Some(load_runtime_checkpoint(path)?),
        _ => None,
    };
    let policy = ContextPolicy::from_str_checked(&args.context_policy)?;
    let root = args
        .root
        .clone()
        .unwrap_or(std::env::current_dir().context("current directory")?);
    if args.doctor_mode {
        let code = doctor::run_doctor(root).await;
        std::process::exit(code);
    }
    // Model configuration is a pure preflight: validate the provider env
    // BEFORE the workspace or journal exists, so a bad key or profile is a
    // configuration error that leaves no runtime state behind (M16-01).
    // Doctor above deliberately runs keyless and stays before this.
    let (model, serving_banner, provider_profile_digest) = match try_model_from_env()? {
        agent_compose::ModelSelection::Mock(mock) => {
            eprintln!("demo mode: AGENT_DEMO=1 selected the explicit mock transport");
            (
                mock,
                "serving: demo mock transport (AGENT_DEMO=1)".to_string(),
                None,
            )
        }
        agent_compose::ModelSelection::Provider(provider, profile) => {
            eprintln!("{}", profile.banner());
            let digest = profile.digest();
            let banner = format!("serving: {} | profile digest {digest}", profile.banner());
            (provider, banner, Some(digest))
        }
    };
    let workspace = Workspace::open(&root).await?;
    let restore_checkpoint = if restore_latest {
        let resolved = resolve_latest_checkpoint(&workspace.state_dir().join("checkpoints"))?;
        let checkpoint = load_runtime_checkpoint(&resolved)?;
        Some((resolved, checkpoint))
    } else {
        restore_checkpoint.map(|checkpoint| (restore_arg.clone().unwrap_or_default(), checkpoint))
    };
    let journal = Arc::new(FileEventJournal::open(workspace.state_dir().join("traces")).await?);

    // The context engine and the model are composition-root choices shared
    // with CLI/eval (agent-compose): the same kernel, tools and UI run
    // against any `ContextEngine` implementation (the A/B/C baselines, and
    // the process-boundary adapter). Rolling/dynamic 与 live eval 共用同一
    // 有界压缩器，避免 TUI 仍走占位折叠。
    let maintenance_budget = maintenance_budget_from_env()?;
    let context_engine = build_context_engine(
        policy,
        workspace.state_dir(),
        Some(model.clone()),
        try_maintenance_transport_from_env()?,
        &maintenance_budget,
    )
    .await?;
    // 授权映射是组合根的决定：一份内置注册表同时交给审批门、能力
    // 分发器与内核租约路径。
    let verification_recipes = Arc::new(VerificationRecipes::discover(&workspace)?);
    let host_policies = Arc::new(
        HostToolPolicyRegistry::with_builtins_and_verification(&verification_recipes)
            .map_err(anyhow::Error::msg)?,
    );
    let (approval, interactive) = if read_only {
        (
            Arc::new(PolicyApprovalGate::read_only()) as Arc<dyn agent_contracts::ApprovalGate>,
            None,
        )
    } else if args.is_headless() {
        (
            cli::headless_approval(false, &grant_args, host_policies.clone()).await?,
            None,
        )
    } else {
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        let task_gate =
            Arc::new(TaskApprovalGate::new(gate.clone()).with_host_policies(host_policies.clone()));
        for json in &grant_args {
            let grant: agent_contracts::StandingGrant = serde_json::from_str(json)
                .with_context(|| format!("invalid --grant JSON: {json}"))?;
            task_gate.grant(grant).await?;
        }
        (
            task_gate.clone() as Arc<dyn agent_contracts::ApprovalGate>,
            Some(InteractiveHandle {
                broker,
                gate,
                task_grants: task_gate,
            }),
        )
    };

    // One shared composition (agent-compose): the module host, the
    // capability-aware dispatcher, the optional event/artifact modules and
    // the kernel services are wired identically for TUI/CLI/eval. The
    // actor is spawned but not started yet — subscribe first so
    // `RunStarted` is observable.
    let checkpoint_dir = workspace.state_dir().join("checkpoints");
    // Unix host-death containment is armed here: this binary's main
    // dispatches on the watchdog marker, so re-entering it is safe.
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
    // 交互运行默认启用持久预留屏障：路径落在状态目录的权威层，
    // 可用 --effect-reservation-journal= 覆盖。崩溃后启动对账按
    // 经纪预留分类未决操作；评估组合保持 None，不扰动冻结测量。
    let reservation_journal = effect_reservation_journal.unwrap_or_else(|| {
        workspace
            .state_dir()
            .join("authority")
            .join("broker-reservations.jsonl")
    });
    // After the workspace exists (so `.focus-agent/...` paths work) and
    // before the actor starts, so a missing parent directory fails closed.
    let mut jsonl_sink = if args.is_headless() {
        if let Some(path) = args.jsonl_out.as_deref() {
            eprintln!("jsonl: {}", path.display());
        }
        Some(cli::JsonlWriter::open(args.jsonl_out.as_deref())?)
    } else {
        None
    };
    let composed = compose(ComposeConfig {
        provider_profile_digest,
        defer_proof_refresh: defer_proof,
        shadow_context_frame: false,
        workspace,
        context_engine,
        model,
        approval,
        base_tools,
        capability_aware: true,
        journal: Some(journal),
        artifact_store: Some(artifact_store),
        output_broker: Some(output_broker),
        max_tool_rounds: max_rounds,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: Some(reservation_journal),
        verification_recipes: Some(verification_recipes.clone()),
        project_proof_refresh: !verification_recipes.as_ref().is_empty(),
        // One supervision decision for both process lanes: this binary's
        // main dispatches on the watchdog marker, so the host proof lane
        // receives the same containment the dispatcher got above (M17-B1).
        host_death_watchdog: true,
        // Harness/eval compositions register no external capabilities by default.
        mcp_servers: Vec::new(),
        plugins: None,
    })
    .await?;
    let mut runtime_events = composed.subscribe();
    composed.instance.start().await?;
    if let Some((path, checkpoint)) = restore_checkpoint {
        composed
            .instance
            .restore(checkpoint)
            .await
            .map_err(|error| {
                anyhow::Error::new(error).context(format!(
                    "startup restore from {} failed; the runtime refused the checkpoint before any mutation",
                    path.display()
                ))
            })?;
    }

    if args.is_headless() {
        eprintln!("{serving_banner}");
        let action = if args.continue_task {
            cli::HeadlessAction::Continue
        } else {
            cli::HeadlessAction::Prompt {
                text: args
                    .prompt
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("--prompt is required for a headless run"))?,
                work: args.work,
            }
        };
        let jsonl = jsonl_sink
            .take()
            .ok_or_else(|| anyhow::anyhow!("headless JSONL sink was not opened"))?;
        let run = cli::run_headless(
            composed.handle().clone(),
            &mut runtime_events,
            action,
            args.headless_timeout(),
            jsonl,
        )
        .await;
        let shutdown_result = composed.shutdown().await;
        return match (run, shutdown_result) {
            (Err(run_error), _) => Err(run_error),
            (Ok(_), Err(shutdown_error)) => {
                Err(anyhow::Error::new(shutdown_error).context("runtime shutdown failed"))
            }
            (Ok((outcome, _sink)), Ok(())) => std::process::exit(outcome.exit),
        };
    }
    debug_assert!(jsonl_sink.is_none());

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    let result = run_session(
        &mut TerminalSource,
        &mut TerminalSink::new(&mut terminal),
        composed.handle().clone(),
        &composed.instance,
        &mut runtime_events,
        interactive,
        policy.as_str(),
        checkpoint_dir,
        serving_banner,
        max_rounds,
        defer_proof,
    )
    .await;

    // cancel -> stop actor (flush journal, RunCompleted) -> stop modules ->
    // join the actor; any failure is aggregated into one error.
    let shutdown_result = composed.shutdown().await;
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    match (result, shutdown_result) {
        (Err(ui_error), _) => Err(ui_error),
        (Ok(()), Err(shutdown_error)) => {
            Err(anyhow::Error::new(shutdown_error).context("runtime shutdown failed"))
        }
        (Ok(()), Ok(())) => Ok(()),
    }
}
