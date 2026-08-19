//! Capability admission: the core-side authority that decides whether a
//! dynamic capability may enter the runtime at all, and with what maturity
//! and activation it starts. Admission is a *core* decision — the runtime
//! asks, the core admits — so the checks (schema caps, authority
//! derivation, maturity pinning) live here, not in the runtime's mutable
//! registry. Everything is a pure function of the manifest and its declared
//! tool schemas except the collision pass, which the registry feeds with its
//! own live state through [`AdmissionContext`]; the authority never touches
//! the registry.

use std::collections::HashSet;

use agent_contracts::{
    AgentError, AgentResult, CapabilityActivation, CapabilityManifest, CapabilityStatus,
    CapabilityTransport, PROCESS_RUN, ToolRisk, ToolSpec, WORKSPACE_WRITE, is_known_permission,
    validate_capability_id,
};

/// Registration limits for capability-declared tool schemas: a single
/// capability must not be able to grow the model surface without bound — a
/// huge schema, a huge description or a huge tool count is itself context
/// pollution. The limits are enforced at registration (validated once, then
/// cached), so a runaway capability is rejected before it ever reaches the
/// catalog.
pub const MAX_TOOLS_PER_CAPABILITY: usize = 32;
pub const MAX_TOOL_NAME_CHARS: usize = 64;
pub const MAX_TOOL_DESCRIPTION_CHARS: usize = 200;
pub const MAX_TOOL_SCHEMA_BYTES: usize = 4 * 1024;

/// The stateless admission authority for dynamic capabilities. All checks
/// are pure functions of the manifest + declared tool schemas, so the same
/// admission rules apply no matter which registry (or future host) asks.
#[derive(Debug, Default, Clone, Copy)]
pub struct CapabilityAdmission;

impl CapabilityAdmission {
    /// Validate everything about a registration that depends only on the
    /// manifest and its declared tool schemas — no registry state. Runs
    /// before the registry's lock, so a slow, re-entrant or panicking
    /// capability implementation can only stall at register time, never
    /// under the registry's lock.
    pub fn validate_static(
        manifest: &CapabilityManifest,
        tool_specs: &[ToolSpec],
    ) -> AgentResult<()> {
        // The id is identity: it is validated before anything derived from
        // it (tool names, routes, directories).
        validate_capability_id(&manifest.id).map_err(AgentError::InvalidRequest)?;
        validate_tool_specs(&manifest.id, tool_specs)?;
        validate_manifest_authority(manifest, tool_specs)?;
        Ok(())
    }

    /// Validate the collision pass, which needs the registry's live state:
    /// duplicate ids, missing declared dependencies, tool names that shadow
    /// the runtime's own, and tool names already owned by another
    /// capability. The registry builds an [`AdmissionContext`] from its own
    /// internals and calls this under its write lock; the model must never
    /// see a half-wired tool or an ambiguous route.
    pub fn validate_collisions(
        manifest: &CapabilityManifest,
        tool_names: &[&str],
        ctx: &AdmissionContext<'_>,
    ) -> AgentResult<()> {
        if (ctx.is_registered)(&manifest.id) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{}' is already registered",
                manifest.id
            )));
        }
        for requirement in &manifest.requires {
            if !(ctx.is_registered)(requirement) {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' requires '{}' which is not registered",
                    manifest.id, requirement
                )));
            }
        }
        for name in tool_names {
            if ctx.reserved_names.contains(*name) {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' declares tool '{name}', which is reserved by the runtime; capabilities cannot shadow core tools",
                    manifest.id
                )));
            }
        }
        for name in tool_names {
            if let Some(owner) = (ctx.owner_of_tool)(name) {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' declares tool '{name}', which is already owned by capability '{owner}'",
                    manifest.id
                )));
            }
        }
        Ok(())
    }

    /// The maturity ladder is climbed, not declared: out-of-process
    /// (external/LLM-authored) capabilities always start Experimental, so
    /// an LLM cannot promote its own module to Stable.
    pub fn initial_status(manifest: &CapabilityManifest) -> CapabilityStatus {
        if manifest.transport != CapabilityTransport::Builtin
            && manifest.status != CapabilityStatus::Experimental
        {
            CapabilityStatus::Experimental
        } else {
            manifest.status
        }
    }

    /// Activation is granted, not declared: only the trusted in-process
    /// core is usable immediately; external capabilities enter Disabled
    /// and need an explicit enable before anything runs.
    pub fn initial_activation(manifest: &CapabilityManifest) -> CapabilityActivation {
        if manifest.transport == CapabilityTransport::Builtin {
            CapabilityActivation::Enabled
        } else {
            CapabilityActivation::Disabled
        }
    }
}

