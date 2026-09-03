use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextMaterializationIdentity,
    ContextMaterializationMiss, ContextMaterializationMissReason, ContextMaterializationMisses,
    ContextQuery, ContextStateTransition, EventJournal, MaterializedContext, ModelCapabilities,
    ModelChunk, ModelEventSink, ModelOutput, ModelRequest, ModelTransport, RunId, RuntimeEvent,
    RuntimeEventEnvelope, ScopeId, ScopeKind, ToolCall, ToolDispatcher, ToolExecutionAttribution,
    ToolExecutionPurpose, ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
    VerificationReuse,
};

use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;

use crate::harness::*;

const COMPLETION_VERIFY_IDENTITY_MATERIAL: &str = "completion-test verifier v1";
const COMPLETION_ACCEPTANCE_DOMAIN: &str = "completion-fixture";

/// Fails exactly the checkpoint captured while TaskCompleted is prepared.
/// The subsequent rollback and failure-resume checkpoint remain available,
/// which isolates the cross-plane transaction path from store failures.
#[derive(Debug, Default)]
struct FailTerminalCheckpointContext {
    terminal_prepared: tokio::sync::Mutex<bool>,
    fail_once: AtomicBool,
}

#[derive(Debug)]
struct FailTaskCompletedJournal;

#[async_trait::async_trait]
impl EventJournal for FailTaskCompletedJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::TaskCompleted { .. }) {
            return Err(AgentError::Storage(
                "injected terminal audit append failure".into(),
            ));
        }
        Ok(())
    }
}

impl FailTerminalCheckpointContext {
    fn new() -> Self {
        Self {
            terminal_prepared: tokio::sync::Mutex::new(false),
            fail_once: AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl ContextEngine for FailTerminalCheckpointContext {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::TaskCompleted { .. }) {
            *self.terminal_prepared.lock().await = true;
        }
        Ok(())
    }

    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }

    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext::default())
    }

    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }

    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
    }

    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }

    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        let terminal_prepared = *self.terminal_prepared.lock().await;
        if terminal_prepared && self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(AgentError::Storage(
                "injected terminal checkpoint assembly failure".into(),
            ));
        }
        Ok(json!({ "terminal_prepared": terminal_prepared }))
    }

    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        *self.terminal_prepared.lock().await = data
            .get("terminal_prepared")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(())
    }
}

/// Prepend one trusted verifier decision without changing the wrapped model's
/// own round script. Successful model-side completion tests must establish the
/// same evidence authority production now requires.
#[derive(Debug)]
struct WithCompletionVerificationModel<M> {
    inner: Arc<M>,
    verification_sent: std::sync::atomic::AtomicBool,
}

impl<M> WithCompletionVerificationModel<M> {
    fn new(inner: Arc<M>) -> Self {
        Self {
            inner,
            verification_sent: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl<M> ModelTransport for WithCompletionVerificationModel<M>
where
    M: ModelTransport + std::fmt::Debug + 'static,
{
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        if !self
            .verification_sent
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "completion-verify".into(),
                    name: "test.verify".into(),
                    arguments: json!({"command": "test completion fixture"}),
                }],
                usage: Default::default(),
            });
        }
        self.inner.complete(request).await
    }
}

/// Add one exact-current-world verifier to any completion dispatcher while
/// preserving the dispatcher's own completion/artifact behavior.
#[derive(Debug)]
struct WithCompletionVerificationTools<D> {
    inner: D,
}

fn completion_acceptance_declaration() -> agent_contracts::VerificationCoverageDeclaration {
    agent_contracts::VerificationCoverageDeclaration {
        domain_id: COMPLETION_ACCEPTANCE_DOMAIN.into(),
        declaration_revision: 1,
        source_digest: agent_contracts::ContentDigest::sha256_bytes(
            b"completion-acceptance-declaration/v1",
        )
        .to_string(),
    }
}

#[async_trait::async_trait]
impl<D> ToolDispatcher for WithCompletionVerificationTools<D>
where
    D: ToolDispatcher + std::fmt::Debug + Send + Sync,
{
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(ToolSpec {
            name: "test.verify".into(),
            description: "trusted completion fixture verifier".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        });
        specs
    }

    fn execution_attribution(&self, call: &ToolCall) -> ToolExecutionAttribution {
        if call.name == "test.verify" {
            return ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                Vec::<String>::new(),
                VerificationReuse::ExactCurrentWorld,
            )
            .with_verification_identity_material(COMPLETION_VERIFY_IDENTITY_MATERIAL)
            .with_verification_recipe(agent_contracts::VerificationRecipeProvenance {
                recipe_id: "test.verify".into(),
                recipe_revision: "v1".into(),
                coverage_domain: Some(COMPLETION_ACCEPTANCE_DOMAIN.into()),
                domain_declaration_revision: Some(1),
                domain_source_digest: completion_acceptance_declaration().source_digest,
                class_identity_digest: "completion-fixture-class".into(),
            });
        }
        self.inner.execution_attribution(call)
    }

    fn verification_coverage_declarations(
        &self,
    ) -> Vec<agent_contracts::VerificationCoverageDeclaration> {
        vec![completion_acceptance_declaration()]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if request.call.name == "test.verify" {
            return Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "completion fixture verified".into(),
                model_content: "completion fixture verified".into(),
                artifact_ref: None,
                metadata: json!({"verification": true, "command": "test completion fixture"}),
            }));
        }
        self.inner.execute(request).await
    }
}

async fn declare_completion_acceptance(handle: &agent_runtime::RuntimeHandle, goal: &str) {
    handle.set_focus(goal.into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                completion_policy: Some(
                    agent_runtime::task::TaskCompletionPolicy::EvidenceRequired,
                ),
                acceptance_criteria: Some(vec![
                    agent_runtime::task::AcceptanceCriterion::declared(
                        "trusted completion fixture passes",
                        &completion_acceptance_declaration(),
                    ),
                ]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Structured completion: `task.complete` attaches a typed proposal that the
// runtime commits at the turn's safe point (after the turn commits) as the
// active task's CompletionRecord.
// ---------------------------------------------------------------------------

/// Calls `task.complete` with the given summary on round 0, then finishes.
#[derive(Debug)]
struct CompletionProposalModel {
    summary: &'static str,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for CompletionProposalModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "task.complete".into(),
                    arguments: json!({"summary": self.summary, "artifacts": []}),
                }],
                usage: Default::default(),
            })
        } else {
            Ok(ModelOutput {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }
}

/// Proposes completion once, then records the next request so refusal tests
/// can prove the authoritative gate result reached the model rather than
/// existing only as an audit warning.
#[derive(Debug)]
struct CompletionRefusalModel {
    rounds: AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ModelTransport for CompletionRefusalModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        Ok(if round == 0 {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "completion-refusal".into(),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "optimistic completion", "artifacts": []}),
                }],
                usage: Default::default(),
            }
        } else {
            ModelOutput {
                content: "continuing after the refusal".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }
        })
    }
}

/// Serves `task.complete` by attaching the typed completion directive,
/// exactly like the real tool.
#[derive(Debug)]
struct CompletionToolDispatcher {
    workspace: Option<agent_workspace::Workspace>,
}

#[async_trait::async_trait]
impl ToolDispatcher for CompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "task.complete".into(),
            description: "propose completion".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let summary: String = request.call.arguments["summary"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut artifacts: Vec<String> = request.call.arguments["artifacts"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(workspace) = &self.workspace {
            artifacts.push(
                workspace
                    .write_artifact(request.run_id, "completion", "txt", b"completion evidence")
                    .await?,
            );
        }
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
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal { summary, artifacts },
            ),
        })
    }
}

#[derive(Debug)]
struct CompletionProgressDispatcher {
    inner: CompletionToolDispatcher,
}

#[async_trait::async_trait]
impl ToolDispatcher for CompletionProgressDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(ToolSpec {
            name: "task.manage".into(),
            description: "update task progress".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        });
        specs
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if request.call.name != "task.manage" {
            return self.inner.execute(request).await;
        }
        let base_anchor_revision = request.call.arguments["base_anchor_revision"]
            .as_u64()
            .unwrap_or_default();
        let current_interpretation = request.call.arguments["current_interpretation"]
            .as_str()
            .map(str::to_string);
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "progress proposed".into(),
                model_content: String::new(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::UpdateTaskProgress(
                agent_contracts::TaskProgressProposal {
                    base_anchor_revision,
                    current_interpretation,
                    plan_progress: None,
                    open_loops: None,
                    next_action: None,
                },
            ),
        })
    }
}

async fn run_model_visible_completion_refusal(
    declare_evidence_policy: bool,
) -> (
    ToolOutput,
    Vec<String>,
    agent_runtime::checkpoint::RuntimeCheckpoint,
) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = Arc::new(CompletionRefusalModel {
        rounds: AtomicUsize::new(0),
        requests: requests.clone(),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher { workspace: None },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    if declare_evidence_policy {
        declare_completion_acceptance(handle, "finish only with evidence").await;
    } else {
        handle
            .set_focus("operator-owned completion".into())
            .await
            .unwrap();
    }
    handle.user_message("try to finish".into()).await.unwrap();

    let mut completion_output = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    completion_output = Some(output);
                }
                RuntimeEvent::TaskCompleted { .. } => {
                    panic!("a refused completion proposal must not complete the task")
                }
                RuntimeEvent::TurnCompleted if completion_output.is_some() => break,
                _ => {}
            }
        }
    }
    let output = completion_output.expect("task.complete must publish its authoritative result");
    let checkpoint = instance.checkpoint().await.unwrap();
    instance.shutdown().await.unwrap();
    let captured = requests.lock().unwrap().clone();
    (output, captured, checkpoint)
}

