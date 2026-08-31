//! Probe binary for OS sandbox tests (`tests/landlock.rs`,
//! `tests/integrity.rs`, `tests/rlimits.rs`).
//!
//! Usage:
//! - `sandbox_probe <write-root> <denied-dir>` — write-fence checks
//! - `sandbox_probe alloc <bytes>` — try to allocate that many bytes
//! - `sandbox_probe fsize <path> <bytes>` — try to write that many bytes
//! - `sandbox_probe signal <pid>` — try `kill(pid, 0)` (Landlock signal scope)
//! - `sandbox_probe nofile <count>` — try to open that many extra files
//! - `sandbox_probe inherit-fd <fd>` — try to write to an inherited fd
//! - `sandbox_probe core` — print this process's `RLIMIT_CORE` (getrlimit)
//! - `sandbox_probe pri` — print Linux `RLIMIT_NICE` / `RLIMIT_RTPRIO` /
//!   `no_new_privs` after sandbox `pre_exec`
//! - `sandbox_probe jobmem` — print this process's Job-Object commit ceiling
//! - `sandbox_probe jobprio` — print this process's Job-Object priority class
//!
//! Write-fence: under a landlock / Low-IL confinement with `write-root`
//! as the only write root, the probe must create a file inside
//! `write-root`, must be refused creating one inside `denied-dir` at the
//! OS layer, and must still read a system file. On Landlock ABI v4+ a TCP
//! connect must be refused with `PermissionDenied`; older kernels print
//! `tcp-connect:unhandled`. Every check prints one line; the final line
//! is `RESULT:PASS` or `RESULT:FAIL`.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.as_slice() {
        [_, cmd, path, bytes] if cmd == "fsize" => fsize_main(path, bytes),
        [_, cmd, bytes] if cmd == "alloc" => alloc_main(bytes),
        [_, cmd, pid] if cmd == "signal" => signal_main(pid),
        [_, cmd, count] if cmd == "nofile" => nofile_main(count),
        [_, cmd, fd] if cmd == "inherit-fd" => inherit_fd_main(fd),
        [_, cmd] if cmd == "core" => core_main(),
        [_, cmd] if cmd == "pri" => pri_main(),
        [_, cmd] if cmd == "jobmem" => jobmem_main(),
        [_, cmd] if cmd == "jobprio" => jobprio_main(),
        [_, write_root, denied] => write_fence_main(write_root, denied),
        _ => {
            eprintln!(
                "usage: sandbox_probe <write-root> <denied-dir>\n       sandbox_probe alloc <bytes>\n       sandbox_probe fsize <path> <bytes>\n       sandbox_probe signal <pid>\n       sandbox_probe nofile <count>\n       sandbox_probe inherit-fd <fd>\n       sandbox_probe core\n       sandbox_probe pri\n       sandbox_probe jobmem\n       sandbox_probe jobprio"
            );
            std::process::exit(2);
        }
    }
}

fn alloc_main(bytes: &str) {
    let n: usize = match bytes.parse() {
        Ok(n) if n > 0 => n,
        _ => {
            eprintln!("alloc: invalid byte count");
            std::process::exit(2);
        }
    };
    println!("alloc-start:{n}");
    let mut blob = Vec::new();
    match blob.try_reserve_exact(n) {
        Ok(()) => {
            blob.resize(n, 1);
            let sink: u64 = blob.iter().map(|b| *b as u64).sum();
            std::hint::black_box(sink);
            println!("alloc:succeeded");
            println!("RESULT:FAIL");
            std::process::exit(1);
        }
        Err(error) => {
            println!("alloc:refused");
            println!("  detail: {error}");
            println!("RESULT:PASS");
            std::process::exit(0);
        }
    }
}

fn fsize_main(path: &str, bytes: &str) {
    let n: usize = match bytes.parse() {
        Ok(n) if n > 0 => n,
        _ => {
            eprintln!("fsize: invalid byte count");
            std::process::exit(2);
        }
    };
    // `RLIMIT_FSIZE` delivers `SIGXFSZ` as well as `EFBIG`. Ignore the
    // signal so a refused write can print instead of dying.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
    println!("fsize-start:{n}");
    let blob = vec![1u8; n];
    match std::fs::write(path, &blob) {
        Ok(()) => {
            println!("fsize:succeeded");
            println!("RESULT:FAIL");
            std::process::exit(1);
        }
        Err(error) => {
            println!("fsize:refused");
            println!("  detail: {error}");
            println!("RESULT:PASS");
            std::process::exit(0);
        }
    }
}

