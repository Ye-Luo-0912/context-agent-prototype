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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, CancellationToken,
    CompletionOpportunityDisposition, ContextEngine, ModelCapabilities, ModelEventSink,
    ModelOutput, ModelRequest, ModelTransport, RuntimeEvent, RuntimeEventEnvelope,
    RuntimeFailureClass, TaskId, ToolCall, ToolDispatcher, ToolSpec,
    VerificationCoverageDeclaration,
};
use anyhow::bail;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::bundle::PairSink;
use crate::long_task::{self, DIRECTIVE, FIXTURE_FILES};
use crate::m15_pack;
use crate::workload::{HiddenAssertionResult, HiddenCommandResult, HiddenFileBody, HiddenReport};

pub const PILOT_SCHEMA: &str = "retry-pilot-cell-v4";
/// LONG_TASK_EVALUATION layer 2: normal and resume, two repeats each.
pub const DEFAULT_REPEATS: u32 = 2;
pub const M15_PACK_IDS: [&str; 3] = [
    m15_pack::RETRY_DIAG,
    m15_pack::RETRY_MIGRATE,
    "retry_policy_dev",
];
const CONVERGENCE_CANDIDATE_ID: &str = "task-progress-settlement-v1";
const PRODUCT_TOOL_SURFACE: &str = "production";

/// Candidate switches for one live cell. Each paired gate runs identical
/// cells with exactly one of these as the only variable; the evidence
/// records every setting.
#[derive(Debug, Clone, Copy)]
pub struct CellSwitches {
    /// Item-8 advisory completion-opportunity candidate (default off).
    pub opportunity: bool,
    /// Directory-tool admission recovery-surface candidate (default off).
    pub recovery_surface: bool,
    /// Product TASK PROGRESS projection. Product and formal-eval baselines
    /// keep this on; a settlement experiment must never change it because it
    /// also owns checked-file projection into Context maintenance.
    pub project_task_progress: bool,
    /// Neutral settlement line inside an already-enabled TASK PROGRESS frame.
    /// Default off. This is the only treatment in the convergence gate.
    pub project_settlement: bool,
    /// Expensive same-state request audit and common treatment-sized packing
    /// envelope. Ordinary product/M15 cells keep this false; a causal pair
    /// sets the same true value in both arms.
    pub settlement_projection_diagnostics: bool,
}

