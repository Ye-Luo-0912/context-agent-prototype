//! Trusted host effect binding: the model-facing [`crate::ToolSpec`] is not
//! an authority document.
//!
//! A plugin manifest may *request* permissions. Only a host-installed
//! policy decides which argument is which real resource. An unknown name
//! with `ToolRisk::ProcessExecution` does not become a process grant just
//! because it has a `command` field.
//!
//! Layering (CORE-11): this module defines the *vocabulary* only — the
//! policy types, the [`HostToolPolicies`] lookup trait, and the one
//! derivation every consumer shares. The builtin implementations live in
//! `tool-runtime`; trusted composition (`agent-compose`) owns the
//! registry that combines them with operator-admitted plugin bindings.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EffectIntent, ToolCall, ToolRisk, ToolSpec, exec_argv_intent, shell_exec_intent};

/// Host-installed mapping from one tool's arguments onto a real effect.
/// Argument/name references are owned so plugin admission can install
/// bindings for non-builtin names.
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

/// Operator-owned lookup from tool name to host policy. Trusted
/// composition provides the implementation: builtin entries contributed
/// by `tool-runtime` plus admitted plugin bindings. Lookup results are
/// the only route to a concrete [`EffectIntent`] — a policy absent here
/// falls back to the declared-risk empty bound (never a grant).
pub trait HostToolPolicies: Send + Sync {
    fn policy_for(&self, tool_name: &str) -> Option<&HostToolPolicy>;

    /// Derive the concrete effect intent of one call under this mapping.
    /// Provided so every consumer (approval gate, lease minting, commit
    /// checks) shares one derivation and cannot drift.
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

/// Derive the concrete effect bound under an empty policy source: every
/// non-read-only call collapses to its declared-risk empty bound, which
/// can never match a grant. Used by compositions that admit no tools and
/// by tests of the fail-closed floor.
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

    /// Empty source: the fail-closed floor every composition starts from.
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
