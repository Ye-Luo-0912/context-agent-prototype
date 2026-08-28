//! M15 development-pack fixtures 2 and 3 (`M15_ACCEPTANCE.md` §2):
//! `retry_diag_dev` (seeded-defect diagnosis) and `retry_migrate_dev`
//! (multi-file migration). Everything here is harness-owned and
//! network-free: seed files, the one user directive, the hidden check
//! table, and the injected behavioral oracle are frozen constants with
//! deterministic self-tests — the seeded workspace fails the contract
//! checks, the scripted minimal solution passes all of them, and the
//! oracle rejects the seed while accepting that same solution.
//!
//! Seeds intentionally differ in role from `retry_policy_dev`: the diag
//! seed's own tests encode the WRONG behavior (all green while violating
//! the documented contract), which is exactly the diagnosis pressure the
//! task measures.

use std::path::Path;

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Fixture registry
// ---------------------------------------------------------------------------

pub const RETRY_DIAG: &str = "retry_diag_dev";
pub const RETRY_MIGRATE: &str = "retry_migrate_dev";

pub struct M15Fixture {
    pub id: &'static str,
    pub directive: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    pub checks: &'static [PackCheck],
    pub oracle_name: &'static str,
    pub oracle_source: &'static str,
}

/// One hidden, read-only predicate over the final workspace.
pub struct PackCheck {
    pub path: &'static str,
    pub name: &'static str,
    pub accept: fn(&str) -> bool,
}

pub fn fixture(id: &str) -> Option<&'static M15Fixture> {
    FIXTURES.iter().find(|fixture| fixture.id == id)
}

pub const FIXTURES: &[M15Fixture] = &[RETRY_DIAG_DEV, RETRY_MIGRATE_DEV];

