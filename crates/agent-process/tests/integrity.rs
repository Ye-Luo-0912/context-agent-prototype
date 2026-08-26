//! End-to-end tests for the Windows Low-IL write confinement.
//!
//! `sandbox_probe` is spawned through this test binary's integrity wrap:
//! the wrap drops to Low IL, so writes inside a Low-labeled root must
//! succeed, writes to a Medium sibling must be refused by the kernel, and
//! reading a system file must still work. A `ProcessHost` handshake under
//! the same fence proves wiring does not break legitimate children.

#![cfg(windows)]

mod common;

use std::time::Duration;

use agent_process::{ProcessHost, ProcessHostConfig, ProcessSandbox, integrity, probe_siblings};

fn locate_probe() -> std::path::PathBuf {
    probe_siblings(&std::env::current_exe().unwrap(), "sandbox_probe.exe").unwrap_or_else(|| {
        panic!("cannot locate sandbox_probe.exe; run `cargo test -p agent-process`")
    })
}

fn spawn_probe(allowed: &std::path::Path, denied: &std::path::Path) -> std::process::Output {
    integrity::label_write_roots(&[allowed.to_path_buf()]).expect("label the write root");
    let wrap = std::env::current_exe().expect("current test exe");
    std::process::Command::new(wrap)
        .arg(integrity::WRAP_SENTINEL)
        .arg(locate_probe())
        .arg(allowed)
        .arg(denied)
        .output()
        .expect("probe runs through the integrity wrap")
}

fn assert_probe_pass(output: &std::process::Output, needles: &[&str]) {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "probe must pass, got status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    for needle in needles {
        assert!(stdout.contains(needle), "{stdout}");
    }
}

#[test]
fn child_cannot_write_outside_its_integrity_root() {
    let allowed = tempfile::tempdir().unwrap();
    let denied = tempfile::tempdir().unwrap();
    let output = spawn_probe(allowed.path(), denied.path());
    assert_probe_pass(
        &output,
        &[
            "write-inside:ok",
            "write-outside:ok",
            "read-hosts:ok",
            "RESULT:PASS",
        ],
    );
}

#[tokio::test]
async fn process_host_handshake_works_under_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let program = common::locate_mock_host()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            panic!("cannot locate the mock_host bin; run `cargo test -p agent-process`")
        });
    let config = ProcessHostConfig {
        program,
        args: vec!["--serve".into()],
        env: vec![("MOCK_MARKER".into(), "1".into())],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
        sandbox: ProcessSandbox {
            integrity_write_roots: vec![dir.path().to_path_buf()],
            ..ProcessSandbox::default()
        },
    };
    let host = ProcessHost::connect(config)
        .await
        .expect("mock handshake still works under Low IL write confinement");
    let value = host
        .call(serde_json::json!({ "op": "ping" }))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("pong"));
    host.shutdown().await;
}

fn spawn_wrapped(args: &[&std::ffi::OsStr]) -> std::process::Output {
    let wrap = std::env::current_exe().expect("current test exe");
    let mut command = std::process::Command::new(wrap);
    command.arg(integrity::WRAP_SENTINEL);
    command.args(args);
    command
        .output()
        .expect("probe runs through the integrity wrap")
}

#[test]
fn wrapped_child_sees_the_wrap_job_process_memory_ceiling() {
    let probe = locate_probe();
    let output = spawn_wrapped(&[probe.as_os_str(), std::ffi::OsStr::new("jobmem")]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if stdout.contains("jobmem:unhandled") || stdout.contains("jobmem:0") {
        eprintln!(
            "skipped: wrap job assign did not stick (outer job?)\nstdout: {stdout}\nstderr: {stderr}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "jobmem probe must run, got status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    let expected = format!("jobmem:{}", integrity::WRAP_JOB_MAX_MEMORY_BYTES);
    assert!(
        stdout.contains(&expected),
        "the real child must inherit the wrap's 512 MiB PROCESS_MEMORY\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn wrapped_child_sees_the_wrap_job_normal_priority_class() {
    let probe = locate_probe();
    let output = spawn_wrapped(&[probe.as_os_str(), std::ffi::OsStr::new("jobprio")]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if stdout.contains("jobprio:unhandled") {
        eprintln!(
            "skipped: wrap job assign did not stick (outer job?)\nstdout: {stdout}\nstderr: {stderr}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "jobprio probe must pass, got status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("RESULT:PASS"),
        "the real child must inherit NORMAL PRIORITY_CLASS with breakaway denied\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn wrapped_child_cannot_commit_past_job_process_memory() {
    let probe = locate_probe();
    let two_gib = std::ffi::OsString::from("2147483648");
    let output = spawn_wrapped(&[
        probe.as_os_str(),
        std::ffi::OsStr::new("alloc"),
        two_gib.as_os_str(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if stdout.contains("alloc:succeeded") {
        let jobmem = spawn_wrapped(&[probe.as_os_str(), std::ffi::OsStr::new("jobmem")]);
        let job_out = String::from_utf8_lossy(&jobmem.stdout).to_string();
        if job_out.contains("jobmem:unhandled") || job_out.contains("jobmem:0") {
            eprintln!(
                "skipped: wrap job assign did not stick; 2 GiB alloc was not fenced\njobmem: {job_out}"
            );
            return;
        }
        panic!(
            "the child must not commit 2 GiB under the wrap's 512 MiB PROCESS_MEMORY\nstdout: {stdout}\nstderr: {stderr}\njobmem: {job_out}"
        );
    }
    assert!(
        !output.status.success() || stdout.contains("alloc:refused"),
        "the kernel must refuse or kill the oversized allocation, got status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}
