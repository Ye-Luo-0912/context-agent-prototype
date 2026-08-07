mod mock_model;
mod state;
mod ui;

use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agent_contracts::{ApprovalDecision, ContextEngine, ModelTransport};
use agent_kernel::{
    AgentKernel, AgentKernelConfig, ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate,
};
use agent_storage::FileEventJournal;
use agent_workspace::Workspace;
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
/// the kernel to the UI, the gate carries the user's decision back.
struct InteractiveHandle {
    broker: Arc<ApprovalBroker>,
    gate: Arc<InteractiveApprovalGate>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut read_only = false;
    let mut context_policy = "dynamic".to_string();
    let mut root_arg: Option<PathBuf> = None;
    for arg in std::env::args().skip(1) {
        if arg == "--read-only" {
            read_only = true;
        } else if let Some(value) = arg.strip_prefix("--context=") {
            context_policy = value.to_string();
        } else if root_arg.is_none() {
            root_arg = Some(PathBuf::from(arg));
        }
    }
    let root = root_arg.unwrap_or(std::env::current_dir().context("current directory")?);
    let workspace = Workspace::open(&root).await?;
    let journal = FileEventJournal::open(workspace.state_dir().join("traces")).await?;

    // The context engine is a composition-root choice: the same kernel, tools
    // and UI run against any `ContextEngine` implementation (P3 A/B/C, and
    // the P5 process-boundary adapter).
    let context_engine: Arc<dyn ContextEngine> = match context_policy.as_str() {
        "append" => Arc::new(AppendOnlyEngine::new()),
        "rolling" => Arc::new(RollingSummaryEngine::with_config(RollingConfig::default())),
        "dynamic" => Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        "service" => {
            connect_engine(&ContextServiceConfig {
                engine: ServiceEngine::Dynamic,
                ..ContextServiceConfig::default()
            })
            .await?
        }
        other => anyhow::bail!(
            "unknown --context policy: {other} (expected append | rolling | dynamic | service)"
        ),
    };
    let tool_dispatcher = Arc::new(BuiltinToolDispatcher::new(workspace.clone()));
    let model: Arc<dyn ModelTransport> = build_model();
    let (approval, interactive) = if read_only {
        (
            Arc::new(PolicyApprovalGate::read_only()) as Arc<dyn agent_contracts::ApprovalGate>,
            None,
        )
    } else {
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        (
            gate.clone() as Arc<dyn agent_contracts::ApprovalGate>,
            Some(InteractiveHandle { broker, gate }),
        )
    };
    let journal = Arc::new(journal);

    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        context_engine,
        model,
        tool_dispatcher,
        approval,
        Some(journal),
    ));
    let mut runtime_events = kernel.subscribe();
    kernel.start().await?;

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    let result = run_ui(
        &mut terminal,
        kernel.clone(),
        &mut runtime_events,
        interactive,
        &context_policy,
        workspace.state_dir().join("checkpoints"),
    )
    .await;

    let _ = kernel.stop().await;
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

async fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    kernel: Arc<AgentKernel>,
    runtime_events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    interactive: Option<InteractiveHandle>,
    context_policy: &str,
    checkpoint_dir: PathBuf,
) -> anyhow::Result<()> {
    let mut app = AppState::new(kernel.run_id());
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
                        let kernel = kernel.clone();
                        let goal = goal.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = kernel.set_focus(goal).await {
                                tracing::error!(%error, "set focus failed");
                            }
                        });
                        continue;
                    }
                    if let Some(content) = trimmed.strip_prefix("/pin ") {
                        let kernel = kernel.clone();
                        let content = content.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = kernel.pin(content).await {
                                tracing::error!(%error, "pin failed");
                            }
                        });
                        continue;
                    }
                    if let Some(summary) = trimmed.strip_prefix("/done ") {
                        let kernel = kernel.clone();
                        let summary = summary.trim().to_string();
                        tokio::spawn(async move {
                            if let Err(error) = kernel.complete_current_task(summary).await {
                                tracing::error!(%error, "complete task failed");
                            }
                        });
                        continue;
                    }
                    if trimmed == "/context" {
                        let kernel = kernel.clone();
                        tokio::spawn(async move {
                            if let Err(error) = kernel.emit_diagnostics().await {
                                tracing::error!(%error, "context diagnostics failed");
                            }
                        });
                        continue;
                    }
                    if trimmed == "/checkpoint" {
                        let path =
                            checkpoint_dir.join(format!("{}-{}.json", kernel.run_id(), now_ms()));
                        match kernel.checkpoint().await {
                            Ok(value) => {
                                let bytes = match serde_json::to_vec_pretty(&value) {
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
                                        "checkpoint saved: {}",
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
                    if trimmed == "/cancel" {
                        kernel.cancel_current_turn().await;
                        continue;
                    }

                    if app.busy {
                        app.push_system(
                            "Agent is busy; wait for the current turn to finish.".into(),
                        );
                        continue;
                    }
                    let kernel = kernel.clone();
                    tokio::spawn(async move {
                        if let Err(error) = kernel.handle_user_message(input).await {
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
    });
    Arc::new(RetryingTransport::new(
        provider,
        3,
        Duration::from_millis(500),
    ))
}
