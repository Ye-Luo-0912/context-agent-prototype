//! Host-owned verification recipes.
//!
//! A recipe is discovered or installed by the trusted composition root. The
//! model sees only its bounded id/description and cannot supply argv, cwd or
//! environment. The same immutable set produces both the tool implementation
//! and the Core host-effect policy, preventing authority/execution drift.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use agent_contracts::{
    HostEffectBinding, HostExecRecipe, HostToolPolicy, RuntimeFactsView, VerificationReuse,
};
use agent_workspace::Workspace;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERIFY_RUN_TOOL_NAME: &str = "verify.run";
pub const MAX_VERIFICATION_RECIPES: usize = 16;
const MAX_RECIPE_ID_BYTES: usize = 96;
const MAX_RECIPE_DESCRIPTION_BYTES: usize = 240;
const MAX_PACKAGE_JSON_BYTES: u64 = 256 * 1024;
const MAX_DISCOVERY_DIR_ENTRIES: usize = 256;
const MAX_IDENTITY_ENV_VARS: usize = 512;
const MAX_IDENTITY_ENV_BYTES: usize = 256 * 1024;
const MAX_EXACT_INPUTS: usize = 32;
const MAX_EXACT_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WORKSPACE_SNAPSHOT_ENTRIES: usize = 4096;

/// One immutable process verification recipe. Construction validates the
/// same process bounds used by `process.run`; callers cannot smuggle an
/// unbounded catalog into the model/tool boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRecipe {
    pub id: String,
    pub description: String,
    pub revision: String,
    /// Exact PASS reuse is an explicit host assertion, never inferred from a
    /// command name. Discovered general test runners remain TaskScoped;
    /// side-effect-contained deterministic recipes may opt in.
    pub reuse: VerificationReuse,
    /// Host assertion that the recipe cannot modify source/workspace inputs;
    /// generated output is confined to ignored/runtime paths. General test
    /// runners default false and keep conservative Unknown invalidation.
    pub source_read_only: bool,
    /// Explicit source inputs for exact recipes. Empty is valid for a
    /// workspace-independent probe such as `compiler --version`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exact_inputs: Vec<String>,
    /// Host-declared coverage domain: what a PASS of this recipe proves,
    /// shared by all sibling recipes the host lists in one equivalence
    /// class. `None` keeps the recipe exact-only. Declared only by the
    /// host table; never model-authorable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_domain: Option<String>,
    /// Hash the complete bounded workspace file set (excluding runtime and
    /// VCS state) for exact identity. This is intentionally separate from
    /// `exact_inputs`: a compiler target may load sibling modules that are not
    /// explicit in argv.
    #[serde(default, skip_serializing_if = "is_false")]
    pub exact_workspace_snapshot: bool,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub timeout_ms: u64,
}

