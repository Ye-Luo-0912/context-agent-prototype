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

use agent_contracts::{
    AgentError, AgentResult, ArtifactHandle, BoundedRead, Effect, OperationEffectContext, RunId,
    WorkspaceHandle,
};
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
    effect_context: Option<OperationEffectContext>,
}

impl ConfinedWorkspaceHandle {
    pub fn new(workspace: &Workspace, tool: &str) -> Self {
        Self {
            workspace: workspace.clone(),
            tool: tool.to_string(),
            effect_context: None,
        }
    }

    pub fn new_with_effect_context(
        workspace: &Workspace,
        tool: &str,
        effect_context: OperationEffectContext,
    ) -> Self {
        Self {
            workspace: workspace.clone(),
            tool: tool.to_string(),
            effect_context: Some(effect_context),
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
        // Validation and open are fused into one directory-handle-relative
        // descent, so a link swap cannot redirect the read outside the
        // workspace; the read goes through the pinned handle.
        let confined = self.workspace.confined_open_read(relative).await?;
        use tokio::io::AsyncReadExt;
        let mut file = confined.into_tokio();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .await
            .map_err(|e| AgentError::Io(format!("read {relative}: {e}")))?;
        Ok(bytes)
    }

    async fn read_bounded(&self, relative: &str, max_bytes: usize) -> AgentResult<BoundedRead> {
        // Metadata and bytes come from the same pinned handle, preserving
        // the confinement guarantee while applying the allocation bound
        // before any content is read.
        let confined = self.workspace.confined_open_read(relative).await?;
        let byte_len = confined
            .metadata()
            .map_err(|e| AgentError::Io(format!("metadata {relative}: {e}")))?
            .len();
        use tokio::io::AsyncReadExt;
        let mut file = confined.into_tokio().take(max_bytes as u64);
        let mut content = Vec::new();
        file.read_to_end(&mut content)
            .await
            .map_err(|e| AgentError::Io(format!("read {relative}: {e}")))?;
        Ok(BoundedRead {
            content,
            byte_len,
            truncated: byte_len > max_bytes as u64,
        })
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
        let prepared = match &self.effect_context {
            Some(context) => {
                transaction
                    .prepare_with_effect_context(content, context.clone())
                    .await?
            }
            None => transaction.prepare(content).await?,
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_read_returns_only_the_requested_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![b'x'; 128 * 1024];
        tokio::fs::write(dir.path().join("large.bin"), &content)
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let handle = ConfinedWorkspaceHandle::new(&workspace, "test");

        let result = handle.read_bounded("large.bin", 4096).await.unwrap();

        assert_eq!(result.content, content[..4096]);
        assert_eq!(result.byte_len, content.len() as u64);
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn zero_length_bounded_read_does_not_read_content() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("value.txt"), b"value")
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let handle = ConfinedWorkspaceHandle::new(&workspace, "test");

        let result = handle.read_bounded("value.txt", 0).await.unwrap();

        assert!(result.content.is_empty());
        assert_eq!(result.byte_len, 5);
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn bounded_read_preserves_workspace_confinement() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let handle = ConfinedWorkspaceHandle::new(&workspace, "test");

        let error = handle.read_bounded("../outside.txt", 16).await.unwrap_err();

        assert!(
            matches!(error, AgentError::InvalidRequest(_)),
            "the bounded primitive must reject the same parent escape as a full read: {error}"
        );
    }
}
