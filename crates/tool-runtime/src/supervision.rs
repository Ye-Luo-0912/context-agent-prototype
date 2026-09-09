//! Host-child supervision ledger (PROCESS-01, hardened by M17-B1): every
//! host-spawned child process is recorded while it runs, so a crashed or
//! SIGKILLed host cannot leave it unsupervised.
//!
//! Ledger contract:
//!
//! * **Identity, not pid numbers.** A row stores the child's OS creation
//!   identity token (`agent-process::lifecycle`). Reconciliation only kills
//!   a pid whose current identity still matches the recorded one — a
//!   reused pid is never signalled, and rows without a usable identity are
//!   never trusted with a kill.
//! * **Fallible IO.** Recording, releasing and reading return typed
//!   results; a ledger that cannot be read or written is never conflated
//!   with "no pending children". The composition root refuses to reuse the
//!   workspace when the ledger is unreadable or holds unresolved rows.
//! * **Explicit end receipts.** A row is removed when the child was
//!   confirmed reaped (`ChildLease::confirm_reaped`) or when reconciliation
//!   confirms the kill. `Drop` without a receipt is conservative: a
//!   conclusively-dead child's stale row self-heals away, a live child's
//!   row stays for the next startup to decide — Drop never fakes
//!   confirmation.
//! * **Bounded.** Row count and file size are capped; overflow refuses the
//!   record (and therefore the spawn) instead of silently losing
//!   supervision.

use std::fmt;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

#[cfg(test)]
use agent_process::process_is_running;
use agent_process::{
    ProcessCleanupOutcome, ProcessState, capture_process_identity, inspect_process,
    terminate_matching_process_tree,
};
use serde_json::json;

/// Upper bound on ledger rows. A row is one supervised child; rows of dead
/// children self-heal, so only a pathological session could approach this.
const MAX_LEDGER_ROWS: usize = 256;
/// Upper bound on the encoded ledger file.
const MAX_LEDGER_BYTES: u64 = 64 * 1024;

// Serialization is IO coordination, never an authority or a task scheduler.
// The stable companion lock survives atomic replacement of the data file.
static LEDGER_MUTATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct LedgerLock {
    _file: std::fs::File,
    _thread: std::sync::MutexGuard<'static, ()>,
}

/// Total budget a waiter may spend trying to become the ledger lock holder
/// before it gives up. Per-attempt backoff grows and caps independently; the
/// *total* wait is bounded by this budget so a lock that stays held still
/// expires the waiter at the budget instead of scaling with the backoff.
const LOCK_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2_000);
const LOCK_FIRST_BACKOFF_MS: u64 = 25;
const LOCK_MAX_BACKOFF_MS: u64 = 250;

/// Why a [`retry_flock`] wait ended without acquiring the lock.
#[derive(Debug)]
enum LockRetryError {
    /// The total wait budget elapsed while the lock stayed contended.
    TimedOut,
    /// The flock call itself failed with a real IO error.
    Hard(std::io::Error),
}

/// Retry a non-blocking flock until it succeeds, fails hard, or the total
/// wait budget elapses. `backoff_ms` is the per-attempt pause (it grows and
/// caps); the deadline is tracked separately in elapsed time, so a held lock
/// correctly expires the waiter exactly at `budget` rather than scaling the
/// wait with the growing backoff.
fn retry_flock<F>(budget: std::time::Duration, mut try_lock: F) -> Result<(), LockRetryError>
where
    F: FnMut() -> Result<(), std::fs::TryLockError>,
{
    let deadline = std::time::Instant::now() + budget;
    let mut backoff_ms = LOCK_FIRST_BACKOFF_MS;
    loop {
        match try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(LockRetryError::TimedOut);
                }
                // Sleep for at most the remaining budget: a timeout must
                // expire at the total budget even while the lock stays held.
                let remaining = deadline.duration_since(now);
                let sleep_ms = backoff_ms.min(remaining.as_millis() as u64);
                std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                backoff_ms = (backoff_ms * 2).min(LOCK_MAX_BACKOFF_MS);
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(LockRetryError::Hard(error));
            }
        }
    }
}

