//! CORE-3（F04 收尾）：分页与 checkpoint 保留同一语义。会话 1 的
//! `search.grep` 溢出走快照 artifact 并在正文给出 cursor；预算停下落
//! 检查点。冷恢复后的新组合用**恢复前捕获的 cursor**继续翻同一份快照，
//! 即使源文件在两次会话之间已被改写：翻页必须命中同一快照（明确历史
//! 身份）、不重扫、不把改写后的文件内容混进结果。模型脚本与断言全部
//! 走真实组合（真实 grep 工具、真实 checkpoint/restore、真实事件流）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    RuntimeEventEnvelope, RuntimeFailureClass, ToolCall, ToolOutput,
};
use agent_core::{PolicyApprovalGate, TaskApprovalGate};
use agent_workspace::Workspace;
use serde_json::json;

const MATCH_LINES: usize = 120;

/// 会话 1：无条件发起溢出 grep。会话 2：用恢复前捕获的 cursor 翻下一
/// 页，然后收尾。cursor 由测试从会话 1 的 ToolFinished 事件取出后注入
/// 会话 2 的模型，模拟「模型在重启前拿到过这个指针」。
struct PagingModel {
    step: AtomicUsize,
    cursor: Option<String>,
}

impl PagingModel {
    fn fresh() -> Self {
        Self {
            step: AtomicUsize::new(0),
            cursor: None,
        }
    }

    fn resumed(cursor: String) -> Self {
        Self {
            step: AtomicUsize::new(0),
            cursor: Some(cursor),
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for PagingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 4096,
            context_window: None,
        }
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        match (&self.cursor, step) {
            (None, 0) => Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "grep-0".into(),
                    name: "search.grep".into(),
                    arguments: json!({"pattern": "match_", "limit": 300}),
                }],
                usage: Default::default(),
            }),
            (Some(cursor), 0) => Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "grep-resumed".into(),
                    name: "search.grep".into(),
                    arguments: json!({"pattern": "match_", "limit": 300, "cursor": cursor}),
                }],
                usage: Default::default(),
            }),
            _ => Ok(ModelOutput {
                content: "[scripted] paging done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            }),
        }
    }
}

