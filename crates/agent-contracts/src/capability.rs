//! The dynamic capability platform: the extension interface for capabilities
//! an LLM (or any external actor) can register at runtime.
//!
//! This is the second plane of the runtime's extension model. The first
//! plane — trusted core services — stays typed: `ServiceRegistry` lookups
//! for the high-frequency context/model/approval/event/artifact services.
//! The dynamic plane is a capability: a manifest plus a runtime object that
//! advertises tool schemas and handles calls. The module host turns the
//! advertised schemas into part of the runtime's tool provider, so a
//! registered capability is immediately callable by the model.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    AgentResult, CancellationToken, Effect, EffectIntent, RUNTIME_CONTEXT_CONTROL,
    RuntimeDirective, ToolCall, ToolOutput, ToolSpec,
};

/// A prefix read whose allocation is bounded before I/O begins.
///
/// `byte_len` is the size reported by the already-open file handle when the
/// read started. `truncated` records whether that observed size exceeded the
/// requested bound. As with any file read, concurrent mutation can make the
/// observed size differ from the bytes available while the read runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedRead {
    /// The file prefix, never larger than the caller's requested bound.
    pub content: Vec<u8>,
    /// Size reported by metadata on the pinned file handle before reading.
    pub byte_len: u64,
    /// Whether `byte_len` exceeded the caller's requested bound.
    pub truncated: bool,
}

/// When a capability's service is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityLifecycle {
    /// Started when the host starts.
    Eager,
    /// Started on first invocation.
    Lazy,
}

/// Maturity of a capability on the self-improvement ladder. External
/// capabilities always enter as `Experimental` — the LLM cannot declare its
/// own module `Stable`; the ladder is climbed through testing and
/// validation (the replay/evaluation infrastructure is the future
/// evaluator), not by declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// Newly published; not yet exercised by real runs.
    #[default]
    Experimental,
    /// Exercised by replay/evaluation scenarios.
    Tested,
    /// Passed validation against the evaluation gate.
    Validated,
    /// Approved for normal use.
    Stable,
    /// Superseded; kept only for migration warnings.
    Deprecated,
}

impl CapabilityStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Experimental => "experimental",
            Self::Tested => "tested",
            Self::Validated => "validated",
            Self::Stable => "stable",
            Self::Deprecated => "deprecated",
        }
    }
}

/// Whether a registered capability is usable at all. Maturity
/// (`CapabilityStatus`) says how good a capability is; activation says
/// whether the runtime will run it. The two are independent axes: an
/// external capability enters as `Experimental` + `Disabled`, so an LLM
/// cannot publish a module and immediately run it inside the agent —
/// enabling is an operator/evaluator action, and a misbehaving capability
/// can be suspended without unregistering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityActivation {
    /// Registered but not usable until explicitly enabled. The default for
    /// external (out-of-process) capabilities.
    #[default]
    Disabled,
    /// Usable: tools can be loaded onto the model surface and invoked.
    Enabled,
    /// Suspended after misbehavior or operator action; nothing runs until
    /// an explicit re-enable.
    Quarantined,
}

impl CapabilityActivation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Quarantined => "quarantined",
        }
    }

    /// Whether the runtime may load and run this capability right now.
    pub const fn usable(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// What a capability provides to the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// A callable tool the model can invoke.
    Tool,
    /// A multi-step, parameterized procedure built from tools.
    Skill,
    /// A background/typed service the runtime talks to.
    Service,
}

/// How a capability's service is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTransport {
    /// In-process trusted core. The first version keeps the trusted core
    /// in-process and never loads Rust plugins: the ABI is not a stable
    /// plugin boundary, and a crashed plugin must not take the runtime down.
    #[serde(alias = "InProcess", alias = "in_process")]
    Builtin,
    /// A separate process speaking a framed protocol (the context-service
    /// shape: versioned handshake, per-request deadlines, frame bounds).
    /// The preferred plane for LLM/third-party extensions, with sandboxing
    /// as a later concern.
    #[serde(alias = "Process")]
    Process { program: String },
    // Future: Wasm — sandboxed in-process plugins. Deliberately not part of
    // the first version.
}

