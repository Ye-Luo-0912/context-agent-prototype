//! Host capability configuration (B4/N8): bounded, fail-closed parsing of
//! the MCP server declarations and plugin package roots the host may be
//! started with.
//!
//! `--mcp-config` names a JSON file whose top-level value is a bounded array
//! of MCP server declarations; `--plugins-root` names a directory whose
//! subdirectories each may carry a `plugin.json` package manifest. Both
//! ingest paths refuse everything they cannot fully honor — a parse failure,
//! an over-bound declaration, a manifest that fails admission, or a root
//! that is not a directory all fail startup closed. The operator's explicit
//! configuration IS the enabling act: every discovered plugin package is
//! installed, enabled and its declared skills activated by the caller.

use std::path::{Path, PathBuf};

use agent_capability_process::McpServerDecl;
use agent_contracts::PluginPackageManifest;
use serde::Deserialize;

/// How many MCP server declarations one config file may carry.
pub const MAX_MCP_SERVERS: usize = 32;
/// Upper bound on one config file's raw size (a JSON array of bounded
/// declarations; nothing here needs more).
pub const MAX_CONFIG_FILE_BYTES: u64 = 256 * 1024;
/// Capability-id byte bound for an MCP server id.
pub const MAX_MCP_ID_BYTES: usize = 64;
/// Byte bound on the program path/name to spawn.
pub const MAX_MCP_PROGRAM_BYTES: usize = 1024;
/// Byte bound on a single argument or permission word.
pub const MAX_MCP_WORD_BYTES: usize = 512;
/// How many spawn arguments one declaration may carry.
pub const MAX_MCP_ARGS: usize = 64;
/// How many declared permission words one declaration may carry.
pub const MAX_MCP_PERMISSIONS: usize = 16;
/// The fixed manifest file name a plugin package directory must carry.
pub const PLUGIN_MANIFEST_FILE: &str = "plugin.json";

/// One MCP server declaration, exactly as the operator wrote it: the wire
/// shape mirrors `McpServerDecl` minus the platform-only write roots (a
/// production config never grants extra landlock roots; compose wires the
/// adapter with only its private cwd).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpConfigEntry {
    id: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

impl McpConfigEntry {
    fn validate(&self) -> anyhow::Result<()> {
        validate_server_id(&self.id)?;
        if self.program.is_empty() || self.program.len() > MAX_MCP_PROGRAM_BYTES {
            anyhow::bail!(
                "MCP server '{}' program must be 1..={MAX_MCP_PROGRAM_BYTES} bytes",
                self.id
            );
        }
        if self.args.len() > MAX_MCP_ARGS {
            anyhow::bail!(
                "MCP server '{}' declares {} args, above the {MAX_MCP_ARGS} bound",
                self.id,
                self.args.len()
            );
        }
        for arg in &self.args {
            if arg.is_empty() || arg.len() > MAX_MCP_WORD_BYTES {
                anyhow::bail!(
                    "MCP server '{}' has an arg outside 1..={MAX_MCP_WORD_BYTES} bytes",
                    self.id
                );
            }
        }
        if self.permissions.len() > MAX_MCP_PERMISSIONS {
            anyhow::bail!(
                "MCP server '{}' declares {} permissions, above the {MAX_MCP_PERMISSIONS} bound",
                self.id,
                self.permissions.len()
            );
        }
        for permission in &self.permissions {
            if permission.is_empty() || permission.len() > MAX_MCP_WORD_BYTES {
                anyhow::bail!(
                    "MCP server '{}' has a permission word outside 1..={MAX_MCP_WORD_BYTES} bytes",
                    self.id
                );
            }
        }
        Ok(())
    }

    fn into_decl(self) -> McpServerDecl {
        let McpConfigEntry {
            id,
            version,
            name,
            summary,
            program,
            args,
            permissions,
        } = self;
        McpServerDecl {
            id,
            version,
            name,
            summary,
            program,
            args,
            permissions,
            // A configured server may mutate only its private cwd; the
            // operator never names extra write roots through this surface.
            extra_write_roots: Vec::new(),
        }
    }
}

