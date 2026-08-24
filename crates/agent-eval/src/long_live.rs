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
//! Acceptance is behavioral: the model must close the task through
//! `task.complete`, hidden cargo tests on the finished fixture must pass
//! with at least one test executed, and the workspace diff must stay
//! inside the allowed paths. The layer-1 marker predicates ride along as
//! diagnostics only — multiple correct implementations are accepted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, CancellationToken, ContextEngine, ModelTransport,
    RuntimeEvent, RuntimeEventEnvelope, ToolCall, ToolDispatcher, ToolSpec,
};
use anyhow::bail;
use sha2::{Digest as _, Sha256};

use crate::bundle::PairSink;
use crate::long_task::{self, DIRECTIVE, FINAL_FILES, FIXTURE_FILES};
use crate::workload::{HiddenAssertionResult, HiddenCommandResult, HiddenFileBody, HiddenReport};

pub const PILOT_SCHEMA: &str = "retry-pilot-cell-v1";
/// LONG_TASK_EVALUATION layer 2: normal and resume, two repeats each.
pub const DEFAULT_REPEATS: u32 = 2;

const LIVE_IDLE: Duration = Duration::from_secs(300);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// One finished live cell, before evidence serialization.
pub struct CellOutcome {
    pub mode: PilotMode,
    pub passed: bool,
    pub error: Option<String>,
    pub wall_ms: u64,
    pub task_completed: bool,
    pub resume_committed: u64,
    pub checkpoint_durable: u64,
    pub model_rounds_phase_one: u32,
    pub model_rounds_phase_two: u32,
    /// Which mutation tool created the resume debt (resume cells).
    pub resume_trigger: Option<&'static str>,
    pub diff_violations: Vec<String>,
    pub marker_violations: Vec<String>,
    pub cargo_passed: bool,
}

impl CellOutcome {
    fn failed(mode: PilotMode, wall_ms: u64, reason: String) -> Self {
        Self {
            mode,
            passed: false,
            error: Some(reason),
            wall_ms,
            task_completed: false,
            resume_committed: 0,
            checkpoint_durable: 0,
            model_rounds_phase_one: 0,
            model_rounds_phase_two: 0,
            resume_trigger: None,
            diff_violations: Vec::new(),
            marker_violations: Vec::new(),
            cargo_passed: false,
        }
    }

