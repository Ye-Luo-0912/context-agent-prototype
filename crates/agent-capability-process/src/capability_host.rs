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
    CapabilityOutcome, CapabilityTransport, EffectIntent, ProcessInvokeResponse, ToolCall,
    ToolRisk, WORKSPACE_READ, WORKSPACE_WRITE, WireEffect, WorkspaceHandle, validate_capability_id,
};
use agent_platform_protocol::FEATURE_LEGACY_INVOKE_OUTPUT;
use agent_process::{
    HostLifecycle, MAX_SYSTEM_REQUESTS_PER_CALL, ProcessHost, ProcessHostConfig, ProcessSandbox,
    RestartCircuit, SystemBroker,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// A `Capability` whose service is a separate process. The manifest's
/// declared `tools` are served to the model without starting the process;
/// `start()` connects it, `invoke()` forwards each call and decodes the
/// `ToolOutput` from the response's `value`.
pub struct ProcessCapabilityAdapter {
    manifest: CapabilityManifest,
    config: ProcessHostConfig,
    /// Explicit lifecycle so a failed replacement connect cannot look like
    /// a first start and skip [`RestartCircuit`].
    host: Mutex<HostLifecycle<ProcessHost>>,
    restart: RestartCircuit,
}

/// How many raw bytes one brokered `fs.read` answer may carry. The control
/// plane is not a file transport: larger files come back as a bounded
/// prefix with a truncation marker, and the host's system-answer cap
/// (`ProcessHostConfig::max_system_answer_bytes`) re-checks the encoded
/// answer at the frame boundary.
pub const BROKER_FS_READ_MAX_BYTES: usize = 256 * 1024;

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
        let private_dir = private_capability_dir(&manifest.id);
        // The mid-invoke system broker is this capability's sanctioned I/O
        // path, so the per-call byte budget must cover the worst legitimate
        // exchange: one request + one response plus every brokered
        // round trip. The broker's own answers are bounded (see
        // `InvokeFsBroker`), so this cumulative cap is the backstop that
        // makes a flood cost real work.
        let max_system_answer_bytes = 512 * 1024;
        let config = ProcessHostConfig {
            program,
            args: Vec::new(),
            env: Vec::new(),
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_frame_bytes: 16 * 1024 * 1024,
            max_call_bytes: (16usize * 1024 * 1024).saturating_mul(2).saturating_add(
                MAX_SYSTEM_REQUESTS_PER_CALL.saturating_mul(max_system_answer_bytes),
            ),
            max_system_answer_bytes,
            offered_features: Default::default(),
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
                cwd: Some(private_dir.clone()),
                // Hard ceilings enforced by the kernel on Unix (rlimits)
                // and Windows (Job-Object: the active-process ceiling uses
                // `process_limit`, plus a per-process memory ceiling below).
                cpu_time_limit_secs: 60,
                process_limit: 16,
                // The child's stderr is piped and drained into a bounded
                // tail, never inherited unbounded into the parent console.
                stderr_capture_bytes: 64 * 1024,
                // A per-process memory ceiling enforced by the Job-Object,
                // so a runaway capability child cannot exhaust the machine.
                #[cfg(windows)]
                job_max_memory_bytes: 512 * 1024 * 1024,
                // Virtual address-space ceiling on Unix (`RLIMIT_AS`). This
                // is coarser than the Windows commit charge; 2 GiB still
                // bounds a runaway allocator without failing ordinary exec.
                #[cfg(unix)]
                max_memory_bytes: 2u64 * 1024 * 1024 * 1024,
                // Per-file size ceiling (`RLIMIT_FSIZE`). Bounds a child
                // filling its private write root; not I/O bandwidth.
                #[cfg(unix)]
                max_file_bytes: 256 * 1024 * 1024,
                // Open-file ceiling (`RLIMIT_NOFILE`). Bounds fd
                // exhaustion; inherited fds other than stdio are closed
                // in the same pre_exec (`MOD-13`). That hook also zeros
                // RLIMIT_CORE so a crash cannot dump secrets (`MOD-15`).
                // On Linux it also clamps NICE/RTPRIO and sets
                // no_new_privs (`MOD-16`).
                #[cfg(unix)]
                max_open_files: 1024,
                // OS-level write confinement (Linux landlock / Windows Low IL):
                // the child may create, modify or destroy filesystem state
                // only inside its own private dir. Reads are gated by the
                // app-level broker; this is the kernel fence no application
                // logic can bypass.
                #[cfg(target_os = "linux")]
                landlock_write_roots: vec![private_dir.clone()],
                #[cfg(windows)]
                integrity_write_roots: vec![private_dir.clone()],
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
            host: Mutex::new(HostLifecycle::NeverStarted),
            restart: RestartCircuit::new(agent_process::DEFAULT_MAX_CONNECTION_RESTARTS),
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
        if let HostLifecycle::Serving(host) = &*slot
            && host.status().health.allows_call()
        {
            return Ok(());
        }
        if slot.connect_kind() == agent_process::ConnectKind::Restart {
            self.restart.try_acquire()?;
            if let HostLifecycle::Serving(old) = std::mem::replace(
                &mut *slot,
                HostLifecycle::Quarantined {
                    reason: "restarting".into(),
                },
            ) {
                old.shutdown().await;
            }
        }
        match ProcessHost::connect(self.config.clone()).await {
            Ok(host) => {
                let attestation = host.sandbox_attestation();
                let profile = self.manifest.sandbox_profile;
                if !profile.allows_start(attestation.capabilities) {
                    host.shutdown().await;
                    let error = AgentError::Context(format!(
                        "capability '{}' sandbox profile {:?} is not covered by enforced {:?} (backend {} {})",
                        self.manifest.id,
                        profile,
                        attestation.capabilities,
                        attestation.backend,
                        attestation.backend_version
                    ));
                    slot.record_connect_failure(error.to_string());
                    return Err(error);
                }
                *slot = HostLifecycle::Serving(host);
                Ok(())
            }
            Err(error) => {
                slot.record_connect_failure(error.to_string());
                Err(error)
            }
        }
    }

    async fn stop(&self) -> AgentResult<()> {
        if let HostLifecycle::Serving(host) =
            std::mem::replace(&mut *self.host.lock().await, HostLifecycle::Stopped)
        {
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
        let host = slot.serving().ok_or_else(|| {
            AgentError::Context(format!(
                "capability '{}' process is not started",
                self.manifest.id
            ))
        })?;
        // Mid-invoke system requests stay brokered. A non-empty wire-effect
        // list stages only after the host proves the canonical actual intent
        // is covered by the approved bound; otherwise the path stays
        // fail-closed so a child cannot widen a narrow path authorization.
        let broker = InvokeFsBroker {
            id: &self.manifest.id,
            grant: &ctx.granted_permissions,
            workspace: ctx.workspace.as_ref(),
        };
        // The request is the trusted source of invocation identity. A
        // capability child may report content and status, but it must not
        // relabel the result as another call or tool at this boundary.
        let request_call_id = call.id.clone();
        let request_tool_name = call.name.clone();
        let allow_legacy = host.allows_feature(FEATURE_LEGACY_INVOKE_OUTPUT);
        let value = host
            .call_with_cancel_and_broker(
                json!({
                    "op": "invoke",
                    "call": call,
                    "permissions": &ctx.granted_permissions,
                }),
                &ctx.cancel,
                &broker,
            )
            .await?;
        let response = decode_process_invoke_response(
            value,
            request_call_id,
            request_tool_name,
            allow_legacy,
        )?;
        if response.effects.is_empty() {
            return Ok(CapabilityOutcome::Value(response.output));
        }
        stage_proven_wire_effects(&self.manifest.id, &ctx, response).await
    }
}

/// Decode the current `{output, effects}` envelope. The historical plain
/// `ToolOutput` shape is accepted only when `legacy.invoke-output.v1` was
/// crossed at ping; otherwise a child cannot silently reopen the old shape.
fn decode_process_invoke_response(
    value: Value,
    request_call_id: String,
    request_tool_name: String,
    allow_legacy: bool,
) -> AgentResult<ProcessInvokeResponse> {
    let is_current_envelope = value.get("output").is_some();
    let mut response = if is_current_envelope {
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode capability output: {e}")))?
    } else if allow_legacy {
        ProcessInvokeResponse {
            output: serde_json::from_value(value)
                .map_err(|e| AgentError::Context(format!("decode capability output: {e}")))?,
            effects: Vec::new(),
        }
    } else {
        return Err(AgentError::InvalidRequest(
            "plain ToolOutput is disabled unless legacy.invoke-output.v1 was negotiated at ping"
                .into(),
        ));
    };
    response.output.call_id = request_call_id;
    response.output.tool_name = request_tool_name;
    Ok(response)
}

fn unproven_wire_effects(id: &str, count: usize) -> AgentError {
    AgentError::InvalidRequest(format!(
        "capability '{id}' returned {count} wire effect(s), but process wire effects are disabled until the host can prove canonical actual intent is within the invocation lease; no workspace mutation was staged"
    ))
}

/// Stage process wire effects only when each actual write is covered by the
/// approved invocation intent. Identity without a covering bound, a
/// widened path, missing `workspace:write`, or an unconfined path stays
/// fail-closed and never calls `prepare_write`.
async fn stage_proven_wire_effects(
    id: &str,
    ctx: &CapabilityInvocationContext,
    response: ProcessInvokeResponse,
) -> AgentResult<CapabilityOutcome> {
    let count = response.effects.len();
    let [WireEffect::WorkspaceWrite { path, content_b64 }] = response.effects.as_slice() else {
        return Err(unproven_wire_effects(id, count));
    };
    if !ctx
        .granted_permissions
        .iter()
        .any(|permission| permission == WORKSPACE_WRITE)
    {
        return Err(unproven_wire_effects(id, count));
    }
    let Some(approved) = ctx.approved_intent.as_ref() else {
        return Err(unproven_wire_effects(id, count));
    };
    let confined = confined_relative_path(id, path)?;
    let content = decode_wire_content(id, content_b64)?;
    let actual = EffectIntent::WorkspaceWrite {
        path: confined.to_string(),
        content_bytes: content.len() as u64,
    };
    if !approved.covers(&actual) {
        return Err(unproven_wire_effects(id, count));
    }
    let workspace = ctx.workspace.as_ref().ok_or_else(|| {
        AgentError::InvalidRequest(format!(
            "capability '{id}' returned {count} wire effect(s), but process wire effects are disabled until the host can prove canonical actual intent is within the invocation lease; no workspace mutation was staged"
        ))
    })?;
    let effect = workspace.prepare_write(confined, &content).await?;
    Ok(CapabilityOutcome::EffectRequest {
        output: response.output,
        effect,
    })
}

fn decode_wire_content(id: &str, content_b64: &str) -> AgentResult<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(content_b64)
        .map_err(|_| {
            AgentError::InvalidRequest(format!(
                "capability '{id}' returned 1 wire effect(s), but process wire effects are disabled until the host can prove canonical actual intent is within the invocation lease; no workspace mutation was staged"
            ))
        })
}

