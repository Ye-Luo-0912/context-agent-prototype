//! Dev-only hermetic gate runner (`--doctor`). It probes the exact
//! executables, owned helper binaries and Provider data plane the local
//! gates depend on, then runs the same bounded format/check/Clippy/build/
//! test list CI runs, and emits one bounded readiness report into a unique
//! non-overwriting directory.
//!
//! Boundaries: the report is a derived check over existing digests and
//! manifests — never a second evidence authority, never a `STATUS.md`
//! writer, and never a launcher for the formal preflight or the predeclared
//! window. Secrets are never persisted: the serving identity is recorded
//! without the API key and probe failures carry error classes, not provider
//! response bodies.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use agent_contracts::ModelTransport;
use anyhow::Context as _;
use provider_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider};

use crate::envfile;

/// One probe or gate step with its bounded outcome.
pub struct DoctorStep {
    pub name: &'static str,
    pub passed: bool,
    /// False when the step could not run (e.g. no provider configured); it
    /// never fails the doctor by itself but is surfaced as a finding.
    pub required: bool,
    pub detail: String,
    pub duration_ms: u128,
}

/// Captured command output is kept bounded: failures carry the tail, never
/// the whole log.
const OUTPUT_TAIL_BYTES: usize = 4 * 1024;

/// Hard wall-clock ceiling per gate command. `cargo test` legitimately takes
/// minutes; the ceiling only exists so a wedged child cannot hang the doctor
/// forever. Killing the direct child is best-effort on all platforms.
fn gate_timeout(step: &str) -> Duration {
    match step {
        "toolchain" => Duration::from_secs(180),
        "check" | "clippy" | "build" | "helpers" => Duration::from_secs(900),
        "tests" => Duration::from_secs(3600),
        _ => Duration::from_secs(300),
    }
}