fn lock_ledger(state_dir: &Path) -> LedgerResult<LedgerLock> {
    let thread = LEDGER_MUTATION.lock().map_err(|_| LedgerError::Io {
        action: "lock",
        source: std::io::Error::other("ledger mutex poisoned"),
    })?;
    let path = ledger_path(state_dir).with_extension("lock");
    std::fs::create_dir_all(path.parent().expect("authority parent")).map_err(|source| {
        LedgerError::Io {
            action: "prepare",
            source,
        }
    })?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| LedgerError::Io {
            action: "lock",
            source,
        })?;
    // `try_lock` is non-blocking: a second process (two compositions started
    // against the same workspace) or a same-process flock quirk surfaces as
    // WouldBlock. Retry with per-attempt backoff instead of failing the
    // operation — the lock is IO coordination, and a *bounded total* wait is
    // what makes the serialization actually usable across processes. The
    // total budget is tracked independently of the backoff so a lock that
    // stays held expires the waiter at the budget rather than hanging.
    retry_flock(LOCK_WAIT_TIMEOUT, || file.try_lock()).map_err(|retry_error| {
        let source = match retry_error {
            LockRetryError::TimedOut => std::io::Error::other(std::fs::TryLockError::WouldBlock),
            LockRetryError::Hard(error) => error,
        };
        LedgerError::Io {
            action: "lock",
            source,
        }
    })?;
    Ok(LedgerLock {
        _file: file,
        _thread: thread,
    })
}

fn sync_ledger_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Ledger path under the workspace state dir (authority layer). Exposed so
/// the composition root can name the file in manual-resolution errors.
pub fn ledger_path(state_dir: &Path) -> PathBuf {
    state_dir.join("authority").join("host-children.jsonl")
}

/// A typed ledger failure. Callers must not treat any of these as "no
/// pending children".
#[derive(Debug)]
pub enum LedgerError {
    Io {
        action: &'static str,
        source: std::io::Error,
    },
    /// A ledger line is not a valid row. Manual review required; the file
    /// is never silently truncated.
    Corrupt { line: usize },
    /// Recording one more row would exceed a bound. The spawn is refused.
    OverLimit { rows: usize },
    /// pid 0 cannot be a supervised child.
    InvalidPid,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { action, source } => {
                write!(f, "supervision ledger {action} failed: {source}")
            }
            Self::Corrupt { line } => {
                write!(f, "supervision ledger line {line} is not a valid row")
            }
            Self::OverLimit { rows } => {
                write!(
                    f,
                    "supervision ledger is full ({rows} rows); refusing to record another unsupervisable child"
                )
            }
            Self::InvalidPid => write!(f, "pid 0 cannot be a supervised child"),
        }
    }
}

impl std::error::Error for LedgerError {}

pub type LedgerResult<T> = Result<T, LedgerError>;

/// One ledger row. `identity` is the child's OS creation token; `None`
/// covers both legacy rows (written before identities existed) and rows
/// whose identity could not be captured — neither is ever trusted with a
/// kill.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LedgerRow {
    pid: u32,
    identity: Option<String>,
    purpose: String,
}

fn parse_row(line: &str, index: usize) -> LedgerResult<LedgerRow> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| LedgerError::Corrupt { line: index })?;
    let pid = value
        .get("pid")
        .and_then(|v| v.as_u64())
        .ok_or(LedgerError::Corrupt { line: index })?;
    let pid = u32::try_from(pid).map_err(|_| LedgerError::Corrupt { line: index })?;
    let identity = value
        .get("identity")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let purpose = value
        .get("purpose")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_owned();
    Ok(LedgerRow {
        pid,
        identity,
        purpose,
    })
}

fn encode_row(row: &LedgerRow) -> String {
    let identity = row
        .identity
        .as_ref()
        .map(|token| json!(token))
        .unwrap_or(serde_json::Value::Null);
    json!({ "pid": row.pid, "identity": identity, "purpose": row.purpose }).to_string()
}

fn read_rows(state_dir: &Path) -> LedgerResult<Option<Vec<LedgerRow>>> {
    let file = match std::fs::File::open(ledger_path(state_dir)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(LedgerError::Io {
                action: "read",
                source: error,
            });
        }
    };
    let mut content = String::new();
    file.take(MAX_LEDGER_BYTES + 1)
        .read_to_string(&mut content)
        .map_err(|source| LedgerError::Io {
            action: "read",
            source,
        })?;
    if content.len() as u64 > MAX_LEDGER_BYTES {
        return Err(LedgerError::OverLimit { rows: 0 });
    }
    let mut rows = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if rows.len() >= MAX_LEDGER_ROWS {
            return Err(LedgerError::OverLimit { rows: rows.len() });
        }
        rows.push(parse_row(line, index + 1)?);
    }
    Ok(Some(rows))
}

