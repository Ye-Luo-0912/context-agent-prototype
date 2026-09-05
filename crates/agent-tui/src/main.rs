mod doctor;
mod state;
mod ui;

use std::{io, path::PathBuf, sync::Arc, time::Duration};

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, build_context_engine, compose,
    try_model_from_env,
};
use agent_contracts::{ApprovalDecision, StandingGrant};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate, TaskApprovalGate};
use agent_runtime::{
    CheckpointStore, RuntimeCheckpoint, RuntimeHandle, RuntimeInstance, decode_checkpoint_bytes,
    decode_checkpoint_file,
};
use agent_storage::FileEventJournal;
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use anyhow::Context;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use state::AppState;
use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

/// Cap on the note channel that carries off-input-thread command output
/// (`/tasks`, `/grants`) back into the alternate-screen frame. The channel
/// is bounded so a pathological catalog listing cannot grow the UI's
/// pending-notice queue without limit; full notifications are dropped whole
/// rather than blocking the command task.
const NOTICE_CHANNEL_CAP: usize = 64;

/// UI-side handles for interactive approval: the broker carries requests from
/// the kernel to the UI, the gate carries the user's decision back, and the
/// task gate holds the standing grants (established from `--grant` on the
/// command line, revocable from the UI).
struct InteractiveHandle {
    broker: Arc<ApprovalBroker>,
    gate: Arc<InteractiveApprovalGate>,
    task_grants: Arc<TaskApprovalGate>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut read_only = false;
    let mut context_policy = "dynamic".to_string();
    let mut root_arg: Option<PathBuf> = None;
    let mut grant_args: Vec<String> = Vec::new();
    let mut effect_reservation_journal: Option<PathBuf> = None;
    let mut restore_arg: Option<PathBuf> = None;
    let mut doctor_mode = false;
    let mut max_rounds: Option<usize> = None;
    let mut defer_proof = false;
    for arg in std::env::args().skip(1) {
        if arg == "--doctor" {
            doctor_mode = true;
        } else if arg == "--read-only" {
            read_only = true;
        } else if let Some(value) = arg.strip_prefix("--context=") {
            context_policy = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--grant=") {
            grant_args.push(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--effect-reservation-journal=") {
            effect_reservation_journal = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--restore=") {
            restore_arg = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--max-rounds=") {
            max_rounds = Some(parse_max_rounds(value)?);
        } else if arg == "--defer-proof" {
            defer_proof = true;
        } else if root_arg.is_none() {
            root_arg = Some(PathBuf::from(arg));
        }
    }
    if read_only && restore_arg.is_some() {
        anyhow::bail!("--restore cannot be combined with --read-only");
    }
    // An invalid checkpoint must fail before the workspace, host or any
    // state is touched: read, parse and validate an explicit path up
    // front. `--restore=latest` resolves after the workspace opens, then
    // validates the same way before anything starts.
    let restore_latest = matches!(&restore_arg, Some(path) if path.as_os_str() == "latest");
    let restore_checkpoint = match (&restore_arg, restore_latest) {
        (Some(path), false) => Some(load_runtime_checkpoint(path)?),
        _ => None,
    };
    let policy = ContextPolicy::from_str_checked(&context_policy)?;
    let root = root_arg.unwrap_or(std::env::current_dir().context("current directory")?);
    if doctor_mode {
        let code = doctor::run_doctor(root).await;
        std::process::exit(code);
    }
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
    let context_engine =
        build_context_engine(policy, workspace.state_dir(), Some(model.clone())).await?;
    if read_only && !grant_args.is_empty() {
        anyhow::bail!("--grant cannot be combined with --read-only");
    }
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
    } else {
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        let task_gate =
            Arc::new(TaskApprovalGate::new(gate.clone()).with_host_policies(host_policies.clone()));
        for json in &grant_args {
            let grant: StandingGrant = serde_json::from_str(json)
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
    let base_tools = Arc::new(BuiltinToolDispatcher::with_config_and_verification_recipes(
        workspace.clone(),
        Default::default(),
        (*verification_recipes).clone(),
    ));
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

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    let result = run_ui(
        &mut terminal,
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

#[allow(clippy::too_many_arguments)]
async fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    handle: RuntimeHandle,
    runtime: &RuntimeInstance,
    runtime_events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    interactive: Option<InteractiveHandle>,
    context_policy: &str,
    checkpoint_dir: PathBuf,
    serving_banner: String,
    max_rounds: Option<usize>,
    defer_proof: bool,
) -> anyhow::Result<()> {
    let mut app = AppState::new(handle.run_id());
    app.push_system(format!("context policy: {context_policy}"));
    app.push_system(serving_banner);
    if let Some(rounds) = max_rounds {
        app.execution_budget = Some(rounds);
        app.push_system(format!(
            "execution budget: {rounds} model rounds per turn (--max-rounds)"
        ));
    }
    if defer_proof {
        app.push_system(
            "deferred proof refresh: enabled (--defer-proof); default stays inline".into(),
        );
    }

    // Requests that arrived before this loop started (e.g. during startup).
    if let Some(handle) = &interactive {
        for request in handle.broker.pending().await {
            app.begin_approval(request);
        }
    }
    let mut approval_rx = interactive.as_ref().map(|handle| handle.broker.subscribe());

    // Command output that resolves off the input thread (/tasks, /grants)
    // comes back through this channel: printing to stdout directly would
    // corrupt the alternate-screen frame.
    let (notice_tx, mut notice_rx) = tokio::sync::mpsc::channel::<String>(NOTICE_CHANNEL_CAP);

    loop {
        let traces_dir = checkpoint_dir
            .parent()
            .map(|state_dir| state_dir.join("traces"))
            .unwrap_or_else(|| checkpoint_dir.clone());
        loop {
            match runtime_events.try_recv() {
                Ok(event) => app.apply_runtime_event(event),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    // A Lagged receiver dropped events it never saw. Hide
                    // nothing: name the loss, rebuild the projection from
                    // the durable journal, and continue.
                    let folded = app.resync_projection(&traces_dir).await;
                    app.push_system(format!(
                        "warning: the UI fell behind and dropped {skipped} runtime events; the status projection was resynced from the journal ({folded} events folded)"
                    ));
                }
                Err(_) => break,
            }
        }
        if let Some(rx) = &mut approval_rx {
            while let Ok(request) = rx.try_recv() {
                app.begin_approval(request);
            }
        }
        while let Ok(line) = notice_rx.try_recv() {
            app.push_system(line);
        }

        terminal.draw(|frame| ui::render(frame, &app))?;

        if event::poll(Duration::from_millis(30))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }

            // While a write/process tool waits for permission, y/n (or
            // Enter/Esc) resolve the prompt; other keys are ignored.
            if app.pending_approval.is_some() {
                let Some(handle) = &interactive else {
                    app.clear_approval();
                    continue;
                };
                let request_id = app
                    .pending_approval
                    .as_ref()
                    .map(|p| p.request_id.clone())
                    .unwrap_or_default();
                let decision = match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => Some(ApprovalDecision::Allow),
                    KeyCode::Char('n') | KeyCode::Esc => Some(ApprovalDecision::Deny),
                    _ => None,
                };
                if let Some(decision) = decision {
                    let granted = handle.gate.respond(&request_id, decision).await;
                    app.clear_approval();
                    app.push_system(match (decision, granted) {
                        (ApprovalDecision::Allow, true) => "approval granted".into(),
                        (ApprovalDecision::Deny, true) => "approval denied".into(),
                        (_, false) => "approval request already resolved".into(),
                    });
                }
                continue;
            }

            match key.code {
                KeyCode::Char(ch) => app.input.push(ch),
                KeyCode::Backspace => {
                    app.input.pop();
                }
                KeyCode::Enter => {
                    let input = std::mem::take(&mut app.input);
                    let trimmed = input.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if trimmed == "/quit" {
                        break;
                    }
                    if let Some(goal) = trimmed.strip_prefix("/focus ") {
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        let goal = goal.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = handle.set_focus(goal).await {
                                let _ = notice_tx.try_send(format!("focus failed: {error}"));
                            }
                        });
                        continue;
                    }
                    if let Some(id_text) = trimmed.strip_prefix("/task ") {
                        // Activate an existing task by id (resume its
                        // scopes). Task ids come from `/tasks`; activation
                        // alone does not start a turn — `/continue` does.
                        match id_text.trim().parse::<agent_contracts::TaskId>() {
                            Ok(task_id) => {
                                let handle = handle.clone();
                                let notice_tx = notice_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(error) = handle.activate_task(task_id).await {
                                        let _ = notice_tx.try_send(format!("task failed: {error}"));
                                    }
                                });
                            }
                            Err(error) => {
                                app.push_system(format!("invalid task id: {error}"));
                            }
                        }
                        continue;
                    }
                    if trimmed == "/tasks" {
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        tokio::spawn(async move {
                            match handle.list_tasks().await {
                                Ok(tasks) => {
                                    for task in tasks {
                                        let _ = notice_tx.try_send(format!(
                                            "task {} [{:?}] tools=r{}/{} {}",
                                            task.id,
                                            task.status,
                                            task.tool_requirement_revision,
                                            task.tool_requirement_count,
                                            task.goal
                                        ));
                                    }
                                }
                                Err(error) => {
                                    let _ = notice_tx.try_send(format!("tasks failed: {error}"));
                                }
                            }
                        });
                        continue;
                    }
                    if let Some(goal_text) = trimmed.strip_prefix("/work ") {
                        let goal = goal_text.trim().to_string();
                        if goal.is_empty() {
                            app.push_system("usage: /work <goal>".to_string());
                            continue;
                        }
                        // Explicit long-task entry, composed from the
                        // existing actor paths: set_focus creates (or
                        // resumes) the task while idle, an empty tool
                        // requirement set gains a PreferSurface demand for
                        // task.manage, and the goal is delivered once
                        // through the normal user-message path (busy →
                        // the runtime's own queue). No second orchestrator.
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.set_focus(goal.clone()).await {
                                let _ = notice_tx.try_send(format!("work failed: {error}"));
                                return;
                            }
                            let tasks = match handle.list_tasks().await {
                                Ok(tasks) => tasks,
                                Err(error) => {
                                    let _ = notice_tx.try_send(format!("work failed: {error}"));
                                    return;
                                }
                            };
                            // Match by goal: set_focus just created or
                            // resumed exactly this task; a blind first-Active
                            // pick could hit an older open task.
                            let task = tasks
                                .iter()
                                .find(|task| {
                                    task.goal == goal
                                        && matches!(task.status, agent_runtime::TaskStatus::Active)
                                })
                                .or_else(|| {
                                    tasks.iter().find(|task| {
                                        matches!(task.status, agent_runtime::TaskStatus::Active)
                                    })
                                });
                            if let Some(task) = task
                                && task.tool_requirement_count == 0
                            {
                                // Fill only an empty requirement set: a
                                // blind whole-set replace would drop
                                // someone else's entries. task.manage stays
                                // capability.manage-loadable either way.
                                if let Err(error) = handle
                                    .replace_task_tool_requirements(
                                        task.id,
                                        task.tool_requirement_revision,
                                        vec![agent_contracts::ToolSurfaceRequirement {
                                            tool_name: "task.manage".into(),
                                            demand:
                                                agent_contracts::ToolSurfaceDemand::PreferSurface,
                                            reason: "long-task checklist".into(),
                                        }],
                                    )
                                    .await
                                {
                                    let _ = notice_tx.try_send(format!(
                                        "work: task.manage not attached: {error}"
                                    ));
                                }
                            }
                            if let Err(error) = handle.user_message(goal).await {
                                let _ = notice_tx.try_send(format!("work failed: {error}"));
                            }
                        });
                        continue;
                    }
                    if trimmed == "/plan" {
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        tokio::spawn(async move {
                            match handle.task_plan_view().await {
                                Ok(Some(view)) => {
                                    for line in format_plan_lines(&view) {
                                        let _ = notice_tx.try_send(line);
                                    }
                                }
                                Ok(None) => {
                                    let _ = notice_tx
                                        .try_send("no active task; /work <goal> starts one".into());
                                }
                                Err(error) => {
                                    let _ = notice_tx.try_send(format!("plan failed: {error}"));
                                }
                            }
                        });
                        continue;
                    }
                    if trimmed == "/grants" {
                        let Some(handle) = &interactive else {
                            continue;
                        };
                        let task_grants = handle.task_grants.clone();
                        let notice_tx = notice_tx.clone();
                        tokio::spawn(async move {
                            let grants = task_grants.active_grants().await;
                            if grants.is_empty() {
                                let _ = notice_tx.try_send("no active standing grants".into());
                            }
                            for grant in grants {
                                let _ = notice_tx.try_send(format!(
                                    "grant {} risk={:?} workspace={:?} argv={:?} shell={:?} \
                                     max_runs={:?} max_bytes={:?} expires_at_ms={}",
                                    grant.id,
                                    grant.risk,
                                    grant.target.workspace_path_prefix,
                                    grant.target.exec_argv_prefix,
                                    grant.target.shell_command_digest,
                                    grant.constraint.max_runs,
                                    grant.constraint.max_content_bytes,
                                    grant.expires_at_ms,
                                ));
                            }
                        });
                        continue;
                    }
                    if let Some(id) = trimmed.strip_prefix("/revoke ") {
                        let Some(handle) = &interactive else {
                            app.push_system(
                                "grant revoke needs an interactive (non read-only) session"
                                    .to_string(),
                            );
                            continue;
                        };
                        let id = id.trim();
                        if id.is_empty() {
                            app.push_system("usage: /revoke <grant-id>".to_string());
                            continue;
                        }
                        let task_grants = handle.task_grants.clone();
                        let notice_tx = notice_tx.clone();
                        let id = id.to_string();
                        tokio::spawn(async move {
                            if task_grants.revoke(&id).await {
                                let _ = notice_tx.try_send(format!("grant {id} revoked"));
                            } else {
                                let _ = notice_tx.try_send(format!("no live grant with id {id}"));
                            }
                        });
                        continue;
                    }
                    if trimmed == "/suspend" {
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.suspend_task().await {
                                let _ = notice_tx.try_send(format!("suspend failed: {error}"));
                            }
                        });
                        continue;
                    }
                    if let Some(content) = trimmed.strip_prefix("/pin ") {
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        let content = content.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = handle.pin(content).await {
                                let _ = notice_tx.try_send(format!("pin failed: {error}"));
                            }
                        });
                        continue;
                    }
                    if let Some(summary) = trimmed.strip_prefix("/done ") {
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        let summary = summary.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = handle.complete_current_task(summary).await {
                                let _ = notice_tx.try_send(format!("done failed: {error}"));
                            }
                        });
                        continue;
                    }
                    if trimmed == "/context" {
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.emit_diagnostics().await {
                                let _ = notice_tx
                                    .try_send(format!("context diagnostics failed: {error}"));
                            }
                        });
                        continue;
                    }
                    if trimmed == "/status" {
                        for line in app.render_status() {
                            app.push_system(line);
                        }
                        continue;
                    }
                    if trimmed == "/diag-export" {
                        let state_dir = checkpoint_dir
                            .parent()
                            .map(std::path::Path::to_path_buf)
                            .unwrap_or_else(|| checkpoint_dir.clone());
                        let projection_lines = app.status_projection.lines();
                        let tail: Vec<String> = app
                            .messages
                            .iter()
                            .rev()
                            .take(40)
                            .map(|message| message.content.clone())
                            .collect();
                        match doctor::export_diagnostics(&state_dir, projection_lines, tail).await {
                            Ok(path) => {
                                app.push_system(format!("diagnostics written: {}", path.display()))
                            }
                            Err(error) => {
                                app.push_system(format!("diagnostics export failed: {error}"))
                            }
                        }
                        continue;
                    }
                    if trimmed == "/checkpoints" {
                        // Bounded newest-first listing straight from the
                        // runtime checkpoint store, so save/list/resume is
                        // discoverable from the product host without
                        // filesystem spelunking.
                        let store = CheckpointStore::new(checkpoint_dir.clone());
                        match store.list(CHECKPOINT_LIST_LIMIT).await {
                            Ok(rows) if rows.is_empty() => {
                                app.push_system(
                                    "no checkpoints saved yet; /checkpoint writes one".to_string(),
                                );
                            }
                            Ok(rows) => {
                                for row in &rows {
                                    let age = row
                                        .modified
                                        .elapsed()
                                        .map(|elapsed| format!("{elapsed:?} ago"))
                                        .unwrap_or_else(|_| "unknown age".into());
                                    app.push_system(format!(
                                        "{} ({} bytes, {})",
                                        row.artifact, row.payload_bytes, age
                                    ));
                                }
                            }
                            Err(error) => {
                                app.push_system(format!("checkpoint list failed: {error}"))
                            }
                        }
                        continue;
                    }
                    if trimmed == "/checkpoint" {
                        // The manual save rides the same atomic envelope
                        // store as the automatic safe points: one format,
                        // one retention domain, checksum verified on load.
                        let store = CheckpointStore::new(checkpoint_dir.clone());
                        match runtime.checkpoint().await {
                            Ok(checkpoint) => {
                                let tasks = checkpoint.tasks.tasks.len();
                                let bytes = match serde_json::to_vec(&checkpoint) {
                                    Ok(bytes) => bytes,
                                    Err(error) => {
                                        app.push_system(format!(
                                            "checkpoint serialize failed: {error}"
                                        ));
                                        continue;
                                    }
                                };
                                match store.write_atomic(&bytes).await {
                                    Ok(stored) => {
                                        app.last_checkpoint = Some(stored.artifact.clone());
                                        app.push_system(format!(
                                            "checkpoint saved ({tasks} tasks): {}",
                                            stored.artifact
                                        ));
                                    }
                                    Err(error) => {
                                        app.push_system(format!("checkpoint write failed: {error}"))
                                    }
                                }
                            }
                            Err(error) => app.push_system(format!("checkpoint failed: {error}")),
                        }
                        continue;
                    }
                    if let Some(restore_target) = trimmed.strip_prefix("/restore ") {
                        let result = async {
                            let path =
                                resolve_restore_target(&checkpoint_dir, restore_target.trim());
                            let bytes = tokio::fs::read(&path).await.map_err(|error| {
                                anyhow::anyhow!("read {}: {error}", path.display())
                            })?;
                            let checkpoint = decode_checkpoint_bytes(&bytes)
                                .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
                            runtime
                                .restore(checkpoint)
                                .await
                                .map_err(anyhow::Error::from)
                        }
                        .await;
                        match result {
                            Ok(()) => app.push_system(
                                "runtime restored; /continue resumes the active task".to_string(),
                            ),
                            Err(error) => app.push_system(format!("restore failed: {error}")),
                        }
                        continue;
                    }
                    if trimmed == "/cancel" {
                        if let Err(error) = handle.cancel_turn().await {
                            app.push_system(format!("cancel failed: {error}"));
                        }
                        continue;
                    }
                    if trimmed == "/continue" {
                        // Re-run the active task's stored directive in a
                        // fresh turn: no new instruction identity is minted
                        // and the stored directive is not re-ingested.
                        // Refusals (no active task, busy runtime, recovery
                        // required) surface here; the started turn itself
                        // is event-driven (`TaskContinuationStarted`).
                        let handle = handle.clone();
                        let notice_tx = notice_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.continue_active_task().await {
                                let _ = notice_tx.try_send(format!("continue failed: {error}"));
                            }
                        });
                        continue;
                    }

                    if trimmed == "/help" {
                        for line in HELP_LINES {
                            app.push_system((*line).to_string());
                        }
                        continue;
                    }
                    if trimmed.starts_with('/') {
                        app.push_system(format!(
                            "unknown command {trimmed}; /help lists the product commands,                              and non-command input needs no leading slash"
                        ));
                        continue;
                    }
                    // Normal input always goes to the runtime: an idle
                    // runtime starts a turn, a busy one queues it in the
                    // existing single dialogue slot. The command reply plus
                    // the `UserInput` lifecycle events own the visible
                    // disposition (queued / applied / rejected) — the UI no
                    // longer drops input on its own busy guess.
                    let handle = handle.clone();
                    let notice_tx = notice_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle.user_message(input).await {
                            let _ = notice_tx.try_send(format!("input not accepted: {error}"));
                        }
                    });
                }
                KeyCode::PageUp => app.scroll = app.scroll.saturating_add(8),
                KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(8),
                KeyCode::Tab => app.toggle_context_panel(),
                KeyCode::Esc => app.input.clear(),
                _ => {}
            }
        }
    }

    Ok(())
}

