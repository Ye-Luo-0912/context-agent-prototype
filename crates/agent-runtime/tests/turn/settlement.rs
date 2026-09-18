//! Derived settlement decision boundary at the actor level: the runtime
//! emits settlement labels on `ExecutionFrontier` events and never blocks
//! the model's choice of ordinary final, durable `task.complete`, or
//! concrete continuation. These scenarios are deterministic scripted
//! models over the real actor; no live provider is involved.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentResult, AttentionState, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemId,
    ContextItemSummary, ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, ContextRetention, ContextScope, ContextStateTransition, EventJournal,
    MaterializedContext, MaterializedItem, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RuntimeDirective, RuntimeEvent, RuntimeEventEnvelope, SemanticState,
    SettlementLabel, TaskProgressProposal, ToolCall, ToolDispatcher, ToolExecutionAttribution,
    ToolExecutionPurpose, ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
    ToolSurfacePlanReport, ToolSurfacePlanStatus, VerificationReuse,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices, approx_layer_tokens};
use serde_json::json;

use crate::harness::*;

/// One scripted model round.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Mutate `src/lib.rs` to this revision.
    Write(&'static str),
    /// Run the accepting verifier; the revision it claims to cover must be
    /// the revision the mutation left on disk.
    Verify(&'static str),
    /// Run a verifier from a different host-declared coverage domain.
    VerifyOther(&'static str),
    /// Apply one progress-only anchor CAS at the stated live base revision.
    Manage(u64),
    /// Propose durable `task.complete`.
    Complete(&'static str),
    /// Ordinary final answer, no tool calls.
    Plain(&'static str),
}

/// Host-side identity material of the scripted verifier. The dispatcher
/// hashes this into the exact verification identity stamped onto the PASS;
/// post-observation acceptance receipts in these tests must carry the same
/// digest or they resolve to nothing.
const VERIFY_IDENTITY_MATERIAL: &str = "scripted verify recipe v1";
const ACCEPTANCE_DOMAIN: &str = "workspace-tests";

fn acceptance_declaration() -> agent_contracts::VerificationCoverageDeclaration {
    agent_contracts::VerificationCoverageDeclaration {
        domain_id: ACCEPTANCE_DOMAIN.into(),
        declaration_revision: 1,
        source_digest: agent_contracts::ContentDigest::sha256_bytes(
            b"scripted-workspace-tests-declaration/v1",
        )
        .to_string(),
    }
}

fn api_acceptance_declaration() -> agent_contracts::VerificationCoverageDeclaration {
    agent_contracts::VerificationCoverageDeclaration {
        domain_id: "api-contract".into(),
        declaration_revision: 7,
        source_digest: agent_contracts::ContentDigest::sha256_bytes(
            b"scripted-api-contract-declaration/v7",
        )
        .to_string(),
    }
}

fn verify_identity() -> String {
    agent_contracts::ContentDigest::sha256_bytes(VERIFY_IDENTITY_MATERIAL.trim().as_bytes())
        .to_string()
}

/// Plays a round script exactly; a request beyond the script panics, so a
/// runtime that refuses to settle (or auto-stops) fails the test loudly.
/// Every model request's full message text is appended to `requests` so
/// request-level projection tests can assert on the assembled prompt, and
/// the whole wire request to `full_requests` so packing tests can compare
/// published counts against the request actually sent.
#[derive(Debug)]
struct SettlementModel {
    script: Vec<Step>,
    rounds: AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    full_requests: Arc<std::sync::Mutex<Vec<ModelRequest>>>,
}

#[async_trait::async_trait]
impl ModelTransport for SettlementModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let text = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.requests.lock().unwrap().push(text);
        self.full_requests.lock().unwrap().push(request.clone());
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        let Some(step) = self.script.get(round) else {
            panic!("the runtime requested round {round} beyond the script");
        };
        Ok(match step {
            Step::Write(revision) => ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("write-{round}"),
                    name: "fs.write".into(),
                    arguments: json!({"path": "src/lib.rs", "revision": revision, "body": "// v"}),
                }],
                usage: Default::default(),
            },
            Step::Verify(revision) => ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("verify-{round}"),
                    name: "test.verify".into(),
                    arguments: json!({"command": "cargo test", "revision": revision}),
                }],
                usage: Default::default(),
            },
            Step::VerifyOther(revision) => ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("verify-other-{round}"),
                    name: "test.verify.other".into(),
                    arguments: json!({"command": "cargo test -- api", "revision": revision}),
                }],
                usage: Default::default(),
            },
            Step::Manage(base_anchor_revision) => ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("manage-{round}"),
                    name: "task.manage".into(),
                    arguments: json!({
                        "base_anchor_revision": base_anchor_revision,
                        "plan_progress": ["verification observed; prepare closure"]
                    }),
                }],
                usage: Default::default(),
            },
            Step::Complete(summary) => ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("complete-{round}"),
                    name: "task.complete".into(),
                    arguments: json!({"summary": summary, "artifacts": []}),
                }],
                usage: Default::default(),
            },
            Step::Plain(text) => ModelOutput {
                content: (*text).into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            },
        })
    }
}

