//! Linux landlock confinement for child processes (OS-level write
//! fence, plus TCP deny when the kernel speaks ABI v4).
//!
//! Landlock (kernel 5.13+) lets an unprivileged process restrict *its own*
//! access irrevocably, and every descendant inherits the restriction. This
//! module applies the restriction in the child right before `exec` (via
//! `pre_exec`), so the spawned program runs with a kernel-enforced fence:
//! it can create, modify or destroy filesystem state only under the
//! configured write roots, and on ABI v4+ (kernel 6.7+) it cannot
//! `bind`/`connect` TCP — no port rules are added, so every TCP port is
//! denied. ABI v5 (kernel 6.10+) handles `LANDLOCK_ACCESS_FS_IOCTL_DEV`
//! without granting it, so newly opened character/block devices cannot
//! ioctl (inherited stdin/stdout/stderr stay usable). ABI v6 (kernel
//! 6.12+) also scopes abstract Unix sockets and
//! outbound signals (`LANDLOCK_SCOPE_SIGNAL`) when the kernel accepts
//! that handled-access set: the child cannot signal processes outside
//! its landlock domain (parent, siblings, the rest of the machine).
//!
//! Deliberate scope: filesystem *reads* are not handled. The child must
//! be able to load its executable and shared libraries, so a read fence
//! would have to enumerate loader paths (fragile across distros); instead
//! reads stay gated by the application-level broker (`fs.read` answered
//! by the host under the invocation's grant). UDP, raw sockets, netlink
//! and pathname Unix sockets stay unhandled (Landlock has no UDP bit;
//! seccomp/AppContainer stay out of v0). Windows has no Landlock.
//!
//! ABI floors on the write claim: cross-hierarchy renames are blocked
//! only from ABI v2 (`FS_REFER`, kernel 5.19+) and truncate /
//! open-with-O_TRUNC only from ABI v3 (`FS_TRUNCATE`, kernel 6.2+). The
//! adapter attests `fs_write_confined` only when those floors are
//! kernel-enforced; on ABI 1/2 the ruleset still confines create /
//! overwrite / unlink, but truncate (and on ABI 1, rename) stay raw
//! kernel behavior outside the claim.
//!
//! ABI adaptation: the handled-access set is tried newest-first (v6 net+
//! scope, v4 net, then v5/v3/v2/v1 filesystem-only). Older kernels refuse
//! unknown bits with `EINVAL`, or `E2BIG` when the attr is larger than
//! the kernel's struct and the extra bytes are non-zero, so those errors
//! continue to the next candidate. `landlock_restrict_self` demands the
//! no_new_privs bit (or CAP_SYS_ADMIN), so the child sets it via `prctl`
//! first — a side effect that also stops a setuid/setgid binary in the
//! confined tree from escalating at exec.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

/// Landlock syscall numbers (x86_64 and aarch64; stable since 5.13).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_LANDLOCK_CREATE_RULESET: i64 = 444;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_LANDLOCK_ADD_RULE: i64 = 445;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_LANDLOCK_RESTRICT_SELF: i64 = 446;

/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: usize = 1;

const FS_WRITE_FILE: u64 = 1 << 1;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13; // ABI v2 (kernel 5.19+)
const FS_TRUNCATE: u64 = 1 << 14; // ABI v3 (kernel 6.2+)
const FS_IOCTL_DEV: u64 = 1 << 15; // ABI v5 (kernel 6.10+)

/// `LANDLOCK_ACCESS_NET_BIND_TCP` / `CONNECT_TCP` (ABI v4, kernel 6.7+).
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const NET_TCP: u64 = NET_BIND_TCP | NET_CONNECT_TCP;