/// Canonical identity of every frozen input that can change a pack's task or
/// verdict. Function pointers are represented by their stable check names;
/// deterministic self-tests pin the corresponding predicate behavior.
pub fn spec_sha256(id: &str) -> String {
    let fixture = fixture(id).expect("registered fixture id");
    let mut hasher = Sha256::new();
    hasher.update(fixture.id.as_bytes());
    hasher.update(b"\n");
    hasher.update(fixture.directive.as_bytes());
    hasher.update(b"\n");
    for (path, body) in fixture.files {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
        hasher.update(body.as_bytes());
        hasher.update(b"\n");
    }
    for check in fixture.checks {
        hasher.update(check.path.as_bytes());
        hasher.update(b"\n");
        hasher.update(check.name.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(fixture.oracle_name.as_bytes());
    hasher.update(b"\n");
    hasher.update(fixture.oracle_source.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Materialize the frozen seed into a fresh workspace root.
pub fn seed(root: &Path, id: &str) -> anyhow::Result<()> {
    let fixture = fixture(id).ok_or_else(|| anyhow::anyhow!("unknown pack fixture {id}"))?;
    for (path, body) in fixture.files {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, body)?;
    }
    Ok(())
}

/// Run the hidden checks against the final workspace; results stay bounded
/// and reported per check so a failed cell names what it missed.
pub fn hidden_check_results(root: &Path, id: &str) -> Vec<(&'static str, &'static str, bool)> {
    let Some(fixture) = fixture(id) else {
        return Vec::new();
    };
    fixture
        .checks
        .iter()
        .map(|check| {
            let body = std::fs::read_to_string(root.join(check.path)).unwrap_or_default();
            (check.path, check.name, (check.accept)(&body))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// retry_diag_dev — seeded off-by-one backoff defect
// ---------------------------------------------------------------------------

const DIAG_DIRECTIVE: &str = "The job runner's documented retry contract says the first retry \
     waits exactly `base_delay_ms`, growth doubles and saturates at `max_delay_ms`, and large \
     attempt numbers saturate at the cap rather than wrapping to zero. Every check in the \
     repo is green, yet staging observed the FIRST retry waiting twice the configured base. \
     Investigate `src/backoff.rs`, write `DIAGNOSIS.md` at the workspace root naming the \
     responsible function and the precise mechanism, then apply the minimal fix and correct any \
     test that encoded the wrong behavior. Keep the public `next_delay` signature unchanged; \
     run the project checks and report the result.";

const DIAG_CARGO: &str = r#"[package]
name = "jobrunner"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"

[dependencies]
"#;

const DIAG_LIB: &str = r#"//! Job runner with a documented bounded-backoff retry contract.
pub mod backoff;
pub mod config;
pub mod sleeper;

pub use backoff::next_delay;
pub use config::RetryConfig;
pub use sleeper::FakeSleeper;
"#;

const DIAG_CONFIG: &str = r#"//! Frozen retry configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryConfig {
    /// Delay before the first retry, in milliseconds.
    pub base_delay_ms: u64,
    /// Upper bound for any computed delay, in milliseconds.
    pub max_delay_ms: u64,
}
"#;

const DIAG_SLEEPER: &str = r#"//! Fake sleeper recording the requested delays instead of waiting.
#[derive(Debug, Default)]
pub struct FakeSleeper {
    pub delays_ms: Vec<u64>,
}

impl FakeSleeper {
    pub fn sleep(&mut self, delay_ms: u64) {
        self.delays_ms.push(delay_ms);
    }
}
"#;

/// The SEEDED state: attempt is 1-based but the shift uses it directly, so
/// the first retry waits 2x base. The seed's own unit tests encode the
/// wrong table, which is why every in-repo check stays green.
const DIAG_BACKOFF_SEED: &str = r#"//! Bounded exponential backoff.
use crate::config::RetryConfig;

/// Delay before retry number `attempt` (1-based). The documented contract:
/// the first retry waits exactly `base_delay_ms`; growth doubles and
/// saturates at `max_delay_ms`.
pub fn next_delay(attempt: u32, config: &RetryConfig) -> u64 {
    let shift = attempt.min(63);
    let raw = config.base_delay_ms << shift;
    if raw > config.max_delay_ms {
        config.max_delay_ms
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RetryConfig {
        RetryConfig { base_delay_ms: 100, max_delay_ms: 1_000 }
    }

    #[test]
    fn delays_match_the_configured_table() {
        // Encodes the observed staging behavior; see DIAGNOSIS when the
        // contract ever disagrees with this table.
        assert_eq!(next_delay(1, &cfg()), 200);
        assert_eq!(next_delay(2, &cfg()), 400);
        assert_eq!(next_delay(4, &cfg()), 1_000);
    }
}
"#;

/// The scripted minimal solution used by the deterministic self-test.
#[cfg(test)]
const DIAG_BACKOFF_FIXED: &str = r#"//! Bounded exponential backoff.
use crate::config::RetryConfig;

/// Delay before retry number `attempt` (1-based). The documented contract:
/// the first retry waits exactly `base_delay_ms`; growth doubles and
/// saturates at `max_delay_ms`, never wrapping on large attempts.
pub fn next_delay(attempt: u32, config: &RetryConfig) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    let raw = (config.base_delay_ms as u128) << shift;
    raw.min(config.max_delay_ms as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RetryConfig {
        RetryConfig { base_delay_ms: 100, max_delay_ms: 1_000 }
    }

    #[test]
    fn delays_match_the_documented_contract() {
        assert_eq!(next_delay(1, &cfg()), 100);
        assert_eq!(next_delay(2, &cfg()), 200);
        assert_eq!(next_delay(11, &cfg()), 1_000);
    }
}
"#;

#[cfg(test)]
const DIAG_DIAGNOSIS_SOLVED: &str = r#"# Diagnosis

`next_delay` in `src/backoff.rs` treats the 1-based `attempt` as the shift
exponent directly (`attempt.min(63)`), so the first retry shifts by one and
waits `2 x base_delay_ms`. The seed unit test encoded the wrong table, which
kept every check green. Fix: shift by `attempt.saturating_sub(1)`, widen to
`u128` before the shift so large attempts saturate at `max_delay_ms` instead
of wrapping to zero, then cap with `min(max_delay_ms)`; the test now asserts
the documented contract.
"#;

/// Harness-owned oracle: the documented contract, injected only after the
/// run and executed as an isolated integration target.
const DIAG_ORACLE_NAME: &str = "m15_diag_oracle";

const DIAG_ORACLE_SOURCE: &str = r#"//! Harness-owned behavioral oracle; copied in by the evaluation harness.
use jobrunner::{next_delay, RetryConfig};

fn cfg(base: u64, max: u64) -> RetryConfig {
    RetryConfig { base_delay_ms: base, max_delay_ms: max }
}

#[test]
fn first_retry_waits_exactly_the_base_delay() {
    assert_eq!(next_delay(1, &cfg(100, 1_000)), 100);
    assert_eq!(next_delay(1, &cfg(100, 150)), 100, "cap below growth still first-retry = base");
}

#[test]
fn growth_doubles_then_saturates() {
    assert_eq!(next_delay(2, &cfg(100, 1_000)), 200);
    assert_eq!(next_delay(3, &cfg(100, 1_000)), 400);
    assert_eq!(next_delay(11, &cfg(100, 1_000)), 1_000);
    assert_eq!(next_delay(63, &cfg(100, 1_000)), 1_000);
}

#[test]
fn signature_stays_public_and_copyable() {
    let config = cfg(50, 500);
    assert_eq!(next_delay(1, &config), 50);
}
"#;

const DIAG_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", DIAG_CARGO),
    ("src/lib.rs", DIAG_LIB),
    ("src/config.rs", DIAG_CONFIG),
    ("src/sleeper.rs", DIAG_SLEEPER),
    ("src/backoff.rs", DIAG_BACKOFF_SEED),
    (
        "README.md",
        "# jobrunner (diag fixture)\n\nRetry contract: first retry = `base_delay_ms`; double and saturate at `max_delay_ms`.\n",
    ),
];

const DIAG_CHECKS: &[PackCheck] = &[
    PackCheck {
        path: "DIAGNOSIS.md",
        name: "diagnosis names next_delay and the off-by-one mechanism",
        accept: |body| {
            body.contains("next_delay")
                && body.contains("attempt")
                && (body.contains("shift")
                    || body.contains("off-by-one")
                    || body.contains("first retry"))
        },
    },
    PackCheck {
        path: "src/backoff.rs",
        name: "shift corrected and overflow-safe",
        accept: |body| {
            body.contains("saturating_sub(1)")
                && (body.contains("u128") || body.contains("leading_zeros"))
                && !body.contains("attempt.min(63)")
        },
    },
    PackCheck {
        path: "src/backoff.rs",
        name: "public signature unchanged",
        accept: |body| body.contains("pub fn next_delay(attempt: u32"),
    },
    PackCheck {
        path: "src/backoff.rs",
        name: "seed's wrong table no longer asserted",
        accept: |body| !body.contains("next_delay(1, &cfg()), 200"),
    },
    PackCheck {
        path: "src/config.rs",
        name: "config untouched",
        accept: |body| {
            body.contains("pub base_delay_ms: u64") && body.contains("pub max_delay_ms: u64")
        },
    },
    PackCheck {
        path: "README.md",
        name: "documented contract untouched",
        accept: |body| body.contains("first retry = `base_delay_ms`"),
    },
];

// ---------------------------------------------------------------------------
// retry_migrate_dev — monolith split with API-compat oracle
// ---------------------------------------------------------------------------

const MIGRATE_DIRECTIVE: &str = "Split `RetryPolicy` and `Backoff` (the struct, its impl, and the enum) out of \
     `src/lib.rs` into a new `src/policy.rs` module. `lib.rs` must re-export the same public names \
     so every existing import path keeps compiling, and `job.rs`, `metrics.rs` and `usage.rs` must \
     reference them through `crate::policy::`. Public API, behavior and all tests stay green; run \
     the project checks and report the result.";

const MIGRATE_CARGO: &str = DIAG_CARGO;

const MIGRATE_LIB_SEED: &str = r#"//! Monolithic job runner: retry policy still lives in the crate root.
pub mod job;
pub mod metrics;
pub mod usage;

/// Retry policy for one job: attempts and the delay table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
}

/// How the delay grows between attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    Fixed,
    Double,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, base_delay_ms: u64) -> Self {
        Self { max_attempts, base_delay_ms }
    }

    pub fn delay_before(&self, attempt: u32, backoff: Backoff) -> u64 {
        match backoff {
            Backoff::Fixed => self.base_delay_ms,
            Backoff::Double => self.base_delay_ms.saturating_mul(attempt.max(1) as u64),
        }
    }
}

pub fn max_attempts(policy: &RetryPolicy) -> u32 {
    policy.max_attempts
}
"#;

const MIGRATE_JOB: &str = r#"//! Job execution against the crate-root policy.
use crate::RetryPolicy;

pub struct Job {
    pub name: String,
}

impl Job {
    pub fn should_retry(&self, policy: &RetryPolicy, attempt: u32) -> bool {
        attempt < policy.max_attempts
    }
}
"#;

const MIGRATE_METRICS: &str = r#"//! Retry metrics.
use crate::Backoff;

#[derive(Debug, Default)]
pub struct RetryMetrics {
    pub fixed_delays: u32,
    pub doubled_delays: u32,
}

impl RetryMetrics {
    pub fn observe(&mut self, backoff: Backoff) {
        match backoff {
            Backoff::Fixed => self.fixed_delays += 1,
            Backoff::Double => self.doubled_delays += 1,
        }
    }
}
"#;

const MIGRATE_USAGE: &str = r#"//! Cross-module usage that must keep compiling after the split.
use crate::{RetryPolicy, Backoff};

pub fn total_delay(policy: &RetryPolicy, attempts: u32, backoff: Backoff) -> u64 {
    (1..attempts).map(|attempt| policy.delay_before(attempt, backoff)).sum()
}
"#;

/// The scripted minimal solution: policy.rs defines, lib.rs re-exports,
/// call sites import through `crate::policy::`.
#[cfg(test)]
const MIGRATE_POLICY_SOLVED: &str = r#"//! Retry policy extracted from the crate root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    Fixed,
    Double,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, base_delay_ms: u64) -> Self {
        Self { max_attempts, base_delay_ms }
    }

    pub fn delay_before(&self, attempt: u32, backoff: Backoff) -> u64 {
        match backoff {
            Backoff::Fixed => self.base_delay_ms,
            Backoff::Double => self.base_delay_ms.saturating_mul(attempt.max(1) as u64),
        }
    }
}
"#;

