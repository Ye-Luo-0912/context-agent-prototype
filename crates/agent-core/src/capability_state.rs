//! Capability state authority: the core-side owner of a registered
//! capability's *state* — its effective maturity and its activation. The
//! runtime's mutable registry asks this authority for the effective state
//! and routes every transition (enable/disable/quarantine) through it, so
//! the record of what a capability may do stays in the trusted core, never
//! in a runtime structure the model can reach. Admission (whether a
//! capability may enter, and with what initial state) is the sibling
//! authority in `capability_admission`; this one owns the state after
//! admission.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use agent_contracts::{AgentError, AgentResult, CapabilityActivation, CapabilityStatus};

/// The state the core holds for one registered capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityState {
    /// Effective maturity (already pinned to Experimental by admission for
    /// external capabilities; fixed at registration and never re-climbed
    /// by declaration).
    pub status: CapabilityStatus,
    /// Whether the runtime may run this capability at all. External
    /// capabilities enter Disabled; only an explicit enable (operator or
    /// evaluator) makes them usable.
    pub activation: CapabilityActivation,
}

/// Owns the authoritative activation/maturity state of every registered
/// capability, plus its effective permission grant. Registration and
/// activation transitions are surface mutations of the runtime registry
/// too, so the registry coordinates them under its own `surface_gate`; the
/// authority is the single source of truth for the state itself. The grant
/// is the admission-validated manifest permissions, captured once at
/// registration: a capability that returns a different manifest after
/// registration cannot escalate what it holds.
#[derive(Default)]
pub struct CapabilityStateAuthority {
    inner: RwLock<HashMap<String, CapabilityState>>,
    /// The effective permission grant per capability (immutable after
    /// registration). Kept separate from `inner` so surface views that
    /// clone the state map do not copy permission lists too.
    grants: RwLock<HashMap<String, Arc<Vec<String>>>>,
}