fn confined_relative_path<'a>(id: &str, path: &'a str) -> AgentResult<&'a str> {
    let relative = std::path::Path::new(path);
    if path.is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(AgentError::InvalidRequest(format!(
            "capability '{id}' requested an absolute or empty path '{path}'"
        )));
    }
    if relative
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(AgentError::InvalidRequest(format!(
            "capability '{id}' requested an escaping path '{path}'"
        )));
    }
    Ok(path)
}

/// A broker for the child's mid-invoke system requests. Slice 1 brokers
/// filesystem reads: `{"system": "fs.read", "path": <relative>}` is
/// answered from the confined workspace handle when the invocation holds
/// `workspace:read`; every other op is refused. The broker is the
/// enforcement point for "experimental code cannot exceed the permissions
/// granted to it": the path is confined by construction (relative, no `..`,
/// never absolute) and the read goes through the workspace handle, never an
/// absolute filesystem path.
///
/// Network access is *deny-by-default by design*: no network permission word
/// exists anywhere in the permission vocabulary, so there is nothing to
/// grant. Recognized network ops are refused with an explicit message
/// instead of falling through to the generic unknown-op refusal, so the
/// policy is nameable and testable; everything else stays refused too.
struct InvokeFsBroker<'a> {
    id: &'a str,
    grant: &'a [String],
    workspace: Option<&'a Arc<dyn WorkspaceHandle>>,
}

