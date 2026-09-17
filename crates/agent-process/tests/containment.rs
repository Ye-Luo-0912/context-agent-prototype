//! Windows containment regressions for the two real production spawn
//! entries of this crate:
//!
//! 1. `ProcessHost::connect` — the generic JSON-lines host spawn.
//! 2. `integrity::run_wrap` — the Low-IL wrap that CreateProcess-es the
//!    real program.
//!
//! The contract under test (C0 pattern, applied to both entries): the
//! child is created suspended, the required Job-Object assignment happens
//! before any target code runs, and only a confirmed assignment plus a
//! confirmed resume produce a running child. Consequently every descendant
//! is born inside the job and dies with it, and a refused assignment
//! recovers the never-running child fail-closed instead of publishing an
//! uncontained child.
//!
//! Identity checks use creation tokens (PID reuse cannot fake a death).

#![cfg(windows)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use agent_process::{
    ProcessHost, ProcessHostConfig, ProcessSandbox, ProcessState, capture_process_identity,
    inspect_process, integrity, probe_siblings,
};
use serde_json::json;

/// The test-only seam `contained_spawn` reads to force the kernel to
/// refuse the required assignment (see that module). The wrap reads it in
/// its own process (passed through the command environment); the host test
/// sets it process-globally under a mutex.
const TEST_FORCE_ASSIGN_FAILURE: &str = "AGENT_PROCESS_TEST_FORCE_JOB_ASSIGN_FAILURE";

/// Serializes every test in this file: the forced-failure seam is read
/// from the environment, and children spawned while it is set
/// process-globally (the host test) would inherit it.
static ENV_SEAM: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn locate_probe() -> std::path::PathBuf {
    probe_siblings(&std::env::current_exe().unwrap(), "sandbox_probe.exe").unwrap_or_else(|| {
        panic!("cannot locate sandbox_probe.exe; run `cargo test -p agent-process`")
    })
}

fn mock_host_program() -> String {
    common::locate_mock_host()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            panic!("cannot locate the mock_host bin; run `cargo test -p agent-process`")
        })
}

