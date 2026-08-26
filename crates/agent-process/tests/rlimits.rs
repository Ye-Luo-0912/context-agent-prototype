//! Unix rlimit ceilings (`RLIMIT_AS`, `RLIMIT_FSIZE`,
//! `RLIMIT_NOFILE` plus inherited-fd close, `RLIMIT_CORE=0`,
//! Linux `RLIMIT_NICE`/`RLIMIT_RTPRIO`=0 plus `no_new_privs`).
//!
//! The alloc probe tries to map 2 GiB under a 512 MiB address-space cap.
//! The fsize probe tries to write 1 MiB under a 64 KiB file-size cap.
//! The nofile probe tries to open 64 extra files under a 16-fd cap.
//! The inherit-fd probe writes to a parent fd that had `O_CLOEXEC`
//! cleared; `close_inherited_fds` must make that write `EBADF`.
//! The core probe reads `getrlimit(RLIMIT_CORE)` after `apply_unix_rlimits`
//! (all other args 0) and must see both cur and max as 0 — it does not
//! crash the child. The kernel must refuse or kill the other probes.
//! Handshake tests prove `ProcessHost` still works with production-sized
//! ceilings.

#![cfg(unix)]

mod common;

use std::os::unix::io::IntoRawFd;
use std::os::unix::process::CommandExt;
use std::time::Duration;

use agent_process::{
    ProcessHost, ProcessHostConfig, ProcessSandbox, apply_unix_rlimits, close_inherited_fds,
    probe_siblings,
};

fn locate_probe() -> std::path::PathBuf {
    let name = "sandbox_probe";
    probe_siblings(&std::env::current_exe().unwrap(), name)
        .unwrap_or_else(|| panic!("cannot locate sandbox_probe; run `cargo test -p agent-process`"))
}

fn mock_host_config(sandbox: ProcessSandbox) -> ProcessHostConfig {
    let program = common::locate_mock_host()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            panic!("cannot locate the mock_host bin; run `cargo test -p agent-process`")
        });
    ProcessHostConfig {
        program,
        args: vec!["--serve".into()],
        env: vec![("MOCK_MARKER".into(), "1".into())],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
        sandbox,
    }
}

