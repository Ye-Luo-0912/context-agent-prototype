//! 子进程身份与存活检查。
//!
//! PID 会复用：恢复路径只在能对上 OS 级创建令牌时才杀树。
//! 令牌拿不到时宁可留下孤儿，也不误杀后来占用同一 PID 的进程。

#[cfg(not(windows))]
use crate::kill_process_tree;

/// 一次 spawn 留下的可核对身份。`identity_token` 为空表示当前平台
/// 无法钉住创建时间，恢复时不得按 PID 杀进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub identity_token: String,
}

/// An OS observation, not a heuristic over exit codes. An error from
/// `inspect_process` means the state could not be established; it is never
/// evidence that the process exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    Running(ProcessIdentity),
    Exited,
}

/// Cleanup observations remain distinct from permission to execute/replay.
/// Even ExitConfirmed only establishes process exit, not what it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessCleanupOutcome {
    AlreadyExited,
    IdentityMismatch,
    ExitConfirmed,
    Unconfirmed { reason: String },
}

pub fn inspect_process(pid: u32) -> Result<ProcessState, String> {
    if pid == 0 {
        return Err("pid 0 is not a managed child".into());
    }
    #[cfg(windows)]
    {
        windows_inspect_process(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_inspect_process(pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        match unix_liveness(pid)? {
            false => Ok(ProcessState::Exited),
            true => Ok(ProcessState::Running(ProcessIdentity {
                pid,
                identity_token: String::new(),
            })),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err("process observation is unsupported on this platform".into())
    }
}

/// 读取 OS 创建身份；保留句柄的已退出进程或 zombie 也可能仍可读取。
/// 身份可读不是存活证明；清理路径使用 `inspect_process`。
pub fn capture_process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    if pid == 0 {
        return Err("pid 0 is not a managed child".into());
    }
    Ok(ProcessIdentity {
        pid,
        identity_token: identity_token(pid)?,
    })
}

/// 仍在运行且创建令牌与记录一致。空令牌永远不匹配。
pub fn process_identity_matches(pid: u32, identity_token: &str) -> bool {
    if pid == 0 || identity_token.is_empty() {
        return false;
    }
    matches!(inspect_process(pid), Ok(ProcessState::Running(identity)) if identity.identity_token == identity_token)
}

/// Conservative compatibility predicate: true includes an unverifiable
/// process. Recovery code must use the typed observation, not invert this.
pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    !matches!(inspect_process(pid), Ok(ProcessState::Exited))
}

/// 旧调用方的兼容接口，只有发出信号后确认退出才返回 true。
/// 恢复与台账必须使用 `terminate_matching_process_tree` 的类型化结果。
pub fn kill_matching_process_tree(pid: u32, identity_token: &str) -> bool {
    matches!(
        terminate_matching_process_tree(pid, identity_token),
        Ok(ProcessCleanupOutcome::ExitConfirmed)
    )
}

/// Check creation identity before signalling, then require a positive exit
/// observation. A failed lookup after kill must never become a cleanup ACK.
/// Windows holds the verified root handle through signalling and exit
/// confirmation. Whole-tree containment still belongs to the spawn-time
/// job/watchdog; this API does not reconstruct that containment at restore.
pub fn terminate_matching_process_tree(
    pid: u32,
    identity_token: &str,
) -> Result<ProcessCleanupOutcome, String> {
    #[cfg(windows)]
    {
        windows_terminate_matching_tree(pid, identity_token)
    }
    #[cfg(not(windows))]
    {
        terminate_observed(pid, identity_token, inspect_process, |pid| {
            kill_process_tree(pid);
            Ok(())
        })
    }
}

fn terminate_observed(
    pid: u32,
    identity_token: &str,
    mut inspect: impl FnMut(u32) -> Result<ProcessState, String>,
    signal: impl FnOnce(u32) -> Result<(), String>,
) -> Result<ProcessCleanupOutcome, String> {
    match inspect(pid)? {
        ProcessState::Exited => return Ok(ProcessCleanupOutcome::AlreadyExited),
        ProcessState::Running(identity) => {
            if identity_token.is_empty() || identity.identity_token.is_empty() {
                return Err(format!("process {pid} has no verifiable creation identity"));
            }
            if identity.identity_token != identity_token {
                return Ok(ProcessCleanupOutcome::IdentityMismatch);
            }
        }
    }
    if let Err(reason) = signal(pid) {
        return Ok(ProcessCleanupOutcome::Unconfirmed { reason });
    }
    for attempt in 0..=20 {
        match inspect(pid) {
            Ok(ProcessState::Exited) => return Ok(ProcessCleanupOutcome::ExitConfirmed),
            Ok(ProcessState::Running(identity))
                if !identity.identity_token.is_empty()
                    && identity.identity_token != identity_token =>
            {
                return Ok(ProcessCleanupOutcome::ExitConfirmed);
            }
            Err(reason) => return Ok(ProcessCleanupOutcome::Unconfirmed { reason }),
            _ => {}
        }
        if attempt < 20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    Ok(ProcessCleanupOutcome::Unconfirmed {
        reason: format!("process {pid} exit was not confirmed before the cleanup deadline"),
    })
}

fn identity_token(pid: u32) -> Result<String, String> {
    #[cfg(windows)]
    {
        windows_identity_token(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_identity_token(pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = pid;
        Ok(String::new())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(String::new())
    }
}

#[cfg(windows)]
fn windows_process_handle(
    pid: u32,
    access: u32,
) -> Result<std::os::windows::io::OwnedHandle, std::io::Error> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Threading::OpenProcess;
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // Every successful open immediately enters RAII, including error paths.
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle.cast()) })
}

