use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentResult, ContextEngine, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport,
    RuntimeEvent, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput,
    ToolRisk, ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;
use tokio::sync::Mutex;

use crate::harness::TestContextEngine;

struct RepeatedReadsModel {
    decisions: AtomicUsize,
    window: Option<usize>,
    final_round: usize,
    final_request: Mutex<Option<ModelRequest>>,
}

#[async_trait::async_trait]
impl ModelTransport for RepeatedReadsModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calls: true,
            context_window: self.window,
            max_output_tokens: 8192,
            ..Default::default()
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.decisions.fetch_add(1, Ordering::SeqCst);
        if round == self.final_round {
            *self.final_request.lock().await = Some(request);
            return Ok(ModelOutput {
                content: "source inspection done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            });
        }
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("read-{round}"),
                name: "fs.read".into(),
                arguments: json!({"path": format!("api/module-{round}.py")}),
            }],
            usage: Default::default(),
        })
    }
}

pub(super) struct ReadFixtures {
    pub(super) expose_resource: bool,
    pub(super) body_bytes: usize,
    pub(super) first_observation: Option<(&'static str, bool)>,
}

#[async_trait::async_trait]
impl ToolDispatcher for ReadFixtures {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a source module".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let path = request.call.arguments["path"].as_str().unwrap();
        let metadata = if self.expose_resource {
            json!({"path": path, "revision": "fixture-v1", "covers_file": true})
        } else {
            json!({})
        };
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: if path == "api/module-0.py" {
                self.first_observation.map(|(_, ok)| ok).unwrap_or(true)
            } else {
                true
            },
            summary: format!("read {path}"),
            model_content: if path == "api/module-0.py" {
                if let Some((content, _)) = self.first_observation {
                    content.to_string()
                } else if self.body_bytes == 0 {
                    format!("body {path}")
                } else {
                    format!("body {path} {}", "x".repeat(self.body_bytes))
                }
            } else if self.body_bytes == 0 {
                format!("body {path}")
            } else {
                format!("body {path} {}", "x".repeat(self.body_bytes))
            },
            artifact_ref: None,
            metadata,
        }))
    }
}

