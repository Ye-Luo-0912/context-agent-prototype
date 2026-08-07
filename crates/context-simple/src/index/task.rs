use agent_contracts::{ContextItem, FocusState};

/// Whether an item belongs to a task other than the active focus (or to a
/// completed task after focus is cleared). Such items are capped at Archived
/// unless the current focus strongly reactivates them, so completed-task
/// detail does not leak back into active attention on recency alone.
pub(crate) fn is_stale_task(item: &ContextItem, focus: Option<&FocusState>) -> bool {
    match (item.task_id, focus) {
        (Some(item_task), Some(active)) => item_task != active.task_id,
        (Some(_), None) => true,
        _ => false,
    }
}