impl VerificationRecipe {
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        revision: impl Into<String>,
        argv: Vec<String>,
    ) -> Result<Self, String> {
        let recipe = Self {
            id: id.into(),
            description: description.into(),
            revision: revision.into(),
            reuse: VerificationReuse::TaskScoped,
            source_read_only: false,
            coverage_domain: None,
            exact_inputs: Vec::new(),
            exact_workspace_snapshot: false,
            argv,
            cwd: None,
            env: BTreeMap::new(),
            timeout_ms: 120_000,
        };
        recipe.validate()?;
        Ok(recipe)
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Result<Self, String> {
        self.cwd = Some(cwd.into());
        self.validate()?;
        Ok(self)
    }

    pub fn with_exact_current_world_reuse(mut self) -> Self {
        self.reuse = VerificationReuse::ExactCurrentWorld;
        self.source_read_only = true;
        self
    }

    pub fn with_exact_inputs(mut self, inputs: Vec<String>) -> Result<Self, String> {
        self.exact_inputs = inputs;
        self.validate()?;
        Ok(self)
    }

    pub fn with_exact_workspace_snapshot(mut self) -> Result<Self, String> {
        self.exact_workspace_snapshot = true;
        self.validate()?;
        Ok(self)
    }

    /// Declare the coverage domain this recipe proves. Only exact,
    /// source-read-only recipes may join a domain: domain-equivalent reuse
    /// extends exact-current identity, never widens its preconditions.
    pub fn with_coverage_domain(mut self, domain: impl Into<String>) -> Result<Self, String> {
        self.coverage_domain = Some(domain.into());
        if self.reuse != VerificationReuse::ExactCurrentWorld || !self.source_read_only {
            return Err(
                "coverage domains require source-read-only ExactCurrentWorld recipes".into(),
            );
        }
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() || self.id.len() > MAX_RECIPE_ID_BYTES {
            return Err(format!(
                "verification recipe id must contain 1..={MAX_RECIPE_ID_BYTES} bytes"
            ));
        }
        if self.reuse == VerificationReuse::ExactCurrentWorld && !self.source_read_only {
            return Err(
                "ExactCurrentWorld verification recipes must assert source_read_only".into(),
            );
        }
        if self.exact_workspace_snapshot
            && (self.reuse != VerificationReuse::ExactCurrentWorld || !self.source_read_only)
        {
            return Err(
                "exact workspace snapshots require source-read-only ExactCurrentWorld reuse".into(),
            );
        }
        if self.description.trim().is_empty()
            || self.description.len() > MAX_RECIPE_DESCRIPTION_BYTES
        {
            return Err(format!(
                "verification recipe description must contain 1..={MAX_RECIPE_DESCRIPTION_BYTES} bytes"
            ));
        }
        if self.revision.trim().is_empty() || self.revision.len() > 96 {
            return Err("verification recipe revision must contain 1..=96 bytes".into());
        }
        if let Some(domain) = self.coverage_domain.as_deref()
            && (domain.trim().is_empty() || domain.len() > MAX_RECIPE_ID_BYTES)
        {
            return Err(format!(
                "coverage domain must contain 1..={MAX_RECIPE_ID_BYTES} bytes"
            ));
        }
        if self.argv.is_empty() || self.argv.len() > 64 {
            return Err("verification recipe argv must contain 1..=64 arguments".into());
        }
        if self
            .argv
            .iter()
            .any(|arg| arg.is_empty() || arg.chars().count() > 16_384)
        {
            return Err(
                "verification recipe argv entries must contain 1..=16384 characters".into(),
            );
        }
        if self.env.len() > 64
            || self.env.iter().any(|(key, value)| {
                key.is_empty() || key.len() > 256 || value.chars().count() > 16_384
            })
            || self
                .env
                .iter()
                .map(|(key, value)| key.len().saturating_add(value.len()))
                .sum::<usize>()
                > 64 * 1024
        {
            return Err("verification recipe env exceeds process bounds".into());
        }
        if let Some(cwd) = self.cwd.as_deref() {
            let path = Path::new(cwd);
            if cwd.trim().is_empty()
                || path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(
                    "verification recipe cwd must be a confined workspace-relative path".into(),
                );
            }
        }
        if self.exact_inputs.len() > MAX_EXACT_INPUTS
            || self.exact_inputs.iter().any(|input| {
                let path = Path::new(input);
                input.trim().is_empty()
                    || input.len() > 512
                    || path.is_absolute()
                    || path.components().any(|component| {
                        matches!(
                            component,
                            std::path::Component::ParentDir | std::path::Component::Prefix(_)
                        )
                    })
            })
        {
            return Err("verification exact_inputs exceed confined bounds".into());
        }
        if !(100..=120_000).contains(&self.timeout_ms) {
            return Err("verification recipe timeout_ms must be in 100..=120000".into());
        }
        Ok(())
    }
}

/// Sorted, deduplicated immutable recipe set shared by dispatcher and
/// authority policy construction.
#[derive(Debug, Clone, Default)]
pub struct VerificationRecipes {
    recipes: Vec<VerificationRecipe>,
    domains: Vec<VerificationCoverageDomain>,
}

const MAX_COVERAGE_DOMAINS: usize = 8;
const MAX_CLASS_MEMBERS: usize = 8;

