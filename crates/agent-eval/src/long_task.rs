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
    AgentResult, CompletionOpportunityDisposition, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, RuntimeEventEnvelope, ScopeId, ScopeKind, ToolCall,
    VerificationCoverageDeclaration,
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
    for (path, name, passed) in hidden_check_results(root) {
        if !passed {
            violations.push(format!("{path}: {name}"));
        }
    }
    violations
}

/// Per-check pass/fail against the workspace, for evidence assertions.
pub fn hidden_check_results(root: &Path) -> Vec<(&'static str, &'static str, bool)> {
    HIDDEN_CHECKS
        .iter()
        .map(|check| {
            let body = std::fs::read_to_string(root.join(check.path)).unwrap_or_default();
            (check.path, check.name, (check.accept)(&body))
        })
        .collect()
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
            required_item_ids: Vec::new(),
            required_misses: Default::default(),
            optional_misses: Default::default(),
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
    #[allow(dead_code)]
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

const GATE_ACCEPTANCE_DOMAIN: &str = "retry-policy-build";
const GATE_ACCEPTANCE_CRITERION: &str =
    "the completed retry-policy crate compiles against its frozen public module graph";

/// Build the verifier catalog and the exact host declaration consumed by
/// Runtime's completion gate. Compiler output stays under runtime state;
/// every source input that rustc may load is hashed into the PASS identity.
fn gate_verification_projection() -> anyhow::Result<(
    tool_runtime::VerificationRecipes,
    VerificationCoverageDeclaration,
)> {
    let exact_inputs = FIXTURE_FILES
        .iter()
        .filter(|(path, _)| path.starts_with("src/"))
        .map(|(path, _)| (*path).to_string())
        .collect();
    let recipe = tool_runtime::VerificationRecipe::new(
        "gate",
        "Compile the retry-policy fixture without mutating its source inputs",
        "retry-policy-build-v1",
        vec![
            "rustc".into(),
            "--edition=2021".into(),
            "--crate-name".into(),
            "jobrunner".into(),
            "--crate-type".into(),
            "lib".into(),
            "--emit=metadata=.focus-agent/artifacts/jobrunner-gate.rmeta".into(),
            "src/lib.rs".into(),
        ],
    )
    .map_err(|error| anyhow!("gate recipe: {error}"))?
    .with_exact_current_world_reuse()
    .with_exact_inputs(exact_inputs)
    .map_err(|error| anyhow!("gate exact inputs: {error}"))?
    .with_coverage_domain(GATE_ACCEPTANCE_DOMAIN)
    .map_err(|error| anyhow!("gate coverage domain: {error}"))?;
    let recipes = tool_runtime::VerificationRecipes::new(vec![recipe])
        .and_then(|recipes| {
            recipes.with_domains(vec![tool_runtime::VerificationCoverageDomain {
                domain_id: GATE_ACCEPTANCE_DOMAIN.into(),
                declaration_revision: 1,
                members: vec!["gate".into()],
            }])
        })
        .map_err(|error| anyhow!("gate verification projection: {error}"))?;
    let declaration = recipes
        .coverage_declaration(GATE_ACCEPTANCE_DOMAIN)
        .cloned()
        .ok_or_else(|| anyhow!("gate acceptance declaration is missing"))?;
    Ok((recipes, declaration))
}

/// Install host-owned completion authority while the actor is idle. The
/// model can later earn coverage only by running the matching trusted recipe.
async fn declare_gate_acceptance(
    handle: &agent_runtime::RuntimeHandle,
    declaration: &VerificationCoverageDeclaration,
) -> anyhow::Result<()> {
    let task = handle
        .list_tasks()
        .await?
        .into_iter()
        .find(|task| task.status == agent_runtime::TaskStatus::Active)
        .ok_or_else(|| anyhow!("gate task is not active after set_focus"))?;
    handle
        .patch_task_anchor(
            task.id,
            task.anchor_revision,
            agent_runtime::task::AnchorPatch {
                completion_policy: Some(agent_runtime::TaskCompletionPolicy::EvidenceRequired),
                acceptance_criteria: Some(vec![agent_runtime::AcceptanceCriterion::declared(
                    GATE_ACCEPTANCE_CRITERION,
                    declaration,
                )]),
                ..agent_runtime::task::AnchorPatch::default()
            },
        )
        .await?;
    Ok(())
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
    opportunity_switch: bool,
) -> anyhow::Result<RuntimeInstance> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    // The exact same projection supplies both the executable recipe and the
    // completion-authority snapshot. Cold instances must recompose the same
    // declaration instead of inheriting an unchecked checkpoint claim.
    let (recipes, _) = gate_verification_projection()?;
    let dispatcher = tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
        workspace.clone(),
        tool_runtime::ToolLifecycleConfig::default(),
        recipes,
    );
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
    .with_artifact_workspace(Arc::new(workspace))
    .with_project_completion_opportunity(opportunity_switch);
    // An intentional standalone composition: services are built directly
    // and the host starts with no modules before the runtime is spawned.
    let mut host = ModuleHost::new();
    host.start().await?;
    let instance = RuntimeInstance::spawn(host, services);
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
                RuntimeEvent::TaskResumeCommitted { ref debt, .. } => {
                    if debt.iter().any(|reason| reason == "opportunity_offered") {
                        labels.push("resume:opportunity_offered".into());
                    } else {
                        labels.push("resume".into());
                    }
                }
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
                "base_anchor_revision": 1,
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
    let instance_a = spawn_instance(root, &journal_path, phase_one_model, false).await?;
    let handle_a = instance_a.handle();
    let mut events_a = handle_a.subscribe();
    // A second independent subscription keeps the durable tuple observable
    // even after the label drains consumed their own view of the stream.
    let mut events_capture = handle_a.subscribe();
    handle_a
        .set_focus(DIRECTIVE.to_string())
        .await
        .map_err(|e| anyhow!("set_focus: {e}"))?;
    let (_, gate_acceptance) = gate_verification_projection()?;
    declare_gate_acceptance(handle_a, &gate_acceptance)
        .await
        .map_err(|e| anyhow!("declare acceptance: {e}"))?;
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

    // ---- Cold boundary: the ONLY thing crossing into phase two is the
    // exact acknowledged tuple (artifact, checksum, sequence, capability
    // generation) captured from the durable event. The in-memory
    // checkpoint object is dropped with the phase-one runtime.
    let mut durable_tuple: Option<(String, String, u64, u64)> = None;
    while let Ok(envelope) = events_capture.try_recv() {
        if let RuntimeEvent::CheckpointDurable {
            ref artifact,
            ref checksum,
            sequence,
            capability_generation,
            ..
        } = envelope.event
        {
            durable_tuple = Some((
                artifact.clone(),
                checksum.clone(),
                sequence,
                capability_generation,
            ));
        }
    }
    let (artifact, ack_checksum, acked_sequence, acked_generation) =
        durable_tuple.ok_or_else(|| anyhow!("phase-a never acknowledged a durable checkpoint"))?;
    instance_a
        .shutdown()
        .await
        .map_err(|e| anyhow!("phase-a shutdown: {e}"))?;

    // Phase two loads THAT artifact cold: envelope checksum verified by the
    // store, then the ack digest and snapshot sequence cross-checked against
    // the tuple before anything restores. Any mismatch bails here — phase B
    // must never silently resume from a stale or foreign snapshot.
    let store = agent_runtime::CheckpointStore::new(root.join(".focus-agent").join("checkpoints"));
    let payload = store
        .load_verified(&artifact)
        .await
        .map_err(|e| anyhow!("cold load of {artifact}: {e}"))?;
    {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(&payload);
        let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        anyhow::ensure!(
            digest_hex == ack_checksum,
            "cold artifact digest {digest_hex} does not match the acknowledged checksum {ack_checksum}"
        );
    }
    let checkpoint: RuntimeCheckpoint = serde_json::from_slice(&payload)
        .map_err(|e| anyhow!("cold payload deserialization: {e}"))?;
    anyhow::ensure!(
        checkpoint.snapshot_sequence == acked_sequence,
        "cold artifact sequence {} does not match the acknowledged sequence {acked_sequence}",
        checkpoint.snapshot_sequence
    );
    anyhow::ensure!(
        checkpoint.capability_generation == acked_generation,
        "cold artifact capability generation {} does not match the acknowledged {acked_generation}",
        checkpoint.capability_generation
    );
    checkpoint
        .validate()
        .map_err(|e| anyhow!("cold artifact failed validation: {e}"))?;

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
                "base_anchor_revision": 2,
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
            "task.manage",
            "c5",
            json!({
                "base_anchor_revision": 3,
                "plan_progress": [
                    "implemented bounded transient retries",
                    "updated the public error taxonomy and README"
                ],
                "open_loops": [],
                "next_action": ""
            }),
        ),
        ScriptGateModel::call("verify.run", "c6", json!({"recipe_id": "gate"})),
        ScriptGateModel::call(
            "capability.manage",
            "c7",
            json!({"op": "load", "name": "task.complete"}),
        ),
        ScriptGateModel::call(
            "task.complete",
            "c8",
            json!({"summary": "bounded exponential retry policy implemented and documented"}),
        ),
    ]));
    let instance_b = spawn_instance(root, &journal_path, phase_two_model, false)
        .await
        .map_err(|e| anyhow!("phase-b spawn: {e}"))?;
    instance_b
        .restore(checkpoint)
        .await
        .map_err(|e| anyhow!("restore: {e}"))?;
    let handle_b = instance_b.handle();
    let mut events_b = handle_b.subscribe();
    // Third subscription: keeps the FINAL (terminal) artifact tuple for the
    // post-completion cold restore below.
    let mut events_b_capture = handle_b.subscribe();
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

    // ---- Terminal cold restore: the completion ack's exact tuple is the
    // only handle phase C receives. It must load, digest-match, validate,
    // and restore into a fresh instance whose task plane shows the task
    // Completed — the durable truth chain closing where it started.
    let mut final_tuple: Option<(String, String, u64, u64)> = None;
    while let Ok(envelope) = events_b_capture.try_recv() {
        if let RuntimeEvent::CheckpointDurable {
            ref artifact,
            ref checksum,
            sequence,
            capability_generation,
            ..
        } = envelope.event
        {
            final_tuple = Some((
                artifact.clone(),
                checksum.clone(),
                sequence,
                capability_generation,
            ));
        }
    }
    let (final_artifact, final_checksum, final_sequence, final_generation) =
        final_tuple.ok_or_else(|| anyhow!("phase-b never acknowledged the terminal checkpoint"))?;
    let terminal_payload = store
        .load_verified(&final_artifact)
        .await
        .map_err(|e| anyhow!("cold load of terminal {final_artifact}: {e}"))?;
    {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(&terminal_payload);
        let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        anyhow::ensure!(
            digest_hex == final_checksum,
            "terminal artifact digest does not match its acknowledgement"
        );
    }
    let terminal_checkpoint: RuntimeCheckpoint = serde_json::from_slice(&terminal_payload)
        .map_err(|e| anyhow!("terminal payload deserialization: {e}"))?;
    anyhow::ensure!(
        terminal_checkpoint.snapshot_sequence == final_sequence
            && terminal_checkpoint.capability_generation == final_generation,
        "terminal artifact identity tuple drifted from its acknowledgement"
    );
    terminal_checkpoint
        .validate()
        .map_err(|e| anyhow!("terminal artifact failed validation: {e}"))?;
    let instance_c = spawn_instance(
        root,
        &journal_path,
        Arc::new(ScriptGateModel::new(Vec::new())),
        false,
    )
    .await
    .map_err(|e| anyhow!("phase-c spawn: {e}"))?;
    instance_c
        .restore(terminal_checkpoint)
        .await
        .map_err(|e| anyhow!("terminal cold restore: {e}"))?;
    let completed_restored = instance_c
        .handle()
        .list_tasks()
        .await?
        .iter()
        .any(|task| matches!(task.status, agent_runtime::TaskStatus::Completed));
    anyhow::ensure!(
        completed_restored,
        "the fresh instance must see the completed task plane"
    );
    let shutdown = instance_c.shutdown().await;
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

