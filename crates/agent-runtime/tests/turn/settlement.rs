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
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    RuntimeEventEnvelope, RuntimeDirective, SettlementLabel, ToolCall, ToolDispatcher,
    ToolExecutionAttribution, ToolExecutionRequest, ToolExecutionPurpose, ToolOutcome, ToolOutput,
    ToolRisk, ToolSpec, VerificationReuse,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
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
    /// Propose durable `task.complete`.
    Complete(&'static str),
    /// Ordinary final answer, no tool calls.
    Plain(&'static str),
}

/// Host-side identity material of the scripted verifier. The dispatcher
/// hashes this into the exact verification identity stamped onto the PASS;
/// the task-aware acceptance claims in these tests must carry the same
/// digest or they resolve to nothing.
const VERIFY_IDENTITY_MATERIAL: &str = "scripted verify recipe v1";

fn verify_identity() -> String {
    agent_contracts::ContentDigest::sha256_bytes(VERIFY_IDENTITY_MATERIAL.trim().as_bytes())
        .to_string()
}

/// Plays a round script exactly; a request beyond the script panics, so a
/// runtime that refuses to settle (or auto-stops) fails the test loudly.
/// Every model request's full message text is appended to `requests` so
/// request-level projection tests can assert on the assembled prompt.
#[derive(Debug)]
struct SettlementModel {
    script: Vec<Step>,
    rounds: AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
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
                name: "task.complete".into(),
                description: "propose completion".into(),
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
            .with_verification_identity_material(VERIFY_IDENTITY_MATERIAL),
            _ => ToolExecutionAttribution::default(),
        }
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
            "test.verify" => Ok(ToolOutcome::Value(ToolOutput {
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
            _ => Err(agent_contracts::AgentError::Tool("unexpected tool".into())),
        }
    }
}

async fn settlement_instance_with(
    dir: &std::path::Path,
    script: Vec<Step>,
    project_progress: bool,
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
        }),
        Arc::new(SettlementToolDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    )
    .with_artifact_workspace(workspace)
    .with_project_task_progress(project_progress);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut turn_done = false;
        // Durable post-turn events (checkpoint, TaskCompleted) are emitted
        // after the turn boundary, so once the queue is quiet we sweep a
        // bounded grace window before returning.
        let mut quiet = false;
        while tokio::time::Instant::now() < deadline {
            let mut drained_any = false;
            while let Ok(envelope) = self.events.try_recv() {
                turn_done |= matches!(envelope.event, RuntimeEvent::TurnCompleted);
                drained_any = true;
                seen.push(envelope);
            }
            if turn_done && !drained_any {
                if quiet {
                    break;
                }
                quiet = true;
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
/// this to declare acceptance criteria/coverage and any blocking open loop
/// or next action; the task-aware gate fails closed at `VerifiedCurrent`
/// without it.
async fn started_task_with_patch(
    instance: &RuntimeInstance,
    patch: agent_runtime::AnchorPatch,
) {
    let handle = instance.handle();
    handle.start().await.unwrap();
    handle.set_focus("implement bounded retry".into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let revision = handle.patch_task_anchor(task_id, 0, patch).await.unwrap();
    assert_eq!(revision, 1, "the declared task anchor must advance by one");
}

/// The acceptance authority every settled-candidate scenario needs: one
/// criterion with a coverage claim resolving to the scripted verifier's
/// exact identity.
fn settled_patch() -> agent_runtime::AnchorPatch {
    agent_runtime::AnchorPatch {
        acceptance_criteria: Some(vec!["tests pass for the current world".into()]),
        acceptance_coverage: Some(vec![agent_runtime::task::AcceptanceCoverage {
            criterion_index: 0,
            verification_identity: verify_identity(),
        }]),
        ..agent_runtime::AnchorPatch::default()
    }
}

async fn user_turn(
    instance: &RuntimeInstance,
    collector: RuntimeEventEnvelopeCollector,
) -> Vec<RuntimeEventEnvelope> {
    let handle = instance.handle();
    handle.set_focus("implement bounded retry".into()).await.unwrap();
    handle.user_message("keep going".into()).await.unwrap();
    collector.drain_until_turn_completed().await
}

async fn drive(instance: &RuntimeInstance, collector: RuntimeEventEnvelopeCollector) -> Vec<RuntimeEventEnvelope> {
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
    let (instance, collector) =
        settlement_instance(dir.path(), vec![Step::Write("r2"), Step::Verify("r2"), Step::Complete("done")]).await;
    let events = drive(&instance, collector).await;

    let labels = settlement_labels(&events);
    assert!(
        labels.contains(&SettlementLabel::SettledCandidate),
        "the verified mutation must surface a settled candidate: {labels:?}"
    );
    assert!(
        events.iter().any(|envelope| matches!(
            envelope.event,
            RuntimeEvent::TaskCompleted { .. }
        )),
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
async fn settled_work_leaves_ordinary_final_user_choice_intact() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("finished")],
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
        events.iter().any(|envelope| matches!(
            envelope.event,
            RuntimeEvent::AssistantMessage { .. }
        )),
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
async fn unmet_acceptance_coverage_stays_verified_current_not_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("coverage?")],
    )
    .await;
    // Criteria without any coverage claim: the gate fails closed.
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            acceptance_criteria: Some(vec!["tests pass for the current world".into()]),
            ..agent_runtime::AnchorPatch::default()
        },
    )
    .await;
    let events = user_turn(&instance, collector).await;
    let labels = settlement_labels(&events);
    assert!(
        labels.contains(&SettlementLabel::VerifiedCurrent),
        "execution is ready but no criterion has explicit evidence: {labels:?}"
    );
    assert!(
        !labels.contains(&SettlementLabel::SettledCandidate),
        "a declared criterion without explicit coverage must never be a candidate: {labels:?}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn open_loop_blocks_candidate_even_with_full_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("loop open")],
    )
    .await;
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            acceptance_criteria: Some(vec!["tests pass for the current world".into()]),
            acceptance_coverage: Some(vec![agent_runtime::task::AcceptanceCoverage {
                criterion_index: 0,
                verification_identity: verify_identity(),
            }]),
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
async fn next_action_blocks_candidate_even_with_full_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let (instance, collector) = settlement_instance(
        dir.path(),
        vec![Step::Write("r2"), Step::Verify("r2"), Step::Plain("next action")],
    )
    .await;
    started_task_with_patch(
        &instance,
        agent_runtime::AnchorPatch {
            acceptance_criteria: Some(vec!["tests pass for the current world".into()]),
            acceptance_coverage: Some(vec![agent_runtime::task::AcceptanceCoverage {
                criterion_index: 0,
                verification_identity: verify_identity(),
            }]),
            next_action: Some("write the missing test".into()),
            ..agent_runtime::AnchorPatch::default()
        },
    )
    .await;
    let events = user_turn(&instance, collector).await;
    let labels = settlement_labels(&events);
    assert!(
        !labels.contains(&SettlementLabel::SettledCandidate),
        "a non-empty next action must block the candidate: {labels:?}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn boundary_criterion_change_reopens_and_redeclared_coverage_resettles() {
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

    // Boundary change: a second criterion moves the verification basis and
    // leaves one criterion without a coverage claim (identity still covers
    // only the first).
    let handle = instance.handle();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let revision = handle
        .patch_task_anchor(
            task_id,
            1,
            agent_runtime::AnchorPatch {
                acceptance_criteria: Some(vec![
                    "tests pass for the current world".into(),
                    "api unchanged".into(),
                ]),
                acceptance_coverage: Some(vec![agent_runtime::task::AcceptanceCoverage {
                    criterion_index: 0,
                    verification_identity: verify_identity(),
                }]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 2);

    let reopened = continue_turn(
        &instance,
        RuntimeEventEnvelopeCollector {
            events: instance.handle().subscribe(),
        },
    )
    .await;
    let reopened_labels = settlement_labels(&reopened);
    assert!(
        !reopened_labels.contains(&SettlementLabel::SettledCandidate),
        "the moved basis and new criterion must reopen the proposal: {reopened_labels:?}"
    );
    assert!(
        reopened_labels.contains(&SettlementLabel::VerifiedCurrent),
        "a fresh verification covers the new basis while the uncovered criterion keeps it below candidate: {reopened_labels:?}"
    );

    // Now claim the second criterion too; the next verified mutation must
    // settle again.
    let revision = handle
        .patch_task_anchor(
            task_id,
            2,
            agent_runtime::AnchorPatch {
                acceptance_coverage: Some(vec![
                    agent_runtime::task::AcceptanceCoverage {
                        criterion_index: 0,
                        verification_identity: verify_identity(),
                    },
                    agent_runtime::task::AcceptanceCoverage {
                        criterion_index: 1,
                        verification_identity: verify_identity(),
                    },
                ]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 3);

    let resettled = continue_turn(
        &instance,
        RuntimeEventEnvelopeCollector {
            events: instance.handle().subscribe(),
        },
    )
    .await;
    assert!(
        settlement_labels(&resettled).contains(&SettlementLabel::SettledCandidate),
        "full declared coverage with a fresh verification must settle: {:?}",
        settlement_labels(&resettled)
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
    let resumed = continue_turn(
        &instance,
        RuntimeEventEnvelopeCollector {
            events: instance.handle().subscribe(),
        },
    )
    .await;
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
            !captured[0].contains("TASK PROGRESS anchor_rev="),
            "before any mutation the progress view is empty and renders no block"
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
            measured, 2,
            "the write and the verified rounds both carry a bounded TASK PROGRESS block"
        );
    }
    instance.shutdown().await.unwrap();
}