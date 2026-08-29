//! `retry_policy_dev` live pilot (LONG_TASK_EVALUATION.md layer 2): the C
//! engine runs the frozen one-directive fixture with a real model in
//! `normal` and `resume` modes.
//!
//! The resume interruption is a semantic event, not a fixed round number:
//! after the first durably settled workspace mutation the harness waits for
//! its durable checkpoint, stops the runtime, restores a fresh instance
//! from the shared durable authority lineage and continues the SAME
//! directive through `continue_active_task`.
//!
//! Acceptance is behavioral, harness-owned and independent of anything the
//! evaluated agent may add: a frozen oracle test exercising only the seed's
//! public API is injected into the finished workspace after the run and
//! executed in isolation. Outcome dimensions (behavior, diff, closure,
//! continuation, provider health) are recorded separately; the final
//! verdict stays conjunctive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, CancellationToken,
    CompletionOpportunityDisposition, ContextEngine, ModelTransport, RuntimeEvent,
    RuntimeEventEnvelope, RuntimeFailureClass, TaskId, ToolCall, ToolDispatcher, ToolSpec,
};
use anyhow::bail;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::bundle::PairSink;
use crate::long_task::{self, DIRECTIVE, FIXTURE_FILES};
use crate::m15_pack;
use crate::workload::{HiddenAssertionResult, HiddenCommandResult, HiddenFileBody, HiddenReport};

pub const PILOT_SCHEMA: &str = "retry-pilot-cell-v3";
/// LONG_TASK_EVALUATION layer 2: normal and resume, two repeats each.
pub const DEFAULT_REPEATS: u32 = 2;
pub const M15_PACK_IDS: [&str; 3] = [
    m15_pack::RETRY_DIAG,
    m15_pack::RETRY_MIGRATE,
    "retry_policy_dev",
];

/// Candidate switches for one live cell. Each paired gate runs identical
/// cells with exactly one of these as the only variable; the evidence
/// records every setting.
#[derive(Debug, Clone, Copy, Default)]
pub struct CellSwitches {
    /// Item-8 advisory completion-opportunity candidate (default off).
    pub opportunity: bool,
    /// Directory-tool admission recovery-surface candidate (default off).
    pub recovery_surface: bool,
}

/// Harness-owned behavioral oracle. Injected into the finished workspace
/// after the run (so the evaluated agent never sees it) and executed as an
/// isolated integration target. It pins only the frozen public API and the
/// directive's stated semantics — never a concrete growth formula or patch
/// shape, so multiple correct implementations pass.
/// Frozen per-pack identity + evaluation hooks for one development task.
/// `run_cell` is pack-generic; every retry-specific seam routes through
/// these function pointers so a new pack cannot drift the harness.
pub type SeedFn = Box<dyn Fn(&Path) -> anyhow::Result<()>>;
pub type HiddenViolationsFn = Box<dyn Fn(&Path) -> Vec<String>>;
pub type HiddenResultsFn = Box<dyn Fn(&Path) -> Vec<(&'static str, &'static str, bool)>>;

pub(crate) struct PackSpec {
    pub id: &'static str,
    pub identity_sha256: Box<dyn Fn() -> String>,
    pub seed: SeedFn,
    pub directive: Box<dyn Fn() -> &'static str>,
    pub seed_files: Box<dyn Fn() -> Vec<&'static str>>,
    pub hidden_violations: HiddenViolationsFn,
    pub hidden_results: HiddenResultsFn,
    pub oracle: Box<dyn Fn() -> (&'static str, &'static str)>,
    /// ExactCurrentWorld recipe inputs; empty = no exact recipe for this
    /// pack (verify.run stays absent from its surface).
    pub exact_recipe_inputs: Box<dyn Fn() -> Vec<String>>,
    /// Per-pack allowed-diff predicate over workspace-relative paths.
    pub allowed_diff: Box<dyn Fn(&str) -> bool>,
}

/// The same raw cell facts can serve gates with different lifecycle policy.
/// Selection happens before the run and is persisted with the dimensions so
/// no report can silently change its acceptance rule afterward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceProfile {
    ClosureRequired,
    M15V1,
}

impl AcceptanceProfile {
    pub fn id(self) -> &'static str {
        match self {
            Self::ClosureRequired => "closure_required",
            Self::M15V1 => "m15_v1",
        }
    }

    fn requires_closure(self) -> bool {
        matches!(self, Self::ClosureRequired)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellFailureClass {
    HarnessSetup,
    HarnessWatchdog,
    ProviderTransport,
    ModelOutputLimit,
    Model,
    InputBudget,
    RoundBudget,
    Runtime,
}

impl CellFailureClass {
    fn id(self) -> &'static str {
        match self {
            Self::HarnessSetup => "harness_setup",
            Self::HarnessWatchdog => "harness_watchdog",
            Self::ProviderTransport => "provider_transport",
            Self::ModelOutputLimit => "model_output_limit",
            Self::Model => "model",
            Self::InputBudget => "input_budget",
            Self::RoundBudget => "round_budget",
            Self::Runtime => "runtime",
        }
    }

    fn not_run(self) -> bool {
        matches!(
            self,
            Self::HarnessSetup | Self::HarnessWatchdog | Self::ProviderTransport
        )
    }
}

impl From<RuntimeFailureClass> for CellFailureClass {
    fn from(value: RuntimeFailureClass) -> Self {
        match value {
            RuntimeFailureClass::ProviderTransport => Self::ProviderTransport,
            RuntimeFailureClass::ModelOutputLimit => Self::ModelOutputLimit,
            RuntimeFailureClass::Model => Self::Model,
            RuntimeFailureClass::InputBudget => Self::InputBudget,
            RuntimeFailureClass::RoundBudget => Self::RoundBudget,
            RuntimeFailureClass::Runtime => Self::Runtime,
        }
    }
}

#[derive(Debug, Clone)]
struct CellFailure {
    class: CellFailureClass,
    retryable: bool,
    message: String,
}

