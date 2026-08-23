//! 受信的宿主效果绑定：模型可见的 [`crate::ToolSpec`] 不是授权文书。
//!
//! 插件清单可以*请求*权限，但只有宿主安装的策略决定哪个参数对应哪个
//! 真实资源。未知名字即使带着 `ToolRisk::ProcessExecution` 和 `command`
//! 字段，也不会因此变成进程授权。
//!
//! 分层：本模块只定义词汇——策略类型、[`HostToolPolicies`] 查找
//! trait、以及所有消费方共用的那一个意图推导。内置表实现在
//! tool-runtime；组合根持有把内置项与插件准入合并的注册表。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EffectIntent, ToolCall, ToolRisk, ToolSpec, exec_argv_intent, shell_exec_intent};

/// 宿主安装的"单个工具参数 → 真实效果"映射。参数/名字用 owned 字符串，
/// 插件准入才能装非内置的名字。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostToolPolicy {
    pub tool_name: String,
    pub binding: HostEffectBinding,
}

/// How the host interprets this tool's arguments. Not carried on
/// [`crate::ToolSpec`]: a generated plugin cannot bind itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostEffectBinding {
    ReadOnly,
    WorkspaceWrite {
        path_arg: String,
        content_args: Vec<String>,
    },
    ExecArgv {
        argv_arg: String,
    },
    ShellExec {
        command_arg: String,
        dialect_arg: String,
    },
    /// `process.session`: `start` is ExecArgv; poll/stop do not spawn.
    SessionExec {
        argv_arg: String,
        action_arg: String,
    },
}

/// 工具名到宿主策略的查找。实现由组合根提供：tool-runtime 贡献内置
/// 项，加上准入的插件绑定。查到的结果是通往具体 [`EffectIntent`] 的
/// 唯一路径——这里没有的策略退回声明风险的空界限，绝不变成授权。
pub trait HostToolPolicies: Send + Sync {
    fn policy_for(&self, tool_name: &str) -> Option<&HostToolPolicy>;

    /// 当前策略集的版本号：实现了 [`HostPolicySnapshot`] 的提供方返回
    /// `Some(revision)`，供租约盖章与审计；无版本概念的实现返回 None。
    fn policy_revision(&self) -> Option<u64> {
        None
    }
    /// 在本映射下推导一次调用的具体效果意图。所有消费方（审批门、
    /// 租约铸造、提交检查）共用这一份推导，不会漂移。
    fn effect_intent(&self, call: &ToolCall, spec: &ToolSpec) -> EffectIntent {
        match self.policy_for(&call.name) {
            Some(policy) => policy.intent_from(&call.arguments),
            // A plugin ToolSpec with ProcessExecution / WorkspaceWrite and
            // lucky argument names does not become an intent: empty bounds
            // never match a grant.
            None => match spec.risk {
                ToolRisk::ReadOnly => EffectIntent::ReadOnly,
                ToolRisk::WorkspaceWrite => EffectIntent::WorkspaceWrite {
                    path: String::new(),
                    content_bytes: 0,
                },
                ToolRisk::ProcessExecution => exec_argv_intent(&[]),
            },
        }
    }
}

/// 版本化策略快照（M12 P0）：一次解析拿到的不可变表项集合 + 单调
/// revision + 内容摘要。消费方持有 [`std::sync::Arc`] 并绑定 revision；
/// 注册表安装新准入后 revision 前进，旧租约/旧准入凭据可据此检测失配。
/// digest 是 FNV-1a 完整性标记（排序后的 JCS 规范字节），不是安全哈希。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPolicySnapshot {
    entries: Vec<HostToolPolicy>,
    revision: u64,
    digest: String,
}

