use agent_contracts::{
    ContextDiagnostics, ContextSelection, ContextStateTransition, OperationId, RunId, RuntimeEvent,
    RuntimeEventEnvelope, RuntimeInputId, TaskId, ToolSurfaceBlockReason, ToolSurfaceDemand,
    ToolSurfacePlanReport, ToolSurfacePlanStatus, TurnId,
};
use agent_core::ApprovalRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone)]
pub struct UiMessage {
    pub role: UiRole,
    pub content: String,
    /// R7: the event that produced this row, or `None` for a session-local row
    /// (the opening banner, a command notice, an off-loop reply). A journal
    /// replay drops the event-derived rows and rebuilds them, so the same log
    /// read twice yields the same visible transcript and a replayed SYSTEM row
    /// cannot push a real dialogue row out of the window.
    pub event_identity: Option<(RunId, u64)>,
}

/// A workspace-write / process-execution call waiting for the user's y/n.
///
/// `detail` preserves the COMPLETE request — tool name, risk, request id and
/// every argument at full length — as scrollable lines (no 220-char cap). The
/// UI renders it in a scrollable panel so the operator can read and verify
/// everything before answering. `truncated` is set only when the renderer's
/// own defensive hard cap was hit (Core already bounds the request); it lets
/// the panel mark the omission instead of silently dropping data.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub tool_name: String,
    pub detail: Vec<String>,
    pub truncated: bool,
}

/// Per-argument value cap for the secondary conversation-log summary. The
/// full value is always available in the scrollable approval panel; here we
/// mark the cut with a trailing '…' so it never reads as complete.
const ARG_VALUE_CAP: usize = 120;
/// How many arguments the secondary conversation-log summary names before
/// pointing at the scrollable approval panel.
const ARG_PREVIEW_COUNT: usize = 8;

const MAX_PANEL_TRANSITIONS: usize = 100;
/// Hard cap on rendered transcript rows. The durable transcript is the
/// runtime event journal; the UI only keeps a bounded window so a long
/// session cannot grow TUI memory without bound.
const MAX_RENDERED_MESSAGES: usize = 400;
const MAX_TOOL_SURFACE_PREVIEW_ROWS_PER_KIND: usize = 3;
const MAX_TOOL_SURFACE_PREVIEW_NAME_CHARS: usize = 48;
const MAX_TOOL_SURFACE_MESSAGE_CHARS: usize = 640;
/// Bounded result-card buckets (`/review`). The card is event-derived
/// display material; the durable change ledger stays in
/// `.focus-agent/changes.jsonl` and the completion record in the journal.
const MAX_CARD_FILES: usize = 32;
const MAX_CARD_CHECKS: usize = 32;
const MAX_CARD_FAILURES: usize = 8;
const MAX_CARD_LINE_CHARS: usize = 160;
/// How many finished tasks' cards are kept for `/review` after the focus
/// moves on. Bounded: this is display material, not a second task ledger.
const MAX_ARCHIVED_CARDS: usize = 8;
/// Per-file read cap for a projection resync (32 MiB), enforced by the
/// reader before allocation.
const RESYNC_FILE_BYTES: usize = 32 * 1024 * 1024;
/// How many applied `(RunId, seq)` identities the view remembers. Exact
/// once-only application is what stops a redelivered broadcast event from
/// re-counting tokens or re-activating a superseded operation; the bound
/// keeps a long session's memory flat. Identities old enough to be evicted
/// are still covered by the contiguous replay watermark.
const MAX_APPLIED_EVENTS: usize = 8192;
/// How many transcript rows are remembered by event identity so a replay
/// cannot append the same row twice. Bounded per `MAX_APPLIED_EVENTS`
/// reasoning: the durable transcript itself is capped at
/// `MAX_RENDERED_MESSAGES` rows, so this only needs to outlive the window
/// a resync can re-read.
const MAX_SHOWN_MESSAGE_EVENTS: usize = 4096;

/// One file a mutating tool wrote this session, identified by the
/// structured `metadata.path` the tool stamped — never parsed from prose.
/// `task_id` names the task the write is attributed to, so material from
/// two tasks can never be read as one task's result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CardChangedFile {
    pub path: String,
    pub tool: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

/// One trusted verification-class run this session (verify.run,
/// shell.exec, process.run) with its real outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CardCheck {
    pub tool: String,
    pub ok: bool,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CardCompletion {
    pub task_id: TaskId,
    pub anchor_revision: u64,
    pub summary: String,
}

/// One recorded failure with the task it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CardFailure {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

/// Serializes result-card snapshot commits: a single writer, ordered by the
/// SESSION publish sequence, so an older snapshot that finishes later can
/// never overwrite a newer one.
///
/// R4: this is deliberately NOT the card's own `revision`. A card is per-task
/// and starts from zero again after a task switch, so using its revision as
/// the publish watermark made the second task's snapshot look "older" than the
/// first task's and dropped it. The publish sequence is monotonic for the life
/// of the session and is never reset by a task switch or a projection rebuild.
#[derive(Debug, Default)]
struct CardSnapshotState {
    last_written_publish: u64,
}

/// The bounded, event-derived result card `/review` renders. It lists only
/// what this session's own tool calls prove; pre-existing workspace
/// modifications are deliberately not attributed.
///
/// The card belongs to ONE task: `task_id` names it, and a different task
/// starts a different card, so task A's durable completion header can never
/// sit above task B's changes. `omitted_*` counts what the display caps
/// refused to append, so a late failure is never silently absent from the
/// review — the count is visible even when the entry is not.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResultCard {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// The card's own content revision (per task). A task switch starts a new
    /// card, so this may legitimately return to a small value.
    #[serde(default)]
    pub revision: u64,
    /// Session publish sequence of the snapshot that carries this card.
    /// Monotonic across task switches; this is what orders artifacts on disk.
    #[serde(default)]
    pub publish_seq: u64,
    pub changed_files: Vec<CardChangedFile>,
    pub checks: Vec<CardCheck>,
    pub completion: Option<CardCompletion>,
    pub failures: Vec<CardFailure>,
    #[serde(default)]
    pub omitted_files: usize,
    #[serde(default)]
    pub omitted_checks: usize,
    #[serde(default)]
    pub omitted_failures: usize,
    /// Every verification-class run that ended FAILED, counted BEFORE the
    /// display cap could refuse it. `checks` is a bounded display window;
    /// this is the fact the window must never be mistaken for.
    #[serde(default)]
    pub failed_checks_total: usize,
}

impl ResultCard {
    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty()
            && self.checks.is_empty()
            && self.completion.is_none()
            && self.failures.is_empty()
    }

    /// Total recorded checks, including the ones the display cap refused.
    pub fn total_checks(&self) -> usize {
        self.checks.len().saturating_add(self.omitted_checks)
    }

    /// FAILED checks still visible in the bounded display window. Use
    /// `failed_checks_total` for the account: a failure the cap refused is
    /// still a failure.
    pub fn failed_checks_in_window(&self) -> usize {
        self.checks.iter().filter(|check| !check.ok).count()
    }
}

fn is_verification_tool(tool_name: &str) -> bool {
    matches!(tool_name, "verify.run" | "shell.exec" | "process.run")
}

fn is_mutating_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "fs.write" | "edit.patch" | "edit.replace" | "fs.mkdir"
    )
}

fn bounded_card_line(text: &str) -> String {
    let mut bounded: String = text.chars().take(MAX_CARD_LINE_CHARS).collect();
    if text.chars().count() > MAX_CARD_LINE_CHARS {
        bounded.push('…');
    }
    bounded
}

/// Return `text` trimmed to `cap` chars plus whether it was cut. Used by the
/// secondary approval summary so a truncated value is marked, never silently
/// presented as complete.
fn bounded_preview(text: &str, cap: usize) -> (String, bool) {
    let cut = text.chars().count() > cap;
    (text.chars().take(cap).collect(), cut)
}

fn demand_label(demand: ToolSurfaceDemand) -> &'static str {
    match demand {
        ToolSurfaceDemand::KeepReady => "ready",
        ToolSurfaceDemand::PreferSurface => "prefer",
        ToolSurfaceDemand::MustSurface => "must",
    }
}

fn block_reason_label(reason: ToolSurfaceBlockReason) -> &'static str {
    match reason {
        ToolSurfaceBlockReason::Unavailable => "unavailable",
        ToolSurfaceBlockReason::SchemaBudget => "required schema budget",
        ToolSurfaceBlockReason::ProviderInputBudget => "provider input budget",
    }
}

