//! Current directive identity through live continuation and a fresh runtime.
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_contracts::{
    AgentResult, ArtifactLocator, ContextEngine, InputKind, MAX_TASK_ANCHOR_TEXT_CHARS,
    ModelCapabilities, ModelMessage, ModelOutput, ModelRequest, ModelRole, ModelTransport,
    RuntimeEvent, RuntimeEventEnvelope, RuntimeInputEnvelope,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    AuthorityRecoveryServices, CheckpointStore, ModuleHost, RuntimeInstance, RuntimeServices,
};
use agent_storage::FileOperationJournal;
use agent_workspace::Workspace;
use context_baselines::RollingSummaryEngine;

#[derive(Default)]
struct RecordingModel(Mutex<Vec<Vec<ModelMessage>>>);
#[async_trait::async_trait]
impl ModelTransport for RecordingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_window: Some(32_000),
            max_output_tokens: 256,
            ..Default::default()
        }
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.0.lock().unwrap().push(request.messages);
        Ok(ModelOutput {
            content: "execution segment finished; task remains active".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

async fn instance(
    dir: Option<&Path>,
    model: Arc<RecordingModel>,
) -> (
    RuntimeInstance,
    Option<Arc<Workspace>>,
    Arc<RollingSummaryEngine>,
) {
    let context = Arc::new(RollingSummaryEngine::new());
    let workspace = match dir {
        Some(dir) => Some(Arc::new(Workspace::open(dir).await.unwrap())),
        None => None,
    };
    let mut services = if let Some(dir) = dir {
        RuntimeServices::try_new(
            CoreAuthorityConfig::default(),
            context.clone(),
            model,
            Arc::new(super::harness::TestToolDispatcher),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
            AuthorityRecoveryServices::new(
                Arc::new(
                    FileOperationJournal::open(dir.join("runtime-operations.jsonl"))
                        .unwrap()
                        .0,
                ),
                None,
            ),
        )
        .unwrap()
    } else {
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            context.clone(),
            model,
            Arc::new(super::harness::TestToolDispatcher),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        )
    };
    if let Some(workspace) = &workspace {
        services = services.with_artifact_workspace(workspace.clone());
    }
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let instance = RuntimeInstance::spawn(host, services);
    instance.start().await.unwrap();
    (instance, workspace, context)
}

async fn finish(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> Vec<RuntimeInputEnvelope> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut inputs = Vec::new();
        loop {
            match events.recv().await.unwrap().event {
                RuntimeEvent::UserMessageAccepted { input } => inputs.push(input),
                RuntimeEvent::TurnCompleted => return inputs,
                RuntimeEvent::TurnCommitFailed { .. } => panic!("directive turn failed to commit"),
                _ => {}
            }
        }
    })
    .await
    .expect("turn must settle")
}

fn body() -> String {
    // Preserve both the non-ASCII tail and significant surrounding whitespace.
    format!(
        "  {}\n必须保留公共 API；尾部约束不可遗漏。\n  ",
        "a".repeat(2_100)
    )
}

fn assert_received(model: &RecordingModel, expected: &str, count: usize) {
    let requests = model.0.lock().unwrap();
    assert_eq!(requests.len(), count);
    for request in requests.iter() {
        assert!(
            request
                .iter()
                .any(|message| message.role == ModelRole::User && message.content == expected),
            "each request must carry the exact full directive, including its tail and whitespace"
        );
    }
}

