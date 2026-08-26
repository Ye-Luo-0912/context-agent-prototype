//! 组合根持有的宿主授权注册表：内置实现来自 tool-runtime，运维审核过
//! 的插件绑定经准入安装——清单请求本身永远不等于授权。同一实例必须
//! 同时交给内核配置与审批门，两边才不会漂移。

use std::collections::HashMap;
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
    /// 版本化快照缓存：`admit` 后失效，下一次
    /// [`Self::resolve_policy`] 以递增 revision 重建。消费方持有
    /// Arc 并绑定 revision；revision 变化即策略已换版。
    snapshot: RwLock<Option<Arc<HostPolicySnapshot>>>,
    revision: AtomicU64,
    /// 每个准入绑定的当前纪元：安装时分配新值、撤销时移除。租约
    /// 签发时盖章，提交期失配即按该绑定围栏——与快照 revision
    /// （表身份，防重释）语义分离，纪元从不承担表身份职责。
    binding_epochs: std::sync::Mutex<HashMap<String, u64>>,
    next_binding_epoch: AtomicU64,
}

impl HostToolPolicyRegistry {
    /// 仅内置表——所有组合共用的 fail-closed 起点。未准入的插件工具
    /// 没有条目。
    pub fn with_builtins() -> Self {
        Self::with_builtin_extensions(Vec::new())
            .expect("the static builtin host policy table must be valid")
    }

    /// Install trusted first-party extensions derived by the composition
    /// root. Unlike plugin admission these entries become immutable builtin
    /// authority and cannot later be shadowed. `verify.run` uses this path so
    /// dispatcher and Core receive the same recipe table.
    pub fn with_builtin_extensions(extensions: Vec<HostToolPolicy>) -> Result<Self, String> {
        let mut builtins = tool_runtime::BUILTIN_TOOL_POLICIES.clone();
        for extension in extensions {
            if extension.tool_name.trim().is_empty() {
                return Err("builtin extension tool name must not be empty".into());
            }
            if builtins
                .iter()
                .any(|policy| policy.tool_name == extension.tool_name)
            {
                return Err(format!(
                    "duplicate builtin host policy '{}'",
                    extension.tool_name
                ));
            }
            builtins.push(extension);
        }
        Ok(Self {
            builtins,
            admitted: Vec::new(),
            snapshot: RwLock::new(None),
            revision: AtomicU64::new(1),
            binding_epochs: std::sync::Mutex::new(HashMap::new()),
            next_binding_epoch: AtomicU64::new(1),
        })
    }

    pub fn with_builtins_and_verification(
        recipes: &tool_runtime::VerificationRecipes,
    ) -> Result<Self, String> {
        Self::with_builtin_extensions(recipes.host_policy().into_iter().collect())
    }

    /// 安装一条运维审核过的插件绑定。撞内置名或重复准入一律失败：
    /// 内置工具的授权不重新下放。成功后使快照失效并为绑定分配新纪元。
    pub fn admit(&mut self, policy: HostToolPolicy) -> Result<(), String> {
        self.ensure_admissible(&policy)?;
        let tool_name = policy.tool_name.clone();
        self.admitted.push(policy);
        self.assign_binding_epoch(&tool_name);
        *self
            .snapshot
            .write()
            .expect("host policy snapshot poisoned") = None;
        Ok(())
    }

    /// 为刚安装的绑定分配当前纪元：每次（重）安装都是新值。
    fn assign_binding_epoch(&self, tool_name: &str) {
        let epoch = self.next_binding_epoch.fetch_add(1, Ordering::Relaxed);
        self.binding_epochs
            .lock()
            .expect("binding epochs poisoned")
            .insert(tool_name.to_string(), epoch);
    }

    /// 准入前的共享检查：撞内置名或重复准入一律拒绝。批量准入先对
    /// 全部条目跑完这里，再统一落盘——半装状态比整体拒绝更危险。
    fn ensure_admissible(&self, policy: &HostToolPolicy) -> Result<(), String> {
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
        Ok(())
    }