#[cfg(windows)]
fn windows_token_from_handle(handle: &std::os::windows::io::OwnedHandle) -> Result<String, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    unsafe {
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        if GetProcessTimes(
            handle.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return Err(format!(
                "query process creation time: {}",
                std::io::Error::last_os_error()
            ));
        }
        let token = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        Ok(format!("{token:016x}"))
    }
}

#[cfg(windows)]
fn windows_identity_token(pid: u32) -> Result<String, String> {
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    let handle = windows_process_handle(pid, PROCESS_QUERY_LIMITED_INFORMATION)
        .map_err(|error| format!("open process {pid} for identity: {error}"))?;
    windows_token_from_handle(&handle)
}

#[cfg(windows)]
fn windows_inspect_process(pid: u32) -> Result<ProcessState, String> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Threading::{
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };
    let handle = match windows_process_handle(
        pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
    ) {
        Ok(handle) => handle,
        // pid 0 was rejected before this call. A nonexistent nonzero PID is
        // INVALID_PARAMETER; ACCESS_DENIED and all other errors stay unknown.
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
            return Ok(ProcessState::Exited);
        }
        Err(error) => return Err(format!("observe process {pid}: {error}")),
    };
    windows_state_from_handle(pid, &handle)
}

#[cfg(windows)]
fn windows_state_from_handle(
    pid: u32,
    handle: &std::os::windows::io::OwnedHandle,
) -> Result<ProcessState, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(ProcessState::Exited),
        WAIT_TIMEOUT => Ok(ProcessState::Running(ProcessIdentity {
            pid,
            identity_token: windows_token_from_handle(handle)?,
        })),
        _ => Err(format!(
            "probe process {pid} wait state: {}",
            std::io::Error::last_os_error()
        )),
    }
}

#[cfg(windows)]
fn windows_terminate_matching_tree(pid: u32, token: &str) -> Result<ProcessCleanupOutcome, String> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Threading::{
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    if pid == 0 {
        return Err("pid 0 is not a managed child".into());
    }
    let handle = match windows_process_handle(
        pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
    ) {
        Ok(handle) => handle,
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
            return Ok(ProcessCleanupOutcome::AlreadyExited);
        }
        Err(error) => return Err(format!("open process {pid} for cleanup: {error}")),
    };
    // This handle owns the root object (and its PID) continuously. Neither
    // the external tree helper nor the fallback can hit a reused root PID.
    terminate_observed(
        pid,
        token,
        |pid| windows_state_from_handle(pid, &handle),
        |pid| windows_signal_tree(pid, &handle),
    )
}

