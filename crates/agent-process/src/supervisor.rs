//! Child supervision: pid, job object, tree kill, and reap.
//!
//! This is the PLAT-05 process-control seam. It does not speak JSON-lines.
//! Protocol ping-pong stays on [`crate::ProcessHost`]; framed bytes stay on
//! [`crate::DuplexTransport`] / [`crate::FramedProtocolSession`]. Adapters
//! must not grow a second kill policy: MCP stdio owns a [`ProcessSupervisor`],
//! not a raw `Child`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::process::Child;
use tokio::sync::Mutex;

use crate::host::kill_process_tree;

#[cfg(windows)]
use crate::host::JobObject;

/// Owns one spawned child and its containment.
///
/// Kill is synchronous so timeout/cancel paths can fence the tree without
/// awaiting. Reap is async and must run before those paths return: kill
/// then await wait, so the child is not a zombie and late output cannot be
/// admitted. Drop kills the tree if [`Self::reap`] has not yet cleared the
/// pid (Drop cannot await).
pub struct ProcessSupervisor {
    child: Mutex<Child>,
    pid: AtomicU32,
    #[cfg(windows)]
    job: Mutex<Option<JobObject>>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
}

impl ProcessSupervisor {
    pub fn new(
        child: Child,
        pid: u32,
        #[cfg(windows)] job: Option<JobObject>,
        stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    ) -> Self {
        Self {
            child: Mutex::new(child),
            pid: AtomicU32::new(pid),
            #[cfg(windows)]
            job: Mutex::new(job),
            stderr_tail,
        }
    }

    /// Supervise a child that already had stdio pipes taken and has no Job
    /// Object (MCP discards stderr). [`ProcessHost`] uses [`Self::new`] so
    /// it can attach quotas and a stderr ring.
    pub fn from_child(child: Child, pid: u32) -> Self {
        Self::new(
            child,
            pid,
            #[cfg(windows)]
            None,
            Arc::new(Mutex::new(VecDeque::new())),
        )
    }

    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed)
    }

    /// Kill the child and every descendant. Does not wait.
    pub fn kill_tree(&self) {
        let pid = self.pid();
        #[cfg(windows)]
        {
            let terminated = if let Ok(guard) = self.job.try_lock()
                && let Some(job) = guard.as_ref()
            {
                job.terminate()
            } else {
                false
            };
            if !terminated {
                kill_process_tree(pid);
            }
        }
        #[cfg(not(windows))]
        kill_process_tree(pid);
        #[cfg(unix)]
        {
            if pid != 0
                && unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } != 0
                && let Ok(mut child) = self.child.try_lock()
            {
                let _ = child.start_kill();
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            if let Ok(mut child) = self.child.try_lock() {
                let _ = child.start_kill();
            }
        }
    }

    /// Await the child's exit. Safe after [`Self::kill_tree`] or a graceful
    /// shutdown; a second wait on an already-reaped child returns immediately.
    /// Clears the pid afterwards so Drop cannot `kill_process_tree` a
    /// numeric pid the OS has already reused.
    pub async fn reap(&self) {
        let mut child = self.child.lock().await;
        let _ = child.wait().await;
        self.pid.store(0, Ordering::Relaxed);
    }

    /// Kill the tree then await reap. Error/cancel/timeout paths use this
    /// before returning.
    pub async fn terminate(&self) {
        self.kill_tree();
        self.reap().await;
    }

    pub async fn stderr_tail(&self) -> String {
        let mut ring = self.stderr_tail.lock().await;
        String::from_utf8_lossy(ring.make_contiguous()).into_owned()
    }
}

impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        // Explicit stop/cancel/timeout paths kill and reap first. This
        // backstop covers a dropped host or MCP client: `kill_on_drop` on
        // the spawn command covers the direct child; the tree kill covers
        // descendants. Reap already stored pid 0, so this is a no-op then.
        if self.pid() != 0 {
            self.kill_tree();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    fn sleeper() -> tokio::process::Command {
        #[cfg(windows)]
        {
            let mut command = tokio::process::Command::new("ping");
            command.args(["-n", "20", "127.0.0.1"]);
            command.stdout(Stdio::null());
            command.stderr(Stdio::null());
            command.kill_on_drop(true);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = tokio::process::Command::new("sleep");
            command.arg("20");
            command.stdout(Stdio::null());
            command.stderr(Stdio::null());
            command.kill_on_drop(true);
            command
        }
    }

    #[tokio::test]
    async fn terminate_kills_and_reaps_the_child() {
        let child = sleeper().spawn().unwrap();
        let pid = child.id().unwrap_or(0);
        let supervisor = ProcessSupervisor::from_child(child, pid);
        supervisor.terminate().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            supervisor.pid(),
            0,
            "reap must clear pid so Drop cannot kill a reused pid"
        );
        let mut child = supervisor.child.lock().await;
        assert!(
            child.try_wait().unwrap().is_some(),
            "terminate must reap so the child is not left running"
        );
    }

    #[tokio::test]
    async fn drop_kills_the_child_tree() {
        let child = sleeper().spawn().unwrap();
        let pid = child.id().unwrap_or(0);
        assert_ne!(pid, 0, "sleeper must report a pid");
        drop(ProcessSupervisor::from_child(child, pid));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !crate::lifecycle::process_is_running(pid),
            "dropping the supervisor must kill the child, not orphan it"
        );
    }
}
