//! Architectural dependency checks for the Agent OS trust boundary.
//!
//! These assertions intentionally inspect Cargo's production/build graph and
//! ignore dev-dependencies: contract tests may compose concrete implementations,
//! while shipped libraries must preserve the dependency direction in AGENTS.md.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

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

#[test]
fn production_dependencies_preserve_the_agent_os_boundary() {
    let graph = WorkspaceGraph::load();

    // AGENTS.md's explicit forbidden dependencies, checked transitively so a
    // helper crate cannot hide a concrete implementation from the boundary.
    for (from, to) in [
        ("agent-core", "context-simple"),
        ("agent-core", "agent-tui"),
        ("agent-runtime", "context-simple"),
        ("agent-runtime", "tool-runtime"),
        ("tool-runtime", "context-simple"),
        ("context-simple", "tool-runtime"),
        ("agent-core", "agent-runtime"),
    ] {
        assert_no_path(&graph, from, to);
    }

    // Contracts remain the bottom layer. A path dependency here would make
    // every trait consumer inherit a concrete implementation or authority.
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

    // The transport-independent Platform protocol is also a bottom-layer
    // contract. It may reuse shared IDs from agent-contracts, but must not
    // acquire process, runtime, Core, workspace or concrete implementation
    // dependencies as adapters migrate onto it.
    let protocol_dependencies = graph.direct_dependencies("agent-platform-protocol");
    assert_eq!(
        protocol_dependencies,
        &BTreeSet::from(["agent-contracts".to_owned()]),
        "agent-platform-protocol production/build dependencies must remain exactly agent-contracts; got: {}",
        protocol_dependencies
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    for forbidden in [
        "agent-core",
        "agent-runtime",
        "agent-process",
        "agent-workspace",
        "context-simple",
        "tool-runtime",
    ] {
        assert_no_path(&graph, "agent-platform-protocol", forbidden);
    }

    // Platform owns the semantic router and may depend on the bottom-layer
    // DTO crate; the inverse dependency would let protocol code reach the
    // actor/Core and turn a wire contract into a second orchestrator.
    assert!(
        graph
            .direct_dependencies("agent-runtime")
            .contains("agent-platform-protocol"),
        "agent-runtime must own the Platform Protocol router seam"
    );
    assert_no_path(&graph, "agent-platform-protocol", "agent-runtime");

    // Current isolated/adapted Platform clients are the executable proxy for
    // the future SDK rule: they may use contracts/process facilities, never
    // Core authority or RuntimeActor internals (directly or through helpers).
    for client in [
        "agent-process",
        "agent-capability-process",
        "agent-context-service",
        "context-contextcore",
        "provider-openai",
        "tool-runtime",
    ] {
        assert_no_path(&graph, client, "agent-core");
        assert_no_path(&graph, client, "agent-runtime");
    }

    // Concrete context/tool implementations are selected only by explicit
    // composition or product roots. Dev-only conformance dependencies were
    // removed while constructing the graph above.
    let concrete_implementations = BTreeSet::from([
        "context-simple",
        "context-baselines",
        "context-contextcore",
        "tool-runtime",
    ]);
    let composition_roots = BTreeSet::from([
        "agent-compose",
        "agent-tui",
        "agent-eval",
        "agent-replay",
        "agent-context-service",
    ]);
    let mut offending_edges = Vec::new();
    for (consumer, dependencies) in &graph.edges {
        for dependency in dependencies {
            if concrete_implementations.contains(dependency.as_str())
                && !composition_roots.contains(consumer.as_str())
            {
                offending_edges.push(format!("{consumer} -> {dependency}"));
            }
        }
    }
    assert!(
        offending_edges.is_empty(),
        "concrete implementations escaped the composition roots: {}",
        offending_edges.join(", ")
    );
}
