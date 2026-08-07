use agent_contracts::{
    ContextItem, ContextItemId, ContextKind, ContextState, ContextStateTransition,
};

use crate::engine::State;
use crate::index::entity::{entities_match, extract_entities};

/// A user message reads as a decision when it carries a directive verb
/// ("use X", "switch to Y", "revert", "drop Z", ...). Explicit, keyword
/// based, explainable — no learned scoring.
pub(crate) fn classify_decision(text: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "use ",
        "switch",
        "revert",
        "drop ",
        "adopt",
        "prefer",
        "instead of",
        "no, ",
        "actually ",
        "replace ",
        "remove ",
    ];
    let lower = text.to_lowercase();
    KEYWORDS.iter().any(|keyword| lower.contains(keyword))
}

/// True when the item is permanently excluded from model requests: a
/// superseded decision or a verified-fixed error, whatever its score.
pub(crate) fn is_excluded(item: &ContextItem) -> bool {
    item.tags
        .iter()
        .any(|tag| tag == "superseded" || tag == "verified-fixed")
}

/// Queue supersession for every non-dropped decision item that shares an
/// entity with the incoming decision (except `except_id`, the new item).
pub(crate) fn queue_decision_supersessions(
    state: &mut State,
    content: &str,
    reason_prefix: &str,
    except_id: Option<ContextItemId>,
) {
    let entities = extract_entities(content);
    if entities.is_empty() {
        return;
    }
    for item in &mut state.items {
        if Some(item.id) == except_id {
            continue;
        }
        let is_decision =
            item.kind == ContextKind::Decision || item.tags.iter().any(|tag| tag == "decision");
        if !is_decision || item.state == ContextState::Dropped {
            continue;
        }
        if item.tags.iter().any(|tag| tag == "superseded") {
            continue;
        }
        let prior_entities = extract_entities(&item.content);
        if entities_match(&entities, &prior_entities) {
            let snippet: String = item.content.chars().take(60).collect();
            state
                .pending_supersessions
                .push((item.id, format!("{reason_prefix}: '{snippet}'")));
        }
    }
}

/// Queue verification for every non-dropped, unverified error item that
/// shares an entity with a successful observation.
pub(crate) fn queue_error_verifications(state: &mut State, content: &str, reason: &str) {
    let entities = extract_entities(content);
    if entities.is_empty() {
        return;
    }
    for item in &mut state.items {
        if item.kind != ContextKind::Error || item.state == ContextState::Dropped {
            continue;
        }
        if is_excluded(item) {
            continue;
        }
        let prior_entities = extract_entities(&item.content);
        if entities_match(&entities, &prior_entities) {
            state
                .pending_verifications
                .push((item.id, reason.to_string()));
        }
    }
}

/// Queue recurrence-supersession for every non-dropped error item that
/// shares an entity with a new failure: one live error per failure site,
/// the latest one.
pub(crate) fn queue_error_recurrence(state: &mut State, content: &str, round: u64) {
    let entities = extract_entities(content);
    if entities.is_empty() {
        return;
    }
    for item in &mut state.items {
        if item.kind != ContextKind::Error || item.state == ContextState::Dropped {
            continue;
        }
        if is_excluded(item) {
            continue;
        }
        let prior_entities = extract_entities(&item.content);
        if entities_match(&entities, &prior_entities) {
            state.pending_supersessions.push((
                item.id,
                format!(
                    "recurring failure supersedes earlier error (round {round}, same entities)"
                ),
            ));
        }
    }
}

/// Apply queued supersession intents as observable state changes.
pub(crate) fn drain_supersessions(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let supersessions = std::mem::take(&mut state.pending_supersessions);
    for (item_id, reason) in supersessions {
        let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) else {
            continue;
        };
        if item.state == ContextState::Dropped {
            continue;
        }
        if item.state != ContextState::Archived {
            transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: item.state,
                to: ContextState::Archived,
                turn,
                reason: reason.clone(),
            });
        }
        item.state = ContextState::Archived;
        item.relevance = 0.0;
        item.tags.push("superseded".into());
    }
    transitions
}

/// Apply queued verification intents as observable state changes.
pub(crate) fn drain_verifications(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let verifications = std::mem::take(&mut state.pending_verifications);
    for (item_id, reason) in verifications {
        let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) else {
            continue;
        };
        if item.state == ContextState::Dropped {
            continue;
        }
        if item.state != ContextState::Archived {
            transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: item.state,
                to: ContextState::Archived,
                turn,
                reason: reason.clone(),
            });
        }
        item.state = ContextState::Archived;
        item.relevance = 0.0;
        item.tags.push("verified-fixed".into());
    }
    transitions
}
