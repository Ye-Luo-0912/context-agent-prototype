//! `Capability` implemented over the shared `ProcessHost`: the generic
//! process-capability adapter.
//!
//! A capability process speaks the same JSON-lines shape as the context
//! service (versioned handshake, framed request/response, per-request
//! deadlines, poisoned connection). This adapter translates `Capability`
//! calls onto `{"op": "invoke", "call": ...}` and back, so a process
//! capability never writes its own stdio framing — exactly like the
//! context-service adapter, the host owns that once.

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, Capability, CapabilityInvocationContext, CapabilityManifest,
    CapabilityOutcome, CapabilityTransport, ToolCall, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use crate::host::{ProcessHost, ProcessHostConfig};

/// A `Capability` whose service is a separate process. The manifest's
/// declared `tools` are served to the model without starting the process;
/// `start()` connects it, `invoke()` forwards each call and decodes the
/// `ToolOutput` from the response's `value`.
pub struct ProcessCapabilityAdapter {
    manifest: CapabilityManifest,
    config: ProcessHostConfig,
    /// `None` until `start()` connects the child; the capability is not
    /// usable before then.
    host: Mutex<Option<ProcessHost>>,
}

impl ProcessCapabilityAdapter {
    /// Build the adapter from a manifest declaring `CapabilityTransport::Process`.
    pub fn from_manifest(manifest: CapabilityManifest) -> AgentResult<Self> {
        let program = match &manifest.transport {
            CapabilityTransport::Process { program } => program.clone(),
            other => {
                return Err(AgentError::InvalidRequest(format!(
                    "process capability requires Process transport, got {other:?}"
                )));
            }
        };
        let config = ProcessHostConfig {
            program,
            args: Vec::new(),
            env: Vec::new(),
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_frame_bytes: 16 * 1024 * 1024,
            sandbox: crate::host::ProcessSandbox {
                // No parent secrets: only the non-secret platform essentials
                // are inherited; anything else must be granted explicitly
                // via `env` overrides. OPENAI_API_KEY, HOME, credentials and
                // friends never cross the boundary by default.
                env_whitelist: Some(vec![
                    "PATH".into(),
                    "SystemRoot".into(),
                    "SystemDrive".into(),
                    "TEMP".into(),
                    "TMP".into(),
                ]),
                // A dedicated working directory per capability, created at
                // connect — never the parent's cwd, so a generated
                // capability cannot roam the workspace by relative paths.
                cwd: Some(
                    std::env::temp_dir()
                        .join(format!("context-agent-capability-{}", manifest.id)),
                ),
                // Hard ceilings enforced by the kernel on Unix.
                cpu_time_limit_secs: 60,
                process_limit: 16,
            },
        };
        Ok(Self::with_config(manifest, config))
    }

    /// Build the adapter with an explicit transport config (custom
    /// deadlines, sandbox environment, test doubles).
    pub fn with_config(manifest: CapabilityManifest, config: ProcessHostConfig) -> Self {
        Self {
            manifest,
            config,
            host: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Capability for ProcessCapabilityAdapter {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    // `tool_specs` defaults to `manifest.tools` — the process capability's
    // declared schemas, advertised without starting the process.

    async fn start(&self) -> AgentResult<()> {
        let mut slot = self.host.lock().await;
        if slot.is_some() {
            return Ok(()); // already connected
        }
        *slot = Some(ProcessHost::connect(self.config.clone()).await?);
        Ok(())
    }

    async fn stop(&self) -> AgentResult<()> {
        if let Some(host) = self.host.lock().await.take() {
            host.shutdown().await;
        }
        Ok(())
    }

    async fn invoke(
        &self,
        call: ToolCall,
        ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        let slot = self.host.lock().await;
        let host = slot.as_ref().ok_or_else(|| {
            AgentError::Context(format!(
                "capability '{}' process is not started",
                self.manifest.id
            ))
        })?;
        // The process receives the call plus the granted permissions so it
        // knows what it may do; the sandbox enforcing those grants is a
        // later concern, like the context service's own boundary. The
        // invocation context's cancellation token aborts a long-running
        // call immediately: `/cancel` or a superseded operation kills the
        // subprocess tree instead of waiting for the request deadline.
        let value = host
            .call_with_cancel(
                json!({
                    "op": "invoke",
                    "call": call,
                    "permissions": ctx.granted_permissions,
                }),
                &ctx.cancel,
            )
            .await?;
        let output: ToolOutput = serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode capability output: {e}")))?;
        // An out-of-process capability applies its own side effects inside
        // the subprocess; across the wire the runtime only ever sees a
        // completed value. Staged effect / directive transport is a future
        // protocol extension.
        Ok(CapabilityOutcome::Value(output))
    }
}

/// Convenience: load a process capability from its manifest, ready for the
/// registry (which expects `Arc<dyn Capability>`).
pub fn load_process_capability(manifest: CapabilityManifest) -> AgentResult<Arc<dyn Capability>> {
    Ok(Arc::new(ProcessCapabilityAdapter::from_manifest(manifest)?))
}