/// The trusted dispatcher the scripted model calls into. Like a real host,
/// it declares the execution purpose of each call before dispatch: `fs.write`
/// is a bounded Mutate, `test.verify` (an in-process verifier, not the
/// process-family `verify.run`) is a reusable verification, and outputs
/// carry the typed metadata the observation path relies on (`fs.write` stamps
/// a Known mutation, `test.verify` names the covered resource at the admitted
/// revision, `task.complete` attaches the completion directive).
#[derive(Debug)]
struct SettlementToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for SettlementToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "fs.write".into(),
                description: "scripted write".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::WorkspaceWrite,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "test.verify".into(),
                description: "scripted verifier".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "test.verify.other".into(),
                description: "scripted API verifier".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "task.complete".into(),
                description: "propose completion".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "task.manage".into(),
                description: "scripted progress CAS".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
        ]
    }
    fn execution_attribution(&self, call: &ToolCall) -> ToolExecutionAttribution {
        match call.name.as_str() {
            "fs.write" => ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Mutate,
                ["src/lib.rs".to_string()],
                VerificationReuse::None,
            ),
            "test.verify" => ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                Vec::new(),
                VerificationReuse::ExactCurrentWorld,
            )
            .with_verification_identity_material(VERIFY_IDENTITY_MATERIAL)
            .with_verification_recipe(agent_contracts::VerificationRecipeProvenance {
                recipe_id: "test.verify".into(),
                recipe_revision: "v1".into(),
                coverage_domain: Some(ACCEPTANCE_DOMAIN.into()),
                domain_declaration_revision: Some(1),
                domain_source_digest: acceptance_declaration().source_digest,
                class_identity_digest: "scripted-class".into(),
            }),
            "test.verify.other" => ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                Vec::new(),
                VerificationReuse::ExactCurrentWorld,
            )
            .with_verification_identity_material("scripted api verify recipe v1")
            .with_verification_recipe(agent_contracts::VerificationRecipeProvenance {
                recipe_id: "test.verify.other".into(),
                recipe_revision: "v1".into(),
                coverage_domain: Some("api-contract".into()),
                domain_declaration_revision: Some(7),
                domain_source_digest: api_acceptance_declaration().source_digest,
                class_identity_digest: "scripted-api-class".into(),
            }),
            _ => ToolExecutionAttribution::default(),
        }
    }

    fn verification_coverage_declarations(
        &self,
    ) -> Vec<agent_contracts::VerificationCoverageDeclaration> {
        // Contract order is canonical by domain id.
        vec![api_acceptance_declaration(), acceptance_declaration()]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let revision = request.call.arguments["revision"]
            .as_str()
            .unwrap_or("r2")
            .to_string();
        match request.call.name.as_str() {
            "fs.write" => Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "wrote src/lib.rs".into(),
                model_content: "wrote src/lib.rs".into(),
                artifact_ref: None,
                metadata: json!({"path": "src/lib.rs", "revision": revision}),
            })),
            "test.verify" | "test.verify.other" => Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "tests passed".into(),
                model_content: "tests passed".into(),
                artifact_ref: None,
                metadata: json!({
                    "command": "cargo test",
                    "verification": true,
                    "path": "src/lib.rs",
                    "revision": revision,
                }),
            })),
            "task.complete" => {
                let summary = request.call.arguments["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: request.call.id,
                        tool_name: request.call.name,
                        ok: true,
                        summary: "completion proposed".into(),
                        model_content: "completion proposed".into(),
                        artifact_ref: None,
                        metadata: json!({}),
                    },
                    directive: RuntimeDirective::CompleteTask(
                        agent_contracts::CompletionProposal {
                            summary,
                            artifacts: Vec::new(),
                        },
                    ),
                })
            }
            "task.manage" => Ok(ToolOutcome::RuntimeDirective {
                output: ToolOutput {
                    call_id: request.call.id,
                    tool_name: request.call.name,
                    ok: true,
                    summary: "progress proposed".into(),
                    model_content: "progress proposed".into(),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                directive: RuntimeDirective::UpdateTaskProgress(TaskProgressProposal {
                    base_anchor_revision: request.call.arguments["base_anchor_revision"]
                        .as_u64()
                        .unwrap(),
                    current_interpretation: None,
                    plan_progress: Some(vec!["verification observed; prepare closure".into()]),
                    open_loops: None,
                    next_action: None,
                }),
            }),
            _ => Err(agent_contracts::AgentError::Tool("unexpected tool".into())),
        }
    }
}

#[tokio::test]
async fn distinct_domain_passes_accumulate_criterion_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::VerifyOther("r2"),
            Step::Plain("done"),
        ],
    )
    .await;
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
            acceptance_criteria: Some(vec![
                agent_runtime::task::AcceptanceCriterion::declared(
                    "workspace tests pass",
                    &acceptance_declaration(),
                ),
                agent_runtime::task::AcceptanceCriterion::declared(
                    "the public API remains compatible",
                    &api_acceptance_declaration(),
                ),
            ]),
            ..agent_runtime::AnchorPatch::default()
        },
    )
    .await;
    let events = user_turn(&instance, collector).await;
    assert!(
        settlement_labels(&events).contains(&SettlementLabel::SettledCandidate),
        "both explicitly matched domain receipts should settle the task"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    let anchor = &checkpoint.tasks.tasks[0].anchor;
    assert_eq!(anchor.acceptance_coverage.len(), 2);
    assert_eq!(
        anchor.acceptance_coverage[0].coverage_domain,
        ACCEPTANCE_DOMAIN
    );
    assert_eq!(
        anchor.acceptance_coverage[1].coverage_domain,
        "api-contract"
    );
    assert_eq!(anchor.acceptance_coverage[1].domain_declaration_revision, 7);
    assert!(events.iter().any(|envelope| matches!(
        &envelope.event,
        RuntimeEvent::AcceptanceReceiptsRecorded { criterion_indices, .. }
            if criterion_indices == &[1]
    )));
    instance.shutdown().await.unwrap();
}