#[test]
fn child_cannot_allocate_past_rlimit_as() {
    let mut command = std::process::Command::new(locate_probe());
    command.args(["alloc", "2147483648"]);
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: (512 * 1024 * 1024) as libc::rlim_t,
                rlim_max: (512 * 1024 * 1024) as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().expect("probe runs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !stdout.contains("alloc:succeeded"),
        "the child must not commit 2 GiB under a 512 MiB RLIMIT_AS\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !output.status.success() || stdout.contains("alloc:refused"),
        "the kernel must refuse or kill the oversized allocation, got status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}

#[test]
fn child_cannot_write_past_rlimit_fsize() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("oversize.bin");
    let mut command = std::process::Command::new(locate_probe());
    command.args([
        "fsize",
        path.to_str().expect("utf-8 path"),
        "1048576", // 1 MiB write under a 64 KiB file cap
    ]);
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: (64 * 1024) as libc::rlim_t,
                rlim_max: (64 * 1024) as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().expect("probe runs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !stdout.contains("fsize:succeeded"),
        "the child must not write 1 MiB under a 64 KiB RLIMIT_FSIZE\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !output.status.success() || stdout.contains("fsize:refused"),
        "the kernel must refuse or kill the oversized write, got status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}

#[test]
fn child_cannot_open_past_rlimit_nofile() {
    let mut command = std::process::Command::new(locate_probe());
    command.args(["nofile", "64"]);
    unsafe {
        command.pre_exec(|| {
            apply_unix_rlimits(0, 0, 0, 0, 16)?;
            close_inherited_fds();
            Ok(())
        });
    }
    let output = command.output().expect("probe runs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !stdout.contains("nofile:succeeded"),
        "the child must not open 64 extra files under a 16-fd RLIMIT_NOFILE\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !output.status.success() || stdout.contains("nofile:refused"),
        "the kernel must refuse the extra opens, got status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}

#[test]
fn child_cannot_keep_inherited_fds_without_cloexec() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("leaked.txt");
    std::fs::write(&path, b"keep").expect("seed file");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open leaked file");
    let fd = file.into_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert!(flags >= 0, "F_GETFD");
        assert_eq!(
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC),
            0,
            "clear O_CLOEXEC so the fd would leak without close_inherited_fds"
        );
    }
    let mut command = std::process::Command::new(locate_probe());
    command.args(["inherit-fd", &fd.to_string()]);
    unsafe {
        command.pre_exec(|| {
            apply_unix_rlimits(0, 0, 0, 0, 32)?;
            close_inherited_fds();
            Ok(())
        });
    }
    let output = command.output().expect("probe runs");
    unsafe {
        libc::close(fd);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stdout.contains("inherit-fd:closed"),
        "the child must not keep a parent fd that had O_CLOEXEC cleared\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "inherit-fd probe should exit 0 when the fd is closed, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}

#[tokio::test]
async fn process_host_handshake_works_under_memory_rlimit() {
    let host = ProcessHost::connect(mock_host_config(ProcessSandbox {
        max_memory_bytes: 2u64 * 1024 * 1024 * 1024,
        ..ProcessSandbox::default()
    }))
    .await
    .expect("mock handshake still works under RLIMIT_AS");
    let value = host
        .call(serde_json::json!({ "op": "ping" }))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("pong"));
    host.shutdown().await;
}

#[tokio::test]
async fn process_host_handshake_works_under_file_rlimit() {
    let host = ProcessHost::connect(mock_host_config(ProcessSandbox {
        max_file_bytes: 256 * 1024 * 1024,
        ..ProcessSandbox::default()
    }))
    .await
    .expect("mock handshake still works under RLIMIT_FSIZE");
    let value = host
        .call(serde_json::json!({ "op": "ping" }))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("pong"));
    host.shutdown().await;
}

#[tokio::test]
async fn process_host_handshake_works_under_nofile_rlimit() {
    let host = ProcessHost::connect(mock_host_config(ProcessSandbox {
        max_open_files: 1024,
        ..ProcessSandbox::default()
    }))
    .await
    .expect("mock handshake still works under RLIMIT_NOFILE");
    let value = host
        .call(serde_json::json!({ "op": "ping" }))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("pong"));
    host.shutdown().await;
}

#[test]
fn child_sees_rlimit_core_zero_when_sandbox_pre_exec_runs() {
    // Other apply_unix_rlimits args use 0 = unlimited; CORE is still
    // forced to zero. Probe getrlimit — do not crash the child.
    let mut command = std::process::Command::new(locate_probe());
    command.args(["core"]);
    unsafe {
        command.pre_exec(|| apply_unix_rlimits(0, 0, 0, 0, 0));
    }
    let output = command.output().expect("probe runs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stdout.contains("core:0"),
        "soft RLIMIT_CORE must be 0 after apply_unix_rlimits\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("core-max:0"),
        "hard RLIMIT_CORE must be 0 after apply_unix_rlimits\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "core probe should exit 0 when both limits are zero, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn child_sees_priority_rlimits_and_no_new_privs_when_sandbox_pre_exec_runs() {
    // Other apply_unix_rlimits args use 0 = unlimited; Linux still clamps
    // NICE/RTPRIO to 0 and sets no_new_privs. Probe getrlimit / prctl —
    // do not try setpriority.
    let mut command = std::process::Command::new(locate_probe());
    command.args(["pri"]);
    unsafe {
        command.pre_exec(|| apply_unix_rlimits(0, 0, 0, 0, 0));
    }
    let output = command.output().expect("probe runs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stdout.contains("nice:0"),
        "soft RLIMIT_NICE must be 0 after apply_unix_rlimits\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("nice-max:0"),
        "hard RLIMIT_NICE must be 0 after apply_unix_rlimits\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("rtprio:0"),
        "soft RLIMIT_RTPRIO must be 0 after apply_unix_rlimits\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("rtprio-max:0"),
        "hard RLIMIT_RTPRIO must be 0 after apply_unix_rlimits\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("nnp:1"),
        "PR_GET_NO_NEW_PRIVS must be 1 after apply_unix_rlimits\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "pri probe should exit 0 when nice/rtprio are zero and nnp is set, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}
