use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextRetention, ContextScope,
    ContextStateTransition, Label, LifecycleLabel, Scope, ScopeId, ScopeKind, ScopeState, TaskId,
};

use crate::engine::State;
use crate::gc::reachability::is_excluded;

/// Pinned or durable items, and items carrying a core content label, are the
/// durable outcomes of the scope. Everything else in a closed scope is
/// released.
fn should_promote(item: &ContextItem) -> bool {
    retention_or_tag_promotable(item.retention, &item.tags)
}

/// The durable-outcome test shared by resident items and external entries
/// (both carry `retention` and `tags`; the external body lives in the store
/// but its membership identity is promoted the same way).
fn retention_or_tag_promotable(retention: ContextRetention, tags: &[Label]) -> bool {
    matches!(
        retention,
        ContextRetention::Pinned | ContextRetention::Durable
    ) || tags.iter().any(|tag| tag.is_promotable())
}

/// Lazily open the single session scope of the run. Every run has exactly
/// one; it is never closed.
pub(crate) fn ensure_session(state: &mut State) -> ScopeId {
    if let Some(scope) = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Session)
    {
        return scope.id;
    }
    let session = Scope {
        id: ScopeId::new(),
        parent: None,
        kind: ScopeKind::Session,
        state: ScopeState::Active,
        task_id: None,
        goal: None,
        opened_tick: state.event_seq,
        last_active_tick: state.event_seq,
        closed_tick: None,
    };
    let id = session.id;
    state.scopes.push(session);
    state.active_scope_id = Some(id);
    id
}

/// Open (or reactivate) the task scope for `task_id` and make it the active
/// scope, suspending the task and focus scopes of the previously active task
/// when the focus switches to another task.
pub(crate) fn ensure_task_scope(state: &mut State, task_id: TaskId) -> ScopeId {
    let session = ensure_session(state);
    let task_scope_id = if let Some(existing) = state.scopes.iter_mut().find(|scope| {
        scope.kind == ScopeKind::Task
            && scope.task_id == Some(task_id)
            && scope.state != ScopeState::Closed
    }) {
        existing.state = ScopeState::Active;
        existing.last_active_tick = state.event_seq;
        existing.id
    } else {
        let scope = Scope {
            id: ScopeId::new(),
            parent: Some(session),
            kind: ScopeKind::Task,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: state.focus.as_ref().map(|f| f.goal.clone()),
            opened_tick: state.event_seq,
            last_active_tick: state.event_seq,
            closed_tick: None,
        };
        let id = scope.id;
        state.scopes.push(scope);
        id
    };
    for scope in state.scopes.iter_mut() {
        let is_other_task = scope.kind == ScopeKind::Task && scope.id != task_scope_id;
        let is_other_focus =
            scope.kind == ScopeKind::Focus && scope.parent.is_some_and(|p| p != task_scope_id);
        if (is_other_task || is_other_focus) && scope.state == ScopeState::Active {
            scope.state = ScopeState::Suspended;
        }
    }
    state.active_scope_id = Some(task_scope_id);
    task_scope_id
}

/// Open the focus scope of the current task, or touch the existing one when
/// it is still open. The focus scope is the attention container of a task:
/// it stays active across turns and suspends when another task takes over.
pub(crate) fn open_focus_scope(state: &mut State) -> ScopeId {
    let Some(task_id) = state.focus.as_ref().map(|f| f.task_id) else {
        return ensure_session(state);
    };
    let task_scope = ensure_task_scope(state, task_id);
    if let Some(existing) = state.scopes.iter_mut().find(|scope| {
        scope.kind == ScopeKind::Focus
            && scope.parent == Some(task_scope)
            && scope.state != ScopeState::Closed
    }) {
        existing.state = ScopeState::Active;
        existing.last_active_tick = state.event_seq;
        state.active_scope_id = Some(existing.id);
        return existing.id;
    }
    let scope = Scope {
        id: ScopeId::new(),
        parent: Some(task_scope),
        kind: ScopeKind::Focus,
        state: ScopeState::Active,
        task_id: Some(task_id),
        goal: state.focus.as_ref().map(|f| f.goal.clone()),
        opened_tick: state.event_seq,
        last_active_tick: state.event_seq,
        closed_tick: None,
    };
    let id = scope.id;
    state.scopes.push(scope);
    state.active_scope_id = Some(id);
    id
}

