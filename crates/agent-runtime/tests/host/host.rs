use std::sync::{Arc, Mutex};

use agent_contracts::{AgentResult, CapabilityLifecycle, ToolRisk, ToolSpec};
use agent_core::CoreAuthorityConfig;
use agent_runtime::{
    CapabilityId, ContextModule, ModelModule, Module, ModuleHost, RuntimeInstance, RuntimeServices,
    ServiceRegistry, ToolModule,
};
use serde_json::json;

use crate::harness::*;

#[tokio::test]
async fn host_registers_and_looks_up_typed_capabilities() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(Arc::new(StubContextEngine))))
        .unwrap();
    host.add_module(Arc::new(ModelModule::new(Arc::new(StubModel))))
        .unwrap();
    host.add_module(Arc::new(ToolModule::new(Arc::new(StubTools))))
        .unwrap();
    host.add_module(Arc::new(agent_runtime::ApprovalModule::new(Arc::new(
        StubApproval,
    ))))
    .unwrap();
    host.start().await.unwrap();

    // Typed lookups return the exact capability and stay usable.
    let engine = host.registry().context_service().unwrap();
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(diagnostics.total_items, 0);
    assert!(host.registry().model_provider().is_ok());
    assert!(host.registry().tool_provider().is_ok());
    assert!(host.registry().approval_policy().is_ok());
    // Optional capabilities are absent unless a module published them.
    assert!(host.registry().event_store().unwrap().is_none());
    assert!(host.registry().artifact_store().unwrap().is_none());

    host.stop().await.unwrap();
}

#[tokio::test]
async fn host_rejects_duplicate_capability_claims() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(Arc::new(StubContextEngine))))
        .unwrap();
    let duplicate = host.add_module(Arc::new(ContextModule::new(Arc::new(StubContextEngine))));
    assert!(
        duplicate.is_err(),
        "a second context module must be rejected at composition time"
    );
    assert!(
        duplicate
            .unwrap_err()
            .to_string()
            .contains("already claimed"),
        "the error must name the conflict"
    );
}

#[tokio::test]
async fn host_starts_in_order_and_stops_in_reverse() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();

    struct RecordingModule {
        name: &'static str,
        order: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Module for RecordingModule {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> Vec<CapabilityId> {
            Vec::new()
        }
        fn register(&self, _registry: &mut ServiceRegistry) -> AgentResult<()> {
            Ok(())
        }
        async fn start(&self) -> AgentResult<()> {
            self.order
                .lock()
                .unwrap()
                .push(format!("start:{}", self.name));
            Ok(())
        }
        async fn stop(&self) -> AgentResult<()> {
            self.order
                .lock()
                .unwrap()
                .push(format!("stop:{}", self.name));
            Ok(())
        }
    }

    host.add_module(Arc::new(RecordingModule {
        name: "context",
        order: order.clone(),
    }))
    .unwrap();
    host.add_module(Arc::new(RecordingModule {
        name: "model",
        order: order.clone(),
    }))
    .unwrap();

    host.start().await.unwrap();
    host.stop().await.unwrap();

    let order = order.lock().unwrap().clone();
    assert_eq!(
        order,
        vec![
            "start:context".to_string(),
            "start:model".to_string(),
            "stop:model".to_string(),
            "stop:context".to_string(),
        ]
    );
}

#[tokio::test]
async fn external_modules_publish_typed_services_publicly() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ExternalModule {
        service: Arc::new(CustomService),
    }))
    .unwrap();
    host.start().await.unwrap();

    // Any consumer can retrieve the typed service through the public get.
    let service: Arc<CustomService> = host
        .registry()
        .get(CUSTOM_SERVICE, "custom service")
        .unwrap();
    let _ = service;
    assert!(
        host.registry()
            .get::<CustomService>(CUSTOM_SERVICE, "custom service")
            .is_ok()
    );

    host.stop().await.unwrap();
}

#[tokio::test]
async fn host_stops_capabilities_before_typed_modules() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();
    host.register_capability(Arc::new(RecordingCapability::new(log.clone())))
        .unwrap();
    host.add_module(Arc::new(ScriptedModule {
        name: "context",
        log: log.clone(),
        fail_start: false,
        fail_stop: false,
    }))
    .unwrap();
    host.add_module(Arc::new(ScriptedModule {
        name: "model",
        log: log.clone(),
        fail_start: false,
        fail_stop: false,
    }))
    .unwrap();

    host.start().await.unwrap();
    host.stop().await.unwrap();

    let order = log.lock().unwrap().clone();
    // The capability may depend on a typed service (EventStore etc.), so it
    // must be stopped before the modules are.
    assert!(
        order.iter().position(|s| s == "stop:capability")
            < order.iter().position(|s| s == "stop:model"),
        "capabilities stop before typed modules: {order:?}"
    );
    assert_eq!(
        order,
        vec![
            "start:context".to_string(),
            "start:model".to_string(),
            "start:capability".to_string(),
            "stop:capability".to_string(),
            "stop:model".to_string(),
            "stop:context".to_string(),
        ]
    );
}