/// A context engine whose materialized frame always reports one required
/// context miss (budget exclusion), so completion readiness must surface
/// `RequiredContextUnavailable` and refuse the proposal.
#[derive(Debug, Default)]
struct RequiredMissContextEngine;

#[async_trait::async_trait]
impl ContextEngine for RequiredMissContextEngine {
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
        let mut required_misses = ContextMaterializationMisses::default();
        required_misses.push(ContextMaterializationMiss {
            identity: ContextMaterializationIdentity::new(
                "context://run/required",
                None,
                "evidence_refs",
                1,
            ),
            reason: ContextMaterializationMissReason::BudgetExcluded,
        });
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
            task: None,
            items: Vec::new(),
            external: agent_contracts::ContextMapView::default(),
            selected: Vec::new(),
            approx_tokens: 0,
            foreground: Vec::new(),
            required_item_ids: Vec::new(),
            required_misses,
            optional_misses: Default::default(),
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
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

/// The hard-required-context-miss negative of the completion matrix: a
/// materialized frame that cannot supply a required body keeps completion
/// refused with the typed blocker and an operator repair stage, even when
/// every evidence row is otherwise current.
#[tokio::test]
async fn hard_required_context_miss_keeps_completion_refused() {
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(RequiredMissContextEngine),
        Arc::new(CompletionRefusalModel {
            rounds: AtomicUsize::new(0),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher { workspace: None },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "finish only with evidence").await;
    handle.user_message("try to finish".into()).await.unwrap();

    let mut completion_output = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    completion_output = Some(output);
                }
                RuntimeEvent::TaskCompleted { .. } => {
                    panic!("a required-context miss must keep completion refused")
                }
                RuntimeEvent::TurnCompleted if completion_output.is_some() => break,
                _ => {}
            }
        }
    }
    let output = completion_output.expect("task.complete must publish its authoritative result");
    assert!(!output.ok);
    assert_eq!(output.metadata["refused"].as_str(), Some("completion_gate"));
    let blockers = output.metadata["blockers"]
        .as_array()
        .expect("refusal must carry typed blockers");
    assert!(
        blockers.iter().any(|blocker| blocker
            .get("required_context_unavailable")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|value| value["remaining"] == 1)),
        "the typed required-context blocker must be present: {blockers:?}"
    );
    assert_eq!(
        output.metadata["repair_plan"]["steps"][0]["kind"],
        "operator_required"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_complete_without_evidence_policy_returns_a_model_visible_refusal() {
    let (output, requests, checkpoint) = run_model_visible_completion_refusal(false).await;
    assert!(!output.ok);
    assert_eq!(output.metadata["refused"], "completion_gate");
    assert!(
        output.metadata["blockers"]
            .to_string()
            .contains("operator_closure_only")
    );
    assert!(
        output.metadata["blockers"]
            .to_string()
            .contains("acceptance_undeclared")
    );
    assert!(output.model_content.contains("was not accepted by Runtime"));
    assert!(output.model_content.contains("completion_repair/v2"));
    assert_eq!(
        output.metadata["repair_plan"]["schema"],
        "completion-repair.v2"
    );
    assert_eq!(
        output.metadata["repair_plan"]["steps"][0]["kind"],
        "operator_required"
    );
    assert!(
        requests
            .get(1)
            .is_some_and(|request| request.contains("was not accepted by Runtime")),
        "the next model decision must receive the refusal output"
    );
    assert!(
        requests.get(1).is_some_and(|request| {
            request.contains("completion_repair/v2 basis")
                && request.contains("TASK PROGRESS")
                && request.contains("operator_required")
        }),
        "the next decision must receive the freshly derived repair record in TASK PROGRESS"
    );
    assert!(checkpoint.tasks.completed.is_empty());
}

#[tokio::test]
async fn task_complete_without_a_registered_proof_route_fails_closed() {
    let (output, requests, checkpoint) = run_model_visible_completion_refusal(true).await;
    assert!(!output.ok);
    assert_eq!(output.metadata["refused"], "completion_gate");
    assert!(
        output.metadata["blockers"]
            .to_string()
            .contains("acceptance_uncovered")
    );
    assert!(output.model_content.contains("lack current coverage"));
    assert!(
        output.metadata["repair_plan"]["steps"]
            .as_array()
            .is_some_and(|steps| steps.len() == 1 && steps[0]["kind"] == "operator_required")
    );
    assert!(
        requests
            .get(1)
            .is_some_and(|request| request.contains("cannot prove a current exact recipe_id"))
    );
    assert!(
        requests
            .get(1)
            .is_some_and(|request| request.contains("lack current coverage")),
        "the uncovered criterion must be actionable in the next model decision"
    );
    assert!(checkpoint.tasks.completed.is_empty());
}

#[tokio::test]
async fn task_complete_proposal_commits_the_typed_record_at_turn_end() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let model = Arc::new(CompletionProposalModel {
        summary: "the task is done",
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(WithCompletionVerificationModel::new(model.clone())),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher {
                workspace: Some((*workspace).clone()),
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "finish the work").await;
    handle.user_message("finish the work".into()).await.unwrap();

    let mut completed_event = None;
    let mut pending_receipt = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::ToolFinished { output, .. } = &envelope.event
                && output.tool_name == "task.complete"
            {
                pending_receipt = Some(output.clone());
            }
            if let RuntimeEvent::TaskCompleted {
                task_id,
                anchor_revision,
                summary,
            } = &envelope.event
            {
                completed_event = Some((*task_id, *anchor_revision, summary.clone()));
            }
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                break;
            }
        }
        if completed_event.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (task_id, anchor_revision, summary) =
        completed_event.expect("the completion proposal must commit");
    assert_eq!(summary, "the task is done");
    let pending_receipt = pending_receipt.expect("accepted tool output is audited");
    assert!(pending_receipt.ok);
    assert_eq!(
        pending_receipt.metadata["completion_state"],
        "pending_terminal_commit"
    );
    assert!(
        pending_receipt
            .model_content
            .contains("pending_terminal_commit")
    );

    // The typed record is durable in the checkpoint, with the proposal's
    // artifact ref attached — the shared commit transaction end to end.
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint
        .tasks
        .completed
        .iter()
        .find(|record| record.task_id == task_id)
        .expect("a completed task owns exactly one CompletionRecord");
    assert_eq!(record.anchor_revision, anchor_revision);
    assert_eq!(record.summary, "the task is done");
    assert_eq!(record.artifacts.len(), 2);
    assert!(
        record
            .artifacts
            .iter()
            .any(|reference| reference.contains("/completion/"))
    );
    assert!(
        record
            .artifacts
            .iter()
            .any(|reference| reference.contains("/assistant-response/"))
    );
    assert!(
        record.final_output_digest.is_some(),
        "the final output digest must be retained"
    );
    assert_eq!(
        model.rounds.load(Ordering::SeqCst),
        1,
        "accepted task.complete is already the terminal model decision"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn deferred_completion_failure_rolls_back_context_and_persists_resume_fact() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let context = Arc::new(FailTerminalCheckpointContext::new());
    let model = Arc::new(CompletionProposalModel {
        summary: "the task is done",
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(WithCompletionVerificationModel::new(model)),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher {
                workspace: Some((*workspace).clone()),
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "finish the work").await;
    handle.user_message("finish the work".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut failure = None;
    let mut failure_checkpoint_landed = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(100), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::CompletionCommitFailed {
                    task_id,
                    retryable,
                    reason,
                } => failure = Some((task_id, retryable, reason)),
                RuntimeEvent::CheckpointDurable { .. } if failure.is_some() => {
                    failure_checkpoint_landed = true;
                    break;
                }
                _ => {}
            }
        }
    }

    let (task_id, retryable, reason) = failure.expect("typed failure must be audited");
    assert!(retryable);
    assert!(reason.contains("injected terminal checkpoint"));
    assert!(
        failure_checkpoint_landed,
        "the failure resume must reach a durable checkpoint"
    );
    assert!(
        !*context.terminal_prepared.lock().await,
        "failed terminal assembly must restore the pre-completion context plane"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(!checkpoint.terminal_commit);
    assert_eq!(checkpoint.current_task_id, Some(task_id));
    let task = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .unwrap();
    let projected = task
        .resume
        .completion_commit_failure
        .as_ref()
        .expect("the bounded failure is checkpointed on the active task");
    assert_eq!(projected.attempts, 1);
    assert!(projected.reason.contains("injected terminal checkpoint"));
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn post_commit_audit_failure_never_projects_a_pending_completion_failure() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(WithCompletionVerificationModel::new(Arc::new(
            CompletionProposalModel {
                summary: "the task is done",
                rounds: AtomicUsize::new(0),
            },
        ))),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher {
                workspace: Some((*workspace).clone()),
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailTaskCompletedJournal)),
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "finish the work").await;
    handle.user_message("finish the work".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_recovery = false;
    let mut saw_pending_failure = false;
    let mut saw_pending_failure_debt = false;
    while tokio::time::Instant::now() < deadline && !saw_recovery {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(100), events.recv()).await
        {
            saw_recovery |= matches!(&envelope.event, RuntimeEvent::RecoveryRequired);
            saw_pending_failure |=
                matches!(&envelope.event, RuntimeEvent::CompletionCommitFailed { .. });
            if let RuntimeEvent::TaskResumeCommitted { debt, .. } = &envelope.event {
                saw_pending_failure_debt |=
                    debt.iter().any(|row| row == "completion_commit_failed");
            }
        }
    }
    assert!(saw_recovery, "the terminal audit gap must fence the run");
    assert!(
        !saw_pending_failure,
        "a committed task cannot be projected as a retryable pending completion"
    );
    let tasks = handle.list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, agent_runtime::TaskStatus::Completed);
    assert!(!saw_pending_failure_debt);
    instance.shutdown().await.unwrap();
}