/// Open a fresh scope under `parent` (or the current active scope when
/// `parent` is `None`) and make it the active scope. The runtime drives
/// tool scopes this way: a scope opens when its tool starts, not when the
/// observation is later persisted.
pub(crate) fn open_scope(state: &mut State, kind: ScopeKind, parent: Option<ScopeId>) -> ScopeId {
    let scope = Scope {
        id: ScopeId::new(),
        parent: parent.or(state.active_scope_id),
        kind,
        state: ScopeState::Active,
        task_id: state.focus.as_ref().map(|f| f.task_id),
        goal: None,
        opened_tick: state.event_seq,
        last_active_tick: state.event_seq,
        closed_tick: None,
    };
    let id = scope.id;
    state.scopes.push(scope);
    state.active_scope_id = Some(id);
    id
}

/// Close a scope the runtime opened: mark it closed, promote its durable
/// members to the nearest open ancestor, reactivate the parent, and return
/// the transitions the close produced. Unknown or already-closed scopes are
/// no-ops.
pub(crate) fn close_scope(state: &mut State, scope_id: ScopeId) -> Vec<ContextStateTransition> {
    let Some(index) = state.scopes.index_of(scope_id) else {
        return Vec::new();
    };
    if state.scopes[index].state == ScopeState::Closed {
        return Vec::new();
    }
    let scope = {
        let scope = state.scopes.get_mut(index).expect("index_of slot exists");
        scope.state = ScopeState::Closed;
        scope.closed_tick = Some(state.event_seq);
        scope.clone()
    };
    let parent_id = nearest_open_parent(state, &scope);
    if state.active_scope_id == Some(scope.id) {
        state.active_scope_id = parent_id;
    }
    close_members(state, &scope, parent_id, state.turn)
}

/// Close the currently active focus scope of the focused task as an
/// *episode boundary*: durable outcomes promote to the task scope and
/// ordinary working-set dialogue is evicted, so the working set tracks the
/// current episode plus unresolved semantic state instead of the whole task
/// transcript. The task scope stays open — this is not task completion. The
/// engine calls this before a new user message opens a fresh focus scope
/// when the semantic-boundary or turn-budget signal fires.
pub(crate) fn close_focus_episode(state: &mut State) -> Vec<ContextStateTransition> {
    let Some(focus) = state.focus.as_ref() else {
        return Vec::new();
    };
    let focus_id = state
        .scopes
        .iter()
        .find(|scope| {
            scope.kind == ScopeKind::Focus
                && scope.task_id == Some(focus.task_id)
                && scope.state != ScopeState::Closed
        })
        .map(|scope| scope.id);
    let Some(focus_id) = focus_id else {
        return Vec::new();
    };
    close_scope(state, focus_id)
}

/// Queue the completed task's scope and every open descendant (focus
/// episodes and the tool frames inside them) for close. A task close must
/// not leave deep descendants open: a tool frame under the task's focus
/// would otherwise keep pointing at scopes that are already closed. The
/// close (promotion + eviction) is applied by maintenance so the resulting
/// transitions are observable.
pub(crate) fn queue_task_scope_close(state: &mut State, task_id: TaskId) {
    let Some(task_scope) = state
        .scopes
        .iter()
        .find(|scope| {
            scope.kind == ScopeKind::Task
                && scope.task_id == Some(task_id)
                && scope.state != ScopeState::Closed
        })
        .map(|scope| scope.id)
    else {
        return;
    };
    state.pending_closed_scopes.push(task_scope);
    // Depth-first walk collects every open descendant, not just the direct
    // focus child: a tool frame nested under the focus is a descendant of
    // the task and must close with it, or it keeps pointing at scopes that
    // are already closed.
    let mut frontier = vec![task_scope];
    while let Some(parent) = frontier.pop() {
        for scope in &state.scopes {
            if scope.parent == Some(parent) && scope.state != ScopeState::Closed {
                state.pending_closed_scopes.push(scope.id);
                frontier.push(scope.id);
            }
        }
    }
}