#[tokio::test]
async fn host_start_rolls_back_everything_when_a_later_module_fails() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();
    for name in ["a", "b"] {
        host.add_module(Arc::new(ScriptedModule {
            name,
            log: log.clone(),
            fail_start: false,
            fail_stop: false,
        }))
        .unwrap();
    }
    host.add_module(Arc::new(ScriptedModule {
        name: "c",
        log: log.clone(),
        fail_start: true,
        fail_stop: false,
    }))
    .unwrap();

    let error = host.start().await.expect_err("start must fail");
    assert!(
        error.to_string().contains("c start failed"),
        "the original failure must be reported: {error}"
    );

    // A and B started, so the transaction must stop them again (reverse).
    let order = log.lock().unwrap().clone();
    assert_eq!(
        order,
        vec![
            "start:a".to_string(),
            "start:b".to_string(),
            "start:c".to_string(),
            "stop:b".to_string(),
            "stop:a".to_string(),
        ]
    );
}

#[tokio::test]
async fn host_stop_runs_every_stop_and_aggregates_all_errors() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();
    host.register_capability(Arc::new(RecordingCapability::failing_stop(log.clone())))
        .unwrap();
    host.add_module(Arc::new(ScriptedModule {
        name: "a",
        log: log.clone(),
        fail_start: false,
        fail_stop: true,
    }))
    .unwrap();
    host.add_module(Arc::new(ScriptedModule {
        name: "b",
        log: log.clone(),
        fail_start: false,
        fail_stop: false,
    }))
    .unwrap();

    host.start().await.unwrap();
    let error = host.stop().await.expect_err("stop must aggregate errors");
    let message = error.to_string();
    assert!(
        message.contains("capability stop failed") && message.contains("a stop failed"),
        "every stop failure must be reported: {message}"
    );

    // Every stop ran even though the first one failed.
    let order = log.lock().unwrap().clone();
    assert_eq!(
        order.iter().filter(|s| s.starts_with("stop:")).count(),
        3,
        "all stops run best effort: {order:?}"
    );
}

#[tokio::test]
async fn registration_rejects_oversized_or_malformed_tool_schemas() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();

    // An oversized input schema (above the 4 KB cap) is rejected at
    // registration — a single capability must not be able to blow up the
    // model surface with one giant schema.
    let mut big = DemoCapability::new(
        "big-schema",
        CapabilityLifecycle::Lazy,
        Arc::new(Mutex::new(false)),
    );
    big.tool_names = vec!["big.tool".into()];
    big.manifest.tools = vec![ToolSpec {
        name: "big.tool".into(),
        description: "x".into(),
        input_schema: json!({"padding": "x".repeat(5 * 1024)}),
        risk: ToolRisk::ReadOnly,
        output_budget: None,
        roles: Vec::new(),
    }];
    let error = registry
        .register(Arc::new(big))
        .expect_err("an oversized schema must be rejected");
    assert!(error.to_string().contains("schema"), "{error}");

    // Too many tools per capability (above the 32-tool cap).
    let mut many = DemoCapability::new(
        "many-tools",
        CapabilityLifecycle::Lazy,
        Arc::new(Mutex::new(false)),
    );
    many.tool_names = (0..40).map(|i| format!("many.tool{i}")).collect();
    let error = registry
        .register(Arc::new(many))
        .expect_err("a tool count above the cap must be rejected");
    assert!(error.to_string().contains("per-capability cap"), "{error}");

    // A malformed tool name is rejected.
    let mut bad = DemoCapability::new(
        "bad-name",
        CapabilityLifecycle::Lazy,
        Arc::new(Mutex::new(false)),
    );
    bad.tool_names = vec!["bad name!".into()];
    let error = registry
        .register(Arc::new(bad))
        .expect_err("a malformed tool name must be rejected");
    assert!(error.to_string().contains("[A-Za-z0-9._:-]"), "{error}");
}

#[tokio::test]
async fn host_rejects_a_duplicate_start() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(Arc::new(StubContextEngine))))
        .unwrap();
    host.start().await.unwrap();

    let error = host
        .start()
        .await
        .expect_err("a second start must be rejected");
    assert!(
        error.to_string().contains("cannot be started twice"),
        "{error}"
    );
    host.stop().await.unwrap();
}

#[test]
#[should_panic(expected = "requires the module host to have reached Serving")]
fn spawn_over_an_unstarted_host_panics() {
    // A runtime spawned over unstarted modules would observe half-built
    // state; the composition contract makes this a panic instead of
    // returning an instance the caller cannot reason about.
    let host = ModuleHost::new();
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(StubContextEngine),
        Arc::new(StubModel),
        Arc::new(StubTools),
        Arc::new(StubApproval),
        None,
    );
    let _ = RuntimeInstance::spawn(host, services);
}