/// A completion proposal followed by a failed sibling action must not skip
/// the model's recovery decision. This guards the conservative half of the
/// one-shot rule: only an entirely successful batch terminalizes directly.
#[derive(Debug)]
struct CompletionWithFailedSiblingModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for CompletionWithFailedSiblingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "complete".into(),
                        name: "task.complete".into(),
                        arguments: json!({"summary": "completion still stands", "artifacts": []}),
                    },
                    ToolCall {
                        id: "fail".into(),
                        name: "always.fail".into(),
                        arguments: json!({}),
                    },
                ],
                usage: Default::default(),
            })
        } else {
            Ok(ModelOutput {
                content: "handled the failed sibling".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }
}

#[derive(Debug)]
struct CompletionWithFailureDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for CompletionWithFailureDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = CompletionToolDispatcher { workspace: None }.specs();
        specs.push(ToolSpec {
            name: "always.fail".into(),
            description: "deterministic test failure".into(),
            input_schema: json!({"type": "object", "additionalProperties": false}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        });
        specs
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if request.call.name == "task.complete" {
            return CompletionToolDispatcher { workspace: None }
                .execute(request)
                .await;
        }
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: false,
            summary: "expected failure".into(),
            model_content: "expected failure".into(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

#[tokio::test]
async fn task_complete_waits_for_model_when_a_sibling_action_failed() {
    let model = Arc::new(CompletionWithFailedSiblingModel {
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(WithCompletionVerificationModel::new(model.clone())),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionWithFailureDispatcher,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "finish carefully").await;
    handle
        .user_message("finish carefully".into())
        .await
        .unwrap();

    let mut failed_batch_seen = false;
    let mut settled_failures = Vec::new();
    let mut assistant_content = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ExecutionBatchSettled { failed, .. } => {
                    settled_failures.push(failed);
                    failed_batch_seen |= failed == 1;
                }
                RuntimeEvent::AssistantMessage { content } => assistant_content = Some(content),
                RuntimeEvent::TaskCompleted { .. } => break,
                _ => {}
            }
        }
    }

    assert!(
        failed_batch_seen,
        "the failed sibling must be audited; settled failure counts={settled_failures:?}"
    );
    assert_eq!(
        assistant_content.as_deref(),
        Some("handled the failed sibling")
    );
    assert_eq!(
        model.rounds.load(Ordering::SeqCst),
        2,
        "the failed batch must be returned to the model"
    );
    instance.shutdown().await.unwrap();
}
// ---------------------------------------------------------------------------

/// A model that answers with one very long plain-text message — far beyond
/// the engine's bounded ContextItem cap — so the raw-evidence artifact is
/// the only place the *complete* final response survives.
#[derive(Debug)]
struct LongResponseModel(usize);

#[async_trait::async_trait]
impl ModelTransport for LongResponseModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "x".repeat(self.0),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        sink.on_chunk(ModelChunk::Done).await?;
        self.complete(request).await
    }
}

/// Raw-evidence retention (CONTEXT_RUNTIME_TODO "Persist the exact final
/// response before ContextItem truncation"): with an artifact workspace
/// wired, the actor writes the *full* final assistant response to an
/// artifact before the bounded ContextItem is built, so an oversized
/// response survives intact even though the engine's copy would truncate
/// it.
#[tokio::test]
async fn final_assistant_response_is_persisted_in_full_before_contextitem_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    // Far beyond the default ContextItem cap (16,000 chars): only an
    // untruncated artifact preserves the raw output.
    let content_len = 40_000;
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(LongResponseModel(content_len)),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace.clone());
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    handle
        .user_message("write the report".into())
        .await
        .unwrap();

    // The file is created before it is populated, so path existence is not
    // a publication barrier. `TurnCompleted` is emitted only after the
    // pinned artifact handle has been fully written and flushed.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(envelope) if matches!(envelope.event, RuntimeEvent::TurnCompleted) => break,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime event stream closed before TurnCompleted")
                }
            }
        }
    })
    .await
    .expect("the turn must complete before reading its raw evidence");

    // Read the single published assistant-response artifact back.
    // user-input bodies also live under artifacts/; this assertion is about
    // the final assistant response only.
    let artifacts_dir = workspace.state_dir().join("artifacts");
    let artifacts = collect_owner_files(&artifacts_dir, "assistant-response");
    assert_eq!(
        artifacts.len(),
        1,
        "exactly one assistant-response artifact per final response, got {artifacts:?}"
    );
    let content = std::fs::read_to_string(&artifacts[0]).unwrap();
    assert_eq!(
        content.len(),
        content_len,
        "the artifact must carry the complete untruncated response"
    );
}

fn collect_txt_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(collect_txt_files(&path));
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !name.starts_with('.'))
            {
                out.push(path);
            }
        }
    }
    out
}

fn collect_owner_files(dir: &std::path::Path, owner: &str) -> Vec<std::path::PathBuf> {
    collect_txt_files(dir)
        .into_iter()
        .filter(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .contains(&format!("/{owner}/"))
        })
        .collect()
}

/// Proposes `task.complete` with a bounded but non-trivial summary. The
/// summary itself is the terminal assistant response, so Runtime must write
/// that exact body as raw evidence without another model call.
#[derive(Debug)]
struct CompletingLongModel {
    rounds: AtomicUsize,
    content_len: usize,
}

#[async_trait::async_trait]
impl ModelTransport for CompletingLongModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "task.complete".into(),
                    arguments: json!({
                        "summary": "x".repeat(self.content_len),
                        "artifacts": []
                    }),
                }],
                usage: Default::default(),
            })
        } else {
            panic!("accepted task.complete must not request a confirmation round")
        }
    }
}

/// The CompletionRecord carries the raw-evidence artifact of the terminal
/// completion summary, independent of the model's self-declared artifacts.
#[tokio::test]
async fn completion_record_attaches_the_raw_final_response_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let content_len = agent_contracts::MAX_COMPLETION_SUMMARY_CHARS;
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(WithCompletionVerificationModel::new(Arc::new(
            CompletingLongModel {
                rounds: AtomicUsize::new(0),
                content_len,
            },
        ))),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher { workspace: None },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace.clone());
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "finish the work").await;
    handle.user_message("finish the work".into()).await.unwrap();

    let mut task_id = None;
    let mut events = handle.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskCompleted {
                task_id: completed_task,
                ..
            } = envelope.event
            {
                task_id = Some(completed_task);
            }
        }
        if task_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let task_id = task_id.expect("the completion proposal must commit");

    // The CompletionRecord carries exactly one raw-evidence ref, naming the
    // assistant-response artifact.
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint
        .tasks
        .completed
        .iter()
        .find(|record| record.task_id == task_id)
        .expect("a completed task owns exactly one CompletionRecord");
    let raw_refs: Vec<&String> = record
        .artifacts
        .iter()
        .filter(|reference| reference.contains("assistant-response"))
        .collect();
    assert_eq!(
        raw_refs.len(),
        1,
        "the CompletionRecord must attach the raw final-response artifact: {:?}",
        record.artifacts
    );

    // The artifact exists and carries the complete untruncated response.
    let artifacts_dir = workspace.state_dir().join("artifacts");
    let files = collect_owner_files(&artifacts_dir, "assistant-response");
    assert_eq!(
        files.len(),
        1,
        "one assistant-response artifact per final response"
    );
    let content = std::fs::read_to_string(&files[0]).unwrap();
    assert_eq!(
        content.len(),
        content_len,
        "the raw response must be intact"
    );
    assert!(
        files[0]
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("assistant-response")
            || raw_refs[0].contains("assistant-response"),
        "the attached ref must name the assistant-response artifact"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_proposal_cannot_attach_a_cross_run_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let foreign_ref = workspace
        .write_artifact(RunId::new(), "foreign", "txt", b"foreign evidence")
        .await
        .unwrap();
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionProposalModel {
            summary: "must not commit",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(FixedCompletionToolDispatcher {
            artifact: foreign_ref,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    handle.user_message("finish".into()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(
        checkpoint.tasks.completed.is_empty(),
        "a foreign-run evidence ref must not enter a CompletionRecord"
    );
    instance.shutdown().await.unwrap();
}

#[derive(Debug)]
struct FixedCompletionToolDispatcher {
    artifact: String,
}

#[async_trait::async_trait]
impl ToolDispatcher for FixedCompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        CompletionToolDispatcher { workspace: None }.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
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
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary,
                    artifacts: vec![self.artifact.clone()],
                },
            ),
        })
    }
}

