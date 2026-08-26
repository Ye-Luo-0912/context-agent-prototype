//! Plugin package activation authority: the core-side owner of an installed
//! package's activation state, mirroring `CapabilityStateAuthority`
//! Admission decides whether a package may be installed at all
//! (`PluginPackageAdmission`); this authority owns *whether it runs* after
//! installation, and installation never implies activation (ECO-04): a
//! package enters `Installed` and stays inert until an explicit operator
//! action moves it. Transitions are validated, so the state machine cannot
//! be driven into an inconsistent state (e.g. quarantined -> active
//! without going through a human step).

use std::collections::HashMap;
use std::sync::RwLock;

use agent_contracts::PluginActivation;

/// The stateless transition table: which source states may move to a given
/// target. `Installed` is a terminal install-time state — a package never
/// "returns to installed" (uninstall + reinstall is the way to start
/// over); `Quarantined` may only leave through `Disabled` (a human
/// step), never straight back to `Active`.
fn can_transition(from: PluginActivation, to: PluginActivation) -> bool {
    match to {
        PluginActivation::Active => matches!(
            from,
            PluginActivation::Installed | PluginActivation::Disabled
        ),
        PluginActivation::Disabled => {
            matches!(
                from,
                PluginActivation::Active | PluginActivation::Quarantined
            )
        }
        PluginActivation::Quarantined => matches!(
            from,
            PluginActivation::Installed | PluginActivation::Active | PluginActivation::Disabled
        ),
        PluginActivation::Installed => false,
    }
}

/// The core-side activation authority for installed plugin packages.
#[derive(Debug, Default)]
pub struct PluginStateAuthority {
    states: RwLock<HashMap<String, PluginActivation>>,
}

impl PluginStateAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current activation of an installed package, if any.
    pub fn activation(&self, id: &str) -> Option<PluginActivation> {
        self.states
            .read()
            .expect("plugin state poisoned")
            .get(id)
            .copied()
    }

    /// Record a fresh install: the package enters `Installed` (inert).
    /// The registry calls this only after admission passed and the id is
    /// not already present.
    pub fn install(&self, id: &str) {
        self.states
            .write()
            .expect("plugin state poisoned")
            .insert(id.to_string(), PluginActivation::Installed);
    }

    /// Validate and apply a transition. Unknown packages and transitions
    /// the table forbids are refused with a reason.
    pub fn set_activation(&self, id: &str, next: PluginActivation) -> Result<(), String> {
        let mut states = self.states.write().expect("plugin state poisoned");
        let current = states
            .get(id)
            .copied()
            .ok_or_else(|| format!("package '{id}' is not installed"))?;
        if !can_transition(current, next) {
            return Err(format!(
                "package '{id}' cannot move from {} to {}",
                current.as_str(),
                next.as_str()
            ));
        }
        states.insert(id.to_string(), next);
        Ok(())
    }

    /// Remove a package's state on uninstall.
    pub fn uninstall(&self, id: &str) {
        self.states
            .write()
            .expect("plugin state poisoned")
            .remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_never_activates() {
        let authority = PluginStateAuthority::new();
        authority.install("pack");
        assert_eq!(
            authority.activation("pack"),
            Some(PluginActivation::Installed),
            "a fresh install must be inert"
        );
    }

    #[test]
    fn activation_path_is_ordered() {
        let authority = PluginStateAuthority::new();
        authority.install("pack");
        authority
            .set_activation("pack", PluginActivation::Active)
            .expect("installed -> active");
        authority
            .set_activation("pack", PluginActivation::Disabled)
            .expect("active -> disabled");
        authority
            .set_activation("pack", PluginActivation::Quarantined)
            .expect("disabled -> quarantined");
        authority
            .set_activation("pack", PluginActivation::Disabled)
            .expect("quarantined -> disabled (human step)");
        assert_eq!(
            authority.activation("pack"),
            Some(PluginActivation::Disabled)
        );
    }

    #[test]
    fn forbidden_transitions_are_refused() {
        let authority = PluginStateAuthority::new();
        authority.install("pack");
        // Back to Installed is never allowed.
        let error = authority
            .set_activation("pack", PluginActivation::Installed)
            .unwrap_err();
        assert!(error.contains("cannot move"), "{error}");
        // Quarantined cannot jump straight back to Active.
        authority
            .set_activation("pack", PluginActivation::Quarantined)
            .expect("installed -> quarantined");
        let error = authority
            .set_activation("pack", PluginActivation::Active)
            .unwrap_err();
        assert!(error.contains("cannot move"), "{error}");
    }

    #[test]
    fn unknown_package_is_refused() {
        let authority = PluginStateAuthority::new();
        let error = authority
            .set_activation("missing", PluginActivation::Active)
            .unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        assert_eq!(authority.activation("missing"), None);
    }

    #[test]
    fn uninstall_clears_state() {
        let authority = PluginStateAuthority::new();
        authority.install("pack");
        authority.uninstall("pack");
        assert_eq!(authority.activation("pack"), None);
    }
}
