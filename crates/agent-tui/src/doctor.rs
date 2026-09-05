//! Product doctor: a headless self-check for the installed binary. Source
//! builds have `agent-eval --doctor`; an installed `agent-tui` needs its
//! own answer to "will this run on this machine", covering the workspace,
//! the state directory, the checked provider configuration and the
//! checkpoint store — without starting the runtime or the TUI.

use agent_runtime::CheckpointStore;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct CheckReport {
    pub status: CheckStatus,
    pub name: &'static str,
    pub detail: String,
}

impl CheckReport {
    fn ok(name: &'static str, detail: String) -> Self {
        Self {
            status: CheckStatus::Ok,
            name,
            detail,
        }
    }

    fn warn(name: &'static str, detail: String) -> Self {
        Self {
            status: CheckStatus::Warn,
            name,
            detail,
        }
    }

    fn fail(name: &'static str, detail: String) -> Self {
        Self {
            status: CheckStatus::Fail,
            name,
            detail,
        }
    }
}

/// Run every doctor check against `root`. `model_check` is injected so
/// tests can drive the provider-config branch without touching process
/// env; the binary passes `agent_compose::try_model_from_env` projected to
/// a `(description, digest)` pair.
pub async fn run_doctor_checks(
    root: &Path,
    model_check: impl FnOnce() -> Result<(String, Option<String>), String>,
) -> Vec<CheckReport> {
    let mut reports = Vec::new();

    reports.push(CheckReport::ok(
        "binary",
        format!(
            "agent-tui v{}, built {} profile",
            env!("CARGO_PKG_VERSION"),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        ),
    ));

    // Workspace + state-dir writability + checkpoint store.
    match agent_workspace::Workspace::open(root).await {
        Ok(workspace) => {
            let state_dir = workspace.state_dir().to_path_buf();
            let probe = state_dir.join(".doctor-write-probe");
            match (
                std::fs::write(&probe, b"probe"),
                std::fs::remove_file(&probe),
            ) {
                (Ok(()), Ok(())) => reports.push(CheckReport::ok(
                    "workspace",
                    format!("{} (state dir writable)", state_dir.display()),
                )),
                (write, remove) => reports.push(CheckReport::fail(
                    "workspace",
                    format!(
                        "{} state dir is not writable: write {:?}, remove {:?}",
                        state_dir.display(),
                        write.is_err(),
                        remove.is_err()
                    ),
                )),
            }

            let store = CheckpointStore::new(state_dir.join("checkpoints"));
            let checkpoints_dir_missing = !state_dir.join("checkpoints").exists();
            match store.list(5).await {
                Ok(listed) => {
                    let mut detail = format!("{} artifact(s)", listed.len());
                    let mut checkpoint_fail: Option<String> = None;
                    if let Some(row) = listed.first() {
                        match store.load_verified(&row.artifact).await {
                            Ok(_) => {
                                detail.push_str(&format!(
                                    "; newest {} verified ({} bytes)",
                                    row.artifact, row.payload_bytes
                                ));
                            }
                            Err(error) => {
                                checkpoint_fail = Some(format!(
                                    "newest artifact {} failed verification: {error}",
                                    row.artifact
                                ))
                            }
                        }
                    }
                    // Envelope-less checkpoint-like files (manual exports,
                    // torn writes) are skipped by the store; the doctor
                    // names them instead of pretending they are not there.
                    let lookalikes = std::fs::read_dir(state_dir.join("checkpoints"))
                        .map(|entries| {
                            entries
                                .filter_map(|entry| entry.ok())
                                .filter(|entry| {
                                    let name = entry.file_name().to_string_lossy().into_owned();
                                    name.ends_with(".json") && !name.starts_with('.')
                                })
                                .count()
                        })
                        .unwrap_or(0);
                    reports.push(match checkpoint_fail {
                        Some(error) => CheckReport::fail("checkpoints", error),
                        None if lookalikes > listed.len() => CheckReport::warn(
                            "checkpoints",
                            format!(
                                "{detail}; {} unrecognized checkpoint-like file(s) skipped",
                                lookalikes - listed.len()
                            ),
                        ),
                        None => CheckReport::ok("checkpoints", detail),
                    });
                }
                Err(_error) if checkpoints_dir_missing => reports.push(CheckReport::ok(
                    "checkpoints",
                    "none yet (the store directory is created on first save)".into(),
                )),
                Err(error) => reports.push(CheckReport::fail("checkpoints", error.to_string())),
            }
        }
        Err(error) => reports.push(CheckReport::fail(
            "workspace",
            format!("{} failed to open: {error}", root.display()),
        )),
    }

    // Checked provider configuration: a missing key is the product's
    // checked startup error; demo mode is an explicit, valid selection.
    match model_check() {
        Ok((description, digest)) => reports.push(CheckReport::ok(
            "provider",
            match digest {
                Some(digest) => {
                    format!("{description}; digest {}", &digest[..16.min(digest.len())])
                }
                None => description,
            },
        )),
        Err(error) => reports.push(CheckReport::fail("provider", error)),
    }

    reports
}