/// Poll until `path` exists, then return its two pid lines
/// ("<self>\n<descendant>\n").
fn read_tree_pids(path: &Path) -> (u32, u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Some((first, second)) = content.lines().next().zip(content.lines().nth(1))
            && let (Ok(child), Ok(descendant)) = (first.parse(), second.parse())
        {
            return (child, descendant);
        }
        assert!(
            Instant::now() < deadline,
            "the tree fixture never published its pids at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Assert `pid` is gone, PID-reuse-safe: either the OS reports it exited,
/// or a different creation identity now owns the number.
fn assert_pid_dead(pid: u32, token: &str, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match inspect_process(pid) {
            Ok(ProcessState::Exited) => return,
            Ok(ProcessState::Running(identity)) if identity.identity_token != token => {
                // The number was reused: the original exited.
                return;
            }
            Ok(ProcessState::Running(_)) => {}
            Err(reason) => panic!("observe {label} (pid {pid}): {reason}"),
        }
        assert!(
            Instant::now() < deadline,
            "{label} (pid {pid}) survived the containment kill"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn identity_token(pid: u32, label: &str) -> String {
    capture_process_identity(pid)
        .unwrap_or_else(|reason| panic!("capture identity of {label} (pid {pid}): {reason}"))
        .identity_token
}

/// Entry 1 — ProcessHost::connect: a child that spawns a descendant
/// immediately at startup must have both processes inside the host's job,
/// so dropping the host (the job owner dies / kills) reaps the whole tree.
/// Also the normal-path control: the handshake and one call still work
/// under the suspended-create + confirmed-resume spawn.
#[allow(clippy::await_holding_lock)]
// the env seam guard must span the
// awaited connect: children spawned concurrently would inherit the seam.
#[tokio::test]
async fn host_job_kills_the_child_and_its_immediate_descendant() {
    let _guard = ENV_SEAM.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("descendant.pids");
    let host = ProcessHost::connect(ProcessHostConfig {
        program: mock_host_program(),
        args: vec!["--serve".into()],
        env: vec![
            ("MOCK_MARKER".into(), "1".into()),
            (
                "MOCK_DESCENDANT_PIDFILE".into(),
                pidfile.to_string_lossy().into_owned(),
            ),
        ],
        startup_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
        sandbox: ProcessSandbox {
            // A quota makes the job REQUIRED: the assignment must be
            // confirmed before the mock runs any code.
            process_limit: 8,
            ..ProcessSandbox::default()
        },
    })
    .await
    .expect("connect must work under a required job (normal path)");
    let value = host
        .call(json!({ "op": "ping" }))
        .await
        .expect("a contained child still serves the protocol");
    assert_eq!(value, json!("pong"));

    let (child_pid, descendant_pid) = read_tree_pids(&pidfile);
    let child_token = identity_token(child_pid, "the host child");
    let descendant_token = identity_token(descendant_pid, "the immediate descendant");
    // Host death without a graceful shutdown: the supervisor's kill path
    // terminates the job, and the job's KILL_ON_JOB_CLOSE covers the rest.
    std::mem::drop(host);
    assert_pid_dead(child_pid, &child_token, "the host child");
    assert_pid_dead(
        descendant_pid,
        &descendant_token,
        "the descendant born at child startup",
    );
}

/// Entry 1, fail-closed: a forced assignment refusal must surface a typed
/// error that names the failed step and confirms the recovery kill, and
/// the never-running child must not have executed (no descendant pidfile
/// ever appears).
#[allow(clippy::await_holding_lock)]
// the env seam guard must span the
// awaited connect: children spawned concurrently would inherit the seam.
#[tokio::test]
async fn host_forced_assignment_failure_is_refused_without_a_runnable_child() {
    let _guard = ENV_SEAM.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("descendant.pids");
    // Test-only seam: serialized by the mutex and removed before any
    // assertion can spawn another child.
    unsafe { std::env::set_var(TEST_FORCE_ASSIGN_FAILURE, "1") };
    let result = ProcessHost::connect(ProcessHostConfig {
        program: mock_host_program(),
        args: vec!["--serve".into()],
        env: vec![
            ("MOCK_MARKER".into(), "1".into()),
            (
                "MOCK_DESCENDANT_PIDFILE".into(),
                pidfile.to_string_lossy().into_owned(),
            ),
        ],
        startup_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
        sandbox: ProcessSandbox {
            process_limit: 8,
            ..ProcessSandbox::default()
        },
    })
    .await;
    unsafe { std::env::remove_var(TEST_FORCE_ASSIGN_FAILURE) };
    let message = match result {
        Ok(_) => panic!("a refused required assignment must refuse the connection"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("job assignment"),
        "the typed error must name the failed step: {message}"
    );
    assert!(
        message.contains("death was confirmed"),
        "the typed error must attest the recovery kill from evidence: {message}"
    );
    // The never-running child cannot have spawned anything.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        assert!(
            !pidfile.exists(),
            "the recovered child must not have executed target code"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pidfile.exists());
}

/// Entry 2 — integrity run_wrap: the wrap's job must cover the Low-IL
/// child and the descendant it spawns immediately. Killing ONLY the wrap
/// process (no /T tree walk) closes the wrap's job handle; the kernel's
/// KILL_ON_JOB_CLOSE must reap the whole contained tree.
#[test]
fn wrap_job_kills_the_child_and_its_immediate_descendant() {
    let _guard = ENV_SEAM.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    integrity::label_write_roots(&[root.path().to_path_buf()]).expect("label the wrap write root");
    let mut wrap = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(integrity::WRAP_SENTINEL)
        .arg(locate_probe())
        .arg("tree")
        .arg(root.path())
        // Belt and braces: the seam must never reach a normal-path wrap,
        // whatever the process environment holds when this runs.
        .env_remove(TEST_FORCE_ASSIGN_FAILURE)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the integrity wrap");
    let (child_pid, descendant_pid) = read_tree_pids(&root.path().join("tree.pids"));
    let child_token = identity_token(child_pid, "the wrapped child");
    let descendant_token = identity_token(descendant_pid, "the wrapped child's descendant");
    // Kill only the wrap itself: the job-handle close is the containment
    // mechanism under test. The exit is observed with a bounded try_wait
    // loop, NOT wait_with_output: a descendant that wrongly survives also
    // inherits the wrap's stderr pipe, and waiting for that pipe's EOF
    // would turn the defect into a hang instead of a failed assertion.
    wrap.kill().expect("kill the wrap process");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match wrap.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => panic!("observe the killed wrap: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "the killed wrap did not exit within the bound"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_pid_dead(child_pid, &child_token, "the wrapped child");
    assert_pid_dead(
        descendant_pid,
        &descendant_token,
        "the wrapped child's descendant",
    );
}

/// Entry 2, fail-closed: a forced assignment refusal inside the wrap must
/// exit nonzero with the typed containment message, and the never-running
/// child must not have spawned its descendant.
#[test]
fn wrap_forced_assignment_failure_leaves_no_runnable_child() {
    let _guard = ENV_SEAM.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    integrity::label_write_roots(&[root.path().to_path_buf()]).expect("label the wrap write root");
    let tree_dir = root.path().join("tree");
    std::fs::create_dir_all(&tree_dir).expect("create the tree dir");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(integrity::WRAP_SENTINEL)
        .arg(locate_probe())
        .arg("tree")
        .arg(&tree_dir)
        .env(TEST_FORCE_ASSIGN_FAILURE, "1")
        .output()
        .expect("run the wrap with the forced assignment failure");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert_eq!(
        output.status.code(),
        Some(1),
        "the wrap must fail closed, stdout: {:?}\nstderr: {stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("integrity wrap") && stderr.contains("job assignment"),
        "the wrap must surface the typed containment refusal: {stderr}"
    );
    assert!(
        stderr.contains("death was confirmed"),
        "the wrap's refusal must attest the recovery kill: {stderr}"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        assert!(
            !tree_dir.join("tree.pids").exists(),
            "the recovered child must not have executed target code"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!tree_dir.join("tree.pids").exists());
}