#[derive(Debug)]
struct BulkCompletionToolDispatcher {
    workspace: agent_workspace::Workspace,
    unique_artifacts: usize,
    duplicate_first: bool,
}

#[async_trait::async_trait]
impl ToolDispatcher for BulkCompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        CompletionToolDispatcher { workspace: None }.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let mut artifacts = Vec::new();
        for index in 0..self.unique_artifacts {
            artifacts.push(
                self.workspace
                    .write_artifact(
                        request.run_id,
                        &format!("proposal-{index:02}"),
                        "txt",
                        format!("evidence {index}").as_bytes(),
                    )
                    .await?,
            );
        }
        if self.duplicate_first && !artifacts.is_empty() {
            artifacts.insert(1, artifacts[0].clone());
        }
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
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary: "complete with evidence".into(),
                    artifacts,
                },
            ),
        })
    }
}

#[derive(Debug)]
struct DirectoryCompletionToolDispatcher {
    workspace: agent_workspace::Workspace,
}

#[async_trait::async_trait]
impl ToolDispatcher for DirectoryCompletionToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        CompletionToolDispatcher { workspace: None }.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        // Materialize the run directory, then try to smuggle that directory
        // into the proposal as though it were an artifact file.
        self.workspace
            .write_artifact(request.run_id, "seed", "txt", b"seed")
            .await?;
        let directory = format!("artifact://.focus-agent/artifacts/{}", request.run_id);
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
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary: "must not commit".into(),
                    artifacts: vec![directory],
                },
            ),
        })
    }
}

async fn wait_for_completed_record(
    instance: &RuntimeInstance,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> agent_runtime::checkpoint::RuntimeCheckpoint {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })
        {
            return instance.checkpoint().await.unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "completion did not commit before deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn completion_artifacts_keep_raw_evidence_first_and_cap_the_merged_set() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(WithCompletionVerificationModel::new(Arc::new(
            CompletionProposalModel {
                summary: "ignored by dispatcher",
                rounds: AtomicUsize::new(0),
            },
        ))),
        Arc::new(WithCompletionVerificationTools {
            inner: BulkCompletionToolDispatcher {
                workspace: (*workspace).clone(),
                unique_artifacts: agent_contracts::MAX_COMPLETION_ARTIFACTS,
                duplicate_first: false,
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
    declare_completion_acceptance(handle, "finish with many artifacts").await;
    handle
        .user_message("finish with many artifacts".into())
        .await
        .unwrap();

    let checkpoint = wait_for_completed_record(&instance, &mut events).await;
    let artifacts = &checkpoint.tasks.completed[0].artifacts;
    assert_eq!(artifacts.len(), agent_contracts::MAX_COMPLETION_ARTIFACTS);
    assert!(artifacts[0].contains("assistant-response"));
    assert!(artifacts[1].contains("proposal-00"));
    assert!(artifacts.iter().any(|item| item.contains("proposal-30")));
    assert!(!artifacts.iter().any(|item| item.contains("proposal-31")));
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_artifacts_are_normalized_and_stably_deduplicated() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(WithCompletionVerificationModel::new(Arc::new(
            CompletionProposalModel {
                summary: "ignored by dispatcher",
                rounds: AtomicUsize::new(0),
            },
        ))),
        Arc::new(WithCompletionVerificationTools {
            inner: BulkCompletionToolDispatcher {
                workspace: (*workspace).clone(),
                unique_artifacts: 1,
                duplicate_first: true,
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
    declare_completion_acceptance(handle, "finish with duplicate artifacts").await;
    handle
        .user_message("finish with duplicate artifacts".into())
        .await
        .unwrap();

    let checkpoint = wait_for_completed_record(&instance, &mut events).await;
    let artifacts = &checkpoint.tasks.completed[0].artifacts;
    assert_eq!(artifacts.len(), 2, "raw evidence plus one unique proposal");
    assert!(artifacts[0].contains("assistant-response"));
    assert!(artifacts[1].contains("proposal-00"));
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_safe_point_rejects_a_current_run_directory_reference() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionProposalModel {
            summary: "must not commit",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(DirectoryCompletionToolDispatcher {
            workspace: (*workspace).clone(),
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    instance.start().await.unwrap();
    instance
        .handle()
        .user_message("finish with a directory".into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        instance
            .checkpoint()
            .await
            .unwrap()
            .tasks
            .completed
            .is_empty()
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn focus_switch_clears_previous_tasks_raw_assistant_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(PlainModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
    handle.user_message("task A work".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
    }

    handle.set_focus("task B".into()).await.unwrap();
    handle
        .complete_current_task("task B complete".into())
        .await
        .unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint.tasks.completed.last().unwrap();
    assert_eq!(record.summary, "task B complete");
    assert!(
        record.artifacts.is_empty(),
        "task B must not inherit task A's raw assistant artifact: {:?}",
        record.artifacts
    );
    instance.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// One-shot and terminal-safety proofs for *accepted* completions,
// independent of any long-flow baseline: the retained runs had
// zero completion calls, so these properties need their own deterministic
// evidence through the real actor.
// ---------------------------------------------------------------------------

/// One model decision per script entry.
#[derive(Debug)]
enum CompletionRound {
    /// Call `task.complete` with this summary.
    Complete(&'static str),
    /// A plain final answer with no tool calls.
    Plain(&'static str),
}

/// Plays its script round by round and panics if the runtime asks for a
/// decision the script does not contain — an extra round after an accepted
/// completion would be exactly such a violation.
#[derive(Debug)]
struct ScriptedCompletionModel {
    script: Vec<CompletionRound>,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for ScriptedCompletionModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        let Some(decision) = self.script.get(round) else {
            panic!("the runtime requested model round {round} beyond the script");
        };
        Ok(match decision {
            CompletionRound::Complete(summary) => ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call-{round}"),
                    name: "task.complete".into(),
                    arguments: json!({"summary": summary, "artifacts": []}),
                }],
                usage: Default::default(),
            },
            CompletionRound::Plain(text) => ModelOutput {
                content: (*text).into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            },
        })
    }
}

async fn completion_services(
    dir: &tempfile::TempDir,
    model: Arc<ScriptedCompletionModel>,
) -> RuntimeServices {
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(WithCompletionVerificationModel::new(model)),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher {
                workspace: Some((*workspace).clone()),
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace)
}

/// Two `task.complete` calls in one successful batch must commit exactly
/// one CompletionRecord. The typed proposal slot holds the last accepted
/// proposal of the batch, so "second" wins; whatever the order, one-shot
/// storage is the terminal-safety contract under proof.
#[tokio::test]
async fn duplicate_completions_in_one_batch_commit_exactly_one_record() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedCompletionModel {
        script: vec![
            CompletionRound::Complete("first"),
            CompletionRound::Complete("second"),
        ],
        rounds: AtomicUsize::new(0),
    });
    let services = completion_services(&dir, model.clone()).await;
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
    declare_completion_acceptance(handle, "finish the work").await;
    handle.user_message("finish the work".into()).await.unwrap();

    // The TaskCompleted event lands after TurnCompleted at the safe point,
    // so waiting for it implies the whole commit transaction ran.
    let mut summaries_seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
            Ok(Ok(envelope)) => {
                if let RuntimeEvent::TaskCompleted { summary, .. } = &envelope.event {
                    summaries_seen.push(summary.clone());
                }
            }
            _ => assert!(
                tokio::time::Instant::now() < deadline,
                "the duplicated completion never committed"
            ),
        }
        if !summaries_seen.is_empty() {
            break;
        }
    }
    // Quiesce and prove nothing else rides past acceptance.
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(envelope) = events.try_recv() {
        if let RuntimeEvent::TaskCompleted { summary, .. } = &envelope.event {
            summaries_seen.push(summary.clone());
        }
        assert!(
            !matches!(envelope.event, RuntimeEvent::RecoveryRequired),
            "a duplicated batch must never fence the runtime: {:?}",
            envelope.event
        );
    }
    assert_eq!(
        summaries_seen.len(),
        1,
        "one batch owns at most one committed completion record"
    );
    assert!(
        summaries_seen[0] == "first" || summaries_seen[0] == "second",
        "the committed record must be one of the batch's accepted proposals, got {:?}",
        summaries_seen[0]
    );
    // Which of two concurrently settling proposals wins the single slot is
    // unspecified; what matters for terminal safety is that exactly one
    // durable record exists and the turn still ends without another round.

    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(
        checkpoint.tasks.completed.len(),
        1,
        "exactly one durable CompletionRecord exists"
    );
    assert_eq!(
        model.rounds.load(Ordering::SeqCst),
        1,
        "an accepted completion is already the terminal decision — no next round"
    );
    instance.shutdown().await.unwrap();
}

/// An accepted completion stays terminal for its own turn while a queued
/// user message still drains into a clean follow-up turn: no duplicate
/// record, no error, no recovery fence.
#[tokio::test]
async fn an_accepted_completion_leaves_a_clean_turn_for_queued_input() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedCompletionModel {
        script: vec![
            CompletionRound::Complete("done once"),
            CompletionRound::Plain("next"),
        ],
        rounds: AtomicUsize::new(0),
    });
    let services = completion_services(&dir, model.clone()).await;
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
    declare_completion_acceptance(handle, "finish the work").await;
    handle.user_message("finish the work".into()).await.unwrap();
    // Queued before the first turn finishes: it must drain into a fresh
    // turn after the one-shot completion, not ride along inside it.
    handle
        .user_message("and then continue".into())
        .await
        .unwrap();

    // One continuous collection pass covers the whole run: the first
    // TurnCompleted may land before the TaskCompleted event, so counters
    // must exist before either arrives.
    let mut turn_completed_events = 0usize;
    let mut task_completed_events = 0usize;
    let mut accepted_input_bodies = std::collections::HashSet::new();
    let mut failures = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(envelope)) => match &envelope.event {
                RuntimeEvent::TurnCompleted => turn_completed_events += 1,
                RuntimeEvent::TaskCompleted { .. } => task_completed_events += 1,
                // One input is accounted at queue time and again when it
                // drains, so count distinct bodies.
                RuntimeEvent::UserMessageAccepted { input } => {
                    accepted_input_bodies.insert(input.preview.clone());
                }
                RuntimeEvent::RecoveryRequired | RuntimeEvent::Error { .. } => {
                    failures.push(format!("{:?}", envelope.event))
                }
                _ => {}
            },
            Ok(Err(error)) => {
                // A lagged subscriber silently losing events would fake a
                // terminal-safety violation; surface it instead.
                failures.push(format!("event stream error: {error}"));
            }
            Err(_) => {}
        }
        if task_completed_events >= 1 && turn_completed_events >= 2 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    // Quiesce before counting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    loop {
        match events.try_recv() {
            Ok(envelope) => match &envelope.event {
                RuntimeEvent::TurnCompleted => turn_completed_events += 1,
                RuntimeEvent::TaskCompleted { .. } => task_completed_events += 1,
                RuntimeEvent::UserMessageAccepted { input } => {
                    accepted_input_bodies.insert(input.preview.clone());
                }
                RuntimeEvent::RecoveryRequired | RuntimeEvent::Error { .. } => {
                    failures.push(format!("{:?}", envelope.event))
                }
                _ => {}
            },
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                failures.push(format!("event stream lagged; {skipped} events dropped"));
            }
            Err(_) => break,
        }
    }
    assert_eq!(
        accepted_input_bodies.len(),
        2,
        "both queued messages must be accounted for by the input ledger"
    );
    assert_eq!(
        turn_completed_events, 2,
        "the queued input must drain into a follow-up turn (tasks={task_completed_events}, failures={failures:?})"
    );
    assert_eq!(
        task_completed_events, 1,
        "only the accepted completion commits a record — the plain follow-up turn must not"
    );
    assert!(
        failures.is_empty(),
        "terminal safety means no errors or fences around the edge: {failures:?}"
    );

    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(
        checkpoint.tasks.completed.len(),
        1,
        "the completed-task catalog holds exactly the one accepted record"
    );
    assert_eq!(
        model.rounds.load(Ordering::SeqCst),
        2,
        "one terminal decision per turn, nothing more"
    );
    instance.shutdown().await.unwrap();
}