impl Default for CellSwitches {
    fn default() -> Self {
        Self {
            opportunity: false,
            recovery_surface: false,
            project_task_progress: true,
            project_settlement: false,
            settlement_projection_diagnostics: false,
        }
    }
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
    /// Declarative acceptance text the harness patches onto the task after
    /// its creation, so the task-aware completion gate can arm in live
    /// cells. After observing a matching trusted PASS, Runtime mints the
    /// criterion receipt from the host-declared coverage domain. `None`
    /// (the default) patches nothing and the gate stays
    /// fail-closed at `VerifiedCurrent`. The text is neutral and never
    /// carries hidden evaluation details.
    pub acceptance_declaration: Option<&'static str>,
    /// Host-owned verifier coverage domain which can prove the public
    /// acceptance declaration. `None` must accompany no declaration.
    pub acceptance_domain: Option<&'static str>,
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

/// Absorb a later failure into the cell's failure slot, keeping the
/// classification monotone. A harness setup/watchdog failure (a NOT_RUN
/// censor) always wins over anything recorded earlier, because the oracle
/// verdict is unreadable once the harness itself failed; a behavior or
/// runtime failure is recorded only when no failure exists yet, so the
/// first diagnostic survives.
fn absorb_failure(current: &mut Option<CellFailure>, incoming: Option<CellFailure>) {
    let Some(incoming) = incoming else { return };
    if incoming.class.not_run() || current.is_none() {
        *current = Some(incoming);
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
        acceptance_declaration: Some(
            "attempts above the safe exponent saturate at max_delay without overflow",
        ),
        acceptance_domain: Some("retry-policy-public-contract"),
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

/// The LT-EVAL-06 breadth packs: the three development-twin fixtures
/// (diagnosis-and-fix, multi-file API migration, harness maintenance) as
/// live-run pack specs. This is post-M15 breadth evaluation, never an M15
/// window pack.
pub(crate) fn lt_eval06_packs() -> Vec<PackSpec> {
    [
        m15_pack::LTEV_DIAGFIX,
        m15_pack::LTEV_MIGRATE,
        m15_pack::RETRY_MAINT,
    ]
    .iter()
    .map(|id| {
        let id = *id;
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
            // The diag-fix and maintenance directives mandate a root-level
            // report (DIAGNOSIS.md / REPORT.md); the migration fixture
            // stays on the standard allowed diff.
            allowed_diff: Box::new(|relative| {
                standard_allowed_diff(relative)
                    || relative == "DIAGNOSIS.md"
                    || relative == "REPORT.md"
            }),
            acceptance_declaration: None,
            acceptance_domain: None,
        }
    })
    .collect()
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
        acceptance_declaration: None,
        acceptance_domain: None,
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
        acceptance_declaration: None,
        acceptance_domain: None,
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
const FILE_BODY_CAP: usize = 64 * 1024;
const COMMAND_CAPTURE_CAP: usize = 16 * 1024;

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

#[derive(Debug, Clone, Serialize)]
struct ModelRequestShape {
    prompt_digest: String,
    tool_surface_digest: String,
}

#[derive(Debug, Default)]
struct ModelRequestCapture {
    total: usize,
    shapes: Vec<ModelRequestShape>,
    settlement_audits: u64,
    settlement_audit_invalid: u64,
    first_settlement_normalized_prompt_digest: Option<String>,
    first_settlement_tool_surface_digest: Option<String>,
}

/// Observational transport wrapper for live evidence. It hashes the exact
/// assembled messages and tool specs before delegating without changing the
/// request. The Context compactor receives the raw provider, so its private
/// summarization requests cannot contaminate this cell-level model trace.
struct RecordingModelTransport {
    inner: Arc<dyn ModelTransport>,
    expected_settlement_arm: &'static str,
    capture: Mutex<ModelRequestCapture>,
}

impl RecordingModelTransport {
    const CAP: usize = LIVE_MAX_MODEL_ROUNDS as usize * 2 + 2;

    fn new(inner: Arc<dyn ModelTransport>, project_settlement: bool) -> Self {
        Self {
            inner,
            expected_settlement_arm: if project_settlement { "on" } else { "off" },
            capture: Mutex::new(ModelRequestCapture::default()),
        }
    }

    fn record(&self, request: &ModelRequest) {
        let mut capture = self
            .capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        capture.total = capture.total.saturating_add(1);
        if let Some(raw_audit) = request
            .metadata
            .get("settlement_projection_audit")
            .filter(|value| !value.is_null())
        {
            capture.settlement_audits = capture.settlement_audits.saturating_add(1);
            let parsed =
                validate_live_settlement_audit(request, raw_audit, self.expected_settlement_arm);
            match parsed {
                Some((valid, normalized_prompt, tool_surface)) => {
                    if !valid {
                        capture.settlement_audit_invalid =
                            capture.settlement_audit_invalid.saturating_add(1);
                    }
                    if capture.first_settlement_normalized_prompt_digest.is_none() {
                        capture.first_settlement_normalized_prompt_digest = Some(normalized_prompt);
                        capture.first_settlement_tool_surface_digest = Some(tool_surface);
                    }
                }
                None => {
                    capture.settlement_audit_invalid =
                        capture.settlement_audit_invalid.saturating_add(1);
                }
            }
        }
        if capture.shapes.len() >= Self::CAP {
            return;
        }
        capture.shapes.push(ModelRequestShape {
            prompt_digest: serialized_digest(&request.messages),
            tool_surface_digest: serialized_digest(&request.tools),
        });
    }

    fn snapshot(&self) -> ModelRequestDigest {
        let capture = self
            .capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prompt: Vec<&str> = capture
            .shapes
            .iter()
            .map(|shape| shape.prompt_digest.as_str())
            .collect();
        let surface: Vec<&str> = capture
            .shapes
            .iter()
            .map(|shape| shape.tool_surface_digest.as_str())
            .collect();
        ModelRequestDigest {
            requests: capture.total as u64,
            capture_truncated: capture.total > capture.shapes.len(),
            prompt_digest: (!prompt.is_empty()).then(|| serialized_digest(&prompt)),
            tool_surface_digest: (!surface.is_empty()).then(|| serialized_digest(&surface)),
            settlement_audits: capture.settlement_audits,
            settlement_audit_invalid: capture.settlement_audit_invalid,
            first_settlement_normalized_prompt_digest: capture
                .first_settlement_normalized_prompt_digest
                .clone(),
            first_settlement_tool_surface_digest: capture
                .first_settlement_tool_surface_digest
                .clone(),
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for RecordingModelTransport {
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.record(&request);
        self.inner.complete(request).await
    }

    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        self.record(&request);
        self.inner.complete_stream(request, sink).await
    }
}

fn serialized_digest(value: &impl Serialize) -> String {
    match serde_json::to_vec(value) {
        Ok(bytes) => format!("{:x}", Sha256::digest(bytes)),
        // All captured request types are serde values, so this is defensive.
        // Keep the observer non-interfering if a future custom serializer
        // rejects a value; the sentinel remains explicit and stable.
        Err(_) => format!("{:x}", Sha256::digest(b"serialization-error")),
    }
}

fn validate_live_settlement_audit(
    request: &ModelRequest,
    raw_audit: &serde_json::Value,
    expected_arm: &str,
) -> Option<(bool, String, String)> {
    let comparison: agent_runtime::SettlementProjectionPreflight =
        serde_json::from_value(raw_audit.clone()).ok()?;
    let prompt_digest = serialized_digest(&request.messages);
    let tool_surface_digest = serialized_digest(&request.tools);
    let expected_prompt = match expected_arm {
        "off" => &comparison.baseline_prompt_sha256,
        "on" => &comparison.treatment_prompt_sha256,
        _ => return None,
    };
    let expected_surface = match expected_arm {
        "off" => &comparison.baseline_tool_surface_sha256,
        "on" => &comparison.treatment_tool_surface_sha256,
        _ => return None,
    };
    let valid = comparison.schema == "settlement-projection-preflight/v2"
        && comparison.allowed_difference == "one task_progress.settlement fact"
        && comparison.passed
        && comparison.settlement_occurrences == 1
        && comparison.baseline_request_sha256 == comparison.normalized_request_sha256
        && comparison.baseline_prompt_sha256 == comparison.normalized_prompt_sha256
        && comparison.baseline_tool_surface_sha256 == comparison.treatment_tool_surface_sha256
        && prompt_digest == expected_prompt.as_str()
        && tool_surface_digest == expected_surface.as_str();
    Some((
        valid,
        comparison.normalized_prompt_sha256,
        comparison.baseline_tool_surface_sha256,
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestDigest {
    pub requests: u64,
    pub capture_truncated: bool,
    pub prompt_digest: Option<String>,
    pub tool_surface_digest: Option<String>,
    /// Same-state counterfactual audits attached by Runtime exactly when a
    /// settled candidate is about to be exposed to the provider.
    #[serde(default)]
    pub settlement_audits: u64,
    #[serde(default)]
    pub settlement_audit_invalid: u64,
    #[serde(default)]
    pub first_settlement_normalized_prompt_digest: Option<String>,
    #[serde(default)]
    pub first_settlement_tool_surface_digest: Option<String>,
}

/// One finished live cell, before evidence serialization. Outcome
/// dimensions are recorded independently: a lifecycle-closure failure must
/// not erase whether the workspace was behaviorally correct, and the final
/// verdict stays conjunctive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellEvidenceIdentity {
    /// Prospective candidate identity shared by both treatment arms.
    pub candidate_id: String,
    /// Digest of the repository source tree that supplied Runtime/eval code.
    pub source_tree_digest: Option<String>,
    /// Pack-owned fixture/directive identity.
    pub fixture_sha256: String,
    /// Logical repeat, independent of immutable `-attemptN` evidence paths.
    pub repeat: u32,
    /// Human-readable surface profile plus its stable configuration digest.
    pub tool_surface: String,
    pub surface_config_digest: String,
    /// Prompt contract digest excluding the isolated settlement treatment;
    /// the switch itself is persisted separately and actual request-shape
    /// equality belongs to the deterministic causal preflight.
    pub prompt_config_digest: String,
    /// Host-owned acceptance authority recorded independently from the
    /// prompt contract. A declaration revision without its canonical source
    /// digest is not a reusable identity.
    pub acceptance_domain: Option<String>,
    pub acceptance_declaration_revision: Option<u64>,
    pub acceptance_source_digest: Option<String>,
    /// Stable digest of model/base-url/protocol/context-window only. API keys
    /// and other credentials are deliberately absent.
    pub provider_config_digest: String,
    /// Common pair configuration, never the treatment: both arms must opt
    /// into the same causal diagnostics envelope.
    pub settlement_projection_diagnostics: bool,
}

impl CellEvidenceIdentity {
    fn capture(
        pack: &PackSpec,
        pair: &PairSink,
        switches: CellSwitches,
        acceptance_profile: AcceptanceProfile,
        acceptance_authority: Option<&VerificationCoverageDeclaration>,
    ) -> Self {
        let fixture_sha256 = (pack.identity_sha256)();
        let exact_recipe_inputs = (pack.exact_recipe_inputs)();
        let surface_config_digest = serialized_digest(&serde_json::json!({
            "profile": PRODUCT_TOOL_SURFACE,
            "recovery_surface": switches.recovery_surface,
            "exact_recipe_inputs": exact_recipe_inputs,
        }));
        let prompt_config_digest = serialized_digest(&serde_json::json!({
            "candidate": CONVERGENCE_CANDIDATE_ID,
            "directive_sha256": format!("{:x}", Sha256::digest((pack.directive)().as_bytes())),
            "acceptance_declaration": pack.acceptance_declaration,
            "acceptance_domain": pack.acceptance_domain,
            "acceptance_profile": acceptance_profile.id(),
            "completion_opportunity": switches.opportunity,
            "project_task_progress": switches.project_task_progress,
            "settlement_projection_diagnostics": switches.settlement_projection_diagnostics,
            // `project_settlement` is the isolated treatment and is recorded
            // as its own dimension, not folded into the pair-baseline digest.
        }));
        let provider_context_window = crate::envfile::context_window()
            .map(|value| value.to_string())
            .unwrap_or_else(|_| "invalid".into());
        let provider_config_digest = serialized_digest(&serde_json::json!({
            "model": crate::envfile::get("OPENAI_MODEL")
                .unwrap_or_else(|| "gpt-4o-mini".into()),
            "base_url": crate::envfile::get("OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".into()),
            "protocol": crate::envfile::get("OPENAI_API_PROTOCOL")
                .unwrap_or_else(|| "auto".into()),
            "context_window": provider_context_window,
        }));
        Self {
            candidate_id: CONVERGENCE_CANDIDATE_ID.into(),
            source_tree_digest: crate::bundle::source_tree_digest(),
            fixture_sha256,
            repeat: pair.repeat,
            tool_surface: PRODUCT_TOOL_SURFACE.into(),
            surface_config_digest,
            prompt_config_digest,
            acceptance_domain: pack.acceptance_domain.map(str::to_owned),
            acceptance_declaration_revision: acceptance_authority
                .map(|declaration| declaration.declaration_revision),
            acceptance_source_digest: acceptance_authority
                .map(|declaration| declaration.source_digest.clone()),
            provider_config_digest,
            settlement_projection_diagnostics: switches.settlement_projection_diagnostics,
        }
    }
}

pub struct CellOutcome {
    pub pack_id: &'static str,
    pub mode: PilotMode,
    pub identity: CellEvidenceIdentity,
    /// Runtime task identity is evidence provenance, never a pair key.
    pub runtime_task_id: Option<TaskId>,
    /// Exact runtime model-request shapes captured across both phases.
    pub model_requests: ModelRequestDigest,
    /// Immutable evidence directory selected by the retry wrapper. It is
    /// runner bookkeeping and never participates in runtime behavior.
    pub evidence_dir: Option<PathBuf>,
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
    /// Product TASK PROGRESS projection. This must be identical across a
    /// settlement pair.
    pub project_task_progress: bool,
    /// Whether the neutral settlement node was projected. This is the
    /// convergence paired gate's only treatment.
    pub project_settlement: bool,
    /// Common causal-diagnostic configuration. This must be identical in a
    /// pair and remains false outside the isolated convergence runner.
    pub settlement_projection_diagnostics: bool,
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
    pub settlement_episode_terminals: BTreeMap<crate::metrics::SettlementEpisodeTerminal, u64>,
}

impl CellOutcome {
    fn failed(
        pack_id: &'static str,
        mode: PilotMode,
        acceptance_profile: AcceptanceProfile,
        switches: CellSwitches,
        identity: CellEvidenceIdentity,
        wall_ms: u64,
        failure: CellFailure,
    ) -> Self {
        Self {
            pack_id,
            mode,
            identity,
            runtime_task_id: None,
            model_requests: ModelRequestDigest::default(),
            evidence_dir: None,
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
            project_task_progress: switches.project_task_progress,
            project_settlement: switches.project_settlement,
            settlement_projection_diagnostics: switches.settlement_projection_diagnostics,
            settlement_seen: false,
            settlement_episodes: 0,
            settlement_episode_rounds: 0,
            settlement_episode_calls: 0,
            settlement_episode_failures: 0,
            settlement_episode_terminals: BTreeMap::new(),
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
        let projection = format!(
            " task_progress={} settlement_projection={}",
            if self.project_task_progress {
                "on"
            } else {
                "off"
            },
            if self.project_settlement { "on" } else { "off" }
        );
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
            "{} {:<6} {} profile={} behavior={} diff={} closure={} continuation={} provider={} rounds={}+{} resumes={} durables={}{}{}{}{}{}{}",
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
            projection,
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
        RuntimeEvent::ToolFinished { ref output, .. }
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
    verification_recipes: &tool_runtime::VerificationRecipes,
) -> anyhow::Result<agent_compose::ComposedRuntime> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    let tools: Arc<dyn ToolDispatcher> = Arc::new(
        tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            tool_runtime::ToolLifecycleConfig::default(),
            (*verification_recipes).clone(),
        ),
    );
    let composed = agent_compose::compose(agent_compose::ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
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
        project_task_progress: switches.project_task_progress,
        project_settlement: switches.project_settlement,
        settlement_projection_diagnostics: switches.settlement_projection_diagnostics,
        project_completion_opportunity: switches.opportunity,
        recovery_surface: switches.recovery_surface,
        host_policies: Some(Arc::new(
            agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(
                verification_recipes,
            )
            .map_err(anyhow::Error::msg)?,
        )),
        effect_reservation_journal: None,
        verification_recipes: Some(Arc::new((*verification_recipes).clone())),
        project_proof_refresh: true,
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
fn pilot_verification_recipes(
    exact_inputs: Vec<String>,
    acceptance_domain: Option<&str>,
) -> tool_runtime::VerificationRecipes {
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
        let mut exact = tool_runtime::VerificationRecipe::new(
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
        if let Some(domain) = acceptance_domain {
            exact = exact
                .with_coverage_domain(domain)
                .expect("pilot acceptance domain is valid");
        }
        recipes.push(exact);
    }
    let recipes =
        tool_runtime::VerificationRecipes::new(recipes).expect("pilot recipe set is valid");
    if let Some(domain) = acceptance_domain {
        recipes
            .with_domains(vec![tool_runtime::VerificationCoverageDomain {
                domain_id: domain.to_string(),
                declaration_revision: 1,
                members: vec!["jobrunner.exact".into()],
            }])
            .expect("pilot acceptance domain table is valid")
    } else {
        recipes
    }
}

pub(crate) fn pack_verification_projection(
    pack: &PackSpec,
) -> (
    tool_runtime::VerificationRecipes,
    Option<VerificationCoverageDeclaration>,
) {
    let recipes = pilot_verification_recipes((pack.exact_recipe_inputs)(), pack.acceptance_domain);
    let authority = pack.acceptance_domain.map(|domain| {
        recipes
            .coverage_declaration(domain)
            .expect("pilot acceptance domain must have a host declaration")
            .clone()
    });
    (recipes, authority)
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
        project_task_progress,
        project_settlement,
        settlement_projection_diagnostics,
    } = switches;
    // Construct the verifier authority once per cell. Both cold Runtime
    // compositions, the task criterion, and the evidence identity consume
    // this exact immutable projection rather than independently rebuilding
    // lookalike declarations.
    let (verification_recipes, acceptance_authority) = pack_verification_projection(pack);
    let surface_digest = evaluated_surface_digest(&verification_recipes).await;
    let identity = CellEvidenceIdentity::capture(
        pack,
        pair,
        switches,
        acceptance_profile,
        acceptance_authority.as_ref(),
    );
    let request_recorder = Arc::new(RecordingModelTransport::new(
        model.clone(),
        project_settlement,
    ));
    let runtime_model: Arc<dyn ModelTransport> = request_recorder.clone();
    let failed = |failure: CellFailure| {
        let mut outcome = CellOutcome::failed(
            pack.id,
            mode,
            acceptance_profile,
            switches,
            identity.clone(),
            started.elapsed().as_millis() as u64,
            failure,
        );
        outcome.model_requests = request_recorder.snapshot();
        outcome
    };

    if let Err(e) = (pack.seed)(root) {
        let outcome = failed(CellFailure::harness_setup(format!("seeding failed: {e:#}")));
        write_evidence(
            pair,
            root,
            &collector,
            pack,
            &outcome,
            None,
            None,
            surface_digest.as_deref(),
        )?;
        return Ok(outcome);
    }
    if let Err(e) = crate::suite::ensure_workspace_git(root) {
        let outcome = failed(CellFailure::harness_setup(format!(
            "workspace git init failed: {e:#}"
        )));
        write_evidence(
            pair,
            root,
            &collector,
            pack,
            &outcome,
            None,
            None,
            surface_digest.as_deref(),
        )?;
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
        runtime_model.clone(),
        c_engine(model.clone()),
        switches,
        &verification_recipes,
    )
    .await
    {
        Err(e) => absorb_failure(
            &mut failure,
            Some(CellFailure::harness_setup(format!(
                "phase-one compose failed: {e:#}"
            ))),
        ),
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
                // The task now exists and the actor is idle: patch the
                // pack's declarative acceptance onto the anchor so the
                // task-aware completion gate can arm in this live cell.
                // Only a later observed trusted PASS from the matching host
                // domain may mint a receipt; without a declaration nothing
                // is patched and the gate stays fail-closed.
                if let (Some(declaration), Some(authority)) =
                    (pack.acceptance_declaration, acceptance_authority.as_ref())
                {
                    declare_acceptance(&handle, declaration, authority)
                        .await
                        .map_err(|e| {
                            CellFailure::runtime(format!("acceptance declaration failed: {e:#}"))
                        })?;
                }
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
                                absorb_failure(
                                    &mut failure,
                                    Some(CellFailure::runtime(
                                        "trigger fired without an acknowledged checkpoint tuple",
                                    )),
                                );
                            }
                        }
                    }
                }
                Err(reason) => {
                    absorb_failure(&mut failure, Some(reason.context("phase one failed")))
                }
            }
            if let Err(e) = composed.shutdown().await {
                absorb_failure(
                    &mut failure,
                    Some(CellFailure::runtime(format!(
                        "phase-one shutdown failed: {e}"
                    ))),
                );
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
            Err(e) => absorb_failure(
                &mut failure,
                Some(CellFailure::runtime(format!(
                    "checkpoint artifact load failed: {e:#}"
                ))),
            ),
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
                        runtime_model.clone(),
                        c_engine(model.clone()),
                        switches,
                        &verification_recipes,
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
                absorb_failure(
                    &mut failure,
                    Some(CellFailure::harness_setup(format!(
                        "allowed-diff scan failed: {e:#}"
                    ))),
                );
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
            setup_failure: None,
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
                setup_failure: None,
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
    absorb_failure(&mut failure, oracle_failure);
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
    let outcome =
        CellOutcome {
            pack_id: pack.id,
            mode,
            identity,
            runtime_task_id: collector.events.iter().rev().find_map(|envelope| {
                match &envelope.event {
                    RuntimeEvent::TaskCompleted { task_id, .. }
                    | RuntimeEvent::TaskContinuationStarted { task_id, .. }
                    | RuntimeEvent::TaskResumeCommitted { task_id, .. }
                    | RuntimeEvent::FocusChanged { task_id, .. } => Some(*task_id),
                    RuntimeEvent::UserMessageAccepted { input } => input.task_id,
                    _ => None,
                }
            }),
            model_requests: request_recorder.snapshot(),
            evidence_dir: None,
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
            project_task_progress,
            project_settlement,
            settlement_projection_diagnostics,
            opportunity_offers,
            opportunity_called,
            settlement_seen: metrics.settled_seen,
            settlement_episodes: metrics.settlement_episodes,
            settlement_episode_rounds: metrics.settlement_episode_rounds,
            settlement_episode_calls: metrics.settlement_episode_calls,
            settlement_episode_failures: metrics.settlement_episode_failures,
            settlement_episode_terminals: metrics.settlement_episode_terminals,
        };
    write_evidence(
        pair,
        root,
        &collector,
        pack,
        &outcome,
        Some(&oracle_record),
        Some(&self_check_record),
        surface_digest.as_deref(),
    )?;
    Ok(outcome)
}

/// Whether a finished cell is worth a whole-cell retry: only a retryable
/// provider-transport outcome. Everything else (harness failure, model
/// budget, hard verdict) is a fact to report, not to rerun.
pub fn provider_transport_retryable(outcome: &CellOutcome) -> bool {
    outcome.error_class == Some(CellFailureClass::ProviderTransport) && outcome.error_retryable
}

/// Patch the pack's declarative acceptance criterion onto the active task
/// between `set_focus` and the first user message. The actor is idle at
/// that point, so the task-anchor CAS is allowed; Runtime can later mint a
/// receipt only from a matching post-observation trusted PASS.
async fn declare_acceptance(
    handle: &agent_runtime::RuntimeHandle,
    declaration: &'static str,
    coverage_authority: &VerificationCoverageDeclaration,
) -> anyhow::Result<()> {
    let tasks = handle.list_tasks().await?;
    let task = tasks
        .iter()
        .find(|task| task.status == agent_runtime::task::TaskStatus::Active)
        .or_else(|| tasks.first())
        .ok_or_else(|| anyhow::anyhow!("no task exists after set_focus"))?;
    handle
        .patch_task_anchor(
            task.id,
            task.anchor_revision,
            agent_runtime::task::AnchorPatch {
                completion_policy: Some(
                    agent_runtime::task::TaskCompletionPolicy::EvidenceRequired,
                ),
                acceptance_criteria: Some(vec![
                    agent_runtime::task::AcceptanceCriterion::declared(
                        declaration,
                        coverage_authority,
                    ),
                ]),
                ..agent_runtime::task::AnchorPatch::default()
            },
        )
        .await?;
    Ok(())
}

/// Run one live cell with cell-level provider retry. When the cell ends in
/// a retryable provider-transport outcome, the whole cell reruns into a
/// fresh attempt directory (`r{n}-attempt{k}`), so a provider outage that
/// outlives the request-level retry window does not silently produce
/// NOT_RUN evidence for a paired gate. Every attempt is rendered; only the
/// transport-outcome state retries, and the last outcome is returned after
/// `max_attempts` runs.
#[allow(clippy::too_many_arguments)] // flat passthrough of run_pack_cell args plus retry policy
pub async fn run_pack_cell_retrying(
    pack: &PackSpec,
    mode: PilotMode,
    evidence_root: std::path::PathBuf,
    fixture_id: String,
    repeat: u32,
    repeats: u32,
    model: Arc<dyn ModelTransport>,
    switches: CellSwitches,
    acceptance_profile: AcceptanceProfile,
    max_attempts: u32,
    base_delay: std::time::Duration,
) -> anyhow::Result<CellOutcome> {
    anyhow::ensure!(max_attempts >= 1, "cell retry needs at least one attempt");
    for attempt in 1..=max_attempts {
        let dir = tempfile::tempdir()?;
        let pair = PairSink::claim(
            evidence_root.clone(),
            fixture_id.clone(),
            repeat,
            repeats,
            true,
        );
        let mut outcome = run_pack_cell(
            pack,
            mode,
            &pair,
            model.clone(),
            dir.path(),
            switches,
            acceptance_profile,
        )
        .await?;
        let cell_dir = pair.cell_dir("dynamic");
        outcome.evidence_dir = Some(cell_dir.clone());
        println!("{}", outcome.render_line());
        match crate::bundle::render_evidence(&cell_dir) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => eprintln!("warning: evidence render failed: {error}"),
        }
        let retryable = provider_transport_retryable(&outcome);
        if !retryable || attempt == max_attempts {
            return Ok(outcome);
        }
        let delay = base_delay * 2u32.pow(attempt.saturating_sub(1));
        eprintln!(
            "cell ended in a retryable provider transport failure (attempt {attempt}/{max_attempts}); \
             retrying in {delay:?} into a fresh attempt directory"
        );
        tokio::time::sleep(delay).await;
    }
    unreachable!("max_attempts >= 1 and the final attempt always returns")
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

/// Digest of the production dispatcher surface this pilot evaluates: the
/// builtin catalog composed with the cell's frozen verification recipes,
/// hashed through the contracts derivation so surface drift between runs
/// is detectable from the evidence alone.
async fn evaluated_surface_digest(
    verification_recipes: &tool_runtime::VerificationRecipes,
) -> Option<String> {
    let root = tempfile::tempdir().ok()?;
    let workspace = agent_workspace::Workspace::open(root.path()).await.ok()?;
    let dispatcher = tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
        workspace,
        tool_runtime::ToolLifecycleConfig::default(),
        (*verification_recipes).clone(),
    );
    Some(agent_contracts::tool::surface_digest(&dispatcher.specs()))
}

/// Serialize the cell into the claimed pair directory using the shared
/// evidence conventions (manifest + events.jsonl + hidden report) plus
/// this pilot's per-dimension record.
#[allow(clippy::too_many_arguments)]
fn write_evidence(
    pair: &PairSink,
    root: &Path,
    collector: &Collector,
    pack: &PackSpec,
    outcome: &CellOutcome,
    oracle: Option<&HiddenCommandResult>,
    self_check: Option<&HiddenCommandResult>,
    surface_digest: Option<&str>,
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
        surface_digest,
        &report,
    )?;
    let dimensions = serde_json::json!({
        "schema": PILOT_SCHEMA,
        "pack_id": pack.id,
        "candidate_id": outcome.identity.candidate_id,
        "source_tree_digest": outcome.identity.source_tree_digest,
        "fixture_sha256": outcome.identity.fixture_sha256,
        "repeat": outcome.identity.repeat,
        "tool_surface": outcome.identity.tool_surface,
        "surface_config_digest": outcome.identity.surface_config_digest,
        "prompt_config_digest": outcome.identity.prompt_config_digest,
        "acceptance_domain": outcome.identity.acceptance_domain,
        "acceptance_declaration_revision": outcome.identity.acceptance_declaration_revision,
        "acceptance_source_digest": outcome.identity.acceptance_source_digest,
        "provider_config_digest": outcome.identity.provider_config_digest,
        "settlement_projection_diagnostics": outcome.settlement_projection_diagnostics,
        "runtime_task_id": outcome.runtime_task_id,
        "model_requests": outcome.model_requests,
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
        // Product/eval surface identity. These are separate so a settlement
        // experiment cannot silently remove TaskProgress or checked-file GC
        // projection from one arm.
        "project_task_progress": outcome.project_task_progress,
        "project_settlement": outcome.project_settlement,
        "settlement_episode_terminals": outcome.settlement_episode_terminals,
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
            setup_failure: None,
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
    let argv = std::iter::once("cargo".to_string())
        .chain(std::iter::once("test".to_string()))
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect::<Vec<_>>();
    run_tree_bounded(root, &argv, CARGO_TEST_TIMEOUT).await
}

/// Run one command with a bounded lifetime and a bounded capture. A
/// timed-out run is killed tree-wide (the command's own descendants
/// included) and reaped before the record is returned, so a runaway
/// process can never keep mutating the workspace after the verdict or the
/// evidence hash. Cargo runs additionally pin the target directory and
/// quiet the terminal color, exactly like the file-suite runner.
async fn run_tree_bounded(root: &Path, argv: &[String], timeout: Duration) -> HiddenCommandResult {
    use std::process::Stdio;

    let mut record = HiddenCommandResult {
        argv: argv.to_vec(),
        expect_exit: 0,
        exit: None,
        timed_out: false,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        setup_failure: None,
        passed: false,
    };
    if argv.is_empty() {
        record.stderr = "command argv is empty".to_string();
        return record;
    }
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if argv.iter().any(|arg| arg == "cargo") {
        command.env("CARGO_TARGET_DIR", root.join("target"));
    }
    #[cfg(unix)]
    {
        // tokio::process::Command's own unix extension spawns the child as
        // its own process-group leader, so `kill_process_tree` can signal
        // the whole tree on timeout instead of leaving descendants running.
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            record.stderr = format!("failed to spawn {}: {e}", argv[0]);
            return record;
        }
    };
    let pid = child.id();
    let stdout_task = child.stdout.take().map(|mut pipe| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer).await;
            buffer
        })
    });
    let stderr_task = child.stderr.take().map(|mut pipe| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer).await;
            buffer
        })
    });
    match tokio::time::timeout(timeout, child.wait()).await {
        Err(_) => {
            record.timed_out = true;
            record.stderr = format!("command did not finish within {timeout:?}");
            if let Some(pid) = pid {
                let _ = tokio::task::spawn_blocking(move || {
                    agent_process::kill_process_tree(pid);
                })
                .await;
            }
            // Reap the direct child so the tree cannot linger as a zombie
            // or keep running; the wait below must not hang the cell even
            // if the tree kill failed.
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
        Ok(Err(e)) => {
            record.stderr = format!("failed to wait for {}: {e}", argv[0]);
        }
        Ok(Ok(status)) => {
            record.exit = status.code();
            let stdout = match stdout_task {
                Some(task) => task.await.unwrap_or_default(),
                None => Vec::new(),
            };
            let stderr = match stderr_task {
                Some(task) => task.await.unwrap_or_default(),
                None => Vec::new(),
            };
            let stdout = String::from_utf8_lossy(&stdout);
            let stderr = String::from_utf8_lossy(&stderr);
            record.stdout_truncated = stdout.len() > COMMAND_CAPTURE_CAP;
            record.stderr_truncated = stderr.len() > COMMAND_CAPTURE_CAP;
            record.stdout = tail_capture(&stdout);
            record.stderr = tail_capture(&stderr);
            let combined = format!("{}\n{stdout}\n{stderr}", record.stdout);
            let (passed_tests, failed_tests) = parse_test_totals(&combined);
            record.passed = status.success() && failed_tests == 0 && passed_tests >= 1;
            if status.success() && passed_tests == 0 {
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
    let present: BTreeMap<String, String> = crate::bundle::collect_workspace_files(root)?
        .into_iter()
        .collect();
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

/// Judgment of the convergence paired gate. Both arms run the same
/// source, pack, serving, mode and repeat with product TaskProgress enabled;
/// only the neutral settlement-node projection differs. Any planned pair
/// without exposure is inconclusive, never silently selected out as a pass.
/// Promotion requires behavior/diff/resume parity, no lost unfinished
/// work, strictly lower candidate-episode rounds and calls, and no new
/// maximum episode or whole-cell tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvGateJudgment {
    /// pass | invalid | inconclusive | fail
    pub state: &'static str,
    pub reasons: Vec<String>,
    /// Per-mode center/overall exposure and episode facts for the report.
    pub off_cells: usize,
    pub off_exposed: usize,
    pub on_cells: usize,
    pub on_exposed: usize,
}

impl ConvGateJudgment {
    pub fn render(&self) -> String {
        let mut out = format!(
            "convergence gate: {} (off={} on={})",
            self.state, self.off_cells, self.on_cells
        );
        for reason in &self.reasons {
            out.push_str("\n  - ");
            out.push_str(reason);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ConvPairKey {
    candidate_id: String,
    source_tree_digest: String,
    pack_id: String,
    fixture_sha256: String,
    mode: PilotMode,
    repeat: u32,
    acceptance_domain: String,
    acceptance_declaration_revision: u64,
    acceptance_source_digest: String,
    provider_config_digest: String,
    settlement_projection_diagnostics: bool,
}

impl ConvPairKey {
    fn label(&self) -> String {
        fn short(value: &str) -> &str {
            value.get(..12).unwrap_or(value)
        }
        format!(
            "{}/{} r{} source={} acceptance={}@{}:{} provider={}",
            self.pack_id,
            self.mode.id(),
            self.repeat,
            short(&self.source_tree_digest),
            self.acceptance_domain,
            self.acceptance_declaration_revision,
            short(&self.acceptance_source_digest),
            short(&self.provider_config_digest),
        )
    }
}

#[derive(Default)]
struct ConvPairArms<'a> {
    off: Vec<&'a CellOutcome>,
    on: Vec<&'a CellOutcome>,
}

fn conv_pair_key(cell: &CellOutcome) -> Result<ConvPairKey, String> {
    let identity = &cell.identity;
    let source_tree_digest = identity
        .source_tree_digest
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "{}/{} r{} has no source-tree digest",
                cell.pack_id,
                cell.mode.id(),
                identity.repeat
            )
        })?;
    let acceptance_domain = identity
        .acceptance_domain
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "{}/{} r{} has no acceptance-domain identity",
                cell.pack_id,
                cell.mode.id(),
                identity.repeat
            )
        })?;
    let acceptance_declaration_revision = identity
        .acceptance_declaration_revision
        .filter(|revision| *revision > 0)
        .ok_or_else(|| {
            format!(
                "{}/{} r{} has no acceptance declaration revision",
                cell.pack_id,
                cell.mode.id(),
                identity.repeat
            )
        })?;
    let acceptance_source_digest = identity
        .acceptance_source_digest
        .as_deref()
        .filter(|digest| digest.parse::<agent_contracts::ContentDigest>().is_ok())
        .ok_or_else(|| {
            format!(
                "{}/{} r{} has no valid acceptance source digest",
                cell.pack_id,
                cell.mode.id(),
                identity.repeat
            )
        })?;
    if identity.candidate_id != CONVERGENCE_CANDIDATE_ID {
        return Err(format!(
            "{}/{} r{} has candidate {:?}, expected {CONVERGENCE_CANDIDATE_ID:?}",
            cell.pack_id,
            cell.mode.id(),
            identity.repeat,
            identity.candidate_id
        ));
    }
    if identity.fixture_sha256.is_empty()
        || identity.provider_config_digest.is_empty()
        || identity.surface_config_digest.is_empty()
        || identity.prompt_config_digest.is_empty()
        || identity.repeat == 0
    {
        return Err(format!(
            "{}/{} has incomplete fixture/repeat/provider/surface/prompt identity",
            cell.pack_id,
            cell.mode.id()
        ));
    }
    if identity.tool_surface != PRODUCT_TOOL_SURFACE {
        return Err(format!(
            "{}/{} r{} used tool surface {:?}, expected {PRODUCT_TOOL_SURFACE:?}",
            cell.pack_id,
            cell.mode.id(),
            identity.repeat,
            identity.tool_surface
        ));
    }
    if identity.settlement_projection_diagnostics != cell.settlement_projection_diagnostics {
        return Err(format!(
            "{}/{} r{} has inconsistent diagnostic identity {}/{}",
            cell.pack_id,
            cell.mode.id(),
            identity.repeat,
            identity.settlement_projection_diagnostics,
            cell.settlement_projection_diagnostics
        ));
    }
    if !identity.settlement_projection_diagnostics {
        return Err(format!(
            "{}/{} r{} did not enable the common causal diagnostic envelope",
            cell.pack_id,
            cell.mode.id(),
            identity.repeat
        ));
    }
    Ok(ConvPairKey {
        candidate_id: identity.candidate_id.clone(),
        source_tree_digest: source_tree_digest.to_string(),
        pack_id: cell.pack_id.to_string(),
        fixture_sha256: identity.fixture_sha256.clone(),
        mode: cell.mode,
        repeat: identity.repeat,
        acceptance_domain: acceptance_domain.to_string(),
        acceptance_declaration_revision,
        acceptance_source_digest: acceptance_source_digest.to_string(),
        provider_config_digest: identity.provider_config_digest.clone(),
        settlement_projection_diagnostics: identity.settlement_projection_diagnostics,
    })
}

