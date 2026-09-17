//! Windows contained spawn: the single entry that establishes required
//! Job-Object containment BEFORE the child runs any target code.
//!
//! Both real production spawn paths in this crate — the generic
//! [`crate::ProcessHost`] spawn and the Low-IL integrity wrap
//! (`integrity::run_wrap`) — create the child with `CREATE_SUSPENDED`,
//! perform the required `AssignProcessToJobObject` while the child has not
//! executed a single instruction, and only then resume the initial thread.
//! A descendant born before the assignment never joins the job and survives
//! the job's host-death kill; a suspended child cannot have descendants, so
//! every descendant of a resumed child is born inside the job.
//!
//! Fail-closed contract: when the required assignment is refused, or the
//! resume cannot be confirmed, the never-running child is killed through
//! its own kernel handle and its death is confirmed within a bounded wait
//! — a typed error is returned and no runnable child is handed to the
//! caller. The child never ran, so it provably has no descendants to walk.
//!
//! Boundary honesty: an OUTER job (a CI runner, an outer supervisor) may
//! already confine this process and its descendants, so a refused inner
//! assignment does not prove the tree would escape. The typed rejection
//! states only what was established: the configured inner containment was
//! NOT confirmed before the child ran, therefore the child was recovered
//! and the spawn refused. Approval, `EffectIntent` and permission
//! semantics stay in Core; this module owns creation, assignment, resume
//! and recovery only.
//!
//! The suspended-create pattern and `resume_suspended_process` follow the
//! C0 fix in `tool-runtime`'s process tool (same kernel calls, same
//! fail-closed shape). tool-runtime keeps its own copy this slice; an
//! integrator may later converge the two on this entry.

/// Test-only seam name: when this variable is set in the spawning process,
/// the required assignment is performed against a null job handle so the
/// kernel deterministically refuses it. Production never sets it; it exists
/// so the fail-closed recovery can be exercised through the real
/// production entries (whose own job handles are valid).
const TEST_FORCE_ASSIGN_FAILURE: &str = "AGENT_PROCESS_TEST_FORCE_JOB_ASSIGN_FAILURE";

/// How long the fail-closed recovery waits for the never-running child's
/// exit after signalling it. A terminated suspended process exits
/// immediately; the bound only trips if the kernel wedges, in which case
/// the error says the death was not confirmed instead of guessing.
const RECOVERY_BOUND: std::time::Duration = std::time::Duration::from_secs(5);
const RECOVERY_POLL: std::time::Duration = std::time::Duration::from_millis(10);

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

/// A freshly spawned, never-running (`CREATE_SUSPENDED`) child, seen
/// through the two command flavors this crate spawns: tokio for the
/// JSON-lines host, std for the Low-IL wrap. The entry assigns the job and
/// recovers failures through the child's own kernel handles.
pub(crate) trait SuspendedChild {
    fn pid(&self) -> u32;
    /// The `CreateProcess` process handle. Used for the assignment (and the
    /// recovery kill) instead of a fresh `OpenProcess`-by-pid: the handle is
    /// already owned, carries the needed access, and cannot name a reused
    /// pid.
    fn process_handle(&self) -> HANDLE;
    /// Terminate the never-running child through the owned handle.
    fn kill(&mut self);
    /// Non-blocking exit probe (an error is "unknown", not "exited").
    fn try_exited(&mut self) -> std::io::Result<bool>;
}

impl SuspendedChild for std::process::Child {
    fn pid(&self) -> u32 {
        std::process::Child::id(self)
    }

    fn process_handle(&self) -> HANDLE {
        use std::os::windows::io::AsRawHandle;
        self.as_raw_handle()
    }

    fn kill(&mut self) {
        let _ = std::process::Child::kill(self);
    }

    fn try_exited(&mut self) -> std::io::Result<bool> {
        Ok(self.try_wait()?.is_some())
    }
}

impl SuspendedChild for tokio::process::Child {
    fn pid(&self) -> u32 {
        tokio::process::Child::id(self).unwrap_or(0)
    }

    fn process_handle(&self) -> HANDLE {
        // The child was just spawned by this entry, so the handle is always
        // present; a missing handle cannot be assigned (fail-closed below).
        self.raw_handle().unwrap_or(std::ptr::null_mut())
    }

    fn kill(&mut self) {
        let _ = self.start_kill();
    }

    fn try_exited(&mut self) -> std::io::Result<bool> {
        Ok(self.try_wait()?.is_some())
    }
}

/// A command that can spawn a never-running child.
pub(crate) trait ContainedCommand {
    type Child: SuspendedChild;

    /// Set `CREATE_SUSPENDED` and spawn. The flag replaces the default
    /// creation flags exactly like the C0 fix in tool-runtime; std itself
    /// still ORs in `CREATE_UNICODE_ENVIRONMENT` for the environment block.
    fn spawn_suspended(&mut self) -> std::io::Result<Self::Child>;
}

