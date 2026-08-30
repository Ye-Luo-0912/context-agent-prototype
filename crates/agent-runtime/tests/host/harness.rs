use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use agent_contracts::{
    AgentError, AgentResult, ApprovalGate, CancellationToken, Capability,
    CapabilityInvocationContext, CapabilityLifecycle, CapabilityManifest, CapabilityOutcome,
    CapabilityStatus, CapabilityTransport, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RunId, ScopeId, ScopeKind, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolLifecycle, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_runtime::{CapabilityId, Module, ServiceRegistry};
use serde_json::json;

#[derive(Debug)]
pub(crate) struct StubContextEngine;

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
            foreground: Vec::new(),
            required_item_ids: Vec::new(),
            required_misses: Default::default(),
            optional_misses: Default::default(),
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
pub(crate) struct StubModel;

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
pub(crate) struct StubTools;

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
            roles: Vec::new(),
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
pub(crate) struct RequiredLargeTools;

#[async_trait::async_trait]
impl ToolDispatcher for RequiredLargeTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "required.large".into(),
            description: "x".repeat(20_000),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }

    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(agent_contracts::AgentError::Tool("stub".into()))
    }
}

#[derive(Debug)]
pub(crate) struct StubApproval;

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

/// A capability id an external crate could publish, beyond the core set.
pub(crate) const CUSTOM_SERVICE: CapabilityId = CapabilityId("custom-service");

/// A typed service defined outside the core module set.
#[derive(Debug)]
pub(crate) struct CustomService;

/// A module written like an external crate: it publishes a typed service
/// through the public `ServiceRegistry::register` path.
pub(crate) struct ExternalModule {
    pub(crate) service: Arc<CustomService>,
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

/// A demo dynamic capability: read-only tools, echo-style invoke, and a
/// shared started flag so tests can observe the lifecycle.
pub(crate) struct DemoCapability {
    pub(crate) manifest: CapabilityManifest,
    pub(crate) tool_names: Vec<String>,
    pub(crate) started: Arc<Mutex<bool>>,
}

impl DemoCapability {
    pub(crate) fn new(id: &str, lifecycle: CapabilityLifecycle, started: Arc<Mutex<bool>>) -> Self {
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
                sandbox_profile: Default::default(),
            },
            tool_names: vec![format!("{id}.demo")],
            started,
        }
    }

    pub(crate) fn with_dependency(id: &str, dependency: &str) -> Self {
        let mut capability = Self::new(id, CapabilityLifecycle::Eager, Arc::new(Mutex::new(false)));
        capability.manifest.requires.push(dependency.into());
        capability
    }

    pub(crate) fn declared_stable(id: &str, transport: CapabilityTransport) -> Self {
        let mut capability = Self::new(id, CapabilityLifecycle::Lazy, Arc::new(Mutex::new(false)));
        capability.manifest.status = CapabilityStatus::Stable;
        capability.manifest.transport = transport;
        capability
    }

    /// A capability serving exactly the given tool names (for collision and
    /// activation tests).
    pub(crate) fn with_tool_names(
        id: &str,
        tool_names: &[&str],
        transport: CapabilityTransport,
    ) -> Self {
        let mut capability = Self::new(id, CapabilityLifecycle::Lazy, Arc::new(Mutex::new(false)));
        capability.manifest.transport = transport;
        capability.tool_names = tool_names.iter().map(|name| name.to_string()).collect();
        capability
    }

    /// A capability with an explicit declared authority (permissions + tool
    /// schemas), for the registration-validation tests.
    pub(crate) fn with_authority(id: &str, permissions: &[&str], tools: Vec<ToolSpec>) -> Self {
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
                roles: Vec::new(),
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
pub(crate) struct ContextCapturingCapability {
    pub(crate) manifest: CapabilityManifest,
    pub(crate) captured: Arc<Mutex<Option<CapabilityInvocationContext>>>,
}

impl ContextCapturingCapability {
    pub(crate) fn with_permissions(id: &str, permissions: &[&str]) -> Self {
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
                sandbox_profile: Default::default(),
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
            roles: Vec::new(),
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

pub(crate) async fn execute(dispatcher: Arc<dyn ToolDispatcher>, tool: &str) -> ToolOutput {
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

/// Execute a tool and return the error text, for asserting on the gate.
pub(crate) async fn execute_raw(dispatcher: Arc<dyn ToolDispatcher>, tool: &str) -> String {
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
pub(crate) struct ScriptedModule {
    pub(crate) name: &'static str,
    pub(crate) log: Arc<Mutex<Vec<String>>>,
    pub(crate) fail_start: bool,
    pub(crate) fail_stop: bool,
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
pub(crate) struct RecordingCapability {
    pub(crate) log: Arc<Mutex<Vec<String>>>,
    pub(crate) fail_stop: bool,
}

impl RecordingCapability {
    pub(crate) fn new(log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            log,
            fail_stop: false,
        }
    }
    pub(crate) fn failing_stop(log: Arc<Mutex<Vec<String>>>) -> Self {
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
            sandbox_profile: Default::default(),
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

/// A capability whose `start()` is slow on purpose and fails on the first
/// attempt, so lifecycle tests can exercise the race window and the
/// Failed -> retry path.
pub(crate) struct InstrumentedCapability {
    pub(crate) manifest: CapabilityManifest,
    pub(crate) starts: Arc<AtomicUsize>,
    pub(crate) fail_first: Arc<AtomicBool>,
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

pub(crate) fn instrumented_manifest(id: &str) -> CapabilityManifest {
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
        sandbox_profile: Default::default(),
    }
}