fn bounded_tool_name(name: &str) -> String {
    let mut chars = name.chars();
    let mut bounded: String = chars
        .by_ref()
        .take(MAX_TOOL_SURFACE_PREVIEW_NAME_CHARS)
        .collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn bounded_tool_surface_message(report: &ToolSurfacePlanReport) -> String {
    let status = match report.status {
        ToolSurfacePlanStatus::Ready => "ready".to_string(),
        ToolSurfacePlanStatus::Unsatisfiable { reason } => {
            format!("blocked ({})", block_reason_label(reason))
        }
    };
    let mut message = format!(
        "tool surface r{} round {}: {status}; selected {} (≈{} tok), omitted {}, blocked {}; input ≈{}/{} tok",
        report.surface_revision,
        report.model_round,
        report.selected_total,
        report.selected_schema_tokens,
        report.omitted_total,
        report.blocked_total,
        report.estimated_input_tokens,
        report.input_budget_tokens,
    );

    let selected: Vec<String> = report
        .selected
        .iter()
        .take(MAX_TOOL_SURFACE_PREVIEW_ROWS_PER_KIND)
        .map(|entry| {
            format!(
                "{}:{}",
                bounded_tool_name(&entry.tool_name),
                demand_label(entry.demand)
            )
        })
        .collect();
    if !selected.is_empty() {
        let total = report.selected_total.max(report.selected.len());
        message.push_str(&format!("; selected [{}]", selected.join(", ")));
        if total > selected.len() {
            message.push_str(&format!(" +{} more", total - selected.len()));
        }
    }

    let omitted: Vec<String> = report
        .omitted
        .iter()
        .take(MAX_TOOL_SURFACE_PREVIEW_ROWS_PER_KIND)
        .map(|entry| {
            format!(
                "{}:{}",
                bounded_tool_name(&entry.tool_name),
                entry.reason.as_str()
            )
        })
        .collect();
    if !omitted.is_empty() {
        let total = report.omitted_total.max(report.omitted.len());
        message.push_str(&format!("; omitted [{}]", omitted.join(", ")));
        if total > omitted.len() {
            message.push_str(&format!(" +{} more", total - omitted.len()));
        }
    }

    let blocked: Vec<String> = report
        .blocked
        .iter()
        .take(MAX_TOOL_SURFACE_PREVIEW_ROWS_PER_KIND)
        .map(|entry| {
            format!(
                "{}:{}",
                bounded_tool_name(&entry.tool_name),
                block_reason_label(entry.reason)
            )
        })
        .collect();
    if !blocked.is_empty() {
        let total = report.blocked_total.max(report.blocked.len());
        message.push_str(&format!("; blocked [{}]", blocked.join(", ")));
        if total > blocked.len() {
            message.push_str(&format!(" +{} more", total - blocked.len()));
        }
    }

    if message.chars().count() > MAX_TOOL_SURFACE_MESSAGE_CHARS {
        let mut bounded: String = message
            .chars()
            .take(MAX_TOOL_SURFACE_MESSAGE_CHARS.saturating_sub(1))
            .collect();
        bounded.push('…');
        bounded
    } else {
        message
    }
}

fn tool_transcript_text(output: &agent_contracts::ToolOutput) -> String {
    format!(
        "{}\n{}",
        output.summary,
        output.artifact_ref.clone().unwrap_or_default()
    )
}

pub struct AppState {
    pub run_id: RunId,
    pub input: String,
    pub messages: Vec<UiMessage>,
    /// Event-derived status read model (runtime-owned projection).
    pub status_projection: agent_runtime::status::StatusProjection,
    pub context: ContextDiagnostics,
    pub context_selected: Vec<ContextSelection>,
    pub context_transitions: Vec<ContextStateTransition>,
    pub show_context_panel: bool,
    pub streaming: bool,
    pub status: String,
    pub tool_status: String,
    pub busy: bool,
    /// How many wrapped rows above the latest transcript the operator is
    /// holding in view (PageUp). Zero follows the tail so new [YOU]/AGENT/
    /// TOOL rows stay visible instead of hiding under the opening SYSTEM
    /// banners.
    pub scroll: u16,
    pub pending_approval: Option<PendingApproval>,
    /// How many rows the operator has paged down into the scrollable
    /// approval detail panel. Top-anchored: zero shows the request header,
    /// larger values reveal later arguments and the trailing sentinel.
    pub approval_scroll: u16,
    /// Cumulative provider-reported token usage for the live run (fed by
    /// `RuntimeEvent::ModelUsed`).
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The model operation whose streamed deltas are currently being
    /// rendered. A delta that does not match this identity belongs to a
    /// superseded turn and is dropped — the fence against a cancelled
    /// turn's late text leaking into the next turn's transcript.
    current_op: Option<(TurnId, OperationId, u64)>,
    /// Whether the transcript's last row is a bubble this turn's stream
    /// opened. `AssistantMessage` may only *finalize* such a row; without
    /// this, a later turn's reply would overwrite an earlier turn's row
    /// whenever the two happened to be adjacent.
    streaming_row_open: bool,
    /// The input id currently sitting in the runtime's single dialogue
    /// queue slot, so its later `Applied` record can be named as the
    /// queued input running rather than a silent status flip.
    queued_input_id: Option<RuntimeInputId>,
    /// Bounded event-derived status projection: the task the runtime
    /// currently anchors, and how many effect-ack debts remain unresolved.
    /// `/status` renders these; nothing here drives effects.
    pub current_task: Option<(TaskId, u64)>,
    /// The task the operator most recently saw this session. Used as the
    /// `expected_task_id` for identity-checked commands, so a command typed
    /// against task A cannot silently act on whatever became active later.
    /// Not cleared by `FocusCleared`: the operator's last observation is
    /// still task A even while it is suspended.
    pub observed_task: Option<TaskId>,
    pub unresolved_ack_debts: usize,
    pub last_checkpoint: Option<String>,
    /// The explicit per-turn model-round budget (`--max-rounds`). `None`
    /// means the runtime default applies; the value is only a display
    /// fact for `/status`, never an authority.
    pub execution_budget: Option<usize>,
    /// Bounded event-derived review material for `/review`. The UI only
    /// displays it; nothing here drives effects.
    pub result_card: ResultCard,
    /// Cards for tasks the focus has already left, newest last. A card is
    /// bounded material for ONE task; switching tasks archives instead of
    /// merging, so `/review` can still answer for the task just finished.
    archived_cards: Vec<ResultCard>,
    /// Workspace state dir (`.focus-agent`), used to persist the latest
    /// result card as a small JSON artifact at task completion.
    pub state_dir: Option<std::path::PathBuf>,
    /// Single-writer gate for the card snapshot; see `persist_result_card`.
    card_snapshot_gate: std::sync::Arc<tokio::sync::Mutex<CardSnapshotState>>,
    /// Session-monotonic snapshot publish sequence. Never reset by a task
    /// switch or a projection rebuild — see `CardSnapshotState` for why.
    card_publish_seq: u64,
    /// True while the journal replay is rebuilding the view. Historical
    /// `TaskCompleted` events must not each re-publish a snapshot: a replay is
    /// a read of the past, not a new delivery.
    replaying: bool,
    /// Highest durable sequence of this run already folded from the
    /// journal by a resync. Live events at or below it are skipped by the
    /// projection fold so a post-resync replay never double-counts.
    resynced_through_seq: Option<u64>,
    /// Every `(RunId, seq)` already folded into this view — live or
    /// replayed. An event is applied **at most once**, so a redelivered
    /// broadcast event can neither double-count tokens nor re-activate a
    /// superseded operation. Bounded FIFO; see `MAX_APPLIED_EVENTS`.
    applied_events: std::collections::VecDeque<(RunId, u64)>,
    applied_index: std::collections::HashSet<(RunId, u64)>,
    /// Event identities whose transcript row was already appended. Keyed by
    /// the event, never by the message text: two legitimate replies with
    /// identical wording are two events and must both appear.
    shown_message_events: std::collections::VecDeque<(RunId, u64)>,
    shown_message_index: std::collections::HashSet<(RunId, u64)>,
    /// Input ids whose user bubble was already appended (queued then
    /// applied share one id, and a replay must not duplicate the bubble).
    /// R7: bounded like every other identity structure — keeping only the
    /// 400-row transcript bounded proved nothing about this index.
    shown_input_events: std::collections::VecDeque<RuntimeInputId>,
    shown_input_index: std::collections::HashSet<RuntimeInputId>,
    /// The durable event currently being folded, so every row it appends can
    /// carry the identity a replay needs.
    current_event: Option<(RunId, u64)>,
    /// Set when the journal replay could not verify a contiguous prefix —
    /// a bad line, a short read or an unreadable file. The view then says
    /// so instead of presenting a partial picture as complete.
    pub view_partial: bool,
    /// Why the view is partial, for `/status`.
    pub view_partial_reason: Option<String>,
}

impl AppState {
    pub fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            input: String::new(),
            messages: vec![UiMessage {
                role: UiRole::System,
                // Session-local: never dropped by a replay.
                event_identity: None,
                content: "Prototype ready. /help lists the product commands; Tab inspects the working context. Try `demo: list files` and `demo: write hello`.".into(),
            }],
            context: ContextDiagnostics::default(),
            context_selected: Vec::new(),
            context_transitions: Vec::new(),
            show_context_panel: false,
            streaming: false,
            status: "idle".into(),
            tool_status: "none".into(),
            busy: false,
            scroll: 0,
            pending_approval: None,
            approval_scroll: 0,
            input_tokens: 0,
            output_tokens: 0,
            current_op: None,
            streaming_row_open: false,
            queued_input_id: None,
            current_task: None,
            observed_task: None,
            unresolved_ack_debts: 0,
            last_checkpoint: None,
            execution_budget: None,
            result_card: ResultCard::default(),
            archived_cards: Vec::new(),
            state_dir: None,
            card_snapshot_gate: std::sync::Arc::new(tokio::sync::Mutex::new(
                CardSnapshotState::default(),
            )),
            card_publish_seq: 0,
            replaying: false,
            resynced_through_seq: None,
            applied_events: std::collections::VecDeque::new(),
            applied_index: std::collections::HashSet::new(),
            shown_message_events: std::collections::VecDeque::new(),
            shown_message_index: std::collections::HashSet::new(),
            shown_input_events: std::collections::VecDeque::new(),
            shown_input_index: std::collections::HashSet::new(),
            current_event: None,
            view_partial: false,
            view_partial_reason: None,
            status_projection: agent_runtime::status::StatusProjection::default(),
        }
    }

    /// The bounded `/status` read model: an event-derived snapshot of run,
    /// task anchor, recovery debts and the last saved checkpoint. It never
    /// drives effects and rebuilds from the same events on restart.
    pub fn render_status(&self) -> Vec<String> {
        // The projection is folded from the same public event stream every
        // host consumes; the lines below are UI-local additions only.
        let mut lines = self.status_projection.lines();
        if let Some(rounds) = self.execution_budget {
            lines.push(format!("execution budget: {rounds} model rounds per turn"));
        }
        match &self.last_checkpoint {
            Some(path) => lines.push(format!("last manual checkpoint: {path}")),
            None => lines.push("last manual checkpoint: none this session".into()),
        }
        if let Some(approval) = &self.pending_approval {
            lines.push(format!("pending approval: {}", approval.tool_name));
        }
        // Honest coverage: a replay that could not verify a contiguous
        // prefix says so instead of presenting a partial picture as the
        // whole story. Nothing here drives effects.
        if self.view_partial {
            lines.push(format!(
                "view: PARTIAL — {}",
                self.view_partial_reason
                    .as_deref()
                    .unwrap_or("journal replay could not be verified")
            ));
        } else if self.resynced_through_seq.is_some() {
            lines.push("view: replayed from the durable journal (contiguous)".into());
        }
        lines
    }

    /// Persist the latest result card as one small JSON artifact under the
    /// workspace state dir, so `/review` survives a restart.
    ///
    /// Fire-and-forget display material — a write failure is not a runtime
    /// fault — but it must not let an OLDER card overwrite a newer one, nor
    /// leave a half-written file to be read as "no result". Writes are
    /// serialized through one gate and committed by rename, and a snapshot
    /// whose revision is not newer than what was already committed is
    /// dropped.
    fn persist_result_card(&mut self) {
        let Some(state_dir) = self.state_dir.clone() else {
            return;
        };
        if self.replaying {
            // R7: a replay re-reads history. Publishing a snapshot per
            // historical `TaskCompleted` would be a side effect of reading the
            // past, and could race the live card. The live path owns writes.
            return;
        }
        // R4: the publish order is the SESSION's, not the card's. The card's
        // own revision restarts with each task; the publish sequence does not.
        self.result_card.revision = self.result_card.revision.saturating_add(1);
        self.card_publish_seq = self.card_publish_seq.saturating_add(1);
        self.result_card.publish_seq = self.card_publish_seq;
        let publish = self.card_publish_seq;
        let Ok(bytes) = serde_json::to_vec(&self.result_card) else {
            return;
        };
        let path = state_dir.join("artifacts").join("result-card-latest.json");
        let gate = std::sync::Arc::clone(&self.card_snapshot_gate);
        tokio::spawn(async move {
            let mut state = gate.lock().await;
            if publish <= state.last_written_publish {
                // A newer snapshot already landed; an older one must never
                // overwrite it just because it finished later.
                return;
            }
            let Some(parent) = path.parent() else {
                return;
            };
            if tokio::fs::create_dir_all(parent).await.is_err() {
                return;
            }
            // Write-then-rename: a reader never observes a partial card.
            let staging = path.with_extension(format!("json.tmp-{publish}"));
            if tokio::fs::write(&staging, &bytes).await.is_err() {
                let _ = tokio::fs::remove_file(&staging).await;
                return;
            }
            if tokio::fs::rename(&staging, &path).await.is_err() {
                let _ = tokio::fs::remove_file(&staging).await;
                return;
            }
            state.last_written_publish = publish;
        });
    }

    /// Rebuild the status projection from the durable trace journal after
    /// a broadcast Lagged: the folded projection is replaced with one
    /// replayed from disk for THIS run only, so the view recovers instead
    /// of drifting or blending other runs' state. Bounded: at most 16
    /// NEWEST journal files, each read capped at 32 MiB before allocation.
    /// Returns (folded count, partial) — partial means a candidate file
    /// could not be read or was truncated at the cap, so the rebuilt
    /// projection may be incomplete and must say so.
    pub async fn resync_projection(&mut self, traces_dir: &std::path::Path) -> (usize, bool) {
        use tokio::io::AsyncReadExt as _;
        let Ok(mut entries) = tokio::fs::read_dir(traces_dir).await else {
            return (0, false);
        };
        let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            files.push((
                metadata
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                path,
            ));
        }
        // Newest first: a resync wants THIS run's freshest journals, not a
        // museum of the oldest ones.
        files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        files.truncate(16);
        // Rebuild this run's event-derived view from the durable journal:
        // reset the folded fields to their pre-run baseline and forget this
        // run's applied identities so the replay re-applies its events
        // through the SAME fold the live path uses. The transcript is
        // append-only and the local view state (draft, scroll, panel
        // toggle, an approval on screen) is not event-derived, so neither
        // is touched here.
        self.reset_event_derived_view();
        self.forget_applied_for_run(self.run_id);
        self.resynced_through_seq = None;
        self.view_partial = false;
        self.view_partial_reason = None;
        // R7: the replay rebuilds the view from the past; it must not also
        // re-publish a snapshot for every historical `TaskCompleted`.
        self.replaying = true;
        let mut folded = 0usize;
        let mut min_seq: Option<u64> = None;
        let mut max_seq: Option<u64> = None;
        let mut parsed: u64 = 0;
        let mut bad_lines: u64 = 0;
        let mut partial = false;
        let mut partial_reason: Option<String> = None;
        for (_, path) in &files {
            let Ok(file) = tokio::fs::File::open(path).await else {
                partial = true;
                partial_reason
                    .get_or_insert_with(|| format!("unreadable journal {}", path.display()));
                continue;
            };
            // The size cap is enforced by the reader, before any
            // allocation: a huge journal cannot blow up the UI's memory.
            let mut bytes = Vec::new();
            if file
                .take(RESYNC_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .is_err()
            {
                partial = true;
                partial_reason.get_or_insert_with(|| format!("failed reading {}", path.display()));
                continue;
            }
            if bytes.len() > RESYNC_FILE_BYTES {
                partial = true;
                partial_reason.get_or_insert_with(|| {
                    format!("journal {} exceeds the read cap", path.display())
                });
                bytes.truncate(RESYNC_FILE_BYTES);
                // Keep only complete lines from the truncated prefix.
                if let Some(pos) = bytes.iter().rposition(|byte| *byte == b'\n') {
                    bytes.truncate(pos + 1);
                } else {
                    bytes.clear();
                }
            }
            for line in String::from_utf8_lossy(&bytes).lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(envelope) =
                    serde_json::from_str::<agent_contracts::RuntimeEventEnvelope>(line)
                else {
                    // A line we could not parse is unverified coverage, not
                    // an absence of events: say so instead of silently
                    // treating the max sequence as a continuous watermark.
                    bad_lines += 1;
                    partial = true;
                    partial_reason.get_or_insert_with(|| "journal line failed to parse".into());
                    continue;
                };
                // Current run only: other runs' tasks, consumptions and
                // completions must never blend into this run's status.
                if envelope.run_id != self.run_id {
                    continue;
                }
                // Same fold rule as the live path — one reducer, so a
                // replayed view cannot be built by different rules.
                if self.claim_event(envelope.run_id, envelope.seq) {
                    self.apply_event(envelope.clone());
                    folded += 1;
                    parsed += 1;
                    min_seq = Some(min_seq.map_or(envelope.seq, |min| min.min(envelope.seq)));
                    max_seq = Some(max_seq.map_or(envelope.seq, |max| max.max(envelope.seq)));
                }
            }
        }
        // Contiguity: a span that does not account for every sequence means
        // the journal has a hole, so no contiguous coverage may be claimed.
        // The exact-once identity set still prevents any double-apply; the
        // watermark is only the stronger claim that a whole prefix was
        // durably verified, and it is withheld when it cannot be made.
        if let (Some(min), Some(max)) = (min_seq, max_seq) {
            let span = max.saturating_sub(min).saturating_add(1);
            if span != parsed {
                partial = true;
                partial_reason.get_or_insert_with(|| {
                    format!("journal sequences are not contiguous ({parsed} of {span})")
                });
            }
        }
        if partial {
            self.view_partial = true;
            self.view_partial_reason = Some(
                partial_reason.unwrap_or_else(|| "journal replay could not be verified".into()),
            );
        } else {
            // Replay watermark: durable events of this run at or below the
            // folded sequence were just counted from disk; the live
            // broadcast may still deliver them, and re-folding would
            // double-count. Only a verified contiguous prefix is claimed.
            self.resynced_through_seq = max_seq;
        }
        self.replaying = false;
        let _ = bad_lines;
        (folded, self.view_partial)
    }

    pub fn push_system(&mut self, content: String) {
        self.push_message(UiRole::System, content);
    }

    /// Append one transcript row, draining the oldest rows beyond the
    /// render cap. Every transcript write goes through here so no caller
    /// can reintroduce an unbounded list.
    fn push_message(&mut self, role: UiRole, content: String) {
        self.messages.push(UiMessage {
            role,
            content,
            event_identity: self.current_event,
        });
        // A freshly appended row is not a streamed bubble any more.
        self.streaming_row_open = false;
        let overflow = self.messages.len().saturating_sub(MAX_RENDERED_MESSAGES);
        if overflow > 0 {
            self.messages.drain(..overflow);
        }
    }

    /// Begin showing an interactive approval request. The full request is
    /// preserved in `detail` (every argument at full length) so the operator
    /// can scroll and verify it; only the secondary conversation-log summary
    /// is bounded, and it marks any truncation explicitly.
    pub fn begin_approval(&mut self, request: ApprovalRequest) {
        let tool_name = request.spec.name.clone();
        let risk = format!("{:?}", request.spec.risk);

        let mut detail: Vec<String> = Vec::new();
        detail.push(format!("Tool: {tool_name}"));
        detail.push(format!("Risk: {risk}"));
        detail.push(format!("Request id: {}", request.request_id));
        if !request.spec.description.is_empty() {
            detail.push(format!("Description: {}", request.spec.description));
        }
        detail.push(String::new());
        detail.push("Arguments:".to_string());

        let mut truncated = false;
        // Keep the COMPLETE request. Core already bounds the request size, so
        // this hard cap is defensive only; when it does bite, name the
        // omission so the operator never thinks they saw everything.
        const HARD_CAP: usize = 1 << 20;
        let full_args = serde_json::to_string(&request.call.arguments).unwrap_or_default();
        if full_args.len() > HARD_CAP {
            truncated = true;
            let capped: String = full_args.chars().take(HARD_CAP).collect();
            detail.push(format!("  {capped}"));
            detail.push(
                "[…] arguments truncated: the request exceeded the panel's hard display cap".into(),
            );
        } else if let Some(map) = request.call.arguments.as_object() {
            for (key, value) in map {
                let rendered = serde_json::to_string(value).unwrap_or_default();
                detail.push(format!("  {key}: {rendered}"));
            }
            if map.is_empty() {
                detail.push("  (no arguments)".into());
            }
        } else {
            detail.push(format!("  {full_args}"));
        }
        detail.push(String::new());
        detail.push(format!("— end of request {} —", request.request_id));

        self.pending_approval = Some(PendingApproval {
            request_id: request.request_id.clone(),
            tool_name: tool_name.clone(),
            detail,
            truncated,
        });
        self.approval_scroll = 0;
        self.busy = true;
        self.status = "awaiting approval".into();
        self.push_system(format!(
            "approval required: {tool_name} (risk: {:?})",
            request.spec.risk
        ));
        // Bounded secondary summary in the conversation log. Truncation is
        // marked: a value cut at {ARG_VALUE_CAP} chars gets a trailing '…',
        // and arguments past the first {ARG_PREVIEW_COUNT} name the count and
        // point the operator at the scrollable approval panel.
        if let Some(map) = request.call.arguments.as_object() {
            for (key, value) in map.iter().take(ARG_PREVIEW_COUNT) {
                let rendered = serde_json::to_string(value).unwrap_or_default();
                let (shown, cut) = bounded_preview(&rendered, ARG_VALUE_CAP);
                if cut {
                    self.push_system(format!("  {key}: {shown}…"));
                } else {
                    self.push_system(format!("  {key}: {shown}"));
                }
            }
            let extra = map.len().saturating_sub(ARG_PREVIEW_COUNT);
            if extra > 0 {
                self.push_system(format!(
                    "  …and {extra} more arguments (scroll the approval panel for the full list)"
                ));
            }
        }
    }

    pub fn clear_approval(&mut self) {
        self.pending_approval = None;
        self.approval_scroll = 0;
    }

    pub fn toggle_context_panel(&mut self) {
        self.show_context_panel = !self.show_context_panel;
    }

    fn record_transitions(&mut self, transitions: Vec<ContextStateTransition>) {
        for transition in transitions {
            self.context_transitions.push(transition);
        }
        let overflow = self
            .context_transitions
            .len()
            .saturating_sub(MAX_PANEL_TRANSITIONS);
        if overflow > 0 {
            self.context_transitions.drain(..overflow);
        }
    }

    /// Claim `(run_id, seq)` for application. Returns false when this event
    /// was already folded, either because the journal replay covered it (at
    /// or below the verified contiguous watermark) or because it is a
    /// redelivery of an event this view already applied.
    ///
    /// Only DURABLE events may be claimed. See
    /// [`agent_contracts::RuntimeEvent::is_live_only`] for why the live
    /// fragments must not be.
    fn claim_event(&mut self, run_id: RunId, seq: u64) -> bool {
        if run_id == self.run_id
            && let Some(watermark) = self.resynced_through_seq
            && seq <= watermark
        {
            return false;
        }
        let key = (run_id, seq);
        if !self.applied_index.insert(key) {
            return false;
        }
        self.applied_events.push_back(key);
        while self.applied_events.len() > MAX_APPLIED_EVENTS {
            if let Some(evicted) = self.applied_events.pop_front() {
                self.applied_index.remove(&evicted);
            }
        }
        true
    }

    /// Claim `(run_id, seq)` as the identity of a transcript row. The
    /// transcript outlives a projection rebuild, so this memory is
    /// deliberately NOT cleared by a resync: a replay re-applies events but
    /// must never append a row the operator already has.
    fn claim_message_row(&mut self, run_id: RunId, seq: u64) -> bool {
        let key = (run_id, seq);
        if !self.shown_message_index.insert(key) {
            return false;
        }
        self.shown_message_events.push_back(key);
        while self.shown_message_events.len() > MAX_SHOWN_MESSAGE_EVENTS {
            if let Some(evicted) = self.shown_message_events.pop_front() {
                self.shown_message_index.remove(&evicted);
            }
        }
        true
    }

    /// Claim `input_id` as the identity of a user bubble. Bounded like the
    /// other identity memories, and cleared with the event-derived rows it
    /// protects.
    fn claim_input_row(&mut self, input_id: RuntimeInputId) -> bool {
        if !self.shown_input_index.insert(input_id) {
            return false;
        }
        self.shown_input_events.push_back(input_id);
        while self.shown_input_events.len() > MAX_SHOWN_MESSAGE_EVENTS {
            if let Some(evicted) = self.shown_input_events.pop_front() {
                self.shown_input_index.remove(&evicted);
            }
        }
        true
    }

    /// Drop every event-derived row and the identities that guard them, so a
    /// replay rebuilds them from the journal. Session-local rows survive.
    fn drop_event_derived_rows(&mut self) {
        self.messages
            .retain(|message| message.event_identity.is_none());
        self.shown_message_events.clear();
        self.shown_message_index.clear();
        self.shown_input_events.clear();
        self.shown_input_index.clear();
    }

    /// Forget this run's applied identities so a journal replay re-applies
    /// its events. Used only by a rebuild, which first resets the folded
    /// fields; the transcript's row identities are intentionally kept.
    fn forget_applied_for_run(&mut self, run_id: RunId) {
        self.applied_events.retain(|key| key.0 != run_id);
        self.applied_index.retain(|key| key.0 != run_id);
    }

    /// Reset every event-derived field to its pre-run baseline so a journal
    /// replay can rebuild it through the same fold the live path uses.
    /// Purely local view state (draft input, scroll positions, the context
    /// panel toggle, an approval currently on screen) is not event-derived
    /// and is left alone, as is the append-only transcript.
    fn reset_event_derived_view(&mut self) {
        self.status_projection = agent_runtime::status::StatusProjection::default();
        self.status = "idle".into();
        self.tool_status = "none".into();
        self.busy = false;
        self.streaming = false;
        self.current_op = None;
        self.streaming_row_open = false;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.current_task = None;
        self.observed_task = None;
        self.unresolved_ack_debts = 0;
        self.last_checkpoint = None;
        self.result_card = ResultCard::default();
        self.archived_cards.clear();
        self.queued_input_id = None;
        self.context = ContextDiagnostics::default();
        self.context_selected.clear();
        self.context_transitions.clear();
        // R7: the transcript is rebuilt from the journal, not appended to it.
        self.drop_event_derived_rows();
        self.current_event = None;
    }

    /// Test-only view of the replay watermark.
    #[cfg(test)]
    fn resync_watermark_for_test(&self) -> Option<u64> {
        self.resynced_through_seq
    }

    /// Point the review card at `task_id`. When the current card belongs to
    /// a different task it is archived (boundedly) rather than merged, so
    /// one task's changes can never be read under another task's completion
    /// header.
    fn begin_card_for_task(&mut self, task_id: Option<TaskId>) {
        if self.result_card.task_id == task_id {
            return;
        }
        if self.result_card.is_empty() {
            self.result_card = ResultCard::default();
        } else {
            self.archived_cards
                .push(std::mem::take(&mut self.result_card));
            let overflow = self.archived_cards.len().saturating_sub(MAX_ARCHIVED_CARDS);
            if overflow > 0 {
                self.archived_cards.drain(..overflow);
            }
        }
        self.result_card.task_id = task_id;
    }

    /// The card `/review` renders: the focused task's own material when it
    /// has any, otherwise the most recently finished task's. Never a blend
    /// of two tasks.
    pub fn review_card(&self) -> Option<&ResultCard> {
        if !self.result_card.is_empty() {
            return Some(&self.result_card);
        }
        self.archived_cards.last()
    }

    pub fn apply_runtime_event(&mut self, envelope: RuntimeEventEnvelope) {
        // R1: a journal cursor is NOT a stream-fragment identity. The contract
        // already says so — `RuntimeEvent::is_live_only()` documents that
        // `ModelDelta`/`ModelRetrying` repeat the preceding durable cursor and
        // must not be filtered by a delivery cursor (agent-host already
        // honours it). This consumer did not, so the first event claimed
        // `(RunId, seq)` and every live fragment afterwards was dropped as a
        // redelivery — no streamed text, no retry progress, and the operation
        // fence never even ran.
        //
        // Their belonging is therefore decided by the identity they DO carry:
        // `(TurnId, OperationId, generation)` via the `current_op` fence in
        // `apply_event`.
        if !envelope.event.is_live_only() && !self.claim_event(envelope.run_id, envelope.seq) {
            // Durable event already folded, in full: skipping it entirely is
            // what stops a redelivered terminal event from re-activating a
            // superseded operation or re-counting its tokens.
            return;
        }
        self.apply_event(envelope);
    }

    /// The single event fold. Live consumption and journal replay both go
    /// through here, so a replayed view cannot be built by a different rule
    /// than the live one.
    fn apply_event(&mut self, envelope: RuntimeEventEnvelope) {
        let run_id = envelope.run_id;
        let seq = envelope.seq;
        // R7: every row this event appends carries its identity, so a replay
        // can drop and rebuild exactly the event-derived part of the view.
        self.current_event = Some((run_id, seq));
        self.status_projection.fold(&envelope.event);
        match envelope.event {
            RuntimeEvent::RunStarted => self.status = "ready".into(),
            // EXEC-3 (E07): restore succeeded, but some protected evidence
            // references could not be admitted — surfaced, never inferred.
            RuntimeEvent::RestoreEvidenceDegraded { unadmitted_runs } => {
                self.status = format!(
                    "restored, but {} protected reference(s) are unreadable: {}",
                    unadmitted_runs.len(),
                    unadmitted_runs.join(", ")
                );
            }
            RuntimeEvent::UserMessageAccepted { input } => {
                if input.appears_in_user_transcript() {
                    // Row identity is the input id when the runtime stamped
                    // one (a queued input and its later applied record share
                    // it); otherwise this event. Never the message text.
                    let already_shown = match input.input_id {
                        Some(id) => !self.claim_input_row(id),
                        None => !self.claim_message_row(run_id, seq),
                    };
                    if !already_shown {
                        self.push_message(UiRole::User, input.preview.clone());
                    }
                    if input.is_applied() {
                        // The queued slot reaching the applied state is a
                        // visible disposition change, not just a status flip.
                        if input
                            .input_id
                            .is_some_and(|id| self.queued_input_id == Some(id))
                        {
                            self.queued_input_id = None;
                            self.push_system("queued input applied; running now".into());
                        }
                        self.busy = true;
                        self.status = "working".into();
                        // Follow the tail: scroll 0 is "latest", not the
                        // opening SYSTEM banners.
                        self.scroll = 0;
                    } else {
                        if let Some(id) = input.input_id
                            && self.queued_input_id != Some(id)
                        {
                            self.queued_input_id = Some(id);
                            self.push_system(
                                "input queued; it runs after the current turn (/cancel stops the turn)"
                                    .into(),
                            );
                        }
                        self.status = "queued".into();
                    }
                } else if input.lifecycle == agent_contracts::InputLifecycle::Rejected {
                    self.push_system(format!("input rejected: {}", input.preview));
                } else if input.lifecycle == agent_contracts::InputLifecycle::InterruptCommitted {
                    self.push_system(format!("turn interrupted: {}", input.preview));
                }
            }
            RuntimeEvent::FocusChanged { task_id, goal } => {
                // A different task gets a different review card. Keeping one
                // card across a task switch is what let task B's changes
                // render under task A's durable completion header.
                self.begin_card_for_task(Some(task_id));
                self.observed_task = Some(task_id);
                self.push_system(format!("focus -> task {task_id}: {goal}"));
            }
            RuntimeEvent::FocusCleared => {
                self.begin_card_for_task(None);
                self.push_system("focus cleared (task suspended)".into());
            }
            RuntimeEvent::TaskToolRequirementsChanged {
                task_id,
                revision,
                requirements,
            } => {
                self.push_system(format!(
                    "task {task_id} tool requirements r{revision}: {} entries",
                    requirements.len()
                ));
            }
            RuntimeEvent::TaskAnchorChanged {
                task_id,
                revision,
                changed_fields,
                patch_kind,
            } => {
                // An anchor revision belongs to its own task (F06): a
                // background anchor change on one task must never steal the
                // focused-task slot of another. The audit row below still
                // records every event.
                if self
                    .current_task
                    .is_none_or(|(current, _)| current == task_id)
                {
                    self.current_task = Some((task_id, revision));
                }
                // Bounded audit row: the event names the moved fields and
                // the authority split (autonomous vs boundary), never the
                // anchor content (which lives in the checkpoint).
                self.push_system(format!(
                    "task {task_id} anchor r{revision} updated ({patch_kind:?}): {}",
                    changed_fields.join(", ")
                ));
            }
            RuntimeEvent::TaskProgressUpdated {
                task_id,
                accepted,
                anchor_revision,
                changed_fields,
                reason,
            } => {
                // task.manage 的确定性结果：拒绝时状态未变，reason 说明
                // 类别（如过期基准修订），便于从事件流核对 CAS 结果。
                if accepted {
                    if self
                        .current_task
                        .is_none_or(|(current, _)| current == task_id)
                    {
                        self.current_task = Some((task_id, anchor_revision));
                    }
                    if changed_fields.is_empty() {
                        self.push_system(format!(
                            "task {task_id} progress already current at r{anchor_revision}"
                        ));
                    } else {
                        self.push_system(format!(
                            "task {task_id} progress recorded at r{anchor_revision}: {}",
                            changed_fields.join(", ")
                        ));
                    }
                } else {
                    self.push_system(format!("task {task_id} progress refused: {reason}"));
                }
            }
            RuntimeEvent::TaskResumeCommitted {
                task_id,
                anchor_revision,
                debt,
                ..
            } => {
                self.push_system(format!(
                    "task {task_id} resume installed at r{anchor_revision}: {}",
                    debt.join(", ")
                ));
            }
            RuntimeEvent::CheckpointDurable {
                bytes, artifact, ..
            } => {
                self.push_system(format!("checkpoint durable: {artifact} ({bytes}B)"));
            }
            RuntimeEvent::CheckpointWriteFailed { reason } => {
                self.push_system(format!("checkpoint write failed: {reason}"));
            }
            RuntimeEvent::CompletionOpportunity {
                disposition,
                task_id,
                key,
                reason,
                ..
            } => {
                // Advisory lifecycle row: surface only the state changes a
                // user would act on, not every not_ready consult.
                if !matches!(
                    disposition,
                    agent_contracts::CompletionOpportunityDisposition::NotReady
                ) {
                    self.push_system(format!(
                        "completion opportunity {:?} for task {task_id} (key {key}): {reason}",
                        disposition
                    ));
                }
            }
            RuntimeEvent::TaskContinuationStarted { task_id, .. } => {
                self.push_system(format!("task {task_id} continuation started"));
            }
            RuntimeEvent::Pinned { content } => {
                self.push_system(format!("pinned: {content}"));
            }
            RuntimeEvent::ContextPrepared {
                diagnostics,
                selected,
                ..
            } => {
                self.context = diagnostics;
                self.context_selected = selected;
                self.status = "model context prepared".into();
            }
            RuntimeEvent::ContextDegraded {
                model_round,
                required_misses,
                optional_misses,
                ..
            } => {
                let required = required_misses.total();
                let optional = optional_misses.total();
                self.status = if required > 0 {
                    "required context unavailable".into()
                } else {
                    "context partially unavailable".into()
                };
                self.push_system(format!(
                    "context degraded in model round {model_round}: required misses {required}, optional misses {optional}"
                ));
            }
            RuntimeEvent::ContextConsumed { ack } => {
                self.status = format!(
                    "model consumed {} context items and {} external refs",
                    ack.item_ids.len(),
                    ack.external_item_ids.len()
                );
            }
            RuntimeEvent::ContextMaintained { report, .. } => {
                self.context = report.diagnostics;
                self.record_transitions(report.transitions);
            }
            RuntimeEvent::ContextCompacted {
                reason,
                input_tokens,
                output_tokens,
                source_items,
                ..
            } => {
                self.push_system(format!(
                    "context compacted ({reason:?}): {input_tokens}->{output_tokens} tokens, {source_items} sources"
                ));
            }
            RuntimeEvent::ContextGc { report } => {
                let evicted_buffer = report.diagnostics.warm_items;
                self.context = report.diagnostics;
                // Evictions and reactivations are lifecycle events worth
                // showing in the panel: GC must be explainable.
                for eviction in report.evictions {
                    self.context_transitions.push(ContextStateTransition {
                        item_id: eviction.item_id,
                        kind: eviction.kind,
                        scope: eviction.scope,
                        from: agent_contracts::AttentionState::Archived,
                        to: agent_contracts::AttentionState::Archived,
                        turn: self.context.turn,
                        reason: format!(
                            "evicted (gen {}): {}",
                            eviction.generation, eviction.reason
                        ),
                    });
                }
                for reactivation in report.reactivations {
                    self.context_transitions.push(ContextStateTransition {
                        item_id: reactivation.item_id,
                        kind: reactivation.kind,
                        scope: reactivation.scope,
                        from: agent_contracts::AttentionState::Archived,
                        to: agent_contracts::AttentionState::Active,
                        turn: self.context.turn,
                        reason: format!("reactivated: {}", reactivation.reason),
                    });
                }
                let overflow = self
                    .context_transitions
                    .len()
                    .saturating_sub(MAX_PANEL_TRANSITIONS);
                if overflow > 0 {
                    self.context_transitions.drain(..overflow);
                }
                if report.evicted > 0 || report.reactivated > 0 {
                    self.push_system(format!(
                        "context gc: marked {} roots, evicted {}, reactivated {} (resident {}, evicted buffer {})",
                        report.marked_roots,
                        report.evicted,
                        report.reactivated,
                        report.resident,
                        evicted_buffer,
                    ));
                }
            }
            RuntimeEvent::StorageGc { report } => {
                // Storage GC is the only place information is permanently
                // deleted; surface the conservative report so every
                // deletion is observable.
                if report.deleted > 0 || report.io_errors > 0 {
                    self.push_system(format!(
                        "storage gc: scanned {}, permanently deleted {} (io errors {})",
                        report.scanned, report.deleted, report.io_errors,
                    ));
                }
            }
            RuntimeEvent::ToolSurfacePlanned { report } => {
                let ready = matches!(report.status, ToolSurfacePlanStatus::Ready);
                let message = bounded_tool_surface_message(&report);
                if ready {
                    self.status = "tool surface prepared".into();
                } else {
                    // An unsatisfiable report means no provider operation was
                    // started, so clear any stale live-stream fence.
                    self.current_op = None;
                    self.busy = false;
                    self.streaming = false;
                    self.status = "tool surface blocked".into();
                }
                self.push_system(message);
            }
            RuntimeEvent::ToolLeasesReconciled {
                boundary, report, ..
            } => {
                if report.released_to_warm > 0 {
                    self.push_system(format!(
                        "tool leases reconciled at {boundary:?}: {} optional schema(s) left the surface, {} retained by a live root",
                        report.released_to_warm, report.retained_by_root
                    ));
                }
            }
            RuntimeEvent::ModelStarted {
                turn_id,
                operation_id,
                generation,
                surface_revision,
                model_round,
                ..
            } => {
                self.current_op = Some((turn_id, operation_id, generation));
                self.busy = true;
                self.streaming = false;
                self.status = if surface_revision == 0 {
                    "model".into()
                } else {
                    format!("model round {model_round} (surface {surface_revision})")
                };
            }
            RuntimeEvent::ModelDelta {
                turn_id,
                operation_id,
                generation,
                delta,
            } => {
                // Generation fence: only the current operation's stream may
                // render. A late delta from a cancelled turn is dropped.
                if self.current_op != Some((turn_id, operation_id, generation)) {
                    return;
                }
                self.streaming = true;
                self.status = "model (streaming)".into();
                match self.messages.last_mut() {
                    Some(last) if last.role == UiRole::Assistant && self.streaming_row_open => {
                        last.content.push_str(&delta);
                    }
                    _ => {
                        self.push_message(UiRole::Assistant, delta);
                    }
                }
                self.streaming_row_open = true;
            }
            RuntimeEvent::AssistantMessage { content } => {
                self.streaming = false;
                // Dedup by event identity, never by the reply text: two
                // turns may legitimately answer with identical wording and
                // both must stay in the transcript.
                if self.claim_message_row(run_id, seq) {
                    // Only finalize a row this turn's own stream opened. An
                    // earlier turn's row must never be overwritten just
                    // because it happens to be the last one.
                    if self.streaming_row_open
                        && let Some(last) = self.messages.last_mut()
                        && last.role == UiRole::Assistant
                    {
                        last.content = content;
                    } else {
                        self.push_message(UiRole::Assistant, content);
                    }
                }
                self.streaming_row_open = false;
            }
            RuntimeEvent::OperationAccepted { .. } => {
                // This is an authority/discovery event for authorized
                // Platform observers. It deliberately does not mean the tool
                // body has started, so the UI waits for `ToolStarted` before
                // changing user-visible execution state.
            }
            RuntimeEvent::ToolStarted { call } => {
                self.busy = true;
                self.tool_status = format!("running {}", call.name);
            }
            RuntimeEvent::ToolFinished { output, .. } => {
                self.tool_status = format!("{}: {}", output.tool_name, output.summary);
                // Result-card folding: mutating tools are identified by the
                // structured path they stamped; verification-class tools
                // record their real outcome. Display material only — the
                // durable ledger stays in changes.jsonl and the journal.
                if is_mutating_tool(&output.tool_name)
                    && let Some(path) = output.file_path()
                {
                    let entry = CardChangedFile {
                        path: bounded_card_line(path),
                        tool: output.tool_name.clone(),
                        ok: output.ok,
                        // Attributed to the card's own task, so a path that
                        // both tasks touched is still two facts, not one.
                        task_id: self.result_card.task_id,
                    };
                    let slot = self
                        .result_card
                        .changed_files
                        .iter()
                        .position(|file| file.path == entry.path && file.tool == entry.tool);
                    match slot {
                        Some(index) => self.result_card.changed_files[index] = entry,
                        None => {
                            if self.result_card.changed_files.len() < MAX_CARD_FILES {
                                self.result_card.changed_files.push(entry);
                            } else {
                                // The cap refused the entry: count it so the
                                // omission is visible rather than silent.
                                self.result_card.omitted_files += 1;
                            }
                        }
                    }
                }
                if is_verification_tool(&output.tool_name) {
                    // R5: the failure is counted BEFORE the display cap can
                    // refuse the entry. `checks` is a bounded window; the
                    // account must not read as "0 FAILED" because the window
                    // was full. Counting here is safe against redelivery:
                    // `apply_event` runs at most once per durable event.
                    if !output.ok {
                        self.result_card.failed_checks_total =
                            self.result_card.failed_checks_total.saturating_add(1);
                    }
                    if self.result_card.checks.len() < MAX_CARD_CHECKS {
                        self.result_card.checks.push(CardCheck {
                            tool: output.tool_name.clone(),
                            ok: output.ok,
                            summary: bounded_card_line(&output.summary),
                            artifact: output.artifact_ref.clone(),
                            task_id: self.result_card.task_id,
                        });
                    } else {
                        self.result_card.omitted_checks += 1;
                    }
                }
                let text = tool_transcript_text(&output);
                // Identity-deduped like the other transcript rows: a
                // replayed tool result must not append a second row, but
                // two distinct calls that printed the same summary must.
                if self.claim_message_row(run_id, seq) {
                    self.push_message(UiRole::Tool, text);
                }
            }
            RuntimeEvent::ToolScopeClosed {
                scope_id,
                transitions,
            } => {
                // A tool frame closed: show the lifecycle transitions the
                // close produced (promotions out of the frame) in the same
                // panel as every other transition.
                self.push_system(format!("tool scope {scope_id} closed"));
                self.record_transitions(transitions);
            }
            RuntimeEvent::ExecutionFrontier { .. } => {
                // 收敛账目不进消息面板：advisory 已由 TASK PROGRESS 渲染，
                // 这里只是保持 match 穷尽。
            }
            RuntimeEvent::ExecutionBatchSettled { .. } => {
                // Body-free action accounting belongs to eval/audit, not the
                // conversational panel.
            }
            RuntimeEvent::ProtocolBodyCacheStats { .. } => {
                // 正文缓存账目同理：指标归 eval 聚合，UI 不重复渲染。
            }
            RuntimeEvent::ExecutionObligation { .. } => {
                // 义务账目同理：typed 计数归 eval 聚合，advisory 已由
                // TASK PROGRESS 渲染。
            }
            RuntimeEvent::ExecutionNegativeFact { .. } => {
                // Speculative absence lifecycle is an audit/eval signal. Its
                // truthful no-dispatch result is already visible as the
                // corresponding ToolFinished row.
            }
            RuntimeEvent::ExecutionVerificationPass { .. } => {
                // Exact PASS lifecycle is audit/eval data. A reused result is
                // already visible through its truthful ToolFinished row.
            }
            RuntimeEvent::AcceptanceReceiptsRecorded { .. } => {
                // Criterion receipt lifecycle is bounded audit data; the
                // resulting readiness is already projected in task progress.
            }
            RuntimeEvent::Diagnostics { diagnostics } => {
                self.context = diagnostics.clone();
                self.push_system(format!(
                    "context total={} active={} cooling={} archived={} dropped={} active≈{} tok turn={} round={}",
                    diagnostics.total_items,
                    diagnostics.active_items,
                    diagnostics.cooling_items,
                    diagnostics.archived_items,
                    diagnostics.tombstoned_items,
                    diagnostics.approx_active_tokens,
                    diagnostics.turn,
                    diagnostics.tool_round,
                ));
            }
            RuntimeEvent::Warning { message } => self.push_system(format!("warning: {message}")),
            RuntimeEvent::Error { message } => {
                self.busy = false;
                self.status = "error".into();
                self.push_system(format!("error: {message}"));
            }
            RuntimeEvent::TaskCompleted {
                task_id,
                anchor_revision,
                summary,
                ..
            } => {
                self.current_task = None;
                // The completion header belongs to the task it names, so the
                // card is bound to that task: material recorded for a
                // different task must never render under this header.
                self.result_card.task_id = Some(task_id);
                self.observed_task = Some(task_id);
                self.result_card.completion = Some(CardCompletion {
                    task_id,
                    anchor_revision,
                    summary: bounded_card_line(&summary),
                });
                self.push_system(format!(
                    "task {task_id} completed (anchor r{anchor_revision}): {summary}"
                ));
                self.persist_result_card();
            }
            RuntimeEvent::CompletionCommitFailed {
                task_id,
                retryable,
                reason,
            } => {
                self.status = "completion failed".into();
                self.push_system(format!(
                    "task {task_id} completion commit failed{}: {reason}",
                    if retryable { " (retryable)" } else { "" }
                ));
            }
            RuntimeEvent::RuntimeCommitBarrier { .. } => {
                // Durability/replay marker. The paired lifecycle event owns
                // the user-visible state transition.
            }
            RuntimeEvent::TurnCompleted => {
                // The turn is over: any delta still in flight belongs to a
                // superseded operation and must not render.
                self.current_op = None;
                self.busy = false;
                self.streaming = false;
                self.status = "idle".into();
                self.tool_status = "none".into();
                // Under OperatorClosureOnly a normal final ends the turn,
                // not the task: name the state so a produced result is
                // visibly awaiting review instead of reading as done.
                if self.current_task.is_some() {
                    self.push_system(
                        "turn ended; the task stays active awaiting operator review (=/done closes durably)"
                            .into(),
                    );
                }
            }
            RuntimeEvent::TurnCancelled { reason, .. } => {
                self.current_op = None;
                self.busy = false;
                self.streaming = false;
                self.status = "cancelled".into();
                self.tool_status = "none".into();
                self.push_system(format!("turn cancelled ({reason:?})"));
            }
            RuntimeEvent::TurnCommitFailed { phase, message } => {
                // The model answered, but the runtime did not durably commit
                // the turn: surface the failure instead of an idle state.
                self.current_op = None;
                self.busy = false;
                self.streaming = false;
                self.status = "commit_failed".into();
                self.tool_status = "none".into();
                self.push_system(format!(
                    "turn commit failed at {phase}: {message} — recovery required"
                ));
            }
            RuntimeEvent::RecoveryRequired => {
                self.busy = false;
                self.status = "recovery_required".into();
            }
            RuntimeEvent::EffectAckDebt { debt } => {
                self.busy = false;
                self.status = "recovery_required".into();
                self.unresolved_ack_debts = self.unresolved_ack_debts.saturating_add(1);
                self.push_system(format!(
                    "effect {} acknowledged as {} but its acknowledgement is unresolved: {}",
                    debt.effect_id,
                    debt.settlement.label(),
                    debt.error
                ));
            }
            RuntimeEvent::EffectAckDebtResolved { debt, resolution } => {
                self.unresolved_ack_debts = self.unresolved_ack_debts.saturating_sub(1);
                let kind = match &resolution {
                    agent_contracts::EffectReconciliation::NotManaged => "not managed",
                    agent_contracts::EffectReconciliation::NotApplied { .. } => "not applied",
                    agent_contracts::EffectReconciliation::Applied { .. } => "applied",
                    agent_contracts::EffectReconciliation::CompletedValue { .. } => {
                        "completed value"
                    }
                    agent_contracts::EffectReconciliation::Ambiguous { .. } => "ambiguous",
                };
                self.push_system(format!(
                    "effect {} ack debt reconciled as {}: reservation {}",
                    debt.effect_id, kind, debt.reservation_id
                ));
            }
            RuntimeEvent::Failure {
                class,
                retryable,
                message,
            } => {
                // A typed execution failure. A retryable failure leaves the
                // turn busy (the runtime may retry); a terminal one stops
                // the spinner. The class drives policy; the message is a
                // bounded diagnostic only.
                self.busy = retryable;
                self.status = format!("failed ({class:?})");
                self.push_system(format!(
                    "execution failed ({class:?}{}): {message}",
                    if retryable { ", retryable" } else { "" }
                ));
                if class == agent_contracts::RuntimeFailureClass::RoundBudget {
                    // The runtime settled the turn as a deliberate refusal,
                    // not a fault: name the recovery path instead of leaving
                    // a bare "budget" dead-end. /continue starts a new
                    // segment; cold resume needs a saved checkpoint.
                    self.push_system(
                        "stopped at the round budget; /continue starts a new segment, /plan shows \
                         remaining work — run /checkpoint first if you may need to restart the \
                         process"
                            .into(),
                    );
                }
                if !retryable {
                    if self.result_card.failures.len() < MAX_CARD_FAILURES {
                        self.result_card.failures.push(CardFailure {
                            summary: bounded_card_line(&format!("({class:?}) {message}")),
                            task_id: self.result_card.task_id,
                        });
                    } else {
                        // A late failure must not vanish just because the
                        // card is full: the omission is counted so `/review`
                        // can say material was dropped.
                        self.result_card.omitted_failures += 1;
                    }
                }
            }
            RuntimeEvent::ModelRetrying {
                attempt, delay_ms, ..
            } => {
                // Live-only progress signal: the transport is inside its
                // bounded retry policy. Keep the spinner honest without
                // clearing any in-flight operation state.
                self.status = "model (retrying)".into();
                self.push_system(format!(
                    "model attempt {attempt} failed, retrying in {delay_ms} ms"
                ));
            }
            RuntimeEvent::RuntimeRestored {
                checkpoint_version,
                restored_run_id,
                rebased_tasks,
                capabilities_applied,
                ..
            } => {
                // A live restore committed: surface the bounded audit
                // summary as a system line; full detail stays in the event.
                self.status = "restored".into();
                self.push_system(format!(
                    "restore committed: checkpoint v{checkpoint_version} from run {restored_run_id}, \
                     {rebased_tasks} task requirement set(s) rebased, capabilities {}",
                    if capabilities_applied {
                        "applied"
                    } else {
                        "unchanged"
                    }
                ));
            }
            RuntimeEvent::ModelUsed {
                input_tokens,
                output_tokens,
                attempts,
                retries,
                ..
            } => {
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                let retry_note = if retries > 0 {
                    format!(" attempts={attempts} retries={retries} (tokens lower-bound)")
                } else {
                    String::new()
                };
                self.push_system(format!(
                    "model used: {input_tokens} in + {output_tokens} out (run: {} in + {} out){retry_note}",
                    self.input_tokens, self.output_tokens
                ));
            }
            RuntimeEvent::ShadowDecision {
                call_name,
                legacy_allowed,
                shadow,
            } => {
                // ACI v2 shadow-mode audit row: what the intent-derived gate
                // would decide beside the legacy decision that ran. Bounded
                // to the verdict, never the arguments.
                let shadow_label = match &shadow {
                    agent_contracts::ShadowVerdict::Granted { grant_id, .. } => {
                        format!("v2 grant '{grant_id}'")
                    }
                    agent_contracts::ShadowVerdict::Denied { .. } => "v2 deny".to_string(),
                };
                self.push_system(format!(
                    "shadow approval: {call_name} legacy={} -> {shadow_label}",
                    if legacy_allowed { "allow" } else { "deny" }
                ));
            }
            RuntimeEvent::LeaseIssued {
                lease_id,
                call_name,
                grant_id,
                expires_at_ms,
            } => {
                // ACI v2 §6 audit row: a side-effecting call got a bounded
                // commit-time authorization. The expiry makes the window
                // visible; a grant name (when the v2 gate granted the
                // intent) explains the coverage.
                let covered = match &grant_id {
                    Some(grant_id) => format!(" via grant '{grant_id}'"),
                    None => String::new(),
                };
                self.push_system(format!(
                    "authority lease {lease_id} for {call_name}{covered} expires at {expires_at_ms}ms"
                ));
            }
            RuntimeEvent::RunCompleted => {
                self.busy = false;
                self.status = "stopped".into();
            }
            RuntimeEvent::ContextFrameShadow { manifest } => {
                // Shadow measurement: surface a compact line only; the
                // full manifest lives in the event journal for the gates.
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest) {
                    let digest = value["frame_digest"]
                        .as_str()
                        .and_then(|digest| digest.get(..12))
                        .unwrap_or("?")
                        .to_string();
                    self.push_system(format!(
                        "shadow frame: digest {} tokens {} misses {} dups {}",
                        digest,
                        value["approx_tokens_total"].as_u64().unwrap_or(0),
                        value["required_misses"].as_u64().unwrap_or(0),
                        value["duplicates_removed"].as_u64().unwrap_or(0),
                    ));
                }
            }
        }
    }
}