/// Compatibility callers own a newly spawned child; recovery additionally
/// validates its recorded creation token via windows_terminate_matching_tree.
#[cfg(windows)]
pub(crate) fn windows_kill_process_tree(pid: u32) {
    use windows_sys::Win32::System::Threading::{
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    if let Ok(handle) = windows_process_handle(
        pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
    ) {
        let _ = windows_signal_tree(pid, &handle);
    }
}

#[cfg(windows)]
fn windows_signal_tree(pid: u32, handle: &std::os::windows::io::OwnedHandle) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, TerminateProcess};

    let tree_result = (|| {
        let directory = windows_system_directory()?;
        let mut helper = std::process::Command::new(directory.join("taskkill.exe"))
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .current_dir(&directory)
            .env_clear()
            .env(
                "SystemRoot",
                directory.parent().ok_or("system directory has no parent")?,
            )
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| format!("start system tree helper: {error}"))?;
        wait_for_tree_helper(&mut helper, std::time::Duration::from_secs(2))
    })();
    // Use the already-owned handle. An exited process can reject another
    // terminate call with ACCESS_DENIED, so check that same object's wait
    // state before classifying it as a failure.
    if unsafe { TerminateProcess(handle.as_raw_handle(), 1) } == 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(
            windows_state_from_handle(pid, handle),
            Ok(ProcessState::Exited)
        ) {
            return Err(format!("terminate owned process {pid}: {error}"));
        }
    }
    tree_result
}

#[cfg(windows)]
fn windows_system_directory() -> Result<std::path::PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buffer = [0u16; 32768];
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(format!(
            "resolve system directory: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(std::ffi::OsString::from_wide(&buffer[..len]).into())
}

#[cfg(windows)]
fn wait_for_tree_helper(
    child: &mut std::process::Child,
    budget: std::time::Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    let reason = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("tree helper failed: {status}")),
            Err(error) => break format!("observe tree helper: {error}"),
            Ok(None) if std::time::Instant::now() >= deadline => {
                break "tree helper timed out".into();
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    };
    let _ = child.kill();
    let reap_deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < reap_deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(reason)
}

#[cfg(unix)]
fn unix_liveness(pid: u32) -> Result<bool, String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err("invalid positive process id".into());
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EPERM) => Ok(true),
        Some(libc::ESRCH) => Ok(false),
        _ => Err(format!("probe process {pid} liveness: {error}")),
    }
}

#[cfg(target_os = "linux")]
fn linux_inspect_process(pid: u32) -> Result<ProcessState, String> {
    if pid > i32::MAX as u32 {
        return Err("invalid positive process id".into());
    }
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) => {
            // hidepid/proc permissions can hide a live PID. Missing proc data
            // is not enough: only ESRCH from the OS liveness probe proves exit.
            return match unix_liveness(pid)? {
                false => Ok(ProcessState::Exited),
                true => Err(format!("read process {pid} state: {error}")),
            };
        }
    };
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .ok_or_else(|| format!("process {pid} stat is missing state"))?;
    if matches!(state, "Z" | "X") {
        return Ok(ProcessState::Exited);
    }
    Ok(ProcessState::Running(ProcessIdentity {
        pid,
        identity_token: linux_token_from_stat(pid, &stat)?,
    }))
}

#[cfg(target_os = "linux")]
fn linux_token_from_stat(pid: u32, stat: &str) -> Result<String, String> {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read boot id: {error}"))?;
    let after_comm = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("process {pid} stat is missing comm"))?;
    let starttime = after_comm
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| format!("process {pid} stat is missing starttime"))?;
    starttime
        .parse::<u64>()
        .map_err(|_| format!("process {pid} has invalid starttime"))?;
    if boot_id.trim().is_empty() {
        return Err("empty boot identity".into());
    }
    Ok(format!("{}:{starttime}", boot_id.trim()))
}