/// Scripts two off-surface attempts (canonical dotted name, then provider
/// wire spelling), an exact-current verify, then one completion proposal.
/// Refusals stay visible in the next request; the task closes in one pass.
#[derive(Debug)]
struct OffSurfaceAttemptModel {
    rounds: AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ModelTransport for OffSurfaceAttemptModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        let call = match round {
            0 => ToolCall {
                id: "off-surface-dotted".into(),
                name: "fs.write".into(),
                arguments: json!({"path": "src/lib.rs", "body": "fix"}),
            },
            1 => ToolCall {
                id: "off-surface-wire".into(),
                name: "fs_mkdir".into(),
                arguments: json!({"path": "src/generated"}),
            },
            2 => ToolCall {
                id: "exact-verify".into(),
                name: "test.verify".into(),
                arguments: json!({"suite": "all"}),
            },
            _ => ToolCall {
                id: "complete".into(),
                name: "task.complete".into(),
                arguments: json!({"summary": "implemented and verified", "artifacts": []}),
            },
        };
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: vec![call],
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn unloaded_surface_attempts_are_visible_but_never_completion_debt() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = Arc::new(OffSurfaceAttemptModel {
        rounds: AtomicUsize::new(0),
        requests: requests.clone(),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher { workspace: None },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "implement and verify before closing").await;
    handle.user_message("do the work".into()).await.unwrap();

    let mut task_completed = false;
    let mut surface_refusals = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !task_completed {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if matches!(output.tool_name.as_str(), "fs.write" | "fs_mkdir") =>
                {
                    surface_refusals.push(output);
                }
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    assert!(
                        output.ok,
                        "completion must be accepted once: {}",
                        output.model_content
                    );
                }
                RuntimeEvent::TaskCompleted { .. } => task_completed = true,
                _ => {}
            }
        }
    }
    assert!(
        task_completed,
        "an off-surface attempt incident must not block a verified completion"
    );
    assert_eq!(
        surface_refusals.len(),
        2,
        "both the dotted canonical name and the wire spelling must be refused"
    );
    for refused in &surface_refusals {
        assert!(!refused.ok);
        assert_eq!(
            refused.failure_class(),
            Some(agent_contracts::ToolFailureClass::SurfaceUnavailable),
            "off-surface refusals keep the typed surface-unavailable class: {refused:?}"
        );
        assert_eq!(refused.metadata["executed"], false);
    }
    let captured = requests.lock().unwrap().clone();
    assert!(
        captured.get(1).is_some_and(
            |text| text.contains("only schemas in that captured surface may be called")
        ),
        "the first refusal must be visible to the next model decision"
    );
    assert!(
        captured.get(2).is_some_and(
            |text| text.contains("only schemas in that captured surface may be called")
        ),
        "the wire-spelling refusal must also be visible to the next model decision"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed.len(), 1);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn refusal_persists_a_durable_repair_record_with_criterion_details() {
    let (output, requests, checkpoint) = run_model_visible_completion_refusal(true).await;
    assert!(!output.ok);
    assert_eq!(output.metadata["refused"], "completion_gate");

    // The repair stage carries the exact uncovered criterion straight from
    // the readiness decision, not a separately re-derived guess.
    let step = &output.metadata["repair_plan"]["steps"][0];
    assert_eq!(step["kind"], "operator_required");
    let details = step["criterion_details"]
        .as_array()
        .expect("criterion details");
    assert_eq!(details.len(), 1);
    assert_eq!(details[0]["coverage_domain"], COMPLETION_ACCEPTANCE_DOMAIN);
    assert!(
        details[0]["criterion_text"]
            .as_str()
            .is_some_and(|text| text.contains("completion fixture"))
    );

    // The gate refusal left a durable basis-stamped record on the task that
    // survives the turn and subsequent checkpoints, so a deferred safe-point
    // refusal can resume the exact stage instead of re-deriving it.
    let active = checkpoint
        .tasks
        .active
        .expect("task stays active after refusal");
    let record = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.id == active)
        .and_then(|task| task.resume.completion_repair.as_ref())
        .expect("a refused completion must persist its repair stage");
    assert_eq!(record.refusal_count, 1);
    assert!(record.basis_anchor_revision.is_some());
    assert_eq!(record.plan["schema"], "completion-repair.v2");
    assert_eq!(record.plan["steps"][0]["kind"], "operator_required");
    assert_eq!(
        record.plan["steps"][0]["criterion_details"][0]["coverage_domain"],
        COMPLETION_ACCEPTANCE_DOMAIN
    );
    assert!(
        requests
            .get(1)
            .is_some_and(|request| request.contains("cannot prove a current exact recipe_id")),
        "the durable stage must reach the next model decision"
    );
}

/// Proposes completion twice in one turn: a refused gate gets a second
/// explicit attempt against the same basis, so the durable record can prove
/// consecutive-refusal accounting.
#[derive(Debug)]
struct DoubleRefusalModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for DoubleRefusalModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        Ok(if round <= 1 {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("completion-refusal-{round}"),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "optimistic completion", "artifacts": []}),
                }],
                usage: Default::default(),
            }
        } else {
            ModelOutput {
                content: "continuing after the second refusal".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }
        })
    }
}

