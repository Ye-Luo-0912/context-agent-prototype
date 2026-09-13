//! `RuntimeInstance`: one object that owns the whole runtime and its ordered
//! shutdown. Composition roots no longer juggle the module host, the actor
//! handle and the join handle separately; `shutdown()` runs the full teardown
//! and aggregates every error instead of swallowing them.

use std::sync::Arc;

use agent_contracts::{AgentError, AgentResult};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::checkpoint::RuntimeCheckpoint;
use crate::command::RuntimeHandle;
use crate::host::ModuleHost;
use crate::services::RuntimeServices;

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
    /// The one cross-plane capture/restore implementation. Shared (not
    /// duplicated) with any long-lived entry point that must offer save and
    /// restore while this instance keeps owning shutdown.
    checkpoints: Arc<RuntimeCheckpointPlane>,
}

/// F5: the cross-plane checkpoint seam, shareable with a serving host.
///
/// A complete runtime checkpoint spans two planes: the actor's own state and
/// the host-owned capability surface. The actor-only dump is deliberately
/// crate-private, so a client cannot persist a partial artifact and call it a
/// checkpoint. This handle is the *only* way an external entry point (the
/// Platform host's save/restore routes) reaches the full transaction, and it
/// runs exactly the same steps [`RuntimeInstance::checkpoint`] and
/// [`RuntimeInstance::restore`] run — there is no second flow to drift from.
pub struct RuntimeCheckpointPlane {
    handle: RuntimeHandle,
    capabilities: Arc<crate::capability::CapabilityRegistry>,
    /// Serializes the full cross-plane restore transaction. An actor-side
    /// restore token rejects stale finalization, but it cannot undo a late
    /// capability-registry write from an older concurrent caller; holding
    /// this gate across prepare -> capability restore -> finalize prevents
    /// that split-brain interleaving.
    restore_gate: Mutex<()>,
}

impl RuntimeCheckpointPlane {
    /// Capture actor state, context state, the host-owned capability surface
    /// and the durable Core authority marker as one artifact.
    pub async fn capture(&self) -> AgentResult<RuntimeCheckpoint> {
        self.handle.checkpoint().await
    }

    /// The full two-phase restore: prepare (validate + install under the
    /// actor's recovery fence) -> apply the capability plane -> durably
    /// publish the commit. A failed marker or barrier leaves normal mutation
    /// blocked instead of exposing a half-restored runtime.
    pub async fn restore(&self, checkpoint: RuntimeCheckpoint) -> AgentResult<()> {
        let _restore = self.restore_gate.lock().await;
        let capabilities = checkpoint.capabilities.clone();
        let restore_id = self.handle.prepare_restore(checkpoint).await?;
        let applied = self.capabilities.restore(&capabilities);
        self.handle.finalize_restore(restore_id, applied > 0).await
    }
}

impl RuntimeInstance {
    /// Spawn the actor over the resolved services. The host must already
    /// have reached Serving; the services may come from its registry
    /// (`RuntimeServices::from_registry`) or be built directly. The kernel
    /// is derived from the services inside this seam — a composition root
    /// never constructs the authority facade itself. Spawning over an
    /// unstarted host is a composition bug and panics rather than leaking
    /// a runtime over half-built modules.
    pub fn spawn(host: ModuleHost, services: RuntimeServices) -> Self {
        assert!(
            host.is_started(),
            "RuntimeInstance::spawn requires the module host to have reached Serving"
        );
        // Safe-point checkpoints must capture the full plane set: hand the
        // actor a read-only registry handle unless the composition root
        // already wired one. The host stays the registration authority;
        // this is a mechanical snapshot source, not a second orchestrator.
        let mut services = services;
        if services.capability_snapshot_for_spawn().is_none() {
            services.set_capability_registry(host.capability_registry());
        }
        let services = Arc::new(services);
        let (handle, task) = crate::actor::spawn_runtime(services);
        let checkpoints = Arc::new(RuntimeCheckpointPlane {
            handle: handle.clone(),
            capabilities: host.capability_registry(),
            restore_gate: Mutex::new(()),
        });
        Self {
            host,
            handle,
            task,
            checkpoints,
        }
    }

    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }

    /// F5: the shareable cross-plane capture/restore seam. A serving host
    /// clones this to expose save/restore routes while the instance keeps
    /// owning shutdown; both paths execute the same transaction.
    pub fn checkpoint_plane(&self) -> Arc<RuntimeCheckpointPlane> {
        Arc::clone(&self.checkpoints)
    }

    /// Start the runtime (emits `RunStarted`). Subscribe first to see it.
    pub async fn start(&self) -> AgentResult<()> {
        self.handle.start().await
    }

    /// A cross-plane runtime checkpoint: actor state (task table, current
    /// task), context state, the host-owned capability surface, and a
    /// read-only marker for the durable Core authority prefix. Core operation
    /// truth remains in its WAL; the checkpoint references and verifies it
    /// rather than copying or rewinding it.
    ///
    /// The one checkpoint capture contract lives in the actor's safe-point
    /// assembler: capability-plane generation handshake around every plane
    /// read, with bounded retries against a stable generation. The external
    /// instance path and automatic safe points therefore cannot drift.
    pub async fn checkpoint(&self) -> AgentResult<RuntimeCheckpoint> {
        self.checkpoints.capture().await
    }

    /// Restore the whole runtime from a checkpoint through a two-phase
    /// handshake. The actor first validates and transactionally installs
    /// the Core authority marker before any mutation, then advances the live
    /// epoch and transactionally installs context + task authority while
    /// raising its recovery fence. The host applies capability state with a
    /// fail-closed monotonic meet, and the actor durably publishes the
    /// resulting commit before clearing the fence. A failed marker or final
    /// barrier leaves normal mutation blocked instead of exposing a
    /// half-restored runtime.
    pub async fn restore(&self, checkpoint: RuntimeCheckpoint) -> AgentResult<()> {
        self.checkpoints.restore(checkpoint).await
    }

    /// Full ordered shutdown. Every step runs even when an earlier one
    /// failed; the errors are aggregated into one result so a journal flush
    /// or module stop failure is visible to the caller.
    pub async fn shutdown(mut self) -> AgentResult<()> {
        let mut errors: Vec<String> = Vec::new();

        // `Stop` owns cancellation and the bounded drain of any late tool
        // completion that may still carry a PreparedEffect. Sending a
        // separate CancelTurn first would clear the turn before Stop sees
        // it; the actor keeps an explicit pending-cleanup identity, but one
        // ordered command is the simpler and stronger shutdown contract.
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
