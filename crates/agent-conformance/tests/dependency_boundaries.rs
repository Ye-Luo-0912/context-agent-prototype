//! Architectural dependency checks for the Agent OS trust boundary.
//!
//! These assertions intentionally inspect Cargo's production/build graph and
//! ignore dev-dependencies: contract tests may compose concrete implementations,
//! while shipped libraries must preserve the dependency direction in AGENTS.md.
//!
//! The checks are a layer/role matrix rather than a denylist of pairs. Every
//! workspace crate carries exactly one role; a crate without a role fails so a
//! new implementation cannot silently join the graph. A role may depend only on
//! the supplier roles the matrix allows, and the AGENTS.md semantic
//! prohibitions are re-checked transitively at role granularity so a helper
//! crate cannot hide a forbidden path.
//!
//! Two narrow edges are documented and enforced at source level, not just as
//! graph facts:
//! - `agent-workspace -> agent-process` exists only for the process-journal
//!   identity/kill helpers and is confined to `process_journal.rs` plus four
//!   exported symbols.
//! - `tool-runtime` is allowed to depend on `agent-contracts` where the
//!   `ContextEngine` trait lives, but must never name it: a source mention is
//!   the only way the semantic prohibition can be violated once the graph
//!   check passes.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

/// Crate -> role assignment. Every production crate in the workspace must be
/// listed here; adding a new crate without assigning a role fails the check.
const ROLES: &[(&str, &[&str])] = &[
    ("contract", &["agent-contracts"]),
    ("wire", &["agent-platform-protocol"]),
    ("context-engine", &["context-simple", "context-baselines"]),
    ("process-layer", &["agent-process"]),
    ("workspace", &["agent-workspace"]),
    ("storage", &["agent-storage"]),
    ("context-adapter", &["context-contextcore"]),
    ("tool-layer", &["tool-runtime"]),
    ("capability-process", &["agent-capability-process"]),
    ("core", &["agent-core"]),
    ("runtime", &["agent-runtime"]),
    ("governance", &["agent-conformance"]),
    ("provider", &["provider-openai"]),
    (
        "composition",
        &[
            "agent-compose",
            "agent-tui",
            "agent-eval",
            "agent-replay",
            "agent-context-service",
        ],
    ),
];

/// Role -> allowed direct supplier roles. Any production/build edge whose
/// supplier role is absent here is a violation; the list must be extended
/// deliberately when the architecture admits a new edge.
const ALLOWED_SUPPLIERS: &[(&str, &[&str])] = &[
    ("contract", &[]),
    ("wire", &["contract"]),
    ("context-engine", &["contract"]),
    ("process-layer", &["contract", "wire"]),
    ("workspace", &["contract", "process-layer"]),
    ("storage", &["contract"]),
    ("context-adapter", &["contract", "process-layer"]),
    ("tool-layer", &["contract", "process-layer", "workspace"]),
    ("capability-process", &["contract", "wire", "process-layer"]),
    ("core", &["contract"]),
    ("runtime", &["contract", "core", "wire", "workspace"]),
    ("governance", &["contract"]),
    ("provider", &["contract"]),
    // Composition roots are the trusted wiring seam that selects concrete
    // implementations. They may consume every role, including each other
    // (agent-tui and agent-eval compose through agent-compose); a role added
    // in the future must be admitted here explicitly or composition crates
    // cannot use it.
    (
        "composition",
        &[
            "contract",
            "wire",
            "context-engine",
            "process-layer",
            "workspace",
            "storage",
            "context-adapter",
            "tool-layer",
            "capability-process",
            "core",
            "runtime",
            "governance",
            "provider",
            "composition",
        ],
    ),
];

/// Semantic prohibitions from AGENTS.md, checked transitively at role
/// granularity so a helper crate cannot hide a forbidden path.
const FORBIDDEN_PATHS: &[(&str, &str)] = &[
    ("core", "context-engine"),
    ("core", "composition"),
    ("runtime", "context-engine"),
    ("runtime", "tool-layer"),
    ("tool-layer", "context-engine"),
    ("context-engine", "tool-layer"),
    ("core", "runtime"),
];

