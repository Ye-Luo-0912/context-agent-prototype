//! Plugin package admission: the core-side authority that decides whether a
//! versioned plugin package manifest may be installed at all. Admission is
//! a *core* decision (mirroring `CapabilityAdmission`): everything here is
//! a pure function of the manifest, so the same rules apply no matter which
//! installer (or future host) asks. Installing a package never implies
//! activation or permission — that is the installer's job (ECO-04); this
//! authority only refuses manifests that cannot be installed at all.
//!
//! Per ECO-01, skills, hooks and adapters are *declared metadata*: they are
//! shape-checked here (id, version, references, bounded counts) but are
//! never interpreted by the runtime. Only `tools` go through the same
//! schema validation as capability tools, so a package cannot smuggle in a
//! schema the capability plane would refuse.

use agent_contracts::{
    AgentError, AgentResult, PluginPackageManifest, VersionRange, is_known_permission,
    validate_capability_id,
};

/// Per-package component caps: a package must not be able to grow the model
/// surface or the install-time workload without bound.
pub const MAX_TOOLS_PER_PACKAGE: usize = 32;
pub const MAX_SKILLS_PER_PACKAGE: usize = 16;
pub const MAX_HOOKS_PER_PACKAGE: usize = 16;
pub const MAX_ADAPTERS_PER_PACKAGE: usize = 8;
pub const MAX_DEPENDENCIES_PER_PACKAGE: usize = 16;
pub const MAX_TESTS_PER_PACKAGE: usize = 16;

/// Field bounds for package identity and component declarations.
pub const MAX_PACKAGE_NAME_CHARS: usize = 100;
pub const MAX_PACKAGE_SUMMARY_CHARS: usize = 500;
pub const MAX_VERSION_CHARS: usize = 64;
pub const MAX_RANGE_CHARS: usize = 64;
pub const MAX_COMPONENT_ID_CHARS: usize = 64;
pub const MAX_COMPONENT_SUMMARY_CHARS: usize = 200;
pub const MAX_REFERENCE_CHARS: usize = 256;
pub const MAX_EVENT_CHARS: usize = 64;
pub const MAX_PROTOCOL_CHARS: usize = 32;
pub const MAX_ENDPOINT_CHARS: usize = 256;
pub const MAX_COMMAND_ARGS: usize = 16;
pub const MAX_COMMAND_ARG_CHARS: usize = 256;

/// The stateless plugin package admission authority.
#[derive(Debug, Default, Clone, Copy)]
pub struct PluginPackageAdmission;

impl PluginPackageAdmission {
    /// Validate everything about a package manifest that depends only on
    /// the manifest itself: identity, versions, compatibility range,
    /// component shapes and counts, dependencies, permissions and tests.
    /// Runs before any registry or installer lock.
    pub fn validate_static(package: &PluginPackageManifest) -> AgentResult<()> {
        validate_capability_id(&package.id).map_err(AgentError::InvalidRequest)?;
        validate_version("package version", &package.version)?;
        validate_range(&package.api)?;
        if package.name.is_empty() || package.name.len() > MAX_PACKAGE_NAME_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' name is {} chars (allowed 1..={MAX_PACKAGE_NAME_CHARS})",
                package.id,
                package.name.len()
            )));
        }
        if package.summary.is_empty() || package.summary.len() > MAX_PACKAGE_SUMMARY_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' summary is {} chars (allowed 1..={MAX_PACKAGE_SUMMARY_CHARS})",
                package.id,
                package.summary.len()
            )));
        }

        // Tools use the capability plane's schema validation verbatim, so a
        // package cannot declare a tool shape the capability plane would
        // refuse (same caps: MAX_TOOLS_PER_CAPABILITY == MAX_TOOLS_PER_PACKAGE).
        crate::capability_admission::validate_tool_specs(&package.id, &package.tools)?;

        validate_skills(package)?;
        validate_hooks(package)?;
        validate_adapters(package)?;
        validate_dependencies(package)?;
        validate_permissions(package)?;
        validate_tests(package)?;
        Ok(())
    }
}

