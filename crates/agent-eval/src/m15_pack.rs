//! Development-pack fixtures (`M15_ACCEPTANCE.md` §2 and the LT-EVAL-06
//! deterministic task pack): `retry_diag_dev` (seeded-defect diagnosis),
//! `retry_migrate_dev` (multi-file migration), and `harness_maint_dev`
//! (evaluation-harness maintenance — a deterministic LT-EVAL-06 fixture,
//! not an M15 window pack). Everything here is harness-owned and
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
pub const RETRY_MAINT: &str = "harness_maint_dev";

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

pub const FIXTURES: &[M15Fixture] = &[
    RETRY_DIAG_DEV,
    RETRY_MIGRATE_DEV,
    RETRY_MAINT_DEV,
    LTEV_DIAGFIX_DEV,
];

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

// ---------------------------------------------------------------------------
// harness_maint_dev — regression-summary maintenance (LT-EVAL-06 task 3)
// ---------------------------------------------------------------------------
//
// The seeded defect is an evaluation-harness classification bug: a
// `NotRun` (censored/unexecuted) row is counted as `failed`, while the
// documented contract counts it separately and keeps
// `total == passed + failed + not_run`. The seeded tests never exercise
// the `NotRun` arm, so they are all green — exactly the diagnosis and
// test-repair pressure the task measures.

const MAINT_DIRECTIVE: &str = "The evaluation harness documents its regression summary \
     contract: `summarize_results` classifies every row as `passed`, `failed` or `not_run`; \
     `total` is the sum of the three classes; and a `not_run` row (a censored or unexecuted \
     case) must NEVER be counted as a failure. Every check in the repo is green, yet staging \
     observed a NOT_RUN row reported as a failure. Investigate `src/lib.rs`, write `REPORT.md` \
     at the workspace root naming the responsible function and the precise mechanism, then \
     apply the minimal fix and add a regression test that locks the `not_run` classification \
     to the documented contract (a mixed batch including at least one `not_run` row must \
     report that row as `not_run`, not `failed`, and satisfy \
     `total == passed + failed + not_run`). Keep the public `summarize_results` signature \
     unchanged; run the project checks and report the result.";

const MAINT_CARGO: &str = r#"[package]
name = "evalkit"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"

[dependencies]
"#;

// Borrowed from the M15 diag seed so the golden/diag pact stays alike.
const MAINT_LIB_SEED: &str = r#"//! Evaluation-harness regression summary.
//!
//! Documented contract: `summarize_results` classifies every row into
//! `passed`, `failed` or `not_run`; `total` is the sum of the three, and a
//! `not_run` row (a censored or unexecuted case) must never be counted as
//! a failure.

/// Outcome of one evaluated case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseStatus {
    /// Check passed.
    Passed,
    /// Check ran and failed.
    Failed,
    /// Check was never executed (censored/transport/unexecuted).
    NotRun,
}

/// One evaluated case with its classification and a bounded detail line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub id: String,
    pub status: CaseStatus,
    pub detail: String,
}

/// Mechanical summary of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub not_run: usize,
}

/// Summarize a batch. `total` must equal `passed + failed + not_run`.
pub fn summarize_results(results: &[TestResult]) -> Summary {
    let mut passed = 0;
    let mut failed = 0;
    // Seeded defect: there is no NotRun arm, so the counter never moves.
    let not_run = 0;
    for result in results {
        match result.status {
            CaseStatus::Passed => passed += 1,
            // Seeded defect: a NOT_RUN row is a censored case, not a
            // failed case, but this arm reports it as failed.
            CaseStatus::Failed | CaseStatus::NotRun => failed += 1,
        }
    }
    Summary {
        total: results.len(),
        passed,
        failed,
        not_run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: CaseStatus) -> TestResult {
        TestResult {
            id: id.to_string(),
            status,
            detail: String::new(),
        }
    }

    // All green: the seeded tests never exercise the NotRun classification.
    #[test]
    fn summarizes_passed_and_failed_rows() {
        let summary = summarize_results(&[
            row("a", CaseStatus::Passed),
            row("b", CaseStatus::Failed),
            row("c", CaseStatus::Passed),
        ]);
        assert_eq!(summary.total, 3);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.not_run, 0);
    }
}
"#;