/// The single file through which `agent-workspace` may reach `agent-process`.
const WORKSPACE_PROCESS_EDGE_FILE: &str = "process_journal.rs";

/// The only `agent-process` symbols the process-journal edge may use.
const WORKSPACE_PROCESS_SYMBOLS: &[&str] = &[
    "capture_process_identity",
    "kill_matching_process_tree",
    "process_identity_matches",
    "ProcessIdentity",
];

#[derive(Debug)]
struct WorkspaceGraph {
    edges: BTreeMap<String, BTreeSet<String>>,
}

impl WorkspaceGraph {
    fn load() -> Self {
        let workspace_root = workspace_root();
        let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let output = Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(&workspace_root)
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to run cargo metadata from {}: {error}",
                    workspace_root.display()
                )
            });
        assert!(
            output.status.success(),
            "cargo metadata failed from {}:\n{}",
            workspace_root.display(),
            String::from_utf8_lossy(&output.stderr)
        );

        let metadata: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("cargo metadata returned invalid JSON: {error}"));
        let packages = metadata["packages"]
            .as_array()
            .expect("cargo metadata packages must be an array");
        let workspace_packages: BTreeSet<&str> = packages
            .iter()
            .map(|package| {
                package["name"]
                    .as_str()
                    .expect("workspace package must have a name")
            })
            .collect();

        let mut edges = BTreeMap::new();
        for package in packages {
            let name = package["name"]
                .as_str()
                .expect("workspace package must have a name");
            let dependencies = package["dependencies"]
                .as_array()
                .expect("workspace package dependencies must be an array");
            let production_dependencies = dependencies
                .iter()
                .filter(|dependency| dependency["kind"].as_str() != Some("dev"))
                .filter_map(|dependency| dependency["name"].as_str())
                .filter(|dependency| workspace_packages.contains(dependency))
                .map(str::to_owned)
                .collect();
            edges.insert(name.to_owned(), production_dependencies);
        }

        Self { edges }
    }

    fn direct_dependencies(&self, package: &str) -> &BTreeSet<String> {
        self.edges
            .get(package)
            .unwrap_or_else(|| panic!("workspace package `{package}` is missing from metadata"))
    }

    fn path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let mut queue = VecDeque::from([(from.to_owned(), vec![from.to_owned()])]);
        let mut visited = BTreeSet::from([from.to_owned()]);

        while let Some((current, path)) = queue.pop_front() {
            for dependency in self.direct_dependencies(&current) {
                let mut next_path = path.clone();
                next_path.push(dependency.clone());
                if dependency == to {
                    return Some(next_path);
                }
                if visited.insert(dependency.clone()) {
                    queue.push_back((dependency.clone(), next_path));
                }
            }
        }
        None
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agent-conformance must live at <workspace>/crates/agent-conformance")
        .to_path_buf()
}

fn assert_no_path(graph: &WorkspaceGraph, from: &str, to: &str) {
    if let Some(path) = graph.path(from, to) {
        panic!(
            "forbidden production/build dependency path: {}",
            path.join(" -> ")
        );
    }
}

/// Build crate -> role and role -> crates maps, rejecting dead role entries
/// and role names that no crate carries.
fn role_index() -> (
    BTreeMap<&'static str, &'static str>,
    BTreeMap<&'static str, Vec<&'static str>>,
) {
    let mut crate_role = BTreeMap::new();
    let mut role_crates: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (role, crates) in ROLES.iter().copied() {
        for crate_name in crates.iter().copied() {
            let previous = crate_role.insert(crate_name, role);
            assert!(
                previous.is_none(),
                "role table assigns `{crate_name}` to multiple roles"
            );
            role_crates.entry(role).or_default().push(crate_name);
        }
    }
    let listed_roles: BTreeSet<&str> = role_crates.keys().copied().collect();
    for (role, suppliers) in ALLOWED_SUPPLIERS.iter().copied() {
        assert!(
            listed_roles.contains(role),
            "allowed-supplier table names unknown role `{role}`"
        );
        for supplier in suppliers.iter().copied() {
            assert!(
                listed_roles.contains(supplier),
                "allowed-supplier table names unknown role `{supplier}`"
            );
        }
    }
    for (from, to) in FORBIDDEN_PATHS.iter().copied() {
        assert!(listed_roles.contains(from) && listed_roles.contains(to));
    }
    (crate_role, role_crates)
}