/// `workspace:read` — a confined read-only view of the workspace root.
pub const WORKSPACE_READ: &str = "workspace:read";
/// `workspace:write` — journaled writes inside the workspace root. The
/// runtime hands a capability the *staged* write path only: the capability
/// computes, the core commits behind the generation fence.
pub const WORKSPACE_WRITE: &str = "workspace:write";
/// `process:run` — the capability may spawn subprocesses (declared for
/// process transport, where the child already is one).
pub const PROCESS_RUN: &str = "process:run";
/// `artifact:write` prefix — the capability may store outputs under the
/// run's artifact directory.
pub const ARTIFACT_WRITE: &str = "artifact:write";

/// A permission a capability may declare. Unknown permission strings are
/// rejected at registration — the runtime denies undeclared access by
/// refusing the declaration in the first place.
pub fn is_known_permission(permission: &str) -> bool {
    permission == WORKSPACE_READ
        || permission == WORKSPACE_WRITE
        || permission == PROCESS_RUN
        || permission == RUNTIME_CONTEXT_CONTROL
        || permission.starts_with(ARTIFACT_WRITE)
}

/// Whether a permission implies side effects (something the approval gate
/// must see and the effect fence must stage). `workspace:read` is the only
/// read-only permission; everything else is a mutation or an execution.
pub fn permission_is_side_effecting(permission: &str) -> bool {
    permission != WORKSPACE_READ
}

/// Conservative grammar for a capability id: lowercase ASCII letters,
/// digits, `.`, `_` or `-`, first character a lowercase letter or digit,
/// at most 64 chars. The id is embedded in directory names and protocol
/// routes, so anything outside this set is a path/route injection risk and
/// is rejected — a capability id is identity, not free text.
pub fn validate_capability_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("capability id is empty".into());
    }
    if id.len() > 64 {
        return Err(format!(
            "capability id '{}' is {} chars (allowed 1..=64)",
            id,
            id.len()
        ));
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!(
            "capability id '{id}' must start with a lowercase letter or digit"
        ));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "capability id '{id}' may only contain lowercase [a-z0-9._-]"
        ));
    }
    Ok(())
}

/// The declarative part of a dynamic capability: stable identity, a human
/// summary, what it provides and requires, declared permissions, and its
/// lifecycle and transport shape. The host validates `requires` at
/// registration; declared permissions become enforced `WorkspaceHandle` /
/// `ArtifactHandle` views inside each invocation's context (the tools also
/// carry `ToolRisk` levels that the approval gate enforces).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityManifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub summary: String,
    /// Maturity on the Experimental -> Stable ladder. External registrations
    /// are pinned to Experimental by the registry, whatever is declared here.
    #[serde(default)]
    pub status: CapabilityStatus,
    /// What this capability provides to the platform.
    #[serde(default)]
    pub provides: Vec<CapabilityKind>,
    /// Declared permissions, e.g. "workspace:read", "process:run".
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Capability ids this one requires; the host rejects registration when
    /// a requirement is missing. The old field name `dependencies` still
    /// deserializes for compatibility.
    #[serde(default, alias = "dependencies")]
    pub requires: Vec<String>,
    /// The tool schemas this capability serves. In-process capabilities
    /// implement `Capability::tool_specs` directly; an out-of-process
    /// capability declares them here so the adapter can advertise them
    /// without starting the process (the trait's default `tool_specs`
    /// returns this list).
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    pub lifecycle: CapabilityLifecycle,
    pub transport: CapabilityTransport,
    /// Isolation floor the host must actually enforce before this
    /// capability may start. Manifests cannot self-attest: Runtime compares
    /// this to post-spawn [`SandboxCapabilities`]. `Trusted` requires
    /// nothing extra (operator-installed). `UntrustedGenerated` fails
    /// closed when the native process plane cannot prove the floor.
    #[serde(default)]
    pub sandbox_profile: SandboxProfile,
}