impl CellFailure {
    fn new(class: CellFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            retryable: false,
            message: message.into(),
        }
    }

    fn from_runtime(
        class: RuntimeFailureClass,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            class: class.into(),
            retryable,
            message: message.into(),
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self::new(CellFailureClass::Runtime, message)
    }

    fn harness_setup(message: impl Into<String>) -> Self {
        Self::new(CellFailureClass::HarnessSetup, message)
    }

    fn context(mut self, prefix: &str) -> Self {
        self.message = format!("{prefix}: {}", self.message);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellVerdict {
    Pass,
    Fail,
    NotRun,
}

impl CellVerdict {
    pub fn id(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::NotRun => "not_run",
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_verdict(
    profile: AcceptanceProfile,
    mode: PilotMode,
    failure_class: Option<CellFailureClass>,
    behavior: &str,
    diff_clean: bool,
    exact_resume_tuple_matched: bool,
    restored: bool,
    continued: bool,
    task_completed: bool,
) -> CellVerdict {
    if failure_class.is_some_and(CellFailureClass::not_run) {
        return CellVerdict::NotRun;
    }
    let resume_ready =
        mode == PilotMode::Normal || (exact_resume_tuple_matched && restored && continued);
    let passed = failure_class.is_none()
        && behavior == "pass"
        && diff_clean
        && resume_ready
        && (!profile.requires_closure() || task_completed);
    if passed {
        CellVerdict::Pass
    } else {
        CellVerdict::Fail
    }
}

fn retry_hidden_violations(root: &Path) -> Vec<String> {
    long_task::hidden_check_violations(root)
}

/// The standard edit allowance: workspace sources plus the manifest.
fn standard_allowed_diff(relative: &str) -> bool {
    relative.starts_with("src/")
        || relative.starts_with("tests/")
        || relative == "README.md"
        || relative == "Cargo.toml"
}

pub(crate) fn retry_pack() -> PackSpec {
    PackSpec {
        id: "retry_policy_dev",
        identity_sha256: Box::new(spec_sha256),
        seed: Box::new(long_task::seed_workspace),
        directive: Box::new(|| DIRECTIVE),
        seed_files: Box::new(|| {
            FIXTURE_FILES
                .iter()
                .map(|(relative, _)| *relative)
                .collect()
        }),
        hidden_violations: Box::new(retry_hidden_violations),
        hidden_results: Box::new(long_task::hidden_check_results),
        oracle: Box::new(|| (ORACLE_TEST_NAME, ORACLE_TEST_SOURCE)),
        exact_recipe_inputs: Box::new(|| {
            vec![
                "Cargo.toml".into(),
                "src/config.rs".into(),
                "src/error.rs".into(),
                "src/lib.rs".into(),
            ]
        }),
        allowed_diff: Box::new(standard_allowed_diff),
    }
}

fn m15_pack_violations(id: &'static str) -> impl Fn(&Path) -> Vec<String> {
    move |root: &Path| {
        m15_pack::hidden_check_results(root, id)
            .into_iter()
            .filter(|(_, _, passed)| !passed)
            .map(|(path, name, _)| format!("{path}: {name}"))
            .collect()
    }
}

fn m15_pack_results(id: &'static str) -> impl Fn(&Path) -> Vec<(&'static str, &'static str, bool)> {
    move |root: &Path| m15_pack::hidden_check_results(root, id)
}

pub(crate) fn m15_diag_pack() -> PackSpec {
    let id = m15_pack::RETRY_DIAG;
    PackSpec {
        id,
        identity_sha256: Box::new(move || m15_pack::spec_sha256(id)),
        seed: Box::new(move |root| m15_pack::seed(root, id)),
        directive: Box::new(move || m15_pack::fixture(id).unwrap().directive),
        seed_files: Box::new(move || {
            m15_pack::fixture(id)
                .unwrap()
                .files
                .iter()
                .map(|(relative, _)| *relative)
                .collect()
        }),
        hidden_violations: Box::new(m15_pack_violations(id)),
        hidden_results: Box::new(m15_pack_results(id)),
        oracle: Box::new(move || {
            let fixture = m15_pack::fixture(id).unwrap();
            (fixture.oracle_name, fixture.oracle_source)
        }),
        exact_recipe_inputs: Box::new(Vec::new),
        // The directive mandates a root-level DIAGNOSIS.md report.
        allowed_diff: Box::new(|relative| {
            standard_allowed_diff(relative) || relative == "DIAGNOSIS.md"
        }),
    }
}

pub(crate) fn m15_migrate_pack() -> PackSpec {
    let id = m15_pack::RETRY_MIGRATE;
    PackSpec {
        id,
        identity_sha256: Box::new(move || m15_pack::spec_sha256(id)),
        seed: Box::new(move |root| m15_pack::seed(root, id)),
        directive: Box::new(move || m15_pack::fixture(id).unwrap().directive),
        seed_files: Box::new(move || {
            m15_pack::fixture(id)
                .unwrap()
                .files
                .iter()
                .map(|(relative, _)| *relative)
                .collect()
        }),
        hidden_violations: Box::new(m15_pack_violations(id)),
        hidden_results: Box::new(m15_pack_results(id)),
        oracle: Box::new(move || {
            let fixture = m15_pack::fixture(id).unwrap();
            (fixture.oracle_name, fixture.oracle_source)
        }),
        exact_recipe_inputs: Box::new(Vec::new),
        allowed_diff: Box::new(standard_allowed_diff),
    }
}

const ORACLE_TEST_NAME: &str = "retry_policy_oracle";
const ORACLE_TEST_SOURCE: &str = r#"//! Harness-owned behavioral oracle; copied in by the evaluation harness
//! after the run. Not authored by the evaluated agent.

use jobrunner::{FakeSleeper, RetryConfig, RetryError, Sleeper};

fn config(max_attempts: u32, base_delay_ms: u64, max_delay_ms: u64) -> RetryConfig {
    RetryConfig {
        max_attempts,
        base_delay_ms,
        max_delay_ms,
    }
}

#[test]
fn first_try_success_never_waits() {
    let mut sleeper = FakeSleeper::default();
    let mut calls = 0u32;
    let result = jobrunner::run_job(&config(3, 25, 100), &mut sleeper, || {
        calls += 1;
        Ok(())
    });
    assert!(result.is_ok());
    assert_eq!(calls, 1);
    assert!(sleeper.sleeps.is_empty());
}

#[test]
fn transient_errors_are_retried_until_success() {
    let mut sleeper = FakeSleeper::default();
    let mut calls = 0u32;
    let result = jobrunner::run_job(&config(5, 10, 500), &mut sleeper, || {
        calls += 1;
        if calls < 4 {
            Err(RetryError::Transient("blip".into()))
        } else {
            Ok(())
        }
    });
    assert!(result.is_ok(), "transient faults must be retried to success");
    assert_eq!(calls, 4);
    assert_eq!(sleeper.sleeps.len(), 3, "one wait per retry");
}

#[test]
fn permanent_errors_return_without_waiting() {
    let mut sleeper = FakeSleeper::default();
    let mut calls = 0u32;
    let result = jobrunner::run_job(&config(9, 10, 500), &mut sleeper, || {
        calls += 1;
        Err(RetryError::Permanent("bad input".into()))
    });
    assert!(matches!(result, Err(RetryError::Permanent(_))));
    assert_eq!(calls, 1);
    assert!(sleeper.sleeps.is_empty());
}

#[test]
fn max_attempts_includes_the_first_call() {
    let mut sleeper = FakeSleeper::default();
    let mut calls = 0u32;
    let result = jobrunner::run_job(&config(1, 10, 100), &mut sleeper, || {
        calls += 1;
        Err(RetryError::Transient("blip".into()))
    });
    assert!(result.is_err());
    assert_eq!(calls, 1, "one attempt means no retry");
    assert!(sleeper.sleeps.is_empty());

    let mut calls = 0u32;
    let mut sleeper = FakeSleeper::default();
    let _ = jobrunner::run_job(&config(4, 10, 100), &mut sleeper, || {
        calls += 1;
        Err(RetryError::Transient("blip".into()))
    });
    assert_eq!(calls, 4, "the budget caps total attempts including the first");
    assert_eq!(sleeper.sleeps.len(), 3);
}

#[test]
fn delays_grow_monotonic_and_saturate_at_max_delay_ms() {
    let mut sleeper = FakeSleeper::default();
    let mut calls = 0u32;
    let _ = jobrunner::run_job(&config(6, 50, 400), &mut sleeper, || {
        calls += 1;
        Err(RetryError::Transient("blip".into()))
    });
    assert_eq!(calls, 6);
    let sleeps = &sleeper.sleeps;
    assert_eq!(sleeps.len(), 5);
    for delay in sleeps {
        assert!(*delay >= 1, "a nonzero base delay must produce a wait");
        assert!(*delay <= 400, "delays saturate at max_delay_ms");
    }
    for pair in sleeps.windows(2) {
        assert!(pair[0] <= pair[1], "exponential growth must not shrink");
    }
}

#[test]
fn huge_base_delays_saturate_instead_of_overflowing() {
    let mut sleeper = FakeSleeper::default();
    let mut calls = 0u32;
    let _ = jobrunner::run_job(&config(4, u64::MAX, 250), &mut sleeper, || {
        calls += 1;
        Err(RetryError::Transient("blip".into()))
    });
    assert_eq!(calls, 4);
    assert_eq!(sleeper.sleeps.len(), 3);
    for delay in &sleeper.sleeps {
        assert_eq!(
            *delay, 250,
            "a base above the cap must saturate at max_delay_ms"
        );
    }
}

#[test]
fn public_api_accepts_a_dyn_sleeper() {
    let mut sleeper = FakeSleeper::default();
    let dyn_sleeper: &mut dyn Sleeper = &mut sleeper;
    let result =
        jobrunner::run_job(&config(2, 5, 50), dyn_sleeper, || {
            Err(RetryError::Transient("once".into()))
        });
    assert!(result.is_err());
}
"#;

// The provider owns its 300-second per-attempt idle timeout and emits a
// retry-progress event before the next bounded attempt. This outer watchdog
// deliberately leaves a grace window so it cannot win the same deadline race.
const LIVE_IDLE: Duration = Duration::from_secs(330);
/// Live cells share one round cap across engines; never raise it for C.
const LIVE_MAX_MODEL_ROUNDS: u32 = 48;
/// After TurnCompleted, drain until this quiet period passes so the final
/// durable checkpoint / completion tail lands before concluding.
const TURN_QUIET_GRACE: Duration = Duration::from_secs(15);
const CARGO_TEST_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_DIFF_FILES: usize = 512;
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const FILE_BODY_CAP: usize = 64 * 1024;
const COMMAND_CAPTURE_CAP: usize = 16 * 1024;
/// Harness-owned or build-artifact paths that never count against the
/// allowed-diff rule.
const SKIP_DIRS: [&str; 4] = [".git", ".focus-agent", ".gate", "target"];
const SKIP_FILES: [&str; 2] = ["Cargo.lock", ".gitignore"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PilotMode {
    Normal,
    Resume,
}

impl PilotMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Resume => "resume",
        }
    }

    /// Parse the CLI mode filter; `None` means both modes.
    pub fn parse(raw: Option<&str>) -> anyhow::Result<Vec<Self>> {
        match raw {
            None => Ok(vec![Self::Normal, Self::Resume]),
            Some("normal") => Ok(vec![Self::Normal]),
            Some("resume") => Ok(vec![Self::Resume]),
            Some(other) => bail!("unknown retry-pilot mode {other:?} (normal|resume)"),
        }
    }
}

struct AllowAllGate;

#[async_trait::async_trait]
impl ApprovalGate for AllowAllGate {
    async fn authorize(
        &self,
        _call: &ToolCall,
        _spec: &ToolSpec,
        _cancel: &CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        Ok(ApprovalDecision::Allow)
    }
}

/// One finished live cell, before evidence serialization. Outcome
/// dimensions are recorded independently: a lifecycle-closure failure must
/// not erase whether the workspace was behaviorally correct, and the final
/// verdict stays conjunctive.
pub struct CellOutcome {
    pub pack_id: &'static str,
    pub mode: PilotMode,
    pub acceptance_profile: AcceptanceProfile,
    pub verdict: CellVerdict,
    pub passed: bool,
    /// Runtime/harness failure reason, if any. Never suppresses the
    /// read-only acceptance dimensions below.
    pub error: Option<String>,
    pub error_class: Option<CellFailureClass>,
    pub error_retryable: bool,
    pub wall_ms: u64,
    /// pass | fail | not_run(reason)
    pub behavior: String,
    /// Oracle observation retained even when a provider/harness failure makes
    /// the acceptance behavior NOT_RUN.
    pub observed_behavior: String,
    /// pass | fail
    pub diff: String,
    /// completed | active | failed
    pub closure: String,
    /// n/a | restored | failed
    pub continuation: String,
    pub exact_resume_tuple_matched: bool,
    pub restored: bool,
    pub continued: bool,
    pub turn_completed: bool,
    pub task_completed: bool,
    /// healthy | transport_failed
    pub provider_health: String,
    /// Agent-authored `cargo test` self-check: pass | fail | not_run
    pub self_check: String,
    pub resume_committed: u64,
    pub checkpoint_durable: u64,
    pub model_rounds_phase_one: u32,
    pub model_rounds_phase_two: u32,
    /// Which mutation tool created the resume debt (resume cells).
    pub resume_trigger: Option<&'static str>,
    pub diff_violations: Vec<String>,
    pub marker_violations: Vec<String>,
    /// Whether the advisory completion-opportunity candidate was enabled
    /// for this cell (the item-8 gate's only variable).
    pub opportunity: bool,
    /// Whether the directory-tool recovery surface candidate was enabled
    /// for this cell (the isolation paired gate's only variable).
    pub recovery_surface: bool,
    /// Offered opportunity keys, in arrival order across both phases.
    pub opportunity_offers: Vec<String>,
    /// The model called `task.complete` after an offer was live.
    pub opportunity_called: bool,
    /// Event-derived settlement exposure: at least one
    /// `ExecutionFrontier` carried `SettledCandidate` (exposure means a
    /// convergence gate can at least be evaluated; zero exposure is
    /// inconclusive). Candidate episodes and the rounds/calls/failures
    /// charged to them replace the old first-candidate lifetime counters;
    /// phase-two work after a reopen is never billed to an earlier episode.
    pub settlement_seen: bool,
    pub settlement_episodes: u64,
    pub settlement_episode_rounds: u64,
    pub settlement_episode_calls: u64,
    pub settlement_episode_failures: u64,
}

impl CellOutcome {
    fn failed(
        pack_id: &'static str,
        mode: PilotMode,
        acceptance_profile: AcceptanceProfile,
        switches: CellSwitches,
        wall_ms: u64,
        failure: CellFailure,
    ) -> Self {
        Self {
            pack_id,
            mode,
            acceptance_profile,
            verdict: CellVerdict::NotRun,
            passed: false,
            error: Some(failure.message),
            error_class: Some(failure.class),
            error_retryable: failure.retryable,
            wall_ms,
            behavior: "not_run".into(),
            observed_behavior: "not_run".into(),
            diff: "fail".into(),
            closure: "failed".into(),
            continuation: if mode == PilotMode::Resume {
                "failed".into()
            } else {
                "n/a".into()
            },
            exact_resume_tuple_matched: false,
            restored: false,
            continued: false,
            turn_completed: false,
            task_completed: false,
            provider_health: "healthy".into(),
            self_check: "not_run".into(),
            resume_committed: 0,
            checkpoint_durable: 0,
            model_rounds_phase_one: 0,
            model_rounds_phase_two: 0,
            resume_trigger: None,
            diff_violations: Vec::new(),
            marker_violations: Vec::new(),
            opportunity: switches.opportunity,
            opportunity_offers: Vec::new(),
            opportunity_called: false,
            recovery_surface: switches.recovery_surface,
            settlement_seen: false,
            settlement_episodes: 0,
            settlement_episode_rounds: 0,
            settlement_episode_calls: 0,
            settlement_episode_failures: 0,
        }
    }

    /// One-line human summary for the runner output.
    pub fn render_line(&self) -> String {
        let status = self.verdict.id().to_ascii_uppercase();
        let trigger = self
            .resume_trigger
            .map(|tool| format!(" trigger={tool}"))
            .unwrap_or_default();
        let opp = if self.opportunity {
            format!(
                " opp=on offers={} called={}",
                self.opportunity_offers.len(),
                self.opportunity_called
            )
        } else {
            " opp=off".to_string()
        };
        let recovery = if self.recovery_surface {
            " recovery=on"
        } else {
            " recovery=off"
        };
        let settlement = if self.settlement_seen {
            format!(
                " settled=seen episodes={} episode_rounds={} episode_calls={} episode_failures={}",
                self.settlement_episodes,
                self.settlement_episode_rounds,
                self.settlement_episode_calls,
                self.settlement_episode_failures
            )
        } else {
            " settled=none".to_string()
        };
        format!(
            "{} {:<6} {} profile={} behavior={} diff={} closure={} continuation={} provider={} rounds={}+{} resumes={} durables={}{}{}{}{}{}",
            self.pack_id,
            self.mode.id(),
            status,
            self.acceptance_profile.id(),
            self.behavior,
            self.diff,
            self.closure,
            self.continuation,
            self.provider_health,
            self.model_rounds_phase_one,
            self.model_rounds_phase_two,
            self.resume_committed,
            self.checkpoint_durable,
            trigger,
            opp,
            recovery,
            settlement,
            self.error
                .as_ref()
                .map(|reason| format!(
                    " error_class={} error={reason}",
                    self.error_class
                        .map(CellFailureClass::id)
                        .unwrap_or("unknown")
                ))
                .unwrap_or_default(),
        )
    }
}

/// Accumulates envelopes across both phases of one cell.
#[derive(Default)]
struct Collector {
    events: Vec<RuntimeEventEnvelope>,
    lagged: u64,
}

impl Collector {
    fn push(&mut self, envelope: RuntimeEventEnvelope) {
        self.events.push(envelope);
    }
}

type EventStream = tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>;

/// Per-phase counters updated while draining a stream.
#[derive(Default)]
struct PhaseState {
    model_rounds: u32,
    cancelled_for_cap: bool,
    /// Set when the resume trigger asked the operator-style cancel.
    cancel_requested: bool,
    mutation_tool: Option<&'static str>,
    durable_after_mutation: bool,
    /// Artifact name of the latest acknowledged durable checkpoint.
    last_durable_artifact: Option<String>,
    /// Full acknowledged tuple for the mutation debt.
    last_durable_sequence: Option<u64>,
    last_durable_checksum: Option<String>,
    last_durable_capability_generation: Option<u64>,
    last_resume_sequence: Option<u64>,
    last_resume_task_id: Option<TaskId>,
    resume_committed: u64,
    checkpoint_durable: u64,
    turn_completed: bool,
    task_completed: bool,
    /// Offered opportunity keys, in arrival order (candidate-on cells).
    opportunity_offers: Vec<String>,
    /// The model called `task.complete` while an offer was live.
    opportunity_called: bool,
}

enum StepOutcome {
    Continue,
    TaskCompleted,
    TurnCompleted,
    /// The turn stopped because this loop requested the cancel.
    ExpectedCancel,
    Fail(CellFailure),
}

impl PhaseState {
    fn over_round_cap(&self) -> bool {
        self.model_rounds > LIVE_MAX_MODEL_ROUNDS
    }

    fn enforce_cap(&mut self, handle: &agent_runtime::RuntimeHandle) {
        if self.over_round_cap() && !self.cancelled_for_cap {
            self.cancelled_for_cap = true;
            let handle = handle.clone();
            tokio::spawn(async move {
                let _ = handle.cancel_turn().await;
            });
        }
    }
}

fn step_event(
    envelope: RuntimeEventEnvelope,
    collector: &mut Collector,
    state: &mut PhaseState,
) -> StepOutcome {
    match envelope.event {
        RuntimeEvent::ModelStarted { .. } => {
            state.model_rounds = state.model_rounds.saturating_add(1);
            collector.push(envelope);
            StepOutcome::Continue
        }
        RuntimeEvent::ToolFinished { ref output }
            if output.ok
                && matches!(
                    output.tool_name.as_str(),
                    "fs.write" | "edit.replace" | "edit.patch"
                ) =>
        {
            if state.mutation_tool.is_none() {
                state.mutation_tool = Some(match output.tool_name.as_str() {
                    "edit.replace" => "edit.replace",
                    "edit.patch" => "edit.patch",
                    _ => "fs.write",
                });
            }
            collector.push(envelope);
            StepOutcome::Continue
        }
        RuntimeEvent::CheckpointDurable {
            ref artifact,
            ref checksum,
            sequence,
            capability_generation,
            ..
        } => {
            state.checkpoint_durable = state.checkpoint_durable.saturating_add(1);
            if !artifact.is_empty() {
                state.last_durable_artifact = Some(artifact.clone());
                state.last_durable_sequence = Some(sequence);
                state.last_durable_checksum = Some(checksum.clone());
                state.last_durable_capability_generation = Some(capability_generation);
            }
            if state.mutation_tool.is_some()
                && state.last_resume_sequence.is_some()
                && Some(sequence) == state.last_resume_sequence
            {
                state.durable_after_mutation = true;
            }
            collector.push(envelope);
            StepOutcome::Continue
        }
        RuntimeEvent::TaskResumeCommitted {
            task_id, sequence, ..
        } => {
            state.resume_committed = state.resume_committed.saturating_add(1);
            state.last_resume_sequence = Some(sequence);
            state.last_resume_task_id = Some(task_id);
            collector.push(envelope);
            StepOutcome::Continue
        }
        RuntimeEvent::TaskCompleted { .. } => {
            state.task_completed = true;
            collector.push(envelope);
            StepOutcome::TaskCompleted
        }
        RuntimeEvent::CompletionOpportunity {
            ref disposition,
            ref key,
            ..
        } => {
            match disposition {
                CompletionOpportunityDisposition::Offered => {
                    state.opportunity_offers.push(key.clone());
                }
                CompletionOpportunityDisposition::Called => state.opportunity_called = true,
                _ => {}
            }
            collector.push(envelope);
            StepOutcome::Continue
        }
        RuntimeEvent::TurnCompleted => {
            state.turn_completed = true;
            collector.push(envelope);
            StepOutcome::TurnCompleted
        }
        RuntimeEvent::TurnCancelled { .. } => {
            collector.push(envelope);
            if state.cancel_requested {
                StepOutcome::ExpectedCancel
            } else if state.cancelled_for_cap {
                StepOutcome::Fail(CellFailure::new(
                    CellFailureClass::RoundBudget,
                    format!("live model-round cap ({LIVE_MAX_MODEL_ROUNDS}) exceeded"),
                ))
            } else {
                StepOutcome::Fail(CellFailure::runtime("turn cancelled"))
            }
        }
        RuntimeEvent::TurnCommitFailed { ref message, .. } => {
            let reason = format!("turn commit failed: {message}");
            collector.push(envelope);
            StepOutcome::Fail(CellFailure::runtime(reason))
        }
        RuntimeEvent::Failure {
            class,
            retryable,
            ref message,
        } => {
            let failure = CellFailure::from_runtime(class, retryable, message.clone());
            collector.push(envelope);
            StepOutcome::Fail(failure)
        }
        RuntimeEvent::Error { ref message } => {
            let reason = format!("runtime error: {message}");
            collector.push(envelope);
            // Settling an operator cancel can surface cleanup errors that
            // do not doom the stop; only fail on them outside that window.
            if state.cancel_requested {
                StepOutcome::Continue
            } else {
                StepOutcome::Fail(CellFailure::runtime(reason))
            }
        }
        RuntimeEvent::RecoveryRequired => {
            collector.push(envelope);
            StepOutcome::Fail(CellFailure::runtime("recovery fence raised during the run"))
        }
        _ => {
            collector.push(envelope);
            StepOutcome::Continue
        }
    }
}

/// Next envelope with the shared idle window; broadcast lag is accounted
/// in the collector and skipped, like every other live runner.
async fn next_envelope(
    receiver: &mut EventStream,
    collector: &mut Collector,
) -> Result<RuntimeEventEnvelope, CellFailure> {
    loop {
        match tokio::time::timeout(LIVE_IDLE, receiver.recv()).await {
            Err(_) => {
                return Err(CellFailure::new(
                    CellFailureClass::HarnessWatchdog,
                    "cell stalled waiting for runtime events",
                ));
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                collector.lagged = collector.lagged.saturating_add(skipped);
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err(CellFailure::runtime("event stream closed"));
            }
            Ok(Ok(envelope)) => return Ok(envelope),
        }
    }
}

/// Wait until the first durably settled workspace mutation has its durable
/// checkpoint, then stop the run the way an operator would: cancel the
/// in-flight turn, wait for the cancellation to settle, and return while
/// the runtime is idle so the checkpoint can be captured. The task
/// finishing before the interruption is a harness failure: there would be
/// nothing left to continue.
async fn wait_resume_trigger(
    receiver: &mut EventStream,
    collector: &mut Collector,
    state: &mut PhaseState,
    handle: &agent_runtime::RuntimeHandle,
) -> Result<(), CellFailure> {
    loop {
        let envelope = next_envelope(receiver, collector).await?;
        match step_event(envelope, collector, state) {
            StepOutcome::Continue => {}
            StepOutcome::ExpectedCancel => {
                // The turn settled from our cancel; pending tool cleanup may
                // still drain for a short window. Wait for true idleness.
                settle_after_cancel(receiver, collector, state).await;
                return Ok(());
            }
            StepOutcome::TaskCompleted => {
                return Err(CellFailure::harness_setup(
                    "task completed before the resume interruption could fire",
                ));
            }
            StepOutcome::TurnCompleted => {
                if state.durable_after_mutation {
                    // Natural turn boundary after the trigger: equally idle
                    // and equally resumable.
                    return Ok(());
                }
                return Err(CellFailure::harness_setup(
                    "turn finished before any durably settled workspace mutation",
                ));
            }
            StepOutcome::Fail(reason) => return Err(reason),
        }
        state.enforce_cap(handle);
        if state.durable_after_mutation && !state.cancel_requested {
            state.cancel_requested = true;
            let _ = handle.cancel_turn().await;
        }
    }
}

/// After an operator-style cancel, give the actor a bounded window to
/// finish explicit tool cleanup before the checkpoint capture: drain
/// events until a few quiet seconds pass.
async fn settle_after_cancel(
    receiver: &mut EventStream,
    collector: &mut Collector,
    state: &mut PhaseState,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let step = Duration::from_millis(500);
    let mut quiet = Duration::ZERO;
    while quiet < Duration::from_secs(4) && Instant::now() < deadline {
        match tokio::time::timeout(step, receiver.recv()).await {
            Err(_) => quiet += step,
            Ok(Err(_)) => return,
            Ok(Ok(envelope)) => {
                let _ = step_event(envelope, collector, state);
                quiet = Duration::ZERO;
            }
        }
    }
}

/// Drive the turn to its terminal outcome. TaskCompleted wins; otherwise
/// TurnCompleted starts one quiet-drain window that captures the final
/// durable checkpoint tail (and a late gated completion, which counts).
async fn run_to_completion(
    receiver: &mut EventStream,
    collector: &mut Collector,
    state: &mut PhaseState,
    handle: &agent_runtime::RuntimeHandle,
) -> Result<(), CellFailure> {
    loop {
        let envelope = next_envelope(receiver, collector).await?;
        match step_event(envelope, collector, state) {
            StepOutcome::Continue => {}
            StepOutcome::TaskCompleted => return Ok(()),
            StepOutcome::TurnCompleted => break,
            StepOutcome::ExpectedCancel => {
                return Err(CellFailure::runtime(
                    "unexpected operator-style cancel during a live run",
                ));
            }
            StepOutcome::Fail(reason) => return Err(reason),
        }
        state.enforce_cap(handle);
    }
    loop {
        match tokio::time::timeout(TURN_QUIET_GRACE, receiver.recv()).await {
            Err(_) => return Ok(()),
            Ok(Err(_)) => return Ok(()),
            Ok(Ok(envelope)) => match step_event(envelope, collector, state) {
                StepOutcome::TaskCompleted => return Ok(()),
                StepOutcome::ExpectedCancel => {
                    return Err(CellFailure::runtime(
                        "unexpected operator-style cancel during a live run",
                    ));
                }
                StepOutcome::Fail(reason) => return Err(reason),
                _ => {}
            },
        }
    }
}

async fn compose_cell(
    root: &Path,
    model: Arc<dyn ModelTransport>,
    engine: Arc<dyn ContextEngine>,
    switches: CellSwitches,
    pack: &PackSpec,
) -> anyhow::Result<agent_compose::ComposedRuntime> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    let verification_recipes = pilot_verification_recipes((pack.exact_recipe_inputs)());
    let tools: Arc<dyn ToolDispatcher> = Arc::new(
        tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            tool_runtime::ToolLifecycleConfig::default(),
            verification_recipes.clone(),
        ),
    );
    let composed = agent_compose::compose(agent_compose::ComposeConfig {
        workspace: workspace.clone(),
        context_engine: engine,
        model,
        approval: Arc::new(AllowAllGate),
        base_tools: tools,
        capability_aware: false,
        journal: None,
        // Durable checkpoints need the workspace as their store: without it
        // every scheduled write fails with "no checkpoint store configured"
        // and nothing is resumable.
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: Some(LIVE_MAX_MODEL_ROUNDS as usize),
        project_task_progress: true,
        project_completion_opportunity: switches.opportunity,
        recovery_surface: switches.recovery_surface,
        host_policies: Some(Arc::new(
            agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(
                &verification_recipes,
            )
            .map_err(anyhow::Error::msg)?,
        )),
        effect_reservation_journal: None,
    })
    .await?;
    composed.instance.start().await?;
    Ok(composed)
}

