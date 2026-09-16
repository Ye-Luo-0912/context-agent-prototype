//! The interactive TUI session: one loop that folds runtime events into
//! [`AppState`], draws frames, reads keys and dispatches the product
//! commands. The loop is split from the terminal behind [`UiSource`] and
//! [`UiSink`] so the exact session path — key input, command dispatch,
//! real runtime handle, event folding — can be driven end-to-end in tests
//! with scripted keys and a capture sink, no pty and no manual operator.

use std::{io, path::PathBuf, sync::Arc, time::Duration};

use agent_contracts::{ApprovalDecision, RuntimeEventEnvelope, TaskId};
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

/// Rows a PageUp/PageDown moves the scrollable approval panel by.
const APPROVAL_PAGE: u16 = 8;

/// Map a key to the approval decision it triggers, or `None` for any key that
/// is not an approval answer (scroll keys are routed by the caller). Kept as
/// a pure function so the routing is unit-testable without a live session.
pub(crate) fn classify_approval_key(code: KeyCode) -> Option<ApprovalDecision> {
    match code {
        KeyCode::Char('y') | KeyCode::Enter => Some(ApprovalDecision::Allow),
        KeyCode::Char('n') | KeyCode::Esc => Some(ApprovalDecision::Deny),
        _ => None,
    }
}

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

/// Bounded queue for commands that act on the runtime's task identity or
/// touch the disk. ONE worker drains it in submission order, so the order
/// the operator typed the commands is the order the runtime receives them.
/// The previous code spawned a detached task per command, which only ever
/// guaranteed arrival order — `/task B` followed by `/continue` could reach
/// the actor in the opposite order and continue the task that happened to
/// be active.
const COMMAND_QUEUE_CAP: usize = 32;

/// How many runtime events one frame may fold before yielding to the
/// keyboard. A flood of events must not starve input.
const DRAIN_BUDGET_PER_FRAME: usize = 2048;

/// A command whose effect depends on the runtime's task identity, or that
/// does slow storage I/O. Submitted in typing order and executed off the
/// draw loop, so a slow disk never freezes the operator's input.
#[derive(Debug)]
enum SessionCommand {
    Work {
        goal: String,
    },
    Focus {
        goal: String,
    },
    Activate {
        task_id: TaskId,
    },
    Suspend {
        observed: Option<TaskId>,
    },
    /// Continue exactly the task the operator observed. A mismatch starts
    /// no turn and is reported naming the live task.
    Continue {
        observed: Option<TaskId>,
    },
    /// Close the observed task. `CompleteTask` has no identity-expecting
    /// variant yet, so this is a checked refusal, not an atomic guarantee:
    /// the worker compares a fresh status snapshot and refuses on mismatch.
    Done {
        summary: String,
        observed: Option<TaskId>,
    },
    Checkpoint,
    Restore {
        target: String,
    },
    /// R6: ordinary user text. It has user-meaningful order like any other
    /// command, so it shares the lane instead of racing it.
    Input {
        text: String,
    },
}

/// A view fact a command resolved off the draw loop. The loop applies it, so
/// the panel still shows e.g. the checkpoint that was just saved even though
/// the slow work did not happen on the draw thread.
#[derive(Debug)]
enum ViewFact {
    CheckpointSaved { artifact: String },
}

/// One submitted command plus its receipt identity. The id ties the channel
/// item to its [`CommandLedger`] entry, so the worker can move exactly that
/// entry through its lifecycle.
#[derive(Debug)]
struct QueuedCommand {
    id: u64,
    command: SessionCommand,
}

/// Lifecycle of one submitted command as the shutdown receipt reports it.
/// This — not any counter — is the authority on what reached the Runtime
/// (O3): a pending count is diagnostic and can never prove "not executed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandStatus {
    /// Accepted by the lane, never dequeued by the worker: it never reached
    /// the Runtime.
    Queued,
    /// Dequeued by the worker; its Runtime interaction has not completed.
    /// After a stop this is reported as `result unknown` — never as "not
    /// executed" and never as a rollback.
    Taken,
}

/// The worker's shutdown receipt: every submission still on the books,
/// separated into what never reached the Runtime and what was in flight when
/// the session stopped waiting.
#[derive(Debug, Default, PartialEq, Eq)]
struct WorkerReceipt {
    /// Commands the worker never dequeued: they never reached the Runtime.
    queued_never_dispatched: usize,
    /// Commands dequeued but not settled: their Runtime effect may or may
    /// not have landed. Honest label: unknown.
    taken_result_unknown: usize,
}

/// Shared, exactly-transitioned state of every submitted command. Entries
/// are registered BEFORE the command enters the channel and removed only
/// when the worker settles them, so the receipt after shutdown is exact:
/// whatever remains is either still-queued or in-flight-at-stop.
///
/// The diagnostic pending count derives from the same entries, which closes
/// the O3 window: the old scheme incremented after `try_send` and
/// decremented after `recv`, so a reader could briefly observe a count that
/// did not correspond to any queue state (down to an underflow), and the
/// exit path read the counter while the worker was still dequeuing.
#[derive(Default)]
struct CommandLedger {
    /// The stop barrier. Once set, the worker dequeues nothing new.
    stop: std::sync::atomic::AtomicBool,
    next_id: std::sync::atomic::AtomicU64,
    entries: std::sync::Mutex<Vec<(u64, CommandStatus)>>,
}