// The scripted minimal solution used by the deterministic self-test.
#[cfg(test)]
const MAINT_LIB_FIXED: &str = r#"//! Evaluation-harness regression summary.
//!
//! Documented contract: `summarize_results` classifies every row into
//! `passed`, `failed` or `not_run`; `total` is the sum of the three, and a
//! `not_run` row (a censored or unexecuted case) must never be counted as
//! a failure.

/// Outcome of one evaluated case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseStatus {
    /// Check passed.
    Passed,
    /// Check ran and failed.
    Failed,
    /// Check was never executed (censored/transport/unexecuted).
    NotRun,
}

/// One evaluated case with its classification and a bounded detail line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub id: String,
    pub status: CaseStatus,
    pub detail: String,
}

/// Mechanical summary of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub not_run: usize,
}

/// Summarize a batch. `total` must equal `passed + failed + not_run`.
pub fn summarize_results(results: &[TestResult]) -> Summary {
    let mut passed = 0;
    let mut failed = 0;
    let mut not_run = 0;
    for result in results {
        match result.status {
            CaseStatus::Passed => passed += 1,
            CaseStatus::Failed => failed += 1,
            CaseStatus::NotRun => not_run += 1,
        }
    }
    Summary {
        total: results.len(),
        passed,
        failed,
        not_run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: CaseStatus) -> TestResult {
        TestResult {
            id: id.to_string(),
            status,
            detail: String::new(),
        }
    }

    #[test]
    fn summarizes_passed_and_failed_rows() {
        let summary = summarize_results(&[
            row("a", CaseStatus::Passed),
            row("b", CaseStatus::Failed),
            row("c", CaseStatus::Passed),
        ]);
        assert_eq!(summary.total, 3);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.not_run, 0);
    }

    // Regression: a NOT_RUN row is censored, never a failure.
    #[test]
    fn not_run_row_is_not_a_failure() {
        let summary = summarize_results(&[
            row("a", CaseStatus::Passed),
            row("b", CaseStatus::NotRun),
            row("c", CaseStatus::Failed),
            row("d", CaseStatus::NotRun),
        ]);
        assert_eq!((summary.passed, summary.failed, summary.not_run), (1, 1, 2));
        assert_eq!(
            summary.total,
            summary.passed + summary.failed + summary.not_run
        );
    }
}
"#;

const MAINT_TESTS_SEED: &str = r#"//! Model-visible integration coverage for the regression summary.
use evalkit::{summarize_results, CaseStatus, TestResult};

fn row(id: &str, status: CaseStatus) -> TestResult {
    TestResult {
        id: id.to_string(),
        status,
        detail: String::new(),
    }
}

#[test]
fn batch_without_not_run_rows_adds_up() {
    let summary = summarize_results(&[
        row("a", CaseStatus::Passed),
        row("b", CaseStatus::Failed),
        row("c", CaseStatus::Passed),
    ]);
    assert_eq!((summary.passed, summary.failed, summary.not_run), (2, 1, 0));
}
"#;

// The scripted minimal solution used by the deterministic self-test.
#[cfg(test)]
const MAINT_TESTS_FIXED: &str = r#"//! Model-visible integration coverage for the regression summary.
use evalkit::{summarize_results, CaseStatus, TestResult};

fn row(id: &str, status: CaseStatus) -> TestResult {
    TestResult {
        id: id.to_string(),
        status,
        detail: String::new(),
    }
}