fn signal_main(pid: &str) {
    #[cfg(not(unix))]
    {
        let _ = pid;
        eprintln!("signal: unix only");
        std::process::exit(2);
    }
    #[cfg(unix)]
    {
        let target: libc::pid_t = match pid.parse() {
            Ok(n) if n > 0 => n,
            _ => {
                eprintln!("signal: invalid pid");
                std::process::exit(2);
            }
        };
        let mut ok = true;
        if unsafe { libc::kill(libc::getpid(), 0) } == 0 {
            println!("signal-self:ok");
        } else {
            println!("signal-self:FAIL");
            println!("  detail: {}", std::io::Error::last_os_error());
            ok = false;
        }
        let ret = unsafe { libc::kill(target, 0) };
        if ret == 0 {
            println!("signal-out:FAIL");
            println!("  detail: kill({target}, 0) succeeded (not scoped!)");
            ok = false;
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EPERM) {
                println!("signal-out:ok");
            } else {
                println!("signal-out:unhandled");
                println!("  detail: {error}");
                ok = false;
            }
        }
        println!("RESULT:{}", if ok { "PASS" } else { "FAIL" });
        std::process::exit(if ok { 0 } else { 1 });
    }
}

fn nofile_main(count: &str) {
    #[cfg(not(unix))]
    {
        let _ = count;
        eprintln!("nofile: unix only");
        std::process::exit(2);
    }
    #[cfg(unix)]
    {
        let n: usize = match count.parse() {
            Ok(n) if n > 0 => n,
            _ => {
                eprintln!("nofile: invalid count");
                std::process::exit(2);
            }
        };
        println!("nofile-start:{n}");
        let mut opened = Vec::new();
        for _ in 0..n {
            match std::fs::File::open("/dev/null") {
                Ok(file) => opened.push(file),
                Err(error)
                    if error.raw_os_error() == Some(libc::EMFILE)
                        || error.raw_os_error() == Some(libc::ENFILE) =>
                {
                    println!("nofile:refused after {}", opened.len());
                    println!("RESULT:PASS");
                    std::process::exit(0);
                }
                Err(error) => {
                    println!("nofile:error");
                    println!("  detail: {error}");
                    println!("RESULT:FAIL");
                    std::process::exit(1);
                }
            }
        }
        println!("nofile:succeeded {}", opened.len());
        println!("RESULT:FAIL");
        std::mem::drop(opened);
        std::process::exit(1);
    }
}

fn inherit_fd_main(fd: &str) {
    #[cfg(not(unix))]
    {
        let _ = fd;
        eprintln!("inherit-fd: unix only");
        std::process::exit(2);
    }
    #[cfg(unix)]
    {
        let fd: libc::c_int = match fd.parse() {
            Ok(n) if n >= 3 => n,
            _ => {
                eprintln!("inherit-fd: invalid fd");
                std::process::exit(2);
            }
        };
        let buf = b"x";
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, 1) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                println!("inherit-fd:closed");
                println!("RESULT:PASS");
                std::process::exit(0);
            }
            println!("inherit-fd:error");
            println!("  detail: {error}");
            println!("RESULT:FAIL");
            std::process::exit(1);
        }
        println!("inherit-fd:leaked");
        println!("RESULT:FAIL");
        std::process::exit(1);
    }
}

fn core_main() {
    #[cfg(not(unix))]
    {
        eprintln!("core: unix only");
        std::process::exit(2);
    }
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit {
            rlim_cur: libc::RLIM_INFINITY,
            rlim_max: libc::RLIM_INFINITY,
        };
        let ret = unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) };
        if ret != 0 {
            println!("core:error");
            println!("  detail: {}", std::io::Error::last_os_error());
            println!("RESULT:FAIL");
            std::process::exit(1);
        }
        println!("core:{}", limit.rlim_cur);
        println!("core-max:{}", limit.rlim_max);
        if limit.rlim_cur == 0 && limit.rlim_max == 0 {
            println!("RESULT:PASS");
            std::process::exit(0);
        }
        println!("RESULT:FAIL");
        std::process::exit(1);
    }
}

