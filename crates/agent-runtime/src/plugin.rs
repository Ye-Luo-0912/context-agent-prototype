//! Plugin package lifecycle: install / inspect / test / enable / disable /
//! quarantine (ECO-04). Installation never implies activation or
//! permission: a package enters `Installed` and stays inert until an
//! explicit operator action moves it. Admission is a core decision
//! (`PluginPackageAdmission`), activation state is owned by the core
//! (`PluginStateAuthority`); this registry owns the installed manifest
//! catalog and the flows that drive them. Declared self-checks (`tests`)
//! run in a sandboxed shape: a private temp cwd, a scrubbed environment
//! (only PATH and platform essentials), a bounded timeout, tree-kill on
//! timeout and bounded output capture — the core never runs a package's
//! test command during a turn.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, PluginActivation, PluginPackageManifest, SkillActivation, SkillSource,
};
use agent_core::{PluginPackageAdmission, PluginStateAuthority};
use tokio::io::AsyncReadExt;

/// Bounds for the sandboxed self-check runner.
pub const PLUGIN_TEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const PLUGIN_TEST_OUTPUT_TAIL_CHARS: usize = 2_000;
pub const PLUGIN_TEST_OUTPUT_TAIL_BYTES: u64 = 8 * 1024;

/// Environment keys a self-check inherits from the parent (plus nothing
/// else): the child must not see API keys, credentials or HOME.
const PLUGIN_TEST_ENV_KEYS: &[&str] = &["PATH", "SystemRoot", "TEMP", "TMP", "COMSPEC", "SHELL"];

/// One installed package.
#[derive(Debug, Clone)]
struct PackageEntry {
    manifest: PluginPackageManifest,
}

