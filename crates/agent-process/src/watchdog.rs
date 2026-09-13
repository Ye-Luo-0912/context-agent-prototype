//! Host-death watchdog (PROCESS-01, Unix arm): OS lifetime containment
//! that survives a SIGKILLed or aborted host, without `pre_exec` hooks
//! (the GH-runner probe proved those silently never execute).
//!
//! Mechanism: the host spawns a small watchdog process — the same
//! executable re-entered through [`WATCHDOG_ENV`] — that holds the read
//! half of a `UnixStream::pair()`. The host holds the write half for the
//! whole child run. If the host dies in any way, the write end closes and
//! the watchdog reads EOF, then kills its own watched process group.
//! Joining that group before exec pins the group identity for the whole
//! run, including after the original leader has exited and been reaped.
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

/// Read `stream` to EOF (the host's death), then kill the group this
/// watchdog joined at spawn. A marker alone cannot select another group.
#[cfg(unix)]
fn watch_stream_to_eof_then_kill<R: std::io::Read>(stream: &mut R, leader: i32) {
    if leader <= 1 || unsafe { libc::getpgrp() } != leader {
        return;
    }
    let mut buf = [0u8; 512];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // A read error other than EOF is treated the same as EOF: the
            // host is unreachable either way and the watched child is
            // better off dead than unsupervised.
            Err(_) => break,
        }
    }
    // Our own membership keeps this group allocated. No /proc scan or
    // recycled numeric PID is consulted at cleanup time. SIGKILL includes
    // the watchdog itself; the parent reaps it on an ordinary shutdown.
    let _ = unsafe { libc::kill(0, libc::SIGKILL) };
}

