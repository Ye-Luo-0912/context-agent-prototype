//! QA (review 6afa25df Q1): the runtime's final consumption ack carries every
//! id of the final `materialized.items` — including a PromptRequired cold
//! body that B1's resolution captured and served while its only residency
//! owner is a pending spill-card row (a later install in the same resolution
//! demoted it back under the small hot cap). The real engine must accept that
//! ack so the round commits; rejecting it used to drop a valid result. The
//! fixture is driven entirely through the public engine surface: ingest →
//! maintain/gc → Cold→External aging → checkpoint spill → restore with a
//! zero-sized first batch leaves every required target a pending row.

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, AnchorRootClaim, AnchorRootStrength, ContextConsumptionAck, ContextEngine,
    ContextGcReport, ContextIngress, ContextItemId, ContextItemSummary, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextSearchQuery, ContextStateTransition,
    ExternalizedContext, FocusState, MaterializedContext, ModelCapabilities, ModelOutput,
    ModelRequest, ModelTransport, RootReason, RuntimeEvent, ScopeId, ScopeKind, StorageGcReport,
    StoreReconcileReport, TaskId, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeServices, spawn_runtime};
use context_simple::{SimpleContextConfig, SimpleContextEngine};

const SENTINELS: [&str; 3] = ["payload-req-alpha", "payload-req-beta", "payload-req-gamma"];

fn fixture_config(dir: &tempfile::TempDir) -> SimpleContextConfig {
    SimpleContextConfig {
        // 捕获把全部条目写成卡片；restore 一张不装（全 pending）。
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 0,
        // B1 解析批内的驱逐被测对象：C 的安装把 A 挤回 pending。
        external_hot_metadata_max_entries: 2,
        gc_buffer_capacity: 0,
        // Cold → External 老化：一代即达，公开 API 驱动捕获门。
        gc_external_ttl_generations: 1,
        external_checkpoint_io_budget_ms: 60_000,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }
}

fn observation_output(id: &str, content: &str, path: &str) -> ToolOutput {
    ToolOutput {
        call_id: id.into(),
        tool_name: "shell.exec".into(),
        ok: true,
        summary: "ok".into(),
        model_content: content.into(),
        artifact_ref: None,
        // 观察正文不进全文索引；路径身份才可搜索（recall 同款）。
        metadata: serde_json::json!({"path": path}),
    }
}

/// Seed three sentinel observations and externalize them through the public
/// surface. The contents exceed the ref-summary preview window so only the
/// real fetch/capture path can carry the full body.
async fn seed_three_required(engine: &SimpleContextEngine) -> [ContextItemId; 3] {
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, "cold ack probe"),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "collect the three required bodies".into(),
        })
        .await
        .unwrap();
    for (index, sentinel) in SENTINELS.iter().enumerate() {
        let content = format!("note {index}: {sentinel} {}", "z".repeat(160));
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: observation_output(
                    &format!("step-{index}"),
                    &content,
                    &format!("{sentinel}.rs"),
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.externalized >= 3,
        "the seed must externalize the three observations: {report:?}"
    );
    // 老化到 External（捕获门要求）：第一个后续 GC 为每条建立世代锚，
    // 下一个跨过 ttl=1 边界。
    engine.gc().await.unwrap();
    engine.gc().await.unwrap();

    let mut ids = [ContextItemId::default(); 3];
    for (slot, sentinel) in SENTINELS.iter().enumerate() {
        let refs = engine
            .search_external(ContextSearchQuery {
                query: sentinel.to_string(),
                kind: None,
                scope: None,
                task_id: None,
                label: None,
                limit: 16,
            })
            .await
            .unwrap();
        assert!(
            !refs.is_empty(),
            "the seeded {sentinel} must be searchable once externalized"
        );
        ids[slot] = refs[0].item_id;
    }
    ids
}

/// Delegate to the real engine; materialize gains the PromptRequired claims
/// for the three seeded ids (the runtime builds the query from the turn and
/// does not know these refs).
struct RequiredRefsEngine {
    inner: SimpleContextEngine,
    required: [ContextItemId; 3],
}

impl RequiredRefsEngine {
    fn new(inner: SimpleContextEngine, required: [ContextItemId; 3]) -> Self {
        Self { inner, required }
    }

    fn query_with_required(&self, query: ContextQuery) -> ContextQuery {
        let mut query = query;
        query.hints.anchor_roots = self
            .required
            .iter()
            .map(|id| AnchorRootClaim {
                item_ref: format!("context://run/{id}"),
                strength: AnchorRootStrength::PromptRequired,
                source_field_id: "working_refs".into(),
                anchor_revision: 3,
                reason: RootReason::HardConstraint,
            })
            .collect();
        query
    }
}