#[tokio::test]
async fn consecutive_refusals_against_the_same_basis_accrue_durably() {
    let model = Arc::new(DoubleRefusalModel {
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionToolDispatcher { workspace: None },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    declare_completion_acceptance(handle, "finish only with evidence").await;
    handle.user_message("try to finish".into()).await.unwrap();

    let mut refusals = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && refusals.len() < 2 {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    assert!(!output.ok, "the gate must refuse both proposals");
                    refusals.push(output);
                }
                RuntimeEvent::TaskCompleted { .. } => {
                    panic!("a refused completion proposal must not complete the task")
                }
                RuntimeEvent::TurnCompleted if !refusals.is_empty() => {
                    // The second refusal ends the turn; bail out of the
                    // receive loop via the deadline guard instead.
                }
                _ => {}
            }
        }
        if refusals.len() == 2 {
            // Wait for the post-refusal text round to finish the turn.
            break;
        }
    }
    assert_eq!(
        refusals.len(),
        2,
        "both explicit completion proposals must be refused"
    );

    let checkpoint = instance.checkpoint().await.unwrap();
    let active = checkpoint.tasks.active.expect("task stays active");
    let record = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.id == active)
        .and_then(|task| task.resume.completion_repair.as_ref())
        .expect("the second refusal must persist the repair record");
    assert_eq!(
        record.refusal_count, 2,
        "consecutive refusals against the same basis must accrue"
    );
    assert_eq!(record.plan["schema"], "completion-repair.v2");
    instance.shutdown().await.unwrap();
}

/// Repeats completion until Runtime moves the repair episode onto its
/// text-only terminal phase. The model follows the captured schema surface:
/// an empty tool list produces an ordinary final answer.
#[derive(Debug)]
struct TerminalRefusalModel {
    rounds: AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ModelTransport for TerminalRefusalModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let text_only = request.tools.is_empty();
        self.requests.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        Ok(if text_only {
            ModelOutput {
                content: "completion remains operator-owned; ending this turn".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }
        } else if matches!(round, 0 | 2 | 4) {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("completion-refusal-{round}"),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "optimistic completion", "artifacts": []}),
                }],
                usage: Default::default(),
            }
        } else if matches!(round, 1 | 3) {
            let base_anchor_revision = if round == 1 { 0 } else { 1 };
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("progress-churn-{round}"),
                    name: "task.manage".into(),
                    arguments: json!({
                        "base_anchor_revision": base_anchor_revision,
                        "current_interpretation": format!("same unfinished authority, wording {round}"),
                    }),
                }],
                usage: Default::default(),
            }
        } else {
            ModelOutput {
                content: "unexpected non-terminal continuation".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }
        })
    }
}

#[tokio::test]
async fn operator_only_refusals_escalate_to_a_terminal_surface() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = Arc::new(TerminalRefusalModel {
        rounds: AtomicUsize::new(0),
        requests: requests.clone(),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionProgressDispatcher {
                inner: CompletionToolDispatcher { workspace: None },
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    // No acceptance declaration: the task stays OperatorClosureOnly with no
    // declared criteria, the exact shape of the diag fixture that previously
    // looped on repeated task.complete refusals.
    handle
        .set_focus("operator-owned completion".into())
        .await
        .unwrap();
    handle.user_message("try to finish".into()).await.unwrap();

    let mut refusals = Vec::new();
    let mut assistant_final = None;
    let mut turn_completed = false;
    let mut task_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !turn_completed {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    assert!(!output.ok, "the gate must refuse every proposal");
                    refusals.push(output);
                }
                RuntimeEvent::TaskCompleted { .. } => {
                    task_completed = true;
                }
                RuntimeEvent::AssistantMessage { content } => assistant_final = Some(content),
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
    }
    assert_eq!(
        refusals.len(),
        3,
        "the third unchanged refusal must enter finalization before another tool call"
    );

    // The first two refusals stay on the ordinary operator-only stage.
    for output in &refusals[..2] {
        let step = &output.metadata["repair_plan"]["steps"][0];
        assert_eq!(step["kind"], "operator_required");
        assert_ne!(
            step["terminal"], true,
            "the first refusals are not terminal"
        );
    }
    // The third refusal closes the semantic repair episode. The following
    // model request receives no tools and therefore cannot repeat the call.
    let step = &refusals[2].metadata["repair_plan"]["steps"][0];
    assert_eq!(step["kind"], "repair_stalled");
    assert_eq!(step["terminal"], true);
    assert_eq!(step["terminal_surface"], "ordinary_final");
    assert!(turn_completed);
    assert!(!task_completed, "ordinary final must not complete the task");
    assert_eq!(
        assistant_final.as_deref(),
        Some("completion remains operator-owned; ending this turn")
    );

    // The durable record persists the terminal episode while the task remains
    // active for an operator or a later user directive.
    let checkpoint = instance.checkpoint().await.unwrap();
    let active = checkpoint.tasks.active.expect("task stays active");
    let record = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|task| task.id == active)
        .and_then(|task| task.resume.completion_repair.as_ref())
        .expect("a refused completion must persist its repair stage");
    assert_eq!(
        record.refusal_count, 3,
        "the terminal refusal must accrue on the semantic episode"
    );
    assert!(record.terminal);
    assert_eq!(record.plan["steps"][0]["terminal"], true);
    assert_eq!(
        record.plan["steps"][0]["terminal_surface"],
        "ordinary_final"
    );

    // Deferred safe-point refusal visibility: the escalated stage reaches the
    // next model decision in TASK PROGRESS, not only the tool metadata.
    let captured = requests.lock().unwrap().clone();
    assert!(
        captured
            .iter()
            .any(|text| text.contains("repair_stalled/terminal")),
        "the terminal stage must be visible to a following model decision"
    );
    assert_eq!(checkpoint.tasks.completed.len(), 0);
    assert_eq!(checkpoint.tasks.active, Some(active));
    instance.shutdown().await.unwrap();
}

/// After one refused completion, repeatedly edits only advisory progress text
/// and never proposes completion again. Runtime must still observe that the
/// typed blocker frontier is unchanged and close the repair action surface.
#[derive(Debug)]
struct ToolOnlyRepairLoopModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for ToolOnlyRepairLoopModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if request.tools.is_empty() {
            return Ok(ModelOutput {
                content: "repair stalled; operator authority is still required".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            });
        }
        if round == 0 {
            return Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "initial-completion".into(),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "premature", "artifacts": []}),
                }],
                usage: Default::default(),
            });
        }
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("cosmetic-progress-{round}"),
                name: "task.manage".into(),
                arguments: json!({
                    "base_anchor_revision": round - 1,
                    "current_interpretation": format!("same operator-owned task, wording {round}"),
                }),
            }],
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn repair_actions_without_typed_progress_cannot_loop_forever() {
    let model = Arc::new(ToolOnlyRepairLoopModel {
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(WithCompletionVerificationTools {
            inner: CompletionProgressDispatcher {
                inner: CompletionToolDispatcher { workspace: None },
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("operator-owned completion".into())
        .await
        .unwrap();
    handle.user_message("try to finish".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut turn_completed = false;
    let mut task_completed = false;
    let mut progress_calls = 0usize;
    while tokio::time::Instant::now() < deadline && !turn_completed {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. } if output.tool_name == "task.manage" => {
                    progress_calls += 1;
                }
                RuntimeEvent::TaskCompleted { .. } => task_completed = true,
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
    }

    assert!(
        turn_completed,
        "semantic repair must end before the round cap"
    );
    assert!(!task_completed);
    assert_eq!(
        progress_calls, 5,
        "five no-progress actions reach the six-step episode bound after the initial refusal"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = checkpoint
        .tasks
        .active
        .and_then(|task_id| {
            checkpoint
                .tasks
                .tasks
                .iter()
                .find(|task| task.id == task_id)
        })
        .and_then(|task| task.resume.completion_repair.as_ref())
        .expect("the terminal semantic episode remains durable");
    assert_eq!(record.refusal_count, 1);
    assert_eq!(record.no_progress_steps, 6);
    assert!(record.terminal);
    assert!(checkpoint.tasks.completed.is_empty());
    instance.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Runtime-owned proof-refresh transaction: a task whose only remaining
// completion blockers are proof-shaped (`VerificationNotCurrent` /
// `AcceptanceUncovered`) may run the host-declared exact verifier once
// before accepting. A PASS lands only when the verifier identity matches
// the trusted host attribution for the same recipe; every failure keeps
// the ordinary refusal.
// ---------------------------------------------------------------------------

/// Serves `verify.run` under the exact name the proof route resolves,
/// plus `task.complete`, over the completion-fixture coverage domain.
#[derive(Debug)]
struct ProofRefreshDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for ProofRefreshDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![ToolSpec {
            name: "verify.run".into(),
            description: "trusted completion fixture verifier".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: Vec::new(),
        }];
        specs.push(ToolSpec {
            name: "task.complete".into(),
            description: "propose completion".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        });
        specs
    }

    fn execution_attribution(&self, call: &ToolCall) -> ToolExecutionAttribution {
        if call.name == "verify.run" {
            return ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                Vec::<String>::new(),
                VerificationReuse::ExactCurrentWorld,
            )
            .with_verification_identity_material(COMPLETION_VERIFY_IDENTITY_MATERIAL)
            .with_verification_recipe(agent_contracts::VerificationRecipeProvenance {
                recipe_id: "completion-fixture-recipe".into(),
                recipe_revision: "v1".into(),
                coverage_domain: Some(COMPLETION_ACCEPTANCE_DOMAIN.into()),
                domain_declaration_revision: Some(1),
                domain_source_digest: completion_acceptance_declaration().source_digest,
                class_identity_digest: "completion-fixture-class".into(),
            });
        }
        ToolExecutionAttribution::default()
    }

    fn verification_coverage_declarations(
        &self,
    ) -> Vec<agent_contracts::VerificationCoverageDeclaration> {
        vec![completion_acceptance_declaration()]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if request.call.name == "verify.run" {
            return Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "completion fixture verified".into(),
                model_content: "completion fixture verified".into(),
                artifact_ref: None,
                metadata: json!({"verification": true}),
            }));
        }
        let summary: String = request.call.arguments["summary"]
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
            directive: agent_contracts::RuntimeDirective::CompleteTask(
                agent_contracts::CompletionProposal {
                    summary,
                    artifacts: Vec::new(),
                },
            ),
        })
    }
}