/// Static facts a registry feeds into the collision pass. The registry
/// constructs this from its own live state (its registered ids, its
/// reserved tool names, its tool ownership map) — the authority never
/// touches the registry; the registry hands it a view.
pub struct AdmissionContext<'a> {
    /// Whether an id is already registered in the registry.
    pub is_registered: &'a dyn Fn(&str) -> bool,
    /// Tool names the runtime owns (builtin + control); capabilities may
    /// never shadow them: routing would otherwise be hijackable by
    /// declaration.
    pub reserved_names: &'a HashSet<String>,
    /// The id of the capability that already owns a tool name, if any.
    pub owner_of_tool: &'a dyn Fn(&str) -> Option<String>,
}

/// Validate the tool schemas a capability (or plugin package) declares at
/// admission: name shape/length, description length, per-schema byte size,
/// duplicate names within the owner, and the per-owner tool count. Shared
/// by capability and plugin admission so a package cannot smuggle in a
/// schema the capability plane would refuse.
pub(crate) fn validate_tool_specs(manifest_id: &str, specs: &[ToolSpec]) -> AgentResult<()> {
    if specs.len() > MAX_TOOLS_PER_CAPABILITY {
        return Err(AgentError::InvalidRequest(format!(
            "capability '{manifest_id}' declares {} tools, above the {MAX_TOOLS_PER_CAPABILITY} per-capability cap",
            specs.len()
        )));
    }
    let mut names = std::collections::HashSet::new();
    for spec in specs {
        if spec.name.is_empty() || spec.name.len() > MAX_TOOL_NAME_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' declares a tool name of {} chars (allowed 1..={MAX_TOOL_NAME_CHARS})",
                spec.name.len()
            )));
        }
        let well_formed = spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'));
        if !well_formed {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' declares tool name '{}': only [A-Za-z0-9._:-] are allowed",
                spec.name
            )));
        }
        if !names.insert(spec.name.clone()) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' declares tool '{name}' twice",
                name = spec.name
            )));
        }
        if spec.description.len() > MAX_TOOL_DESCRIPTION_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' tool '{}' description is {} chars, above the {MAX_TOOL_DESCRIPTION_CHARS} cap",
                spec.name,
                spec.description.len()
            )));
        }
        let bytes = serde_json::to_vec(&spec.input_schema)
            .unwrap_or_default()
            .len();
        if bytes > MAX_TOOL_SCHEMA_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' tool '{}' input schema is {bytes} bytes, above the {MAX_TOOL_SCHEMA_BYTES} cap",
                spec.name
            )));
        }
    }
    Ok(())
}

