//! Host-child supervision ledger (PROCESS-01 Unix remainder): every host-
//! spawned child process is recorded while it runs, so a crashed or
//! SIGKILLed host cannot leave it unsupervised. Reconciliation at startup
//! kills any leftover tree whose entry was never released, and only then
//! clears the ledger — cleanup is acknowledged before the workspace is
//! reused.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use agent_process::{kill_process_tree, process_is_running};
use serde_json::json;

/// Ledger file under the workspace state dir (authority layer).
fn ledger_path(state_dir: &Path) -> PathBuf {
    state_dir.join("authority").join("host-children.jsonl")
}

/// Record a host-spawned child so a crash cannot leave it unsupervised.
/// The entry stays until the child is confirmed reaped or killed.
pub fn record_child(state_dir: &Path, pid: u32, purpose: &str) {
    if pid == 0 {
        return;
    }
    let path = ledger_path(state_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let row = json!({ "pid": pid, "purpose": purpose });
        let _ = writeln!(file, "{row}");
    }
}

/// Remove a released entry (the child was confirmed reaped). Pids are
/// compared as parsed JSON numbers, not substrings — releasing pid 12 must
/// not drop the row of pid 123.
pub fn release_child(state_dir: &Path, pid: u32) {
    let path = ledger_path(state_dir);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut changed = false;
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| {
            let is_target = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|value| value.get("pid").and_then(|v| v.as_u64()))
                == Some(pid as u64);
            if is_target {
                changed = true;
            }
            !is_target
        })
        .collect();
    if !changed {
        return;
    }
    if kept.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let _ = std::fs::write(&path, format!("{}\n", kept.join("\n")));
    }
}

/// Startup reconciliation: kill every still-alive recorded child and clear
/// the ledger, so cleanup is acknowledged before the workspace is reused.
/// Returns the pids that were killed.
pub fn reconcile_children(state_dir: &Path) -> Vec<u32> {
    let path = ledger_path(state_dir);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut killed = Vec::new();
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(pid) = value.get("pid").and_then(|v| v.as_u64()) else {
            continue;
        };
        let pid = pid as u32;
        if process_is_running(pid) {
            kill_process_tree(pid);
            killed.push(pid);
        }
    }
    // Every row was processed: the alive trees were killed and the dead
    // rows are stale by definition. Clearing unconditionally prevents a
    // stale dead pid from later matching an unrelated reused pid.
    let _ = std::fs::remove_file(&path);
    killed
}

/// A recorded child whose ledger entry is released on drop. Every normal
/// exit path (success, tool error, cancellation) drops the lease; a host
/// crash cannot run `Drop`, so the entry survives for the next startup's
/// reconciliation — the division of labor with the pipe-EOF watchdog: the
/// watchdog kills within milliseconds of a crash, the ledger proves what
/// was left behind and gates workspace reuse at the next startup.
pub struct ChildLease {
    state_dir: PathBuf,
    pid: u32,
}

impl Drop for ChildLease {
    fn drop(&mut self) {
        release_child(&self.state_dir, self.pid);
    }
}

/// Record `pid` under `purpose` and return a lease holding the entry.
pub fn lease(state_dir: &Path, pid: u32, purpose: &str) -> ChildLease {
    record_child(state_dir, pid, purpose);
    ChildLease {
        state_dir: state_dir.to_path_buf(),
        pid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    fn spawn_sleeper() -> (std::process::Child, u32) {
        // Spawned as a process-group leader on Unix, matching the
        // production spawn contract that kill_process_tree's group kill
        // depends on.
        #[cfg(unix)]
        let mut command = {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new("sleep");
            command.arg("30").process_group(0);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("ping");
            command.args(["-n", "30", "127.0.0.1"]);
            command
        };
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        (child, pid)
    }

    #[test]
    fn reconcile_kills_a_recorded_leftover_and_clears_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let (_child, pid) = spawn_sleeper();

        record_child(&state_dir, pid, "verify.run");
        let killed = reconcile_children(&state_dir);
        assert_eq!(killed, vec![pid], "the leftover must be killed");

        // The ledger is cleared after acknowledgement: a second reconcile
        // finds nothing.
        assert!(reconcile_children(&state_dir).is_empty());
    }

    #[test]
    fn release_removes_only_the_released_entry() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        record_child(&state_dir, 11, "process.run");
        record_child(&state_dir, 12, "verify.run");

        release_child(&state_dir, 11);
        let content = std::fs::read_to_string(ledger_path(&state_dir)).unwrap();
        assert!(
            content.contains("12") && !content.contains("11"),
            "{content}"
        );
    }

    /// Releasing a pid whose number is a prefix of another pid's number
    /// must not drop the other row (pid 12 vs pid 123).
    #[test]
    fn release_is_exact_and_does_not_touch_prefix_pid_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        record_child(&state_dir, 12, "process.run");
        record_child(&state_dir, 123, "verify.run");

        release_child(&state_dir, 12);
        let content = std::fs::read_to_string(ledger_path(&state_dir)).unwrap();
        let rows: Vec<u64> = content
            .lines()
            .filter_map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| value.get("pid").and_then(|p| p.as_u64()))
            })
            .collect();
        assert_eq!(rows, vec![123], "only pid 12 may be released: {content}");
    }

    /// Reconcile clears stale rows of already-dead children even when it
    /// kills nothing — a stale pid must never later match an unrelated
    /// reused pid.
    #[test]
    fn reconcile_clears_stale_dead_entries_without_killing() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        // Spawn, then fully reap: a real pid that is definitely not running.
        let (mut child, pid) = spawn_sleeper();
        child.kill().ok();
        child.wait().unwrap();
        assert!(!process_is_running(pid));

        record_child(&state_dir, pid, "verify.run");
        let killed = reconcile_children(&state_dir);
        assert!(
            killed.is_empty(),
            "a dead child must not be reported killed"
        );
        assert!(
            !ledger_path(&state_dir).exists(),
            "stale entries must be cleared so a reused pid is never targeted"
        );
    }

    /// The lease records on create and releases on drop; an entry recorded
    /// without a lease (the crash shape) survives for reconciliation.
    #[test]
    fn lease_releases_on_drop_and_a_forgotten_entry_stays_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let (mut child, pid) = spawn_sleeper();

        {
            let _lease = lease(&state_dir, pid, "process.run");
            assert!(ledger_path(&state_dir).exists(), "recorded on create");
        }
        assert!(
            !ledger_path(&state_dir).exists(),
            "the normal path releases the entry on drop"
        );

        // Crash shape: recorded, never released.
        record_child(&state_dir, pid, "process.run");
        let killed = reconcile_children(&state_dir);
        assert_eq!(killed, vec![pid], "the unleased entry is reconciled");
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// A spawned child recorded at spawn is killed by reconcile even if
    /// its parent never reaped it — the PROCESS-01 host-crash gap.
    #[test]
    fn reconcile_kills_a_child_the_parent_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let (mut child, pid) = spawn_sleeper();
        record_child(&state_dir, pid, "process.run");
        // Deliberately do NOT wait on `child` — the host "crashed".
        let killed = reconcile_children(&state_dir);
        assert!(killed.contains(&pid));
        // The killed child is now a zombie owned by this test, and a
        // zombie answers kill(pid, 0) — so the kill is observed by
        // reaping it, not by a liveness probe.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while child.try_wait().ok().flatten().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the abandoned child must die after reconcile"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
