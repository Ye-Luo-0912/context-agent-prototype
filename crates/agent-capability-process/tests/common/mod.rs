//! Shared helpers for the agent-capability-process integration tests.

use std::path::PathBuf;

/// Locate the `mock_host` helper binary of `agent-process`. It is a bin
/// target of that package, so cargo places it at `target/<profile>/mock_host`
/// — a sibling of the `deps/` directory this test binary runs from.
pub fn locate_mock_host() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "mock_host.exe"
    } else {
        "mock_host"
    };
    let current = std::env::current_exe().ok()?;
    agent_process::probe_siblings(&current, name)
}