/// The C arm: production dynamic engine plus the model-backed compactor,
/// exactly like the mech/longflow live cells.
fn c_engine(model: Arc<dyn ModelTransport>) -> Arc<dyn ContextEngine> {
    let engine =
        context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig::default());
    Arc::new(engine.with_compactor(Arc::new(agent_compose::ModelBackedCompactor::new(model))))
}

/// The pilot composition root's verifier set: discovery mirrored for this
/// Cargo fixture (the general runner stays TaskScoped exactly as
/// `VerificationRecipes::discover` would produce it) plus one host-registered
/// source-read-only ExactCurrentWorld recipe. Without the registered exact
/// recipe no live `verify.run` PASS can ever carry an identity, and the
/// opportunity candidate's fail-closed precondition is unreachable (see
/// opportunity-gate REPORT, attempt 1). The set is identical across off/on
/// arms; the switch remains the only paired variable.
fn pilot_verification_recipes(exact_inputs: Vec<String>) -> tool_runtime::VerificationRecipes {
    let mut recipes = Vec::new();
    // Mirror of discovery for a Cargo workspace fixture.
    if let Ok(recipe) = tool_runtime::VerificationRecipe::new(
        "rust.workspace",
        "Run all Cargo workspace tests",
        "cargo-workspace-v1",
        vec!["cargo".into(), "test".into(), "--workspace".into()],
    ) {
        recipes.push(recipe);
    }
    // Host opt-in: the fixture's tests are pure unit tests whose writes stay
    // inside target/, so the source-read-only assertion holds. Declared
    // inputs are exactly the seed-guaranteed files; content changes create a
    // new exact world.
    if !exact_inputs.is_empty() {
        let exact = tool_runtime::VerificationRecipe::new(
            "jobrunner.exact",
            "Exact-world jobrunner test suite (source-read-only)",
            "jobrunner-exact-v1",
            vec!["cargo".into(), "test".into(), "--workspace".into()],
        )
        .and_then(|recipe| {
            recipe
                .with_exact_current_world_reuse()
                .with_exact_inputs(exact_inputs)
        })
        .expect("pilot exact recipe is valid");
        recipes.push(exact);
    }
    tool_runtime::VerificationRecipes::new(recipes).expect("pilot recipe set is valid")
}