/// Read, parse and validate a runtime checkpoint file in any product
/// format (envelope artifact or legacy raw JSON). Every failure mode is a
/// visible configuration-style error; nothing has been started or mutated
/// when this runs.
fn load_runtime_checkpoint(path: &std::path::Path) -> anyhow::Result<RuntimeCheckpoint> {
    decode_checkpoint_file(path).map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "invalid checkpoint {}: the runtime refuses it before any mutation",
            path.display()
        ))
    })
}

/// The product command list `/help` renders. Kept in one place so the
/// welcome hint and the help output cannot drift apart.
const HELP_LINES: [&str; 18] = [
    "/focus <directive> - point the runtime at a task directive",
    "/work <goal> - start (or resume) a long task with task.manage available",
    "/plan - show the active task's checklist, next action and open loops",
    "/pin <note> - pin a durable note into the working set",
    "/done <summary> - close the current task with a summary",
    "/context - inspect the selected working context",
    "/status - run, task anchor, recovery debts, last checkpoint",
    "/tasks - list the run's tasks",
    "/task <id> - activate one task (no turn starts)",
    "/continue - continue the active task's stored directive",
    "/checkpoint - save a runtime checkpoint now",
    "/checkpoints - list saved checkpoints (newest first)",
    "/restore <path> - restore one checkpoint in this session",
    "/grants - list standing effect grants",
    "/revoke <grant-id> - revoke one standing grant",
    "/suspend - suspend the active task at a safe point",
    "/cancel - cancel the in-flight turn",
    "/quit - leave the TUI (Esc clears the input, Ctrl-C quits)",
];

