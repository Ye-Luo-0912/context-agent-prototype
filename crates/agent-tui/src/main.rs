mod state;
mod ui;

use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, build_context_engine, compose,
    model_from_env,
};
use agent_contracts::{ApprovalDecision, StandingGrant};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate, TaskApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeInstance};
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
use tool_runtime::BuiltinToolDispatcher;

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
    for arg in std::env::args().skip(1) {
        if arg == "--read-only" {
            read_only = true;
        } else if let Some(value) = arg.strip_prefix("--context=") {
            context_policy = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--grant=") {
            grant_args.push(value.to_string());
        } else if root_arg.is_none() {
            root_arg = Some(PathBuf::from(arg));
        }
    }
    let policy = ContextPolicy::from_str_checked(&context_policy)?;
    let root = root_arg.unwrap_or(std::env::current_dir().context("current directory")?);
    let workspace = Workspace::open(&root).await?;
    let journal = Arc::new(FileEventJournal::open(workspace.state_dir().join("traces")).await?);

    // The context engine and the model are composition-root choices shared
    // with CLI/eval (agent-compose): the same kernel, tools and UI run
    // against any `ContextEngine` implementation (the A/B/C baselines, and
    // the process-boundary adapter). Rolling/dynamic 与 live eval 共用同一
    // 有界压缩器，避免 TUI 仍走占位折叠。
    let model = model_from_env();
    let context_engine =
        build_context_engine(policy, workspace.state_dir(), Some(model.clone())).await?;
    if read_only && !grant_args.is_empty() {
        anyhow::bail!("--grant cannot be combined with --read-only");
    }
    // 授权映射是组合根的决定：一份内置注册表同时交给审批门、能力
    // 分发器与内核租约路径。
    let host_policies = Arc::new(HostToolPolicyRegistry::with_builtins());
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
    let base_tools = Arc::new(BuiltinToolDispatcher::new(workspace.clone()));
    let artifact_store = Arc::new(workspace.clone());
    let output_broker = Arc::new(WorkspaceOutputBroker::new(workspace.clone().into()));
    let composed = compose(ComposeConfig {
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
        host_policies: Some(host_policies),
    })
    .await?;
    let mut runtime_events = composed.subscribe();
    composed.instance.start().await?;

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
    let (notice_tx, mut notice_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    loop {
        while let Ok(event) = runtime_events.try_recv() {
            app.apply_runtime_event(event);
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
                                        let _ = notice_tx.send(format!(
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
                                let _ = notice_tx.send("no active standing grants".into());
                            }
                            for grant in grants {
                                let _ = notice_tx.send(format!(
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
                    if trimmed == "/checkpoint" {
                        let path =
                            checkpoint_dir.join(format!("{}-{}.json", handle.run_id(), now_ms()));
                        match runtime.checkpoint().await {
                            Ok(checkpoint) => {
                                let bytes = match serde_json::to_vec_pretty(&checkpoint) {
                                    Ok(bytes) => bytes,
                                    Err(error) => {
                                        app.push_system(format!(
                                            "checkpoint serialize failed: {error}"
                                        ));
                                        continue;
                                    }
                                };
                                if let Err(error) = tokio::fs::create_dir_all(&checkpoint_dir).await
                                {
                                    app.push_system(format!("checkpoint dir failed: {error}"));
                                    continue;
                                }
                                match tokio::fs::write(&path, bytes).await {
                                    Ok(()) => app.push_system(format!(
                                        "checkpoint saved ({} tasks): {}",
                                        checkpoint.tasks.tasks.len(),
                                        path.display()
                                    )),
                                    Err(error) => {
                                        app.push_system(format!("checkpoint write failed: {error}"))
                                    }
                                }
                            }
                            Err(error) => app.push_system(format!("checkpoint failed: {error}")),
                        }
                        continue;
                    }
                    if let Some(restore_path) = trimmed.strip_prefix("/restore ") {
                        let path = std::path::PathBuf::from(restore_path.trim());
                        let result = async {
                            let bytes = tokio::fs::read(&path).await.map_err(|error| {
                                anyhow::anyhow!("read {}: {error}", path.display())
                            })?;
                            let checkpoint: agent_runtime::RuntimeCheckpoint =
                                serde_json::from_slice(&bytes).map_err(|error| {
                                    anyhow::anyhow!(
                                        "parse {}: {error} (is this a runtime checkpoint?)",
                                        path.display()
                                    )
                                })?;
                            runtime
                                .restore(checkpoint)
                                .await
                                .map_err(anyhow::Error::from)
                        }
                        .await;
                        match result {
                            Ok(()) => {
                                app.push_system(format!("runtime restored from {}", path.display()))
                            }
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}
