//! Per-account SQLite store.
//!
//! One `store.sqlite3` per account directory, opened in WAL mode with a 5 s
//! busy timeout and `synchronous = NORMAL`. The file is a cache in front of
//! IMAP, never a system of record, so there is no migrator: a version
//! mismatch, a failed `integrity_check` or a file that cannot be opened as a
//! database is dropped and rebuilt empty, with a log line and no user-visible
//! error. The next sync refills it. The `integrity_check` behind that contract
//! is a full walk of the file, so it runs once per file per process (see
//! [`INTEGRITY_CHECKED`]) rather than on every open.
//!
//! This module knows nothing about IMAP, MIME or Markdown. It owns the file,
//! the pragmas and the schema; everything above it speaks SQL.
//!
//! Bodies, raw messages and attachments do not live in the database: they go
//! to the content-addressed blob store in [`blobs`], and rows keep only the
//! hash. The `blobs` table here is that store's refcount index, so a reference
//! can be taken in the same transaction as the row that holds the hash.

pub mod blobs;
pub mod drafts;
pub mod read;
pub mod schema;
pub mod write;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use rusqlite::Connection;

use crate::timing::TimingSpan;

pub use blobs::{BlobHash, BlobStore};
pub use schema::SCHEMA_VERSION;

/// Busy timeout applied to every store connection, in milliseconds. WAL lets
/// many readers run alongside one writer; this smooths the brief contention
/// when a second process writes.
pub const BUSY_TIMEOUT_MS: u64 = 5000;

/// An open per-account store.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open (or create) the store at `path`, rebuilding it from scratch when
    /// the existing file is unusable. Only a genuine filesystem failure (the
    /// directory cannot be created, the rebuilt file cannot be written) is
    /// reported as an error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut span = TimingSpan::with_context("store_open", path.display().to_string());

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating store directory {}", parent.display()))?;
        }

        if path.exists() {
            match open_validated(&path) {
                Ok(conn) => {
                    span.mark("validated");
                    return Ok(Self { conn, path });
                }
                Err(err) => {
                    // Not a user-visible error: the store holds no truth.
                    warn!(
                        "[store] {} is unusable ({err:#}); dropping and rebuilding empty",
                        path.display()
                    );
                    // The file about to be created is a different file, so the
                    // "already checked this incarnation" note must go with the
                    // one being deleted.
                    forget_integrity_check(&path);
                    remove_store_files(&path)?;
                    span.mark("dropped");
                }
            }
        }

        let conn = create_fresh(&path)?;
        info!(
            "[store] created schema v{SCHEMA_VERSION} at {}",
            path.display()
        );
        span.mark("created");
        Ok(Self { conn, path })
    }

    /// Open the store for a named account: `<account_dir>/store.sqlite3`.
    pub fn open_account(account_name: &str) -> Result<Self> {
        Self::open(crate::config::store_path(account_name))
    }

    /// The underlying connection. Callers run their own SQL; the store owns
    /// only the file lifecycle and the schema.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Path of the open database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The schema version stamped in `meta`. Always [`SCHEMA_VERSION`] for a
    /// store that just came back from [`Store::open`].
    pub fn schema_version(&self) -> Result<Option<i64>> {
        schema::stamped_version(&self.conn)
    }
}

/// Files whose `integrity_check` this process has already run and passed,
/// keyed by canonical path, with the number of checks run against each.
///
/// `PRAGMA integrity_check` walks every page of the database file: 240 ms on a
/// 44 MB store. The TUI opens a store per call rather than parking one (see
/// `tui::app::types::open_store`), so an unconditional check ran once per
/// keypress and ten times before the first paint. The check stays load-bearing
/// for the drop-and-rebuild contract in the module docs, so it is amortised
/// rather than removed: the first open of a given file in a given process
/// still validates it in full and still triggers the rebuild on failure, and
/// every later open of that same file trusts the earlier verdict. A file that
/// rots underneath a long-lived process is caught by the next process, which
/// is the same guarantee the pre-store build gave.
static INTEGRITY_CHECKED: OnceLock<Mutex<HashMap<PathBuf, u32>>> = OnceLock::new();

fn integrity_registry() -> &'static Mutex<HashMap<PathBuf, u32>> {
    INTEGRITY_CHECKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Canonical identity of a store file. Canonicalisation needs the file to
/// exist, which it does on every path that reaches here; the raw path is a
/// safe fallback because a miss only means one extra check.
fn integrity_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn integrity_check_count(path: &Path) -> u32 {
    let registry = integrity_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.get(&integrity_key(path)).copied().unwrap_or(0)
}

fn note_integrity_check(path: &Path) {
    let mut registry = integrity_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *registry.entry(integrity_key(path)).or_insert(0) += 1;
}

/// Drop the note for a file that is about to be deleted, so the replacement
/// created at the same path is validated on its own first open.
fn forget_integrity_check(path: &Path) {
    let key = integrity_key(path);
    let mut registry = integrity_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.remove(&key);
}

