//! Plugin package lifecycle: install / inspect / test / enable / disable /
//! quarantine. Installation never implies activation or
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
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, HookFailurePolicy, HookMode, PluginActivation, PluginPackageManifest,
    SkillActivation, SkillSource,
};
use agent_core::{PluginPackageAdmission, PluginStateAuthority};
use tokio::io::AsyncReadExt;

/// Bounds for the sandboxed self-check runner.
pub const PLUGIN_TEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const PLUGIN_TEST_OUTPUT_TAIL_CHARS: usize = 2_000;
pub const PLUGIN_TEST_OUTPUT_TAIL_BYTES: u64 = 8 * 1024;
/// Bounded post-kill wait after a timeout (a failed kill must not hang the
/// runner) and the bound for the output tail drain once the child exits.
const PLUGIN_TEST_KILL_REAP_GRACE: Duration = Duration::from_secs(2);
const PLUGIN_TEST_TAIL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Environment keys a self-check inherits from the parent (plus nothing
/// else): the child must not see API keys, credentials or HOME.
const PLUGIN_TEST_ENV_KEYS: &[&str] = &["PATH", "SystemRoot", "TEMP", "TMP", "COMSPEC", "SHELL"];

/// One installed package.
#[derive(Debug, Clone)]
struct PackageEntry {
    manifest: PluginPackageManifest,
    /// Monotonic install sequence, so cross-package hook ordering has a
    /// deterministic tie-break.
    installed_at: u64,
    /// Where the package's skill bodies live, when the operator installed
    /// it from a directory. `None` for manifest-only installs: their skill
    /// bodies are simply unavailable, never guessed at.
    root: Option<PathBuf>,
}

/// Bound on one skill body read (E1). Skill text enters context as
/// ordinary, non-System-authority tool output; a body over this bound is
/// refused outright — a truncated half-procedure is worse than none.
pub const MAX_SKILL_BODY_BYTES: usize = 64 * 1024;

/// The bounded result of one on-demand skill body read: the body plus the
/// provenance/version trail that says where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillBody {
    pub package: String,
    pub skill: String,
    pub version: String,
    pub provenance: SkillSource,
    pub reference: String,
    pub body: String,
}