/// The runtime's plugin package catalog and lifecycle flows.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    packages: RwLock<HashMap<String, PackageEntry>>,
    state: PluginStateAuthority,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a package: core admission first (pure manifest validation),
    /// then register it in the `Installed` (inert) state. Duplicate ids are
    /// refused. Installing never activates anything.
    pub fn install(&self, manifest: PluginPackageManifest) -> AgentResult<()> {
        // Admission already returns an AgentError; propagate it as-is.
        PluginPackageAdmission::validate_static(&manifest)?;
        let id = manifest.id.clone();
        let mut packages = self.packages.write().expect("plugin catalog poisoned");
        if packages.contains_key(&id) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{id}' is already installed"
            )));
        }
        packages.insert(id.clone(), PackageEntry { manifest });
        self.state.install(&id);
        Ok(())
    }

    /// Uninstall a package and clear its activation state.
    pub fn uninstall(&self, id: &str) -> AgentResult<()> {
        let mut packages = self.packages.write().expect("plugin catalog poisoned");
        if packages.remove(id).is_none() {
            return Err(AgentError::InvalidRequest(format!(
                "package '{id}' is not installed"
            )));
        }
        self.state.uninstall(id);
        Ok(())
    }

    /// Inspect one installed package (metadata + activation).
    pub fn inspect(&self, id: &str) -> Option<PluginPackageView> {
        let packages = self.packages.read().expect("plugin catalog poisoned");
        packages
            .get(id)
            .map(|entry| view(entry, self.state.activation(id)))
    }

    /// List every installed package.
    pub fn list(&self) -> Vec<PluginPackageView> {
        let packages = self.packages.read().expect("plugin catalog poisoned");
        let mut views: Vec<_> = packages
            .iter()
            .map(|(id, entry)| view(entry, self.state.activation(id)))
            .collect();
        views.sort_by(|a, b| a.id.cmp(&b.id));
        views
    }

    /// The activation of one installed package, if any.
    pub fn activation(&self, id: &str) -> Option<PluginActivation> {
        self.state.activation(id)
    }

    /// Move a package to `Active` (installed -> active or disabled ->
    /// active). The transition table lives in the core authority.
    pub fn enable(&self, id: &str) -> AgentResult<()> {
        self.require_installed(id)?;
        self.state
            .set_activation(id, PluginActivation::Active)
            .map_err(AgentError::InvalidRequest)
    }

    /// Move an active package to `Disabled`.
    pub fn disable(&self, id: &str) -> AgentResult<()> {
        self.require_installed(id)?;
        self.state
            .set_activation(id, PluginActivation::Disabled)
            .map_err(AgentError::InvalidRequest)
    }

    /// Quarantine a package (installed/active/disabled -> quarantined)
    /// after misbehavior; nothing runs until an explicit unquarantine.
    pub fn quarantine(&self, id: &str) -> AgentResult<()> {
        self.require_installed(id)?;
        self.state
            .set_activation(id, PluginActivation::Quarantined)
            .map_err(AgentError::InvalidRequest)
    }

    /// Lift a quarantine back to `Disabled` (a human step, never straight
    /// to `Active`).
    pub fn unquarantine(&self, id: &str) -> AgentResult<()> {
        self.require_installed(id)?;
        self.state
            .set_activation(id, PluginActivation::Disabled)
            .map_err(AgentError::InvalidRequest)
    }

    /// Run the package's declared self-checks in the sandboxed runner.
    pub async fn test(&self, id: &str) -> AgentResult<PluginTestReport> {
        self.test_with_timeout(id, PLUGIN_TEST_TIMEOUT).await
    }

    /// Like [`Self::test`] but with an explicit timeout (the tests use a
    /// short one; callers use the default).
    pub async fn test_with_timeout(
        &self,
        id: &str,
        timeout: Duration,
    ) -> AgentResult<PluginTestReport> {
        // The catalog read guard must not outlive this block: the runner
        // below awaits child processes.
        let tests = {
            let packages = self.packages.read().expect("plugin catalog poisoned");
            let entry = packages.get(id).ok_or_else(|| {
                AgentError::InvalidRequest(format!("package '{id}' is not installed"))
            })?;
            entry.manifest.tests.clone()
        };

        let mut results = Vec::new();
        for test in &tests {
            results.push(run_test_command(&test.command, timeout).await);
        }
        Ok(PluginTestReport {
            package: id.to_string(),
            tests: results,
        })
    }

    fn require_installed(&self, id: &str) -> AgentResult<()> {
        let packages = self.packages.read().expect("plugin catalog poisoned");
        if packages.contains_key(id) {
            Ok(())
        } else {
            Err(AgentError::InvalidRequest(format!(
                "package '{id}' is not installed"
            )))
        }
    }

    /// The declared skills of one package, as a bounded metadata view.
    pub fn skills(&self, package: &str) -> Option<Vec<SkillView>> {
        let packages = self.packages.read().expect("plugin catalog poisoned");
        packages.get(package).map(|entry| {
            entry
                .manifest
                .skills
                .iter()
                .map(|skill| SkillView {
                    id: skill.id.clone(),
                    version: skill.version.clone(),
                    summary: skill.summary.clone(),
                    reference: skill.reference.clone(),
                    provenance: skill.provenance,
                    activation: skill.activation,
                })
                .collect()
        })
    }

    /// Activate a declared skill. Metadata intent only (ECO-06): the
    /// runtime never executes a skill and never turns its instructions
    /// into System-authority content — activation only records that the
    /// skill may be offered.
    pub fn activate_skill(&self, package: &str, skill_id: &str) -> AgentResult<()> {
        self.set_skill_activation(package, skill_id, SkillActivation::Active)
    }

    /// Deactivate a declared skill (metadata intent only).
    pub fn deactivate_skill(&self, package: &str, skill_id: &str) -> AgentResult<()> {
        self.set_skill_activation(package, skill_id, SkillActivation::Inactive)
    }

    fn set_skill_activation(
        &self,
        package: &str,
        skill_id: &str,
        next: SkillActivation,
    ) -> AgentResult<()> {
        let mut packages = self.packages.write().expect("plugin catalog poisoned");
        let entry = packages.get_mut(package).ok_or_else(|| {
            AgentError::InvalidRequest(format!("package '{package}' is not installed"))
        })?;
        let skill = entry
            .manifest
            .skills
            .iter_mut()
            .find(|skill| skill.id == skill_id)
            .ok_or_else(|| {
                AgentError::InvalidRequest(format!(
                    "package '{package}' declares no skill '{skill_id}'"
                ))
            })?;
        skill.activation = next;
        Ok(())
    }
}