    /// 安装一批运维审核过的插件包绑定。清单的角色只有两个：提供候选
    /// 工具名、证明包已安装并被审阅——授权内容（含参数名到效果意图的
    /// 绑定）完全来自运维审核产物，清单本身永远不产生授权。任一条目
    /// 不可准入则整批拒绝，绝不留下半装状态。
    pub fn admit_reviewed(
        &mut self,
        reviewed_tool_names: &[String],
        policies: Vec<HostToolPolicy>,
    ) -> Result<usize, String> {
        for policy in &policies {
            if !reviewed_tool_names
                .iter()
                .any(|name| name == &policy.tool_name)
            {
                return Err(format!(
                    "tool '{}' is not part of the reviewed package manifest",
                    policy.tool_name
                ));
            }
        }
        for policy in &policies {
            self.ensure_admissible(policy)?;
        }
        let count = policies.len();
        for policy in &policies {
            self.admitted.push(policy.clone());
            self.assign_binding_epoch(&policy.tool_name);
        }
        *self
            .snapshot
            .write()
            .expect("host policy snapshot poisoned") = None;
        Ok(count)
    }

    /// 撤销一条运维准入的绑定：内置授权不可撤销，未准入的名字报错。
    /// 撤销移除该绑定的纪元——此后解析的新操作不再看到宿主授权，而
    /// 盖着旧纪元的在途租约在提交期按该绑定围栏；已批准的操作仍持
    /// 旧快照，不被重释。
    pub fn revoke_admitted(&mut self, tool_name: &str) -> Result<(), String> {
        if self.builtins.iter().any(|p| p.tool_name == tool_name) {
            return Err(format!(
                "builtin tool '{tool_name}' authority cannot be revoked"
            ));
        }
        let before = self.admitted.len();
        self.admitted.retain(|p| p.tool_name != tool_name);
        if self.admitted.len() == before {
            return Err(format!(
                "tool '{tool_name}' has no admitted binding to revoke"
            ));
        }
        self.binding_epochs
            .lock()
            .expect("binding epochs poisoned")
            .remove(tool_name);
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

    fn binding_epoch(&self, tool_name: &str) -> Option<u64> {
        self.binding_epochs
            .lock()
            .expect("binding epochs poisoned")
            .get(tool_name)
            .copied()
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

    #[test]
    fn verification_extension_is_builtin_and_resolves_recipe_argv() {
        let recipe = tool_runtime::VerificationRecipe::new(
            "project.check",
            "Check the project",
            "v1",
            vec!["cargo".into(), "check".into()],
        )
        .unwrap();
        let recipes = tool_runtime::VerificationRecipes::new(vec![recipe]).unwrap();
        let mut registry =
            HostToolPolicyRegistry::with_builtins_and_verification(&recipes).unwrap();
        let policy = registry.policy_for("verify.run").unwrap();
        assert_eq!(
            policy.intent_from(&json!({"recipe_id": "project.check"})),
            agent_contracts::exec_argv_intent(&["cargo".into(), "check".into()])
        );
        assert!(
            registry
                .admit(HostToolPolicy {
                    tool_name: "verify.run".into(),
                    binding: HostEffectBinding::ReadOnly,
                })
                .is_err(),
            "trusted recipe authority cannot be shadowed by plugin admission"
        );
    }

    ///resolve_policy 返回版本化不可变快照；同一 revision 下
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

    /// 运维准入流：清单只提供候选工具名，授权内容来自运维审核产物。
    #[test]
    fn reviewed_admission_installs_operator_authority_end_to_end() {
        use agent_contracts::ToolCall;

        let mut registry = HostToolPolicyRegistry::with_builtins();
        // 审阅过的清单工具名（来自已安装包的 tools 表）。
        let reviewed = vec!["plugin.notes.write".to_string()];
        let policies = vec![write_binding()];
        assert_eq!(registry.admit_reviewed(&reviewed, policies).unwrap(), 1);

        let call = ToolCall {
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
    fn reviewed_admission_refuses_tools_outside_the_manifest_and_is_atomic() {
        let mut registry = HostToolPolicyRegistry::with_builtins();
        let reviewed = vec!["plugin.notes.write".to_string()];
        let batch = vec![
            write_binding(),
            HostToolPolicy {
                // 清单里没有这个工具名：整批必须拒绝。
                tool_name: "plugin.other.write".into(),
                binding: write_binding().binding,
            },
        ];
        assert!(registry.admit_reviewed(&reviewed, batch).is_err());
        // 原子性：第一条也不能被半装。
        assert!(registry.policy_for("plugin.notes.write").is_none());

        // 审核产物试图给内置工具发绑定：同样整批拒绝。
        let shadow_batch = vec![HostToolPolicy {
            tool_name: "fs.write".into(),
            binding: write_binding().binding,
        }];
        assert!(registry.admit_reviewed(&reviewed, shadow_batch).is_err());
        assert!(
            !matches!(
                registry.policy_for("fs.write").unwrap().binding,
                HostEffectBinding::ReadOnly
            ),
            "a refused batch must leave builtin authority in place"
        );
    }

    #[test]
    fn revocation_removes_only_the_named_admitted_tool() {
        use agent_contracts::HostEffectBinding as Binding;

        let mut registry = HostToolPolicyRegistry::with_builtins();
        let reviewed = vec![
            "plugin.notes.write".to_string(),
            "plugin.cache.clear".to_string(),
        ];
        registry
            .admit_reviewed(
                &reviewed,
                vec![
                    write_binding(),
                    HostToolPolicy {
                        tool_name: "plugin.cache.clear".into(),
                        binding: Binding::ReadOnly,
                    },
                ],
            )
            .unwrap();
        let before = registry.resolve_policy();

        // 内置授权不可撤销。
        assert!(registry.revoke_admitted("fs.write").is_err());
        // 未准入的名字不可撤销。
        assert!(registry.revoke_admitted("plugin.never.admitted").is_err());
        // 撤销一条：另一条不受影响，被撤的工具回到无宿主授权。
        registry.revoke_admitted("plugin.cache.clear").unwrap();
        assert!(registry.policy_for("plugin.notes.write").is_some());
        assert!(registry.policy_for("plugin.cache.clear").is_none());

        let after = registry.resolve_policy();
        assert_ne!(
            after.revision(),
            before.revision(),
            "revocation bumps revision"
        );
        assert_ne!(after.digest(), before.digest());
        // 旧快照消费者不被重释：仍解析出被撤销工具的旧授权。
        assert!(before.policy_for("plugin.cache.clear").is_some());

        // 撤销后可重新走准入流装回新绑定。
        registry
            .admit_reviewed(
                &reviewed,
                vec![HostToolPolicy {
                    tool_name: "plugin.cache.clear".into(),
                    binding: Binding::ReadOnly,
                }],
            )
            .unwrap();
        assert!(registry.policy_for("plugin.cache.clear").is_some());
    }

    #[test]
    fn shared_keeps_one_source_across_handles() {
        let shared = HostToolPolicyRegistry::with_builtins().shared();
        let second: Arc<dyn HostToolPolicies> = shared.clone();
        assert!(second.policy_for("edit.patch").is_some());
    }

    /// 绑定纪元按工具隔离：其他绑定的安装不改变本绑定纪元；撤销
    /// 移除它，重装得到不同新值。内置授权永无纪元。
    #[test]
    fn binding_epochs_are_per_binding_and_survive_only_until_revocation() {
        let mut registry = HostToolPolicyRegistry::with_builtins();
        assert_eq!(registry.binding_epoch("fs.write"), None);

        registry.admit(write_binding()).unwrap();
        let first = registry.binding_epoch("plugin.notes.write");
        assert!(first.is_some(), "admission assigns an epoch");

        registry
            .admit_reviewed(
                &["plugin.cache.clear".to_string()],
                vec![HostToolPolicy {
                    tool_name: "plugin.cache.clear".into(),
                    binding: HostEffectBinding::ReadOnly,
                }],
            )
            .unwrap();
        assert_eq!(
            registry.binding_epoch("plugin.notes.write"),
            first,
            "another tool's admission must not move this binding's epoch"
        );

        registry.revoke_admitted("plugin.notes.write").unwrap();
        assert_eq!(registry.binding_epoch("plugin.notes.write"), None);

        registry.admit(write_binding()).unwrap();
        let second = registry.binding_epoch("plugin.notes.write");
        assert_ne!(second, first, "a re-admitted binding gets a new epoch");
    }
}