#[test]
fn batch_without_not_run_rows_adds_up() {
    let summary = summarize_results(&[
        row("a", CaseStatus::Passed),
        row("b", CaseStatus::Failed),
        row("c", CaseStatus::Passed),
    ]);
    assert_eq!((summary.passed, summary.failed, summary.not_run), (2, 1, 0));
}

// Regression: NOT_RUN rows are censored cases, never failures.
#[test]
fn not_run_rows_are_not_failures() {
    let summary = summarize_results(&[
        row("a", CaseStatus::Passed),
        row("b", CaseStatus::NotRun),
        row("c", CaseStatus::Failed),
        row("d", CaseStatus::NotRun),
    ]);
    assert_eq!((summary.passed, summary.failed, summary.not_run), (1, 1, 2));
    assert_eq!(summary.total, summary.passed + summary.failed + summary.not_run);
}
"#;

// The scripted minimal solution used by the deterministic self-test.
#[cfg(test)]
const MAINT_REPORT_SOLVED: &str = r#"# Diagnosed defect: NOT_RUN rows counted as failed

`summarize_results` in `src/lib.rs` matched `CaseStatus::Failed | CaseStatus::NotRun` into the
`failed` counter, so a censored/unexecuted (not_run) row was reported as a failure and the
`not_run` counter stayed at zero. The minimal fix splits the arm: `CaseStatus::NotRun =>
not_run += 1` keeps `failed` exact and preserves the invariant `total == passed + failed +
not_run`. A regression test in `tests/report.rs` locks the classification with a mixed batch.
"#;

const MAINT_README: &str = "# evalkit (maintenance fixture)\n\nEvaluation-harness regression summary: `summarize_results` classifies rows as `passed`, `failed` or `not_run`; `total` is the sum of the three classes.\n";

const MAINT_FILES_SEED: &[(&str, &str)] = &[
    ("Cargo.toml", MAINT_CARGO),
    ("src/lib.rs", MAINT_LIB_SEED),
    ("tests/report.rs", MAINT_TESTS_SEED),
    ("README.md", MAINT_README),
];

const MAINT_ORACLE_NAME: &str = "harness_maint_oracle";
const MAINT_ORACLE_SOURCE: &str = r#"//! Harness-owned regression oracle; copied in by the evaluation harness
//! after the run. Not authored by the evaluated agent.

use evalkit::{summarize_results, CaseStatus, TestResult};

fn row(id: &str, status: CaseStatus) -> TestResult {
    TestResult {
        id: id.to_string(),
        status,
        detail: String::new(),
    }
}

#[test]
fn not_run_is_separate_from_failed() {
    let summary = summarize_results(&[
        row("a", CaseStatus::Passed),
        row("b", CaseStatus::Failed),
        row("c", CaseStatus::NotRun),
        row("d", CaseStatus::Passed),
    ]);
    assert_eq!(summary.total, 4);
    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 1, "a NOT_RUN row must not count as failed");
    assert_eq!(summary.not_run, 1);
    assert_eq!(
        summary.total,
        summary.passed + summary.failed + summary.not_run,
        "total is the sum of the three classes"
    );
}
"#;

const MAINT_CHECKS: &[PackCheck] = &[
    PackCheck {
        path: "REPORT.md",
        name: "report names summarize_results and the not_run misclassification",
        accept: |body| {
            body.contains("not_run")
                && body.contains("summarize_results")
                && (body.contains("counted as failed") || body.contains("as a failure"))
        },
    },
    PackCheck {
        path: "src/lib.rs",
        name: "failed counts only Failed rows; NotRun has its own arm",
        accept: |body| {
            body.contains("CaseStatus::Failed => failed += 1")
                && body.contains("CaseStatus::NotRun => not_run += 1")
                && !body.contains("| CaseStatus::NotRun => failed += 1")
        },
    },
    PackCheck {
        path: "tests/report.rs",
        name: "regression test locks the not_run classification",
        accept: |body| {
            body.contains("NotRun")
                && body.contains("+ summary.failed +")
                && body.contains("+ summary.not_run")
        },
    },
];

