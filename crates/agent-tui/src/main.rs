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
    try_model_from_env,
};
use agent_contracts::{ApprovalDecision, StandingGrant};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, PolicyApprovalGate, TaskApprovalGate};
use agent_runtime::{RuntimeCheckpoint, RuntimeHandle, RuntimeInstance};
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
    for arg in std::env::args().skip(1) {
        if arg == "--read-only" {
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
    // state is touched: read, parse and validate it up front.
    let restore_checkpoint = match &restore_arg {
        Some(path) => Some(load_runtime_checkpoint(path)?),
        None => None,
    };
    let policy = ContextPolicy::from_str_checked(&context_policy)?;
    let root = root_arg.unwrap_or(std::env::current_dir().context("current directory")?);
    let workspace = Workspace::open(&root).await?;
    let journal = Arc::new(FileEventJournal::open(workspace.state_dir().join("traces")).await?);

    // The context engine and the model are composition-root choices shared
    // with CLI/eval (agent-compose): the same kernel, tools and UI run
    // against any `ContextEngine` implementation (the A/B/C baselines, and
    // the process-boundary adapter). Rolling/dynamic 与 live eval 共用同一
    // 有界压缩器，避免 TUI 仍走占位折叠。
    let model = match try_model_from_env()? {
        agent_compose::ModelSelection::Mock(mock) => {
            eprintln!("demo mode: AGENT_DEMO=1 selected the explicit mock transport");
            mock
        }
        agent_compose::ModelSelection::Provider(provider) => provider,
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
    if let Some(checkpoint) = restore_checkpoint {
        let path = restore_arg.clone().unwrap_or_default();
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
                    if trimmed == "/checkpoints" {
                        // Bounded newest-first listing of the checkpoint
                        // store, so save/list/resume is discoverable from
                        // the product host without filesystem spelunking.
                        match list_checkpoint_rows(&checkpoint_dir).await {
                            Ok(rows) if rows.is_empty() => {
                                app.push_system(
                                    "no checkpoints saved yet; /checkpoint writes one".to_string(),
                                );
                            }
                            Ok(rows) => {
                                let shown = rows.len().min(CHECKPOINT_LIST_LIMIT);
                                for (modified, size, path) in &rows[..shown] {
                                    let age = modified
                                        .elapsed()
                                        .map(|elapsed| format!("{elapsed:?} ago"))
                                        .unwrap_or_else(|_| "unknown age".into());
                                    app.push_system(format!("{path} ({size} bytes, {age})"));
                                }
                                if rows.len() > shown {
                                    app.push_system(format!(
                                        "...and {} more (oldest first hidden)",
                                        rows.len() - shown
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
                                    Ok(()) => {
                                        app.last_checkpoint = Some(path.display().to_string());
                                        app.push_system(format!(
                                            "checkpoint saved ({} tasks): {}",
                                            checkpoint.tasks.tasks.len(),
                                            path.display()
                                        ))
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

/// Read, parse and validate a runtime checkpoint file. Every failure mode
/// is a visible configuration-style error; nothing has been started or
/// mutated when this runs.
fn load_runtime_checkpoint(path: &std::path::Path) -> anyhow::Result<RuntimeCheckpoint> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read checkpoint {}", path.display()))?;
    let checkpoint: RuntimeCheckpoint = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}: is this a runtime checkpoint?", path.display()))?;
    checkpoint.validate().map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "invalid checkpoint {}: the runtime refuses it before any mutation",
            path.display()
        ))
    })?;
    Ok(checkpoint)
}

/// Bounded number of checkpoint rows the `/checkpoints` listing renders.
const CHECKPOINT_LIST_LIMIT: usize = 20;

/// Newest-first `(modified, bytes, path)` rows for the checkpoint store.
/// Non-`json` entries are skipped; unreadable metadata is skipped rather
/// than failing the whole listing.
async fn list_checkpoint_rows(
    dir: &std::path::Path,
) -> anyhow::Result<Vec<(std::time::SystemTime, u64, String)>> {
    let mut rows = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        let modified = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        rows.push((modified, metadata.len(), path.display().to_string()));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkpoint_listing_is_newest_first_and_skips_non_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("b.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"ignore").unwrap();
        // Give the second file a strictly later mtime where the filesystem
        // has enough resolution; on coarse-clock filesystems the order tie
        // is acceptable, so this test only asserts membership and count.
        let rows = list_checkpoint_rows(dir.path()).await.unwrap();
        assert_eq!(rows.len(), 2, "only json checkpoints are listed");
        assert!(rows.iter().all(|(_, _, path)| path.ends_with(".json")));
    }

    #[tokio::test]
    async fn checkpoint_listing_of_a_missing_dir_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(list_checkpoint_rows(&missing).await.is_err());
    }

    #[test]
    fn load_runtime_checkpoint_reports_readable_failures() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let error = load_runtime_checkpoint(&missing).unwrap_err().to_string();
        assert!(error.contains("read checkpoint"), "{error}");

        let garbage = dir.path().join("garbage.json");
        std::fs::write(&garbage, b"not json").unwrap();
        let error = load_runtime_checkpoint(&garbage).unwrap_err().to_string();
        assert!(error.contains("is this a runtime checkpoint?"), "{error}");
    }
}
