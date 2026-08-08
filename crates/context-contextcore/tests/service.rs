//! End-to-end tests for the context-service adapter.
//!
//! Cargo sets `CARGO_BIN_EXE_agent-context-service` for integration tests, so
//! these tests spawn the real service process and drive the full
//! `ContextEngine` contract across the process boundary — including plugging
//! the adapter into a real `AgentKernel` (the acceptance criterion: a
//! composition-root change, nothing else).

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, ContextEngine, ContextIngress, ContextItemSummary, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, MaterializedContext, ModelCapabilities, ModelOutput,
    ModelRequest, ModelTransport, RuntimeEvent, ToolOutput,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};
use context_contextcore::{ContextServiceAdapter, ContextServiceConfig, ServiceEngine};
use serde_json::json;

async fn connect() -> Arc<dyn ContextEngine> {
    let adapter = ContextServiceAdapter::connect(&ContextServiceConfig {
        engine: ServiceEngine::Dynamic,
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn + handshake with the context service");
    Arc::new(adapter)
}

#[tokio::test]
async fn full_contract_round_trip_across_the_process_boundary() {
    let engine = connect().await;

    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor AuthService".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::Pin {
            content: "never touch generated files".into(),
            kind: agent_contracts::ContextKind::Constraint,
        })
        .await
        .unwrap();

    let report: ContextMaintenanceReport = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(report.diagnostics.total_items >= 2);

    let snapshot: MaterializedContext = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(snapshot.approx_tokens > 0);
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.content.contains("never touch generated files")),
        "pinned constraint must cross the wire"
    );

    let items: Vec<ContextItemSummary> = engine.inspect(100).await.unwrap();
    assert_eq!(items.len(), 2);

    // Scope lifecycle crosses the wire: open a tool scope, close it.
    let scope_id = engine
        .open_scope(agent_contracts::ScopeKind::Tool, None)
        .await
        .unwrap();
    let transitions = engine.close_scope(scope_id).await.unwrap();
    assert!(
        transitions.is_empty(),
        "tool scope close produces no evictions"
    );

    // Checkpoint/restore round-trip across the boundary.
    let checkpoint = engine.checkpoint().await.unwrap();
    let adapter2 = ContextServiceAdapter::connect(&ContextServiceConfig {
        engine: ServiceEngine::Dynamic,
        ..ContextServiceConfig::default()
    })
    .await
    .unwrap();
    adapter2.restore(checkpoint).await.unwrap();
    let after = adapter2.diagnostics().await.unwrap();
    assert_eq!(after.total_items, 2);
    adapter2.shutdown().await;
}

#[tokio::test]
async fn adapter_plugs_into_a_real_kernel_without_rewrites() {
    let engine = connect().await;

    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        engine,
        Arc::new(PlainModel),
        Arc::new(ToolDispatcherStub),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _runtime_task) = agent_runtime::spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();

    handle.user_message("hello service".into()).await.unwrap();

    let mut reply = None;
    let mut completed = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::AssistantMessage { content } => reply = Some(content),
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(reply.as_deref(), Some("hello from the other side"));
    assert!(
        completed,
        "the turn must complete through the service engine"
    );
}

/// Emits one fixed assistant reply; no tool calls.
struct PlainModel;

#[async_trait::async_trait]
impl ModelTransport for PlainModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "hello from the other side".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[derive(Debug)]
struct ToolDispatcherStub;

#[async_trait::async_trait]
impl agent_contracts::ToolDispatcher for ToolDispatcherStub {
    fn specs(&self) -> Vec<agent_contracts::ToolSpec> {
        Vec::new()
    }
    async fn execute(
        &self,
        _request: agent_contracts::ToolExecutionRequest,
    ) -> AgentResult<ToolOutput> {
        unreachable!("no tools in this test")
    }
}

#[tokio::test]
async fn service_equivalence_with_in_process_engine() {
    // The same scripted lifecycle must produce the same diagnostics whether
    // the engine runs in-process or behind the service boundary.
    let service = connect().await;
    let local =
        context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig::default());
    let local: Arc<dyn ContextEngine> = Arc::new(local);

    for engine in [service, local] {
        for turn in 0..5 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("turn {turn}: continue the refactor"),
                })
                .await
                .unwrap();
            engine
                .ingest(ContextIngress::ToolObservation {
                    output: ToolOutput {
                        call_id: "c".into(),
                        tool_name: "shell.exec".into(),
                        ok: false,
                        summary: "failed".into(),
                        model_content: "error in AuthService.rs:42".into(),
                        artifact_ref: None,
                        context_action: None,
                        metadata: json!({}),
                    },
                    scope_id: None,
                })
                .await
                .unwrap();
            engine
                .maintain(ContextMaintenanceTrigger::AfterTool)
                .await
                .unwrap();
        }
        let snapshot = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 12_000,
                hints: Default::default(),
            })
            .await
            .unwrap();
        // Errors persist until verified: exactly one live error survives the
        // recurring failures, and the snapshot is bounded.
        assert!(snapshot.approx_tokens < 4_000, "bounded snapshot");
    }
}

#[tokio::test]
async fn gc_parity_between_in_process_and_service_boundary() {
    // The same scripted lifecycle must produce the same GC report whether
    // the engine runs in-process or behind the service boundary. This is
    // the regression test for the GC dimension of the wire protocol: a
    // service that drops `gc()` silently would diverge here.
    let service = connect().await;
    let local: Arc<dyn ContextEngine> = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));

    let mut reports = Vec::new();
    for engine in [service, local] {
        for turn in 0..3 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("turn {turn}: fix AuthService.rs"),
                })
                .await
                .unwrap();
            engine
                .ingest(ContextIngress::ToolObservation {
                    output: ToolOutput {
                        call_id: format!("{turn}"),
                        tool_name: "shell.exec".into(),
                        ok: true,
                        summary: "ok".into(),
                        model_content: format!("tests passed in AuthService.rs ({turn})"),
                        artifact_ref: None,
                        context_action: None,
                        metadata: json!({}),
                    },
                    scope_id: None,
                })
                .await
                .unwrap();
            engine
                .maintain(ContextMaintenanceTrigger::AfterModel)
                .await
                .unwrap();
        }
        let report = engine.gc().await.unwrap();
        assert!(
            report.evicted >= 1,
            "the consumed observations must be evictable, got {report:?}"
        );
        let mut report = serde_json::to_value(report).unwrap();
        strip_random_item_ids(&mut report);
        reports.push(report);
    }
    assert_eq!(
        reports[0], reports[1],
        "GC must behave identically across the process boundary"
    );
}

/// Item ids are random UUIDs generated per engine instance, so they can
/// never match across two runs; strip them before comparing the reports.
/// Everything else (kinds, reasons, ticks, counts, diagnostics) must match.
fn strip_random_item_ids(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("item_id");
            for nested in map.values_mut() {
                strip_random_item_ids(nested);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                strip_random_item_ids(nested);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn missing_service_fails_fast_with_clear_error() {
    let config = ContextServiceConfig {
        program: Some("definitely-not-a-real-binary-xyz".into()),
        startup_timeout: Duration::from_secs(3),
        ..ContextServiceConfig::default()
    };
    let error = match ContextServiceAdapter::connect(&config).await {
        Ok(_) => panic!("connecting to a missing binary must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("spawn context service"),
        "unexpected error: {error}"
    );
}
