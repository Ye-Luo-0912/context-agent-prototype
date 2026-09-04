//! VERIFY-ROUTE-01: the verifier-eligibility gate.
//!
//! A completion criterion can declare the coverage domain whose exact,
//! source-read-only recipe PASS alone mints an acceptance receipt. This
//! gate drives three deterministic cells over the real runtime and the
//! production tool surface with a scripted model and a real trusted recipe
//! projection, so routing is observed from the event stream, not derived
//! from the fixture:
//!
//! - `negative_positive`: a broad task-scoped `cargo test` PASS is valid
//!   evidence but never mints a receipt, so the first task.complete is
//!   refused with "lack current coverage"; running the declared exact
//!   `rustc` recipe then mints the receipt and the second task.complete
//!   closes the task. The model-visible catalog marks both classes, so the
//!   eligibility information is on the surface.
//! - `positive_control`: the first exact PASS already satisfies the
//!   criterion; a repeated verify.run with the same identity reuses the
//!   recorded PASS without a second process, and completion is accepted.
//! - `unrelated_failure`: a failed process.run records an unrelated
//!   failure that survives the exact PASS; the receipt is minted but
//!   completion stays refused by the unresolved-failed-command gate.
//!
//! The rendered REPORT.md/manifest.json under
//! `crates/agent-eval/evidence/verify-route/` are the decision artifact.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, ContextEngine, ModelTransport, RuntimeEvent,
    RuntimeEventEnvelope, ToolCall, ToolSpec, VerificationCoverageDeclaration,
};
use agent_runtime::task::AnchorPatch;
use anyhow::Context as _;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde::Serialize;
use serde_json::json;

use crate::metrics::aggregate_metrics;
use crate::mock_model::ScriptedModel;

const SCHEMA_VERSION: &str = "verify-route.v1";

const DOMAIN_ID: &str = "saturation-boundary";
const CRITERION: &str = "large attempts saturate at max_delay instead of wrapping";
const DIRECTIVE: &str = "implement the saturation boundary so large attempts clamp at max_delay";

const RECIPE_CARGO_ALL: &str = "cargo.all";
const RECIPE_BOUNDARY: &str = "boundary.saturate";

const FIXTURE_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"verify-route-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    ),
    (
        "src/lib.rs",
        r#"//! Saturation-boundary fixture for the verify-route gate.
//! `crate-type=lib` compiles standalone, so the exact recipe never needs a
//! dependency fetch; the saturation itself is the acceptance topic.

/// Upper bound a requested delay is clamped to, in milliseconds.
pub const MAX_DELAY: u64 = 2_000;

/// Return `requested` clamped to `MAX_DELAY`.
pub fn clamp_delay(requested: u64) -> u64 {
    requested.min(MAX_DELAY)
}
"#,
    ),
];

/// Approval policy: allow everything; the cells measure verification
/// routing and the completion gate, not approval policy.
struct AllowAllGate;

#[async_trait::async_trait]
impl ApprovalGate for AllowAllGate {
    async fn authorize(
        &self,
        _call: &ToolCall,
        _spec: &ToolSpec,
        _cancel: &agent_contracts::CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        Ok(ApprovalDecision::Allow)
    }
}

/// The exact same projection supplies both the executable recipe catalog
/// and the completion-authority snapshot, so the completion gate and the
/// processable surface cannot drift apart.
fn verification_projection() -> anyhow::Result<(
    tool_runtime::VerificationRecipes,
    VerificationCoverageDeclaration,
)> {
    let cargo_all = tool_runtime::VerificationRecipe::new(
        RECIPE_CARGO_ALL,
        "Run the fixture's Cargo test suite",
        "cargo-all-v1",
        vec!["cargo".into(), "test".into()],
    )
    .map_err(|error| anyhow::anyhow!("cargo.all recipe: {error}"))?;
    let boundary = tool_runtime::VerificationRecipe::new(
        RECIPE_BOUNDARY,
        "Compile the saturation boundary without mutating sources",
        "boundary-v1",
        vec![
            "rustc".into(),
            "--edition=2021".into(),
            "--crate-name".into(),
            "boundary".into(),
            "--crate-type=lib".into(),
            "--emit=metadata=.focus-agent/artifacts/boundary.rmeta".into(),
            "src/lib.rs".into(),
        ],
    )
    .map_err(|error| anyhow::anyhow!("boundary recipe: {error}"))?
    .with_exact_current_world_reuse()
    .with_exact_inputs(vec!["src/lib.rs".into()])
    .map_err(|error| anyhow::anyhow!("boundary exact inputs: {error}"))?
    .with_coverage_domain(DOMAIN_ID)
    .map_err(|error| anyhow::anyhow!("boundary coverage domain: {error}"))?;
    let recipes = tool_runtime::VerificationRecipes::new(vec![cargo_all, boundary])
        .and_then(|recipes| {
            recipes.with_domains(vec![tool_runtime::VerificationCoverageDomain {
                domain_id: DOMAIN_ID.into(),
                declaration_revision: 1,
                members: vec![RECIPE_BOUNDARY.into()],
            }])
        })
        .map_err(|error| anyhow::anyhow!("verify-route projection: {error}"))?;
    let declaration = recipes
        .coverage_declaration(DOMAIN_ID)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("verify-route acceptance declaration is missing"))?;
    Ok((recipes, declaration))
}

