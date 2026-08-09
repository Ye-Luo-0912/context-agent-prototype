use agent_contracts::{
    ContextDiagnostics, ContextSelection, ContextStateTransition, OperationId, RunId, RuntimeEvent,
    RuntimeEventEnvelope, TurnId,
};
use agent_kernel::ApprovalRequest;

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
            RuntimeEvent::UserMessageAccepted { content } => {
                self.busy = true;
                self.status = "working".into();
                self.scroll = 0;
                self.messages.push(UiMessage {
                    role: UiRole::User,
                    content,
                });
            }
            RuntimeEvent::FocusChanged { task_id, goal } => {
                self.push_system(format!("focus -> task {task_id}: {goal}"));
            }
            RuntimeEvent::FocusCleared => {
                self.push_system("focus cleared (task suspended)".into());
            }
            RuntimeEvent::Pinned { content } => {
                self.push_system(format!("pinned: {content}"));
            }
            RuntimeEvent::ContextPrepared {
                diagnostics,
                selected,
            } => {
                self.context = diagnostics;
                self.context_selected = selected;
                self.status = "model context prepared".into();
            }
            RuntimeEvent::ContextMaintained { report, .. } => {
                self.context = report.diagnostics;
                self.record_transitions(report.transitions);
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
            RuntimeEvent::ModelStarted {
                turn_id,
                operation_id,
                generation,
            } => {
                self.current_op = Some((turn_id, operation_id, generation));
                self.busy = true;
                self.streaming = false;
                self.status = "model".into();
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
            RuntimeEvent::TaskCompleted { summary } => {
                self.push_system(format!("task completed: {summary}"));
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
            RuntimeEvent::ModelUsed {
                input_tokens,
                output_tokens,
            } => {
                // A token meter for the live run: the provider-reported
                // cost of the last model round, surfaced in the status line.
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                self.push_system(format!(
                    "model used: {input_tokens} in + {output_tokens} out (run: {} in + {} out)",
                    self.input_tokens, self.output_tokens
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
    use agent_contracts::RuntimeEventEnvelope;

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
}
