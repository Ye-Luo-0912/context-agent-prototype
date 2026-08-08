//! Module host tests: typed capability registration and lookup, duplicate
//! rejection, and lifecycle ordering.

use std::sync::{Arc, Mutex};

use agent_contracts::{
    AgentResult, ApprovalGate, CancellationToken, Capability, CapabilityActivation,
    CapabilityInvocationContext, CapabilityLifecycle, CapabilityManifest, CapabilityStatus,
    CapabilityTransport, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RunId,
    ScopeId, ScopeKind, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolLifecycle, ToolOutput,
    ToolRisk, ToolSpec,
};
use agent_runtime::{
    APPROVAL_POLICY, CapabilityAwareDispatcher, CapabilityId, ContextModule, ModelModule, Module,
    ModuleHost, ServiceRegistry, ToolModule,
};
use serde_json::json;

#[derive(Debug)]
struct StubContextEngine;

#[async_trait::async_trait]
impl ContextEngine for StubContextEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            focus: None,
            items: Vec::new(),
            external: Vec::new(),
            selected: Vec::new(),
            approx_tokens: 0,
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct StubModel;

#[async_trait::async_trait]
impl ModelTransport for StubModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "ok".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[derive(Debug)]
struct StubTools;

#[async_trait::async_trait]
impl ToolDispatcher for StubTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    /// One builtin tool in the catalog, so the dispatcher's reserved-name
    /// claim has something to reserve.
    fn catalog(&self) -> Vec<agent_contracts::ToolCatalogEntry> {
        vec![agent_contracts::ToolCatalogEntry {
            name: "fs.read".into(),
            state: ToolLifecycle::Available,
            owner: "builtin".into(),
            description: "stub builtin tool".into(),
        }]
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        Err(agent_contracts::AgentError::Tool("stub".into()))
    }
}

#[derive(Debug)]
struct StubApproval;

#[async_trait::async_trait]
impl ApprovalGate for StubApproval {
    async fn authorize(
        &self,
        _call: &agent_contracts::ToolCall,
        _spec: &ToolSpec,
    ) -> AgentResult<agent_contracts::ApprovalDecision> {
        Ok(agent_contracts::ApprovalDecision::Allow)
    }
}

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

/// A capability id an external crate could publish, beyond the core set.
const CUSTOM_SERVICE: CapabilityId = CapabilityId("custom-service");

/// A typed service defined outside the core module set.
#[derive(Debug)]
struct CustomService;

/// A module written like an external crate: it publishes a typed service
/// through the public `ServiceRegistry::register` path.
struct ExternalModule {
    service: Arc<CustomService>,
}

#[async_trait::async_trait]
impl Module for ExternalModule {
    fn name(&self) -> &'static str {
        "external"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        vec![CUSTOM_SERVICE]
    }
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()> {
        registry.register(CUSTOM_SERVICE, self.name(), self.service.clone())
    }
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

/// A demo dynamic capability: read-only tools, echo-style invoke, and a
/// shared started flag so tests can observe the lifecycle.
struct DemoCapability {
    manifest: CapabilityManifest,
    tool_names: Vec<String>,
    started: Arc<Mutex<bool>>,
}

impl DemoCapability {
    fn new(id: &str, lifecycle: CapabilityLifecycle, started: Arc<Mutex<bool>>) -> Self {
        Self {
            manifest: CapabilityManifest {
                id: id.into(),
                version: "1.0.0".into(),
                name: "demo".into(),
                summary: "demo capability".into(),
                status: CapabilityStatus::Experimental,
                provides: vec![agent_contracts::CapabilityKind::Tool],
                permissions: vec!["workspace:read".into()],
                requires: Vec::new(),
                tools: Vec::new(),
                lifecycle,
                transport: CapabilityTransport::Builtin,
            },
            tool_names: vec![format!("{id}.demo")],
            started,
        }
    }

    fn with_dependency(id: &str, dependency: &str) -> Self {
        let mut capability = Self::new(id, CapabilityLifecycle::Eager, Arc::new(Mutex::new(false)));
        capability.manifest.requires.push(dependency.into());
        capability
    }

