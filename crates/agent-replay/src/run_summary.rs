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
    /// EXEC-4 (E08): lines this summary's file skipped — oversized lines
    /// and rows beyond the per-file summary budget. A non-zero count means
    /// the rendered totals are a bounded view, not the whole stream.
    pub omitted_lines: u64,
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

/// Fold one trace JSONL stream into a per-run summary. A run id change
/// inside the stream starts the summary over: one folded summary describes
/// exactly one run, never a blend of the first run with later ones.
pub fn fold_run_summary(lines: impl Iterator<Item = String>, summary: &mut RunTaskSummary) {
    for line in lines {
        let Ok(envelope) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let envelope_run = envelope
            .get("run_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if summary.run_id.is_empty() {
            summary.run_id = envelope_run.to_string();
        } else if !envelope_run.is_empty() && envelope_run != summary.run_id {
            // Mixed-run stream: this fold keeps only the new run. Callers
            // that need every run in one file split by id before folding
            // (or fold once per run id).
            *summary = RunTaskSummary {
                run_id: envelope_run.to_string(),
                ..Default::default()
            };
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
                // The wire shape of `ContextMaterializationMisses` is
                // `{entries: [...], omitted: N}` — there is no `total`
                // field on the wire, so the count is the entries plus the
                // saturating omission counter.
                let items = event
                    .get("required_misses")
                    .map(|misses| {
                        misses
                            .get("entries")
                            .and_then(|entries| entries.as_array())
                            .map(|entries| entries.len())
                            .unwrap_or(0)
                            + as_u64(misses.get("omitted")) as usize
                    })
                    .unwrap_or(0);
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
/// EXEC-4 (E08): 单行读入上限——超长单行按损坏/滥用计入 omitted，绝不
/// 为一行分配无界内存。trace 事件是单行 JSON，合法行远低于此界。
const MAX_TRACE_LINE_BYTES: usize = 1024 * 1024;
/// EXEC-4 (E08)：单文件最多折叠的摘要数（trace 内 run id 反复翻转也不能
/// 让结果集合无限增长）；超出的行计入 omitted。
const MAX_SUMMARIES_PER_FILE: usize = 64;

pub fn run_summaries_from_files(
    paths: &[std::path::PathBuf],
) -> anyhow::Result<Vec<RunTaskSummary>> {
    let mut summaries = Vec::new();
    for path in paths {
        let file = std::fs::File::open(path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        let mut file_summaries: Vec<RunTaskSummary> = Vec::new();
        let mut omitted: u64 = 0;
        loop {
            // EXEC-4：行界内读入——超长单行按损坏计入 omitted，且跳行时
            // 用 fill_buf/consume 保住换行边界（下一行不被吞掉）。
            let next = match next_bounded_line(&mut reader, MAX_TRACE_LINE_BYTES) {
                Ok(next) => next,
                Err(error) => return Err(anyhow::anyhow!("read {}: {error}", path.display())),
            };
            let Some(line) = next else {
                break;
            };
            match line {
                BoundedLine::Line(line) => {
                    let run_id = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("run_id")
                                .and_then(|value| value.as_str())
                                .map(str::to_owned)
                        });
                    let Some(run_id) = run_id else {
                        omitted += 1;
                        continue;
                    };
                    let slot = file_summaries
                        .iter_mut()
                        .find(|summary| summary.run_id == run_id);
                    if let Some(summary) = slot {
                        fold_run_summary([line].into_iter(), summary);
                    } else if file_summaries.len() < MAX_SUMMARIES_PER_FILE {
                        let mut summary = RunTaskSummary::default();
                        fold_run_summary([line].into_iter(), &mut summary);
                        file_summaries.push(summary);
                    } else {
                        // Beyond the per-file summary budget: the rows are
                        // counted as omitted instead of folding forever.
                        omitted += 1;
                    }
                }
                BoundedLine::Oversized => {
                    omitted += 1;
                }
            }
        }
        file_summaries.retain(|summary| !summary.run_id.is_empty());
        for summary in &mut file_summaries {
            summary.omitted_lines = omitted;
        }
        summaries.extend(file_summaries);
    }
    Ok(summaries)
}

/// EXEC-4: one streamed trace line — [`BoundedLine::Oversized`] marks a
/// single line beyond the budget, whose content was discarded at the
/// newline boundary (the following line is never consumed).
enum BoundedLine {
    Line(String),
    Oversized,
}

/// Bounded line reader over any buffered source: fills the buffer, keeps
/// bytes up to the newline, and — once a line crosses the cap — discards
/// whole windows until the newline appears, so the reader stays exactly at
/// the next line's first byte. Memory is O(window + cap), never O(line).
fn next_bounded_line<R: std::io::BufRead>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<BoundedLine>> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            // EOF: a non-empty tail is the file's last (newline-free) line.
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(BoundedLine::Line(
                    String::from_utf8_lossy(&line).into_owned(),
                )))
            };
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(pos) => {
                // The line bytes are `line + available[..pos]`; the newline
                // itself never counts toward the cap.
                let fits = line.len() + pos <= cap;
                if fits {
                    line.extend_from_slice(&available[..pos]);
                }
                reader.consume(pos + 1);
                return if fits {
                    Ok(Some(BoundedLine::Line(
                        String::from_utf8_lossy(&line).into_owned(),
                    )))
                } else {
                    Ok(Some(BoundedLine::Oversized))
                };
            }
            None => {
                let window = available.len();
                if line.len() + window > cap {
                    // The line provably crosses the cap: discard whole
                    // windows (newline included when it appears) until the
                    // boundary, then report one oversized line.
                    reader.consume(window);
                    line.clear();
                    loop {
                        let rest = reader.fill_buf()?;
                        if rest.is_empty() {
                            return Ok(Some(BoundedLine::Oversized));
                        }
                        match rest.iter().position(|byte| *byte == b'\n') {
                            Some(pos) => {
                                reader.consume(pos + 1);
                                return Ok(Some(BoundedLine::Oversized));
                            }
                            None => {
                                let size = rest.len();
                                reader.consume(size);
                            }
                        }
                    }
                }
                line.extend_from_slice(available);
                let size = available.len();
                reader.consume(size);
            }
        }
    }
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
        ];
        let mut summary = RunTaskSummary::default();
        fold_run_summary(lines.into_iter(), &mut summary);
        assert!(!summary.completed);
        assert_eq!(summary.unresolved_ack_debts, 1);
        assert!(summary.recovery_required);
        let rendered = render_run_summary(&summary);
        assert!(rendered.contains("ack debts 1"));
        assert!(rendered.contains("recovery required true"));
        assert!(rendered.contains("still working"));
    }

    /// The degraded-round misses must be counted from a REAL serialized
    /// `RuntimeEvent::ContextDegraded` — the wire shape of
    /// `ContextMaterializationMisses` is `{entries, omitted}` with no
    /// `total` field, so a hand-written `{total}` fixture cannot prove the
    /// projection reads the events the runtime actually publishes.
    #[test]
    fn required_misses_are_counted_from_real_serialized_events() {
        use agent_contracts::{
            ContextMaterializationIdentity, ContextMaterializationMiss,
            ContextMaterializationMissReason, ContextMaterializationMisses,
        };

        let mut misses = ContextMaterializationMisses::default();
        for index in 0..3 {
            misses.push(ContextMaterializationMiss {
                identity: ContextMaterializationIdentity {
                    item_ref: format!("item-{index}"),
                    item_id: None,
                    source_field_id: "constraints".into(),
                    anchor_revision: 0,
                },
                reason: ContextMaterializationMissReason::BudgetExcluded,
            });
        }
        let event = agent_contracts::RuntimeEvent::ContextDegraded {
            turn_id: agent_contracts::TurnId::new(),
            model_round: 1,
            materialization_id: 0,
            required_misses: misses,
            optional_misses: ContextMaterializationMisses::default(),
        };
        let envelope = agent_contracts::RuntimeEventEnvelope {
            run_id: agent_contracts::RunId::new(),
            seq: 7,
            timestamp_ms: 7,
            event,
        };
        let line = serde_json::to_string(&envelope).unwrap();
        assert!(
            !line.contains("\"total\""),
            "the wire must not carry a total field: {line}"
        );

        let mut summary = RunTaskSummary::default();
        fold_run_summary(std::iter::once(line), &mut summary);
        assert_eq!(summary.required_miss_events, 1);
        assert_eq!(
            summary.required_miss_items, 3,
            "entries.len() + omitted must equal the real miss count"
        );
        let rendered = render_run_summary(&summary);
        assert!(rendered.contains("required context misses: 3 item(s)"));
    }

    /// A mid-file run id change restarts the fold: one folded summary
    /// describes exactly one run, never a blend of run-a with run-b.
    #[test]
    fn two_runs_in_one_file_produce_two_summaries() {
        let lines = vec![
            line("run-a", "run_started", ""),
            line("run-a", "user_message_accepted", ""),
            line("run-b", "run_started", ""),
            line("run-b", "user_message_accepted", ""),
            line("run-b", "user_message_accepted", ""),
        ];
        let mut summary = RunTaskSummary::default();
        fold_run_summary(lines.into_iter(), &mut summary);
        assert_eq!(summary.run_id, "run-b");
        assert_eq!(summary.user_messages, 2, "only run-b's events are counted");
        assert!(summary.started, "run-b's own start is part of its summary");
    }
}

