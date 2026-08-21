//! Trusted host effect binding: the model-facing [`crate::ToolSpec`] is not
//! an authority document.
//!
//! A plugin manifest may *request* permissions. Only a host-installed
//! policy decides which argument is which real resource. Builtin tools are
//! listed here; an unknown name with `ToolRisk::ProcessExecution` does not
//! become a process grant just because it has a `command` field.

use serde_json::Value;

use crate::{EffectIntent, ToolCall, ToolRisk, ToolSpec, exec_argv_intent, shell_exec_intent};

/// Host-installed mapping from one tool's arguments onto a real effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostToolPolicy {
    pub tool_name: &'static str,
    pub binding: HostEffectBinding,
}

/// How the host interprets this tool's arguments. Not carried on
/// [`crate::ToolSpec`]: a generated plugin cannot bind itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEffectBinding {
    ReadOnly,
    WorkspaceWrite {
        path_arg: &'static str,
        content_args: &'static [&'static str],
    },
    ExecArgv {
        argv_arg: &'static str,
    },
    ShellExec {
        command_arg: &'static str,
        dialect_arg: &'static str,
    },
    /// `process.session`: `start` is ExecArgv; poll/stop do not spawn.
    SessionExec {
        argv_arg: &'static str,
        action_arg: &'static str,
    },
}

const BUILTIN_POLICIES: &[HostToolPolicy] = &[
    HostToolPolicy {
        tool_name: "fs.list",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "fs.read",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "search.grep",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "artifact.read",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "git.status",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "git.diff",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "capability.manage",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "context.manage",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "task.complete",
        binding: HostEffectBinding::ReadOnly,
    },
    HostToolPolicy {
        tool_name: "fs.write",
        binding: HostEffectBinding::WorkspaceWrite {
            path_arg: "path",
            content_args: &["content"],
        },
    },
    HostToolPolicy {
        tool_name: "edit.replace",
        binding: HostEffectBinding::WorkspaceWrite {
            path_arg: "path",
            content_args: &["new"],
        },
    },
    HostToolPolicy {
        tool_name: "edit.patch",
        binding: HostEffectBinding::WorkspaceWrite {
            path_arg: "path",
            content_args: &[],
        },
    },
    HostToolPolicy {
        tool_name: "process.run",
        binding: HostEffectBinding::ExecArgv { argv_arg: "argv" },
    },
    HostToolPolicy {
        tool_name: "shell.exec",
        binding: HostEffectBinding::ShellExec {
            command_arg: "command",
            dialect_arg: "dialect",
        },
    },
    HostToolPolicy {
        tool_name: "process.session",
        binding: HostEffectBinding::SessionExec {
            argv_arg: "argv",
            action_arg: "action",
        },
    },
];

/// Host policy for a builtin name. Unknown plugins have none: their
/// manifest cannot authorize an effect.
pub fn builtin_host_tool_policy(tool_name: &str) -> Option<&'static HostToolPolicy> {
    BUILTIN_POLICIES
        .iter()
        .find(|policy| policy.tool_name == tool_name)
}

impl HostToolPolicy {
    pub fn intent_from(&self, arguments: &Value) -> EffectIntent {
        match self.binding {
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

/// Derive the conservative effect bound. Builtins use the host policy
/// table. A plugin `ToolSpec` with `ProcessExecution` / `WorkspaceWrite`
/// and lucky argument names does not become an intent: empty bounds never
/// match a grant.
pub fn derive_effect_intent(call: &ToolCall, spec: &ToolSpec) -> EffectIntent {
    if let Some(policy) = builtin_host_tool_policy(&call.name) {
        return policy.intent_from(&call.arguments);
    }
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
    content_args: &[&str],
) -> EffectIntent {
    let path = string_arg(arguments, path_arg);
    let mut content_bytes = 0u64;
    for key in content_args {
        if let Some(content) = arguments.get(*key).and_then(Value::as_str) {
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
            derive_effect_intent(&call, &spec("plugin.process", ToolRisk::ProcessExecution)),
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
            derive_effect_intent(&call, &spec("plugin.write", ToolRisk::WorkspaceWrite)),
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
        let policy = builtin_host_tool_policy("edit.patch").unwrap();
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
        let policy = builtin_host_tool_policy("edit.patch").unwrap();
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
