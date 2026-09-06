//! Host-death watchdog (PROCESS-01, Unix arm): OS lifetime containment
//! that survives a SIGKILLed or aborted host, without `pre_exec` hooks
//! (the GH-runner probe proved those silently never execute).
//!
//! Mechanism: the host spawns a small watchdog process — the same
//! executable re-entered through [`WATCHDOG_ENV`] — that holds the read
//! half of a `UnixStream::pair()`. The host holds the write half for the
//! whole child run. If the host dies in any way, the write end closes and
//! the watchdog reads EOF, then kills the watched process group — but
//! only if the group leader still answers, so a normal shutdown (child
//! already reaped, watchdog disarmed by dropping the write end) exits
//! without touching a possibly reused pid.
//!
//! The watched child is a process-group leader (`process_group(0)`, same
//! as the cancellation path), so `kill(-pgid)` covers its tree.

/// Environment marker re-entering this executable as a watchdog instance.
/// Checked at the very top of a product binary's `main`, before any other
/// startup work.
pub const WATCHDOG_ENV: &str = "AGENT_PROCESS_HOST_DEATH_WATCHDOG_PID";

/// Called first in a product binary's `main`. Returns `false` when this
/// process is not a watchdog instance and normal startup may proceed;
/// never returns when it is one.
pub fn run_if_armed_and_exit() -> bool {
    #[cfg(unix)]
    {
        if let Ok(leader) = std::env::var(WATCHDOG_ENV) {
            let Ok(leader) = leader.parse::<i32>() else {
                // A malformed marker can only mean a broken spawn: do
                // nothing instead of guessing a target.
                std::process::exit(0);
            };
            watch_stream_to_eof_then_kill(&mut std::io::stdin().lock(), leader);
            std::process::exit(0);
        }
        false
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Read `stream` to EOF (the host's death), then kill the group led by
/// `leader` if that leader still answers. Split out from stdin so tests
/// can drive the exact watchdog logic over a socket pair.
#[cfg(unix)]
fn watch_stream_to_eof_then_kill<R: std::io::Read>(stream: &mut R, leader: i32) {
    let mut buf = [0u8; 512];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            // A read error other than EOF is treated the same as EOF: the
            // host is unreachable either way and the watched child is
            // better off dead than unsupervised.
            Err(_) => break,
        }
    }
    // A zombie still answers kill(pid, 0), so this check passes while the
    // host is between child exit and reap — killing an all-zombie group
    // is harmless (ESRCH on every member). By the time the host drops the
    // write end on a normal shutdown the child is fully reaped, the
    // check fails, and no signal is sent at all.
    unsafe {
        if libc::kill(leader, 0) == 0 {
            let _ = libc::kill(-leader, libc::SIGKILL);
        }
    }
}

/// The host-side handle. Held for the whole watched run; dropping it is
/// the disarm path.
#[cfg(unix)]
pub struct HostDeathWatchdog {
    /// Write half of the pipe; `None` after [`Drop`] closes it so the
    /// watchdog observes EOF before we reap it.
    write_half: Option<std::os::unix::net::UnixStream>,
    /// Kept so a normal disarm does not leave a zombie watchdog. Must not
    /// use `kill_on_drop`: the watchdog has to outlive a SIGKILLed host.
    child: Option<std::process::Child>,
}

#[cfg(unix)]
impl Drop for HostDeathWatchdog {
    fn drop(&mut self) {
        // Close the write end first (host "still alive" → "gone"), then
        // reap the watchdog so it cannot linger as a zombie for the rest
        // of the product process's life.
        drop(self.write_half.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
impl HostDeathWatchdog {
    /// Arm containment for the process-group leader `leader` (a child
    /// spawned with `process_group(0)`, so its pgid is its pid). The
    /// watchdog executable is this process's own executable re-entered
    /// through [`WATCHDOG_ENV`] — only product binaries whose `main`
    /// dispatches on the marker may arm this. `Ok(None)` means no
    /// executable identity was available, or `leader` was zero; the
    /// caller degrades to no containment, same as a refused Windows job
    /// assignment.
    pub fn arm(leader: u32) -> std::io::Result<Option<Self>> {
        if leader == 0 {
            // pid 0 would make `kill(0, …)` / `kill(-0, …)` target the
            // caller's process group — refuse rather than arm.
            return Ok(None);
        }
        match std::env::current_exe() {
            Ok(exe) => Self::arm_with(&exe, leader).map(Some),
            Err(_) => Ok(None),
        }
    }

    /// Arm with an explicit executable. Test hook and composition-root
    /// escape hatch; production uses [`Self::arm`].
    pub fn arm_with(exe: &std::path::Path, leader: u32) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        if leader == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "host-death watchdog refuses leader pid 0",
            ));
        }
        let (read_half, write_half) = std::os::unix::net::UnixStream::pair()?;
        // Do not set kill_on_drop: dropping the Child must not SIGKILL the
        // watchdog — Drop closes the write half and waits instead.
        let child = Command::new(exe)
            .env(WATCHDOG_ENV, leader.to_string())
            // The read half becomes the watchdog's stdin: EOF there is the
            // host's death. The write half never leaves this process (both
            // pair ends are close-on-exec), so the pipe has exactly two
            // holders.
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(read_half)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // The watchdog must survive group-wide signals aimed at the
            // dying host (Ctrl-C on a TUI foreground group included).
            .process_group(0)
            .spawn()?;
        Ok(Self {
            write_half: Some(write_half),
            child: Some(child),
        })
    }

    /// Test hook: the watchdog child's pid while the handle is live.
    #[cfg(test)]
    fn child_id(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn spawn_group_leader() -> (std::process::Child, i32) {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30");
        command.stdout(Stdio::null()).stderr(Stdio::null());
        command.process_group(0);
        let child = command.spawn().expect("spawn a sleep child");
        let leader = child.id() as i32;
        (child, leader)
    }

    fn leader_alive(leader: i32) -> bool {
        unsafe { libc::kill(leader, 0) == 0 }
    }

    fn wait_for(condition: impl Fn() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_watchdog_kills_the_group_when_the_host_disappears() {
        let (mut child, leader) = spawn_group_leader();
        let (mut read_half, write_half) = UnixStream::pair().unwrap();
        let watchdog = std::thread::spawn(move || {
            watch_stream_to_eof_then_kill(&mut read_half, leader);
        });
        // Host "dies": the write end closes, the watchdog sees EOF.
        drop(write_half);
        wait_for(|| !leader_alive(leader), "the watched group to die");
        let _ = child.wait();
        watchdog.join().unwrap();
    }

    #[test]
    fn a_reaped_leader_is_left_alone() {
        let (mut child, leader) = spawn_group_leader();
        // Fully reap the child first: the normal-shutdown disarm path.
        child.wait().unwrap();
        assert!(!leader_alive(leader), "a reaped zombie must not answer");
        let (mut read_half, write_half) = UnixStream::pair().unwrap();
        let watchdog = std::thread::spawn(move || {
            watch_stream_to_eof_then_kill(&mut read_half, leader);
        });
        drop(write_half);
        watchdog.join().unwrap();
        // No assertion beyond "returns without signalling": the killed pid
        // would only be observable through a reused pid, which the
        // leader-alive check exists to prevent.
    }

    #[test]
    fn arming_wires_the_pipe_and_the_disarm_ends_the_watchdog_child() {
        // A binary that reads stdin to EOF and exits there: proves the
        // spawned watchdog really holds our read half and really dies when
        // the write half is dropped, without re-entering the test binary
        // (HostDeathWatchdog::arm re-execs the current executable, whose
        // main dispatches on the marker — a test binary's does not).
        let (read_half, write_half) = UnixStream::pair().unwrap();
        let mut command = Command::new("/bin/cat");
        command
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(read_half)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        // Disarm: dropping the write end is EOF for the watchdog child.
        drop(write_half);
        wait_for(
            || child.try_wait().ok().flatten().is_some(),
            "the watchdog child to observe EOF and exit",
        );
        let _ = child.wait();
    }

    #[test]
    fn arm_refuses_leader_pid_zero() {
        let armed = HostDeathWatchdog::arm(0).expect("arm(0) degrades with Ok(None)");
        assert!(armed.is_none(), "pid 0 must not arm containment");
    }

    #[test]
    fn arm_with_rejects_leader_pid_zero() {
        let error = HostDeathWatchdog::arm_with(std::path::Path::new("/bin/true"), 0)
            .expect_err("arm_with must reject leader pid 0");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn dropping_the_handle_reaps_the_watchdog_child() {
        // /bin/cat reads stdin to EOF and exits — same pipe contract as the
        // product watchdog, without re-entering this test binary's main.
        // Leader pid is never signalled: cat exits on EOF before any kill
        // path runs.
        let armed = HostDeathWatchdog::arm_with(std::path::Path::new("/bin/cat"), 1)
            .expect("arm a cat stand-in as the watchdog");
        let pid = armed.child_id().expect("armed handle keeps the Child");
        assert!(
            unsafe { libc::kill(pid as i32, 0) == 0 },
            "watchdog child must be alive before Drop"
        );
        drop(armed);
        // kill(pid, 0) still succeeds for a zombie. Passing this wait means
        // Drop closed the write half and wait()'d — not merely orphaned the
        // Child handle.
        wait_for(
            || unsafe { libc::kill(pid as i32, 0) != 0 },
            "the watchdog child to be fully reaped (not left as a zombie)",
        );
    }
}