/// The runtime's plugin package catalog and lifecycle flows.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    packages: RwLock<HashMap<String, PackageEntry>>,
    state: PluginStateAuthority,
    /// Next `installed_at` sequence number (see `PackageEntry`).
    next_install: std::sync::atomic::AtomicU64,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a package: core admission first (pure manifest validation),
    /// then register it in the `Installed` (inert) state. Duplicate ids are
    /// refused. Installing never activates anything.
    pub fn install(&self, manifest: PluginPackageManifest) -> AgentResult<()> {
        self.install_inner(manifest, None)
    }

    /// Install a package from an on-disk root (E1): the declared skill
    /// references resolve inside this directory only, and their bodies are
    /// readable only while the package and the skill are active. Installing
    /// from a root still never activates anything.
    pub fn install_from_root(
        &self,
        manifest: PluginPackageManifest,
        root: PathBuf,
    ) -> AgentResult<()> {
        if !root.is_dir() {
            return Err(AgentError::InvalidRequest(format!(
                "plugin package root {} is not a directory",
                root.display()
            )));
        }
        self.install_inner(manifest, Some(root))
    }

    fn install_inner(
        &self,
        manifest: PluginPackageManifest,
        root: Option<PathBuf>,
    ) -> AgentResult<()> {
        // Admission already returns an AgentError; propagate it as-is.
        PluginPackageAdmission::validate_static(&manifest)?;
        let id = manifest.id.clone();
        let mut packages = self.packages.write().expect("plugin catalog poisoned");
        if packages.contains_key(&id) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{id}' is already installed"
            )));
        }
        let installed_at = self
            .next_install
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        packages.insert(
            id.clone(),
            PackageEntry {
                manifest,
                installed_at,
                root,
            },
        );
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

    /// The declared hooks of one package, as a bounded metadata view, in
    /// declaration order. Metadata only — nothing fires in v0.
    pub fn hooks(&self, package: &str) -> Option<Vec<HookView>> {
        let packages = self.packages.read().expect("plugin catalog poisoned");
        packages.get(package).map(|entry| {
            entry
                .manifest
                .hooks
                .iter()
                .map(|hook| HookView {
                    id: hook.id.clone(),
                    event: hook.event.clone(),
                    mode: hook.mode,
                    order: hook.order,
                    timeout_ms: hook.timeout_ms,
                    output_budget_chars: hook.output_budget_chars,
                    failure: hook.failure,
                    permissions: hook.permissions.clone(),
                })
                .collect()
        })
    }

    /// The deterministic firing order for one lifecycle event across every
    /// active package: ascending `order`, then package install
    /// order, then declaration order within the package. Packages that are
    /// not `Active` contribute no hooks; an event outside the known
    /// vocabulary yields an empty order. Metadata only — nothing fires in
    /// v0.
    pub fn hook_order(&self, event: &str) -> Vec<HookRef> {
        let packages = self.packages.read().expect("plugin catalog poisoned");
        let mut order: Vec<(u32, u64, usize, String, String)> = Vec::new();
        for (id, entry) in packages.iter() {
            if self.state.activation(id) != Some(PluginActivation::Active) {
                continue;
            }
            for (index, hook) in entry.manifest.hooks.iter().enumerate() {
                if hook.event == event {
                    order.push((
                        hook.order,
                        entry.installed_at,
                        index,
                        id.clone(),
                        hook.id.clone(),
                    ));
                }
            }
        }
        order.sort_by_key(|(order, installed_at, index, _, _)| (*order, *installed_at, *index));
        order
            .into_iter()
            .map(|(_, _, _, package, id)| HookRef { package, id })
            .collect()
    }

    /// Activate a declared skill. Metadata intent only: the
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

    /// On-demand skill body read (E1). Every gate is explicit:
    ///
    /// * the package must be installed AND active,
    /// * the skill itself must be active (the operator offered it),
    /// * the reference resolves confined under the package root — an
    ///   absolute path, a parent component or any prefix/root component
    ///   escapes the package and is refused,
    /// * the body is bounded: an over-cap read is refused, never
    ///   truncated.
    ///
    /// The returned body enters context as ordinary (non-System-authority)
    /// tool output with its provenance and version attached. Nothing here
    /// injects the body into a request on its own, and reading never
    /// grants authority — the skill's scripts still run (or refuse) through
    /// the existing tools and their approval gates.
    pub fn skill_read(&self, package: &str, skill_id: &str) -> AgentResult<SkillBody> {
        let (root, reference, version, provenance, activation) = {
            let packages = self.packages.read().expect("plugin catalog poisoned");
            let entry = packages.get(package).ok_or_else(|| {
                AgentError::InvalidRequest(format!("package '{package}' is not installed"))
            })?;
            let skill = entry
                .manifest
                .skills
                .iter()
                .find(|skill| skill.id == skill_id)
                .ok_or_else(|| {
                    AgentError::InvalidRequest(format!(
                        "package '{package}' declares no skill '{skill_id}'"
                    ))
                })?;
            (
                entry.root.clone(),
                skill.reference.clone(),
                skill.version.clone(),
                skill.provenance,
                skill.activation,
            )
        };
        if self.activation(package) != Some(PluginActivation::Active) {
            return Err(AgentError::InvalidRequest(format!(
                "package '{package}' is not active; enable it before reading its skills"
            )));
        }
        if activation != SkillActivation::Active {
            return Err(AgentError::InvalidRequest(format!(
                "skill '{package}:{skill_id}' is not active; activate it before reading it"
            )));
        }
        let Some(root) = root else {
            return Err(AgentError::InvalidRequest(format!(
                "package '{package}' was installed without a root; its skill bodies are unavailable"
            )));
        };
        let relative = Path::new(&reference);
        let confined = !relative.is_absolute()
            && relative.components().all(|component| {
                matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
        if !confined {
            return Err(AgentError::InvalidRequest(format!(
                "skill reference '{reference}' escapes its package root"
            )));
        }
        let mut bytes = Vec::new();
        {
            use std::io::Read as _;
            confined_open_regular(&root, relative)
                .map_err(|error| {
                    AgentError::Tool(format!("read skill '{package}:{skill_id}': {error}"))
                })?
                .take(MAX_SKILL_BODY_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| {
                    AgentError::Tool(format!("read skill '{package}:{skill_id}': {error}"))
                })?;
        }
        if bytes.len() > MAX_SKILL_BODY_BYTES {
            return Err(AgentError::Tool(format!(
                "skill '{package}:{skill_id}' body exceeds the {MAX_SKILL_BODY_BYTES} byte cap; refused (never truncated)"
            )));
        }
        let body = String::from_utf8(bytes).map_err(|_| {
            AgentError::Tool(format!(
                "skill '{package}:{skill_id}' body is not valid UTF-8"
            ))
        })?;
        Ok(SkillBody {
            package: package.to_owned(),
            skill: skill_id.to_owned(),
            version,
            provenance,
            reference,
            body,
        })
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

/// A bounded metadata view of one declared skill. The reference
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

/// A bounded metadata view of one declared hook. The firing
/// contract is pinned here — ordering, time/output bounds, fail-closed
/// failure policy and a permission set that can never widen the package's
/// own — while the runtime still never fires a hook in v0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookView {
    pub id: String,
    pub event: String,
    pub mode: HookMode,
    pub order: u32,
    pub timeout_ms: Option<u64>,
    pub output_budget_chars: Option<usize>,
    pub failure: HookFailurePolicy,
    pub permissions: Vec<String>,
}

/// One hook in the deterministic firing order for an event:
/// package + hook id; details come from `PluginRegistry::hooks(package)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRef {
    pub package: String,
    pub id: String,
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
        // `process_group` is an inherent method on tokio::process::Command
        // (no trait import needed); start the child in its own process
        // group so a timeout can kill the whole tree, not just the leader.
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

    // Take the pipes before racing the child so the drain runs
    // concurrently: a check that fills the pipe then waits must not block
    // on its own output, and the drain ends at EOF or its own budget — a
    // living child can never stall the runner. The temp dir stays alive
    // until the child is reaped below.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain =
        tokio::spawn(
            async move { drain_output(stdout, stderr, PLUGIN_TEST_OUTPUT_TAIL_BYTES).await },
        );

    // Race the child against the timeout. A failed tree kill is reported
    // instead of ignored — it must not silently leave the check running —
    // and the post-kill wait stays bounded either way.
    let (status, kill_ok) = tokio::select! {
        status = child.wait() => (Some(status.unwrap_or_default()), true),
        _ = tokio::time::sleep(timeout) => {
            let kill_ok = kill_tree(&mut child).await;
            let _ = tokio::time::timeout(PLUGIN_TEST_KILL_REAP_GRACE, child.wait()).await;
            (None, kill_ok)
        }
    };
    let tail = drain.await.unwrap_or_default();

    let exit_code = status.and_then(|s| s.code());
    let timed_out = status.is_none();
    let ok = !timed_out && exit_code == Some(0);
    let mut tail = clip_tail(&tail);
    if timed_out && !kill_ok {
        tail.push_str(" (the check process tree could not be killed)");
    }
    PluginTestResult {
        id,
        ok,
        exit_code,
        timed_out,
        output_tail: tail,
    }
}

/// Read both piped streams into one bounded buffer. Each stream is capped
/// like the tail bound, and the whole read ends at EOF or the drain budget
/// — never on a child that keeps a write end open.
async fn drain_output(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    cap_bytes: u64,
) -> String {
    let stdout_buf = bounded_read(stdout, cap_bytes).await;
    let stderr_buf = bounded_read(stderr, cap_bytes).await;
    format!(
        "{}{}",
        String::from_utf8_lossy(&stdout_buf),
        String::from_utf8_lossy(&stderr_buf)
    )
}

/// Read one stream up to the cap plus one byte (the extra byte marks
/// truncation), within the drain budget.
async fn bounded_read<R>(stream: Option<R>, cap_bytes: u64) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(stream) = stream else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        PLUGIN_TEST_TAIL_DRAIN_GRACE,
        stream.take(cap_bytes + 1).read_to_end(&mut buf),
    )
    .await;
    buf
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
/// so killing the negative pid reaps every descendant. Returns false when
/// the kill could not be issued or is known to have failed, so the timeout
/// path can report the leak instead of silently leaving the check running.
async fn kill_tree(child: &mut tokio::process::Child) -> bool {
    #[cfg(windows)]
    {
        if let Some(pid) = child.id() {
            tokio::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .map(|status| status.success())
                .unwrap_or(false)
        } else {
            child.kill().await.is_ok()
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(pid) = child.id() {
            tokio::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .map(|status| status.success())
                .unwrap_or(false)
        } else {
            child.kill().await.is_ok()
        }
    }
}

/// `FILE_ATTRIBUTE_REPARSE_POINT`: symlinks, junctions / mount points and
/// every other name surrogate carry it, so refusing the attribute refuses
/// the whole family without depending on windows-specific crates.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// Open a skill body inside the package root for reading, refusing
/// anything that is not a plain regular file reached without traversing a
/// link (F17). The lexical confinement check in `skill_read` is only the
/// second line of defense — a package can plant a symlink, junction or
/// FIFO *inside* its root, so the open itself must not follow one:
///
/// * every component (the root, each intermediate directory and the final
///   entry) is inspected with `symlink_metadata` — no component may be a
///   link; on Windows that means any reparse point,
/// * the final entry must be a regular file (FIFOs, devices, sockets and
///   directories are refused before anything is opened, so a planted FIFO
///   cannot stall the read),
/// * the opened handle is re-verified; on unix its (dev, inode) identity
///   must match the inspected entry, so an entry swapped between inspect
///   and open is detected instead of followed.
///
/// This mirrors the workspace's confined-open helper (O_NOFOLLOW + fstat
/// regular-file assertion on unix, reparse rejection on Windows), which is
/// crate-private to `agent-workspace` and therefore re-implemented here on
/// std primitives only. The lexical check stays a second line of defense.
fn confined_open_regular(root: &Path, relative: &Path) -> std::io::Result<std::fs::File> {
    let mut components = relative.components().peekable();
    let mut walked = root.to_path_buf();
    check_no_link_dir(&walked)?;
    while let Some(component) = components.next() {
        walked.push(component);
        if components.peek().is_some() {
            // Intermediate directory: must be a real directory, not a link.
            check_no_link_dir(&walked)?;
        }
    }
    // Final entry: reject links / special files before opening anything.
    let inspected = std::fs::symlink_metadata(&walked)?;
    assert_regular_file(&inspected, &walked)?;
    let file = std::fs::File::open(&walked)?;
    // Re-verify through the open handle: what was opened must still be the
    // regular file that was inspected.
    let opened = file.metadata()?;
    assert_regular_file(&opened, &walked)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (inspected.dev(), inspected.ino()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "skill entry {} changed while being opened",
                    walked.display()
                ),
            ));
        }
    }
    Ok(file)
}

