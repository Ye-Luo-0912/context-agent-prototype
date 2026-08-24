//! `retry_policy_dev`: the frozen one-directive development fixture and
//! its deterministic normal/resume gate (LONG_TASK_EVALUATION.md layer 1).
//!
//! The fixture is a small network-free job runner whose retry policy is
//! incomplete across configuration, errors and execution. The public
//! `run_job` signature is frozen. The deterministic gate drives two real
//! runtime instances over the production tool surface with scripted
//! decisions: phase one mutates the workspace (checkpoint debt -> safe
//! point), the harness stops the runtime, restores a fresh instance from
//! the captured checkpoint, continues the same directive via
//! `continue_active_task`, and phase two finishes and closes the task.
//!
//! Layer-1 hidden checks are marker predicates tied to the accepted
//! scripted solution; behavioral cargo-based oracles belong to the later
//! live layers, not this gate.

use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentResult, ContextEngine, ContextIngress, ContextItemSummary, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextStateTransition, MaterializedContext,
    ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    RuntimeEventEnvelope, ScopeId, ScopeKind, ToolCall,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeCheckpoint, RuntimeInstance};
use anyhow::anyhow;
use serde_json::json;

// ---------------------------------------------------------------------------
// Frozen fixture
// ---------------------------------------------------------------------------

/// The single user directive. Both modes share it verbatim.
pub const DIRECTIVE: &str = "Implement a configurable bounded exponential retry policy. \
Retry only transient errors; permanent errors return immediately; `max_attempts` \
includes the first call; delay growth saturates at `max_delay_ms`; preserve the \
public `run_job` signature; add unit/integration coverage, update the README, run \
the project checks, review the diff and report the result.";

/// The frozen seed. Keys are workspace-relative paths.
pub const FIXTURE_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"jobrunner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    ),
    (
        "src/error.rs",
        r#"//! Error taxonomy for the job runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryError {
    /// Transient faults may be retried (network blips, temporary locks).
    Transient(String),
    /// Permanent faults must return immediately (bad input, missing job).
    Permanent(String),
}

impl RetryError {
    /// TODO(retry-policy): classify correctly. Every error is currently
    /// treated as permanent, so nothing is ever retried.
    pub fn is_transient(&self) -> bool {
        false
    }
}
"#,
    ),
    (
        "src/config.rs",
        r#"//! Retry configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryConfig {
    /// Total attempts allowed, including the first call. Must be >= 1.
    pub max_attempts: u32,
    /// Base delay before the first retry, in milliseconds.
    pub base_delay_ms: u64,
}

impl RetryConfig {
    // TODO(retry-policy): delay growth must saturate at `max_delay_ms`.
}
"#,
    ),
    (
        "src/sleeper.rs",
        r#"//! Injected sleeper so tests never really wait.
pub trait Sleeper {
    fn sleep_ms(&mut self, ms: u64);
}

/// Records every requested delay instead of waiting.
#[derive(Debug, Default)]
pub struct FakeSleeper {
    pub sleeps: Vec<u64>,
}

impl Sleeper for FakeSleeper {
    fn sleep_ms(&mut self, ms: u64) {
        self.sleeps.push(ms);
    }
}
"#,
    ),
    (
        "src/lib.rs",
        r#"mod config;
mod error;
mod sleeper;

pub use config::RetryConfig;
pub use error::RetryError;
pub use sleeper::{FakeSleeper, Sleeper};

/// Runs `job` until it succeeds or the retry budget is exhausted.
/// The public signature is frozen; do not change it.
pub fn run_job(
    _config: &RetryConfig,
    _sleeper: &mut dyn Sleeper,
    job: impl FnMut() -> Result<(), RetryError>,
) -> Result<(), RetryError> {
    // TODO(retry-policy): bounded exponential retries. Retry only
    // transient errors; permanent errors return immediately;
    // max_attempts includes the first call; delays saturate at
    // max_delay_ms.
    job()
}
"#,
    ),
    (
        "README.md",
        "# jobrunner\n\nSmall job runner with a pluggable retry policy.\n\n## Status\n\nThe retry policy is not implemented yet: `run_job` performs a single\nattempt and never waits. Configuration currently exposes `max_attempts`\nand `base_delay_ms` only.\n",
    ),
];