/// One host-declared equivalence class: recipes whose PASS facts are
/// mutually acceptable proofs of the same coverage. Membership is an
/// explicit host assertion evaluated against the current composition; it is
/// never inferred from command text and never checkpointed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCoverageDomain {
    pub domain_id: String,
    /// Bumped when the meaning of the domain changes; older facts then
    /// never reuse across the bump.
    pub declaration_revision: u64,
    /// Exact recipe ids in this class. Every id must be registered as a
    /// source-read-only exact recipe declaring this same domain.
    pub members: Vec<String>,
}

impl VerificationRecipes {
    pub fn new(mut recipes: Vec<VerificationRecipe>) -> Result<Self, String> {
        if recipes.len() > MAX_VERIFICATION_RECIPES {
            return Err(format!(
                "at most {MAX_VERIFICATION_RECIPES} verification recipes may be registered"
            ));
        }
        for recipe in &recipes {
            recipe.validate()?;
        }
        recipes.sort_by(|left, right| left.id.cmp(&right.id));
        if recipes.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err("verification recipe ids must be unique".into());
        }
        Ok(Self {
            recipes,
            domains: Vec::new(),
        })
    }

    /// Attach host-declared equivalence classes. Every member must already
    /// be registered as a source-read-only exact recipe declaring this
    /// domain; anything else fails closed at composition time.
    pub fn with_domains(
        mut self,
        mut domains: Vec<VerificationCoverageDomain>,
    ) -> Result<Self, String> {
        if domains.len() > MAX_COVERAGE_DOMAINS {
            return Err(format!("at most {MAX_COVERAGE_DOMAINS} coverage domains"));
        }
        for domain in &domains {
            if domain.domain_id.trim().is_empty() || domain.domain_id.len() > MAX_RECIPE_ID_BYTES {
                return Err(format!(
                    "coverage domain id must contain 1..={MAX_RECIPE_ID_BYTES} bytes"
                ));
            }
            if domain.declaration_revision == 0 {
                return Err("coverage domain declaration_revision must be >= 1".into());
            }
            if domain.members.is_empty() || domain.members.len() > MAX_CLASS_MEMBERS {
                return Err(format!(
                    "coverage domain members must contain 1..={MAX_CLASS_MEMBERS} recipe ids"
                ));
            }
            let mut members = domain.members.clone();
            members.sort();
            if members.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err("coverage domain members must be unique".into());
            }
            for member in &domain.members {
                let Some(recipe) = self.get(member) else {
                    return Err(format!(
                        "coverage domain member '{member}' is not a registered recipe"
                    ));
                };
                if recipe.reuse != VerificationReuse::ExactCurrentWorld
                    || !recipe.source_read_only
                    || recipe.coverage_domain.as_deref() != Some(domain.domain_id.as_str())
                {
                    return Err(format!(
                        "coverage domain member '{member}' must be an exact source-read-only recipe declaring '{}'",
                        domain.domain_id
                    ));
                }
            }
        }
        domains.sort_by(|left, right| left.domain_id.cmp(&right.domain_id));
        if domains
            .windows(2)
            .any(|pair| pair[0].domain_id == pair[1].domain_id)
        {
            return Err("coverage domain ids must be unique".into());
        }
        self.domains = domains;
        Ok(self)
    }

    pub fn domains(&self) -> &[VerificationCoverageDomain] {
        &self.domains
    }

    /// Whether two (recipe id, revision) pairs sit in one currently declared
    /// class. Both revisions must match the current table entries, so a fact
    /// recorded under an older recipe revision never reuses across a recipe
    /// bump.
    pub fn same_declared_class(&self, left: (&str, &str), right: (&str, &str)) -> bool {
        let resolve = |id: &str, revision: &str| -> Option<&VerificationRecipe> {
            let recipe = self.get(id)?;
            (recipe.revision == revision).then_some(recipe)
        };
        let (Some(left), Some(right)) = (resolve(left.0, left.1), resolve(right.0, right.1)) else {
            return false;
        };
        let (Some(left_domain), Some(right_domain)) =
            (&left.coverage_domain, &right.coverage_domain)
        else {
            return false;
        };
        if left_domain != right_domain {
            return false;
        }
        self.domains.iter().any(|domain| {
            domain.domain_id == *left_domain
                && domain.members.iter().any(|member| member == &left.id)
                && domain.members.iter().any(|member| member == &right.id)
        })
    }

    pub fn discover(workspace: &Workspace) -> Self {
        let root = workspace.root();
        let mut recipes = Vec::new();
        if root.join("Cargo.toml").is_file() {
            push_recipe(
                &mut recipes,
                "rust.workspace",
                "Run all Cargo workspace tests",
                "cargo-workspace-v1",
                ["cargo", "test", "--workspace"],
            );
        } else {
            discover_standalone_rust(root, &mut recipes);
        }
        if root.join("go.mod").is_file() {
            push_recipe(
                &mut recipes,
                "go.all",
                "Run all Go package tests",
                "go-all-v1",
                ["go", "test", "./..."],
            );
        }
        if root.join("package.json").is_file() && package_has_test_script(root) {
            let (id, description, argv): (&str, &str, Vec<String>) =
                if root.join("pnpm-lock.yaml").is_file() {
                    (
                        "node.test.pnpm",
                        "Run the package test script with pnpm",
                        vec!["pnpm".into(), "test".into()],
                    )
                } else if root.join("yarn.lock").is_file() {
                    (
                        "node.test.yarn",
                        "Run the package test script with Yarn",
                        vec!["yarn".into(), "test".into()],
                    )
                } else {
                    (
                        "node.test.npm",
                        "Run the package test script with npm",
                        vec!["npm".into(), "test".into()],
                    )
                };
            if let Ok(recipe) = VerificationRecipe::new(id, description, "node-test-v1", argv) {
                recipes.push(recipe);
            }
        }
        if root.join("pyproject.toml").is_file()
            || root.join("pytest.ini").is_file()
            || root.join("setup.cfg").is_file()
        {
            let python = if cfg!(windows) { "python" } else { "python3" };
            push_recipe(
                &mut recipes,
                "python.pytest",
                "Run the Python test suite with pytest",
                "python-pytest-v1",
                [python, "-m", "pytest"],
            );
        }
        if root.join("CMakeLists.txt").is_file() {
            push_recipe(
                &mut recipes,
                "cmake.ctest",
                "Run CTest for the existing build directory",
                "cmake-ctest-v1",
                ["ctest", "--test-dir", "build", "--output-on-failure"],
            );
        }
        Self::new(recipes).unwrap_or_default()
    }

    pub fn as_slice(&self) -> &[VerificationRecipe] {
        &self.recipes
    }

    pub fn get(&self, id: &str) -> Option<&VerificationRecipe> {
        self.recipes
            .binary_search_by(|recipe| recipe.id.as_str().cmp(id))
            .ok()
            .map(|index| &self.recipes[index])
    }

    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }

    /// Authority binding installed beside the builtin policy table. Only
    /// recipe id and exact argv cross into Core's policy snapshot.
    pub fn host_policy(&self) -> Option<HostToolPolicy> {
        (!self.is_empty()).then(|| HostToolPolicy {
            tool_name: VERIFY_RUN_TOOL_NAME.into(),
            binding: HostEffectBinding::ExecRecipe {
                recipe_arg: "recipe_id".into(),
                recipes: self
                    .recipes
                    .iter()
                    .map(|recipe| HostExecRecipe {
                        id: recipe.id.clone(),
                        argv: recipe.argv.clone(),
                    })
                    .collect(),
            },
        })
    }

    pub(crate) fn identity_material(
        &self,
        recipe: &VerificationRecipe,
        runtime_facts: &RuntimeFactsView,
        workspace_root: &Path,
        executable_identity: &str,
    ) -> Option<String> {
        let recipe_value = serde_json::to_value(recipe).ok()?;
        let canonical_recipe = agent_contracts::jcs_serialize(&recipe_value).ok()?;
        let recipe_digest = format!("sha256-{:x}", Sha256::digest(canonical_recipe.as_bytes()));
        let workspace_inputs = if recipe.exact_workspace_snapshot {
            Some(workspace_input_digest(workspace_root)?)
        } else {
            None
        };
        let value = serde_json::json!({
            "schema": "verification-recipe-identity/v1",
            "recipe": recipe_digest,
            "platform": runtime_facts.platform,
            "architecture": runtime_facts.architecture,
            "executable": executable_identity,
            "inherited_environment": inherited_environment_digest()?,
            "exact_inputs": exact_input_digest(workspace_root, &recipe.exact_inputs)?,
            "workspace_inputs": workspace_inputs,
        });
        agent_contracts::jcs_serialize(&value).ok()
    }

    /// Identity over everything a coverage class holds constant across its
    /// members: platform, architecture, resolved executable, inherited
    /// environment, and the requesting recipe's *current* input digests.
    /// Only the per-recipe digest itself (id/argv/revision) is excluded —
    /// those differences are what declared membership forgives. Because the
    /// current input state is hashed fresh per request, any external input
    /// drift since the recorded PASS breaks the match and dispatches.
    pub(crate) fn class_shared_identity(
        &self,
        recipe: &VerificationRecipe,
        runtime_facts: &RuntimeFactsView,
        workspace_root: &Path,
        executable_identity: &str,
    ) -> Option<String> {
        let value = serde_json::json!({
            "schema": "verification-class-identity/v1",
            "platform": runtime_facts.platform,
            "architecture": runtime_facts.architecture,
            "executable": executable_identity,
            "inherited_environment": inherited_environment_digest()?,
            "exact_inputs": exact_input_digest(workspace_root, &recipe.exact_inputs)?,
            "workspace_inputs": if recipe.exact_workspace_snapshot {
                Some(workspace_input_digest(workspace_root)?)
            } else {
                None
            },
        });
        agent_contracts::jcs_serialize(&value).ok()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Hash the complete workspace input set for recipes whose compiler may load
/// transitive modules. The scan is deterministic, bounded and fail-closed:
/// links/reparse escapes, special files, unreadable entries or size/count
/// overflow disable exact reuse instead of producing a partial identity.
fn workspace_input_digest(root: &Path) -> Option<String> {
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let mut pending = vec![root.to_path_buf()];
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut entries_seen = 0usize;
    let mut declared_bytes = 0u64;

    while let Some(directory) = pending.pop() {
        let mut entries = std::fs::read_dir(directory)
            .ok()?
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).ok()?;
            if relative.components().next().is_some_and(|component| {
                matches!(
                    component.as_os_str().to_str(),
                    Some(".git" | ".focus-agent")
                )
            }) {
                continue;
            }
            entries_seen = entries_seen.checked_add(1)?;
            if entries_seen > MAX_WORKSPACE_SNAPSHOT_ENTRIES {
                return None;
            }
            let file_type = entry.file_type().ok()?;
            if file_type.is_symlink() {
                return None;
            }
            let canonical = std::fs::canonicalize(&path).ok()?;
            if !canonical.starts_with(&canonical_root) {
                return None;
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                let metadata = entry.metadata().ok()?;
                declared_bytes = declared_bytes.checked_add(metadata.len())?;
                if declared_bytes > MAX_EXACT_INPUT_BYTES {
                    return None;
                }
                files.push((relative.to_string_lossy().replace('\\', "/"), path));
            } else {
                return None;
            }
        }
    }

    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    let mut actual_bytes = 0u64;
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        let body = hash_file_bounded(
            &path,
            &mut hasher,
            &mut actual_bytes,
            relative.ends_with(".rs"),
        )?;
        if relative.ends_with(".rs") && rust_source_has_external_input_directive(&body) {
            return None;
        }
        hasher.update([0xff]);
    }
    Some(format!("sha256-{:x}", hasher.finalize()))
}