/// Cold boundary: read the acknowledged safe-point artifact from the
/// workspace store, verify its envelope checksum, and deserialize it. No
/// phase-one in-memory state crosses with it.
async fn load_checkpoint_artifact(
    root: &Path,
    artifact: &str,
) -> anyhow::Result<(agent_runtime::RuntimeCheckpoint, String)> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    let store =
        agent_runtime::checkpoint::CheckpointStore::new(workspace.state_dir().join("checkpoints"));
    let payload = store
        .load_verified(artifact)
        .await
        .map_err(anyhow::Error::msg)?;
    let checksum = Sha256::digest(&payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((serde_json::from_slice(&payload)?, checksum))
}

/// Run one live cell end to end and score it against the finished
/// workspace. Never panics on provider/runtime failures: failures become
/// `CellOutcome::error` so evidence records stay honest.
pub async fn run_cell(
    mode: PilotMode,
    pair: &PairSink,
    model: Arc<dyn ModelTransport>,
    root: &Path,
    switches: CellSwitches,
) -> anyhow::Result<CellOutcome> {
    run_pack_cell(
        &retry_pack(),
        mode,
        pair,
        model,
        root,
        switches,
        AcceptanceProfile::ClosureRequired,
    )
    .await
}

/// Pack-generic live cell. Every retry-specific seam (seed, directive,
/// hidden checks, oracle, exact recipe, diff baseline) routes through the
/// [`PackSpec`], so a new pack cannot silently drift the harness.
pub async fn run_pack_cell(
    pack: &PackSpec,
    mode: PilotMode,
    pair: &PairSink,
    model: Arc<dyn ModelTransport>,
    root: &Path,
    switches: CellSwitches,
    acceptance_profile: AcceptanceProfile,
) -> anyhow::Result<CellOutcome> {
    let started = Instant::now();
    let collector = Collector::default();
    let CellSwitches {
        opportunity,
        recovery_surface,
    } = switches;
    let failed = |failure: CellFailure| {
        CellOutcome::failed(
            pack.id,
            mode,
            acceptance_profile,
            switches,
            started.elapsed().as_millis() as u64,
            failure,
        )
    };

    if let Err(e) = (pack.seed)(root) {
        let outcome = failed(CellFailure::harness_setup(format!("seeding failed: {e:#}")));
        write_evidence(pair, root, &collector, pack, &outcome, None, None)?;
        return Ok(outcome);
    }
    if let Err(e) = crate::suite::ensure_workspace_git(root) {
        let outcome = failed(CellFailure::harness_setup(format!(
            "workspace git init failed: {e:#}"
        )));
        write_evidence(pair, root, &collector, pack, &outcome, None, None)?;
        return Ok(outcome);
    }

    let mut collector = collector;
    let mut failure: Option<CellFailure> = None;
    let mut trigger_tool: Option<&'static str> = None;
    let mut resume_committed = 0u64;
    let mut checkpoint_durable = 0u64;
    let mut rounds_one = 0u32;
    let mut rounds_two = 0u32;
    let mut task_completed = false;
    let mut turn_completed = false;
    let mut exact_resume_tuple_matched = false;
    let mut phase_two_restored = false;
    let mut phase_two_continued = false;
    let mut opportunity_offers = Vec::new();
    let mut opportunity_called = false;

    // ---- Phase one: drive the directive from the clean seed. The engine
    // instance is per-phase; the resume twin must not inherit any
    // phase-one in-memory state through a shared object.
    let mut checkpoint_artifact: Option<String> = None;
    let mut checkpoint_sequence: Option<u64> = None;
    let mut checkpoint_checksum: Option<String> = None;
    let mut checkpoint_capability_generation: Option<u64> = None;
    let mut checkpoint_task_id: Option<TaskId> = None;
    match compose_cell(
        root,
        model.clone(),
        c_engine(model.clone()),
        switches,
        pack,
    )
    .await
    {
        Err(e) => {
            failure = Some(CellFailure::harness_setup(format!(
                "phase-one compose failed: {e:#}"
            )))
        }
        Ok(composed) => {
            let handle = composed.handle().clone();
            let mut events = composed.subscribe();
            let mut state = PhaseState::default();
            let drive: Result<(), CellFailure> = async {
                let directive = (pack.directive)();
                handle
                    .set_focus(directive.to_string())
                    .await
                    .map_err(|e| CellFailure::runtime(format!("set_focus failed: {e}")))?;
                handle
                    .user_message(directive.to_string())
                    .await
                    .map_err(|e| CellFailure::runtime(format!("user_message failed: {e}")))?;
                match mode {
                    PilotMode::Normal => {
                        run_to_completion(&mut events, &mut collector, &mut state, &handle).await?
                    }
                    PilotMode::Resume => {
                        wait_resume_trigger(&mut events, &mut collector, &mut state, &handle)
                            .await?;
                    }
                }
                Ok(())
            }
            .await;
            rounds_one = state.model_rounds;
            resume_committed += state.resume_committed;
            checkpoint_durable += state.checkpoint_durable;
            task_completed |= state.task_completed;
            turn_completed |= state.turn_completed;
            opportunity_offers.extend(state.opportunity_offers.iter().cloned());
            opportunity_called |= state.opportunity_called;
            trigger_tool = state.mutation_tool;
            match drive {
                Ok(()) => {
                    if mode == PilotMode::Resume {
                        // Retain the exact acknowledged tuple across the
                        // boundary: phase two loads and verifies the exact
                        // acknowledged safe-point artifact from disk.
                        match (
                            state.last_durable_artifact.clone(),
                            state.last_durable_sequence,
                            state.last_durable_checksum.clone(),
                            state.last_durable_capability_generation,
                            state.last_resume_task_id,
                        ) {
                            (
                                Some(artifact),
                                Some(seq),
                                Some(sum),
                                Some(generation),
                                Some(task_id),
                            ) => {
                                checkpoint_artifact = Some(artifact);
                                checkpoint_sequence = Some(seq);
                                checkpoint_checksum = Some(sum);
                                checkpoint_capability_generation = Some(generation);
                                checkpoint_task_id = Some(task_id);
                            }
                            _ => {
                                failure = Some(CellFailure::runtime(
                                    "trigger fired without an acknowledged checkpoint tuple",
                                ));
                            }
                        }
                    }
                }
                Err(reason) => failure = Some(reason.context("phase one failed")),
            }
            if let Err(e) = composed.shutdown().await
                && failure.is_none()
            {
                failure = Some(CellFailure::runtime(format!(
                    "phase-one shutdown failed: {e}"
                )));
            }
            while let Ok(envelope) = events.try_recv() {
                collector.push(envelope);
            }
        }
    }

    // ---- Phase two (resume only): cold-load the acknowledged artifact
    // into a fresh runtime and continue the SAME directive.
    if mode == PilotMode::Resume && failure.is_none() {
        let loaded = match &checkpoint_artifact {
            None => Err(anyhow::anyhow!(
                "resume mode reached phase two without a checkpoint"
            )),
            Some(artifact) => load_checkpoint_artifact(root, artifact).await,
        };
        match loaded {
            Err(e) => {
                failure = Some(CellFailure::runtime(format!(
                    "checkpoint artifact load failed: {e:#}"
                )))
            }
            Ok((checkpoint, loaded_checksum)) => {
                if checkpoint_sequence != Some(checkpoint.snapshot_sequence) {
                    failure = Some(CellFailure::runtime(format!(
                        "checkpoint sequence mismatch: expected {:?}, got {}",
                        checkpoint_sequence, checkpoint.snapshot_sequence
                    )));
                } else if checkpoint_checksum.as_deref() != Some(loaded_checksum.as_str()) {
                    failure = Some(CellFailure::runtime(
                        "checkpoint acknowledgement checksum does not match the loaded payload",
                    ));
                } else if checkpoint_capability_generation != Some(checkpoint.capability_generation)
                {
                    failure = Some(CellFailure::runtime(format!(
                        "checkpoint capability generation mismatch: expected {:?}, got {}",
                        checkpoint_capability_generation, checkpoint.capability_generation
                    )));
                } else if checkpoint_task_id != checkpoint.current_task_id {
                    failure = Some(CellFailure::runtime(format!(
                        "checkpoint task mismatch: expected {:?}, got {:?}",
                        checkpoint_task_id, checkpoint.current_task_id
                    )));
                } else if checkpoint.authority.is_none() {
                    failure = Some(CellFailure::runtime(
                        "checkpoint has no durable Core authority lineage",
                    ));
                }
                if failure.is_some() {
                    // Acknowledgement mismatch is a Runtime truth-chain failure,
                    // not a restore attempt.
                } else {
                    match compose_cell(
                        root,
                        model.clone(),
                        c_engine(model.clone()),
                        switches,
                        pack,
                    )
                    .await
                    {
                        Err(e) => {
                            failure = Some(CellFailure::harness_setup(format!(
                                "phase-two compose failed: {e:#}"
                            )))
                        }
                        Ok(composed) => {
                            let handle = composed.handle().clone();
                            let mut events = composed.subscribe();
                            let mut state = PhaseState::default();
                            let drive: Result<(), CellFailure> = async {
                                composed.instance.restore(checkpoint).await.map_err(|e| {
                                    CellFailure::runtime(format!("restore failed: {e}"))
                                })?;
                                phase_two_restored = true;
                                // Restore verifies the checkpoint's authority marker
                                // against the live Core lineage; all other acknowledged
                                // tuple fields were matched above.
                                exact_resume_tuple_matched = true;
                                handle.continue_active_task().await.map_err(|e| {
                                    CellFailure::runtime(format!(
                                        "continue_active_task failed: {e}"
                                    ))
                                })?;
                                phase_two_continued = true;
                                run_to_completion(&mut events, &mut collector, &mut state, &handle)
                                    .await
                            }
                            .await;
                            rounds_two = state.model_rounds;
                            resume_committed += state.resume_committed;
                            checkpoint_durable += state.checkpoint_durable;
                            task_completed |= state.task_completed;
                            turn_completed |= state.turn_completed;
                            opportunity_offers.extend(state.opportunity_offers.iter().cloned());
                            opportunity_called |= state.opportunity_called;
                            match drive {
                                Ok(()) => {}
                                Err(reason) => {
                                    failure = Some(reason.context("phase two failed"));
                                }
                            }
                            if let Err(e) = composed.shutdown().await
                                && failure.is_none()
                            {
                                failure = Some(CellFailure::runtime(format!(
                                    "phase-two shutdown failed: {e}"
                                )));
                            }
                            while let Ok(envelope) = events.try_recv() {
                                collector.push(envelope);
                            }
                        }
                    }
                }
            }
        }
    }

    let wall_ms = started.elapsed().as_millis() as u64;

    // ---- Outcome dimensions, recorded independently. Read-only
    // acceptance always runs while the workspace is inspectable — a
    // missing closure or a late provider failure must not erase whether
    // the implementation was behaviorally correct. The diff scan runs
    // before the oracle injection so build artifacts and the harness's
    // own oracle file can never enter the allowed-diff verdict.
    let provider_failed = failure
        .as_ref()
        .is_some_and(|failure| failure.class == CellFailureClass::ProviderTransport);
    let continuation = match mode {
        PilotMode::Normal => "n/a",
        PilotMode::Resume if phase_two_restored && phase_two_continued => "restored_and_continued",
        PilotMode::Resume if phase_two_restored => "restored_not_continued",
        _ => "failed",
    }
    .to_string();

    let diff_seed_files = (pack.seed_files)();
    let diff_violations =
        match diff_violations(root, &diff_seed_files.to_vec(), pack.allowed_diff.as_ref()) {
            Ok(violations) => violations,
            Err(e) => {
                if failure.is_none() {
                    failure = Some(CellFailure::harness_setup(format!(
                        "allowed-diff scan failed: {e:#}"
                    )));
                }
                vec![format!("diff scan failed: {e:#}")]
            }
        };
    let diff_clean = diff_violations.is_empty();
    let marker_violations = (pack.hidden_violations)(root);

    // Agent-authored self-check over the whole workspace, reported but
    // never gating: run before oracle injection so the injected oracle
    // cannot be executed as part of the self-check.
    let self_check_record = run_cargo_test(root, &["--quiet"]).await;
    // Harness-owned behavioral oracle: ensure parent directory exists
    // before injection, then run as an isolated integration target.
    let (oracle_name, oracle_source) = (pack.oracle)();
    let oracle_path = root.join("tests").join(format!("{oracle_name}.rs"));
    let oracle_record = match std::fs::create_dir_all(oracle_path.parent().unwrap()) {
        Err(e) => HiddenCommandResult {
            argv: vec![
                "cargo".into(),
                "test".into(),
                "--test".into(),
                oracle_name.into(),
            ],
            expect_exit: 0,
            exit: None,
            timed_out: false,
            stdout: String::new(),
            stderr: format!("oracle setup failed: {e}"),
            stdout_truncated: false,
            stderr_truncated: false,
            passed: false,
        },
        Ok(()) => match std::fs::write(&oracle_path, oracle_source) {
            Err(e) => HiddenCommandResult {
                argv: vec![
                    "cargo".into(),
                    "test".into(),
                    "--test".into(),
                    oracle_name.into(),
                ],
                expect_exit: 0,
                exit: None,
                timed_out: false,
                stdout: String::new(),
                stderr: format!("oracle injection failed: {e}"),
                stdout_truncated: false,
                stderr_truncated: false,
                passed: false,
            },
            Ok(()) => run_cargo_test(root, &["--test", oracle_name]).await,
        },
    };
    let _ = std::fs::remove_file(&oracle_path);
    let oracle_failure = if oracle_record.stderr.contains("oracle setup failed")
        || oracle_record.stderr.contains("oracle injection failed")
        || oracle_record.stderr.contains("failed to spawn cargo test")
        || oracle_record.stderr.contains("executed zero tests")
    {
        Some(CellFailure::harness_setup(
            "behavioral oracle could not be prepared or started",
        ))
    } else if oracle_record.timed_out {
        Some(CellFailure::new(
            CellFailureClass::HarnessWatchdog,
            "behavioral oracle exceeded its bounded timeout",
        ))
    } else {
        None
    };
    let observed_behavior = if oracle_record.passed {
        "pass"
    } else if oracle_failure.is_some() {
        "not_run"
    } else {
        "fail"
    }
    .to_string();
    if failure.is_none() {
        failure = oracle_failure;
    }
    let behavior = if failure
        .as_ref()
        .is_some_and(|failure| failure.class.not_run())
    {
        "not_run".to_string()
    } else {
        observed_behavior.clone()
    };
    let self_check = if self_check_record.passed {
        "pass"
    } else if failure.is_some() && !workspace_has_tests(root) {
        "not_run"
    } else {
        "fail"
    };

    let error_class = failure.as_ref().map(|failure| failure.class);
    let error_retryable = failure.as_ref().is_some_and(|failure| failure.retryable);
    let verdict = evaluate_verdict(
        acceptance_profile,
        mode,
        error_class,
        &behavior,
        diff_clean,
        exact_resume_tuple_matched,
        phase_two_restored,
        phase_two_continued,
        task_completed,
    );
    let passed = verdict == CellVerdict::Pass;
    let error = failure.as_ref().map(|failure| failure.message.clone());
    let closure = if task_completed {
        "completed"
    } else if failure.is_some() {
        "failed"
    } else {
        "active"
    };
    // Event-derived settlement exposure, computed once here and reused by
    // the outcome line and the evidence summary; `write_evidence` performs
    // its own aggregation for the full cell bundle.
    let metrics = crate::metrics::aggregate_metrics(&collector.events);
    let outcome = CellOutcome {
        pack_id: pack.id,
        mode,
        acceptance_profile,
        verdict,
        passed,
        error,
        error_class,
        error_retryable,
        wall_ms,
        behavior,
        observed_behavior,
        diff: if diff_clean { "pass" } else { "fail" }.into(),
        closure: closure.into(),
        continuation,
        exact_resume_tuple_matched,
        restored: phase_two_restored,
        continued: phase_two_continued,
        turn_completed,
        task_completed,
        provider_health: if provider_failed {
            "transport_failed".into()
        } else {
            "healthy".into()
        },
        self_check: self_check.into(),
        resume_committed,
        checkpoint_durable,
        model_rounds_phase_one: rounds_one,
        model_rounds_phase_two: rounds_two,
        resume_trigger: trigger_tool,
        diff_violations,
        marker_violations,
        opportunity,
        recovery_surface,
        opportunity_offers,
        opportunity_called,
        settlement_seen: metrics.settled_seen,
        settlement_episodes: metrics.settlement_episodes,
        settlement_episode_rounds: metrics.settlement_episode_rounds,
        settlement_episode_calls: metrics.settlement_episode_calls,
        settlement_episode_failures: metrics.settlement_episode_failures,
    };
    write_evidence(
        pair,
        root,
        &collector,
        pack,
        &outcome,
        Some(&oracle_record),
        Some(&self_check_record),
    )?;
    Ok(outcome)
}