#[cfg(test)]
mod exec4_tests {
    use super::*;

    /// EXEC-4：行界——恰好上限的行折叠、超一字节的行计入 omitted，且其
    /// 后的行仍然被折叠（换行边界不被吞掉）。
    #[test]
    fn bounded_lines_keep_the_boundary_across_an_oversized_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let mut content = String::new();
        // 恰好上限的一行（合法 JSON 事件）：按前后缀精确补齐。
        let prefix = "{\"run_id\":\"r1\",\"event\":{\"type\":\"run_started\"},\"pad\":\"";
        let pad = MAX_TRACE_LINE_BYTES - prefix.len() - 3;
        content.push_str(prefix);
        content.push_str(&"a".repeat(pad));
        content.push_str("\"}\n");
        debug_assert_eq!(
            content.len(),
            MAX_TRACE_LINE_BYTES,
            "the regression needs an exactly-cap line"
        );
        // 超一字节的行。
        content.push_str(&format!(
            "{{\"run_id\":\"r1\",\"pad\":\"{}\"}}\n",
            "b".repeat(MAX_TRACE_LINE_BYTES)
        ));
        // 之后的行必须仍然被折叠到同一 run。
        content.push_str("{\"run_id\":\"r1\",\"event\":{\"type\":\"run_completed\"}}\n");
        std::fs::write(&path, content).unwrap();

