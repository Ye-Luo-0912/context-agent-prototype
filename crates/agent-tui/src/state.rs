use agent_contracts::{
    ContextDiagnostics, ContextSelection, ContextStateTransition, RunId, RuntimeEvent,
    RuntimeEventEnvelope,
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
            RuntimeEvent::FocusChanged { goal } => {
                self.push_system(format!("focus -> {goal}"));
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
                let evicted_buffer = report.diagnostics.evicted_items;
                self.context = report.diagnostics;
                // Evictions and reactivations are lifecycle events worth
                // showing in the panel: GC must be explainable.
                for eviction in report.evictions {
                    self.context_transitions.push(ContextStateTransition {
                        item_id: eviction.item_id,
                        kind: eviction.kind,
                        scope: eviction.scope,
                        from: agent_contracts::ContextState::Archived,
                        to: agent_contracts::ContextState::Archived,
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
                        from: agent_contracts::ContextState::Archived,
                        to: agent_contracts::ContextState::Active,
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
            RuntimeEvent::ModelStarted => {
                self.busy = true;
                self.streaming = false;
                self.status = "model".into();
            }
            RuntimeEvent::ModelDelta { delta } => {
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
                    diagnostics.dropped_items,
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
                self.busy = false;
                self.streaming = false;
                self.status = "idle".into();
                self.tool_status = "none".into();
            }
            RuntimeEvent::RunCompleted => {
                self.busy = false;
                self.status = "stopped".into();
            }
        }
    }
}