/// Bounded number of checklist rows `/plan` renders per view.
const PLAN_ROW_LIMIT: usize = 12;
/// Char cap for one rendered plan row or loop entry.
const PLAN_LINE_CHARS: usize = 160;

fn bounded_plan_line(text: &str) -> String {
    let mut bounded: String = text.chars().take(PLAN_LINE_CHARS).collect();
    if text.chars().count() > PLAN_LINE_CHARS {
        bounded.push('…');
    }
    bounded
}

/// Bounded display of the active task's plan view. `[x]`/`[-]`/`[ ]` are a
/// display convention the model writes through `task.manage`; rows without
/// a prefix pass through as-is. `[x]` is the model's reported progress,
/// never a verification PASS — task completion stays with the existing
/// completion gate.
fn format_plan_lines(view: &agent_contracts::TaskAnchorView) -> Vec<String> {
    let mut lines = vec![format!(
        "plan: {} (anchor r{})",
        bounded_plan_line(&view.original_goal),
        view.revision
    )];
    if view.plan_progress.is_empty() {
        lines.push("  no checklist yet; the model maintains one via task.manage".into());
    } else {
        for row in view.plan_progress.iter().take(PLAN_ROW_LIMIT) {
            lines.push(format!("  {}", bounded_plan_line(row)));
        }
        let overflow = view.plan_progress.len().saturating_sub(PLAN_ROW_LIMIT);
        if overflow > 0 {
            lines.push(format!("  …and {overflow} more rows"));
        }
        lines.push("  ([x] is reported progress, not verification PASS)".into());
    }
    if !view.next_action.is_empty() {
        lines.push(format!("  next: {}", bounded_plan_line(&view.next_action)));
    }
    if !view.open_loops.is_empty() {
        let shown = view.open_loops.len().min(8);
        let loops: Vec<String> = view.open_loops[..shown]
            .iter()
            .map(|loop_text| bounded_plan_line(loop_text))
            .collect();
        let overflow = view.open_loops.len() - shown;
        let more = if overflow > 0 {
            format!(" …and {overflow} more")
        } else {
            String::new()
        };
        lines.push(format!("  open loops: {}{more}", loops.join("; ")));
    }
    lines
}

