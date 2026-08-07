use agent_contracts::{
    ContextItem, ContextItemId, ContextKind, ContextResidency, ContextRetention, ContextScope,
    ContextState,
};

use crate::engine::{SimpleContextConfig, State};
use crate::index::entity::extract_entities;

/// Build a fresh item stamped with the current turn, tick and task. Content
/// is truncated to the configured bound before it enters the heap, and the
/// entity signature is precomputed once so dependency linking, supersession,
/// scoring and GC never re-parse the content.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_item(
    state: &State,
    config: &SimpleContextConfig,
    content: String,
    kind: ContextKind,
    scope: ContextScope,
    retention: ContextRetention,
    importance: f32,
    source: Option<String>,
) -> ContextItem {
    let content = truncate_chars(content, config.max_item_chars);
    let entities = extract_entities(&content);
    ContextItem {
        id: ContextItemId::new(),
        task_id: state.focus.as_ref().map(|f| f.task_id),
        scope_id: state.active_scope_id,
        content,
        kind,
        scope,
        retention,
        state: ContextState::Active,
        importance,
        relevance: 0.5,
        created_tick: state.tick,
        last_access_tick: state.tick,
        access_count: 0,
        created_turn: state.turn,
        last_access_turn: state.turn,
        dependencies: Vec::new(),
        tags: Vec::new(),
        source,
        residency: ContextResidency::Resident,
        gc_generation: 0,
        evicted_at_tick: None,
        entities,
    }
}

/// Token estimate shared across the crate (ascii chars / 4 + non-ascii chars).
/// Delegates to the workspace-wide convention so engines, the prompt
/// assembler and the replay harness measure the same quantity.
pub(crate) fn approx_tokens(text: &str) -> usize {
    agent_contracts::tokens::approx_tokens(text)
}

pub(crate) fn truncate_chars(mut text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    text = text.chars().take(max_chars).collect();
    text.push_str("\n...[truncated by context engine]");
    text
}

pub(crate) fn short_id(id: &ContextItemId) -> String {
    id.to_string().chars().take(8).collect()
}