/// `include*` and `#[path]` can make rustc read outside the workspace. False
/// positives only disable reuse, so this deliberately treats comments and
/// unusual whitespace conservatively rather than trying to parse Rust here.
fn rust_source_has_external_input_directive(body: &[u8]) -> bool {
    let compact = body
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    compact
        .windows(b"include".len())
        .any(|row| row == b"include")
        || (compact.windows(2).any(|row| row == b"#[")
            && compact.windows(b"path".len()).any(|row| row == b"path"))
}

fn exact_input_digest(root: &Path, inputs: &[String]) -> Option<String> {
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let mut hasher = Sha256::new();
    let mut declared_bytes = 0u64;
    let mut actual_bytes = 0u64;
    for relative in inputs {
        let path = root.join(relative);
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        let canonical = std::fs::canonicalize(&path).ok()?;
        if !canonical.starts_with(&canonical_root) {
            return None;
        }
        declared_bytes = declared_bytes.checked_add(metadata.len())?;
        if declared_bytes > MAX_EXACT_INPUT_BYTES {
            return None;
        }
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hash_file_bounded(&path, &mut hasher, &mut actual_bytes, false)?;
        hasher.update([0xff]);
    }
    Some(format!("sha256-{:x}", hasher.finalize()))
}

/// Stream one identity input into the digest while enforcing the aggregate
/// limit against bytes actually read, not only racy metadata observed before
/// the open. Rust source is captured only for the conservative directive
/// check; all other inputs stay constant-memory.
fn hash_file_bounded(
    path: &Path,
    hasher: &mut Sha256,
    actual_bytes: &mut u64,
    capture: bool,
) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut captured = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        *actual_bytes = actual_bytes.checked_add(read as u64)?;
        if *actual_bytes > MAX_EXACT_INPUT_BYTES {
            return None;
        }
        hasher.update(&buffer[..read]);
        if capture {
            captured.extend_from_slice(&buffer[..read]);
        }
    }
    Some(captured)
}

