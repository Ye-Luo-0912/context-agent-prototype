//! Shared internals for the baseline context engines.
//!
//! Baselines A and B intentionally model the *old* way of doing agent memory:
//! everything is appended to a transcript and stays there (A), or the oldest
//! part is periodically collapsed into a placeholder summary when a threshold
//! is crossed (B). Neither performs lifecycle maintenance — that contrast is
//! the point of the A/B/C experiment.

use agent_contracts::{
    AttentionState, ContextDiagnostics, ContextIngress, ContextItemId, ContextItemSummary,
    ContextKind, ContextRetention, ContextScope, MaterializedItem, SemanticState,
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
            scope_id: None,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 0,
            created_turn: self.created_turn,
            last_access_turn: self.created_turn,
            access_count: 0,
            dependencies: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
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
        ContextIngress::ToolObservation { output, .. } => vec![Record {
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
        // Suspension produces no history record in the baselines.
        ContextIngress::FocusCleared => Vec::new(),
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
        // Directives modify existing items; they produce no records in the
        // append/rolling baselines (which only accumulate history).
        ContextIngress::ContextDirective { .. } => Vec::new(),
        // A working-set signal carries no body; baselines accumulate history
        // records, so it produces nothing (the tool observation that follows
        // at turn end is the record).
        ContextIngress::WorkingSetSignal { .. } => Vec::new(),
    }
}

/// Map the retained records to structured working-set items. Baselines never
/// render protocol messages themselves — the prompt assembler does that, so
/// A/B/C measure the selection policy, not the rendering.
pub(crate) fn materialized_items(
    records: &[Record],
    summary: Option<&Record>,
) -> Vec<MaterializedItem> {
    let mut items = Vec::with_capacity(records.len() + usize::from(summary.is_some()));
    if let Some(summary) = summary {
        items.push(MaterializedItem {
            item_id: summary.id,
            kind: summary.kind,
            scope: summary.scope,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            retention: ContextRetention::Durable,
            content: summary.content.clone(),
            source: summary.source.clone(),
        });
    }
    items.extend(records.iter().map(|record| MaterializedItem {
        item_id: record.id,
        kind: record.kind,
        scope: record.scope,
        attention: AttentionState::Active,
        semantic: SemanticState::Live,
        retention: ContextRetention::Working,
        content: record.content.clone(),
        source: record.source.clone(),
    }));
    items
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
        tombstoned_items: dropped,
        approx_active_tokens,
        ..ContextDiagnostics::default()
    }
}