/// A bounded, inspection-friendly view of one installed package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPackageView {
    pub id: String,
    pub version: String,
    pub name: String,
    pub summary: String,
    pub api: String,
    pub activation: PluginActivation,
    pub tools: usize,
    pub skills: usize,
    pub hooks: usize,
    pub adapters: usize,
    pub dependencies: usize,
    pub tests: usize,
}

/// A bounded metadata view of one declared skill (ECO-06). The reference
/// is a package-relative location; the runtime never reads it and never
/// injects skill instructions as System-authority content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillView {
    pub id: String,
    pub version: String,
    pub summary: String,
    pub reference: String,
    pub provenance: SkillSource,
    pub activation: SkillActivation,
}

fn view(entry: &PackageEntry, activation: Option<PluginActivation>) -> PluginPackageView {
    let manifest = &entry.manifest;
    PluginPackageView {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        name: manifest.name.clone(),
        summary: manifest.summary.clone(),
        api: manifest.api.as_str().to_string(),
        activation: activation.unwrap_or(PluginActivation::Installed),
        tools: manifest.tools.len(),
        skills: manifest.skills.len(),
        hooks: manifest.hooks.len(),
        adapters: manifest.adapters.len(),
        dependencies: manifest.dependencies.len(),
        tests: manifest.tests.len(),
    }
}

/// The aggregate result of running a package's declared self-checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTestReport {
    pub package: String,
    pub tests: Vec<PluginTestResult>,
}

/// The result of one declared self-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTestResult {
    pub id: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Bounded tail of the combined stdout/stderr.
    pub output_tail: String,
}

/// Run one declared argv self-check in a sandboxed shape: private temp
/// cwd, scrubbed environment, bounded timeout with tree-kill, bounded
/// output capture. The command is passed verbatim — no shell parsing.
async fn run_test_command(command: &[String], timeout: Duration) -> PluginTestResult {
    let id = "test".to_string();
    let temp = tempfile::tempdir();
    let Some(temp) = temp.ok() else {
        return PluginTestResult {
            id,
            ok: false,
            exit_code: None,
            timed_out: false,
            output_tail: "failed to create the sandbox working directory".into(),
        };
    };

    let mut cmd = tokio::process::Command::new(&command[0]);
    cmd.args(&command[1..])
        .current_dir(temp.path())
        .env_clear()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for key in PLUGIN_TEST_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    #[cfg(unix)]
    {
        use tokio::process::CommandExt;
        // Start the child in its own process group so a timeout can kill
        // the whole tree, not just the leader.
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return PluginTestResult {
                id,
                ok: false,
                exit_code: None,
                timed_out: false,
                output_tail: format!("spawn failed: {error}"),
            };
        }
    };

    // Race the child against the timeout, then capture a bounded output
    // tail. The temp dir is kept alive until the child is reaped.
    let (status, tail) = tokio::select! {
        status = child.wait() => {
            let status = Some(status.unwrap_or_default());
            let tail = drain_tail(&mut child, PLUGIN_TEST_OUTPUT_TAIL_BYTES).await;
            (status, tail)
        }
        _ = tokio::time::sleep(timeout) => {
            kill_tree(&mut child).await;
            let _ = child.wait().await;
            let tail = drain_tail(&mut child, PLUGIN_TEST_OUTPUT_TAIL_BYTES).await;
            (None, tail)
        }
    };

    let exit_code = status.and_then(|s| s.code());
    let timed_out = status.is_none();
    let ok = !timed_out && exit_code == Some(0);
    PluginTestResult {
        id,
        ok,
        exit_code,
        timed_out,
        output_tail: clip_tail(&tail),
    }
}