/// Isolation floor a capability declares. The host compares this to
/// *actually enforced* sandbox capabilities after spawn, not to configured
/// policy. Do not invent another `MOD-xx` slice to paper over residual
/// syscalls; fail closed for untrusted generated code instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProfile {
    /// Operator-trusted / semi-trusted integration. Missing OS fences may
    /// degrade with a warning.
    #[default]
    Trusted,
    /// Extra write/memory/spawn floors. Still native-process.
    Restricted,
    /// LLM-generated untrusted code. Activation requires
    /// `required ⊆ actually_enforced`. Native process currently cannot
    /// satisfy UDP/pathname-Unix/OS-read confinement; that is fail-closed
    /// on purpose (WASI is the V2 candidate for this profile).
    UntrustedGenerated,
}

/// Capabilities the host actually enforced on a child, not the configured
/// wish-list. `false` means "not proven".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub fs_read_confined: bool,
    pub fs_write_confined: bool,
    pub tcp_connect_denied: bool,
    pub udp_denied: bool,
    pub unix_socket_denied: bool,
    /// `RLIMIT_NPROC` / Windows JobObject: a *count quota*, not proof that
    /// arbitrary spawning is impossible. Named for the guarantee it
    /// actually provides; a true spawn-denied/brokered floor is a separate
    /// future item, not this flag.
    #[serde(alias = "process_spawn_controlled")]
    pub process_count_quota: bool,
    pub signal_scoped: bool,
    pub cpu_quota: bool,
    pub memory_quota: bool,
    pub fd_quota: bool,
}

impl SandboxCapabilities {
    /// Every `true` flag on `self` is also `true` on `actual`.
    pub fn subset_of(self, actual: Self) -> bool {
        flag_subset(self.fs_read_confined, actual.fs_read_confined)
            && flag_subset(self.fs_write_confined, actual.fs_write_confined)
            && flag_subset(self.tcp_connect_denied, actual.tcp_connect_denied)
            && flag_subset(self.udp_denied, actual.udp_denied)
            && flag_subset(self.unix_socket_denied, actual.unix_socket_denied)
            && flag_subset(self.process_count_quota, actual.process_count_quota)
            && flag_subset(self.signal_scoped, actual.signal_scoped)
            && flag_subset(self.cpu_quota, actual.cpu_quota)
            && flag_subset(self.memory_quota, actual.memory_quota)
            && flag_subset(self.fd_quota, actual.fd_quota)
    }
}

fn flag_subset(required: bool, actual: bool) -> bool {
    !required || actual
}

impl SandboxProfile {
    pub fn required(self) -> SandboxCapabilities {
        match self {
            Self::Trusted => SandboxCapabilities::default(),
            Self::Restricted => SandboxCapabilities {
                fs_write_confined: true,
                memory_quota: true,
                process_count_quota: true,
                ..SandboxCapabilities::default()
            },
            Self::UntrustedGenerated => SandboxCapabilities {
                // Read confinement and CPU are part of the untrusted floor
                // *now*, before the OS planes can prove them: once UDP /
                // pathname-Unix denials land, a profile that forgot
                // fs-read/CPU would pass activation with absolute host
                // reads and unlimited CPU still open — a containment hole
                // rented from the future. Requiring them today keeps the
                // fail-closed posture honest (native cannot attest either
                // yet, so UntrustedGenerated still refuses to start).
                fs_read_confined: true,
                fs_write_confined: true,
                tcp_connect_denied: true,
                udp_denied: true,
                unix_socket_denied: true,
                process_count_quota: true,
                signal_scoped: true,
                cpu_quota: true,
                memory_quota: true,
                fd_quota: true,
            },
        }
    }

    /// Trusted may start under a weaker actual sandbox. Restricted and
    /// UntrustedGenerated fail closed when the floor is missing.
    pub fn allows_start(self, actual: SandboxCapabilities) -> bool {
        self.required().subset_of(actual)
    }
}

/// A confined view of the agent's workspace handed to a capability for one
/// invocation. Capabilities never hold the workspace, the engine or the
/// memory stores directly — everything they can touch is granted here, and
/// the runtime-owned implementation enforces the boundary (path confinement
/// against the workspace root, mutation through the journaled transaction,
/// no access to the runtime state directory).
#[async_trait]
pub trait WorkspaceHandle: Send + Sync {
    /// The confined root every resolved path stays under.
    fn root(&self) -> &Path;