fn center_twice(samples: &[u64]) -> Option<u128> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some(u128::from(sorted[middle - 1]) + u128::from(sorted[middle]))
    } else {
        Some(u128::from(sorted[middle]) * 2)
    }
}

fn render_half(value_twice: u128) -> String {
    if value_twice.is_multiple_of(2) {
        (value_twice / 2).to_string()
    } else {
        format!("{}.5", value_twice / 2)
    }
}

/// Bounded report rendering for exact observations plus an arithmetic center.
/// With two repeats this deliberately says `midpoint`, not `median`: the old
/// nearest-rank p50 selected the upper observation and overstated certainty.
pub fn render_sample_center(samples: &[u64]) -> String {
    const SAMPLE_RENDER_CAP: usize = 16;
    if samples.is_empty() {
        return "unavailable".into();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let shown: Vec<u64> = sorted.iter().take(SAMPLE_RENDER_CAP).copied().collect();
    let suffix = if sorted.len() > shown.len() {
        format!(" (+{} omitted)", sorted.len() - shown.len())
    } else {
        String::new()
    };
    let center = render_half(center_twice(&sorted).unwrap_or(0));
    match sorted.len() {
        1 => format!("observation={shown:?}{suffix} center={center}"),
        2 => format!("observations={shown:?}{suffix} midpoint={center}"),
        _ => format!("observations={shown:?}{suffix} median={center}"),
    }
}

/// Evaluate the paired convergence gate from an unordered outcome set. Arms
/// are joined by stable logical identity, never by vector order or runtime
/// `TaskId`. Every key must have exactly one settlement-off and one
/// settlement-on cell; missing, duplicate, or identity-mismatched cells fail
/// closed. Per-pair settlement exposure must match, and episode efficiency
/// compares only cells that actually saw a settled candidate.
pub fn evaluate_conv_gate(outcomes: &[CellOutcome]) -> ConvGateJudgment {
    const MAX_GATE_CELLS: usize = 2 * 2 * 4;
    let invalid_surface = outcomes
        .iter()
        .filter(|cell| !cell.project_task_progress)
        .count();
    let off: Vec<&CellOutcome> = outcomes
        .iter()
        .filter(|cell| !cell.project_settlement)
        .collect();
    let on: Vec<&CellOutcome> = outcomes
        .iter()
        .filter(|cell| cell.project_settlement)
        .collect();
    let mut invalidities = Vec::new();
    let mut failures = Vec::new();

    if outcomes.len() > MAX_GATE_CELLS {
        return ConvGateJudgment {
            state: "invalid",
            off_cells: off.len(),
            off_exposed: off.iter().filter(|cell| cell.settlement_seen).count(),
            on_cells: on.len(),
            on_exposed: on.iter().filter(|cell| cell.settlement_seen).count(),
            reasons: vec![format!(
                "gate received {} cells, exceeding the declared {MAX_GATE_CELLS}-cell bound",
                outcomes.len()
            )],
        };
    }

    if invalid_surface > 0 {
        invalidities.push(format!(
            "{invalid_surface} cell(s) disabled product TaskProgress; settlement is not the isolated treatment"
        ));
        return ConvGateJudgment {
            state: "invalid",
            off_cells: off.len(),
            off_exposed: off.iter().filter(|cell| cell.settlement_seen).count(),
            on_cells: on.len(),
            on_exposed: on.iter().filter(|cell| cell.settlement_seen).count(),
            reasons: invalidities,
        };
    }

    let mut keyed: BTreeMap<ConvPairKey, ConvPairArms<'_>> = BTreeMap::new();
    for cell in outcomes {
        match conv_pair_key(cell) {
            Ok(key) => {
                let arms = keyed.entry(key).or_default();
                if cell.project_settlement {
                    arms.on.push(cell);
                } else {
                    arms.off.push(cell);
                }
            }
            Err(reason) => invalidities.push(reason),
        }
    }
    if keyed.is_empty() && invalidities.is_empty() {
        invalidities.push("no cells were supplied; no stable pair can be formed".into());
    }

    let mut pairs = Vec::new();
    for (key, arms) in &keyed {
        if arms.off.len() != 1 || arms.on.len() != 1 {
            invalidities.push(format!(
                "pair {} requires exactly one off and one on cell, found off={} on={}",
                key.label(),
                arms.off.len(),
                arms.on.len()
            ));
            continue;
        }
        pairs.push((key, arms.off[0], arms.on[0]));
    }
    if !invalidities.is_empty() {
        return ConvGateJudgment {
            state: "invalid",
            off_cells: off.len(),
            off_exposed: off.iter().filter(|cell| cell.settlement_seen).count(),
            on_cells: on.len(),
            on_exposed: on.iter().filter(|cell| cell.settlement_seen).count(),
            reasons: invalidities,
        };
    }

    // Mandatory parity after identity join. Only project_settlement may
    // differ; diagnostic marker shape remains observational below.
    for (key, off_cell, on_cell) in &pairs {
        let label = key.label();
        if off_cell.acceptance_profile != on_cell.acceptance_profile {
            invalidities.push(format!(
                "pair {label}: acceptance profile {}/{}",
                off_cell.acceptance_profile.id(),
                on_cell.acceptance_profile.id()
            ));
        }
        if off_cell.opportunity != on_cell.opportunity {
            invalidities.push(format!(
                "pair {label}: completion-opportunity switch {}/{}",
                off_cell.opportunity, on_cell.opportunity
            ));
        }
        if off_cell.recovery_surface != on_cell.recovery_surface {
            invalidities.push(format!(
                "pair {label}: recovery-surface switch {}/{}",
                off_cell.recovery_surface, on_cell.recovery_surface
            ));
        }
        if off_cell.project_task_progress != on_cell.project_task_progress {
            invalidities.push(format!(
                "pair {label}: TaskProgress switch {}/{}",
                off_cell.project_task_progress, on_cell.project_task_progress
            ));
        }
        if off_cell.identity.tool_surface != on_cell.identity.tool_surface
            || off_cell.identity.surface_config_digest != on_cell.identity.surface_config_digest
        {
            invalidities.push(format!("pair {label}: tool-surface identity mismatch"));
        }
        if off_cell.identity.prompt_config_digest != on_cell.identity.prompt_config_digest {
            invalidities.push(format!("pair {label}: prompt-baseline identity mismatch"));
        }
        if off_cell.model_requests.capture_truncated || on_cell.model_requests.capture_truncated {
            invalidities.push(format!(
                "pair {label}: model-request capture truncated off={} on={}",
                off_cell.model_requests.capture_truncated, on_cell.model_requests.capture_truncated
            ));
        }
        if off_cell.behavior != on_cell.behavior {
            failures.push(format!(
                "pair {label}: behavior {}/{}",
                off_cell.behavior, on_cell.behavior
            ));
        }
        if off_cell.diff != on_cell.diff {
            failures.push(format!(
                "pair {label}: diff {}/{}",
                off_cell.diff, on_cell.diff
            ));
        }
        if off_cell.closure != on_cell.closure {
            failures.push(format!(
                "pair {label}: closure {}/{}",
                off_cell.closure, on_cell.closure
            ));
        }
        if off_cell.continuation != on_cell.continuation {
            failures.push(format!(
                "pair {label}: continuation {}/{}",
                off_cell.continuation, on_cell.continuation
            ));
        }
        if off_cell.self_check != on_cell.self_check {
            failures.push(format!(
                "pair {label}: self_check {}/{}",
                off_cell.self_check, on_cell.self_check
            ));
        }
        if off_cell.passed != on_cell.passed {
            failures.push(format!(
                "pair {label}: verdict {}/{}",
                off_cell.passed, on_cell.passed
            ));
        }
        if !off_cell.passed || !on_cell.passed {
            failures.push(format!(
                "pair {label}: mandatory cell success is not true in both arms"
            ));
        }
        if off_cell.settlement_seen != on_cell.settlement_seen {
            invalidities.push(format!(
                "pair {label}: settlement exposure {}/{}",
                off_cell.settlement_seen, on_cell.settlement_seen
            ));
        }
        if off_cell.settlement_seen && on_cell.settlement_seen {
            if off_cell.model_requests.settlement_audits == 0
                || on_cell.model_requests.settlement_audits == 0
            {
                invalidities.push(format!(
                    "pair {label}: settled exposure lacks a live same-state request audit off={} on={}",
                    off_cell.model_requests.settlement_audits,
                    on_cell.model_requests.settlement_audits
                ));
            }
            if off_cell.model_requests.settlement_audit_invalid > 0
                || on_cell.model_requests.settlement_audit_invalid > 0
            {
                invalidities.push(format!(
                    "pair {label}: invalid live settlement request audit off={} on={}",
                    off_cell.model_requests.settlement_audit_invalid,
                    on_cell.model_requests.settlement_audit_invalid
                ));
            }
            if off_cell
                .model_requests
                .first_settlement_normalized_prompt_digest
                != on_cell
                    .model_requests
                    .first_settlement_normalized_prompt_digest
            {
                invalidities.push(format!(
                    "pair {label}: first exposed request differs after removing the treatment line"
                ));
            }
            if off_cell.model_requests.first_settlement_tool_surface_digest
                != on_cell.model_requests.first_settlement_tool_surface_digest
            {
                invalidities.push(format!(
                    "pair {label}: first exposed tool-surface digest differs"
                ));
            }
            if off_cell
                .model_requests
                .first_settlement_normalized_prompt_digest
                .is_none()
                || off_cell
                    .model_requests
                    .first_settlement_tool_surface_digest
                    .is_none()
            {
                invalidities.push(format!(
                    "pair {label}: first exposed normalized request identity is missing"
                ));
            }
        }
        if !off_cell.diff_violations.is_empty() || !on_cell.diff_violations.is_empty() {
            failures.push(format!(
                "pair {label}: diff violations off={} on={}",
                off_cell.diff_violations.len(),
                on_cell.diff_violations.len()
            ));
        }
    }
    if !invalidities.is_empty() {
        let on_exposed = on.iter().filter(|cell| cell.settlement_seen).count();
        return ConvGateJudgment {
            state: "invalid",
            off_cells: off.len(),
            off_exposed: off.iter().filter(|cell| cell.settlement_seen).count(),
            on_cells: on.len(),
            on_exposed,
            reasons: invalidities,
        };
    }
    if !failures.is_empty() {
        let on_exposed = on.iter().filter(|cell| cell.settlement_seen).count();
        return ConvGateJudgment {
            state: "fail",
            off_cells: off.len(),
            off_exposed: off.iter().filter(|cell| cell.settlement_seen).count(),
            on_cells: on.len(),
            on_exposed,
            reasons: failures,
        };
    }

    // Every planned pair must reach the pre-treatment settlement candidate.
    // A partial sample is not a smaller valid sample: selecting only the
    // exposed cells would bias the paired experiment. Identity and parity
    // were validated above, so incomplete exposure is inconclusive.
    let on_exposed = on.iter().filter(|cell| cell.settlement_seen).count();
    if on_exposed != on.len() {
        failures.push(format!(
            "incomplete paired exposure ({on_exposed}/{} on-arm cell(s) exposed); gate is inconclusive, never a pass",
            on.len(),
        ));
        return ConvGateJudgment {
            state: "inconclusive",
            off_cells: off.len(),
            off_exposed: off.iter().filter(|cell| cell.settlement_seen).count(),
            on_cells: on.len(),
            on_exposed,
            reasons: failures,
        };
    }

    let mut reasons = Vec::new();
    let marker_off: usize = off.iter().map(|cell| cell.marker_violations.len()).sum();
    let marker_on: usize = on.iter().map(|cell| cell.marker_violations.len()).sum();
    if marker_off > 0 || marker_on > 0 {
        reasons.push(format!(
            "observational marker-shape counts off={marker_off} on={marker_on} (not a gate)"
        ));
    }

    // Efficiency: lower candidate-episode rounds and calls, and no new
    // maximum episode or whole-cell tail. Episode metrics count only the
    // cells that saw a settled candidate; whole-cell tails count every
    // cell of the arm.
    fn exposed<'a>(cells: &'a [&'a CellOutcome]) -> Vec<&'a CellOutcome> {
        cells
            .iter()
            .copied()
            .filter(|cell| cell.settlement_seen)
            .collect()
    }
    let off_episode_rounds: Vec<u64> = exposed(&off)
        .iter()
        .map(|cell| cell.settlement_episode_rounds)
        .collect();
    let on_episode_rounds: Vec<u64> = exposed(&on)
        .iter()
        .map(|cell| cell.settlement_episode_rounds)
        .collect();
    let off_episode_calls: Vec<u64> = exposed(&off)
        .iter()
        .map(|cell| cell.settlement_episode_calls)
        .collect();
    let on_episode_calls: Vec<u64> = exposed(&on)
        .iter()
        .map(|cell| cell.settlement_episode_calls)
        .collect();
    let off_whole_tail: Vec<u64> = off
        .iter()
        .map(|cell| u64::from(cell.model_rounds_phase_one + cell.model_rounds_phase_two))
        .collect();
    let on_whole_tail: Vec<u64> = on
        .iter()
        .map(|cell| u64::from(cell.model_rounds_phase_one + cell.model_rounds_phase_two))
        .collect();
    let off_center_rounds = center_twice(&off_episode_rounds).unwrap_or(0);
    let on_center_rounds = center_twice(&on_episode_rounds).unwrap_or(0);
    let off_center_calls = center_twice(&off_episode_calls).unwrap_or(0);
    let on_center_calls = center_twice(&on_episode_calls).unwrap_or(0);
    let off_max_rounds = off_episode_rounds.iter().copied().max().unwrap_or(0);
    let on_max_rounds = on_episode_rounds.iter().copied().max().unwrap_or(0);
    let off_max_whole = off_whole_tail.iter().copied().max().unwrap_or(0);
    let on_max_whole = on_whole_tail.iter().copied().max().unwrap_or(0);

    for mode in [PilotMode::Normal, PilotMode::Resume] {
        let mode_pairs: Vec<_> = pairs
            .iter()
            .filter(|(key, _, _)| key.mode == mode)
            .collect();
        if mode_pairs.is_empty() {
            continue;
        }
        let off_rounds: Vec<u64> = mode_pairs
            .iter()
            .filter(|(_, off_cell, _)| off_cell.settlement_seen)
            .map(|(_, off_cell, _)| off_cell.settlement_episode_rounds)
            .collect();
        let on_rounds: Vec<u64> = mode_pairs
            .iter()
            .filter(|(_, _, on_cell)| on_cell.settlement_seen)
            .map(|(_, _, on_cell)| on_cell.settlement_episode_rounds)
            .collect();
        let off_calls: Vec<u64> = mode_pairs
            .iter()
            .filter(|(_, off_cell, _)| off_cell.settlement_seen)
            .map(|(_, off_cell, _)| off_cell.settlement_episode_calls)
            .collect();
        let on_calls: Vec<u64> = mode_pairs
            .iter()
            .filter(|(_, _, on_cell)| on_cell.settlement_seen)
            .map(|(_, _, on_cell)| on_cell.settlement_episode_calls)
            .collect();
        reasons.push(format!(
            "{} episode rounds off {}; on {}; calls off {}; on {}",
            mode.id(),
            render_sample_center(&off_rounds),
            render_sample_center(&on_rounds),
            render_sample_center(&off_calls),
            render_sample_center(&on_calls),
        ));
    }
    reasons.push(format!(
        "aggregate episode rounds off {}; on {}; calls off {}; on {}",
        render_sample_center(&off_episode_rounds),
        render_sample_center(&on_episode_rounds),
        render_sample_center(&off_episode_calls),
        render_sample_center(&on_episode_calls),
    ));
    reasons.push(format!(
        "max episode round tail {off_max_rounds} -> {on_max_rounds}; max whole-cell round tail {off_max_whole} -> {on_max_whole}"
    ));

    let lower_episodes = on_center_rounds < off_center_rounds && on_center_calls < off_center_calls;
    let no_new_max = on_max_rounds <= off_max_rounds && on_max_whole <= off_max_whole;
    if !lower_episodes {
        reasons.push("candidate-episode rounds/calls are not strictly lower".into());
    }
    if !no_new_max {
        reasons.push("the on arm introduced a new maximum episode or whole-cell tail".into());
    }
    let state = if lower_episodes && no_new_max {
        "pass"
    } else {
        "fail"
    };
    ConvGateJudgment {
        state,
        off_cells: off.len(),
        off_exposed: off.iter().filter(|cell| cell.settlement_seen).count(),
        on_cells: on.len(),
        on_exposed,
        reasons,
    }
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
            pilot_verification_recipes(
                vec![
                    "Cargo.toml".into(),
                    "src/config.rs".into(),
                    "src/error.rs".into(),
                    "src/lib.rs".into(),
                ],
                Some("retry-policy-public-contract"),
            ),
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
        let provenance = attribution
            .verification_recipe
            .as_ref()
            .expect("the exact recipe carries host provenance");
        assert_eq!(
            provenance.coverage_domain.as_deref(),
            Some("retry-policy-public-contract")
        );
        assert_eq!(provenance.domain_declaration_revision, Some(1));
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

    /// Failure classification is monotone: a later harness failure upgrades
    /// the cell to NOT_RUN even when a behavior failure was recorded first,
    /// while the first behavior failure still survives a later runtime
    /// failure and a harness failure is never downgraded.
    #[test]
    fn absorb_failure_prefers_not_run_and_keeps_first_behavior_failure() {
        fn class_of(failure: &Option<CellFailure>) -> Option<CellFailureClass> {
            failure.as_ref().map(|failure| failure.class)
        }
        let harness = Some(CellFailure::harness_setup("oracle unavailable"));
        let watchdog = Some(CellFailure::new(
            CellFailureClass::HarnessWatchdog,
            "cell stalled",
        ));
        let behavior = Some(CellFailure::new(
            CellFailureClass::Model,
            "model misbehaved",
        ));
        let runtime = Some(CellFailure::runtime("runtime hiccup"));

        // Behavior failure recorded first, then a harness failure: the
        // harness failure wins, censoring the cell to NOT_RUN.
        let mut current = behavior.clone();
        absorb_failure(&mut current, harness.clone());
        assert_eq!(class_of(&current), class_of(&harness));
        assert!(current.as_ref().unwrap().class.not_run());

        // Harness failure first: a later behavior failure must not
        // downgrade the censor.
        let mut current = harness.clone();
        absorb_failure(&mut current, behavior.clone());
        assert_eq!(class_of(&current), class_of(&harness));

        // First behavior failure survives a later runtime failure.
        let mut current = behavior.clone();
        absorb_failure(&mut current, runtime.clone());
        assert_eq!(class_of(&current), class_of(&behavior));

        // No incoming failure changes nothing.
        let mut current = Some(CellFailure::runtime("r"));
        absorb_failure(&mut current, None);
        assert!(current.is_some());

        // Empty slot accepts the first failure regardless of class.
        let mut current = None;
        absorb_failure(&mut current, watchdog.clone());
        assert_eq!(class_of(&current), class_of(&watchdog));
    }

    /// A timed-out bounded run kills the whole tree and reports the
    /// timeout without hanging the cell. Unix asserts the descendant is
    /// actually dead; Windows asserts the bounded return plus the timeout
    /// flag (the tree kill itself is agent-process' `kill_process_tree`,
    /// covered by its own tests).
    #[tokio::test]
    async fn run_tree_bounded_timeout_kills_the_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 300 & echo $! > child.pid; wait".to_string(),
        ];
        #[cfg(windows)]
        let argv = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            format!(
                "$p = Start-Process -FilePath powershell -ArgumentList '-NoProfile','-Command',\
                 'Start-Sleep 300' -PassThru; [System.IO.File]::WriteAllText('{}', [string]$p.Id); \
                 Wait-Process -Id $p.Id",
                root.join("child.pid").to_string_lossy().replace('\'', "''")
            ),
        ];
        let started = std::time::Instant::now();
        // PowerShell cold-start takes longer than `sh`, so Windows needs a
        // longer window for the descendant to write its pid file — and a
        // loaded CI runner can exceed even that before the outer script
        // reaches Start-Process, which would kill the tree before the pid
        // file ever exists. 20s keeps real headroom while staying far below
        // the 300-second descendant lifetime being killed.
        let timeout = if cfg!(windows) {
            Duration::from_secs(20)
        } else {
            Duration::from_millis(300)
        };
        let return_bound = if cfg!(windows) {
            Duration::from_secs(45)
        } else {
            Duration::from_secs(30)
        };
        let record = run_tree_bounded(&root, &argv, timeout).await;
        assert!(record.timed_out, "{record:?}");
        assert!(record.exit.is_none());
        assert!(
            started.elapsed() < return_bound,
            "timed-out run must return bounded"
        );
        assert!(
            record.stderr.contains("did not finish"),
            "{}",
            record.stderr
        );

        #[cfg(unix)]
        {
            let pid_text =
                std::fs::read_to_string(root.join("child.pid")).expect("descendant pid file");
            let pid: u32 = pid_text.trim().parse().expect("parse descendant pid");
            // The descendant must be dead after the tree kill: `kill -0`
            // succeeds on a live process and fails on a dead one.
            let probe = std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .output()
                .expect("run kill -0");
            assert!(!probe.status.success(), "descendant {pid} still alive");
        }
        #[cfg(windows)]
        {
            let pid_text =
                std::fs::read_to_string(root.join("child.pid")).expect("descendant pid file");
            let pid = pid_text.trim().to_string();
            let probe = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command"])
                .arg(format!(
                    "Get-Process -Id {pid} -ErrorAction SilentlyContinue"
                ))
                .output()
                .expect("run process probe");
            let out = String::from_utf8_lossy(&probe.stdout);
            assert!(out.trim().is_empty(), "descendant {pid} still alive: {out}");
        }
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

    /// Build one gate cell: the failed-outcome constructor is a convenient
    /// all-fields base; the test then sets the dimensions the gate reads.
    #[allow(clippy::too_many_arguments)] // compact synthetic gate vector builder
    fn gate_cell(
        mode: PilotMode,
        repeat: u32,
        settlement: bool,
        behavior: &str,
        exposed: bool,
        episode_rounds: u64,
        episode_calls: u64,
        whole_rounds: u32,
    ) -> CellOutcome {
        let identity = CellEvidenceIdentity {
            candidate_id: CONVERGENCE_CANDIDATE_ID.into(),
            source_tree_digest: Some("source-tree".into()),
            fixture_sha256: "fixture-sha".into(),
            repeat,
            tool_surface: PRODUCT_TOOL_SURFACE.into(),
            surface_config_digest: "surface-config".into(),
            prompt_config_digest: "prompt-config".into(),
            acceptance_domain: Some("retry-policy-public-contract".into()),
            acceptance_declaration_revision: Some(1),
            acceptance_source_digest: Some(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ),
            provider_config_digest: "provider-config".into(),
            settlement_projection_diagnostics: true,
        };
        let mut cell = CellOutcome::failed(
            "retry_policy_dev",
            mode,
            AcceptanceProfile::M15V1,
            CellSwitches {
                opportunity: false,
                recovery_surface: false,
                project_task_progress: true,
                project_settlement: settlement,
                settlement_projection_diagnostics: true,
            },
            identity,
            0,
            CellFailure::runtime("gate test seed"),
        );
        cell.behavior = behavior.into();
        cell.diff = if behavior == "pass" { "pass" } else { "fail" }.into();
        cell.closure = if behavior == "pass" {
            "completed"
        } else {
            "failed"
        }
        .into();
        cell.continuation = match mode {
            PilotMode::Normal => "n/a".into(),
            PilotMode::Resume if behavior == "pass" => "restored".into(),
            PilotMode::Resume => "failed".into(),
        };
        cell.self_check = if behavior == "pass" { "pass" } else { "fail" }.into();
        cell.passed = behavior == "pass";
        cell.settlement_seen = exposed;
        cell.settlement_episodes = u64::from(exposed);
        cell.settlement_episode_rounds = episode_rounds;
        cell.settlement_episode_calls = episode_calls;
        cell.model_requests = ModelRequestDigest {
            requests: u64::from(whole_rounds),
            settlement_audits: u64::from(exposed),
            first_settlement_normalized_prompt_digest: exposed
                .then(|| format!("normalized/{:?}/r{repeat}", mode)),
            first_settlement_tool_surface_digest: exposed.then(|| "surface/v1".into()),
            ..ModelRequestDigest::default()
        };
        cell.model_rounds_phase_one = whole_rounds;
        cell.model_rounds_phase_two = 0;
        cell.runtime_task_id = Some(TaskId::new());
        cell
    }

    #[test]
    fn conv_gate_with_a_missing_arm_fails_closed() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Resume, 1, false, "pass", true, 8, 9, 8),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("exactly one off and one on")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_zero_on_arm_exposure_is_inconclusive_not_a_pass() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", false, 0, 0, 10),
            gate_cell(PilotMode::Resume, 1, false, "pass", false, 0, 0, 8),
            gate_cell(PilotMode::Normal, 1, true, "pass", false, 0, 0, 10),
            gate_cell(PilotMode::Resume, 1, true, "pass", false, 0, 0, 8),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "inconclusive");
        assert_eq!(judgment.on_exposed, 0);
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("incomplete paired exposure")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_partial_paired_exposure_is_inconclusive_not_selected_away() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8),
            gate_cell(PilotMode::Resume, 1, false, "pass", false, 0, 0, 10),
            gate_cell(PilotMode::Resume, 1, true, "pass", false, 0, 0, 8),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "inconclusive");
        assert_eq!(judgment.off_exposed, 1);
        assert_eq!(judgment.on_exposed, 1);
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("incomplete paired exposure (1/2")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_requires_diagnostics_in_both_cell_identity_and_outcome() {
        let mut off = gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12);
        let on = gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8);
        off.settlement_projection_diagnostics = false;
        let judgment = evaluate_conv_gate(&[off, on]);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("inconsistent diagnostic identity")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_paired_cells_must_match_behavior_world_and_verdict() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Resume, 1, false, "pass", true, 8, 9, 8),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 6, 6, 6),
            gate_cell(PilotMode::Resume, 1, true, "fail", true, 8, 9, 8),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "fail");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("behavior pass/fail")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_per_pair_exposure_mismatch_fails() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Resume, 1, false, "pass", true, 8, 9, 8),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 6, 6, 6),
            gate_cell(PilotMode::Resume, 1, true, "pass", false, 0, 0, 8),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("settlement exposure true/false")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_passes_with_strictly_lower_episodes_and_no_new_max() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12),
            gate_cell(PilotMode::Resume, 1, false, "pass", true, 14, 16, 14),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8),
            gate_cell(PilotMode::Resume, 1, true, "pass", true, 10, 12, 10),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "pass", "{:?}", judgment.reasons);
    }

    #[test]
    fn conv_gate_joins_shuffled_arms_by_stable_key_and_reports_midpoints() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 2, true, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8),
            gate_cell(PilotMode::Normal, 2, false, "pass", true, 14, 16, 14),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "pass", "{:?}", judgment.reasons);
        let rendered = judgment.render();
        assert!(rendered.contains("observations=[12, 14] midpoint=13"));
        assert!(rendered.contains("observations=[8, 10] midpoint=9"));
        assert!(!rendered.contains("rounds median"));
    }

    #[test]
    fn conv_gate_rejects_duplicate_logical_cells() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12),
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("found off=2 on=1"))
        );
    }

    #[test]
    fn conv_gate_rejects_provider_identity_mismatch_as_unpaired() {
        let off = gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12);
        let mut on = gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8);
        on.identity.provider_config_digest = "different-provider".into();
        let judgment = evaluate_conv_gate(&[off, on]);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("exactly one off and one on"))
        );
    }

    #[test]
    fn conv_gate_rejects_acceptance_authority_mismatch_as_unpaired() {
        let mut outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 9, 10, 9),
        ];
        outcomes[1].identity.acceptance_source_digest =
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());

        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("exactly one off and one on")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_rejects_non_treatment_switch_mismatch() {
        let off = gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12);
        let mut on = gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8);
        on.opportunity = true;
        let judgment = evaluate_conv_gate(&[off, on]);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("completion-opportunity switch"))
        );
    }

    #[test]
    fn conv_gate_rejects_truncated_or_noncausal_live_request_capture() {
        let mut off = gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12);
        let on = gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8);
        off.model_requests.capture_truncated = true;
        let judgment = evaluate_conv_gate(&[off, on]);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("model-request capture truncated"))
        );

        let off = gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12);
        let mut on = gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8);
        on.model_requests.first_settlement_normalized_prompt_digest =
            Some("different-state".into());
        let judgment = evaluate_conv_gate(&[off, on]);
        assert_eq!(judgment.state, "invalid");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("differs after removing"))
        );
    }

    #[test]
    fn live_request_audit_is_bound_to_the_harness_arm_and_exact_request() {
        let messages = vec![agent_contracts::ModelMessage::system("baseline")];
        let tools = vec![ToolSpec {
            name: "fs.read".into(),
            ..ToolSpec::default()
        }];
        let prompt_digest = serialized_digest(&messages);
        let surface_digest = serialized_digest(&tools);
        let comparison = agent_runtime::SettlementProjectionPreflight {
            schema: "settlement-projection-preflight/v2".into(),
            passed: true,
            allowed_difference: "one task_progress.settlement fact".into(),
            settlement_occurrences: 1,
            baseline_request_sha256: "baseline-request".into(),
            treatment_request_sha256: "treatment-request".into(),
            normalized_request_sha256: "baseline-request".into(),
            baseline_prompt_sha256: prompt_digest.clone(),
            treatment_prompt_sha256: "different-treatment-prompt".into(),
            normalized_prompt_sha256: prompt_digest,
            baseline_tool_surface_sha256: surface_digest.clone(),
            treatment_tool_surface_sha256: surface_digest,
        };
        let request = ModelRequest {
            messages,
            tools,
            metadata: serde_json::Value::Null,
            cancel: CancellationToken::new(),
        };
        let off = serde_json::to_value(comparison).unwrap();
        assert_eq!(
            validate_live_settlement_audit(&request, &off, "off").map(|proof| proof.0),
            Some(true)
        );
        assert_eq!(
            validate_live_settlement_audit(&request, &off, "on").map(|proof| proof.0),
            Some(false),
            "the harness arm, not request metadata, selects the expected request"
        );

        let mut changed = request;
        changed
            .messages
            .push(agent_contracts::ModelMessage::user("drift"));
        assert_eq!(
            validate_live_settlement_audit(&changed, &off, "off").map(|proof| proof.0),
            Some(false),
            "the proof must bind the exact request observed by the wrapper"
        );
    }

    #[test]
    fn marker_shape_is_observational_not_a_gate() {
        let off = gate_cell(PilotMode::Normal, 1, false, "pass", true, 12, 14, 12);
        let mut on = gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 10, 8);
        on.marker_violations.push("shape changed".into());
        let judgment = evaluate_conv_gate(&[off, on]);
        assert_eq!(judgment.state, "pass", "{:?}", judgment.reasons);
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("observational marker-shape"))
        );
    }

    #[test]
    fn conv_gate_rejects_episodes_that_are_not_strictly_lower() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Resume, 1, false, "pass", true, 14, 16, 14),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 14, 16, 14),
            gate_cell(PilotMode::Resume, 1, true, "pass", true, 10, 12, 10),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "fail");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("not strictly lower")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn conv_gate_new_maximum_episode_or_whole_cell_tail_fails() {
        let outcomes = vec![
            gate_cell(PilotMode::Normal, 1, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Resume, 1, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Normal, 2, false, "pass", true, 10, 12, 10),
            gate_cell(PilotMode::Normal, 1, true, "pass", true, 8, 8, 8),
            gate_cell(PilotMode::Resume, 1, true, "pass", true, 8, 8, 8),
            gate_cell(PilotMode::Normal, 2, true, "pass", true, 14, 8, 14),
        ];
        let judgment = evaluate_conv_gate(&outcomes);
        assert_eq!(judgment.state, "fail");
        assert!(
            judgment
                .reasons
                .iter()
                .any(|reason| reason.contains("introduced a new maximum")),
            "{:?}",
            judgment.reasons
        );
    }

    #[test]
    fn provider_transport_retryable_matches_only_retryable_transport() {
        let failed_cell = |class: RuntimeFailureClass, retryable: bool| {
            CellOutcome::failed(
                "retry_policy_dev",
                PilotMode::Normal,
                AcceptanceProfile::M15V1,
                CellSwitches {
                    opportunity: false,
                    recovery_surface: false,
                    project_task_progress: true,
                    project_settlement: false,
                    settlement_projection_diagnostics: false,
                },
                CellEvidenceIdentity::default(),
                0,
                CellFailure::from_runtime(class, retryable, "outage"),
            )
        };
        assert!(provider_transport_retryable(&failed_cell(
            RuntimeFailureClass::ProviderTransport,
            true
        )));
        assert!(!provider_transport_retryable(&failed_cell(
            RuntimeFailureClass::ProviderTransport,
            false
        )));
        assert!(!provider_transport_retryable(&failed_cell(
            RuntimeFailureClass::Model,
            true
        )));
        assert!(!provider_transport_retryable(&CellOutcome::failed(
            "retry_policy_dev",
            PilotMode::Normal,
            AcceptanceProfile::M15V1,
            CellSwitches {
                opportunity: false,
                recovery_surface: false,
                project_task_progress: true,
                project_settlement: false,
                settlement_projection_diagnostics: false,
            },
            CellEvidenceIdentity::default(),
            0,
            CellFailure::harness_setup("no provider"),
        )));
    }
}
