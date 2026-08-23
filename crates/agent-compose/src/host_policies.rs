//! 组合根持有的宿主授权注册表：内置实现来自 tool-runtime，运维审核过
//! 的插件绑定经准入安装——清单请求本身永远不等于授权。同一实例必须
//! 同时交给内核配置与审批门，两边才不会漂移。

use std::sync::Arc;

use agent_contracts::{HostToolPolicies, HostToolPolicy};

pub struct HostToolPolicyRegistry {
    /// 构造时装好的内置项。准入不得遮蔽它们：`fs.write` 的授权属于
    /// 宿主，不属于清单。
    builtins: Vec<HostToolPolicy>,
    /// 运维准入的插件绑定。
    admitted: Vec<HostToolPolicy>,
}

impl HostToolPolicyRegistry {
    /// 仅内置表——所有组合共用的 fail-closed 起点。未准入的插件工具
    /// 没有条目。
    pub fn with_builtins() -> Self {
        Self {
            builtins: tool_runtime::BUILTIN_TOOL_POLICIES.clone(),
            admitted: Vec::new(),
        }
    }

    /// 安装一条运维审核过的插件绑定。撞内置名或重复准入一律失败：
    /// 内置工具的授权不重新下放。
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

    /// 共享句柄：内核配置、审批门与分发器从同一来源接线。
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
