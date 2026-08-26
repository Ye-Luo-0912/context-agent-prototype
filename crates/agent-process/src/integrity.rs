//! Windows integrity-level write confinement.
//!
//! AppContainer stays out of v0. A Medium IL parent labels configured write
//! roots Low and re-spawns through this process with [`WRAP_SENTINEL`]: a
//! CRT constructor hijacks that child, drops this process to Low IL, then
//! CreateProcess-es the real program with inherited stdio. Low IL cannot
//! write up to Medium objects (the parent's tree, `%TEMP%` siblings, most
//! of the user profile). Reads stay allowed (no-read-up is not the
//! default). TCP/UDP are not fenced.
//!
//! The wrap is this same executable, not a second binary: every product
//! and test that links `agent-process` can confine. The wrap creates a
//! Job-Object with `KILL_ON_JOB_CLOSE`, a 512 MiB per-process commit
//! ceiling, `DIE_ON_UNHANDLED_EXCEPTION`, and
//! `PRIORITY_CLASS=NORMAL` so TerminateProcess of the wrap
//! still kills the real child, a runaway Low-IL allocator cannot exhaust
//! the machine, and the child cannot raise HIGH/REALTIME. BREAKAWAY_OK
//! stays unset. ProcessHost's job covers the wrap process; this job
//! covers the program the wrap CreateProcess-es (stdio MCP children take
//! this path on Windows).

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddMandatoryAce, CONTAINER_INHERIT_ACE, CreateWellKnownSid, GetLengthSid,
    InitializeAcl, LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, WinLowLabelSid,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::SystemServices::{
    SE_GROUP_INTEGRITY, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, NORMAL_PRIORITY_CLASS, OpenProcess, OpenProcessToken, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE,
};

/// First argument that turns this executable into the Low-IL wrap.
pub const WRAP_SENTINEL: &str = "__FOCUS_AGENT_INTEGRITY_WRAP_V1__";

/// Per-process commit ceiling on the wrap's Job-Object.
/// Matches the capability adapter's Windows `job_max_memory_bytes`.
/// `RLIMIT_AS` on Unix is coarser (virtual maps); this is commit charge.
pub const WRAP_JOB_MAX_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

#[used]
#[unsafe(link_section = ".CRT$XCU")]
static INTEGRITY_WRAP_CTOR: unsafe extern "C" fn() = integrity_wrap_ctor;

unsafe extern "C" fn integrity_wrap_ctor() {
    maybe_run_as_wrap();
}

/// If argv[1] is [`WRAP_SENTINEL`], drop to Low IL, spawn the remaining
/// argv as the real child, and `exit` with its code. No-op otherwise.
pub fn maybe_run_as_wrap() {
    let mut args = std::env::args_os();
    let Some(_) = args.next() else {
        return;
    };
    let Some(marker) = args.next() else {
        return;
    };
    if marker != WRAP_SENTINEL {
        return;
    }
    let rest: Vec<OsString> = args.collect();
    std::process::exit(run_wrap(&rest));
}

/// Label each write root so a Low IL child can create files there.
/// A root that cannot be labeled fails the spawn (never run unconfined).
pub fn label_write_roots(roots: &[PathBuf]) -> io::Result<()> {
    for root in roots {
        std::fs::create_dir_all(root)?;
        label_path_low(root).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("label integrity write root '{}': {error}", root.display()),
            )
        })?;
    }
    Ok(())
}

/// Rewrite `program`/`args` so the spawned image is this executable in wrap
/// mode. The wrap drops IL and execs the original program with inherited
/// stdio/env/cwd. The *target* program is resolved first so a missing
/// binary stays a spawn error instead of an EOF on the wrap.
pub fn wrap_command(program: &str, args: &[String]) -> io::Result<(String, Vec<String>)> {
    if !program_exists(program) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("program '{program}' not found"),
        ));
    }
    let current = std::env::current_exe()?;
    let mut wrapped = vec![WRAP_SENTINEL.to_string(), program.to_string()];
    wrapped.extend(args.iter().cloned());
    Ok((current.to_string_lossy().into_owned(), wrapped))
}

fn program_exists(program: &str) -> bool {
    let path = Path::new(program);
    if path.exists() {
        return true;
    }
    if path.components().count() > 1 {
        return false;
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join(program).is_file() || dir.join(format!("{program}.exe")).is_file()
            })
        })
        .unwrap_or(false)
}

fn run_wrap(args: &[OsString]) -> i32 {
    let Some(program) = args.first() else {
        eprintln!("integrity wrap: missing program");
        return 2;
    };
    if let Err(error) = drop_to_low_integrity() {
        eprintln!("integrity wrap: drop to Low IL: {error}");
        return 1;
    }
    let job = match create_wrap_job() {
        Ok(job) => job,
        Err(error) => {
            eprintln!("integrity wrap: job object: {error}");
            return 1;
        }
    };
    let mut command = Command::new(program);
    command
        .args(&args[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "integrity wrap: spawn '{}': {error}",
                Path::new(program).display()
            );
            return 1;
        }
    };
    let _ = assign_pid_to_job(job.as_raw_handle(), child.id());
    match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

fn drop_to_low_integrity() -> io::Result<()> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_DEFAULT | TOKEN_QUERY,
            &mut token,
        ) == FALSE
        {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut sid = [0u8; 68];
        let mut sid_len = sid.len() as u32;
        if CreateWellKnownSid(
            WinLowLabelSid,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
        ) == FALSE
        {
            return Err(io::Error::last_os_error());
        }
        let mut label = TOKEN_MANDATORY_LABEL {
            Label: windows_sys::Win32::Security::SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        let size = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32;
        if SetTokenInformation(
            token.as_raw_handle(),
            TokenIntegrityLevel,
            std::ptr::from_mut(&mut label).cast(),
            size,
        ) == FALSE
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn label_path_low(path: &Path) -> io::Result<()> {
    unsafe {
        let mut sid = [0u8; 68];
        let mut sid_len = sid.len() as u32;
        if CreateWellKnownSid(
            WinLowLabelSid,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
        ) == FALSE
        {
            return Err(io::Error::last_os_error());
        }
        let sid_bytes = GetLengthSid(sid.as_mut_ptr().cast()) as usize;
        let acl_size = std::mem::size_of::<ACL>() + 32 + sid_bytes;
        let mut acl = vec![0u8; acl_size];
        if InitializeAcl(acl.as_mut_ptr().cast(), acl_size as u32, ACL_REVISION) == FALSE {
            return Err(io::Error::last_os_error());
        }
        if AddMandatoryAce(
            acl.as_mut_ptr().cast(),
            ACL_REVISION,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
            sid.as_mut_ptr().cast(),
        ) == FALSE
        {
            return Err(io::Error::last_os_error());
        }
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let status = SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl.as_mut_ptr().cast(),
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }
}

fn create_wrap_job() -> io::Result<OwnedHandle> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() || job == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle::from_raw_handle(job);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_PRIORITY_CLASS;
        info.BasicLimitInformation.PriorityClass = NORMAL_PRIORITY_CLASS;
        info.ProcessMemoryLimit = WRAP_JOB_MAX_MEMORY_BYTES as usize;
        let configured = SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}

fn assign_pid_to_job(job: HANDLE, pid: u32) -> bool {
    unsafe {
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return false;
        }
        let assigned = AssignProcessToJobObject(job, process);
        let _ = CloseHandle(process);
        assigned != 0
    }
}
