//! The Trusted Core's filesystem primitive: one atomic-replace operation
//! with platform-specific implementations.
//!
//! `std::fs::rename` is not a portable "atomically replace the destination"
//! abstraction — its overwrite semantics differ across operating systems,
//! and the obvious fallback (remove the target, then rename) destroys
//! atomicity: a crash between the two steps leaves the target missing.
//! Every journaled mutation commit goes through `atomic_replace`, so the
//! semantics are defined in exactly one place.
//!
//! - Unix: `rename(2)` replaces the destination atomically.
//! - Windows: `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` (the kernel
//!   replaces the destination; `MOVEFILE_WRITE_THROUGH` asks for the move
//!   to be flushed to disk before returning).

use std::io;
use std::path::Path;

/// Atomically replace `dst` with the file at `src`. Both paths must be
/// absolute, workspace-confined paths (callers resolve them first).
pub async fn atomic_replace(src: &Path, dst: &Path) -> io::Result<()> {
    replace_sync(src, dst)
}

#[cfg(unix)]
fn replace_sync(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::rename(src, dst)
}

#[cfg(windows)]
fn replace_sync(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both pointers reference nul-terminated wide buffers that stay
    // alive for the call; the flags never attempt a directory move.
    let ok = unsafe {
        MoveFileExW(
            src_wide.as_ptr(),
            dst_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_sync(src: &Path, dst: &Path) -> io::Result<()> {
    // Last resort for targets without a native replace primitive: try the
    // rename first (it replaces on most systems), and only on an
    // AlreadyExists error fall back to remove + rename. Not atomic — kept
    // so other platforms still compile and work.
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(dst)?;
            std::fs::rename(src, dst)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replace_overwrites_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("stage.tmp");
        let dst = dir.path().join("target.txt");
        std::fs::write(&src, b"new content").unwrap();
        std::fs::write(&dst, b"old content").unwrap();

        atomic_replace(&src, &dst).await.unwrap();
        let content = std::fs::read_to_string(&dst).unwrap();
        assert_eq!(content, "new content", "the destination is replaced");
        assert!(!src.exists(), "the staged file is consumed by the replace");
    }

    #[tokio::test]
    async fn replace_creates_a_missing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("stage.tmp");
        let dst = dir.path().join("fresh.txt");
        std::fs::write(&src, b"hello").unwrap();

        atomic_replace(&src, &dst).await.unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello");
    }
}