/// The deterministic repair matrix: rejecting completion while a second
/// criterion is still uncovered derives the projected stage from current
/// readiness. Once the first criterion's PASS mints its receipt, the
/// projected stage names only the second criterion; the follow-up PASS
/// then closes it and the next completion is accepted.
#[tokio::test]
async fn criterion_a_pass_advances_the_projected_repair_stage_to_criterion_b() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r1"),
            Step::Verify("r1"),
            Step::Complete("partial"),
            Step::VerifyOther("r1"),
            Step::Complete("done"),
        ],
    )
    .await;
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
            acceptance_criteria: Some(vec![
                agent_runtime::task::AcceptanceCriterion::declared(
                    "workspace tests pass",
                    &acceptance_declaration(),
                ),
                agent_runtime::task::AcceptanceCriterion::declared(
                    "the public API remains compatible",
                    &api_acceptance_declaration(),
                ),
            ]),
            ..agent_runtime::AnchorPatch::default()
        },
    )
    .await;
    let events = user_turn(&instance, collector).await;

    let receipt_batches: Vec<Vec<u32>> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            RuntimeEvent::AcceptanceReceiptsRecorded {
                criterion_indices, ..
            } => Some(criterion_indices.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        receipt_batches,
        vec![vec![0], vec![1]],
        "each domain PASS must mint exactly its own criterion receipt"
    );

    let completions: Vec<ToolOutput> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            RuntimeEvent::ToolFinished { output, .. } if output.tool_name == "task.complete" => {
                Some(output.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(completions.len(), 2);

    // First completion: the workspace-tests receipt is already minted, so
    // the projected repair stage names only the still-uncovered criterion.
    assert!(!completions[0].ok, "the partial completion must be refused");
    assert_eq!(
        completions[0].metadata["refused"].as_str(),
        Some("completion_gate")
    );
    let details = completions[0].metadata["repair_plan"]["steps"][0]["criterion_details"]
        .as_array()
        .expect("the refusal must carry criterion details");
    assert_eq!(
        details.len(),
        1,
        "the criterion-0 PASS must advance the stage off criterion 0"
    );
    assert_eq!(details[0]["criterion_index"], 1);
    assert_eq!(details[0]["coverage_domain"], "api-contract");

    // Second completion: the api-contract receipt lands and the task closes.
    assert!(
        completions[1].ok,
        "both covered criteria must accept completion"
    );
    assert!(
        events
            .iter()
            .any(|envelope| matches!(&envelope.event, RuntimeEvent::TaskCompleted { .. }))
    );
    instance.shutdown().await.unwrap();
}

#[derive(Debug)]
struct FailAcceptanceReceiptJournal;

#[async_trait::async_trait]
impl EventJournal for FailAcceptanceReceiptJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(
            &envelope.event,
            RuntimeEvent::AcceptanceReceiptsRecorded { .. }
        ) {
            return Err(agent_contracts::AgentError::Storage(
                "simulated acceptance-receipt event failure".into(),
            ));
        }
        Ok(())
    }
}

#[tokio::test]
async fn receipt_event_failure_fences_recovery_without_partial_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector, _) = settlement_instance_with_journal(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("done")],
        false,
        Some(Arc::new(FailAcceptanceReceiptJournal)),
    )
    .await;
    started_task_with_patch(&instance, settled_patch()).await;
    let handle = instance.handle();
    handle.user_message("keep going".into()).await.unwrap();

    let mut receiver = collector.events;
    let mut recovery_seen = false;
    let mut settled_candidate_seen = false;
    let mut receipt_seen = false;
    let mut completed_seen = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline && !recovery_seen {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), receiver.recv()).await
        {
            recovery_seen |= matches!(&envelope.event, RuntimeEvent::RecoveryRequired);
            settled_candidate_seen |= matches!(
                &envelope.event,
                RuntimeEvent::ExecutionFrontier {
                    settlement: Some(SettlementLabel::SettledCandidate),
                    ..
                }
            );
            receipt_seen |= matches!(
                &envelope.event,
                RuntimeEvent::AcceptanceReceiptsRecorded { .. }
            );
            completed_seen |= matches!(&envelope.event, RuntimeEvent::TaskCompleted { .. });
        }
    }
    assert!(
        recovery_seen,
        "a missing receipt audit row must recovery-fence Runtime"
    );
    assert!(
        !settled_candidate_seen,
        "the uncommitted receipt must never produce partial readiness"
    );
    assert!(
        !receipt_seen && !completed_seen,
        "a failed audit append must expose neither a receipt nor completion"
    );
    assert!(matches!(
        instance.checkpoint().await,
        Err(agent_contracts::AgentError::RecoveryRequired(_))
    ));
    instance.shutdown().await.unwrap();
}

async fn settlement_instance_with(
    dir: &std::path::Path,
    script: Vec<Step>,
    project_settlement: bool,
) -> (
    RuntimeInstance,
    RuntimeEventEnvelopeCollector,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    settlement_instance_with_journal(dir, script, project_settlement, None).await
}

async fn settlement_instance_with_journal(
    dir: &std::path::Path,
    script: Vec<Step>,
    project_settlement: bool,
    event_journal: Option<Arc<dyn EventJournal>>,
) -> (
    RuntimeInstance,
    RuntimeEventEnvelopeCollector,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let capture = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = Arc::new(agent_workspace::Workspace::open(dir).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(SettlementModel {
            script,
            rounds: AtomicUsize::new(0),
            requests: capture.clone(),
            full_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
        Arc::new(SettlementToolDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        event_journal,
    )
    .with_artifact_workspace(workspace)
    .with_project_task_progress(true)
    .with_project_settlement(project_settlement);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let collector = RuntimeEventEnvelopeCollector {
        events: instance.handle().subscribe(),
    };
    (instance, collector, capture)
}

async fn settlement_instance(
    dir: &std::path::Path,
    script: Vec<Step>,
) -> (RuntimeInstance, RuntimeEventEnvelopeCollector) {
    let (instance, collector, _) = settlement_instance_with(dir, script, false).await;
    (instance, collector)
}

struct RuntimeEventEnvelopeCollector {
    events: tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
}

impl RuntimeEventEnvelopeCollector {
    async fn drain_until_turn_completed(mut self) -> Vec<RuntimeEventEnvelope> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut turn_done = false;
        // Post-turn durable transactions (the task-completion batch) are
        // emitted after the turn barrier from the same actor step, with a
        // journal flush between them. Under CI load that flush can take
        // well over 100ms, so a plain one-round quiet probe was flaky:
        // the collector returned between the two batches. Sweep a bounded
        // window of ~1s of consecutive quiet after the turn barrier before
        // declaring the turn fully drained; the 5s deadline still caps the
        // whole sweep.
        const POST_TURN_QUIET_MS: u64 = 1000;
        let mut quiet_start: Option<tokio::time::Instant> = None;
        while tokio::time::Instant::now() < deadline {
            let mut drained_any = false;
            loop {
                match self.events.try_recv() {
                    Ok(envelope) => {
                        turn_done |= matches!(envelope.event, RuntimeEvent::TurnCompleted);
                        drained_any = true;
                        seen.push(envelope);
                    }
                    // A lagged receiver means events were produced faster
                    // than this drain; never treat that as quiet time.
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        drained_any = true;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    // The instance shut down: no further events will come.
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        return seen;
                    }
                }
            }
            if turn_done && !drained_any {
                let now = tokio::time::Instant::now();
                let entry = *quiet_start.get_or_insert(now);
                if now.duration_since(entry) >= Duration::from_millis(POST_TURN_QUIET_MS) {
                    break;
                }
            } else {
                quiet_start = None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            turn_done,
            "turn must complete inside the test deadline; saw {} events",
            seen.len()
        );
        seen
    }
}

fn settlement_labels(events: &[RuntimeEventEnvelope]) -> Vec<SettlementLabel> {
    events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            RuntimeEvent::ExecutionFrontier { settlement, .. } => *settlement,
            _ => None,
        })
        .collect()
}