impl CapabilityStateAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the admission-decided state and effective permission grant of
    /// a newly registered capability. Defense in depth: admission already
    /// rejected duplicate ids under the registry's lock; this refuses a
    /// second record for the same id too, so a registration and a state
    /// record can never diverge.
    pub fn register(
        &self,
        id: &str,
        state: CapabilityState,
        permissions: Vec<String>,
    ) -> AgentResult<()> {
        let mut inner = self.inner.write().expect("capability state poisoned");
        if inner.contains_key(id) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{id}' is already registered"
            )));
        }
        inner.insert(id.to_string(), state);
        self.grants
            .write()
            .expect("capability state poisoned")
            .insert(id.to_string(), Arc::new(permissions));
        Ok(())
    }

    /// The effective permission grant of a registered capability: what the
    /// runtime may hand it at invoke time, fixed at registration.
    pub fn granted_permissions(&self, id: &str) -> Option<Arc<Vec<String>>> {
        self.grants
            .read()
            .expect("capability state poisoned")
            .get(id)
            .cloned()
    }

    /// The effective maturity of a registered capability.
    pub fn status(&self, id: &str) -> Option<CapabilityStatus> {
        self.inner
            .read()
            .expect("capability state poisoned")
            .get(id)
            .map(|state| state.status)
    }

    /// The current activation of a registered capability.
    pub fn activation(&self, id: &str) -> Option<CapabilityActivation> {
        self.inner
            .read()
            .expect("capability state poisoned")
            .get(id)
            .map(|state| state.activation)
    }

    /// Transition a capability's activation. The authority decides: today
    /// every transition is allowed (an operator or evaluator may enable,
    /// disable or quarantine at any time), and the quarantine/re-enable
    /// rules can tighten here without touching the runtime. Unknown ids are
    /// refused.
    pub fn set_activation(&self, id: &str, activation: CapabilityActivation) -> AgentResult<()> {
        let mut inner = self.inner.write().expect("capability state poisoned");
        let state = inner.get_mut(id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
        })?;
        state.activation = activation;
        Ok(())
    }

    /// Snapshot of every registered capability's state, for checkpoints.
    pub fn snapshot(&self) -> Vec<(String, CapabilityState)> {
        let inner = self.inner.read().expect("capability state poisoned");
        let mut entries: Vec<_> = inner
            .iter()
            .map(|(id, state)| (id.clone(), *state))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Re-apply checkpoint state. Ids not registered in this run are
    /// skipped (their flags have nothing to apply to); a registered id's
    /// state is replaced with the checkpoint's.
    pub fn restore(&self, states: &[(String, CapabilityState)]) {
        let mut inner = self.inner.write().expect("capability state poisoned");
        for (id, state) in states {
            if let Some(current) = inner.get_mut(id) {
                *current = *state;
            }
        }
    }

    /// The full state map, for building surface views without holding this
    /// authority's lock across a registry read.
    pub fn state_map(&self) -> HashMap<String, CapabilityState> {
        self.inner
            .read()
            .expect("capability state poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: CapabilityStatus, activation: CapabilityActivation) -> CapabilityState {
        CapabilityState { status, activation }
    }

    fn register(authority: &CapabilityStateAuthority, id: &str, activation: CapabilityActivation) {
        authority
            .register(
                id,
                state(CapabilityStatus::Experimental, activation),
                vec![format!("{id}:permission")],
            )
            .unwrap();
    }

    #[test]
    fn register_then_reads_reflect_the_state() {
        let authority = CapabilityStateAuthority::new();
        register(&authority, "demo", CapabilityActivation::Enabled);
        assert_eq!(
            authority.status("demo"),
            Some(CapabilityStatus::Experimental)
        );
        assert_eq!(
            authority.activation("demo"),
            Some(CapabilityActivation::Enabled)
        );
        assert_eq!(authority.status("missing"), None);
        assert_eq!(authority.activation("missing"), None);
    }

    #[test]
    fn register_records_the_effective_grant() {
        let authority = CapabilityStateAuthority::new();
        authority
            .register(
                "demo",
                state(
                    CapabilityStatus::Experimental,
                    CapabilityActivation::Enabled,
                ),
                vec!["workspace:read".to_string(), "process:run".to_string()],
            )
            .unwrap();
        let grant = authority
            .granted_permissions("demo")
            .expect("a registered capability must have its grant");
        assert_eq!(
            grant.as_slice(),
            &["workspace:read".to_string(), "process:run".to_string()]
        );
        assert_eq!(authority.granted_permissions("missing"), None);
    }

    #[test]
    fn register_rejects_duplicate_ids() {
        let authority = CapabilityStateAuthority::new();
        authority
            .register(
                "demo",
                state(
                    CapabilityStatus::Experimental,
                    CapabilityActivation::Enabled,
                ),
                Vec::new(),
            )
            .unwrap();
        let error = authority
            .register(
                "demo",
                state(
                    CapabilityStatus::Experimental,
                    CapabilityActivation::Disabled,
                ),
                Vec::new(),
            )
            .expect_err("a duplicate state record must be rejected");
        assert!(error.to_string().contains("already registered"), "{error}");
    }

    #[test]
    fn set_activation_records_every_transition() {
        let authority = CapabilityStateAuthority::new();
        register(&authority, "demo", CapabilityActivation::Enabled);
        authority
            .set_activation("demo", CapabilityActivation::Disabled)
            .unwrap();
        assert_eq!(
            authority.activation("demo"),
            Some(CapabilityActivation::Disabled)
        );
        authority
            .set_activation("demo", CapabilityActivation::Quarantined)
            .unwrap();
        assert_eq!(
            authority.activation("demo"),
            Some(CapabilityActivation::Quarantined)
        );
        authority
            .set_activation("demo", CapabilityActivation::Enabled)
            .unwrap();
        assert_eq!(
            authority.activation("demo"),
            Some(CapabilityActivation::Enabled)
        );
    }

    #[test]
    fn set_activation_rejects_unknown_ids() {
        let authority = CapabilityStateAuthority::new();
        let error = authority
            .set_activation("missing", CapabilityActivation::Disabled)
            .expect_err("an unknown id must be rejected");
        assert!(error.to_string().contains("not registered"), "{error}");
    }

    #[test]
    fn snapshot_restore_round_trips_the_state() {
        let authority = CapabilityStateAuthority::new();
        authority
            .register(
                "a",
                state(
                    CapabilityStatus::Experimental,
                    CapabilityActivation::Enabled,
                ),
                vec!["workspace:read".to_string()],
            )
            .unwrap();
        authority
            .register(
                "b",
                state(CapabilityStatus::Stable, CapabilityActivation::Disabled),
                Vec::new(),
            )
            .unwrap();
        authority
            .set_activation("a", CapabilityActivation::Quarantined)
            .unwrap();

        let snapshot = authority.snapshot();
        let restored = CapabilityStateAuthority::new();
        restored
            .register(
                "a",
                state(
                    CapabilityStatus::Experimental,
                    CapabilityActivation::Enabled,
                ),
                vec!["workspace:read".to_string()],
            )
            .unwrap();
        restored.restore(&snapshot);

        assert_eq!(
            restored.activation("a"),
            Some(CapabilityActivation::Quarantined)
        );
        // b was never registered in the restored authority: skipped, not
        // fabricated.
        assert_eq!(restored.status("b"), None);
        // The grant is immutable per registration: restore re-applies
        // flags, never the permission record.
        assert_eq!(
            restored
                .granted_permissions("a")
                .map(|grant| grant.to_vec()),
            Some(vec!["workspace:read".to_string()])
        );
    }

    #[test]
    fn restore_skips_ids_not_registered_in_this_run() {
        let authority = CapabilityStateAuthority::new();
        register(&authority, "live", CapabilityActivation::Enabled);
        authority.restore(&[(
            "ghost".to_string(),
            state(CapabilityStatus::Stable, CapabilityActivation::Disabled),
        )]);
        assert_eq!(authority.status("ghost"), None);
        assert_eq!(
            authority.activation("live"),
            Some(CapabilityActivation::Enabled)
        );
    }

    #[test]
    fn state_map_reflects_the_current_state() {
        let authority = CapabilityStateAuthority::new();
        register(&authority, "demo", CapabilityActivation::Enabled);
        authority
            .set_activation("demo", CapabilityActivation::Quarantined)
            .unwrap();
        let map = authority.state_map();
        assert_eq!(
            map.get("demo").map(|s| s.activation),
            Some(CapabilityActivation::Quarantined)
        );
    }
}