fn run_captured(mut command: Command, timeout: Duration) -> std::io::Result<(bool, String)> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    // Drain both pipes on worker threads while polling for exit. A gate that
    // writes more than the OS pipe buffer (the workspace test list easily
    // does) would otherwise block on write forever and only the timeout
    // would reap it.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_drain = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buffer);
        }
        buffer
    });
    let stderr_drain = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = stderr {
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buffer);
        }
        buffer
    });
    let started = Instant::now();
    let timed_out;
    loop {
        if let Some(status) = child.try_wait()? {
            let text =
                String::from_utf8_lossy(&stdout_drain.join().unwrap_or_default()).into_owned();
            let err_text =
                String::from_utf8_lossy(&stderr_drain.join().unwrap_or_default()).into_owned();
            let mut combined = text;
            combined.push_str(&err_text);
            return Ok((status.success(), tail(&combined)));
        }
        if started.elapsed() > timeout {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    debug_assert!(timed_out);
    // The drains keep reading until the killed child's pipes close, so the
    // captured tail still shows where the gate stopped.
    let text = String::from_utf8_lossy(&stdout_drain.join().unwrap_or_default()).into_owned();
    let err_text = String::from_utf8_lossy(&stderr_drain.join().unwrap_or_default()).into_owned();
    let mut combined = text;
    combined.push_str(&err_text);
    Ok((
        false,
        format!(
            "timed out after {}s\n{}",
            timeout.as_secs(),
            tail(&combined)
        ),
    ))
}

fn tail(text: &str) -> String {
    if text.len() <= OUTPUT_TAIL_BYTES {
        return text.trim_end().to_string();
    }
    let mut start = text.len() - OUTPUT_TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", text[start..].trim_end())
}

fn version_probe(name: &'static str, args: &[&str]) -> DoctorStep {
    let started = Instant::now();
    let mut command = Command::new(name);
    command.args(args);
    match run_captured(command, gate_timeout("toolchain")) {
        Ok((true, out)) => DoctorStep {
            name,
            passed: true,
            required: true,
            detail: out.lines().next().unwrap_or("").trim().to_string(),
            duration_ms: started.elapsed().as_millis(),
        },
        Ok((false, out)) => DoctorStep {
            name,
            passed: false,
            required: true,
            detail: out,
            duration_ms: started.elapsed().as_millis(),
        },
        Err(error) => DoctorStep {
            name,
            passed: false,
            required: true,
            detail: format!("not runnable: {error}"),
            duration_ms: started.elapsed().as_millis(),
        },
    }
}

/// The Python probe resolves `python` from PATH exactly as the tests do, so
/// the Windows Store stub (exit 9009 or a redirect) is diagnosed as a
/// finding here instead of surfacing later as an unexplained test failure.
fn python_probe() -> DoctorStep {
    let started = Instant::now();
    let name = "python";
    let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
    command.args([
        "-c",
        "import sys; print(sys.executable, sys.version.split()[0])",
    ]);
    match run_captured(command, gate_timeout("toolchain")) {
        Ok((true, out)) => DoctorStep {
            name,
            passed: true,
            required: true,
            detail: out.lines().next().unwrap_or("").trim().to_string(),
            duration_ms: started.elapsed().as_millis(),
        },
        Ok((false, out)) => DoctorStep {
            name,
            passed: false,
            required: true,
            detail: format!(
                "the resolved interpreter is not usable (a Windows Store stub exits 9009): {out}"
            ),
            duration_ms: started.elapsed().as_millis(),
        },
        Err(error) => DoctorStep {
            name,
            passed: false,
            required: true,
            detail: format!("not runnable: {error}"),
            duration_ms: started.elapsed().as_millis(),
        },
    }
}

/// Owned helper binaries: build them explicitly and refresh their mtimes,
/// the same freshness rule CI applies (the context-service binary is the
/// helper downstream tests spawn) so a warm-cache restore cannot leave a
/// stale helper behind.
fn helpers_probe() -> DoctorStep {
    let started = Instant::now();
    let name = "helper-binaries";
    let mut build = Command::new("cargo");
    build.args(["build", "-p", "agent-context-service"]);
    let (passed, detail) = match run_captured(build, gate_timeout("helpers")) {
        Ok((true, _)) => {
            let binary_name = if cfg!(windows) {
                "agent-context-service.exe"
            } else {
                "agent-context-service"
            };
            let path = std::path::Path::new("target")
                .join("debug")
                .join(binary_name);
            match std::fs::OpenOptions::new().append(true).open(&path) {
                Ok(file) => {
                    let times = std::fs::FileTimes::new()
                        .set_modified(SystemTime::now())
                        .set_accessed(SystemTime::now());
                    match file.set_times(times) {
                        Ok(()) => (true, format!("built and fresh: {}", path.display())),
                        Err(error) => (false, format!("helper freshness failed: {error}")),
                    }
                }
                Err(_) => (
                    false,
                    format!(
                        "helper freshness failed: {} missing after build",
                        path.display()
                    ),
                ),
            }
        }
        Ok((false, out)) => (false, out),
        Err(error) => (false, format!("not runnable: {error}")),
    };
    DoctorStep {
        name,
        passed,
        required: true,
        detail,
        duration_ms: started.elapsed().as_millis(),
    }
}

/// Serving identity without secrets, as pinned in eval.env or the process
/// environment.
fn serving_identity() -> String {
    let model = envfile::get("OPENAI_MODEL").unwrap_or_else(|| "<unset>".into());
    let base = envfile::get("OPENAI_BASE_URL").unwrap_or_else(|| "<unset>".into());
    let protocol = envfile::get("OPENAI_API_PROTOCOL").unwrap_or_else(|| "<default>".into());
    format!("{model} @ {base} protocol={protocol}")
}

/// One tiny request through the exact selected model/protocol data plane,
/// with no retry wrapper: one attempt, bounded timeout, error classes only.
async fn provider_probe() -> DoctorStep {
    let started = Instant::now();
    let name = "provider-data-plane";
    let Some(api_key) = envfile::get("OPENAI_API_KEY").filter(|key| !key.trim().is_empty()) else {
        return DoctorStep {
            name,
            passed: true,
            required: false,
            detail: "skipped: no OPENAI_API_KEY configured (the deterministic gates do not need a provider)".into(),
            duration_ms: started.elapsed().as_millis(),
        };
    };
    let base_url =
        envfile::get("OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let model = envfile::get("OPENAI_MODEL").unwrap_or_else(|| "gpt-4o-mini".to_string());
    let protocol = match envfile::get("OPENAI_API_PROTOCOL")
        .as_deref()
        .map(OpenAiProtocol::parse)
    {
        Some(Ok(protocol)) => protocol,
        Some(Err(error)) => {
            return DoctorStep {
                name,
                passed: false,
                required: false,
                detail: format!("the pinned OPENAI_API_PROTOCOL does not parse: {error}"),
                duration_ms: started.elapsed().as_millis(),
            };
        }
        None => OpenAiProtocol::default(),
    };
    let context_window = envfile::context_window().unwrap_or(128_000);
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key,
        base_url,
        model,
        protocol,
        max_output_tokens: 16,
        timeout: Duration::from_secs(60),
        send_stream_options: true,
        send_max_tokens: true,
        max_stream_bytes: provider_openai::DEFAULT_MAX_STREAM_BYTES,
        context_window: Some(context_window),
    });
    let request = agent_contracts::ModelRequest {
        messages: vec![agent_contracts::ModelMessage {
            role: agent_contracts::ModelRole::User,
            content: "Reply with the single word ok".into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        tools: Vec::new(),
        metadata: serde_json::json!({ "purpose": "doctor-probe" }),
        cancel: agent_contracts::CancellationToken::new(),
    };
    match provider.complete(request).await {
        Ok(output) => DoctorStep {
            name,
            passed: true,
            required: false,
            detail: format!(
                "healthy via the pinned protocol: output_tokens={:?} content_bytes={}",
                output.usage.output_tokens,
                output.content.len()
            ),
            duration_ms: started.elapsed().as_millis(),
        },
        Err(error) => DoctorStep {
            name,
            passed: false,
            required: false,
            detail: format!("the selected serving did not answer one tiny request: {error:#}"),
            duration_ms: started.elapsed().as_millis(),
        },
    }
}

fn gate_step(name: &'static str, args: &[&str]) -> DoctorStep {
    let started = Instant::now();
    let mut command = Command::new("cargo");
    command.args(args);
    let (passed, detail) = match run_captured(command, gate_timeout(name)) {
        Ok((true, out)) => (true, out.lines().last().unwrap_or("ok").trim().to_string()),
        Ok((false, out)) => (false, out),
        Err(error) => (false, format!("not runnable: {error}")),
    };
    DoctorStep {
        name,
        passed,
        required: true,
        detail,
        duration_ms: started.elapsed().as_millis(),
    }
}

/// Run every step and write the bounded readiness report into a unique
/// non-overwriting directory. Returns the steps so the CLI can exit
/// non-zero on a required failure.
pub async fn run_doctor(output_root: &std::path::Path) -> anyhow::Result<Vec<DoctorStep>> {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|delta| delta.as_millis())
        .unwrap_or_default();
    let run_dir = output_root.join(format!("{stamp}-doctor"));
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create doctor dir {}", run_dir.display()))?;

    let mut steps = Vec::new();
    steps.push(version_probe("git", &["--version"]));
    steps.push(version_probe("cargo", &["--version"]));
    steps.push(version_probe("rustc", &["--version"]));
    steps.push(python_probe());
    steps.push(helpers_probe());
    steps.push(provider_probe().await);
    steps.push(gate_step("format", &["fmt", "--all", "--", "--check"]));
    steps.push(gate_step(
        "check",
        &["check", "--workspace", "--all-targets", "--all-features"],
    ));
    steps.push(gate_step(
        "clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    ));
    steps.push(gate_step("build", &["build", "--workspace"]));
    steps.push(gate_step(
        "tests",
        &["test", "--workspace", "--all-targets"],
    ));

    let local_ready = steps
        .iter()
        .filter(|step| step.required)
        .all(|step| step.passed);
    let live_ready = steps
        .iter()
        .find(|step| step.name == "provider-data-plane")
        .map(|step| step.passed && step.detail.contains("healthy"))
        .unwrap_or(false);

    let source_digest = crate::bundle::source_tree_digest();
    let pack = crate::suite::load_pack().ok();
    let markdown = render_report(
        &steps,
        local_ready,
        live_ready,
        source_digest.as_deref(),
        pack.as_ref()
            .map(|pack| (pack.manifest.frozen, pack.blockers.len())),
        &serving_identity(),
    );
    std::fs::write(run_dir.join("REPORT.md"), &markdown)?;
    let json = serde_json::json!({
        "schema": "agent-eval-doctor.v1",
        "local_ready": local_ready,
        "live_ready": live_ready,
        "source_tree_digest": source_digest,
        "serving": serving_identity(),
        "steps": steps.iter().map(|step| serde_json::json!({
            "name": step.name,
            "passed": step.passed,
            "required": step.required,
            "duration_ms": step.duration_ms,
            "detail_chars": step.detail.len(),
        })).collect::<Vec<_>>(),
    });
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    eprintln!("doctor report: {}", run_dir.join("REPORT.md").display());
    println!("{markdown}");
    Ok(steps)
}

fn render_report(
    steps: &[DoctorStep],
    local_ready: bool,
    live_ready: bool,
    source_digest: Option<&str>,
    pack: Option<(bool, usize)>,
    serving: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# agent-eval doctor readiness report\n\n");
    out.push_str(&format!(
        "local gates ready: **{local_ready}** · live serving ready: **{live_ready}**\n\n"
    ));
    out.push_str(&format!(
        "- serving identity: `{serving}` (no secrets recorded)\n"
    ));
    match source_digest {
        Some(digest) => out.push_str(&format!(
            "- source tree digest: `{}`\n",
            &digest[..16.min(digest.len())]
        )),
        None => out.push_str("- source tree digest: unavailable (git not runnable?)\n"),
    }
    match pack {
        Some((frozen, blockers)) => out.push_str(&format!(
            "- suite pack: frozen={frozen} blockers={blockers}\n"
        )),
        None => out.push_str("- suite pack: not loadable\n"),
    }
    out.push_str("\n| step | verdict | ms | detail |\n| --- | --- | ---: | --- |\n");
    for step in steps {
        let verdict = if step.passed { "pass" } else { "FAIL" };
        let detail = step.detail.replace('|', "\\|").replace('\n', " ");
        let detail = if detail.chars().count() > 200 {
            format!("{}…", detail.chars().take(200).collect::<String>())
        } else {
            detail
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            step.name, verdict, step.duration_ms, detail
        ));
    }
    out.push_str(
        "\nThis report is a derived check over existing digests; it is not formal evidence,\n\
         does not authorize a preflight or window, and never chains into one.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_tail_keeps_the_last_bytes_only() {
        let long = "x".repeat(OUTPUT_TAIL_BYTES * 2);
        let cut = tail(&long);
        assert!(cut.len() < OUTPUT_TAIL_BYTES + 8, "tail must be bounded");
        assert!(cut.starts_with('…'));

        let short = "short output";
        assert_eq!(tail(short), short);
    }

    #[test]
    fn report_is_bounded_and_names_its_boundaries() {
        let steps = vec![
            DoctorStep {
                name: "format",
                passed: true,
                required: true,
                detail: "clean".into(),
                duration_ms: 12,
            },
            DoctorStep {
                name: "provider-data-plane",
                passed: false,
                required: false,
                detail: "the selected serving did not answer one tiny request: transport error"
                    .into(),
                duration_ms: 60_000,
            },
        ];
        let report = render_report(&steps, false, false, Some("abcd"), Some((true, 0)), "m @ b");
        assert!(report.contains("local gates ready: **false**"));
        assert!(report.contains("live serving ready: **false**"));
        assert!(report.contains("never chains into one"));
        assert!(report.contains("transport error"));
        // A long failure detail must not balloon the bounded report.
        let noisy = vec![DoctorStep {
            name: "tests",
            passed: false,
            required: true,
            detail: "e".repeat(100_000),
            duration_ms: 1,
        }];
        let report = render_report(&noisy, false, false, None, None, "m @ b");
        assert!(
            report.len() < 4_000,
            "the rendered report must stay bounded"
        );
    }
}