/// Bounded display of the latest result card for `/review`. It names what
/// changed (from the session's own tool calls), what was actually checked
/// and with what outcome, what failed, and whether the task durably
/// completed. Pre-existing workspace modifications are explicitly NOT
/// attributed; nothing here triggers a model call or a write.
pub fn format_result_lines(card: &ResultCard) -> Vec<String> {
    if card.is_empty() {
        return vec![
            "no result material yet — complete a task (or run tools) and /review will show \
             what changed, what was checked and what remains"
                .into(),
        ];
    }
    let mut lines = Vec::new();
    let scope = match card.task_id {
        Some(task_id) => format!("task {task_id}"),
        None => "no focused task".to_string(),
    };
    match &card.completion {
        Some(completion) => {
            lines.push(format!(
                "review: task {} durably completed at anchor r{}: {}",
                completion.task_id, completion.anchor_revision, completion.summary
            ));
        }
        None => {
            lines.push(format!(
                "review: {scope} — no durable task completion this session; awaiting operator \
                 review (/done closes durably). Below is only this task's own material"
            ));
        }
    }
    if card.changed_files.is_empty() {
        lines.push("  changed files: none recorded from this session's tool calls".into());
    } else {
        lines.push("  changed files (this session's tools only):".into());
        lines.push(format!(
            "    scope: {scope} — showing {} of {} recorded",
            card.changed_files.len().min(MAX_CARD_FILES),
            card.changed_files.len().saturating_add(card.omitted_files)
        ));
        for file in card.changed_files.iter().take(MAX_CARD_FILES) {
            lines.push(format!(
                "    {} {} [{}]",
                file.tool,
                bounded_card_line(&file.path),
                if file.ok { "ok" } else { "FAILED" }
            ));
        }
        if card.omitted_files > 0 {
            lines.push(format!(
                "    …and {} more files were NOT shown (beyond the display cap)",
                card.omitted_files
            ));
        }
    }
    if card.checks.is_empty() && card.omitted_checks == 0 {
        lines.push(
            "  checks: none recorded — a green transcript is not verification; run \
             verify.run or an equivalent check"
                .into(),
        );
    } else {
        lines.push("  checks run:".into());
        // R5: the account vs the window are named separately. A failure the
        // cap refused is still a failure, so the full count comes from the
        // pre-cap tally; the window count says what is actually listed.
        lines.push(format!(
            "    scope: {scope} — {} recorded, {} FAILED (of which {} in the {}-row \
             display window), {} not shown",
            card.total_checks(),
            card.failed_checks_total,
            card.failed_checks_in_window(),
            MAX_CARD_CHECKS,
            card.omitted_checks,
        ));
        for check in card.checks.iter().take(MAX_CARD_CHECKS) {
            let artifact = check
                .artifact
                .as_deref()
                .map(|artifact| format!(" -> {artifact}"))
                .unwrap_or_default();
            lines.push(format!(
                "    {} [{}] {}{artifact}",
                check.tool,
                if check.ok { "ok" } else { "FAILED" },
                check.summary
            ));
        }
        if card.omitted_checks > 0 {
            lines.push(format!(
                "    …and {} more checks were NOT shown (beyond the display cap); \
                 {} recorded check(s) FAILED in total",
                card.omitted_checks, card.failed_checks_total
            ));
        }
    }
    if !card.failures.is_empty() || card.omitted_failures > 0 {
        lines.push("  unresolved failures this session:".into());
        for failure in card.failures.iter().take(MAX_CARD_FAILURES) {
            lines.push(format!("    {}", failure.summary));
        }
        if card.omitted_failures > 0 {
            lines.push(format!(
                "    …and {} more failures were NOT shown (beyond the display cap)",
                card.omitted_failures
            ));
        }
    }
    lines.push(
        "  not attributed: pre-existing workspace modifications; /plan shows remaining \
         work — [x]-style progress is the model's report, not verification"
            .into(),
    );
    lines
}

