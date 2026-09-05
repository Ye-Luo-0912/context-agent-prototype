use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextKind, ContextStateTransition, CoreLabel,
    LifecycleLabel, SemanticState,
};

use crate::engine::State;
use crate::index::entity::{
    entities_match, extract_entities, is_file_body_entry, is_file_body_observation,
    observation_file_path,
};

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
/// semantic state. The scan covers the heap, the warm buffer and the
/// external map, so an earlier decision is superseded wherever its body
/// currently sits.
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
    let is_decision = |item: &ContextItem| {
        item.kind == ContextKind::Decision
            || item.tags.iter().any(|tag| tag.is_core(CoreLabel::Decision))
    };
    let matches = |item: &ContextItem| -> bool {
        item.id != by_id && is_decision(item) && !item.semantic.is_dead() && !is_excluded(item)
    };
    for item in &mut state.items {
        if !matches(item) || !entities_match(&entities, &item.entities) {
            continue;
        }
        let snippet: String = item.content.chars().take(60).collect();
        state
            .pending_supersessions
            .push((item.id, by_id, format!("{reason_prefix}: '{snippet}'")));
    }
    for item in &mut state.eviction_buffer {
        if !matches(item) || !entities_match(&entities, &item.entities) {
            continue;
        }
        let snippet: String = item.content.chars().take(60).collect();
        state
            .pending_supersessions
            .push((item.id, by_id, format!("{reason_prefix}: '{snippet}'")));
    }
    for entry in &state.external {
        let decision = entry.kind == ContextKind::Decision
            || entry
                .tags
                .iter()
                .any(|tag| tag.is_core(CoreLabel::Decision));
        if entry.item_id == by_id || !decision || entry.semantic.is_dead() {
            continue;
        }
        if entities_match(&entities, &entry.entities) {
            state.pending_supersessions.push((
                entry.item_id,
                by_id,
                format!("{reason_prefix}: stored decision"),
            ));
        }
    }
}

/// Queue verification for every live, unverified error item that shares an
/// entity with a successful observation. `by_id` is the successful
/// observation that verified the error. Also covers the warm buffer and the
/// external map: an error that left Resident is still the same error and
/// still gets verified by a later success.
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
    let matches = |item: &ContextItem| -> bool {
        item.kind == ContextKind::Error && !item.semantic.is_dead() && !is_excluded(item)
    };
    for item in &mut state.items {
        if matches(item) && entities_match(&entities, &item.entities) {
            state
                .pending_verifications
                .push((item.id, by_id, reason.to_string()));
        }
    }
    for item in &mut state.eviction_buffer {
        if matches(item) && entities_match(&entities, &item.entities) {
            state
                .pending_verifications
                .push((item.id, by_id, reason.to_string()));
        }
    }
    for entry in &state.external {
        if entry.kind == ContextKind::Error
            && entry.item_id != by_id
            && entry.semantic.is_live()
            && entities_match(&entities, &entry.entities)
        {
            state
                .pending_verifications
                .push((entry.item_id, by_id, reason.to_string()));
        }
    }
}