// ---------------------------------------------------------------------------
// CompletionOpportunity deterministic off/on replay (Roadmap item 8 freeze)
// ---------------------------------------------------------------------------

/// What one arm of the replay observed, body-free.
#[derive(Debug, Default)]
#[allow(dead_code)]
struct OpportunityArmObservation {
    turn_completed: bool,
    task_completed: bool,
    /// Every opportunity event regardless of disposition.
    opportunity_events_total: u64,
    offered_keys: Vec<String>,
    completed_keys: Vec<String>,
    /// Surface decisions that preferred `task.complete`, as positions in
    /// the ordered marker stream below.
    surfaced_positions: Vec<usize>,
    /// Positional markers for order-sensitive assertions.
    markers: Vec<&'static str>,
}

/// Summary of the off/on pair.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct OpportunityReplayReport {
    pub on_offered_once: bool,
    /// Diagnostic: when the offer's checkpoint debt survives to a later
    /// resume commit it shows here; when the leased decision completes the
    /// task in the same window the terminal freeze retires it instead, so
    /// absence is not a defect. The crash-window proof of once-per-basis
    /// offer survival belongs to the cold-resume matrix.
    pub on_offer_debt_owed: bool,
    pub on_surfaced_after_offer: bool,
    pub on_called_after_offer: bool,
    pub on_completed: bool,
    pub on_completed_key_matches_offer: bool,
    pub off_opportunity_events_zero: bool,
    pub off_never_surfaced: bool,
}

