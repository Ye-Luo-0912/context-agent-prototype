//! Confined directory-handle-relative filesystem primitives.
//!
//! `Workspace::resolve_relative` validates a path component by component and
//! returns an absolute path string; the caller then opens that string later.
//! Between the validation and the open, an attacker can swap any component
//! for a symlink / junction / reparse point that points outside the
//! workspace — the classic check-then-use (TOCTOU) race. Path-string
//! validation can never close that window.
//!
//! This module closes it by making "validate" and "open" one operation:
//! every component is resolved *relative to the already-open parent
//! directory handle*, and each component is opened with link-following
//! disabled. A directory handle pins the directory object itself — renaming
//! or replacing the path after we hold the handle cannot redirect the
//! descent, because the next step is taken through the handle, not the
//! name. Reparse substitution is rejected at open time.
//!
//! - Unix: `openat` with `O_NOFOLLOW` (`O_DIRECTORY` for intermediate
//!   components), `mkdirat`, `renameat`, `unlinkat`.
//! - Windows: `NtCreateFile` with a `RootDirectory` handle and
//!   `FILE_OPEN_REPARSE_POINT`, plus an explicit reparse-tag check after
//!   every open (any nonzero tag — symlink, junction, mount point,
//!   cloud placeholder — is rejected); the atomic replace uses
//!   `SetFileInformationByHandle` with `FILE_RENAME_INFO` relative to the
//!   parent handle.
//!
//! Not covered: bind mounts on Unix (they require privileges to create and
//! are a directory, not a link) and the workspace root itself (it is
//! canonicalized at open and owned by the trusted core).

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle};

#[cfg(windows)]
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile, NtSetInformationFile,
    },
};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileAttributeTagInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
    },
    System::IO::IO_STATUS_BLOCK,
};

/// An open directory handle plus the lexical path it was opened through
/// (for error messages and journal display only — the handle is the
/// authority, not the path).
pub struct ConfinedDir {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    display: PathBuf,
}

impl std::fmt::Debug for ConfinedDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfinedDir")
            .field("display", &self.display)
            .finish()
    }
}

impl ConfinedDir {
    /// Open a workspace root directory handle. The root is canonicalized by
    /// the caller; `O_NOFOLLOW` / reparse rejection still guards it here so
    /// a swapped root cannot redirect the descent.
    pub fn open_root(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let fd = open_dir_fd(libc::AT_FDCWD, path.as_os_str())?;
            Ok(Self {
                fd,
                display: path.to_path_buf(),
            })
        }
        #[cfg(windows)]
        {
            let handle = open_root_handle(path)?;
            Ok(Self {
                handle,
                display: path.to_path_buf(),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {
                display: path.to_path_buf(),
            })
        }
    }

    /// The lexical path this handle was opened through (display only).
    pub fn display(&self) -> &Path {
        &self.display
    }