fn validate_skills(package: &PluginPackageManifest) -> AgentResult<()> {
    if package.skills.len() > MAX_SKILLS_PER_PACKAGE {
        return Err(AgentError::InvalidRequest(format!(
            "package '{}' declares {} skills, above the {MAX_SKILLS_PER_PACKAGE} per-package cap",
            package.id,
            package.skills.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for skill in &package.skills {
        validate_component_id("skill", &package.id, &skill.id)?;
        if !seen.insert(skill.id.as_str()) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' declares skill '{}' twice",
                package.id, skill.id
            )));
        }
        validate_version("skill version", &skill.version)?;
        if skill.summary.is_empty() || skill.summary.len() > MAX_COMPONENT_SUMMARY_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' skill '{}' summary is {} chars (allowed 1..={MAX_COMPONENT_SUMMARY_CHARS})",
                package.id,
                skill.id,
                skill.summary.len()
            )));
        }
        // The reference is a package-relative location: non-empty, bounded,
        // no absolute/escaping prefix, no control characters.
        if skill.reference.is_empty() || skill.reference.len() > MAX_REFERENCE_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' skill '{}' reference is {} chars (allowed 1..={MAX_REFERENCE_CHARS})",
                package.id,
                skill.id,
                skill.reference.len()
            )));
        }
        if skill.reference.starts_with('/')
            || skill.reference.starts_with("..")
            || skill.reference.contains('\\')
            || skill.reference.chars().any(char::is_control)
        {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' skill '{}' reference must be a package-relative path",
                package.id, skill.id
            )));
        }
    }
    Ok(())
}

fn validate_hooks(package: &PluginPackageManifest) -> AgentResult<()> {
    if package.hooks.len() > MAX_HOOKS_PER_PACKAGE {
        return Err(AgentError::InvalidRequest(format!(
            "package '{}' declares {} hooks, above the {MAX_HOOKS_PER_PACKAGE} per-package cap",
            package.id,
            package.hooks.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for hook in &package.hooks {
        validate_component_id("hook", &package.id, &hook.id)?;
        if !seen.insert(hook.id.as_str()) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' declares hook '{}' twice",
                package.id, hook.id
            )));
        }
        if hook.event.is_empty() || hook.event.len() > MAX_EVENT_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' hook '{}' event is {} chars (allowed 1..={MAX_EVENT_CHARS})",
                package.id,
                hook.id,
                hook.event.len()
            )));
        }
        let well_formed = hook
            .event
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'));
        if !well_formed {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' hook '{}' event '{}' may only contain lowercase [a-z0-9_.-]",
                package.id, hook.id, hook.event
            )));
        }
        // Mode is an enum, so observe/gate are the only possibilities.
        let _ = hook.mode;
    }
    Ok(())
}

fn validate_adapters(package: &PluginPackageManifest) -> AgentResult<()> {
    if package.adapters.len() > MAX_ADAPTERS_PER_PACKAGE {
        return Err(AgentError::InvalidRequest(format!(
            "package '{}' declares {} adapters, above the {MAX_ADAPTERS_PER_PACKAGE} per-package cap",
            package.id,
            package.adapters.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for adapter in &package.adapters {
        validate_component_id("adapter", &package.id, &adapter.id)?;
        if !seen.insert(adapter.id.as_str()) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' declares adapter '{}' twice",
                package.id, adapter.id
            )));
        }
        if adapter.protocol.is_empty() || adapter.protocol.len() > MAX_PROTOCOL_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' adapter '{}' protocol is {} chars (allowed 1..={MAX_PROTOCOL_CHARS})",
                package.id,
                adapter.id,
                adapter.protocol.len()
            )));
        }
        if adapter.endpoint.is_empty() || adapter.endpoint.len() > MAX_ENDPOINT_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' adapter '{}' endpoint is {} chars (allowed 1..={MAX_ENDPOINT_CHARS})",
                package.id,
                adapter.id,
                adapter.endpoint.len()
            )));
        }
        if adapter.endpoint.chars().any(char::is_control) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' adapter '{}' endpoint contains control characters",
                package.id, adapter.id
            )));
        }
    }
    Ok(())
}

fn validate_dependencies(package: &PluginPackageManifest) -> AgentResult<()> {
    if package.dependencies.len() > MAX_DEPENDENCIES_PER_PACKAGE {
        return Err(AgentError::InvalidRequest(format!(
            "package '{}' declares {} dependencies, above the {MAX_DEPENDENCIES_PER_PACKAGE} per-package cap",
            package.id,
            package.dependencies.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for dependency in &package.dependencies {
        validate_capability_id(&dependency.id).map_err(AgentError::InvalidRequest)?;
        if !seen.insert(dependency.id.as_str()) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' depends on '{}' twice",
                package.id, dependency.id
            )));
        }
        validate_range(&dependency.range)?;
    }
    Ok(())
}