impl ContainedCommand for std::process::Command {
    type Child = std::process::Child;

    fn spawn_suspended(&mut self) -> std::io::Result<Self::Child> {
        use std::os::windows::process::CommandExt;
        self.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        self.spawn()
    }
}

impl ContainedCommand for tokio::process::Command {
    type Child = tokio::process::Child;

    fn spawn_suspended(&mut self) -> std::io::Result<Self::Child> {
        self.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        self.spawn()
    }
}

/// Why a contained spawn was refused. The never-running child was already
/// recovered when this is returned.
#[derive(Debug)]
pub(crate) struct ContainmentFailure {
    step: &'static str,
    pid: u32,
    detail: String,
    death_confirmed: bool,
}

impl ContainmentFailure {
    /// Whether the recovery observed the child's exit. `false` means the
    /// kill could not be confirmed — callers must surface that as a
    /// recovery concern, not as a clean refusal.
    pub(crate) fn death_confirmed(&self) -> bool {
        self.death_confirmed
    }
}

impl std::fmt::Display for ContainmentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "required containment could not be established before the child ran: \
             {} for pid {} was refused ({}) while the child was still suspended; \
             the never-running child was killed and its death {}",
            self.step,
            self.pid,
            self.detail,
            if self.death_confirmed {
                "was confirmed"
            } else {
                "WAS NOT confirmed"
            }
        )
    }
}

/// The typed error of the contained-spawn entry.
#[derive(Debug)]
pub(crate) enum ContainedSpawnError {
    /// The suspended spawn itself failed (missing binary, and so on).
    Spawn(std::io::Error),
    /// The containment sequence failed; the child was recovered fail-closed.
    Containment(ContainmentFailure),
}

impl From<std::io::Error> for ContainedSpawnError {
    fn from(error: std::io::Error) -> Self {
        Self::Spawn(error)
    }
}

impl std::fmt::Display for ContainedSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "spawn: {error}"),
            Self::Containment(failure) => write!(f, "{failure}"),
        }
    }
}

/// Assign one process (by its owned kernel handle) to a job. `false` means
/// the kernel refused — nested-job confinement, a reached ceiling, or an
/// invalid handle. Shared by the entry and the Job-Object's pid-based
/// wrapper so both speak to the same kernel call.
pub(crate) fn assign_process_handle(job: HANDLE, process: HANDLE) -> bool {
    if job.is_null() || process.is_null() {
        return false;
    }
    unsafe { AssignProcessToJobObject(job, process) != 0 }
}

/// Spawn `command` suspended, perform the required job assignment while
/// the child has not run, then resume the initial thread. `Ok(child)` is
/// the only outcome in which a child is running: the assignment (when a
/// job was required) and the resume were both confirmed first.
///
/// `required_job` is a borrowed, non-owning handle (`None` = no job was
/// required for this spawn; the child is still created suspended and
/// resumed through the same confirmed path).
pub(crate) fn spawn_contained<C: ContainedCommand>(
    command: &mut C,
    required_job: Option<HANDLE>,
) -> Result<C::Child, ContainedSpawnError> {
    let mut child = command.spawn_suspended()?;
    let pid = child.pid();
    if let Some(job) = required_job {
        // Test-only seam: swap the valid handle for a null one so the
        // kernel refuses the assignment deterministically. See the const
        // docs; production never sets the variable.
        let job = if std::env::var_os(TEST_FORCE_ASSIGN_FAILURE).is_some() {
            std::ptr::null_mut()
        } else {
            job
        };
        if !assign_process_handle(job, child.process_handle()) {
            let detail = std::io::Error::last_os_error().to_string();
            return Err(ContainedSpawnError::Containment(recover(
                &mut child,
                "the required job assignment",
                detail,
            )));
        }
    }
    if !resume_suspended_process(pid) {
        return Err(ContainedSpawnError::Containment(recover(
            &mut child,
            "the confirmed resume of the suspended child",
            "no thread of the suspended child could be resumed".into(),
        )));
    }
    Ok(child)
}

/// Kill the never-running child and confirm its death within
/// [`RECOVERY_BOUND`]. The child was created suspended and has not
/// executed, so it has no descendants: killing (and confirming) the child
/// itself is the whole recovery.
fn recover(
    child: &mut impl SuspendedChild,
    step: &'static str,
    detail: String,
) -> ContainmentFailure {
    let pid = child.pid();
    child.kill();
    let deadline = std::time::Instant::now() + RECOVERY_BOUND;
    let mut confirmed = false;
    while std::time::Instant::now() < deadline {
        match child.try_exited() {
            Ok(true) => {
                confirmed = true;
                break;
            }
            Ok(false) => {}
            // An observation error is unknown, not exited: keep polling
            // until the bound, then report the death as unconfirmed.
            Err(_) => {}
        }
        std::thread::sleep(RECOVERY_POLL);
    }
    ContainmentFailure {
        step,
        pid,
        detail,
        death_confirmed: confirmed,
    }
}