    fn declared_stable(id: &str, transport: CapabilityTransport) -> Self {
        let mut capability = Self::new(id, CapabilityLifecycle::Lazy, Arc::new(Mutex::new(false)));
        capability.manifest.status = CapabilityStatus::Stable;
        capability.manifest.transport = transport;
        capability
    }

    /// A capability serving exactly the given tool names (for collision and
    /// activation tests).
    fn with_tool_names(id: &str, tool_names: &[&str], transport: CapabilityTransport) -> Self {
        let mut capability = Self::new(id, CapabilityLifecycle::Lazy, Arc::new(Mutex::new(false)));
        capability.manifest.transport = transport;
        capability.tool_names = tool_names.iter().map(|name| name.to_string()).collect();
        capability
    }
}

#[async_trait::async_trait]
impl Capability for DemoCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tool_names
            .iter()
            .map(|name| ToolSpec {
                name: name.clone(),
                description: "demo tool".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            })
            .collect()
    }
    async fn start(&self) -> AgentResult<()> {
        *self.started.lock().unwrap() = true;
        Ok(())
    }
    async fn invoke(
        &self,
        call: ToolCall,
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<ToolOutput> {
        Ok(ToolOutput {
            call_id: call.id,
            tool_name: call.name.clone(),
            ok: true,
            summary: "demo ran".into(),
            model_content: format!("demo handled {}", call.name),
            artifact_ref: None,
            context_action: None,
            metadata: json!({}),
        })
    }
}