    /// Resolve a relative path against the workspace root; absolute paths,
    /// `..` escapes and symlink redirects are rejected.
    async fn resolve(&self, relative: &str) -> AgentResult<PathBuf>;

    /// Read a file, confined like `resolve`.
    async fn read(&self, relative: &str) -> AgentResult<Vec<u8>>;

    /// Read at most `max_bytes` from the start of a file, confined like
    /// `resolve`. Implementations must apply the bound while reading; they
    /// must never implement this as a full `read` followed by truncation.
    async fn read_bounded(&self, relative: &str, max_bytes: usize) -> AgentResult<BoundedRead>;

    /// Write a file through the runtime's journaled, atomic mutation
    /// transaction, confined like `resolve`.
    async fn write(&self, relative: &str, content: &[u8]) -> AgentResult<()>;

    /// Stage a journaled write without applying it: the returned `Effect`
    /// is committed by the runtime after the generation fence, exactly like
    /// a builtin tool's `PreparedEffect`. Capabilities that want their
    /// side effects behind the effect fence use this instead of `write` —
    /// the capability stages, the core executes.
    async fn prepare_write(&self, relative: &str, content: &[u8]) -> AgentResult<Box<dyn Effect>>;
}

/// A confined view of the artifact store: large outputs land under the
/// run's artifact directory and come back as `artifact://` references
/// (bounded model-facing output, invariant 4).
#[async_trait]
pub trait ArtifactHandle: Send + Sync {
    /// Store bytes under the run's artifact directory and return the
    /// artifact reference for `ToolOutput::artifact_ref`.
    async fn store(&self, name: &str, bytes: &[u8]) -> AgentResult<String>;
}

/// Everything a capability may touch during one invocation. Built by the
/// runtime at execute time from the manifest's declared permissions and the
/// handles the composition root wired in; capabilities receive this instead
/// of raw process/engine access, so declared permissions are enforced by
/// construction, not by trust.
#[derive(Clone)]
pub struct CapabilityInvocationContext {
    /// The permissions granted for this invocation (the manifest's declared
    /// permissions, as approved by the runtime). Informational — the
    /// enforcement is the handles below plus the kernel's approval gate.
    pub granted_permissions: Vec<String>,
    /// Confined workspace access; present only when the manifest declared
    /// `workspace:read` (read-only view) or `workspace:write` (journaled
    /// writes allowed).
    pub workspace: Option<Arc<dyn WorkspaceHandle>>,
    /// Confined artifact-store access; present only when declared.
    pub artifacts: Option<Arc<dyn ArtifactHandle>>,
    /// Cooperative cancellation for this invocation (the execution
    /// request's token), so long-running capability calls can be aborted.
    pub cancel: CancellationToken,
    /// Approved concrete intent for this invocation (upper bound from
    /// [`crate::derive_effect_intent`]). Process wire effects may stage
    /// only when the host-canonical actual intent is [`EffectIntent::covers`]
    /// by this bound. `None` means the host cannot prove coverage, so
    /// non-empty wire effects stay fail-closed.
    pub approved_intent: Option<EffectIntent>,
}

/// What a capability invocation produced. The core owns *all* side-effect
/// execution: a capability either returns a plain bounded output, stages an
/// `EffectRequest` for the runtime to commit (behind the generation fence,
/// like a builtin tool), or attaches a `RuntimeDirective` — which the
/// dispatcher only forwards when the manifest declares
/// `RUNTIME_CONTEXT_CONTROL`. A capability never applies a side effect
/// directly; it submits it.
pub enum CapabilityOutcome {
    /// The invocation produced only an output; nothing to commit.
    Value(ToolOutput),
    /// The invocation stages a side effect for the core to commit after
    /// the generation fence (the capability computes, the core executes).
    EffectRequest {
        output: ToolOutput,
        effect: Box<dyn Effect>,
    },
    /// The invocation asks the runtime to change runtime-owned state.
    /// Dispatchers enforce `RUNTIME_CONTEXT_CONTROL` before forwarding.
    RuntimeDirective {
        output: ToolOutput,
        directive: RuntimeDirective,
    },
}