/// Validate the authority a manifest declares against what the runtime will
/// actually enforce. The approval gate auto-allows `ReadOnly` tools, so the
/// risk label must be *derived* from the declared authority, never
/// self-declared by a side-effecting capability:
///
/// - every declared permission must be a known permission string (unknown
///   access is denied by refusing the declaration);
/// - a capability that declares any side-effecting permission may not mark
///   any tool `ReadOnly` (a process that can write must not auto-allow);
/// - a tool's risk may not exceed its grant (a `WorkspaceWrite` tool needs
///   `workspace:write`, a `ProcessExecution` tool needs `process:run`);
/// - a process-transport capability may declare `workspace:write` because
///   the wire effect broker stages its mutations: the adapter validates the
///   child's structured wire effects against the grant and commits them
///   through the confined handle behind the generation fence.
fn validate_manifest_authority(
    manifest: &CapabilityManifest,
    tool_specs: &[ToolSpec],
) -> AgentResult<()> {
    for permission in &manifest.permissions {
        if !is_known_permission(permission) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{}' declares unknown permission '{permission}'; allowed: workspace:read, workspace:write, process:run, runtime:context-control, artifact:*",
                manifest.id
            )));
        }
    }
    let declares_approval_gated_mutation = manifest
        .permissions
        .iter()
        .any(|p| p == WORKSPACE_WRITE || p == PROCESS_RUN);
    if declares_approval_gated_mutation {
        for spec in tool_specs {
            if spec.risk == ToolRisk::ReadOnly {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' declares workspace-write/process-run authority but tool '{}' self-declares ReadOnly; risk is derived from declared authority, never self-declared (ReadOnly auto-allows at the approval gate)",
                    manifest.id, spec.name
                )));
            }
        }
    }
    for spec in tool_specs {
        match spec.risk {
            ToolRisk::WorkspaceWrite => {
                if !manifest.permissions.iter().any(|p| p == WORKSPACE_WRITE) {
                    return Err(AgentError::InvalidRequest(format!(
                        "capability '{}' tool '{}' needs the '{WORKSPACE_WRITE}' permission, which is not declared",
                        manifest.id, spec.name
                    )));
                }
            }
            ToolRisk::ProcessExecution => {
                if !manifest.permissions.iter().any(|p| p == PROCESS_RUN) {
                    return Err(AgentError::InvalidRequest(format!(
                        "capability '{}' tool '{}' needs the '{PROCESS_RUN}' permission, which is not declared",
                        manifest.id, spec.name
                    )));
                }
            }
            ToolRisk::ReadOnly => {}
        }
    }
    // A process capability may declare `workspace:write` — but only because
    // the wire effect broker exists: the child stages structured wire
    // effects and the adapter commits them through the confined workspace
    // handle behind the generation fence. The child itself never writes.
    // Enforcement of the write path is the adapter's job; a process whose
    // adapter is not the wire-brokering one simply cannot be registered
    // (there is only one adapter).
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::CapabilityLifecycle;
    use serde_json::json;

    fn manifest(id: &str, permissions: &[&str], requires: &[&str]) -> CapabilityManifest {
        CapabilityManifest {
            id: id.into(),
            version: "0.1.0".into(),
            name: id.into(),
            summary: "test".into(),
            status: CapabilityStatus::Experimental,
            provides: Vec::new(),
            permissions: permissions.iter().map(|p| (*p).to_string()).collect(),
            requires: requires.iter().map(|r| (*r).to_string()).collect(),
            tools: Vec::new(),
            lifecycle: CapabilityLifecycle::Lazy,
            transport: CapabilityTransport::Builtin,
        }
    }

    fn tool(name: &str, risk: ToolRisk) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "test tool".into(),
            input_schema: json!({"type": "object"}),
            risk,
            output_budget: None,
            roles: Vec::new(),
        }
    }

    #[test]
    fn static_validation_rejects_oversized_schema() {
        let m = manifest("big", &[], &[]);
        let specs = vec![ToolSpec {
            name: "big.run".into(),
            description: "x".into(),
            input_schema: json!({"padding": "x".repeat(5 * 1024)}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }];
        let error = CapabilityAdmission::validate_static(&m, &specs)
            .expect_err("an oversized schema must be rejected");
        assert!(error.to_string().contains("schema"), "{error}");
    }

    #[test]
    fn static_validation_rejects_tool_count_above_the_cap() {
        let m = manifest("many", &[], &[]);
        let specs: Vec<ToolSpec> = (0..40)
            .map(|i| tool(&format!("many.t{i}"), ToolRisk::ReadOnly))
            .collect();
        let error = CapabilityAdmission::validate_static(&m, &specs)
            .expect_err("a tool count above the cap must be rejected");
        assert!(error.to_string().contains("per-capability cap"), "{error}");
    }

    #[test]
    fn static_validation_rejects_malformed_tool_name() {
        let m = manifest("bad-name", &[], &[]);
        let specs = vec![tool("bad name!", ToolRisk::ReadOnly)];
        let error = CapabilityAdmission::validate_static(&m, &specs)
            .expect_err("a malformed tool name must be rejected");
        assert!(error.to_string().contains("[A-Za-z0-9._:-]"), "{error}");
    }

    #[test]
    fn static_validation_rejects_unknown_permission() {
        let m = manifest("perm", &["workspace:teleport"], &[]);
        let specs = vec![tool("perm.run", ToolRisk::ReadOnly)];
        let error = CapabilityAdmission::validate_static(&m, &specs)
            .expect_err("an unknown permission must be rejected");
        assert!(error.to_string().contains("unknown permission"), "{error}");
    }

    #[test]
    fn static_validation_derives_risk_from_declared_authority() {
        // A write-permissioned capability must not self-declare ReadOnly:
        // ReadOnly auto-allows at the approval gate.
        let m = manifest("write-tool", &["workspace:write"], &[]);
        let specs = vec![tool("write-tool.run", ToolRisk::ReadOnly)];
        let error = CapabilityAdmission::validate_static(&m, &specs)
            .expect_err("a mutating capability must not self-declare ReadOnly");
        assert!(error.to_string().contains("ReadOnly"), "{error}");

        // A tool whose risk exceeds its grant is refused.
        let m = manifest("over-granted", &["workspace:read"], &[]);
        let specs = vec![tool("over-granted.run", ToolRisk::WorkspaceWrite)];
        let error = CapabilityAdmission::validate_static(&m, &specs)
            .expect_err("a tool may not exceed its grant");
        assert!(error.to_string().contains("workspace:write"), "{error}");
    }

    #[test]
    fn static_validation_admits_a_well_formed_capability() {
        let m = manifest("ok", &["workspace:read"], &[]);
        let specs = vec![tool("ok.run", ToolRisk::ReadOnly)];
        CapabilityAdmission::validate_static(&m, &specs)
            .expect("a well-formed capability must pass");
    }

    #[test]
    fn collision_validation_rejects_duplicate_id() {
        let m = manifest("dup", &[], &[]);
        let is_registered = |id: &str| id == "dup";
        let reserved = HashSet::new();
        let owner_of_tool = |_: &str| None::<String>;
        let ctx = AdmissionContext {
            is_registered: &is_registered,
            reserved_names: &reserved,
            owner_of_tool: &owner_of_tool,
        };
        let error = CapabilityAdmission::validate_collisions(&m, &[], &ctx)
            .expect_err("a duplicate id must be rejected");
        assert!(error.to_string().contains("already registered"), "{error}");
    }

    #[test]
    fn collision_validation_rejects_missing_requirement() {
        let m = manifest("needy", &[], &["missing-dep"]);
        // An empty registry: the id itself is free, but the declared
        // requirement is not registered, so the requires check fires.
        let is_registered = |_: &str| false;
        let reserved = HashSet::new();
        let owner_of_tool = |_: &str| None::<String>;
        let ctx = AdmissionContext {
            is_registered: &is_registered,
            reserved_names: &reserved,
            owner_of_tool: &owner_of_tool,
        };
        let error = CapabilityAdmission::validate_collisions(&m, &[], &ctx)
            .expect_err("a missing requirement must be rejected");
        assert!(error.to_string().contains("requires"), "{error}");
    }

    #[test]
    fn collision_validation_rejects_reserved_tool_names() {
        let m = manifest("shadow", &[], &[]);
        let is_registered = |_: &str| false;
        let mut reserved = HashSet::new();
        reserved.insert("fs.read".to_string());
        let owner_of_tool = |_: &str| None::<String>;
        let ctx = AdmissionContext {
            is_registered: &is_registered,
            reserved_names: &reserved,
            owner_of_tool: &owner_of_tool,
        };
        let error = CapabilityAdmission::validate_collisions(&m, &["fs.read"], &ctx)
            .expect_err("shadowing a reserved tool name must be rejected");
        assert!(error.to_string().contains("reserved"), "{error}");
    }

    #[test]
    fn collision_validation_rejects_already_owned_tool_names() {
        let m = manifest("second", &[], &[]);
        let is_registered = |id: &str| id == "first";
        let reserved = HashSet::new();
        let owner_of_tool = |name: &str| (name == "shared.tool").then(|| "first".to_string());
        let ctx = AdmissionContext {
            is_registered: &is_registered,
            reserved_names: &reserved,
            owner_of_tool: &owner_of_tool,
        };
        let error = CapabilityAdmission::validate_collisions(&m, &["shared.tool"], &ctx)
            .expect_err("a second owner of the same tool name must be rejected");
        assert!(error.to_string().contains("already owned"), "{error}");
    }

    #[test]
    fn collision_validation_passes_a_clear_registration() {
        let m = manifest("fresh", &[], &[]);
        let is_registered = |_: &str| false;
        let reserved = HashSet::new();
        let owner_of_tool = |_: &str| None::<String>;
        let ctx = AdmissionContext {
            is_registered: &is_registered,
            reserved_names: &reserved,
            owner_of_tool: &owner_of_tool,
        };
        CapabilityAdmission::validate_collisions(&m, &["fresh.run"], &ctx)
            .expect("a clear registration must pass");
    }

    #[test]
    fn external_capabilities_are_pinned_to_experimental_and_disabled() {
        let mut m = manifest("ext", &[], &[]);
        m.transport = CapabilityTransport::Process {
            program: "plugin".into(),
        };
        m.status = CapabilityStatus::Stable;
        assert_eq!(
            CapabilityAdmission::initial_status(&m),
            CapabilityStatus::Experimental,
            "external capabilities enter at the bottom of the maturity ladder"
        );
        assert_eq!(
            CapabilityAdmission::initial_activation(&m),
            CapabilityActivation::Disabled,
            "external capabilities enter disabled; enabling is an operator action"
        );

        // An already-Experimental external capability stays Experimental.
        m.status = CapabilityStatus::Experimental;
        assert_eq!(
            CapabilityAdmission::initial_status(&m),
            CapabilityStatus::Experimental
        );
    }

    #[test]
    fn builtin_capabilities_keep_declared_status_and_start_enabled() {
        let m = manifest("core", &[], &[]);
        assert_eq!(
            CapabilityAdmission::initial_status(&m),
            CapabilityStatus::Experimental,
            "a builtin keeps whatever status it declared"
        );
        assert_eq!(
            CapabilityAdmission::initial_activation(&m),
            CapabilityActivation::Enabled,
            "the trusted in-process core is usable immediately"
        );
    }
}