/// Apply queued scope closes. Each close promotes the durable outcomes of
/// the scope to the nearest open ancestor and releases the rest of the
/// working set, recording every item transition.
pub(crate) fn drain_closed_scopes(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let queued = std::mem::take(&mut state.pending_closed_scopes);
    for scope_id in queued {
        let Some(index) = state.scopes.index_of(scope_id) else {
            continue;
        };
        if state.scopes[index].state == ScopeState::Closed {
            continue;
        }
        let scope = {
            let scope = state.scopes.get_mut(index).expect("index_of slot exists");
            scope.state = ScopeState::Closed;
            scope.closed_tick = Some(state.event_seq);
            scope.clone()
        };
        let parent_id = nearest_open_parent(state, &scope);
        if state.active_scope_id == Some(scope.id) {
            state.active_scope_id = parent_id;
        }
        transitions.extend(close_members(state, &scope, parent_id, turn));
    }
    transitions
}

/// The nearest ancestor that is still open, used as the promotion target of
/// a closing scope (a focus child of a closing task promotes to the session).
fn nearest_open_parent(state: &State, scope: &Scope) -> Option<ScopeId> {
    let mut current = scope.parent;
    while let Some(id) = current {
        let closed = state
            .scopes
            .by_id(id)
            .is_none_or(|scope| scope.state == ScopeState::Closed);
        if !closed {
            return Some(id);
        }
        current = state.scopes.by_id(id).and_then(|scope| scope.parent);
    }
    None
}