/// One structured side effect a *process* capability asks the runtime to
/// commit. The child never applies the mutation itself: it declares intent
/// over the wire, the adapter validates it against the capability's
/// declared permissions and stages it through the confined workspace
/// handle, and the runtime commits it behind the generation fence — the
/// capability computes, the core executes, exactly like a builtin tool's
/// `PreparedEffect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WireEffect {
    /// Write a file inside the workspace root. Requires the capability to
    /// have declared `workspace:write`; the adapter resolves and stages the
    /// write through the confined handle, never on the child's behalf
    /// directly. Content is base64 so arbitrary bytes cross JSON safely.
    WorkspaceWrite { path: String, content_b64: String },
}

/// The wire response of a process-capability `invoke`: the bounded output
/// plus the structured effects the child asks the runtime to commit.
/// A plain `ToolOutput` is accepted only after `legacy.invoke-output.v1`
/// is crossed at ping; the current shape is `{output, effects}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInvokeResponse {
    pub output: ToolOutput,
    #[serde(default)]
    pub effects: Vec<WireEffect>,
}

impl std::fmt::Debug for CapabilityOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Value(output) => f.debug_tuple("Value").field(output).finish(),
            Self::EffectRequest { output, .. } => f
                .debug_struct("EffectRequest")
                .field("output", output)
                .field("effect", &"<staged effect>")
                .finish(),
            Self::RuntimeDirective { output, directive } => f
                .debug_struct("RuntimeDirective")
                .field("output", output)
                .field("directive", directive)
                .finish(),
        }
    }
}

/// The runtime object behind a capability: a manifest, the tool schemas it
/// exposes to the model, and the invocation handler. The chain is
/// `Service -> Capability -> Tool Schema -> LLM`: a registered capability's
/// tools join the runtime's tool provider, and model calls route back to
/// `invoke`.
#[async_trait]
pub trait Capability: Send + Sync {
    fn manifest(&self) -> &CapabilityManifest;