fn pri_main() {
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("pri: linux only");
        std::process::exit(2);
    }
    #[cfg(target_os = "linux")]
    {
        let mut ok = true;
        let mut print_rlimit = |name: &str, resource| {
            let mut limit = libc::rlimit {
                rlim_cur: libc::RLIM_INFINITY,
                rlim_max: libc::RLIM_INFINITY,
            };
            if unsafe { libc::getrlimit(resource, &mut limit) } != 0 {
                println!("{name}:error");
                println!("  detail: {}", std::io::Error::last_os_error());
                ok = false;
                return;
            }
            println!("{name}:{}", limit.rlim_cur);
            println!("{name}-max:{}", limit.rlim_max);
            if limit.rlim_cur != 0 || limit.rlim_max != 0 {
                ok = false;
            }
        };
        print_rlimit("nice", libc::RLIMIT_NICE);
        print_rlimit("rtprio", libc::RLIMIT_RTPRIO);
        const PR_GET_NO_NEW_PRIVS: libc::c_long = 39;
        let nnp = unsafe { libc::syscall(libc::SYS_prctl, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        if nnp < 0 {
            println!("nnp:error");
            println!("  detail: {}", std::io::Error::last_os_error());
            ok = false;
        } else {
            println!("nnp:{nnp}");
            if nnp != 1 {
                ok = false;
            }
        }
        println!("RESULT:{}", if ok { "PASS" } else { "FAIL" });
        std::process::exit(if ok { 0 } else { 1 });
    }
}

fn jobmem_main() {
    #[cfg(not(windows))]
    {
        eprintln!("jobmem: windows only");
        std::process::exit(2);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let mut returned = 0u32;
            let ok = QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                &mut returned,
            );
            if ok == 0 {
                println!("jobmem:unhandled");
                println!("  detail: {}", std::io::Error::last_os_error());
                println!("RESULT:FAIL");
                std::process::exit(2);
            }
            println!("jobmem:{}", info.ProcessMemoryLimit);
            println!("RESULT:PASS");
            std::process::exit(0);
        }
    }
}

fn jobprio_main() {
    #[cfg(not(windows))]
    {
        eprintln!("jobprio: windows only");
        std::process::exit(2);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, QueryInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::NORMAL_PRIORITY_CLASS;
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let mut returned = 0u32;
            let ok = QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                &mut returned,
            );
            if ok == 0 {
                println!("jobprio:unhandled");
                println!("  detail: {}", std::io::Error::last_os_error());
                println!("RESULT:FAIL");
                std::process::exit(2);
            }
            let flags = info.BasicLimitInformation.LimitFlags;
            let class = info.BasicLimitInformation.PriorityClass;
            println!("jobprio:{class}");
            println!("jobflags:{flags:#x}");
            let pinned = class == NORMAL_PRIORITY_CLASS
                && flags & JOB_OBJECT_LIMIT_PRIORITY_CLASS != 0
                && flags & JOB_OBJECT_LIMIT_BREAKAWAY_OK == 0
                && flags & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK == 0;
            if pinned {
                println!("RESULT:PASS");
                std::process::exit(0);
            }
            println!("RESULT:FAIL");
            std::process::exit(1);
        }
    }
}