#[cfg(test)]
const MIGRATE_LIB_SOLVED: &str = r#"//! Job runner; retry policy lives in `policy` and is re-exported unchanged.
pub mod job;
pub mod metrics;
pub mod policy;
pub mod usage;

pub use policy::{Backoff, RetryPolicy};

pub fn max_attempts(policy: &RetryPolicy) -> u32 {
    policy.max_attempts
}
"#;

#[cfg(test)]
const MIGRATE_JOB_SOLVED: &str = r#"//! Job execution against the extracted policy module.
use crate::policy::RetryPolicy;

pub struct Job {
    pub name: String,
}

impl Job {
    pub fn should_retry(&self, policy: &RetryPolicy, attempt: u32) -> bool {
        attempt < policy.max_attempts
    }
}
"#;

#[cfg(test)]
const MIGRATE_METRICS_SOLVED: &str = r#"//! Retry metrics against the extracted policy module.
use crate::policy::Backoff;

#[derive(Debug, Default)]
pub struct RetryMetrics {
    pub fixed_delays: u32,
    pub doubled_delays: u32,
}

impl RetryMetrics {
    pub fn observe(&mut self, backoff: Backoff) {
        match backoff {
            Backoff::Fixed => self.fixed_delays += 1,
            Backoff::Double => self.doubled_delays += 1,
        }
    }
}
"#;

