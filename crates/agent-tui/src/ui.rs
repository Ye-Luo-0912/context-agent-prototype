use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::state::{AppState, PendingApproval, UiRole};

pub fn render(frame: &mut Frame<'_>, app: &AppState) {
    // When an approval is pending, shrink the history so the scrollable
    // approval panel gets the rest of the screen.
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if app.pending_approval.is_some() {
            [
                Constraint::Length(4),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(8),
            ]
        } else {
            [
                Constraint::Min(10),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Length(3),
            ]
        })
        .split(frame.area());

    let (history_area, inspect_area) = if app.show_context_panel {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(root[0]);
        (columns[0], Some(columns[1]))
    } else {
        (root[0], None)
    };

    let history = conversation_lines(app);
    let inner_width = history_area.width.saturating_sub(2);
    let inner_height = history_area.height.saturating_sub(2);
    let scroll = conversation_scroll(&history, inner_width, inner_height, app.scroll);
    let history = Paragraph::new(Text::from(history))
        .block(Block::default().borders(Borders::ALL).title("Conversation"))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(history, history_area);

    if let Some(area) = inspect_area {
        render_context_panel(frame, area, app);
    }

    let status = Paragraph::new(format!(
        "run: {}\nstatus: {}\ntool: {}",
        app.run_id, app.status, app.tool_status
    ))
    .block(Block::default().borders(Borders::ALL).title("Runtime"));
    frame.render_widget(status, root[1]);

    let context = Paragraph::new(format!(
        "active={}  cooling={}  archived={}  dropped={}  total={}\nactive budget~{} tok  focus_generation={}  turn={}  round={}",
        app.context.active_items,
        app.context.cooling_items,
        app.context.archived_items,
        app.context.tombstoned_items,
        app.context.total_items,
        app.context.approx_active_tokens,
        app.context.focus_generation,
        app.context.turn,
        app.context.tool_round,
    ))
    .block(Block::default().borders(Borders::ALL).title("Context Working Set"));
    frame.render_widget(context, root[2]);

    if let Some(pending) = &app.pending_approval {
        render_approval(frame, root[3], pending, app.approval_scroll);
        return;
    }

    let input_area = root[3];
    let inner_w = input_area.width.saturating_sub(2);
    // Cursor column is the TERMINAL DISPLAY width of the input (CJK and
    // wide chars count as 2), not the Rust `char` count — otherwise the
    // cursor drifts on Chinese / wide input.
    let cursor_col = display_width(&app.input);
    // Horizontal viewport: keep the cursor on screen for input longer than
    // the panel. The view follows the cursor from the left.
    let hscroll = if cursor_col >= inner_w as usize {
        (cursor_col - inner_w as usize + 1) as u16
    } else {
        0
    };
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("Input"))
        .scroll((0, hscroll));
    frame.render_widget(input, input_area);
    let cursor_x = input_area.x
        + 1
        + (cursor_col.saturating_sub(hscroll as usize)).min(inner_w as usize) as u16;
    let cursor_y = input_area.y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Render the pending approval as a scrollable panel. `detail` already holds
/// the COMPLETE request; `holdback` (set by PageUp/PageDown in the session
/// loop) pages through it from the top. The trailing sentinel line
/// (`— end of request <id> —`) is always reachable because the scroll offset
/// is clamped to the last page.
fn render_approval(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    pending: &PendingApproval,
    holdback: u16,
) {
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    let offset = approval_scroll_offset(&pending.detail, inner_w, inner_h, holdback);

    let mut lines: Vec<Line> = Vec::with_capacity(pending.detail.len() + 1);
    if pending.truncated {
        lines.push(Line::from(
            "[!] arguments were truncated to fit the panel — the request exceeded the hard display cap",
        ));
    }
    for detail in &pending.detail {
        lines.push(Line::from(detail.clone()));
    }

    let title = format!(
        "Approval Required — {}  ([y]allow [n]deny [Enter] [Esc] · PgUp/PgDn scroll)",
        pending.tool_name
    );
    let panel = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(panel, area);
}

/// Top-anchored scroll offset for the approval panel. `holdback` is how many
/// rows the operator paged down from the top; it is clamped to the last page
/// so every argument and the trailing sentinel stay reachable.
fn approval_scroll_offset(detail: &[String], inner_w: u16, inner_h: u16, holdback: u16) -> u16 {
    let total = wrapped_rows(detail, inner_w);
    let max_skip = total.saturating_sub(inner_h.max(1) as usize);
    holdback.min(max_skip as u16)
}