/// Hash the complete inherited environment without copying any raw value
/// into attribution, events or checkpoints. A toolchain flag/proxy/runtime
/// change therefore creates a new exact-verification world.
fn inherited_environment_digest() -> Option<String> {
    let mut pairs = Vec::new();
    let mut total_bytes = 0usize;
    for (key, value) in std::env::vars_os() {
        if pairs.len() >= MAX_IDENTITY_ENV_VARS {
            return None;
        }
        let key = key.to_string_lossy().into_owned();
        let value = value.to_string_lossy().into_owned();
        total_bytes = total_bytes
            .saturating_add(key.len())
            .saturating_add(value.len());
        if total_bytes > MAX_IDENTITY_ENV_BYTES {
            return None;
        }
        pairs.push((key, value));
    }
    pairs.sort();
    let mut hasher = Sha256::new();
    for (key, value) in pairs {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        hasher.update(value.as_bytes());
        hasher.update([0xff]);
    }
    Some(format!("sha256-{:x}", hasher.finalize()))
}

fn push_recipe<const N: usize>(
    recipes: &mut Vec<VerificationRecipe>,
    id: &str,
    description: &str,
    revision: &str,
    argv: [&str; N],
) {
    if let Ok(recipe) = VerificationRecipe::new(
        id,
        description,
        revision,
        argv.into_iter().map(str::to_owned).collect(),
    ) {
        recipes.push(recipe);
    }
}