/// Open an existing file and prove it is a usable store of the current schema
/// version: readable as a
/// database, passing `integrity_check` (once per file per process, see
/// [`INTEGRITY_CHECKED`]), stamped with the current version and structurally
/// complete.
fn open_validated(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    apply_pragmas(&conn)?;

    if integrity_check_count(path) == 0 {
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .context("running integrity_check")?;
        note_integrity_check(path);
        if !integrity.eq_ignore_ascii_case("ok") {
            return Err(anyhow!("integrity_check returned {integrity}"));
        }
    }

    match schema::stamped_version(&conn)? {
        Some(SCHEMA_VERSION) => {}
        Some(other) => return Err(anyhow!("schema version {other}, expected {SCHEMA_VERSION}")),
        None => return Err(anyhow!("no schema version stamp")),
    }

    if !schema::all_tables_present(&conn)? {
        return Err(anyhow!("schema v{SCHEMA_VERSION} is incomplete"));
    }

    Ok(conn)
}

/// Create the file and stamp the current schema version.
fn create_fresh(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("creating {}", path.display()))?;
    apply_pragmas(&conn)?;
    schema::create(&conn)?;
    Ok(conn)
}

/// WAL, busy timeout and `synchronous = NORMAL`. WAL is a property of the file
/// and survives; the other two are per-connection and are set on every open.
fn apply_pragmas(conn: &Connection) -> Result<()> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .context("setting journal_mode = WAL")?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(anyhow!("journal_mode is {mode}, expected wal"));
    }
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .context("setting busy_timeout")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("setting synchronous = NORMAL")?;
    // Off by default in SQLite, and `message_blobs` leans on it: the cascade
    // is what keeps a message's blob-reference list from outliving the row.
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("setting foreign_keys = ON")?;
    Ok(())
}

