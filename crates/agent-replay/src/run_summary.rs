//! RUN-PROJECTION: the smallest rebuildable Run/Task projection, folded
//! offline from a trace's public event stream. It answers "what happened
//! in this run" — lifecycle, per-task goals and outcomes, checkpoint
//! index, recovery debts, costs — without touching runtime internals or
//! any store. Read-only, disposable, rebuildable by re-reading the trace:
//! never a second authority.

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct TaskSummaryRow {
    pub task_id: String,
    pub goal: String,
    pub anchor_revision: u64,
    pub completed: bool,
    pub completion_summary: String,
}

#[derive(Debug, Clone, Default)]
pub struct RunTaskSummary {
    pub run_id: String,
    pub started: bool,
    pub completed: bool,
    pub user_messages: usize,
    pub model_rounds: usize,
    pub tool_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub unresolved_ack_debts: usize,
    pub recovery_required: bool,
    pub required_miss_events: usize,
    pub required_miss_items: usize,
    pub checkpoints: Vec<(String, u64)>,
    pub tasks: Vec<TaskSummaryRow>,
}

impl RunTaskSummary {
    fn task_row(&mut self, task_id: &str) -> &mut TaskSummaryRow {
        let existing = self
            .tasks
            .iter()
            .position(|row| row.task_id == task_id)
            .unwrap_or_else(|| {
                self.tasks.push(TaskSummaryRow {
                    task_id: task_id.to_string(),
                    ..Default::default()
                });
                self.tasks.len() - 1
            });
        &mut self.tasks[existing]
    }
}

fn as_u64(value: Option<&Value>) -> u64 {
    value.and_then(|value| value.as_u64()).unwrap_or(0)
}

fn as_str(value: Option<&Value>) -> &str {
    value.and_then(|value| value.as_str()).unwrap_or("")
}