const RETRY_MAINT_DEV: M15Fixture = M15Fixture {
    id: RETRY_MAINT,
    directive: MAINT_DIRECTIVE,
    files: MAINT_FILES_SEED,
    checks: MAINT_CHECKS,
    oracle_name: MAINT_ORACLE_NAME,
    oracle_source: MAINT_ORACLE_SOURCE,
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

    #[test]
    fn maint_seed_fails_checks_and_minimal_fix_passes_all() {
        let (_dir, seeded) = seed_and(RETRY_MAINT, |_| {});
        assert!(
            seeded.iter().any(|passed| !passed),
            "the seeded misclassification must fail the contract checks: {seeded:?}"
        );
        let (_dir2, solved) = seed_and(RETRY_MAINT, |root| {
            overwrite(root, "src/lib.rs", MAINT_LIB_FIXED);
            overwrite(root, "tests/report.rs", MAINT_TESTS_FIXED);
            overwrite(root, "REPORT.md", MAINT_REPORT_SOLVED);
        });
        assert!(
            solved.iter().all(|passed| *passed),
            "the minimal fix must pass every check: {solved:?}"
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
        assert_eq!(
            spec_sha256(RETRY_MAINT),
            "c586021e9be53f8f0c4451f9894ff07bdea6a06481909f0f3fab7f700b9a2d91"
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
        let maint_solved: &[(&str, &str)] = &[
            ("src/lib.rs", MAINT_LIB_FIXED),
            ("tests/report.rs", MAINT_TESTS_FIXED),
            ("REPORT.md", MAINT_REPORT_SOLVED),
        ];
        for (id, solved) in [
            (RETRY_DIAG, diag_solved),
            (RETRY_MIGRATE, migrate_solved),
            (RETRY_MAINT, maint_solved),
        ] {
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
        let combos: [(&str, Overrides, &str, bool); 6] = [
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
            (RETRY_MAINT, &[], "maint-seed", false),
            (
                RETRY_MAINT,
                &[
                    ("src/lib.rs", MAINT_LIB_FIXED),
                    ("tests/report.rs", MAINT_TESTS_FIXED),
                    ("REPORT.md", MAINT_REPORT_SOLVED),
                ],
                "maint-solved",
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
    fn registry_resolves_all_and_oracles_are_frozen_constants() {
        assert_eq!(FIXTURES.len(), 3);
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

// ---------------------------------------------------------------------------
// ltev_diagfix — diagnosis-and-fix, defect location NOT named (LT-EVAL-06
// task 1)
// ---------------------------------------------------------------------------
//
// The pricing crate computes discounts in half-cents and the documented
// contract rounds half-cents AWAY FROM ZERO. The seeded defect
// (`halves / 2` integer truncation) hides in `src/rounding.rs` while the
// symptom surfaces in `src/receipt.rs`; the directive names neither. The
// seeded tests only exercise even half-cent products, so they are all
// green — exactly the unnamed-location diagnosis pressure LT-EVAL-06
// task 1 measures.

pub const LTEV_DIAGFIX: &str = "ltev_diagfix";

const LTEV_DIAGFIX_DIRECTIVE: &str = "Customers report that discounted totals are one cent low. \
     The pricing contract says discounts are computed in half-cents and half-cents round AWAY FROM ZERO; \
     every check in the repo is green. Find the defect somewhere in this crate, write DIAGNOSIS.md at the \
     workspace root naming the responsible file, function and precise mechanism, then apply the minimal fix \
     and extend tests/totals.rs with a regression case for an odd number of half-cents (positive and negative). \
     Keep the public signatures unchanged; run the project checks and report the result.";

const LTEV_DIAGFIX_CARGO: &str = r#"[package]
name = "receiptkit"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"

[dependencies]
"#;

const LTEV_DIAGFIX_LIB: &str = r#"//! Receipt pricing with documented half-cent rounding.
pub mod receipt;
pub mod rounding;

pub use rounding::{discounted, round_half_cent};
"#;

const LTEV_DIAGFIX_ROUNDING_SEED: &str = r#"//! Half-cent rounding. Contract: half-cents round AWAY FROM ZERO
//! (1 half -> 1 cent, 3 halves -> 2 cents, -3 halves -> -2 cents).

/// Round a half-cent value to whole cents; half away from zero.
pub fn round_half_cent(halves: i64) -> i64 {
    halves / 2
}

/// Discounted total in whole cents. The pricing engine computes the
/// discounted amount in half-cents, so the rounding contract applies.
pub fn discounted(amount_cents: i64, percent: u32) -> i64 {
    round_half_cent(amount_cents * (100 - percent as i64))
}
"#;

const LTEV_DIAGFIX_ROUNDING_FIXED: &str = r#"//! Half-cent rounding. Contract: half-cents round AWAY FROM ZERO
//! (1 half -> 1 cent, 3 halves -> 2 cents, -3 halves -> -2 cents).

/// Round a half-cent value to whole cents; half away from zero.
pub fn round_half_cent(halves: i64) -> i64 {
    (halves + halves.signum()) / 2
}

/// Discounted total in whole cents. The pricing engine computes the
/// discounted amount in half-cents, so the rounding contract applies.
pub fn discounted(amount_cents: i64, percent: u32) -> i64 {
    round_half_cent(amount_cents * (100 - percent as i64))
}
"#;

const LTEV_DIAGFIX_RECEIPT: &str = r#"//! Receipt rendering: the surface where the one-cent-low totals show up.
use crate::rounding::discounted;

/// One discounted receipt line.
pub fn line(amount_cents: i64, percent: u32) -> String {
    format!("total {}c", discounted(amount_cents, percent))
}
"#;

const LTEV_DIAGFIX_TESTS_SEED: &str = r#"//! Model-visible coverage for the pricing contract.
use receiptkit::{discounted, round_half_cent};

#[test]
fn even_half_cent_products_round_exactly() {
    assert_eq!(round_half_cent(4), 2);
    assert_eq!(round_half_cent(0), 0);
    assert_eq!(discounted(200, 50), 5000);
    assert_eq!(discounted(-200, 50), -5000);
}
"#;

const LTEV_DIAGFIX_TESTS_FIXED: &str = r#"//! Model-visible coverage for the pricing contract.
use receiptkit::{discounted, round_half_cent};

#[test]
fn even_half_cent_products_round_exactly() {
    assert_eq!(round_half_cent(4), 2);
    assert_eq!(round_half_cent(0), 0);
    assert_eq!(discounted(200, 50), 5000);
    assert_eq!(discounted(-200, 50), -5000);
}

#[test]
fn odd_half_cent_counts_round_away_from_zero() {
    // regression: an odd number of half-cents truncated toward zero
    assert_eq!(round_half_cent(1), 1);
    assert_eq!(round_half_cent(3), 2);
    assert_eq!(round_half_cent(-3), -2);
    assert_eq!(discounted(101, 33), 3384);
    assert_eq!(discounted(-101, 33), -3384);
}
"#;

const LTEV_DIAGFIX_README: &str = "receiptkit (diagnosis fixture)\n\n\
Pricing contract: discounts are computed in half-cents and half-cents round \
AWAY FROM ZERO (1 half -> 1 cent, 3 halves -> 2 cents, -3 halves -> -2 cents).\n";

const LTEV_DIAGFIX_DIAGNOSIS_SOLVED: &str = "DIAGNOSIS\n\nThe defect is in src/rounding.rs, \
function round_half_cent: `halves / 2` uses integer division, which truncates \
TOWARD ZERO, so an odd number of half-cents loses its half. The contract requires \
half away from zero. Fixed by adding the sign before halving.\n";

const LTEV_DIAGFIX_FILES_SEED: &[(&str, &str)] = &[
    ("Cargo.toml", LTEV_DIAGFIX_CARGO),
    ("src/lib.rs", LTEV_DIAGFIX_LIB),
    ("src/rounding.rs", LTEV_DIAGFIX_ROUNDING_SEED),
    ("src/receipt.rs", LTEV_DIAGFIX_RECEIPT),
    ("tests/totals.rs", LTEV_DIAGFIX_TESTS_SEED),
    ("README.md", LTEV_DIAGFIX_README),
];

const LTEV_DIAGFIX_ORACLE_NAME: &str = "ltev_diagfix_oracle";
const LTEV_DIAGFIX_ORACLE_SOURCE: &str = r#"//! Harness-owned behavioral oracle; copied in by the evaluation harness
//! after the run. Not authored by the evaluated agent.

use receiptkit::{discounted, round_half_cent};

#[test]
fn half_cent_rounding_is_away_from_zero() {
    assert_eq!(round_half_cent(0), 0);
    assert_eq!(round_half_cent(1), 1);
    assert_eq!(round_half_cent(3), 2);
    assert_eq!(round_half_cent(5), 3);
    assert_eq!(round_half_cent(-1), -1);
    assert_eq!(round_half_cent(-3), -2);
    assert_eq!(round_half_cent(4), 2);
}

#[test]
fn discounted_totals_recover_the_lost_half() {
    assert_eq!(discounted(101, 33), 3384);
    assert_eq!(discounted(-101, 33), -3384);
    assert_eq!(discounted(200, 50), 5000);
}
"#;

const LTEV_DIAGFIX_CHECKS: &[PackCheck] = &[
    PackCheck {
        path: "DIAGNOSIS.md",
        name: "report names rounding.rs, round_half_cent and the truncation mechanism",
        accept: |body| {
            body.contains("round_half_cent")
                && body.contains("rounding.rs")
                && (body.contains("truncat") || body.contains("toward zero"))
        },
    },
    PackCheck {
        path: "src/rounding.rs",
        name: "seed truncation is gone and the public signatures are unchanged",
        accept: |body| {
            !body.contains("halves / 2")
                && body.contains("pub fn round_half_cent(halves: i64) -> i64")
                && body.contains("pub fn discounted(amount_cents: i64, percent: u32) -> i64")
        },
    },
    PackCheck {
        path: "tests/totals.rs",
        name: "regression coverage for odd half-cent counts, positive and negative",
        accept: |body| {
            body.contains("round_half_cent(1)")
                && body.contains("round_half_cent(3)")
                && body.contains("round_half_cent(-3)")
        },
    },
    PackCheck {
        path: "README.md",
        name: "documented contract untouched",
        accept: |body| body == LTEV_DIAGFIX_README,
    },
];

const LTEV_DIAGFIX_DEV: M15Fixture = M15Fixture {
    id: LTEV_DIAGFIX,
    directive: LTEV_DIAGFIX_DIRECTIVE,
    files: LTEV_DIAGFIX_FILES_SEED,
    checks: LTEV_DIAGFIX_CHECKS,
    oracle_name: LTEV_DIAGFIX_ORACLE_NAME,
    oracle_source: LTEV_DIAGFIX_ORACLE_SOURCE,
};

#[cfg(test)]
mod ltev_diagfix_tests {
    use super::*;

    #[test]
    fn registered_in_the_fixture_registry() {
        assert!(fixture(LTEV_DIAGFIX).is_some());
        assert_eq!(FIXTURES.len(), 4);
    }

    /// Frozen at introduction (LT-EVAL-06 task 1, 2026-09-05).
    #[test]
    fn digest_is_frozen() {
        assert_eq!(
            spec_sha256(LTEV_DIAGFIX),
            "f867a1c5a10b65557bc520af8290e376b7839af664f635b6662e22139435b0bd"
        );
    }
}
