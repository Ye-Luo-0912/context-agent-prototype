//! Capture the bounded Runtime Facts profile from the host and workspace.

use std::ffi::OsStr;
use std::sync::OnceLock;

use agent_contracts::{RUNTIME_FACTS_MAX_MARKERS, RuntimeFactsView, bound_marker};

use crate::Workspace;
use crate::confined::ConfinedDir;

/// Known root entries that are useful as project markers. Cap is 16; this
/// list is exactly that size so a full hit set stays inside the facts block.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "CMakeLists.txt",
    "Cargo.toml",
    "Gemfile",
    "composer.json",
    "go.mod",
    "lib",
    "mix.exs",
    "package.json",
    "pom.xml",
    "pubspec.yaml",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "src",
    "build.gradle",
];

/// Host-only profile: OS product/release and architecture, no workspace
/// markers. Used when composition has not attached a workspace yet.
pub fn capture_host_runtime_facts() -> RuntimeFactsView {
    RuntimeFactsView::new(detect_platform(), detect_architecture(), Vec::new())
}

impl Workspace {
    /// Confined root scan of known project markers plus the immutable host
    /// profile. Callers refresh only the marker portion after a committed
    /// workspace mutation or a successful `shell.exec` / `process.run`.
    pub fn runtime_facts(&self) -> RuntimeFactsView {
        RuntimeFactsView::new(
            detect_platform(),
            detect_architecture(),
            self.project_markers(),
        )
    }

    /// Sorted, bounded names of known manifests/dirs at the workspace root.
    /// Unknown names are never invented.
    pub fn project_markers(&self) -> Vec<String> {
        let Ok(root) = ConfinedDir::open_root(&self.root) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for name in PROJECT_MARKERS {
            if found.len() >= RUNTIME_FACTS_MAX_MARKERS {
                break;
            }
            if root.open_existing(OsStr::new(name)).is_ok() {
                let marker = bound_marker(name);
                if !marker.is_empty() {
                    found.push(marker);
                }
            }
        }
        found.sort();
        found
    }
}

fn detect_architecture() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64".into(),
        "aarch64" => "aarch64".into(),
        _ => "unknown".into(),
    }
}

fn detect_platform() -> String {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(detect_platform_inner).clone()
}

fn detect_platform_inner() -> String {
    #[cfg(windows)]
    {
        windows_product()
    }
    #[cfg(target_os = "linux")]
    {
        linux_product()
    }
    #[cfg(target_os = "macos")]
    {
        macos_product()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        "unknown".into()
    }
}

#[cfg(windows)]
fn windows_product() -> String {
    let output = std::process::Command::new("cmd")
        .args(["/C", "ver"])
        .output();
    let Ok(output) = output else {
        return "windows unknown".into();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    parse_windows_ver(&text).unwrap_or_else(|| "windows unknown".into())
}

#[cfg(windows)]
fn parse_windows_ver(text: &str) -> Option<String> {
    // `Microsoft Windows [Version 10.0.26300.1]`
    let start = text.find("Version ")? + "Version ".len();
    let rest = text[start..].trim_start();
    let version: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let _minor = parts.next();
    let build: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some(if major >= 10 && build >= 22000 {
        "windows 11".into()
    } else if major >= 10 {
        "windows 10".into()
    } else if major > 0 {
        format!("windows {major}")
    } else {
        "windows unknown".into()
    })
}

#[cfg(target_os = "linux")]
fn linux_product() -> String {
    let Ok(text) = std::fs::read_to_string("/etc/os-release") else {
        return "linux unknown".into();
    };
    parse_os_release(&text)
}

#[cfg(target_os = "linux")]
fn parse_os_release(text: &str) -> String {
    let mut id = None;
    let mut version_id = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ID=") {
            id = Some(unquote(value));
        } else if let Some(value) = line.strip_prefix("VERSION_ID=") {
            version_id = Some(unquote(value));
        }
    }
    match (id, version_id) {
        (Some(id), Some(version)) if !id.is_empty() && !version.is_empty() => {
            format!("{id} {version}")
        }
        (Some(id), _) if !id.is_empty() => format!("{id} unknown"),
        _ => "linux unknown".into(),
    }
}

#[cfg(target_os = "linux")]
fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

#[cfg(target_os = "macos")]
fn macos_product() -> String {
    let output = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output();
    let Ok(output) = output else {
        return "macos unknown".into();
    };
    let version = String::from_utf8_lossy(&output.stdout);
    let major = version
        .trim()
        .split('.')
        .next()
        .filter(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()));
    match major {
        Some(major) => format!("macos {major}"),
        None => "macos unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architecture_is_normalized() {
        let arch = detect_architecture();
        assert!(arch == "x86_64" || arch == "aarch64" || arch == "unknown");
    }

    #[cfg(windows)]
    #[test]
    fn windows_ver_hides_build_number() {
        assert_eq!(
            parse_windows_ver("Microsoft Windows [Version 10.0.26300.1]").as_deref(),
            Some("windows 11")
        );
        assert_eq!(
            parse_windows_ver("Microsoft Windows [Version 10.0.19045.3803]").as_deref(),
            Some("windows 10")
        );
        let facts = capture_host_runtime_facts();
        assert!(
            facts.platform == "windows 11"
                || facts.platform == "windows 10"
                || facts.platform.starts_with("windows ")
        );
        assert!(!facts.render().contains("26300"));
        assert!(facts.markers.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn os_release_uses_id_and_version() {
        assert_eq!(
            parse_os_release("ID=ubuntu\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n"),
            "ubuntu 24.04"
        );
        assert_eq!(parse_os_release("ID=debian\n"), "debian unknown");
        assert_eq!(parse_os_release(""), "linux unknown");
    }

    #[tokio::test]
    async fn project_markers_are_confined_and_not_invented() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let markers = workspace.project_markers();
        assert_eq!(markers, vec![".git", "Cargo.toml", "src"]);
        assert!(!markers.iter().any(|m| m == "package.json"));
        let facts = workspace.runtime_facts();
        assert!(facts.render().contains("Cargo.toml"));
        assert!(
            !facts
                .render()
                .contains(dir.path().to_string_lossy().as_ref())
        );
    }
}