/// A transient fork/exec resource failure (clone EAGAIN under fork-bomb
/// guards, ENOMEM under memory pressure) must not permanently strip
/// containment from an already-running child: the caller degrades to no
/// containment on an arm error, so a single unlucky clone silently orphans
/// the whole watched tree (observed as CI run 34754942152's exact proof
/// tree outliving a SIGKILLed host with no watchdog ever forked). The arm
/// therefore retries only the transient error class, a bounded number of
/// times, before surfacing the failure.
#[cfg(unix)]
const ARM_SPAWN_ATTEMPTS: u32 = 3;
#[cfg(unix)]
const ARM_SPAWN_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
/// Run `spawn`, retrying only transient OS resource failures. Permanent
/// failures (a missing executable, a refused group) return immediately.
#[cfg(unix)]
fn spawn_watchdog_with_transient_retry(
    mut spawn: impl FnMut() -> std::io::Result<std::process::Child>,
) -> std::io::Result<std::process::Child> {
    let mut attempt = 1;
    loop {
        match spawn() {
            Ok(child) => return Ok(child),
            Err(error) if attempt < ARM_SPAWN_ATTEMPTS && is_transient_spawn_failure(&error) => {
                attempt += 1;
                std::thread::sleep(ARM_SPAWN_RETRY_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn is_transient_spawn_failure(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EAGAIN) | Some(libc::ENOMEM)
    )
}

/// The host-side handle. Held for the whole watched run; dropping it is
/// the disarm path.
#[cfg(unix)]
#[derive(Debug)]
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
        // of the product process's life. The reap is bounded: a watchdog
        // that fails to exit after EOF (wedged, or spawned from an
        // executable that ignores the marker) must not hold teardown
        // forever. The un-reaped Child handle reserves the pid, so a
        // direct signal to it can never hit a reused pid.
        drop(self.write_half.take());
        const WATCHDOG_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        if let Some(mut child) = self.child.take() {
            let deadline = std::time::Instant::now() + WATCHDOG_REAP_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Ok(None) | Err(_) => {
                        // Bounded wait exhausted: stop the watchdog
                        // process and reap it. If even this reap cannot be
                        // observed, the zombie dies with us — never block
                        // shutdown on the watchdog.
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
                        }
                        let kill_deadline = std::time::Instant::now() + WATCHDOG_REAP_TIMEOUT;
                        loop {
                            match child.try_wait() {
                                Ok(Some(_)) | Err(_) => break,
                                Ok(None) if std::time::Instant::now() < kill_deadline => {
                                    std::thread::sleep(std::time::Duration::from_millis(25));
                                }
                                Ok(None) => break,
                            }
                        }
                        break;
                    }
                }
            }
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
    /// executable identity was available, or `leader` was invalid; the
    /// caller degrades to no containment, same as a refused Windows job
    /// assignment.
    pub fn arm(leader: u32) -> std::io::Result<Option<Self>> {
        if leader <= 1 || leader > i32::MAX as u32 {
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
        if leader <= 1 || leader > i32::MAX as u32 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "host-death watchdog requires a positive managed group leader",
            ));
        }
        // This API is called while the host still owns the unreaped child.
        // Refuse its own group, and join only an existing child-led group.
        if unsafe { libc::getpgrp() } == leader as i32
            || unsafe { libc::getpgid(leader as i32) } != leader as i32
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "host-death watchdog requires a separate child process group",
            ));
        }
        let (read_half, write_half) = std::os::unix::net::UnixStream::pair()?;
        // Do not set kill_on_drop: dropping the Child must not SIGKILL the
        // watchdog — Drop closes the write half and waits instead.
        let child = spawn_watchdog_with_transient_retry(|| {
            // A fresh dup per attempt: a failed attempt consumes its own
            // Stdio copy while the original stays owned here, so the pipe
            // still ends with exactly two holders after the winning spawn.
            let stdin_half = read_half.try_clone()?;
            Command::new(exe)
                .env(WATCHDOG_ENV, leader.to_string())
                // The read half becomes the watchdog's stdin: EOF there is
                // the host's death. The write half never leaves this
                // process (both pair ends are close-on-exec).
                .stdin(Stdio::from(std::os::fd::OwnedFd::from(stdin_half)))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // Join before exec, while the child identity is still owned.
                // The watched group is separate from the host's foreground
                // group, so host-only signals do not stop this watchdog.
                .process_group(leader as i32)
                .spawn()
        })?;
        drop(read_half);
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
    use std::os::unix::process::CommandExt;
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

    fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// M17-B1: a watchdog process that never exits on EOF must not hold
    /// teardown — Drop bounds the reap and kills the wedged stand-in.
    #[test]
    fn dropping_the_handle_bounds_the_reap_of_a_wedged_watchdog() {
        // `sleep` never reads stdin, so it stays alive after the write
        // half is dropped; the old Drop blocked in child.wait() for its
        // whole runtime.
        let (read_half, write_half) = UnixStream::pair().unwrap();
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(read_half)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().expect("spawn the wedged watchdog stand-in");
        let pid = child.id();
        let armed = HostDeathWatchdog {
            write_half: Some(write_half),
            child: Some(child),
        };
        let start = Instant::now();
        drop(armed);
        // The bounded reap spends its own 5s deadline before killing; the
        // assertion is that Drop is bounded at all, not that it is
        // instantaneous — the failure shape it prevents is waiting out the
        // stand-in's full 30s runtime.
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "Drop must not wait out the watchdog's full runtime"
        );
        wait_for(
            || unsafe { libc::kill(pid as i32, 0) != 0 },
            "the wedged watchdog to be killed and reaped",
        );
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
        let (mut leader, leader_pid) = spawn_group_leader();
        let armed =
            HostDeathWatchdog::arm_with(std::path::Path::new("/bin/cat"), leader_pid as u32)
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
        crate::kill_process_tree(leader_pid as u32);
        leader.wait().unwrap();
    }

    #[test]
    fn arm_refuses_invalid_or_host_group_ids() {
        for invalid in [1, u32::MAX] {
            assert!(HostDeathWatchdog::arm(invalid).unwrap().is_none());
            assert!(
                HostDeathWatchdog::arm_with(std::path::Path::new("/bin/cat"), invalid).is_err()
            );
        }
        assert!(
            HostDeathWatchdog::arm_with(
                std::path::Path::new("/bin/cat"),
                unsafe { libc::getpgrp() } as u32,
            )
            .is_err()
        );
    }

    /// A transient clone failure (EAGAIN/ENOMEM under runner pressure) must
    /// not permanently strip containment from an already-running child: the
    /// arm retries the transient class until it arms. Without the retry the
    /// first assertion's spawner would surface its EAGAIN as a final arm
    /// error and the caller would silently degrade to no containment.
    #[test]
    fn transient_spawn_failures_are_retried_until_the_watchdog_arms() {
        let mut transient_failures = 2_u32;
        let mut child = spawn_watchdog_with_transient_retry(|| {
            if transient_failures > 0 {
                transient_failures -= 1;
                return Err(std::io::Error::from_raw_os_error(libc::EAGAIN));
            }
            Command::new("/bin/cat").stdin(Stdio::null()).spawn()
        })
        .expect("a transiently failing spawn must eventually arm");
        assert_eq!(
            transient_failures, 0,
            "the spawn was retried, not abandoned"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Permanent failures are not retried: the caller sees the real error
    /// immediately instead of a delayed degrade with the child uncontained.
    #[test]
    fn permanent_spawn_failures_return_without_retry() {
        let start = Instant::now();
        let mut attempts = 0_u32;
        let result = spawn_watchdog_with_transient_retry(|| {
            attempts += 1;
            Err(std::io::Error::from_raw_os_error(libc::ENOENT))
        });
        assert!(result.is_err());
        assert_eq!(attempts, 1, "a permanent failure must not be retried");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a permanent failure must return without the retry backoff"
        );
    }
}
