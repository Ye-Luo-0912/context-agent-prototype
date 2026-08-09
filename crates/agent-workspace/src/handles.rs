//! Confined capability handles: the `WorkspaceHandle` / `ArtifactHandle`
//! implementations that capabilities receive inside a
//! `CapabilityInvocationContext`.
//!
//! Every access funnels through `Workspace`'s existing confinement and
//! mutation machinery, so a capability — however hostile — cannot escape the
//! workspace root, reach the runtime state directory, or write without a
//! journal entry. The handle is the enforcement boundary for the capability
//! plane: it is what makes a declared permission real.

use std::path::{Path, PathBuf};

use agent_contracts::{AgentError, AgentResult, ArtifactHandle, Effect, RunId, WorkspaceHandle};
use async_trait::async_trait;

use crate::Workspace;

/// A `WorkspaceHandle` backed by the real `Workspace`: paths resolve through
/// `resolve_relative` (escape/symlink confinement), writes go through
/// `begin_mutation` (journaled, atomic, state-dir protection).
pub struct ConfinedWorkspaceHandle {
    workspace: Workspace,
    /// Identity recorded in the change journal (the capability/tool that
    /// opened the mutation).
    tool: String,
}

impl ConfinedWorkspaceHandle {
    pub fn new(workspace: &Workspace, tool: &str) -> Self {
        Self {
            workspace: workspace.clone(),
            tool: tool.to_string(),
        }
    }
}

#[async_trait]
impl WorkspaceHandle for ConfinedWorkspaceHandle {
    fn root(&self) -> &Path {
        self.workspace.root()
    }

    async fn resolve(&self, relative: &str) -> AgentResult<PathBuf> {
        self.workspace.resolve_relative(relative).await
    }

    async fn read(&self, relative: &str) -> AgentResult<Vec<u8>> {
        let path = self.workspace.resolve_relative(relative).await?;
        tokio::fs::read(&path)
            .await
            .map_err(|e| AgentError::Io(format!("read {}: {e}", path.display())))
    }

    async fn write(&self, relative: &str, content: &[u8]) -> AgentResult<()> {
        let transaction = self
            .workspace
            .begin_mutation(&self.tool, "write", relative)
            .await?;
        transaction.apply(content).await
    }

    async fn prepare_write(&self, relative: &str, content: &[u8]) -> AgentResult<Box<dyn Effect>> {
        let transaction = self
            .workspace
            .begin_mutation(&self.tool, "write", relative)
            .await?;
        let prepared = transaction.prepare(content).await?;
        Ok(Box::new(prepared))
    }
}

/// An `ArtifactHandle` backed by the run's artifact directory: large
/// outputs land under `.focus-agent/artifacts/<run>/` and come back as
/// `artifact://` references, keeping model-facing output bounded.
pub struct ArtifactStoreHandle {
    workspace: Workspace,
    run_id: RunId,
}

impl ArtifactStoreHandle {
    pub fn new(workspace: &Workspace, run_id: RunId) -> Self {
        Self {
            workspace: workspace.clone(),
            run_id,
        }
    }
}

#[async_trait]
impl ArtifactHandle for ArtifactStoreHandle {
    async fn store(&self, name: &str, bytes: &[u8]) -> AgentResult<String> {
        // Split "name.ext" into a readable prefix + extension; the store
        // sanitizes both and appends a unique id.
        let (prefix, extension) = match name.rsplit_once('.') {
            Some((prefix, extension)) if !extension.is_empty() => {
                (prefix.to_string(), extension.to_string())
            }
            _ => (name.to_string(), String::new()),
        };
        self.workspace
            .write_artifact(self.run_id, &prefix, &extension, bytes)
            .await
    }
}
