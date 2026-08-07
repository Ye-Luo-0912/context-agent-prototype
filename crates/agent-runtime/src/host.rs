//! The module host: a uniform lifecycle (register, validate, start, stop)
//! over typed capabilities. There is no universal `handle_event` — modules
//! publish typed services into the registry and consumers look them up by
//! type.

use std::{any::Any, collections::HashMap, sync::Arc};

use agent_contracts::{
    AgentError, AgentResult, ApprovalGate, ContextEngine, EventJournal, ModelTransport,
    ToolDispatcher,
};
use agent_workspace::Workspace;
use async_trait::async_trait;

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

    pub(crate) fn register<T: Send + Sync + 'static + ?Sized>(
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

    pub(crate) fn get<T: Send + Sync + 'static + ?Sized>(
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
/// reverse order.
#[derive(Default)]
pub struct ModuleHost {
    modules: Vec<Arc<dyn Module>>,
    registry: ServiceRegistry,
    started: bool,
}

impl ModuleHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn registry(&self) -> &ServiceRegistry {
        &self.registry
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

    pub async fn start(&mut self) -> AgentResult<()> {
        for module in &self.modules {
            module.start().await?;
        }
        self.started = true;
        Ok(())
    }

    /// Stop modules in reverse registration order (dependents before
    /// dependencies).
    pub async fn stop(&mut self) -> AgentResult<()> {
        for module in self.modules.iter().rev() {
            module.stop().await?;
        }
        self.started = false;
        Ok(())
    }
}
