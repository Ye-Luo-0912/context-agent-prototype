//! Context directives that move items between body locations.
//!
//! `context.admit` re-enters an item into the working set under its
//! *original* id — one lifecycle transition, identity preserved. When the
//! target's content lives in the context store, the directive follows the
//! same plan -> io -> commit phases as the GC: plan under the lock, read
//! outside it, re-apply under a fresh lock, so the state lock is never
//! held across disk IO.
//!
//! `context.derive` persists a fact as a *new* item with a `DerivedFrom`
//! edge to the source ref — the derivation is traceable but never confuses
//! the source ref's identity with a copy.

use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextResidency, ContextRetention, ContextScope,
    ContextStateTransition, DependencyEdge, DependencyKind, ScopeId, ScopeKind, ScopeState,
};

use crate::engine::{SimpleContextConfig, State};

/// Under the lock: what `context.admit` needs before it can apply.
pub(crate) enum AdmitPlan {
    /// No store read: the target is resident or in the warm buffer.
    InMemory,
    /// The content lives in the store: read the file, then re-apply.
    ReadExternal(ContextItemId),
    /// The target exists but its semantic lifecycle ended — terminal states
    /// never resurrect, so the admit is refused with an explainable reason.
    Refused(String),
    /// Stale id: the target is nowhere. Silent no-op, like every other
    /// directive on a target that left.
    Missing,
}

pub(crate) fn plan_admit(state: &State, item_id: ContextItemId) -> AdmitPlan {
    if state.items.iter().any(|item| item.id == item_id) {
        return AdmitPlan::InMemory;
    }
    if state.eviction_buffer.iter().any(|item| item.id == item_id) {
        return AdmitPlan::InMemory;
    }
    match state.external.get(item_id) {
        Some(entry) if crate::store::externally_retrievable(entry) => {
            AdmitPlan::ReadExternal(item_id)
        }
        Some(_) => AdmitPlan::Refused(
            "admit refused: the item's semantic lifecycle ended (terminal states never resurrect)"
                .to_string(),
        ),
        None => AdmitPlan::Missing,
    }
}

/// Apply `context.admit`: bring the item back into the working set with the
/// same id, producing exactly one lifecycle transition. `external_read` is
/// the store read planned under the lock and executed outside it; it is
/// re-validated here because a concurrent lifecycle transition may have
/// made the entry terminal while the file was read.
///
/// Returns `Some(reason)` when a quota refused the admit; a stale target is
/// a silent no-op (the model saw a ref that just left — the next
/// materialization drops it).
pub(crate) fn apply_admit(
    state: &mut State,
    config: &SimpleContextConfig,
    item_id: ContextItemId,
    reason: &str,
    external_read: Option<(ContextItemId, Option<ContextItem>)>,
) -> Option<String> {
    if state.admits_this_turn >= config.max_admits_per_turn {
        return Some(format!(
            "admit refused: {} admits this turn (cap {})",
            state.admits_this_turn, config.max_admits_per_turn
        ));
    }
    let now_tick = state.tick;
    let turn = state.turn;

    // Already resident: identity is preserved trivially, no transition.
    if state.items.iter().any(|item| item.id == item_id) {
        return None;
    }

    // Warm buffer: move into the heap now (no IO).
    if let Some(index) = state
        .eviction_buffer
        .iter()
        .position(|item| item.id == item_id)
    {
        let mut item = state.eviction_buffer.remove(index);
        if !item.semantic.is_live() {
            return Some(
                "admit refused: the item's semantic lifecycle ended (terminal states never resurrect)"
                    .to_string(),
            );
        }
        let from = item.attention;
        reenter_working_set(&mut item, now_tick, state);
        let transition = admit_transition(&item, from, turn, reason);
        state.items.push(item);
        state.pending_ingest_transitions.push(transition);
        state.admits_this_turn += 1;
        return None;
    }

    // External: the content was read outside the lock. Re-check membership
    // and retrievability after IO, then re-enter the heap under the same id.
    if let Some((read_id, Some(mut item))) = external_read
        && read_id == item_id
    {
        let retrievable = state
            .external
            .get(item_id)
            .is_some_and(crate::store::externally_retrievable);
        if !retrievable {
            // The entry became terminal or left while the file was read —
            // nothing to admit, and no mutation to roll back.
            return None;
        }
        let from = item.attention;
        reenter_working_set(&mut item, now_tick, state);
        let transition = admit_transition(&item, from, turn, reason);
        state.external.retain(|entry| entry.item_id != item_id);
        state.items.push(item);
        state.pending_ingest_transitions.push(transition);
        state.admits_this_turn += 1;
        return None;
    }

    // Stale id or the store read failed — silent no-op, like every other
    // directive on a target that left.
    None
}