impl CommandLedger {
    fn stop_accepting(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_stopped(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Register a submission before it enters the channel. Every command in
    /// the channel therefore always has a ledger entry.
    fn register(&self) -> u64 {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entries
            .lock()
            .expect("the ledger mutex is never held across an await")
            .push((id, CommandStatus::Queued));
        id
    }

    /// Remove the entry of a submission the channel refused (full or closed):
    /// it never became queue state.
    fn retract(&self, id: u64) {
        self.entries
            .lock()
            .expect("the ledger mutex is never held across an await")
            .retain(|(entry_id, _)| *entry_id != id);
    }

    /// The worker dequeued this command. Called synchronously right after
    /// `recv` resolves — before any await — so an abort can never leave a
    /// taken command recorded as queued.
    fn mark_taken(&self, id: u64) {
        let mut entries = self
            .entries
            .lock()
            .expect("the ledger mutex is never held across an await");
        for (entry_id, status) in entries.iter_mut() {
            if *entry_id == id {
                *status = CommandStatus::Taken;
            }
        }
    }

    /// The worker completed this command's Runtime interaction (its outcome
    /// went to the notice channel, Ok or Err alike); it is off the books.
    fn settle(&self, id: u64) {
        self.entries
            .lock()
            .expect("the ledger mutex is never held across an await")
            .retain(|(entry_id, _)| *entry_id != id);
    }

    /// Diagnostic: how many submissions have not started executing. May be
    /// read at any time; it carries no "did it reach the Runtime" meaning.
    fn queued(&self) -> usize {
        self.entries
            .lock()
            .expect("the ledger mutex is never held across an await")
            .iter()
            .filter(|(_, status)| *status == CommandStatus::Queued)
            .count()
    }

    /// The shutdown receipt: the only semantic answer to "what happened to
    /// the commands the operator typed".
    fn receipt(&self) -> WorkerReceipt {
        let entries = self
            .entries
            .lock()
            .expect("the ledger mutex is never held across an await");
        WorkerReceipt {
            queued_never_dispatched: entries
                .iter()
                .filter(|(_, status)| *status == CommandStatus::Queued)
                .count(),
            taken_result_unknown: entries
                .iter()
                .filter(|(_, status)| *status == CommandStatus::Taken)
                .count(),
        }
    }
}

/// Execute queued commands in submission order. Runs for the life of the
/// session; the sender being dropped ends it, and the session's stop barrier
/// ends it sooner. Every taken command is marked on the ledger before the
/// first await, so the shutdown receipt can never misreport a taken command
/// as queued.
async fn run_command_worker(
    mut commands: tokio::sync::mpsc::Receiver<QueuedCommand>,
    handle: RuntimeHandle,
    plane: std::sync::Arc<agent_runtime::RuntimeCheckpointPlane>,
    notice_tx: tokio::sync::mpsc::Sender<String>,
    view_tx: tokio::sync::mpsc::Sender<ViewFact>,
    checkpoint_dir: PathBuf,
    ledger: std::sync::Arc<CommandLedger>,
) {
    use agent_runtime::{ContinueOutcome, SuspendOutcome};

    loop {
        // The stop barrier: once the session stops accepting, nothing
        // further is dequeued, let alone dispatched.
        if ledger.is_stopped() {
            break;
        }
        let Some(queued) = commands.recv().await else {
            break;
        };
        // This submission is now executing, not queued. Marked before any
        // await: tokio cancellation can only land at an await point, so an
        // abort mid-command always finds the entry marked Taken.
        ledger.mark_taken(queued.id);
        match queued.command {
            SessionCommand::Work { goal } => {
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
            }
            SessionCommand::Focus { goal } => {
                if let Err(error) = handle.set_focus(goal).await {
                    let _ = notice_tx.try_send(format!("focus failed: {error}"));
                }
            }
            SessionCommand::Activate { task_id } => {
                if let Err(error) = handle.activate_task(task_id).await {
                    let _ = notice_tx.try_send(format!("task failed: {error}"));
                }
            }
            SessionCommand::Suspend { observed } => {
                match handle.suspend_task_expecting(observed).await {
                    Ok(SuspendOutcome::Suspended { .. }) => {}
                    Ok(other) => {
                        let _ = notice_tx.try_send(format!(
                            "suspend refused: the runtime is not on the task you observed ({other:?})"
                        ));
                    }
                    Err(error) => {
                        let _ = notice_tx.try_send(format!("suspend failed: {error}"));
                    }
                }
            }
            SessionCommand::Continue { observed } => {
                match handle.continue_active_task_expecting(observed).await {
                    Ok(ContinueOutcome::Continued { task_id }) => {
                        let _ = notice_tx.try_send(format!("continuing task {task_id}"));
                    }
                    Ok(ContinueOutcome::ExpectedTaskMismatch { active_task_id }) => {
                        // No turn was started. Name what is actually live so
                        // the operator can re-target instead of silently
                        // continuing someone else's work.
                        let _ = notice_tx.try_send(identity_mismatch_notice(
                            "continue",
                            observed,
                            active_task_id,
                            "no turn started",
                        ));
                    }
                    Err(error) => {
                        let _ = notice_tx.try_send(format!("continue failed: {error}"));
                    }
                }
            }
            SessionCommand::Done { summary, observed } => {
                // `CompleteTask` carries no expected identity in the shared
                // contract, so this is a fresh-snapshot check immediately
                // before the call — it narrows the window, it does not close
                // it. Refusing is the safe direction: never close a task the
                // operator was not looking at.
                match handle.status_snapshot().await {
                    Ok(snapshot) if observed.is_some() && snapshot.focus_task_id != observed => {
                        let _ = notice_tx.try_send(identity_mismatch_notice(
                            "done",
                            observed,
                            snapshot.focus_task_id,
                            "nothing was closed",
                        ));
                    }
                    Ok(_) => {
                        if let Err(error) = handle.complete_current_task(summary).await {
                            let _ = notice_tx.try_send(format!("done failed: {error}"));
                        }
                    }
                    Err(error) => {
                        let _ = notice_tx.try_send(format!("done failed: {error}"));
                    }
                }
            }
            SessionCommand::Checkpoint => {
                // The runtime snapshot and the atomic store write are both
                // off the draw loop: the operator's keyboard stays live while
                // a large checkpoint is written.
                let store = CheckpointStore::new(checkpoint_dir.clone());
                match plane.capture().await {
                    Ok(checkpoint) => {
                        let tasks = checkpoint.tasks.tasks.len();
                        match serde_json::to_vec(&checkpoint) {
                            Ok(bytes) => match store.write_atomic(&bytes).await {
                                Ok(stored) => {
                                    let line = format!(
                                        "checkpoint saved ({tasks} tasks): {}",
                                        stored.artifact
                                    );
                                    let _ = view_tx
                                        .try_send(ViewFact::CheckpointSaved {
                                            artifact: stored.artifact.clone(),
                                        })
                                        .map_err(|_| ());
                                    let _ = notice_tx.try_send(line);
                                }
                                Err(error) => {
                                    let _ = notice_tx
                                        .try_send(format!("checkpoint write failed: {error}"));
                                }
                            },
                            Err(error) => {
                                let _ = notice_tx
                                    .try_send(format!("checkpoint serialize failed: {error}"));
                            }
                        }
                    }
                    Err(error) => {
                        let _ = notice_tx.try_send(format!("checkpoint failed: {error}"));
                    }
                }
            }
            SessionCommand::Restore { target } => {
                let path = resolve_restore_target(&checkpoint_dir, target.trim());
                let result = async {
                    let bytes = read_checkpoint_bounded(&path).await?;
                    let checkpoint = decode_checkpoint_bytes(&bytes)
                        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
                    plane.restore(checkpoint).await.map_err(anyhow::Error::from)
                }
                .await;
                match result {
                    Ok(()) => {
                        // Keep the operator-visible wording the product
                        // already documents.
                        let _ = notice_tx.try_send(
                            "runtime restored; /continue resumes the active task".to_string(),
                        );
                    }
                    Err(error) => {
                        let _ = notice_tx.try_send(format!("restore failed: {error}"));
                    }
                }
            }
            SessionCommand::Input { text } => {
                // R6: ordinary text rides the same lane, so a correction
                // typed after `/task B` cannot overtake it.
                if let Err(error) = handle.user_message(text).await {
                    let _ = notice_tx.try_send(format!("input not accepted: {error}"));
                }
            }
        }
        // The Runtime interaction for this command completed — its outcome
        // went to the notice channel, Ok or Err alike. Settled either way.
        ledger.settle(queued.id);
    }
}

/// The refusal notice for an identity-checked command that named a task the
/// runtime is not on. Names both sides so the operator can re-target instead
/// of assuming the command took effect. Pure so the wording is testable
/// without a live runtime.
fn identity_mismatch_notice(
    command: &str,
    observed: Option<TaskId>,
    live: Option<TaskId>,
    effect: &str,
) -> String {
    fn name(task_id: Option<TaskId>) -> String {
        task_id.map_or_else(|| "no task".to_string(), |task_id| task_id.to_string())
    }
    format!(
        "{command} refused: you observed {}, the live task is {} — {effect}",
        name(observed),
        name(live)
    )
}

/// The ordered command lane: one bounded queue, one consumer, plus the
/// shared [`CommandLedger`] the worker keeps exact. The session owns this
/// value's lifetime so it can stop accepting work and — through the worker's
/// receipt — name what never ran and what was still in flight, instead of
/// dropping the sender and assuming the queue was cancelled. `queued()` is a
/// diagnostic only; the receipt is the semantic authority.
#[derive(Clone)]
pub(crate) struct CommandLane {
    tx: tokio::sync::mpsc::Sender<QueuedCommand>,
    ledger: std::sync::Arc<CommandLedger>,
}

impl CommandLane {
    /// Submit one command, in order, or say why it was not accepted. A full
    /// lane is reported rather than silently dropped: the operator must know
    /// that a typed action did not reach the runtime. The ledger entry is
    /// registered before the send and retracted if the send is refused, so
    /// the count can never underflow (O3).
    fn submit(&self, app: &mut AppState, command: SessionCommand) -> bool {
        let shown = format!("{command:?}");
        let id = self.ledger.register();
        match self.tx.try_send(QueuedCommand { id, command }) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.ledger.retract(id);
                // The pending count here is diagnostic: it sizes the backlog
                // for the operator; the shutdown receipt, not this number,
                // decides what "never executed" means.
                app.push_system(format!(
                    "command queue is full ({COMMAND_QUEUE_CAP}, {pending} pending); {shown} was NOT submitted — wait for the pending commands to finish",
                    pending = self.pending()
                ));
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.ledger.retract(id);
                app.push_system("command worker is gone; the command was not submitted".into());
                false
            }
        }
    }