/// Strict `--max-rounds` parsing. The budget counts MODEL rounds — the
/// same unit the runtime enforces (`Failure { RoundBudget }`) and the
/// status banner renders — never tool calls. Zero or garbage is a
/// startup error before any workspace mutation; there is no infinite
/// value: a long task gets an explicitly larger finite budget.
fn parse_max_rounds(value: &str) -> anyhow::Result<usize> {
    let rounds: usize = value.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid --max-rounds {value:?}: expected a positive integer (model rounds)"
        )
    })?;
    if rounds == 0 {
        anyhow::bail!("invalid --max-rounds 0: the budget must be at least 1 model round");
    }
    Ok(rounds)
}

/// Resume discovery: the newest saved checkpoint in the store. A missing
/// or empty store is a configuration error with the fix in the message.
fn resolve_latest_checkpoint(dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let replaces = match &newest {
            None => true,
            Some((newest_at, _)) => modified > *newest_at,
        };
        if replaces {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path).ok_or_else(|| {
        anyhow::anyhow!(
            "no checkpoint found in {}: /checkpoint saves one, or pass --restore=<file>              for an explicit path",
            dir.display()
        )
    })
}

/// Bounded number of checkpoint rows the `/checkpoints` listing renders.
const CHECKPOINT_LIST_LIMIT: usize = 20;

