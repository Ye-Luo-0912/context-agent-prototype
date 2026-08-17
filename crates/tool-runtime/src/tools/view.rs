//! One model-visible workspace policy for ordinary file tools (`TOOL-VIEW-01`).
//!
//! `.focus-agent` and raw `.git` internals are not files for the model to
//! explore. Sealed evidence stays on `artifact.read`; VCS state stays on
//! `git.*`.

use agent_contracts::{AgentError, ToolFailureClass, ToolOutput, tool_failure_output};
use agent_workspace::Workspace;
use serde_json::json;
use tokio::fs;

const PARENT_HINT_ENTRIES: usize = 12;

/// Directory names hidden from ordinary list/read/search/code navigation.
pub(crate) fn is_hidden_name(name: &str) -> bool {
    #[cfg(windows)]
    {
        name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".focus-agent")
    }
    #[cfg(not(windows))]
    {
        name == ".git" || name == ".focus-agent"
    }
}

/// True when a workspace-relative path is under `.git` or `.focus-agent`.
pub(crate) fn ordinary_view_blocked(relative: &str) -> bool {
    first_component(relative).is_some_and(is_hidden_name)
}

fn first_component(relative: &str) -> Option<&str> {
    let bytes = relative.as_bytes();
    let mut start = 0usize;
    for (idx, byte) in bytes.iter().copied().enumerate() {
        if byte == b'/' || byte == b'\\' {
            if idx > start {
                let part = &relative[start..idx];
                if part != "." && !part.is_empty() {
                    return Some(part);
                }
            }
            start = idx + 1;
        }
    }
    if start < relative.len() {
        let part = &relative[start..];
        if !part.is_empty() && part != "." {
            return Some(part);
        }
    }
    None
}

fn is_focus_agent_path(path: &str) -> bool {
    first_component(path).is_some_and(|name| {
        #[cfg(windows)]
        {
            name.eq_ignore_ascii_case(".focus-agent")
        }
        #[cfg(not(windows))]
        {
            name == ".focus-agent"
        }
    })
}

pub(crate) fn hidden_path_output(call_id: &str, tool_name: &str, path: &str) -> ToolOutput {
    let hint = if is_focus_agent_path(path) {
        "Sealed run evidence is available through artifact.read; ordinary file tools cannot open .focus-agent."
    } else {
        "Version-control state is available through git.status and git.diff; ordinary file tools cannot open raw .git internals."
    };
    tool_failure_output(
        call_id,
        tool_name,
        ToolFailureClass::HiddenPath,
        format!("{tool_name} refused: hidden_path"),
        format!("{path} is not part of the model-visible workspace.\n{hint}"),
        json!({
            "path": path,
            "recovery_hint": hint,
        }),
    )
}

pub(crate) fn is_not_found_error(error: &AgentError) -> bool {
    match error {
        AgentError::Io(message) | AgentError::InvalidRequest(message) => {
            agent_contracts::message_looks_like_not_found(message)
        }
        _ => false,
    }
}

pub(crate) async fn missing_path_output(
    workspace: &Workspace,
    call_id: &str,
    tool_name: &str,
    path: &str,
) -> ToolOutput {
    let (parent, entries) = parent_topology_hint(workspace, path).await;
    let listing = if entries.is_empty() {
        "(none)".to_string()
    } else {
        entries.join(", ")
    };
    let hint = format!("parent `{parent}` exists; entries: [{listing}]");
    tool_failure_output(
        call_id,
        tool_name,
        ToolFailureClass::PathNotFound,
        format!("{tool_name} refused: path_not_found"),
        format!(
            "{path} was not found.\n{hint}\nDo not invent Cargo.toml, package.json or src/lib.rs when they are absent."
        ),
        json!({
            "path": path,
            "parent": parent,
            "parent_entries": entries,
            "recovery_hint": hint,
        }),
    )
}

async fn parent_topology_hint(workspace: &Workspace, path: &str) -> (String, Vec<String>) {
    let mut current = parent_relative(path);
    loop {
        match list_visible_names(workspace, &current).await {
            Ok(entries) => {
                let parent = if current.is_empty() {
                    "."
                } else {
                    current.as_str()
                };
                return (parent.to_string(), entries);
            }
            Err(_) if current.is_empty() => return (".".into(), Vec::new()),
            Err(_) => current = parent_relative(&current),
        }
    }
}

fn parent_relative(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

async fn list_visible_names(workspace: &Workspace, relative: &str) -> Result<Vec<String>, ()> {
    let path = workspace.resolve_relative(relative).await.map_err(|_| ())?;
    let mut reader = fs::read_dir(&path).await.map_err(|_| ())?;
    let mut names = Vec::new();
    while let Ok(Some(entry)) = reader.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_hidden_name(&name) {
            continue;
        }
        names.push(name);
        if names.len() >= PARENT_HINT_ENTRIES * 4 {
            break;
        }
    }
    names.sort();
    names.truncate(PARENT_HINT_ENTRIES);
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_git_and_state_dir_on_both_separators() {
        assert!(ordinary_view_blocked(".git"));
        assert!(ordinary_view_blocked(".git/HEAD"));
        assert!(ordinary_view_blocked(".focus-agent/changes.jsonl"));
        assert!(ordinary_view_blocked(".focus-agent\\traces\\x.jsonl"));
        assert!(!ordinary_view_blocked(".gitignore"));
        assert!(!ordinary_view_blocked("src/lib.rs"));
        assert!(!ordinary_view_blocked(".github/workflows/ci.yml"));
        assert!(!ordinary_view_blocked(""));
    }

    #[test]
    fn windows_ntstatus_missing_paths_are_not_found() {
        assert!(is_not_found_error(&AgentError::Io(
            "open dir C:\\tmp\\src: NTSTATUS 0xc0000034".into()
        )));
        assert!(is_not_found_error(&AgentError::Io(
            "open C:\\tmp\\lib.rs: not found (NTSTATUS 0xc000003a)".into()
        )));
        assert!(!is_not_found_error(&AgentError::Io(
            "open C:\\tmp\\x: NTSTATUS 0xc0000022".into()
        )));
    }
}