/// Delete the database and its WAL sidecars. A leftover `-wal` next to a fresh
/// main file is how a "rebuilt" store comes back haunted.
fn remove_store_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let candidate = PathBuf::from(name);
        if candidate.exists() {
            fs::remove_file(&candidate)
                .with_context(|| format!("removing {}", candidate.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn insert_message(
        store: &Store,
        account: &str,
        mailbox: &str,
        uid: i64,
        message_id: &str,
    ) -> rusqlite::Result<usize> {
        store.conn().execute(
            "INSERT INTO messages (account, mailbox, uid, message_id) VALUES (?1, ?2, ?3, ?4)",
            (account, mailbox, uid, message_id),
        )
    }

    #[test]
    fn empty_directory_yields_the_current_schema() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("acct").join("store.sqlite3");
        let store = Store::open(&path).unwrap();

        assert!(path.exists(), "store file was not created");
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        assert!(schema::all_tables_present(store.conn()).unwrap());
    }

    #[test]
    fn version_stamp_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");

        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        }

        // Reopening an intact store keeps the same file and stamp.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        assert_eq!(
            schema::get_meta(store.conn(), schema::META_APP_VERSION).unwrap(),
            Some(env!("CARGO_PKG_VERSION").to_string())
        );
    }

    #[test]
    fn truncated_file_is_dropped_and_rebuilt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");

        {
            let store = Store::open(&path).unwrap();
            insert_message(&store, "alice", "inbox", 1, "<a@example.com>").unwrap();
        }

        // Garbage that SQLite cannot open as a database at all.
        {
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(b"not a database, just bytes").unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        let count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "rebuilt store must be empty");
    }

    /// The check is what makes the drop-and-rebuild contract real, so it is
    /// amortised rather than dropped: the first open of a file validates it,
    /// every later open in the same process trusts that verdict. A freshly
    /// created file is intact by construction and is not checked at all.
    #[test]
    fn the_integrity_check_runs_once_per_file_per_process() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");

        drop(Store::open(&path).unwrap());
        assert_eq!(
            integrity_check_count(&path),
            0,
            "a file this process just created needs no integrity check"
        );

        drop(Store::open(&path).unwrap());
        assert_eq!(integrity_check_count(&path), 1, "the first reopen validates");

        for _ in 0..5 {
            drop(Store::open(&path).unwrap());
        }
        assert_eq!(
            integrity_check_count(&path),
            1,
            "later opens must skip the full-file walk"
        );
    }

    /// Corruption that only `integrity_check` can see (the header still reads
    /// as a database) still costs the file its contents on the first open of
    /// the process, and the rebuilt file is validated on its own next open
    /// rather than inheriting the dead one's verdict.
    #[test]
    fn a_corrupted_page_is_still_caught_on_the_first_open() {
        use std::io::{Seek, SeekFrom, Write as _};

        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        {
            let store = Store::open(&path).unwrap();
            for uid in 0..400 {
                insert_message(&store, "alice", "inbox", uid, &format!("<m{uid}@example.com>"))
                    .unwrap();
            }
        }
        let page_size: i64 = {
            let conn = Connection::open(&path).unwrap();
            conn.query_row("PRAGMA page_size", [], |row| row.get(0)).unwrap()
        };

        // Scribble over the middle of the file, leaving page 1 (the header and
        // the schema) readable: the file still opens, and only a full walk
        // notices.
        {
            let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
            let len = f.metadata().unwrap().len();
            let offset = (len / 2).max(page_size as u64);
            f.seek(SeekFrom::Start(offset)).unwrap();
            f.write_all(&vec![0x5a; page_size as usize]).unwrap();
            f.sync_all().unwrap();
        }
        assert_eq!(
            integrity_check_count(&path),
            0,
            "nothing in this process has validated this file yet"
        );

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        let count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "a corrupted store must be dropped and rebuilt");
        drop(store);

        assert_eq!(
            integrity_check_count(&path),
            0,
            "the rebuilt file must not inherit the dead one's verdict"
        );
        drop(Store::open(&path).unwrap());
        assert_eq!(integrity_check_count(&path), 1);
    }

    #[test]
    fn wrongly_stamped_version_is_dropped_and_rebuilt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");

        {
            let store = Store::open(&path).unwrap();
            insert_message(&store, "alice", "inbox", 1, "<a@example.com>").unwrap();
            schema::set_meta(store.conn(), schema::META_SCHEMA_VERSION, "99").unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        let count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "rebuilt store must be empty");
    }

    #[test]
    fn unstamped_database_is_dropped_and_rebuilt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");

        // A valid SQLite file that is not one of ours.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (x);").unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        assert!(schema::all_tables_present(store.conn()).unwrap());
    }

    #[test]
    fn wal_pragmas_are_set() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();

        let journal_mode: String = store
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");

        let busy_timeout: i64 = store
            .conn()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, BUSY_TIMEOUT_MS as i64);

        // 1 == NORMAL
        let synchronous: i64 = store
            .conn()
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1);
    }

    #[test]
    fn pragmas_survive_a_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        drop(Store::open(&path).unwrap());

        let store = Store::open(&path).unwrap();
        let journal_mode: String = store
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
        let busy_timeout: i64 = store
            .conn()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, BUSY_TIMEOUT_MS as i64);
    }

    #[test]
    fn duplicate_account_mailbox_uid_is_rejected() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();

        insert_message(&store, "alice", "inbox", 7, "<a@example.com>").unwrap();
        let err = insert_message(&store, "alice", "inbox", 7, "<b@example.com>").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("unique"),
            "expected a UNIQUE violation, got {err}"
        );

        // The same UID in another mailbox, and another UID in the same
        // mailbox, are both fine.
        insert_message(&store, "alice", "archive", 7, "<c@example.com>").unwrap();
        insert_message(&store, "alice", "inbox", 8, "<d@example.com>").unwrap();
    }

    #[test]
    fn duplicate_message_id_is_accepted() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();

        // Cross-mailbox copy: same message-id, different (mailbox, uid).
        insert_message(&store, "alice", "inbox", 1, "<shared@example.com>").unwrap();
        insert_message(&store, "alice", "archive", 42, "<shared@example.com>").unwrap();

        let count: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE message_id = ?1",
                ["<shared@example.com>"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn outbox_rejects_an_unknown_state() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();

        let insert = |state: &str| {
            store.conn().execute(
                "INSERT INTO outbox (account, target_mailbox, message_id, raw_blob, state)
                 VALUES ('alice', 'sent', '<m@example.com>', 'deadbeef', ?1)",
                [state],
            )
        };
        for state in ["pending_send", "sent_pending_append", "done", "failed"] {
            insert(state).unwrap();
        }
        assert!(insert("almost_sent").is_err());
    }

    #[test]
    fn blobs_table_rejects_a_negative_refcount() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();

        store
            .conn()
            .execute(
                "INSERT INTO blobs (hash, size, refcount) VALUES ('abc', 12, 1)",
                [],
            )
            .unwrap();
        assert!(store
            .conn()
            .execute("UPDATE blobs SET refcount = refcount - 2", [])
            .is_err());

        // A duplicate hash is the same blob, not a second row.
        assert!(store
            .conn()
            .execute(
                "INSERT INTO blobs (hash, size, refcount) VALUES ('abc', 12, 1)",
                [],
            )
            .is_err());
    }

    #[test]
    fn missing_blobs_table_forces_a_rebuild() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");

        {
            let store = Store::open(&path).unwrap();
            insert_message(&store, "alice", "inbox", 1, "<a@example.com>").unwrap();
            store.conn().execute_batch("DROP TABLE blobs;").unwrap();
            assert!(!schema::all_tables_present(store.conn()).unwrap());
        }

        let store = Store::open(&path).unwrap();
        assert!(schema::all_tables_present(store.conn()).unwrap());
        let count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "rebuilt store must be empty");
    }

    #[test]
    fn drafts_are_keyed_by_account_and_id() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();

        let insert = |account: &str, id: &str| {
            store.conn().execute(
                "INSERT INTO drafts (account, id, slug) VALUES (?1, ?2, 'a-slug')",
                (account, id),
            )
        };
        insert("alice", "d1").unwrap();
        insert("bob", "d1").unwrap();
        assert!(insert("alice", "d1").is_err());
    }
}