/// Fold one trace JSONL stream into a per-run summary.
pub fn fold_run_summary(lines: impl Iterator<Item = String>, summary: &mut RunTaskSummary) {
    for line in lines {
        let Ok(envelope) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if summary.run_id.is_empty() {
            summary.run_id = envelope
                .get("run_id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
        }
        let Some(event) = envelope.get("event") else {
            continue;
        };
        let Some(kind) = event.get("type").and_then(|value| value.as_str()) else {
            continue;
        };
        match kind {
            "run_started" => summary.started = true,
            "run_completed" => summary.completed = true,
            "user_message_accepted" => summary.user_messages += 1,
            "model_started" => summary.model_rounds += 1,
            "model_used" => {
                summary.input_tokens += as_u64(event.get("input_tokens"));
                summary.output_tokens += as_u64(event.get("output_tokens"));
            }
            "tool_started" => summary.tool_calls += 1,
            "focus_changed" => {
                let task_id = as_str(event.get("task_id"));
                if !task_id.is_empty() {
                    summary.task_row(task_id).goal = as_str(event.get("goal")).to_string();
                }
            }
            "task_anchor_changed" => {
                let task_id = as_str(event.get("task_id"));
                if !task_id.is_empty() {
                    let revision = as_u64(event.get("revision"));
                    let row = summary.task_row(task_id);
                    row.anchor_revision = row.anchor_revision.max(revision);
                }
            }
            "task_completed" => {
                let task_id = as_str(event.get("task_id"));
                if !task_id.is_empty() {
                    let row = summary.task_row(task_id);
                    row.completed = true;
                    row.completion_summary = as_str(event.get("summary")).to_string();
                    row.anchor_revision = row
                        .anchor_revision
                        .max(as_u64(event.get("anchor_revision")));
                }
            }
            "checkpoint_durable" => {
                let artifact = as_str(event.get("artifact")).to_string();
                if !artifact.is_empty() {
                    summary
                        .checkpoints
                        .push((artifact, as_u64(event.get("bytes"))));
                }
            }
            "effect_ack_debt" => summary.unresolved_ack_debts += 1,
            "effect_ack_debt_resolved" => {
                summary.unresolved_ack_debts = summary.unresolved_ack_debts.saturating_sub(1)
            }
            "recovery_required" => summary.recovery_required = true,
            "context_degraded" => {
                let items = event
                    .get("required_misses")
                    .and_then(|misses| misses.get("total"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as usize;
                if items > 0 {
                    summary.required_miss_events += 1;
                    summary.required_miss_items += items;
                }
            }
            _ => {}
        }
    }
}

/// Fold each trace file into its own summary (traces are single-run, but a
/// run id change inside one file starts a new summary).
pub fn run_summaries_from_files(
    paths: &[std::path::PathBuf],
) -> anyhow::Result<Vec<RunTaskSummary>> {
    let mut summaries = Vec::new();
    for path in paths {
        let content = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
        let mut summary = RunTaskSummary::default();
        fold_run_summary(content.lines().map(|line| line.to_string()), &mut summary);
        if !summary.run_id.is_empty() {
            summaries.push(summary);
        }
    }
    Ok(summaries)
}

/// Compact human report for one run.
pub fn render_run_summary(summary: &RunTaskSummary) -> String {
    let lifecycle = if summary.completed {
        "completed"
    } else if summary.started {
        "in progress"
    } else {
        "unknown"
    };
    let mut out = String::new();
    out.push_str(&format!(
        "# Run {} — {}\nuser messages {} | model rounds {} | tool calls {} | tokens in {} / out {}\n",
        summary.run_id,
        lifecycle,
        summary.user_messages,
        summary.model_rounds,
        summary.tool_calls,
        summary.input_tokens,
        summary.output_tokens,
    ));
    out.push_str(&format!(
        "recovery: ack debts {} | recovery required {}\n",
        summary.unresolved_ack_debts, summary.recovery_required,
    ));
    if summary.required_miss_events > 0 {
        out.push_str(&format!(
            "required context misses: {} item(s) across {} degraded round(s)\n",
            summary.required_miss_items, summary.required_miss_events,
        ));
    }
    out.push_str(&format!("checkpoints: {}\n", summary.checkpoints.len()));
    for (artifact, bytes) in &summary.checkpoints {
        out.push_str(&format!("  - {artifact} ({bytes} bytes)\n"));
    }
    out.push_str(&format!("tasks: {}\n", summary.tasks.len()));
    for task in &summary.tasks {
        let state = if task.completed {
            "completed"
        } else {
            "active"
        };
        out.push_str(&format!(
            "  - {} [{state}] anchor r{} — {}\n",
            task.task_id,
            task.anchor_revision,
            if task.completion_summary.is_empty() {
                task.goal.clone()
            } else {
                task.completion_summary.clone()
            }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(run: &str, event: &str, extra: &str) -> String {
        format!(
            r#"{{"run_id":"{run}","seq":1,"timestamp_ms":1,"event":{{"type":"{event}"{extra}}}}}"#
        )
    }

    #[test]
    fn a_full_run_folds_into_the_projection() {
        let lines = vec![
            line("run-1", "run_started", ""),
            line(
                "run-1",
                "focus_changed",
                r#", "task_id": "t-1", "goal": "fix the retry table""#,
            ),
            line("run-1", "user_message_accepted", ""),
            line("run-1", "model_started", ""),
            line(
                "run-1",
                "model_used",
                r#", "input_tokens": 900, "output_tokens": 40"#,
            ),
            line("run-1", "tool_started", ""),
            line(
                "run-1",
                "checkpoint_durable",
                r#", "artifact": "checkpoint-1-x.json", "bytes": 2048"#,
            ),
            line(
                "run-1",
                "effect_ack_debt",
                r#", "debt": {"operation_id": "o", "effect_id": "e", "reservation_id": "r", "settlement": {"kind":"applied","durability":"Durable"}, "error": "lost"}"#,
            ),
            line("run-1", "effect_ack_debt_resolved", ""),
            line(
                "run-1",
                "task_completed",
                r#", "task_id": "t-1", "anchor_revision": 4, "summary": "the retry table is fixed""#,
            ),
            line("run-1", "run_completed", ""),
        ];
        let mut summary = RunTaskSummary::default();
        fold_run_summary(lines.into_iter(), &mut summary);
        assert!(summary.started && summary.completed);
        assert_eq!(summary.user_messages, 1);
        assert_eq!(summary.model_rounds, 1);
        assert_eq!(summary.tool_calls, 1);
        assert_eq!(summary.input_tokens, 900);
        assert_eq!(summary.unresolved_ack_debts, 0);
        assert_eq!(summary.checkpoints.len(), 1);
        assert_eq!(summary.tasks.len(), 1);
        let task = &summary.tasks[0];
        assert_eq!(task.goal, "fix the retry table");
        assert!(task.completed);
        assert_eq!(task.completion_summary, "the retry table is fixed");
        assert_eq!(task.anchor_revision, 4);

        let rendered = render_run_summary(&summary);
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("checkpoint-1-x.json"));
        assert!(rendered.contains("the retry table is fixed"));
    }

    #[test]
    fn open_debts_misses_and_active_tasks_stay_visible() {
        let lines = vec![
            line("run-2", "run_started", ""),
            line(
                "run-2",
                "focus_changed",
                r#", "task_id": "t-9", "goal": "still working""#,
            ),
            line(
                "run-2",
                "effect_ack_debt",
                r#", "debt": {"operation_id": "o", "effect_id": "e", "reservation_id": "r", "settlement": {"kind":"applied","durability":"Durable"}, "error": "lost"}"#,
            ),
            line("run-2", "recovery_required", ""),
            line(
                "run-2",
                "context_degraded",
                r#", "required_misses": {"total": 2, "entries": [], "omitted": 0}"#,
            ),
        ];
        let mut summary = RunTaskSummary::default();
        fold_run_summary(lines.into_iter(), &mut summary);
        assert!(!summary.completed);
        assert_eq!(summary.unresolved_ack_debts, 1);
        assert!(summary.recovery_required);
        assert_eq!(summary.required_miss_events, 1);
        assert_eq!(summary.required_miss_items, 2);
        let rendered = render_run_summary(&summary);
        assert!(rendered.contains("ack debts 1"));
        assert!(rendered.contains("recovery required true"));
        assert!(rendered.contains("still working"));
    }
}
