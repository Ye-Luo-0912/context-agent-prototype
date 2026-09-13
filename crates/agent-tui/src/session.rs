//! The interactive TUI session: one loop that folds runtime events into
//! [`AppState`], draws frames, reads keys and dispatches the product
//! commands. The loop is split from the terminal behind [`UiSource`] and
//! [`UiSink`] so the exact session path — key input, command dispatch,
//! real runtime handle, event folding — can be driven end-to-end in tests
//! with scripted keys and a capture sink, no pty and no manual operator.

use std::{io, path::PathBuf, sync::Arc, time::Duration};

use agent_contracts::{ApprovalDecision, RuntimeEventEnvelope};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, TaskApprovalGate};
use agent_runtime::{CheckpointStore, RuntimeHandle, RuntimeInstance, decode_checkpoint_bytes};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::state::AppState;

/// Cap on the note channel that carries off-input-thread command output
/// (`/tasks`, `/grants`) back into the alternate-screen frame. The channel
/// is bounded so a pathological catalog listing cannot grow the UI's
/// pending-notice queue without limit; full notifications are dropped whole
/// rather than blocking the command task.
pub(crate) const NOTICE_CHANNEL_CAP: usize = 64;

/// UI-side handles for interactive approval: the broker carries requests from
/// the kernel to the UI, the gate carries the user's decision back, and the
/// task gate holds the standing grants (established from `--grant` on the
/// command line, revocable from the UI).
pub(crate) struct InteractiveHandle {
    pub(crate) broker: Arc<ApprovalBroker>,
    pub(crate) gate: Arc<InteractiveApprovalGate>,
    pub(crate) task_grants: Arc<TaskApprovalGate>,
}

/// Where key events come from. The terminal implementation polls crossterm;
/// tests feed a scripted queue. Async so a test source can park without
/// starving the runtime actor on the same executor.
pub(crate) trait UiSource {
    async fn poll_key(&mut self, timeout: Duration) -> io::Result<Option<KeyEvent>>;
}

/// The real terminal source: crossterm poll/read, zero-wait slicing so the
/// surrounding async loop keeps yielding to the runtime actor.
pub(crate) struct TerminalSource;

impl UiSource for TerminalSource {
    async fn poll_key(&mut self, timeout: Duration) -> io::Result<Option<KeyEvent>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if event::poll(Duration::ZERO)? {
                return Ok(if let Event::Key(key) = event::read()? {
                    Some(key)
                } else {
                    None
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

/// Where frames go. The terminal implementation renders through ratatui;
/// tests capture snapshots for assertions.
pub(crate) trait UiSink {
    fn draw(&mut self, app: &AppState) -> io::Result<()>;
}

/// The real terminal sink.
pub(crate) struct TerminalSink<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>,
}

impl<'a> TerminalSink<'a> {
    pub(crate) fn new(terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>) -> Self {
        Self { terminal }
    }
}

impl UiSink for TerminalSink<'_> {
    fn draw(&mut self, app: &AppState) -> io::Result<()> {
        self.terminal
            .draw(|frame| crate::ui::render(frame, app))
            .map(drop)
    }
}

/// Drive one interactive session to completion. Returns when the operator
/// quits (Ctrl-C or `/quit`); the caller owns runtime shutdown.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_session<S: UiSource, O: UiSink>(
    source: &mut S,
    sink: &mut O,
    handle: RuntimeHandle,
    runtime: &RuntimeInstance,
    runtime_events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
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
    app.state_dir = checkpoint_dir.parent().map(std::path::Path::to_path_buf);

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
                    // the durable journal (current run only), and continue.
                    let (folded, partial) = app.resync_projection(&traces_dir).await;
                    app.push_system(format!(
                        "warning: the UI fell behind and dropped {skipped} runtime events; the status projection was resynced from the journal ({folded} events folded){}",
                        if partial { " — PARTIAL: a journal file was unreadable or truncated" } else { "" }
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

        sink.draw(&app)?;

        let Some(key) = source.poll_key(Duration::from_millis(30)).await? else {
            continue;
        };
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
                let keep_running = dispatch_command(
                    &mut app,
                    trimmed,
                    &handle,
                    runtime,
                    interactive.as_ref(),
                    &notice_tx,
                    &checkpoint_dir,
                )
                .await?;
                if !keep_running {
                    break;
                }
            }
            // scroll is holdback from the latest row: PageUp reads older
            // SYSTEM/dialogue, PageDown returns toward the live tail.
            KeyCode::PageUp => app.scroll = app.scroll.saturating_add(8),
            KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(8),
            KeyCode::Tab => app.toggle_context_panel(),
            KeyCode::Esc => app.input.clear(),
            _ => {}
        }
    }