/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` / `LANDLOCK_SCOPE_SIGNAL`
/// (ABI v6, kernel 6.12+).
const SCOPE_ABSTRACT_UNIX: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;
const SCOPE_IPC: u64 = SCOPE_ABSTRACT_UNIX | SCOPE_SIGNAL;

/// Every right that can create, modify or destroy filesystem state. Reads
/// are intentionally absent (see the module docs).
const WRITE_ACCESS: u64 = FS_WRITE_FILE
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM;

/// Handled-access candidates, newest ABI first: the first that creates a
/// ruleset wins on the running kernel. `FS_REFER` (cross-hierarchy
/// renames), `FS_TRUNCATE` (open-with-truncate) and `FS_IOCTL_DEV`
/// (ioctl on newly opened devices) are included only where the kernel
/// knows them. `FS_IOCTL_DEV` is handled but never granted, so device
/// ioctls are deny-all when the kernel knows the bit.
const HANDLED_CANDIDATES: [u64; 4] = [
    WRITE_ACCESS | FS_TRUNCATE | FS_REFER | FS_IOCTL_DEV, // ABI v5 (kernel 6.10+)
    WRITE_ACCESS | FS_TRUNCATE | FS_REFER,                // ABI v3 (kernel 6.2+)
    WRITE_ACCESS | FS_REFER,                              // ABI v2 (kernel 5.19+)
    WRITE_ACCESS,                                         // ABI v1 (kernel 5.13+)
];

/// Smallest legal `landlock_ruleset_attr` (handled_access_fs only).
#[repr(C)]
struct RulesetAttrFs {
    handled_access_fs: u64,
}

/// ABI v4 attr (`handled_access_net`).
#[repr(C)]
struct RulesetAttrFsNet {
    handled_access_fs: u64,
    handled_access_net: u64,
}

/// ABI v6 attr (`scoped`).
#[repr(C)]
struct RulesetAttrFsNetScope {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// Smallest legal `landlock_path_beneath_attr` (allowed_access +
/// parent_fd; the kernel zero-fills the rest).
#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: RawFd,
}

/// Whether the running kernel supports landlock at all (probed with the
/// smallest valid ruleset). The parent checks this once before wiring a
/// child, so an unsupported kernel degrades to a warning instead of
/// failing every sandboxed spawn.
pub fn available() -> bool {
    let attr = RulesetAttrFs {
        handled_access_fs: WRITE_ACCESS,
    };
    close_if_ruleset(unsafe {
        create_ruleset(
            std::ptr::from_ref(&attr).cast(),
            std::mem::size_of::<RulesetAttrFs>(),
        )
    })
}

/// Whether the running kernel will handle TCP bind/connect in a landlock
/// ruleset (ABI v4 / Linux 6.7+). Older landlock kernels still get the
/// write fence; this probe is for tests that require the TCP half.
pub fn tcp_deny_available() -> bool {
    let attr = RulesetAttrFsNet {
        handled_access_fs: WRITE_ACCESS,
        handled_access_net: NET_TCP,
    };
    close_if_ruleset(unsafe {
        create_ruleset(
            std::ptr::from_ref(&attr).cast(),
            std::mem::size_of::<RulesetAttrFsNet>(),
        )
    })
}

/// Whether the running kernel will handle device ioctl in a landlock
/// ruleset (ABI v5 / Linux 6.10+). Older landlock kernels still get the
/// write fence; this probe is for tests that require the ioctl half.
pub fn ioctl_dev_deny_available() -> bool {
    let attr = RulesetAttrFs {
        handled_access_fs: WRITE_ACCESS | FS_TRUNCATE | FS_REFER | FS_IOCTL_DEV,
    };
    close_if_ruleset(unsafe {
        create_ruleset(
            std::ptr::from_ref(&attr).cast(),
            std::mem::size_of::<RulesetAttrFs>(),
        )
    })
}

/// Whether the running kernel will scope abstract Unix sockets and
/// outbound signals in a landlock ruleset (ABI v6 / Linux 6.12+). Older
/// landlock kernels still get the write fence (and TCP deny on ABI v4);
/// this probe is for tests that require the signal half.
pub fn signal_scope_available() -> bool {
    let attr = RulesetAttrFsNetScope {
        handled_access_fs: WRITE_ACCESS,
        handled_access_net: NET_TCP,
        scoped: SCOPE_IPC,
    };
    close_if_ruleset(unsafe {
        create_ruleset(
            std::ptr::from_ref(&attr).cast(),
            std::mem::size_of::<RulesetAttrFsNetScope>(),
        )
    })
}

/// Highest landlock ABI this kernel accepts, probed by creating real
/// rulesets newest-first (`0` = landlock unsupported). Attestation uses
/// this as the backend version so the write fence names its ABI.
pub fn abi_level() -> u8 {
    if signal_scope_available() {
        return 6;
    }
    if ioctl_dev_deny_available() {
        return 5;
    }
    if tcp_deny_available() {
        return 4;
    }
    for (candidate, level) in [
        (WRITE_ACCESS | FS_TRUNCATE | FS_REFER, 3_u8),
        (WRITE_ACCESS | FS_REFER, 2),
    ] {
        let attr = RulesetAttrFs {
            handled_access_fs: candidate,
        };
        if close_if_ruleset(unsafe {
            create_ruleset(
                std::ptr::from_ref(&attr).cast(),
                std::mem::size_of::<RulesetAttrFs>(),
            )
        }) {
            return level;
        }
    }
    u8::from(available())
}

fn close_if_ruleset(ret: i64) -> bool {
    if ret >= 0 {
        unsafe {
            libc::close(ret as RawFd);
        }
        true
    } else {
        false
    }
}

unsafe fn create_ruleset(attr: *const u8, size: usize) -> i64 {
    // SAFETY: `attr` is a landlock_ruleset_attr of `size` bytes; the
    // kernel copies min(size, its struct) and rejects extra non-zero
    // bytes with E2BIG. Called from the parent probe or from pre_exec.
    unsafe { libc::syscall(SYS_LANDLOCK_CREATE_RULESET, attr as usize, size, 0usize) }
}

fn last_os_is_retryable() -> bool {
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EINVAL) | Some(libc::E2BIG)
    )
}

/// Newest ABI first. `EINVAL` / `E2BIG` mean this kernel does not know
/// the attr; any other error is fatal (landlock unusable in this
/// namespace). Returns `(ruleset_fd, handled_access_fs)`.
fn create_handled_ruleset() -> io::Result<(RawFd, u64)> {
    for fs in HANDLED_CANDIDATES {
        let attr = RulesetAttrFsNetScope {
            handled_access_fs: fs,
            handled_access_net: NET_TCP,
            scoped: SCOPE_IPC,
        };
        let ret = unsafe {
            create_ruleset(
                std::ptr::from_ref(&attr).cast(),
                std::mem::size_of::<RulesetAttrFsNetScope>(),
            )
        };
        if ret >= 0 {
            return Ok((ret as RawFd, fs));
        }
        if !last_os_is_retryable() {
            return Err(io::Error::last_os_error());
        }
    }
    for fs in HANDLED_CANDIDATES {
        let attr = RulesetAttrFsNet {
            handled_access_fs: fs,
            handled_access_net: NET_TCP,
        };
        let ret = unsafe {
            create_ruleset(
                std::ptr::from_ref(&attr).cast(),
                std::mem::size_of::<RulesetAttrFsNet>(),
            )
        };
        if ret >= 0 {
            return Ok((ret as RawFd, fs));
        }
        if !last_os_is_retryable() {
            return Err(io::Error::last_os_error());
        }
    }
    for fs in HANDLED_CANDIDATES {
        let attr = RulesetAttrFs {
            handled_access_fs: fs,
        };
        let ret = unsafe {
            create_ruleset(
                std::ptr::from_ref(&attr).cast(),
                std::mem::size_of::<RulesetAttrFs>(),
            )
        };
        if ret >= 0 {
            return Ok((ret as RawFd, fs));
        }
        if !last_os_is_retryable() {
            return Err(io::Error::last_os_error());
        }
    }
    Err(io::Error::from_raw_os_error(libc::EINVAL))
}

/// The confinement a child will inherit: `O_PATH` file descriptors for the
/// write roots, opened in the parent. Raw fds are kept so the `pre_exec`
/// closure only makes syscalls (no allocation, no locks) — the parent's
/// copy is dropped after spawn, the child's copies are closed by
/// `O_CLOEXEC` at exec.
pub struct ChildRules {
    write_root_fds: Vec<RawFd>,
}

impl ChildRules {
    /// Open the write roots. A root that cannot be opened is a
    /// configuration error (the caller fails the spawn rather than running
    /// the child unconfined).
    pub fn open(roots: &[PathBuf]) -> io::Result<ChildRules> {
        let mut fds = Vec::with_capacity(roots.len());
        for root in roots {
            match open_path(root) {
                Ok(fd) => fds.push(fd),
                Err(error) => {
                    for fd in &fds {
                        unsafe {
                            libc::close(*fd);
                        }
                    }
                    return Err(io::Error::new(
                        error.kind(),
                        format!("open landlock write root '{}': {error}", root.display()),
                    ));
                }
            }
        }
        Ok(ChildRules {
            write_root_fds: fds,
        })
    }
}

impl Drop for ChildRules {
    fn drop(&mut self) {
        for fd in &self.write_root_fds {
            unsafe {
                libc::close(*fd);
            }
        }
    }
}

/// Create the ruleset, add one path-beneath rule per write root, and
/// restrict the calling process irrevocably. Runs inside `pre_exec` — the
/// child is single-threaded and this only makes raw syscalls, so it is
/// async-signal-safe. On error the spawn fails (fail closed: a child that
/// cannot be confined does not run).
pub fn apply_in_child(rules: &ChildRules) -> io::Result<()> {
    // Newest ABI first: TCP deny (no net-port rules → every TCP port
    // denied) and abstract-Unix plus signal scope when the kernel knows
    // them; older kernels still get the write fence alone.
    let (ruleset_fd, handled) = create_handled_ruleset()?;

    // One rule per write root, granting the write set (plus truncate when
    // the kernel handles it). `FS_REFER` and `FS_IOCTL_DEV` stay
    // ungranted: cross-tree renames are denied even between two write
    // roots, and newly opened character/block devices cannot ioctl.
    // No `LANDLOCK_RULE_NET_PORT` rules are added: handled TCP
    // bind/connect with an empty port set is deny-all.
    let granted = WRITE_ACCESS | (handled & FS_TRUNCATE);
    for fd in &rules.write_root_fds {
        let attr = PathBeneathAttr {
            allowed_access: granted,
            parent_fd: *fd,
        };
        let ret = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset_fd as usize,
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr as usize,
                0usize,
            )
        };
        if ret < 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(ruleset_fd);
            }
            return Err(error);
        }
    }

    // `landlock_restrict_self` requires the no_new_privs bit (or
    // CAP_SYS_ADMIN), so set it first. This also hardens the exec that
    // follows: a setuid/setgid binary can no longer escalate. `apply_
    // unix_rlimits` sets the same bit on Linux even when landlock is
    // skipped. `syscall(SYS_prctl)` stays async-signal-safe
    // inside pre_exec; libc 0.2 does not export `prctl`/`PR_*` on gnu.
    const PR_SET_NO_NEW_PRIVS: libc::c_long = 38;
    if unsafe { libc::syscall(libc::SYS_prctl, PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(ruleset_fd);
        }
        return Err(error);
    }

    let ret = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd as usize, 0usize) };
    unsafe {
        libc::close(ruleset_fd);
    }
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Open one directory as an `O_PATH` fd (no read/write permission needed —
/// the kernel resolves it by fd when the rule is added).
fn open_path(path: &Path) -> io::Result<RawFd> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `c` is a valid NUL-terminated path; the flags request no
    // data access and `O_CLOEXEC` keeps the fd out of exec'd images (it is
    // only needed inside pre_exec, which runs before that exec).
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_path_accepts_a_real_directory_and_rejects_missing() {
        let dir = tempfile::tempdir().unwrap();
        let fd = open_path(dir.path()).expect("an existing dir opens as O_PATH");
        unsafe {
            libc::close(fd);
        }
        assert!(open_path(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn child_rules_drop_releases_every_original_fd() {
        let dir = tempfile::tempdir().unwrap();
        let rules = ChildRules::open(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(rules.write_root_fds.len(), 1);
        let fd = rules.write_root_fds[0];
        let mut before = std::mem::MaybeUninit::<libc::stat>::zeroed();
        assert_eq!(unsafe { libc::fstat(fd, before.as_mut_ptr()) }, 0);
        let before = unsafe { before.assume_init() };
        drop(rules);
        // Descriptor numbers are process-global and another parallel test can
        // reuse this number immediately. EBADF is the common case; if reuse
        // won the race, it must no longer identify the directory opened by
        // ChildRules.
        let mut after = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let ret = unsafe { libc::fstat(fd, after.as_mut_ptr()) };
        if ret == -1 {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        } else {
            assert_eq!(ret, 0);
            let after = unsafe { after.assume_init() };
            assert_ne!(
                (after.st_dev, after.st_ino),
                (before.st_dev, before.st_ino),
                "a reused descriptor must not retain the dropped write-root identity"
            );
        }
    }

    #[test]
    fn tcp_deny_probe_does_not_panic() {
        let _ = tcp_deny_available();
    }

    #[test]
    fn signal_scope_probe_does_not_panic() {
        let _ = signal_scope_available();
    }

    #[test]
    fn ioctl_dev_probe_does_not_panic() {
        let _ = ioctl_dev_deny_available();
    }
}