/// 同一文件路径的更新 **文件正文**（`fs.read` / unsourced replay header）
/// 只有在能证明覆盖时才覆盖旧正文：内容修订不同（明确的过期边界），或
/// 同一修订下新正文完整包含旧正文（新窗口覆盖旧窗口/全文重读）。同版本
/// 的非重叠片段是互补证据，不得仅因路径相同被 supersede；缺修订或无法
/// 证明覆盖时保守保留。带 `metadata.path` 的 `shell.exec` 只把路径写入
/// 身份索引，不是文件正文，不得互相 supersede。按结构化路径精确匹配，
/// 回退到正文首行；不用实体子串（`Session::start` 会把三个文件缠在一起）。
pub(crate) fn queue_file_body_supersessions(state: &mut State, new_item: &ContextItem) {
    if !is_file_body_observation(new_item) {
        return;
    }
    let Some(path) = observation_file_path(new_item).map(str::to_owned) else {
        return;
    };
    let by_id = new_item.id;
    let is_same_file = |item: &ContextItem| -> bool {
        item.id != by_id
            && item.kind == ContextKind::ToolObservation
            && !item.semantic.is_dead()
            && !is_excluded(item)
            && is_file_body_observation(item)
            && observation_file_path(item) == Some(path.as_str())
    };
    let supersedes = |item: &ContextItem| -> bool { supersedes_file_body(new_item, item) };
    let reason_for = |item: &ContextItem| -> String {
        if supersedes_stale_revision(new_item, item) {
            format!("superseded by a newer revision of {path}")
        } else {
            format!("superseded by a covering re-read of {path}")
        }
    };
    for item in &mut state.items {
        if is_same_file(item) && supersedes(item) {
            let reason = reason_for(item);
            state.pending_supersessions.push((item.id, by_id, reason));
        }
    }
    for item in &mut state.eviction_buffer {
        if is_same_file(item) && supersedes(item) {
            let reason = reason_for(item);
            state.pending_supersessions.push((item.id, by_id, reason));
        }
    }
    for entry in &state.external {
        if entry.item_id == by_id
            || entry.kind != ContextKind::ToolObservation
            || entry.semantic.is_dead()
            || !is_file_body_entry(entry)
        {
            continue;
        }
        if entry.file_path.as_deref() == Some(path.as_str())
            || entry.entities.iter().any(|entity| entity == &path)
        {
            // Stored entries carry no content revision, so staleness cannot
            // be proven and the blob body is not loaded for a containment
            // check: conservatively keep the stored fragment. It stays
            // retrievable; a same-revision stored body is still valid, and
            // an admitted one is compared like any resident body.
        }
    }
}

/// A newer read supersedes an older body of the same file only when it
/// proves coverage: a different content revision (explicit stale boundary)
/// or a same-revision body that fully contains the older fragment's body.
fn supersedes_file_body(new_item: &ContextItem, old: &ContextItem) -> bool {
    supersedes_stale_revision(new_item, old) || supersedes_same_revision_body(new_item, old)
}

fn supersedes_stale_revision(new_item: &ContextItem, old: &ContextItem) -> bool {
    match (&old.file_revision, &new_item.file_revision) {
        (Some(old_rev), Some(new_rev)) => old_rev != new_rev,
        _ => false,
    }
}

/// Same content revision: the newer body supersedes the older one only when
/// it literally contains it (a window covering the older window, or an
/// identical re-read). Disjoint windows of one version coexist; unknown
/// bodies are never proven and are kept.
fn supersedes_same_revision_body(new_item: &ContextItem, old: &ContextItem) -> bool {
    match (&old.file_revision, &new_item.file_revision) {
        (Some(old_rev), Some(new_rev)) if old_rev == new_rev => {
            !old.content.is_empty() && new_item.content.contains(&old.content)
        }
        _ => false,
    }
}

/// Queue recurrence-supersession for every live error item that shares an
/// entity with a new failure: one live error per failure site, the latest
/// one. `by_id` is the new failure that supersedes the earlier one.
///
/// The scan covers every body location with a retained entity signature:
/// the resident heap, the warm reversible buffer, and the external map.
/// A recurring failure supersedes an earlier error wherever that error's
/// body currently sits.
pub(crate) fn queue_error_recurrence(
    state: &mut State,
    content: &str,
    round: u64,
    by_id: ContextItemId,
) {
    let entities = extract_entities(content);
    if entities.is_empty() {
        return;
    }
    let reason = |round: u64| {
        format!("recurring failure supersedes earlier error (round {round}, same entities)")
    };
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
            state
                .pending_supersessions
                .push((item.id, by_id, reason(round)));
        }
    }
    for item in &mut state.eviction_buffer {
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
            state
                .pending_supersessions
                .push((item.id, by_id, reason(round)));
        }
    }
    for entry in &state.external {
        if entry.kind != ContextKind::Error || entry.semantic.is_dead() {
            continue;
        }
        if entry.item_id == by_id {
            continue;
        }
        if entities_match(&entities, &entry.entities) {
            state
                .pending_supersessions
                .push((entry.item_id, by_id, reason(round)));
        }
    }
}

