//! Cross-process locking for a workbook.
//!
//! Two concurrent `name-match` runs on the same workbook would both read the
//! pre-change file, each write its own result, and the later write would
//! silently discard the earlier one. The lock serializes that read-modify-write
//! span. It uses the standard library's file locking, so there is no extra
//! dependency, and the kernel releases the lock if the process dies.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::{CliError, ErrorKind};
use crate::resolved_path;

/// A held lock guarding one workbook.
///
/// Released when dropped, which covers normal returns, error paths and panics.
pub struct WorkbookLock {
    file: File,
    /// Path of the sidecar lock file, kept for error messages.
    path: PathBuf,
    /// How long the caller had to wait before the lock was granted.
    waited: Duration,
}

impl WorkbookLock {
    /// Acquire the lock for `workbook`, waiting if another process holds it.
    ///
    /// A non-blocking attempt runs first so the common uncontended case costs
    /// nothing. When that fails the call blocks, announcing the wait on stderr
    /// so a caller does not mistake it for a hang. No timeout is applied: the
    /// kernel releases the lock as soon as the holder exits, even if killed.
    pub fn acquire(workbook: &Path) -> Result<Self, CliError> {
        let path = lock_path(workbook);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| CliError {
                kind: ErrorKind::Io,
                message: format!("无法创建锁文件 {}：{error}", path.display()),
            })?;

        let started = Instant::now();
        if file.try_lock().is_err() {
            eprintln!(
                "name-match: 检测到另一进程正在处理 {}，等待中…",
                workbook.display()
            );
            file.lock().map_err(|error| CliError {
                kind: ErrorKind::Io,
                message: format!("无法锁定 {}：{error}", path.display()),
            })?;
        }

        Ok(Self {
            file,
            path,
            waited: started.elapsed(),
        })
    }

    /// How long the caller waited for this lock.
    pub fn waited(&self) -> Duration {
        self.waited
    }

    /// Path of the sidecar lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkbookLock {
    fn drop(&mut self) {
        // The kernel releases the lock when the handle closes; releasing
        // explicitly keeps the intent obvious and ignores a failure, because
        // there is nothing useful to do about it during a drop.
        let _ = self.file.unlock();
    }
}

/// Sidecar lock path for a workbook: `统计.xlsx` -> `.统计.xlsx.lock`.
///
/// The workbook file itself is never locked: `Workbook::save` replaces it with
/// a rename, which would swap out the locked inode and let a second process
/// lock the new file while the first still holds the old one.
///
/// The path is resolved to an absolute form first so `统计.xlsx` and
/// `./统计.xlsx` map to the same lock.
pub fn lock_path(workbook: &Path) -> PathBuf {
    let resolved = resolved_path(workbook);
    let name = workbook
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workbook".to_string());
    resolved.with_file_name(format!(".{name}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    #[test]
    fn lock_path_is_a_hidden_sibling_of_the_workbook() {
        let path = lock_path(Path::new("统计.xlsx"));
        assert!(path.is_absolute(), "the lock path must be absolute");
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            ".统计.xlsx.lock"
        );
    }

    #[test]
    fn relative_and_absolute_workbook_paths_share_one_lock() {
        let dir = temp_dir();
        let absolute = dir.path().join("统计.xlsx");
        std::fs::write(&absolute, b"x").unwrap();

        let from_absolute = lock_path(&absolute);
        let from_relative = lock_path(Path::new("统计.xlsx"));
        assert_eq!(
            from_relative.file_name(),
            from_absolute.file_name(),
            "both spellings must produce the same sidecar name"
        );
        assert!(from_relative.is_absolute());
    }

    #[test]
    fn a_held_lock_blocks_a_second_attempt_then_frees_on_drop() {
        let dir = temp_dir();
        let workbook = dir.path().join("统计.xlsx");
        std::fs::write(&workbook, b"x").unwrap();

        let first = WorkbookLock::acquire(&workbook).expect("first acquire");
        assert!(
            first.waited() < Duration::from_millis(50),
            "an uncontended acquire should return immediately, waited {:?}",
            first.waited()
        );
        assert!(first.path().exists());

        let sidecar = OpenOptions::new()
            .read(true)
            .write(true)
            .open(first.path())
            .expect("open sidecar");
        assert!(
            sidecar.try_lock().is_err(),
            "the lock must still be held by the first guard"
        );

        drop(first);

        assert!(sidecar.try_lock().is_ok(), "lock should be free after drop");
    }

    #[test]
    fn reacquiring_after_release_succeeds() {
        let dir = temp_dir();
        let workbook = dir.path().join("统计.xlsx");
        std::fs::write(&workbook, b"x").unwrap();

        for _ in 0..3 {
            let guard = WorkbookLock::acquire(&workbook).expect("acquire");
            drop(guard);
        }
    }

    #[test]
    fn different_workbooks_use_different_locks() {
        let dir = temp_dir();
        let first = dir.path().join("a.xlsx");
        let second = dir.path().join("b.xlsx");
        std::fs::write(&first, b"x").unwrap();
        std::fs::write(&second, b"x").unwrap();

        assert_ne!(lock_path(&first), lock_path(&second));

        let held = WorkbookLock::acquire(&first).expect("hold first");
        let other = WorkbookLock::acquire(&second).expect("second is independent");
        assert!(
            other.waited() < Duration::from_millis(50),
            "an unrelated workbook must not wait, waited {:?}",
            other.waited()
        );
        drop(held);
    }

    #[test]
    fn a_missing_directory_reports_an_io_error_naming_the_lock_file() {
        let dir = temp_dir();
        let missing = dir.path().join("不存在").join("统计.xlsx");

        let error = WorkbookLock::acquire(&missing).err().expect("should fail");
        assert_eq!(error.kind, ErrorKind::Io);
        assert!(
            error.message.contains(".lock"),
            "the message should name the lock file: {}",
            error.message
        );
    }
}