        let summaries = run_summaries_from_files(&[path]).unwrap();
        eprintln!("DBG summaries={summaries:?}");
        let summary = &summaries[0];
        assert_eq!(summary.run_id, "r1");
        assert!(summary.started, "the exactly-cap line folded");
        assert!(summary.completed, "the line after the oversized one folded");
        assert_eq!(summary.omitted_lines, 1, "the over-cap line was counted");
    }

    /// EXEC-4：整文件不再一次性装入——用超长单行 + 大量行验证内存路径
    /// 有界（结果集合有摘要数预算，超出的计入 omitted）。
    #[test]
    fn per_file_summary_budget_and_omissions_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.jsonl");
        let mut content = String::new();
        for index in 0..(MAX_SUMMARIES_PER_FILE + 10) {
            content.push_str("{\"run_id\":\"run-");
            content.push_str(&index.to_string());
            content.push_str(
                "\",\"event\":{\"type\":\"run_started\"}}
",
            );
        }
        content.push_str("not an envelope\n");
        std::fs::write(&path, content).unwrap();

        let summaries = run_summaries_from_files(&[path]).unwrap();
        assert_eq!(summaries.len(), MAX_SUMMARIES_PER_FILE);
        assert_eq!(
            summaries[0].omitted_lines,
            10 + 1,
            "beyond-budget and non-envelope rows are counted"
        );
    }
}
