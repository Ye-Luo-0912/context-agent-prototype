//! Run the real re-exec watchdog in a separate process: group signalling
//! must never be simulated by a thread in the test runner's own group.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_process::watchdog::{HostDeathWatchdog, WATCHDOG_ENV};

fn probe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_sandbox_probe"))
}

fn wait_for(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out: {description}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_leader(member_file: Option<&Path>) -> Child {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        if member_file.is_some() {
            "sleep 20 >/dev/null 2>&1 & printf '%s' \"$!\" > \"$1\"; IFS= read -r release_line"
        } else {
            "IFS= read -r release_line"
        },
    ]);
    command.arg("watchdog-test");
    if let Some(file) = member_file {
        command.arg(file);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0).spawn().unwrap()
}

fn release_leader(child: &mut Child) {
    child.stdin.take().unwrap().write_all(b"release\n").unwrap();
    wait_for(|| child.try_wait().unwrap().is_some(), "leader exit");
}

#[test]
fn real_watchdog_eof_kills_the_owned_group_and_leaves_other_groups_alive() {
    let mut leader = spawn_leader(None);
    let mut unrelated = spawn_leader(None);
    let watchdog = HostDeathWatchdog::arm_with(probe(), leader.id()).unwrap();
    drop(watchdog);
    wait_for(
        || leader.try_wait().unwrap().is_some(),
        "watched child exit",
    );
    assert!(unrelated.try_wait().unwrap().is_none());
    release_leader(&mut unrelated);
}

#[test]
fn membership_pins_the_group_after_the_last_work_child_is_reaped() {
    let mut leader = spawn_leader(None);
    let group = leader.id() as i32;
    let watchdog = HostDeathWatchdog::arm_with(probe(), leader.id()).unwrap();
    release_leader(&mut leader);
    assert_eq!(
        unsafe { libc::kill(-group, 0) },
        0,
        "watchdog must pin the group"
    );
    drop(watchdog);
    wait_for(
        || unsafe { libc::kill(-group, 0) != 0 },
        "watchdog/group release",
    );
}

#[cfg(target_os = "linux")]
#[test]
fn real_watchdog_kills_members_after_the_original_leader_was_reaped() {
    let directory = tempfile::tempdir().unwrap();
    let member_file = directory.path().join("member.pid");
    let mut leader = spawn_leader(Some(&member_file));
    let watchdog = HostDeathWatchdog::arm_with(probe(), leader.id()).unwrap();
    let mut member = None;
    wait_for(
        || {
            member = std::fs::read_to_string(&member_file)
                .ok()
                .and_then(|s| s.parse::<u32>().ok());
            member.is_some()
        },
        "background member PID",
    );
    let member = member.unwrap();
    release_leader(&mut leader);
    assert!(matches!(
        agent_process::inspect_process(member),
        Ok(agent_process::ProcessState::Running(_))
    ));
    drop(watchdog);
    wait_for(
        || {
            matches!(
                agent_process::inspect_process(member),
                Ok(agent_process::ProcessState::Exited)
            )
        },
        "orphaned member exit",
    );
}

#[test]
fn a_marker_cannot_target_another_group_or_an_invalid_id() {
    let mut leader = spawn_leader(None);
    for marker in [
        "0".to_owned(),
        "-1".into(),
        u32::MAX.to_string(),
        leader.id().to_string(),
    ] {
        let mut impostor = Command::new(probe())
            .env(WATCHDOG_ENV, marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        wait_for(
            || impostor.try_wait().unwrap().is_some(),
            "invalid watchdog exit",
        );
        assert!(
            leader.try_wait().unwrap().is_none(),
            "foreign group must not be signalled"
        );
    }
    release_leader(&mut leader);
}
