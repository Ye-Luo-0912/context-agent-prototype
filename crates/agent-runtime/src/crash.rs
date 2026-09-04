//! Environment-gated crash failpoints for durability acceptance tests.
//!
//! A failpoint is armed by listing its name in `FOCUS_AGENT_FAILPOINTS`
//! (comma-separated). An armed failpoint terminates the process the way a
//! hard kill does — no destructors, no journal flush, exit code 9 — so a
//! spawning test observes exactly the residue a real crash leaves behind.
//! Failpoints are inert unless the variable names them; production runs
//! never set it.

use std::sync::OnceLock;

/// Terminate the process when `name` is listed in `FOCUS_AGENT_FAILPOINTS`.
pub fn failpoint(name: &str) {
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