fn package_has_test_script(root: &Path) -> bool {
    let path = root.join("package.json");
    if std::fs::metadata(&path)
        .map(|metadata| metadata.len() > MAX_PACKAGE_JSON_BYTES)
        .unwrap_or(true)
    {
        return false;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("scripts")?
                .get("test")?
                .as_str()
                .map(str::to_owned)
        })
        .is_some_and(|script| {
            let script = script.trim();
            !script.is_empty() && !script.contains("no test specified")
        })
}

fn discover_standalone_rust(root: &Path, recipes: &mut Vec<VerificationRecipe>) {
    let src = root.join("src");
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    let mut files = Vec::new();
    for (index, entry) in entries.flatten().enumerate() {
        if index >= MAX_DISCOVERY_DIR_ENTRIES {
            // Incomplete discovery must not pretend that an arbitrary first
            // page is the project verifier catalog.
            return;
        }
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files.sort();
    files.truncate(8);
    for path in files {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        let short = format!("{:x}", Sha256::digest(relative.as_bytes()));
        let output = if cfg!(windows) {
            format!(".focus-agent/verify-{}.exe", &short[..12])
        } else {
            format!(".focus-agent/verify-{}", &short[..12])
        };
        let id = format!("rust.compile-tests:{relative}");
        let description = format!("Compile the standalone Rust test target {relative}");
        if let Ok(recipe) = VerificationRecipe::new(
            id,
            description,
            "rustc-test-compile-v1",
            vec![
                "rustc".into(),
                "--test".into(),
                relative.clone(),
                "-o".into(),
                output,
            ],
        ) {
            // `rustc --test` only compiles one declared source into the
            // runtime state directory. It neither runs user tests nor writes
            // source files, so its complete current-world identity is safe
            // to reuse. General runners above remain TaskScoped because test
            // bodies may have arbitrary side effects.
            if let Ok(recipe) = recipe
                .with_exact_current_world_reuse()
                .with_exact_inputs(vec![relative.clone()])
                .and_then(VerificationRecipe::with_exact_workspace_snapshot)
            {
                recipes.push(recipe);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_recipe(id: &str, revision: &str) -> VerificationRecipe {
        VerificationRecipe::new(id, "desc", revision, vec!["cargo".into(), "test".into()])
            .unwrap()
            .with_exact_current_world_reuse()
    }

    fn class_table() -> VerificationRecipes {
        let a = exact_recipe("verify.a", "rev-1")
            .with_coverage_domain("workspace-tests")
            .unwrap();
        let b = exact_recipe("verify.b", "rev-1")
            .with_coverage_domain("workspace-tests")
            .unwrap();
        let c = exact_recipe("verify.c", "rev-1")
            .with_coverage_domain("other-domain")
            .unwrap();
        let plain = exact_recipe("verify.plain", "rev-1");
        VerificationRecipes::new(vec![a, b, c, plain])
            .unwrap()
            .with_domains(vec![VerificationCoverageDomain {
                domain_id: "workspace-tests".into(),
                declaration_revision: 3,
                members: vec!["verify.a".into(), "verify.b".into()],
            }])
            .unwrap()
    }

    #[tokio::test]
    async fn discovers_manifest_recipe_and_policy_from_one_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = VerificationRecipes::discover(&workspace);
        assert_eq!(recipes.as_slice().len(), 1);
        assert_eq!(recipes.as_slice()[0].id, "rust.workspace");
        let policy = recipes.host_policy().unwrap();
        assert_eq!(
            policy.intent_from(&serde_json::json!({"recipe_id": "rust.workspace"})),
            agent_contracts::exec_argv_intent(&[
                "cargo".into(),
                "test".into(),
                "--workspace".into()
            ])
        );
    }

    #[tokio::test]
    async fn standalone_rust_recipe_is_generic_and_writes_only_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/protocol.rs"), "fn main() {}\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = VerificationRecipes::discover(&workspace);
        let recipe = recipes.get("rust.compile-tests:src/protocol.rs").unwrap();
        assert_eq!(&recipe.argv[..3], ["rustc", "--test", "src/protocol.rs"]);
        assert!(recipe.argv[4].starts_with(".focus-agent/verify-"));
        assert!(recipe.exact_workspace_snapshot);
    }

    #[tokio::test]
    async fn standalone_identity_covers_sibling_modules_and_fails_closed_on_include() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/protocol.rs"),
            "mod helper; fn main() { helper::value(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/helper.rs"),
            "pub fn value() -> u8 { 1 }\n",
        )
        .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = VerificationRecipes::discover(&workspace);
        let recipe = recipes.get("rust.compile-tests:src/protocol.rs").unwrap();
        let facts = workspace.runtime_facts();
        let first = recipes
            .identity_material(recipe, &facts, dir.path(), "rustc-v1")
            .unwrap();
        std::fs::write(
            dir.path().join("src/helper.rs"),
            "pub fn value() -> u8 { 2 }\n",
        )
        .unwrap();
        let second = recipes
            .identity_material(recipe, &facts, dir.path(), "rustc-v1")
            .unwrap();
        assert_ne!(first, second, "a transitive module must change identity");

        std::fs::write(
            dir.path().join("src/protocol.rs"),
            "include!(\"generated.rs\");\n",
        )
        .unwrap();
        assert!(
            recipes
                .identity_material(recipe, &facts, dir.path(), "rustc-v1")
                .is_none(),
            "external-input directives must downgrade exact reuse"
        );
    }

    #[test]
    fn streaming_identity_reader_enforces_the_actual_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growing-input.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_EXACT_INPUT_BYTES + 1).unwrap();
        drop(file);

        let mut hasher = Sha256::new();
        let mut actual_bytes = 0;
        assert!(hash_file_bounded(&path, &mut hasher, &mut actual_bytes, false).is_none());
        assert!(actual_bytes > MAX_EXACT_INPUT_BYTES);
    }

    #[test]
    fn duplicate_ids_and_unbounded_sets_fail_closed() {
        let recipe = VerificationRecipe::new("same", "same", "v1", vec!["echo".into()]).unwrap();
        assert!(VerificationRecipes::new(vec![recipe.clone(), recipe]).is_err());
        let many = (0..=MAX_VERIFICATION_RECIPES)
            .map(|index| {
                VerificationRecipe::new(format!("r{index}"), "recipe", "v1", vec!["echo".into()])
                    .unwrap()
            })
            .collect();
        assert!(VerificationRecipes::new(many).is_err());
    }

    #[test]
    fn siblings_in_one_declared_class_are_equivalent() {
        let table = class_table();
        assert!(table.same_declared_class(("verify.a", "rev-1"), ("verify.b", "rev-1")));
        assert!(table.same_declared_class(("verify.b", "rev-1"), ("verify.a", "rev-1")));
    }

    #[test]
    fn stale_revisions_cross_domains_and_undeclared_pairs_fail_closed() {
        let table = class_table();
        // revision drift on the recorded side
        assert!(!table.same_declared_class(("verify.a", "rev-0"), ("verify.b", "rev-1")));
        // revision drift on the requested side
        assert!(!table.same_declared_class(("verify.a", "rev-1"), ("verify.c2", "rev-1")));
        // different declared domains
        assert!(!table.same_declared_class(("verify.a", "rev-1"), ("verify.c", "rev-1")));
        // recipe without a domain never matches
        assert!(!table.same_declared_class(("verify.plain", "rev-1"), ("verify.a", "rev-1")));
        // unregistered id
        assert!(!table.same_declared_class(("missing", "rev-1"), ("verify.a", "rev-1")));
    }

    #[test]
    fn domain_members_must_reference_registered_declaring_recipes() {
        let a = exact_recipe("verify.a", "rev-1")
            .with_coverage_domain("d")
            .unwrap();
        let table = VerificationRecipes::new(vec![a]).unwrap();
        let missing = table.clone().with_domains(vec![VerificationCoverageDomain {
            domain_id: "d".into(),
            declaration_revision: 1,
            members: vec!["ghost".into()],
        }]);
        assert!(missing.is_err());
        let wrong = VerificationRecipes::new(vec![exact_recipe("verify.plain", "rev-1")])
            .unwrap()
            .with_domains(vec![VerificationCoverageDomain {
                domain_id: "d".into(),
                declaration_revision: 1,
                members: vec!["verify.plain".into()],
            }]);
        assert!(wrong.is_err());
    }

    #[test]
    fn task_scoped_recipes_cannot_declare_coverage_domains() {
        let plain =
            VerificationRecipe::new("plain", "desc", "r", vec!["cargo".into(), "test".into()])
                .unwrap();
        assert!(plain.with_coverage_domain("d").is_err());
    }
}