#[cfg(test)]
const MIGRATE_USAGE_SOLVED: &str = r#"//! Cross-module usage against the extracted policy module.
use crate::policy::{Backoff, RetryPolicy};

pub fn total_delay(policy: &RetryPolicy, attempts: u32, backoff: Backoff) -> u64 {
    (1..attempts).map(|attempt| policy.delay_before(attempt, backoff)).sum()
}
"#;

const MIGRATE_ORACLE_NAME: &str = "m15_migrate_oracle";

const MIGRATE_ORACLE_SOURCE: &str = r#"//! Harness-owned API-compat oracle; copied in by the evaluation harness.
use jobrunner::{usage::total_delay, Backoff, RetryPolicy};

#[test]
fn reexported_names_keep_working() {
    let policy = RetryPolicy::new(3, 40);
    assert_eq!(jobrunner::max_attempts(&policy), 3);
    assert_eq!(policy.delay_before(2, Backoff::Double), 80);
}

#[test]
fn new_module_path_is_public() {
    let policy = jobrunner::policy::RetryPolicy::new(2, 25);
    assert_eq!(jobrunner::policy::Backoff::Fixed as u32, 0);
    assert_eq!(total_delay(&policy, 4, Backoff::Double), 25 + 50 + 75);
}
"#;

const MIGRATE_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", MIGRATE_CARGO),
    ("src/lib.rs", MIGRATE_LIB_SEED),
    ("src/job.rs", MIGRATE_JOB),
    ("src/metrics.rs", MIGRATE_METRICS),
    ("src/usage.rs", MIGRATE_USAGE),
    (
        "README.md",
        "# jobrunner (migrate fixture)\n\nPublic API: `RetryPolicy`, `Backoff`, `max_attempts` — import paths must not change.\n",
    ),
];