/// Start the instance, focus the scripted task, and apply one anchor patch
/// to its fresh revision-0 anchor before the first user turn. Tests use
/// this to declare acceptance criteria and any blocking open loop
/// or next action; the task-aware gate fails closed at `VerifiedCurrent`
/// without it.
async fn started_task_with_patch(instance: &RuntimeInstance, patch: agent_runtime::AnchorPatch) {
    let handle = instance.handle();
    handle.start().await.unwrap();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let revision = handle.patch_task_anchor(task_id, 0, patch).await.unwrap();
    assert_eq!(revision, 1, "the declared task anchor must advance by one");
}

/// The acceptance authority every settled-candidate scenario needs: one
/// criterion whose domain can receive a post-observation receipt from the
/// scripted verifier.
fn settled_patch() -> agent_runtime::AnchorPatch {
    agent_runtime::AnchorPatch {
        completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
        acceptance_criteria: Some(vec![agent_runtime::task::AcceptanceCriterion::declared(
            "tests pass for the current world",
            &acceptance_declaration(),
        )]),
        ..agent_runtime::AnchorPatch::default()
    }
}

async fn user_turn(
    instance: &RuntimeInstance,
    collector: RuntimeEventEnvelopeCollector,
) -> Vec<RuntimeEventEnvelope> {
    let handle = instance.handle();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();
    collector.drain_until_turn_completed().await
}

async fn drive(
    instance: &RuntimeInstance,
    collector: RuntimeEventEnvelopeCollector,
) -> Vec<RuntimeEventEnvelope> {
    started_task_with_patch(instance, settled_patch()).await;
    user_turn(instance, collector).await
}

/// A further user turn on an already-started instance: re-focusing the same
/// goal resumes the same task, so multi-turn scenarios share one scripted
/// round sequence across user turns.
async fn continue_turn(
    instance: &RuntimeInstance,
    collector: RuntimeEventEnvelopeCollector,
) -> Vec<RuntimeEventEnvelope> {
    user_turn(instance, collector).await
}

#[tokio::test]
async fn settled_work_commits_durable_closure_with_current_verification() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Complete("done"),
        ],
    )
    .await;
    let events = drive(&instance, collector).await;

    let labels = settlement_labels(&events);
    assert!(
        labels.contains(&SettlementLabel::SettledCandidate),
        "the verified mutation must surface a settled candidate: {labels:?}"
    );
    assert!(
        events
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })),
        "the model's durable closure choice must commit"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint
        .tasks
        .completed
        .last()
        .expect("a completed task owns exactly one CompletionRecord");
    assert_eq!(
        record.verification_status,
        agent_runtime::task::CompletionVerificationStatus::Current,
        "durable closure after a Current verifier must record Current verification"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn progress_only_cas_after_a_receipt_preserves_completion_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            // Acceptance declaration is revision 1 and the post-PASS
            // receipt transaction advances the full anchor to revision 2.
            Step::Manage(2),
            Step::Complete("done after progress bookkeeping"),
        ],
    )
    .await;
    let events = drive(&instance, collector).await;

    assert!(events.iter().any(|envelope| matches!(
        &envelope.event,
        RuntimeEvent::TaskProgressUpdated {
            accepted: true,
            anchor_revision: 3,
            ..
        }
    )));
    assert!(
        settlement_labels(&events).contains(&SettlementLabel::SettledCandidate),
        "a progress-only CAS must not make the live execution task-state stale"
    );
    assert!(
        events
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. }))
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed[0].anchor_revision, 3);
    instance.shutdown().await.unwrap();
}

/// The actor mints acceptance receipts from an already-observed trusted
/// verification pass: a task that declares criteria but carries no receipt
/// arms the task-aware gate once the matching verifier PASSes, so live cells
/// do not need a second, identity-synchronized writer.
#[tokio::test]
async fn declared_acceptance_is_bound_by_the_trusted_verification_pass() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("done")],
    )
    .await;
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
            acceptance_criteria: Some(vec![agent_runtime::task::AcceptanceCriterion::declared(
                "tests pass for the current world",
                &acceptance_declaration(),
            )]),
            // Deliberately no acceptance_coverage: only the post-observation
            // actor transaction may mint the receipt.
            ..agent_runtime::AnchorPatch::default()
        },
    )
    .await;
    let events = user_turn(&instance, collector).await;
    let labels = settlement_labels(&events);
    assert!(
        labels.contains(&SettlementLabel::SettledCandidate),
        "the declared criterion must be covered by the trusted verification pass: {labels:?}"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    let task = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.status != agent_runtime::task::TaskStatus::Completed)
        .expect("the active task stays open after the ordinary final");
    assert_eq!(
        task.anchor.acceptance_coverage.len(),
        1,
        "the actor must have minted exactly one criterion receipt"
    );
    assert_eq!(
        task.anchor.acceptance_coverage[0].verification_identity,
        verify_identity(),
        "the receipt must resolve to the scripted verifier's exact identity"
    );
    instance.shutdown().await.unwrap();
}

