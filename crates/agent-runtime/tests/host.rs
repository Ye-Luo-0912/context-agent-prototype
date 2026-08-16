//! Module host tests: typed capability registration and lookup, duplicate
//! rejection, and lifecycle ordering.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use agent_contracts::{
    AgentError, AgentResult, ApprovalGate, CancellationToken, Capability, CapabilityActivation,
    CapabilityInvocationContext, CapabilityLifecycle, CapabilityManifest, CapabilityOutcome,
    CapabilityStatus, CapabilityTransport, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RunId, ScopeId, ScopeKind, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolLifecycle, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_runtime::{
    APPROVAL_POLICY, CapabilityAwareDispatcher, CapabilityId, CapabilityRunState, ContextModule,
    ModelModule, Module, ModuleHost, ServiceRegistry, ToolModule,
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
            materialization_id: 0,
            focus: None,
            task: None,
            items: Vec::new(),
            external: agent_contracts::ContextMapView::default(),
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
            risk: agent_contracts::ToolRisk::ReadOnly,
        }]
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(agent_contracts::AgentError::Tool("stub".into()))
    }
}

/// A base dispatcher that deliberately relies on the ToolDispatcher
/// fail-closed default: its large schema is mandatory because it does not
/// opt into round-local omission.
#[derive(Debug)]
struct RequiredLargeTools;

#[async_trait::async_trait]
impl ToolDispatcher for RequiredLargeTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "required.large".into(),
            description: "x".repeat(20_000),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }]
    }

    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
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
        _cancel: &agent_contracts::CancellationToken,
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

    /// A capability with an explicit declared authority (permissions + tool
    /// schemas), for the registration-validation tests.
    fn with_authority(id: &str, permissions: &[&str], tools: Vec<ToolSpec>) -> Self {
        let mut capability = Self::new(id, CapabilityLifecycle::Lazy, Arc::new(Mutex::new(false)));
        capability.manifest.permissions = permissions.iter().map(|p| p.to_string()).collect();
        capability.manifest.tools = tools;
        capability
    }
}