fn seed_workspace(root: &Path) -> anyhow::Result<()> {
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

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
    }
}

fn verify_args(recipe_id: &str) -> serde_json::Value {
    json!({ "recipe_id": recipe_id })
}

fn complete_args() -> serde_json::Value {
    json!({ "summary": "saturation boundary implemented and coverage earned" })
}

/// One user turn over the real runtime: install the host-declared
/// acceptance criterion on the focus task, then drive the scripted model
/// through the entire tool loop until the turn settles.
async fn drive_cell(
    root: &Path,
    model: Arc<dyn ModelTransport>,
) -> anyhow::Result<Vec<RuntimeEventEnvelope>> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    let (recipes, declaration) = verification_projection()?;
    let lifecycle = tool_runtime::ToolLifecycleConfig {
        always_loaded: vec![
            "fs.list".to_string(),
            "fs.read".to_string(),
            "process.run".to_string(),
            "task.complete".to_string(),
        ],
        ..Default::default()
    };
    let tools: Arc<dyn agent_contracts::ToolDispatcher> = Arc::new(
        tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            lifecycle,
            recipes.clone(),
        ),
    );
    let context_engine: Arc<dyn ContextEngine> =
        Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    let composed = compose(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace,
        context_engine,
        model,
        approval: Arc::new(AllowAllGate),
        base_tools: tools,
        capability_aware: false,
        journal: None,
        artifact_store: None,
        output_broker: None,
        max_tool_rounds: Some(32),
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(Arc::new(
            agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
                .map_err(anyhow::Error::msg)?,
        )),
        effect_reservation_journal: None,
        verification_recipes: None,
        project_proof_refresh: false,
    })
    .await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    let handle = composed.handle();
    handle
        .set_focus(DIRECTIVE.to_string())
        .await
        .map_err(|error| anyhow::anyhow!("set_focus: {error}"))?;
    let active = handle
        .list_tasks()
        .await?
        .into_iter()
        .find(|task| task.status == agent_runtime::TaskStatus::Active)
        .ok_or_else(|| anyhow::anyhow!("no active task after set_focus"))?;
    handle
        .patch_task_anchor(
            active.id,
            active.anchor_revision,
            AnchorPatch {
                completion_policy: Some(agent_runtime::TaskCompletionPolicy::EvidenceRequired),
                acceptance_criteria: Some(vec![agent_runtime::AcceptanceCriterion::declared(
                    CRITERION,
                    &declaration,
                )]),
                ..AnchorPatch::default()
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("patch_task_anchor: {error}"))?;
    handle
        .user_message(DIRECTIVE.to_string())
        .await
        .map_err(|error| anyhow::anyhow!("user_message: {error}"))?;
    let mut capture = Vec::new();
    loop {
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(120), events.recv())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for events"))??;
        let done = matches!(
            envelope.event,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::Error { .. }
                | RuntimeEvent::TurnCommitFailed { .. }
        );
        capture.push(envelope);
        if done {
            break;
        }
    }
    composed.shutdown().await?;
    while let Ok(envelope) = events.try_recv() {
        capture.push(envelope);
    }
    Ok(capture)
}

// ---------------------------------------------------------------------------
// Event-derived observation helpers
// ---------------------------------------------------------------------------

fn count_started(events: &[RuntimeEventEnvelope], tool: &str) -> usize {
    events
        .iter()
        .filter(|envelope| {
            matches!(
                &envelope.event,
                RuntimeEvent::ToolStarted { call } if call.name == tool
            )
        })
        .count()
}

fn count_ok_finished(events: &[RuntimeEventEnvelope], tool: &str) -> usize {
    events
        .iter()
        .filter(|envelope| {
            matches!(
                &envelope.event,
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == tool && output.ok
            )
        })
        .count()
}