async fn final_request_for_window(window: Option<usize>, body_bytes: usize) -> ModelRequest {
    let model = Arc::new(RepeatedReadsModel {
        decisions: AtomicUsize::new(0),
        window,
        final_round: 12,
        final_request: Mutex::new(None),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(ReadFixtures {
            expose_resource: false,
            body_bytes,
            first_observation: None,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("inspect twelve source modules".into())
        .await
        .unwrap();
    handle
        .user_message("read all twelve modules".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                events.recv().await.unwrap().event,
                RuntimeEvent::TurnCompleted
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    instance.shutdown().await.unwrap();
    assert_eq!(model.decisions.load(Ordering::SeqCst), 13);
    model.final_request.lock().await.take().unwrap()
}

#[tokio::test]
async fn spare_provider_window_retains_all_small_turn_results_on_the_actual_model_request() {
    let with_headroom = final_request_for_window(Some(32768), 0).await;
    let without_headroom = final_request_for_window(Some(24000), 0).await;
    let has_earliest_body = |request: &ModelRequest| {
        request
            .messages
            .iter()
            .any(|message| message.content.contains("body api/module-0.py"))
    };
    assert!(has_earliest_body(&with_headroom));
    assert!(!has_earliest_body(&without_headroom));
    assert!(
        without_headroom
            .messages
            .iter()
            .any(|message| { message.content.contains("TURN CHECKPOINT:") })
    );
    assert!(
        !with_headroom
            .messages
            .iter()
            .any(|message| { message.content.contains("TURN CHECKPOINT:") })
    );
}

#[tokio::test]
async fn oversized_recent_tool_results_checkpoint_further_before_context_selection() {
    let request = final_request_for_window(Some(32768), 17_000).await;
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.content.contains("TURN CHECKPOINT:")),
        "the provider window must make older complete exchanges leave the wire"
    );
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.content.contains("body api/module-11.py")),
        "a recent completed exchange must remain available"
    );
    assert!(
        !request
            .messages
            .iter()
            .any(|message| message.content.contains("body api/module-0.py")),
        "old bodies should be represented by the checkpoint, not sent raw"
    );
}

#[tokio::test]
async fn accepted_tool_observation_enters_real_context_before_the_following_model_round() {
    let directory = tempfile::tempdir().unwrap();
    let engine = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig {
            context_store_dir: Some(directory.path().join("context-store")),
            ..Default::default()
        },
    ));
    let model = Arc::new(RepeatedReadsModel {
        decisions: AtomicUsize::new(0),
        window: Some(32768),
        final_round: 12,
        final_request: Mutex::new(None),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        engine.clone(),
        model,
        Arc::new(ReadFixtures {
            expose_resource: true,
            body_bytes: 0,
            first_observation: None,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("inspect twelve source modules".into())
        .await
        .unwrap();
    handle
        .user_message("Read api/module-0.py and keep working on the other modules".into())
        .await
        .unwrap();
    let mut prepared = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await.unwrap().event {
                RuntimeEvent::ContextPrepared {
                    diagnostics,
                    selected,
                    ..
                } => {
                    prepared.push((
                        diagnostics.total_items,
                        diagnostics.tool_round,
                        selected.len(),
                    ));
                }
                RuntimeEvent::TurnCompleted => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();
    instance.shutdown().await.unwrap();
    assert!(prepared.len() >= 2);
    assert!(
        prepared[1].0 > prepared[0].0 && prepared[1].1 > prepared[0].1,
        "a settled observation must be visible to Context before the next model decision: {prepared:?}"
    );
    assert!(
        prepared[1].2 > 0,
        "the relevant accepted observation should be selected, not merely counted: {prepared:?}"
    );
    assert_eq!(
        engine.diagnostics().await.unwrap().tool_round,
        12,
        "turn-final settlement must not ingest the early observations again"
    );
    let restored = context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig {
        context_store_dir: Some(directory.path().join("context-store")),
        ..Default::default()
    });
    restored.restore(checkpoint.context).await.unwrap();
    assert_eq!(
        restored.diagnostics().await.unwrap().tool_round,
        12,
        "cold restoration must keep the same accepted observation count"
    );
}

#[tokio::test]
async fn provider_headroom_keeps_context_selectable_and_old_failures_fetchable() {
    const BLOCKER: &str = "MySQL FLUSH TABLES privilege is unavailable in this task";
    let directory = tempfile::tempdir().unwrap();
    let engine = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig {
            context_store_dir: Some(directory.path().join("context-store")),
            ..Default::default()
        },
    ));
    let model = Arc::new(RepeatedReadsModel {
        decisions: AtomicUsize::new(0),
        window: Some(65_536),
        final_round: 55,
        final_request: Mutex::new(None),
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig {
            max_tool_rounds: 56,
            ..Default::default()
        },
        engine.clone(),
        model.clone(),
        Arc::new(ReadFixtures {
            expose_resource: true,
            body_bytes: 6_000,
            first_observation: Some((BLOCKER, false)),
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("migrate a MySQL database to PostgreSQL".into())
        .await
        .unwrap();
    handle
        .user_message("Inspect the migration modules and verify a viable cutover".into())
        .await
        .unwrap();
    let mut prepared = Vec::new();
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            match events.recv().await.unwrap().event {
                RuntimeEvent::ContextPrepared { selected, .. } => prepared.push(
                    selected
                        .into_iter()
                        .map(|item| (item.item_id, item.reason))
                        .collect::<Vec<_>>(),
                ),
                RuntimeEvent::TurnCompleted => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    instance.shutdown().await.unwrap();
    assert_eq!(model.decisions.load(Ordering::SeqCst), 56);
    let request = model.final_request.lock().await.take().unwrap();
    let errors = engine
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == agent_contracts::ContextKind::Error)
        .collect::<Vec<_>>();
    assert!(
        prepared.last().is_some_and(|selected| !selected.is_empty()),
        "provider-only history headroom must leave a usable Context allocation"
    );
    assert_eq!(
        errors.len(),
        1,
        "the unresolved failure remains one live item"
    );
    assert!(errors[0].semantic.is_live());
    let stored = engine.fetch_external(errors[0].id).await.unwrap().unwrap();
    assert!(
        stored.content.contains(BLOCKER),
        "the failed observation must remain retrievable after the long turn"
    );
    assert!(request.messages.iter().any(|message| {
        message.content.contains("[ToolObservation") || message.content.contains("[FileObservation")
    }));
}
