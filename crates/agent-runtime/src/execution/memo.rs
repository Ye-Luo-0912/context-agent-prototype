//! Phase-2 semantic read memo. Not wired into dispatch.
//!
//! After Execution Coherence, identical read/query operations can reuse a
//! prior observation when `world_revision` and the resource identity still
//! match. That saves tool cost; it cannot replace the state algorithm,
//! because the model round has already happened.
//!
//! Write, patch, and shell side-effects must never be memoized.
#![allow(dead_code)]

/// Identity of a read/query observation. Arguments must be normalized.
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

/// Tools that may later be memoized. Never includes writers or shells.
pub fn is_memoizable_read(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "fs.read" | "fs.list" | "git.status" | "git.diff" | "git.log" | "search.grep"
    )
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
        assert!(is_memoizable_read("git.status"));
        assert!(is_memoizable_read("search.grep"));
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