/// Apply `context.derive`: persist a fact as a *new* item (new id) with an
/// explicit `DerivedFrom` edge to the source ref. The source must still
/// exist (heap, warm buffer or retrievable external entry); a stale ref is
/// a silent no-op. The derived item is bounded: per-turn count (quota) and
/// per-item content length (`max_item_chars`).
pub(crate) fn apply_derive(
    state: &mut State,
    config: &SimpleContextConfig,
    item_id: ContextItemId,
    fact: String,
) -> Option<String> {
    if state.derives_this_turn >= config.max_derived_items_per_turn {
        return Some(format!(
            "derive refused: {} derives this turn (cap {})",
            state.derives_this_turn, config.max_derived_items_per_turn
        ));
    }
    let source_exists = state.items.iter().any(|item| item.id == item_id)
        || state.eviction_buffer.iter().any(|item| item.id == item_id)
        || state
            .external
            .get(item_id)
            .is_some_and(crate::store::externally_retrievable);
    if !source_exists {
        return None;
    }
    // make_item mints a NEW id and stamps task/scope/turn; the derived note
    // lands in the current working scope (see `working_scope_id`), not the
    // transient tool frame of the context.manage call itself.
    let mut item = crate::item::make_item(
        state,
        config,
        fact,
        agent_contracts::ContextKind::Note,
        ContextScope::Task,
        ContextRetention::Working,
        0.6,
        Some("derived".to_string()),
    );
    item.scope_id = working_scope_id(state);
    // The only edge is the explicit DerivedFrom link — no auto entity
    // linking, so the derivation is unambiguous and traceable.
    item.dependencies.push(DependencyEdge {
        target: item_id,
        kind: DependencyKind::DerivedFrom,
    });
    state.items.push(item);
    state.derives_this_turn += 1;
    None
}

/// Re-enter an admitted item into the working set: active attention,
/// resident residency, a fresh GC generation, access stamps, and a scope
/// stamp into the current working scope so the materializer can select it
/// without a hot-entity match. Identity (the item id) is preserved.
///
/// The lifecycle timestamps are refreshed: an explicitly admitted item is a
/// deliberate, fresh working-set member — its presence is new even though
/// its content is old — so the ephemeral TTL does not tombstone it the
/// moment it re-enters. Provenance of the *content* lives in the store; the
/// timestamps here account for the item's presence in the heap.
fn reenter_working_set(item: &mut ContextItem, now_tick: u64, state: &State) {
    item.attention = AttentionState::Active;
    item.relevance = item.relevance.max(0.5);
    item.residency = ContextResidency::Resident;
    item.gc_generation = 0;
    item.evicted_at_tick = None;
    item.created_tick = now_tick;
    item.created_turn = state.turn;
    item.last_access_tick = now_tick;
    item.last_access_turn = state.turn;
    item.access_count = item.access_count.saturating_add(1);
    item.scope_id = working_scope_id(state);
    if state.focus.is_some() {
        item.scope = ContextScope::Task;
    }
}

/// The scope the working set currently lives in: the open task (or focus)
/// scope of the active task, falling back to the session scope when there
/// is no focus. Never the transient tool frame of the directive call.
fn working_scope_id(state: &State) -> Option<ScopeId> {
    let active_task = state.focus.as_ref().map(|focus| focus.task_id);
    state
        .scopes
        .iter()
        .find(|scope| {
            scope.state != ScopeState::Closed
                && (scope.kind == ScopeKind::Task || scope.kind == ScopeKind::Focus)
                && scope.task_id == active_task
        })
        .map(|scope| scope.id)
        .or_else(|| {
            state
                .scopes
                .iter()
                .find(|scope| scope.kind == ScopeKind::Session)
                .map(|scope| scope.id)
        })
}

/// The one lifecycle transition an admit produces: the item's attention
/// moves to Active with an explainable reason.
fn admit_transition(
    item: &ContextItem,
    from: AttentionState,
    turn: u64,
    reason: &str,
) -> ContextStateTransition {
    ContextStateTransition {
        item_id: item.id,
        kind: item.kind,
        scope: item.scope,
        from,
        to: AttentionState::Active,
        turn,
        reason: format!("admitted by model directive: {reason}"),
    }
}