/// Check the role matrix over the real production/build graph: every crate is
/// registered, every direct edge is allowed for the consumer's role, and every
/// forbidden role path stays unreachable transitively.
fn check_role_matrix(graph: &WorkspaceGraph) -> Vec<String> {
    let (crate_role, role_crates) = role_index();
    let mut violations = Vec::new();

    for name in graph.edges.keys() {
        if !crate_role.contains_key(name.as_str()) {
            violations.push(format!(
                "workspace crate `{name}` has no role; new production crates must be assigned \
                 a role and admitted supplier roles in this matrix"
            ));
        }
    }
    for (role, crates) in &role_crates {
        for crate_name in crates {
            if !graph.edges.contains_key(*crate_name) {
                violations.push(format!(
                    "role `{role}` lists `{crate_name}`, but that crate is absent from the \
                     workspace graph; rename or retire the stale entry"
                ));
            }
        }
    }

    for (consumer, dependencies) in &graph.edges {
        let Some(consumer_role) = crate_role.get(consumer.as_str()) else {
            continue;
        };
        for dependency in dependencies {
            let Some(dependency_role) = crate_role.get(dependency.as_str()) else {
                continue;
            };
            let allowed = ALLOWED_SUPPLIERS
                .iter()
                .find(|(role, _)| role == consumer_role)
                .map(|(_, suppliers)| suppliers)
                .expect("consumer role must appear in the allowed-supplier table");
            if !allowed.contains(dependency_role) {
                violations.push(format!(
                    "undocumented production/build edge: {consumer} ({consumer_role}) -> \
                     {dependency} ({dependency_role}); role `{consumer_role}` may depend only on: {}",
                    allowed.join(", ")
                ));
            }
        }
    }

    for (from_role, to_role) in FORBIDDEN_PATHS {
        let from_crates = role_crates
            .get(from_role)
            .expect("forbidden role must exist");
        let to_crates = role_crates.get(to_role).expect("forbidden role must exist");
        for from in from_crates {
            if !graph.edges.contains_key(*from) {
                continue;
            }
            for to in to_crates {
                if !graph.edges.contains_key(*to) {
                    continue;
                }
                if let Some(path) = graph.path(from, to) {
                    violations.push(format!(
                        "forbidden production/build dependency path: {}",
                        path.join(" -> ")
                    ));
                }
            }
        }
    }

    violations
}

/// Recursively collect Rust source files under a directory.
fn source_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("source directory must be readable") {
            let path = entry.expect("directory entry must be readable").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// Collect every `agent_process` symbol referenced by a source text: both
/// `agent_process::Name` paths and `use agent_process::{..., Name as Alias}`.
fn agent_process_symbols(text: &str) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();
    let mut rest = text;
    while let Some(position) = rest.find("agent_process::") {
        let after = &rest[position + "agent_process::".len()..];
        let identifier: String = after
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        if !identifier.is_empty() {
            symbols.insert(identifier);
        }
        rest = after;
    }
    let blocks: Vec<&str> = text.split("use agent_process::").skip(1).collect();
    for block in blocks {
        let trimmed = block.trim_start();
        if trimmed.starts_with('{') {
            if let Some(close) = trimmed.find('}') {
                for name in trimmed[1..close].split(',') {
                    let name = name.trim();
                    let base = name.split_whitespace().next().unwrap_or(name);
                    if !base.is_empty() {
                        symbols.insert(base.to_owned());
                    }
                }
            }
        } else {
            let identifier: String = trimmed
                .chars()
                .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                .collect();
            if !identifier.is_empty() {
                symbols.insert(identifier);
            }
        }
    }
    symbols
}