/// Total wrapped rows a list of lines occupies at `inner_width`, using the
/// same display-width rule ratatui's `Paragraph` wraps by. This is the single
/// source of truth for the approval scroll bound so the viewport edge matches
/// the rendered layout — no separate "total width ÷ width" estimate.
fn wrapped_rows(lines: &[String], inner_width: u16) -> usize {
    let width = inner_width.max(1) as usize;
    lines
        .iter()
        .map(|line| display_width(line).max(1).div_ceil(width))
        .sum()
}

/// Terminal display column width of `s`. ASCII is 1 column; East-Asian wide /
/// fullwidth ranges are 2; everything else (combining, narrow) is 1. This is
/// the width rule ratatui uses for layout, so cursor placement and wrapping
/// agree with what is actually drawn. (`unicode-width` would give the same
/// answer; it is a transitive dependency here and is intentionally not added
/// to keep this change to the three named source files, so we inline the
/// focused wide-char table instead.)
fn display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

fn char_display_width(c: char) -> usize {
    if c.is_ascii() {
        1
    } else if is_wide(c) {
        2
    } else {
        1
    }
}

fn is_wide(c: char) -> bool {
    let u = c as u32;
    (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0x303E).contains(&u)
        || (0x3041..=0x33FF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xA000..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u)
}

fn render_context_panel(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &AppState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let selected_lines: Vec<Line> = app
        .context_selected
        .iter()
        .flat_map(|selection| {
            // Two real lines: the score/id header, then the reason indented
            // on its own row. Embedding '\n' in a single `Line` would collapse
            // it, like the conversation body used to.
            vec![
                Line::from(format!(
                    "score={:.2} tok={} {}",
                    selection.score,
                    selection.approx_tokens,
                    short_id(&selection.item_id)
                )),
                Line::from(format!("  {}", selection.reason)),
            ]
        })
        .collect();
    let selected_paragraph = if selected_lines.is_empty() {
        Paragraph::new("(no selection yet)")
    } else {
        Paragraph::new(Text::from(selected_lines))
    };
    let selected = selected_paragraph.block(
        Block::default()
            .borders(Borders::ALL)
            .title("Selected (latest model turn)"),
    );
    frame.render_widget(selected, rows[0]);

    let transition_lines: Vec<Line> = app
        .context_transitions
        .iter()
        .rev()
        .flat_map(|transition| {
            // Header line + indented reason line, kept as two real rows.
            vec![
                Line::from(format!(
                    "turn {}: {:?}->{:?} {:?}",
                    transition.turn, transition.from, transition.to, transition.kind
                )),
                Line::from(format!("  {}", transition.reason)),
            ]
        })
        .collect();
    let transitions_paragraph = if transition_lines.is_empty() {
        Paragraph::new("(no transitions yet)")
    } else {
        Paragraph::new(Text::from(transition_lines))
    };
    let transitions = transitions_paragraph.block(
        Block::default()
            .borders(Borders::ALL)
            .title("Lifecycle transitions"),
    );
    frame.render_widget(transitions, rows[1]);
}

fn short_id(id: &agent_contracts::ContextItemId) -> String {
    let text = id.to_string();
    text.chars().take(8).collect()
}

pub(crate) fn conversation_lines(app: &AppState) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for message in &app.messages {
        let prefix = match message.role {
            UiRole::User => "YOU",
            UiRole::Assistant => "AGENT",
            UiRole::Tool => "TOOL",
            UiRole::System => "SYSTEM",
        };
        out.push(Line::from(Span::raw(format!("[{prefix}]"))));
        // Preserve the original multi-line structure: a ratatui `Line` is NOT
        // a multi-line container — `Line::from(String)` drops embedded
        // newlines — so each source newline becomes its own rendered `Line`.
        // This keeps code blocks, error stacks, plans and help text on their
        // real rows, which later `Wrap` can only re-flow, never reconstruct.
        for line in message.content.split('\n') {
            out.push(Line::from(line.to_string()));
        }
        out.push(Line::from(""));
    }
    out
}