#[tokio::test]
async fn full_directive_survives_live_and_cold_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(RecordingModel::default());
    let (source, workspace, context) = instance(Some(dir.path()), model.clone()).await;
    let handle = source.handle();
    handle
        .set_focus("inspect implementation".into())
        .await
        .unwrap();
    let mut events = handle.subscribe();
    let body = body();
    handle.user_message(body.clone()).await.unwrap();
    let first_inputs = finish(&mut events).await;
    let original = first_inputs
        .iter()
        .find(|input| input.kind == InputKind::Dialogue)
        .unwrap();
    let before = source.checkpoint().await.unwrap();
    let task = before
        .tasks
        .tasks
        .iter()
        .find(|task| Some(task.id) == before.current_task_id)
        .unwrap();
    assert!(task.turn_intent.chars().count() <= MAX_TASK_ANCHOR_TEXT_CHARS);
    assert!(
        task.current_directive
            .as_ref()
            .unwrap()
            .inline_body
            .is_none()
    );
    assert_eq!(
        task.current_directive.as_ref().unwrap().input.body_ref,
        original.body_ref
    );
    let basis = (
        task.id,
        task.resume.directive_revision,
        task.anchor.verification_revision,
    );
    handle.continue_active_task().await.unwrap();
    let inputs = finish(&mut events).await;
    assert_received(&model, &body, 2);
    for input in inputs
        .iter()
        .filter(|input| input.kind == InputKind::TaskContinuation)
    {
        assert!(input.input_id.is_none());
        assert_eq!(input.causal_parent, original.input_id);
        assert_eq!(input.body_ref, original.body_ref);
        assert_eq!(input.bytes, body.len() as u64);
    }
    assert!(!inputs.iter().any(|input| input.kind == InputKind::Dialogue));
    // Continuation did not re-ingest another user message into Rolling.
    let context_data = context.checkpoint().await.unwrap();
    assert_eq!(
        context_data["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["content"] == body)
            .count(),
        1
    );
    let checkpoint = source.checkpoint().await.unwrap();
    let store = CheckpointStore::new(dir.path().join("retained-checkpoints"));
    let stored = store
        .write_atomic(&serde_json::to_vec(&checkpoint).unwrap())
        .await
        .unwrap();
    let decoded = agent_runtime::decode_checkpoint_file(
        &dir.path()
            .join("retained-checkpoints")
            .join(stored.artifact),
    )
    .unwrap();
    source.shutdown().await.unwrap();
    drop(workspace);
    drop(context);
    let (fresh, _, _) = instance(Some(dir.path()), model.clone()).await;
    fresh.restore(decoded).await.unwrap();
    let mut events = fresh.handle().subscribe();
    fresh.handle().continue_active_task().await.unwrap();
    let inputs = finish(&mut events).await;
    assert_received(&model, &body, 3);
    assert!(
        inputs
            .iter()
            .any(|input| input.body_ref == original.body_ref
                && input.kind == InputKind::TaskContinuation)
    );
    let after = fresh.checkpoint().await.unwrap();
    let task = after
        .tasks
        .tasks
        .iter()
        .find(|task| task.id == basis.0)
        .unwrap();
    assert_eq!(
        (
            task.id,
            task.resume.directive_revision,
            task.anchor.verification_revision
        ),
        basis
    );
    fresh.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_directive_without_artifact_workspace_uses_bounded_inline_body() {
    let model = Arc::new(RecordingModel::default());
    let (instance, _, _) = instance(None, model.clone()).await;
    let mut events = instance.handle().subscribe();
    let body = body();
    instance.handle().user_message(body.clone()).await.unwrap();
    finish(&mut events).await;
    let checkpoint = instance.checkpoint().await.unwrap();
    let directive = checkpoint.tasks.tasks[0]
        .current_directive
        .as_ref()
        .unwrap();
    assert_eq!(directive.inline_body.as_deref(), Some(body.as_str()));
    assert!(directive.input.body_ref.is_none());
    instance.handle().continue_active_task().await.unwrap();
    finish(&mut events).await;
    assert_received(&model, &body, 2);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_directive_artifact_refuses_continuation_without_using_preview() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(RecordingModel::default());
    let (instance, workspace, _) = instance(Some(dir.path()), model.clone()).await;
    let mut events = instance.handle().subscribe();
    instance.handle().user_message(body()).await.unwrap();
    finish(&mut events).await;
    let before = instance.checkpoint().await.unwrap();
    let directive = before.tasks.tasks[0].current_directive.as_ref().unwrap();
    let reference = directive.input.body_ref.as_deref().unwrap();
    let locator = ArtifactLocator::parse(reference).unwrap();
    let (_, file) = workspace
        .unwrap()
        .open_artifact_for_run(reference, locator.run_id())
        .await
        .unwrap();
    let path = file.display().to_path_buf();
    drop(file);
    tokio::fs::remove_file(path).await.unwrap();
    let error = instance.handle().continue_active_task().await.unwrap_err();
    assert!(error.to_string().contains("cannot read retained input"));
    assert_eq!(model.0.lock().unwrap().len(), 1);
    let after = instance.checkpoint().await.unwrap();
    assert_eq!(
        after.tasks.tasks[0].resume.directive_revision,
        before.tasks.tasks[0].resume.directive_revision
    );
    while let Ok(envelope) = events.try_recv() {
        assert!(!matches!(
            envelope.event,
            RuntimeEvent::TaskContinuationStarted { .. }
        ));
    }
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_directive_at_the_old_preview_cap_requires_resubmission() {
    let model = Arc::new(RecordingModel::default());
    let (instance, _, _) = instance(None, model.clone()).await;
    let mut events = instance.handle().subscribe();
    instance.handle().user_message(body()).await.unwrap();
    finish(&mut events).await;
    let mut checkpoint = instance.checkpoint().await.unwrap();
    checkpoint.tasks.tasks[0].current_directive = None;
    instance.restore(checkpoint.clone()).await.unwrap();
    assert!(
        instance
            .handle()
            .continue_active_task()
            .await
            .unwrap_err()
            .to_string()
            .contains("legacy task directive may be truncated")
    );
    assert_eq!(model.0.lock().unwrap().len(), 1);
    checkpoint.tasks.tasks[0].turn_intent = "complete legacy instruction".into();
    instance.restore(checkpoint).await.unwrap();
    instance.handle().continue_active_task().await.unwrap();
    finish(&mut events).await;
    assert_eq!(model.0.lock().unwrap().len(), 2);
    instance.shutdown().await.unwrap();
}