/// Without any declared acceptance criteria the gate stays fail-closed:
/// the verified world can reach `VerifiedCurrent` but never the
/// task-aware candidate.
#[tokio::test]
async fn undeclared_acceptance_stays_fail_closed_at_verified_current() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("done")],
    )
    .await;
    let handle = instance.handle();
    handle.start().await.unwrap();
    handle
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    let events = user_turn(&instance, collector).await;
    let labels = settlement_labels(&events);
    assert!(
        !labels.contains(&SettlementLabel::SettledCandidate),
        "without declared acceptance criteria the gate fails closed: {labels:?}"
    );
    assert!(
        labels.contains(&SettlementLabel::VerifiedCurrent),
        "the execution-local readiness must still surface: {labels:?}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn settled_work_leaves_ordinary_final_user_choice_intact() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("finished"),
        ],
    )
    .await;
    let events = drive(&instance, collector).await;

    assert!(
        settlement_labels(&events).contains(&SettlementLabel::SettledCandidate),
        "the verified mutation must surface a settled candidate"
    );
    assert!(
        !events
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })),
        "no auto-close: ordinary final must not commit a durable completion"
    );
    assert!(
        events
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::AssistantMessage { .. })),
        "the ordinary final answer must reach the user"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn mutation_after_settlement_reopens_and_re_verify_settles_again() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Write("r3"),
            Step::Verify("r3"),
            Step::Complete("done after the second round"),
        ],
    )
    .await;
    let events = drive(&instance, collector).await;

    // The settlement label must leave SettledCandidate after the second
    // mutation and re-enter it after the fresh verification.
    let labels = settlement_labels(&events);
    let first_settled = labels
        .iter()
        .position(|label| *label == SettlementLabel::SettledCandidate)
        .expect("first verified mutation settles");
    let reopened = labels[first_settled + 1..]
        .iter()
        .position(|label| *label != SettlementLabel::SettledCandidate)
        .map(|offset| first_settled + 1 + offset)
        .expect("a new mutation must reopen the settlement");
    assert_ne!(labels[reopened], SettlementLabel::SettledCandidate);
    assert!(
        labels[reopened..].contains(&SettlementLabel::SettledCandidate),
        "re-verification must settle again: {labels:?}"
    );

    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint.tasks.completed.last().unwrap();
    assert_eq!(
        record.verification_status,
        agent_runtime::task::CompletionVerificationStatus::Current
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn valid_remaining_work_stays_executable_after_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Write("r3"),
            Step::Plain("continue with the missing test"),
        ],
    )
    .await;
    let events = drive(&instance, collector).await;

    assert!(
        !events
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })),
        "remaining work must not be auto-closed after a settled moment"
    );
    let tasks = instance.handle().list_tasks().await.unwrap();
    assert!(
        tasks
            .iter()
            .any(|task| task.status == agent_runtime::task::TaskStatus::Active),
        "the task stays active for continuation"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn mutation_after_settlement_without_fresh_verify_is_stale_not_settled() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Write("r3"),
            Step::Plain("the tests are stale now"),
        ],
    )
    .await;
    let events = drive(&instance, collector).await;
    let labels = settlement_labels(&events);

    let first_settled = labels
        .iter()
        .position(|label| *label == SettlementLabel::SettledCandidate)
        .expect("the first verified mutation settles: {labels:?}");
    assert!(
        labels[first_settled + 1..].contains(&SettlementLabel::VerificationDue),
        "a new mutation without a fresh verification must be stale, not settled: {labels:?}"
    );
    assert!(
        !labels[first_settled + 1..].contains(&SettlementLabel::SettledCandidate),
        "no re-settlement without a fresh verification: {labels:?}"
    );
    assert!(
        !events
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })),
        "stale verification must not auto-close the task"
    );

    let checkpoint = instance.checkpoint().await.unwrap();
    let task = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.status != agent_runtime::task::TaskStatus::Completed)
        .expect("the active task owns the stale resume");
    assert_eq!(
        task.resume.validity(),
        agent_runtime::VerificationState::Stale,
        "the trusted verification basis is stale until a fresh verification covers the new revision"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn criterion_change_reopens_and_trusted_pass_mints_fresh_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("settled"),
            Step::Write("r3"),
            Step::Verify("r3"),
            Step::Plain("reopened"),
            Step::Write("r4"),
            Step::Verify("r4"),
            Step::Plain("covered again"),
        ],
    )
    .await;
    let first_turn = drive(&instance, collector).await;
    assert!(
        settlement_labels(&first_turn).contains(&SettlementLabel::SettledCandidate),
        "the declared task settles before the boundary change"
    );

    // Completion-authority change: a second criterion moves the
    // verification basis and clears old receipts; the next trusted PASS
    // mints fresh receipts for both matching criteria.
    let handle = instance.handle();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let base_revision = handle.list_tasks().await.unwrap()[0].anchor_revision;
    let revision = handle
        .patch_task_anchor(
            task_id,
            base_revision,
            agent_runtime::AnchorPatch {
                acceptance_criteria: Some(vec![
                    agent_runtime::task::AcceptanceCriterion::declared(
                        "tests pass for the current world",
                        &acceptance_declaration(),
                    ),
                    agent_runtime::task::AcceptanceCriterion::declared(
                        "api unchanged",
                        &acceptance_declaration(),
                    ),
                ]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, base_revision + 1);

    let reopened = continue_turn(
        &instance,
        RuntimeEventEnvelopeCollector {
            events: instance.handle().subscribe(),
        },
    )
    .await;
    let reopened_labels = settlement_labels(&reopened);
    assert!(
        reopened_labels.contains(&SettlementLabel::SettledCandidate),
        "the fresh trusted pass auto-binds the newly declared criterion and re-settles the task: {reopened_labels:?}"
    );

    let resettled = continue_turn(
        &instance,
        RuntimeEventEnvelopeCollector {
            events: instance.handle().subscribe(),
        },
    )
    .await;
    assert!(
        settlement_labels(&resettled).contains(&SettlementLabel::SettledCandidate),
        "a later verified mutation keeps the auto-bound coverage current: {:?}",
        settlement_labels(&resettled)
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn open_loop_blocks_candidate_even_with_full_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("loop open"),
        ],
    )
    .await;
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
            acceptance_criteria: Some(vec![agent_runtime::task::AcceptanceCriterion::declared(
                "tests pass for the current world",
                &acceptance_declaration(),
            )]),
            open_loops: Some(vec!["verify edge cases".into()]),
            ..agent_runtime::AnchorPatch::default()
        },
    )
    .await;
    let events = user_turn(&instance, collector).await;
    let labels = settlement_labels(&events);
    assert!(
        !labels.contains(&SettlementLabel::SettledCandidate),
        "an anchored open loop must block the candidate: {labels:?}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn next_action_is_advisory_when_authority_and_evidence_are_complete() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("next action"),
        ],
    )
    .await;
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
            acceptance_criteria: Some(vec![agent_runtime::task::AcceptanceCriterion::declared(
                "tests pass for the current world",
                &acceptance_declaration(),
            )]),
            next_action: Some("write the missing test".into()),
            ..agent_runtime::AnchorPatch::default()
        },
    )
    .await;
    let events = user_turn(&instance, collector).await;
    let labels = settlement_labels(&events);
    assert!(
        labels.contains(&SettlementLabel::SettledCandidate),
        "model-owned next-action guidance must not self-deadlock a fully covered task: {labels:?}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn proposal_settlement_survives_cancel_resume_and_commits_durably() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("settled"),
            Step::Complete("durable after resume"),
        ],
    )
    .await;
    let first_turn = drive(&instance, collector).await;
    assert!(
        settlement_labels(&first_turn).contains(&SettlementLabel::SettledCandidate),
        "the proposal settles before the interruption"
    );
    assert!(
        !first_turn
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })),
        "the first turn must not close anything"
    );

    // Interrupt the task and resume it: the settlement proposal must be
    // derived again from the persisted resume, not from the transcript.
    instance.handle().suspend_task().await.unwrap();
    let tasks = instance.handle().list_tasks().await.unwrap();
    assert!(
        tasks
            .iter()
            .any(|task| task.status == agent_runtime::task::TaskStatus::Suspended),
        "suspend must leave one suspended task"
    );
    // Reactivate the suspended task, then continue the stored directive. A
    // new Dialogue input would intentionally advance directive_revision and
    // require a fresh verification; TaskContinuation preserves the basis.
    let resumed_collector = RuntimeEventEnvelopeCollector {
        events: instance.handle().subscribe(),
    };
    instance
        .handle()
        .set_focus("implement bounded retry".into())
        .await
        .unwrap();
    instance.handle().continue_active_task().await.unwrap();
    let resumed = resumed_collector.drain_until_turn_completed().await;
    assert!(
        resumed
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })),
        "the resumed model's durable closure choice must commit"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint
        .tasks
        .completed
        .last()
        .expect("a completed task owns exactly one CompletionRecord");
    assert_eq!(
        record.verification_status,
        agent_runtime::task::CompletionVerificationStatus::Current,
        "the settlement proposal and its Current verification must survive cancel/resume"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_user_instruction_advances_directive_revision_and_invalidates_prior_proof() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("settled"),
            Step::Write("r3"),
            Step::Plain("fresh instruction mutation, no re-verify"),
        ],
    )
    .await;
    let first = drive(&instance, collector).await;
    assert!(
        settlement_labels(&first).contains(&SettlementLabel::SettledCandidate),
        "the first verified mutation settles: {:?}",
        settlement_labels(&first)
    );
    let revision_before = {
        let checkpoint = instance.checkpoint().await.unwrap();
        checkpoint
            .tasks
            .tasks
            .iter()
            .find(|task| task.status != agent_runtime::task::TaskStatus::Completed)
            .expect("the active task owns the revision")
            .resume
            .directive_revision
    };

    // A NEW user instruction, unlike a continuation, is re-ingested as
    // dialogue: it advances the directive revision, so the prior PASS can
    // no longer vouch for the current directive, and the following mutation
    // without a fresh verification stays stale rather than settled.
    let resumed_collector = RuntimeEventEnvelopeCollector {
        events: instance.handle().subscribe(),
    };
    let second = continue_turn(&instance, resumed_collector).await;
    let labels = settlement_labels(&second);
    assert!(
        !labels.contains(&SettlementLabel::SettledCandidate),
        "a new instruction followed by an unverified mutation must not re-settle: {labels:?}"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    let task = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.status != agent_runtime::task::TaskStatus::Completed)
        .expect("the active task owns the advanced revision");
    assert!(
        task.resume.directive_revision > revision_before,
        "a new user instruction must advance the directive revision"
    );
    assert_eq!(
        task.resume.validity(),
        agent_runtime::VerificationState::Stale,
        "the new instruction's mutation invalidates the prior proof without a fresh verification"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn continuation_preserves_directive_revision_but_mutation_invalidates_old_proof() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("settled"),
            Step::Write("r3"),
            Step::Plain("continuation mutation, no re-verify"),
        ],
    )
    .await;
    let first = drive(&instance, collector).await;
    assert!(
        settlement_labels(&first).contains(&SettlementLabel::SettledCandidate),
        "the verified mutation settles before the continuation: {:?}",
        settlement_labels(&first)
    );
    let revision_before = {
        let checkpoint = instance.checkpoint().await.unwrap();
        checkpoint
            .tasks
            .tasks
            .iter()
            .find(|task| task.status != agent_runtime::task::TaskStatus::Completed)
            .expect("the active task owns the revision")
            .resume
            .directive_revision
    };

    // A continuation re-runs the stored directive without re-ingesting it
    // as dialogue: the directive revision is preserved. A real mutation
    // after the continuation still invalidates the old proof until a fresh
    // verification covers the new world.
    let resumed_collector = RuntimeEventEnvelopeCollector {
        events: instance.handle().subscribe(),
    };
    instance.handle().continue_active_task().await.unwrap();
    let second = resumed_collector.drain_until_turn_completed().await;
    let labels = settlement_labels(&second);
    assert!(
        !labels.contains(&SettlementLabel::SettledCandidate),
        "a mutation after continuation without a fresh verification must not re-settle: {labels:?}"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    let task = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.status != agent_runtime::task::TaskStatus::Completed)
        .expect("the active task owns the preserved revision");
    assert_eq!(
        task.resume.directive_revision, revision_before,
        "continuation must not mint a new directive revision"
    );
    assert_eq!(
        task.resume.validity(),
        agent_runtime::VerificationState::Stale,
        "a real mutation after continuation invalidates the old proof without a fresh verification"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_restore_preserves_settlement_and_reopen_resettles() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Plain("settled"),
            Step::Write("r3"),
            Step::Verify("r3"),
            Step::Plain("done after cold resume"),
        ],
    )
    .await;
    let first_turn = drive(&instance, collector).await;
    assert!(
        settlement_labels(&first_turn).contains(&SettlementLabel::SettledCandidate),
        "the proposal settles before the cold load"
    );

    // Cold load: restore the whole runtime from its checkpoint (the only
    // ephemeral-Core-supported path is same-run restore). The settlement
    // must be derived again from the restored typed facts.
    let checkpoint = instance.checkpoint().await.unwrap();
    instance.restore(checkpoint).await.unwrap();

    let resumed = continue_turn(
        &instance,
        RuntimeEventEnvelopeCollector {
            events: instance.handle().subscribe(),
        },
    )
    .await;
    let labels = settlement_labels(&resumed);
    assert!(
        labels.contains(&SettlementLabel::VerificationDue),
        "the restored mutation must reopen the settled proposal: {labels:?}"
    );
    assert!(
        labels.contains(&SettlementLabel::SettledCandidate),
        "a fresh verification must re-settle after the cold load: {labels:?}"
    );
    assert!(
        !resumed
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })),
        "cold restore must not auto-close the task"
    );

    let final_checkpoint = instance.checkpoint().await.unwrap();
    let task = final_checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.status != agent_runtime::task::TaskStatus::Completed)
        .expect("the active task owns the resumed resume");
    assert_eq!(
        task.resume.validity(),
        agent_runtime::VerificationState::Current,
        "the restored verification, mutation and re-verification leave a Current basis"
    );
    instance.shutdown().await.unwrap();
}