/// Rewrite the ledger with exactly `rows` (empty removes the file). The
/// rewrite goes through a synced temporary + rename so a crash mid-release
/// can never corrupt the ledger into an unreadable state.
fn write_rows(state_dir: &Path, rows: &[LedgerRow]) -> LedgerResult<()> {
    let path = ledger_path(state_dir);
    if rows.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                return sync_ledger_directory(path.parent().expect("authority parent")).map_err(
                    |source| LedgerError::Io {
                        action: "sync clear",
                        source,
                    },
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(LedgerError::Io {
                    action: "clear",
                    source: error,
                });
            }
        }
    }
    let payload = rows.iter().map(encode_row).collect::<Vec<_>>().join("\n") + "\n";
    if rows.len() > MAX_LEDGER_ROWS || payload.len() as u64 > MAX_LEDGER_BYTES {
        return Err(LedgerError::OverLimit { rows: rows.len() });
    }
    let temporary = path.with_extension("jsonl.tmp");
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(payload.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        // Rust's rename replaces an existing file on Windows too. Never
        // delete the old ledger on failure: that would lose *other* live rows.
        std::fs::rename(&temporary, &path)?;
        sync_ledger_directory(path.parent().expect("authority parent"))
    };
    write().map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        LedgerError::Io {
            action: "rewrite",
            source: error,
        }
    })
}

/// True when the pid is conclusively not a live child: either gone, or
/// (Linux) a zombie — a zombie answers `kill(pid, 0)` but is already dead;
/// its parent's reap or init's reap settles it, and no signal is needed.
fn conclusively_gone(pid: u32) -> bool {
    matches!(inspect_process(pid), Ok(ProcessState::Exited))
}

#[cfg(all(test, target_os = "linux"))]
fn is_zombie_linux(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')').map(|(_, rest)| {
                rest.split_whitespace()
                    .next()
                    .is_some_and(|state| state == "Z")
            })
        })
        .unwrap_or(false)
}

/// Record a host-spawned child so a crash cannot leave it unsupervised.
/// `identity_token` is the child's OS creation token captured right after
/// spawn; an empty token is stored as "identity unavailable" and never
/// trusted with a kill. The entry stays until the child is confirmed
/// reaped, killed with confirmed exit, or proven stale.
pub fn record_child(
    state_dir: &Path,
    pid: u32,
    identity_token: &str,
    purpose: &str,
) -> LedgerResult<()> {
    let _lock = lock_ledger(state_dir)?;
    if pid == 0 {
        return Ok(());
    }
    // Bounds are checked against the parsed ledger so a corrupt file can
    // never be silently appended to.
    let mut rows = read_rows(state_dir)?.unwrap_or_default();
    let encoded_len = encode_row(&LedgerRow {
        pid,
        identity: (!identity_token.is_empty()).then(|| identity_token.to_owned()),
        purpose: purpose.to_owned(),
    });
    if rows.len() >= MAX_LEDGER_ROWS
        || (MAX_LEDGER_BYTES.saturating_sub(
            rows.iter()
                .map(|row| encode_row(row).len() as u64 + 1)
                .sum::<u64>(),
        ) as usize)
            < encoded_len.len()
    {
        return Err(LedgerError::OverLimit { rows: rows.len() });
    }
    rows.push(LedgerRow {
        pid,
        identity: (!identity_token.is_empty()).then(|| identity_token.to_owned()),
        purpose: purpose.to_owned(),
    });
    write_rows(state_dir, &rows)
}

/// Remove one released row, matched by pid AND identity token (an empty
/// token matches rows recorded without one). A failed release keeps the
/// row — conservative, the next reconcile self-heals a dead child's row.
fn release_row(state_dir: &Path, pid: u32, identity_token: &str) -> LedgerResult<bool> {
    let _lock = lock_ledger(state_dir)?;
    let Some(rows) = read_rows(state_dir)? else {
        return Ok(false);
    };
    let matches_released = |row: &LedgerRow| {
        row.pid == pid
            && match (&row.identity, identity_token.is_empty()) {
                (Some(token), false) => token == identity_token,
                (None, true) => true,
                _ => false,
            }
    };
    let changed = rows.iter().any(matches_released);
    if !changed {
        return Ok(false);
    }
    let kept: Vec<LedgerRow> = rows
        .into_iter()
        .filter(|row| !matches_released(row))
        .collect();
    write_rows(state_dir, &kept)?;
    Ok(true)
}