#[async_trait::async_trait]
impl ContextEngine for RequiredRefsEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.inner.ingest(ingress).await
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.inner.maintain(trigger).await
    }
    async fn gc(&self) -> AgentResult<ContextGcReport> {
        self.inner.gc().await
    }
    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        self.inner
            .materialize(self.query_with_required(query))
            .await
    }
    async fn acknowledge_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()> {
        self.inner.acknowledge_consumption(ack).await
    }
    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        self.inner.open_scope(kind, parent).await
    }
    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        self.inner.close_scope(scope_id).await
    }
    async fn diagnostics(&self) -> AgentResult<agent_contracts::ContextDiagnostics> {
        self.inner.diagnostics().await
    }
    async fn storage_gc(&self) -> AgentResult<StorageGcReport> {
        self.inner.storage_gc().await
    }
    async fn reconcile_store(&self) -> AgentResult<StoreReconcileReport> {
        self.inner.reconcile_store().await
    }
    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        self.inner.inspect(limit).await
    }
    async fn search_external(
        &self,
        query: ContextSearchQuery,
    ) -> AgentResult<Vec<ExternalizedContext>> {
        self.inner.search_external(query).await
    }
    async fn inspect_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<ExternalizedContext>> {
        self.inner.inspect_external(item_id).await
    }
    async fn fetch_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<agent_contracts::ContextItem>> {
        self.inner.fetch_external(item_id).await
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        self.inner.checkpoint().await
    }
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        self.inner.restore(data).await
    }
}

/// Final answer whose prompt is recorded: the required bodies must actually
/// reach the model request through B1's capture.
#[derive(Debug, Default)]
struct RecordingModel {
    prompts: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ModelTransport for RecordingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.prompts.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect(),
        );
        Ok(ModelOutput {
            content: "done".into(),
            tool_calls: Vec::new(),
            usage: agent_contracts::ModelUsage {
                input_tokens: Some(40),
                output_tokens: Some(4),
                attempts: 1,
                ..Default::default()
            },
        })
    }
}

#[derive(Debug, Default)]
struct NoTools;

#[async_trait::async_trait]
impl ToolDispatcher for NoTools {
    fn specs(&self) -> Vec<agent_contracts::ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        panic!("this turn must not dispatch tools")
    }
}

/// QA (Q1) runtime integration: the round must commit — no consumption-ack
/// rejection, `TurnCompleted` arrives — and the required bodies must have
/// reached the model request; the cold row stays serviceable afterwards.
#[tokio::test]
async fn cold_required_bodies_commit_through_the_real_runtime_ack() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimpleContextEngine::new(fixture_config(&dir));
    let required = seed_three_required(&source).await;
    let checkpoint = source.checkpoint().await.unwrap();
    // 公开的 pending 证明：三张卡片全在 manifest（spilled）、inline 段为
    // 空，restore 批次为 0 → restore 后每个目标只有冷定位行。（不要用
    // search 探测——它会安装命中的 pending 卡。）
    assert_eq!(
        checkpoint
            .get("external_spilled")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(3),
        "setup: all three bodies must be spilled to cards"
    );
    assert_eq!(
        checkpoint
            .get("external")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(0),
        "setup: the checkpoint carries no inline external entries"
    );

    let inner = SimpleContextEngine::new(fixture_config(&dir));
    inner.restore(checkpoint).await.unwrap();

    let model = Arc::new(RecordingModel::default());
    let context = Arc::new(RequiredRefsEngine::new(inner, required));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        model.clone(),
        Arc::new(NoTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task): (RuntimeHandle, tokio::task::JoinHandle<()>) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle
        .user_message("continue with the required bodies".into())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(envelope) => {
                    if let RuntimeEvent::Error { message } = &envelope.event {
                        panic!(
                            "the cold-served required bodies must not fail the round: {message}"
                        );
                    }
                    if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event stream closed before TurnCompleted")
                }
            }
        }
    })
    .await
    .expect("the round did not commit within the test deadline");

    // B1 的捕获把三份正文送进了真实模型请求。锁作用域在 await 之前结束。
    let prompt_snapshot: Vec<String> = model.prompts.lock().unwrap().clone();
    assert!(
        !prompt_snapshot.is_empty(),
        "the runtime must have issued at least one model request"
    );
    for sentinel in SENTINELS {
        assert!(
            prompt_snapshot
                .iter()
                .any(|prompt| prompt.contains(sentinel)),
            "required body {sentinel} must reach the model prompt: {prompt_snapshot:?}"
        );
    }

    // 提交之后冷行仍然可服务：按 id 取回正文。
    let fetched = context
        .fetch_external(required[0])
        .await
        .unwrap()
        .expect("the consumed cold row stays serviceable by id");
    assert!(fetched.content.contains(SENTINELS[0]));
    handle.stop().await.unwrap();
}
