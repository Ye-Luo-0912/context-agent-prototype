use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::state::{AppState, UiRole};

pub fn render(frame: &mut Frame<'_>, app: &AppState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(3),
        ])
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

    let history = app
        .messages
        .iter()
        .flat_map(|message| {
            let prefix = match message.role {
                UiRole::User => "YOU",
                UiRole::Assistant => "AGENT",
                UiRole::Tool => "TOOL",
                UiRole::System => "SYSTEM",
            };
            vec![
                Line::from(Span::raw(format!("[{prefix}]"))),
                Line::from(message.content.clone()),
                Line::from(""),
            ]
        })
        .collect::<Vec<_>>();
    let history = Paragraph::new(Text::from(history))
        .block(Block::default().borders(Borders::ALL).title("Conversation"))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0));
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
        app.context.dropped_items,
        app.context.total_items,
        app.context.approx_active_tokens,
        app.context.focus_generation,
        app.context.turn,
        app.context.tool_round,
    ))
    .block(Block::default().borders(Borders::ALL).title("Context Working Set"));
    frame.render_widget(context, root[2]);

    let input = if let Some(pending) = &app.pending_approval {
        let prompt = Paragraph::new(format!(
            "Tool `{}` requests permission to run with:\n{}\n\n[y] allow    [n] deny    [Enter] allow    [Esc] deny",
            pending.tool_name, pending.args_preview
        ))
        .block(Block::default().borders(Borders::ALL).title("Approval Required"));
        frame.render_widget(prompt, root[3]);
        return;
    } else {
        Paragraph::new(app.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("Input"))
    };
    frame.render_widget(input, root[3]);
    let cursor_x = root[3].x
        + 1
        + app
            .input
            .chars()
            .count()
            .min(root[3].width.saturating_sub(2) as usize) as u16;
    let cursor_y = root[3].y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_context_panel(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &AppState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let selected_lines: Vec<Line> = app
        .context_selected
        .iter()
        .map(|selection| {
            Line::from(format!(
                "score={:.2} tok={} {}\n  {}", // split reason onto its own indented line
                selection.score,
                selection.approx_tokens,
                short_id(&selection.item_id),
                selection.reason
            ))
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
        .map(|transition| {
            Line::from(format!(
                "turn {}: {:?}->{:?} {:?}\n  {}",
                transition.turn, transition.from, transition.to, transition.kind, transition.reason
            ))
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
