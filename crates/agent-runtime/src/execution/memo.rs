//! Phase-2 semantic read memo. Not wired into dispatch.
//!
//! Keep it that way through Execution Coherence V1. Memo fires *after* the
//! model has already chosen a tool, so it saves I/O, not a model round.
//! Foreground Evidence is the cheaper path: the model never issues the
//! extra `fs.read`.
//!
//! First wired version: `fs.read` only, keyed by path + line range +
//! content revision. Do not memo `search.grep` / `git.diff` / `git.status`
//! until a workspace snapshot identity exists. Write, patch, and shell
//! side-effects must never be memoized.
#![allow(dead_code)]

/// Identity of a read observation. Arguments must be normalized.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SemanticOperationKey {
    pub tool_name: String,
    pub args_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationMemo {
    pub key: SemanticOperationKey,
    pub world_revision: u64,
    pub result_ref: String,
}

/// Tools that may later be memoized. First version is `fs.read` only.
pub fn is_memoizable_read(tool_name: &str) -> bool {
    tool_name == "fs.read"
}

/// Phase 2 is not enabled. Always miss.
pub fn lookup<'a>(
    _memos: &'a [ObservationMemo],
    _key: &SemanticOperationKey,
    _world_revision: u64,
) -> Option<&'a ObservationMemo> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_and_shells_are_never_memoizable() {
        assert!(is_memoizable_read("fs.read"));
        assert!(!is_memoizable_read("fs.list"));
        assert!(!is_memoizable_read("git.status"));
        assert!(!is_memoizable_read("git.diff"));
        assert!(!is_memoizable_read("search.grep"));
        assert!(!is_memoizable_read("fs.write"));
        assert!(!is_memoizable_read("edit.replace"));
        assert!(!is_memoizable_read("edit.patch"));
        assert!(!is_memoizable_read("shell.exec"));
        assert!(!is_memoizable_read("process.run"));
    }

    #[test]
    fn lookup_is_always_a_miss_in_phase_1() {
        let memos = [ObservationMemo {
            key: SemanticOperationKey {
                tool_name: "fs.read".into(),
                args_digest: "abc".into(),
            },
            world_revision: 1,
            result_ref: "artifact://unused".into(),
        }];
        let key = SemanticOperationKey {
            tool_name: "fs.read".into(),
            args_digest: "abc".into(),
        };
        assert!(lookup(&memos, &key, 1).is_none());
    }
}
