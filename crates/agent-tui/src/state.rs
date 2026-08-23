use agent_contracts::{
    ContextDiagnostics, ContextSelection, ContextStateTransition, OperationId, RunId, RuntimeEvent,
    RuntimeEventEnvelope, RuntimeInputId, ToolSurfaceBlockReason, ToolSurfaceDemand,
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
}

/// A workspace-write / process-execution call waiting for the user's y/n.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub tool_name: String,
    pub args_preview: String,
}

const MAX_PANEL_TRANSITIONS: usize = 100;
const MAX_TOOL_SURFACE_PREVIEW_ROWS_PER_KIND: usize = 3;
const MAX_TOOL_SURFACE_PREVIEW_NAME_CHARS: usize = 48;
const MAX_TOOL_SURFACE_MESSAGE_CHARS: usize = 640;

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

pub struct AppState {
    pub run_id: RunId,
    pub input: String,
    pub messages: Vec<UiMessage>,
    pub context: ContextDiagnostics,
    pub context_selected: Vec<ContextSelection>,
    pub context_transitions: Vec<ContextStateTransition>,
    pub show_context_panel: bool,
    pub streaming: bool,
    pub status: String,
    pub tool_status: String,
    pub busy: bool,
    pub scroll: u16,
    pub pending_approval: Option<PendingApproval>,
    /// Cumulative provider-reported token usage for the live run (fed by
    /// `RuntimeEvent::ModelUsed`).
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The model operation whose streamed deltas are currently being
    /// rendered. A delta that does not match this identity belongs to a
    /// superseded turn and is dropped — the fence against a cancelled
    /// turn's late text leaking into the next turn's transcript.
    current_op: Option<(TurnId, OperationId, u64)>,
    /// Queued 然后 Applied 共用 input_id，避免用户气泡重复。
    last_shown_input_id: Option<RuntimeInputId>,
}

impl AppState {
    pub fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            input: String::new(),
            messages: vec![UiMessage {
                role: UiRole::System,
                content: "Prototype ready. /focus, /pin, /done, /context, /checkpoint, /cancel, /quit. Tab: context inspect. Try `demo: list files`.".into(),
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
            input_tokens: 0,
            output_tokens: 0,
            current_op: None,
            last_shown_input_id: None,
        }
    }

    pub fn push_system(&mut self, content: String) {
        self.messages.push(UiMessage {
            role: UiRole::System,
            content,
        });
    }

    pub fn begin_approval(&mut self, request: ApprovalRequest) {
        let preview = serde_json::to_string(&request.call.arguments).unwrap_or_default();
        let preview: String = preview.chars().take(220).collect();
        let tool_name = request.spec.name;
        self.pending_approval = Some(PendingApproval {
            request_id: request.request_id,
            tool_name: tool_name.clone(),
            args_preview: preview,
        });
        self.busy = true;
        self.status = "awaiting approval".into();
        self.push_system(format!("approval required: {tool_name}"));
    }

    pub fn clear_approval(&mut self) {
        self.pending_approval = None;
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

    pub fn apply_runtime_event(&mut self, envelope: RuntimeEventEnvelope) {
        match envelope.event {
            RuntimeEvent::RunStarted => self.status = "ready".into(),
            RuntimeEvent::UserMessageAccepted { input } => {
                if input.appears_in_user_transcript() {
                    let already_shown = input
                        .input_id
                        .is_some_and(|id| self.last_shown_input_id == Some(id));
                    if !already_shown {
                        if let Some(id) = input.input_id {
                            self.last_shown_input_id = Some(id);
                        }
                        self.messages.push(UiMessage {
                            role: UiRole::User,
                            content: input.preview.clone(),
                        });
                    }
                    if input.is_applied() {
                        self.busy = true;
                        self.status = "working".into();
                        self.scroll = 0;
                    } else {
                        self.status = "queued".into();
                    }
                } else if input.lifecycle == agent_contracts::InputLifecycle::Rejected {
                    self.push_system(format!("input rejected: {}", input.preview));
                } else if input.lifecycle == agent_contracts::InputLifecycle::InterruptCommitted {
                    self.push_system(format!("turn interrupted: {}", input.preview));
                }
            }
            RuntimeEvent::FocusChanged { task_id, goal } => {
                self.push_system(format!("focus -> task {task_id}: {goal}"));
            }
            RuntimeEvent::FocusCleared => {
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
                // Bounded audit row: the event names the moved fields and
                // the authority split (autonomous vs boundary), never the
                // anchor content (which lives in the checkpoint).
                self.push_system(format!(
                    "task {task_id} anchor r{revision} updated ({patch_kind:?}): {}",
                    changed_fields.join(", ")
                ));
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
                    Some(last) if last.role == UiRole::Assistant => last.content.push_str(&delta),
                    _ => self.messages.push(UiMessage {
                        role: UiRole::Assistant,
                        content: delta,
                    }),
                }
            }
            RuntimeEvent::AssistantMessage { content } => {
                self.streaming = false;
                match self.messages.last_mut() {
                    Some(last) if last.role == UiRole::Assistant => last.content = content,
                    _ => self.messages.push(UiMessage {
                        role: UiRole::Assistant,
                        content,
                    }),
                }
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
            RuntimeEvent::ToolFinished { output } => {
                self.tool_status = format!("{}: {}", output.tool_name, output.summary);
                self.messages.push(UiMessage {
                    role: UiRole::Tool,
                    content: format!(
                        "{}\n{}",
                        output.summary,
                        output.artifact_ref.unwrap_or_default()
                    ),
                });
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
            RuntimeEvent::ProtocolBodyCacheStats { .. } => {
                // 正文缓存账目同理：指标归 eval 聚合，UI 不重复渲染。
            }
            RuntimeEvent::ExecutionObligation { .. } => {
                // 义务账目同理：typed 计数归 eval 聚合，advisory 已由
                // TASK PROGRESS 渲染。
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
            } => {
                self.push_system(format!(
                    "task {task_id} completed (anchor r{anchor_revision}): {summary}"
                ));
            }
            RuntimeEvent::TurnCompleted => {
                // The turn is over: any delta still in flight belongs to a
                // superseded operation and must not render.
                self.current_op = None;
                self.busy = false;
                self.streaming = false;
                self.status = "idle".into();
                self.tool_status = "none".into();
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        RuntimeEventEnvelope, ToolSurfaceBlock, ToolSurfaceOmission, ToolSurfaceOmissionReason,
        ToolSurfaceSelection, ToolSurfaceSourceRevisions,
    };

    fn envelope(event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: RunId::new(),
            seq: 1,
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
}
