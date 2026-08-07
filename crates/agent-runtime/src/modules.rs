//! Concrete modules wrapping the engine contracts. Each module publishes one
//! typed capability into the host registry.

use std::sync::Arc;

use agent_contracts::{
    AgentResult, ApprovalGate, ContextEngine, EventJournal, ModelTransport, ToolDispatcher,
};
use agent_workspace::Workspace;
use async_trait::async_trait;

use crate::host::{
    APPROVAL_POLICY, ARTIFACT_STORE, CONTEXT_SERVICE, CapabilityId, EVENT_STORE, MODEL_PROVIDER,
    Module, ServiceRegistry, TOOL_PROVIDER,
};

pub struct ContextModule {
    engine: Arc<dyn ContextEngine>,
}

impl ContextModule {
    pub fn new(engine: Arc<dyn ContextEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl Module for ContextModule {
    fn name(&self) -> &'static str {
        "context"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        vec![CONTEXT_SERVICE]
    }
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()> {
        registry.register(CONTEXT_SERVICE, self.name(), self.engine.clone())
    }
}

pub struct ModelModule {
    model: Arc<dyn ModelTransport>,
}

impl ModelModule {
    pub fn new(model: Arc<dyn ModelTransport>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Module for ModelModule {
    fn name(&self) -> &'static str {
        "model"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        vec![MODEL_PROVIDER]
    }
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()> {
        registry.register(MODEL_PROVIDER, self.name(), self.model.clone())
    }
}

pub struct ToolModule {
    tools: Arc<dyn ToolDispatcher>,
}

impl ToolModule {
    pub fn new(tools: Arc<dyn ToolDispatcher>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Module for ToolModule {
    fn name(&self) -> &'static str {
        "tools"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        vec![TOOL_PROVIDER]
    }
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()> {
        registry.register(TOOL_PROVIDER, self.name(), self.tools.clone())
    }
}

pub struct ApprovalModule {
    policy: Arc<dyn ApprovalGate>,
}

impl ApprovalModule {
    pub fn new(policy: Arc<dyn ApprovalGate>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Module for ApprovalModule {
    fn name(&self) -> &'static str {
        "approval"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        vec![APPROVAL_POLICY]
    }
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()> {
        registry.register(APPROVAL_POLICY, self.name(), self.policy.clone())
    }
}

pub struct EventModule {
    journal: Arc<dyn EventJournal>,
}

impl EventModule {
    pub fn new(journal: Arc<dyn EventJournal>) -> Self {
        Self { journal }
    }
}

#[async_trait]
impl Module for EventModule {
    fn name(&self) -> &'static str {
        "events"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        vec![EVENT_STORE]
    }
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()> {
        registry.register(EVENT_STORE, self.name(), self.journal.clone())
    }
}

pub struct ArtifactModule {
    workspace: Arc<Workspace>,
}

impl ArtifactModule {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Module for ArtifactModule {
    fn name(&self) -> &'static str {
        "artifacts"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        vec![ARTIFACT_STORE]
    }
    fn register(&self, registry: &mut ServiceRegistry) -> AgentResult<()> {
        registry.register(ARTIFACT_STORE, self.name(), self.workspace.clone())
    }
}
