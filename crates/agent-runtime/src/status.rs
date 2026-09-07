//! `StatusProjection`: a bounded, event-derived status read model — the
//! `RunStateAggregator` direction the architecture names for reusable view
//! state. It folds the same public `RuntimeEvent` stream every host already
//! subscribes to into one snapshot that answers "how is the agent doing":
//! lifecycle, task and focus, tokens, in-flight operation, effect-ack
//! debts, latest durable checkpoint, required-context misses and recovery.
//! It never reads runtime internals, so TUI, CLI and any future host render
//! one truth, and a UI that falls behind its broadcast can rebuild the
//! snapshot by replaying events instead of trusting a stale projection.

use std::collections::HashMap;

use agent_contracts::{RunId, RuntimeEvent, TaskId};

/// Bounded number of recent warning texts retained for the snapshot.
const MAX_RETAINED_WARNINGS: usize = 3;
/// Bounded per-task anchor-revision table. Revisions belong to one task;
/// the table never compares or maxes across tasks and evicts arbitrarily
/// beyond the runtime's own task-record cap (the focused entry is kept).
const MAX_TRACKED_ANCHOR_REVISIONS: usize = 256;

/// What the runtime is doing right now, as far as events can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InFlight {
    Model,
    Tool(String),
}

/// Event-fold status projection. Start one per run at process start, feed
/// every envelope through [`StatusProjection::fold`], render with
/// [`StatusProjection::lines`].
#[derive(Debug, Clone, Default)]
pub struct StatusProjection {
    started: bool,
    completed: bool,
    turns_completed: u64,
    model_rounds: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    in_flight: Option<InFlight>,
    current_task: Option<TaskId>,
    /// Whether the CURRENT task durably completed (`TaskCompleted`). Under
    /// `OperatorClosureOnly` a normal final ends only the turn — an active
    /// task without this flag is awaiting operator review, not done.
    task_durably_completed: bool,
    task_completion_summary: String,
    focus_goal: String,
    /// Each task's own latest anchor revision (F06: revisions are per-task;
    /// a background anchor change on one task must never leak into another
    /// task's display through a cross-task `max`).
    anchor_revisions: HashMap<TaskId, u64>,
    unresolved_ack_debts: usize,
    last_checkpoint: Option<(String, u64)>,
    required_miss_events: u32,
    required_miss_items: u32,
    restored_from: Option<RunId>,
    recent_warnings: Vec<String>,
}