/// The TASK PROGRESS block of one captured request: the text from its
/// header up to the following focus section, so a request-level 2,048-char
/// bound can measure exactly the block (plus its trailing newline) rather
/// than the whole message.
fn progress_block_bounds(request: &str) -> Option<(usize, usize)> {
    let start = request.find("TASK PROGRESS anchor_rev=")?;
    let end = request[start..]
        .find("\nCURRENT DIRECTIVE")
        .map(|offset| start + offset)
        .unwrap_or(request.len());
    Some((start, end))
}

#[tokio::test]
async fn settlement_fact_is_absent_when_projection_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector, requests) = settlement_instance_with(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("done")],
        false,
    )
    .await;
    let events = drive(&instance, collector).await;
    assert!(
        settlement_labels(&events).contains(&SettlementLabel::SettledCandidate),
        "the label itself is derived regardless of the projection switch"
    );
    {
        let captured = requests.lock().unwrap();
        assert!(
            captured
                .iter()
                .all(|request| !request.contains("TASK SETTLED")),
            "projection is default-off: no model request may carry the settlement fact"
        );
        assert!(
            captured
                .iter()
                .any(|request| request.contains("TASK PROGRESS anchor_rev=")),
            "the off arm must retain TaskProgress; only the settlement fact is ablated"
        );
    }
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn settlement_fact_reaches_the_request_only_for_the_task_aware_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector, requests) = settlement_instance_with(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("done")],
        true,
    )
    .await;
    let events = drive(&instance, collector).await;
    assert!(settlement_labels(&events).contains(&SettlementLabel::SettledCandidate));
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 3, "three model rounds in one turn");
        assert!(
            !captured[0].contains("TASK SETTLED"),
            "before any mutation the task is not a candidate"
        );
        assert!(
            !captured[1].contains("TASK SETTLED"),
            "after the write there is no trusted verification yet"
        );
        assert!(
            captured[2].contains("TASK SETTLED"),
            "the request consuming the verified result must carry the neutral fact"
        );
    }
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn settlement_fact_disappears_on_reopen_and_returns_after_reverify() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector, requests) = settlement_instance_with(
        dir.path(),
        vec![
            Step::Write("r2"),
            Step::Verify("r2"),
            Step::Write("r3"),
            Step::Verify("r3"),
            Step::Plain("final"),
        ],
        true,
    )
    .await;
    drive(&instance, collector).await;
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 5, "five model rounds across the reopen");
        assert!(captured[2].contains("TASK SETTLED"), "first verify settles");
        assert!(
            !captured[3].contains("TASK SETTLED"),
            "the r3 mutation reopens the candidate immediately"
        );
        assert!(
            captured[4].contains("TASK SETTLED"),
            "the fresh verification settles the candidate again"
        );
    }
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_progress_block_stays_within_cap_when_settlement_projected() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector, requests) = settlement_instance_with(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("done")],
        true,
    )
    .await;
    let events = drive(&instance, collector).await;
    assert!(settlement_labels(&events).contains(&SettlementLabel::SettledCandidate));
    {
        let captured = requests.lock().unwrap();
        assert!(
            captured[0].contains("DECISION BUDGET: round 1 of"),
            "before any mutation the actor still projects its real decision budget"
        );
        let mut measured = 0;
        for request in captured.iter() {
            if let Some((start, end)) = progress_block_bounds(request) {
                measured += 1;
                assert!(
                    end - start <= agent_contracts::MAX_TASK_PROGRESS_PROMPT_CHARS,
                    "TASK PROGRESS must stay under the hard cap even with the settlement line: {}",
                    end - start
                );
            }
        }
        assert_eq!(
            measured, 3,
            "all decisions carry a bounded TASK PROGRESS block, including the budget-only first round"
        );
    }
    instance.shutdown().await.unwrap();
}