/// R1 fixtures: a stable run identity plus increasing journal sequences, so a
/// fold test models a REAL single-run stream. The previous shape minted a fresh
/// `RunId` per event while pinning `seq = 1`, which silently bypassed the
/// `(RunId, seq)` identity the consumer applies — that is why the live-stream
/// regression had no failing test.
#[cfg(test)]
fn fixture_run() -> RunId {
    static RUN: std::sync::OnceLock<RunId> = std::sync::OnceLock::new();
    *RUN.get_or_init(RunId::new)
}

#[cfg(test)]
fn fixture_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_window_stays_bounded_and_keeps_the_newest() {
        let mut app = AppState::new(RunId::new());
        for index in 0..(MAX_RENDERED_MESSAGES + 50) {
            app.push_system(format!("row-{index}"));
        }
        assert_eq!(app.messages.len(), MAX_RENDERED_MESSAGES);
        assert!(app.messages[0].content.ends_with(&format!("row-{}", 50)));
        assert_eq!(
            app.messages.last().unwrap().content,
            format!("row-{}", MAX_RENDERED_MESSAGES + 49)
        );
    }
    use agent_contracts::{
        RuntimeEventEnvelope, ToolSurfaceBlock, ToolSurfaceOmission, ToolSurfaceOmissionReason,
        ToolSurfaceSelection, ToolSurfaceSourceRevisions,
    };

    fn envelope(event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: fixture_run(),
            seq: fixture_seq(),
            timestamp_ms: 0,
            event,
        }
    }

    fn delta(turn: TurnId, op: OperationId, generation: u64, text: &str) -> RuntimeEvent {
        RuntimeEvent::ModelDelta {
            turn_id: turn,
            operation_id: op,
            generation,
            delta: text.into(),
        }
    }

    /// R1: `LiveSink` publishes every fragment with the SAME journal cursor as
    /// the durable `ModelStarted` that opened the stream. A durable-sequence
    /// identity must therefore not swallow them, or normal streamed output and
    /// retry progress vanish — the regression this review found. The fragments
    /// are validated by the identity they DO carry
    /// (`TurnId`/`OperationId`/`generation`), not by a journal cursor.
    #[test]
    fn live_fragments_reusing_the_start_cursor_still_render() {
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        let turn = TurnId::new();
        let op = OperationId::new();
        // The durable start event occupies a real journal sequence...
        let cursor = 100u64;
        let live = |event: RuntimeEvent| RuntimeEventEnvelope {
            run_id,
            seq: cursor,
            timestamp_ms: 0,
            event,
        };
        app.apply_runtime_event(live(RuntimeEvent::ModelStarted {
            turn_id: turn,
            operation_id: op,
            generation: 1,
            surface_revision: 0,
            model_round: 1,
            prompt_layers: Default::default(),
            turn_checkpoint: Default::default(),
        }));

        // ...and every live fragment repeats that same cursor.
        app.apply_runtime_event(live(delta(turn, op, 1, "第一段")));
        let transcript = |app: &AppState| -> String {
            app.messages
                .iter()
                .filter(|message| message.role == UiRole::Assistant)
                .map(|message| message.content.clone())
                .collect()
        };
        assert!(
            transcript(&app).contains("第一段"),
            "a live fragment sharing the start cursor must still render: {:?}",
            transcript(&app)
        );

        app.apply_runtime_event(live(RuntimeEvent::ModelRetrying {
            turn_id: turn,
            operation_id: op,
            generation: 1,
            attempt: 2,
            delay_ms: 50,
        }));
        assert!(
            app.status.contains("retrying"),
            "the retry progress signal must reach the view: {}",
            app.status
        );

        app.apply_runtime_event(live(delta(turn, op, 1, "第二段")));
        let text = transcript(&app);
        assert!(
            text.contains("第一段") && text.contains("第二段"),
            "both fragments must be visible in order: {text:?}"
        );
    }

    /// The counterpart of R1: exempting live fragments from the durable
    /// identity must NOT weaken the operation fence — a fragment from a
    /// superseded generation is still dropped.
    #[test]
    fn a_live_fragment_from_a_superseded_generation_is_still_dropped() {
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        let turn = TurnId::new();
        let current = OperationId::new();
        let superseded = OperationId::new();
        let cursor = 200u64;
        let live = |event: RuntimeEvent| RuntimeEventEnvelope {
            run_id,
            seq: cursor,
            timestamp_ms: 0,
            event,
        };
        app.apply_runtime_event(live(RuntimeEvent::ModelStarted {
            turn_id: turn,
            operation_id: current,
            generation: 7,
            surface_revision: 0,
            model_round: 1,
            prompt_layers: Default::default(),
            turn_checkpoint: Default::default(),
        }));
        app.apply_runtime_event(live(delta(turn, current, 7, "current")));
        // Same cursor, older generation: the fence still rejects it.
        app.apply_runtime_event(live(delta(turn, superseded, 2, "SUPERSEDED")));
        let text: String = app
            .messages
            .iter()
            .filter(|message| message.role == UiRole::Assistant)
            .map(|message| message.content.clone())
            .collect();
        assert!(text.contains("current"), "{text:?}");
        assert!(
            !text.contains("SUPERSEDED"),
            "the operation fence must still drop a superseded fragment: {text:?}"
        );
    }

    #[test]
    fn late_deltas_from_a_superseded_operation_are_dropped() {
        let mut app = AppState::new(RunId::new());

        let turn = TurnId::new();
        let op_a = OperationId::new();
        let op_b = OperationId::new();
        app.apply_runtime_event(envelope(RuntimeEvent::ModelStarted {
            turn_id: turn,
            operation_id: op_a,
            generation: 3,
            surface_revision: 8,
            model_round: 2,
            turn_checkpoint: Default::default(),
            prompt_layers: Default::default(),
        }));

        // The current operation's deltas render.
        app.apply_runtime_event(envelope(delta(turn, op_a, 3, "hello ")));
        app.apply_runtime_event(envelope(delta(turn, op_a, 3, "world")));
        let rendered: String = app
            .messages
            .iter()
            .filter(|m| m.role == UiRole::Assistant)
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(rendered, "hello world");

        // A delta from a superseded operation (a cancelled turn's provider
        // still flushing) must not leak into the transcript.
        app.apply_runtime_event(envelope(delta(turn, op_b, 3, "LATE")));
        let rendered: String = app
            .messages
            .iter()
            .filter(|m| m.role == UiRole::Assistant)
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(rendered, "hello world", "stale deltas must be dropped");

        // After the turn ends, even the old operation's own late deltas are
        // dropped.
        app.apply_runtime_event(envelope(RuntimeEvent::TurnCompleted));
        app.apply_runtime_event(envelope(delta(turn, op_a, 3, "STALE")));
        let rendered: String = app
            .messages
            .iter()
            .filter(|m| m.role == UiRole::Assistant)
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(rendered, "hello world", "post-turn deltas must be dropped");
    }

    #[test]
    fn turn_cancelled_clears_live_operation_without_claiming_completion() {
        let mut app = AppState::new(RunId::new());
        let turn = TurnId::new();
        let operation = OperationId::new();
        app.apply_runtime_event(envelope(RuntimeEvent::ModelStarted {
            turn_id: turn,
            operation_id: operation,
            generation: 4,
            surface_revision: 1,
            model_round: 1,
            turn_checkpoint: Default::default(),
            prompt_layers: Default::default(),
        }));

        app.apply_runtime_event(envelope(RuntimeEvent::TurnCancelled {
            turn_id: turn,
            task_id: None,
            operation_id: Some(operation),
            cancelled_generation: 4,
            effective_generation: 5,
            reason: agent_contracts::TurnCancellationReason::Requested,
        }));

        assert!(!app.busy);
        assert!(!app.streaming);
        assert_eq!(app.status, "cancelled");
        assert!(app.current_op.is_none());
    }

    #[test]
    fn tool_surface_event_is_rendered_with_defensive_bounds() {
        let mut app = AppState::new(RunId::new());
        let long_name = "very-long-tool-name-".repeat(20);
        let selected = (0..100)
            .map(|index| ToolSurfaceSelection {
                tool_name: format!("{long_name}{index}"),
                demand: ToolSurfaceDemand::PreferSurface,
                origin: agent_contracts::ToolSurfaceOrigin::CatalogLoadedOptional,
                approx_tokens: 10,
            })
            .collect();
        let omitted = (0..100)
            .map(|index| ToolSurfaceOmission {
                tool_name: format!("{long_name}{index}"),
                demand: ToolSurfaceDemand::PreferSurface,
                origin: agent_contracts::ToolSurfaceOrigin::CatalogLoadedOptional,
                reason: ToolSurfaceOmissionReason::SchemaBudget,
                approx_tokens: 10,
            })
            .collect();
        let blocked = (0..100)
            .map(|index| ToolSurfaceBlock {
                tool_name: format!("{long_name}{index}"),
                demand: ToolSurfaceDemand::MustSurface,
                reason: ToolSurfaceBlockReason::ProviderInputBudget,
            })
            .collect();

        app.apply_runtime_event(envelope(RuntimeEvent::ToolSurfacePlanned {
            report: ToolSurfacePlanReport {
                turn_id: TurnId::new(),
                model_round: 4,
                surface_revision: 21,
                source_revisions: ToolSurfaceSourceRevisions::default(),
                status: ToolSurfacePlanStatus::Ready,
                selected,
                selected_total: 100,
                omitted,
                omitted_total: 100,
                blocked,
                blocked_total: 100,
                selected_schema_tokens: 900,
                mandatory_schema_tokens: 100,
                estimated_input_tokens: 1_200,
                input_budget_tokens: 2_000,
            },
        }));

        let rendered = &app.messages.last().expect("surface summary").content;
        assert!(rendered.contains("tool surface r21 round 4"));
        assert!(rendered.contains("+97 more"));
        assert!(rendered.chars().count() <= MAX_TOOL_SURFACE_MESSAGE_CHARS);
        assert_eq!(app.status, "tool surface prepared");
    }

    #[test]
    fn unsatisfiable_surface_clears_a_stale_model_fence() {
        let mut app = AppState::new(RunId::new());
        let turn = TurnId::new();
        let operation = OperationId::new();
        app.apply_runtime_event(envelope(RuntimeEvent::ModelStarted {
            turn_id: turn,
            operation_id: operation,
            generation: 5,
            surface_revision: 3,
            model_round: 1,
            turn_checkpoint: Default::default(),
            prompt_layers: Default::default(),
        }));

        app.apply_runtime_event(envelope(RuntimeEvent::ToolSurfacePlanned {
            report: ToolSurfacePlanReport {
                turn_id: turn,
                model_round: 2,
                surface_revision: 4,
                source_revisions: ToolSurfaceSourceRevisions::default(),
                status: ToolSurfacePlanStatus::Unsatisfiable {
                    reason: ToolSurfaceBlockReason::ProviderInputBudget,
                },
                selected: Vec::new(),
                selected_total: 0,
                omitted: Vec::new(),
                omitted_total: 0,
                blocked: Vec::new(),
                blocked_total: 1,
                selected_schema_tokens: 0,
                mandatory_schema_tokens: 1_000,
                estimated_input_tokens: 1_500,
                input_budget_tokens: 900,
            },
        }));
        app.apply_runtime_event(envelope(delta(turn, operation, 5, "LATE")));

        assert_eq!(app.status, "tool surface blocked");
        assert!(!app.busy);
        assert!(
            app.messages
                .iter()
                .all(|message| !message.content.contains("LATE"))
        );
    }

    #[test]
    fn typed_failure_and_retry_progress_render_without_claiming_idle() {
        let mut app = AppState::new(RunId::new());
        let turn = TurnId::new();
        let operation = OperationId::new();

        // A retryable model failure keeps the spinner honest while the
        // transport is inside its bounded retry policy.
        app.apply_runtime_event(envelope(RuntimeEvent::ModelStarted {
            turn_id: turn,
            operation_id: operation,
            generation: 6,
            surface_revision: 1,
            model_round: 1,
            turn_checkpoint: Default::default(),
            prompt_layers: Default::default(),
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ModelRetrying {
            turn_id: turn,
            operation_id: operation,
            generation: 6,
            attempt: 2,
            delay_ms: 250,
        }));
        assert!(app.busy);
        assert_eq!(app.status, "model (retrying)");
        assert!(
            app.messages
                .last()
                .expect("retry notice")
                .content
                .contains("retrying in 250 ms")
        );

        // A terminal typed failure stops the spinner and names the class in
        // the system line; the policy class is never reconstructed from text.
        app.apply_runtime_event(envelope(RuntimeEvent::Failure {
            class: agent_contracts::RuntimeFailureClass::ProviderTransport,
            retryable: false,
            message: "window closed".into(),
        }));
        assert!(!app.busy);
        assert_eq!(app.status, "failed (ProviderTransport)");
        assert!(
            app.messages
                .last()
                .expect("failure notice")
                .content
                .contains("execution failed (ProviderTransport): window closed")
        );
    }

    #[test]
    fn round_budget_failure_names_the_continuation_path() {
        let mut app = AppState::new(RunId::new());
        app.apply_runtime_event(envelope(RuntimeEvent::Failure {
            class: agent_contracts::RuntimeFailureClass::RoundBudget,
            retryable: false,
            message: "tool round budget exhausted after 16 rounds".into(),
        }));
        assert!(!app.busy);
        let joined = app
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("RoundBudget"), "{joined}");
        assert!(
            joined.contains("/continue starts a new segment"),
            "{joined}"
        );
        assert!(joined.contains("/plan shows remaining work"), "{joined}");
        assert!(joined.contains("/checkpoint"), "{joined}");
        // The budget shown in /status is the CLI-declared one, in the same
        // unit the runtime enforces.
        app.execution_budget = Some(48);
        let status = app.render_status().join("\n");
        assert!(
            status.contains("execution budget: 48 model rounds"),
            "{status}"
        );
    }

    #[test]
    fn rejected_user_input_is_system_notice_not_a_turn() {
        let mut app = AppState::new(RunId::new());
        let input = agent_contracts::RuntimeInputEnvelope::from_preview("second")
            .with_lifecycle(agent_contracts::InputLifecycle::Rejected);
        app.apply_runtime_event(envelope(RuntimeEvent::UserMessageAccepted { input }));
        assert!(!app.busy);
        let last = app.messages.last().expect("rejected notice");
        assert_eq!(last.role, UiRole::System);
        assert!(last.content.contains("rejected"));
        assert!(last.content.contains("second"));
        assert!(
            app.messages
                .iter()
                .all(|message| message.role != UiRole::User),
            "rejected input must not appear as a user turn"
        );
    }

    #[test]
    fn queued_then_applied_same_id_is_a_single_user_bubble() {
        let mut app = AppState::new(RunId::new());
        let id = RuntimeInputId::new();
        let mut queued = agent_contracts::RuntimeInputEnvelope::from_preview("later");
        queued.input_id = Some(id);
        queued.lifecycle = agent_contracts::InputLifecycle::Queued;
        app.apply_runtime_event(envelope(RuntimeEvent::UserMessageAccepted {
            input: queued.clone(),
        }));
        let mut applied = queued;
        applied.lifecycle = agent_contracts::InputLifecycle::Applied;
        app.apply_runtime_event(envelope(RuntimeEvent::UserMessageAccepted {
            input: applied,
        }));
        let user_bubbles: Vec<_> = app
            .messages
            .iter()
            .filter(|message| message.role == UiRole::User)
            .collect();
        assert_eq!(user_bubbles.len(), 1);
        assert_eq!(user_bubbles[0].content, "later");
        assert!(app.busy);
    }

    fn tool_output(
        name: &str,
        ok: bool,
        summary: &str,
        path: Option<&str>,
    ) -> agent_contracts::ToolOutput {
        let mut metadata = serde_json::Value::Null;
        if let Some(path) = path {
            metadata = serde_json::json!({ "path": path });
        }
        agent_contracts::ToolOutput {
            call_id: "call-1".into(),
            tool_name: name.into(),
            ok,
            summary: summary.into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata,
        }
    }

    #[test]
    fn result_card_folds_mutations_checks_failures_and_completion() {
        let mut app = AppState::new(RunId::new());
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.write", true, "wrote 12 lines", Some("src/lib.rs")),
            facts: None,
        }));
        // Same tool + path updates in place instead of duplicating a row.
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.write", true, "wrote 30 lines", Some("src/lib.rs")),
            facts: None,
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("edit.patch", true, "patched", Some("src/other.rs")),
            facts: None,
        }));
        // Reads and failures of read-shaped tools do not enter changed files.
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.read", true, "120 lines", Some("src/lib.rs")),
            facts: None,
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("verify.run", true, "15 tests pass", None),
            facts: None,
        }));
        let mut failed_check = tool_output("shell.exec", false, "cargo test exit 1", None);
        failed_check.artifact_ref = Some("artifacts/shell-9.log".into());
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: failed_check,
            facts: None,
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::Failure {
            class: agent_contracts::RuntimeFailureClass::Model,
            retryable: false,
            message: "provider refused the completion".into(),
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::TaskCompleted {
            task_id: TaskId::new(),
            anchor_revision: 4,
            summary: "migration landed".into(),
            artifacts: Vec::new(),
            final_output_digest: None,
        }));

        let card = &app.result_card;
        assert_eq!(
            card.changed_files.len(),
            2,
            "read tools never count as changes"
        );
        assert_eq!(card.changed_files[0].path, "src/lib.rs");
        assert_eq!(card.checks.len(), 2);
        assert!(card.checks[1].artifact.is_some());
        assert_eq!(card.failures.len(), 1);
        assert!(card.completion.is_some());

        let lines = format_result_lines(card).join(
            "
",
        );
        assert!(lines.contains("durably completed at anchor r4"), "{lines}");
        assert!(lines.contains("fs.write src/lib.rs [ok]"), "{lines}");
        assert!(lines.contains("verify.run [ok] 15 tests pass"), "{lines}");
        assert!(lines.contains("shell.exec [FAILED]"), "{lines}");
        assert!(lines.contains("artifacts/shell-9.log"), "{lines}");
        assert!(lines.contains("unresolved failures"), "{lines}");
        assert!(lines.contains("not attributed"), "{lines}");
        assert!(lines.contains("not verification"), "{lines}");
    }

    #[test]
    fn result_card_buckets_stay_bounded() {
        let mut app = AppState::new(RunId::new());
        for index in 0..(MAX_CARD_FILES + 10) {
            app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
                output: tool_output("fs.write", true, "w", Some(&format!("f/{index}.rs"))),
                facts: None,
            }));
        }
        assert_eq!(app.result_card.changed_files.len(), MAX_CARD_FILES);
        for _ in 0..(MAX_CARD_CHECKS + 5) {
            app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
                output: tool_output("verify.run", true, "ok", None),
                facts: None,
            }));
        }
        assert_eq!(app.result_card.checks.len(), MAX_CARD_CHECKS);
    }

    #[test]
    fn empty_card_reviews_as_no_material() {
        let app = AppState::new(RunId::new());
        let lines = format_result_lines(&app.result_card);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no result material yet"));
    }

    #[test]
    fn dialogue_events_map_to_user_assistant_and_tool_bubbles() {
        let mut app = AppState::new(RunId::new());
        let input = agent_contracts::RuntimeInputEnvelope::from_preview("请列出目录并写笔记.md");
        app.apply_runtime_event(envelope(RuntimeEvent::UserMessageAccepted { input }));
        app.apply_runtime_event(envelope(RuntimeEvent::AssistantMessage {
            content: "先看目录，再写笔记。".into(),
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.list", true, "3 entries", None),
            facts: None,
        }));

        let user = app
            .messages
            .iter()
            .find(|message| message.role == UiRole::User)
            .expect("user bubble");
        assert_eq!(user.content, "请列出目录并写笔记.md");
        let assistant = app
            .messages
            .iter()
            .find(|message| message.role == UiRole::Assistant)
            .expect("assistant bubble");
        assert_eq!(assistant.content, "先看目录，再写笔记。");
        let tool = app
            .messages
            .iter()
            .find(|message| message.role == UiRole::Tool)
            .expect("tool bubble");
        assert!(tool.content.contains("3 entries"), "{}", tool.content);
        assert!(
            app.messages
                .iter()
                .any(|message| message.role == UiRole::System),
            "SYSTEM banners stay"
        );
    }

    #[tokio::test]
    async fn completed_task_persists_the_result_card_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppState::new(RunId::new());
        app.state_dir = Some(dir.path().to_path_buf());
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.write", true, "wrote", Some("src/lib.rs")),
            facts: None,
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::TaskCompleted {
            task_id: TaskId::new(),
            anchor_revision: 2,
            summary: "done".into(),
            artifacts: Vec::new(),
            final_output_digest: None,
        }));
        let mut bytes = None;
        for _ in 0..200 {
            match tokio::fs::read(dir.path().join("artifacts/result-card-latest.json")).await {
                Ok(data) => {
                    bytes = Some(data);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        let bytes = bytes.expect("result card artifact must be written");
        let card: ResultCard = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(card.changed_files.len(), 1);
        assert!(card.completion.is_some());
    }

    /// Task A's durable completion header must never sit above task B's
    /// changes, and B's review must not list A's material.
    #[test]
    fn a_completed_tasks_header_never_covers_another_tasks_changes() {
        let mut app = AppState::new(RunId::new());
        let task_a = TaskId::new();
        let task_b = TaskId::new();
        app.apply_runtime_event(envelope(RuntimeEvent::FocusChanged {
            task_id: task_a,
            goal: "task A".into(),
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.write", true, "wrote a", Some("a.txt")),
            facts: None,
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::TaskCompleted {
            task_id: task_a,
            anchor_revision: 1,
            summary: "A done".into(),
            artifacts: Vec::new(),
            final_output_digest: None,
        }));

        // The focus moves on: task B writes and its verification FAILS.
        app.apply_runtime_event(envelope(RuntimeEvent::FocusChanged {
            task_id: task_b,
            goal: "task B".into(),
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.write", true, "wrote b", Some("b.txt")),
            facts: None,
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("verify.run", false, "2 assertions failed", None),
            facts: None,
        }));

        let card = app
            .review_card()
            .expect("task B must have review material")
            .clone();
        assert_eq!(
            card.task_id,
            Some(task_b),
            "the card must belong to the task being reviewed"
        );
        assert!(
            card.completion.is_none(),
            "task B must not inherit task A's completion header"
        );
        let rendered = format_result_lines(&card).join("\n");
        assert!(
            !rendered.contains("A done"),
            "task A's durable completion must not cover task B: {rendered}"
        );
        assert!(
            !rendered.contains("a.txt"),
            "task A's change must not appear in task B's review: {rendered}"
        );
        assert!(rendered.contains("b.txt"), "{rendered}");
        assert!(rendered.contains("FAILED"), "{rendered}");
    }

    /// A failure that arrives after the display cap is full must still be
    /// visible as a count — the old `len - cap` arithmetic could never see
    /// it, so the entry silently vanished from the review.
    #[test]
    fn a_failed_check_beyond_the_card_cap_is_counted_not_hidden() {
        let mut app = AppState::new(RunId::new());
        for index in 0..MAX_CARD_CHECKS {
            app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
                output: tool_output("verify.run", true, &format!("check {index} passed"), None),
                facts: None,
            }));
        }
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("verify.run", false, "check 33 FAILED", None),
            facts: None,
        }));

        assert_eq!(app.result_card.checks.len(), MAX_CARD_CHECKS);
        assert_eq!(
            app.result_card.omitted_checks, 1,
            "the refused check must be counted, not dropped"
        );
        assert_eq!(app.result_card.total_checks(), MAX_CARD_CHECKS + 1);
        let rendered = format_result_lines(&app.result_card).join("\n");
        assert!(
            rendered.contains("more checks were NOT shown"),
            "the review must name the omission: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{} recorded", MAX_CARD_CHECKS + 1)),
            "the review must report the true total: {rendered}"
        );
    }

    /// Snapshots are versioned and committed by rename: the newest card is
    /// what survives, and no staging file is left for a reader to trip on.
    #[tokio::test]
    async fn card_snapshots_are_versioned_and_committed_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppState::new(RunId::new());
        app.state_dir = Some(dir.path().to_path_buf());
        let task = TaskId::new();
        app.apply_runtime_event(envelope(RuntimeEvent::FocusChanged {
            task_id: task,
            goal: "g".into(),
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.write", true, "wrote", Some("one.txt")),
            facts: None,
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::TaskCompleted {
            task_id: task,
            anchor_revision: 1,
            summary: "first".into(),
            artifacts: Vec::new(),
            final_output_digest: None,
        }));
        // A second commit races the first one's spawned writer.
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("fs.write", true, "wrote", Some("two.txt")),
            facts: None,
        }));
        app.persist_result_card();

        let path = dir.path().join("artifacts/result-card-latest.json");
        let mut newest = None;
        for _ in 0..300 {
            if let Ok(bytes) = tokio::fs::read(&path).await
                && let Ok(parsed) = serde_json::from_slice::<ResultCard>(&bytes)
                && parsed.revision >= 2
            {
                newest = Some(parsed);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let newest = newest.expect("the newest snapshot must be committed");
        assert_eq!(
            newest.changed_files.len(),
            2,
            "the newer card must win: {newest:?}"
        );
        let mut entries = tokio::fs::read_dir(dir.path().join("artifacts"))
            .await
            .unwrap();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".tmp-"),
                "a staging file must not survive the commit: {name}"
            );
        }
    }

    /// R4: the snapshot publish order belongs to the SESSION. A card's own
    /// revision restarts with every task, so using it as the write watermark
    /// made task B's snapshot look older than task A's and dropped it — B's
    /// card was correct in memory and stale on disk after a restart.
    #[tokio::test]
    async fn consecutive_tasks_all_publish_and_the_newest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppState::new(RunId::new());
        app.state_dir = Some(dir.path().to_path_buf());
        let path = dir.path().join("artifacts/result-card-latest.json");
        let mut tasks = Vec::new();
        for index in 0..3u32 {
            let task = TaskId::new();
            tasks.push(task);
            let file = format!("task{index}.txt");
            app.apply_runtime_event(envelope(RuntimeEvent::FocusChanged {
                task_id: task,
                goal: format!("task {index}"),
            }));
            app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
                output: tool_output("fs.write", true, "wrote", Some(&file)),
                facts: None,
            }));
            app.apply_runtime_event(envelope(RuntimeEvent::TaskCompleted {
                task_id: task,
                anchor_revision: 1,
                summary: format!("task {index} done"),
                artifacts: Vec::new(),
                final_output_digest: None,
            }));
        }
        let newest = *tasks.last().unwrap();
        let mut card = None;
        for _ in 0..300 {
            if let Ok(bytes) = tokio::fs::read(&path).await
                && let Ok(parsed) = serde_json::from_slice::<ResultCard>(&bytes)
                && parsed.task_id == Some(newest)
            {
                card = Some(parsed);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("the newest task's card must be the one on disk after A/B/C");
        assert_eq!(card.task_id, Some(newest));
        assert!(
            card.publish_seq >= 3,
            "the publish sequence must not restart per task: {card:?}"
        );
        assert_eq!(
            card.changed_files.len(),
            1,
            "only the newest task's material may be in its own card: {card:?}"
        );
        assert_eq!(card.changed_files[0].path, "task2.txt", "{card:?}");
    }

    /// R5: a failure the display cap refused is still a failure. The previous
    /// accounting reported `33 recorded, 0 FAILED, 1 not shown`, which names
    /// the omission but states a wrong overall failure count.
    #[test]
    fn an_omitted_failed_check_does_not_read_as_zero_failures() {
        let mut app = AppState::new(RunId::new());
        for index in 0..MAX_CARD_CHECKS {
            app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
                output: tool_output("verify.run", true, &format!("check {index} passed"), None),
                facts: None,
            }));
        }
        // The 33rd check FAILS and the cap refuses the entry.
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: tool_output("verify.run", false, "check 33 FAILED", None),
            facts: None,
        }));
        assert_eq!(app.result_card.omitted_checks, 1);
        assert_eq!(
            app.result_card.failed_checks_total, 1,
            "the refused failure must still be counted"
        );
        assert_eq!(
            app.result_card.failed_checks_in_window(),
            0,
            "the display window genuinely holds no failure"
        );
        let rendered = format_result_lines(&app.result_card).join("\n");
        assert!(rendered.contains("33 recorded"), "{rendered}");
        assert!(
            rendered.contains("1 FAILED"),
            "the account must name the failure the window could not show: {rendered}"
        );
    }

    #[test]
    fn queued_input_transitions_render_explicit_disposition_lines() {
        let mut app = AppState::new(RunId::new());
        let id = RuntimeInputId::new();
        let mut queued = agent_contracts::RuntimeInputEnvelope::from_preview("later");
        queued.input_id = Some(id);
        queued.lifecycle = agent_contracts::InputLifecycle::Queued;
        app.apply_runtime_event(envelope(RuntimeEvent::UserMessageAccepted {
            input: queued.clone(),
        }));
        assert_eq!(app.status, "queued");
        assert!(
            app.messages
                .iter()
                .any(|message| message.content.contains("input queued;")),
            "queueing must be visible"
        );

        let mut applied = queued;
        applied.lifecycle = agent_contracts::InputLifecycle::Applied;
        app.apply_runtime_event(envelope(RuntimeEvent::UserMessageAccepted {
            input: applied,
        }));
        assert!(app.busy);
        assert_eq!(app.status, "working");
        assert!(
            app.messages
                .iter()
                .any(|message| message.content.contains("queued input applied")),
            "the queued slot reaching the applied state must be named"
        );
        // Exactly one disposition line per transition: a re-Queued replay of
        // the same id must not stack duplicate notices.
        assert_eq!(
            app.messages
                .iter()
                .filter(|message| message.content.contains("input queued;"))
                .count(),
            1
        );
    }
}