/// Drain the child's piped stdout/stderr into one bounded buffer (bytes
/// capped, then char-clipped for the model-facing tail).
async fn drain_tail(child: &mut tokio::process::Child, cap_bytes: u64) -> String {
    let mut output = String::new();
    if let Some(stdout) = child.stdout.take() {
        let mut buf = Vec::new();
        let _ = stdout.take(cap_bytes + 1).read_to_end(&mut buf).await;
        output.push_str(&String::from_utf8_lossy(&buf));
    }
    if let Some(stderr) = child.stderr.take() {
        let mut buf = Vec::new();
        let _ = stderr.take(cap_bytes + 1).read_to_end(&mut buf).await;
        output.push_str(&String::from_utf8_lossy(&buf));
    }
    output
}

fn clip_tail(text: &str) -> String {
    let clipped: String = text
        .chars()
        .rev()
        .take(PLUGIN_TEST_OUTPUT_TAIL_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if clipped.chars().count() < text.chars().count() {
        format!("... (output truncated) {clipped}")
    } else {
        clipped
    }
}

/// Kill the child's whole tree: on Windows `taskkill /T /F` walks the tree
/// by pid; on Unix the child leads its own process group (set at spawn),
/// so killing the negative pid reaps every descendant.
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    {
        if let Some(pid) = child.id() {
            let _ = tokio::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        } else {
            let _ = child.kill().await;
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(pid) = child.id() {
            let _ = tokio::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        } else {
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{SkillDeclaration, TestDeclaration, ToolRisk, ToolSpec, VersionRange};
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

    fn package_with_skill(
        id: &str,
        skill: SkillDeclaration,
        tests: Vec<TestDeclaration>,
    ) -> PluginPackageManifest {
        PluginPackageManifest {
            id: id.into(),
            version: "1.0.0".into(),
            name: id.into(),
            summary: "test package".into(),
            api: VersionRange("0.1".into()),
            tools: vec![tool(&format!("{id}.run"))],
            skills: vec![skill],
            hooks: Vec::new(),
            adapters: Vec::new(),
            dependencies: Vec::new(),
            permissions: vec!["workspace:read".into()],
            tests,
        }
    }

    fn package_with_tests(id: &str, tests: Vec<TestDeclaration>) -> PluginPackageManifest {
        PluginPackageManifest {
            id: id.into(),
            version: "1.0.0".into(),
            name: id.into(),
            summary: "test package".into(),
            api: VersionRange("0.1".into()),
            tools: vec![tool(&format!("{id}.run"))],
            skills: Vec::new(),
            hooks: Vec::new(),
            adapters: Vec::new(),
            dependencies: Vec::new(),
            permissions: vec!["workspace:read".into()],
            tests,
        }
    }

    fn ok_command() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "exit 0".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "exit 0".into()]
        }
    }

    fn fail_command() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "exit 3".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "exit 3".into()]
        }
    }

    fn sleep_command() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["ping".into(), "-n".into(), "60".into(), "127.0.0.1".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sleep".into(), "60".into()]
        }
    }

    #[test]
    fn install_registers_but_never_activates() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests("pack", Vec::new()))
            .expect("install succeeds");
        assert_eq!(
            registry.activation("pack"),
            Some(PluginActivation::Installed),
            "installation must never imply activation"
        );
        let view = registry.inspect("pack").expect("package is inspectable");
        assert_eq!(view.id, "pack");
        assert_eq!(view.activation, PluginActivation::Installed);
        assert_eq!(view.tools, 1);
    }

    #[test]
    fn duplicate_install_and_unknown_ops_are_refused() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests("pack", Vec::new()))
            .expect("install succeeds");
        assert!(
            registry
                .install(package_with_tests("pack", Vec::new()))
                .is_err(),
            "duplicate install must be refused"
        );
        assert!(
            registry.enable("missing").is_err(),
            "enabling an unknown package must fail"
        );
        assert!(
            registry.inspect("missing").is_none(),
            "inspecting an unknown package must yield nothing"
        );
    }

    #[test]
    fn install_rejects_invalid_manifest() {
        let registry = PluginRegistry::new();
        let mut package = package_with_tests("bad", Vec::new());
        package.permissions = vec!["network:all".into()];
        let error = registry.install(package).unwrap_err();
        assert!(error.to_string().contains("unknown permission"), "{error}");
        assert_eq!(registry.inspect("bad"), None);
    }

    #[test]
    fn activation_flow_enable_disable_quarantine() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests("pack", Vec::new()))
            .expect("install succeeds");

        registry.enable("pack").expect("installed -> active");
        assert_eq!(registry.activation("pack"), Some(PluginActivation::Active));

        registry.disable("pack").expect("active -> disabled");
        assert_eq!(
            registry.activation("pack"),
            Some(PluginActivation::Disabled)
        );

        registry
            .quarantine("pack")
            .expect("disabled -> quarantined");
        assert_eq!(
            registry.activation("pack"),
            Some(PluginActivation::Quarantined)
        );

        // A quarantined package cannot jump straight back to active.
        let error = registry.enable("pack").unwrap_err();
        assert!(error.to_string().contains("cannot move"), "{error}");

        registry
            .unquarantine("pack")
            .expect("quarantined -> disabled");
        registry.enable("pack").expect("disabled -> active");
    }

    #[test]
    fn list_sorts_and_uninstall_clears() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests("b-pack", Vec::new()))
            .expect("install b");
        registry
            .install(package_with_tests("a-pack", Vec::new()))
            .expect("install a");
        let ids: Vec<String> = registry.list().into_iter().map(|v| v.id).collect();
        assert_eq!(ids, vec!["a-pack".to_string(), "b-pack".to_string()]);

        registry.uninstall("a-pack").expect("uninstall a");
        assert_eq!(registry.inspect("a-pack"), None);
        assert_eq!(registry.activation("a-pack"), None);
    }

    #[tokio::test]
    async fn test_runs_self_checks_and_reports_failures() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests(
                "pack",
                vec![
                    TestDeclaration {
                        id: "ok".into(),
                        command: ok_command(),
                    },
                    TestDeclaration {
                        id: "fails".into(),
                        command: fail_command(),
                    },
                ],
            ))
            .expect("install succeeds");

        let report = registry.test("pack").await.expect("tests run");
        assert_eq!(report.package, "pack");
        assert_eq!(report.tests.len(), 2);
        assert!(
            report.tests[0].ok,
            "exit 0 must pass: {:?}",
            report.tests[0]
        );
        assert!(!report.tests[1].ok, "exit 3 must fail");
        assert_eq!(report.tests[1].exit_code, Some(3));
        assert!(!report.tests[1].timed_out);
    }

    #[tokio::test]
    async fn test_times_out_and_kills_the_tree() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests(
                "pack",
                vec![TestDeclaration {
                    id: "slow".into(),
                    command: sleep_command(),
                }],
            ))
            .expect("install succeeds");

        let report = registry
            .test_with_timeout("pack", Duration::from_millis(1_500))
            .await
            .expect("tests run");
        let result = &report.tests[0];
        assert!(result.timed_out, "a sleeping check must time out");
        assert!(!result.ok);
    }

    #[test]
    fn skill_views_are_metadata_and_never_read_instructions() {
        // ECO-06 anchor: the skill's referenced instruction file exists on
        // disk with a marker, but no registry view ever reads it — a skill
        // is metadata, never a source of System-authority content.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();
        std::fs::write(
            dir.path().join("skills/do-thing.md"),
            "TOP-SECRET-SKILL-INSTRUCTIONS\n",
        )
        .unwrap();

        let registry = PluginRegistry::new();
        registry
            .install(package_with_skill(
                "pack",
                SkillDeclaration {
                    id: "do-thing".into(),
                    version: "1.0.0".into(),
                    summary: "does the thing".into(),
                    reference: "skills/do-thing.md".into(),
                    provenance: SkillSource::Package,
                    activation: SkillActivation::Inactive,
                },
                Vec::new(),
            ))
            .expect("install succeeds");

        // Every model-facing view of the package must be free of the
        // instruction content: the runtime never opens the reference.
        let views = registry.skills("pack").expect("skills are viewable");
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, "do-thing");
        assert_eq!(views[0].reference, "skills/do-thing.md");
        let serialized = format!("{views:?}{:?}", registry.inspect("pack"));
        assert!(
            !serialized.contains("TOP-SECRET-SKILL-INSTRUCTIONS"),
            "skill instructions must never reach a model-facing view"
        );
        // The package's model surface is tools only; a skill adds nothing.
        let view = registry.inspect("pack").expect("package view");
        assert_eq!(view.tools, 1);
        assert_eq!(view.skills, 1);
    }

    #[test]
    fn activate_skill_records_intent_without_runtime_effect() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_skill(
                "pack",
                SkillDeclaration {
                    id: "do-thing".into(),
                    version: "1.0.0".into(),
                    summary: "does the thing".into(),
                    reference: "skills/do-thing.md".into(),
                    provenance: SkillSource::Operator,
                    activation: SkillActivation::Inactive,
                },
                Vec::new(),
            ))
            .expect("install succeeds");

        registry
            .activate_skill("pack", "do-thing")
            .expect("activate records intent");
        let skills = registry.skills("pack").expect("skills are viewable");
        assert_eq!(skills[0].activation, SkillActivation::Active);
        assert_eq!(skills[0].provenance, SkillSource::Operator);

        registry
            .deactivate_skill("pack", "do-thing")
            .expect("deactivate records intent");
        assert_eq!(
            registry.skills("pack").unwrap()[0].activation,
            SkillActivation::Inactive
        );

        // Activating a skill changes no surface and starts nothing: the
        // package's tool count and activation are untouched.
        let view = registry.inspect("pack").expect("package view");
        assert_eq!(view.tools, 1);
        assert_eq!(view.activation, PluginActivation::Installed);

        // Unknown skills are refused.
        assert!(
            registry.activate_skill("pack", "nope").is_err(),
            "activating an undeclared skill must fail"
        );
        assert!(
            registry.activate_skill("missing", "do-thing").is_err(),
            "activating a skill in an unknown package must fail"
        );
    }

    #[tokio::test]
    async fn test_runs_outside_the_workspace_with_a_scrubbed_env() {
        // The self-check must see a private cwd and a scrubbed environment:
        // no API keys, no HOME. The command prints its cwd and the value of
        // a planted secret (empty when scrubbed), and we assert both.
        #[cfg(windows)]
        let print = vec![
            "cmd".into(),
            "/C".into(),
            "echo CWD=%CD% && echo SECRET=%SECRET%".into(),
        ];
        #[cfg(not(windows))]
        let print = vec![
            "sh".into(),
            "-c".into(),
            "echo CWD=$PWD SECRET=${SECRET:-<unset>}".into(),
        ];

        // Plant a secret in the parent environment before spawning. The
        // scrub must drop it before the child ever starts. (`set_var` is
        // unsafe on edition 2024; this is test-only global-env mutation,
        // not an optimization.)
        unsafe {
            std::env::set_var("PLUGIN_TEST_SECRET_VAR", "s3cr3t");
        }
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests(
                "pack",
                vec![TestDeclaration {
                    id: "probe".into(),
                    command: print,
                }],
            ))
            .expect("install succeeds");
        let report = registry
            .test_with_timeout("pack", Duration::from_secs(10))
            .await
            .expect("tests run");
        let tail = &report.tests[0].output_tail;
        unsafe {
            std::env::remove_var("PLUGIN_TEST_SECRET_VAR");
        }

        assert!(
            tail.contains("SECRET=<unset>") || tail.contains("SECRET="),
            "{tail}"
        );
        assert!(
            !tail.contains("s3cr3t"),
            "the scrubbed env must not leak the planted secret: {tail}"
        );
        assert!(
            tail.contains("CWD="),
            "the private cwd must be reported: {tail}"
        );
    }
}