/// A bare store artifact name (`checkpoint-*.json`, no path separators)
/// resolves inside the checkpoint directory; anything else is an explicit
/// filesystem path the user typed.
fn resolve_restore_target(checkpoint_dir: &std::path::Path, target: &str) -> std::path::PathBuf {
    let is_plain_artifact_name = !target.contains('/')
        && !target.contains('\\')
        && target.starts_with("checkpoint-")
        && target.ends_with(".json");
    if is_plain_artifact_name {
        checkpoint_dir.join(target)
    } else {
        std::path::PathBuf::from(target)
    }
}

#[cfg(test)]
mod plan_format_tests {
    use super::*;

    fn view() -> agent_contracts::TaskAnchorView {
        agent_contracts::TaskAnchorView {
            revision: 7,
            original_goal: "migrate the config module".into(),
            current_interpretation: String::new(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            plan_progress: vec![
                "[x] locate the config reader".into(),
                "[-] add the target key".into(),
                "[ ] run related checks".into(),
                "unprefixed historical row".into(),
            ],
            open_loops: vec!["confirm old default behavior".into()],
            next_action: "edit the parser and add a regression".into(),
        }
    }

    #[test]
    fn plan_lines_render_rows_next_and_loops_with_the_progress_disclaimer() {
        let lines = format_plan_lines(&view());
        let joined = lines.join("\n");
        assert!(lines[0].contains("migrate the config module"), "{joined}");
        assert!(lines[0].contains("r7"), "{joined}");
        assert!(joined.contains("[x] locate the config reader"));
        // Unprefixed rows pass through as-is, without a fabricated prefix.
        assert!(joined.contains("unprefixed historical row"));
        assert!(joined.contains("next: edit the parser"));
        assert!(joined.contains("open loops: confirm old default behavior"));
        // The checklist is progress reporting, not verification truth.
        assert!(joined.contains("not verification PASS"));
    }

    #[test]
    fn plan_lines_stay_bounded_and_name_the_empty_case() {
        let mut big = view();
        big.plan_progress = (0..30).map(|i| format!("row {i}")).collect();
        big.open_loops = (0..20).map(|i| format!("loop {i}")).collect();
        let lines = format_plan_lines(&big);
        assert!(lines.iter().any(|line| line.contains("…and 18 more rows")));
        assert!(lines.iter().any(|line| line.contains("…and 12 more")));
        assert!(lines.len() < 30, "render stays bounded: {:?}", lines.len());

        let mut empty = view();
        empty.plan_progress.clear();
        empty.next_action.clear();
        empty.open_loops.clear();
        let lines = format_plan_lines(&empty);
        assert!(lines.iter().any(|line| line.contains("no checklist yet")));
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("not verification PASS"))
        );

