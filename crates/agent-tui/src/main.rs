mod args;
mod cli;
mod doctor;
mod session;
mod state;
mod ui;
mod work;

use std::{
    io,
    sync::{Arc, Mutex},
};

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, build_context_engine, compose,
    maintenance_budget_from_env, try_maintenance_transport_from_env, try_model_from_env,
};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate, TaskApprovalGate};
use agent_storage::FileEventJournal;
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use anyhow::Context;
use crossterm::{
    cursor::Show,
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use session::{
    InteractiveHandle, TerminalSink, TerminalSource, load_runtime_checkpoint,
    resolve_latest_checkpoint, run_session,
};
use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

// ===== U5: terminal session guard =====
//
// The interactive terminal is engaged in three independent steps: raw mode,
// the alternate screen, and clearing it. The old code enabled raw mode with
// `?` early returns between the steps, so any `?` after `enable_raw_mode()`
// bypassed the tail restore and left the terminal wedged (raw/alternate,
// cursor hidden). The guard tracks exactly which states *this process*
// turned on and restores them on early return, on `Drop`, and in a panic
// hook. It does NOT claim to recover a terminal killed by SIGKILL — only the
// states this process itself engaged.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermStep {
    EnableRaw,
    EnterAlt,
    Clear,
    ShowCursor,
    LeaveAlt,
    DisableRaw,
}

/// The terminal operations the guard drives. Abstracted so the guard's
/// state machine is unit-testable without a PTY (see `RecordingTermBackend`
/// in the `guard_tests` module).
trait TermBackend: Send {
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn clear(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
    /// Real backends own the live `Terminal`; fakes return `None` so the
    /// session can still draw through it while keeping the guard generic.
    fn terminal_mut(&mut self) -> Option<&mut Terminal<CrosstermBackend<io::Stdout>>>;
}

struct RealTermBackend {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl RealTermBackend {
    fn new() -> io::Result<Self> {
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }
}

impl TermBackend for RealTermBackend {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()
    }
    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.terminal.clear()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }
    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }
    fn disable_raw_mode(&mut self) -> io::Result<()> {
        terminal::disable_raw_mode()
    }
    fn terminal_mut(&mut self) -> Option<&mut Terminal<CrosstermBackend<io::Stdout>>> {
        Some(&mut self.terminal)
    }
}

/// Tracks which interactive-terminal states this process engaged. Restores
/// them (in reverse order) on `restore`, `Drop`, and via the panic hook.
#[derive(Debug)]
struct TerminalGuard<B: TermBackend> {
    backend: B,
    raw_mode: bool,
    alternate_screen: bool,
    /// Observed enable/restore sequence. Ignored in production; unit tests
    /// read it to prove the state machine engages and restores in order,
    /// and restores partial state after an early failure.
    steps: Vec<TermStep>,
}

impl<B: TermBackend> TerminalGuard<B> {
    /// Engage the interactive terminal step by step. If any step fails after
    /// a prior state was already turned on, the partially-engaged states are
    /// restored *before* the error is returned — so an early `?` can never
    /// leave the terminal wedged.
    fn open(mut backend: B) -> anyhow::Result<Self> {
        install_terminal_panic_hook();
        let mut steps = Vec::new();

        backend
            .enable_raw_mode()
            .map_err(|e| anyhow::anyhow!("enable raw mode: {e}"))?;
        steps.push(TermStep::EnableRaw);
        set_active_terminal_raw(true);

        if let Err(error) = backend.enter_alternate_screen() {
            let _ = Self {
                backend,
                raw_mode: true,
                alternate_screen: false,
                steps,
            }
            .restore();
            return Err(anyhow::anyhow!("enter alternate screen: {error}"));
        }
        steps.push(TermStep::EnterAlt);
        set_active_terminal_alt(true);

        if let Err(error) = backend.clear() {
            let _ = Self {
                backend,
                raw_mode: true,
                alternate_screen: true,
                steps,
            }
            .restore();
            return Err(anyhow::anyhow!("clear terminal: {error}"));
        }
        steps.push(TermStep::Clear);

        Ok(Self {
            backend,
            raw_mode: true,
            alternate_screen: true,
            steps,
        })
    }

    /// Restore every state this process engaged, in reverse order, and report
    /// each failure instead of swallowing it.
    fn restore(&mut self) -> Vec<anyhow::Error> {
        let mut errors = Vec::new();
        if self.raw_mode || self.alternate_screen {
            if let Err(e) = self.backend.show_cursor() {
                errors.push(anyhow::anyhow!("show cursor: {e}"));
            }
            self.steps.push(TermStep::ShowCursor);
        }
        if self.alternate_screen {
            if let Err(e) = self.backend.leave_alternate_screen() {
                errors.push(anyhow::anyhow!("leave alternate screen: {e}"));
            }
            self.steps.push(TermStep::LeaveAlt);
            self.alternate_screen = false;
        }
        if self.raw_mode {
            if let Err(e) = self.backend.disable_raw_mode() {
                errors.push(anyhow::anyhow!("disable raw mode: {e}"));
            }
            self.steps.push(TermStep::DisableRaw);
            self.raw_mode = false;
        }
        set_active_terminal_raw(false);
        set_active_terminal_alt(false);
        errors
    }