#[cfg(test)]
mod status_projection_tests {
    use super::*;
    use agent_contracts::EffectReconciliation;

    #[test]
    fn status_projection_tracks_task_debts_and_checkpoint() {
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        assert!(app.current_task.is_none());
        assert_eq!(app.unresolved_ack_debts, 0);

        // Real event order: the task becomes focused before its anchor
        // moves. F06: the anchor event belongs to its own task — it may
        // refresh the focused task but never invent a focused task.
        let focused = TaskId::new();
        app.apply_runtime_event(envelope(
            1,
            RuntimeEvent::FocusChanged {
                task_id: focused,
                goal: "focused goal".into(),
            },
        ));
        app.apply_runtime_event(envelope(
            2,
            RuntimeEvent::TaskAnchorChanged {
                task_id: focused,
                revision: 3,
                changed_fields: vec!["plan_progress".into()],
                patch_kind: agent_contracts::AnchorPatchKind::Autonomous,
            },
        ));
        assert_eq!(app.current_task, Some((focused, 3)));

        // An anchor bump on an unrelated task must not steal the slot.
        app.apply_runtime_event(envelope(
            3,
            RuntimeEvent::TaskAnchorChanged {
                task_id: TaskId::new(),
                revision: 9,
                changed_fields: Vec::new(),
                patch_kind: agent_contracts::AnchorPatchKind::Autonomous,
            },
        ));
        assert_eq!(app.current_task, Some((focused, 3)));

        app.apply_runtime_event(envelope(
            4,
            RuntimeEvent::EffectAckDebt {
                debt: serde_json::from_value(serde_json::json!({
                    "operation_id": agent_contracts::OperationId::new(),
                    "effect_id": agent_contracts::EffectId::new(),
                    "reservation_id": "pass/1",
                    "settlement": { "kind": "applied", "durability": "Durable" },
                    "error": "ack lost",
                }))
                .unwrap(),
            },
        ));
        assert_eq!(app.unresolved_ack_debts, 1);

        app.apply_runtime_event(envelope(
            5,
            RuntimeEvent::EffectAckDebtResolved {
                debt: serde_json::from_value(serde_json::json!({
                    "operation_id": agent_contracts::OperationId::new(),
                    "effect_id": agent_contracts::EffectId::new(),
                    "reservation_id": "pass/1",
                    "settlement": { "kind": "applied", "durability": "Durable" },
                    "error": "ack lost",
                }))
                .unwrap(),
                resolution: EffectReconciliation::NotApplied {
                    evidence: Some("never dispatched".into()),
                },
            },
        ));
        assert_eq!(app.unresolved_ack_debts, 0);

        app.last_checkpoint = Some("cp.json".into());
        let lines = app.render_status();
        let joined = lines.join("\n");
        // The projection renders task/debt truth; the manual checkpoint is
        // the UI-local extra.
        assert!(joined.contains("task:") && !joined.contains("task: none"));
        assert!(joined.contains("ack_debts=0"));
        assert!(joined.contains("last manual checkpoint: cp.json"));
    }

