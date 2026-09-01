use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, ContextEngine, ContextIngress, ContextItemId, Effect,
    EffectDurability, EffectReceipt, ModelCapabilities, ModelMessage, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome,
    ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec,
};

use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};
use serde_json::json;
use tokio::sync::Mutex;

use crate::harness::*;

// ---------------------------------------------------------------------------
// Resource policy: the context meta-tools are bounded by quotas in the
// engine, and a refused directive surfaces to the model as a warning — the
// LLM cannot root the whole heap (or exhaust runtime resources) through
// context.manage. Tools never touch the engine — the runtime routes the
// directive and the engine's quota answers.
// ---------------------------------------------------------------------------

/// Emits `context.hint` on the first two rounds, then a plain reply.
#[derive(Debug)]
struct HintModel {
    item_id: ContextItemId,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for HintModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round < 2 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("hint-{round}"),
                    name: "context.hint".into(),
                    arguments: json!({"item_id": self.item_id.to_string(), "keep": true}),
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

/// Serves `context.hint`: emits a `GcHint` directive with the requested
/// item id and keep flag, exactly like the real `context.manage gc_hint`.
#[derive(Debug)]
struct HintToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for HintToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "context.hint".into(),
            description: "keep an item alive across GC passes".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let item_id: ContextItemId = request
            .call
            .arguments
            .get("item_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .parse()
            .map_err(|error| AgentError::InvalidRequest(format!("bad item id: {error}")))?;
        let keep_alive = request
            .call
            .arguments
            .get("keep")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "hint queued".into(),
                model_content: "hint queued".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive: agent_contracts::RuntimeDirective::Context(
                agent_contracts::ContextAction::GcHint {
                    item_id,
                    keep_alive,
                },
            ),
        })
    }
}

#[tokio::test]
async fn hint_quota_refuses_excess_meta_tool_requests() {
    // A real reference engine with a keep-alive cap of one item: the model
    // hints the same item twice, and the second hint must be refused by
    // the quota — the meta-tool cannot root the whole heap.
    let engine = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig {
            max_keep_alive_items: 1,
            ..context_simple::SimpleContextConfig::default()
        },
    ));
    engine
        .ingest(ContextIngress::UserMessage {
            content: "pin this".into(),
        })
        .await
        .unwrap();
    let summaries = engine.inspect(usize::MAX).await.unwrap();
    let item_id = summaries[0].id;

    let handle = spawn_with(
        Arc::new(HintModel {
            item_id,
            rounds: AtomicUsize::new(0),
        }),
        engine.clone(),
        Arc::new(HintToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("pin the item".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut refused = None;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::Warning { message } => {
                    if message.contains("context directive refused") {
                        refused = Some(message);
                    }
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && refused.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "the turn must complete");
    let refused = refused.expect("the second hint must be refused by the quota");
    assert!(
        refused.contains("keep_alive") && refused.contains("cap 1"),
        "the refusal must name the quota and its cap, got: {refused}"
    );
}

// ---------------------------------------------------------------------------
// Commit-time authority lease (ACI v2 §6): a side-effecting call that
// overruns its lease window is rolled back at commit time and reported as
// a failed tool result — the world must not change after the
// authorization expired, even though the tool computation finished.
// ---------------------------------------------------------------------------

/// A staged write that records whether it was committed or rolled back.
#[derive(Debug, Default)]
struct TracingWriteEffect {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Effect for TracingWriteEffect {
    fn describe(&self) -> String {
        "tracing write".into()
    }
    async fn commit(self: Box<Self>) -> EffectReceipt {
        self.committed.fetch_add(1, Ordering::SeqCst);
        EffectReceipt::Applied {
            durability: EffectDurability::Durable,
            evidence: Some("tx-1".into()),
        }
    }
    async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// One `fs.write` on the first round, a plain reply on the second; records
/// every message list it received.
#[derive(Debug, Default)]
struct LeaseToolModel {
    rounds: AtomicUsize,
    requests: Mutex<Vec<Vec<ModelMessage>>>,
}

#[async_trait::async_trait]
impl ModelTransport for LeaseToolModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().await.push(request.messages.clone());
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "fs.write".into(),
                    arguments: json!({"path": "src/main.rs", "content": "fn main() {}"}),
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

/// Serves `fs.write` by sleeping far past the lease window, then staging
/// its write effect — the commit arrives after the authorization expired.
#[derive(Debug)]
struct SlowWriteDispatcher {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolDispatcher for SlowWriteDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.write".into(),
            description: "write a file".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        // Overrun the 1ms lease window, then stage the effect.
        tokio::time::sleep(Duration::from_millis(80)).await;
        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "staged".into(),
                model_content: "staged write".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(TracingWriteEffect {
                committed: self.committed.clone(),
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

#[tokio::test]
async fn expired_authority_lease_rolls_back_the_staged_effect() {
    let model = Arc::new(LeaseToolModel::default());
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(SlowWriteDispatcher {
        committed: committed.clone(),
        rolled_back: rolled_back.clone(),
    });
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig {
            // 1ms window: any real dispatch overruns it deterministically.
            lease_ttl_ms: Some(1),
            ..CoreAuthorityConfig::default()
        },
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("write it".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut refused_output = None;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. } => {
                    if output.tool_name == "fs.write" {
                        refused_output = Some(output);
                    }
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && refused_output.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(completed, "the turn must complete");
    let output = refused_output.expect("the write tool result must be published");
    assert!(
        !output.ok && output.model_content.contains("not applied"),
        "the overrun write must surface as a failed tool result: {output:?}"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        0,
        "the overrun effect must never commit"
    );
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        1,
        "the overrun effect must be rolled back"
    );

    // The second model round must see the failure, not a success.
    let requests = model.requests.lock().await;
    assert!(
        requests.len() >= 2,
        "the turn must have a second model round"
    );
    let serialized = serde_json::to_string(requests.last().unwrap()).unwrap();
    assert!(
        serialized.contains("not applied"),
        "the failed write must reach the model: {serialized}"
    );
}