/// The known network system ops. Recognized so the refusal names the policy;
/// the broker never consults a grant for these — none exists.
fn is_network_system_op(op: &str) -> bool {
    matches!(
        op,
        "net.fetch" | "net.connect" | "http.get" | "http.request"
    )
}

#[async_trait]
impl SystemBroker for InvokeFsBroker<'_> {
    async fn handle(&self, request: Value) -> AgentResult<Value> {
        match request.get("system").and_then(Value::as_str) {
            Some("fs.read") => self.handle_fs_read(&request).await,
            Some(op) if is_network_system_op(op) => Err(AgentError::InvalidRequest(format!(
                "capability '{}' requested network op '{op}': network access is deny-by-default, no network permission exists",
                self.id
            ))),
            Some(other) => Err(AgentError::InvalidRequest(format!(
                "capability '{}' requested unknown system op '{other}'",
                self.id
            ))),
            None => Err(AgentError::InvalidRequest(format!(
                "capability '{}' sent a malformed system request",
                self.id
            ))),
        }
    }
}

impl InvokeFsBroker<'_> {
    async fn handle_fs_read(&self, request: &Value) -> AgentResult<Value> {
        if !self
            .grant
            .iter()
            .any(|permission| permission == WORKSPACE_READ)
        {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{}' requested a brokered read without '{WORKSPACE_READ}' permission",
                self.id
            )));
        }
        let path = request
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::InvalidRequest("fs.read requires a string 'path'".into()))?;
        let relative = confined_relative_path(self.id, path)?;
        let workspace = self.workspace.ok_or_else(|| {
            AgentError::InvalidRequest(format!(
                "capability '{}' has no workspace handle: '{WORKSPACE_READ}' was not granted",
                self.id
            ))
        })?;
        let content = workspace
            .read_bounded(relative, BROKER_FS_READ_MAX_BYTES)
            .await?;
        // The control plane is not a file transport: a large file is
        // served as a bounded prefix with a truncation marker, never copied
        // base64-whole through the JSON pipe. The host caps the encoded
        // answer again; this bound keeps the broker's own work bounded too.
        Ok(json!({
            "content_b64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &content.content,
            ),
            "byte_len": content.byte_len,
            "truncated": content.truncated,
        }))
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

    fn output_value(call_id: &str, tool_name: &str) -> Value {
        serde_json::to_value(agent_contracts::ToolOutput {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            ok: true,
            summary: "done".into(),
            model_content: "bounded result".into(),
            artifact_ref: None,
            metadata: json!({}),
        })
        .unwrap()
    }

    #[test]
    fn structured_response_with_empty_effects_decodes_and_uses_request_identity() {
        let response = decode_process_invoke_response(
            json!({
                "output": output_value("forged-call", "forged.tool"),
                "effects": [],
            }),
            "requested-call".into(),
            "process-demo.invoke".into(),
            false,
        )
        .unwrap();

        assert!(response.effects.is_empty());
        assert_eq!(response.output.call_id, "requested-call");
        assert_eq!(response.output.tool_name, "process-demo.invoke");
        assert_eq!(response.output.model_content, "bounded result");
    }

    #[test]
    fn legacy_plain_output_is_rejected_unless_negotiated() {
        let error = decode_process_invoke_response(
            output_value("forged-call", "forged.tool"),
            "requested-call".into(),
            "process-demo.invoke".into(),
            false,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("legacy.invoke-output.v1"),
            "plain ToolOutput must stay closed without negotiation: {error}"
        );
    }

    #[test]
    fn legacy_plain_output_uses_request_identity_when_negotiated() {
        let response = decode_process_invoke_response(
            output_value("forged-call", "forged.tool"),
            "requested-call".into(),
            "process-demo.invoke".into(),
            true,
        )
        .unwrap();

        assert!(response.effects.is_empty());
        assert_eq!(response.output.call_id, "requested-call");
        assert_eq!(response.output.tool_name, "process-demo.invoke");
    }

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