/// Whether the agent left any test of its own behind (decides whether a
/// failed workspace self-check is `fail` or `not_run`).
fn workspace_has_tests(root: &Path) -> bool {
    if list_tests_dir(root).is_empty() {
        return false;
    }
    let source_files = [
        "src/lib.rs",
        "src/error.rs",
        "src/config.rs",
        "src/sleeper.rs",
    ];
    source_files.iter().any(|file| {
        std::fs::read_to_string(root.join(file))
            .map(|body| body.contains("#[cfg(test)]") || body.contains("#[test]"))
            .unwrap_or(false)
    })
}

/// Serialize the cell into the claimed pair directory using the shared
/// evidence conventions (manifest + events.jsonl + hidden report) plus
/// this pilot's per-dimension record.
fn write_evidence(
    pair: &PairSink,
    root: &Path,
    collector: &Collector,
    pack: &PackSpec,
    outcome: &CellOutcome,
    oracle: Option<&HiddenCommandResult>,
    self_check: Option<&HiddenCommandResult>,
) -> anyhow::Result<()> {
    let report = build_hidden_report(outcome, root, pack, oracle, self_check);
    let metrics = crate::metrics::aggregate_metrics(&collector.events);
    let cell_dir = pair.cell_dir("dynamic");
    crate::bundle::write_cell_parts(
        &cell_dir,
        pack.id,
        &(pack.identity_sha256)(),
        "dynamic",
        pair,
        &collector.events,
        &metrics,
        outcome.passed,
        outcome.wall_ms,
        outcome.error.as_deref(),
        root,
        collector.lagged,
        0,
        Some("production"),
        &report,
    )?;
    let dimensions = serde_json::json!({
        "schema": PILOT_SCHEMA,
        "pack_id": pack.id,
        "mode": outcome.mode.id(),
        "acceptance_profile": outcome.acceptance_profile.id(),
        "verdict": outcome.verdict.id(),
        "behavioral_oracle": outcome.behavior,
        "observed_behavioral_oracle": outcome.observed_behavior,
        "allowed_diff": outcome.diff,
        "task_closure": outcome.closure,
        "continuation": outcome.continuation,
        "exact_resume_tuple_matched": outcome.exact_resume_tuple_matched,
        "restored": outcome.restored,
        "continued": outcome.continued,
        "turn_completed": outcome.turn_completed,
        "task_completed": outcome.task_completed,
        "wall_ms": outcome.wall_ms,
        "model_rounds_phase_one": outcome.model_rounds_phase_one,
        "model_rounds_phase_two": outcome.model_rounds_phase_two,
        "resume_committed": outcome.resume_committed,
        "checkpoint_durable": outcome.checkpoint_durable,
        "provider_runtime": outcome.provider_health,
        "workspace_self_check": outcome.self_check,
        "final_passed": outcome.passed,
        "runtime_error": outcome.error,
        "runtime_error_class": outcome.error_class,
        "runtime_error_retryable": outcome.error_retryable,
        // Item-8 candidate bookkeeping: the switch setting and the
        // per-cell opportunity account (offers per key, call-through).
        "completion_opportunity": if outcome.opportunity { "on" } else { "off" },
        "opportunity_offers": outcome.opportunity_offers,
        "opportunity_called": outcome.opportunity_called,
        // Directory-tool admission gate bookkeeping: the candidate switch
        // is the only variable between the two paired arms.
        "recovery_surface": if outcome.recovery_surface { "on" } else { "off" },
    });
    let dimensions_path = pair.cell_dir("dynamic").join("dimensions.json");
    std::fs::write(&dimensions_path, serde_json::to_vec_pretty(&dimensions)?)?;
    Ok(())
}