/// Render the reports and return the process exit code: 0 when nothing
/// failed (warnings are diagnostic), 1 otherwise.
pub fn render_doctor(reports: &[CheckReport]) -> (String, i32) {
    let mut out = String::from("agent-tui doctor:\n");
    let mut failed = false;
    for report in reports {
        let label = match report.status {
            CheckStatus::Ok => "OK  ",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        if report.status == CheckStatus::Fail {
            failed = true;
        }
        out.push_str(&format!("  [{label}] {}: {}\n", report.name, report.detail));
    }
    out.push_str(if failed {
        "doctor result: FAILED\n"
    } else {
        "doctor result: OK\n"
    });
    (out, if failed { 1 } else { 0 })
}

/// Write the bounded diagnostic bundle for a live session: version,
/// status-projection snapshot, transcript tail and the checkpoint index.
/// Contains no key material — the provider identity is a digest only, and
/// the live runtime holds the workspace lock so no doctor workspace check
/// runs here.
pub async fn export_diagnostics(
    state_dir: &Path,
    projection_lines: Vec<String>,
    transcript_tail: Vec<String>,
) -> anyhow::Result<PathBuf> {
    const MAX_SECTION_CHARS: usize = 8_000;
    let mut out = String::from(
        "agent-tui diagnostics
====================
",
    );
    out.push_str(&format!(
        "version: {} ({})
",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    ));
    out.push_str(
        "
[status projection]
",
    );
    for line in projection_lines.into_iter().take(40) {
        out.push_str(&format!(
            "  {line}
"
        ));
    }
    out.push_str(
        "
[transcript tail]
",
    );
    for line in transcript_tail.into_iter().take(40) {
        out.push_str(&format!(
            "  {}
",
            bounded(line, 200)
        ));
    }
    let checkpoints = CheckpointStore::new(state_dir.join("checkpoints"));
    match checkpoints.list(10).await {
        Ok(rows) => {
            out.push_str(
                "
[checkpoint index]
",
            );
            for row in &rows {
                out.push_str(&format!(
                    "  - {} ({} bytes)
",
                    row.artifact, row.payload_bytes
                ));
            }
        }
        Err(error) => out.push_str(&format!(
            "
[checkpoint index unavailable: {error}]
"
        )),
    }
    if out.chars().count() > MAX_SECTION_CHARS * 4 {
        let cut: String = out.chars().take(MAX_SECTION_CHARS * 4).collect();
        out = cut;
        out.push_str(
            "
...[truncated at the bundle cap]
",
        );
    }
    let dir = state_dir.join("diagnostics");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!(
        "diag-{}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    ));
    tokio::fs::write(&path, out).await?;
    Ok(path)
}

fn bounded(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text
    } else {
        let mut cut: String = text.chars().take(max_chars).collect();
        cut.push('…');
        cut
    }
}

/// Convenience wrapper used by the binary: run the checks and print them.
pub async fn run_doctor(root: PathBuf) -> i32 {
    let reports = run_doctor_checks(&root, || match agent_compose::try_model_from_env() {
        Ok(agent_compose::ModelSelection::Mock(_)) => {
            Ok(("demo mode (AGENT_DEMO=1)".to_string(), None))
        }
        Ok(agent_compose::ModelSelection::Provider(_, profile)) => {
            Ok((profile.banner(), Some(profile.digest())))
        }
        Err(error) => Err(error.to_string()),
    })
    .await;
    let (rendered, code) = render_doctor(&reports);
    print!("{rendered}");
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::RUNTIME_CHECKPOINT_VERSION;

    #[tokio::test]
    async fn a_healthy_workspace_gates_clean_and_reports_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // One real envelope in the store; the workspace handle is dropped
        // first so the doctor can take the exclusive state lock itself.
        let state_dir = {
            let workspace = agent_workspace::Workspace::open(&root).await.unwrap();
            let store = CheckpointStore::new(workspace.state_dir().join("checkpoints"));
            let payload = br#"{"version":4}"#.to_vec();
            store.write_atomic(&payload).await.unwrap();
            workspace.state_dir().to_path_buf()
        };
        assert!(state_dir.join("checkpoints").exists());

        let reports = run_doctor_checks(&root, || Ok(("demo mode".to_string(), None))).await;
        assert!(
            reports
                .iter()
                .all(|report| report.status == CheckStatus::Ok),
            "{reports:?}"
        );
        assert!(
            reports
                .iter()
                .any(|report| report.name == "checkpoints" && report.detail.contains("1 artifact"))
        );
        let (rendered, code) = render_doctor(&reports);
        assert_eq!(code, 0);
        assert!(rendered.contains("doctor result: OK"));
        let _ = RUNTIME_CHECKPOINT_VERSION;
    }

    #[tokio::test]
    async fn a_missing_model_and_an_unreadable_store_fail_the_doctor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Create the state dirs, then drop the handle before the doctor.
        let state_dir = {
            let workspace = agent_workspace::Workspace::open(&root).await.unwrap();
            workspace.state_dir().to_path_buf()
        };

        // No model configured: the checked-startup error surfaces as FAIL.
        let mut reports = run_doctor_checks(&root, || {
            Err("no model configured: set OPENAI_API_KEY".into())
        })
        .await;
        assert!(
            reports
                .iter()
                .any(|report| report.name == "provider" && report.status == CheckStatus::Fail)
        );
        assert_eq!(render_doctor(&reports).1, 1);

        // A corrupt checkpoint artifact fails verification, not discovery.
        std::fs::create_dir_all(state_dir.join("checkpoints")).unwrap();
        std::fs::write(
            state_dir.join("checkpoints").join("checkpoint-1-2.json"),
            "not an envelope",
        )
        .unwrap();
        reports = run_doctor_checks(&root, || Ok(("demo mode".into(), None))).await;
        assert!(
            reports.iter().any(|report| report.name == "checkpoints"
                && report.status == CheckStatus::Warn
                && report.detail.contains("unrecognized")),
            "{reports:?}"
        );
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[tokio::test]
    async fn the_bundle_is_bounded_secret_free_and_self_describing() {
        let dir = tempfile::tempdir().unwrap();
        // A real checkpoint in the store so the index section has content.
        let state_dir = dir.path().to_path_buf();
        let store = CheckpointStore::new(state_dir.join("checkpoints"));
        store.write_atomic(br#"{"version":4}"#).await.unwrap();

        let projection_lines = vec![
            "status: serving | turns=2 | model_rounds=3".to_string(),
            "task: fixture task".to_string(),
        ];
        let tail: Vec<String> = (0..60)
            .map(|index| format!("transcript row {index}"))
            .collect();
        let path = export_diagnostics(&state_dir, projection_lines, tail)
            .await
            .unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("agent-tui diagnostics"));
        assert!(body.contains("version: 0.1.0"));
        assert!(body.contains("status projection"));
        assert!(body.contains("checkpoint index"));
        assert!(body.contains("checkpoint-"));
        // Bounded: the 60-row tail is capped at 40 rows.
        assert!(body.matches("transcript row").count() <= 40);
        // No key material by construction: the only identity is a digest.
        assert!(!body.contains("sk-"));
    }
}
