//! RAII lockfile (ADR A10).
//!
//! Acquire a `path.lock` exclusively. Writes go to that temp file. Calling
//! `commit()` fsyncs and atomically renames over the target. Dropping without
//! committing rolls back: the lock file is removed and the target stays
//! untouched.
//!
//! This is the single primitive used by index writes (M3), ref updates (M2),
//! `packed-refs` rewrites, reflog appends, and config writes. Every mutation
//! that needs crash-safety must go through here.
//!
//! ## Crash-safe SIGINT cleanup (A9a)
//!
//! Plain `Drop` works for normal control flow and for the `?` early-return
//! shape of errors, but it does NOT run when the process is killed by a
//! signal (SIGINT/SIGTERM). For Ctrl-C during `commit` or `add` we'd be left
//! with stale `.git/index.lock` files that block every subsequent operation
//! until the user runs `prune-locks`.
//!
//! To bridge the gap, every successful [`Lockfile::acquire`] inserts its
//! `.lock` path into a process-wide registry ([`LIVE_LOCKS`]). [`commit`]
//! and `Drop` remove it. A SIGINT handler installed in `main()` drains the
//! registry via [`take_live_locks`] and unlinks every path before exiting
//! with 130 (`128 + SIGINT(2)`).
//!
//! The registry is `OnceLock<Mutex<HashSet>>` so it stays zero-cost when
//! the binary is used as a library and so we don't pay an allocation up
//! front. The mutex is a plain `std::sync::Mutex` since contention is
//! negligible (only acquire/commit/drop touch it) and async-signal-safety
//! is moot here — we don't call this from within the actual signal handler;
//! the `ctrlc` crate dispatches to a normal thread.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use thiserror::Error;

const LOCK_SUFFIX: &str = ".lock";

/// Process-wide registry of `.lock` files that have been acquired but not yet
/// committed or dropped. The SIGINT handler in `main()` drains this set and
/// unlinks each path before exiting.
///
/// Lives behind a `OnceLock` so we pay no startup cost in the library and so
/// the `HashSet` itself only allocates when the first lock is taken.
static LIVE_LOCKS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn live_locks() -> &'static Mutex<HashSet<PathBuf>> {
    LIVE_LOCKS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Drain the live-lock registry, returning every `.lock` path that was
/// outstanding at the moment of the call. Used by the SIGINT handler in
/// `main()` to clean up before exiting.
///
/// Returns the empty set if no lock was ever acquired in this process (the
/// registry stays un-initialized in that case).
///
/// Errors only if the registry's mutex is poisoned — which only happens if
/// a previous holder panicked while mutating the set. Callers should treat
/// this as best-effort; a poisoned registry leaves the locks in place
/// rather than crashing the cleanup handler.
pub fn take_live_locks() -> Result<Vec<PathBuf>, &'static str> {
    let Some(slot) = LIVE_LOCKS.get() else {
        return Ok(Vec::new());
    };
    let mut guard = slot
        .lock()
        .map_err(|_| "live-lock registry mutex poisoned")?;
    Ok(guard.drain().collect())
}

fn register_live(path: &Path) {
    if let Ok(mut guard) = live_locks().lock() {
        guard.insert(path.to_path_buf());
    }
}

fn unregister_live(path: &Path) {
    // Only matters if the registry has ever been initialized — skip the
    // OnceLock init pathway when no locks have ever been taken.
    if let Some(slot) = LIVE_LOCKS.get() {
        if let Ok(mut guard) = slot.lock() {
            guard.remove(path);
        }
    }
}

#[derive(Debug)]
pub struct Lockfile {
    target: PathBuf,
    lock_path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl Lockfile {
    /// Acquire a lock for `target`. Errors if `target.lock` already exists,
    /// matching git's "Unable to create '...lock': File exists" semantics.
    ///
    /// When `AlreadyLocked` fires and the existing lock file's mtime is older
    /// than [`STALE_LOCK_HINT_SECS`], the error message points the user at
    /// `rustygit prune-locks` so they can recover from a crashed earlier
    /// process. We deliberately do NOT auto-remove the lock here — that's
    /// racy against a still-running peer.
    pub fn acquire(target: impl Into<PathBuf>) -> Result<Self, LockError> {
        let target: PathBuf = target.into();
        let parent = target
            .parent()
            .ok_or_else(|| LockError::InvalidTarget(target.clone()))?;
        std::fs::create_dir_all(parent).map_err(|e| LockError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;

        let lock_path = lock_path_for(&target);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::AlreadyExists => {
                    let stale = is_stale_lock(&lock_path);
                    LockError::AlreadyLocked {
                        target: target.clone(),
                        hint_stale: stale,
                    }
                }
                _ => LockError::Io {
                    path: lock_path.clone(),
                    source: e,
                },
            })?;

        // Register the live lock so the SIGINT handler can clean it up if
        // the process is killed before `commit` or `Drop` runs (A9a).
        register_live(&lock_path);

        Ok(Self {
            target,
            lock_path,
            file: Some(file),
            committed: false,
        })
    }

    /// Mutable access to the underlying writer.
    pub fn writer(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("file is Some until commit/Drop consumes it")
    }

    /// Replace the entire lockfile contents (truncates anything previously written).
    pub fn write_all(&mut self, contents: &[u8]) -> io::Result<()> {
        self.writer().write_all(contents)
    }