impl OpportunityReplayReport {
    #[allow(dead_code)]
    pub fn passed(&self) -> bool {
        self.on_offered_once
            && self.on_surfaced_after_offer
            && self.on_called_after_offer
            && self.on_completed
            && self.on_completed_key_matches_offer
            && self.off_opportunity_events_zero
            && self.off_never_surfaced
    }
}

/// Freeze the already-satisfied-task replay the item-8 promotion gate will
/// reuse: a scripted task performs one durable mutation followed by one
/// trusted verification pass under an unchanged basis. With the candidate
/// enabled that is exactly once-per-basis eligibility — one offer, the
/// leased surface decision closes the task without any explicit load — and
/// with it disabled the identical script must produce zero opportunity
/// observations and never surface `task.complete` from derived readiness.
///
/// RETIRED 2026-08-28: the candidate ended by its decision-grade gate and
/// `task.complete` joined the always-loaded surface, so the arms no longer
/// differ by presence.
#[allow(dead_code)]
pub async fn run_opportunity_replay() -> anyhow::Result<OpportunityReplayReport> {
    let mut report = OpportunityReplayReport::default();

    let scripted_prefix = |id: &str| -> Vec<ToolCall> {
        vec![
            ScriptGateModel::call(
                "fs.write",
                &format!("{id}-write"),
                json!({"path": "src/config.rs", "content": PHASE_ONE_CONFIG}),
            ),
            ScriptGateModel::call(
                "verify.run",
                &format!("{id}-verify"),
                json!({"recipe_id": "gate"}),
            ),
        ]
    };

    // ---- ON arm: after the pass lands, the next decision closes the task
    // through the lease alone (no capability.manage load of task.complete).
    let mut on_calls = scripted_prefix("on");
    on_calls.push(ScriptGateModel::call(
        "task.complete",
        "on-complete",
        json!({"summary": "retry policy implemented and verified"}),
    ));
    let on_dir = tempfile::tempdir()?;
    seed_workspace(on_dir.path())?;
    let on = drive_opportunity_arm(on_dir.path(), true, on_calls).await?;

    // ---- OFF arm: identical work and finish, switch disabled.
    let off_dir = tempfile::tempdir()?;
    seed_workspace(off_dir.path())?;
    let off = drive_opportunity_arm(off_dir.path(), false, scripted_prefix("off")).await?;

    report.on_offered_once = on.offered_keys.len() == 1;
    report.on_completed = on.task_completed;
    report.on_completed_key_matches_offer =
        on.completed_keys.len() == 1 && on.completed_keys == on.offered_keys;
    // Order is positional: the offer must land before the leased surface
    // decision and before the model's call.
    let offer_index = on.markers.iter().position(|marker| *marker == "offered");
    let called_index = on.markers.iter().position(|marker| *marker == "called");
    report.on_surfaced_after_offer = offer_index
        .is_some_and(|offer| on.surfaced_positions.iter().any(|surface| *surface > offer));
    report.on_called_after_offer =
        matches!((offer_index, called_index), (Some(o), Some(c)) if o < c);
    report.off_opportunity_events_zero = off.opportunity_events_total == 0;
    report.off_never_surfaced = off.surfaced_positions.is_empty();
    Ok(report)
}