/// Resume the initial thread of a process created with `CREATE_SUSPENDED`.
/// The suspended child has not executed any code, so its only thread is the
/// kernel's initial one; a fresh snapshot can briefly race the thread-table
/// walk, hence the bounded retries. `false` means no resume was confirmed:
/// the caller must kill the child and refuse the run instead of leaving it
/// suspended. Ported from the C0 fix in tool-runtime's process tool (the
/// ToolHelp walk is feature-gated in windows-sys 0.59; these three kernel32
/// exports have been stable since XP, so they are declared locally instead
/// of adding a dependency feature).
pub(crate) fn resume_suspended_process(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        thread_id: u32,
        owner_process_id: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }

    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> HANDLE;
        fn Thread32First(snapshot: HANDLE, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snapshot: HANDLE, entry: *mut ThreadEntry32) -> i32;
    }

    for _ in 0..3 {
        let resumed = unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
                false
            } else {
                let mut entry = ThreadEntry32 {
                    dw_size: std::mem::size_of::<ThreadEntry32>() as u32,
                    cnt_usage: 0,
                    thread_id: 0,
                    owner_process_id: 0,
                    base_priority: 0,
                    delta_priority: 0,
                    flags: 0,
                };
                let mut resumed = false;
                if Thread32First(snapshot, &mut entry) != 0 {
                    loop {
                        if entry.owner_process_id == pid {
                            let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id);
                            if !thread.is_null() {
                                // A fresh suspended process has exactly one
                                // thread; a successful resume returns the
                                // previous suspend count (1), never
                                // (u32::MAX, the failure marker).
                                if ResumeThread(thread) != u32::MAX {
                                    resumed = true;
                                }
                                let _ = CloseHandle(thread);
                            }
                        }
                        if Thread32Next(snapshot, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                let _ = CloseHandle(snapshot);
                resumed
            }
        };
        if resumed {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet_child() -> std::process::Command {
        let mut command = std::process::Command::new("cmd");
        command.args(["/D", "/S", "/C", "exit /b 0"]);
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    /// Windows containment contract (C0 port): a child created suspended
    /// cannot run — and cannot spawn descendants — until
    /// `resume_suspended_process` confirms the resume. This is what pins
    /// the job assignment ahead of any descendant birth.
    #[test]
    fn a_suspended_child_runs_only_after_the_confirmed_resume() {
        let mut command = quiet_child();
        let mut child = command.spawn_suspended().expect("spawn suspended");
        let pid = child.pid();
        // A suspended process cannot exit: its code has not run.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !child.try_exited().unwrap(),
            "a suspended child must not have executed"
        );
        assert!(
            resume_suspended_process(pid),
            "the initial thread of a fresh suspended process must resume"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the resumed child must exit promptly"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Fail-closed contract: a refused required assignment must recover the
    /// never-running child (kill + confirmed death) and return a typed
    /// rejection naming the step — never hand a running child to the
    /// caller, never leave a suspended orphan. A null job handle makes the
    /// kernel refuse deterministically; the production seam (the test-only
    /// environment variable) routes real entries into this same arm.
    #[test]
    fn assignment_failure_recovers_the_never_running_child() {
        let mut command = quiet_child();
        let error = spawn_contained(&mut command, Some(std::ptr::null_mut()))
            .expect_err("a null job handle must be refused");
        let ContainedSpawnError::Containment(failure) = &error else {
            panic!("the refusal must be the typed containment error, got {error}");
        };
        assert!(
            failure.death_confirmed(),
            "the never-running child must be killed and its exit confirmed"
        );
        assert!(
            error.to_string().contains("pid "),
            "the typed error must name the recovered child: {error}"
        );
        assert!(
            error.to_string().contains("job assignment"),
            "the typed error must name the failed step: {error}"
        );
    }

    /// A successful contained spawn hands back a child that actually runs:
    /// assignment (when required) and resume are both confirmed, then the
    /// target executes and exits with its own code.
    #[test]
    fn a_contained_child_runs_and_reports_its_own_exit_code() {
        let mut command = std::process::Command::new("cmd");
        command.args(["/D", "/S", "/C", "exit /b 42"]);
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // No job required: the same suspended-create + confirmed-resume
        // path, with no assignment step.
        let mut child = spawn_contained(&mut command, None).expect("spawn with no required job");
        let status = child.wait().expect("wait for the contained child");
        assert_eq!(status.code(), Some(42));
    }
}