    #[test]
    fn the_approval_prompt_names_the_risk_and_each_argument() {
        let mut app = AppState::new(RunId::new());
        let request = agent_core::ApprovalRequest {
            request_id: "req-1".into(),
            call: agent_contracts::ToolCall {
                id: "call-1".into(),
                name: "fs.write".into(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "content": "the new contents"
                }),
            },
            spec: agent_contracts::ToolSpec {
                name: "fs.write".into(),
                description: "write a file".into(),
                input_schema: serde_json::json!({}),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
                roles: Vec::new(),
                output_budget: None,
            },
        };
        app.begin_approval(request);
        let joined = app
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(joined.contains("approval required: fs.write"), "{joined}");
        assert!(joined.contains("WorkspaceWrite"), "{joined}");
        assert!(joined.contains("  path: \"src/lib.rs\""), "{joined}");
        assert!(joined.contains("  content:"), "{joined}");
    }

    #[test]
    fn approval_detail_keeps_full_arguments_and_marks_truncation() {
        let mut app = AppState::new(RunId::new());
        let long = "x".repeat(500);
        let mut map = serde_json::Map::new();
        for i in 1..=12 {
            map.insert(
                format!("arg{i}"),
                serde_json::Value::String(format!("{long}-{i}")),
            );
        }
        let request = agent_core::ApprovalRequest {
            request_id: "req-full".into(),
            call: agent_contracts::ToolCall {
                id: "call-full".into(),
                name: "fs.write".into(),
                arguments: serde_json::Value::Object(map),
            },
            spec: agent_contracts::ToolSpec {
                name: "fs.write".into(),
                description: "write a file".into(),
                input_schema: serde_json::json!({}),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
                roles: Vec::new(),
                output_budget: None,
            },
        };
        app.begin_approval(request);
        let pending = app.pending_approval.as_ref().unwrap();
        assert!(
            !pending.truncated,
            "a 500-char argument is far under the panel's hard cap"
        );
        // The full value survives in detail — no 220-char preview cap.
        let detail = pending.detail.join("\n");
        assert!(
            detail.contains(&format!("{long}-12")),
            "arg12's full value must be present: {detail}"
        );
        assert!(
            detail.contains("arg1:") && detail.contains("arg12:"),
            "all 12 arguments are listed, not capped at 8: {detail}"
        );
        assert!(
            detail.contains("— end of request req-full —"),
            "the trailing sentinel line is present: {detail}"
        );

        // The secondary conversation-log summary marks truncation explicitly.
        let log = app
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        // A value longer than ARG_VALUE_CAP (120) gets the trailing '…'.
        assert!(
            log.contains("  arg1:") && log.contains('…'),
            "a cut log value must be marked with '…': {log}"
        );
        // Arguments past the first 8 name the count and point at the panel.
        assert!(
            log.contains("…and 4 more arguments"),
            "the log must name the extra arguments beyond 8: {log}"
        );
    }

    fn envelope(seq: u64, event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: fixture_run(),
            seq,
            timestamp_ms: seq,
            event,
        }
    }
}

