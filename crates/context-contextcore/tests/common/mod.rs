//! Shared helpers for the context-contextcore integration tests.

use std::path::PathBuf;

/// Locate the `mock_host` test target. Cargo does not export
/// `CARGO_BIN_EXE_*` for test targets (only for bins), and test binaries
/// always carry a `-<metadata hash>` suffix, so scan the deps directory
/// next to this test binary for `mock_host-*` entries. Stale hashed copies
/// can linger after source changes, so pick the most recently built one.
pub fn locate_mock_host() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with("mock_host-") && name.ends_with(suffix)
                })
                .unwrap_or(false)
        })
        .collect();
    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    });
    candidates.into_iter().next_back()
}
