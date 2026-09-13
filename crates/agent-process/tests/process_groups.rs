//! Read-only process-group observation (Linux). Containment diagnostics —
//! most notably the host-death watchdog, which has no name — must be
//! matched by group membership, never by a pid number.
#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_sleeper(group: i32) -> std::process::Child {
    Command::new("/bin/sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(group)
        .spawn()
        .unwrap()
}

fn wait_for(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out: {description}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn kill_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_joined_group_member_is_observed_beside_its_leader() {
    let mut leader = spawn_sleeper(0);
    let leader_pid = leader.id();
    let mut joiner = spawn_sleeper(leader_pid as i32);
    wait_for(
        || {
            let members = agent_process::process_group_members(leader_pid).unwrap();
            members.contains(&leader_pid) && members.contains(&joiner.id())
        },
        "the joined member to appear in the leader's group",
    );
    kill_reap(&mut joiner);
    kill_reap(&mut leader);
}

/// A killed-but-unreaped member is a zombie: it still occupies the group
/// but can never hold a pipe or signal anyone, so it is not a live
/// containment member.
#[test]
fn zombies_are_not_reported_as_live_members() {
    let mut leader = spawn_sleeper(0);
    let leader_pid = leader.id();
    let mut joiner = spawn_sleeper(leader_pid as i32);
    wait_for(
        || {
            agent_process::process_group_members(leader_pid)
                .unwrap()
                .contains(&joiner.id())
        },
        "the joiner to appear in the group",
    );
    let _ = joiner.kill();
    // Deliberately NOT reaped yet: the observation must exclude the zombie.
    wait_for(
        || {
            !agent_process::process_group_members(leader_pid)
                .unwrap()
                .contains(&joiner.id())
        },
        "the zombie member to be excluded",
    );
    kill_reap(&mut joiner);
    kill_reap(&mut leader);
}

#[test]
fn a_fresh_group_reports_only_its_leader() {
    let mut leader = spawn_sleeper(0);
    let leader_pid = leader.id();
    assert_eq!(
        agent_process::process_group_members(leader_pid).unwrap(),
        vec![leader_pid],
    );
    kill_reap(&mut leader);
}
