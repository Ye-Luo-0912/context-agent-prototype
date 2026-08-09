//! The runtime scope tree owns its id index.
//!
//! `State.scopes` used to be a bare `Vec<Scope>` that close/restore paths
//! linear-scanned by id (`iter().position(|s| s.id == id)`), and the
//! ancestor walk in `nearest_open_parent` / `belongs_to` re-scanned per
//! hop. `ScopeTree` binds the storage and an id index together: scope ids
//! are immutable after creation, so the only structural mutation is
//! `push` (insert at slot + index in one step); every other mutation
//! touches non-indexed fields (state, ticks, goal) through `get_mut` /
//! `by_id_mut` / `iter_mut`, which cannot drift the index. `by_id` turns
//! the close/ancestor lookups into O(1), and the length guard survives
//! only as a safety net for direct test pushes and restored checkpoints.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use agent_contracts::{Scope, ScopeId};

#[derive(Debug, Default)]
pub(crate) struct ScopeTree {
    scopes: Vec<Scope>,
    /// scope id -> slot in `self.scopes`.
    id_index: HashMap<ScopeId, usize>,
    /// Expected `self.scopes.len()`; a mismatch means the tree changed
    /// without the index (direct test pushes, restored checkpoints).
    len_guard: usize,
}

impl ScopeTree {
    pub(crate) fn push(&mut self, scope: Scope) -> ScopeId {
        let id = scope.id;
        let slot = self.scopes.len();
        self.id_index.insert(id, slot);
        self.scopes.push(scope);
        self.len_guard = self.scopes.len();
        id
    }

    /// O(1) lookup by id (close, ancestor walks).
    pub(crate) fn by_id(&self, id: ScopeId) -> Option<&Scope> {
        self.id_index
            .get(&id)
            .and_then(|slot| self.scopes.get(*slot))
    }

    pub(crate) fn index_of(&self, id: ScopeId) -> Option<usize> {
        self.id_index.get(&id).copied()
    }

    /// Mutable iteration for *non-indexed* fields only (suspension on
    /// focus switch, active-scope restamping).
    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, Scope> {
        self.scopes.iter_mut()
    }

    /// Slot access for *non-indexed* fields only (state, ticks, goal).
    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut Scope> {
        self.scopes.get_mut(index)
    }

    /// Safety net for direct pushes that bypassed the tree (tests,
    /// restored checkpoints): rebuild when the length no longer matches
    /// the indexed length.
    pub(crate) fn ensure_consistent(&mut self) {
        if self.len_guard != self.scopes.len() {
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        self.id_index.clear();
        for (slot, scope) in self.scopes.iter().enumerate() {
            self.id_index.insert(scope.id, slot);
        }
        self.len_guard = self.scopes.len();
    }
}

/// Checkpoints serialize only the scopes; the id index is derived state
/// and rebuilt on restore.
impl Serialize for ScopeTree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.scopes.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ScopeTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let scopes = Vec::<Scope>::deserialize(deserializer)?;
        let mut tree = Self {
            scopes,
            id_index: HashMap::new(),
            len_guard: 0,
        };
        tree.rebuild();
        Ok(tree)
    }
}

impl std::ops::Deref for ScopeTree {
    type Target = Vec<Scope>;

    fn deref(&self) -> &Self::Target {
        &self.scopes
    }
}

impl<'a> IntoIterator for &'a ScopeTree {
    type Item = &'a Scope;
    type IntoIter = std::slice::Iter<'a, Scope>;

    fn into_iter(self) -> Self::IntoIter {
        self.scopes.iter()
    }
}

impl<'a> IntoIterator for &'a mut ScopeTree {
    type Item = &'a mut Scope;
    type IntoIter = std::slice::IterMut<'a, Scope>;

    fn into_iter(self) -> Self::IntoIter {
        self.scopes.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ScopeKind, ScopeState};

    fn scope() -> Scope {
        Scope {
            id: ScopeId::new(),
            parent: None,
            kind: ScopeKind::Session,
            state: ScopeState::Active,
            task_id: None,
            goal: None,
            opened_tick: 1,
            last_active_tick: 1,
            closed_tick: None,
        }
    }

    #[test]
    fn push_then_id_lookup_is_o1() {
        let mut tree = ScopeTree::default();
        let a = ScopeId::new();
        let b = ScopeId::new();
        let mut sa = scope();
        sa.id = a;
        let mut sb = scope();
        sb.id = b;
        tree.push(sa);
        tree.push(sb);

        assert_eq!(tree.by_id(a).unwrap().id, a);
        assert_eq!(tree.by_id(b).unwrap().id, b);
        assert_eq!(tree.index_of(a), Some(0));
        assert_eq!(tree.index_of(b), Some(1));
        assert!(tree.by_id(ScopeId::new()).is_none());
    }

    #[test]
    fn field_mutations_do_not_drift_the_index() {
        let mut tree = ScopeTree::default();
        let a = ScopeId::new();
        let mut sa = scope();
        sa.id = a;
        tree.push(sa);

        // Close a scope by id: non-indexed fields, no index move needed.
        let index = tree.index_of(a).unwrap();
        tree.get_mut(index).unwrap().state = ScopeState::Closed;
        tree.ensure_consistent();
        assert_eq!(tree.by_id(a).unwrap().state, ScopeState::Closed);
        assert_eq!(tree.index_of(a), Some(index));
    }

    #[test]
    fn serialize_roundtrip_rebuilds_the_index() {
        let mut tree = ScopeTree::default();
        let a = ScopeId::new();
        let mut sa = scope();
        sa.id = a;
        tree.push(sa);

        let bytes = serde_json::to_vec(&tree).unwrap();
        let restored: ScopeTree = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored.by_id(a).unwrap().id, a);
    }
}