/// Light capability-id shape check shared by MCP ids (full capability
/// admission runs inside compose registration). Lowercase ASCII identifier,
/// same grammar the protocol and capability plane use.
fn validate_server_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.len() > MAX_MCP_ID_BYTES {
        anyhow::bail!("MCP server id must be 1..={MAX_MCP_ID_BYTES} bytes");
    }
    let mut bytes = id.bytes();
    let Some(first) = bytes.next() else {
        anyhow::bail!("MCP server id must not be empty");
    };
    if !first.is_ascii_lowercase() {
        anyhow::bail!("MCP server id must start with a lowercase ASCII letter");
    }
    if !bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        anyhow::bail!(
            "MCP server id must contain only lowercase ASCII letters, digits, '.', '_' or '-'"
        );
    }
    Ok(())
}

/// Parse the host's MCP server declarations from a JSON file. The file must
/// be a bounded JSON array of declarations; unknown fields, over-bounds and
/// any read/decode failure refuse the whole configuration (fail closed —
/// operator intent is never silently dropped).
pub fn parse_mcp_config(path: &Path) -> anyhow::Result<Vec<McpServerDecl>> {
    use anyhow::Context as _;
    let metadata =
        std::fs::metadata(path).with_context(|| format!("read MCP config {}", path.display()))?;
    if metadata.len() > MAX_CONFIG_FILE_BYTES {
        anyhow::bail!(
            "MCP config {} exceeds the {MAX_CONFIG_FILE_BYTES}-byte bound",
            path.display()
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("read MCP config {}", path.display()))?;
    let entries: Vec<McpConfigEntry> = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parse MCP config {} as a JSON array of server declarations",
            path.display()
        )
    })?;
    if entries.len() > MAX_MCP_SERVERS {
        anyhow::bail!(
            "MCP config {} declares {} servers, above the {MAX_MCP_SERVERS} bound",
            path.display(),
            entries.len()
        );
    }
    // Deduplicate ids: two declarations for the same id would collide in the
    // capability registry; refuse instead of silently keeping one.
    let mut seen = std::collections::HashSet::new();
    for entry in &entries {
        entry.validate()?;
        if !seen.insert(entry.id.as_str()) {
            anyhow::bail!(
                "MCP config {} declares server id '{}' more than once",
                path.display(),
                entry.id
            );
        }
    }
    Ok(entries.into_iter().map(McpConfigEntry::into_decl).collect())
}