fn validate_permissions(package: &PluginPackageManifest) -> AgentResult<()> {
    let mut seen = std::collections::HashSet::new();
    for permission in &package.permissions {
        if !is_known_permission(permission) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' declares unknown permission '{permission}'",
                package.id
            )));
        }
        if !seen.insert(permission.as_str()) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' declares permission '{permission}' twice",
                package.id
            )));
        }
    }
    Ok(())
}

fn validate_tests(package: &PluginPackageManifest) -> AgentResult<()> {
    if package.tests.len() > MAX_TESTS_PER_PACKAGE {
        return Err(AgentError::InvalidRequest(format!(
            "package '{}' declares {} tests, above the {MAX_TESTS_PER_PACKAGE} per-package cap",
            package.id,
            package.tests.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for test in &package.tests {
        validate_component_id("test", &package.id, &test.id)?;
        if !seen.insert(test.id.as_str()) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' declares test '{}' twice",
                package.id, test.id
            )));
        }
        if test.command.is_empty() || test.command.len() > MAX_COMMAND_ARGS {
            return Err(AgentError::InvalidRequest(format!(
                "package '{}' test '{}' command must be 1..={MAX_COMMAND_ARGS} argv items",
                package.id, test.id
            )));
        }
        for arg in &test.command {
            if arg.is_empty() || arg.len() > MAX_COMMAND_ARG_CHARS {
                return Err(AgentError::InvalidRequest(format!(
                    "package '{}' test '{}' has an argv item of {} chars (allowed 1..={MAX_COMMAND_ARG_CHARS})",
                    package.id,
                    test.id,
                    arg.len()
                )));
            }
        }
    }
    Ok(())
}

fn validate_component_id(kind: &str, package_id: &str, id: &str) -> AgentResult<()> {
    if id.is_empty() || id.len() > MAX_COMPONENT_ID_CHARS {
        return Err(AgentError::InvalidRequest(format!(
            "package '{package_id}' {kind} id is {} chars (allowed 1..={MAX_COMPONENT_ID_CHARS})",
            id.len()
        )));
    }
    let well_formed = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !well_formed {
        return Err(AgentError::InvalidRequest(format!(
            "package '{package_id}' {kind} id '{id}' may only contain lowercase [a-z0-9._-]"
        )));
    }
    Ok(())
}

/// Version shape: non-empty, bounded, printable ASCII without whitespace or
/// control characters (a semver-ish token like `1.2.0` or `0.1.0-rc1`).
fn validate_version(what: &str, version: &str) -> AgentResult<()> {
    if version.is_empty() || version.len() > MAX_VERSION_CHARS {
        return Err(AgentError::InvalidRequest(format!(
            "{what} is {} chars (allowed 1..={MAX_VERSION_CHARS})",
            version.len()
        )));
    }
    let well_formed = version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
    if !well_formed {
        return Err(AgentError::InvalidRequest(format!(
            "{what} '{version}' may only contain [a-zA-Z0-9._-+]"
        )));
    }
    Ok(())
}