    /// Establish a directory-entry durability barrier where the platform
    /// exposes one. Windows has no supported directory `FlushFileBuffers`
    /// equivalent; callers still sync the created file itself and retain the
    /// documented power-loss window for its directory entry.
    pub(crate) fn sync_all(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            // SAFETY: `dup` creates a new descriptor referring to the same
            // pinned directory; `File` takes sole ownership of the duplicate.
            let duplicated = unsafe { libc::dup(self.fd.as_raw_fd()) };
            if duplicated < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { std::fs::File::from_raw_fd(duplicated) };
            file.sync_all()
        }
        #[cfg(windows)]
        {
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            std::fs::File::open(&self.display)?.sync_all()
        }
    }

    /// Open an intermediate component as a directory, refusing to follow a
    /// link (symlink / junction / any reparse point).
    pub(crate) fn open_child_dir(&self, name: &OsStr) -> io::Result<Self> {
        let child_display = self.display.join(name);
        #[cfg(unix)]
        {
            let fd = open_dir_fd(self.fd.as_raw_fd(), name)?;
            Ok(Self {
                fd,
                display: child_display,
            })
        }
        #[cfg(windows)]
        {
            let handle = nt_open_relative(
                self.handle.as_raw_handle(),
                name,
                DIR_ACCESS,
                FILE_OPEN,
                FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            )?;
            check_not_reparse(handle, &child_display)?;
            // SAFETY: `handle` is a fresh kernel handle with no other owner.
            Ok(Self {
                handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
                display: child_display,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            if !child_display.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("not a directory: {}", child_display.display()),
                ));
            }
            Ok(Self {
                display: child_display,
            })
        }
    }

    /// Create a directory under this handle. Fails with `AlreadyExists`
    /// when the name is taken, so a pre-planted link cannot be shadowed.
    pub(crate) fn create_child_dir(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let cname = to_cstring(name)?;
            // SAFETY: `cname` is a valid NUL-terminated path; mode applies
            // only on creation. The new directory is private to the runtime.
            let rc = unsafe { libc::mkdirat(self.fd.as_raw_fd(), cname.as_ptr(), 0o700) };
            if rc == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        #[cfg(windows)]
        {
            let handle = nt_open_relative(
                self.handle.as_raw_handle(),
                name,
                DIR_ACCESS,
                FILE_CREATE,
                FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            )?;
            // SAFETY: the just-created directory handle has no other owner;
            // dropping it immediately closes the handle (the directory
            // itself stays on disk).
            drop(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) });
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            std::fs::create_dir(self.display.join(name))
        }
    }

    /// Open an existing child (file or directory) without following a link.
    /// The returned handle pins the object, so metadata and reads taken
    /// through it cannot be redirected by a later path swap.
    pub(crate) fn open_existing(&self, name: &OsStr) -> io::Result<std::fs::File> {
        #[cfg(windows)]
        let child_display = self.display.join(name);
        #[cfg(unix)]
        {
            let cname = to_cstring(name)?;
            // SAFETY: `cname` is NUL-terminated; O_NOFOLLOW refuses links.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    cname.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh descriptor with no other owner.
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }
        #[cfg(windows)]
        {
            match nt_open_relative(
                self.handle.as_raw_handle(),
                name,
                GENERIC_READ | SYNCHRONIZE,
                FILE_OPEN,
                FILE_OPEN_REPARSE_POINT,
            ) {
                Ok(handle) => {
                    check_not_reparse(handle, &child_display)?;
                    // SAFETY: `handle` is a fresh kernel handle.
                    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
                }
                // A directory needs the FILE_DIRECTORY_FILE option; retry so
                // mutation bookkeeping can measure a directory target.
                Err(e) if e.kind() == io::ErrorKind::IsADirectory => {
                    let handle = nt_open_relative(
                        self.handle.as_raw_handle(),
                        name,
                        GENERIC_READ | SYNCHRONIZE,
                        FILE_OPEN,
                        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
                    )?;
                    check_not_reparse(handle, &child_display)?;
                    // SAFETY: `handle` is a fresh kernel handle.
                    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
                }
                Err(e) => Err(e),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            std::fs::File::open(self.display.join(name))
        }
    }

    /// Create a brand-new file under this handle. Exclusive creation means
    /// a pre-planted link at the name makes the operation fail instead of
    /// following it.
    pub(crate) fn create_new_file(&self, name: &OsStr) -> io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            let cname = to_cstring(name)?;
            // SAFETY: `cname` is NUL-terminated; O_EXCL + O_NOFOLLOW refuse
            // existing entries and links. The new file is private.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    cname.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh descriptor with no other owner.
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }
        #[cfg(windows)]
        {
            let handle = nt_open_relative(
                self.handle.as_raw_handle(),
                name,
                GENERIC_WRITE | DELETE_ACCESS | SYNCHRONIZE,
                FILE_CREATE,
                FILE_OPEN_REPARSE_POINT,
            )?;
            // SAFETY: `handle` is a fresh kernel handle with no other owner.
            Ok(unsafe { std::fs::File::from_raw_handle(handle) })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            options.open(self.display.join(name))
        }
    }

    /// Open or create one regular child file for a trusted runtime journal.
    /// The file is resolved relative to this pinned directory and links are
    /// never followed. Callers still own serialization, locking and sync.
    pub(crate) fn open_or_create_regular_file(&self, name: &OsStr) -> io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            let cname = to_cstring(name)?;
            // SAFETY: `cname` is NUL-terminated. O_NOFOLLOW rejects a link,
            // and opening a directory read/write fails instead of accepting
            // it as the authority journal.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    cname.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh descriptor with no other owner.
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }
        #[cfg(windows)]
        {
            let handle = nt_open_relative(
                self.handle.as_raw_handle(),
                name,
                GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE,
                FILE_OPEN_IF,
                FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            )?;
            check_not_reparse(handle, &self.display.join(name))?;
            // SAFETY: `handle` is a fresh kernel handle with no other owner.
            Ok(unsafe { std::fs::File::from_raw_handle(handle) })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).read(true).write(true);
            options.open(self.display.join(name))
        }
    }

    /// Atomically replace `to_name` with the file opened as `from_file`
    /// (`from_name` is its name under this directory). Relative to the
    /// handle, so a swap of either name cannot redirect the operation.
    pub(crate) fn replace_file(
        &self,
        from_file: &std::fs::File,
        from_name: &OsStr,
        to_name: &OsStr,
    ) -> io::Result<()> {
        #[cfg(unix)]
        {
            // Unix renames by name under the pinned directory; the handle
            // is only needed for the Windows FileRenameInformation path.
            let _ = from_file;
            let from_c = to_cstring(from_name)?;
            let to_c = to_cstring(to_name)?;
            // SAFETY: both names are NUL-terminated; renameat resolves them
            // relative to this pinned directory handle.
            let rc = unsafe {
                libc::renameat(
                    self.fd.as_raw_fd(),
                    from_c.as_ptr(),
                    self.fd.as_raw_fd(),
                    to_c.as_ptr(),
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        #[cfg(windows)]
        {
            let _ = from_name;
            let to_wide: Vec<u16> = to_name.encode_wide().collect();
            let name_bytes = (to_wide.len() * 2) as u32;
            // `FileName` is a trailing flexible array that starts at its own
            // offset (the struct also carries tail padding); copying to
            // `size_of` would leave the kernel reading an empty name.
            //
            // The rename goes through NtSetInformationFile, not the
            // SetFileInformationByHandle wrapper: the wrapper's FileRenameInfo
            // class rejects a nonzero RootDirectory with ERROR_INVALID_PARAMETER
            // on this Windows generation, while the native call honors the
            // pinned directory handle. RootDirectory pins this directory so
            // the rename cannot be redirected by a path swap.
            let name_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
            let total = name_offset + name_bytes as usize;
            let mut buf = vec![0u8; total];
            // SAFETY: `info` points into `buf` which is large enough for the
            // struct; the name is copied at the FileName offset and
            // `FileNameLength` tells the kernel how many bytes follow.
            // RootDirectory pins this directory so the rename cannot be
            // redirected by a path swap.
            unsafe {
                let info = buf.as_mut_ptr() as *mut FILE_RENAME_INFO;
                (*info).Anonymous.ReplaceIfExists = 1;
                (*info).RootDirectory = self.handle.as_raw_handle();
                (*info).FileNameLength = name_bytes;
                std::ptr::copy_nonoverlapping(
                    to_wide.as_ptr() as *const u8,
                    buf.as_mut_ptr().add(name_offset),
                    name_bytes as usize,
                );
                let mut io_status: IO_STATUS_BLOCK = std::mem::zeroed();
                let status = NtSetInformationFile(
                    from_file.as_raw_handle(),
                    &mut io_status,
                    buf.as_ptr() as *const core::ffi::c_void,
                    buf.len() as u32,
                    FileRenameInformation,
                );
                if status < 0 {
                    return Err(ntstatus_to_io(status));
                }
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = from_file;
            std::fs::rename(self.display.join(from_name), self.display.join(to_name))
        }
    }

    /// Remove a file under this handle. Used to clean up staged temp files;
    /// their names carry a fresh UUID, so a path-based remove cannot be
    /// aimed at an attacker-chosen target.
    pub(crate) fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let cname = to_cstring(name)?;
            // SAFETY: `cname` is NUL-terminated; unlinkat is relative to
            // this pinned directory handle.
            let rc = unsafe { libc::unlinkat(self.fd.as_raw_fd(), cname.as_ptr(), 0) };
            if rc == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_file(self.display.join(name))
        }
    }
}

/// An open file handle pinned by `ConfinedDir::open_existing` /
/// `create_new_file`. Metadata and reads through this handle refer to the
/// opened object even if its path is swapped afterwards.
pub struct ConfinedFile {
    file: std::fs::File,
    display: PathBuf,
}

impl ConfinedFile {
    pub(crate) fn new(file: std::fs::File, display: PathBuf) -> Self {
        Self { file, display }
    }

    /// The lexical path the file was opened through (display only).
    pub fn display(&self) -> &Path {
        &self.display
    }

    /// Metadata of the opened object (through the pinned handle, not the
    /// path — a concurrent swap cannot change what this reports).
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }

    /// Take the pinned handle as a Tokio file for async reads.
    pub fn into_tokio(self) -> tokio::fs::File {
        tokio::fs::File::from_std(self.file)
    }

    /// Take the pinned handle back as a plain file.
    pub fn into_std(self) -> std::fs::File {
        self.file
    }
}