/// The host's exact verifier identity for the completion-fixture material.
fn completion_verifier_identity() -> String {
    agent_contracts::ContentDigest::sha256_bytes(
        COMPLETION_VERIFY_IDENTITY_MATERIAL.trim().as_bytes(),
    )
    .to_string()
}

/// Scripted host verifier: records each request and yields queued outcomes.
/// `None` outcomes count as errors, so refusal paths can assert zero calls
/// without fabricating results.
#[derive(Debug, Clone)]
struct ScriptedProofVerifier {
    outcomes: Arc<tokio::sync::Mutex<VecDeque<AgentResult<agent_runtime::ProofVerifierOutcome>>>>,
    calls: Arc<std::sync::Mutex<Vec<agent_runtime::ProofVerifierRequest>>>,
}

impl ScriptedProofVerifier {
    fn new(outcomes: Vec<AgentResult<agent_runtime::ProofVerifierOutcome>>) -> Self {
        Self {
            outcomes: Arc::new(tokio::sync::Mutex::new(outcomes.into())),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<agent_runtime::ProofVerifierRequest> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl agent_runtime::ProofVerifier for ScriptedProofVerifier {
    fn exact_recipe_for_domain(
        &self,
        declaration: &agent_contracts::VerificationCoverageDeclaration,
    ) -> Option<String> {
        (declaration == &completion_acceptance_declaration())
            .then(|| "completion-fixture-recipe".into())
    }

    async fn verify_exact(
        &self,
        request: agent_runtime::ProofVerifierRequest,
    ) -> AgentResult<agent_runtime::ProofVerifierOutcome> {
        self.calls.lock().unwrap().push(request.clone());
        self.outcomes
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| Err(AgentError::Tool("no scripted proof outcome queued".into())))
    }
}

/// Round 0 verifies, round 2 proposes completion; odd rounds end the turn.
/// The first user turn ends after the verification, a second user turn
/// moves the directive (stale PASS/receipt) and proposes completion.
#[derive(Debug)]
struct ProofRefreshModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for ProofRefreshModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        Ok(if round == 0 {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "proof-verify".into(),
                    name: "verify.run".into(),
                    arguments: json!({"recipe_id": "completion-fixture-recipe"}),
                }],
                usage: Default::default(),
            }
        } else if round == 2 {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "proof-complete".into(),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "the fixture is verified", "artifacts": []}),
                }],
                usage: Default::default(),
            }
        } else {
            ModelOutput {
                content: "round done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }
        })
    }
}

/// Runs the proof-refresh scenario: first a user turn verifies (PASS
/// recorded against directive revision N), then a second user turn moves
/// the directive and proposes completion. Returns the authoritative
/// `task.complete` output, the verifier's recorded requests and the
/// checkpoint once the second turn settles.
async fn run_proof_refresh_scenario(
    verifier: Arc<ScriptedProofVerifier>,
    open_loop: bool,
) -> (
    ToolOutput,
    Vec<agent_runtime::ProofVerifierRequest>,
    agent_runtime::checkpoint::RuntimeCheckpoint,
) {
    let model = Arc::new(ProofRefreshModel {
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(ProofRefreshDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    )
    .with_proof_verifier(verifier.clone())
    .with_project_proof_refresh(true);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("finish only with evidence".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let patch = agent_runtime::AnchorPatch {
        completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
        acceptance_criteria: Some(vec![agent_runtime::task::AcceptanceCriterion::declared(
            "trusted completion fixture passes",
            &completion_acceptance_declaration(),
        )]),
        open_loops: open_loop.then(|| vec!["verify one more edge case".into()]),
        ..agent_runtime::AnchorPatch::default()
    };
    handle.patch_task_anchor(task_id, 0, patch).await.unwrap();

    // Turn 1: the model verifies the fixture (PASS observed + receipt).
    handle
        .user_message("verify the fixture".into())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            break;
        }
    }

    // Turn 2: the world moved (directive revision), the stale proof is the
    // only remaining blocker, and the model proposes completion.
    handle
        .user_message("now finish the task".into())
        .await
        .unwrap();
    let mut completion_output = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    completion_output = Some(output);
                }
                RuntimeEvent::TurnCompleted if completion_output.is_some() => break,
                _ => {}
            }
        }
    }
    let output = completion_output.expect("task.complete must publish its authoritative result");
    let checkpoint = instance.checkpoint().await.unwrap();
    instance.shutdown().await.unwrap();
    (output, verifier.requests(), checkpoint)
}

#[tokio::test]
async fn proof_refresh_resolves_the_exact_recipe_without_model_history() {
    let verifier = Arc::new(ScriptedProofVerifier::new(vec![Ok(
        agent_runtime::ProofVerifierOutcome {
            ok: true,
            summary: "fixture verified from the host declaration".into(),
            verification_identity: completion_verifier_identity(),
        },
    )]));
    let model = Arc::new(CompletionProposalModel {
        summary: "cold proof route completed",
        rounds: AtomicUsize::new(0),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(ProofRefreshDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    )
    .with_proof_verifier(verifier.clone())
    .with_project_proof_refresh(true);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("finish from a declared proof domain".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                completion_policy: Some(
                    agent_runtime::task::TaskCompletionPolicy::EvidenceRequired,
                ),
                acceptance_criteria: Some(vec![
                    agent_runtime::task::AcceptanceCriterion::declared(
                        "trusted completion fixture passes",
                        &completion_acceptance_declaration(),
                    ),
                ]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();

    handle
        .user_message("finish without a prior verify.run call".into())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut completed = false;
    while tokio::time::Instant::now() < deadline && !completed {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TaskCompleted { .. })
        {
            completed = true;
        }
    }

    assert!(
        completed,
        "the host-declared cold route must admit completion"
    );
    assert_eq!(verifier.call_count(), 1);
    assert_eq!(
        verifier.requests()[0].recipe_id,
        "completion-fixture-recipe"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(checkpoint.tasks.active.is_none());
    assert_eq!(checkpoint.tasks.completed.len(), 1);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_proof_repair_names_the_exact_recipe_when_auto_refresh_is_off() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let verifier = Arc::new(ScriptedProofVerifier::new(Vec::new()));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(CompletionRefusalModel {
            rounds: AtomicUsize::new(0),
            requests: requests.clone(),
        }),
        Arc::new(ProofRefreshDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    )
    .with_proof_verifier(verifier.clone());
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.set_focus("cold proof repair".into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                completion_policy: Some(
                    agent_runtime::task::TaskCompletionPolicy::EvidenceRequired,
                ),
                acceptance_criteria: Some(vec![
                    agent_runtime::task::AcceptanceCriterion::declared(
                        "trusted completion fixture passes",
                        &completion_acceptance_declaration(),
                    ),
                ]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    handle.user_message("finish".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut refused = None;
    let mut turn_completed = false;
    while tokio::time::Instant::now() < deadline && !turn_completed {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    refused = Some(output);
                }
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
    }
    let refused = refused.expect("completion refusal must remain model-visible");
    assert_eq!(
        refused.metadata["repair_plan"]["steps"][0]["kind"],
        "proof_refresh"
    );
    assert_eq!(
        refused.metadata["repair_plan"]["steps"][0]["recipe_id"],
        "completion-fixture-recipe"
    );
    assert_eq!(verifier.call_count(), 0, "automatic execution is still off");
    assert!(requests.lock().unwrap().iter().any(|request| {
        request.contains("proof_refresh") && request.contains("recipe_id=completion-fixture-recipe")
    }));
    instance.shutdown().await.unwrap();
}

#[derive(Debug)]
struct RepeatedCompletionAfterProofFailureModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for RepeatedCompletionAfterProofFailureModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        Ok(if request.tools.is_empty() {
            ModelOutput {
                content: "the exact proof remains failed".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }
        } else {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("repeat-completion-{round}"),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "still premature", "artifacts": []}),
                }],
                usage: Default::default(),
            }
        })
    }
}

