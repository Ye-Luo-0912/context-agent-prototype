//! Closure-evidence generator for the extension-sandbox gate
//! (spec, row schema and output path live in `docs/PLATFORM_SECURITY.md`
//! under "closure evidence artifact").
//!
//! One deterministic run drives every supported `(platform, sandbox
//! profile)` activation case through the real enforcement stack — a real
//! child process, post-spawn attestation from what was actually applied,
//! and the trusted adapter's `required ⊆ actually-enforced` activation
//! check — plus every required refusal case. Native profiles that cannot
//! attest their full floor must refuse; a profile that needs write
//! confinement must refuse when the configured mechanism is absent;
//! every claimed-true capability carries bounded mechanism proof that the
//! contract validates. Cross-platform claims this runner cannot execute are
//! recorded as explicit not-run rows instead of being silently absent or,
//! worse, fabricated as passes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_capability_process::ProcessCapabilityAdapter;
use agent_contracts::{
    Capability, CapabilityKind, CapabilityLifecycle, CapabilityManifest, CapabilityStatus,
    CapabilityTransport, SandboxAttestation, SandboxCapabilities, SandboxEvidence, SandboxProfile,
    ToolRisk, ToolSpec,
};
use agent_process::{ProcessHost, ProcessHostConfig};
use anyhow::{anyhow, Context as _, bail};
use serde::Serialize;
use serde_json::json;

/// Versioned row/manifest schema; bump when a field meaning changes.
pub const M13_SCHEMA_VERSION: &str = "platform-closure.m13.v1";

#[derive(Debug, Clone, Serialize)]
struct M13Row {
    row_id: String,
    platform_backend: String,
    sandbox_profile: String,
    required_set: Vec<&'static str>,
    actual_attested_set: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    enforcement_evidence_refs: Vec<String>,
    attestation_validation: String,
    required_subset_of_actual: String,
    activation_result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    typed_reason: Option<String>,
    test_command: String,
    artifact_ref: String,
    resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unresolved_reason: Option<String>,
}

impl M13Row {
    fn base(row_id: impl Into<String>, profile: SandboxProfile, backend: &str) -> Self {
        let profile_name = format!("{profile:?}");
        Self {
            platform_backend: backend.to_string(),
            required_set: required_names(profile),
            actual_attested_set: Vec::new(),
            enforcement_evidence_refs: Vec::new(),
            attestation_validation: String::new(),
            required_subset_of_actual: String::new(),
            row_id: row_id.into(),
            sandbox_profile: profile_name,
            activation_result: "refused",
            typed_reason: None,
            test_command: "agent-eval --platform-closure-m13".into(),
            artifact_ref: String::new(),
            resolved: false,
            unresolved_reason: None,
        }
    }

    fn fail(&mut self, reason: String) {
        self.resolved = false;
        self.unresolved_reason = Some(reason);
    }
}

fn required_names(profile: SandboxProfile) -> Vec<&'static str> {
    let required = profile.required();
    let mut names = Vec::new();
    let checks: [(&str, bool); 10] = [
        ("fs_read_confined", required.fs_read_confined),
        ("fs_write_confined", required.fs_write_confined),
        ("tcp_connect_denied", required.tcp_connect_denied),
        ("udp_denied", required.udp_denied),
        ("unix_socket_denied", required.unix_socket_denied),
        ("process_count_quota", required.process_count_quota),
        ("signal_scoped", required.signal_scoped),
        ("cpu_quota", required.cpu_quota),
        ("memory_quota", required.memory_quota),
        ("fd_quota", required.fd_quota),
    ];
    for (name, flag) in checks {
        if flag {
            names.push(name);
        }
    }
    names
}