    fn terminal_mut(&mut self) -> Option<&mut Terminal<CrosstermBackend<io::Stdout>>> {
        self.backend.terminal_mut()
    }

    #[cfg(test)]
    fn steps(&self) -> &[TermStep] {
        &self.steps
    }
}

impl<B: TermBackend> Drop for TerminalGuard<B> {
    fn drop(&mut self) {
        // On drop we still restore, but errors cannot escape a destructor;
        // `restore` already drives the real backend and records into `steps`.
        let _ = self.restore();
    }
}

/// The terminal state this process currently holds, shared with the panic
/// hook so a panic can restore the terminal even though the guard value is
/// not reachable from the hook.
struct ActiveTerminalState {
    raw: bool,
    alt: bool,
}

static ACTIVE_TERMINAL: Mutex<ActiveTerminalState> = Mutex::new(ActiveTerminalState {
    raw: false,
    alt: false,
});

fn set_active_terminal_raw(on: bool) {
    if let Ok(mut state) = ACTIVE_TERMINAL.lock() {
        state.raw = on;
    }
}

fn set_active_terminal_alt(on: bool) {
    if let Ok(mut state) = ACTIVE_TERMINAL.lock() {
        state.alt = on;
    }
}

/// Install a panic hook (once) that restores the terminal before chaining the
/// previous hook, so a panic never leaves the operator's prompt wedged in
/// raw/alternate state with a hidden cursor.
fn install_terminal_panic_hook() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(mut state) = ACTIVE_TERMINAL.lock()
                && (state.raw || state.alt)
            {
                let _ = execute!(io::stdout(), Show);
                if state.alt {
                    let _ = execute!(io::stdout(), LeaveAlternateScreen);
                }
                if state.raw {
                    let _ = terminal::disable_raw_mode();
                }
                state.raw = false;
                state.alt = false;
            }
            previous(info);
        }));
    });
}

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
    let (model, serving_banner, provider_profile_digest, cache_endpoint) =
        match try_model_from_env()? {
            agent_compose::ModelSelection::Mock(mock) => {
                eprintln!("demo mode: AGENT_DEMO=1 selected the explicit mock transport");
                (
                    mock,
                    "serving: demo mock transport (AGENT_DEMO=1)".to_string(),
                    None,
                    None,
                )
            }
            agent_compose::ModelSelection::Provider(provider, profile) => {
                eprintln!("{}", profile.banner());
                let digest = profile.digest();
                let banner = format!("serving: {} | profile digest {digest}", profile.banner());
                let endpoint = profile.base_url.clone();
                (provider, banner, Some(digest), Some(endpoint))
            }
        };
    let workspace = Workspace::open(&root).await?;
    // N04: the composition root's stable cache-routing namespace (host
    // workspace + configured endpoint). Demo/mock compositions stay
    // keyless.
    let cache_routing = cache_endpoint.map(|endpoint| agent_contracts::PromptCacheRouting {
        isolation: "tui".to_string(),
        workspace: workspace.root().display().to_string(),
        endpoint,
    });
    let maintenance_cache_key = cache_routing
        .as_ref()
        .map(|routing| routing.key_for("compaction", "maintenance"));
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
        maintenance_cache_key,
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
    } else if std::env::var("AGENT_AUTO_APPROVE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        // Local/dev convenience: skip interactive prompts and allow
        // workspace writes + process/shell effects. Not a product flag.
        eprintln!("agent-tui: AGENT_AUTO_APPROVE=1 — using permissive approval (no prompts)");
        let task_gate = Arc::new(
            TaskApprovalGate::new(Arc::new(PolicyApprovalGate::permissive()))
                .with_host_policies(host_policies.clone()),
        );
        for json in &grant_args {
            let grant: agent_contracts::StandingGrant = serde_json::from_str(json)
                .with_context(|| format!("invalid --grant JSON: {json}"))?;
            task_gate.grant(grant).await?;
        }
        (task_gate as Arc<dyn agent_contracts::ApprovalGate>, None)
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
        cache_routing,
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

    // U5: engage the interactive terminal behind a guard. Any `?` or early
    // return past this point restores raw/alternate/cursor — the guard owns
    // the terminal and restores on `Drop`; a panic restores via the hook.
    let mut term = TerminalGuard::open(
        RealTermBackend::new().map_err(|e| anyhow::anyhow!("create terminal: {e}"))?,
    )?;
    let result = run_session(
        &mut TerminalSource,
        &mut TerminalSink::new(
            term.terminal_mut()
                .expect("the real backend always owns a terminal"),
        ),
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

    // U5: two independent responsibilities, settled separately:
    //   1. release the interactive terminal state FIRST, so an operator
    //      regains a usable prompt even if runtime cleanup later stalls;
    //   2. then run the bounded, reportable Runtime shutdown and aggregate
    //      its error on its own.
    let term_errors = term.restore();
    let shutdown_result = composed.shutdown().await;

    match (result, shutdown_result, term_errors) {
        (Err(ui_error), _, _) => Err(ui_error),
        (_, Err(shutdown_error), _) => {
            Err(anyhow::Error::new(shutdown_error).context("runtime shutdown failed"))
        }
        (_, _, errors) if !errors.is_empty() => {
            for error in &errors {
                eprintln!("terminal restore: {error}");
            }
            Err(anyhow::anyhow!(
                "terminal restore failed after the session ended"
            ))
        }
        (Ok(()), Ok(()), _) => Ok(()),
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    /// Test-only backend: records every operation it is asked to perform and
    /// never touches a real terminal, so the guard's state machine is
    /// observable without a PTY. `fail_at` makes one step return an error to
    /// prove the guard restores partial state after an early failure.
    #[derive(Debug, Clone)]
    struct RecordingTermBackend {
        steps: std::sync::Arc<std::sync::Mutex<Vec<TermStep>>>,
        fail_at: Option<TermStep>,
    }

    impl RecordingTermBackend {
        fn new() -> Self {
            Self {
                steps: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                fail_at: None,
            }
        }
        fn with_failure(fail_at: TermStep) -> Self {
            Self {
                steps: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                fail_at: Some(fail_at),
            }
        }
    }

    impl TermBackend for RecordingTermBackend {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            self.steps.lock().unwrap().push(TermStep::EnableRaw);
            if self.fail_at == Some(TermStep::EnableRaw) {
                Err(io::Error::other("injected failure"))
            } else {
                Ok(())
            }
        }
        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.steps.lock().unwrap().push(TermStep::EnterAlt);
            if self.fail_at == Some(TermStep::EnterAlt) {
                Err(io::Error::other("injected failure"))
            } else {
                Ok(())
            }
        }
        fn clear(&mut self) -> io::Result<()> {
            self.steps.lock().unwrap().push(TermStep::Clear);
            if self.fail_at == Some(TermStep::Clear) {
                Err(io::Error::other("injected failure"))
            } else {
                Ok(())
            }
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.steps.lock().unwrap().push(TermStep::ShowCursor);
            Ok(())
        }
        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.steps.lock().unwrap().push(TermStep::LeaveAlt);
            Ok(())
        }
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.steps.lock().unwrap().push(TermStep::DisableRaw);
            Ok(())
        }
        fn terminal_mut(&mut self) -> Option<&mut Terminal<CrosstermBackend<io::Stdout>>> {
            None
        }
    }

    /// The guard must engage (EnableRaw, EnterAlt, Clear) and, on drop,
    /// restore in reverse order (ShowCursor, LeaveAlt, DisableRaw) — proving
    /// the state machine is correct and observable without a PTY.
    #[test]
    fn guard_engages_then_restores_in_reverse_order() {
        let backend = RecordingTermBackend::new();
        let guard = TerminalGuard::open(backend.clone()).unwrap();
        assert_eq!(
            guard.steps(),
            &[TermStep::EnableRaw, TermStep::EnterAlt, TermStep::Clear]
        );
        // Restoring happens on `Drop`.
        drop(guard);
        assert_eq!(
            backend.steps.lock().unwrap().as_slice(),
            &[
                TermStep::EnableRaw,
                TermStep::EnterAlt,
                TermStep::Clear,
                TermStep::ShowCursor,
                TermStep::LeaveAlt,
                TermStep::DisableRaw,
            ]
        );
    }

    /// The core U5 regression: a `?` early return after `enable_raw_mode`
    /// used to bypass the tail restore and leave the terminal wedged. The
    /// guard must restore the states it had already engaged (here: raw mode)
    /// when a later step fails.
    #[test]
    fn guard_restores_partial_state_after_early_failure() {
        let backend = RecordingTermBackend::with_failure(TermStep::EnterAlt);
        let error = TerminalGuard::open(backend.clone()).unwrap_err();
        let recorded = backend.steps.lock().unwrap();
        assert!(
            recorded.contains(&TermStep::EnableRaw),
            "raw mode was engaged before the failure"
        );
        assert!(
            recorded.contains(&TermStep::DisableRaw),
            "raw mode must be restored after the early failure: {recorded:?}"
        );
        assert!(
            !recorded.contains(&TermStep::LeaveAlt),
            "alternate screen was never engaged, so it must not be left: {recorded:?}"
        );
        assert!(
            error.to_string().contains("alternate screen"),
            "the error must name the failing step: {error}"
        );
    }
}