/// Seed a fresh fixture workspace. Refuses to overwrite an existing file.
pub fn seed_workspace(root: &Path) -> anyhow::Result<()> {
    for (relative, contents) in FIXTURE_FILES {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            anyhow::bail!("refusing to overwrite existing fixture file {}", relative);
        }
        std::fs::write(&path, contents)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Hidden checks (layer 1: accepted-solution markers)
// ---------------------------------------------------------------------------

/// One named file-content predicate over the final workspace.
struct HiddenCheck {
    path: &'static str,
    name: &'static str,
    accept: fn(&str) -> bool,
}

const HIDDEN_CHECKS: &[HiddenCheck] = &[
    HiddenCheck {
        path: "src/config.rs",
        name: "max_delay_ms field exists exactly once",
        accept: |body| body.matches("pub max_delay_ms: u64").count() == 1,
    },
    HiddenCheck {
        path: "src/error.rs",
        name: "transient classification implemented",
        accept: |body| {
            body.contains("RetryError::Transient(")
                && body.contains("=> true")
                && body.contains("=> false")
        },
    },
    HiddenCheck {
        path: "src/lib.rs",
        name: "retry loop bounds on max_attempts",
        accept: |body| body.contains("1..") && body.contains("max_attempts"),
    },
    HiddenCheck {
        path: "src/lib.rs",
        name: "delay growth saturates at max_delay_ms",
        accept: |body| body.contains(".min("),
    },
    HiddenCheck {
        path: "src/lib.rs",
        name: "public run_job signature preserved",
        accept: |body| body.contains("sleeper: &mut dyn Sleeper,"),
    },
    HiddenCheck {
        path: "README.md",
        name: "README documents the saturation boundary",
        accept: |body| body.contains("`max_delay_ms`"),
    },
];

/// Run every hidden check against the workspace; returns violations.
pub fn hidden_check_violations(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    for check in HIDDEN_CHECKS {
        let path = root.join(check.path);
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        if !(check.accept)(&body) {
            violations.push(format!("{}: {}", check.path, check.name));
        }
    }
    violations
}

// ---------------------------------------------------------------------------
// Deterministic gate harness
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct GateContextEngine;

#[async_trait::async_trait]
impl ContextEngine for GateContextEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
            task: None,
            items: Vec::new(),
            external: Default::default(),
            selected: Vec::new(),
            approx_tokens: 0,
            foreground: Vec::new(),
            diagnostics: Default::default(),
        })
    }
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> AgentResult<agent_contracts::ContextDiagnostics> {
        Ok(Default::default())
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
}

/// Calls the one scripted builtin per model round, in order.
#[derive(Debug)]
struct ScriptGateModel {
    calls: Vec<ToolCall>,
    round: AtomicUsize,
}

impl ScriptGateModel {
    fn new(calls: Vec<ToolCall>) -> Self {
        Self {
            calls,
            round: AtomicUsize::new(0),
        }
    }

    fn call(name: &str, id: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for ScriptGateModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        if let Some(call) = self.calls.get(round) {
            return Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![call.clone()],
                usage: Default::default(),
            });
        }
        Ok(ModelOutput {
            content: "phase finished".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

/// Summary of one deterministic gate run.
#[derive(Debug, Default)]
pub struct GateReport {
    pub resume_committed: u64,
    pub checkpoint_durable: u64,
    pub continuation_started: bool,
    pub task_completed: bool,
    pub duplicated_effect: Option<String>,
    pub hidden_violations: Vec<String>,
    pub order_ok: bool,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        self.resume_committed > 0
            && self.checkpoint_durable >= 2
            && self.continuation_started
            && self.task_completed
            && self.duplicated_effect.is_none()
            && self.hidden_violations.is_empty()
            && self.order_ok
    }
}

/// Run the deterministic normal/resume gate end to end.
pub async fn run_deterministic_gate() -> anyhow::Result<GateReport> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_path_buf();
    seed_workspace(&root)?;
    let report = drive_two_phases(&root).await?;
    Ok(report)
}

