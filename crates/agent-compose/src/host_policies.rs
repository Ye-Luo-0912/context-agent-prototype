//! 组合根持有的宿主授权注册表：内置实现来自 tool-runtime，运维审核过
//! 的插件绑定经准入安装——清单请求本身永远不等于授权。同一实例必须
//! 同时交给内核配置与审批门，两边才不会漂移。

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

use agent_contracts::{HostPolicySnapshot, HostToolPolicies, HostToolPolicy};

pub struct HostToolPolicyRegistry {
    /// 构造时装好的内置项。准入不得遮蔽它们：`fs.write` 的授权属于
    /// 宿主，不属于清单。
    builtins: Vec<HostToolPolicy>,
    /// 运维准入的插件绑定。
    admitted: Vec<HostToolPolicy>,
    /// 版本化快照缓存（M12 P0）：`admit` 后失效，下一次
    /// [`Self::resolve_policy`] 以递增 revision 重建。消费方持有
    /// Arc 并绑定 revision；revision 变化即策略已换版。
    snapshot: RwLock<Option<Arc<HostPolicySnapshot>>>,
    revision: AtomicU64,
}

impl HostToolPolicyRegistry {
    /// 仅内置表——所有组合共用的 fail-closed 起点。未准入的插件工具
    /// 没有条目。
    pub fn with_builtins() -> Self {
        Self {
            builtins: tool_runtime::BUILTIN_TOOL_POLICIES.clone(),
            admitted: Vec::new(),
            snapshot: RwLock::new(None),
            revision: AtomicU64::new(1),
        }
    }

    /// 安装一条运维审核过的插件绑定。撞内置名或重复准入一律失败：
    /// 内置工具的授权不重新下放。成功后使快照失效。
    pub fn admit(&mut self, policy: HostToolPolicy) -> Result<(), String> {
        if self
            .builtins
            .iter()
            .any(|p| p.tool_name == policy.tool_name)
        {
            return Err(format!(
                "plugin admission may not shadow builtin tool '{}'",
                policy.tool_name
            ));
        }
        if self
            .admitted
            .iter()
            .any(|p| p.tool_name == policy.tool_name)
        {
            return Err(format!(
                "tool '{}' already has an admitted binding",
                policy.tool_name
            ));
        }
        self.admitted.push(policy);
        *self
            .snapshot
            .write()
            .expect("host policy snapshot poisoned") = None;
        Ok(())
    }

    /// 解析当前策略为版本化不可变快照：同一 revision 下重复调用返回
    /// 同一 Arc（零拷贝），`admit` 之后 revision 前进并重建。
    pub fn resolve_policy(&self) -> Arc<HostPolicySnapshot> {
        if let Some(snapshot) = self
            .snapshot
            .read()
            .expect("host policy snapshot poisoned")
            .clone()
        {
            return snapshot;
        }
        let mut guard = self
            .snapshot
            .write()
            .expect("host policy snapshot poisoned");
        // 双检：并发重建只发生一次，revision 只被真正的新表消费。
        if let Some(snapshot) = guard.clone() {
            return snapshot;
        }
        let revision = self.revision.fetch_add(1, Ordering::Relaxed);
        let mut entries = self.builtins.clone();
        entries.extend(self.admitted.iter().cloned());
        let snapshot = Arc::new(HostPolicySnapshot::resolve(entries, revision));
        *guard = Some(snapshot.clone());
        snapshot
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

    fn policy_revision(&self) -> Option<u64> {
        Some(self.resolve_policy().revision())
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
            !matches!(
                registry.policy_for("fs.write").unwrap().binding,
                HostEffectBinding::ReadOnly
            ),
            "a failed admission must leave the builtin binding in place"
        );
    }

    /// M12 P0：resolve_policy 返回版本化不可变快照；同一 revision 下
    /// 重复解析是同一 Arc，admission 换版后 revision 前进、digest 变化。
    #[test]
    fn resolve_policy_returns_versioned_snapshots() {
        let mut registry = HostToolPolicyRegistry::with_builtins();
        let before = registry.resolve_policy();
        assert!(std::sync::Arc::ptr_eq(&before, &registry.resolve_policy()));
        assert_eq!(before.revision(), 1);
        assert!(!before.is_empty());

        registry.admit(write_binding()).unwrap();
        let after = registry.resolve_policy();
        assert_ne!(
            after.revision(),
            before.revision(),
            "admission bumps revision"
        );
        assert_ne!(
            after.digest(),
            before.digest(),
            "admission changes content digest"
        );
        assert_eq!(after.len(), before.len() + 1);
        // 旧消费方持有的快照仍可独立工作（Arc 隔离）。
        assert!(before.policy_for("plugin.notes.write").is_none());
        assert!(after.policy_for("plugin.notes.write").is_some());
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