const MIGRATE_CHECKS: &[PackCheck] = &[
    PackCheck {
        path: "src/policy.rs",
        name: "policy module defines both public items",
        accept: |body| body.contains("pub struct RetryPolicy") && body.contains("pub enum Backoff"),
    },
    PackCheck {
        path: "src/lib.rs",
        name: "crate root no longer defines the struct",
        accept: |body| !body.contains("pub struct RetryPolicy"),
    },
    PackCheck {
        path: "src/lib.rs",
        name: "crate root re-exports the same public names",
        accept: |body| {
            body.contains("pub use policy::")
                && body.contains("RetryPolicy")
                && body.contains("Backoff")
        },
    },
    PackCheck {
        path: "src/lib.rs",
        name: "crate-root helper survives the split",
        accept: |body| body.contains("pub fn max_attempts("),
    },
    PackCheck {
        path: "src/job.rs",
        name: "job imports through the policy module",
        accept: |body| body.contains("crate::policy::"),
    },
    PackCheck {
        path: "src/metrics.rs",
        name: "metrics imports through the policy module",
        accept: |body| body.contains("crate::policy::"),
    },
    PackCheck {
        path: "src/usage.rs",
        name: "usage imports through the policy module",
        accept: |body| body.contains("crate::policy::"),
    },
    PackCheck {
        path: "README.md",
        name: "public-API contract note untouched",
        accept: |body| body.contains("import paths must not change"),
    },
];

const RETRY_DIAG_DEV: M15Fixture = M15Fixture {
    id: RETRY_DIAG,
    directive: DIAG_DIRECTIVE,
    files: DIAG_FILES,
    checks: DIAG_CHECKS,
    oracle_name: DIAG_ORACLE_NAME,
    oracle_source: DIAG_ORACLE_SOURCE,
};

const RETRY_MIGRATE_DEV: M15Fixture = M15Fixture {
    id: RETRY_MIGRATE,
    directive: MIGRATE_DIRECTIVE,
    files: MIGRATE_FILES,
    checks: MIGRATE_CHECKS,
    oracle_name: MIGRATE_ORACLE_NAME,
    oracle_source: MIGRATE_ORACLE_SOURCE,
};

