mod mock_model;
mod state;
mod ui;

use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agent_contracts::{ApprovalDecision, ContextEngine, ModelTransport, StandingGrant};
use agent_kernel::{
    AgentKernelConfig, ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate,
    TaskApprovalGate,
};
use agent_runtime::{
    ApprovalModule, ArtifactModule, CapabilityAwareDispatcher, ContextModule, EventModule,
    ModelModule, ModuleHost, RuntimeHandle, RuntimeInstance, RuntimeServices, ToolModule,
};
use agent_storage::FileEventJournal;
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use anyhow::Context;
use context_baselines::{AppendOnlyEngine, RollingConfig, RollingSummaryEngine};
use context_contextcore::{ContextServiceConfig, ServiceEngine, connect_engine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mock_model::MockModelTransport;
use provider_openai::{OpenAiConfig, OpenAiProvider, RetryingTransport};
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
    let root = root_arg.unwrap_or(std::env::current_dir().context("current directory")?);
    let workspace = Workspace::open(&root).await?;
    let journal = FileEventJournal::open(workspace.state_dir().join("traces")).await?;

    // The context engine is a composition-root choice: the same kernel, tools
    // and UI run against any `ContextEngine` implementation (the A/B/C baselines, and
    // the process-boundary adapter).
    let context_engine: Arc<dyn ContextEngine> = match context_policy.as_str() {
        "append" => Arc::new(AppendOnlyEngine::new()),
        "rolling" => Arc::new(RollingSummaryEngine::with_config(RollingConfig::default())),
        "dynamic" => Arc::new(SimpleContextEngine::new(SimpleContextConfig {
            // The external context store lives with the other runtime state,
            // never guessed from the CWD: a run started from a crate
            // directory must not scatter `.focus-agent/context-store`
            // folders around the tree.
            context_store_dir: Some(workspace.state_dir().join("context-store")),
            ..SimpleContextConfig::default()
        })),
        "service" => {
            connect_engine(&ContextServiceConfig {
                engine: ServiceEngine::Dynamic,
                // The service's context store must live under the workspace
                // state dir too — the child never guesses a CWD-relative path.
                store_dir: Some(workspace.state_dir().join("context-store")),
                ..ContextServiceConfig::default()
            })
            .await?
        }
        other => anyhow::bail!(
            "unknown --context policy: {other} (expected append | rolling | dynamic | service)"
        ),
    };
    let model: Arc<dyn ModelTransport> = build_model();
    if read_only && !grant_args.is_empty() {
        anyhow::bail!("--grant cannot be combined with --read-only");
    }
    let (approval, interactive) = if read_only {
        (
            Arc::new(PolicyApprovalGate::read_only()) as Arc<dyn agent_contracts::ApprovalGate>,
            None,
        )
    } else {
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        let task_gate = Arc::new(TaskApprovalGate::new(gate.clone()));
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
    let journal = Arc::new(journal);

    // The module host publishes typed capabilities with a uniform lifecycle
    // (register, validate, start, stop). The kernel consumes the same
    // capabilities back from the registry, so composition conflicts fail
    // fast before anything runs.
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(context_engine)))?;
    host.add_module(Arc::new(ModelModule::new(model)))?;
    // The tool provider merges the built-in tools with dynamic capabilities:
    // capabilities registered against the shared registry (even mid-run) are
    // exposed to the model on the next request.
    let capability_registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::with_workspace(
        Arc::new(BuiltinToolDispatcher::new(workspace.clone())),
        capability_registry,
        // Capabilities that declare workspace/artifact permissions receive
        // confined handles into the same workspace the builtin tools use.
        Some(Arc::new(workspace.clone())),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher)))?;
    host.add_module(Arc::new(ApprovalModule::new(approval.clone())))?;
    host.add_module(Arc::new(EventModule::new(journal.clone())))?;
    host.add_module(Arc::new(ArtifactModule::new(Arc::new(workspace.clone()))))?;
    host.start().await?;

    let kernel_config = AgentKernelConfig {
        // The composition-root output broker: bounds every model-facing
        // tool field and spills oversized content under the run's
        // artifact directory before it reaches the actor.
        output_broker: Some(Arc::new(WorkspaceOutputBroker::new(
            workspace.clone().into(),
        ))),
        ..AgentKernelConfig::default()
    };
    // The composition seam: every service the run needs is resolved from
    // the module host's typed registry and handed to the runtime as one
    // `RuntimeServices`; the kernel is derived inside the runtime.
    let services = RuntimeServices::from_registry(host.registry(), kernel_config)?;
    // The runtime actor owns all subsequent mutation: commands are serialized
    // and long-running turns report back as operations, so focus/pin/task
    // commands can no longer race an in-flight turn. The instance owns the
    // host, the handle and the actor task, so shutdown runs in one ordered
    // step and surfaces every error.
    let runtime = RuntimeInstance::spawn(host, services);
    let mut runtime_events = runtime.handle().subscribe();
    runtime.start().await?;

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    let result = run_ui(
        &mut terminal,
        runtime.handle().clone(),
        &runtime,
        &mut runtime_events,
        interactive,
        &context_policy,
        workspace.state_dir().join("checkpoints"),
    )
    .await;

    // cancel -> stop actor (flush journal, RunCompleted) -> stop modules ->
    // join the actor; any failure is aggregated into one error.
    let shutdown_result = runtime.shutdown().await;
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

    loop {
        while let Ok(event) = runtime_events.try_recv() {
            app.apply_runtime_event(event);
        }
        if let Some(rx) = &mut approval_rx {
            while let Ok(request) = rx.try_recv() {
                app.begin_approval(request);
            }
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
                        tokio::spawn(async move {
                            match handle.list_tasks().await {
                                Ok(tasks) => {
                                    for task in tasks {
                                        println!(
                                            "task {} [{:?}] tools=r{}/{} {}",
                                            task.id,
                                            task.status,
                                            task.tool_requirement_revision,
                                            task.tool_requirement_count,
                                            task.goal
                                        );
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
                        tokio::spawn(async move {
                            let grants = task_grants.active_grants().await;
                            if grants.is_empty() {
                                println!("no active standing grants");
                            }
                            for grant in grants {
                                println!(
                                    "grant {} risk={:?} workspace={:?} command={:?} \
                                     max_runs={:?} max_bytes={:?} expires_at_ms={}",
                                    grant.id,
                                    grant.risk,
                                    grant.target.workspace_path_prefix,
                                    grant.target.process_command_prefix,
                                    grant.constraint.max_runs,
                                    grant.constraint.max_content_bytes,
                                    grant.expires_at_ms,
                                );
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
                        handle.cancel_turn().await;
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

/// Composition-root model selection: a real OpenAI-compatible provider when
/// `OPENAI_API_KEY` is set, otherwise the mock transport.
///
/// Optional overrides:
/// - `OPENAI_BASE_URL` (default `https://api.openai.com/v1`) — point at
///   DeepSeek (`https://api.deepseek.com/v1`), Qwen, Moonshot, GLM, ...
/// - `OPENAI_MODEL` (default `gpt-4o-mini`)
fn build_model() -> Arc<dyn ModelTransport> {
    let Ok(api_key) = std::env::var("OPENAI_API_KEY") else {
        return Arc::new(MockModelTransport);
    };
    if api_key.trim().is_empty() {
        return Arc::new(MockModelTransport);
    }

    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key,
        base_url,
        model,
        max_output_tokens: 4096,
        timeout: Duration::from_secs(120),
        send_stream_options: true,
        send_max_tokens: true,
        max_stream_bytes: provider_openai::DEFAULT_MAX_STREAM_BYTES,
    });
    Arc::new(RetryingTransport::new(
        provider,
        3,
        Duration::from_millis(500),
    ))
}