/// Discover plugin package roots under `root`, deterministically. Each
/// subdirectory that carries a `plugin.json` is parsed and admitted; a
/// directory without one is skipped as a non-package; a manifest that exists
/// but fails parse or admission refuses the whole discovery (fail closed).
/// Returns packages in directory-name order.
pub fn discover_plugin_packages(
    root: &Path,
) -> anyhow::Result<Vec<(PluginPackageManifest, PathBuf)>> {
    use anyhow::Context as _;
    if !root.is_dir() {
        anyhow::bail!("plugins root {} is not a directory", root.display());
    }
    let mut entries: Vec<_> = std::fs::read_dir(root)
        .with_context(|| format!("read plugins root {}", root.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("read plugins root {}", root.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut packages = Vec::new();
    for entry in entries {
        let package_root = entry.path();
        if !package_root.is_dir() {
            continue;
        }
        let manifest_path = package_root.join(PLUGIN_MANIFEST_FILE);
        if !manifest_path.is_file() {
            // Not a package directory; deliberately not an error.
            continue;
        }
        let bytes = std::fs::read(&manifest_path)
            .with_context(|| format!("read plugin manifest {}", manifest_path.display()))?;
        let manifest: PluginPackageManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse plugin manifest {}", manifest_path.display()))?;
        agent_core::PluginPackageAdmission::validate_static(&manifest)
            .with_context(|| format!("admit plugin manifest {}", manifest_path.display()))?;
        packages.push((manifest, package_root));
    }
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn mcp_config_parses_declarations_bounded_and_rejects_unknown_fields() {
        let dir = temp_dir();
        let config = dir.path().join("mcp.json");
        write(
            &config,
            r#"[
                {"id": "fs.server", "version": "1.0.0", "name": "fs",
                 "summary": "file tools", "program": "mcp-fs",
                 "args": ["--root", "/srv/ws"], "permissions": ["read"]},
                {"id": "shell.exec", "program": "mcp-shell", "permissions": ["workspace_write", "exec"]}
            ]"#,
        );
        let servers = parse_mcp_config(&config).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].id, "fs.server");
        assert_eq!(servers[0].program, "mcp-fs");
        assert_eq!(servers[0].args, vec!["--root", "/srv/ws"]);
        assert!(
            servers[0].extra_write_roots.is_empty(),
            "no extra write roots from config"
        );
        assert_eq!(servers[1].id, "shell.exec");
        assert_eq!(servers[1].permissions, vec!["workspace_write", "exec"]);

        // Unknown fields are refused (serde deny_unknown_fields).
        write(
            &config,
            r#"[{"id": "a", "program": "p", "surprise": true}]"#,
        );
        assert!(parse_mcp_config(&config).is_err());

        // An empty program is refused.
        write(&config, r#"[{"id": "a", "program": ""}]"#);
        assert!(parse_mcp_config(&config).is_err());

        // A bad id is refused.
        write(&config, r#"[{"id": "Not-Valid_Id!", "program": "p"}]"#);
        assert!(parse_mcp_config(&config).is_err());

        // Over the server-count bound.
        let mut many = String::from("[");
        for i in 0..=MAX_MCP_SERVERS {
            if i > 0 {
                many.push(',');
            }
            many.push_str(&format!(r#"{{"id": "srv{i}", "program": "p"}}"#));
        }
        many.push(']');
        write(&config, &many);
        assert!(parse_mcp_config(&config).is_err());

        // Duplicate ids are refused.
        write(
            &config,
            r#"[{"id": "a", "program": "p1"}, {"id": "a", "program": "p2"}]"#,
        );
        assert!(parse_mcp_config(&config).is_err());

        // A missing file is an error, never an empty configuration.
        assert!(parse_mcp_config(&dir.path().join("absent.json")).is_err());
    }

    #[test]
    fn plugin_root_discovers_packages_deterministically_and_skips_non_packages() {
        let dir = temp_dir();
        let manifest = r#"{
            "id": "pkg-a", "version": "1.0.0", "name": "Package A", "summary": "skills",
            "api": "0.1",
            "skills": [{"id": "skill-1", "version": "1.0.0", "summary": "s",
                        "reference": "skills/skill-1.md", "provenance": "package"}]
        }"#;
        // Two package directories (out of name order) plus one non-package.
        write(
            &dir.path().join("zeta").join(PLUGIN_MANIFEST_FILE),
            manifest,
        );
        let beta = manifest
            .replace("pkg-a", "pkg-b")
            .replace("Package A", "Package B");
        write(&dir.path().join("alpha").join(PLUGIN_MANIFEST_FILE), &beta);
        write(&dir.path().join("notes"), "not a plugin");

        let packages = discover_plugin_packages(dir.path()).unwrap();
        let ids: Vec<&str> = packages.iter().map(|(m, _)| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["pkg-b", "pkg-a"],
            "deterministic directory-name order"
        );

        // A manifest that exists but fails admission refuses the whole root.
        write(
            &dir.path().join("bad").join(PLUGIN_MANIFEST_FILE),
            r#"{"id": "bad pkg", "version": "1.0.0", "name": "", "summary": "s"}"#,
        );
        assert!(discover_plugin_packages(dir.path()).is_err());

        // A root that is not a directory is refused.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(discover_plugin_packages(&file).is_err());
        assert!(discover_plugin_packages(&dir.path().join("absent")).is_err());

        // A broken manifest file fails closed too.
        write(
            &dir.path().join("broken").join(PLUGIN_MANIFEST_FILE),
            "{ not json",
        );
        assert!(discover_plugin_packages(dir.path()).is_err());
    }
}
