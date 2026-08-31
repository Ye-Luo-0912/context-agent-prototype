//! The module host: a uniform lifecycle (register, validate, start, stop)
//! over two extension planes. The trusted core plane publishes typed
//! capabilities into the registry (no universal `handle_event` — consumers
//! look services up by type). The dynamic plane accepts `Capability`s at
//! composition time or mid-run; their tools join the runtime's tool
//! provider so the model can call them.

use std::{any::Any, collections::HashMap, sync::Arc};

use agent_contracts::{
    AgentError, AgentResult, ApprovalGate, Capability, ContextEngine, EventJournal, ModelTransport,
    ToolDispatcher,
};
use agent_workspace::Workspace;
use async_trait::async_trait;

use crate::capability::CapabilityRegistry;

/// Typed capability ids the module host knows about. Values in the registry
/// are stored and looked up through typed accessors, so callers never pass
/// raw strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapabilityId(pub &'static str);

pub const CONTEXT_SERVICE: CapabilityId = CapabilityId("context-service");
pub const MODEL_PROVIDER: CapabilityId = CapabilityId("model-provider");
pub const TOOL_PROVIDER: CapabilityId = CapabilityId("tool-provider");
pub const APPROVAL_POLICY: CapabilityId = CapabilityId("approval-policy");
pub const EVENT_STORE: CapabilityId = CapabilityId("event-store");
pub const ARTIFACT_STORE: CapabilityId = CapabilityId("artifact-store");

/// A module publishes typed capabilities and follows a uniform lifecycle.
/// `register` runs at composition time (capability publication), `start` and
/// `stop` bracket the run, in registration order and reverse order.
#[async_trait]
pub trait Module: Send + Sync {
    fn name(&self) -> &'static str;
    /// The capability ids this module publishes (validated at register time:
    /// no two modules may claim the same id).
    fn capabilities(&self) -> Vec<CapabilityId>;
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()>;
    async fn start(&self) -> AgentResult<()> {
        Ok(())
    }
    async fn stop(&self) -> AgentResult<()> {
        Ok(())
    }
}

/// Typed service registry. Values are stored as `Arc<T>` (T may be a trait
/// object) and returned through typed accessors, so a consumer gets exactly
/// the capability it asked for or a clear error.
#[derive(Default)]
pub struct ServiceRegistry {
    claims: HashMap<CapabilityId, String>,
    services: HashMap<CapabilityId, Box<dyn Any + Send + Sync>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Which module claimed a capability id, if any.
    pub fn claims(&self, id: CapabilityId) -> Option<&str> {
        self.claims.get(&id).map(String::as_str)
    }

    /// Publish a typed service. Public so modules from external crates can
    /// extend the trusted core plane with their own typed capabilities.
    pub fn register<T: Send + Sync + 'static + ?Sized>(
        &mut self,
        id: CapabilityId,
        module: &str,
        service: Arc<T>,
    ) -> AgentResult<()> {
        if let Some(claimed_by) = self.claims.get(&id) {
            return Err(AgentError::InvalidRequest(format!(
                "capability {id:?} already claimed by module '{claimed_by}'"
            )));
        }
        self.claims.insert(id, module.to_string());
        self.services.insert(id, Box::new(service));
        Ok(())
    }

    /// Look a typed service up. Public so consumers outside the crate can
    /// retrieve capabilities they did not publish.
    pub fn get<T: Send + Sync + 'static + ?Sized>(
        &self,
        id: CapabilityId,
        what: &str,
    ) -> AgentResult<Arc<T>> {
        self.services
            .get(&id)
            .and_then(|boxed| boxed.downcast_ref::<Arc<T>>())
            .cloned()
            .ok_or_else(|| {
                AgentError::InvalidRequest(format!(
                    "capability {id:?} ({what}) not available; is its module registered?"
                ))
            })
    }

    pub fn context_service(&self) -> AgentResult<Arc<dyn ContextEngine>> {
        self.get(CONTEXT_SERVICE, "context engine")
    }

    pub fn model_provider(&self) -> AgentResult<Arc<dyn ModelTransport>> {
        self.get(MODEL_PROVIDER, "model provider")
    }

    pub fn tool_provider(&self) -> AgentResult<Arc<dyn ToolDispatcher>> {
        self.get(TOOL_PROVIDER, "tool provider")
    }

    pub fn approval_policy(&self) -> AgentResult<Arc<dyn ApprovalGate>> {
        self.get(APPROVAL_POLICY, "approval policy")
    }

    /// The event journal is optional in the prototype.
    pub fn event_store(&self) -> AgentResult<Option<Arc<dyn EventJournal>>> {
        Ok(self.get(EVENT_STORE, "event store").ok())
    }

    /// The artifact store is optional in the prototype.
    pub fn artifact_store(&self) -> AgentResult<Option<Arc<Workspace>>> {
        Ok(self.get(ARTIFACT_STORE, "artifact store").ok())
    }
}

