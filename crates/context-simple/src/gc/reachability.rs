use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextKind, ContextStateTransition, CoreLabel,
    LifecycleLabel, SemanticState,
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
/// superseded decision or a verified-fixed error, whatever its score. The
/// semantic state is authoritative; the legacy lifecycle labels are only
/// honored for pre-split checkpoints until restore migrates them.
pub(crate) fn is_excluded(item: &ContextItem) -> bool {
    item.semantic.is_dead()
        || item.tags.iter().any(|tag| {
            tag.is_lifecycle(LifecycleLabel::Superseded)
                || tag.is_lifecycle(LifecycleLabel::VerifiedFixed)
        })
}

/// Queue supersession for every live decision item that shares an entity
/// with the incoming decision. `by_id` is the new decision's id: it both
/// excludes the new item itself and becomes the `by` of the Superseded
/// semantic state.
pub(crate) fn queue_decision_supersessions(
    state: &mut State,
    content: &str,
    reason_prefix: &str,
    by_id: ContextItemId,
) {
    let entities = extract_entities(content);
    if entities.is_empty() {
        return;
    }
    for item in &mut state.items {
        if item.id == by_id {
            continue;
        }
        let is_decision = item.kind == ContextKind::Decision
            || item.tags.iter().any(|tag| tag.is_core(CoreLabel::Decision));
        if !is_decision || item.semantic.is_dead() {
            continue;
        }
        if entities_match(&entities, &item.entities) {
            let snippet: String = item.content.chars().take(60).collect();
            state
                .pending_supersessions
                .push((item.id, by_id, format!("{reason_prefix}: '{snippet}'")));
        }
    }
}

/// Queue verification for every live, unverified error item that shares an
/// entity with a successful observation. `by_id` is the successful
/// observation that verified the error.
pub(crate) fn queue_error_verifications(
    state: &mut State,
    content: &str,
    reason: &str,
    by_id: ContextItemId,
) {
    let entities = extract_entities(content);
    if entities.is_empty() {
        return;
    }
    for item in &mut state.items {
        if item.kind != ContextKind::Error || item.semantic.is_dead() {
            continue;
        }
        if is_excluded(item) {
            continue;
        }
        if entities_match(&entities, &item.entities) {
            state
                .pending_verifications
                .push((item.id, by_id, reason.to_string()));
        }
    }
}

/// Queue recurrence-supersession for every live error item that shares an
/// entity with a new failure: one live error per failure site, the latest
/// one. `by_id` is the new failure that supersedes the earlier one.
pub(crate) fn queue_error_recurrence(state: &mut State, content: &str, round: u64, by_id: ContextItemId) {
    let entities = extract_entities(content);
    if entities.is_empty() {
        return;
    }
    for item in &mut state.items {
        if item.kind != ContextKind::Error || item.semantic.is_dead() {
            continue;
        }
        if item.id == by_id {
            continue;
        }
        if is_excluded(item) {
            continue;
        }
        if entities_match(&entities, &item.entities) {
            state.pending_supersessions.push((
                item.id,
                by_id,
                format!(
                    "recurring failure supersedes earlier error (round {round}, same entities)"
                ),
            ));
        }
    }
}

/// Apply queued supersession intents as observable state changes: the older
/// decision is archived and its semantic state becomes
/// `Superseded { by }` — terminal, never resurrected by Context GC.
pub(crate) fn drain_supersessions(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let supersessions = std::mem::take(&mut state.pending_supersessions);
    for (item_id, by_id, reason) in supersessions {
        let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) else {
            continue;
        };
        if item.semantic.is_dead() {
            continue;
        }
        if item.attention != AttentionState::Archived {
            transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: item.attention,
                to: AttentionState::Archived,
                turn,
                reason: reason.clone(),
            });
        }
        item.attention = AttentionState::Archived;
        item.relevance = 0.0;
        item.semantic = SemanticState::Superseded { by: Some(by_id) };
    }
    transitions
}

/// Apply queued verification intents as observable state changes: the error
/// is archived and its semantic state becomes `VerifiedFixed { by }`.
pub(crate) fn drain_verifications(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let verifications = std::mem::take(&mut state.pending_verifications);
    for (item_id, by_id, reason) in verifications {
        let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) else {
            continue;
        };
        if item.semantic.is_dead() {
            continue;
        }
        if item.attention != AttentionState::Archived {
            transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: item.attention,
                to: AttentionState::Archived,
                turn,
                reason: reason.clone(),
            });
        }
        item.attention = AttentionState::Archived;
        item.relevance = 0.0;
        item.semantic = SemanticState::VerifiedFixed { by: Some(by_id) };
    }
    transitions
}
