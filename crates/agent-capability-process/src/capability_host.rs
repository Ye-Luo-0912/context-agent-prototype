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
    CapabilityOutcome, CapabilityTransport, ProcessInvokeResponse, ToolCall, ToolOutput, ToolRisk,
    WORKSPACE_WRITE, WireEffect, validate_capability_id,
};
use agent_process::{ProcessHost, ProcessHostConfig, ProcessSandbox};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

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
        validate_capability_id(&manifest.id).map_err(AgentError::InvalidRequest)?;
        // Risk is derived from declared authority, never self-declared: a
        // process that can write the workspace must not auto-allow through
        // a ReadOnly tool at the approval gate (the registry enforces the
        // same rule; the adapter must not trust a manifest the registry
        // never saw, e.g. `with_config` in tests).
        if manifest.permissions.iter().any(|p| p == WORKSPACE_WRITE) {
            for spec in &manifest.tools {
                if spec.risk == ToolRisk::ReadOnly {
                    return Err(AgentError::InvalidRequest(format!(
                        "capability '{}' declares '{WORKSPACE_WRITE}' but tool '{}' self-declares ReadOnly; risk is derived from declared authority, never self-declared",
                        manifest.id, spec.name
                    )));
                }
            }
        }
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
            sandbox: ProcessSandbox {
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
                // A private working directory per capability — the id is
                // validated to a conservative grammar *and* suffixed with
                // an unpredictable nonce, so two runs (or two capabilities
                // reusing an id) never share a predictable temp path and a
                // hostile pre-created directory cannot be a symlink trap.
                cwd: Some(private_capability_dir(&manifest.id)),
                // Hard ceilings enforced by the kernel on Unix.
                cpu_time_limit_secs: 60,
                process_limit: 16,
                // The child's stderr is piped and drained into a bounded
                // tail, never inherited unbounded into the parent console.
                stderr_capture_bytes: 64 * 1024,
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
        // knows what it may do. The response is either a plain `ToolOutput`
        // (no side effects — the historical shape) or a
        // `ProcessInvokeResponse` carrying structured wire effects the child
        // asks the runtime to commit: the child never mutates anything
        // itself, it declares intent. The adapter validates every effect
        // against the granted permissions and stages it through the
        // confined workspace handle, so a process mutation crosses the same
        // generation-fence effect commit as a builtin tool's `PreparedEffect`.
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
        match serde_json::from_value::<ProcessInvokeResponse>(value.clone()) {
            Ok(response) if !response.effects.is_empty() => {
                let effects = self.stage_wire_effects(&response.effects, &ctx).await?;
                Ok(CapabilityOutcome::EffectRequest {
                    output: response.output,
                    effect: Box::new(effects),
                })
            }
            _ => {
                // No wire effects: either the child answered with the
                // historical plain `ToolOutput` shape, or it declared an
                // empty effect list. Either way the output passes through.
                let output: ToolOutput = serde_json::from_value(value)
                    .map_err(|e| AgentError::Context(format!("decode capability output: {e}")))?;
                Ok(CapabilityOutcome::Value(output))
            }
        }
    }
}

impl ProcessCapabilityAdapter {
    /// Validate every wire effect against the granted permissions and stage
    /// it through the confined workspace handle. The child declared intent;
    /// the runtime's handle does the actual path resolution and staging, so
    /// an undeclared or over-granted effect is refused before anything can
    /// land.
    async fn stage_wire_effects(
        &self,
        effects: &[WireEffect],
        ctx: &CapabilityInvocationContext,
    ) -> AgentResult<Vec<Box<dyn agent_contracts::Effect>>> {
        let mut staged: Vec<Box<dyn agent_contracts::Effect>> = Vec::new();
        for effect in effects {
            match effect {
                WireEffect::WorkspaceWrite { path, content_b64 } => {
                    if !ctx.granted_permissions.iter().any(|p| p == WORKSPACE_WRITE) {
                        return Err(AgentError::InvalidRequest(format!(
                            "capability '{}' declared a workspace write effect without '{WORKSPACE_WRITE}' permission",
                            self.manifest.id
                        )));
                    }
                    let workspace = ctx.workspace.as_ref().ok_or_else(|| {
                        AgentError::InvalidRequest(format!(
                            "capability '{}' has no workspace handle: '{WORKSPACE_WRITE}' was not granted",
                            self.manifest.id
                        ))
                    })?;
                    let content = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        content_b64,
                    )
                    .map_err(|e| {
                        AgentError::Context(format!(
                            "capability '{}' sent an invalid base64 write payload: {e}",
                            self.manifest.id
                        ))
                    })?;
                    staged.push(workspace.prepare_write(path, &content).await?);
                }
            }
        }
        Ok(staged)
    }
}

/// Convenience: load a process capability from its manifest, ready for the
/// registry (which expects `Arc<dyn Capability>`).
pub fn load_process_capability(manifest: CapabilityManifest) -> AgentResult<Arc<dyn Capability>> {
    Ok(Arc::new(ProcessCapabilityAdapter::from_manifest(manifest)?))
}

/// A private, unpredictable working directory for one capability: the
/// validated id plus a random nonce, so no two runs share a path and a
/// hostile pre-created directory cannot predict it. The parent is the OS
/// temp dir (never the workspace, never the launch cwd).
fn private_capability_dir(id: &str) -> std::path::PathBuf {
    let nonce = agent_contracts::ContextItemId::new();
    std::env::temp_dir().join(format!("context-agent-capability-{id}-{nonce}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_capability_dirs_are_unpredictable_and_path_safe() {
        let a = private_capability_dir("process-demo");
        let b = private_capability_dir("process-demo");
        assert_ne!(a, b, "two builds must never share a predictable path");
        assert_eq!(a.parent(), Some(std::env::temp_dir().as_path()));
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("context-agent-capability-process-demo-"),
            "unexpected name: {name}"
        );
        // The id is validated before it ever reaches this function, but the
        // nonce suffix alone keeps even a hostile pre-created directory
        // from being predicted.
        let without_id = name.trim_start_matches("context-agent-capability-");
        assert!(without_id.len() > "process-demo-".len());
    }
}