fn build_hidden_report(
    outcome: &CellOutcome,
    root: &Path,
    pack: &PackSpec,
    oracle: Option<&HiddenCommandResult>,
    self_check: Option<&HiddenCommandResult>,
) -> HiddenReport {
    let checks = (pack.hidden_results)(root);
    let assertions: Vec<HiddenAssertionResult> = checks
        .iter()
        .map(|(path, name, passed)| HiddenAssertionResult {
            path: (*path).to_string(),
            pred: (*name).to_string(),
            needles: Vec::new(),
            min: None,
            count: None,
            passed: *passed,
            file_exists: root.join(path).exists(),
        })
        .collect();
    let mut paths: Vec<String> = (pack.seed_files)()
        .into_iter()
        .map(|path| path.to_string())
        .collect();
    let tests = list_tests_dir(root);
    paths.extend(tests);
    let files: Vec<HiddenFileBody> = paths
        .iter()
        .map(|relative| file_body(root, relative))
        .collect();
    // Both cargo invocations are recorded; the gating oracle first, then
    // the agent-authored workspace self-check.
    let mut commands: Vec<HiddenCommandResult> = Vec::new();
    match oracle {
        Some(result) => commands.push(result.clone()),
        None => commands.push(HiddenCommandResult {
            argv: cargo_argv(),
            expect_exit: 0,
            exit: None,
            timed_out: false,
            stdout: String::new(),
            stderr: "oracle not run".into(),
            stdout_truncated: false,
            stderr_truncated: false,
            passed: false,
        }),
    }
    if let Some(result) = self_check {
        let mut record = result.clone();
        record
            .stderr
            .push_str("\n(self-check over agent-authored tests; informational)");
        commands.push(record);
    }
    HiddenReport {
        schema: PILOT_SCHEMA.to_string(),
        kind: "retry_pilot_oracle".into(),
        fixture_id: format!("{}-{}", pack.id, outcome.mode.id()),
        expected_edit: String::new(),
        passed: outcome.passed,
        replay_complete: true,
        assertions,
        files,
        commands,
    }
}