/// Move the scope's surviving items: durable outcomes are promoted to the
/// parent scope, the rest of a completed task's or closed episode's
/// working set is evicted. Task closes release the whole working set.
/// Focus closes are episode boundaries: they promote durable outcomes and
/// evict ordinary dialogue so the working set tracks the current episode
/// instead of the whole task transcript. Tool scopes promote their durable
/// outcomes and leave the ephemeral/working results to residency and error
/// verification — a tool frame is a container boundary, not an eviction
/// pass. Session scopes are never closed.
fn close_members(
    state: &mut State,
    scope: &Scope,
    parent_id: Option<ScopeId>,
    turn: u64,
) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    if matches!(scope.kind, ScopeKind::Session) {
        return transitions;
    }
    let parent_scope =
        parent_id
            .and_then(|pid| state.scopes.by_id(pid))
            .map_or(ContextScope::Session, |parent| match parent.kind {
                ScopeKind::Session => ContextScope::Session,
                ScopeKind::Task | ScopeKind::Focus => ContextScope::Task,
                ScopeKind::Tool => ContextScope::Turn,
            });
    // Promotions re-stamp `scope_id`; the matching index moves are queued
    // here and applied after the heap loop (the loop holds `state.items`
    // mutably, so the index cannot be touched inside it).
    let mut scope_updates: Vec<(ContextItemId, Option<ScopeId>, Option<ScopeId>)> = Vec::new();
    for item in &mut state.items {
        if !belongs_to(&state.scopes, item, scope) {
            continue;
        }
        // Terminal semantic death always wins: a semantically dead item
        // (tombstoned, superseded, verified-fixed) stays dead through a
        // scope close. Everything else — including items the residency
        // machine already cooled to Archived — may still be a durable
        // outcome of the scope and must get its promotion chance.
        if !item.semantic.is_live() || is_excluded(item) {
            continue;
        }
        if should_promote(item) {
            if let Some(update) = promote(
                item,
                parent_scope,
                parent_id,
                scope.kind,
                turn,
                &mut transitions,
            ) {
                scope_updates.push(update);
            }
        } else if matches!(scope.kind, ScopeKind::Task | ScopeKind::Focus) {
            transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: item.attention,
                to: AttentionState::Archived,
                turn,
                reason: format!(
                    "{} closed: {}",
                    kind_name(scope.kind),
                    if scope.kind == ScopeKind::Task {
                        "task completed, working set evicted".to_string()
                    } else {
                        "episode rotated, ordinary dialogue evicted".to_string()
                    }
                ),
            });
            item.attention = AttentionState::Archived;
            item.relevance = 0.0;
        }
    }
    for (id, from, to) in scope_updates {
        // The heap re-stamps the item and moves the scope bucket in one
        // step, so the authoritative `scope_id` and the index never drift.
        if let Some(index) = state.items.indexes().get(id) {
            state.items.update_scope(index, from, to);
        }
    }

    // Warm buffer members of the closing scope get the same promotion: a
    // durable outcome does not lose its scope promotion just because it
    // was evicted before the scope closed. A promoted item re-enters the
    // heap — promotion means resident, not just re-stamped. Terminal
    // semantics and excluded items stay out, exactly like the heap pass.
    let mut index = state.eviction_buffer.len();
    while index > 0 {
        index -= 1;
        let promote_this = {
            let item = &state.eviction_buffer[index];
            belongs_to(&state.scopes, item, scope)
                && item.semantic.is_live()
                && !is_excluded(item)
                && should_promote(item)
        };
        if promote_this {
            let mut item = state.eviction_buffer.remove(index);
            promote(
                &mut item,
                parent_scope,
                parent_id,
                scope.kind,
                turn,
                &mut transitions,
            );
            // The heap push indexes the item at its slot in the same step.
            state.items.push(item);
        }
    }

    // External entries of the closing scope get the same membership
    // promotion as resident and warm bodies. Their content lives in the
    // store, so there is nothing to re-enter — the promotion re-stamps the
    // *identity*: scope/scope_id point at the nearest open ancestor,
    // retention upgrades to durable, the move is labeled, and attention
    // moves to Active exactly like a resident promotion (recall always
    // re-enters the working set anyway, so this never misleads the
    // materializer). Legacy entries without a scope stamp fall back to the
    // task id. Non-durable bodies stay where they are; terminal semantics
    // never resurrect, even as identity.
    for entry in &mut state.external {
        if !belongs_to_external(&state.scopes, entry, scope) {
            continue;
        }
        if !entry.semantic.is_live() || !retention_or_tag_promotable(entry.retention, &entry.tags) {
            continue;
        }
        // Same no-op guard as the resident promote: already a member of
        // the promotion target means the entry was promoted by an earlier
        // close (or was created there) — do not re-stamp or double-label.
        if entry.scope_id.is_some_and(|sid| Some(sid) == parent_id) {
            continue;
        }
        let from = entry.attention;
        entry.scope = parent_scope;
        entry.scope_id = parent_id;
        entry.retention = ContextRetention::Durable;
        entry.tags.push(Label::lifecycle(LifecycleLabel::Promoted));
        if entry.attention != AttentionState::Active {
            entry.attention = AttentionState::Active;
            transitions.push(ContextStateTransition {
                item_id: entry.item_id,
                kind: entry.kind,
                scope: entry.scope,
                from,
                to: AttentionState::Active,
                turn,
                reason: format!(
                    "external entry promoted by {} scope close",
                    kind_name(scope.kind)
                ),
            });
        }
    }
    transitions
}

/// An item belongs to a scope through its `scope_id` — the authoritative
/// membership stamped when the item was created. Task and focus closes also
/// see items of focus descendants (the work done under the task's focus),
/// but tool frames stay out: their observations leave through residency and
/// error verification, not scope close. Items without a `scope_id` (restored
/// old checkpoints) fall back to the pre-scope inference.
fn belongs_to(scopes: &crate::scope_tree::ScopeTree, item: &ContextItem, scope: &Scope) -> bool {
    let Some(item_scope_id) = item.scope_id else {
        return legacy_belongs_to(item, scope);
    };
    scope_id_in_subtree(scopes, item_scope_id, scope)
}

/// Whether `item_scope_id` is `scope.id` itself or a descendant of it in
/// the scope tree. Tool frames stop the walk: an item inside a tool frame
/// does not belong to the enclosing task/focus scope, exactly like the heap
/// rule. `ScopeTree::by_id` is an O(1) index lookup, so membership checks
/// stay O(depth) even when the tree accumulates many closed scopes — the
/// close pass visits every member of a large scope, and a linear scan would
/// turn that into a quadratic hot path.
fn scope_id_in_subtree(
    scopes: &crate::scope_tree::ScopeTree,
    item_scope_id: ScopeId,
    scope: &Scope,
) -> bool {
    if scope.kind == ScopeKind::Tool {
        return item_scope_id == scope.id;
    }
    let mut current = Some(item_scope_id);
    while let Some(sid) = current {
        if sid == scope.id {
            return true;
        }
        let Some(found) = scopes.by_id(sid) else {
            return false;
        };
        if found.kind == ScopeKind::Tool {
            return false;
        }
        current = found.parent;
    }
    false
}