async fn spawn_instance(
    root: &Path,
    journal_path: &Path,
    model: Arc<dyn ModelTransport>,
) -> anyhow::Result<RuntimeInstance> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    let dispatcher = tool_runtime::BuiltinToolDispatcher::new(workspace.clone());
    // Both phases share one durable operation journal, so the restored
    // runtime inherits the authority lineage of the stopped one.
    let operation_journal = Arc::new(agent_storage::FileOperationJournal::open(journal_path)?.0);
    let services = agent_runtime::RuntimeServices::try_new(
        CoreAuthorityConfig::default(),
        Arc::new(GateContextEngine),
        model,
        Arc::new(dispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
        agent_runtime::AuthorityRecoveryServices::new(operation_journal, None),
    )?
    .with_artifact_workspace(Arc::new(workspace));
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.handle().start().await?;
    Ok(instance)
}

async fn drain_until(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
    labels: &mut Vec<String>,
    done: impl Fn(&[String]) -> bool,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TaskResumeCommitted { .. } => labels.push("resume".into()),
                RuntimeEvent::CheckpointDurable { .. } => labels.push("durable".into()),
                RuntimeEvent::CheckpointWriteFailed { reason } => {
                    labels.push(format!("write_failed:{reason}"))
                }
                RuntimeEvent::TaskContinuationStarted { .. } => labels.push("continuation".into()),
                RuntimeEvent::TaskCompleted { .. } => labels.push("completed".into()),
                RuntimeEvent::ToolFinished { ref output } if !output.ok => {
                    let preview: String = output.summary.chars().take(200).collect();
                    labels.push(format!("tool_failed:{}:{preview}", output.tool_name))
                }
                RuntimeEvent::Warning { ref message } => labels.push(format!("warn:{message}")),
                RuntimeEvent::TurnCompleted => labels.push("turn_completed".into()),
                RuntimeEvent::RecoveryRequired => labels.push("RECOVERY_REQUIRED".into()),
                RuntimeEvent::Error { ref message } => {
                    let preview: String = message.chars().take(200).collect();
                    labels.push(format!("error:{preview}"))
                }
                RuntimeEvent::TaskProgressUpdated {
                    accepted,
                    anchor_revision,
                    ..
                } => labels.push(format!("progress_ok:{accepted}@{anchor_revision}")),
                _ => {}
            }
        }
        if done(labels) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(anyhow!("gate deadline exceeded waiting for {:?}", labels))
}