#[tokio::test]
async fn failed_host_proof_is_not_reexecuted_on_the_same_basis() {
    let verifier = Arc::new(ScriptedProofVerifier::new(vec![Ok(
        agent_runtime::ProofVerifierOutcome {
            ok: false,
            summary: "fixture fails".into(),
            verification_identity: String::new(),
        },
    )]));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(RepeatedCompletionAfterProofFailureModel {
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(ProofRefreshDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    )
    .with_proof_verifier(verifier.clone())
    .with_project_proof_refresh(true);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.set_focus("proof must pass".into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                completion_policy: Some(
                    agent_runtime::task::TaskCompletionPolicy::EvidenceRequired,
                ),
                acceptance_criteria: Some(vec![
                    agent_runtime::task::AcceptanceCriterion::declared(
                        "trusted completion fixture passes",
                        &completion_acceptance_declaration(),
                    ),
                ]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    handle.user_message("finish".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut turn_completed = false;
    while tokio::time::Instant::now() < deadline && !turn_completed {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            turn_completed = true;
        }
    }
    assert!(turn_completed);
    assert_eq!(
        verifier.call_count(),
        1,
        "the current-basis failed proof is a negative lease, not a retry invitation"
    );
    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(checkpoint.tasks.completed.is_empty());
    assert!(
        checkpoint
            .tasks
            .active
            .and_then(|active| checkpoint.tasks.tasks.iter().find(|task| task.id == active))
            .and_then(|task| task.resume.completion_repair.as_ref())
            .is_some_and(|record| record.terminal)
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn proof_refresh_passes_only_proof_blocker_and_commits_completion() {
    let verifier = Arc::new(ScriptedProofVerifier::new(vec![Ok(
        agent_runtime::ProofVerifierOutcome {
            ok: true,
            summary: "fixture re-verified at the new directive".into(),
            verification_identity: completion_verifier_identity(),
        },
    )]));
    let (output, requests, checkpoint) = run_proof_refresh_scenario(verifier.clone(), false).await;

    assert!(
        output.ok,
        "the refreshed proof must admit the completion; got {output:?}"
    );
    assert_eq!(
        output.metadata["completion_state"],
        "pending_terminal_commit"
    );
    assert_eq!(
        verifier.call_count(),
        1,
        "the transaction must run the host verifier exactly once"
    );
    let request = requests.into_iter().next().expect("one recorded request");
    assert_eq!(request.recipe_id, "completion-fixture-recipe");
    assert!(
        checkpoint.tasks.completed.iter().any(|record| {
            record.summary == "the fixture is verified" && record.anchor_revision >= 1
        }),
        "the refreshed completion must commit a durable CompletionRecord"
    );
}

#[tokio::test]
async fn proof_refresh_failure_keeps_the_ordinary_refusal() {
    let verifier = Arc::new(ScriptedProofVerifier::new(vec![Ok(
        agent_runtime::ProofVerifierOutcome {
            ok: false,
            summary: "the fixture still fails at the new directive".into(),
            verification_identity: String::new(),
        },
    )]));
    let (output, requests, checkpoint) = run_proof_refresh_scenario(verifier.clone(), false).await;

    assert!(
        !output.ok,
        "a failed verifier must keep the ordinary refusal"
    );
    assert_eq!(output.metadata["refused"], "completion_gate");
    assert_eq!(
        verifier.call_count(),
        1,
        "the transaction still ran the verifier; the refusal comes from its outcome"
    );
    assert_eq!(requests.len(), 1);
    assert!(checkpoint.tasks.completed.is_empty());
}

#[tokio::test]
async fn proof_refresh_identity_mismatch_fails_closed() {
    let verifier = Arc::new(ScriptedProofVerifier::new(vec![Ok(
        agent_runtime::ProofVerifierOutcome {
            ok: true,
            summary: "the verifier claims success".into(),
            verification_identity: "a-different-host-identity".into(),
        },
    )]));
    let (output, _requests, checkpoint) = run_proof_refresh_scenario(verifier.clone(), false).await;

    assert!(
        !output.ok,
        "an identity mismatch must fail closed instead of trusting the PASS"
    );
    assert_eq!(output.metadata["refused"], "completion_gate");
    assert_eq!(
        verifier.call_count(),
        1,
        "the verifier ran, but its identity did not match the host attribution"
    );
    assert!(checkpoint.tasks.completed.is_empty());
}

#[tokio::test]
async fn proof_refresh_verifier_error_keeps_the_ordinary_refusal() {
    let verifier = Arc::new(ScriptedProofVerifier::new(vec![Err(AgentError::Tool(
        "scripted verifier failure".into(),
    ))]));
    let (output, requests, checkpoint) = run_proof_refresh_scenario(verifier.clone(), false).await;

    assert!(!output.ok, "a verifier error must not manufacture a PASS");
    assert_eq!(output.metadata["refused"], "completion_gate");
    assert_eq!(verifier.call_count(), 1);
    assert_eq!(requests.len(), 1);
    assert!(checkpoint.tasks.completed.is_empty());
}

#[tokio::test]
async fn proof_refresh_is_skipped_when_an_open_loop_blocks_completion() {
    let verifier = Arc::new(ScriptedProofVerifier::new(vec![Ok(
        agent_runtime::ProofVerifierOutcome {
            ok: true,
            summary: "must never be produced".into(),
            verification_identity: completion_verifier_identity(),
        },
    )]));
    let (output, _requests, checkpoint) = run_proof_refresh_scenario(verifier.clone(), true).await;

    assert!(
        !output.ok,
        "an open loop is not a proof-shaped blocker; the gate must refuse ordinarily"
    );
    assert_eq!(output.metadata["refused"], "completion_gate");
    assert!(
        output.metadata["blockers"]
            .to_string()
            .contains("open_loops")
    );
    assert_eq!(
        verifier.call_count(),
        0,
        "the transaction must be ineligible while any non-proof blocker remains"
    );
    assert!(checkpoint.tasks.completed.is_empty());
}

/// Same rounds as [`ProofRefreshModel`] (round 0 verifies, round 2 proposes
/// completion, odd rounds end the turn) but every assembled request text is
/// recorded so the projected repair stage can be asserted on the next
/// model decision.
#[derive(Debug)]
struct ProofRefreshRecordingModel {
    rounds: AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ModelTransport for ProofRefreshRecordingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        Ok(if round == 0 {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "proof-verify".into(),
                    name: "verify.run".into(),
                    arguments: json!({"recipe_id": "completion-fixture-recipe"}),
                }],
                usage: Default::default(),
            }
        } else if round == 2 {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "proof-complete".into(),
                    name: "task.complete".into(),
                    arguments: json!({"summary": "the fixture is verified", "artifacts": []}),
                }],
                usage: Default::default(),
            }
        } else {
            ModelOutput {
                content: "round done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }
        })
    }
}

/// Without the runtime-owned proof-refresh transaction, a stale
/// verification refusal must still name the exact final action: refresh the
/// trusted proof with `verify.run` after every workspace call and stop
/// mutating before re-proposing. This is the model-visible repair signal the
/// long-task failure diagnosis needs — a refusal that only says "not
/// current" leaves the model looping.
#[tokio::test]
async fn stale_proof_refusal_projects_a_final_verify_as_the_repair_action() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = Arc::new(ProofRefreshRecordingModel {
        rounds: AtomicUsize::new(0),
        requests: requests.clone(),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(ProofRefreshDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    );
    // Deliberately no `.with_project_proof_refresh(true)`: the shared
    // verification path must refuse with the repair stage, not transparently
    // re-verify.
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("finish only with evidence".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let patch = agent_runtime::AnchorPatch {
        completion_policy: Some(agent_runtime::task::TaskCompletionPolicy::EvidenceRequired),
        acceptance_criteria: Some(vec![agent_runtime::task::AcceptanceCriterion::declared(
            "trusted completion fixture passes",
            &completion_acceptance_declaration(),
        )]),
        ..agent_runtime::AnchorPatch::default()
    };
    handle.patch_task_anchor(task_id, 0, patch).await.unwrap();

    // Turn 1: the model verifies the fixture (PASS observed + receipt).
    handle
        .user_message("verify the fixture".into())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            && matches!(envelope.event, RuntimeEvent::TurnCompleted)
        {
            break;
        }
    }

    // Turn 2: the world moved (directive revision), the stale proof is the
    // only remaining blocker, and the model proposes completion.
    handle
        .user_message("now finish the task".into())
        .await
        .unwrap();
    let mut completion_output = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.tool_name == "task.complete" =>
                {
                    completion_output = Some(output);
                }
                RuntimeEvent::TaskCompleted { .. } => {
                    panic!("a stale proof must keep completion refused")
                }
                RuntimeEvent::TurnCompleted if completion_output.is_some() => break,
                _ => {}
            }
        }
    }
    let output = completion_output.expect("task.complete must publish its authoritative result");
    assert!(
        !output.ok,
        "the stale proof must refuse completion: {output:?}"
    );
    assert_eq!(output.metadata["refused"], "completion_gate");

    let step = &output.metadata["repair_plan"]["steps"][0];
    assert_eq!(step["kind"], "proof_refresh");
    assert_eq!(step["tool"], "verify.run");
    assert_eq!(step["recipe_id"], "completion-fixture-recipe");
    assert_eq!(
        step["must_be_after_workspace_calls"], true,
        "the repair must order the final verification after every workspace call"
    );

    // The projected stage must reach the next model decision so the loop
    // can act on it, not consume the refusal in silence.
    let captured = requests.lock().unwrap().clone();
    assert!(
        captured.iter().enumerate().any(|(index, request)| {
            index >= 1
                && request.contains("proof_refresh")
                && request.contains("run verify.run with recipe_id=completion-fixture-recipe")
                && request.contains("do not run another workspace-changing command afterward")
        }),
        "the next model decision must receive the projected proof_refresh stage: {captured:?}"
    );
    instance.shutdown().await.unwrap();
}
