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
    /// Serializes the full cross-plane restore transaction. An actor-side
    /// restore token rejects stale finalization, but it cannot undo a late
    /// capability-registry write from an older concurrent caller; holding
    /// this gate across prepare -> capability restore -> finalize prevents
    /// that split-brain interleaving.
    restore_gate: Mutex<()>,
}

impl RuntimeInstance {
    /// Spawn the actor over the resolved services. The host must already be
    /// started; the services may come from its registry
    /// (`RuntimeServices::from_registry`) or be built directly. The kernel
    /// is derived from the services inside this seam — a composition root
    /// never constructs the authority facade itself.
    pub fn spawn(host: ModuleHost, services: RuntimeServices) -> Self {
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
        Self {
            host,
            handle,
            task,
            restore_gate: Mutex::new(()),
        }
    }

    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
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
    /// Capture is a freeze handshake: the capability surface generation is
    /// read before the actor snapshot and re-checked after the capability
    /// snapshot. A concurrent surface mutation between the two planes would
    /// otherwise produce a mixed snapshot (actor state from one moment,
    /// capability flags from another); the mismatch is detected and the
    /// capture retried, bounded, instead of silently shipping a torn view.
    /// It need not take the full-restore mutex: actor serialization means a
    /// checkpoint either completes before restore preparation or is refused
    /// by the pending restore fence, while the generation retry catches a
    /// capability mutation between the two snapshot planes.
    pub async fn checkpoint(&self) -> AgentResult<RuntimeCheckpoint> {
        let registry = self.host.capability_registry();
        for _ in 0..3 {
            let generation_before = registry.generation();
            let mut checkpoint = self.handle.checkpoint().await?;
            checkpoint.capabilities = registry.snapshot();
            if registry.generation() == generation_before {
                return Ok(checkpoint);
            }
            // A capability surface mutation landed between the two planes;
            // retry the whole capture against one stable generation.
        }
        Err(AgentError::Internal(
            "capability surface kept changing during checkpoint capture".into(),
        ))
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
        let _restore = self.restore_gate.lock().await;
        let capabilities = checkpoint.capabilities.clone();
        let restore_id = self.handle.prepare_restore(checkpoint).await?;
        let applied = self.host.capability_registry().restore(&capabilities);
        self.handle.finalize_restore(restore_id, applied > 0).await
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
