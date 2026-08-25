use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentResult, ModelCapabilities, ModelChunk, ModelEventSink, ModelOutput, ModelRequest,
    ModelTransport, RunId, RuntimeEvent, RuntimeEventEnvelope, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};

use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;

use crate::harness::*;

// ---------------------------------------------------------------------------
// Structured completion: `task.complete` attaches a typed proposal that the
// runtime commits at the turn's safe point (after the turn commits) as the
// active task's CompletionRecord — the CTX-10 transaction.
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
        model.clone(),
        Arc::new(CompletionToolDispatcher {
            workspace: Some((*workspace).clone()),
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("finish the work".into()).await.unwrap();

    let mut completed_event = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
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

    // The typed record is durable in the checkpoint, with the proposal's
    // artifact ref attached — the CTX-10 transaction end to end.
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
        model.clone(),
        Arc::new(CompletionWithFailureDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("finish carefully".into())
        .await
        .unwrap();

    let mut failed_batch_seen = false;
    let mut assistant_content = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::ExecutionBatchSettled { failed, .. } => {
                    failed_batch_seen |= failed == 1;
                }
                RuntimeEvent::AssistantMessage { content } => assistant_content = Some(content),
                RuntimeEvent::TaskCompleted { .. } => break,
                _ => {}
            }
        }
    }

    assert!(failed_batch_seen, "the failed sibling must be audited");
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
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
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
        Arc::new(CompletingLongModel {
            rounds: AtomicUsize::new(0),
            content_len,
        }),
        Arc::new(CompletionToolDispatcher { workspace: None }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace.clone());
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    handle.start().await.unwrap();
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
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
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
        Arc::new(CompletionProposalModel {
            summary: "ignored by dispatcher",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(BulkCompletionToolDispatcher {
            workspace: (*workspace).clone(),
            unique_artifacts: agent_contracts::MAX_COMPLETION_ARTIFACTS,
            duplicate_first: false,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let mut events = instance.handle().subscribe();
    instance.start().await.unwrap();
    instance
        .handle()
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
        Arc::new(CompletionProposalModel {
            summary: "ignored by dispatcher",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(BulkCompletionToolDispatcher {
            workspace: (*workspace).clone(),
            unique_artifacts: 1,
            duplicate_first: true,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace);
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let mut events = instance.handle().subscribe();
    instance.start().await.unwrap();
    instance
        .handle()
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
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
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
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
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
// independent of any long-flow baseline (CONV-04): the retained runs had
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
        model,
        Arc::new(CompletionToolDispatcher {
            workspace: Some((*workspace).clone()),
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
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
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
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    instance.start().await.unwrap();
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
