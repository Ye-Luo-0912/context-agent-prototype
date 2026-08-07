use agent_contracts::{
    ContextItem, ContextRetention, ContextScope, ContextState, ContextStateTransition, Scope,
    ScopeId, ScopeKind, ScopeState, TaskId,
};

use crate::engine::State;

/// Tags that mark an item as a durable outcome worth keeping when its scope
/// closes: decisions, findings, constraints, open loops, artifact references
/// and evidence references are promoted to the parent scope. Everything else
/// in a closed scope is released.
const PROMOTION_TAGS: [&str; 6] = [
    "decision",
    "finding",
    "constraint",
    "open-loop",
    "artifact-ref",
    "evidence-ref",
];

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

/// Open a tool scope for one tool call, nested under the current attention
/// scope (focus or task). The scope ends when the model consumes the result.
pub(crate) fn open_tool_scope(state: &mut State) -> ScopeId {
    let scope = Scope {
        id: ScopeId::new(),
        parent: state.active_scope_id,
        kind: ScopeKind::Tool,
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

/// The model just consumed the previous round's tool results: queue every
/// open tool scope for close. The ephemeral results themselves leave the
/// heap through residency; this records the container boundary.
pub(crate) fn queue_tool_scope_closes(state: &mut State) {
    for scope in &state.scopes {
        if scope.kind == ScopeKind::Tool && scope.state != ScopeState::Closed {
            state.pending_closed_scopes.push(scope.id);
        }
    }
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
/// the normal lifecycle cools it. Tool scopes are observational containers
/// whose ephemeral results leave via residency, so they touch no items.
fn close_members(
    state: &mut State,
    scope: &Scope,
    parent_id: Option<ScopeId>,
    turn: u64,
) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    if matches!(scope.kind, ScopeKind::Tool | ScopeKind::Session) {
        return transitions;
    }
    let parent_scope = parent_id
        .and_then(|pid| state.scopes.iter().find(|scope| scope.id == pid))
        .map_or(ContextScope::Session, |parent| match parent.kind {
            ScopeKind::Session => ContextScope::Session,
            ScopeKind::Task | ScopeKind::Focus => ContextScope::Task,
            ScopeKind::Tool => ContextScope::Turn,
        });
    for item in &mut state.items {
        if !belongs_to(item, scope) {
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

/// An item belongs to a scope when its own scope marker and task match the
/// container, and for focus scopes it was created while the focus was open.
fn belongs_to(item: &ContextItem, scope: &Scope) -> bool {
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

/// Pinned or durable items, and items carrying a promotion tag, are the
/// durable outcomes of the scope.
fn should_promote(item: &ContextItem) -> bool {
    matches!(
        item.retention,
        ContextRetention::Pinned | ContextRetention::Durable
    ) || item
        .tags
        .iter()
        .any(|tag| PROMOTION_TAGS.contains(&tag.as_str()))
}

/// Promote one item to the parent scope: it becomes durable, moves to the
/// parent's scope marker and is reactivated if it had cooled down. Already
/// promoted items are left alone (the task close and its focus cascade both
/// see the same members).
fn promote(
    item: &mut ContextItem,
    parent_scope: ContextScope,
    kind: ScopeKind,
    turn: u64,
    transitions: &mut Vec<ContextStateTransition>,
) {
    if item.tags.iter().any(|tag| tag == "promoted") {
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
    item.tags.push("promoted".into());
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
