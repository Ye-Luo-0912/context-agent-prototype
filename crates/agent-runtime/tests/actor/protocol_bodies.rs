//! Actor-level protocol-body rehydration: when a turn checkpoint compacts
//! an `fs.read` result out of the retained tail, the exact body must be
//! re-injected into the user-role context frame before the next model
//! decision and one bounded `ProtocolBodyCacheStats` event must describe
//! the restoration (eligible/hit/restored body tokens).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelRole, ModelTransport,
    ModelUsage, RuntimeEvent, ToolCall, ToolDispatcher, ToolExecutionAttribution,
    ToolExecutionPurpose, ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
    VerificationReuse,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};

use crate::harness::*;

/// Seven read rounds create seven completed exchanges, one beyond
/// `TURN_FRAME_KEEP_EXCHANGES` (6), so the first exchange is compacted
/// into the turn checkpoint on the final assembly. An exchange is one
/// assistant tool-call group plus its results, so the reads must be
/// spread across separate model decisions.
const READ_PATHS: [&str; 7] = [
    "src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs", "src/f.rs", "src/g.rs",
];

#[derive(Debug, Default)]
struct ProtocolReadDispatcher {
    executed: AtomicUsize,
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "fs.read".into(),
        description: "read a file body".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        risk: ToolRisk::ReadOnly,
        output_budget: None,
        roles: Vec::new(),
    }
}

#[async_trait::async_trait]
impl ToolDispatcher for ProtocolReadDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![read_spec()]
    }

    fn execution_attribution(&self, _call: &ToolCall) -> ToolExecutionAttribution {
        ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Observe,
            Vec::<String>::new(),
            VerificationReuse::None,
        )
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        self.executed.fetch_add(1, Ordering::SeqCst);
        let path = request
            .call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: "fs.read".into(),
            ok: true,
            summary: "read".into(),
            model_content: format!("body-of-{path}"),
            artifact_ref: None,
            metadata: serde_json::json!({"files": [{"path": path, "revision": "rev-1"}]}),
        }))
    }
}

/// Each decision reads exactly one path, so every read is its own
/// exchange; the decision after the last read finishes the directive.
/// Every assembled request is captured for post-turn assertions.
#[derive(Debug, Default)]
struct SpillReadModel {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    step: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for SpillReadModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calls: true,
            ..ModelCapabilities::default()
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().unwrap().push(request.clone());
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        Ok(if step < READ_PATHS.len() {
            ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("read-{step}"),
                    name: "fs.read".into(),
                    arguments: serde_json::json!({"path": READ_PATHS[step]}),
                }],
                usage: ModelUsage::default(),
            }
        } else {
            ModelOutput {
                content: "done".to_string(),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
            }
        })
    }
}

#[tokio::test]
async fn checkpointed_fs_read_body_reenters_the_user_frame_with_a_stats_event() {
    let tools = Arc::new(ProtocolReadDispatcher::default());
    let model = Arc::new(SpillReadModel::default());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let (handle, _task) = spawn_runtime(Arc::new(services));
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("read the workspace files".into())
        .await
        .unwrap();

    // Collect every stats event plus dispatch counts until the turn ends,
    // then inspect the captured requests to see what the final assembly
    // actually contained.
    let (all_stats, started) = tokio::time::timeout(Duration::from_secs(3), async {
        let mut all_stats = Vec::new();
        let mut started = 0;
        loop {
            match events.recv().await.unwrap().event {
                RuntimeEvent::ProtocolBodyCacheStats {
                    eligible,
                    hit,
                    restored_body_tokens,
                    ..
                } => {
                    all_stats.push((eligible, hit, restored_body_tokens));
                }
                RuntimeEvent::ToolStarted { .. } => started += 1,
                RuntimeEvent::TurnCompleted => break,
                _ => {}
            }
        }
        (all_stats, started)
    })
    .await
    .expect("directive turn completes within the deadline");
    let stats = all_stats
        .iter()
        .copied()
        .find(|(eligible, _, _)| *eligible >= 1)
        .unwrap_or((0, 0, 0));
    handle.stop().await.unwrap();

    assert_eq!(started, READ_PATHS.len(), "every requested read dispatches");

    let requests = model.requests.lock().unwrap();
    assert!(
        requests.len() > READ_PATHS.len(),
        "the directive needs seven read rounds then a final round, got {} requests",
        requests.len()
    );
    let final_request = requests.last().unwrap();
    let restored = final_request
        .messages
        .iter()
        .find(|message| message.content.contains("RESTORED TURN BODIES"))
        .expect("the checkpointed body must be re-injected into the request");
    assert_eq!(restored.role, ModelRole::User);
    assert!(
        restored.content.contains("body-of-src/a.rs"),
        "the exact compacted body must re-enter the frame: {}",
        restored.content
    );
    assert!(
        !restored.content.contains("body-of-src/b.rs"),
        "only compacted bodies are restored, not the retained tail"
    );
    assert!(
        stats.0 >= 1 && stats.1 >= 1 && stats.2 > 0,
        "the stats event must report an eligible restored body, got {stats:?}"
    );
}