#[cfg(test)]
mod resync_tests {
    use super::*;

    #[tokio::test]
    async fn resync_rebuilds_the_projection_from_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let traces = dir.path().join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let mut app = AppState::new(RunId::new());
        // One journal file with a full folded history for THIS run, plus a
        // foreign-run file whose events must never blend in.
        let mut lines = Vec::new();
        for (seq, event) in [
            (1u64, RuntimeEvent::RunStarted),
            (
                2u64,
                RuntimeEvent::FocusChanged {
                    task_id: TaskId::new(),
                    goal: "resynced goal".into(),
                },
            ),
            (
                3u64,
                RuntimeEvent::ModelUsed {
                    input_tokens: 700,
                    output_tokens: 20,
                    attempts: 1,
                    retries: 0,
                    cached_input_tokens: 0,
                    usage_identity: agent_contracts::UsageIdentity::Observed,
                    role: agent_contracts::ModelCallRole::Main,
                    usage: None,
                },
            ),
            (4u64, RuntimeEvent::TurnCompleted),
        ] {
            lines.push(
                serde_json::to_string(&RuntimeEventEnvelope {
                    run_id: app.run_id,
                    seq,
                    timestamp_ms: seq,
                    event,
                })
                .unwrap(),
            );
        }
        std::fs::write(traces.join("run.jsonl"), lines.join("\n")).unwrap();
        let foreign = RuntimeEventEnvelope {
            run_id: RunId::new(),
            seq: 1,
            timestamp_ms: 1,
            event: RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "another run's goal".into(),
            },
        };
        std::fs::write(
            traces.join("newer-other-run.jsonl"),
            serde_json::to_string(&foreign).unwrap(),
        )
        .unwrap();
        let (folded, partial) = app.resync_projection(&traces).await;
        assert!(!partial);
        assert_eq!(folded, 4, "only the current run's events fold");
        let rendered = app.status_projection.lines().join("\n");
        assert!(rendered.contains("resynced goal"), "{rendered}");
        assert!(rendered.contains("turns=1"));
        assert!(rendered.contains("tokens: in=700"));
        assert!(
            !rendered.contains("another run's goal"),
            "foreign-run state must not blend in: {rendered}"
        );
        // The replay watermark is now armed: re-delivering a folded event
        // (same run, same seq) must not double-count the projection.
        app.apply_runtime_event(RuntimeEventEnvelope {
            run_id: app.run_id,
            seq: 1,
            timestamp_ms: 1,
            event: RuntimeEvent::RunStarted,
        });
        let after = app.status_projection.lines().join("\n");
        assert_eq!(after, rendered, "watermarked events are not re-folded");

        // An empty journal dir resets the projection to a blank fold.
        let empty = tempfile::tempdir().unwrap();
        let (folded, partial) = app.resync_projection(empty.path()).await;
        assert!(!partial);
        assert_eq!(folded, 0);
        assert!(app.status_projection.lines()[0].contains("not started"));
    }

    #[tokio::test]
    async fn resync_recovers_dialogue_events_dropped_by_a_lagged_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let traces = dir.path().join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let mut app = AppState::new(RunId::new());
        let input = agent_contracts::RuntimeInputEnvelope::from_preview("请更新笔记.md；");
        let lines = [
            RuntimeEvent::UserMessageAccepted { input },
            RuntimeEvent::AssistantMessage {
                content: "已写入笔记。".into(),
            },
            RuntimeEvent::ToolFinished {
                output: agent_contracts::ToolOutput {
                    call_id: "call-1".into(),
                    tool_name: "fs.write".into(),
                    ok: true,
                    summary: "wrote 4 lines".into(),
                    model_content: String::new(),
                    artifact_ref: None,
                    metadata: serde_json::json!({ "path": "笔记.md" }),
                },
                facts: None,
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            serde_json::to_string(&RuntimeEventEnvelope {
                run_id: app.run_id,
                seq: (index as u64) + 1,
                timestamp_ms: index as u64,
                event,
            })
            .unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(traces.join("run.jsonl"), lines).unwrap();

        // Lag: the live receiver never folded these into the transcript.
        assert!(
            app.messages
                .iter()
                .all(|message| message.role == UiRole::System)
        );
        let (folded, partial) = app.resync_projection(&traces).await;
        assert!(!partial);
        assert_eq!(folded, 3);
        assert!(
            app.messages
                .iter()
                .any(|message| message.role == UiRole::User && message.content.contains("笔记.md")),
            "{:?}",
            app.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
        assert!(app.messages.iter().any(|message| {
            message.role == UiRole::Assistant && message.content.contains("已写入笔记")
        }));
        assert!(
            app.messages
                .iter()
                .any(|message| message.role == UiRole::Tool && message.content.contains("wrote 4"))
        );
        assert!(
            app.messages
                .iter()
                .any(|message| message.role == UiRole::System
                    && message.content.contains("Prototype")),
            "session SYSTEM banners must remain"
        );
    }

    fn run_envelope(run_id: RunId, seq: u64, event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id,
            seq,
            timestamp_ms: seq,
            event,
        }
    }

    fn model_used(input_tokens: u64, output_tokens: u64) -> RuntimeEvent {
        RuntimeEvent::ModelUsed {
            input_tokens,
            output_tokens,
            attempts: 1,
            retries: 0,
            cached_input_tokens: 0,
            usage_identity: agent_contracts::UsageIdentity::Observed,
            role: agent_contracts::ModelCallRole::Main,
            usage: None,
        }
    }

    fn model_started(turn: TurnId, op: OperationId, generation: u64) -> RuntimeEvent {
        RuntimeEvent::ModelStarted {
            turn_id: turn,
            operation_id: op,
            generation,
            surface_revision: 0,
            model_round: 1,
            prompt_layers: Default::default(),
            turn_checkpoint: Default::default(),
        }
    }

    fn journal_line(app: &AppState, seq: u64, event: RuntimeEvent) -> String {
        serde_json::to_string(&run_envelope(app.run_id, seq, event)).unwrap()
    }

    /// R7: reading the same journal twice must render the same visible
    /// transcript. Event-derived SYSTEM rows used to be appended again on
    /// every replay, so in a full window the duplicates pushed real dialogue
    /// rows out — the durable log was intact, the view was not.
    #[tokio::test]
    async fn replaying_the_same_journal_twice_yields_the_same_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let traces = dir.path().join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        let events = [
            RuntimeEvent::RunStarted,
            RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "replayed goal".into(),
            },
            RuntimeEvent::Warning {
                message: "a warning row".into(),
            },
            RuntimeEvent::AssistantMessage {
                content: "the reply".into(),
            },
            RuntimeEvent::TurnCompleted,
        ];
        let lines: Vec<String> = events
            .iter()
            .enumerate()
            .map(|(index, event)| journal_line(&app, index as u64 + 1, event.clone()))
            .collect();
        std::fs::write(traces.join("run.jsonl"), lines.join("\n")).unwrap();

        let transcript = |app: &AppState| -> Vec<String> {
            app.messages
                .iter()
                .map(|message| format!("{:?}|{}", message.role, message.content))
                .collect()
        };
        let (folded, partial) = app.resync_projection(&traces).await;
        assert!(!partial);
        assert_eq!(folded, 5);
        let first = transcript(&app);
        // Every event-derived row was rebuilt, not left in place.
        assert!(
            first.iter().any(|row| row.contains("the reply")),
            "the dialogue must be rebuilt from the journal: {first:?}"
        );

        let (folded_again, partial_again) = app.resync_projection(&traces).await;
        assert!(!partial_again);
        assert_eq!(folded_again, 5, "a replay must still re-fold the journal");
        assert_eq!(
            first,
            transcript(&app),
            "the same journal must render the same visible transcript"
        );
        // The dialogue row survives the rebuild instead of being pushed out.
        assert!(
            transcript(&app).iter().any(|row| row.contains("the reply")),
            "a replayed SYSTEM row must not push real dialogue out of the window"
        );
    }

    /// A redelivered broadcast event was already folded: it must neither
    /// double-count tokens nor re-activate the operation it named. The old
    /// watermark only skipped the *projection* fold, so the local fields
    /// were still mutated; this is the regression for that split.
    #[test]
    fn a_redelivered_event_neither_double_counts_nor_reactivates_an_operation() {
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        let stale_turn = TurnId::new();
        let stale_op = OperationId::new();
        app.apply_runtime_event(run_envelope(run_id, 1, RuntimeEvent::RunStarted));
        app.apply_runtime_event(run_envelope(run_id, 2, model_used(700, 20)));
        app.apply_runtime_event(run_envelope(run_id, 3, RuntimeEvent::TurnCompleted));
        app.apply_runtime_event(run_envelope(
            run_id,
            4,
            model_started(stale_turn, stale_op, 1),
        ));
        app.apply_runtime_event(run_envelope(run_id, 5, RuntimeEvent::TurnCompleted));
        let tokens_before = (app.input_tokens, app.output_tokens);
        assert_eq!(tokens_before, (700, 20));
        assert!(app.current_op.is_none());

        // The broadcast redelivers what the view already applied.
        app.apply_runtime_event(run_envelope(run_id, 2, model_used(700, 20)));
        app.apply_runtime_event(run_envelope(
            run_id,
            4,
            model_started(stale_turn, stale_op, 1),
        ));

        assert_eq!(
            (app.input_tokens, app.output_tokens),
            tokens_before,
            "a redelivered ModelUsed must not be counted twice"
        );
        assert!(
            app.current_op.is_none(),
            "a redelivered ModelStarted must not re-activate a superseded operation"
        );
        assert_eq!(app.status, "idle");
        assert!(!app.busy);
    }

    /// Content is not event identity. Two turns that legitimately answer
    /// with the same words are two events and both rows must survive.
    #[test]
    fn identical_replies_in_different_turns_stay_as_two_rows() {
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        for seq in 1..=2u64 {
            app.apply_runtime_event(run_envelope(
                run_id,
                seq,
                RuntimeEvent::AssistantMessage {
                    content: "已写入笔记。".into(),
                },
            ));
        }
        let rows = app
            .messages
            .iter()
            .filter(|message| {
                message.role == UiRole::Assistant && message.content == "已写入笔记。"
            })
            .count();
        assert_eq!(
            rows, 2,
            "identical wording in two turns must not be deduped"
        );
    }

    /// A replay produces the same view the live path had — not just a
    /// re-folded projection with stale local fields left behind.
    #[tokio::test]
    async fn a_replayed_view_equals_the_live_view() {
        let dir = tempfile::tempdir().unwrap();
        let traces = dir.path().join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let run_id = RunId::new();
        let turn = TurnId::new();
        let op = OperationId::new();
        let events: Vec<RuntimeEvent> = vec![
            RuntimeEvent::RunStarted,
            RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "replay me".into(),
            },
            model_started(turn, op, 1),
            model_used(700, 20),
            RuntimeEvent::AssistantMessage {
                content: "done".into(),
            },
            RuntimeEvent::TurnCompleted,
        ];
        let mut live = AppState::new(run_id);
        let mut lines = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let seq = (index as u64) + 1;
            lines.push(journal_line(&live, seq, event.clone()));
            live.apply_runtime_event(run_envelope(run_id, seq, event.clone()));
        }
        std::fs::write(traces.join("run.jsonl"), lines.join("\n")).unwrap();

        // The replaying view missed the early events; it then recovers from
        // the journal and the broadcast redelivers everything it covered.
        let mut replayed = AppState::new(run_id);
        replayed.apply_runtime_event(run_envelope(
            run_id,
            5,
            RuntimeEvent::AssistantMessage {
                content: "done".into(),
            },
        ));
        let (folded, partial) = replayed.resync_projection(&traces).await;
        assert!(!partial);
        assert_eq!(folded, 6);
        for (index, event) in events.iter().enumerate() {
            replayed.apply_runtime_event(run_envelope(run_id, (index as u64) + 1, event.clone()));
        }

        assert_eq!(
            (replayed.input_tokens, replayed.output_tokens),
            (live.input_tokens, live.output_tokens),
            "the replayed account must equal the live one"
        );
        assert_eq!(replayed.status, live.status);
        assert_eq!(replayed.busy, live.busy);
        assert_eq!(replayed.current_op, live.current_op);
        assert!(
            replayed
                .status_projection
                .lines()
                .join("\n")
                .contains("tokens: in=700"),
            "the replayed projection must carry the same account"
        );
        let rows = |app: &AppState| {
            app.messages
                .iter()
                .filter(|message| message.role == UiRole::Assistant && message.content == "done")
                .count()
        };
        assert_eq!(rows(&live), 1);
        assert_eq!(
            rows(&replayed),
            1,
            "the replay must not append a second copy of an already-shown row"
        );
    }

    /// A journal line we cannot parse is unverified coverage: the view must
    /// say it is partial instead of presenting itself as the whole story.
    #[tokio::test]
    async fn an_unparseable_journal_line_keeps_the_view_partial() {
        let dir = tempfile::tempdir().unwrap();
        let traces = dir.path().join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        let lines = [
            journal_line(&app, 1, RuntimeEvent::RunStarted),
            journal_line(&app, 2, model_used(700, 20)),
            "{ this is not a runtime event }".to_string(),
            journal_line(&app, 4, RuntimeEvent::TurnCompleted),
        ];
        std::fs::write(traces.join("run.jsonl"), lines.join("\n")).unwrap();
        let (folded, partial) = app.resync_projection(&traces).await;
        assert!(partial, "a bad line must mark the replay partial");
        assert_eq!(folded, 3);
        assert!(app.view_partial);
        let reason = app.view_partial_reason.clone().unwrap_or_default();
        assert!(
            reason.contains("parse") || reason.contains("contiguous"),
            "the reason must name the cause: {reason}"
        );
        let rendered = app.render_status().join("\n");
        assert!(
            rendered.contains("PARTIAL"),
            "/status must not present a partial view as complete: {rendered}"
        );
    }

    /// A hole in the journal means no contiguous prefix was verified, so the
    /// view must not claim one — and the events it did fold must still not
    /// be applicable a second time.
    #[tokio::test]
    async fn a_journal_gap_is_never_claimed_as_contiguous_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let traces = dir.path().join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let run_id = RunId::new();
        let mut app = AppState::new(run_id);
        let lines = [
            journal_line(&app, 1, RuntimeEvent::RunStarted),
            journal_line(&app, 2, model_used(700, 20)),
            journal_line(&app, 4, RuntimeEvent::TurnCompleted),
        ];
        std::fs::write(traces.join("run.jsonl"), lines.join("\n")).unwrap();
        let (folded, partial) = app.resync_projection(&traces).await;
        assert_eq!(folded, 3);
        assert!(partial, "a sequence gap must not be reported as complete");
        assert!(app.view_partial);
        assert!(
            app.resync_watermark_for_test().is_none(),
            "no contiguous coverage may be claimed across a gap"
        );
        // The gap did not make already-folded events applicable again.
        app.apply_runtime_event(run_envelope(run_id, 2, model_used(700, 20)));
        assert_eq!((app.input_tokens, app.output_tokens), (700, 20));
    }
}
