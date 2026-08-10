//! The exclusive lock one process takes on a data directory.
//!
//! Two `ferrite-server` instances pointed at the same `FERRITE_DATA` is a
//! configuration mistake with no upper bound on the damage: both replay the
//! journal at startup, both cache pages, and each writes back a version of
//! a page the other has never seen. Nothing in the page format, the
//! checksums or the journal defends against it — they all assume a single
//! writer — so the defence has to be refusing to start.
//!
//! The lock is held by the operating system on an open file handle rather
//! than by the presence of a file. That distinction is the whole design: a
//! lock file that is *created* on start and *deleted* on exit does not
//! survive a crash, and a stale one left behind by a killed process would
//! stop the very restart that recovery exists for. A handle-based lock is
//! released by the kernel when the process dies, however it dies.

use std::fs::{File, OpenOptions};
use std::path::Path;

use ferrite_common::FerriteError;

pub const LOCK_FILE: &str = "ferrite.lock";

/// An exclusive claim on a data directory, released when dropped or when
/// the process exits.
#[derive(Debug)]
pub struct DirectoryLock {
    _file: File,
}

impl DirectoryLock {
    /// Takes the lock, or reports who has it.
    pub fn acquire(dir: &Path) -> Result<Self, FerriteError> {
        let path = dir.join(LOCK_FILE);
        let file = open_exclusive(&path).map_err(|err| {
            FerriteError::Storage(format!(
                "another process is already using the data directory {}: {err}. \
                 Two servers writing the same files would corrupt them, so this one \
                 refuses to start.",
                dir.display()
            ))
        })?;
        Ok(Self { _file: file })
    }
}

/// Opens the lock file in a way that no second opener can repeat.
///
/// Windows enforces this at open time through the share mode, so the
/// `OpenOptions` call itself is the lock. Unix has no equivalent, so the
/// file opens normally and `flock` follows.
#[cfg(windows)]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    // share_mode(0): no other handle to this file may be opened at all,
    // by this process or any other, until this one is closed.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

#[cfg(unix)]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // `LOCK_NB` so a second instance is refused immediately rather than
    // waiting for the first one to exit, which is what an operator wants
    // to be told about. The lock lives on the open file description, so
    // the kernel drops it when the process does.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// No lock on a platform that is neither, which is honest rather than a
/// false claim of protection.
#[cfg(not(any(windows, unix)))]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferrite-lock-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn a_second_holder_is_refused_and_a_released_one_is_not() {
        let dir = scratch("twice");
        let first = DirectoryLock::acquire(&dir).expect("the first holder");
        assert!(
            DirectoryLock::acquire(&dir).is_err(),
            "two holders of one data directory is the corruption this prevents"
        );
        drop(first);
        DirectoryLock::acquire(&dir).expect("the lock is released with its holder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file left behind must not be a lock in itself: a crash leaves
    /// it there, and a restart has to succeed.
    #[test]
    fn a_leftover_lock_file_does_not_block_a_restart() {
        let dir = scratch("leftover");
        drop(DirectoryLock::acquire(&dir).expect("first"));
        assert!(dir.join(LOCK_FILE).exists(), "the file outlives the lock");
        DirectoryLock::acquire(&dir).expect("a leftover file must not stop a restart");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