    Ok(())
}

/// Dispatch one submitted input line. `Ok(false)` ends the session
/// (`/quit`); every other product command keeps the loop alive. Display
/// and side effects ride the same paths as before the extraction: direct
/// state pushes for inline answers, the notice channel for off-loop
/// command tasks.
#[allow(clippy::too_many_arguments)]
async fn dispatch_command(
    app: &mut AppState,
    trimmed: &str,
    handle: &RuntimeHandle,
    runtime: &RuntimeInstance,
    interactive: Option<&InteractiveHandle>,
    notice_tx: &tokio::sync::mpsc::Sender<String>,
    checkpoint_dir: &std::path::Path,
) -> anyhow::Result<bool> {
    if trimmed == "/quit" {
        return Ok(false);
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
        return Ok(true);
    }
    if let Some(id_text) = trimmed.strip_prefix("/task ") {
        // Activate an existing task by id (resume its scopes). Task ids
        // come from `/tasks`; activation alone does not start a turn —
        // `/continue` does.
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
        return Ok(true);
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
        return Ok(true);
    }
    if let Some(goal_text) = trimmed.strip_prefix("/work ") {
        let goal = goal_text.trim().to_string();
        if goal.is_empty() {
            app.push_system("usage: /work <goal>".to_string());
            return Ok(true);
        }
        // Explicit long-task entry through the shared atomic `start_work`
        // command (P1): the actor creates (or resumes) the task while idle,
        // attaches the PreferSurface demand for task.manage onto an empty
        // requirement set, and delivers the goal once through the normal
        // user-message path inside one serialized command. No second
        // orchestrator.
        let handle = handle.clone();
        let notice_tx = notice_tx.clone();
        tokio::spawn(async move {
            match crate::work::start_long_task(&handle, goal).await {
                Ok(submission) => {
                    if let Some(warning) = submission.task_manage_notice {
                        let _ = notice_tx.try_send(warning);
                    }
                }
                Err(error) => {
                    let _ = notice_tx.try_send(format!("work failed: {error}"));
                }
            }
        });
        return Ok(true);
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
                    let _ = notice_tx.try_send("no active task; /work <goal> starts one".into());
                }
                Err(error) => {
                    let _ = notice_tx.try_send(format!("plan failed: {error}"));
                }
            }
        });
        return Ok(true);
    }
    if trimmed == "/review" {
        // Render the latest result card: freshest is the in-session
        // event-derived card; otherwise fall back to the persisted
        // artifact from a previous task. Display only — no model call,
        // no write.
        if !app.result_card.is_empty() {
            for line in crate::state::format_result_lines(&app.result_card) {
                app.push_system(line);
            }
        } else {
            let notice_tx = notice_tx.clone();
            let state_dir = app.state_dir.clone();
            tokio::spawn(async move {
                let Some(state_dir) = state_dir else {
                    let _ = notice_tx.try_send("no result material yet".into());
                    return;
                };
                let path = state_dir.join("artifacts").join("result-card-latest.json");
                match tokio::fs::read(&path).await {
                    Ok(bytes) => match serde_json::from_slice::<crate::state::ResultCard>(&bytes) {
                        Ok(card) if !card.is_empty() => {
                            for line in crate::state::format_result_lines(&card) {
                                let _ = notice_tx.try_send(line);
                            }
                        }
                        _ => {
                            let _ = notice_tx.try_send("no result material yet".into());
                        }
                    },
                    Err(_) => {
                        let _ = notice_tx.try_send("no result material yet".into());
                    }
                }
            });
        }
        return Ok(true);
    }
    if trimmed == "/grants" {
        let Some(handle) = interactive else {
            return Ok(true);
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
        return Ok(true);
    }
    if let Some(id) = trimmed.strip_prefix("/revoke ") {
        let Some(handle) = interactive else {
            app.push_system(
                "grant revoke needs an interactive (non read-only) session".to_string(),
            );
            return Ok(true);
        };
        let id = id.trim();
        if id.is_empty() {
            app.push_system("usage: /revoke <grant-id>".to_string());
            return Ok(true);
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
        return Ok(true);
    }
    if trimmed == "/suspend" {
        let handle = handle.clone();
        let notice_tx = notice_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = handle.suspend_task().await {
                let _ = notice_tx.try_send(format!("suspend failed: {error}"));
            }
        });
        return Ok(true);
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
        return Ok(true);
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
        return Ok(true);
    }
    if trimmed == "/context" {
        let handle = handle.clone();
        let notice_tx = notice_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = handle.emit_diagnostics().await {
                let _ = notice_tx.try_send(format!("context diagnostics failed: {error}"));
            }
        });
        return Ok(true);
    }
    if trimmed == "/status" {
        for line in app.render_status() {
            app.push_system(line);
        }
        return Ok(true);
    }
    if trimmed == "/diag-export" {
        let state_dir = checkpoint_dir
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| checkpoint_dir.to_path_buf());
        let projection_lines = app.status_projection.lines();
        let tail: Vec<String> = app
            .messages
            .iter()
            .rev()
            .take(40)
            .map(|message| message.content.clone())
            .collect();
        match crate::doctor::export_diagnostics(&state_dir, projection_lines, tail).await {
            Ok(path) => app.push_system(format!("diagnostics written: {}", path.display())),
            Err(error) => app.push_system(format!("diagnostics export failed: {error}")),
        }
        return Ok(true);
    }
    if trimmed == "/checkpoints" {
        // Bounded newest-first listing straight from the runtime
        // checkpoint store, so save/list/resume is discoverable from the
        // product host without filesystem spelunking.
        let store = CheckpointStore::new(checkpoint_dir.to_path_buf());
        match store.list(CHECKPOINT_LIST_LIMIT).await {
            Ok(rows) if rows.is_empty() => {
                app.push_system("no checkpoints saved yet; /checkpoint writes one".to_string());
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
            Err(error) => app.push_system(format!("checkpoint list failed: {error}")),
        }
        return Ok(true);
    }
    if trimmed == "/checkpoint" {
        // The manual save rides the same atomic envelope store as the
        // automatic safe points: one format, one retention domain,
        // checksum verified on load.
        let store = CheckpointStore::new(checkpoint_dir.to_path_buf());
        match runtime.checkpoint().await {
            Ok(checkpoint) => {
                let tasks = checkpoint.tasks.tasks.len();
                let bytes = match serde_json::to_vec(&checkpoint) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        app.push_system(format!("checkpoint serialize failed: {error}"));
                        return Ok(true);
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
                    Err(error) => app.push_system(format!("checkpoint write failed: {error}")),
                }
            }
            Err(error) => app.push_system(format!("checkpoint failed: {error}")),
        }
        return Ok(true);
    }
    if let Some(restore_target) = trimmed.strip_prefix("/restore ") {
        let result = async {
            let path = resolve_restore_target(checkpoint_dir, restore_target.trim());
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
            let checkpoint = decode_checkpoint_bytes(&bytes)
                .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
            runtime
                .restore(checkpoint)
                .await
                .map_err(anyhow::Error::from)
        }
        .await;
        match result {
            Ok(()) => {
                app.push_system("runtime restored; /continue resumes the active task".to_string())
            }
            Err(error) => app.push_system(format!("restore failed: {error}")),
        }
        return Ok(true);
    }
    if trimmed == "/cancel" {
        if let Err(error) = handle.cancel_turn().await {
            app.push_system(format!("cancel failed: {error}"));
        }
        return Ok(true);
    }
    if trimmed == "/continue" {
        // Re-run the active task's stored directive in a fresh turn: no
        // new instruction identity is minted and the stored directive is
        // not re-ingested. Refusals (no active task, busy runtime,
        // recovery required) surface here; the started turn itself is
        // event-driven (`TaskContinuationStarted`).
        let handle = handle.clone();
        let notice_tx = notice_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = handle.continue_active_task().await {
                let _ = notice_tx.try_send(format!("continue failed: {error}"));
            }
        });
        return Ok(true);
    }

    if trimmed == "/help" {
        for line in HELP_LINES {
            app.push_system((*line).to_string());
        }
        return Ok(true);
    }
    if trimmed.starts_with('/') {
        app.push_system(format!(
            "unknown command {trimmed}; /help lists the product commands,                              and non-command input needs no leading slash"
        ));
        return Ok(true);
    }
    // Normal input always goes to the runtime: an idle runtime starts a
    // turn, a busy one queues it in the existing single dialogue slot.
    // The command reply plus the `UserInput` lifecycle events own the
    // visible disposition (queued / applied / rejected) — the UI no
    // longer drops input on its own busy guess.
    let handle = handle.clone();
    let notice_tx = notice_tx.clone();
    let input = trimmed.to_string();
    tokio::spawn(async move {
        if let Err(error) = handle.user_message(input).await {
            let _ = notice_tx.try_send(format!("input not accepted: {error}"));
        }
    });
    Ok(true)
}

