//! Shared helpers for the agent-process integration tests.

use std::path::PathBuf;

/// Locate the `mock_host` helper binary. It is a bin target of this
/// package, so cargo places it at `target/<profile>/mock_host` — a sibling
/// of the `deps/` directory this test binary runs from. The generic probe
/// covers both layouts.
pub fn locate_mock_host() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "mock_host.exe"
    } else {
        "mock_host"
    };
    let current = std::env::current_exe().ok()?;
    agent_process::probe_siblings(&current, name)
}
