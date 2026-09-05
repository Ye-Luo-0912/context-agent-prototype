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
    let (model, provider_profile_digest) = match try_model_from_env()? {
        agent_compose::ModelSelection::Mock(mock) => {
            eprintln!("demo mode: AGENT_DEMO=1 selected the explicit mock transport");
            (mock, None)
        }
        agent_compose::ModelSelection::Provider(provider, profile) => {
            eprintln!("{}", profile.banner());
            (provider, Some(profile.digest()))
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
        defer_proof_refresh: false,
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
        max_tool_rounds: None,
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

async fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    handle: RuntimeHandle,
    runtime: &RuntimeInstance,
    runtime_events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    interactive: Option<InteractiveHandle>,
    context_policy: &str,
    checkpoint_dir: PathBuf,
) -> anyhow::Result<()> {
    let mut app = AppState::new(handle.run_id());
    app.push_system(format!("context policy: {context_policy}"));

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
                        let goal = goal.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = handle.set_focus(goal).await {
                                tracing::error!(%error, "set focus failed");
                            }
                        });
                        continue;
                    }
                    if let Some(id_text) = trimmed.strip_prefix("/task ") {
                        // Activate an existing task by id (resume its
                        // scopes). Task ids come from `/tasks`.
                        match id_text.trim().parse::<agent_contracts::TaskId>() {
                            Ok(task_id) => {
                                let handle = handle.clone();
                                tokio::spawn(async move {
                                    if let Err(error) = handle.activate_task(task_id).await {
                                        tracing::error!(%error, "activate task failed");
                                    }
                                });
                            }
                            Err(error) => {
                                tracing::error!(%error, "invalid task id: {id_text}");
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
                                Err(error) => tracing::error!(%error, "list tasks failed"),
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
                        tokio::spawn(async move {
                            if let Err(error) = handle.suspend_task().await {
                                tracing::error!(%error, "suspend task failed");
                            }
                        });
                        continue;
                    }
                    if let Some(content) = trimmed.strip_prefix("/pin ") {
                        let handle = handle.clone();
                        let content = content.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = handle.pin(content).await {
                                tracing::error!(%error, "pin failed");
                            }
                        });
                        continue;
                    }
                    if let Some(summary) = trimmed.strip_prefix("/done ") {
                        let handle = handle.clone();
                        let summary = summary.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = handle.complete_current_task(summary).await {
                                tracing::error!(%error, "complete task failed");
                            }
                        });
                        continue;
                    }
                    if trimmed == "/context" {
                        let handle = handle.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.emit_diagnostics().await {
                                tracing::error!(%error, "context diagnostics failed");
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
                            Ok(()) => app.push_system("runtime restored".to_string()),
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
                    if app.busy {
                        app.push_system(
                            "Agent is busy; wait for the current turn to finish.".into(),
                        );
                        continue;
                    }
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle.user_message(input).await {
                            tracing::error!(%error, "agent turn failed");
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
const HELP_LINES: [&str; 14] = [
    "/focus <directive> - point the runtime at a task directive",
    "/pin <note> - pin a durable note into the working set",
    "/done <summary> - close the current task with a summary",
    "/context - inspect the selected working context",
    "/status - run, task anchor, recovery debts, last checkpoint",
    "/tasks - list the run's tasks",
    "/checkpoint - save a runtime checkpoint now",
    "/checkpoints - list saved checkpoints (newest first)",
    "/restore <path> - restore one checkpoint in this session",
    "/grants - list standing effect grants",
    "/revoke <grant-id> - revoke one standing grant",
    "/suspend - suspend the active task at a safe point",
    "/cancel - cancel the in-flight turn",
    "/quit - leave the TUI",
];

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
mod tests {
    use super::*;

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