/// External entries carry the same membership stamp as resident items. When
/// the stamp exists the scope subtree decides; legacy entries that predate
/// it fall back to the task id — a task or focus close promotes the whole
/// task line, so matching the task is the safe approximation (tool scopes
/// never match by task: a tool frame is not a task container).
fn belongs_to_external(
    scopes: &crate::scope_tree::ScopeTree,
    entry: &agent_contracts::ExternalizedContext,
    scope: &Scope,
) -> bool {
    let Some(item_scope_id) = entry.scope_id else {
        return scope.kind != ScopeKind::Tool
            && scope.task_id.is_some()
            && scope.task_id == entry.task_id;
    };
    scope_id_in_subtree(scopes, item_scope_id, scope)
}

/// The pre-`scope_id` membership rule, kept for items without a scope stamp.
fn legacy_belongs_to(item: &ContextItem, scope: &Scope) -> bool {
    match scope.kind {
        ScopeKind::Task => item.scope == ContextScope::Task && item.task_id == scope.task_id,
        ScopeKind::Focus => {
            item.scope == ContextScope::Task
                && item.task_id == scope.task_id
                && item.created_tick >= scope.opened_tick
        }
        ScopeKind::Tool | ScopeKind::Session => false,
    }
}

/// Pinned or durable items, and items carrying a core content label, are the
/// durable outcomes of the scope. Promotion moves the item to the nearest
/// open ancestor: both the descriptive `scope` and the authoritative
/// `scope_id` membership stamp are updated, so later closes of the parent
/// scope still see the item. The caller applies the returned index move
/// (`item_id`, old scope, new scope) after its heap loop.
///
/// An item is promoted again when a *higher* ancestor closes (episode
/// rotation promotes focus outcomes to the task scope; task close then
/// promotes them to the session) — the `Promoted` label records that the
/// item moved, it does not freeze it at its first target. The no-op guard
/// is "already a member of the promotion target", which is what prevents
/// the same scope from processing an item twice.
fn promote(
    item: &mut ContextItem,
    parent_scope: ContextScope,
    parent_id: Option<ScopeId>,
    kind: ScopeKind,
    turn: u64,
    transitions: &mut Vec<ContextStateTransition>,
) -> Option<(ContextItemId, Option<ScopeId>, Option<ScopeId>)> {
    if item.scope_id.is_some_and(|sid| Some(sid) == parent_id) {
        return None;
    }
    // Legacy items without a scope stamp cannot compare targets; the label
    // is their only repeat guard.
    if item.scope_id.is_none()
        && item
            .tags
            .iter()
            .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted))
    {
        return None;
    }
    let from = item.attention;
    if matches!(
        item.retention,
        ContextRetention::Ephemeral | ContextRetention::Working
    ) {
        item.retention = ContextRetention::Durable;
    }
    // Keep the authoritative membership stamp and the scope index in sync.
    let scope_update = (item.id, item.scope_id, parent_id);
    item.scope = parent_scope;
    item.scope_id = parent_id;
    item.tags.push(Label::lifecycle(LifecycleLabel::Promoted));
    if item.attention != AttentionState::Active {
        item.attention = AttentionState::Active;
        item.relevance = 0.5;
        transitions.push(ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from,
            to: AttentionState::Active,
            turn,
            reason: format!("promoted by {} scope close", kind_name(kind)),
        });
    }
    Some(scope_update)
}

fn kind_name(kind: ScopeKind) -> &'static str {
    match kind {
        ScopeKind::Session => "session",
        ScopeKind::Task => "task",
        ScopeKind::Focus => "focus",
        ScopeKind::Tool => "tool",
    }
}