/// Owns the module list and the registry. Modules are added before `start`,
/// validated for capability conflicts, then started in order and stopped in
/// reverse order. Dynamic capabilities are registered against a shared
/// registry that the composition root hands to the tool provider, so they
/// can be published even after the host started.
#[derive(Default)]
pub struct ModuleHost {
    modules: Vec<Arc<dyn Module>>,
    registry: ServiceRegistry,
    capabilities: Arc<CapabilityRegistry>,
    started: bool,
}

impl ModuleHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn registry(&self) -> &ServiceRegistry {
        &self.registry
    }

    /// The shared dynamic-capability registry. Hand it to a
    /// `CapabilityAwareDispatcher` at composition time so capabilities
    /// registered later (even mid-run) are picked up by the tool provider.
    pub fn capability_registry(&self) -> Arc<CapabilityRegistry> {
        self.capabilities.clone()
    }

    /// Publish a dynamic capability. Unlike modules this is not gated on the
    /// host lifecycle: the LLM or any external actor can register new
    /// capabilities while the runtime is running, and their tools appear on
    /// the next model request.
    pub fn register_capability(&self, capability: Arc<dyn Capability>) -> AgentResult<()> {
        self.capabilities.register(capability)
    }

    /// Whether the host reached the serving state via [`Self::start`].
    /// [`RuntimeInstance::spawn`] proves this before it will accept a host,
    /// so a runtime can never be spawned over unstarted modules.
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Register a module and publish its capabilities. Rejects duplicate
    /// capability claims up front so composition fails fast.
    pub fn add_module(&mut self, module: Arc<dyn Module>) -> AgentResult<()> {
        if self.started {
            return Err(AgentError::InvalidRequest(
                "modules cannot be added after the host started".into(),
            ));
        }
        for id in module.capabilities() {
            if let Some(claimed_by) = self.registry.claims(id) {
                return Err(AgentError::InvalidRequest(format!(
                    "module '{}' claims {id:?} already claimed by '{claimed_by}'",
                    module.name()
                )));
            }
        }
        module.register(&mut self.registry)?;
        self.modules.push(module);
        Ok(())
    }

    /// Start every module in registration order, then the eager dynamic
    /// capabilities. Transactional: if any module or capability fails to
    /// start, everything that already started is stopped again (best
    /// effort) and all errors — the original failure plus every rollback
    /// failure — are aggregated into one result. A host that already
    /// reached Serving rejects a duplicate start instead of restarting
    /// every module under the same identity.
    pub async fn start(&mut self) -> AgentResult<()> {
        if self.started {
            return Err(AgentError::InvalidRequest(
                "the module host cannot be started twice".into(),
            ));
        }
        let mut started: Vec<Arc<dyn Module>> = Vec::new();
        for module in &self.modules {
            if let Err(first) = module.start().await {
                return Err(self.rollback_start(started, first).await);
            }
            started.push(module.clone());
        }
        if let Err(first) = self.capabilities.start_eager().await {
            return Err(self.rollback_start(started, first).await);
        }
        self.started = true;
        Ok(())
    }

    /// Undo a partially completed start: stop the eager capabilities and
    /// every module that already started, aggregating all failures with the
    /// original start error.
    async fn rollback_start(
        &mut self,
        started: Vec<Arc<dyn Module>>,
        first: AgentError,
    ) -> AgentError {
        let mut errors = vec![first];
        if let Err(error) = self.capabilities.stop_all().await {
            errors.push(error);
        }
        for module in started.iter().rev() {
            if let Err(error) = module.stop().await {
                errors.push(error);
            }
        }
        aggregate_errors(errors)
    }

    /// Stop the dynamic capabilities first — a capability may depend on a
    /// typed service (EventStore / ArtifactStore), so the services must
    /// outlive their consumers — then stop the typed modules in reverse
    /// registration order. Best effort: every stop runs even when an
    /// earlier one fails, and all errors are aggregated into one result.
    pub async fn stop(&mut self) -> AgentResult<()> {
        let mut errors = Vec::new();
        if let Err(error) = self.capabilities.stop_all().await {
            errors.push(error);
        }
        for module in self.modules.iter().rev() {
            if let Err(error) = module.stop().await {
                errors.push(error);
            }
        }
        self.started = false;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(aggregate_errors(errors))
        }
    }
}

/// Join multiple errors into one message so a best-effort start/stop can
/// report every failure instead of the first one it hit.
fn aggregate_errors(errors: Vec<AgentError>) -> AgentError {
    let message = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    AgentError::Internal(message)
}