    /// The tool schemas this capability contributes to the model. Defaults
    /// to the manifest's declared `tools` — the shape an out-of-process
    /// capability uses; in-process capabilities override it.
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.manifest().tools.clone()
    }

    /// Handle one tool call routed to this capability. The context carries
    /// the granted permissions and confined handles; the capability must
    /// route all workspace/artifact access through them.
    async fn invoke(
        &self,
        call: ToolCall,
        ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome>;

    async fn start(&self) -> AgentResult<()> {
        Ok(())
    }

    async fn stop(&self) -> AgentResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_id_grammar_is_conservative() {
        // Acceptable: lowercase start, path-safe chars, bounded length.
        for id in [
            "a",
            "process-demo",
            "cap.1_x",
            "a1",
            "x".repeat(64).as_str(),
        ] {
            assert!(validate_capability_id(id).is_ok(), "should accept {id:?}");
        }
        // Rejected: anything that could escape a path or a route.
        for id in [
            "",
            "Uppercase",
            "../escape",
            "a/b",
            "a\\b",
            "a b",
            "a:b",
            "a;rm",
            "a$b",
            "-leading",
            ".leading",
            "x".repeat(65).as_str(),
        ] {
            assert!(
                validate_capability_id(id).is_err(),
                "should reject {id:?} — a capability id is identity, not free text"
            );
        }
    }

    #[test]
    fn permission_classification_matches_the_boundary() {
        assert!(is_known_permission(WORKSPACE_READ));
        assert!(is_known_permission(WORKSPACE_WRITE));
        assert!(is_known_permission(PROCESS_RUN));
        assert!(is_known_permission(RUNTIME_CONTEXT_CONTROL));
        assert!(is_known_permission("artifact:write"));
        assert!(!is_known_permission("fs:everything"));
        assert!(!is_known_permission("network:anywhere"));

        assert!(!permission_is_side_effecting(WORKSPACE_READ));
        assert!(permission_is_side_effecting(WORKSPACE_WRITE));
        assert!(permission_is_side_effecting(PROCESS_RUN));
        assert!(permission_is_side_effecting(RUNTIME_CONTEXT_CONTROL));
        assert!(permission_is_side_effecting("artifact:write"));
    }

    #[test]
    fn wire_effect_round_trips_binary_content_over_json() {
        // A process capability's mutation intent crosses the boundary as a
        // tagged JSON object; base64 keeps arbitrary bytes safe through
        // serde_json. The decode must reconstruct the exact byte sequence —
        // a text-only wire would silently mangle binary file content.
        let binary: Vec<u8> = (0u8..=255u8).collect();
        let content_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &binary);
        let effect = WireEffect::WorkspaceWrite {
            path: "out.bin".into(),
            content_b64,
        };

        let value = serde_json::to_value(&effect).unwrap();
        assert_eq!(value["op"], "workspace_write", "the tag must be snake_case");
        assert_eq!(value["path"], "out.bin");
        assert!(value["content_b64"].is_string());

        let back: WireEffect = serde_json::from_value(value).unwrap();
        match back {
            WireEffect::WorkspaceWrite { path, content_b64 } => {
                assert_eq!(path, "out.bin");
                let decoded = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &content_b64,
                )
                .expect("the wire payload must be valid base64");
                assert_eq!(decoded, binary, "every byte must survive the round trip");
            }
        }
    }

    #[test]
    fn untrusted_generated_fails_closed_without_full_native_attestation() {
        let trusted_actual = SandboxCapabilities {
            fs_write_confined: true,
            memory_quota: true,
            process_count_quota: true,
            ..SandboxCapabilities::default()
        };
        assert!(SandboxProfile::Trusted.allows_start(SandboxCapabilities::default()));
        assert!(SandboxProfile::Restricted.allows_start(trusted_actual));
        assert!(
            !SandboxProfile::UntrustedGenerated.allows_start(trusted_actual),
            "native process without UDP/Unix/TCP proof must not start untrusted generated code"
        );
        let full = SandboxCapabilities {
            fs_read_confined: true,
            fs_write_confined: true,
            tcp_connect_denied: true,
            udp_denied: true,
            unix_socket_denied: true,
            process_count_quota: true,
            signal_scoped: true,
            cpu_quota: true,
            memory_quota: true,
            fd_quota: true,
        };
        assert!(SandboxProfile::UntrustedGenerated.allows_start(full));

        // The floor includes fs-read confinement and CPU *now*, so a future
        // native plane that proves UDP/Unix denial still cannot activate
        // untrusted generated code while absolute host reads or unlimited
        // CPU remain open.
        let without_fs_read = SandboxCapabilities {
            fs_read_confined: false,
            ..full
        };
        assert!(
            !SandboxProfile::UntrustedGenerated.allows_start(without_fs_read),
            "missing fs-read confinement must keep the profile fail-closed"
        );
        let without_cpu = SandboxCapabilities {
            cpu_quota: false,
            ..full
        };
        assert!(
            !SandboxProfile::UntrustedGenerated.allows_start(without_cpu),
            "missing CPU quota must keep the profile fail-closed"
        );
    }

    #[test]
    fn process_count_quota_field_name_keeps_wire_compatibility() {
        // The flag was renamed from `process_spawn_controlled`; frames a
        // pre-rename peer sent (full-field attestations under the old
        // name) must still deserialize.
        let full = SandboxCapabilities {
            process_count_quota: true,
            memory_quota: true,
            ..SandboxCapabilities::default()
        };
        let legacy = serde_json::to_string(&full)
            .unwrap()
            .replace("process_count_quota", "process_spawn_controlled");
        let decoded: SandboxCapabilities = serde_json::from_str(&legacy).unwrap();
        assert!(decoded.process_count_quota);
        assert!(decoded.memory_quota);
        let encoded = serde_json::to_string(&full).unwrap();
        assert!(
            encoded.contains("process_count_quota"),
            "new frames carry the honest name: {encoded}"
        );
    }
}
