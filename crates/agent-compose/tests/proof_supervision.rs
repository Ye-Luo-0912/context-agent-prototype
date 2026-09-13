//! Real Rust host death through RecipeProofRunner::verify_exact. This is
//! a deterministic host fixture, not a provider or GUI acceptance claim.
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_process::{ProcessIdentity, ProcessState, capture_process_identity, inspect_process};

struct Host(Child);
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let deadline = Instant::now() + Duration::from_secs(1);
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn wait_for(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out: {description}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_identity(path: &Path, host: &mut Host, stderr: &Path) -> ProcessIdentity {
    let mut pid = None;
    wait_for(
        || {
            assert!(
                host.0.try_wait().unwrap().is_none(),
                "proof host exited: {}",
                std::fs::read_to_string(stderr).unwrap_or_default()
            );
            pid = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.parse::<u32>().ok());
            pid.is_some()
        },
        "proof child PID",
    );
    capture_process_identity(pid.unwrap()).unwrap()
}

fn owned_process_exited(identity: &ProcessIdentity) -> bool {
    match inspect_process(identity.pid) {
        Ok(ProcessState::Exited) => true,
        Ok(ProcessState::Running(current)) => {
            !current.identity_token.is_empty() && current.identity_token != identity.identity_token
        }
        Err(_) => false,
    }
}

/// The host's stderr is the only place the spawn path reports a degraded
/// containment arm (the proof runner eprintlns into this redirected file
/// and continues without containment). The fixture reads it so a degraded
/// arm fails immediately with its OS error instead of surfacing as a blind
/// tree-exit timeout whose only fact is "workers still Running".
fn host_stderr(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Degrade markers eprintln'd by the tool-runtime spawn path when the
/// requested host-death containment could not be established.
const CONTAINMENT_DEGRADE_MARKERS: [&str; 2] = [
    "host-death watchdog arm failed",
    "host-death job assign skipped",
];

fn containment_degrade(stderr: &str) -> Option<&'static str> {
    CONTAINMENT_DEGRADE_MARKERS
        .into_iter()
        .find(|marker| stderr.contains(marker))
}

/// Linux deterministic barrier: the product's host-death containment for
/// this tree IS the watchdog process, observable as a live group member
/// beside the two workers. The fixture waits for that arming event before
/// killing the host, so a containment arm that lost its post-spawn race or
/// degraded silently fails here — attributed, with the host's stderr —
/// instead of as a 20s tree-exit timeout. CI run 34754942152 had exactly
/// that absent containment: the SIGKILLed host's workers (no watchdog was
/// ever forked beside them) outlived the whole wait, and kernel-side
/// containment that does fire cannot be observed late by a poll loop.
#[cfg(target_os = "linux")]
fn wait_for_armed_containment(leader: &ProcessIdentity, member: &ProcessIdentity, stderr: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let armed = agent_process::process_group_members(leader.pid)
            .unwrap_or_default()
            .into_iter()
            .any(|pid| pid != leader.pid && pid != member.pid);
        if armed {
            return;
        }
        let captured = host_stderr(stderr);
        if let Some(marker) = containment_degrade(&captured) {
            panic!("host-death containment degraded before the kill ({marker}): {captured}");
        }
        assert!(
            Instant::now() < deadline,
            "host-death containment was not armed before the kill (no watchdog beside workers \
             {} and {} in group {}): {captured}",
            leader.pid,
            member.pid,
            leader.pid,
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn killing_the_rust_host_cleans_the_exact_proof_tree_without_a_completion_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let stderr = directory.path().join("host.stderr");
    let mut command = Command::new(env!("CARGO_BIN_EXE_crash_child"));
    command
        .arg(&root)
        .arg("--proof-supervision")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).unwrap());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut host = Host(command.spawn().unwrap());
    let state = root.join(".focus-agent/proof-supervision");
    let leader = read_identity(&state.join("leader.pid"), &mut host, &stderr);
    let member = read_identity(&state.join("member.pid"), &mut host, &stderr);
    wait_for(
        || {
            std::fs::read_to_string(state.join("heartbeat"))
                .ok()
                .is_some_and(|s| s.parse::<u64>().unwrap_or(0) > 0)
        },
        "member heartbeat",
    );
    assert!(!state.join("result").exists());
    #[cfg(target_os = "linux")]
    wait_for_armed_containment(&leader, &member, &stderr);

    // Only this test-owned host is killed. On Unix this is SIGKILL; on
    // Windows it is TerminateProcess. Its Rust destructors cannot run.
    host.0.kill().unwrap();
    wait_for(|| host.0.try_wait().unwrap().is_some(), "host exit");
    // Host-death containment is kernel-side (Unix watchdog EOF group kill /
    // Windows KILL_ON_JOB_CLOSE) and finishes in milliseconds once armed —
    // a poll loop cannot observe it "late", so a tree outliving the host
    // means containment was absent, not slow (run 34748602228's earlier
    // "load jitter" reading was wrong: it was the same silent degrade).
    // The degrade markers fail the wait immediately with the OS error;
    // the 20s cap stays well under the workers' 30s self-expiry so a
    // missed containment still fails rather than passing via the workers
    // timing out on their own.
    let tree_deadline = Instant::now() + Duration::from_secs(20);
    while !(owned_process_exited(&leader) && owned_process_exited(&member)) {
        let captured = host_stderr(&stderr);
        if let Some(marker) = containment_degrade(&captured) {
            panic!(
                "host-death containment silently degraded ({marker}); \
                 the exact proof tree cannot be cleaned by a dead host: {captured}"
            );
        }
        assert!(
            Instant::now() < tree_deadline,
            "timed out: exact proof tree exit (leader pid {} inspect {:?}; member pid {} inspect {:?}; host stderr: {})",
            leader.pid,
            inspect_process(leader.pid),
            member.pid,
            inspect_process(member.pid),
            captured,
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !state.join("result").exists(),
        "host death cannot mint a proof receipt"
    );

    let outcome =
        tool_runtime::supervision::reconcile_children(&root.join(".focus-agent")).unwrap();
    assert!(
        outcome.is_clean(),
        "confirmed dead children can release supervision: {outcome:?}"
    );
    assert!(!state.join("result").exists());
}