async fn drive_two_phases(root: &Path) -> anyhow::Result<GateReport> {
    let mut report = GateReport::default();

    // ---- Phase one: read, progress CAS, durable mutation, finish turn.
    // Directive tools are catalog-cold optionals: the scripted decisions
    // follow the production path and lease them through `capability.manage`
    // before the first call.
    let phase_one_model = Arc::new(ScriptGateModel::new(vec![
        ScriptGateModel::call("fs.read", "r0", json!({"path": "src/config.rs"})),
        ScriptGateModel::call(
            "capability.manage",
            "r1",
            json!({"op": "load", "name": "task.manage"}),
        ),
        ScriptGateModel::call(
            "task.manage",
            "r2",
            json!({
                "base_anchor_revision": 0,
                "plan_progress": ["read the retry configuration"],
                "next_action": "add max_delay_ms"
            }),
        ),
        ScriptGateModel::call(
            "fs.write",
            "r3",
            json!({
                "path": "src/config.rs",
                "content": PHASE_ONE_CONFIG
            }),
        ),
    ]));
    let journal_path = root.join(".gate").join("operations.log");
    let instance_a = spawn_instance(root, &journal_path, phase_one_model).await?;
    let handle_a = instance_a.handle();
    let mut events_a = handle_a.subscribe();
    handle_a
        .set_focus(DIRECTIVE.to_string())
        .await
        .map_err(|e| anyhow!("set_focus: {e}"))?;
    handle_a
        .user_message(DIRECTIVE.to_string())
        .await
        .map_err(|e| anyhow!("user_message: {e}"))?;

    // Wait past the settled mutation: safe point + durable write + turn end.
    let mut labels_a = Vec::new();
    drain_until(
        &mut events_a,
        &mut labels_a,
        |labels| labels.iter().any(|l| l == "turn_completed"),
        tokio::time::Instant::now() + Duration::from_secs(10),
    )
    .await?;
    drain_until(
        &mut events_a,
        &mut labels_a,
        |labels| labels.iter().any(|l| l == "durable"),
        tokio::time::Instant::now() + Duration::from_secs(10),
    )
    .await?;

    // Stop the runtime and capture its planes.
    let checkpoint: RuntimeCheckpoint = instance_a
        .checkpoint()
        .await
        .map_err(|e| anyhow!("phase-a checkpoint: {e}; labels_a={labels_a:?}"))?;
    instance_a
        .shutdown()
        .await
        .map_err(|e| anyhow!("phase-a shutdown: {e}"))?;

    // ---- Restore into a fresh runtime and continue the SAME directive.
    let phase_two_model = Arc::new(ScriptGateModel::new(vec![
        ScriptGateModel::call(
            "capability.manage",
            "c0",
            json!({"op": "load", "name": "task.manage"}),
        ),
        ScriptGateModel::call(
            "task.manage",
            "c1",
            json!({
                "base_anchor_revision": 1,
                "current_interpretation": "policy shape agreed; implement the loop",
                "next_action": "implement transient-only retries with saturation"
            }),
        ),
        ScriptGateModel::call(
            "fs.write",
            "c2",
            json!({"path": "src/lib.rs", "content": PHASE_TWO_LIB}),
        ),
        ScriptGateModel::call(
            "fs.write",
            "c3",
            json!({"path": "src/error.rs", "content": PHASE_TWO_ERROR}),
        ),
        ScriptGateModel::call(
            "fs.write",
            "c4",
            json!({"path": "README.md", "content": PHASE_TWO_README}),
        ),
        ScriptGateModel::call(
            "capability.manage",
            "c5",
            json!({"op": "load", "name": "task.complete"}),
        ),
        ScriptGateModel::call(
            "task.complete",
            "c6",
            json!({"summary": "bounded exponential retry policy implemented and documented"}),
        ),
    ]));
    let instance_b = spawn_instance(root, &journal_path, phase_two_model)
        .await
        .map_err(|e| anyhow!("phase-b spawn: {e}"))?;
    instance_b
        .restore(checkpoint)
        .await
        .map_err(|e| anyhow!("restore: {e}"))?;
    let handle_b = instance_b.handle();
    let mut events_b = handle_b.subscribe();
    handle_b
        .continue_active_task()
        .await
        .map_err(|e| anyhow!("continue_active_task: {e}"))?;

    let mut labels_b = Vec::new();
    drain_until(
        &mut events_b,
        &mut labels_b,
        |labels| labels.iter().any(|l| l == "completed"),
        tokio::time::Instant::now() + Duration::from_secs(15),
    )
    .await?;
    let shutdown = instance_b.shutdown().await;
    shutdown?;

    // ---- Assertions over both event streams and the final workspace.
    report.resume_committed = (labels_a
        .iter()
        .chain(labels_b.iter())
        .filter(|l| **l == "resume")
        .count()) as u64;
    report.checkpoint_durable = (labels_a
        .iter()
        .chain(labels_b.iter())
        .filter(|l| **l == "durable")
        .count()) as u64;
    report.continuation_started = labels_b.iter().any(|l| l == "continuation");
    report.task_completed = labels_b.iter().any(|l| l == "completed");
    // Completion ordering is positional: TurnCompleted, then the final
    // durable checkpoint, then TaskCompleted (adjacent pairs would always
    // miss because the durable event lands between the two).
    let turn_end = labels_b.iter().position(|l| l == "turn_completed");
    let final_durable = labels_b.iter().rposition(|l| l == "durable");
    let completed = labels_b.iter().rposition(|l| l == "completed");
    report.order_ok = matches!((turn_end, final_durable, completed), (Some(t), Some(d), Some(c)) if t < d && d < c);

    // Exactly-once effects: the phase-one marker appears once, and every
    // mutated file equals its accepted final content byte-for-byte.
    for (relative, expected) in FINAL_FILES {
        let actual = std::fs::read_to_string(root.join(relative)).unwrap_or_default();
        if actual != *expected {
            report.duplicated_effect.get_or_insert_with(|| {
                format!("{relative} does not match the accepted final content")
            });
        }
    }
    report.duplicated_effect.get_or_insert_with(|| {
        let body = std::fs::read_to_string(root.join("src/config.rs")).unwrap_or_default();
        if body.matches("pub max_delay_ms: u64").count() > 1 {
            "config.rs was written twice".to_string()
        } else {
            String::new()
        }
    });
    if report
        .duplicated_effect
        .as_deref()
        .is_some_and(str::is_empty)
    {
        report.duplicated_effect = None;
    }

    report.hidden_violations = hidden_check_violations(root);
    Ok(report)
}