fn write_fence_main(write_root: &str, denied: &str) {
    let mut ok = true;
    let check = |label: &str, passed: bool, detail: String, ok: &mut bool| {
        println!("{label}:{}", if passed { "ok" } else { "FAIL" });
        if !passed {
            println!("  detail: {detail}");
            *ok = false;
        }
    };

    // The kernel ABI the fence actually runs under, so a gate test can
    // demand `:ok` exactly for the ops that ABI enforces.
    #[cfg(target_os = "linux")]
    println!("abi:{}", agent_process::landlock::abi_level());
    #[cfg(not(target_os = "linux"))]
    println!("abi:none");

    // 1. Creating a file inside the write root must succeed.
    let inside = std::path::Path::new(&write_root).join("probe-inside.txt");
    match std::fs::write(&inside, b"x") {
        Ok(()) => check("write-inside", true, String::new(), &mut ok),
        Err(error) => check("write-inside", false, error.to_string(), &mut ok),
    }

    // 2. Creating a file outside every write root must be refused by the
    // kernel (EACCES/EROFS), not by application logic.
    let outside = std::path::Path::new(&denied).join("probe-outside.txt");
    match std::fs::write(&outside, b"x") {
        Ok(()) => check(
            "write-outside",
            false,
            "succeeded (not confined!)".into(),
            &mut ok,
        ),
        Err(error) => check("write-outside", true, error.to_string(), &mut ok),
    }

    // Fixture ops: the parent pre-creates one file per mutation, because a
    // confined child cannot create anything outside its roots. When the
    // fixture is absent (simple probes do not pre-create it) the op is
    // not attempted and reported as unhandled.

    // 3. Overwriting an existing file outside every write root must be
    // refused by the kernel (WRITE_FILE is handled from ABI v1).
    let overwrite = std::path::Path::new(&denied).join("probe-overwrite.txt");
    if overwrite.exists() {
        match std::fs::write(&overwrite, b"y") {
            Ok(()) => check(
                "overwrite-outside",
                false,
                format!(
                    "overwrite of {} succeeded (not confined!)",
                    overwrite.display()
                ),
                &mut ok,
            ),
            Err(error) => check("overwrite-outside", true, error.to_string(), &mut ok),
        }
    } else {
        println!("overwrite-outside:unhandled");
        println!("  detail: fixture missing");
    }

    // 4. Truncating an existing file outside every write root is
    // kernel-refused from ABI v3 (LANDLOCK_ACCESS_FS_TRUNCATE covers open
    // with O_TRUNC and truncate/ftruncate). Older ABIs leave truncate
    // unhandled; the probe reports that raw gap instead of failing the
    // fence (the attestation gates `fs_write_confined` on this ABI).
    let truncate = std::path::Path::new(&denied).join("probe-truncate.txt");
    if truncate.exists() {
        let refused = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&truncate)
            .and_then(|file| file.set_len(0))
            .is_err();
        if refused {
            check("truncate-outside", true, String::new(), &mut ok);
        } else {
            println!("truncate-outside:unhandled");
            println!("  detail: truncate succeeded (ABI < 3 does not handle FS_TRUNCATE)");
        }
    } else {
        println!("truncate-outside:unhandled");
        println!("  detail: fixture missing");
    }

    // 5. Renaming an existing file from outside into the write root is a
    // cross-hierarchy move: kernel-refused from ABI v2
    // (LANDLOCK_ACCESS_FS_REFER handled, never granted). ABI v1 does not
    // handle REFER; the probe reports that raw gap.
    let rename_src = std::path::Path::new(&denied).join("probe-rename-src.txt");
    let rename_dst = std::path::Path::new(&write_root).join("probe-renamed.txt");
    if rename_src.exists() {
        match std::fs::rename(&rename_src, &rename_dst) {
            Ok(()) => {
                println!("rename-outside:unhandled");
                println!("  detail: rename succeeded (ABI < 2 does not handle FS_REFER)");
                let _ = std::fs::remove_file(&rename_dst);
            }
            Err(error) => check("rename-outside", true, error.to_string(), &mut ok),
        }
    } else {
        println!("rename-outside:unhandled");
        println!("  detail: fixture missing");
    }

    // 6. Unlinking an existing file outside every write root must be
    // refused by the kernel (REMOVE_FILE is handled from ABI v1).
    let unlink = std::path::Path::new(&denied).join("probe-unlink.txt");
    if unlink.exists() {
        match std::fs::remove_file(&unlink) {
            Ok(()) => check(
                "unlink-outside",
                false,
                format!("unlink of {} succeeded (not confined!)", unlink.display()),
                &mut ok,
            ),
            Err(error) => check("unlink-outside", true, error.to_string(), &mut ok),
        }
    } else {
        println!("unlink-outside:unhandled");
        println!("  detail: fixture missing");
    }

    // 3. Reading a system file stays allowed (the read floor).
    #[cfg(windows)]
    let (read_label, read_ok) = {
        let hosts = r"C:\Windows\System32\drivers\etc\hosts";
        ("read-hosts", std::fs::read_to_string(hosts).is_ok())
    };
    #[cfg(not(windows))]
    let (read_label, read_ok) = (
        "read-passwd",
        std::fs::read_to_string("/etc/passwd").is_ok(),
    );
    check(
        read_label,
        read_ok,
        format!("cannot read a system file ({read_label})"),
        &mut ok,
    );

    // 4. TCP connect is denied when the kernel handles Landlock ABI v4
    // (bind/connect bits, no port rules). PermissionDenied is the kernel
    // fence. ConnectionRefused / TimedOut / Ok mean the syscall was
    // allowed — ABI < 4, not a write-fence failure.
    match std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 1)),
        std::time::Duration::from_millis(250),
    ) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            check("tcp-connect", true, String::new(), &mut ok);
        }
        other => {
            let detail = match other {
                Ok(_) => "connected (TCP not fenced)".into(),
                Err(error) => format!("{error} (syscall allowed; ABI < 4)"),
            };
            println!("tcp-connect:unhandled");
            println!("  detail: {detail}");
        }
    }

    // 5. Device ioctl is denied when the kernel handles Landlock ABI v5
    // (`LANDLOCK_ACCESS_FS_IOCTL_DEV` handled, never granted). FIONREAD
    // is not in the always-allowed ioctl list. PermissionDenied is the
    // kernel fence. ENOTTY / Ok mean the syscall was allowed — ABI < 5,
    // not a write-fence failure. Inherited stdio is unaffected.
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        match std::fs::File::open("/dev/null") {
            Ok(file) => {
                let mut pending: libc::c_int = 0;
                let ret = unsafe { libc::ioctl(file.as_raw_fd(), libc::FIONREAD, &mut pending) };
                if ret == 0 {
                    println!("ioctl-dev:unhandled");
                    println!("  detail: FIONREAD on /dev/null succeeded (ABI < 5)");
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::PermissionDenied {
                        check("ioctl-dev", true, String::new(), &mut ok);
                    } else {
                        println!("ioctl-dev:unhandled");
                        println!("  detail: {error} (syscall allowed; ABI < 5)");
                    }
                }
            }
            Err(error) => {
                println!("ioctl-dev:unhandled");
                println!("  detail: open /dev/null: {error}");
            }
        }
    }

    println!("RESULT:{}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
