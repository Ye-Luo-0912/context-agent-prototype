//! Crate-local crash failpoints for durability acceptance tests — same
//! contract as `agent_runtime::crash`: a name listed in the
//! `FOCUS_AGENT_FAILPOINTS` environment variable terminates the process
//! like a hard kill (no cleanup, exit code 9). Inert unless the variable
//! names the failpoint. Duplicated here because `agent-core` must not
//! depend on `agent-runtime`.

use std::sync::OnceLock;

/// Terminate the process when `name` is listed in `FOCUS_AGENT_FAILPOINTS`.
pub(crate) fn failpoint(name: &str) {
    static ARMED: OnceLock<Vec<String>> = OnceLock::new();
    let armed = ARMED.get_or_init(|| {
        std::env::var("FOCUS_AGENT_FAILPOINTS")
            .map(|value| {
                value
                    .split(',')
                    .map(|entry| entry.trim().to_string())
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    });
    if armed.iter().any(|armed| armed == name) {
        eprintln!("crash failpoint reached: {name}");
        std::process::exit(9);
    }
}