#[async_trait::async_trait]
impl Capability for DemoCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn tool_specs(&self) -> Vec<ToolSpec> {
        // Declared schemas win (a real capability can serve custom
        // schemas); the plain name-generated shapes are the fallback.
        if !self.manifest.tools.is_empty() {
            return self.manifest.tools.clone();
        }
        self.tool_names
            .iter()
            .map(|name| ToolSpec {
                name: name.clone(),
                description: "demo tool".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
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
    ) -> AgentResult<CapabilityOutcome> {
        Ok(CapabilityOutcome::Value(ToolOutput {
            call_id: call.id,
            tool_name: call.name.clone(),
            ok: true,
            summary: "demo ran".into(),
            model_content: format!("demo handled {}", call.name),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

/// A capability that captures the invocation context it received, so the
/// permission tests can assert exactly which handles were granted — and
/// prove that a handle the manifest never declared is absent by
/// construction, not by trust.
struct ContextCapturingCapability {
    manifest: CapabilityManifest,
    captured: Arc<Mutex<Option<CapabilityInvocationContext>>>,
}

impl ContextCapturingCapability {
    fn with_permissions(id: &str, permissions: &[&str]) -> Self {
        Self {
            manifest: CapabilityManifest {
                id: id.into(),
                version: "1.0.0".into(),
                name: id.into(),
                summary: "records its invocation context".into(),
                status: CapabilityStatus::Experimental,
                provides: vec![agent_contracts::CapabilityKind::Tool],
                permissions: permissions.iter().map(|p| p.to_string()).collect(),
                requires: Vec::new(),
                tools: Vec::new(),
                lifecycle: CapabilityLifecycle::Lazy,
                transport: CapabilityTransport::Builtin,
            },
            captured: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait::async_trait]
impl Capability for ContextCapturingCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn tool_specs(&self) -> Vec<ToolSpec> {
        // The risk is *derived* from the declared authority, exactly like
        // the runtime's own validation: a capability that can write the
        // workspace must never surface a ReadOnly tool (ReadOnly
        // auto-allows at the approval gate).
        let risk = if self
            .manifest
            .permissions
            .iter()
            .any(|p| p == "workspace:write" || p == "process:run")
        {
            ToolRisk::WorkspaceWrite
        } else {
            ToolRisk::ReadOnly
        };
        vec![ToolSpec {
            name: format!("{}.run", self.manifest.id),
            description: "recording tool".into(),
            input_schema: json!({"type": "object"}),
            risk,
            output_budget: None,
        }]
    }
    async fn start(&self) -> AgentResult<()> {
        Ok(())
    }
    async fn invoke(
        &self,
        call: ToolCall,
        ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        *self.captured.lock().unwrap() = Some(ctx);
        Ok(CapabilityOutcome::Value(ToolOutput {
            call_id: call.id,
            tool_name: call.name.clone(),
            ok: true,
            summary: "recorded".into(),
            model_content: format!("recorded {}", call.name),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

async fn execute(dispatcher: Arc<dyn ToolDispatcher>, tool: &str) -> ToolOutput {
    let run_id = RunId::new();
    let call = ToolCall {
        id: "c1".into(),
        name: tool.into(),
        arguments: json!({}),
    };
    // This helper deliberately exercises the dispatcher without spawning a
    // RuntimeActor. Mirror the Core-owned dispatch shape for effectful test
    // tools; production callers receive this context only after Core has
    // persisted the operation/effect identity.
    let effect_context = dispatcher.inspect_tool(tool).and_then(|spec| {
        (spec.risk != ToolRisk::ReadOnly).then(|| agent_contracts::OperationEffectContext {
            identity: agent_contracts::ToolOperationIdentity {
                run_id,
                task_id: None,
                turn_id: agent_contracts::TurnId::new(),
                scope_id: None,
                operation_id: agent_contracts::OperationId::new(),
                generation: 1,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                argument_digest: agent_contracts::ArgumentDigest::from_json(&call.arguments),
            },
            effect_id: agent_contracts::EffectId::new(),
        })
    });
    let outcome = dispatcher
        .execute(ToolExecutionRequest {
            run_id,
            call,
            effect_context,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    match outcome {
        ToolOutcome::Value(output) => output,
        ToolOutcome::PreparedEffect { .. }
        | ToolOutcome::RuntimeDirective { .. }
        | ToolOutcome::EngineQuery { .. } => panic!("test dispatcher returns plain values"),
    }
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
async fn capability_authority_is_derived_and_validated_at_registration() {
    let host = ModuleHost::new();

    // An id outside the conservative grammar is a path/route injection
    // risk and is refused before anything else.
    let bad_id = DemoCapability::with_authority("../escape", &["workspace:read"], Vec::new());
    let error = host
        .register_capability(Arc::new(bad_id))
        .expect_err("a path-unsafe id must be rejected");
    assert!(
        error.to_string().contains("capability id"),
        "the refusal must name the id rule: {error}"
    );

    // Self-declared ReadOnly on a workspace-write capability: ReadOnly
    // auto-allows at the approval gate, so a mutating capability must
    // never self-declare it — the risk is derived from the authority.
    let write_tool = DemoCapability::with_authority("write-tool", &["workspace:write"], Vec::new());
    let error = host
        .register_capability(Arc::new(write_tool))
        .expect_err("a write-permissioned capability must not self-declare ReadOnly");
    assert!(
        error.to_string().contains("ReadOnly"),
        "the refusal must name the self-declared risk: {error}"
    );

    // A tool whose risk exceeds its grant is refused: WorkspaceWrite
    // without the permission, ProcessExecution without the permission.
    let over_granted = DemoCapability::with_authority(
        "over-granted",
        &["workspace:read"],
        vec![ToolSpec {
            name: "over-granted.run".into(),
            description: "writes".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
        }],
    );
    let error = host
        .register_capability(Arc::new(over_granted))
        .expect_err("a tool risk may not exceed the declared grant");
    assert!(
        error.to_string().contains("workspace:write"),
        "the refusal must name the missing grant: {error}"
    );

    // A process capability may declare workspace:write now that the wire
    // effect broker exists: the child stages structured wire effects and
    // the adapter commits them through the confined workspace handle behind
    // the generation fence — the child itself never writes.
    let process_write = DemoCapability::with_authority(
        "proc-write",
        &["workspace:write"],
        vec![ToolSpec {
            name: "proc-write.run".into(),
            description: "writes".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
        }],
    );
    let process_write = {
        let mut capability = process_write;
        capability.manifest.transport = CapabilityTransport::Process {
            program: "x".into(),
        };
        capability
    };
    host.register_capability(Arc::new(process_write))
        .expect("a process capability may declare workspace:write through the wire effect broker");

    // A read-only process capability is fine: read authority, ReadOnly
    // tool, no broker needed.
    let process_read = DemoCapability::with_authority("proc-read", &["workspace:read"], Vec::new());
    let process_read = {
        let mut capability = process_read;
        capability.manifest.transport = CapabilityTransport::Process {
            program: "x".into(),
        };
        capability
    };
    host.register_capability(Arc::new(process_read))
        .expect("a read-only process capability is allowed");
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
        &[agent_contracts::CAPABILITY_MANAGE],
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
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err()
        .to_string()
}

/// A module that records lifecycle calls and can be told to fail its start
/// or stop.
struct ScriptedModule {
    name: &'static str,
    log: Arc<Mutex<Vec<String>>>,
    fail_start: bool,
    fail_stop: bool,
}

#[async_trait::async_trait]
impl Module for ScriptedModule {
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
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        if self.fail_start {
            return Err(agent_contracts::AgentError::Internal(format!(
                "{} start failed",
                self.name
            )));
        }
        Ok(())
    }
    async fn stop(&self) -> AgentResult<()> {
        self.log.lock().unwrap().push(format!("stop:{}", self.name));
        if self.fail_stop {
            return Err(agent_contracts::AgentError::Internal(format!(
                "{} stop failed",
                self.name
            )));
        }
        Ok(())
    }
}

/// A capability that records its stop (and can fail it).
struct RecordingCapability {
    log: Arc<Mutex<Vec<String>>>,
    fail_stop: bool,
}

impl RecordingCapability {
    fn new(log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            log,
            fail_stop: false,
        }
    }
    fn failing_stop(log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            log,
            fail_stop: true,
        }
    }
}

#[async_trait::async_trait]
impl Capability for RecordingCapability {
    fn manifest(&self) -> &CapabilityManifest {
        static MANIFEST: std::sync::OnceLock<CapabilityManifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| CapabilityManifest {
            id: "recording".into(),
            version: "1.0.0".into(),
            name: "recording".into(),
            summary: "records lifecycle".into(),
            status: CapabilityStatus::Experimental,
            provides: vec![agent_contracts::CapabilityKind::Tool],
            permissions: Vec::new(),
            requires: Vec::new(),
            tools: Vec::new(),
            lifecycle: CapabilityLifecycle::Eager,
            transport: CapabilityTransport::Builtin,
        })
    }
    async fn start(&self) -> AgentResult<()> {
        self.log.lock().unwrap().push("start:capability".into());
        Ok(())
    }
    async fn invoke(
        &self,
        call: ToolCall,
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        Ok(CapabilityOutcome::Value(ToolOutput {
            call_id: call.id,
            tool_name: call.name,
            ok: true,
            summary: "ok".into(),
            model_content: "ok".into(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
    async fn stop(&self) -> AgentResult<()> {
        self.log.lock().unwrap().push("stop:capability".into());
        if self.fail_stop {
            return Err(agent_contracts::AgentError::Internal(
                "capability stop failed".into(),
            ));
        }
        Ok(())
    }
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
async fn snapshot_generation_tracks_dynamic_capability_changes() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = CapabilityAwareDispatcher::new(Arc::new(StubTools), registry.clone());

    let before = dispatcher.snapshot().generation;
    let capability = DemoCapability::new(
        "gen-a",
        CapabilityLifecycle::Lazy,
        Arc::new(Mutex::new(false)),
    );
    registry.register(Arc::new(capability)).unwrap();
    let after_register = dispatcher.snapshot().generation;
    assert!(
        after_register > before,
        "registration must bump the surface generation"
    );

    registry.load_tool("gen-a.demo").unwrap();
    let after_load = dispatcher.snapshot().generation;
    assert!(
        after_load > after_register,
        "loading must bump the generation"
    );

    registry.unload_tool("gen-a.demo").unwrap();
    let after_unload = dispatcher.snapshot().generation;
    assert!(
        after_unload > after_load,
        "unloading must bump the generation"
    );

    registry.enable("gen-a").unwrap();
    let after_activate = dispatcher.snapshot().generation;
    assert!(
        after_activate > after_unload,
        "activation changes must bump the generation"
    );
}

#[test]
fn snapshot_never_silently_trims_a_fail_closed_required_schema() {
    let dispatcher = CapabilityAwareDispatcher::new(
        Arc::new(RequiredLargeTools),
        Arc::new(agent_runtime::CapabilityRegistry::new()),
    );

    let snapshot = dispatcher.snapshot();
    assert!(
        snapshot
            .specs
            .iter()
            .any(|spec| spec.name == "required.large"),
        "the initial schema cap must preserve fail-closed schemas"
    );
    assert!(
        agent_runtime::approx_layer_tokens(&snapshot.specs)
            > agent_runtime::budget::MAX_TOOL_SURFACE_TOKENS,
        "an oversized mandatory set remains visible so the actor can fail explicitly"
    );
}

#[tokio::test]
async fn unified_search_pages_and_spills_to_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    for id in ["ext.a", "ext.b", "ext.c"] {
        registry
            .register(Arc::new(DemoCapability::new(
                id,
                CapabilityLifecycle::Lazy,
                Arc::new(Mutex::new(false)),
            )))
            .unwrap();
    }
    let dispatcher = CapabilityAwareDispatcher::with_workspace(
        Arc::new(StubTools),
        registry.clone(),
        Some(workspace.clone()),
    );

    let output = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c".into(),
                name: agent_contracts::CAPABILITY_MANAGE.into(),
                arguments: json!({"op": "search", "limit": 2}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    let output = match output {
        ToolOutcome::Value(output) => output,
        other => panic!("capability.manage search must return a plain value, got {other:?}"),
    };
    assert!(output.ok);
    assert!(
        output.artifact_ref.is_some(),
        "a catalog larger than the page must spill to an artifact"
    );
    assert!(
        output.model_content.lines().count() <= 2,
        "the model must only see the bounded page: {}",
        output.model_content
    );
    assert_eq!(output.metadata["has_more"], true);
    assert_eq!(
        output.metadata["total"], 4,
        "fs.read + three capability tools"
    );
}

#[tokio::test]
async fn unified_search_matches_description_and_reports_not_found() {
    let dispatcher = CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        Arc::new(agent_runtime::CapabilityRegistry::new()),
    );
    let before: Vec<_> = dispatcher
        .snapshot()
        .specs
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let search = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c".into(),
                name: agent_contracts::CAPABILITY_MANAGE.into(),
                arguments: json!({"op": "search", "query": "STUB BUILTIN"}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    let search = match search {
        ToolOutcome::Value(output) => output,
        other => panic!("expected value, got {other:?}"),
    };
    assert!(search.ok);
    assert!(
        search.model_content.contains("fs.read"),
        "capability search must match description case-insensitively: {}",
        search.model_content
    );
    let after: Vec<_> = dispatcher
        .snapshot()
        .specs
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert_eq!(
        before, after,
        "search must not load a tool onto the surface"
    );

    let inspect = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c2".into(),
                name: agent_contracts::CAPABILITY_MANAGE.into(),
                arguments: json!({"op": "inspect", "name": "missing.tool"}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    let inspect = match inspect {
        ToolOutcome::Value(output) => output,
        other => panic!("expected value, got {other:?}"),
    };
    assert!(!inspect.ok);
    assert_eq!(inspect.metadata["miss"], "not_found");
}

/// A capability whose `start()` is slow on purpose and fails on the first
/// attempt, so lifecycle tests can exercise the race window and the
/// Failed -> retry path.
struct InstrumentedCapability {
    manifest: CapabilityManifest,
    starts: Arc<AtomicUsize>,
    fail_first: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Capability for InstrumentedCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn start(&self) -> AgentResult<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        // Give a concurrent caller time to observe the pre-start state.
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if self.fail_first.swap(false, Ordering::SeqCst) {
            return Err(AgentError::Internal("instrumented start failure".into()));
        }
        Ok(())
    }
    async fn invoke(
        &self,
        call: ToolCall,
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        Ok(CapabilityOutcome::Value(ToolOutput {
            call_id: call.id,
            tool_name: call.name.clone(),
            ok: true,
            summary: "ran".into(),
            model_content: "ran".into(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

fn instrumented_manifest(id: &str) -> CapabilityManifest {
    CapabilityManifest {
        id: id.into(),
        version: "1.0.0".into(),
        name: id.into(),
        summary: "instrumented".into(),
        status: CapabilityStatus::Experimental,
        provides: vec![agent_contracts::CapabilityKind::Tool],
        permissions: vec!["workspace:read".into()],
        requires: Vec::new(),
        tools: Vec::new(),
        lifecycle: CapabilityLifecycle::Lazy,
        transport: CapabilityTransport::Builtin,
    }
}

#[tokio::test]
async fn concurrent_ensure_started_serializes_to_a_single_start() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    let starts = Arc::new(AtomicUsize::new(0));
    host.register_capability(Arc::new(InstrumentedCapability {
        manifest: instrumented_manifest("slow"),
        starts: starts.clone(),
        fail_first: Arc::new(AtomicBool::new(false)),
    }))
    .unwrap();

    // Both callers race the same transition; the per-capability lifecycle
    // lock must collapse them into exactly one `start()`.
    let (a, b) = tokio::join!(
        registry.ensure_started("slow"),
        registry.ensure_started("slow"),
    );
    a.unwrap();
    b.unwrap();

    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "concurrent ensure_started calls must produce exactly one start()"
    );
    let entry = registry
        .catalog()
        .into_iter()
        .find(|entry| entry.id == "slow")
        .expect("catalog must list the capability");
    assert_eq!(
        entry.run_state,
        CapabilityRunState::Started,
        "a successful start must leave the capability Started"
    );
}

#[tokio::test]
async fn failed_start_is_observable_and_a_later_start_retries() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    let starts = Arc::new(AtomicUsize::new(0));
    host.register_capability(Arc::new(InstrumentedCapability {
        manifest: instrumented_manifest("flaky"),
        starts: starts.clone(),
        fail_first: Arc::new(AtomicBool::new(true)),
    }))
    .unwrap();

    let first = registry.ensure_started("flaky").await;
    assert!(first.is_err(), "the instrumented first start must fail");
    assert_eq!(
        registry
            .catalog()
            .into_iter()
            .find(|entry| entry.id == "flaky")
            .map(|entry| entry.run_state),
        Some(CapabilityRunState::Failed),
        "a failed start must be observable as Failed"
    );

    // The failure is not sticky: a later start retries the transition.
    registry.ensure_started("flaky").await.unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(
        registry
            .catalog()
            .into_iter()
            .find(|entry| entry.id == "flaky")
            .map(|entry| entry.run_state),
        Some(CapabilityRunState::Started)
    );
}

#[tokio::test]
async fn catalog_rows_are_cached_and_invalidate_on_surface_changes() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    registry
        .register(Arc::new(DemoCapability::new(
            "cache-demo",
            CapabilityLifecycle::Lazy,
            Arc::new(Mutex::new(false)),
        )))
        .unwrap();

    // An unchanged catalog serves the cached rows: repeated discovery
    // reads must not rebuild the derived metadata per call.
    let first = registry.catalog_rows();
    let second = registry.catalog_rows();
    assert!(
        Arc::ptr_eq(&first, &second),
        "unchanged catalog must serve the cached rows"
    );

    // A surface change (load) invalidates the cache and the fresh rows
    // reflect the new lifecycle state.
    registry.load_tool("cache-demo.demo").unwrap();
    let third = registry.catalog_rows();
    assert!(
        !Arc::ptr_eq(&first, &third),
        "a load must invalidate the cache"
    );
    assert!(
        third
            .iter()
            .any(|row| row.name == "cache-demo.demo" && row.state == ToolLifecycle::Loaded),
        "a loaded tool must report Loaded in the fresh rows"
    );

    // An executing tool flips its row to Active; the cache must not serve
    // a stale Loaded state across a call boundary.
    registry.mark_active("cache-demo.demo");
    let fourth = registry.catalog_rows();
    assert!(
        !Arc::ptr_eq(&third, &fourth),
        "an active mark must invalidate the cache"
    );
    assert!(
        fourth
            .iter()
            .any(|row| row.name == "cache-demo.demo" && row.state == ToolLifecycle::Active),
        "an executing tool must report Active in the fresh rows"
    );
    registry.mark_idle("cache-demo.demo");
    let fifth = registry.catalog_rows();
    assert!(
        !Arc::ptr_eq(&fourth, &fifth),
        "an idle mark must invalidate the cache"
    );
    assert!(
        fifth
            .iter()
            .any(|row| row.name == "cache-demo.demo" && row.state == ToolLifecycle::Loaded),
        "an idle tool returns to Loaded in the fresh rows"
    );
}

/// The permission Core grants nothing undeclared: the runtime builds the
/// invocation context from the manifest's declared permissions alone, so a
/// capability that never declared a workspace permission receives no
/// workspace handle at all, and one that declared only reads cannot write —
/// blocked by construction, not by trust.
#[tokio::test]
async fn undeclared_permissions_receive_no_handle() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();

    // Four capabilities with different declared grants, plus one declaring
    // an unknown permission string — which the registry now refuses up
    // front: unknown access is denied by refusing the declaration.
    let no_ws = Arc::new(ContextCapturingCapability::with_permissions(
        "no-ws",
        &[agent_contracts::RUNTIME_CONTEXT_CONTROL],
    ));
    let no_ws_captured = no_ws.captured.clone();
    let read_only = Arc::new(ContextCapturingCapability::with_permissions(
        "read-only",
        &["workspace:read"],
    ));
    let read_only_captured = read_only.captured.clone();
    let write_ws = Arc::new(ContextCapturingCapability::with_permissions(
        "write-ws",
        &["workspace:write"],
    ));
    let write_ws_captured = write_ws.captured.clone();
    let unknown =
        ContextCapturingCapability::with_permissions("unknown-perm", &["totally-made-up:perm"]);
    let registration = host.register_capability(Arc::new(unknown));
    assert!(
        registration.is_err(),
        "an unknown permission string must be refused at registration"
    );
    assert!(
        registration
            .unwrap_err()
            .to_string()
            .contains("unknown permission"),
        "the refusal must name the unknown permission"
    );
    for capability in [no_ws, read_only, write_ws] {
        host.register_capability(capability).unwrap();
    }

    let dispatcher = Arc::new(CapabilityAwareDispatcher::with_workspace(
        Arc::new(StubTools),
        registry.clone(),
        Some(workspace.clone()),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Builtin capabilities are Enabled on registration, so the full model
    // path works: load each tool, call it, inspect what it received.
    let tools = host.registry().tool_provider().unwrap();
    for id in ["no-ws", "read-only", "write-ws"] {
        registry.load_tool(&format!("{id}.run")).unwrap();
        let output = execute(tools.clone(), &format!("{id}.run")).await;
        assert!(output.ok, "{id}: the recording call must succeed");
    }

    // 1. No workspace permission declared -> no workspace handle at all.
    let ctx = no_ws_captured.lock().unwrap().take().unwrap();
    assert_eq!(
        ctx.granted_permissions,
        [agent_contracts::RUNTIME_CONTEXT_CONTROL]
    );
    assert!(
        ctx.workspace.is_none(),
        "a capability that declared no workspace permission must receive no workspace handle"
    );
    assert!(ctx.artifacts.is_none(), "no artifact permission declared");

    // 2. Write declared -> a staged-only handle: the direct write path is
    //    refused (a mutation applied during invoke would bypass the
    //    generation fence, cancellation and effect rollback), and the
    //    mutation must be prepared as an Effect and committed by the core.
    //    This lands the file the read-only capability will read back below.
    let ctx = write_ws_captured.lock().unwrap().take().unwrap();
    assert_eq!(ctx.granted_permissions, ["workspace:write"]);
    let handle = ctx
        .workspace
        .expect("workspace:write must receive a handle");
    let error = handle
        .write("granted.txt", b"x")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("must be staged"),
        "the direct write must be refused and name the staged path: {error}"
    );
    let effect = handle
        .prepare_write("granted.txt", b"granted content")
        .await
        .expect("prepare_write must stage the mutation");
    let receipt = effect.commit().await;
    assert!(
        matches!(
            &receipt,
            agent_contracts::EffectReceipt::Applied {
                durability: agent_contracts::EffectDurability::Durable,
                ..
            }
        ),
        "the staged effect commits durably: {receipt:?}"
    );
    assert_eq!(
        handle.read("granted.txt").await.unwrap(),
        b"granted content"
    );
    let bounded = handle.read_bounded("granted.txt", 7).await.unwrap();
    assert_eq!(bounded.content, b"granted");
    assert_eq!(bounded.byte_len, b"granted content".len() as u64);
    assert!(bounded.truncated);

    // 3. Read-only declared -> a read-only handle: reads work, both write
    //    paths are blocked with an error naming the missing grant.
    let ctx = read_only_captured.lock().unwrap().take().unwrap();
    assert_eq!(ctx.granted_permissions, ["workspace:read"]);
    let handle = ctx.workspace.expect("workspace:read must receive a handle");
    assert_eq!(
        handle.read("granted.txt").await.unwrap(),
        b"granted content",
        "the read-only handle must still read the workspace"
    );
    assert_eq!(
        handle
            .read_bounded("granted.txt", 1024)
            .await
            .unwrap()
            .content,
        b"granted content",
        "the read-only wrapper must preserve bounded-read access"
    );
    let error = handle.write("x.txt", b"x").await.unwrap_err().to_string();
    assert!(
        error.contains("workspace:write was not granted"),
        "the write refusal must name the grant: {error}"
    );
    let error = match handle.prepare_write("x.txt", b"x").await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("prepare_write must be refused without the grant"),
    };
    assert!(
        error.contains("workspace:write was not granted"),
        "the staged-write refusal must name the grant: {error}"
    );

    // 4. An unknown permission string is refused at registration (asserted
    //    above) — unknown access is denied by refusing the declaration,
    //    before any handle could ever be granted.

    host.stop().await.unwrap();
}