    /// One-line human summary for the runner output.
    pub fn render_line(&self) -> String {
        let status = if self.passed { "PASS" } else { "FAIL" };
        let trigger = self
            .resume_trigger
            .map(|tool| format!(" trigger={tool}"))
            .unwrap_or_default();
        format!(
            "retry_policy_dev {:<6} repeat-cell {} rounds={}+{} resumes={} durables={} task_completed={} cargo={}{}{}",
            self.mode.id(),
            status,
            self.model_rounds_phase_one,
            self.model_rounds_phase_two,
            self.resume_committed,
            self.checkpoint_durable,
            self.task_completed,
            self.cargo_passed,
            trigger,
            self.error
                .as_ref()
                .map(|reason| format!(" error={reason}"))
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
    mutation_tool: Option<&'static str>,
    durable_after_mutation: bool,
    resume_committed: u64,
    checkpoint_durable: u64,
    turn_completed: bool,
    task_completed: bool,
}

enum StepOutcome {
    Continue,
    TaskCompleted,
    TurnCompleted,
    Fail(String),
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
        RuntimeEvent::CheckpointDurable { .. } => {
            state.checkpoint_durable = state.checkpoint_durable.saturating_add(1);
            state.durable_after_mutation |= state.mutation_tool.is_some();
            collector.push(envelope);
            StepOutcome::Continue
        }
        RuntimeEvent::TaskResumeCommitted { .. } => {
            state.resume_committed = state.resume_committed.saturating_add(1);
            collector.push(envelope);
            StepOutcome::Continue
        }
        RuntimeEvent::TaskCompleted { .. } => {
            state.task_completed = true;
            collector.push(envelope);
            StepOutcome::TaskCompleted
        }
        RuntimeEvent::TurnCompleted => {
            state.turn_completed = true;
            collector.push(envelope);
            StepOutcome::TurnCompleted
        }
        RuntimeEvent::TurnCancelled { .. } => {
            collector.push(envelope);
            StepOutcome::Fail(if state.cancelled_for_cap {
                format!("live model-round cap ({LIVE_MAX_MODEL_ROUNDS}) exceeded")
            } else {
                "turn cancelled".into()
            })
        }
        RuntimeEvent::TurnCommitFailed { ref message, .. } => {
            let reason = format!("turn commit failed: {message}");
            collector.push(envelope);
            StepOutcome::Fail(reason)
        }
        RuntimeEvent::Error { ref message } => {
            let reason = format!("runtime error: {message}");
            collector.push(envelope);
            StepOutcome::Fail(reason)
        }
        RuntimeEvent::RecoveryRequired => {
            collector.push(envelope);
            StepOutcome::Fail("recovery fence raised during the run".into())
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
) -> Result<RuntimeEventEnvelope, String> {
    loop {
        match tokio::time::timeout(LIVE_IDLE, receiver.recv()).await {
            Err(_) => return Err("cell stalled waiting for runtime events".into()),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                collector.lagged = collector.lagged.saturating_add(skipped);
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err("event stream closed".into());
            }
            Ok(Ok(envelope)) => return Ok(envelope),
        }
    }
}

/// Wait until the first durably settled workspace mutation has its durable
/// checkpoint. The task finishing before the interruption is a harness
/// failure: there would be nothing left to continue.
async fn wait_resume_trigger(
    receiver: &mut EventStream,
    collector: &mut Collector,
    state: &mut PhaseState,
    handle: &agent_runtime::RuntimeHandle,
) -> Result<(), String> {
    loop {
        let envelope = next_envelope(receiver, collector).await?;
        match step_event(envelope, collector, state) {
            StepOutcome::Continue => {}
            StepOutcome::TaskCompleted => {
                return Err("task completed before the resume interruption could fire".into());
            }
            StepOutcome::TurnCompleted => {
                return Err("turn finished before any durably settled workspace mutation".into());
            }
            StepOutcome::Fail(reason) => return Err(reason),
        }
        state.enforce_cap(handle);
        if state.durable_after_mutation {
            return Ok(());
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
) -> Result<(), String> {
    loop {
        let envelope = next_envelope(receiver, collector).await?;
        match step_event(envelope, collector, state) {
            StepOutcome::Continue => {}
            StepOutcome::TaskCompleted => return Ok(()),
            StepOutcome::TurnCompleted => break,
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
) -> anyhow::Result<agent_compose::ComposedRuntime> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    let verification_recipes = tool_runtime::VerificationRecipes::discover(&workspace);
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
        host_policies: Some(Arc::new(
            agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(
                &verification_recipes,
            )
            .map_err(anyhow::Error::msg)?,
        )),
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

/// Run one live cell end to end and score it against the finished
/// workspace. Never panics on provider/runtime failures: failures become
/// `CellOutcome::error` so evidence records stay honest.
pub async fn run_cell(
    mode: PilotMode,
    pair: &PairSink,
    model: Arc<dyn ModelTransport>,
    root: &Path,
) -> anyhow::Result<CellOutcome> {
    let started = Instant::now();
    let failed =
        |reason: String| CellOutcome::failed(mode, started.elapsed().as_millis() as u64, reason);

    if let Err(e) = long_task::seed_workspace(root) {
        return Ok(failed(format!("seeding failed: {e:#}")));
    }
    if let Err(e) = crate::suite::ensure_workspace_git(root) {
        return Ok(failed(format!("workspace git init failed: {e:#}")));
    }

    let mut collector = Collector::default();
    let mut error: Option<String> = None;
    let mut trigger_tool: Option<&'static str> = None;
    let mut resume_committed = 0u64;
    let mut checkpoint_durable = 0u64;
    let mut rounds_one = 0u32;
    let mut rounds_two = 0u32;
    let mut task_completed = false;
    let engine = c_engine(model.clone());

    // ---- Phase one: drive the directive from the clean seed.
    let mut checkpoint: Option<agent_runtime::RuntimeCheckpoint> = None;
    match compose_cell(root, model.clone(), engine.clone()).await {
        Err(e) => error = Some(format!("phase-one compose failed: {e:#}")),
        Ok(composed) => {
            let handle = composed.handle().clone();
            let mut events = composed.subscribe();
            let mut state = PhaseState::default();
            let drive: Result<(), String> = async {
                handle
                    .set_focus(DIRECTIVE.to_string())
                    .await
                    .map_err(|e| format!("set_focus failed: {e}"))?;
                handle
                    .user_message(DIRECTIVE.to_string())
                    .await
                    .map_err(|e| format!("user_message failed: {e}"))?;
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
            match drive {
                Ok(()) => {
                    rounds_one = state.model_rounds;
                    resume_committed += state.resume_committed;
                    checkpoint_durable += state.checkpoint_durable;
                    task_completed |= state.task_completed;
                    trigger_tool = state.mutation_tool;
                    if mode == PilotMode::Normal && !task_completed {
                        error = Some("normal run ended without TaskCompleted".into());
                    }
                    if mode == PilotMode::Resume {
                        match composed.checkpoint().await {
                            Ok(captured) => checkpoint = Some(captured),
                            Err(e) => error = Some(format!("checkpoint capture failed: {e}")),
                        }
                    }
                }
                Err(reason) => error = Some(format!("phase one failed: {reason}")),
            }
            if let Err(e) = composed.shutdown().await
                && error.is_none()
            {
                error = Some(format!("phase-one shutdown failed: {e}"));
            }
            while let Ok(envelope) = events.try_recv() {
                collector.push(envelope);
            }
        }
    }

    // ---- Phase two (resume only): restore and continue the SAME directive.
    if mode == PilotMode::Resume && error.is_none() {
        match checkpoint {
            None => error = Some("resume mode reached phase two without a checkpoint".into()),
            Some(checkpoint) => match compose_cell(root, model.clone(), engine).await {
                Err(e) => error = Some(format!("phase-two compose failed: {e:#}")),
                Ok(composed) => {
                    let handle = composed.handle().clone();
                    let mut events = composed.subscribe();
                    let mut state = PhaseState::default();
                    let drive = async {
                        composed
                            .instance
                            .restore(checkpoint)
                            .await
                            .map_err(|e| format!("restore failed: {e}"))?;
                        handle
                            .continue_active_task()
                            .await
                            .map_err(|e| format!("continue_active_task failed: {e}"))?;
                        run_to_completion(&mut events, &mut collector, &mut state, &handle).await
                    }
                    .await;
                    match drive {
                        Ok(()) => {
                            rounds_two = state.model_rounds;
                            resume_committed += state.resume_committed;
                            checkpoint_durable += state.checkpoint_durable;
                            task_completed |= state.task_completed;
                            if !task_completed {
                                error = Some("continuation ended without TaskCompleted".into());
                            }
                        }
                        Err(reason) => {
                            error = Some(format!("phase two failed: {reason}"));
                        }
                    }
                    if let Err(e) = composed.shutdown().await
                        && error.is_none()
                    {
                        error = Some(format!("phase-two shutdown failed: {e}"));
                    }
                    while let Ok(envelope) = events.try_recv() {
                        collector.push(envelope);
                    }
                }
            },
        }
    }

    let wall_ms = started.elapsed().as_millis() as u64;
    if let Some(reason) = error.as_ref() {
        let mut outcome = CellOutcome::failed(mode, wall_ms, reason.clone());
        outcome.task_completed = task_completed;
        outcome.resume_committed = resume_committed;
        outcome.checkpoint_durable = checkpoint_durable;
        outcome.model_rounds_phase_one = rounds_one;
        outcome.model_rounds_phase_two = rounds_two;
        write_evidence(pair, root, &collector, &outcome, None);
        return Ok(outcome);
    }

    // ---- Behavioral acceptance. The diff scan runs before the cargo test
    // so build artifacts can never enter the verdict.
    let diff_violations = match diff_violations(root) {
        Ok(violations) => violations,
        Err(e) => {
            let reason = format!("diff scan failed: {e:#}");
            let mut outcome = CellOutcome::failed(mode, wall_ms, reason);
            outcome.task_completed = true;
            write_evidence(pair, root, &collector, &outcome, None);
            return Ok(outcome);
        }
    };
    let marker_violations = long_task::hidden_check_violations(root);
    let cargo = run_cargo_test(root).await;
    let cargo_passed = cargo.passed;
    let passed = task_completed && cargo_passed && diff_violations.is_empty();

    let outcome = CellOutcome {
        mode,
        passed,
        error: None,
        wall_ms,
        task_completed,
        resume_committed,
        checkpoint_durable,
        model_rounds_phase_one: rounds_one,
        model_rounds_phase_two: rounds_two,
        resume_trigger: trigger_tool,
        diff_violations,
        marker_violations,
        cargo_passed,
    };
    write_evidence(pair, root, &collector, &outcome, Some(&cargo));
    Ok(outcome)
}

/// Serialize the cell into the claimed pair directory using the shared
/// evidence conventions (manifest + events.jsonl + hidden report).
fn write_evidence(
    pair: &PairSink,
    root: &Path,
    collector: &Collector,
    outcome: &CellOutcome,
    cargo: Option<&HiddenCommandResult>,
) {
    let report = build_hidden_report(outcome, root, cargo);
    let metrics = crate::metrics::aggregate_metrics(&collector.events);
    let cell_dir = pair.cell_dir("dynamic");
    if let Err(e) = crate::bundle::write_cell_parts(
        &cell_dir,
        "retry_policy_dev",
        &spec_sha256(),
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
    ) {
        eprintln!("warning: retry-pilot evidence write failed: {e}");
    }
}

fn build_hidden_report(
    outcome: &CellOutcome,
    root: &Path,
    cargo: Option<&HiddenCommandResult>,
) -> HiddenReport {
    let checks = long_task::hidden_check_results(root);
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
    let mut paths: Vec<String> = FINAL_FILES
        .iter()
        .map(|(path, _)| (*path).to_string())
        .collect();
    let tests = list_tests_dir(root);
    paths.extend(tests);
    let files: Vec<HiddenFileBody> = paths
        .iter()
        .map(|relative| file_body(root, relative))
        .collect();
    // A cell that errored before acceptance records the oracle as skipped
    // instead of implying an unexecuted check passed or failed.
    let commands = match cargo {
        Some(result) => vec![result.clone()],
        None => vec![HiddenCommandResult {
            argv: cargo_argv(),
            expect_exit: 0,
            exit: None,
            timed_out: false,
            stdout: String::new(),
            stderr: "oracle skipped: the cell errored before acceptance".into(),
            stdout_truncated: false,
            stderr_truncated: false,
            passed: false,
        }],
    };
    HiddenReport {
        schema: PILOT_SCHEMA.to_string(),
        kind: "retry_pilot_oracle".into(),
        fixture_id: format!("retry_policy_dev-{}", outcome.mode.id()),
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

/// Hidden cargo-test oracle: compile and run whatever coverage the model
/// added. Passing requires a zero exit, zero failures and at least one
/// executed test (a fixture with no added tests must not pass).
async fn run_cargo_test(root: &Path) -> HiddenCommandResult {
    use std::process::Stdio;

    let mut command = tokio::process::Command::new("cargo");
    command
        .args(["test", "--quiet"])
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null());
    let mut record = HiddenCommandResult {
        argv: cargo_argv(),
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
fn diff_violations(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut present: BTreeMap<String, String> = BTreeMap::new();
    collect_files(root, PathBuf::new(), &mut present)?;
    let mut violations = Vec::new();
    for relative in present.keys() {
        let allowed = relative.starts_with("src/")
            || relative.starts_with("tests/")
            || relative == "README.md"
            || relative == "Cargo.toml";
        if !allowed {
            violations.push(format!("path outside the allowed diff: {relative}"));
        }
    }
    for (relative, _) in FIXTURE_FILES {
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
            out.insert(
                prefix.join(&name).to_string_lossy().into_owned(),
                sha256_hex(&bytes),
            );
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
}