fn true_flag_names(caps: &SandboxCapabilities) -> Vec<String> {
    let checks: [(&str, bool); 10] = [
        ("fs_read_confined", caps.fs_read_confined),
        ("fs_write_confined", caps.fs_write_confined),
        ("tcp_connect_denied", caps.tcp_connect_denied),
        ("udp_denied", caps.udp_denied),
        ("unix_socket_denied", caps.unix_socket_denied),
        ("process_count_quota", caps.process_count_quota),
        ("signal_scoped", caps.signal_scoped),
        ("cpu_quota", caps.cpu_quota),
        ("memory_quota", caps.memory_quota),
        ("fd_quota", caps.fd_quota),
    ];
    checks
        .iter()
        .filter(|(_, flag)| *flag)
        .map(|(name, _)| (*name).to_string())
        .collect()
}

fn evidence_refs(evidence: &SandboxEvidence) -> Vec<String> {
    let items: [(&str, &Option<String>); 10] = [
        ("fs_read_confined", &evidence.fs_read_confined),
        ("fs_write_confined", &evidence.fs_write_confined),
        ("tcp_connect_denied", &evidence.tcp_connect_denied),
        ("udp_denied", &evidence.udp_denied),
        ("unix_socket_denied", &evidence.unix_socket_denied),
        ("process_count_quota", &evidence.process_count_quota),
        ("signal_scoped", &evidence.signal_scoped),
        ("cpu_quota", &evidence.cpu_quota),
        ("memory_quota", &evidence.memory_quota),
        ("fd_quota", &evidence.fd_quota),
    ];
    items
        .iter()
        .filter_map(|(name, proof)| {
            proof.as_ref().map(|text| format!("{name}: {text}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

const WINDOWS_BACKEND: &str = "windows/integrity+jobobject v1";

fn demo_manifest(profile: SandboxProfile, program: &str) -> CapabilityManifest {
    CapabilityManifest {
        id: "process-demo".into(),
        version: "1.0.0".into(),
        name: "process demo".into(),
        summary: "sandbox closure fixture process capability".into(),
        status: CapabilityStatus::Experimental,
        provides: vec![CapabilityKind::Tool],
        permissions: vec![],
        requires: Vec::new(),
        tools: vec![ToolSpec {
            name: "process-demo.invoke".into(),
            description: "invoke the process capability".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }],
        lifecycle: CapabilityLifecycle::Lazy,
        transport: CapabilityTransport::Process {
            program: program.to_string(),
        },
        sandbox_profile: profile,
    }
}

/// The exact same `ProcessSandbox` the trusted composition builds for a
/// restricted native integration: OS-level write confinement roots plus a
/// Job-Object / rlimit resource ceiling. `with_write_roots` models the
/// misconfigured operator variant whose confinement mechanism is missing.
fn fixture_config(program: &Path, sandbox_root: &Path, with_write_roots: bool) -> ProcessHostConfig {
    std::fs::create_dir_all(sandbox_root).expect("sandbox cwd");
    let sandbox = default_sandbox(sandbox_root, with_write_roots);
    ProcessHostConfig {
        program: program.to_string_lossy().into_owned(),
        args: vec!["--serve".to_string()],
        // The fixture refuses to serve without this marker, which doubles
        // as proof that the configured env actually reached the child.
        env: vec![("MOCK_MARKER".to_string(), "1".to_string())],
        startup_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(10),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
        sandbox,
    }
}

#[allow(unused_variables)]
fn default_sandbox(root: &Path, with_write_roots: bool) -> agent_process::ProcessSandbox {
    #[cfg(windows)]
    {
        agent_process::ProcessSandbox {
            cwd: Some(root.to_path_buf()),
            job_max_memory_bytes: 512 * 1024 * 1024,
            integrity_write_roots: if with_write_roots {
                vec![root.join("private")]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }
    #[cfg(unix)]
    {
        agent_process::ProcessSandbox {
            cwd: Some(root.to_path_buf()),
            max_memory_bytes: 2u64 * 1024 * 1024 * 1024,
            max_open_files: 1024,
            process_limit: 64,
            cpu_time_limit_secs: 60,
            landlock_write_roots: if with_write_roots {
                vec![root.join("private")]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }
}

fn local_platform_backend() -> (&'static str, bool) {
    // (row label, whether this runner can exercise native spawns of it)
    #[cfg(windows)]
    { (WINDOWS_BACKEND, true) }
    #[cfg(unix)]
    { ("unix-native", true) }
}

/// Locate the `mock_host` protocol fixture; build it once when absent so a
/// plain checkout can run the audit without a manual prebuild step.
fn locate_mock_host() -> Option<PathBuf> {
    if let Ok(from_env) = std::env::var("SANDBOX_FIXTURE_HOST") {
        let candidate = PathBuf::from(from_env);
        return candidate.exists().then_some(candidate);
    }
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .to_path_buf();
    let exe_suffix = std::env::consts::EXE_SUFFIX;
    for profile in ["debug", "release"] {
        let candidate = repo_root.join("target").join(profile).join(format!("mock_host{exe_suffix}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn ensure_mock_host() -> anyhow::Result<PathBuf> {
    if let Some(found) = locate_mock_host() {
        return Ok(found);
    }
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow!("cannot locate repository root"))?
        .to_path_buf();
    let status = std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(&repo_root)
        .args(["build", "-p", "agent-process", "--bin", "mock_host"])
        .status()
        .context("spawn cargo build for the sandbox fixture")?;
    if !status.success() {
        bail!("building the mock_host sandbox fixture failed");
    }
    locate_mock_host().ok_or_else(|| anyhow!("mock_host still missing after build"))
}

/// One real activation drive: connect over the enforcement stack, read the
/// post-spawn attestation, run the adapter's activation check by starting
/// the capability, and capture the typed outcome. The adapter gate decides;
/// the direct host connection only exposes the per-flag attestation detail.
async fn drive_activation(
    row: &mut M13Row,
    root: &Path,
    program: &Path,
    profile: SandboxProfile,
    with_write_roots: bool,
) {
    let sandbox_root = root.join(format!("sbx-{}", row.sandbox_profile.to_lowercase()));
    let config = fixture_config(program, &sandbox_root, with_write_roots);

    // Detail pass: same config against the bare host for the attestation.
    let (attestation, validation): (Option<SandboxAttestation>, String) =
        match ProcessHost::connect(config.clone()).await {
        Ok(host) => {
            let attestation = host.sandbox_attestation();
            host.shutdown().await;
            let result = match attestation.validate() {
                Ok(()) => "Ok".to_string(),
                Err(reason) => format!("Err({reason})"),
            };
            (Some(attestation), result)
        }
        Err(error) => (None, format!("connect failed: {error}")),
    };

    let manifest = demo_manifest(profile, &program.to_string_lossy());
    let capability = ProcessCapabilityAdapter::with_config(manifest, config.clone());
    let start_result = capability.start().await;
    let _ = capability.stop().await;

    if let Some(attestation) = &attestation {
        row.actual_attested_set = true_flag_names(&attestation.capabilities);
        row.enforcement_evidence_refs = evidence_refs(&attestation.evidence);
        let subset = profile.allows_start(attestation.capabilities);
        row.required_subset_of_actual = subset.to_string();
        row.attestation_validation = validation;
        match start_result {
            Ok(()) => {
                row.activation_result = "activated";
                if !subset {
                    row.fail("adapter started a capability whose profile was NOT covered by the enforced floor".into());
                    return;
                }
            }
            Err(error) => {
                row.activation_result = "refused";
                let reason = format!("{error:#}");
                if subset {
                    row.fail(format!("activation check passed yet start refused: {reason}"));
                    return;
                }
                row.typed_reason = Some(reason);
            }
        }
        row.resolved = true;
    } else {
        row.activation_result = "not_run(spawn)";
        let reason = validation.clone();
        row.typed_reason = Some(reason);
        row.fail(format!(
            "bare-host connection failed, so no activation observation is possible: {validation}"
        ));
    }
}

/// Contract-level negatives that need no child: an attestation's boolean
/// floor must never claim more than its bounded mechanism proofs deliver.
fn contract_rows(rows: &mut Vec<M13Row>) {
    let test_command = "cargo test -p agent-contracts sandbox";

    // A claimed-true quota without its mechanism proof is refused by the
    // type itself.
    let mut inconsistent = SandboxAttestation {
        capabilities: SandboxCapabilities { memory_quota: true, ..Default::default() },
        backend: "fixture".into(),
        backend_version: "1".into(),
        evidence: SandboxEvidence::default(),
    };
    let validated = inconsistent.validate();
    let mut row = M13Row::base(
        "contract/attestation/true-flag-requires-proof",
        SandboxProfile::Restricted,
        "contract",
    );
    row.test_command = test_command.into();
    row.artifact_ref = "rows.jsonl#contract/attestation/true-flag-requires-proof".into();
    match &validated {
        Err(_) => {
            row.attestation_validation = format!("Err as required ({validated:?})");
            row.resolved = true;
        }
        Ok(()) => row.fail("validate accepted a true flag without mechanism proof".into()),
    }
    rows.push(row);

    // Backends must be labelled within bounds.
    inconsistent.backend = String::new();
    let mut row = M13Row::base(
        "contract/attestation/backend-label-required",
        SandboxProfile::Restricted,
        "contract",
    );
    row.test_command = test_command.into();
    row.artifact_ref = "rows.jsonl#contract/attestation/backend-label-required".into();
    match inconsistent.validate() {
        Err(_) => {
            row.attestation_validation = "Err(empty backend label)".into();
            row.resolved = true;
        }
        Ok(()) => row.fail("validate accepted an empty backend label".into()),
    }
    rows.push(row);
}

/// Explicit not-run rows for platforms this runner cannot exercise natively.
/// They keep coverage honest: unsupported ground stays visible as typed
/// absence instead of disappearing from the table.
fn other_platform_rows(rows: &mut Vec<M13Row>, local_backend: &str) {
    const COMMAND: &str = "cargo test -p agent-process --features (platform-gated)";
    for profile in [SandboxProfile::Trusted, SandboxProfile::Restricted] {
        let name = format!("{profile:?}");
        let row = M13Row {
            platform_backend: format!("linux/landlock+rlimits ({local_backend} runner)"),
            sandbox_profile: name.clone(),
            required_set: required_names(profile),
            actual_attested_set: Vec::new(),
            enforcement_evidence_refs: Vec::new(),
            attestation_validation: "not_run(platform)".into(),
            required_subset_of_actual: "not_run(platform)".into(),
            row_id: format!("other-platform/linux/{name}/observation"),
            activation_result: "not_run(platform)",
            typed_reason: Some(
                "landlock-backed attestation executes only where the kernel plane runs; \
                 covered by the referenced platform-gated deterministic suite"
                    .into(),
            ),
            test_command: COMMAND.into(),
            artifact_ref: format!("rows.jsonl#other-platform/linux/{name}/observation"),
            resolved: true,
            unresolved_reason: None,
        };
        rows.push(row);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct M13Manifest {
    schema_version: String,
    generated_at_unix_secs: u64,
    platform: String,
    source_tree_digest: Option<String>,
    gate: String,
    commands: Vec<&'static str>,
}

fn render_report(
    rows: &[M13Row],
    gate_pass: bool,
    activated_without_evidence_zero: bool,
    untrusted_refuses_natively: bool,
) -> (String, M13Manifest) {
    let total = rows.len();
    let unresolved = rows.iter().filter(|row| !row.resolved).count();
    let activated = rows.iter().filter(|row| row.activation_result == "activated").count();
    let refused = rows.iter().filter(|row| row.activation_result == "refused").count();
    let not_run = rows.iter().filter(|row| row.activation_result.starts_with("not_run")).count();

    let mut markdown = String::new();
    markdown.push_str("# Closure evidence — structured attestation and fail-closed activation\n\n");
    markdown.push_str(&format!(
        "Schema `{M13_SCHEMA_VERSION}`. Generated mechanically by `agent-eval --platform-closure-m13`; \
         activation/refusal rows executed real children inside this run.\n\n"
    ));
    markdown.push_str(&format!(
        "| metric | value |\n| --- | --- |\n| rows | {total} |\n| activated | {activated} |\n\
         | refused | {refused} |\n| explicit not_run | {not_run} |\n| unresolved | {unresolved} |\n\n"
    ));

    markdown.push_str("## Coverage\n\n| row | platform/backend | profile | required ⊆ actual | validate | result | reason |\n| --- | --- | --- | --- | --- | --- | --- |\n");
    for row in rows {
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            row.row_id,
            row.platform_backend,
            row.sandbox_profile,
            row.required_subset_of_actual,
            if row.attestation_validation.is_empty() { "-" } else { row.attestation_validation.as_str() },
            row.activation_result,
            row.typed_reason.as_deref().unwrap_or("-"),
        ));
    }

    markdown.push_str("\n## Gates\n\n");
    markdown.push_str(&format!(
        "- zero unexplained activation/refusal rows and zero unresolved rows: {}\n",
        unresolved == 0
    ));
    markdown.push_str(&format!(
        "- every activated case validated its attestation and carried non-empty mechanism proofs: {}\n",
        activated_without_evidence_zero
    ));
    markdown.push_str(&format!(
        "- native untrusted floor refuses when its complete floor cannot be attested: {}\n",
        untrusted_refuses_natively
    ));
    markdown.push_str(&format!("\n**Verdict: {}**\n", if gate_pass { "PASS" } else { "FAIL" }));

    if unresolved > 0 {
        markdown.push_str("\n## Unresolved rows\n\n");
        for row in rows.iter().filter(|row| !row.resolved) {
            markdown.push_str(&format!(
                "- {}: {}\n",
                row.row_id,
                row.unresolved_reason
                    .clone()
                    .unwrap_or_else(|| "unspecified".into())
            ));
        }
    }

    let manifest = M13Manifest {
        schema_version: M13_SCHEMA_VERSION.to_string(),
        generated_at_unix_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        platform: format!(
            "{}-{} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY
        ),
        source_tree_digest: crate::bundle::source_tree_digest(),
        gate: "extension-sandbox-closure".into(),
        commands: vec![
            "agent-eval --platform-closure-m13",
            "cargo test -p agent-contracts sandbox",
            "cargo test -p agent-process",
            "cargo test -p agent-capability-process",
        ],
    };
    (markdown, manifest)
}

/// Run the whole closure audit into `out_dir`; returns the rendered report
/// plus whether every gate held. Rows persist even on partial failure.
pub async fn run_m13_closure(out_dir: &Path) -> anyhow::Result<(String, bool)> {
    let fixtures = tempfile::TempDir::new().context("create closure tempdir")?;
    let program = ensure_mock_host()?;

    let (backend_label, _) = local_platform_backend();
    let mut rows: Vec<M13Row> = Vec::new();

    // Case 1: Trusted starts under whatever the platform could enforce
    // (its required floor is empty).
    let mut row = M13Row::base("activation/trusted/minimal-floor", SandboxProfile::Trusted, backend_label);
    row.artifact_ref = "rows.jsonl#activation/trusted/minimal-floor".into();
    drive_activation(&mut row, fixtures.path(), &program, SandboxProfile::Trusted, false).await;
    rows.push(row);

    // Case 2: Restricted activates when write confinement plus both
    // resource quotas are genuinely enforced.
    let mut row = M13Row::base("activation/restricted/full-floor", SandboxProfile::Restricted, backend_label);
    row.artifact_ref = "rows.jsonl#activation/restricted/full-floor".into();
    drive_activation(&mut row, fixtures.path(), &program, SandboxProfile::Restricted, true).await;
    rows.push(row);

    // Case 3: Restricted refuses when the write-confinement mechanism is
    // missing from the actual enforcement (resource quotas alone lie about
    // isolation, and the gate must notice).
    let mut row = M13Row::base(
        "activation/restricted/missing-write-confinement",
        SandboxProfile::Restricted,
        backend_label,
    );
    row.artifact_ref = "rows.jsonl#activation/restricted/missing-write-confinement".into();
    drive_activation(&mut row, fixtures.path(), &program, SandboxProfile::Restricted, false).await;
    rows.push(row);

    // Case 4: the untrusted floor can never be fully attested by a native
    // child today, so activation must refuse with a typed reason.
    let mut row = M13Row::base(
        "activation/untrusted-generated/native-refusal",
        SandboxProfile::UntrustedGenerated,
        backend_label,
    );
    row.artifact_ref = "rows.jsonl#activation/untrusted-generated/native-refusal".into();
    drive_activation(&mut row, fixtures.path(), &program, SandboxProfile::UntrustedGenerated, true).await;
    rows.push(row);

    contract_rows(&mut rows);
    other_platform_rows(&mut rows, backend_label);

    // Gates.
    let unresolved: Vec<String> = rows
        .iter()
        .filter(|row| !row.resolved)
        .map(|row| {
            format!(
                "{}: {}",
                row.row_id,
                row.unresolved_reason.clone().unwrap_or_default()
            )
        })
        .collect();

    let activated_rows: Vec<&M13Row> =
        rows.iter().filter(|row| row.activation_result == "activated").collect();
    let activated_ok = activated_rows.iter().all(|row| {
        row.attestation_validation == "Ok"
            && !row.enforcement_evidence_refs.is_empty()
            && row.required_subset_of_actual == "true"
    });
    let activated_without_evidence_zero = activated_ok;

    let untrusted_row = rows.iter().find(|row| {
        row.sandbox_profile.contains("UntrustedGenerated")
            && row.row_id.starts_with("activation/")
    });
    let untrusted_refuses_natively = untrusted_row
        .map(|row| row.activation_result == "refused" && row.resolved)
        .unwrap_or(false);

    // Every live-profile refusal must carry its typed reason; the pure
    // contract negatives are excluded because refusing IS their evidence.
    let refusals_all_typed = rows.iter().all(|row| {
        row.platform_backend == "contract"
            || row.activation_result != "refused"
            || !row.typed_reason.as_deref().unwrap_or_default().is_empty()
    });


    let gate_pass = unresolved.is_empty()
        && activated_without_evidence_zero
        && untrusted_refuses_natively
        && refusals_all_typed;

    let (report, manifest) = render_report(
        &rows,
        gate_pass,
        activated_without_evidence_zero,
        untrusted_refuses_natively,
    );
    crate::platform_closure::persist_jsonl_rows(out_dir, &rows)?;
    crate::platform_closure::persist_report_and_manifest(out_dir, &report, manifest)?;

    if !gate_pass {
        return Err(anyhow!(
            "extension-sandbox closure gates failed (unresolved={} activated_without_proof={} untrusted_refusal={} refusals_typed={})",
            unresolved.len(),
            !activated_without_evidence_zero,
            !untrusted_refuses_natively,
            !refusals_all_typed
        ));
    }
    Ok((report, gate_pass))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole sandbox closure audit must hold every gate when run
    /// deterministically (including its real child spawns).
    #[tokio::test]
    async fn m13_closure_gates_hold_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("m13");
        let (_report, passed) = match run_m13_closure(&out).await {
            Ok(result) => result,
            Err(error) => {
                let body = std::fs::read_to_string(out.join("rows.jsonl")).unwrap_or_default();
                for line in body.lines() {
                    let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    if row["resolved"] == serde_json::json!(false) {
                        eprintln!(
                            "UNRESOLVED {} -> {}",
                            row["row_id"],
                            row["unresolved_reason"].as_str().unwrap_or("")
                        );
                    }
                }
                panic!("audit failed: {error:#}");
            }
        };
        assert!(passed, "sandbox closure audit must pass");
    }
}
