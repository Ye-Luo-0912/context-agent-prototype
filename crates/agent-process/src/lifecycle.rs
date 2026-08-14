//! 子进程身份与存活检查。
//!
//! PID 会复用：恢复路径只在能对上 OS 级创建令牌时才杀树。
//! 令牌拿不到时宁可留下孤儿，也不误杀后来占用同一 PID 的进程。

use crate::kill_process_tree;

/// 一次 spawn 留下的可核对身份。`identity_token` 为空表示当前平台
/// 无法钉住创建时间，恢复时不得按 PID 杀进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub identity_token: String,
}

/// 读取 pid 当前的 OS 身份令牌。进程已退出时失败。
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
    capture_process_identity(pid)
        .ok()
        .is_some_and(|identity| identity.identity_token == identity_token)
}

pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        windows_is_running(pid)
    }
    #[cfg(unix)]
    {
        unix_is_running(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// 仅在身份令牌对得上时杀树。Windows 的 taskkill 是异步的，
/// 所以这里短等确认，避免把仍在退出中的孩子当成杀树失败。
pub fn kill_matching_process_tree(pid: u32, identity_token: &str) -> bool {
    if !process_identity_matches(pid, identity_token) {
        return false;
    }
    kill_process_tree(pid);
    for _ in 0..20 {
        if !process_identity_matches(pid, identity_token) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    !process_identity_matches(pid, identity_token)
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
fn windows_identity_token(pid: u32) -> Result<String, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return Err(format!("open process {pid} for identity failed"));
        }
        let mut created = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exited = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut kernel = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut user = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        if ok == 0 {
            return Err(format!("query process {pid} create time failed"));
        }
        let token = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        Ok(format!("{token:016x}"))
    }
}

#[cfg(windows)]
fn windows_is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0_u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        let _ = CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(unix)]
fn unix_is_running(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "linux")]
fn linux_identity_token(pid: u32) -> Result<String, String> {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read boot id: {error}"))?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("read process {pid} stat: {error}"))?;
    let after_comm = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("process {pid} stat is missing comm"))?;
    let starttime = after_comm
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| format!("process {pid} stat is missing starttime"))?;
    Ok(format!("{}:{starttime}", boot_id.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