/// Apply queued supersession intents as observable state changes: the older
/// decision is archived and its semantic state becomes
/// `Superseded { by }` — terminal, never resurrected by Context GC.
///
/// The target may live in any body location: the resident heap, the warm
/// reversible buffer, or the external map. Lifecycle authority must not
/// depend on where the body currently sits: a decision that was
/// evicted and externalized is still the same decision and still gets
/// superseded.
pub(crate) fn drain_supersessions(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let supersessions = std::mem::take(&mut state.pending_supersessions);
    for (item_id, by_id, reason) in supersessions {
        if let Some(transition) = apply_terminal_semantic(
            state,
            item_id,
            SemanticState::Superseded { by: Some(by_id) },
            &reason,
            turn,
        ) {
            transitions.push(transition);
        }
    }
    transitions
}

/// Apply queued verification intents as observable state changes: the error
/// is archived and its semantic state becomes `VerifiedFixed { by }` — also
/// independent of body location, so an error that left Resident still gets
/// verified when a later successful result fixes it.
pub(crate) fn drain_verifications(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let verifications = std::mem::take(&mut state.pending_verifications);
    for (item_id, by_id, reason) in verifications {
        if let Some(transition) = apply_terminal_semantic(
            state,
            item_id,
            SemanticState::VerifiedFixed { by: Some(by_id) },
            &reason,
            turn,
        ) {
            transitions.push(transition);
        }
    }
    transitions
}

/// Apply one terminal semantic transition to an item in whatever body
/// location it currently occupies: resident heap, warm buffer, or external
/// map. Semantic transitions are monotonic — a dead target stays dead — and
/// the change is recorded as an observable transition. `None` when the item
/// is unknown or already terminal.
fn apply_terminal_semantic(
    state: &mut State,
    item_id: ContextItemId,
    terminal: SemanticState,
    reason: &str,
    turn: u64,
) -> Option<ContextStateTransition> {
    // Resident heap.
    if let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) {
        if item.semantic.is_dead() {
            return None;
        }
        let transition = ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from: item.attention,
            to: AttentionState::Archived,
            turn,
            reason: reason.to_string(),
        };
        item.attention = AttentionState::Archived;
        item.relevance = 0.0;
        item.semantic = terminal;
        state.mark_catalog(item_id);
        return Some(transition);
    }
    // Warm reversible buffer.
    if let Some(item) = state
        .eviction_buffer
        .iter_mut()
        .find(|item| item.id == item_id)
    {
        if item.semantic.is_dead() {
            return None;
        }
        let transition = ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from: item.attention,
            to: AttentionState::Archived,
            turn,
            reason: reason.to_string(),
        };
        item.attention = AttentionState::Archived;
        item.relevance = 0.0;
        item.semantic = terminal;
        state.mark_catalog(item_id);
        return Some(transition);
    }
    // Stored (Cold / External): metadata only — the entry keeps its terminal
    // state in the map and is no longer retrievable.
    if let Some(entry) = state.external.get_mut(item_id) {
        if entry.semantic.is_dead() {
            return None;
        }
        let transition = ContextStateTransition {
            item_id: entry.item_id,
            kind: entry.kind,
            scope: entry.scope,
            from: entry.attention,
            to: AttentionState::Archived,
            turn,
            reason: reason.to_string(),
        };
        entry.attention = AttentionState::Archived;
        entry.semantic = terminal;
        state.mark_catalog(item_id);
        return Some(transition);
    }
    None
}
