//! 内置工具的宿主授权实现：本 crate 所分发工具的具体"参数→效果"绑定
//! 表。表放在处理器旁边——两边读同一批参数名；契约层只承载词汇，
//! 组合根负责把这里的内容装进注册表。

use std::sync::LazyLock;

use agent_contracts::{HostEffectBinding, HostToolPolicies, HostToolPolicy};

fn readonly(tool_name: &str) -> HostToolPolicy {
    HostToolPolicy {
        tool_name: tool_name.to_string(),
        binding: HostEffectBinding::ReadOnly,
    }
}

/// 本分发器的内置授权项，每个被分发的工具名一条；未知名字没有条目，
/// 保持 fail-closed。
pub static BUILTIN_TOOL_POLICIES: LazyLock<Vec<HostToolPolicy>> = LazyLock::new(|| {
    vec![
        readonly("fs.list"),
        readonly("fs.read"),
        readonly("search.grep"),
        readonly("artifact.read"),
        readonly("git.status"),
        readonly("git.diff"),
        readonly("capability.manage"),
        readonly("context.manage"),
        readonly("task.complete"),
        HostToolPolicy {
            tool_name: "fs.write".into(),
            binding: HostEffectBinding::WorkspaceWrite {
                path_arg: "path".into(),
                content_args: vec!["content".into()],
            },
        },
        HostToolPolicy {
            tool_name: "edit.replace".into(),
            binding: HostEffectBinding::WorkspaceWrite {
                path_arg: "path".into(),
                content_args: vec!["new".into()],
            },
        },
        HostToolPolicy {
            tool_name: "edit.patch".into(),
            binding: HostEffectBinding::WorkspaceWrite {
                path_arg: "path".into(),
                content_args: vec![],
            },
        },
        HostToolPolicy {
            tool_name: "process.run".into(),
            binding: HostEffectBinding::ExecArgv {
                argv_arg: "argv".into(),
            },
        },
        HostToolPolicy {
            tool_name: "shell.exec".into(),
            binding: HostEffectBinding::ShellExec {
                command_arg: "command".into(),
                dialect_arg: "dialect".into(),
            },
        },
        HostToolPolicy {
            tool_name: "process.session".into(),
            binding: HostEffectBinding::SessionExec {
                argv_arg: "argv".into(),
                action_arg: "action".into(),
            },
        },
    ]
});

/// 内置表之上的 [`HostToolPolicies`] 实现。无状态，组合内共享一份即可。
pub struct BuiltinToolPolicies;

impl HostToolPolicies for BuiltinToolPolicies {
    fn policy_for(&self, tool_name: &str) -> Option<&HostToolPolicy> {
        BUILTIN_TOOL_POLICIES
            .iter()
            .find(|policy| policy.tool_name == tool_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        EffectIntent, ToolCall, ToolRisk, ToolSemanticRole, ToolSpec, exec_argv_intent,
    };
    use serde_json::json;

    fn source() -> BuiltinToolPolicies {
        BuiltinToolPolicies
    }

    fn spec(name: &str, risk: ToolRisk) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            risk,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        }
    }

    #[test]
    fn every_builtin_name_resolves_and_unknowns_do_not() {
        for policy in BUILTIN_TOOL_POLICIES.iter() {
            assert!(
                source().policy_for(&policy.tool_name).is_some(),
                "{} must resolve",
                policy.tool_name
            );
        }
        assert!(source().policy_for("plugin.process").is_none());
    }

    #[test]
    fn write_intent_binds_only_the_host_mapped_arguments() {
        let call = ToolCall {
            id: "c".into(),
            name: "fs.write".into(),
            arguments: json!({"path": "src/a.rs", "content": "fn main() {}", "extra": 1}),
        };
        assert_eq!(
            source().effect_intent(&call, &spec("fs.write", ToolRisk::WorkspaceWrite)),
            EffectIntent::WorkspaceWrite {
                path: "src/a.rs".into(),
                content_bytes: "fn main() {}".len() as u64,
            }
        );
    }

    #[test]
    fn process_intent_covers_the_spawned_argv() {
        let call = ToolCall {
            id: "c".into(),
            name: "process.run".into(),
            arguments: json!({"argv": ["cargo", "test"]}),
        };
        let intent =
            source().effect_intent(&call, &spec("process.run", ToolRisk::ProcessExecution));
        assert_eq!(intent, exec_argv_intent(&["cargo".into(), "test".into()]));
    }

    #[test]
    fn session_poll_stays_read_only() {
        let call = ToolCall {
            id: "c".into(),
            name: "process.session".into(),
            arguments: json!({"action": "poll"}),
        };
        assert_eq!(
            source().effect_intent(&call, &spec("process.session", ToolRisk::ProcessExecution)),
            EffectIntent::ReadOnly
        );
    }
}