/// Read, parse and validate a runtime checkpoint file in any product
/// format (envelope artifact or legacy raw JSON). Every failure mode is a
/// visible configuration-style error; nothing has been started or mutated
/// when this runs.
pub(crate) fn load_runtime_checkpoint(
    path: &std::path::Path,
) -> anyhow::Result<agent_runtime::RuntimeCheckpoint> {
    agent_runtime::decode_checkpoint_file(path).map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "invalid checkpoint {}: the runtime refuses it before any mutation",
            path.display()
        ))
    })
}

/// The product command list `/help` renders. Kept in one place so the
/// welcome hint and the help output cannot drift apart.
const HELP_LINES: [&str; 19] = [
    "/focus <directive> - point the runtime at a task directive",
    "/work <goal> - start (or resume) a long task with task.manage available",
    "/plan - show the active task's checklist, next action and open loops",
    "/review - show what changed, what was checked, what remains",
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

/// Resume discovery: the newest saved checkpoint in the store. A missing
/// or empty store is a configuration error with the fix in the message.
pub(crate) fn resolve_latest_checkpoint(
    dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
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

/// End-to-end tests of the interactive session loop itself: a real
/// composed runtime (capability-aware dispatcher, output broker, task-gate
/// approval with pre-seeded standing grants), a scripted model, and the
/// exact `run_session` loop driven by a scripted key queue with a capture
/// sink. These replace the former manual TUI walkthroughs — no pty, no
/// human operator.
#[cfg(test)]
mod tui_e2e {
    use super::*;
    use agent_compose::{
        ComposeConfig, ContextPolicy, HostToolPolicyRegistry, build_context_engine, compose,
    };
    use agent_contracts::{
        AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelRole, ModelTransport,
        RuntimeEvent, RuntimeFailureClass, StandingGrant, ToolCall,
    };
    use agent_core::{ApprovalBroker, InteractiveApprovalGate, TaskApprovalGate};
    use agent_storage::FileEventJournal;
    use agent_workspace::{Workspace, WorkspaceOutputBroker};
    use anyhow::Context;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

    type Captured = Arc<std::sync::Mutex<Vec<String>>>;

    /// Compose the interactive product path: capability-aware dispatcher,
    /// output broker, journal/artifact store, and the interactive approval
    /// wiring (broker + gate + task gate) with pre-seeded standing grants —
    /// the same shape main() wires for a TUI session.
    async fn tui_compose(
        root: &std::path::Path,
        grants: &[String],
        model: Arc<dyn ModelTransport>,
        max_tool_rounds: Option<usize>,
    ) -> anyhow::Result<(agent_compose::ComposedRuntime, InteractiveHandle, PathBuf)> {
        let workspace = Workspace::open(root).await?;
        let checkpoint_dir = workspace.state_dir().join("checkpoints");
        let journal = Arc::new(FileEventJournal::open(workspace.state_dir().join("traces")).await?);
        let recipes = Arc::new(VerificationRecipes::discover(&workspace)?);
        let host_policies = Arc::new(
            HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
                .map_err(anyhow::Error::msg)?,
        );
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        let task_gate =
            Arc::new(TaskApprovalGate::new(gate.clone()).with_host_policies(host_policies.clone()));
        for json in grants {
            let grant: StandingGrant = serde_json::from_str(json)
                .with_context(|| format!("invalid grant JSON: {json}"))?;
            task_gate.grant(grant).await?;
        }
        let approval = task_gate.clone() as Arc<dyn agent_contracts::ApprovalGate>;
        let interactive = InteractiveHandle {
            broker,
            gate,
            task_grants: task_gate,
        };
        let context_engine = build_context_engine(
            ContextPolicy::Dynamic,
            workspace.state_dir(),
            Some(model.clone()),
        )
        .await?;
        let base_tools = Arc::new(BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            Default::default(),
            (*recipes).clone(),
        ));
        let reservation = workspace
            .state_dir()
            .join("authority")
            .join("broker-reservations.jsonl");
        let artifact_store = Arc::new(workspace.clone());
        let output_broker = Arc::new(WorkspaceOutputBroker::new(workspace.clone().into()));
        let composed = compose(ComposeConfig {
            provider_profile_digest: None,
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
            max_tool_rounds,
            project_task_progress: true,
            project_settlement: false,
            settlement_projection_diagnostics: false,
            project_completion_opportunity: false,
            recovery_surface: false,
            host_policies: Some(host_policies),
            effect_reservation_journal: Some(reservation),
            verification_recipes: Some(recipes.clone()),
            project_proof_refresh: !recipes.is_empty(),
            // Session-loop E2E shares the product binary's dispatch.
            host_death_watchdog: true,
            // Harness/eval compositions register no external capabilities by default.
            mcp_servers: Vec::new(),
            plugins: None,
        })
        .await?;
        Ok((composed, interactive, checkpoint_dir))
    }

    /// A workspace-write standing grant for one file inside the workspace.
    fn write_grant(file: &str) -> String {
        serde_json::json!({
            "id": format!("write-{file}"),
            "risk": "WorkspaceWrite",
            "target": { "workspace_path_prefix": file },
            "constraint": {},
            "expires_at_ms": u64::MAX,
        })
        .to_string()
    }

    /// Scripted key source: pops from the orchestrator's queue. After the
    /// orchestrator disappears (it sends quit when done) a disconnect must
    /// not leave the session polling forever — it quits the session.
    struct KeySource {
        rx: tokio::sync::mpsc::Receiver<KeyEvent>,
        disconnected: bool,
    }

    impl UiSource for KeySource {
        async fn poll_key(&mut self, timeout: Duration) -> io::Result<Option<KeyEvent>> {
            if self.disconnected {
                return Ok(Some(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )));
            }
            match tokio::time::timeout(timeout, self.rx.recv()).await {
                Ok(Some(key)) => Ok(Some(key)),
                Ok(None) => {
                    self.disconnected = true;
                    Ok(Some(KeyEvent::new(
                        KeyCode::Char('c'),
                        KeyModifiers::CONTROL,
                    )))
                }
                Err(_) => Ok(None),
            }
        }
    }

    /// Capture sink: every draw stores the messages, the status projection
    /// lines and the last checkpoint name, so assertions read what the
    /// operator would have seen.
    #[derive(Clone)]
    struct CaptureSink {
        captured: Captured,
    }

    impl UiSink for CaptureSink {
        fn draw(&mut self, app: &AppState) -> io::Result<()> {
            let mut frame = String::new();
            for message in &app.messages {
                frame.push_str(&message.content);
                frame.push('\n');
            }
            for line in app.status_projection.lines() {
                frame.push_str(&line);
                frame.push('\n');
            }
            if let Some(path) = &app.last_checkpoint {
                frame.push_str("last_checkpoint: ");
                frame.push_str(path);
                frame.push('\n');
            }
            self.captured.lock().unwrap().push(frame);
            Ok(())
        }
    }

    async fn send_line(tx: &tokio::sync::mpsc::Sender<KeyEvent>, line: &str) {
        for ch in line.chars() {
            tx.send(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .await
                .expect("the session key channel stays open");
        }
        tx.send(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the session key channel stays open");
    }

    fn quit(tx: &tokio::sync::mpsc::Sender<KeyEvent>) {
        let _ = tx.try_send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    async fn wait_for_event(
        rx: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
        what: &str,
        pred: impl Fn(&RuntimeEvent) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            match rx.try_recv() {
                Ok(envelope) => {
                    if pred(&envelope.event) {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    panic!("event stream closed while waiting for {what}");
                }
            }
        }
    }

    async fn wait_for_line(captured: &Captured, needle: &str, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            {
                let guard = captured.lock().unwrap();
                if guard.iter().any(|frame| frame.contains(needle)) {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {needle:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_until_file(root: &std::path::Path, name: &str, contains: &str, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let path = root.join(name);
        loop {
            if let Ok(content) = std::fs::read_to_string(&path)
                && content.contains(contains)
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what} ({name} to contain {contains:?})"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn transcript(captured: &Captured) -> String {
        captured.lock().unwrap().join("\n")
    }

    /// The whole closure loop a user walks after /work: the produced
    /// result reads as awaiting operator review, /done closes durably
    /// through the operator path, /status names both states. Replaces the
    /// manual M16-02 closure-semantics walkthrough.
    #[tokio::test]
    async fn tui_e2e_work_status_names_awaiting_review_then_done_closes_durably() {
        struct WorkModel;
        #[async_trait::async_trait]
        impl ModelTransport for WorkModel {
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities {
                    streaming: true,
                    tool_calls: true,
                    max_output_tokens: 4096,
                    context_window: None,
                }
            }
            async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
                let has_tool_result = request
                    .messages
                    .iter()
                    .any(|message| message.role == ModelRole::Tool);
                if !has_tool_result {
                    return Ok(ModelOutput {
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "plan-1".into(),
                            name: "task.manage".into(),
                            arguments: serde_json::json!({
                                "base_anchor_revision": 0,
                                "plan_progress": ["[x] read the module"],
                                "next_action": "add the feature",
                            }),
                        }],
                        usage: Default::default(),
                    });
                }
                Ok(ModelOutput {
                    content: "[scripted] work delivered".into(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (composed, interactive, checkpoint_dir) =
            tui_compose(&root, &[], Arc::new(WorkModel), None)
                .await
                .unwrap();
        let mut ui_events = composed.subscribe();
        let mut orch_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = CaptureSink {
            captured: captured.clone(),
        };
        let orch_captured = captured.clone();
        let orch = tokio::spawn(async move {
            send_line(&key_tx, "/work add the feature").await;
            wait_for_event(&mut orch_events, "the first TurnCompleted", |event| {
                matches!(event, RuntimeEvent::TurnCompleted)
            })
            .await;
            send_line(&key_tx, "/status").await;
            wait_for_line(
                &orch_captured,
                "awaiting operator review",
                "/status awaiting review",
            )
            .await;
            send_line(&key_tx, "/done feature landed").await;
            wait_for_event(&mut orch_events, "TaskCompleted", |event| {
                matches!(event, RuntimeEvent::TaskCompleted { .. })
            })
            .await;
            send_line(&key_tx, "/status").await;
            wait_for_line(
                &orch_captured,
                "durably completed (operator accepted): feature landed",
                "/status durable closure",
            )
            .await;
            quit(&key_tx);
        });

        let mut source = KeySource {
            rx: key_rx,
            disconnected: false,
        };
        let mut sink = sink;
        let session = tokio::time::timeout(
            Duration::from_secs(90),
            run_session(
                &mut source,
                &mut sink,
                composed.handle().clone(),
                &composed.instance,
                &mut ui_events,
                Some(interactive),
                "dynamic",
                checkpoint_dir,
                "serving: scripted e2e model".to_string(),
                None,
                false,
            ),
        )
        .await;
        assert!(session.is_ok(), "the session hung: {session:?}");
        session.expect("timeout").expect("session error");
        orch.await.expect("orchestrator task");
        composed.shutdown().await.unwrap();

        let seen = transcript(&captured);
        assert!(seen.contains("awaiting operator review"), "{seen}");
        assert!(
            seen.contains("durably completed (operator accepted): feature landed"),
            "{seen}"
        );
        assert!(!seen.contains("done failed"), "{seen}");
    }

    /// Busy-time input is queued, named, and later applied — not dropped.
    /// The first model call blocks until the orchestrator releases it, so
    /// the "second ask" provably arrives mid-turn. Replaces the manual
    /// M16-01 busy-input walkthrough.
    #[tokio::test]
    async fn tui_e2e_busy_input_is_queued_named_and_applied() {
        struct GatedModel {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ModelTransport for GatedModel {
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities {
                    streaming: true,
                    tool_calls: true,
                    max_output_tokens: 4096,
                    context_window: None,
                }
            }
            async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    self.entered.notify_one();
                    // Hold the turn open until the test says go; the bound
                    // only keeps a broken run from hanging forever.
                    let _ = tokio::time::timeout(Duration::from_secs(60), self.release.notified())
                        .await;
                }
                Ok(ModelOutput {
                    content: format!("[scripted] turn {call} done"),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let model = Arc::new(GatedModel {
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            calls: AtomicUsize::new(0),
        });
        let (composed, interactive, checkpoint_dir) =
            tui_compose(&root, &[], model.clone(), None).await.unwrap();
        let mut ui_events = composed.subscribe();
        let mut orch_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = CaptureSink {
            captured: captured.clone(),
        };
        let entered = model.entered.clone();
        let release = model.release.clone();
        let orch_captured = captured.clone();
        let orch = tokio::spawn(async move {
            send_line(&key_tx, "/work first goal").await;
            entered.notified().await;
            // Mid-turn: the runtime is busy. This input must be queued.
            send_line(&key_tx, "second ask").await;
            wait_for_line(&orch_captured, "input queued", "the queued disposition").await;
            release.notify_one();
            for _ in 0..2 {
                wait_for_event(&mut orch_events, "a TurnCompleted", |event| {
                    matches!(event, RuntimeEvent::TurnCompleted)
                })
                .await;
            }
            wait_for_line(
                &orch_captured,
                "queued input applied",
                "the applied disposition",
            )
            .await;
            quit(&key_tx);
        });

        let mut source = KeySource {
            rx: key_rx,
            disconnected: false,
        };
        let mut sink = sink;
        let session = tokio::time::timeout(
            Duration::from_secs(90),
            run_session(
                &mut source,
                &mut sink,
                composed.handle().clone(),
                &composed.instance,
                &mut ui_events,
                Some(interactive),
                "dynamic",
                checkpoint_dir,
                "serving: scripted e2e model".to_string(),
                None,
                false,
            ),
        )
        .await;
        assert!(session.is_ok(), "the session hung: {session:?}");
        session.expect("timeout").expect("session error");
        orch.await.expect("orchestrator task");
        composed.shutdown().await.unwrap();

        assert!(
            model.calls.load(Ordering::SeqCst) >= 2,
            "the queued input must run as its own turn"
        );
        let seen = transcript(&captured);
        assert!(!seen.contains("input not accepted"), "{seen}");
    }

    /// Budget stop → /checkpoint → /restore → /continue, all inside one
    /// interactive session: the second segment lands after the restore.
    /// Replaces the manual M16-01 "continue after restore" walkthrough.
    #[tokio::test]
    async fn tui_e2e_budget_checkpoint_restore_continue_lands_the_next_segment() {
        struct TwoFileModel {
            step: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ModelTransport for TwoFileModel {
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities {
                    streaming: true,
                    tool_calls: true,
                    max_output_tokens: 4096,
                    context_window: None,
                }
            }
            async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
                let step = self.step.fetch_add(1, Ordering::SeqCst);
                let path = if step == 0 {
                    "file_a.txt"
                } else {
                    "file_b.txt"
                };
                Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("write-{step}"),
                        name: "fs.write".into(),
                        arguments: serde_json::json!({
                            "path": path,
                            "content": format!("segment {} content", step + 1),
                        }),
                    }],
                    usage: Default::default(),
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let model = Arc::new(TwoFileModel {
            step: AtomicUsize::new(0),
        });
        let grants = [write_grant("file_a.txt"), write_grant("file_b.txt")];
        let (composed, interactive, checkpoint_dir) =
            tui_compose(&root, &grants, model.clone(), Some(1))
                .await
                .unwrap();
        let mut ui_events = composed.subscribe();
        let mut orch_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = CaptureSink {
            captured: captured.clone(),
        };
        let orch_root = root.clone();
        let orch_checkpoint_dir = checkpoint_dir.clone();
        let orch_captured = captured.clone();
        let orch = tokio::spawn(async move {
            send_line(&key_tx, "/work write both files").await;
            wait_for_event(&mut orch_events, "the round-budget failure", |event| {
                matches!(
                    event,
                    RuntimeEvent::Failure {
                        class: RuntimeFailureClass::RoundBudget,
                        ..
                    }
                )
            })
            .await;
            // A round-budget stop is a deliberate recoverable stop: the
            // turn is dropped without a TurnCompleted. The operator-real
            // way to know the runtime is idle again is that /checkpoint
            // (an idle-only command) stops refusing — retry until it
            // lands.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the runtime never went idle after the budget stop"
                );
                send_line(&key_tx, "/checkpoint").await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let landed = {
                    let guard = orch_captured.lock().unwrap();
                    guard.iter().any(|frame| frame.contains("checkpoint saved"))
                };
                if landed {
                    break;
                }
            }
            // Restore through the same in-session command the operator
            // types; the target is what cold-start discovery would pick.
            let restore_target =
                resolve_latest_checkpoint(&orch_checkpoint_dir).expect("a saved checkpoint");
            send_line(&key_tx, &format!("/restore {}", restore_target.display())).await;
            wait_for_line(&orch_captured, "runtime restored", "/restore").await;
            send_line(&key_tx, "/continue").await;
            wait_until_file(
                &orch_root,
                "file_b.txt",
                "segment 2 content",
                "/continue after restore",
            )
            .await;
            quit(&key_tx);
        });

        let mut source = KeySource {
            rx: key_rx,
            disconnected: false,
        };
        let mut sink = sink;
        let session = tokio::time::timeout(
            Duration::from_secs(90),
            run_session(
                &mut source,
                &mut sink,
                composed.handle().clone(),
                &composed.instance,
                &mut ui_events,
                Some(interactive),
                "dynamic",
                checkpoint_dir,
                "serving: scripted e2e model".to_string(),
                None,
                false,
            ),
        )
        .await;
        assert!(session.is_ok(), "the session hung: {session:?}");
        session.expect("timeout").expect("session error");
        orch.await.expect("orchestrator task");
        composed.shutdown().await.unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("file_a.txt")).unwrap(),
            "segment 1 content",
            "the pre-checkpoint segment must survive"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("file_b.txt")).unwrap(),
            "segment 2 content",
            "/continue after /restore must land the next segment"
        );
    }

    /// The dual-workspace review walkthrough on the interactive path: a
    /// workspace holds the user's own file and a planted bug; the agent
    /// fixes only the bug; /review renders the result card attributing
    /// the agent's write and never claiming the user's file. Replaces the
    /// manual M16-05 dual-workspace walkthrough (the headless variant
    /// lives in the cli.rs tests).
    #[tokio::test]
    async fn tui_e2e_review_attributes_the_agents_change_not_the_users() {
        struct FixModel;
        #[async_trait::async_trait]
        impl ModelTransport for FixModel {
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities {
                    streaming: true,
                    tool_calls: true,
                    max_output_tokens: 4096,
                    context_window: None,
                }
            }
            async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
                let has_tool_result = request
                    .messages
                    .iter()
                    .any(|message| message.role == ModelRole::Tool);
                if !has_tool_result {
                    return Ok(ModelOutput {
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "fix-read".into(),
                            name: "fs.read".into(),
                            arguments: serde_json::json!({ "path": "service.txt" }),
                        }],
                        usage: Default::default(),
                    });
                }
                let read_the_file = request.messages.iter().any(|message| {
                    message.role == ModelRole::Tool && message.content.contains("mode=broken")
                });
                if read_the_file {
                    return Ok(ModelOutput {
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "fix-write".into(),
                            name: "fs.write".into(),
                            arguments: serde_json::json!({
                                "path": "service.txt",
                                "content": "mode=fixed\n",
                            }),
                        }],
                        usage: Default::default(),
                    });
                }
                Ok(ModelOutput {
                    content: "[scripted] fix delivered".into(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::fs::write(root.join("config.txt"), "user_setting=keep\n").unwrap();
        std::fs::write(root.join("service.txt"), "mode=broken\n").unwrap();
        let grants = [write_grant("service.txt")];
        let (composed, interactive, checkpoint_dir) =
            tui_compose(&root, &grants, Arc::new(FixModel), None)
                .await
                .unwrap();
        let mut ui_events = composed.subscribe();
        let mut orch_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = CaptureSink {
            captured: captured.clone(),
        };
        let orch_captured = captured.clone();
        let orch = tokio::spawn(async move {
            send_line(&key_tx, "/work fix the mode bug in service.txt").await;
            wait_for_event(&mut orch_events, "the fix turn's TurnCompleted", |event| {
                matches!(event, RuntimeEvent::TurnCompleted)
            })
            .await;
            send_line(&key_tx, "/review").await;
            // The card must attribute the agent's write...
            wait_for_line(
                &orch_captured,
                "changed files (this session's tools only):",
                "/review card",
            )
            .await;
            wait_for_line(
                &orch_captured,
                "service.txt",
                "the agent's write in the card",
            )
            .await;
            quit(&key_tx);
        });

        let mut source = KeySource {
            rx: key_rx,
            disconnected: false,
        };
        let mut sink = sink;
        let session = tokio::time::timeout(
            Duration::from_secs(90),
            run_session(
                &mut source,
                &mut sink,
                composed.handle().clone(),
                &composed.instance,
                &mut ui_events,
                Some(interactive),
                "dynamic",
                checkpoint_dir,
                "serving: scripted e2e model".to_string(),
                None,
                false,
            ),
        )
        .await;
        assert!(session.is_ok(), "the session hung: {session:?}");
        session.expect("timeout").expect("session error");
        orch.await.expect("orchestrator task");
        composed.shutdown().await.unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("service.txt")).unwrap(),
            "mode=fixed\n",
            "the fix must land"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("config.txt")).unwrap(),
            "user_setting=keep\n",
            "the user's own file must not be touched"
        );
        // The card renders the awaiting-review state and scopes its change
        // list to this session's tools: the user's own file is not claimed.
        let seen = transcript(&captured);
        let card_start = seen.find("review:").expect("the review header");
        let card = &seen[card_start..];
        assert!(
            card.contains("awaiting operator review"),
            "the card names the closure state: {card}"
        );
        assert!(
            card.contains("service.txt"),
            "the agent's change is attributed: {card}"
        );
        assert!(
            !card.contains("config.txt"),
            "the user's own file must not be claimed by the card: {card}"
        );
    }
}
