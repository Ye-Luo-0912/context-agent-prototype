use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, RuntimeEvent, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolRisk,
    ToolSpec, ToolSurfaceSnapshot,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};

use crate::harness::*;

/// CORE-04 end to end: the composition-root output broker runs inside the
/// kernel, spills an oversized tool result to the run's artifact directory
/// and the model-facing preview stays bounded — the truncated middle is no
/// longer lost for a producer that did not spill.
#[tokio::test]
async fn output_broker_spills_oversized_tool_output_end_to_end() {
    use agent_contracts::{CancellationToken, MAX_TOOL_MODEL_CONTENT_CHARS, ToolCall, ToolOutput};
    use agent_workspace::{Workspace, WorkspaceOutputBroker};

    struct BigOutputDispatcher {
        output: ToolOutput,
    }
    #[async_trait::async_trait]
    impl ToolDispatcher for BigOutputDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "big.tool".into(),
                description: "oversized".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            }]
        }
        async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            Ok(ToolOutcome::Value(self.output.clone()))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(dir.path()).await.unwrap());
    let full_content = format!("BEGIN{}\nEND", "payload".repeat(10_000));
    let surface = ToolSurfaceSnapshot {
        specs: vec![ToolSpec {
            name: "big.tool".into(),
            description: "oversized".into(),
            input_schema: serde_json::json!({}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }],
        ..ToolSurfaceSnapshot::default()
    };
    let core = agent_core::build_core_port(
        CoreAuthorityConfig {
            output_broker: Some(Arc::new(WorkspaceOutputBroker::new(workspace.clone()))),
            ..CoreAuthorityConfig::default()
        },
        Arc::new(TestContextEngine),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "big.tool".into(),
                ok: true,
                summary: "done".into(),
                model_content: full_content.clone(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let run_id = core.run_id();
    let generation = core.current_authority_epoch();
    let tool_call = ToolCall {
        id: "c1".into(),
        name: "big.tool".into(),
        arguments: serde_json::json!({}),
    };
    let identity = agent_contracts::ToolOperationIdentity {
        run_id,
        task_id: None,
        turn_id: agent_contracts::TurnId::new(),
        scope_id: None,
        operation_id: agent_contracts::OperationId::new(),
        generation,
        call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        argument_digest: agent_contracts::ArgumentDigest::from_json(&tool_call.arguments),
    };
    let agent_core::ToolOperationAdmission::Accepted { permit, .. } = core
        .admit_tool_operation(identity, &tool_call, generation)
        .expect("test operation admission must succeed")
    else {
        panic!("fresh test operation must receive a dispatch permit")
    };
    let permit = core
        .publish_tool_operation(permit, &tool_call)
        .await
        .unwrap();
    let execution = core
        .execute_published_tool(permit, tool_call, CancellationToken::new(), &surface)
        .await;
    let agent_core::CoreToolExecution { outcome, lease, .. } = execution;
    assert!(
        lease.is_none(),
        "a read-only call carries no commit-time lease"
    );
    let ToolOutcome::Value(output) = outcome else {
        panic!("expected a plain value outcome");
    };
    assert!(
        output.model_content.chars().count() <= MAX_TOOL_MODEL_CONTENT_CHARS,
        "the model-facing preview must stay bounded"
    );
    assert!(output.model_content.contains("output broker truncated"));
    let reference = output.artifact_ref.expect("oversized output must spill");
    assert!(reference.starts_with("artifact://v1/"));
    let locator = agent_contracts::ArtifactLocator::parse(&reference).expect("sealed locator");
    assert_eq!(locator.owner(), "tool-output");
    assert!(locator.is_sealed());

    // The full content was stored once under the run's artifact directory.
    let path = workspace
        .state_dir()
        .join("artifacts")
        .join(run_id.to_string())
        .join(locator.owner())
        .join(locator.digest().unwrap().to_string());
    let stored = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(stored, full_content);
}

#[tokio::test]
async fn user_message_event_is_a_bounded_preview_while_ingest_keeps_the_body() {
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = kernel_with(Arc::new(StreamingModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    let body = "unique-user-input-".to_string() + &"x".repeat(400);
    handle.user_message(body.clone()).await.unwrap();

    let mut accepted = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                accepted = Some(input);
            }
        }
        if accepted.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let input = accepted.expect("UserMessageAccepted");
    assert_eq!(
        input.preview.chars().count(),
        agent_contracts::USER_INPUT_PREVIEW_CHARS
    );
    assert!(
        !input.preview.contains(&body),
        "the journal must not carry the full body"
    );
    assert_eq!(input.bytes, body.len() as u64);
    assert_eq!(input.kind, agent_contracts::InputKind::Dialogue);
    assert_eq!(input.lifecycle, agent_contracts::InputLifecycle::Applied);
    assert_eq!(input.source, agent_contracts::InputSource::User);
    assert_eq!(
        input.authority,
        agent_contracts::InputAuthority::UserSteering
    );
    assert!(input.proposal.is_none());
    assert!(
        input.body_ref.is_none(),
        "this kernel has no artifact workspace"
    );
    {
        let ingested = context.user_messages.lock().unwrap();
        assert_eq!(ingested.as_slice(), std::slice::from_ref(&body));
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn focus_and_cancel_commands_do_not_go_through_the_user_message_envelope() {
    let (handle, _task) = start(Arc::new(StreamingModel)).await;
    let mut events = handle.subscribe();
    handle
        .set_focus("keep the auth service".into())
        .await
        .unwrap();
    handle.cancel_turn().await.unwrap();

    let mut saw_user_message = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::UserMessageAccepted { .. }) {
                saw_user_message = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !saw_user_message,
        "/focus and /cancel must stay direct RuntimeCommand paths"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn user_message_stores_the_exact_body_once_when_a_workspace_is_wired() {
    let dir = tempfile::tempdir().unwrap();
    let workspace =
        std::sync::Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let context = Arc::new(RecordingContextEngine::default());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(StreamingModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(workspace.clone());
    let (handle, _task) = spawn_runtime(std::sync::Arc::new(services));
    handle.start().await.unwrap();
    let mut events = handle.subscribe();
    let body = "exact user body for evidence plane";
    handle.user_message(body.into()).await.unwrap();

    let mut accepted = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                accepted = Some(input);
            }
        }
        if accepted.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let input = accepted.expect("UserMessageAccepted");
    let reference = input.body_ref.expect("workspace must seal a body_ref");
    let locator = agent_contracts::ArtifactLocator::parse(&reference).expect("sealed locator");
    assert_eq!(locator.owner(), "user-input");
    assert_eq!(
        input.digest.as_deref(),
        locator.digest().map(|d| d.to_string()).as_deref()
    );
    let path = workspace
        .state_dir()
        .join("artifacts")
        .join(locator.run_id().to_string())
        .join(locator.owner())
        .join(locator.digest().unwrap().to_string());
    let stored = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(stored, body);
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn busy_user_message_is_recorded_as_rejected_and_not_ingested() {
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = kernel_with(Arc::new(HangingModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    let turn = handle.clone();
    let turn_task = tokio::spawn(async move { turn.user_message("first".into()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    handle.user_message("second".into()).await.unwrap();
    let overflow = handle.user_message("third".into()).await;
    assert!(
        overflow.unwrap_err().to_string().contains("queued"),
        "overflow UserMessage still fail-closes"
    );

    let mut rejected = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event
                && input.lifecycle == agent_contracts::InputLifecycle::Rejected
            {
                rejected = Some(input);
            }
        }
        if rejected.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let input = rejected.expect("overflow UserMessage must leave a Rejected record");
    assert_eq!(input.preview, "third");
    assert!(
        input.turn_id.is_none(),
        "rejected input never started a turn"
    );
    assert!(input.body_ref.is_none(), "rejected input is not sealed");
    {
        let ingested = context.user_messages.lock().unwrap();
        assert_eq!(
            ingested.as_slice(),
            &["first".to_string()],
            "rejected body must not enter context"
        );
    }

    handle.cancel_turn().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), turn_task).await;
    handle.cancel_turn().await.unwrap();
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn queued_user_message_applies_after_the_busy_turn_is_cancelled() {
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = kernel_with(Arc::new(HangingModel), context.clone());
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    let turn = handle.clone();
    let turn_task = tokio::spawn(async move { turn.user_message("first".into()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.user_message("second".into()).await.unwrap();

    let mut queued = None;
    let mut applied_first = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                if input.lifecycle == agent_contracts::InputLifecycle::Applied
                    && input.preview == "first"
                {
                    applied_first = Some(input);
                } else if input.lifecycle == agent_contracts::InputLifecycle::Queued {
                    queued = Some(input);
                }
            }
        }
        if queued.is_some() && applied_first.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued = queued.expect("second UserMessage must be Queued");
    let applied_first = applied_first.expect("first UserMessage must be Applied");
    assert_eq!(queued.preview, "second");
    assert_eq!(queued.causal_parent, applied_first.input_id);
    assert!(
        context.user_messages.lock().unwrap().as_slice() == ["first".to_string()],
        "queued body must not ingest until the busy turn ends"
    );

    handle.cancel_turn().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), turn_task).await;

    let mut applied_second = false;
    let mut interrupted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                if input.lifecycle == agent_contracts::InputLifecycle::InterruptCommitted {
                    interrupted = true;
                    assert_eq!(input.kind, agent_contracts::InputKind::CancelTurn);
                    assert_eq!(input.causal_parent, applied_first.input_id);
                }
                if input.lifecycle == agent_contracts::InputLifecycle::Applied
                    && input.preview == "second"
                {
                    applied_second = true;
                    assert_eq!(input.input_id, queued.input_id);
                }
            }
        }
        if applied_second && interrupted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(interrupted, "cancel must publish InterruptCommitted");
    assert!(applied_second, "queued dialogue must apply after cancel");
    assert!(
        context
            .user_messages
            .lock()
            .unwrap()
            .iter()
            .any(|body| body == "second"),
        "drained queue must ingest the queued body"
    );

    handle.cancel_turn().await.unwrap();
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn applied_user_input_is_consumed_then_archived_when_the_turn_commits() {
    let (handle, _task) = start(Arc::new(StreamingModel)).await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut lifecycles = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::UserMessageAccepted { input } = envelope.event {
                lifecycles.push(input.lifecycle);
            }
        }
        if lifecycles.contains(&agent_contracts::InputLifecycle::Archived) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        lifecycles.contains(&agent_contracts::InputLifecycle::Applied),
        "turn start must publish Applied, got {lifecycles:?}"
    );
    assert!(
        lifecycles.contains(&agent_contracts::InputLifecycle::Consumed),
        "model consumption must publish Consumed, got {lifecycles:?}"
    );
    assert!(
        lifecycles.contains(&agent_contracts::InputLifecycle::Archived),
        "TurnCompleted must publish Archived, got {lifecycles:?}"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn structurally_empty_model_completion_does_not_complete_the_turn() {
    use std::sync::atomic::Ordering;

    let model = Arc::new(StructurallyEmptyModel::default());
    let (handle, _task) = start(model.clone()).await;
    let mut events = handle.subscribe();
    handle
        .user_message("Append to src/scratch.md: printer is in room 4B.".into())
        .await
        .unwrap();

    let (saw_error, saw_completed, saw_consumed) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut saw_error = false;
            let mut saw_completed = false;
            let mut saw_consumed = false;
            loop {
                match events.recv().await {
                    Ok(envelope) => match envelope.event {
                        RuntimeEvent::Error { message } => {
                            assert!(
                                message.contains("structurally empty"),
                                "unexpected error: {message}"
                            );
                            saw_error = true;
                            break;
                        }
                        RuntimeEvent::TurnCompleted => saw_completed = true,
                        RuntimeEvent::UserMessageAccepted { input }
                            if input.lifecycle == agent_contracts::InputLifecycle::Consumed =>
                        {
                            saw_consumed = true;
                        }
                        _ => {}
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            (saw_error, saw_completed, saw_consumed)
        })
        .await
        .expect("structurally empty completion was not fenced");

    assert!(saw_error);
    assert!(!saw_completed, "empty 0/0 must not TurnCompleted");
    assert!(
        !saw_consumed,
        "empty retries must not publish input Consumed"
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        3,
        "one attempt plus two bounded retries"
    );
    handle.stop().await.unwrap();
}