const PHASE_ONE_CONFIG: &str = r#"//! Retry configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryConfig {
    /// Total attempts allowed, including the first call. Must be >= 1.
    pub max_attempts: u32,
    /// Base delay before the first retry, in milliseconds.
    pub base_delay_ms: u64,
    /// Upper bound for any computed delay, in milliseconds.
    pub max_delay_ms: u64,
}
"#;

const PHASE_TWO_LIB: &str = r#"mod config;
mod error;
mod sleeper;

pub use config::RetryConfig;
pub use error::RetryError;
pub use sleeper::{FakeSleeper, Sleeper};

/// Runs `job` until it succeeds or the retry budget is exhausted.
/// The public signature is frozen; do not change it.
pub fn run_job(
    config: &RetryConfig,
    sleeper: &mut dyn Sleeper,
    mut job: impl FnMut() -> Result<(), RetryError>,
) -> Result<(), RetryError> {
    let attempts = config.max_attempts.max(1);
    for attempt in 1..=attempts {
        match job() {
            Ok(()) => return Ok(()),
            Err(error) if !error.is_transient() => return Err(error),
            Err(error) if attempt == attempts => return Err(error),
            Err(_) => {}
        }
        let shift = attempt.saturating_sub(1);
        let raw = config.base_delay_ms.saturating_mul(1u64 << shift.min(16));
        sleeper.sleep_ms(raw.min(config.max_delay_ms));
    }
    unreachable!("retry loop always returns inside the bounded attempts")
}
"#;

const PHASE_TWO_ERROR: &str = r#"//! Error taxonomy for the job runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryError {
    /// Transient faults may be retried (network blips, temporary locks).
    Transient(String),
    /// Permanent faults must return immediately (bad input, missing job).
    Permanent(String),
}

impl RetryError {
    /// Only transient faults may be retried.
    pub fn is_transient(&self) -> bool {
        match self {
            RetryError::Transient(_) => true,
            RetryError::Permanent(_) => false,
        }
    }
}
"#;

const PHASE_TWO_README: &str = r#"# jobrunner

Small job runner with a pluggable retry policy.

## Retry policy

- only transient errors are retried; permanent errors return immediately
- `max_attempts` includes the first call
- delay growth starts at `base_delay_ms` and saturates at `max_delay_ms`
- waits are injected through [`Sleeper`], so tests never really wait
"#;

const FINAL_FILES: &[(&str, &str)] = &[
    ("src/config.rs", PHASE_ONE_CONFIG),
    ("src/lib.rs", PHASE_TWO_LIB),
    ("src/error.rs", PHASE_TWO_ERROR),
    ("README.md", PHASE_TWO_README),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// CI-reachable form of the `--long-task-gate` CLI run: the scripted
    /// normal/resume pair over the real production tool surface must pass
    /// every acceptance predicate end to end.
    #[tokio::test]
    async fn deterministic_gate_passes() {
        let report = run_deterministic_gate().await.expect("gate run completes");
        assert!(
            report.passed(),
            "resume_committed={} checkpoint_durable={} continuation={} completed={} \
             duplicated_effect={:?} order_ok={} hidden_violations={:?}",
            report.resume_committed,
            report.checkpoint_durable,
            report.continuation_started,
            report.task_completed,
            report.duplicated_effect,
            report.order_ok,
            report.hidden_violations,
        );
    }

    #[test]
    fn seed_refuses_to_overwrite_an_existing_fixture_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "operator content").unwrap();
        assert!(seed_workspace(dir.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
            "operator content",
        );
    }
}
