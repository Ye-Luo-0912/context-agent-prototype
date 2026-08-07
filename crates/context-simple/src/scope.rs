use agent_contracts::{
    ContextItem, ContextRetention, ContextScope, ContextState, ContextStateTransition, Label,
    LifecycleLabel, Scope, ScopeId, ScopeKind, ScopeState, TaskId,
};

use crate::engine::State;

/// Pinned or durable items, and items carrying a core content label, are the
/// durable outcomes of the scope. Everything else in a closed scope is
/// released.
fn should_promote(item: &ContextItem) -> bool {
    matches!(
        item.retention,
        ContextRetention::Pinned | ContextRetention::Durable
    ) || item.tags.iter().any(|tag| tag.is_promotable())
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
        opened_tick: state.tick,
        last_active_tick: state.tick,
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
        existing.last_active_tick = state.tick;
        existing.id
    } else {
        let scope = Scope {
            id: ScopeId::new(),
            parent: Some(session),
            kind: ScopeKind::Task,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: state.focus.as_ref().map(|f| f.goal.clone()),
            opened_tick: state.tick,
            last_active_tick: state.tick,
            closed_tick: None,
        };
        let id = scope.id;
        state.scopes.push(scope);
        id
    };
    for scope in &mut state.scopes {
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
        existing.last_active_tick = state.tick;
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
        opened_tick: state.tick,
        last_active_tick: state.tick,
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
        opened_tick: state.tick,
        last_active_tick: state.tick,
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
    let Some(index) = state.scopes.iter().position(|scope| scope.id == scope_id) else {
        return Vec::new();
    };
    if state.scopes[index].state == ScopeState::Closed {
        return Vec::new();
    }
    state.scopes[index].state = ScopeState::Closed;
    state.scopes[index].closed_tick = Some(state.tick);
    let scope = state.scopes[index].clone();
    let parent_id = nearest_open_parent(state, &scope);
    if state.active_scope_id == Some(scope.id) {
        state.active_scope_id = parent_id;
    }
    close_members(state, &scope, parent_id, state.turn)
}

/// Queue the completed task's scope, plus its focus child, for close. The
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
    for scope in &state.scopes {
        if scope.parent == Some(task_scope) && scope.state != ScopeState::Closed {
            state.pending_closed_scopes.push(scope.id);
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
        let Some(index) = state.scopes.iter().position(|scope| scope.id == scope_id) else {
            continue;
        };
        if state.scopes[index].state == ScopeState::Closed {
            continue;
        }
        state.scopes[index].state = ScopeState::Closed;
        state.scopes[index].closed_tick = Some(state.tick);
        let scope = state.scopes[index].clone();
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
            .iter()
            .find(|scope| scope.id == id)
            .is_none_or(|scope| scope.state == ScopeState::Closed);
        if !closed {
            return Some(id);
        }
        current = state
            .scopes
            .iter()
            .find(|scope| scope.id == id)
            .and_then(|scope| scope.parent);
    }
    None
}

/// Move the scope's surviving items: durable outcomes are promoted to the
/// parent scope, the rest of a completed task's working set is evicted.
/// Focus closes only promote — the working set returns to the task scope and
/// the normal lifecycle cools it. Tool scopes promote their durable outcomes
/// and leave the ephemeral/working results to residency and error
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
    let parent_scope = parent_id
        .and_then(|pid| state.scopes.iter().find(|scope| scope.id == pid))
        .map_or(ContextScope::Session, |parent| match parent.kind {
            ScopeKind::Session => ContextScope::Session,
            ScopeKind::Task | ScopeKind::Focus => ContextScope::Task,
            ScopeKind::Tool => ContextScope::Turn,
        });
    // A precomputed view of the tree so membership can be checked while
    // items are mutated.
    let scope_index: Vec<(ScopeId, ScopeKind, Option<ScopeId>)> = state
        .scopes
        .iter()
        .map(|scope| (scope.id, scope.kind, scope.parent))
        .collect();
    for item in &mut state.items {
        if !belongs_to(&scope_index, item, scope) {
            continue;
        }
        if matches!(item.state, ContextState::Dropped | ContextState::Archived) {
            continue;
        }
        if should_promote(item) {
            promote(item, parent_scope, scope.kind, turn, &mut transitions);
        } else if scope.kind == ScopeKind::Task {
            transitions.push(ContextStateTransition {
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                from: item.state,
                to: ContextState::Archived,
                turn,
                reason: "task completed: scope closed, working set evicted".to_string(),
            });
            item.state = ContextState::Archived;
            item.relevance = 0.0;
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
fn belongs_to(
    scopes: &[(ScopeId, ScopeKind, Option<ScopeId>)],
    item: &ContextItem,
    scope: &Scope,
) -> bool {
    let Some(item_scope_id) = item.scope_id else {
        return legacy_belongs_to(item, scope);
    };
    if scope.kind == ScopeKind::Tool {
        return item_scope_id == scope.id;
    }
    let mut current = Some(item_scope_id);
    while let Some(sid) = current {
        if sid == scope.id {
            return true;
        }
        let Some((_, kind, parent)) = scopes.iter().find(|(id, ..)| *id == sid) else {
            return false;
        };
        if *kind == ScopeKind::Tool {
            return false;
        }
        current = *parent;
    }
    false
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
/// durable outcomes of the scope.
fn promote(
    item: &mut ContextItem,
    parent_scope: ContextScope,
    kind: ScopeKind,
    turn: u64,
    transitions: &mut Vec<ContextStateTransition>,
) {
    if item
        .tags
        .iter()
        .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted))
    {
        return;
    }
    let from = item.state;
    if matches!(
        item.retention,
        ContextRetention::Ephemeral | ContextRetention::Working
    ) {
        item.retention = ContextRetention::Durable;
    }
    item.scope = parent_scope;
    item.tags.push(Label::lifecycle(LifecycleLabel::Promoted));
    if item.state != ContextState::Active {
        item.state = ContextState::Active;
        item.relevance = 0.5;
        transitions.push(ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from,
            to: ContextState::Active,
            turn,
            reason: format!("promoted by {} scope close", kind_name(kind)),
        });
    }
}

fn kind_name(kind: ScopeKind) -> &'static str {
    match kind {
        ScopeKind::Session => "session",
        ScopeKind::Task => "task",
        ScopeKind::Focus => "focus",
        ScopeKind::Tool => "tool",
    }
}