/// Range shape: non-empty, bounded, printable ASCII without control
/// characters. Actual semver matching is the installer's job (ECO-04);
/// admission only refuses ranges that cannot be parsed as a range token at
/// all.
fn validate_range(range: &VersionRange) -> AgentResult<()> {
    if range.0.is_empty() || range.0.len() > MAX_RANGE_CHARS {
        return Err(AgentError::InvalidRequest(format!(
            "version range is {} chars (allowed 1..={MAX_RANGE_CHARS})",
            range.0.len()
        )));
    }
    if range.0.chars().any(char::is_control) {
        return Err(AgentError::InvalidRequest(
            "version range contains control characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{HookMode, ToolRisk, ToolSpec};
    use serde_json::json;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "a tool".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }
    }

    fn valid_package() -> PluginPackageManifest {
        PluginPackageManifest {
            id: "acme-pack".into(),
            version: "1.2.0".into(),
            name: "acme pack".into(),
            summary: "demo package".into(),
            api: VersionRange("0.1".into()),
            tools: vec![tool("acme.frob")],
            skills: vec![agent_contracts::SkillDeclaration {
                id: "do-thing".into(),
                version: "1.0.0".into(),
                summary: "does the thing".into(),
                reference: "skills/do-thing.md".into(),
            }],
            hooks: vec![agent_contracts::HookDeclaration {
                id: "observe-model".into(),
                event: "before_model".into(),
                mode: HookMode::Observe,
            }],
            adapters: vec![agent_contracts::AdapterDeclaration {
                id: "mcp-main".into(),
                protocol: "mcp".into(),
                endpoint: "stdio".into(),
            }],
            dependencies: vec![agent_contracts::PackageDependency {
                id: "base-pack".into(),
                range: VersionRange(">=0.2".into()),
            }],
            permissions: vec!["workspace:read".into()],
            tests: vec![agent_contracts::TestDeclaration {
                id: "smoke".into(),
                command: vec!["check".into(), "--quick".into()],
            }],
        }
    }

    #[test]
    fn valid_package_passes_static_admission() {
        PluginPackageAdmission::validate_static(&valid_package())
            .expect("a well-formed package passes");
    }

    #[test]
    fn rejects_bad_package_id() {
        let mut package = valid_package();
        package.id = "Acme-Pack!".into();
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("id"), "{error}");
    }

    #[test]
    fn rejects_unknown_or_duplicate_permissions() {
        let mut package = valid_package();
        package.permissions = vec!["network:all".into()];
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("unknown permission"), "{error}");

        let mut package = valid_package();
        package.permissions = vec!["workspace:read".into(), "workspace:read".into()];
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn rejects_escaping_skill_reference() {
        let mut package = valid_package();
        package.skills[0].reference = "../outside.md".into();
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("package-relative"), "{error}");

        let mut package = valid_package();
        package.skills[0].reference = "/etc/thing.md".into();
        assert!(PluginPackageAdmission::validate_static(&package).is_err());
    }

    #[test]
    fn rejects_bad_hook_event_and_duplicate_components() {
        let mut package = valid_package();
        package.hooks[0].event = "Before Model".into();
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("lowercase"), "{error}");

        let mut package = valid_package();
        package.skills.push(package.skills[0].clone());
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn rejects_bad_dependency_and_version() {
        let mut package = valid_package();
        package.dependencies[0].id = "UPPER".into();
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("id"), "{error}");

        let mut package = valid_package();
        package.version = "".into();
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("chars"), "{error}");
    }

    #[test]
    fn rejects_oversized_tools_via_capability_schema_rules() {
        // A package cannot smuggle a tool schema the capability plane would
        // refuse: oversized schema bytes are rejected by the shared check.
        let mut package = valid_package();
        package.tools[0].input_schema = json!({
            "type": "object",
            "padding": "x".repeat(crate::capability_admission::MAX_TOOL_SCHEMA_BYTES + 1),
        });
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("schema"), "{error}");
    }

    #[test]
    fn rejects_bad_test_command() {
        let mut package = valid_package();
        package.tests[0].command = Vec::new();
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("argv"), "{error}");

        let mut package = valid_package();
        package.tests[0].command = vec!["run".into(), "x".repeat(MAX_COMMAND_ARG_CHARS + 1)];
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("chars"), "{error}");
    }

    #[test]
    fn rejects_component_count_overflow() {
        let mut package = valid_package();
        for i in 0..(MAX_SKILLS_PER_PACKAGE + 1) {
            package.skills.push(agent_contracts::SkillDeclaration {
                id: format!("skill-{i}"),
                version: "1.0.0".into(),
                summary: "s".into(),
                reference: "skills/s.md".into(),
            });
        }
        let error = PluginPackageAdmission::validate_static(&package).unwrap_err();
        assert!(error.to_string().contains("skills"), "{error}");
    }

    #[test]
    fn serde_round_trip_preserves_a_valid_manifest() {
        let package = valid_package();
        let serialized = serde_json::to_string(&package).unwrap();
        let parsed: PluginPackageManifest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed.id, package.id);
        assert_eq!(parsed.version, package.version);
        assert_eq!(parsed.tools.len(), package.tools.len());
        assert_eq!(parsed.skills, package.skills);
        assert_eq!(parsed.hooks, package.hooks);
        assert_eq!(parsed.adapters, package.adapters);
        assert_eq!(parsed.dependencies, package.dependencies);
        assert_eq!(parsed.permissions, package.permissions);
        assert_eq!(parsed.tests, package.tests);
        PluginPackageAdmission::validate_static(&parsed)
            .expect("a round-tripped manifest still passes");
    }
}