// ---------------------------------------------------------------------------
// Deterministic self-tests: the seeded state fails the contract checks and
// the scripted minimal solution passes all of them.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn seed_and(id: &str, solve: impl FnOnce(&Path)) -> (tempfile::TempDir, Vec<bool>) {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), id).unwrap();
        solve(dir.path());
        let results = hidden_check_results(dir.path(), id)
            .into_iter()
            .map(|(_, _, passed)| passed)
            .collect();
        (dir, results)
    }

    fn overwrite(root: &Path, relative: &str, body: &str) {
        std::fs::write(root.join(relative), body).unwrap();
    }

    #[test]
    fn diag_seed_fails_checks_and_minimal_fix_passes_all() {
        let (_dir, seeded) = seed_and(RETRY_DIAG, |_| {});
        assert!(
            seeded.iter().any(|passed| !passed),
            "the seeded defect must fail the contract checks: {seeded:?}"
        );
        let (_dir2, solved) = seed_and(RETRY_DIAG, |root| {
            overwrite(root, "src/backoff.rs", DIAG_BACKOFF_FIXED);
            overwrite(root, "DIAGNOSIS.md", DIAG_DIAGNOSIS_SOLVED);
        });
        assert!(
            solved.iter().all(|passed| *passed),
            "the minimal fix must pass every check: {solved:?}"
        );
    }

    #[test]
    fn migrate_seed_fails_checks_and_split_passes_all() {
        let (_dir, seeded) = seed_and(RETRY_MIGRATE, |_| {});
        assert!(
            seeded.iter().any(|passed| !passed),
            "the monolith must fail the split checks: {seeded:?}"
        );
        let (_dir2, solved) = seed_and(RETRY_MIGRATE, |root| {
            overwrite(root, "src/policy.rs", MIGRATE_POLICY_SOLVED);
            overwrite(root, "src/lib.rs", MIGRATE_LIB_SOLVED);
            overwrite(root, "src/job.rs", MIGRATE_JOB_SOLVED);
            overwrite(root, "src/metrics.rs", MIGRATE_METRICS_SOLVED);
            overwrite(root, "src/usage.rs", MIGRATE_USAGE_SOLVED);
        });
        assert!(
            solved.iter().all(|passed| *passed),
            "the split must pass every check: {solved:?}"
        );
    }

    const CARGO_TEST_TIMEOUT: Duration = Duration::from_secs(600);

    /// Run the harness-owned oracle the same way the live harness does
    /// (post-run injection as an isolated integration target) and report
    /// whether it compiled and every test passed.
    async fn oracle_passes(root: &Path, oracle_name: &str) -> bool {
        use std::process::Stdio;

        let mut command = tokio::process::Command::new("cargo");
        command
            .arg("test")
            .arg("--test")
            .arg(oracle_name)
            .current_dir(root)
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null());
        match tokio::time::timeout(CARGO_TEST_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output.status.success(),
            _ => false,
        }
    }

    /// Materialize the workspace with the oracle injected and report the
    /// oracle verdict; `overrides` may replace seeded files with the
    /// scripted solution.
    async fn oracle_passes_on(id: &str, overrides: &[(&str, &str)]) -> bool {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), id).unwrap();
        for (relative, body) in overrides {
            std::fs::write(dir.path().join(relative), body).unwrap();
        }
        let fixture = fixture(id).unwrap();
        let tests = dir.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(
            tests.join(format!("{}.rs", fixture.oracle_name)),
            fixture.oracle_source,
        )
        .unwrap();
        oracle_passes(dir.path(), fixture.oracle_name).await
    }

    /// Recorded pack identities; changing a frozen fixture constant
    /// requires updating these values deliberately, so the regenerated
    /// digest is visible before the formal window.
    #[test]
    fn recorded_pack_digests_are_frozen() {
        assert_eq!(
            spec_sha256(RETRY_DIAG),
            "2fff51573097fe4c833215420dd0da74f11a645ef5c859bdd9bba87e5b427eeb"
        );
        assert_eq!(
            spec_sha256(RETRY_MIGRATE),
            "26d69fa1d4ccd00452b3ceb88f2a6ec7fbb977989df6d6f4e2f1e345660679cb"
        );
    }

    /// The harness-owned oracle must reject each pack's untouched seed and
    /// accept its scripted minimal solution, so the live post-run verdict
    /// can never contradict the deterministic fixture.
    #[tokio::test]
    async fn oracles_accept_reference_solutions_and_reject_seeds() {
        let diag_solved: &[(&str, &str)] = &[
            ("src/backoff.rs", DIAG_BACKOFF_FIXED),
            ("DIAGNOSIS.md", DIAG_DIAGNOSIS_SOLVED),
        ];
        let migrate_solved: &[(&str, &str)] = &[
            ("src/policy.rs", MIGRATE_POLICY_SOLVED),
            ("src/lib.rs", MIGRATE_LIB_SOLVED),
            ("src/job.rs", MIGRATE_JOB_SOLVED),
            ("src/metrics.rs", MIGRATE_METRICS_SOLVED),
            ("src/usage.rs", MIGRATE_USAGE_SOLVED),
        ];
        for (id, solved) in [(RETRY_DIAG, diag_solved), (RETRY_MIGRATE, migrate_solved)] {
            assert!(
                !oracle_passes_on(id, &[]).await,
                "oracle must reject the untouched {id} seed"
            );
            assert!(
                oracle_passes_on(id, solved).await,
                "oracle must accept the scripted {id} solution"
            );
        }
    }

    /// Materialize a variant and `cargo check` it. #[ignore]d so the unit
    /// suite stays fast; run explicitly with `cargo test -p agent-eval
    /// m15_pack -- --ignored` (and once before freezing the window).
    #[tokio::test]
    #[ignore]
    async fn variants_cargo_check() {
        type Overrides = &'static [(&'static str, &'static str)];
        let combos: [(&str, Overrides, &str, bool); 4] = [
            (RETRY_DIAG, &[], "diag-seed", false),
            (
                RETRY_DIAG,
                &[
                    ("src/backoff.rs", DIAG_BACKOFF_FIXED),
                    ("DIAGNOSIS.md", DIAG_DIAGNOSIS_SOLVED),
                ],
                "diag-solved",
                true,
            ),
            (RETRY_MIGRATE, &[], "migrate-seed", false),
            (
                RETRY_MIGRATE,
                &[
                    ("src/policy.rs", MIGRATE_POLICY_SOLVED),
                    ("src/lib.rs", MIGRATE_LIB_SOLVED),
                    ("src/job.rs", MIGRATE_JOB_SOLVED),
                    ("src/metrics.rs", MIGRATE_METRICS_SOLVED),
                    ("src/usage.rs", MIGRATE_USAGE_SOLVED),
                ],
                "migrate-solved",
                true,
            ),
        ];
        for (id, overrides, label, with_oracle) in combos {
            let dir = tempfile::tempdir().unwrap();
            seed(dir.path(), id).unwrap();
            for (relative, body) in overrides {
                std::fs::write(dir.path().join(relative), body).unwrap();
            }
            if with_oracle {
                let fixture = fixture(id).unwrap();
                let tests = dir.path().join("tests");
                std::fs::create_dir_all(&tests).unwrap();
                std::fs::write(
                    tests.join(format!("{}.rs", fixture.oracle_name)),
                    fixture.oracle_source,
                )
                .unwrap();
            }
            let status = std::process::Command::new("cargo")
                .current_dir(dir.path())
                .args(["check", "--quiet", "--all-targets"])
                .status()
                .expect("cargo available");
            assert!(status.success(), "variant {label} must cargo-check");
        }
    }

    #[test]
    fn registry_resolves_both_and_oracles_are_frozen_constants() {
        assert_eq!(FIXTURES.len(), 2);
        for entry in FIXTURES {
            assert!(fixture(entry.id).is_some());
            assert!(!entry.directive.is_empty());
            assert!(!entry.oracle_source.is_empty());
            assert!(!entry.checks.is_empty());
            assert!(
                entry
                    .files
                    .iter()
                    .all(|(path, body)| !path.is_empty() && !body.is_empty())
            );
        }
    }
}