/// T1/R2 fixture: an engine that ignores the pack budget and, on its LAST
/// materialization (the settled final round), returns one REQUIRED body far
/// larger than the whole context frame budget. Earlier rounds stay clean so
/// the task can settle; the runtime's final packing layer must then displace
/// the body in that round, record the typed `BudgetExcluded` required miss,
/// and revoke the settlement projection on the exact request being sent.
/// The counts published with that round must describe the request that
/// actually goes out, not the pre-revocation assembly.
#[derive(Debug, Default)]
struct OversizedRequiredBodyEngine {
    calls: std::sync::atomic::AtomicUsize,
    /// The 1-based materialization call that carries the oversized body.
    oversized_on_call: usize,
}

const OVERSIZED_REQUIRED_BODY_MARKER: &str = "OVERSIZED-REQUIRED-BODY-MARKER";

impl OversizedRequiredBodyEngine {
    fn settling_final_round(oversized_on_call: usize) -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            oversized_on_call,
        }
    }
}

#[async_trait::async_trait]
impl ContextEngine for OversizedRequiredBodyEngine {
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
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if call != self.oversized_on_call {
            return Ok(MaterializedContext {
                materialization_id: 0,
                focus: None,
                task: None,
                items: Vec::new(),
                external: agent_contracts::ContextMapView::default(),
                selected: Vec::new(),
                approx_tokens: 0,
                foreground: Vec::new(),
                required_item_ids: Vec::new(),
                required_misses: Default::default(),
                optional_misses: Default::default(),
                diagnostics: ContextDiagnostics::default(),
            });
        }
        let item_id = ContextItemId::new();
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
            task: None,
            items: vec![MaterializedItem {
                item_id,
                kind: ContextKind::FileObservation,
                scope: ContextScope::Task,
                attention: AttentionState::Active,
                semantic: SemanticState::Live,
                retention: ContextRetention::Working,
                content: format!("{OVERSIZED_REQUIRED_BODY_MARKER}{}", "r".repeat(120_000)),
                source: None,
                file_path: None,
                file_revision: None,
                file_start_line: None,
                file_end_line: None,
                partial_body: false,
            }],
            external: agent_contracts::ContextMapView::default(),
            selected: Vec::new(),
            approx_tokens: 30_000,
            foreground: Vec::new(),
            required_item_ids: vec![item_id],
            required_misses: Default::default(),
            optional_misses: Default::default(),
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn open_scope(
        &self,
        _kind: agent_contracts::ScopeKind,
        _parent: Option<agent_contracts::ScopeId>,
    ) -> AgentResult<agent_contracts::ScopeId> {
        Ok(agent_contracts::ScopeId::new())
    }
    async fn close_scope(
        &self,
        _scope_id: agent_contracts::ScopeId,
    ) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
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

/// The settlement rig wired to the over-budget required body engine, with a
/// full wire-request capture. `project_settlement` on means the settled
/// candidate arm is the one actually sent, so the post-miss revocation
/// changes the published request.
#[allow(clippy::type_complexity)]
async fn settlement_instance_with_overbudget_required_body(
    dir: &std::path::Path,
    script: Vec<Step>,
    project_settlement: bool,
) -> (
    RuntimeInstance,
    RuntimeEventEnvelopeCollector,
    Arc<std::sync::Mutex<Vec<ModelRequest>>>,
) {
    let full_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = Arc::new(agent_workspace::Workspace::open(dir).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(OversizedRequiredBodyEngine::settling_final_round(
            script.len(),
        )),
        Arc::new(SettlementModel {
            script,
            rounds: AtomicUsize::new(0),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            full_requests: full_requests.clone(),
        }),
        Arc::new(SettlementToolDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    )
    .with_artifact_workspace(workspace)
    .with_project_task_progress(true)
    .with_project_settlement(project_settlement);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let collector = RuntimeEventEnvelopeCollector {
        events: instance.handle().subscribe(),
    };
    (instance, collector, full_requests)
}

/// R2 counterexample: the round that packs a required-body loss revokes the
/// settlement projection and rebuilds its request. The Ready report and the
/// budget refusal check must then count THAT final request — the whole
/// published triple (request, coverage, counts) comes from one assembly.
/// Before the unified packing slice this scenario could not even publish
/// honestly: the displaced required id stayed listed in `required_item_ids`
/// and the round fenced in the final materialization validation.
#[tokio::test]
async fn settlement_projection_revocation_republishes_counts_from_the_final_request() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector, requests) = settlement_instance_with_overbudget_required_body(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("done")],
        true,
    )
    .await;
    started_task_with_patch(&instance, settled_patch()).await;
    let events = user_turn(&instance, collector).await;
    assert!(
        settlement_labels(&events).contains(&SettlementLabel::SettledCandidate),
        "the verified mutation settles the candidate before the final round"
    );

    let degraded = events
        .iter()
        .filter(|envelope| matches!(envelope.event, RuntimeEvent::ContextDegraded { .. }))
        .count();
    assert!(
        degraded > 0,
        "the displaced required body must degrade the frame honestly, not fence it"
    );
    assert!(
        events
            .iter()
            .all(|envelope| !matches!(envelope.event, RuntimeEvent::TurnCommitFailed { .. })),
        "a typed budget miss is not a preparation fault"
    );

    {
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), 3, "write, verify and final rounds all send");
        // The revocation must actually have fired on the sent arm: the final
        // request is back to the baseline shape.
        let final_text = sent[2]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !final_text.contains("TASK SETTLED"),
            "the required miss revokes the settlement projection from the sent arm"
        );
        assert!(
            !final_text.contains(OVERSIZED_REQUIRED_BODY_MARKER),
            "the oversized required body is honestly absent from the sent request"
        );

        // Counts and request come from the same assembly: every Ready report
        // prices exactly the request its round sent.
        let ready_reports = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                RuntimeEvent::ToolSurfacePlanned { report }
                    if report.status == ToolSurfacePlanStatus::Ready =>
                {
                    Some(report.clone())
                }
                _ => None,
            })
            .collect::<Vec<ToolSurfacePlanReport>>();
        assert_eq!(
            ready_reports.len(),
            sent.len(),
            "one Ready report per sent request"
        );
        for (round, report) in ready_reports.iter().enumerate() {
            let actual = approx_layer_tokens(&sent[round].messages)
                + approx_layer_tokens(&sent[round].tools);
            assert_eq!(
                report.estimated_input_tokens, actual,
                "round {round}: the published count must be recomputed from the final request after the projection revision"
            );
        }
    }
    instance.shutdown().await.unwrap();
}