fn cargo_argv() -> Vec<String> {
    ["cargo", "test", "--quiet"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Hidden cargo-test runner over the finished workspace. `args` selects
/// the target set: the harness-owned oracle runs as an isolated
/// integration test, the agent-authored self-check runs everything.
async fn run_cargo_test(root: &Path, args: &[&str]) -> HiddenCommandResult {
    use std::process::Stdio;

    let mut command = tokio::process::Command::new("cargo");
    command
        .arg("test")
        .args(args)
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null());
    let mut record = HiddenCommandResult {
        argv: std::iter::once("cargo".to_string())
            .chain(std::iter::once("test".to_string()))
            .chain(args.iter().map(|arg| (*arg).to_string()))
            .collect(),
        expect_exit: 0,
        exit: None,
        timed_out: false,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        passed: false,
    };
    match tokio::time::timeout(CARGO_TEST_TIMEOUT, command.output()).await {
        Err(_) => {
            record.timed_out = true;
            record.stderr = format!("cargo test did not finish within {CARGO_TEST_TIMEOUT:?}");
        }
        Ok(Err(e)) => record.stderr = format!("failed to spawn cargo test: {e}"),
        Ok(Ok(output)) => {
            record.exit = output.status.code();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            record.stdout_truncated = stdout.len() > COMMAND_CAPTURE_CAP;
            record.stderr_truncated = stderr.len() > COMMAND_CAPTURE_CAP;
            record.stdout = tail_capture(&stdout);
            record.stderr = tail_capture(&stderr);
            let combined = format!("{}\n{stdout}\n{stderr}", record.stdout);
            let (passed_tests, failed_tests) = parse_test_totals(&combined);
            record.passed = output.status.success() && failed_tests == 0 && passed_tests >= 1;
            if output.status.success() && passed_tests == 0 {
                record
                    .stderr
                    .push_str("\noracle: cargo test succeeded but executed zero tests");
            }
        }
    }
    record
}

fn tail_capture(text: &str) -> String {
    if text.len() <= COMMAND_CAPTURE_CAP {
        text.to_string()
    } else {
        let cut = text.len() - COMMAND_CAPTURE_CAP;
        let boundary = text[cut..]
            .find('\n')
            .map(|offset| cut + offset + 1)
            .unwrap_or(cut);
        format!("…{}", &text[boundary..])
    }
}

/// Sum the `N passed` / `N failed` counters of every `test result:` line
/// across all compiled targets. Token-pair scanning tolerates the `ok.` /
/// `FAILED.` prefixes cargo puts before the counters.
fn parse_test_totals(text: &str) -> (u32, u32) {
    let mut passed = 0u32;
    let mut failed = 0u32;
    for line in text.lines().filter(|line| line.contains("test result:")) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        for pair in fields.windows(2) {
            let Ok(count) = pair[0].parse::<u32>() else {
                continue;
            };
            match pair[1].trim_end_matches([',', ';', '.']) {
                "passed" => passed = passed.saturating_add(count),
                "failed" => failed = failed.saturating_add(count),
                _ => {}
            }
        }
    }
    (passed, failed)
}