#[cfg(unix)]
fn open_dir_fd(parent: libc::c_int, name: &OsStr) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let cname = to_cstring(name)?;
    // SAFETY: `cname` is NUL-terminated; O_NOFOLLOW refuses a link at this
    // component and O_DIRECTORY refuses anything that is not a directory.
    let fd = unsafe {
        libc::openat(
            parent,
            cname.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor with no other owner.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn to_cstring(name: &OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path component contains a NUL byte",
        )
    })
}

#[cfg(windows)]
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Directory handles must support traversal (relative opens) *and* adding
/// entries (the atomic replace renames into the directory through the
/// handle), so read and write access are combined.
#[cfg(windows)]
const DIR_ACCESS: u32 = GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE;

/// `DELETE` is required on a file before `FileRenameInfo` can rename it.
#[cfg(windows)]
const DELETE_ACCESS: u32 = 0x0001_0000;

#[cfg(windows)]
fn open_root_handle(path: &Path) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated wide path; OPEN_REPARSE_POINT
    // means a swapped root link is opened, then rejected by the tag check.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DIR_ACCESS,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    check_not_reparse(handle, path)?;
    // SAFETY: `handle` is a fresh kernel handle with no other owner.
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) })
}

/// Relative open through a parent directory handle. `FILE_OPEN_REPARSE_POINT`
/// opens a link itself instead of following it, so the caller can reject it.
#[cfg(windows)]
fn nt_open_relative(
    parent: std::os::windows::io::RawHandle,
    name: &OsStr,
    access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<HANDLE> {
    use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;

    let wide: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
    let unicode = UNICODE_STRING {
        Length: ((wide.len() - 1) * 2) as u16,
        MaximumLength: (wide.len() * 2) as u16,
        Buffer: wide.as_ptr() as *mut u16,
    };
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE as u32,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    let mut io_status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: `object_attributes` (and the UNICODE_STRING it points to)
    // stay alive for the call; `handle` and `io_status` receive kernel
    // output. The parent handle pins the directory the name is resolved
    // against, which is the whole point of this module. BACKUP_INTENT lets
    // traversal succeed for callers without special privileges.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &object_attributes,
            &mut io_status,
            std::ptr::null(),
            0,
            SHARE_ALL,
            disposition,
            options | FILE_OPEN_FOR_BACKUP_INTENT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status >= 0 {
        Ok(handle)
    } else {
        Err(ntstatus_to_io(status))
    }
}

#[cfg(windows)]
fn check_not_reparse(handle: HANDLE, display: &Path) -> io::Result<()> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: `info` points at a FILE_ATTRIBUTE_TAG_INFO-sized buffer the
    // kernel fills in; FileAttributeTagInfo is a fixed-size query.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            &mut info as *mut FILE_ATTRIBUTE_TAG_INFO as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.ReparseTag != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is a reparse point (tag {:#010x}); reparse substitution is rejected at open time",
                display.display(),
                info.ReparseTag
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn ntstatus_to_io(status: i32) -> io::Error {
    const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
    const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
    const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
    const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
    match status {
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND => io::Error::new(
            io::ErrorKind::NotFound,
            format!("not found (NTSTATUS {status:#010x})"),
        ),
        STATUS_OBJECT_NAME_COLLISION => io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("NTSTATUS {status:#010x}"),
        ),
        STATUS_FILE_IS_A_DIRECTORY => io::Error::new(
            io::ErrorKind::IsADirectory,
            format!("NTSTATUS {status:#010x}"),
        ),
        other => io::Error::other(format!("NTSTATUS {other:#010x}")),
    }
}

#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