/// Startup reconciliation outcome. `killed` rows are confirmed dead and
/// cleared; `unverified` (no usable identity) and `unconfirmed` (signalled
/// without confirmed exit) rows are KEPT and must block workspace reuse
/// until resolved manually; `cleared_stale` rows were provably dead or
/// their pid now belongs to a different process.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub killed: Vec<u32>,
    pub unverified: Vec<u32>,
    pub unconfirmed: Vec<u32>,
    pub cleared_stale: Vec<u32>,
}

impl ReconcileOutcome {
    /// True when every row was resolved with a confirmed outcome and the
    /// workspace may be reused.
    pub fn is_clean(&self) -> bool {
        self.unverified.is_empty() && self.unconfirmed.is_empty()
    }
}

#[cfg(test)]
thread_local! {
    /// Per-test-thread switch so parallel tests never poison each other's
    /// kill confirmation.
    static SIMULATE_UNCONFIRMED_KILL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn simulate_unconfirmed_kill() -> bool {
    SIMULATE_UNCONFIRMED_KILL.with(std::cell::Cell::get)
}

/// Kill an identity-verified tree and confirm the exit. The caller has
/// already matched the child's creation identity, so the confirmation is
/// liveness-based: a terminated child counts as dead even where its
/// creation identity would still resolve (a lingering Windows process
/// object keeps its creation time; a Linux zombie keeps its starttime).
fn kill_and_confirm(pid: u32, identity: &str) -> Result<ProcessCleanupOutcome, String> {
    #[cfg(test)]
    if simulate_unconfirmed_kill() {
        agent_process::kill_process_tree(pid);
        return Ok(ProcessCleanupOutcome::Unconfirmed {
            reason: "injected missing exit confirmation".into(),
        });
    }
    terminate_matching_process_tree(pid, identity)
}

/// Startup reconciliation: resolve every recorded child before the
/// workspace is reused. Read failures and corrupt rows are typed errors —
/// never an empty success. Rows that cannot be resolved with a confirmed
/// outcome stay in the ledger and surface through
/// [`ReconcileOutcome::unverified`] / [`ReconcileOutcome::unconfirmed`].
pub fn reconcile_children(state_dir: &Path) -> LedgerResult<ReconcileOutcome> {
    let _lock = lock_ledger(state_dir)?;
    let Some(rows) = read_rows(state_dir)? else {
        return Ok(ReconcileOutcome::default());
    };
    let mut outcome = ReconcileOutcome::default();
    let mut kept = Vec::new();
    for row in rows {
        let current = match inspect_process(row.pid) {
            Ok(ProcessState::Exited) => {
                outcome.cleared_stale.push(row.pid);
                continue;
            }
            Ok(ProcessState::Running(current)) => current,
            Err(_) => {
                outcome.unverified.push(row.pid);
                kept.push(row);
                continue;
            }
        };
        let Some(identity) = row.identity.as_deref().filter(|token| !token.is_empty()) else {
            // Legacy or identity-less row for a live pid: the pid number
            // alone never justifies a kill.
            outcome.unverified.push(row.pid);
            kept.push(row);
            continue;
        };
        if current.identity_token.is_empty() {
            outcome.unverified.push(row.pid);
            kept.push(row);
            continue;
        }
        match current {
            // The pid still belongs to the recorded child: contained kill
            // with confirmed exit.
            current if current.identity_token == identity => {
                match kill_and_confirm(row.pid, identity) {
                    Ok(ProcessCleanupOutcome::ExitConfirmed) => outcome.killed.push(row.pid),
                    Ok(
                        ProcessCleanupOutcome::AlreadyExited
                        | ProcessCleanupOutcome::IdentityMismatch,
                    ) => outcome.cleared_stale.push(row.pid),
                    Ok(ProcessCleanupOutcome::Unconfirmed { .. }) => {
                        // Signals were sent but the exit could not be
                        // confirmed; the row stays so the next startup tries
                        // again instead of reporting a cleanup that did not
                        // happen.
                        outcome.unconfirmed.push(row.pid);
                        kept.push(row);
                    }
                    Err(_) => {
                        outcome.unverified.push(row.pid);
                        kept.push(row);
                    }
                }
            }
            // The pid is a different process now: our child is gone and
            // its number was reused. Stale, never signalled.
            _ => outcome.cleared_stale.push(row.pid),
        }
    }
    write_rows(state_dir, &kept)?;
    Ok(outcome)
}

/// A recorded child whose supervision ends only through an explicit
/// confirmed receipt or the next startup's reconciliation.
///
/// Every normal exit path calls [`ChildLease::confirm_reaped`] after the
/// child was observed exited; a host crash cannot run any drop, so the row
/// survives for the next startup's reconciliation — the division of labor
/// with the pipe-EOF watchdog: the watchdog kills within milliseconds of a
/// crash, the ledger proves what was left behind and gates workspace reuse
/// at the next startup.
#[derive(Debug)]
pub struct ChildLease {
    state_dir: PathBuf,
    pid: u32,
    identity_token: String,
    released: bool,
}

impl ChildLease {
    /// Explicit end-of-supervision receipt: the child was observed exited
    /// (reaped). Removes the ledger row. A failed release keeps the row —
    /// conservative, the next startup's reconcile clears a dead child's
    /// row.
    pub fn confirm_reaped(&mut self) -> LedgerResult<()> {
        if self.released {
            return Ok(());
        }
        release_row(&self.state_dir, self.pid, &self.identity_token)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for ChildLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Conservative cleanup only: a conclusively-dead child's stale row
        // self-heals away; a live child's row STAYS so the next startup's
        // reconcile decides. Drop never removes a live child's supervision.
        if conclusively_gone(self.pid) {
            let _ = release_row(&self.state_dir, self.pid, &self.identity_token);
        }
    }
}

/// Record `pid` with its captured OS identity under `purpose` and return a
/// lease holding the entry. Fails when the ledger cannot guarantee the
/// record — the caller must then refuse to leave the child running.
pub fn lease(state_dir: &Path, pid: u32, purpose: &str) -> LedgerResult<ChildLease> {
    if pid == 0 {
        return Err(LedgerError::InvalidPid);
    }
    let identity_token = capture_process_identity(pid)
        .map(|identity| identity.identity_token)
        .unwrap_or_default();
    record_child(state_dir, pid, &identity_token, purpose)?;
    Ok(ChildLease {
        state_dir: state_dir.to_path_buf(),
        pid,
        identity_token,
        released: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    #[cfg(windows)]
    #[test]
    fn protected_process_lookup_failure_keeps_the_ledger_and_blocks_reuse() {
        let (_dir, state_dir) = state_dir();
        // Always identity-less, even on an elevated host: no branch may
        // authorize signalling this real protected system process.
        record_child(&state_dir, 4, "", "unverifiable").unwrap();
        let lease = ChildLease {
            state_dir: state_dir.clone(),
            pid: 4,
            identity_token: String::new(),
            released: false,
        };
        drop(lease);
        assert!(
            ledger_path(&state_dir).is_file(),
            "Drop cannot equate inaccessible with exited"
        );
        let outcome = reconcile_children(&state_dir).unwrap();
        assert_eq!(outcome.unverified, vec![4]);
        assert!(!outcome.is_clean());
        assert!(outcome.killed.is_empty() && outcome.cleared_stale.is_empty());
        assert_eq!(read_rows_raw(&state_dir).len(), 1);
    }

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
        let child = command.spawn().unwrap();
        let pid = child.id();
        (child, pid)
    }

    fn state_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        (dir, state_dir)
    }

    fn write_raw_ledger(state_dir: &Path, lines: &[String]) {
        let path = ledger_path(state_dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn raw_row(pid: u32, identity: Option<&str>) -> String {
        let identity = identity
            .map(|token| json!(token))
            .unwrap_or(serde_json::Value::Null);
        json!({ "pid": pid, "identity": identity, "purpose": "process.run" }).to_string()
    }

    fn read_rows_raw(state_dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(ledger_path(state_dir))
            .unwrap()
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn concurrent_records_and_releases_keep_every_unreleased_child() {
        let (_dir, state_dir) = state_dir();
        let barrier = std::sync::Barrier::new(32);
        std::thread::scope(|scope| {
            for id in 1..=32 {
                let state_dir = &state_dir;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    record_child(state_dir, id, "creation", "test").unwrap();
                    if id % 2 == 0 {
                        release_row(state_dir, id, "creation").unwrap();
                    }
                });
            }
        });
        let mut ids = read_rows(&state_dir)
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|row| row.pid)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, (1..=32).filter(|id| id % 2 != 0).collect::<Vec<_>>());
    }

    #[test]
    fn oversized_ledgers_fail_before_parsing_or_reconciliation() {
        let (_dir, state_dir) = state_dir();
        let path = ledger_path(&state_dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_LEDGER_BYTES * 1024)
            .unwrap();
        assert!(matches!(
            reconcile_children(&state_dir),
            Err(LedgerError::OverLimit { .. })
        ));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_LEDGER_BYTES * 1024
        );
        write_raw_ledger(
            &state_dir,
            &(0..=MAX_LEDGER_ROWS)
                .map(|_| raw_row(123, Some("creation")))
                .collect::<Vec<_>>(),
        );
        assert!(matches!(
            reconcile_children(&state_dir),
            Err(LedgerError::OverLimit { .. })
        ));
    }

    #[test]
    fn reconcile_kills_a_recorded_leftover_and_clears_the_ledger() {
        let (_dir, state_dir) = state_dir();
        let (_child, pid) = spawn_sleeper();
        let token = capture_process_identity(pid).unwrap().identity_token;

        record_child(&state_dir, pid, &token, "verify.run").unwrap();
        let outcome = reconcile_children(&state_dir).unwrap();
        assert_eq!(outcome.killed, vec![pid], "the leftover must be killed");
        assert!(outcome.is_clean());

        // The ledger is cleared after acknowledgement: a second reconcile
        // finds nothing.
        assert_eq!(
            reconcile_children(&state_dir).unwrap(),
            ReconcileOutcome::default()
        );
    }

    /// F01: a recorded identity that no longer matches the live pid means
    /// the recorded child is gone and the pid was reused — the live
    /// process at that pid is never signalled and the row is stale-cleared.
    /// Exercised against a real live process with a deliberately wrong
    /// creation token.
    #[test]
    fn reconcile_never_signals_a_live_pid_whose_recorded_identity_differs() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();
        let wrong_token = "00000000-0000-0000-0000-000000000000:999999";

        record_child(&state_dir, pid, wrong_token, "verify.run").unwrap();
        let outcome = reconcile_children(&state_dir).unwrap();
        assert!(outcome.killed.is_empty(), "a wrong token must never kill");
        assert_eq!(
            outcome.cleared_stale,
            vec![pid],
            "the row is provably not this live process"
        );
        assert!(outcome.is_clean(), "nothing unresolved remains");
        assert!(!ledger_path(&state_dir).exists(), "the stale row is gone");
        assert!(
            process_is_running(pid),
            "the live process must not have been touched"
        );
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// F01: a legacy row (written before identities existed) for a live pid
    /// is never auto-trusted with a kill.
    #[test]
    fn reconcile_keeps_a_legacy_row_without_identity_for_manual_review() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();

        write_raw_ledger(
            &state_dir,
            &[json!({ "pid": pid, "purpose": "legacy" }).to_string()],
        );
        let outcome = reconcile_children(&state_dir).unwrap();
        assert!(outcome.killed.is_empty());
        assert_eq!(outcome.unverified, vec![pid]);
        assert!(!outcome.is_clean());
        assert!(
            process_is_running(pid),
            "the live process must not have been touched"
        );
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// A legacy row whose pid is dead is stale: cleared without a kill so a
    /// reused pid can never match it later.
    #[test]
    fn reconcile_clears_a_legacy_row_whose_pid_is_dead() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!process_is_running(pid));

        write_raw_ledger(
            &state_dir,
            &[json!({ "pid": pid, "purpose": "legacy" }).to_string()],
        );
        let outcome = reconcile_children(&state_dir).unwrap();
        assert_eq!(outcome.cleared_stale, vec![pid]);
        assert!(outcome.is_clean());
        assert!(!ledger_path(&state_dir).exists());
    }

    /// A corrupted ledger is a typed error, never "no pending children".
    #[test]
    fn reconcile_errors_on_a_corrupt_ledger_without_killing_anything() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();
        let token = capture_process_identity(pid).unwrap().identity_token;
        write_raw_ledger(
            &state_dir,
            &["{\"pid\": not-json".to_owned(), raw_row(pid, Some(&token))],
        );
        let error = reconcile_children(&state_dir).unwrap_err();
        assert!(error.to_string().contains("not a valid row"), "{error}");
        assert!(
            process_is_running(pid),
            "a corrupt ledger must not kill anything"
        );
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// An unreadable ledger is a typed error, never an empty success.
    #[test]
    fn reconcile_errors_when_the_ledger_is_unreadable() {
        let (_dir, state_dir) = state_dir();
        // The ledger path is a DIRECTORY: opening it for reading fails.
        let path = ledger_path(&state_dir);
        std::fs::create_dir_all(&path).unwrap();
        let error = reconcile_children(&state_dir).unwrap_err();
        assert!(error.to_string().contains("read failed"), "{error}");
    }

    /// F02: a kill whose exit cannot be confirmed is reported unresolved
    /// and its row is kept — cleanup success is never faked.
    #[test]
    fn reconcile_reports_unconfirmed_when_the_exit_cannot_be_confirmed() {
        let _guard = SimulateUnconfirmed::enable();
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();
        let token = capture_process_identity(pid).unwrap().identity_token;
        record_child(&state_dir, pid, &token, "verify.run").unwrap();

        let outcome = reconcile_children(&state_dir).unwrap();
        assert!(outcome.killed.is_empty());
        assert_eq!(outcome.unconfirmed, vec![pid]);
        assert!(!outcome.is_clean());
        assert_eq!(read_rows_raw(&state_dir).len(), 1, "the row is kept");
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// Linux: a zombie child is conclusively dead — stale-cleared without a
    /// signal and without the kill-confirmation wait.
    #[cfg(target_os = "linux")]
    #[test]
    fn reconcile_stale_clears_a_zombie_without_signalling() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();
        child.kill().unwrap();
        // Deliberately NOT reaped: a zombie owned by this test.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !is_zombie_linux(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(conclusively_gone(pid), "the zombie must be seen as gone");

        let token = capture_process_identity(pid).unwrap().identity_token;
        record_child(&state_dir, pid, &token, "verify.run").unwrap();
        let start = std::time::Instant::now();
        let outcome = reconcile_children(&state_dir).unwrap();
        assert_eq!(outcome.cleared_stale, vec![pid]);
        assert!(outcome.killed.is_empty() && outcome.unconfirmed.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "a zombie must not go through the kill-confirmation wait"
        );
        child.wait().unwrap();
    }

    /// Releasing a pid whose number is a prefix of another pid's number
    /// must not drop the other row (pid 12 vs pid 123).
    #[test]
    fn release_is_exact_and_does_not_touch_prefix_pid_numbers() {
        let (_dir, state_dir) = state_dir();
        let token = "token-a";
        record_child(&state_dir, 12, token, "process.run").unwrap();
        record_child(&state_dir, 123, "token-b", "verify.run").unwrap();

        release_row(&state_dir, 12, token).unwrap();
        let rows: Vec<u64> = read_rows_raw(&state_dir)
            .iter()
            .filter_map(|value| value.get("pid").and_then(|p| p.as_u64()))
            .collect();
        assert_eq!(rows, vec![123], "only pid 12 may be released");
    }

    /// The lease records on create; only the explicit reaped receipt
    /// removes the row.
    #[test]
    fn lease_is_removed_by_the_confirmed_reaped_receipt() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();

        let mut lease = lease(&state_dir, pid, "process.run").unwrap();
        assert_eq!(read_rows_raw(&state_dir).len(), 1, "recorded on create");
        lease.confirm_reaped().unwrap();
        assert!(
            !ledger_path(&state_dir).exists(),
            "the confirmed receipt removes the row"
        );
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// M17-B1: Drop without a receipt keeps a LIVE child's row (supervision
    /// continues at the next startup) and self-heals a DEAD child's row.
    #[test]
    fn drop_without_a_receipt_is_conservative() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();

        {
            let _lease = lease(&state_dir, pid, "process.run").unwrap();
        }
        assert_eq!(
            read_rows_raw(&state_dir).len(),
            1,
            "a live child's row survives a receipt-less drop"
        );

        // A dead child's stale row (including the one kept above) resolves
        // at the next startup's reconcile — Drop itself only heals a row it
        // just recorded for an already-dead child.
        child.kill().unwrap();
        child.wait().unwrap();
        let outcome = reconcile_children(&state_dir).unwrap();
        assert!(
            outcome.cleared_stale.contains(&pid),
            "the dead child's rows must stale-clear: {outcome:?}"
        );
        assert!(outcome.is_clean());
        assert!(
            !ledger_path(&state_dir).exists(),
            "resolved rows leave no ledger behind"
        );
    }

    /// The crash shape: recorded without a lease, never released. The next
    /// startup's reconcile kills it (identity matches).
    #[test]
    fn a_forgotten_entry_stays_recorded_and_is_reconciled() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();
        let token = capture_process_identity(pid).unwrap().identity_token;

        record_child(&state_dir, pid, &token, "process.run").unwrap();
        let outcome = reconcile_children(&state_dir).unwrap();
        assert_eq!(
            outcome.killed,
            vec![pid],
            "the unleased entry is reconciled"
        );
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// A spawned child recorded at spawn is killed by reconcile even if
    /// its parent never reaped it — the PROCESS-01 host-crash gap.
    #[test]
    fn reconcile_kills_a_child_the_parent_abandoned() {
        let (_dir, state_dir) = state_dir();
        let (mut child, pid) = spawn_sleeper();
        let token = capture_process_identity(pid).unwrap().identity_token;
        record_child(&state_dir, pid, &token, "process.run").unwrap();
        // Deliberately do NOT wait on `child` — the host "crashed".
        let outcome = reconcile_children(&state_dir).unwrap();
        assert!(outcome.killed.contains(&pid));
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

    /// A ledger that cannot be written fails the record (and therefore the
    /// lease) — an unsupervisable child is refused, not silently spawned.
    #[test]
    fn lease_fails_when_the_ledger_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        // `state/authority` as a regular file makes every ledger IO fail.
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("authority"), "not a directory").unwrap();
        let (_child, pid) = spawn_sleeper();

        let error = lease(&state_dir, pid, "process.run").unwrap_err();
        assert!(error.to_string().contains("failed"), "{error}");
        let _ = std::fs::remove_file(state_dir.join("authority"));
        let _child = _child;
    }

    /// R04: a waiter whose total budget elapses while the lock stays held
    /// returns at the budget (the per-attempt backoff is never conflated with
    /// the total wait budget). On the old behavior this looped forever and the
    /// bounded assertion below failed; the new wait expires ~at the budget.
    #[test]
    fn lock_wait_expires_at_the_total_budget_even_while_held() {
        let start = std::time::Instant::now();
        let result = retry_flock(std::time::Duration::from_millis(150), || {
            Err(std::fs::TryLockError::WouldBlock)
        });
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(LockRetryError::TimedOut)),
            "a lock that stays held must time out, got {result:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(120),
            "the waiter must not return before its budget: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "must expire at the budget, not hang: {elapsed:?}"
        );
    }

    /// R04: a contended lock that frees within the budget is acquired; the
    /// backoff and the budget are independent.
    #[test]
    fn lock_wait_succeeds_when_the_lock_frees_within_budget() {
        let mut attempts = 0u32;
        let result = retry_flock(std::time::Duration::from_secs(1), || {
            attempts += 1;
            if attempts <= 3 {
                Err(std::fs::TryLockError::WouldBlock)
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(attempts, 4);
    }

    /// The ledger is bounded: recording past the row cap is a typed error,
    /// never a silent loss of supervision.
    #[test]
    fn record_child_refuses_to_grow_past_the_row_cap() {
        let (_dir, state_dir) = state_dir();
        let rows: Vec<String> = (1..=MAX_LEDGER_ROWS)
            .map(|pid| raw_row(pid as u32, Some("dead-token")))
            .collect();
        write_raw_ledger(&state_dir, &rows);
        let (mut child, pid) = spawn_sleeper();
        let token = capture_process_identity(pid).unwrap().identity_token;

        let error = record_child(&state_dir, pid, &token, "verify.run").unwrap_err();
        assert!(matches!(error, LedgerError::OverLimit { .. }), "{error}");
        let _ = child.kill();
        child.wait().unwrap();
    }

    /// Test-scoped switch for the unconfirmed-kill accounting path.
    struct SimulateUnconfirmed;
    impl SimulateUnconfirmed {
        fn enable() -> Self {
            SIMULATE_UNCONFIRMED_KILL.with(|cell| cell.set(true));
            Self
        }
    }
    impl Drop for SimulateUnconfirmed {
        fn drop(&mut self) {
            SIMULATE_UNCONFIRMED_KILL.with(|cell| cell.set(false));
        }
    }
}