    /// How many submissions have not started executing. Diagnostic only —
    /// may briefly include a command the worker is taking right now, and
    /// never proves anything about what reached the runtime.
    fn pending(&self) -> usize {
        self.ledger.queued()
    }
}

/// Submit one command through the lane. Thin wrapper so dispatch sites read
/// the same as before the lane carried a pending counter.
fn submit_command(lane: &CommandLane, app: &mut AppState, command: SessionCommand) -> bool {
    lane.submit(app, command)
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
    // U6/R6: one bounded, ordered lane for everything with user-meaningful
    // order — task identity commands, the slow storage commands, AND ordinary
    // text. A single worker drains it in submission order, so the operator's
    // typing order is the order the runtime receives.
    let (command_tx, command_rx) = tokio::sync::mpsc::channel::<QueuedCommand>(COMMAND_QUEUE_CAP);
    let (view_tx, mut view_rx) = tokio::sync::mpsc::channel::<ViewFact>(8);
    let command_ledger = std::sync::Arc::new(CommandLedger::default());
    let lane = CommandLane {
        tx: command_tx,
        ledger: std::sync::Arc::clone(&command_ledger),
    };
    // The session owns the worker's handle: on EVERY exit it stops accepting,
    // raises the worker's stop barrier, reclaims the task and reads the
    // worker's receipt, instead of dropping the sender and assuming the
    // queue was cancelled.
    let command_worker = tokio::spawn(run_command_worker(
        command_rx,
        handle.clone(),
        runtime.checkpoint_plane(),
        notice_tx.clone(),
        view_tx,
        checkpoint_dir.clone(),
        std::sync::Arc::clone(&command_ledger),
    ));

    // Q6: the loop no longer exits through `?`. Whatever ends it — Ctrl-C,
    // /quit, a draw failure, a key failure or a dispatch failure — is
    // captured here and runs into the one shared cleanup below.
    let mut outcome: anyhow::Result<()> = Ok(());

    loop {
        let traces_dir = checkpoint_dir
            .parent()
            .map(|state_dir| state_dir.join("traces"))
            .unwrap_or_else(|| checkpoint_dir.clone());
        // Bounded per-frame drain: a flood of events must not starve input.
        let mut drained = 0usize;
        while drained < DRAIN_BUDGET_PER_FRAME {
            match runtime_events.try_recv() {
                Ok(event) => {
                    app.apply_runtime_event(event);
                    drained += 1;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    // A Lagged receiver dropped events it never saw. Hide
                    // nothing: name the loss, rebuild the view from the
                    // durable journal (current run only), and continue. The
                    // rebuild marks itself PARTIAL when it cannot verify a
                    // contiguous prefix, and /status says so too.
                    let (folded, partial) = app.resync_projection(&traces_dir).await;
                    app.push_system(format!(
                        "warning: the UI fell behind and dropped {skipped} runtime events; the view was resynced from the journal ({folded} events folded){}",
                        if partial {
                            format!(
                                " — PARTIAL: {}",
                                app.view_partial_reason
                                    .as_deref()
                                    .unwrap_or("the replay could not be verified")
                            )
                        } else {
                            String::new()
                        }
                    ));
                    drained += 1;
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
        while let Ok(fact) = view_rx.try_recv() {
            // View facts resolved by an off-loop command task, applied here
            // so the panel still reflects them.
            match fact {
                ViewFact::CheckpointSaved { artifact } => app.last_checkpoint = Some(artifact),
            }
        }

        if let Err(error) = sink.draw(&app) {
            outcome = Err(anyhow::Error::new(error).context("drawing the session frame failed"));
            break;
        }

        let key = match source.poll_key(Duration::from_millis(30)).await {
            Ok(key) => key,
            Err(error) => {
                outcome =
                    Err(anyhow::Error::new(error).context("reading the operator's key failed"));
                break;
            }
        };
        let Some(key) = key else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break;
        }

        // While a write/process tool waits for permission, y/n (or
        // Enter/Esc) resolve the prompt; PageUp/PageDown scroll the approval
        // detail so the operator can read the whole request before answering;
        // anything else is ignored. The decision is bound to the request_id
        // currently on screen, so a stale key for an already-resolved request
        // can never approve a newer one that arrives in the meantime.
        if app.pending_approval.is_some() {
            let Some(handle) = &interactive else {
                app.clear_approval();
                continue;
            };
            match key.code {
                KeyCode::PageUp => {
                    app.approval_scroll = app.approval_scroll.saturating_sub(APPROVAL_PAGE);
                    continue;
                }
                KeyCode::PageDown => {
                    app.approval_scroll = app.approval_scroll.saturating_add(APPROVAL_PAGE);
                    continue;
                }
                _ => {}
            }
            let decision = classify_approval_key(key.code);
            if let Some(decision) = decision {
                // Bind the answer to the request_id shown right now.
                let request_id = app
                    .pending_approval
                    .as_ref()
                    .map(|p| p.request_id.clone())
                    .unwrap_or_default();
                let granted = handle.gate.respond(&request_id, decision).await;
                // Only drop the displayed request if it is the one we just
                // answered; a request that arrived in the meantime stays put
                // for the operator to review on its own.
                if app
                    .pending_approval
                    .as_ref()
                    .is_some_and(|p| p.request_id == request_id)
                {
                    app.clear_approval();
                }
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
                    interactive.as_ref(),
                    &notice_tx,
                    &lane,
                    &checkpoint_dir,
                )
                .await;
                match keep_running {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
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

    // Q6: one cleanup entry for EVERY exit — normal quit, draw failure, key
    // failure, dispatch failure. Order and responsibilities:
    //   1. stop accepting new commands (the loop above was the only
    //      submitter);
    //   2. raise the worker's stop barrier — from here on the worker
    //      dequeues nothing, so no queued command is dispatched behind the
    //      session's back;
    //   3. reclaim the worker with abort+join. The barrier already stopped
    //      dequeues; abort only bounds an in-flight slow command (the
    //      established R6/A3 semantics: the operator asked to leave, and the
    //      runtime's own ordered shutdown settles task state). The receipt —
    //      not the abort — says what happened: abort is never reported as
    //      "not executed" and never as a rollback;
    //   4. read the worker's receipt and name both what never reached the
    //      runtime and what stays with an unknown result.
    // The terminal guard and the Runtime shutdown remain the caller's two
    // separate responsibilities (U5), settled after this returns.
    drop(lane);
    command_ledger.stop_accepting();
    command_worker.abort();
    let joined = command_worker.await;
    let receipt = command_ledger.receipt();
    if receipt.queued_never_dispatched > 0 {
        let line = format!(
            "{count} queued command(s) never executed — the session ended first",
            count = receipt.queued_never_dispatched
        );
        // Best-effort transcript record; the final frame below draws it
        // before the terminal is restored.
        app.push_system(line.clone());
        eprintln!("warning: {line}");
    }
    if receipt.taken_result_unknown > 0 {
        let line = format!(
            "{count} command(s) were still executing when the session ended; their runtime result is unknown — this is not a rollback",
            count = receipt.taken_result_unknown
        );
        app.push_system(line.clone());
        eprintln!("warning: {line}");
    }
    if let Err(join_error) = joined
        && !join_error.is_cancelled()
    {
        // A cancelled abort is the expected stop; a panic in the worker
        // is a real failure and must not be swallowed.
        let message = format!("command worker failed: {join_error}");
        app.push_system(message.clone());
        eprintln!("warning: {message}");
        if outcome.is_ok() {
            outcome = Err(anyhow::anyhow!(message));
        }
    }
    // One final frame so the receipt is actually drawn before the terminal
    // is restored. Best-effort: a sink that just failed stays failed.
    let _ = sink.draw(&app);
    outcome
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
    interactive: Option<&InteractiveHandle>,
    notice_tx: &tokio::sync::mpsc::Sender<String>,
    commands: &CommandLane,
    checkpoint_dir: &std::path::Path,
) -> anyhow::Result<bool> {
    if trimmed == "/quit" {
        return Ok(false);
    }
    // U6: the task an identity-checked command should act on, as the
    // operator last observed it.
    let observed = app.observed_task;
    if let Some(goal) = trimmed.strip_prefix("/focus ") {
        submit_command(
            commands,
            app,
            SessionCommand::Focus {
                goal: goal.trim().to_string(),
            },
        );
        return Ok(true);
    }
    if let Some(id_text) = trimmed.strip_prefix("/task ") {
        // Activate an existing task by id (resume its scopes). Task ids
        // come from `/tasks`; activation alone does not start a turn —
        // `/continue` does.
        match id_text.trim().parse::<agent_contracts::TaskId>() {
            Ok(task_id) => {
                submit_command(commands, app, SessionCommand::Activate { task_id });
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
        // orchestrator. Queued so it cannot overtake a task command the
        // operator typed first.
        submit_command(commands, app, SessionCommand::Work { goal });
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
        // Render the result card: the focused task's own material when it
        // has any, otherwise the most recently finished task's. Never a
        // blend of two tasks — a card belongs to exactly one TaskId, so
        // task A's completion header cannot sit above task B's changes.
        // Display only — no model call, no write.
        if let Some(card) = app.review_card() {
            let card = card.clone();
            for line in crate::state::format_result_lines(&card) {
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
        submit_command(commands, app, SessionCommand::Suspend { observed });
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
        // Queued for ordering and checked against the observed task in the
        // worker (CompleteTask has no identity-expecting variant yet).
        submit_command(
            commands,
            app,
            SessionCommand::Done {
                summary: summary.trim().to_string(),
                observed,
            },
        );
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
        // Queued in typing order and executed off the draw loop: the manual
        // save rides the same atomic envelope store as the automatic safe
        // points (one format, one retention domain, checksum verified on
        // load), and a slow disk no longer freezes the operator's keyboard.
        submit_command(commands, app, SessionCommand::Checkpoint);
        return Ok(true);
    }
    if let Some(restore_target) = trimmed.strip_prefix("/restore ") {
        submit_command(
            commands,
            app,
            SessionCommand::Restore {
                target: restore_target.trim().to_string(),
            },
        );
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
        //
        // U6: submitted in typing order and identity-checked against the
        // task the operator observed, so `/task B` immediately followed by
        // `/continue` cannot continue whatever happened to be active.
        submit_command(commands, app, SessionCommand::Continue { observed });
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
    //
    // R6: it goes through the SAME bounded ordered lane as the task
    // commands, because a correction typed after `/task B` is meaningful
    // only if it reaches the runtime after `/task B` does. A detached
    // spawn here could overtake the queue while the worker waited on a
    // slow checkpoint, delivering the correction to the *previous* task.
    submit_command(
        commands,
        app,
        SessionCommand::Input {
            text: trimmed.to_string(),
        },
    );
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
/// EXEC-4 (E08): one open handle, cap+1 read — an oversized or grown file
/// is refused before its bytes land in memory, and reading from the handle
/// makes a post-stat size change irrelevant. A legal file reads
/// byte-for-byte exactly as before.
async fn read_checkpoint_bounded(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    file.take(agent_runtime::MAX_CHECKPOINT_ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > agent_runtime::MAX_CHECKPOINT_ARTIFACT_BYTES as u64 {
        anyhow::bail!(
            "{} exceeds the checkpoint artifact bound ({})",
            path.display(),
            agent_runtime::MAX_CHECKPOINT_ARTIFACT_BYTES
        );
    }
    Ok(bytes)
}

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

    /// EXEC-4 residual (R2-12): the startup entries (`--restore` and
    /// latest) inherit the shared bounded read — a file past the artifact
    /// bound is refused by name at load, never buffered whole and never
    /// misreported as a parse failure.
    #[test]
    fn startup_restore_refuses_an_oversized_file_at_the_shared_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(agent_runtime::MAX_CHECKPOINT_ARTIFACT_BYTES as u64 + 1)
            .unwrap();
        drop(file);
        let error = load_runtime_checkpoint(&path).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("exceeds the checkpoint artifact bound"),
            "the startup path must name the bound at read time, got: {message}"
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
        maintenance_budget_from_env, try_maintenance_transport_from_env,
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
            try_maintenance_transport_from_env()?,
            &maintenance_budget_from_env()?,
            None,
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
            cache_routing: None,
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

    /// Q6 worker-lifecycle harness pieces. The session here is driven with a
    /// fully pre-buffered key script whose source fails once the script is
    /// exhausted (or a sink that fails on a marker input line). On the
    /// current-thread test executor every scripted key poll resolves
    /// immediately, so the session loop reaches its `?` early exit without
    /// ever yielding: the command worker provably could not have taken any
    /// submitted command. Whatever reaches the runtime afterwards is
    /// therefore work a leaking worker dispatched after the session ended.
    struct ScriptedErrorSource {
        rx: tokio::sync::mpsc::Receiver<KeyEvent>,
    }

    impl UiSource for ScriptedErrorSource {
        async fn poll_key(&mut self, _timeout: Duration) -> io::Result<Option<KeyEvent>> {
            match self.rx.recv().await {
                Some(key) => Ok(Some(key)),
                None => Err(io::Error::other("injected key read failure")),
            }
        }
    }

    /// Renders normally until the operator's input buffer holds `trigger`,
    /// then fails — the same `?` path a real render failure takes.
    struct FailOnInputSink {
        inner: CaptureSink,
        trigger: &'static str,
    }

    impl UiSink for FailOnInputSink {
        fn draw(&mut self, app: &AppState) -> io::Result<()> {
            if app.input == self.trigger {
                return Err(io::Error::other("injected draw failure"));
            }
            self.inner.draw(app)
        }
    }

    /// A scripted model that counts turn executions: every plain input that
    /// reaches the runtime costs exactly one call.
    struct TurnCountingModel {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl ModelTransport for TurnCountingModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: true,
                tool_calls: true,
                max_output_tokens: 4096,
                context_window: None,
            }
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ModelOutput {
                content: "[scripted] turn done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }

    fn keys(line: &str) -> Vec<KeyEvent> {
        let mut keys: Vec<KeyEvent> = line
            .chars()
            .map(|ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .collect();
        keys.push(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        keys
    }

    /// Give a surviving (detached) worker a fair chance to wrongly dispatch
    /// the queue before asserting it did not.
    async fn drain_executor_for_wrong_dispatch() {
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A non-empty result names the turns that ran after the session ended —
    /// the observable signature of a worker that was never reclaimed.
    fn model_dispatch_report(calls: &AtomicUsize) -> String {
        let count = calls.load(Ordering::SeqCst);
        if count == 0 {
            String::new()
        } else {
            format!("{count} queued input(s) reached the runtime after the session ended")
        }
    }

    /// Q6: a key-read failure exits through `poll_key`'s `?`. The session
    /// must still stop the command worker, keep the queued input out of the
    /// runtime, and name the work that never ran.
    #[tokio::test]
    async fn a_key_read_failure_still_reclaims_the_worker_and_names_undispatched_input() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let (composed, interactive, checkpoint_dir) = tui_compose(
            &root,
            &[],
            Arc::new(TurnCountingModel {
                calls: calls.clone(),
            }),
            None,
        )
        .await
        .unwrap();
        let mut ui_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        for key in keys("hello one")
            .into_iter()
            .chain(keys("hello two"))
            .chain(keys("hello three"))
        {
            key_tx.try_send(key).unwrap();
        }
        drop(key_tx);

        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sink = CaptureSink {
            captured: captured.clone(),
        };
        let mut source = ScriptedErrorSource { rx: key_rx };
        let session = tokio::time::timeout(
            Duration::from_secs(60),
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
        let error = session
            .unwrap()
            .expect_err("the injected key read failure must surface");
        assert!(
            format!("{error:#}").contains("injected key read failure"),
            "the session error must be the injected one, got: {error}"
        );

        drain_executor_for_wrong_dispatch().await;
        assert_eq!(
            model_dispatch_report(&calls),
            "",
            "queued input must not reach the runtime after the session ended"
        );
        let seen = transcript(&captured);
        assert!(
            seen.contains("never executed"),
            "the session must name the work that never ran: {seen}"
        );
        composed.shutdown().await.unwrap();
    }

    /// Q6: a draw failure exits through `sink.draw`'s `?` with the same
    /// unified-cleanup obligations as any other exit.
    #[tokio::test]
    async fn a_draw_failure_still_reclaims_the_worker_and_names_undispatched_input() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let (composed, interactive, checkpoint_dir) = tui_compose(
            &root,
            &[],
            Arc::new(TurnCountingModel {
                calls: calls.clone(),
            }),
            None,
        )
        .await
        .unwrap();
        let mut ui_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        for key in keys("hello one").into_iter().chain(keys("x")) {
            key_tx.try_send(key).unwrap();
        }
        drop(key_tx);

        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sink = FailOnInputSink {
            inner: CaptureSink {
                captured: captured.clone(),
            },
            trigger: "x",
        };
        let mut source = ScriptedErrorSource { rx: key_rx };
        let session = tokio::time::timeout(
            Duration::from_secs(60),
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
        let error = session
            .unwrap()
            .expect_err("the injected draw failure must surface");
        assert!(
            format!("{error:#}").contains("injected draw failure"),
            "the session error must be the injected one, got: {error}"
        );

        drain_executor_for_wrong_dispatch().await;
        assert_eq!(
            model_dispatch_report(&calls),
            "",
            "queued input must not reach the runtime after the session ended"
        );
        composed.shutdown().await.unwrap();
    }

    /// Q6: the normal `/quit` path keeps its cleanup and now draws the
    /// shutdown receipt, so the operator actually sees which typed commands
    /// never ran.
    #[tokio::test]
    async fn a_normal_quit_reports_queued_commands_in_the_final_frame_and_dispatches_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let (composed, interactive, checkpoint_dir) = tui_compose(
            &root,
            &[],
            Arc::new(TurnCountingModel {
                calls: calls.clone(),
            }),
            None,
        )
        .await
        .unwrap();
        let mut ui_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        for key in keys("hello again").into_iter().chain(keys("/quit")) {
            key_tx.try_send(key).unwrap();
        }
        drop(key_tx);

        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sink = CaptureSink {
            captured: captured.clone(),
        };
        let mut source = ScriptedErrorSource { rx: key_rx };
        let session = tokio::time::timeout(
            Duration::from_secs(60),
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
        session.unwrap().expect("a normal quit is not an error");

        let seen = transcript(&captured);
        assert!(
            seen.contains("never executed"),
            "the final frame must report the queued command that never ran: {seen}"
        );

        drain_executor_for_wrong_dispatch().await;
        assert_eq!(
            model_dispatch_report(&calls),
            "",
            "input queued behind /quit must not run after the session ended"
        );
        composed.shutdown().await.unwrap();
    }

    /// Q6: a queued checkpoint (slow storage in flight through the lane)
    /// must never reach the disk once the session has ended, on any exit
    /// path.
    #[tokio::test]
    async fn a_queued_checkpoint_never_reaches_disk_after_the_session_ends() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let (composed, interactive, checkpoint_dir) = tui_compose(
            &root,
            &[],
            Arc::new(TurnCountingModel {
                calls: calls.clone(),
            }),
            None,
        )
        .await
        .unwrap();
        let mut ui_events = composed.subscribe();
        composed.instance.start().await.unwrap();

        let (key_tx, key_rx) = tokio::sync::mpsc::channel::<KeyEvent>(256);
        for key in keys("/checkpoint") {
            key_tx.try_send(key).unwrap();
        }
        drop(key_tx);

        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sink = CaptureSink {
            captured: captured.clone(),
        };
        let mut source = ScriptedErrorSource { rx: key_rx };
        let session = tokio::time::timeout(
            Duration::from_secs(60),
            run_session(
                &mut source,
                &mut sink,
                composed.handle().clone(),
                &composed.instance,
                &mut ui_events,
                Some(interactive),
                "dynamic",
                checkpoint_dir.clone(),
                "serving: scripted e2e model".to_string(),
                None,
                false,
            ),
        )
        .await;
        assert!(session.is_ok(), "the session hung: {session:?}");
        let error = session
            .unwrap()
            .expect_err("the injected key read failure must surface");
        assert!(
            format!("{error:#}").contains("injected key read failure"),
            "the session error must be the injected one, got: {error}"
        );

        drain_executor_for_wrong_dispatch().await;
        let artifacts: Vec<String> = std::fs::read_dir(&checkpoint_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            artifacts.is_empty(),
            "a checkpoint queued in the lane must not be written after the session ended: {artifacts:?}"
        );
        composed.shutdown().await.unwrap();
    }

    /// The worker's own contract: once the stop barrier is raised and the
    /// worker is reclaimed, everything still on the ledger is queued-never-
    /// dispatched, nothing reaches the runtime afterwards, and the receipt
    /// names it. On the current-thread executor the worker is aborted before
    /// its first poll, so every submission is provably still queued.
    #[tokio::test]
    async fn the_stop_barrier_and_abort_leave_queued_commands_undispatched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let (composed, _interactive, checkpoint_dir) = tui_compose(
            &root,
            &[],
            Arc::new(TurnCountingModel {
                calls: calls.clone(),
            }),
            None,
        )
        .await
        .unwrap();
        composed.instance.start().await.unwrap();

        let (command_tx, command_rx) =
            tokio::sync::mpsc::channel::<QueuedCommand>(COMMAND_QUEUE_CAP);
        let (notice_tx, _notice_rx) = tokio::sync::mpsc::channel::<String>(NOTICE_CHANNEL_CAP);
        let (view_tx, _view_rx) = tokio::sync::mpsc::channel::<ViewFact>(8);
        let ledger = Arc::new(CommandLedger::default());
        let worker = tokio::spawn(run_command_worker(
            command_rx,
            composed.handle().clone(),
            composed.instance.checkpoint_plane(),
            notice_tx,
            view_tx,
            checkpoint_dir.clone(),
            Arc::clone(&ledger),
        ));

        let mut app = AppState::new(composed.handle().run_id());
        let lane = CommandLane {
            tx: command_tx,
            ledger: Arc::clone(&ledger),
        };
        assert!(lane.submit(&mut app, SessionCommand::Input { text: "one".into() }));
        assert!(lane.submit(&mut app, SessionCommand::Input { text: "two".into() }));
        drop(lane);

        // The same shutdown order the session uses: barrier, then reclaim.
        ledger.stop_accepting();
        worker.abort();
        let joined = worker.await;
        assert!(
            joined.is_err(),
            "an aborted worker reports cancellation, not success: {joined:?}"
        );
        assert_eq!(
            ledger.receipt(),
            WorkerReceipt {
                queued_never_dispatched: 2,
                taken_result_unknown: 0
            },
            "commands the worker never dequeued are queued, never executed"
        );

        drain_executor_for_wrong_dispatch().await;
        assert_eq!(
            model_dispatch_report(&calls),
            "",
            "nothing queued may reach the runtime after the barrier and abort"
        );
        composed.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod exec4_restore_read_tests {
    use super::*;

    /// EXEC-4 (E08)：恰好 checkpoint 工件上限的稀疏文件照常读入（解码由
    /// 既有摘要验证负责）；超上限一字节的文件在**读入阶段**被拒绝——错误
    /// 点名边界，而不是先吃下整个文件再失败。
    #[tokio::test]
    async fn restore_read_refuses_an_oversized_file_at_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let cap = agent_runtime::MAX_CHECKPOINT_ARTIFACT_BYTES as u64;

        // 恰好上限：稀疏写入（洞读作零），读入成功且字节数精确。
        let exact = dir.path().join("exact.json");
        let file = std::fs::File::create(&exact).unwrap();
        file.set_len(cap).unwrap();
        drop(file);
        let bytes = read_checkpoint_bounded(&exact).await.unwrap();
        assert_eq!(bytes.len() as u64, cap);

        // 超上限一字节：拒绝发生在读入阶段，错误点名边界。
        let over = dir.path().join("over.json");
        let file = std::fs::File::create(&over).unwrap();
        file.set_len(cap + 1).unwrap();
        drop(file);
        let error = read_checkpoint_bounded(&over).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds the checkpoint artifact bound"),
            "the refusal names the bound: {error}"
        );
    }
}

#[cfg(test)]
mod command_queue_tests {
    use super::*;

    fn app() -> AppState {
        AppState::new(agent_contracts::RunId::new())
    }

    /// A lane plus the receiving end a test drives directly.
    fn test_lane(cap: usize) -> (CommandLane, tokio::sync::mpsc::Receiver<QueuedCommand>) {
        let (tx, rx) = tokio::sync::mpsc::channel::<QueuedCommand>(cap);
        (
            CommandLane {
                tx,
                ledger: std::sync::Arc::new(CommandLedger::default()),
            },
            rx,
        )
    }

    /// The whole point of the lane: one consumer sees the commands in the
    /// order the operator typed them.
    #[tokio::test]
    async fn queued_commands_are_delivered_in_submission_order() {
        let (tx, mut rx) = test_lane(COMMAND_QUEUE_CAP);
        let mut app = app();
        let first = TaskId::new();
        let second = TaskId::new();
        assert!(submit_command(
            &tx,
            &mut app,
            SessionCommand::Activate { task_id: first }
        ));
        assert!(submit_command(
            &tx,
            &mut app,
            SessionCommand::Continue {
                observed: Some(second)
            }
        ));
        match rx.recv().await.expect("first command").command {
            SessionCommand::Activate { task_id } => assert_eq!(task_id, first),
            other => panic!("commands arrived out of order: {other:?}"),
        }
        match rx.recv().await.expect("second command").command {
            SessionCommand::Continue { observed } => assert_eq!(observed, Some(second)),
            other => panic!("commands arrived out of order: {other:?}"),
        }
    }

    /// A full lane must report the drop. Silently discarding a typed action
    /// is exactly the failure this lane exists to prevent.
    #[tokio::test]
    async fn a_full_queue_reports_the_drop_instead_of_losing_the_command() {
        let (tx, _held_so_the_channel_stays_full) = test_lane(1);
        let mut app = app();
        assert!(submit_command(&tx, &mut app, SessionCommand::Checkpoint));
        assert!(
            !submit_command(&tx, &mut app, SessionCommand::Checkpoint),
            "the second submission cannot be delivered"
        );
        let last = app.messages.last().expect("a reported drop");
        assert!(
            last.content.contains("command queue is full"),
            "the drop must be named: {}",
            last.content
        );
        assert!(
            last.content.contains("NOT submitted"),
            "the drop must say the command did not reach the runtime: {}",
            last.content
        );
    }

    /// A closed worker is also reported, never assumed to have run.
    #[tokio::test]
    async fn a_closed_worker_is_reported() {
        let (tx, rx) = test_lane(1);
        drop(rx);
        let mut app = app();
        assert!(!submit_command(&tx, &mut app, SessionCommand::Checkpoint));
        let last = app.messages.last().expect("a reported drop");
        assert!(
            last.content.contains("command worker is gone"),
            "{}",
            last.content
        );
    }

    /// R6: with the worker parked (a slow command in flight), the task
    /// command and the ordinary text typed after it are still delivered in
    /// typed order. Before this change the text never entered the lane at all,
    /// so it could reach the previous task while `/task B` sat queued.
    #[tokio::test]
    async fn ordinary_text_cannot_overtake_a_pending_task_command() {
        let (tx, mut rx) = test_lane(COMMAND_QUEUE_CAP);
        let mut app = app();
        let task = TaskId::new();
        // The worker is not consuming yet: this is the paused-worker window.
        assert!(submit_command(
            &tx,
            &mut app,
            SessionCommand::Activate { task_id: task }
        ));
        assert!(submit_command(
            &tx,
            &mut app,
            SessionCommand::Input {
                text: "按这个要求修改".into(),
            }
        ));
        assert_eq!(tx.pending(), 2, "both submissions are still queued");
        // The consumer starts now and must see them in the typed order.
        match rx.recv().await.expect("first").command {
            SessionCommand::Activate { task_id } => assert_eq!(task_id, task),
            other => panic!("the correction overtook its task command: {other:?}"),
        }
        match rx.recv().await.expect("second").command {
            SessionCommand::Input { text } => assert_eq!(text, "按这个要求修改"),
            other => panic!("the correction overtook its task command: {other:?}"),
        }
    }

    /// R6: the session can name work that never started, instead of dropping
    /// the sender and assuming the queue was cancelled.
    #[tokio::test]
    async fn the_lane_reports_submissions_that_have_not_started() {
        let (tx, _held) = test_lane(4);
        let mut app = app();
        assert!(submit_command(&tx, &mut app, SessionCommand::Checkpoint));
        assert!(submit_command(
            &tx,
            &mut app,
            SessionCommand::Input { text: "hi".into() }
        ));
        assert_eq!(tx.pending(), 2);
    }

    /// O3: the receipt — not the diagnostic count — separates what never
    /// reached the runtime from what was taken and stays with an unknown
    /// result.
    #[test]
    fn the_receipt_separates_queued_taken_and_settled_commands() {
        let ledger = CommandLedger::default();
        let first = ledger.register();
        let second = ledger.register();
        assert_eq!(
            ledger.receipt(),
            WorkerReceipt {
                queued_never_dispatched: 2,
                taken_result_unknown: 0
            }
        );
        // Taken: in flight, result unknown — never reported as "not executed".
        ledger.mark_taken(first);
        assert_eq!(
            ledger.receipt(),
            WorkerReceipt {
                queued_never_dispatched: 1,
                taken_result_unknown: 1
            }
        );
        // Settled: the runtime interaction completed (outcome went to
        // notices); it is off the books either way.
        ledger.settle(first);
        assert_eq!(
            ledger.receipt(),
            WorkerReceipt {
                queued_never_dispatched: 1,
                taken_result_unknown: 0
            }
        );
        ledger.settle(second);
        assert_eq!(ledger.receipt(), WorkerReceipt::default());
        assert_eq!(ledger.queued(), 0);
    }

    /// O3: a refused submission (full or closed lane) leaves no ledger
    /// entry, so the count can never underflow.
    #[tokio::test]
    async fn a_refused_submission_leaves_no_receipt_entry() {
        let (tx, _held) = test_lane(1);
        let mut app = app();
        assert!(submit_command(&tx, &mut app, SessionCommand::Checkpoint));
        assert!(!submit_command(&tx, &mut app, SessionCommand::Checkpoint));
        assert_eq!(tx.pending(), 1, "only the accepted submission counts");
        drop(_held);
        assert!(
            !submit_command(&tx, &mut app, SessionCommand::Checkpoint),
            "a closed lane refuses"
        );
        assert_eq!(tx.pending(), 1, "the refusal must not change the count");
    }

    /// The refusal names both sides, so the operator can see which task the
    /// runtime is actually on.
    #[test]
    fn an_identity_mismatch_names_both_tasks() {
        let observed = TaskId::new();
        let live = TaskId::new();
        let notice =
            identity_mismatch_notice("continue", Some(observed), Some(live), "no turn started");
        assert!(notice.contains(&observed.to_string()), "{notice}");
        assert!(notice.contains(&live.to_string()), "{notice}");
        assert!(notice.contains("no turn started"), "{notice}");

        let none = identity_mismatch_notice("done", None, Some(live), "nothing was closed");
        assert!(none.contains("no task"), "{none}");
        assert!(none.contains(&live.to_string()), "{none}");
    }
}

#[cfg(test)]
mod approval_key_tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn classify_approval_key_maps_y_enter_to_allow_and_n_esc_to_deny() {
        assert_eq!(
            classify_approval_key(KeyCode::Char('y')),
            Some(ApprovalDecision::Allow)
        );
        assert_eq!(
            classify_approval_key(KeyCode::Enter),
            Some(ApprovalDecision::Allow)
        );
        assert_eq!(
            classify_approval_key(KeyCode::Char('n')),
            Some(ApprovalDecision::Deny)
        );
        assert_eq!(
            classify_approval_key(KeyCode::Esc),
            Some(ApprovalDecision::Deny)
        );
        // Navigation and other keys are not answers; the caller scrolls or
        // ignores them so the operator can read the request before deciding.
        assert_eq!(classify_approval_key(KeyCode::PageUp), None);
        assert_eq!(classify_approval_key(KeyCode::PageDown), None);
        assert_eq!(classify_approval_key(KeyCode::Char('x')), None);
    }
}

/// The decision must be bound to the request_id currently on screen: a stale
/// confirmation for an already-resolved request must never approve a newer one
/// that arrives in the meantime. Driven with a real broker + gate (no pty).
#[cfg(test)]
mod approval_binding_tests {
    use super::*;
    use agent_contracts::{
        ApprovalDecision, ApprovalGate, CancellationToken, RunId, ToolCall, ToolRisk, ToolSpec,
    };
    use agent_core::{ApprovalBroker, InteractiveApprovalGate};
    use std::sync::Arc;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            risk: ToolRisk::WorkspaceWrite,
            roles: Vec::new(),
            output_budget: None,
        }
    }

    #[tokio::test]
    async fn expired_request_confirmation_does_not_approve_a_later_request() {
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));

        // Request A is submitted and shown to the operator.
        let call_a = ToolCall {
            id: "c-a".into(),
            name: "fs.write".into(),
            arguments: serde_json::json!({ "path": "a.txt" }),
        };
        let gate_a = gate.clone();
        let task_a = tokio::spawn(async move {
            gate_a
                .authorize(&call_a, &spec("fs.write"), &CancellationToken::new())
                .await
        });

        let mut rx = broker.subscribe();
        let req_a = rx.recv().await.expect("request A broadcast");
        let mut app = AppState::new(RunId::new());
        app.begin_approval(req_a);
        let id_a = app.pending_approval.as_ref().unwrap().request_id.clone();

        // A expires: resolved elsewhere (kernel timeout / cancelled turn).
        assert!(
            gate.respond(&id_a, ApprovalDecision::Allow).await,
            "A should be resolvable"
        );
        assert!(
            matches!(task_a.await.unwrap(), Ok(ApprovalDecision::Allow)),
            "A's waiter sees Allow"
        );

        // Stale [y] for the expired A: bound to A's id, must be rejected.
        let stale = app.pending_approval.as_ref().unwrap().request_id.clone();
        let granted = gate.respond(&stale, ApprovalDecision::Allow).await;
        assert!(
            !granted,
            "an expired request's confirmation must NOT be accepted"
        );

        // A new request B arrives and is shown.
        let call_b = ToolCall {
            id: "c-b".into(),
            name: "fs.write".into(),
            arguments: serde_json::json!({ "path": "b.txt" }),
        };
        let gate_b = gate.clone();
        let task_b = tokio::spawn(async move {
            gate_b
                .authorize(&call_b, &spec("fs.write"), &CancellationToken::new())
                .await
        });
        let req_b = rx.recv().await.expect("request B broadcast");
        let id_b = req_b.request_id.clone();
        app.begin_approval(req_b);
        assert_eq!(app.pending_approval.as_ref().unwrap().request_id, id_b);

        // B must still be unresolved: the stale A confirm never touched it.
        let still_pending = broker.pending().await.iter().any(|r| r.request_id == id_b);
        assert!(
            still_pending,
            "the later request B must not be approved by a stale confirmation for A"
        );

        // Clean up so the background task does not hang.
        let _ = gate.respond(&id_b, ApprovalDecision::Deny).await;
        let _ = task_b.await;
    }
}
