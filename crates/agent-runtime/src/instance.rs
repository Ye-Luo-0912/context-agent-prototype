//! `RuntimeInstance`: one object that owns the whole runtime and its ordered
//! shutdown. Composition roots no longer juggle the module host, the actor
//! handle and the join handle separately; `shutdown()` runs the full teardown
//! and aggregates every error instead of swallowing them.

use std::sync::Arc;

use agent_contracts::{AgentError, AgentResult};
use agent_kernel::AgentKernel;
use tokio::task::JoinHandle;

use crate::checkpoint::RuntimeCheckpoint;
use crate::command::RuntimeHandle;
use crate::host::ModuleHost;

/// Owns the module host, the actor handle and the actor task. `shutdown` is
/// the only way a run should end:
///
/// ```text
/// cancel any turn
///   → stop the actor (kernel stop: flush journal, emit RunCompleted)
///   → stop the module host (reverse registration order)
///   → join the actor task
///   → aggregate errors
/// ```
pub struct RuntimeInstance {
    host: ModuleHost,
    handle: RuntimeHandle,
    task: JoinHandle<()>,
}

impl RuntimeInstance {
    /// Spawn the actor over a composed kernel. The host must already be
    /// started and its services consumed by the kernel.
    pub fn spawn(host: ModuleHost, kernel: Arc<AgentKernel>) -> Self {
        let (handle, task) = crate::actor::spawn_runtime(kernel);
        Self { host, handle, task }
    }

    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }

    /// Start the runtime (emits `RunStarted`). Subscribe first to see it.
    pub async fn start(&self) -> AgentResult<()> {
        self.handle.start().await
    }

    /// A complete runtime checkpoint: the actor's state (task table, current
    /// task) plus the context engine's checkpoint, plus the dynamic
    /// capability surface state the host owns. This — not the context
    /// checkpoint alone — is the snapshot a restart can restore from.
    pub async fn checkpoint(&self) -> AgentResult<RuntimeCheckpoint> {
        let mut checkpoint = self.handle.checkpoint().await?;
        checkpoint.capabilities = self.host.capability_registry().snapshot();
        Ok(checkpoint)
    }

    /// Restore the whole runtime from a checkpoint. The actor validates and
    /// transactionally restores context + task authority first; only after
    /// that succeeds are the infallible capability flags re-applied. A busy,
    /// malformed or context-incompatible checkpoint therefore cannot
    /// partially change the capability surface.
    pub async fn restore(&self, checkpoint: RuntimeCheckpoint) -> AgentResult<()> {
        let capabilities = checkpoint.capabilities.clone();
        self.handle.restore(checkpoint).await?;
        self.host.capability_registry().restore(&capabilities);
        Ok(())
    }

    /// Full ordered shutdown. Every step runs even when an earlier one
    /// failed; the errors are aggregated into one result so a journal flush
    /// or module stop failure is visible to the caller.
    pub async fn shutdown(mut self) -> AgentResult<()> {
        let mut errors: Vec<String> = Vec::new();

        self.handle.cancel_turn().await;
        if let Err(error) = self.handle.stop().await {
            errors.push(format!("runtime stop: {error}"));
        }
        if let Err(error) = self.host.stop().await {
            errors.push(format!("module host stop: {error}"));
        }
        if self.task.await.is_err() {
            errors.push("actor task panicked".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(AgentError::Internal(errors.join("; ")))
        }
    }
}
