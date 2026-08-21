//! End-to-end tests for the Linux landlock confinement (M13: OS-level
//! write filtering, TCP deny on ABI v4+, device-ioctl deny on ABI v5,
//! signal scope on ABI v6).
//!
//! The `sandbox_probe` bin is spawned under a confinement whose only write
//! root is one tempdir; it must be able to write there, must be refused at
//! the OS layer when writing to a sibling tempdir, and must still read
//! system files. On ABI v4+ it must also be refused a TCP connect with
//! `PermissionDenied`. On ABI v5+ it must be refused a device ioctl with
//! `PermissionDenied`. On ABI v6 it must be refused `kill(parent, 0)` with
//! `EPERM`. A second test proves the `ProcessHost` handshake still works
//! when the sandbox carries landlock roots, so wiring the field does not
//! break legitimate children.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use agent_process::{ProcessHost, ProcessHostConfig, ProcessSandbox, landlock, probe_siblings};

/// Locate the `sandbox_probe` bin (sibling of this test's binary).
fn locate_probe() -> std::path::PathBuf {
    probe_siblings(&std::env::current_exe().unwrap(), "sandbox_probe").unwrap_or_else(|| {
        panic!("cannot locate the sandbox_probe bin; run `cargo test -p agent-process`")
    })
}

fn spawn_probe(allowed: &std::path::Path, denied: &std::path::Path) -> std::process::Output {
    let rules = landlock::ChildRules::open(&[allowed.to_path_buf()]).unwrap();
    let mut command = std::process::Command::new(locate_probe());
    command.args([allowed.to_str().unwrap(), denied.to_str().unwrap()]);
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || landlock::apply_in_child(&rules));
    }
    command.output().expect("probe runs")
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
fn child_cannot_write_outside_its_landlock_root() {
    if !landlock::available() {
        eprintln!("landlock unavailable on this kernel; skipping");
        return;
    }
    let allowed = tempfile::tempdir().unwrap();
    let denied = tempfile::tempdir().unwrap();
    let output = spawn_probe(allowed.path(), denied.path());
    // The outside write must have been refused by the kernel at the OS
    // layer (EACCES/EROFS) — the probe prints `write-outside:ok` only when
    // the write failed.
    assert_probe_pass(
        &output,
        &[
            "write-inside:ok",
            "write-outside:ok",
            "read-passwd:ok",
            "RESULT:PASS",
        ],
    );
}

#[test]
fn child_cannot_connect_tcp_under_landlock() {
    if !landlock::tcp_deny_available() {
        eprintln!("landlock TCP deny unavailable on this kernel; skipping");
        return;
    }
    let allowed = tempfile::tempdir().unwrap();
    let denied = tempfile::tempdir().unwrap();
    let output = spawn_probe(allowed.path(), denied.path());
    assert_probe_pass(&output, &["tcp-connect:ok", "RESULT:PASS"]);
}

#[test]
fn child_cannot_ioctl_devices_under_landlock() {
    if !landlock::ioctl_dev_deny_available() {
        eprintln!("landlock device-ioctl deny unavailable on this kernel; skipping");
        return;
    }
    let allowed = tempfile::tempdir().unwrap();
    let denied = tempfile::tempdir().unwrap();
    let output = spawn_probe(allowed.path(), denied.path());
    assert_probe_pass(&output, &["ioctl-dev:ok", "RESULT:PASS"]);
}

#[test]
fn child_cannot_signal_outside_its_landlock_domain() {
    if !landlock::signal_scope_available() {
        eprintln!("landlock signal scope unavailable on this kernel; skipping");
        return;
    }
    let allowed = tempfile::tempdir().unwrap();
    let parent = std::process::id();
    let rules = landlock::ChildRules::open(&[allowed.path().to_path_buf()]).unwrap();
    let mut command = std::process::Command::new(locate_probe());
    command.args(["signal", &parent.to_string()]);
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || landlock::apply_in_child(&rules));
    }
    let output = command.output().expect("probe runs");
    assert_probe_pass(&output, &["signal-self:ok", "signal-out:ok", "RESULT:PASS"]);
}

#[tokio::test]
async fn process_host_handshake_works_under_landlock() {
    if !landlock::available() {
        eprintln!("landlock unavailable on this kernel; skipping");
        return;
    }
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
            landlock_write_roots: vec![dir.path().to_path_buf()],
            ..ProcessSandbox::default()
        },
    };
    let host = ProcessHost::connect(config)
        .await
        .expect("mock handshake still works under landlock write confinement");
    let value = host
        .call(serde_json::json!({ "op": "ping" }))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("pong"));
    host.shutdown().await;
}