async fn execute(dispatcher: Arc<dyn ToolDispatcher>, tool: &str) -> ToolOutput {
    dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c1".into(),
                name: tool.into(),
                arguments: json!({}),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn dynamic_capabilities_reach_the_model_and_route_calls() {
    let mut host = ModuleHost::new();
    let capability_registry = host.capability_registry();
    let started = Arc::new(Mutex::new(false));
    host.register_capability(Arc::new(DemoCapability::new(
        "demo",
        CapabilityLifecycle::Eager,
        started.clone(),
    )))
    .unwrap();

    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        capability_registry,
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Eager capabilities start with the host.
    assert!(
        *started.lock().unwrap(),
        "eager capability starts at host start"
    );

    // Registration alone keeps the capability's tools off the model
    // surface: they are catalog-visible but Available.
    let tools = host.registry().tool_provider().unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(
        !names.contains(&"demo.demo".to_string()),
        "unloaded capability tools must not be on the surface"
    );
    let catalog = dispatcher.catalog();
    let row = catalog
        .iter()
        .find(|entry| entry.name == "demo.demo")
        .expect("capability tools are discoverable in the catalog");
    assert_eq!(row.state, ToolLifecycle::Available);
    assert_eq!(row.owner, "demo");

    // Explicit load puts the capability's tools on the surface.
    dispatcher.load_tool("demo.demo").unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(names.contains(&"demo.demo".to_string()));

    // A call routed by name reaches the capability.
    let output = execute(tools, "demo.demo").await;
    assert!(output.ok);
    assert_eq!(output.model_content, "demo handled demo.demo");

    host.stop().await.unwrap();
}

#[tokio::test]
async fn capabilities_can_be_registered_mid_run_and_lazy_start_on_use() {
    let mut host = ModuleHost::new();
    let capability_registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        capability_registry,
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Mid-run registration: the tool is discoverable but not on the
    // surface until it is loaded.
    let started = Arc::new(Mutex::new(false));
    host.register_capability(Arc::new(DemoCapability::new(
        "late",
        CapabilityLifecycle::Lazy,
        started.clone(),
    )))
    .unwrap();

    let tools = host.registry().tool_provider().unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(
        !names.contains(&"late.demo".to_string()),
        "unloaded capability tools must not be on the surface"
    );
    assert!(
        !*started.lock().unwrap(),
        "a lazy capability is not started at registration"
    );

    // Load it, then the first invocation starts it (lazy lifecycle).
    dispatcher.load_tool("late.demo").unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(names.contains(&"late.demo".to_string()));
    let output = execute(tools, "late.demo").await;
    assert!(output.ok);
    assert!(
        *started.lock().unwrap(),
        "lazy capability starts on first use"
    );

    host.stop().await.unwrap();
}

#[tokio::test]
async fn capability_dependencies_are_validated() {
    let host = ModuleHost::new();
    let orphan = DemoCapability::with_dependency("orphan", "missing-capability");
    let error = host
        .register_capability(Arc::new(orphan))
        .expect_err("a capability with an unmet requirement must be rejected");
    assert!(
        error.to_string().contains("requires"),
        "the error must name the missing requirement: {error}"
    );

    // Duplicate ids are rejected too.
    let started = Arc::new(Mutex::new(false));
    let first = DemoCapability::new("dup", CapabilityLifecycle::Eager, started.clone());
    let second = DemoCapability::new("dup", CapabilityLifecycle::Eager, started);
    host.register_capability(Arc::new(first)).unwrap();
    let error = host
        .register_capability(Arc::new(second))
        .expect_err("duplicate capability ids must be rejected");
    assert!(error.to_string().contains("already registered"), "{error}");
}

#[tokio::test]
async fn missing_capability_lookup_fails_with_a_clear_error() {
    let host = ModuleHost::new();
    match host.registry().context_service() {
        Ok(_) => panic!("a missing capability must fail"),
        Err(error) => assert!(
            error.to_string().contains("context-service"),
            "the error must name the missing capability, got: {error}"
        ),
    }
    // Module claims are empty for an unregistered id.
    assert!(host.registry().claims(APPROVAL_POLICY).is_none());
}

#[tokio::test]
async fn external_capabilities_start_experimental_regardless_of_declared_status() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();

    // An out-of-process capability declares itself Stable; the platform must
    // not let the LLM promote its own module.
    registry
        .register(Arc::new(DemoCapability::declared_stable(
            "ext-llm-module",
            CapabilityTransport::Process {
                program: "plugin".into(),
            },
        )))
        .unwrap();
    assert_eq!(
        registry.status("ext-llm-module"),
        Some(CapabilityStatus::Experimental),
        "external capabilities enter at the bottom of the maturity ladder"
    );
    assert_eq!(
        registry.activation("ext-llm-module"),
        Some(CapabilityActivation::Disabled),
        "external capabilities enter disabled; enabling is an operator action"
    );

    // The catalog reports the effective status and activation, not the
    // declaration.
    let entry = registry
        .catalog()
        .into_iter()
        .find(|entry| entry.id == "ext-llm-module")
        .expect("registered capability must appear in the catalog");
    assert_eq!(entry.status, CapabilityStatus::Experimental);
    assert_eq!(entry.activation, CapabilityActivation::Disabled);
    assert_eq!(entry.tools, vec!["ext-llm-module.demo".to_string()]);

    // A disabled capability cannot put its tools on the model surface.
    let error = registry
        .load_tool("ext-llm-module.demo")
        .expect_err("loading a disabled capability's tools must fail");
    assert!(
        error.to_string().contains("disabled"),
        "the error must name the activation: {error}"
    );
}

#[tokio::test]
async fn trusted_builtin_capabilities_keep_their_declared_status() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    registry
        .register(Arc::new(DemoCapability::declared_stable(
            "trusted-core",
            CapabilityTransport::Builtin,
        )))
        .unwrap();
    assert_eq!(
        registry.status("trusted-core"),
        Some(CapabilityStatus::Stable),
        "the trusted core declares its own maturity"
    );
    assert_eq!(
        registry.activation("trusted-core"),
        Some(CapabilityActivation::Enabled),
        "the trusted in-process core is usable immediately"
    );
}