/// Compare the finished workspace against the frozen seed. Changed seed
/// files are the assignment; deletions and out-of-bounds paths are not.
fn diff_violations(
    root: &Path,
    diff_seed_files: &[&str],
    allowed_diff: &dyn Fn(&str) -> bool,
) -> anyhow::Result<Vec<String>> {
    let mut present: BTreeMap<String, String> = BTreeMap::new();
    collect_files(root, PathBuf::new(), &mut present)?;
    let mut violations = Vec::new();
    for relative in present.keys() {
        if !allowed_diff(relative) {
            violations.push(format!("path outside the allowed diff: {relative}"));
        }
    }
    for relative in diff_seed_files {
        if !present.contains_key(*relative) {
            violations.push(format!("seed file deleted: {relative}"));
        }
    }
    violations.sort();
    violations.truncate(32);
    Ok(violations)
}

fn collect_files(
    directory: &Path,
    prefix: PathBuf,
    out: &mut BTreeMap<String, String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        out.len() < MAX_DIFF_FILES,
        "workspace holds more than {MAX_DIFF_FILES} files"
    );
    let mut entries: Vec<std::fs::DirEntry> =
        std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_files(&path, prefix.join(&name), out)?;
        } else if SKIP_FILES.contains(&name.as_str()) {
            continue;
        } else {
            let metadata = entry.metadata()?;
            if metadata.len() as usize > MAX_FILE_BYTES {
                anyhow::bail!("file {} exceeds the size cap", prefix.join(&name).display());
            }
            let bytes = std::fs::read(&path)?;
            // Normalize to forward slashes so the policy and seed lookups
            // are identical across host path separators.
            let relative = prefix.join(&name).to_string_lossy().replace('\\', "/");
            out.insert(relative, sha256_hex(&bytes));
        }
    }
    Ok(())
}

/// Bounded body records for the evidence bundle: finals plus added tests.
fn list_tests_dir(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("tests")) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(format!(
                "tests/{}",
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
        }
        if found.len() >= 8 {
            break;
        }
    }
    found.sort();
    found
}

fn file_body(root: &Path, relative: &str) -> HiddenFileBody {
    let path = root.join(relative);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let truncated = bytes.len() > FILE_BODY_CAP;
            let visible = if truncated {
                &bytes[..FILE_BODY_CAP]
            } else {
                &bytes[..]
            };
            HiddenFileBody {
                path: relative.to_string(),
                exists: true,
                sha256: sha256_hex(&bytes),
                bytes: bytes.len(),
                truncated,
                body: String::from_utf8_lossy(visible).into_owned(),
            }
        }
        Err(_) => HiddenFileBody {
            path: relative.to_string(),
            exists: false,
            sha256: String::new(),
            bytes: 0,
            truncated: false,
            body: String::new(),
        },
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Stable identity of directive + seed contents for the manifest.
pub fn spec_sha256() -> String {
    let mut hasher = Sha256::new();
    hasher.update(DIRECTIVE.as_bytes());
    hasher.update(b"\n");
    for (relative, contents) in FIXTURE_FILES {
        hasher.update(relative.as_bytes());
        hasher.update(b"\n");
        hasher.update(contents.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-flight for the opportunity gate: the registered exact recipe
    /// must produce non-empty identity material in this environment. If
    /// capture degrades to TaskScoped (env caps, missing inputs, unresolvable
    /// executable), live eligibility can never arm and the gate would waste
    /// its paired cells measuring nothing again.
    #[tokio::test]
    async fn registered_exact_recipe_produces_identity_material() {
        let dir = tempfile::tempdir().unwrap();
        long_task::seed_workspace(dir.path()).unwrap();
        let workspace = agent_workspace::Workspace::open(dir.path()).await.unwrap();
        let dispatcher = tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace,
            tool_runtime::ToolLifecycleConfig::default(),
            pilot_verification_recipes(vec![
                "Cargo.toml".into(),
                "src/config.rs".into(),
                "src/error.rs".into(),
                "src/lib.rs".into(),
            ]),
        );
        use agent_contracts::{ToolCall, ToolDispatcher as _, ToolExecutionPurpose};
        let call = ToolCall {
            id: "preflight".into(),
            name: "verify.run".into(),
            arguments: serde_json::json!({"recipe_id": "jobrunner.exact"}),
        };
        let attribution = dispatcher.execution_attribution(&call);
        assert_eq!(attribution.purpose, ToolExecutionPurpose::Verify);
        assert!(
            attribution.reusable_verification(),
            "the exact recipe must stay reusable"
        );
        assert!(
            !attribution.verification_identity.is_empty(),
            "identity capture must succeed in this environment; \
             the opportunity gate cannot arm without it"
        );
    }

    #[test]
    fn mode_parse_covers_both_single_and_all() {
        assert_eq!(
            PilotMode::parse(None).unwrap(),
            vec![PilotMode::Normal, PilotMode::Resume]
        );
        assert_eq!(
            PilotMode::parse(Some("normal")).unwrap(),
            vec![PilotMode::Normal]
        );
        assert_eq!(
            PilotMode::parse(Some("resume")).unwrap(),
            vec![PilotMode::Resume]
        );
        assert!(PilotMode::parse(Some("both")).is_err());
    }

    #[test]
    fn cargo_totals_sum_across_targets_and_ignore_noise() {
        let text = "test result: ok. 3 passed; 0 failed; 0 ignored\n\
                    running 1 test\n\
                    test result: FAILED. 1 passed; 2 failed; 0 ignored\n\
                    Doc-tests jobrunner\n\
                    test result: ok. 4 passed; 0 failed; 7 ignored";
        assert_eq!(parse_test_totals(text), (8, 2));
        assert_eq!(parse_test_totals("nothing here"), (0, 0));
    }

    #[test]
    fn diff_rules_accept_edits_and_reject_deletions_and_strays() {
        // Pure function rules are exercised through seed comparison in the
        // deterministic gate; here only the path policy is pinned.
        let allowed = ["src/lib.rs", "tests/retry.rs", "README.md", "Cargo.toml"];
        for path in allowed {
            assert!(
                path.starts_with("src/")
                    || path.starts_with("tests/")
                    || path == "README.md"
                    || path == "Cargo.toml",
                "{path} must stay allowed"
            );
        }
    }

    #[test]
    fn spec_digest_is_stable_across_calls() {
        assert_eq!(spec_sha256(), spec_sha256());
        assert_eq!(spec_sha256().len(), 64);
    }

    #[test]
    fn m15_verdict_does_not_require_lifecycle_closure() {
        assert_eq!(
            evaluate_verdict(
                AcceptanceProfile::M15V1,
                PilotMode::Normal,
                None,
                "pass",
                true,
                false,
                false,
                false,
                false,
            ),
            CellVerdict::Pass
        );
        assert_eq!(
            evaluate_verdict(
                AcceptanceProfile::ClosureRequired,
                PilotMode::Normal,
                None,
                "pass",
                true,
                false,
                false,
                false,
                false,
            ),
            CellVerdict::Fail
        );
    }

    #[test]
    fn m15_resume_requires_the_exact_restored_continuation_chain() {
        let verdict = |exact, restored, continued| {
            evaluate_verdict(
                AcceptanceProfile::M15V1,
                PilotMode::Resume,
                None,
                "pass",
                true,
                exact,
                restored,
                continued,
                false,
            )
        };
        assert_eq!(verdict(true, true, true), CellVerdict::Pass);
        assert_eq!(verdict(false, true, true), CellVerdict::Fail);
        assert_eq!(verdict(true, false, true), CellVerdict::Fail);
        assert_eq!(verdict(true, true, false), CellVerdict::Fail);
    }

    #[test]
    fn provider_outage_is_not_run_but_output_limit_is_a_cell_failure() {
        let verdict = |class| {
            evaluate_verdict(
                AcceptanceProfile::M15V1,
                PilotMode::Normal,
                Some(class),
                "not_run",
                true,
                false,
                false,
                false,
                false,
            )
        };
        assert_eq!(
            verdict(CellFailureClass::ProviderTransport),
            CellVerdict::NotRun
        );
        assert_eq!(
            verdict(CellFailureClass::ModelOutputLimit),
            CellVerdict::Fail
        );
    }

    /// The harness-owned oracle must accept the reference solution and
    /// reject the untouched seed, offline and in isolation.
    #[tokio::test]
    async fn oracle_accepts_reference_solution_and_rejects_seed() {
        let dir = tempfile::tempdir().unwrap();
        long_task::seed_workspace(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        let oracle_path = dir
            .path()
            .join("tests")
            .join(format!("{ORACLE_TEST_NAME}.rs"));
        std::fs::write(&oracle_path, ORACLE_TEST_SOURCE).unwrap();

        let seed_result = run_cargo_test(dir.path(), &["--test", ORACLE_TEST_NAME]).await;
        assert!(
            !seed_result.passed,
            "the seed has no retry policy; the oracle must fail on it"
        );

        for (relative, contents) in long_task::FINAL_FILES {
            std::fs::write(dir.path().join(relative), contents).unwrap();
        }
        let result = run_cargo_test(dir.path(), &["--test", ORACLE_TEST_NAME]).await;
        assert!(
            result.passed,
            "reference solution must pass the oracle\nstdout:\n{}\nstderr:\n{}",
            result.stdout, result.stderr
        );
    }
}
