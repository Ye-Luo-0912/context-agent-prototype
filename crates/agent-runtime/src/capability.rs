//! The dynamic capability plane of the runtime: a shared registry that
//! accepts capabilities at composition time or at runtime, and a tool
//! dispatcher that merges the trusted core's tools with the capabilities'
//! tools so the model can call either.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use agent_contracts::{
    AgentError, AgentResult, Capability, CapabilityLifecycle, CapabilityStatus,
    CapabilityTransport, ToolDispatcher, ToolExecutionRequest, ToolOutput, ToolSpec,
};
use async_trait::async_trait;

struct Entry {
    capability: Arc<dyn Capability>,
    started: bool,
    /// The effective maturity. External (out-of-process) capabilities are
    /// pinned to Experimental regardless of their declared status, so an LLM
    /// cannot promote its own module to Stable.
    status: CapabilityStatus,
}

/// One row of the platform's capability catalog (the discovery surface).
#[derive(Debug, Clone)]
pub struct CapabilityCatalogEntry {
    pub id: String,
    pub status: CapabilityStatus,
    pub transport: CapabilityTransport,
    pub tools: Vec<String>,
}

/// Runtime-mutable registry of dynamic capabilities, shared between the
/// module host (registration) and the tool dispatcher (specs + routing).
/// Registration is not gated on the host lifecycle: a capability can be
/// published mid-run and its tools appear on the next model request.
#[derive(Default)]
pub struct CapabilityRegistry {
    inner: RwLock<HashMap<String, Entry>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a capability. Rejects duplicate ids and missing declared
    /// dependencies so the model never sees a half-wired tool.
    pub fn register(&self, capability: Arc<dyn Capability>) -> AgentResult<()> {
        let manifest = capability.manifest();
        let mut inner = self.inner.write().expect("capability registry poisoned");
        if inner.contains_key(&manifest.id) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{}' is already registered",
                manifest.id
            )));
        }
        for dependency in &manifest.dependencies {
            if !inner.contains_key(dependency) {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' depends on '{}' which is not registered",
                    manifest.id, dependency
                )));
            }
        }
        // The maturity ladder is climbed, not declared: out-of-process
        // (external/LLM-authored) capabilities always start Experimental.
        let status = if manifest.transport != CapabilityTransport::InProcess
            && manifest.status != CapabilityStatus::Experimental
        {
            CapabilityStatus::Experimental
        } else {
            manifest.status
        };
        inner.insert(
            manifest.id.clone(),
            Entry {
                capability,
                started: false,
                status,
            },
        );
        Ok(())
    }

    /// The effective maturity of a registered capability (Experimental for
    /// external capabilities regardless of declaration).
    pub fn status(&self, id: &str) -> Option<CapabilityStatus> {
        self.inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .map(|entry| entry.status)
    }

    /// Snapshot of every registered capability, for the discovery surface.
    pub fn catalog(&self) -> Vec<CapabilityCatalogEntry> {
        let inner = self.inner.read().expect("capability registry poisoned");
        let mut entries: Vec<_> = inner
            .values()
            .map(|entry| {
                let manifest = entry.capability.manifest();
                CapabilityCatalogEntry {
                    id: manifest.id.clone(),
                    status: entry.status,
                    transport: manifest.transport.clone(),
                    tools: entry
                        .capability
                        .tool_specs()
                        .iter()
                        .map(|s| s.name.clone())
                        .collect(),
                }
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// The tool schemas all registered capabilities contribute, so the
    /// runtime's tool provider can expose them to the model.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner
            .values()
            .flat_map(|entry| entry.capability.tool_specs())
            .collect()
    }

    /// The capability that owns a tool name, if any.
    pub fn by_tool(&self, tool_name: &str) -> Option<Arc<dyn Capability>> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.values().find_map(|entry| {
            entry
                .capability
                .tool_specs()
                .iter()
                .any(|spec| spec.name == tool_name)
                .then(|| entry.capability.clone())
        })
    }

    /// Start every eager capability that has not started yet (host start).
    pub async fn start_eager(&self) -> AgentResult<()> {
        let ids: Vec<String> = {
            let inner = self.inner.read().expect("capability registry poisoned");
            inner
                .iter()
                .filter(|(_, entry)| {
                    entry.capability.manifest().lifecycle == CapabilityLifecycle::Eager
                        && !entry.started
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.ensure_started(&id).await?;
        }
        Ok(())
    }

    /// Start a capability on first use (lazy lifecycle) and mark it started.
    pub async fn ensure_started(&self, id: &str) -> AgentResult<()> {
        let already_started = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .is_some_and(|entry| entry.started);
        if already_started {
            return Ok(());
        }
        let capability = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .map(|entry| entry.capability.clone())
            .ok_or_else(|| {
                AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
            })?;
        capability.start().await?;
        if let Some(entry) = self
            .inner
            .write()
            .expect("capability registry poisoned")
            .get_mut(id)
        {
            entry.started = true;
        }
        Ok(())
    }

    /// Stop every capability and reset the started flags (host stop).
    pub async fn stop_all(&self) -> AgentResult<()> {
        let capabilities: Vec<Arc<dyn Capability>> = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .values()
            .map(|entry| entry.capability.clone())
            .collect();
        for capability in capabilities {
            capability.stop().await?;
        }
        for entry in self
            .inner
            .write()
            .expect("capability registry poisoned")
            .values_mut()
        {
            entry.started = false;
        }
        Ok(())
    }
}

/// A `ToolDispatcher` that merges the trusted core's tools with the dynamic
/// capabilities' tools. The kernel keeps talking to one `ToolDispatcher`;
/// capabilities are registered against the shared registry at runtime and
/// their tools appear on the next model request.
pub struct CapabilityAwareDispatcher {
    base: Arc<dyn ToolDispatcher>,
    capabilities: Arc<CapabilityRegistry>,
}

impl CapabilityAwareDispatcher {
    pub fn new(base: Arc<dyn ToolDispatcher>, capabilities: Arc<CapabilityRegistry>) -> Self {
        Self { base, capabilities }
    }
}

#[async_trait]
impl ToolDispatcher for CapabilityAwareDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.base.specs();
        specs.extend(self.capabilities.tool_specs());
        specs
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        if let Some(capability) = self.capabilities.by_tool(&request.call.name) {
            self.capabilities
                .ensure_started(&capability.manifest().id)
                .await?;
            return capability.invoke(request.call).await;
        }
        self.base.execute(request).await
    }
}