impl HostPolicySnapshot {
    /// 由（按工具名排序去重前的）表项构建一个快照。`revision` 由调用
    /// 方（注册表）单调分配。
    pub fn resolve(mut entries: Vec<HostToolPolicy>, revision: u64) -> Self {
        entries.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        let canonical = crate::jcs::serialize(
            &serde_json::to_value(&entries).unwrap_or(Value::Array(Vec::new())),
        )
        .unwrap_or_default();
        let digest = format!("fnv1a-{:016x}", fnv1a_64(canonical.as_bytes()));
        Self {
            entries,
            revision,
            digest,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// 表项数（审计/日志用，有界）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl HostToolPolicies for HostPolicySnapshot {
    fn policy_for(&self, tool_name: &str) -> Option<&HostToolPolicy> {
        self.entries
            .iter()
            .find(|policy| policy.tool_name == tool_name)
    }

    fn policy_revision(&self) -> Option<u64> {
        Some(self.revision)
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl HostToolPolicy {
    pub fn intent_from(&self, arguments: &Value) -> EffectIntent {
        match &self.binding {
            HostEffectBinding::ReadOnly => EffectIntent::ReadOnly,
            HostEffectBinding::WorkspaceWrite {
                path_arg,
                content_args,
            } => workspace_write_intent(arguments, path_arg, content_args),
            HostEffectBinding::ExecArgv { argv_arg } => {
                exec_argv_intent(&string_list(arguments, argv_arg))
            }
            HostEffectBinding::ShellExec {
                command_arg,
                dialect_arg,
            } => {
                let command = string_arg(arguments, command_arg);
                let dialect = string_arg(arguments, dialect_arg);
                shell_exec_intent(&dialect, &command)
            }
            HostEffectBinding::SessionExec {
                argv_arg,
                action_arg,
            } => match string_arg(arguments, action_arg).as_str() {
                "poll" | "stop" => EffectIntent::ReadOnly,
                "start" => exec_argv_intent(&string_list(arguments, argv_arg)),
                _ => exec_argv_intent(&[]),
            },
        }
    }
}

/// 空策略源下的效果界限：一切非只读调用塌缩为声明风险的空界限，
/// 永远匹配不到授权。供未准入任何工具的组合与 fail-closed 底线测试用。
pub fn unbound_effect_intent(spec: &ToolSpec) -> EffectIntent {
    match spec.risk {
        ToolRisk::ReadOnly => EffectIntent::ReadOnly,
        ToolRisk::WorkspaceWrite => EffectIntent::WorkspaceWrite {
            path: String::new(),
            content_bytes: 0,
        },
        ToolRisk::ProcessExecution => exec_argv_intent(&[]),
    }
}

fn string_arg(arguments: &Value, key: &str) -> String {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn string_list(arguments: &Value, key: &str) -> Vec<String> {
    let Some(items) = arguments.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(token) = item.as_str() else {
            return Vec::new();
        };
        if token.is_empty() {
            return Vec::new();
        }
        out.push(token.to_string());
    }
    out
}

fn workspace_write_intent(
    arguments: &Value,
    path_arg: &str,
    content_args: &[String],
) -> EffectIntent {
    let path = string_arg(arguments, path_arg);
    let mut content_bytes = 0u64;
    for key in content_args {
        if let Some(content) = arguments.get(key).and_then(Value::as_str) {
            content_bytes = content_bytes.saturating_add(content.len() as u64);
        }
    }
    if let Some(files) = arguments.get("files").and_then(Value::as_array) {
        // The multi-file form (`edit.patch` `files[]`) authorizes the
        // whole target set, never the first entry: a standing grant for
        // `src/` must not be widened by a second file outside it. The
        // trusted knowledge-plane touch set (`metadata.files[].path`)
        // already carries every target — the authority intent must carry
        // them too (MOD-AUTH-01). Each entry carries its own per-file
        // byte estimate (the hunk delta); the cap is
        // [`crate::MAX_WORKSPACE_WRITE_SET`].
        let mut writes: Vec<crate::WorkspaceWriteBound> = Vec::new();
        for file in files {
            if let Some(file_path) = file.get("path").and_then(Value::as_str) {
                let file_path = file_path.trim();
                if file_path.is_empty() {
                    continue;
                }
                match writes.iter_mut().find(|bound| bound.path == file_path) {
                    Some(existing) => {
                        existing.max_bytes =
                            existing.max_bytes.saturating_add(patch_file_bytes(file));
                    }
                    None => writes.push(crate::WorkspaceWriteBound {
                        path: file_path.to_string(),
                        max_bytes: patch_file_bytes(file),
                    }),
                }
            }
            content_bytes = content_bytes.saturating_add(patch_file_bytes(file));
        }
        writes.truncate(crate::MAX_WORKSPACE_WRITE_SET);
        return match writes.len() {
            // No usable target: keep the (possibly empty) single-path
            // form so the intent can never match a grant.
            0 => EffectIntent::WorkspaceWrite {
                path,
                content_bytes,
            },
            // One file keeps the single-resource shape so grants minted
            // from single-file calls still match exactly.
            1 => EffectIntent::WorkspaceWrite {
                path: writes.remove(0).path,
                content_bytes,
            },
            _ => EffectIntent::WorkspaceWriteSet { writes },
        };
    }
    if let Some(hunks) = arguments.get("hunks") {
        content_bytes = content_bytes.saturating_add(patch_hunk_bytes(hunks));
    }
    EffectIntent::WorkspaceWrite {
        path,
        content_bytes,
    }
}

fn patch_file_bytes(file: &Value) -> u64 {
    file.get("hunks").map(patch_hunk_bytes).unwrap_or(0)
}

fn patch_hunk_bytes(hunks: &Value) -> u64 {
    let Some(hunks) = hunks.as_array() else {
        return 0;
    };
    hunks.iter().fold(0u64, |sum, hunk| {
        sum.saturating_add(
            hunk.get("new")
                .and_then(Value::as_str)
                .map(|text| text.len() as u64)
                .unwrap_or(0),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolSemanticRole;
    use serde_json::json;

    fn spec(name: &str, risk: ToolRisk) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "t".into(),
            input_schema: json!({"type": "object"}),
            risk,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        }
    }

    /// 空来源：所有组合共用的 fail-closed 底线。
    struct NoPolicies;

    impl HostToolPolicies for NoPolicies {
        fn policy_for(&self, _tool_name: &str) -> Option<&HostToolPolicy> {
            None
        }
    }

    #[test]
    fn plugin_process_risk_does_not_bind_command_or_argv() {
        let call = ToolCall {
            id: "c".into(),
            name: "plugin.process".into(),
            arguments: json!({
                "command": "echo safe",
                "argv": ["rm", "-rf", "."]
            }),
        };
        assert_eq!(
            NoPolicies.effect_intent(&call, &spec("plugin.process", ToolRisk::ProcessExecution)),
            exec_argv_intent(&[])
        );
        assert!(!exec_argv_intent(&[]).covers(&exec_argv_intent(&[
            "rm".into(),
            "-rf".into(),
            ".".into()
        ])));
    }

    #[test]
    fn plugin_write_risk_does_not_bind_destination() {
        let call = ToolCall {
            id: "c".into(),
            name: "plugin.write".into(),
            arguments: json!({"destination": "src/a.rs", "payload": "fn main() {}"}),
        };
        assert_eq!(
            NoPolicies.effect_intent(&call, &spec("plugin.write", ToolRisk::WorkspaceWrite)),
            EffectIntent::WorkspaceWrite {
                path: String::new(),
                content_bytes: 0,
            }
        );
    }

    #[test]
    fn edit_patch_multi_file_intent_is_the_whole_target_set() {
        // MOD-AUTH-01 regression: the intent used to carry only the first
        // `files[].path`, so one granted path widened authority over the
        // rest of the set. Every distinct target must be in the intent,
        // each with its own per-file byte estimate (duplicate paths merge
        // their hunks into one bound).
        let policy = HostToolPolicy {
            tool_name: "edit.patch".into(),
            binding: HostEffectBinding::WorkspaceWrite {
                path_arg: "path".into(),
                content_args: vec![],
            },
        };
        let intent = policy.intent_from(&json!({
            "files": [
                {"path": "src/a.rs", "hunks": [{"old": "a", "new": "aa"}]},
                {"path": "secret/b.rs", "hunks": [{"old": "b", "new": "bb"}]},
                {"path": "src/a.rs", "hunks": [{"old": "c", "new": "cc"}]}
            ]
        }));
        assert_eq!(
            intent,
            EffectIntent::WorkspaceWriteSet {
                writes: vec![
                    crate::WorkspaceWriteBound {
                        path: "src/a.rs".into(),
                        max_bytes: 4,
                    },
                    crate::WorkspaceWriteBound {
                        path: "secret/b.rs".into(),
                        max_bytes: 2,
                    },
                ],
            }
        );
    }

    #[test]
    fn edit_patch_single_file_forms_stay_single_resource() {
        let policy = HostToolPolicy {
            tool_name: "edit.patch".into(),
            binding: HostEffectBinding::WorkspaceWrite {
                path_arg: "path".into(),
                content_args: vec![],
            },
        };
        // One `files[]` entry keeps the single-resource shape.
        assert_eq!(
            policy.intent_from(&json!({
                "files": [{"path": "src/a.rs", "hunks": [{"old": "a", "new": "aa"}]}]
            })),
            EffectIntent::WorkspaceWrite {
                path: "src/a.rs".into(),
                content_bytes: 2,
            }
        );
        // The `path`+`hunks` shortcut is unchanged.
        assert_eq!(
            policy.intent_from(&json!({
                "path": "src/a.rs",
                "hunks": [{"old": "a", "new": "aa"}]
            })),
            EffectIntent::WorkspaceWrite {
                path: "src/a.rs".into(),
                content_bytes: 2,
            }
        );
    }
}