impl StatusProjection {
    pub fn fold(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::RunStarted => self.started = true,
            RuntimeEvent::RunCompleted => self.completed = true,
            RuntimeEvent::TurnCompleted => {
                self.turns_completed = self.turns_completed.saturating_add(1);
                self.in_flight = None;
            }
            RuntimeEvent::ModelStarted { .. } => {
                self.model_rounds = self.model_rounds.saturating_add(1);
                self.in_flight = Some(InFlight::Model);
            }
            RuntimeEvent::ModelUsed {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                ..
            } => {
                self.in_flight = None;
                self.input_tokens = self.input_tokens.saturating_add(*input_tokens);
                self.output_tokens = self.output_tokens.saturating_add(*output_tokens);
                self.cached_input_tokens = self
                    .cached_input_tokens
                    .saturating_add(*cached_input_tokens);
            }
            RuntimeEvent::ToolStarted { call } => {
                self.in_flight = Some(InFlight::Tool(call.name.clone()));
            }
            RuntimeEvent::ToolFinished { output, .. } => {
                self.in_flight = None;
                let _ = output;
            }
            RuntimeEvent::FocusChanged { task_id, goal } => {
                if self.current_task != Some(*task_id) {
                    self.task_durably_completed = false;
                }
                self.current_task = Some(*task_id);
                self.focus_goal = goal.clone();
            }
            RuntimeEvent::FocusCleared => {
                self.current_task = None;
                self.task_durably_completed = false;
                self.focus_goal.clear();
            }
            RuntimeEvent::TaskAnchorChanged {
                task_id, revision, ..
            } => {
                // An anchor change belongs to its own task. It never steals
                // focus and never resets the focused task's closure state —
                // only FocusChanged/FocusCleared/TaskCompleted own those
                // transitions.
                if self.anchor_revisions.len() >= MAX_TRACKED_ANCHOR_REVISIONS
                    && !self.anchor_revisions.contains_key(task_id)
                {
                    if let Some(evict) = self
                        .anchor_revisions
                        .keys()
                        .find(|key| Some(**key) != self.current_task)
                        .copied()
                    {
                        self.anchor_revisions.remove(&evict);
                    }
                }
                self.anchor_revisions.insert(*task_id, *revision);
            }
            RuntimeEvent::TaskCompleted {
                task_id, summary, ..
            } => {
                self.current_task = Some(*task_id);
                self.task_durably_completed = true;
                self.task_completion_summary = ellipsize(summary, 120);
            }
            RuntimeEvent::EffectAckDebt { .. } => {
                self.unresolved_ack_debts = self.unresolved_ack_debts.saturating_add(1);
            }
            RuntimeEvent::EffectAckDebtResolved { .. } => {
                self.unresolved_ack_debts = self.unresolved_ack_debts.saturating_sub(1);
            }
            RuntimeEvent::CheckpointDurable {
                bytes, artifact, ..
            } => {
                self.last_checkpoint = Some((artifact.clone(), *bytes));
            }
            RuntimeEvent::ContextDegraded {
                required_misses, ..
            } => {
                let items = required_misses.total();
                if items > 0 {
                    self.required_miss_events = self.required_miss_events.saturating_add(1);
                    self.required_miss_items = self.required_miss_items.saturating_add(items);
                }
            }
            RuntimeEvent::RuntimeRestored {
                restored_run_id, ..
            } => {
                self.restored_from = Some(*restored_run_id);
                self.in_flight = None;
            }
            RuntimeEvent::Warning { message, .. } => {
                self.recent_warnings.push(message.clone());
                let overflow = self
                    .recent_warnings
                    .len()
                    .saturating_sub(MAX_RETAINED_WARNINGS);
                if overflow > 0 {
                    self.recent_warnings.drain(..overflow);
                }
            }
            _ => {}
        }
    }

    pub fn started(&self) -> bool {
        self.started
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    pub fn unresolved_ack_debts(&self) -> usize {
        self.unresolved_ack_debts
    }

    pub fn restored_from(&self) -> Option<RunId> {
        self.restored_from
    }

    /// Human-readable snapshot lines, bounded and newest-meaningful-first.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let lifecycle = if self.completed {
            "completed"
        } else if self.started {
            "serving"
        } else {
            "not started"
        };
        let in_flight = match &self.in_flight {
            None => "none".to_string(),
            Some(InFlight::Model) => "model round".to_string(),
            Some(InFlight::Tool(name)) => format!("tool {name}"),
        };
        lines.push(format!(
            "status: {lifecycle} | turns={} | model_rounds={} | in_flight={in_flight}",
            self.turns_completed, self.model_rounds
        ));
        lines.push(format!(
            "tokens: in={} out={} (cached_in={})",
            self.input_tokens, self.output_tokens, self.cached_input_tokens
        ));
        let anchor_revision = self
            .current_task
            .and_then(|task_id| self.anchor_revisions.get(&task_id))
            .copied()
            .unwrap_or(0);
        match (&self.current_task, anchor_revision) {
            (Some(task_id), revision) => {
                let closure = if self.task_durably_completed {
                    format!(
                        "durably completed (operator accepted): {}",
                        self.task_completion_summary
                    )
                } else {
                    "awaiting operator review (=/done closes durably)".to_string()
                };
                lines.push(format!(
                    "task: {task_id} anchor_revision={revision} goal={} [{closure}]",
                    ellipsize(&self.focus_goal, 120)
                ));
            }
            (None, _) => lines.push("task: none".into()),
        }
        lines.push(format!(
            "recovery: ack_debts={} restored_from={}",
            self.unresolved_ack_debts,
            self.restored_from
                .map(|run_id| run_id.to_string())
                .unwrap_or_else(|| "none".into())
        ));
        match &self.last_checkpoint {
            Some((artifact, bytes)) => lines.push(format!(
                "last durable checkpoint: {artifact} ({bytes} bytes)"
            )),
            None => lines.push("last durable checkpoint: none".into()),
        }
        if self.required_miss_events > 0 {
            lines.push(format!(
                "required context misses: {} items across {} degraded turns",
                self.required_miss_items, self.required_miss_events
            ));
        }
        for warning in &self.recent_warnings {
            lines.push(format!("recent warning: {}", ellipsize(warning, 120)));
        }
        lines
    }
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ContextMaterializationMisses, ToolCall, ToolOutput};
    use serde_json::json;

    fn fold_all(events: &[RuntimeEvent]) -> StatusProjection {
        let mut projection = StatusProjection::default();
        for event in events {
            projection.fold(event);
        }
        projection
    }

    /// M16-02: an active task without a durable completion is displayed
    /// as awaiting operator review; a TaskCompleted event flips it to the
    /// operator-accepted state.
    #[test]
    fn the_task_line_names_the_closure_state() {
        let task_id = TaskId::new();
        let active = fold_all(&[RuntimeEvent::FocusChanged {
            task_id,
            goal: "migrate config".into(),
        }]);
        let lines = active.lines();
        let line = lines
            .iter()
            .find(|line| line.starts_with("task:"))
            .expect("task line");
        assert!(
            line.contains("awaiting operator review"),
            "active task reads as awaiting review: {line}"
        );

        let completed = fold_all(&[
            RuntimeEvent::FocusChanged {
                task_id,
                goal: "migrate config".into(),
            },
            RuntimeEvent::TaskCompleted {
                task_id,
                anchor_revision: 3,
                summary: "migration landed".into(),
            },
        ]);
        let lines = completed.lines();
        let line = lines
            .iter()
            .find(|line| line.starts_with("task:"))
            .expect("task line");
        assert!(
            line.contains("durably completed (operator accepted): migration landed"),
            "{line}"
        );

        // Switching to a different task resets the closure state.
        let switched = fold_all(&[
            RuntimeEvent::TaskCompleted {
                task_id,
                anchor_revision: 3,
                summary: "done".into(),
            },
            RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "new goal".into(),
            },
        ]);
        let lines = switched.lines();
        let line = lines
            .iter()
            .find(|line| line.starts_with("task:"))
            .expect("task line");
        assert!(
            line.contains("awaiting operator review"),
            "a new task starts un-closed: {line}"
        );
    }

    /// F06 acceptance: task A climbs to anchor revision 9, then the run
    /// switches to fresh task B at revision 1. Display and the API truth
    /// must both read B=1 — a background anchor event on A (or any other
    /// task) may never max into B's revision or steal the focus line.
    #[test]
    fn anchor_revisions_belong_to_their_own_task() {
        let task_a = TaskId::new();
        let task_b = TaskId::new();
        let task_c = TaskId::new();

        let switched = fold_all(&[
            RuntimeEvent::FocusChanged {
                task_id: task_a,
                goal: "goal a".into(),
            },
            RuntimeEvent::TaskAnchorChanged {
                task_id: task_a,
                revision: 9,
                changed_fields: Vec::new(),
                patch_kind: agent_contracts::AnchorPatchKind::Autonomous,
            },
            RuntimeEvent::FocusChanged {
                task_id: task_b,
                goal: "goal b".into(),
            },
            RuntimeEvent::TaskAnchorChanged {
                task_id: task_b,
                revision: 1,
                changed_fields: Vec::new(),
                patch_kind: agent_contracts::AnchorPatchKind::Autonomous,
            },
        ]);
        let line = switched
            .lines()
            .into_iter()
            .find(|line| line.starts_with("task:"))
            .expect("task line");
        assert!(
            line.contains("anchor_revision=1") && line.contains("goal b"),
            "display must read B=1, not a cross-task max: {line}"
        );

        // A revision bump on an unrelated third task changes nothing about
        // the focused task's line.
        let mut after_noise = switched;
        after_noise.fold(&RuntimeEvent::TaskAnchorChanged {
            task_id: task_c,
            revision: 5,
            changed_fields: Vec::new(),
            patch_kind: agent_contracts::AnchorPatchKind::Autonomous,
        });
        assert_eq!(after_noise.current_task, Some(task_b));
        let line = after_noise
            .lines()
            .into_iter()
            .find(|line| line.starts_with("task:"))
            .expect("task line");
        assert!(line.contains("anchor_revision=1"), "{line}");

        // Returning to A shows A's own revision again.
        let mut back_to_a = after_noise;
        back_to_a.fold(&RuntimeEvent::FocusChanged {
            task_id: task_a,
            goal: "goal a".into(),
        });
        let line = back_to_a
            .lines()
            .into_iter()
            .find(|line| line.starts_with("task:"))
            .expect("task line");
        assert!(line.contains("anchor_revision=9"), "{line}");
    }

    #[test]
    fn the_projection_folds_a_product_like_event_sequence() {
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "fs.write".into(),
            arguments: json!({}),
        };
        let projection = fold_all(&[
            RuntimeEvent::RunStarted,
            RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "fix the retry table".into(),
            },
            RuntimeEvent::ModelStarted {
                turn_id: agent_contracts::TurnId::new(),
                operation_id: agent_contracts::OperationId::new(),
                generation: 1,
                surface_revision: 0,
                model_round: 1,
                prompt_layers: Default::default(),
                turn_checkpoint: Default::default(),
            },
            RuntimeEvent::ModelUsed {
                input_tokens: 900,
                output_tokens: 30,
                cached_input_tokens: 100,
                attempts: 1,
                retries: 0,
            },
            RuntimeEvent::ToolStarted {
                call: tool_call.clone(),
            },
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: tool_call.id.clone(),
                    tool_name: "fs.write".into(),
                    ok: true,
                    summary: "wrote".into(),
                    model_content: String::new(),
                    artifact_ref: None,
                    metadata: Default::default(),
                },
                facts: None,
            },
            RuntimeEvent::ContextDegraded {
                turn_id: agent_contracts::TurnId::new(),
                model_round: 1,
                materialization_id: 1,
                required_misses: ContextMaterializationMisses::default(),
                optional_misses: ContextMaterializationMisses::default(),
            },
            RuntimeEvent::TurnCompleted,
            RuntimeEvent::RunCompleted,
        ]);
        assert!(projection.started() && projection.completed());
        assert_eq!(projection.turns_completed, 1);
        assert_eq!(projection.model_rounds, 1);
        assert_eq!(projection.input_tokens, 900);
        assert_eq!(projection.output_tokens, 30);
        assert_eq!(projection.unresolved_ack_debts(), 0);
        let rendered = projection.lines().join("\n");
        assert!(rendered.contains("turns=1"));
        assert!(rendered.contains("goal=fix the retry table"));
        assert!(rendered.contains("completed"));
    }

    #[test]
    fn debts_and_misses_and_warnings_stay_bounded_and_counted() {
        let tool_output = ToolOutput {
            call_id: "call-2".into(),
            tool_name: "verify.run".into(),
            ok: false,
            summary: "failed".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: Default::default(),
        };
        let mut projection = StatusProjection::default();
        for _ in 0..5 {
            projection.fold(&RuntimeEvent::EffectAckDebt {
                debt: agent_contracts::EffectAckDebt {
                    operation_id: agent_contracts::OperationId::new(),
                    effect_id: agent_contracts::EffectId::new(),
                    reservation_id: "pass/1".into(),
                    settlement: agent_contracts::EffectAckSettlement::Applied {
                        durability: agent_contracts::EffectDurability::Durable,
                    },
                    error: "ack lost".into(),
                },
            });
        }
        let debt = agent_contracts::EffectAckDebt {
            operation_id: agent_contracts::OperationId::new(),
            effect_id: agent_contracts::EffectId::new(),
            reservation_id: "pass/1".into(),
            settlement: agent_contracts::EffectAckSettlement::Applied {
                durability: agent_contracts::EffectDurability::Durable,
            },
            error: "ack lost".into(),
        };
        projection.fold(&RuntimeEvent::EffectAckDebtResolved {
            debt,
            resolution: agent_contracts::EffectReconciliation::NotManaged,
        });
        assert_eq!(projection.unresolved_ack_debts(), 4);
        for _ in 0..10 {
            projection.fold(&RuntimeEvent::Warning {
                message: format!("w{}", projection.recent_warnings.len()),
            });
        }
        assert!(projection.recent_warnings.len() <= MAX_RETAINED_WARNINGS);
        projection.fold(&RuntimeEvent::ToolFinished {
            output: tool_output,
            facts: None,
        });
        let rendered = projection.lines().join("\n");
        assert!(rendered.contains("ack_debts=4"));
        assert!(rendered.contains("recent warning:"));
    }
}