fn count_event(events: &[RuntimeEventEnvelope], variant: &str) -> usize {
    events
        .iter()
        .filter(|envelope| match variant {
            "receipts" => {
                matches!(
                    &envelope.event,
                    RuntimeEvent::AcceptanceReceiptsRecorded { .. }
                )
            }
            "task_completed" => matches!(&envelope.event, RuntimeEvent::TaskCompleted { .. }),
            _ => false,
        })
        .count()
}

fn position_of(events: &[RuntimeEventEnvelope], variant: &str) -> Option<usize> {
    events.iter().position(|envelope| match variant {
        "receipts" => {
            matches!(
                &envelope.event,
                RuntimeEvent::AcceptanceReceiptsRecorded { .. }
            )
        }
        "task_completed" => matches!(&envelope.event, RuntimeEvent::TaskCompleted { .. }),
        _ => false,
    })
}

fn warning_messages(events: &[RuntimeEventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            RuntimeEvent::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn verify_started_by_recipe(events: &[RuntimeEventEnvelope]) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    for envelope in events {
        if let RuntimeEvent::ToolStarted { call } = &envelope.event
            && call.name == "verify.run"
            && let Some(id) = call.arguments.get("recipe_id").and_then(|v| v.as_str())
        {
            *map.entry(id.to_string()).or_default() += 1;
        }
    }
    map
}

fn criterion_indices(events: &[RuntimeEventEnvelope]) -> Vec<Vec<u32>> {
    events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            RuntimeEvent::AcceptanceReceiptsRecorded {
                criterion_indices, ..
            } => Some(criterion_indices.clone()),
            _ => None,
        })
        .collect()
}

/// Tool calls dispatched from turn start up to and including the first
/// passing run of a declared domain member. A pre-receipt task-scoped
/// PASS therefore costs calls that never satisfy the criterion, which the
/// negative cell makes observable: satisfaction is measured from the
/// stream, never derived from the fixture.
fn calls_to_first_satisfying(events: &[RuntimeEventEnvelope]) -> Option<usize> {
    let mut passing_starts: Vec<usize> = Vec::new();
    for (index, envelope) in events.iter().enumerate() {
        if let RuntimeEvent::ToolStarted { call } = &envelope.event
            && call.name == "verify.run"
            && call.arguments.get("recipe_id").and_then(|v| v.as_str()) == Some(RECIPE_BOUNDARY)
            && events[index + 1..].iter().any(|later| {
                matches!(
                    &later.event,
                    RuntimeEvent::ToolFinished { output, .. }
                        if output.tool_name == "verify.run"
                            && output.ok
                            && output.call_id == call.id
                )
            })
        {
            passing_starts.push(index);
        }
    }
    let first = *passing_starts.first()?;
    Some(
        events[..=first]
            .iter()
            .filter(|envelope| matches!(&envelope.event, RuntimeEvent::ToolStarted { .. }))
            .count(),
    )
}