#[allow(dead_code)]
async fn drive_opportunity_arm(
    root: &Path,
    opportunity_switch: bool,
    calls: Vec<ToolCall>,
) -> anyhow::Result<OpportunityArmObservation> {
    let journal = root.join(".gate").join(format!(
        "{}-operations.log",
        if opportunity_switch { "on" } else { "off" }
    ));
    let instance = spawn_instance(
        root,
        &journal,
        Arc::new(ScriptGateModel::new(calls)),
        opportunity_switch,
    )
    .await?;
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle
        .set_focus(DIRECTIVE.to_string())
        .await
        .map_err(|e| anyhow!("set_focus: {e}"))?;
    handle
        .user_message(DIRECTIVE.to_string())
        .await
        .map_err(|e| anyhow!("user_message: {e}"))?;

    let mut obs = OpportunityArmObservation::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    // `TaskCompleted`/`Completed` publish after `TurnCompleted` (the
    // deferred CTX-10 commit runs behind the turn-end barrier), so the
    // drain keeps going through a short quiet window instead of stopping
    // at the first turn-completed event.
    const QUIET_POLLS: u32 = 15;
    let mut quiet_polls: u32 = 0;
    while tokio::time::Instant::now() < deadline {
        let mut got_any = false;
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolSurfacePlanned { report } => {
                    if report
                        .selected
                        .iter()
                        .any(|row| row.tool_name == "task.complete")
                    {
                        obs.surfaced_positions.push(obs.markers.len());
                    }
                }
                RuntimeEvent::CompletionOpportunity {
                    disposition, key, ..
                } => {
                    obs.opportunity_events_total += 1;
                    match disposition {
                        CompletionOpportunityDisposition::Offered => {
                            obs.offered_keys.push(key);
                            obs.markers.push("offered");
                        }
                        CompletionOpportunityDisposition::Called => {
                            obs.markers.push("called");
                        }
                        CompletionOpportunityDisposition::Completed => {
                            obs.completed_keys.push(key);
                            obs.markers.push("completed_key");
                        }
                        _ => {}
                    }
                }
                RuntimeEvent::TaskResumeCommitted { ref debt, .. } => {
                    if debt.iter().any(|reason| reason == "opportunity_offered") {
                        obs.markers.push("resume_opportunity_debt");
                    }
                }
                RuntimeEvent::TaskCompleted { .. } => {
                    obs.task_completed = true;
                    obs.markers.push("completed");
                }
                RuntimeEvent::TurnCompleted => obs.turn_completed = true,
                _ => {}
            }
            got_any = true;
        }
        if obs.turn_completed && !got_any {
            quiet_polls += 1;
            if quiet_polls >= QUIET_POLLS {
                break;
            }
        } else if got_any {
            quiet_polls = 0;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::ensure!(
        obs.turn_completed,
        "opportunity arm (switch={opportunity_switch}) did not finish: {:?}",
        obs
    );
    let shutdown = instance.shutdown().await;
    shutdown?;
    Ok(obs)
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

/// Byte-exact accepted final contents for the mutated seed files.
pub const FINAL_FILES: &[(&str, &str)] = &[
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

    // The item-8 off/on replay retired 2026-08-28 with the
    // CompletionOpportunity candidate (its decision-grade gate failed
    // promotion) and with `task.complete` joining the always-loaded
    // production surface: the arms no longer differ by surface presence,
    // so the replay has nothing left to isolate.
}
