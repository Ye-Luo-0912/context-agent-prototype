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

    // Only this test-owned host is killed. On Unix this is SIGKILL; on
    // Windows it is TerminateProcess. Its Rust destructors cannot run.
    host.0.kill().unwrap();
    wait_for(|| host.0.try_wait().unwrap().is_some(), "host exit");
    wait_for(
        || owned_process_exited(&leader) && owned_process_exited(&member),
        "exact proof tree exit",
    );
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