/// The task.complete proposal path surfaces its refusal as a warning with
/// the full reason text (the safepoint integration test greps the same
/// prefix).
fn refused_with(events: &[RuntimeEventEnvelope], needle: &str) -> bool {
    warning_messages(events)
        .iter()
        .any(|message| message.contains("completion proposal refused") && message.contains(needle))
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
struct CellEvidence {
    cell: &'static str,
    passed: bool,
    detail: String,
    observed: serde_json::Value,
}

impl CellEvidence {
    fn new(cell: &'static str, passed: bool, detail: String, observed: serde_json::Value) -> Self {
        Self {
            cell,
            passed,
            detail,
            observed,
        }
    }
}

/// Negative + positive: the broad PASS is valid evidence but satisfies no
/// declared domain, so task.complete is refused until the exact recipe
/// mints the receipt.
async fn cell_negative_positive(root: &Path) -> anyhow::Result<CellEvidence> {
    let model = Arc::new(ScriptedModel::new(
        vec![
            tool_call("v1", "verify.run", verify_args(RECIPE_CARGO_ALL)),
            tool_call("c1", "task.complete", complete_args()),
            tool_call("v2", "verify.run", verify_args(RECIPE_BOUNDARY)),
            tool_call("c2", "task.complete", complete_args()),
        ],
        "saturation boundary done",
    ));
    let events = drive_cell(root, model).await?;
    let warnings = warning_messages(&events);
    let refused_no_coverage = refused_with(&events, "lack current coverage");
    let receipt_indices = criterion_indices(&events);
    let receipt = position_of(&events, "receipts");
    let completed = position_of(&events, "task_completed");
    let receipts_before_completion = match (receipt, completed) {
        (Some(r), Some(c)) => r < c,
        _ => false,
    };
    let called = calls_to_first_satisfying(&events);
    let by_recipe = verify_started_by_recipe(&events);
    let observed = json!({
        "verify.run_started_by_recipe": by_recipe,
        "capability.manage_started": count_started(&events, "capability.manage"),
        "calls_to_first_satisfying": called,
        "completion_refused_lack_coverage": refused_no_coverage,
        "receipts": receipt_indices,
        "receipts_before_completion": receipts_before_completion,
        "task_completed": count_event(&events, "task_completed"),
        "warnings": warnings,
    });
    let passed = receipt_indices == vec![vec![0]]
        && refused_no_coverage
        && receipts_before_completion
        && called == Some(3)
        && count_started(&events, "verify.run") == 2
        && count_started(&events, "capability.manage") == 0
        && count_event(&events, "task_completed") == 1;
    let detail = format!(
        "broad cargo.all PASS ({}) then refused task.complete ({}) then exact boundary PASS \
         ({}) closed the task; receipts={:?} warnings={}",
        count_ok_finished(&events, "verify.run"),
        warnings
            .iter()
            .find(|m| m.contains("lack current coverage"))
            .map(|message| message.as_str())
            .unwrap_or("<missing>"),
        count_ok_finished(&events, "verify.run"),
        receipt_indices,
        warnings.len(),
    );
    Ok(CellEvidence::new(
        "negative_positive",
        passed,
        detail,
        observed,
    ))
}

/// Positive control + reuse: the declared recipe satisfies the criterion
/// on the first run; the identical second call reuses the recorded PASS
/// instead of starting a second process.
async fn cell_positive_control(root: &Path) -> anyhow::Result<CellEvidence> {
    let model = Arc::new(ScriptedModel::new(
        vec![
            tool_call("v1", "verify.run", verify_args(RECIPE_BOUNDARY)),
            tool_call("v2", "verify.run", verify_args(RECIPE_BOUNDARY)),
            tool_call("c1", "task.complete", complete_args()),
        ],
        "saturation boundary done",
    ));
    let events = drive_cell(root, model).await?;
    let metrics = aggregate_metrics(&events);
    let warnings = warning_messages(&events);
    let called = calls_to_first_satisfying(&events);
    let observed = json!({
        "verify.run_started": count_started(&events, "verify.run"),
        "verify.run_passed_finished": count_ok_finished(&events, "verify.run"),
        "calls_to_first_satisfying": called,
        "pass_recorded": metrics.verification_pass_recorded,
        "pass_reused": metrics.verification_pass_reused,
        "receipts": criterion_indices(&events),
        "task_completed": count_event(&events, "task_completed"),
        "warnings": warnings,
    });
    let passed = criterion_indices(&events) == vec![vec![0]]
        && called == Some(1)
        && count_started(&events, "verify.run") == 1
        && count_ok_finished(&events, "verify.run") == 2
        && metrics.verification_pass_recorded == 1
        && metrics.verification_pass_reused == 1
        && count_event(&events, "task_completed") == 1
        && !warning_messages(&events)
            .iter()
            .any(|message| message.contains("completion proposal refused"));
    let detail = format!(
        "first boundary PASS satisfied the criterion (calls_to_first={called:?}); \
         second identical verify.run reused the PASS (started={}, finished={}); \
         completion accepted, pass_recorded={} pass_reused={}",
        count_started(&events, "verify.run"),
        count_ok_finished(&events, "verify.run"),
        metrics.verification_pass_recorded,
        metrics.verification_pass_reused,
    );
    Ok(CellEvidence::new(
        "positive_control",
        passed,
        detail,
        observed,
    ))
}

/// Unrelated-failure survival: an unrelated failed process.run survives
/// the exact PASS; the receipt mints, but task.complete stays refused by
/// the unresolved-failed-command gate.
async fn cell_unrelated_failure(root: &Path) -> anyhow::Result<CellEvidence> {
    let model = Arc::new(ScriptedModel::new(
        vec![
            tool_call(
                "p1",
                "process.run",
                json!({"argv": ["no-such-verify-route-binary"]}),
            ),
            tool_call("v1", "verify.run", verify_args(RECIPE_BOUNDARY)),
            tool_call("c1", "task.complete", complete_args()),
        ],
        "saturation boundary done",
    ));
    let events = drive_cell(root, model).await?;
    let warnings = warning_messages(&events);
    let refused_failed_command = refused_with(&events, "unresolved failed command");
    let observed = json!({
        "process.run_started": count_started(&events, "process.run"),
        "process.run_succeeded": count_ok_finished(&events, "process.run"),
        "receipts": criterion_indices(&events),
        "completion_refused_failed_command": refused_failed_command,
        "task_completed": count_event(&events, "task_completed"),
        "warnings": warnings,
    });
    let passed = criterion_indices(&events) == vec![vec![0]]
        && refused_failed_command
        && count_started(&events, "process.run") == 1
        && count_ok_finished(&events, "process.run") == 0
        && count_event(&events, "task_completed") == 0;
    let detail = format!(
        "failed process.run (starts=1, ok=0), exact boundary PASS minted the receipt, \
         task.complete refused: {}",
        warnings
            .iter()
            .find(|m| m.contains("unresolved failed command"))
            .map(|message| message.as_str())
            .unwrap_or("<missing>"),
    );
    Ok(CellEvidence::new(
        "unrelated_failure",
        passed,
        detail,
        observed,
    ))
}

// ---------------------------------------------------------------------------
// Gate runner
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct VerifyRouteManifest {
    schema: &'static str,
    domain_id: String,
    declaration_revision: u64,
    criterion: String,
    cells: Vec<CellEvidence>,
}

/// Run the full gate into `out_dir` and return the rendered REPORT plus
/// whether every cell held. Evidence is persisted even on partial failure
/// so a failed run stays auditable.
pub async fn run_verify_route_gate(out_dir: &Path) -> anyhow::Result<(String, bool)> {
    let dir = tempfile::tempdir().context("create verify-route tempdir")?;
    let root = dir.path().to_path_buf();
    seed_workspace(&root)?;
    let (_, declaration) = verification_projection()
        .context("verify-route projection must compose deterministically")?;

    let cells = vec![
        cell_negative_positive(&root).await?,
        cell_positive_control(&root).await?,
        cell_unrelated_failure(&root).await?,
    ];
    let passed = cells.iter().all(|cell| cell.passed);
    let manifest = VerifyRouteManifest {
        schema: SCHEMA_VERSION,
        domain_id: declaration.domain_id.clone(),
        declaration_revision: declaration.declaration_revision,
        criterion: CRITERION.to_string(),
        cells: cells.clone(),
    };
    std::fs::create_dir_all(out_dir)?;
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let report = render_report(&manifest, passed);
    std::fs::write(out_dir.join("REPORT.md"), &report)?;
    Ok((report, passed))
}

fn render_report(manifest: &VerifyRouteManifest, passed: bool) -> String {
    let mut body = String::new();
    body.push_str("# verify-route gate (verifier eligibility)\n\n");
    body.push_str(&format!(
        "Criterion: `{}`\n\nCoverage domain `{}` (declaration revision {}): only an exact, \
         source-read-only recipe run mints the receipt; a task-scoped PASS records valid \
         evidence but never satisfies a declared criterion.\n\n",
        manifest.criterion, manifest.domain_id, manifest.declaration_revision
    ));
    body.push_str("| cell | verdict | observed |\n|---|---|---|\n");
    for cell in &manifest.cells {
        body.push_str(&format!(
            "| {} | {} | `{}` |\n",
            cell.cell,
            if cell.passed { "PASS" } else { "FAIL" },
            serde_json::to_string(&cell.observed).unwrap_or_default(),
        ));
    }
    body.push_str("\nDetails:\n\n");
    for cell in &manifest.cells {
        body.push_str(&format!(
            "- **{}**: {} (observed `{}`)\n",
            cell.cell,
            cell.detail,
            serde_json::to_string(&cell.observed).unwrap_or_default(),
        ));
    }
    body.push_str(&format!(
        "Verdict: {}\n",
        if passed { "PASS" } else { "FAIL" }
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full eligibility gate must hold every cell deterministically.
    #[tokio::test]
    async fn verify_route_gates_hold_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("verify-route");
        let (_report, passed) = match run_verify_route_gate(&out).await {
            Ok(result) => result,
            Err(error) => {
                let manifest =
                    std::fs::read_to_string(out.join("manifest.json")).unwrap_or_default();
                if !manifest.is_empty() {
                    eprintln!("observed: {manifest}");
                }
                panic!("verify-route gate failed: {error:#}");
            }
        };
        assert!(passed, "verify-route gate must pass");
    }
}