#[tokio::test]
async fn capabilities_cannot_shadow_reserved_core_tool_names() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    // The dispatcher claims the builtin catalog plus the control tools.
    let _dispatcher = CapabilityAwareDispatcher::new(Arc::new(StubTools), registry.clone());

    // Declaring a builtin tool name must be rejected at registration: the
    // route would otherwise be hijackable by declaration.
    let hijack = DemoCapability::with_tool_names(
        "shadow-builtin",
        &["fs.read"],
        CapabilityTransport::Builtin,
    );
    let error = registry
        .register(Arc::new(hijack))
        .expect_err("shadowing a builtin tool name must be rejected");
    assert!(
        error.to_string().contains("reserved"),
        "the error must name the reservation: {error}"
    );

    // Control tools are reserved too.
    let control = DemoCapability::with_tool_names(
        "shadow-control",
        &[agent_contracts::CAPABILITY_SEARCH],
        CapabilityTransport::Builtin,
    );
    let error = registry
        .register(Arc::new(control))
        .expect_err("shadowing a control tool must be rejected");
    assert!(error.to_string().contains("reserved"), "{error}");
}

#[tokio::test]
async fn capabilities_cannot_duplicate_each_others_tool_names() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    registry
        .register(Arc::new(DemoCapability::with_tool_names(
            "first",
            &["shared.tool"],
            CapabilityTransport::Builtin,
        )))
        .unwrap();
    let error = registry
        .register(Arc::new(DemoCapability::with_tool_names(
            "second",
            &["shared.tool"],
            CapabilityTransport::Builtin,
        )))
        .expect_err("a second owner of the same tool name must be rejected");
    assert!(
        error.to_string().contains("already owned"),
        "the error must name the existing owner: {error}"
    );
}

#[tokio::test]
async fn disabled_capabilities_cannot_load_or_run_until_enabled() {
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        registry.clone(),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // An external capability is registered Disabled: nothing may load or
    // run it.
    host.register_capability(Arc::new(DemoCapability::with_tool_names(
        "ext-gated",
        &["ext-gated.run"],
        CapabilityTransport::Process {
            program: "plugin".into(),
        },
    )))
    .unwrap();

    let tools = host.registry().tool_provider().unwrap();
    let error = dispatcher
        .load_tool("ext-gated.run")
        .expect_err("loading a disabled capability must fail");
    assert!(error.to_string().contains("disabled"), "{error}");

    // Enabling makes it loadable and runnable.
    registry.enable("ext-gated").unwrap();
    dispatcher.load_tool("ext-gated.run").unwrap();
    let output = execute(tools, "ext-gated.run").await;
    assert!(output.ok);
    assert_eq!(output.model_content, "demo handled ext-gated.run");

    host.stop().await.unwrap();
}

#[tokio::test]
async fn activation_can_be_disabled_and_quarantined_after_use() {
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        registry.clone(),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Trusted builtin capability starts Enabled and usable.
    host.register_capability(Arc::new(DemoCapability::with_tool_names(
        "flaky",
        &["flaky.run"],
        CapabilityTransport::Builtin,
    )))
    .unwrap();
    let tools = host.registry().tool_provider().unwrap();
    dispatcher.load_tool("flaky.run").unwrap();
    assert!(execute(tools.clone(), "flaky.run").await.ok);

    // After misbehavior the operator disables it: tools leave the surface
    // and calls are blocked at the gate.
    registry.disable("flaky").unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(
        !names.contains(&"flaky.run".to_string()),
        "a disabled capability must leave the model surface"
    );
    let error = execute_raw(tools.clone(), "flaky.run").await;
    assert!(
        error.contains("disabled"),
        "invoking a disabled capability must fail at the gate: {error}"
    );

    // Quarantine is the same gate, with its own label.
    registry.enable("flaky").unwrap();
    registry.quarantine("flaky").unwrap();
    let error = execute_raw(tools, "flaky.run").await;
    assert!(error.contains("quarantined"), "{error}");

    host.stop().await.unwrap();
}

/// Execute a tool and return the error text, for asserting on the gate.
async fn execute_raw(dispatcher: Arc<dyn ToolDispatcher>, tool: &str) -> String {
    dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c1".into(),
                name: tool.into(),
                arguments: json!({}),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err()
        .to_string()
}