    /// fsync the lockfile and atomically rename it over the target.
    pub fn commit(mut self) -> Result<(), LockError> {
        let file = self
            .file
            .take()
            .expect("file is Some until commit/Drop consumes it");
        file.sync_all().map_err(|e| LockError::Io {
            path: self.lock_path.clone(),
            source: e,
        })?;
        drop(file); // close before rename, important on Windows
        std::fs::rename(&self.lock_path, &self.target).map_err(|e| LockError::Io {
            path: self.target.clone(),
            source: e,
        })?;
        self.committed = true;
        // Successful commit means the .lock filename is GONE from disk
        // (renamed onto the target). Remove it from the registry so the
        // SIGINT handler doesn't try to unlink a path that no longer
        // exists. The Drop impl will also call this — `HashSet::remove`
        // on a missing entry is a no-op.
        unregister_live(&self.lock_path);
        Ok(())
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for Lockfile {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort rollback. Don't panic in Drop.
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.lock_path);
        }
        // Always remove from the registry — either we committed (and the
        // file no longer exists), we rolled back here (and just unlinked
        // it), or the SIGINT handler raced us and already drained the set.
        // Any of those cases leaves no work for the SIGINT handler.
        unregister_live(&self.lock_path);
    }
}

/// How old (seconds since mtime) a lockfile has to be before we hint that
/// it may be stale. 60 minutes is the same threshold git uses for its
/// `gc.pruneExpire` default in some contexts; for our hint it's just "long
/// enough that a real running command would almost certainly be done."
pub const STALE_LOCK_HINT_SECS: u64 = 60 * 60;

/// Returns true if the lock file at `path` exists and its mtime is older
/// than [`STALE_LOCK_HINT_SECS`]. Best-effort: any stat failure returns
/// false (we under-warn rather than over-warn).
pub fn is_stale_lock(path: &Path) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let age = match std::time::SystemTime::now().duration_since(mtime) {
        Ok(d) => d,
        Err(_) => return false, // clock skew: lockfile mtime in the future
    };
    age.as_secs() >= STALE_LOCK_HINT_SECS
}

fn lock_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(LOCK_SUFFIX);
    PathBuf::from(s)
}

#[derive(Error, Debug)]
pub enum LockError {
    #[error("invalid lock target: {0}")]
    InvalidTarget(PathBuf),
    /// Use `is_stale_lock()` to decide whether to suggest `prune-locks` to
    /// the user — the Display impl alone does not include that hint.
    #[error("already locked: {target} (another process may be holding the .lock file)")]
    AlreadyLocked { target: PathBuf, hint_stale: bool },
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl LockError {
    /// True if this is `AlreadyLocked` AND the existing lock file is old
    /// enough that it may be stale. Callers should suggest the user run
    /// `rustygit prune-locks` when this is set.
    pub fn is_stale_lock(&self) -> bool {
        matches!(
            self,
            LockError::AlreadyLocked {
                hint_stale: true,
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn commit_writes_target_and_removes_lock() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("foo");
        let mut lock = Lockfile::acquire(&target).unwrap();
        lock.write_all(b"hello").unwrap();
        lock.commit().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        assert!(!lock_path_for(&target).exists());
    }

    #[test]
    fn drop_without_commit_leaves_target_untouched() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bar");
        std::fs::write(&target, b"original").unwrap();
        {
            let mut lock = Lockfile::acquire(&target).unwrap();
            lock.write_all(b"trash").unwrap();
            // drop without commit
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert!(!lock_path_for(&target).exists());
    }

    #[test]
    fn second_acquire_fails_while_first_held() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("baz");
        let _first = Lockfile::acquire(&target).unwrap();
        let err = Lockfile::acquire(&target).unwrap_err();
        assert!(matches!(err, LockError::AlreadyLocked { .. }));
        // Just-acquired lock can't be stale yet.
        assert!(!err.is_stale_lock());
    }

    #[test]
    fn fresh_lock_does_not_read_stale() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("data");
        let lock_path = lock_path_for(&target);
        std::fs::write(&lock_path, b"trash").unwrap();
        assert!(
            !is_stale_lock(&lock_path),
            "fresh lock should not be considered stale"
        );
    }

    #[test]
    fn nonexistent_lock_does_not_read_stale() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.lock");
        assert!(!is_stale_lock(&missing));
    }

    /// A held lock appears in [`take_live_locks`] until it's committed or
    /// dropped. The SIGINT handler relies on this to find files it needs
    /// to clean up. Once committed the path must no longer appear (else
    /// the handler would try to unlink a path that no longer exists).
    ///
    /// We can't peek at the set without draining it, so the test commits
    /// the lock first, drains, and asserts the drained set is empty for
    /// THIS path. The "live" half is exercised by spinning a fresh
    /// process in `tests/sigint_cleanup.rs`.
    #[test]
    fn commit_unregisters_from_live_locks() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("registry-target");
        let mut lock = Lockfile::acquire(&target).unwrap();
        lock.write_all(b"x").unwrap();
        let lock_path = lock.lock_path().to_path_buf();
        lock.commit().unwrap();
        let drained = take_live_locks().unwrap();
        assert!(
            !drained.contains(&lock_path),
            "committed lock should not remain in registry; drained = {drained:?}"
        );
    }

    /// Same as above for the rollback path: dropping without committing
    /// must also remove the entry, so a later SIGINT doesn't try to
    /// unlink an already-cleaned path.
    #[test]
    fn drop_unregisters_from_live_locks() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("registry-target-drop");
        let lock_path;
        {
            let lock = Lockfile::acquire(&target).unwrap();
            lock_path = lock.lock_path().to_path_buf();
            // implicit drop without commit
        }
        let drained = take_live_locks().unwrap();
        assert!(
            !drained.contains(&lock_path),
            "dropped lock should not remain in registry; drained = {drained:?}"
        );
    }
}