/// Intermediate path component: a real directory, never a link.
fn check_no_link_dir(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "skill path component {} is not a plain directory",
                path.display()
            ),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "skill path component {} is not a plain directory",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Final entry: a plain regular file (no symlink / junction / reparse
/// point, no FIFO, device, socket or directory).
fn assert_regular_file(metadata: &std::fs::Metadata, path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("skill entry {} is a symbolic link", path.display()),
            ));
        }
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "skill entry {} is not a regular file (FIFOs, devices and directories are refused)",
                    path.display()
                ),
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "skill entry {} is a link or reparse point (junctions and symlinks are refused)",
                    path.display()
                ),
            ));
        }
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("skill entry {} is not a regular file", path.display()),
            ));
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("skill entry {} is not a regular file", path.display()),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        HookDeclaration, SkillDeclaration, TestDeclaration, ToolRisk, ToolSpec, VersionRange,
    };
    use serde_json::json;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "a tool".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
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

    fn package_with_hooks(id: &str, hooks: Vec<HookDeclaration>) -> PluginPackageManifest {
        PluginPackageManifest {
            id: id.into(),
            version: "1.0.0".into(),
            name: id.into(),
            summary: "test package".into(),
            api: VersionRange("0.1".into()),
            tools: vec![tool(&format!("{id}.run"))],
            skills: Vec::new(),
            hooks,
            adapters: Vec::new(),
            dependencies: Vec::new(),
            permissions: vec!["workspace:read".into()],
            tests: Vec::new(),
        }
    }

    fn observe_hook(id: &str, event: &str, order: u32) -> HookDeclaration {
        HookDeclaration {
            id: id.into(),
            event: event.into(),
            mode: HookMode::Observe,
            order,
            timeout_ms: Some(500),
            output_budget_chars: Some(1_000),
            failure: HookFailurePolicy::RecordAndContinue,
            permissions: vec!["workspace:read".into()],
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

    #[tokio::test]
    async fn test_that_floods_the_pipe_still_completes_bounded() {
        // A check that writes far past the pipe capacity before waiting
        // must not block the runner on its own output: the drain runs
        // concurrently and the timeout path stays bounded. The flooded
        // check is killed and reported timed out, not hung.
        #[cfg(windows)]
        let flood = vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "1..400 | ForEach-Object { 'x' * 400 }; Start-Sleep -Seconds 60".into(),
        ];
        #[cfg(not(windows))]
        let flood = vec![
            "sh".into(),
            "-c".into(),
            "head -c 1048576 /dev/zero; sleep 60".into(),
        ];
        let registry = PluginRegistry::new();
        registry
            .install(package_with_tests(
                "pack",
                vec![TestDeclaration {
                    id: "flood".into(),
                    command: flood,
                }],
            ))
            .expect("install succeeds");

        let started = std::time::Instant::now();
        let report = tokio::time::timeout(
            Duration::from_secs(15),
            registry.test_with_timeout("pack", Duration::from_millis(1_500)),
        )
        .await
        .expect("a flooded check must complete within the outer bound")
        .expect("tests run");
        let result = &report.tests[0];
        assert!(result.timed_out, "a flooding sleeper must time out");
        assert!(!result.ok);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the flooded check must be bounded, not hung"
        );
        assert!(
            result.output_tail.chars().count() <= PLUGIN_TEST_OUTPUT_TAIL_CHARS + 64,
            "the output tail must stay bounded"
        );
    }

    #[test]
    fn skill_views_are_metadata_and_never_read_instructions() {
        // The skill's referenced instruction file exists on
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

    #[test]
    fn hook_views_expose_the_bounded_firing_contract() {
        // The view carries the firing contract — order,
        // time/output bounds, fail-closed policy, subset permissions —
        // while nothing fires in v0.
        let registry = PluginRegistry::new();
        registry
            .install(package_with_hooks(
                "pack",
                vec![
                    observe_hook("a", "before_model", 10),
                    HookDeclaration {
                        id: "gate-b".into(),
                        event: "before_model".into(),
                        mode: HookMode::Gate,
                        order: 5,
                        timeout_ms: None,
                        output_budget_chars: None,
                        failure: HookFailurePolicy::DenyOnFailure,
                        permissions: Vec::new(),
                    },
                ],
            ))
            .expect("install succeeds");

        let views = registry.hooks("pack").expect("hooks are viewable");
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].id, "a");
        assert_eq!(views[0].event, "before_model");
        assert_eq!(views[0].mode, HookMode::Observe);
        assert_eq!(views[0].order, 10);
        assert_eq!(views[0].timeout_ms, Some(500));
        assert_eq!(views[0].output_budget_chars, Some(1_000));
        assert_eq!(views[0].failure, HookFailurePolicy::RecordAndContinue);
        assert_eq!(views[0].permissions, vec!["workspace:read".to_string()]);
        assert_eq!(views[1].id, "gate-b");
        assert_eq!(views[1].failure, HookFailurePolicy::DenyOnFailure);

        // Unknown package: no view.
        assert!(registry.hooks("missing").is_none());
    }

    #[test]
    fn hook_order_is_deterministic_across_active_packages() {
        // Hook ordering: ascending `order` first; ties break by package
        // install order, then declaration order within the package. Only
        // Active packages contribute.
        let registry = PluginRegistry::new();
        registry
            .install(package_with_hooks(
                "pack-a",
                vec![observe_hook("late", "before_model", 50)],
            ))
            .expect("install a");
        registry
            .install(package_with_hooks(
                "pack-b",
                vec![
                    observe_hook("early", "before_model", 1),
                    observe_hook("mid", "before_model", 50),
                ],
            ))
            .expect("install b");
        registry
            .install(package_with_hooks(
                "pack-c",
                vec![observe_hook("z", "after_tool", 0)],
            ))
            .expect("install c");
        // Only active packages contribute hooks.
        registry.enable("pack-a").expect("enable a");
        registry.enable("pack-b").expect("enable b");
        // pack-c stays Installed (inert): its hooks must not appear.

        let order = registry.hook_order("before_model");
        let refs: Vec<(String, String)> = order
            .iter()
            .map(|hook| (hook.package.clone(), hook.id.clone()))
            .collect();
        assert_eq!(
            refs,
            vec![
                // pack-b.early has order 1 -> first.
                ("pack-b".to_string(), "early".to_string()),
                // order 50 ties: pack-a installed before pack-b, so
                // pack-a.late runs before pack-b.mid (install order).
                ("pack-a".to_string(), "late".to_string()),
                ("pack-b".to_string(), "mid".to_string()),
            ]
        );

        // An event with no hooks anywhere, or outside the vocabulary,
        // yields an empty order.
        assert!(registry.hook_order("after_tool").is_empty());
        assert!(registry.hook_order("not_an_event").is_empty());
    }

    #[test]
    fn hook_order_skips_disabled_and_quarantined_packages() {
        // A package that is not Active contributes no hooks: disabled and
        // quarantined packages must not gate or observe lifecycle events.
        let registry = PluginRegistry::new();
        registry
            .install(package_with_hooks(
                "on",
                vec![observe_hook("h", "checkpoint", 0)],
            ))
            .expect("install on");
        registry
            .install(package_with_hooks(
                "off",
                vec![observe_hook("h", "checkpoint", 0)],
            ))
            .expect("install off");
        registry.enable("on").expect("enable on");
        registry.enable("off").expect("enable off");
        assert_eq!(registry.hook_order("checkpoint").len(), 2);

        registry.disable("off").expect("disable off");
        let order = registry.hook_order("checkpoint");
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].package, "on");

        registry.enable("off").expect("re-enable off");
        registry.quarantine("off").expect("quarantine off");
        let order = registry.hook_order("checkpoint");
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].package, "on");
    }

    #[tokio::test]
    async fn test_runs_outside_the_workspace_with_a_scrubbed_env() {
        // The self-check must see a private cwd and a scrubbed environment:
        // no API keys, no HOME. The command prints its cwd and the value of
        // a planted secret (empty when scrubbed), and we assert both. The
        // planted variable is the exact one the command reads (`SECRET`),
        // so a broken scrub would actually leak it — the oracle is not
        // vacuous.
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
            std::env::set_var("SECRET", "s3cr3t");
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
            std::env::remove_var("SECRET");
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

    fn skill_declaration(skill_id: &str, reference: &str) -> SkillDeclaration {
        SkillDeclaration {
            id: skill_id.into(),
            version: "1.2.0".into(),
            summary: "test skill".into(),
            reference: reference.into(),
            provenance: SkillSource::Package,
            activation: SkillActivation::Inactive,
        }
    }

    /// E1: an active skill's body is readable from its package root, with
    /// its provenance and version attached.
    #[test]
    fn skill_read_serves_an_active_skill_from_its_package_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("howto.md"), "# howto\nstep one").unwrap();
        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill("pack", skill_declaration("howto", "howto.md"), Vec::new()),
                dir.path().to_path_buf(),
            )
            .expect("install from root succeeds");
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "howto").unwrap();

        let body = registry.skill_read("pack", "howto").expect("read succeeds");
        assert_eq!(body.body, "# howto\nstep one");
        assert_eq!(body.package, "pack");
        assert_eq!(body.version, "1.2.0");
        assert_eq!(body.provenance, SkillSource::Package);
        assert_eq!(body.reference, "howto.md");
    }

    /// Both activation gates hold: an inactive package refuses, an active
    /// package with an inactive skill refuses, and only the fully offered
    /// skill reads.
    #[test]
    fn skill_read_requires_an_active_package_and_an_active_skill() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.md"), "body").unwrap();
        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill("pack", skill_declaration("s", "s.md"), Vec::new()),
                dir.path().to_path_buf(),
            )
            .unwrap();

        let error = registry.skill_read("pack", "s").unwrap_err();
        assert!(error.to_string().contains("not active"), "{error}");

        registry.enable("pack").unwrap();
        let error = registry.skill_read("pack", "s").unwrap_err();
        assert!(
            error.to_string().contains("not active"),
            "an inactive skill must refuse: {error}"
        );

        registry.activate_skill("pack", "s").unwrap();
        assert!(registry.skill_read("pack", "s").is_ok());
    }

    /// A manifest-only install has no root: its bodies are unavailable,
    /// never guessed at. A reference escaping the root (parent components
    /// or an absolute path) is refused at read time.
    #[test]
    fn skill_read_refuses_manifest_only_installs_and_path_escapes() {
        let registry = PluginRegistry::new();
        registry
            .install(package_with_skill(
                "pack",
                skill_declaration("s", "s.md"),
                Vec::new(),
            ))
            .unwrap();
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "s").unwrap();
        let error = registry.skill_read("pack", "s").unwrap_err();
        assert!(error.to_string().contains("without a root"), "{error}");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.md"), "body").unwrap();
        std::fs::write(dir.path().parent().unwrap().join("secret.txt"), "s3cr3t").unwrap();
        // Escape attempts are refused at ADMISSION, before any root exists —
        // `skill_read`'s own confinement check is the second line of
        // defense behind it.
        let registry = PluginRegistry::new();
        let error = registry
            .install_from_root(
                package_with_skill(
                    "escape",
                    skill_declaration("s", "../secret.txt"),
                    Vec::new(),
                ),
                dir.path().to_path_buf(),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("package-relative"),
            "an escaping reference must be refused at admission: {error}"
        );

        let absolute = if cfg!(windows) { "C:\\x.md" } else { "/x.md" };
        let error = registry
            .install_from_root(
                package_with_skill("absolute", skill_declaration("s", absolute), Vec::new()),
                dir.path().to_path_buf(),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("package-relative"),
            "an absolute reference must be refused at admission: {error}"
        );
    }

    /// An over-cap body is refused outright — a truncated half-procedure
    /// is worse than none.
    #[test]
    fn skill_read_refuses_an_oversized_body_instead_of_truncating() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.md"),
            vec![b'a'; MAX_SKILL_BODY_BYTES + 1],
        )
        .unwrap();
        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill("pack", skill_declaration("big", "big.md"), Vec::new()),
                dir.path().to_path_buf(),
            )
            .unwrap();
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "big").unwrap();
        let error = registry.skill_read("pack", "big").unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    /// F17: a symlink planted inside the package must not redirect a skill
    /// read to a file outside the package, even though the lexical
    /// reference looks confined.
    #[cfg(unix)]
    #[test]
    fn skill_read_refuses_an_in_package_symlink_to_an_outside_file() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("leak.md"),
        )
        .unwrap();
        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill("pack", skill_declaration("leak", "leak.md"), Vec::new()),
                dir.path().to_path_buf(),
            )
            .unwrap();
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "leak").unwrap();

        let error = registry.skill_read("pack", "leak").unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("symbolic link") || message.contains("regular file"),
            "the link must be refused at open: {message}"
        );
        assert!(
            !message.contains("s3cr3t"),
            "the outside content must never surface: {message}"
        );
    }

    /// F17: a symlinked *intermediate* directory (e.g. `skills -> /etc`)
    /// must not pivot the read out of the package either.
    #[cfg(unix)]
    #[test]
    fn skill_read_refuses_a_symlinked_intermediate_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("skills")).unwrap();
        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill(
                    "pack",
                    skill_declaration("leak", "skills/secret.txt"),
                    Vec::new(),
                ),
                dir.path().to_path_buf(),
            )
            .unwrap();
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "leak").unwrap();

        let error = registry.skill_read("pack", "leak").unwrap_err();
        assert!(
            error.to_string().contains("plain directory"),
            "the link must be refused at open: {error}"
        );
    }

    /// F17: a planted FIFO must be refused before it is opened — the read
    /// stays bounded instead of blocking on a writer that never comes.
    #[cfg(unix)]
    #[test]
    fn skill_read_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.md");
        let created = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must exist on unix");
        assert!(created.success(), "mkfifo failed");

        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill("pack", skill_declaration("pipe", "pipe.md"), Vec::new()),
                dir.path().to_path_buf(),
            )
            .unwrap();
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "pipe").unwrap();

        // Run the read on a worker with a hard bound: a regression that
        // opens the FIFO first would hang forever, not just fail.
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = registry.skill_read("pack", "pipe");
            sender.send(result).ok();
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("reading a planted FIFO must be refused before open, not block");
        let error = result.expect_err("a FIFO is not a regular skill body");
        assert!(
            error.to_string().contains("regular file"),
            "the FIFO must be refused as a non-regular file: {error}"
        );
    }

    /// F17: a junction planted inside the package must not redirect a
    /// skill read to a directory outside the package.
    #[cfg(windows)]
    #[test]
    fn skill_read_refuses_an_in_package_junction_to_an_outside_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.path().join("skills"))
            .arg(outside.path())
            .status()
            .expect("mklink must be spawnable");
        assert!(status.success(), "junction creation failed: {status:?}");

        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill(
                    "pack",
                    skill_declaration("leak", "skills/secret.txt"),
                    Vec::new(),
                ),
                dir.path().to_path_buf(),
            )
            .unwrap();
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "leak").unwrap();

        let error = registry.skill_read("pack", "leak").unwrap_err();
        assert!(
            error.to_string().contains("reparse point")
                || error.to_string().contains("plain directory"),
            "the junction must be refused at open: {error}"
        );
    }

    /// F17: the component walk must not reject legitimate nested bodies —
    /// a real directory inside the package reads like a top-level file.
    #[test]
    fn skill_read_serves_a_nested_body_through_a_real_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();
        std::fs::write(dir.path().join("skills/nested.md"), "# nested howto").unwrap();
        let registry = PluginRegistry::new();
        registry
            .install_from_root(
                package_with_skill(
                    "pack",
                    skill_declaration("nested", "skills/nested.md"),
                    Vec::new(),
                ),
                dir.path().to_path_buf(),
            )
            .unwrap();
        registry.enable("pack").unwrap();
        registry.activate_skill("pack", "nested").unwrap();

        let body = registry
            .skill_read("pack", "nested")
            .expect("a nested regular file must read");
        assert_eq!(body.body, "# nested howto");
    }
}