/// The `agent-workspace -> agent-process` edge is documented as a narrow
/// process-journal dependency: a single file may import `agent-process` and
/// only the identity/kill helper symbols.
fn check_workspace_process_edge(workspace_root: &Path) -> Vec<String> {
    let src = workspace_root.join("crates/agent-workspace/src");
    let mut violations = Vec::new();
    for file in source_files(&src) {
        let text = fs::read_to_string(&file).expect("source file must be readable");
        if !text.contains("agent_process") {
            continue;
        }
        let relative = file
            .strip_prefix(&src)
            .expect("source file must live under the crate src directory");
        if relative != Path::new(WORKSPACE_PROCESS_EDGE_FILE) {
            violations.push(format!(
                "agent-workspace/src/{} references agent-process outside the documented \
                 {WORKSPACE_PROCESS_EDGE_FILE} narrow edge",
                relative.display()
            ));
            continue;
        }
        for symbol in agent_process_symbols(&text) {
            if !WORKSPACE_PROCESS_SYMBOLS.contains(&symbol.as_str()) {
                violations.push(format!(
                    "agent-workspace/src/{WORKSPACE_PROCESS_EDGE_FILE} uses agent-process symbol \
                     `{symbol}` outside the allowed edge symbols: {}",
                    WORKSPACE_PROCESS_SYMBOLS.join(", ")
                ));
            }
        }
    }
    violations
}

/// `tool-runtime` may depend on `agent-contracts` (where the `ContextEngine`
/// trait lives) but must never name it; a source mention is a semantic
/// violation the graph alone cannot see.
fn check_tool_runtime_avoids_context_engine(workspace_root: &Path) -> Vec<String> {
    let src = workspace_root.join("crates/tool-runtime/src");
    let mut violations = Vec::new();
    for file in source_files(&src) {
        let text = fs::read_to_string(&file).expect("source file must be readable");
        if text.contains("ContextEngine") {
            violations.push(format!(
                "tool-runtime/src/{file} names ContextEngine; tools return ToolOutput and the \
                 kernel decides what enters context",
                file = file
                    .strip_prefix(&src)
                    .expect("source file must live under the crate src directory")
                    .display()
            ));
        }
    }
    violations
}

#[test]
fn production_dependencies_preserve_the_agent_os_boundary() {
    let graph = WorkspaceGraph::load();

    let violations = check_role_matrix(&graph);
    assert!(
        violations.is_empty(),
        "production/build graph violates the layer/role matrix:\n{}",
        violations.join("\n")
    );

    // Anchored positive edges the matrix only guards negatively: contracts
    // stay the bottom layer and the runtime keeps owning the router seam.
    assert!(
        graph.direct_dependencies("agent-contracts").is_empty(),
        "forbidden production/build edge(s) from agent-contracts: {}",
        graph
            .direct_dependencies("agent-contracts")
            .iter()
            .map(|dependency| format!("agent-contracts -> {dependency}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let protocol_dependencies = graph.direct_dependencies("agent-platform-protocol");
    assert_eq!(
        protocol_dependencies,
        &BTreeSet::from(["agent-contracts".to_owned()]),
        "agent-platform-protocol production/build dependencies must remain exactly \
         agent-contracts; got: {}",
        protocol_dependencies
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        graph
            .direct_dependencies("agent-runtime")
            .contains("agent-platform-protocol"),
        "agent-runtime must own the Platform Protocol router seam"
    );
    assert_no_path(&graph, "agent-platform-protocol", "agent-runtime");
}

#[test]
fn source_level_narrow_edges_are_enforced() {
    let workspace_root = workspace_root();
    let mut violations = Vec::new();
    violations.extend(check_workspace_process_edge(&workspace_root));
    violations.extend(check_tool_runtime_avoids_context_engine(&workspace_root));
    assert!(
        violations.is_empty(),
        "source-level boundary violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn unregistered_crates_fail_until_assigned_a_role() {
    let graph = WorkspaceGraph {
        edges: BTreeMap::from([
            ("agent-contracts".to_owned(), BTreeSet::new()),
            (
                "agent-phantom".to_owned(),
                BTreeSet::from(["agent-contracts".to_owned()]),
            ),
        ]),
    };
    let violations = check_role_matrix(&graph);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("agent-phantom") && violation.contains("no role")),
        "matrix must flag a crate that joins the graph without a role: {violations:?}"
    );
}
