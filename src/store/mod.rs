//! Per-account SQLite store.
//!
//! One `store.sqlite3` per account directory, opened in WAL mode with a 5 s
//! busy timeout and `synchronous = NORMAL`. The file is a cache in front of
//! IMAP, never a system of record, so there is no migrator: a version
//! mismatch, a failed `integrity_check` or a file that cannot be opened as a
//! database is dropped and rebuilt empty, with a log line and no user-visible
//! error. The next sync refills it.
//!
//! This module knows nothing about IMAP, MIME or Markdown. It owns the file,
//! the pragmas and the schema; everything above it speaks SQL.
//!
//! Bodies, raw messages and attachments do not live in the database: they go
//! to the content-addressed blob store in [`blobs`], and rows keep only the
//! hash. The `blobs` table here is that store's refcount index, so a reference
//! can be taken in the same transaction as the row that holds the hash.

pub mod blobs;
pub mod schema;

use std::fs;
use std::path::{Path, PathBuf};
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

/// Open an existing file and prove it is a usable v1 store: readable as a
/// database, passing `integrity_check`, stamped with the current version and
/// structurally complete.
fn open_validated(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    apply_pragmas(&conn)?;

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .context("running integrity_check")?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(anyhow!("integrity_check returned {integrity}"));
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

/// Create the file and stamp schema v1.
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
    fn empty_directory_yields_schema_v1() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("acct").join("store.sqlite3");
        let store = Store::open(&path).unwrap();

        assert!(path.exists(), "store file was not created");
        assert_eq!(store.schema_version().unwrap(), Some(1));
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