async fn product_config(
    root: &std::path::Path,
    model: Arc<PagingModel>,
    max_tool_rounds: Option<usize>,
) -> anyhow::Result<ComposeConfig> {
    let workspace = Workspace::open(root).await?;
    let journal = Arc::new(
        agent_storage::FileEventJournal::open(workspace.state_dir().join("traces")).await?,
    );
    let recipes = Arc::new(tool_runtime::VerificationRecipes::discover(&workspace)?);
    let host_policies = Arc::new(
        agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
            .map_err(anyhow::Error::msg)?,
    );
    // 只读基策略即可：search.grep 是 ReadOnly。
    let task_gate = Arc::new(TaskApprovalGate::new(Arc::new(
        PolicyApprovalGate::read_only(),
    )));
    let context_engine = agent_compose::build_context_engine(
        agent_compose::ContextPolicy::Dynamic,
        workspace.state_dir(),
        None,
        None,
        &agent_compose::MaintenanceBudget::default(),
    )
    .await?;
    let base_tools = Arc::new(
        tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            Default::default(),
            (*recipes).clone(),
        ),
    );
    let reservation = workspace
        .state_dir()
        .join("authority")
        .join("broker-reservations.jsonl");
    Ok(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model,
        approval: task_gate,
        base_tools,
        capability_aware: true,
        journal: Some(journal),
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: Some(reservation),
        verification_recipes: Some(recipes.clone()),
        project_proof_refresh: !recipes.is_empty(),
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    })
}

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
    matches: impl Fn(&RuntimeEvent) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if events
            .try_recv()
            .is_ok_and(|envelope| matches(&envelope.event))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runtime never {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drain one capture receiver into a vec (small volumes; the broadcast
/// buffer never lags in these scripted flows).
fn drain(receiver: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>) -> Vec<ToolOutput> {
    let mut outputs = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(envelope) => match &envelope.event {
                RuntimeEvent::ToolFinished { output, .. } if output.tool_name == "search.grep" => {
                    outputs.push(output.clone());
                }
                _ => {}
            },
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                panic!("the event buffer lagged; the capture is incomplete")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
    outputs
}

/// 保存 → 结束进程 → 恢复 → 用恢复前的 cursor 翻同一份快照。
#[tokio::test]
async fn snapshot_paging_keeps_its_semantics_across_a_checkpoint_restore() -> anyhow::Result<()> {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let mut body = String::new();
    for i in 0..MATCH_LINES {
        body.push_str(&format!("match_{i:03}: something\n"));
    }
    std::fs::write(root.join("big.txt"), &body).unwrap();
    let checkpoint_dir = root.join(".focus-agent").join("checkpoints");

    // ---- 会话 1：溢出 grep（页 1 + 快照 + cursor），预算停。 ----
    let config = product_config(&root, Arc::new(PagingModel::fresh()), Some(1)).await?;
    let composed = compose(config).await?;
    let mut events = composed.subscribe();
    let mut capture = composed.subscribe();
    let handle = composed.handle().clone();
    handle.start().await?;
    handle.set_focus("find all matches".into()).await?;
    handle.user_message("find all matches".into()).await?;
    wait_for(
        &mut events,
        |event| {
            matches!(
                event,
                RuntimeEvent::Failure {
                    class: RuntimeFailureClass::RoundBudget,
                    ..
                }
            )
        },
        "stop at the round budget",
    )
    .await;
    drop(events);
    let greps = drain(&mut capture);
    assert_eq!(greps.len(), 1, "exactly one grep ran in session 1");
    let first = greps.into_iter().next().unwrap();
    assert!(first.ok);
    assert_eq!(first.metadata["hits"], MATCH_LINES);
    let cursor = first.metadata["cursor"]
        .as_str()
        .expect("an overflowing grep must hand back a cursor")
        .to_string();
    let reference = cursor.split('#').next().unwrap().to_string();
    // 正文契约（F04）：续读指针长在正文里，模型不读 metadata 也能续页。
    assert!(
        first.model_content.contains(&format!(
            "continue with artifact.read reference={reference}"
        )),
        "the body must carry the continuation pointer: {}",
        first.model_content
    );
    composed.shutdown().await?;

    // ---- 重启间隙：源文件被外部改写。 ----
    std::fs::write(root.join("big.txt"), "match_999: poisoned after restart\n").unwrap();

    // ---- 会话 2：冷恢复，用恢复前的 cursor 翻下一页。 ----
    let config = product_config(&root, Arc::new(PagingModel::resumed(cursor)), None).await?;
    let composed = compose(config).await?;
    let mut events = composed.subscribe();
    let mut capture = composed.subscribe();
    composed.instance.start().await?;
    let store = agent_runtime::CheckpointStore::new(&checkpoint_dir);
    let rows = store.list(5).await?;
    assert!(!rows.is_empty(), "the budget stop must land a checkpoint");
    let bytes = std::fs::read(checkpoint_dir.join(&rows.first().unwrap().artifact))?;
    let checkpoint = agent_runtime::decode_checkpoint_bytes(&bytes)?;
    checkpoint.validate()?;
    composed.instance.restore(checkpoint).await?;
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;

    composed.handle().continue_active_task().await?;
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::TurnCompleted),
        "finish the resumed segment",
    )
    .await;
    drop(events);
    let greps = drain(&mut capture);
    assert_eq!(
        greps.len(),
        1,
        "exactly one grep ran in session 2: {}",
        greps.len()
    );
    let page = greps.into_iter().next().unwrap();
    assert!(
        page.ok,
        "the pre-restart cursor must still resolve after restore: {page:?}"
    );
    assert_eq!(
        page.metadata["hits"], MATCH_LINES,
        "the page comes from the original snapshot, not a fresh scan"
    );
    // 历史身份：页脚引用恢复前那一份快照 artifact。
    assert!(
        page.model_content.contains(&reference),
        "the page must name the pre-restart snapshot: {}",
        page.model_content
    );
    // 结束标记：101-120 是最后一页（MODEL_HITS=100），正文从 match_100
    // 起、以 end 收尾——末页标记跨恢复仍然在正文里。
    assert!(
        page.model_content.contains("big.txt:101: match_100"),
        "the resumed page must start at the snapshot's line 101: {}",
        page.model_content
    );
    assert!(
        page.model_content
            .contains(&format!("end of results ({MATCH_LINES} total, snapshot")),
        "the final page must carry the end marker: {}",
        page.model_content
    );
    // 快照不可变：重启后写入源文件的内容不得混进快照页。
    assert!(
        !page.model_content.contains("poisoned"),
        "a snapshot page must not see post-restart file edits: {}",
        page.model_content
    );
    composed.shutdown().await?;
    Ok(())
}