#[cfg(target_os = "linux")]
fn linux_identity_token(pid: u32) -> Result<String, String> {
    if pid > i32::MAX as u32 {
        return Err("invalid positive process id".into());
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("read process {pid} stat: {error}"))?;
    linux_token_from_stat(pid, &stat)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(token: &str) -> Result<ProcessState, String> {
        Ok(ProcessState::Running(ProcessIdentity {
            pid: 42,
            identity_token: token.into(),
        }))
    }

    #[test]
    fn unobservable_exit_after_signal_is_not_a_cleanup_receipt() {
        let mut observations = std::collections::VecDeque::from([
            running("creation"),
            Err("access denied while confirming exit".into()),
        ]);
        let signalled = std::cell::Cell::new(false);
        let outcome = terminate_observed(
            42,
            "creation",
            |_| observations.pop_front().unwrap(),
            |_| {
                signalled.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(signalled.get());
        assert!(matches!(outcome, ProcessCleanupOutcome::Unconfirmed { .. }));
    }

    #[test]
    fn unverifiable_or_foreign_identity_never_signals() {
        let no_signal = |_| panic!("unproven process identity must never authorize a signal");
        assert!(
            terminate_observed(42, "creation", |_| Err("access denied".into()), no_signal).is_err()
        );
        assert!(terminate_observed(42, "", |_| running("creation"), no_signal).is_err());
        assert_eq!(
            terminate_observed(42, "old", |_| running("new"), no_signal).unwrap(),
            ProcessCleanupOutcome::IdentityMismatch
        );
    }

    #[test]
    fn confirmed_exit_is_distinct_from_an_already_gone_process() {
        assert_eq!(
            terminate_observed(
                42,
                "creation",
                |_| Ok(ProcessState::Exited),
                |_| panic!("already gone")
            )
            .unwrap(),
            ProcessCleanupOutcome::AlreadyExited
        );
        let mut observations =
            std::collections::VecDeque::from([running("creation"), Ok(ProcessState::Exited)]);
        assert_eq!(
            terminate_observed(
                42,
                "creation",
                |_| observations.pop_front().unwrap(),
                |_| Ok(())
            )
            .unwrap(),
            ProcessCleanupOutcome::ExitConfirmed
        );
    }

    #[test]
    fn a_failed_tree_signal_does_not_become_a_cleanup_receipt() {
        let outcome = terminate_observed(
            42,
            "creation",
            |_| running("creation"),
            |_| Err("tree helper timed out after partial cleanup".into()),
        )
        .unwrap();
        assert!(
            matches!(outcome, ProcessCleanupOutcome::Unconfirmed { reason } if reason.contains("timed out"))
        );
    }

    #[cfg(windows)]
    fn windows_sleeper() -> std::process::Child {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        std::process::Command::new(windows_system_directory().unwrap().join("ping.exe"))
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_checks_the_identity_then_confirms_the_owned_process() {
        let mut child = windows_sleeper();
        let pid = child.id();
        let token = capture_process_identity(pid).unwrap().identity_token;
        assert_eq!(
            terminate_matching_process_tree(pid, "wrong-token").unwrap(),
            ProcessCleanupOutcome::IdentityMismatch
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "identity mismatch must not signal"
        );
        assert_eq!(
            terminate_matching_process_tree(pid, &token).unwrap(),
            ProcessCleanupOutcome::ExitConfirmed
        );
        child.wait().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn wedged_tree_helper_is_terminated_within_the_cleanup_budget() {
        let mut helper = windows_sleeper();
        let start = std::time::Instant::now();
        let result = wait_for_tree_helper(&mut helper, std::time::Duration::from_millis(50));
        assert!(result.unwrap_err().contains("timed out"));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert!(
            helper.try_wait().unwrap().is_some(),
            "owned helper must be reaped"
        );
    }

    #[cfg(windows)]
    #[test]
    fn exit_code_259_is_an_exited_process_not_still_active() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "259"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert_eq!(child.wait().unwrap().code(), Some(259));
        // Child retains its kernel handle, so the PID still resolves.
        assert_eq!(inspect_process(pid).unwrap(), ProcessState::Exited);
        assert!(!process_is_running(pid));
    }

    #[cfg(windows)]
    #[test]
    fn protected_system_process_is_never_reported_exited() {
        // Read-only observation. This test never authorizes a signal to PID 4.
        assert!(!matches!(inspect_process(4), Ok(ProcessState::Exited)));
        assert!(process_is_running(4));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_unsigned_pid_is_not_a_negative_process_group() {
        assert!(inspect_process(u32::MAX).is_err());
        assert!(capture_process_identity(u32::MAX).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreaped_zombie_is_positively_exited() {
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !matches!(inspect_process(pid), Ok(ProcessState::Exited)) {
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!process_is_running(pid));
        assert_eq!(
            terminate_matching_process_tree(pid, "").unwrap(),
            ProcessCleanupOutcome::AlreadyExited
        );
        child.wait().unwrap();
    }

    #[test]
    fn current_process_identity_is_stable_while_alive() {
        let pid = std::process::id();
        let first = capture_process_identity(pid).expect("current process must be inspectable");
        assert_eq!(first.pid, pid);
        if first.identity_token.is_empty() {
            assert!(!process_identity_matches(pid, &first.identity_token));
            return;
        }
        assert!(process_identity_matches(pid, &first.identity_token));
        let second = capture_process_identity(pid).unwrap();
        assert_eq!(first.identity_token, second.identity_token);
    }

    #[test]
    fn missing_or_zero_pid_never_matches() {
        assert!(!process_identity_matches(0, "token"));
        assert!(!process_is_running(0));
        assert!(capture_process_identity(0).is_err());
    }
}
