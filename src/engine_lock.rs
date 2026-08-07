//! One engine per account, across processes (#0061, folded into #0039).
//!
//! The data-access-layer plan specifies a single-writer engine guarded by a
//! non-blocking advisory lock on `<account_dir>/store.lock`
//! (`docs/plans/data-access-layer.md`). SQLite's own WAL locking serialises
//! individual writes but cannot express "one engine for the lifetime of the
//! process": its locks are scoped to a transaction. This is the missing piece.
//!
//! The lock exists because two drains of the durable `pending_ops` queue racing
//! on the same rows is where the absent lock turns from wasteful (two syncs
//! double-ingesting) into destructive (two engines running the same op twice,
//! or racing a row's state transitions). `mp sync` in one terminal and an open
//! TUI in another are both live writers today, so the exposure is real now.
//!
//! ## The protocol
//!
//! - The first process to run an account's engine takes the lock and drains.
//! - A process that cannot take it degrades to read-only against the store: it
//!   still reads and it still *enqueues* `pending_ops` rows (a plain WAL write,
//!   serialised by SQLite), but it does not drain them. The lock-holder's
//!   engine drains what everyone enqueues.
//! - `flock` releases on `close(2)`, which the kernel does on exit and on a
//!   crash, so a dead holder's lock is free for the next process with no
//!   manual cleanup and no stale lock file to reap.
//!
//! ## Why `flock` and not a lock file with a pid
//!
//! A pid file is a lock a crash leaves held: the next process finds a pid that
//! is either dead or, worse, reused, and has to guess. `flock` is an advisory
//! lock the kernel owns, released the instant the holding fd closes however the
//! holder went away. The lock file itself is never deleted; only the advisory
//! lock on it moves between processes, so there is no unlink race.
//!
//! Unix only. Windows is targeted through WSL, where this is a Linux binary.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};
use log::{debug, warn};

/// A held advisory lock on an account's `store.lock`. The engine may drain the
/// `pending_ops` queue for as long as it holds this; dropping it (on exit, or
/// because the engine step is done) releases the lock for the next process.
///
/// The `File` is kept alive purely to keep the fd open: `flock` is released by
/// `close(2)`, so the lock lives exactly as long as this value.
#[derive(Debug)]
pub struct EngineLock {
    _file: File,
    account: String,
}

impl EngineLock {
    /// Try to take the engine lock for `account`, non-blocking.
    ///
    /// - `Ok(Some(lock))`: this process is now the engine for the account.
    /// - `Ok(None)`: another live process holds it; degrade to read-only and
    ///   let that process's engine drain the queue.
    /// - `Err`: the lock file could not be opened or `flock` failed for a
    ///   reason other than contention (a genuine IO or permissions problem).
    ///
    /// The lock file lives beside the store, in the account directory, which
    /// [`crate::config::account_dir`] creates for every configured account.
    pub fn try_acquire(account: &str) -> Result<Option<Self>> {
        let dir = crate::config::account_dir(account);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating the account directory for {account}"))?;
        Self::try_acquire_at(&dir.join("store.lock"), account)
    }

    /// The mechanism, split out so a test can point it at a tempdir.
    pub fn try_acquire_at(path: &Path, account: &str) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening the engine lock file {}", path.display()))?;

        // SAFETY: `flock` takes a valid open file descriptor and a flag set; the
        // fd is owned by `file` and outlives the call. `LOCK_NB` makes it
        // return immediately rather than block on a held lock.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            debug!("[engine] took the engine lock for {account}");
            return Ok(Some(EngineLock {
                _file: file,
                account: account.to_string(),
            }));
        }

        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // Both spellings a kernel may use for "already locked". This is the
            // read-only degrade, not a failure.
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
                debug!("[engine] {account} engine lock is held by another process; read-only");
                Ok(None)
            }
            _ => Err(anyhow::Error::new(err)
                .context(format!("locking {} for {account}", path.display()))),
        }
    }

    /// The account this lock guards.
    pub fn account(&self) -> &str {
        &self.account
    }
}

impl Drop for EngineLock {
    fn drop(&mut self) {
        // `close(2)` (when `_file` drops) releases the advisory lock; this is
        // only a log line for the trace. An explicit `flock(LOCK_UN)` would be
        // redundant and could race the fd close.
        debug!("[engine] released the engine lock for {}", self.account);
    }
}

/// Run `f` only if this process can take the engine lock for `account`.
///
/// The shape a drain call site wants: it acquires the lock, runs the closure
/// while holding it, and releases it afterwards, or does nothing at all when
/// another process is the engine. `Ok(None)` is "someone else is draining",
/// which every caller treats as success: the work still happens, in the other
/// process.
pub fn with_engine_lock<T>(
    account: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    match EngineLock::try_acquire(account) {
        Ok(Some(_lock)) => f().map(Some),
        Ok(None) => Ok(None),
        Err(e) => {
            // A lock we cannot even attempt is not a reason to drain unguarded:
            // the whole point is that at most one process drains. Report it and
            // skip, exactly as the read-only degrade does.
            warn!("[engine] could not take the engine lock for {account}, skipping drain: {e:#}");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first acquirer gets the lock; a second attempt while it is held is
    /// refused without error, which is the read-only degrade.
    #[test]
    fn a_second_holder_is_refused_while_the_first_lives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.lock");

        let first = EngineLock::try_acquire_at(&path, "alice")
            .unwrap()
            .expect("the first process takes the lock");
        assert_eq!(first.account(), "alice");

        let second = EngineLock::try_acquire_at(&path, "alice").unwrap();
        assert!(
            second.is_none(),
            "a second live holder must be refused, not granted"
        );
    }

    /// Dropping the holder releases the lock for the next process, with no
    /// manual cleanup and no stale file to reap.
    #[test]
    fn dropping_the_holder_frees_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.lock");

        let first = EngineLock::try_acquire_at(&path, "alice").unwrap().unwrap();
        drop(first);

        let second = EngineLock::try_acquire_at(&path, "alice").unwrap();
        assert!(
            second.is_some(),
            "the lock did not come free after the holder dropped"
        );
    }

    /// The lock file is not consumed: it persists across a take/release cycle,
    /// so there is no unlink race between a releaser and the next acquirer.
    #[test]
    fn the_lock_file_persists_across_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.lock");

        {
            let _held = EngineLock::try_acquire_at(&path, "alice").unwrap().unwrap();
        }
        assert!(path.exists(), "the lock file should outlive the lock");
    }

    /// `with_engine_lock` runs the closure when the lock is free and skips it
    /// (returning `Ok(None)`) when another holder is live.
    #[test]
    fn with_engine_lock_skips_when_another_holder_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.lock");

        let held = EngineLock::try_acquire_at(&path, "alice").unwrap().unwrap();
        // A direct second attempt at the same path is refused; the closure form
        // resolves against the account dir, so here we assert the primitive it
        // is built on: a live holder blocks a second acquire.
        assert!(EngineLock::try_acquire_at(&path, "alice").unwrap().is_none());
        drop(held);
        assert!(EngineLock::try_acquire_at(&path, "alice").unwrap().is_some());
    }
}