        let mut long = view();
        long.plan_progress = vec!["x".repeat(500)];
        let lines = format_plan_lines(&long);
        // Two-space indent + the bounded row + its ellipsis.
        assert!(lines[1].chars().count() <= PLAN_LINE_CHARS + 3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_rounds_parses_strictly_in_model_rounds() {
        assert_eq!(parse_max_rounds("24").unwrap(), 24);
        assert_eq!(parse_max_rounds(" 64 ").unwrap(), 64);
        for bad in ["0", "-4", "abc", "", "2.5", "99999999999999999999"] {
            let error = parse_max_rounds(bad).unwrap_err().to_string();
            assert!(error.contains("--max-rounds"), "{bad}: {error}");
        }
        let zero = parse_max_rounds("0").unwrap_err().to_string();
        assert!(zero.contains("at least 1"), "{zero}");
    }

    #[test]
    fn plain_artifact_names_resolve_inside_the_checkpoint_dir() {
        let dir = std::path::Path::new("C:\\state\\checkpoints");
        assert_eq!(
            resolve_restore_target(dir, "checkpoint-123-abc.json"),
            dir.join("checkpoint-123-abc.json")
        );
        // Paths — including relative ones — stay as typed.
        assert_eq!(
            resolve_restore_target(dir, "exports/cp.json"),
            std::path::PathBuf::from("exports/cp.json")
        );
        assert_eq!(
            resolve_restore_target(dir, "cp.json"),
            std::path::PathBuf::from("cp.json")
        );
    }

    #[test]
    fn resume_discovery_picks_the_newest_json_checkpoint() {
        use std::fs::FileTimes;

        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("older.json");
        let newer = dir.path().join("newer.json");
        std::fs::write(&older, b"{}").unwrap();
        std::fs::write(&newer, b"{}").unwrap();
        let set_modified = |path: &std::path::Path, at: std::time::SystemTime| {
            let file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            file.set_times(FileTimes::new().set_modified(at)).unwrap();
        };
        set_modified(&older, std::time::SystemTime::UNIX_EPOCH);
        set_modified(&newer, std::time::SystemTime::now());

        let resolved = resolve_latest_checkpoint(dir.path()).unwrap();
        assert_eq!(resolved, newer);

        let empty = tempfile::tempdir().unwrap();
        let error = resolve_latest_checkpoint(empty.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("no checkpoint found"), "{error}");
    }

    #[test]
    fn load_runtime_checkpoint_reports_readable_failures() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let error = load_runtime_checkpoint(&missing).unwrap_err();
        assert!(format!("{error:#}").contains("unreadable"), "{error:#}");

        let garbage = dir.path().join("garbage.json");
        std::fs::write(&garbage, b"not json").unwrap();
        let error = load_runtime_checkpoint(&garbage).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a runtime checkpoint"),
            "{error:#}"
        );
    }
}