/// `holdback` is how many wrapped rows above the latest the operator
/// asked to keep (PageUp). Zero follows the tail so [YOU]/AGENT/TOOL
/// rows are not hidden under the opening SYSTEM banners.
pub(crate) fn conversation_scroll(
    lines: &[Line<'_>],
    inner_width: u16,
    inner_height: u16,
    holdback: u16,
) -> u16 {
    let wrapped = wrapped_row_count(lines, inner_width);
    let max_skip = wrapped.saturating_sub(inner_height.max(1) as usize);
    max_skip.saturating_sub(holdback as usize) as u16
}

fn wrapped_row_count(lines: &[Line<'_>], inner_width: u16) -> usize {
    let width = inner_width.max(1) as usize;
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

/// Visible Conversation rows for a given pane size. Tests use a width
/// wide enough that nothing wraps, so each source line is one row.
#[cfg(test)]
pub(crate) fn visible_conversation(
    app: &AppState,
    inner_width: u16,
    inner_height: u16,
    holdback: u16,
) -> Vec<String> {
    let lines = conversation_lines(app);
    let rows: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let scroll = conversation_scroll(&lines, inner_width, inner_height, holdback) as usize;
    let end = (scroll + inner_height.max(1) as usize).min(rows.len());
    rows.get(scroll..end).unwrap_or(&[]).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use agent_contracts::{RunId, RuntimeEvent, RuntimeEventEnvelope, TaskId};

    fn envelope(event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: RunId::new(),
            seq: 1,
            timestamp_ms: 0,
            event,
        }
    }

    #[test]
    fn conversation_follows_the_latest_dialogue_not_the_opening_banners() {
        let mut app = AppState::new(RunId::new());
        app.push_system("context policy: dynamic".into());
        app.push_system("serving: deepseek-flash | profile digest abc".into());
        app.apply_runtime_event(envelope(RuntimeEvent::FocusChanged {
            task_id: TaskId::new(),
            goal: "请根据笔记.md整理目录".into(),
        }));
        let input =
            agent_contracts::RuntimeInputEnvelope::from_preview("请列出目录并更新笔记.md；");
        app.apply_runtime_event(envelope(RuntimeEvent::UserMessageAccepted { input }));
        app.apply_runtime_event(envelope(RuntimeEvent::AssistantMessage {
            content: "先看目录，再写笔记。".into(),
        }));
        app.apply_runtime_event(envelope(RuntimeEvent::ToolFinished {
            output: agent_contracts::ToolOutput {
                call_id: "call-1".into(),
                tool_name: "fs.list".into(),
                ok: true,
                summary: "3 entries".into(),
                model_content: String::new(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            facts: None,
        }));

        let roles: Vec<_> = app.messages.iter().map(|message| message.role).collect();
        assert!(roles.contains(&UiRole::User), "{roles:?}");
        assert!(roles.contains(&UiRole::Assistant), "{roles:?}");
        assert!(roles.contains(&UiRole::Tool), "{roles:?}");

        // A short pane that can only show the opening SYSTEM/Focus
        // banners if scroll stays pinned at the top.
        let visible = visible_conversation(&app, 80, 10, 0);
        let joined = visible.join("\n");
        assert!(
            joined.contains("[YOU]") || joined.contains("请列出目录"),
            "user prompt must be in the followed viewport:\n{joined}"
        );
        assert!(
            joined.contains("[AGENT]") || joined.contains("先看目录"),
            "assistant reply must be in the followed viewport:\n{joined}"
        );
        assert!(
            joined.contains("[TOOL]") || joined.contains("3 entries"),
            "tool summary must be in the followed viewport:\n{joined}"
        );
        assert!(
            !joined.contains("Prototype ready"),
            "following the tail must leave the opening banner: {joined}"
        );
    }
}

/// Render `app` through the real `ui::render` onto a `TestBackend` and return
/// the visible rows as strings, so assertions inspect what the operator would
/// actually see in the buffer (not internal strings).
#[cfg(test)]
mod render_tests {
    use crate::state::{AppState, UiMessage, UiRole};
    use agent_contracts::{RunId, ToolCall, ToolRisk, ToolSpec};
    use agent_core::ApprovalRequest;
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
    };

    fn render_rows(app: &AppState, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let area = buf.area();
        let mut rows = Vec::with_capacity(area.height as usize);
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            rows.push(row);
        }
        rows
    }

    fn approval_request(n: usize, value: impl Fn(usize) -> String) -> ApprovalRequest {
        let mut map = serde_json::Map::new();
        for i in 1..=n {
            map.insert(format!("arg{i}"), serde_json::Value::String(value(i)));
        }
        ApprovalRequest {
            request_id: "req-detail".into(),
            call: ToolCall {
                id: "c".into(),
                name: "fs.write".into(),
                arguments: serde_json::Value::Object(map),
            },
            spec: ToolSpec {
                name: "fs.write".into(),
                description: "write a file".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::WorkspaceWrite,
                roles: Vec::new(),
                output_budget: None,
            },
        }
    }

    #[test]
    fn multiline_body_keeps_each_source_line_on_its_own_row() {
        let mut app = AppState::new(RunId::new());
        // A code block / error stack: three real lines plus an empty line.
        app.messages.push(UiMessage {
            role: UiRole::User,
            content: "first line\nsecond line\nthird line".into(),
        });
        app.messages.push(UiMessage {
            role: UiRole::Tool,
            content: "seg1\n\nseg2".into(),
        });

        let rows = render_rows(&app, 80, 24);
        let joined = rows.concat();
        let one = rows.iter().position(|r| r.contains("first line")).unwrap();
        let two = rows.iter().position(|r| r.contains("second line")).unwrap();
        let three = rows.iter().position(|r| r.contains("third line")).unwrap();
        assert!(
            one < two && two < three,
            "lines must keep source order: {joined:?}"
        );

        let s1 = rows.iter().position(|r| r.contains("seg1")).unwrap();
        let s2 = rows.iter().position(|r| r.contains("seg2")).unwrap();
        // The interior empty line must survive as its own row: seg2 is at
        // least two rows below seg1 (seg1, blank, seg2).
        assert!(
            s2 >= s1 + 2,
            "the empty line between seg1 and seg2 must be preserved: {rows:?}"
        );
    }

    #[test]
    fn approval_detail_is_full_and_scrollable_to_the_tail_sentinel() {
        let mut app = AppState::new(RunId::new());
        app.begin_approval(approval_request(12, |i| format!("value-for-arg-{i}")));

        // At rest (top of the panel) the trailing sentinel is not yet shown.
        let top = render_rows(&app, 80, 24).concat();
        assert!(
            !top.contains("— end of request req-detail —"),
            "tail sentinel must be hidden before paging: {top:?}"
        );

        // Page to the bottom; the offset is clamped to the last page so the
        // sentinel and every argument stay reachable.
        app.approval_scroll = u16::MAX;
        for &width in &[80u16, 60u16] {
            let rows = render_rows(&app, width, 24);
            let joined = rows.concat();
            assert!(
                joined.contains("— end of request req-detail —"),
                "tail sentinel must be reachable at width {width}: {rows:?}"
            );
            assert!(
                joined.contains("arg12:"),
                "the 12th argument must be viewable after scrolling at width {width}"
            );
            assert!(
                joined.contains("value-for-arg-12"),
                "the full value of arg12 must be visible at width {width}"
            );
        }
    }

    #[test]
    fn long_args_with_shared_prefix_are_verifiable_at_the_tail() {
        // Two long params whose first 200 chars are identical; only the tail
        // differs. The operator must be able to confirm the tail after paging,
        // in both an 80-wide and a narrower (60-wide) terminal.
        let prefix = "Z".repeat(200);
        let mut app = AppState::new(RunId::new());
        app.begin_approval(approval_request(2, |i| format!("{prefix}TAIL_MARK_{i}")));
        app.approval_scroll = u16::MAX;
        for &width in &[80u16, 60u16] {
            let rows = render_rows(&app, width, 24);
            let joined = rows.concat();
            assert!(
                joined.contains("TAIL_MARK_1"),
                "tail of arg1 must be verifiable at width {width}: {rows:?}"
            );
            assert!(
                joined.contains("TAIL_MARK_2"),
                "tail of arg2 must be verifiable at width {width}: {rows:?}"
            );
        }
    }

    #[test]
    fn chinese_input_cursor_lands_on_the_display_column() {
        let mut app = AppState::new(RunId::new());
        app.input = "中文输入测试".into(); // 6 wide chars => display width 12
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &app))
            .expect("draw");
        let cursor = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("cursor is set");
        // Input area is the bottom Length(3) region: x=0, width=80, y=21..23.
        assert_eq!(
            (cursor.x, cursor.y),
            (13, 22),
            "cursor must sit at display-width col 12 + 1, not the 6 char count"
        );
    }

    #[test]
    fn long_chinese_input_stays_within_the_input_viewport() {
        let mut app = AppState::new(RunId::new());
        app.input = "中文".repeat(50); // display width 200, far wider than 80
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &app))
            .expect("draw");
        let cursor = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("cursor is set");
        assert!(
            cursor.x < 80,
            "cursor column {} must stay inside the 80-wide input area",
            cursor.x
        );
        assert_eq!(cursor.y, 22);
    }
}
