//! End-to-end recall acceptance test: externalized information is pulled
//! back on demand through the real runtime loop, and the prompt itself
//! never carried that content.
//!
//! The chain under test is the full retrieval surface: the model calls
//! `context.manage op=fetch` → the tool names an `EngineQuery` → the actor
//! routes it to the kernel → the kernel resolves it against the real
//! engine (which reads the context store) → the model receives the exact
//! content in the tool result. Meanwhile the materialized frame exposes
//! externalized items as refs only (uri + summary), so the fetched content
//! rides the tool channel, never the prompt.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, CONTEXT_MANAGE, ContextEngine, ContextIngress, ContextItemId,
    ContextMaintenanceTrigger, ContextQuery, ContextSearchQuery, EngineQuery, FocusState,
    ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent, TaskId, ToolCall,
    ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};
use agent_runtime::spawn_runtime;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::{Value, json};

/// Drive the engine into a state with at least one externalized ref, and
/// return (the ref's item id, the exact content the store holds for it).
/// The test learns the content from the store so the assertions hold no
/// matter which of the seeded observations survived externalization.
///
/// The seeded contents are longer than the ref summary preview window
/// (120 chars), so the *full* content can never ride the prompt — only the
/// bounded preview can — and the fetch is the only way it comes back.
async fn seed_externalized(engine: &SimpleContextEngine) -> (ContextItemId, String) {
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, "service layer"),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let contents = [
        format!("step 0: fix AuthService.rs {}", "x".repeat(160)),
        format!("step 1: fix AuthService.rs {}", "y".repeat(160)),
    ];
    for (i, content) in contents.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: observation_output(&format!("step-{i}"), true, content),
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
        report.externalized >= 1,
        "the seed must externalize at least one item: {report:?}"
    );

    let refs = engine
        .search_external(ContextSearchQuery {
            query: "AuthService".into(),
            kind: None,
            scope: None,
            task_id: None,
            limit: 16,
        })
        .await
        .unwrap();
    assert!(
        !refs.is_empty(),
        "the seed must leave searchable external refs"
    );
    let target = refs[0].item_id;
    let fetched = engine
        .fetch_external(target)
        .await
        .unwrap()
        .expect("the store must hold the ref it advertises");
    (target, fetched.content)
}

fn observation_output(id: &str, ok: bool, content: &str) -> ToolOutput {
    ToolOutput {
        call_id: id.into(),
        tool_name: "shell.exec".into(),
        ok,
        summary: "ok".into(),
        model_content: content.into(),
        artifact_ref: None,
        metadata: json!({}),
    }
}

/// The minimal retrieval surface: `context.manage` fetch emits the typed
/// `EngineQuery` the runtime resolves — the tool still never touches the
/// engine (invariant 3), it only names what it wants.
#[derive(Debug)]
struct EngineQueryTools;

#[async_trait::async_trait]
impl ToolDispatcher for EngineQueryTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: CONTEXT_MANAGE.into(),
            description: "runtime context control and retrieval".into(),
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {"type": "string", "enum": ["fetch"]},
                    "item_id": {"type": "string"}
                }
            }),
            risk: ToolRisk::ReadOnly,
        }]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let op = request
            .call
            .arguments
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("");
        let item_id = request
            .call
            .arguments
            .get("item_id")
            .and_then(Value::as_str);
        match (op, item_id) {
            ("fetch", Some(id)) => {
                let item_id: ContextItemId = id
                    .parse()
                    .map_err(|error| AgentError::InvalidRequest(format!("bad item id: {error}")))?;
                Ok(ToolOutcome::EngineQuery {
                    output: ToolOutput {
                        call_id: request.call.id.clone(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "querying the context engine".into(),
                        model_content: "resolving...".into(),
                        artifact_ref: None,
                        metadata: json!({"engine_query": true}),
                    },
                    query: EngineQuery::FetchExternal { item_id },
                })
            }
            other => Err(AgentError::Tool(format!("unsupported op: {other:?}"))),
        }
    }
}

/// Two-round scripted model. Round 1 must see the externalized ref in the
/// prompt (and never the content); it answers with a fetch tool call.
/// Round 2 must see the exact content in the tool result; it replies plain
/// so the turn finalizes. Observations are recorded instead of panicking
/// inside the model so a regression fails the assertions, not the task.
struct RecallModel {
    target_id: ContextItemId,
    expected_content: String,
    round_1: Mutex<Vec<String>>,
    round_1_saw_content: AtomicBool,
    round_2_saw_content: AtomicBool,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for RecallModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let all_text: String = request
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let ref_lines: Vec<String> = all_text
            .lines()
            .filter(|line| line.contains("context://run/"))
            .map(str::to_string)
            .collect();

        if round == 0 {
            // Before the fetch the prompt shows the ref with only its
            // bounded preview — never the full content, which is longer
            // than the preview window. That is the "refs only" half of
            // the lifecycle.
            self.round_1.lock().unwrap().extend(ref_lines);
            if all_text.contains(&self.expected_content) {
                self.round_1_saw_content.store(true, Ordering::SeqCst);
            }
            return Ok(ModelOutput {
                content: "pulling the externalized observation back".into(),
                tool_calls: vec![ToolCall {
                    id: "recall-1".into(),
                    name: CONTEXT_MANAGE.into(),
                    arguments: json!({"op": "fetch", "item_id": self.target_id.to_string()}),
                }],
                usage: Default::default(),
            });
        }

        // Round 2: the tool result carries the exact content back.
        if all_text.contains(&self.expected_content) {
            self.round_2_saw_content.store(true, Ordering::SeqCst);
        }
        Ok(ModelOutput {
            content: "recalled the observation".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn recall_turn_pulls_external_content_back_without_polluting_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }));
    let (target_id, expected_content) = seed_externalized(&engine).await;

    // The prompt never carried the full content: materialize right after
    // the seed and confirm the frame exposes the externalized item as a
    // ref (uri + bounded preview), while the working set holds none of
    // its content.
    let before = engine
        .materialize(ContextQuery {
            current_input: "recall the observation".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        before
            .external
            .iter()
            .any(|entry| entry.context_ref.uri.ends_with(&target_id.to_string())),
        "the materialized frame must expose the externalized ref"
    );
    assert!(
        before
            .items
            .iter()
            .all(|item| !item.content.contains(&expected_content)),
        "externalized content must not ride in the working set"
    );

    // The runtime loop pulls it back on demand: model -> tool -> engine
    // query -> kernel -> real engine store read -> model tool result.
    let model = Arc::new(RecallModel {
        target_id,
        expected_content: expected_content.clone(),
        round_1: Mutex::new(Vec::new()),
        round_1_saw_content: AtomicBool::new(false),
        round_2_saw_content: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        engine.clone(),
        model.clone(),
        Arc::new(EngineQueryTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _runtime_task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("recall the step-1 observation".into())
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                completed = true;
            }
        }
        if completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "the recall turn must complete");
    handle.stop().await.unwrap();

    // The model saw the ref in the prompt, fetched it, and got the exact
    // content back — through the tool result, not the prompt.
    let round_1 = model.round_1.lock().unwrap();
    assert!(
        round_1
            .iter()
            .any(|line| line.contains(&target_id.to_string())),
        "round 1 must expose the externalized ref, saw: {round_1:?}"
    );
    assert!(
        !model.round_1_saw_content.load(Ordering::SeqCst),
        "the prompt must not carry the full externalized content before the fetch"
    );
    assert!(
        model.round_2_saw_content.load(Ordering::SeqCst),
        "the model must receive the exact content through the tool result"
    );
}
