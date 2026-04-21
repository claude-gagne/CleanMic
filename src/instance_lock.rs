//! Single-instance lock.
//!
//! A running CleanMic owns a virtual PipeWire source named `CleanMic` plus
//! three helper streams (capture, output, monitor) and a set of explicit
//! `pw-link` edges gluing them together. A second process launching in
//! parallel would try to create its own `CleanMic` node and re-draw the same
//! link graph — the two instances would fight over names, `pw-link` targets,
//! and the shared config file. We prevent that here with an advisory
//! `flock(2)` on a per-user lock file: the first process acquires it for
//! the duration of the program; any subsequent launch finds the lock held
//! and exits cleanly.
//!
//! Advisory locks are released by the kernel when the holding process dies,
//! so a crashed CleanMic does not leave the next launch blocked.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// RAII guard that holds the lock for as long as it lives.
#[derive(Debug)]
pub struct Guard {
    // `None` for the dummy guard returned when the lock infrastructure is
    // unavailable (e.g. unwritable runtime dir). Keeping the file handle
    // alive keeps the flock held; dropping the File releases it.
    _file: Option<File>,
}

impl Guard {
    /// Sentinel guard that holds no lock. Used when we couldn't create the
    /// lock file at all and chose to let the process proceed anyway.
    pub fn dummy() -> Self {
        Self { _file: None }
    }
}

/// Errors from [`acquire`].
#[derive(Debug)]
pub enum Error {
    /// Another process already holds the lock. Carries the lock path for
    /// diagnostics.
    AlreadyRunning(PathBuf),
    /// The lock file could not be created or opened at all — callers may
    /// choose to proceed without a lock in this case.
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Acquire the single-instance lock.
///
/// Returns [`Guard`] on success — keep it alive for the lifetime of the
/// process. Returns [`Error::AlreadyRunning`] if another CleanMic already
/// holds the lock.
pub fn acquire() -> Result<Guard, Error> {
    acquire_at(lock_path())
}

/// Acquire an advisory flock on the given path. Exposed for tests so they
/// don't need to mutate `XDG_RUNTIME_DIR` (which races under the default
/// parallel test runner).
fn acquire_at(path: PathBuf) -> Result<Guard, Error> {
    // Ensure the parent directory exists (XDG runtime dir always exists,
    // but our fallback under /tmp is pre-made by the kernel; /home/<user>/.cache
    // may not exist on a very fresh account).
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    // LOCK_EX | LOCK_NB: exclusive lock, fail immediately if held.
    // SAFETY: libc::flock takes a valid file descriptor; `file` is open.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Guard { _file: Some(file) });
    }

    let err = io::Error::last_os_error();
    // EWOULDBLOCK means the lock is held by another process — that's the
    // single-instance case we care about. Anything else (EBADF, ENOLCK …)
    // is treated as an IO error so callers can log and decide.
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Err(Error::AlreadyRunning(path))
    } else {
        Err(Error::Io(err))
    }
}

/// Resolve the lock-file path.
///
/// Preferred location is `$XDG_RUNTIME_DIR/cleanmic.lock` — a user-scoped
/// tmpfs directory cleared on logout. Falls back to `/tmp/cleanmic-<uid>.lock`
/// when `XDG_RUNTIME_DIR` is unset (SSH sessions, some DEs).
fn lock_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = Path::new(&dir).to_path_buf();
        if !p.as_os_str().is_empty() {
            return p.join("cleanmic.lock");
        }
    }
    // SAFETY: getuid() is always safe.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/cleanmic-{uid}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dummy_guard_drops_cleanly() {
        let g = Guard::dummy();
        drop(g);
    }

    #[test]
    fn second_acquire_reports_already_running() {
        // Uses a tmpdir via acquire_at (no env mutation) so this test is
        // safe under cargo's parallel runner — env-var tricks would race
        // with any other test reading XDG_RUNTIME_DIR.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cleanmic.lock");

        let first = acquire_at(path.clone()).expect("first acquire should succeed");
        match acquire_at(path.clone()) {
            Err(Error::AlreadyRunning(_)) => {}
            other => panic!("second acquire should report AlreadyRunning, got {other:?}"),
        }
        drop(first);
        // After releasing, a fresh acquire on the same path should succeed again.
        let _third = acquire_at(path).expect("acquire after release should succeed");
    }
}
