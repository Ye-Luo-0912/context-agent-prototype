//! The trusted host policy registry (CORE-11). Composition owns the
//! mapping from tool names to real effects: builtin implementations come
//! from `tool-runtime`, and operator-reviewed plugin bindings are
//! admitted here — a manifest request never becomes an entry by itself.
//! The same registry instance must reach the kernel config and the
//! approval gate so approval and lease minting cannot drift.

use std::sync::Arc;

use agent_contracts::{HostToolPolicies, HostToolPolicy};

pub struct HostToolPolicyRegistry {
    /// Builtin entries, installed once at construction. Admissions may
    /// not shadow them: `fs.write`'s authority is the host's, not a
    /// manifest's.
    builtins: Vec<HostToolPolicy>,
    /// Operator-admitted plugin bindings.
    admitted: Vec<HostToolPolicy>,
}

impl HostToolPolicyRegistry {
    /// Builtins only — the fail-closed starting point every composition
    /// shares. Plugin tools have no entry until explicitly admitted.
    pub fn with_builtins() -> Self {
        Self {
            builtins: tool_runtime::BUILTIN_TOOL_POLICIES.clone(),
            admitted: Vec::new(),
        }
    }

    /// Install one operator-reviewed plugin binding. Fails closed on a
    /// builtin name (the host does not re-delegate its own tools) and on
    /// a duplicate admission.
    pub fn admit(&mut self, policy: HostToolPolicy) -> Result<(), String> {
        if self.builtins.iter().any(|p| p.tool_name == policy.tool_name) {
            return Err(format!(
                "plugin admission may not shadow builtin tool '{}'",
                policy.tool_name
            ));
        }
        if self.admitted.iter().any(|p| p.tool_name == policy.tool_name) {
            return Err(format!(
                "tool '{}' already has an admitted binding",
                policy.tool_name
            ));
        }
        self.admitted.push(policy);
        Ok(())
    }

    /// Shared handle for wiring into kernel config, approval gate and
    /// dispatcher from one source.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl HostToolPolicies for HostToolPolicyRegistry {
    fn policy_for(&self, tool_name: &str) -> Option<&HostToolPolicy> {
        self.admitted
            .iter()
            .find(|policy| policy.tool_name == tool_name)
            .or_else(|| {
                self.builtins
                    .iter()
                    .find(|policy| policy.tool_name == tool_name)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{EffectIntent, HostEffectBinding};
    use serde_json::json;

    fn write_binding() -> HostToolPolicy {
        HostToolPolicy {
            tool_name: "plugin.notes.write".into(),
            binding: HostEffectBinding::WorkspaceWrite {
                path_arg: "path".into(),
                content_args: vec!["text".into()],
            },
        }
    }

    #[test]
    fn builtin_names_resolve_and_admission_cannot_shadow_them() {
        let mut registry = HostToolPolicyRegistry::with_builtins();
        assert!(registry.policy_for("fs.write").is_some());
        let shadow = HostToolPolicy {
            tool_name: "fs.write".into(),
            binding: HostEffectBinding::ReadOnly,
        };
        assert!(registry.admit(shadow).is_err());
        assert!(
            !matches!(registry.policy_for("fs.write").unwrap().binding, HostEffectBinding::ReadOnly),
            "a failed admission must leave the builtin binding in place"
        );
    }

    #[test]
    fn admitted_plugin_binding_derives_a_real_intent() {
        let mut registry = HostToolPolicyRegistry::with_builtins();
        registry.admit(write_binding()).unwrap();
        let call = agent_contracts::ToolCall {
            id: "c".into(),
            name: "plugin.notes.write".into(),
            arguments: json!({"path": "notes/a.md", "text": "hello"}),
        };
        let spec = agent_contracts::ToolSpec {
            name: "plugin.notes.write".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![],
        };
        assert_eq!(
            registry.effect_intent(&call, &spec),
            EffectIntent::WorkspaceWrite {
                path: "notes/a.md".into(),
                content_bytes: 5,
            }
        );
    }

    #[test]
    fn duplicate_admission_is_rejected() {
        let mut registry = HostToolPolicyRegistry::with_builtins();
        registry.admit(write_binding()).unwrap();
        assert!(registry.admit(write_binding()).is_err());
    }

    #[test]
    fn shared_keeps_one_source_across_handles() {
        let shared = HostToolPolicyRegistry::with_builtins().shared();
        let second: Arc<dyn HostToolPolicies> = shared.clone();
        assert!(second.policy_for("edit.patch").is_some());
    }
}
