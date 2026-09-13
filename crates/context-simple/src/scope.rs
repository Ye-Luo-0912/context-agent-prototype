use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextRetention, ContextScope,
    ContextStateTransition, Label, LifecycleLabel, Scope, ScopeId, ScopeKind, ScopeState, TaskId,
};

use std::collections::HashMap;

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
/// but its membership identity is promoted the same way). GC uses the same
/// test to decide which open-episode members ordinary dialogue may age out.
pub(crate) fn retention_or_tag_promotable(retention: ContextRetention, tags: &[Label]) -> bool {
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
        // CTX-9: (re)opening a task retires the retirement note — a live
        // task scope is the fresh authority, and a stale "completed" note
        // must not keep declaring it finished.
        state
            .retired_scopes
            .retain(|note| !(note.kind == ScopeKind::Task && note.task_id == Some(task_id)));
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
    // Episode-local turn budget: `FocusState.generation` counts user turns
    // *inside the current episode*, so a rotation must reset it. Without
    // the reset, one overlong episode (the `episode_max_user_turns` guard)
    // permanently exhausts every later episode's budget — the guard would
    // fire on the very next user message and rotate a fresh single-turn
    // episode.
    if let Some(focus) = state.focus.as_mut() {
        focus.generation = 0;
    }
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
    let mut children = std::collections::HashMap::<ScopeId, Vec<ScopeId>>::new();
    for scope in &state.scopes {
        if scope.state != ScopeState::Closed
            && let Some(parent) = scope.parent
        {
            children.entry(parent).or_default().push(scope.id);
        }
    }
    let mut visited = std::collections::HashSet::from([task_scope]);
    while let Some(parent) = frontier.pop() {
        for &child in children.get(&parent).into_iter().flatten() {
            if visited.insert(child) {
                state.pending_closed_scopes.push(child);
                frontier.push(child);
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
    let mut remaining = state.scopes.len();
    while let Some(id) = current {
        if remaining == 0 {
            return None;
        }
        remaining -= 1;
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

    // CTX-6: pending (store-write failed) members of the closing scope get
    // the same durable-outcome promotion as resident, warm and external
    // members — re-stamped *in place* (the retry list keeps owning the body;
    // the owed store write must not be dropped). Terminal semantics and
    // non-promotable bodies stay untouched: eviction is the GC's job, and a
    // dead body is never promoted.
    for item in &mut state.pending_externalize_retry {
        if !belongs_to(&state.scopes, item, scope) {
            continue;
        }
        if !item.semantic.is_live() || is_excluded(item) || !should_promote(item) {
            continue;
        }
        // No heap-index move applies: the body stays in the retry list.
        promote(
            item,
            parent_scope,
            parent_id,
            scope.kind,
            turn,
            &mut transitions,
        );
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
    let mut external_catalog_changed = false;
    for entry in &mut state.external {
        if !belongs_to_external(&state.scopes, entry, scope) {
            continue;
        }
        if !entry.semantic.is_live() || !retention_or_tag_promotable(entry.retention, &entry.tags) {
            // CTX-9: a member that will never promote (terminal, or not a
            // durable outcome) releases its scope stamp. The close already
            // decided its membership; the dead stamp would otherwise pin
            // the whole closed chain in memory and checkpoints forever.
            // Retrieval/promotion semantics are unchanged: terminal
            // entries are never served, non-promotable members are skipped
            // by every later close, and `task_id` keeps the legacy
            // inference available.
            if entry.scope_id.take().is_some() {
                external_catalog_changed = true;
            }
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
        // Mirror resident promotion: closing a scope may upgrade transient
        // retention, but must never downgrade a Pin. The external map keeps
        // a pinned-id index for bounded required-body planning.
        if matches!(
            entry.retention,
            ContextRetention::Ephemeral | ContextRetention::Working
        ) {
            entry.retention = ContextRetention::Durable;
        }
        entry.tags.push(Label::lifecycle(LifecycleLabel::Promoted));
        external_catalog_changed = true;
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
    // Scope, lifecycle labels and attention are catalog search/ranking
    // dimensions. A scope close may update many durable outcomes, so one
    // rebuild keeps the batch O(N); per-id upserts would search all stores
    // K times.
    if external_catalog_changed {
        state.mark_catalog_rebuild();
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
    let mut remaining = scopes.len();
    while let Some(sid) = current {
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
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

/// CTX-9 (R2-07)：一个已退休 scope 的有界事实保留。节点本身（goal、
/// ticks 等历史元数据）离开树、内存与 checkpoint；身份、种类、归属任务
/// 与关闭事实留在这条有界环里，`task_completed` 据此在退休后仍成立，
/// 完成任务不会被误判为未完成而自动召回。环满淘汰最旧——保留期显式
/// 且有界，不是清空。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RetiredScopeNote {
    pub(crate) id: ScopeId,
    pub(crate) kind: ScopeKind,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) closed_tick: Option<u64>,
}

/// Hard bound on the retirement ring. Notes are tiny (id + kind + task +
/// tick); 512 keeps every recent completion fact addressable while the
/// hot metadata stays bounded in memory and checkpoint size.
pub(crate) const MAX_RETIRED_SCOPE_NOTES: usize = 512;

/// CTX-9: retire closed scopes that nothing references anymore, keeping a
/// bounded fact note per retirement. A scope is retirable when all hold:
/// - closed and not the session scope (the session never retires);
/// - no item / external entry / the active scope carries its id (direct
///   referents);
/// - every remaining scope under it is itself retirable (chain integrity:
///   subtree walks and ancestor closes must never hit a missing node) —
///   decided bottom-up, so a whole unreferenced chain retires in one pass.
///
/// The pass runs only above `target_len`; it never opens, closes or
/// re-parents anything. Returns how many scopes were retired.
pub(crate) fn retire_closed_scopes(state: &mut State, target_len: usize) -> usize {
    if state.scopes.len() <= target_len {
        return 0;
    }
    let mut referenced: std::collections::HashSet<ScopeId> = state
        .items
        .iter()
        .filter_map(|item| item.scope_id)
        .collect();
    referenced.extend(
        state
            .pending_externalize_retry
            .iter()
            .filter_map(|item| item.scope_id),
    );
    referenced.extend(
        state
            .eviction_buffer
            .iter()
            .filter_map(|item| item.scope_id),
    );
    referenced.extend(state.external.iter().filter_map(|entry| entry.scope_id));
    if let Some(active) = state.active_scope_id {
        referenced.insert(active);
    }

    // Bottom-up verdicts (children before parents, Kahn order on reversed
    // parent edges): a node is retirable when it is a closed, unreferenced
    // non-session scope and every in-tree child of it is retirable.
    let mut child_count: HashMap<ScopeId, usize> = HashMap::new();
    let mut retirable_children: HashMap<ScopeId, usize> = HashMap::new();
    for scope in state.scopes.iter() {
        if let Some(parent) = scope.parent {
            *child_count.entry(parent).or_insert(0) += 1;
        }
    }
    let mut retirable: HashMap<ScopeId, bool> = HashMap::new();
    let mut queue: std::collections::VecDeque<ScopeId> = state
        .scopes
        .iter()
        .filter(|scope| child_count.get(&scope.id).copied().unwrap_or(0) == 0)
        .map(|scope| scope.id)
        .collect();
    while let Some(id) = queue.pop_front() {
        let self_ok = state.scopes.by_id(id).is_some_and(|scope| {
            scope.state == ScopeState::Closed
                && scope.kind != ScopeKind::Session
                && !referenced.contains(&scope.id)
        });
        let children_all = retirable_children.get(&id).copied().unwrap_or(0)
            == child_count.get(&id).copied().unwrap_or(0);
        let verdict = self_ok && children_all;
        retirable.insert(id, verdict);
        let parent = state.scopes.by_id(id).and_then(|scope| scope.parent);
        if let Some(parent) = parent {
            *retirable_children.entry(parent).or_insert(0) += usize::from(verdict);
            // Enqueue the parent once its last child is decided.
            if retirable_children[&parent] == child_count.get(&parent).copied().unwrap_or(0)
                && !retirable.contains_key(&parent)
            {
                queue.push_back(parent);
            }
        }
    }

    // Remove the retirable nodes and keep their facts. Slot order is
    // creation order, so notes read oldest-first.
    let notes: Vec<RetiredScopeNote> = state
        .scopes
        .iter()
        .filter(|scope| retirable.get(&scope.id).copied().unwrap_or(false))
        .map(|scope| RetiredScopeNote {
            id: scope.id,
            kind: scope.kind,
            task_id: scope.task_id,
            closed_tick: scope.closed_tick,
        })
        .collect();
    let retired = notes.len();
    if retired == 0 {
        return 0;
    }
    state
        .scopes
        .retain_and_rebuild(|scope| !retirable.get(&scope.id).copied().unwrap_or(false));
    // CTX-10（R3-04）：环按时间序维护——既有事实保持最旧在前，新事实追加
    // 在后，满员淘汰最旧前端。最新退休事实（刚完成的任务）绝不能第一个
    // 被淘汰。一旦发生淘汰，环就是「最近窗口」而非完整记录，溢出事实被
    // 显式置位（完成语义据此进入保守模式）。
    let mut merged = std::mem::take(&mut state.retired_scopes);
    merged.extend(notes);
    if merged.len() > MAX_RETIRED_SCOPE_NOTES {
        let drop = merged.len() - MAX_RETIRED_SCOPE_NOTES;
        merged.drain(0..drop);
        state.retirement_ring_overflowed = true;
    }
    state.retired_scopes = merged;
    retired
}

/// CTX-9: the single source of the completed-task fact — a closed Task
/// scope still in the tree, plus every retirement note. One GC pass builds
/// this set once (sweep/reactivate/commit share the snapshot);
/// `task_completion_recorded` answers a single id against the same facts.
pub(crate) fn completed_task_facts(state: &State) -> std::collections::HashSet<TaskId> {
    state
        .scopes
        .iter()
        .filter(|scope| scope.kind == ScopeKind::Task && scope.state == ScopeState::Closed)
        .filter_map(|scope| scope.task_id)
        .chain(
            state
                .retired_scopes
                .iter()
                .filter(|note| note.kind == ScopeKind::Task)
                .filter_map(|note| note.task_id),
        )
        .collect()
}

/// CTX-10 (R3-04): the completed-task fact set plus its completeness. Once
/// the bounded retirement ring has overflowed, a task with no live Task
/// scope in the tree and no note is *unknown*, not active: its scope was
/// necessarily retired (only closed scopes retire) and its note may have
/// been trimmed. Conservatively, unknown counts as completed — a finished
/// task's retained bodies never regain automatic recall just because the
/// bounded window forgot it. Automatic recall always had an explicit-reason
/// requirement; this only closes the "forgotten fact" hole.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompletionFacts {
    completed: std::collections::HashSet<TaskId>,
    in_tree: std::collections::HashSet<TaskId>,
    conservative: bool,
}

impl CompletionFacts {
    /// Whether this task's records are treated as completed (certain fact,
    /// or unknown under the conservative post-overflow rule).
    pub(crate) fn is_completed(&self, task_id: Option<TaskId>) -> bool {
        match task_id {
            None => false,
            Some(task) => {
                self.completed.contains(&task)
                    || (self.conservative && !self.in_tree.contains(&task))
            }
        }
    }
}

/// Build the per-pass completion snapshot (see [`CompletionFacts`]).
pub(crate) fn completion_facts(state: &State) -> CompletionFacts {
    CompletionFacts {
        completed: completed_task_facts(state),
        in_tree: state
            .scopes
            .iter()
            .filter(|scope| scope.kind == ScopeKind::Task)
            .filter_map(|scope| scope.task_id)
            .collect(),
        conservative: state.retirement_ring_overflowed,
    }
}

/// CTX-9 (test/audit helper): whether one task's completion fact is on
/// record — a closed Task scope still in the tree, or a retirement note
/// for one. Production readers build the [`completed_task_facts`] set once
/// per pass; this answers a single id against the same facts.
#[cfg(test)]
pub(crate) fn task_completion_recorded(state: &State, task_id: TaskId) -> bool {
    completed_task_facts(state).contains(&task_id)
}
