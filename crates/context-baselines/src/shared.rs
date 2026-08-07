//! Shared internals for the baseline context engines.
//!
//! Baselines A and B intentionally model the *old* way of doing agent memory:
//! everything is appended to a transcript and stays there (A), or the oldest
//! part is periodically collapsed into a placeholder summary when a threshold
//! is crossed (B). Neither performs lifecycle maintenance — that contrast is
//! the point of the A/B/C experiment.

use agent_contracts::{
    ContextDiagnostics, ContextIngress, ContextItemId, ContextItemSummary, ContextKind,
    ContextScope, ContextState, ModelMessage, ModelRole,
};

/// Token estimator shared by the baseline engines. Must match the convention
/// in `context-simple` (ascii chars / 4 + non-ascii chars) so A, B and C
/// measure the same quantity.
pub(crate) fn approx_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else if !ch.is_whitespace() {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

/// One retained piece of history in a baseline engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Record {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub content: String,
    pub created_turn: u64,
    pub source: Option<String>,
}

impl Record {
    pub fn summary(&self) -> ContextItemSummary {
        ContextItemSummary {
            id: self.id,
            kind: self.kind,
            scope: self.scope,
            state: ContextState::Active,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 0,
            created_turn: self.created_turn,
            last_access_turn: self.created_turn,
            access_count: 0,
            dependencies: Vec::new(),
            source: self.source.clone(),
        }
    }
}

/// Map one ingest event to the history records it should append.
pub(crate) fn records_for_ingress(ingress: &ContextIngress, turn: u64) -> Vec<Record> {
    match ingress {
        ContextIngress::UserMessage { content } => vec![Record {
            id: ContextItemId::new(),
            kind: ContextKind::UserMessage,
            scope: ContextScope::Task,
            content: content.clone(),
            created_turn: turn,
            source: Some("user message".into()),
        }],
        ContextIngress::AssistantMessage { content } => vec![Record {
            id: ContextItemId::new(),
            kind: ContextKind::AssistantMessage,
            scope: ContextScope::Task,
            content: content.clone(),
            created_turn: turn,
            source: Some("assistant message".into()),
        }],
        ContextIngress::ToolObservation { output } => vec![Record {
            id: ContextItemId::new(),
            kind: if output.ok {
                ContextKind::ToolObservation
            } else {
                ContextKind::Error
            },
            scope: ContextScope::Turn,
            content: format!("[{}] {}", output.tool_name, output.model_content),
            created_turn: turn,
            source: Some(format!("tool {}", output.tool_name)),
        }],
        ContextIngress::FocusChanged { focus } => vec![Record {
            id: ContextItemId::new(),
            kind: ContextKind::Goal,
            scope: ContextScope::Task,
            content: format!("FOCUS: {}", focus.goal),
            created_turn: turn,
            source: Some("focus".into()),
        }],
        ContextIngress::Pin { content, kind } => vec![Record {
            id: ContextItemId::new(),
            kind: *kind,
            scope: ContextScope::Pinned,
            content: content.clone(),
            created_turn: turn,
            source: Some("pinned".into()),
        }],
        ContextIngress::TaskCompleted { summary, .. } => vec![Record {
            id: ContextItemId::new(),
            kind: ContextKind::Summary,
            scope: ContextScope::Task,
            content: format!("TASK COMPLETED: {summary}"),
            created_turn: turn,
            source: Some("task summary".into()),
        }],
    }
}

/// Render a retained record as a model-facing message.
pub(crate) fn record_message(record: &Record) -> ModelMessage {
    match record.kind {
        ContextKind::UserMessage => ModelMessage::user(&record.content),
        ContextKind::AssistantMessage => ModelMessage::assistant(&record.content),
        _ => ModelMessage {
            role: ModelRole::Tool,
            content: format!("{}: {}", kind_label(record.kind), record.content),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        },
    }
}

/// Diagnostics for engines where everything retained counts as active.
pub(crate) fn active_diagnostics(
    records: &[Record],
    summary: Option<&Record>,
    dropped: usize,
) -> ContextDiagnostics {
    let mut total = records.len();
    if summary.is_some() {
        total += 1;
    }
    let approx_active_tokens: usize = records
        .iter()
        .map(|record| approx_tokens(&record.content))
        .sum::<usize>()
        + summary.map_or(0, |record| approx_tokens(&record.content));
    ContextDiagnostics {
        total_items: total,
        active_items: total,
        cooling_items: 0,
        archived_items: 0,
        dropped_items: dropped,
        approx_active_tokens,
        ..ContextDiagnostics::default()
    }
}

fn kind_label(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::Goal => "GOAL",
        ContextKind::Constraint => "CONSTRAINT",
        ContextKind::Decision => "DECISION",
        ContextKind::UserMessage => "USER",
        ContextKind::AssistantMessage => "ASSISTANT",
        ContextKind::ToolObservation => "TOOL",
        ContextKind::FileObservation => "FILE",
        ContextKind::Error => "ERROR",
        ContextKind::Summary => "SUMMARY",
        ContextKind::Note => "NOTE",
    }
}

/// Build a full snapshot body shared by both baseline engines: system prompt,
/// optional summary marker, retained records, then the current user input.
pub(crate) fn build_messages(
    system_prompt: &str,
    summary: Option<&Record>,
    records: &[Record],
    current_input: &str,
) -> Vec<ModelMessage> {
    let mut messages = Vec::with_capacity(records.len() + 3);
    messages.push(ModelMessage::system(system_prompt));
    if let Some(summary) = summary {
        messages.push(ModelMessage::system(summary.content.clone()));
    }
    for record in records {
        messages.push(record_message(record));
    }
    messages.push(ModelMessage::user(current_input));
    messages
}
