//! What survives a drop-and-rebuild (#0066).
//!
//! [`super::Store::open`] answers an unusable store file by deleting it and
//! creating an empty one, because `messages` and everything derived from it is
//! a cache the next sync refills. The `outbox` is the one table that is not:
//! it is the record of what has been submitted to a mail server, and
//! `mp outbox list|retry|discard` presents it as durable send state. So the
//! rebuild does three things beyond creating the file:
//!
//! 1. Before the drop, it reads the old `outbox` back defensively (by column
//!    name, tolerating a schema that is not the current one) and carries every
//!    unfinished row into the fresh file, together with a reference on the raw
//!    RFC822 blob it points at. `done` rows have nothing outstanding and are
//!    not carried.
//! 2. It then sweeps the blob tree, deleting every file the rebuilt store has
//!    no refcount row for. Without this, a rebuild leaves the whole blob
//!    directory orphaned: the files survive, the refcounts do not, and nothing
//!    ever reclaims them. The carried outbox rows are exactly what keeps their
//!    own bytes alive through the sweep.
//! 3. It writes a `store-rebuild-<timestamp>.txt` note next to the store when
//!    outbox rows were involved, so a discarded submission is never silent.
//!
//! The sweep assumes the rebuild is the only thing touching the account
//! directory: a blob written by a concurrent ingest whose row has not
//! committed yet would be swept as an orphan. The cost is a refetch, which is
//! the same cost the rebuild itself pays.
//!
//! Nothing here is allowed to fail an open. A store that cannot be salvaged is
//! still a store; every error below is logged and stepped over.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{info, warn};
use rusqlite::{Connection, Row};
use walkdir::WalkDir;

use super::blobs::{BlobHash, BlobStore};

/// Outbox states worth carrying: everything the send machine still owes an
/// answer for. `done` is complete by definition and stays behind.
const CARRIED_STATES: &[&str] = &["pending_send", "sent_pending_append", "failed"];

/// Where an unreadable state lands. A row whose state cannot be trusted must
/// not be re-submitted, and `failed` is the state that means "a human decides".
const UNREADABLE_STATE: &str = "failed";

/// One `outbox` row read out of the file that is about to be deleted.
///
/// Every field is what the old file happened to hold, not what the current
/// schema promises: this is read from a database that just failed validation.
#[derive(Debug, Clone)]
pub(super) struct SalvagedRow {
    account: Option<String>,
    target_mailbox: Option<String>,
    message_id: Option<String>,
    raw_blob: Option<String>,
    state: Option<String>,
    attempts: i64,
    last_error: Option<String>,
    appended_uid: Option<i64>,
    created: Option<i64>,
    updated: Option<i64>,
    submission_started_at: Option<i64>,
    envelope: Option<String>,
}

impl SalvagedRow {
    /// How the row is named in a log line and in the note file.
    fn describe(&self) -> String {
        format!(
            "{} ({})",
            self.message_id.as_deref().unwrap_or("<no message-id>"),
            self.state.as_deref().unwrap_or("unreadable state")
        )
    }
}

/// What a rebuild did, beyond creating an empty file.
#[derive(Debug, Default)]
pub(super) struct RebuildReport {
    /// Outbox rows carried into the fresh store, as `describe` strings.
    pub carried: Vec<String>,
    /// Outbox rows that could not be carried, each with its reason.
    pub lost: Vec<(String, String)>,
    pub swept_files: u64,
    pub swept_bytes: u64,
}

/// The blob root that belongs to a store file: `<account_dir>/blobs/`, the
/// sibling layout [`crate::config::blobs_dir`] lays down.
fn blobs_root(store_path: &Path) -> Option<PathBuf> {
    store_path.parent().map(|dir| dir.join("blobs"))
}

/// Read the unfinished `outbox` rows out of a store that failed validation.
///
/// Best effort by construction: the file may be corrupt, may be some other
/// database, may hold an `outbox` of an older shape. Every column is read by
/// name and a column that is absent or holds the wrong type yields `None`
/// rather than failing the row, because a partially readable submission is
/// still worth showing to a human.
pub(super) fn salvage_outbox(path: &Path) -> Vec<SalvagedRow> {
    let conn = match Connection::open(path) {
        Ok(conn) => conn,
        Err(e) => {
            warn!("[store] no outbox salvage from {}: {e}", path.display());
            return Vec::new();
        }
    };

    let mut stmt = match conn.prepare("SELECT * FROM outbox") {
        Ok(stmt) => stmt,
        Err(e) => {
            // The common case by far: the file is not one of ours, or is too
            // damaged to read a table list from.
            info!("[store] no outbox to salvage from {}: {e}", path.display());
            return Vec::new();
        }
    };
    let rows = match stmt.query_map([], |row| Ok(read_salvaged_row(row))) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("[store] outbox salvage from {} failed: {e}", path.display());
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok(row) => {
                // A finished row owes nothing and is not worth carrying.
                if row.state.as_deref() == Some("done") {
                    continue;
                }
                out.push(row);
            }
            Err(e) => warn!("[store] skipped an unreadable outbox row: {e}"),
        }
    }
    out
}

fn read_salvaged_row(row: &Row<'_>) -> SalvagedRow {
    SalvagedRow {
        account: opt_string(row, "account"),
        target_mailbox: opt_string(row, "target_mailbox"),
        message_id: opt_string(row, "message_id"),
        raw_blob: opt_string(row, "raw_blob"),
        state: opt_string(row, "state"),
        attempts: opt_i64(row, "attempts").unwrap_or(0),
        last_error: opt_string(row, "last_error"),
        appended_uid: opt_i64(row, "appended_uid"),
        created: opt_i64(row, "created"),
        updated: opt_i64(row, "updated"),
        submission_started_at: opt_i64(row, "submission_started_at"),
        envelope: opt_string(row, "envelope"),
    }
}

/// Seconds since the epoch, for a salvaged row that carries no timestamps.
/// Local to this module so the store layer keeps depending on nothing above
/// it.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A text column that may not exist and may not be text.
fn opt_string(row: &Row<'_>, name: &str) -> Option<String> {
    row.get::<_, Option<String>>(name)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// An integer column that may not exist and may not be an integer.
fn opt_i64(row: &Row<'_>, name: &str) -> Option<i64> {
    row.get::<_, Option<i64>>(name).ok().flatten()
}

/// Restore the salvaged rows into the fresh store and sweep the blob tree.
///
/// Returns what happened so the caller can log it and write the note file.
pub(super) fn finish(conn: &Connection, path: &Path, salvaged: Vec<SalvagedRow>) -> RebuildReport {
    let mut report = RebuildReport::default();
    let Some(root) = blobs_root(path) else {
        // Unreachable for any real store path, and still not a place where a
        // submission may vanish without being named.
        for row in &salvaged {
            report.lost.push((
                row.describe(),
                "the store path has no directory to hold a blob store".to_string(),
            ));
        }
        return report;
    };
    let blobs = BlobStore::new(root);

    for row in salvaged {
        match restore_row(conn, &blobs, &row) {
            Ok(()) => report.carried.push(row.describe()),
            Err(reason) => report.lost.push((row.describe(), reason)),
        }
    }

    match sweep_orphan_blobs(conn, &blobs) {
        Ok((files, bytes)) => {
            report.swept_files = files;
            report.swept_bytes = bytes;
        }
        Err(e) => warn!(
            "[store] sweeping orphaned blobs under {} failed: {e:#}",
            blobs.root().display()
        ),
    }

    report
}

/// Write one salvaged row into the fresh `outbox`, with its blob reference.
///
/// The `Err` string is the reason the row could not be carried, phrased for
/// the note file a human reads.
fn restore_row(conn: &Connection, blobs: &BlobStore, row: &SalvagedRow) -> Result<(), String> {
    let account = row
        .account
        .clone()
        .ok_or_else(|| "the row named no account".to_string())?;
    let message_id = row
        .message_id
        .clone()
        .ok_or_else(|| "the row named no message-id".to_string())?;
    let raw_blob = row
        .raw_blob
        .as_deref()
        .ok_or_else(|| "the row named no raw message".to_string())?;
    let hash = BlobHash::parse(raw_blob)
        .map_err(|_| format!("'{raw_blob}' is not a blob hash, so the bytes are unreachable"))?;

    let size = fs::metadata(blobs.path_for(&hash))
        .map_err(|_| "the raw bytes are no longer in the blob store".to_string())?
        .len();

    // An unknown state cannot be re-submitted safely, so it is parked for a
    // human rather than guessed at.
    let (state, last_error) = match row.state.as_deref() {
        Some(state) if CARRIED_STATES.contains(&state) => (state.to_string(), row.last_error.clone()),
        other => (
            UNREADABLE_STATE.to_string(),
            Some(format!(
                "carried across a store rebuild from an unreadable state ({})",
                other.unwrap_or("none")
            )),
        ),
    };

    let now = now_unix();
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("the rebuilt store refused a transaction: {e}"))?;
    tx.execute(
        "INSERT INTO outbox (account, target_mailbox, message_id, raw_blob, state, attempts,
                             last_error, appended_uid, created, updated, submission_started_at,
                             envelope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            account,
            row.target_mailbox,
            message_id,
            hash.as_str(),
            state,
            row.attempts,
            last_error,
            row.appended_uid,
            row.created.unwrap_or(now),
            row.updated.unwrap_or(now),
            row.submission_started_at,
            row.envelope,
        ],
    )
    .map_err(|e| format!("the rebuilt store refused the row: {e}"))?;
    blobs
        .acquire(&tx, &hash, size)
        .map_err(|e| format!("the rebuilt store refused the blob reference: {e:#}"))?;
    tx.commit()
        .map_err(|e| format!("the rebuilt store refused the commit: {e}"))?;
    Ok(())
}

/// Delete every file under the blob root that the store holds no refcount row
/// for, then prune the fan-out directories that emptied.
///
/// Matching is on the file's own name *and* its fan-out position, so a
/// leftover `.tmp` from an interrupted write and a blob sitting in the wrong
/// directory both go the way of the orphans.
fn sweep_orphan_blobs(conn: &Connection, blobs: &BlobStore) -> Result<(u64, u64)> {
    let root = blobs.root();
    if !root.is_dir() {
        return Ok((0, 0));
    }

    let mut retained: HashSet<String> = HashSet::new();
    let mut stmt = conn
        .prepare("SELECT hash FROM blobs")
        .context("listing retained blobs")?;
    let hashes = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for hash in hashes {
        retained.insert(hash?);
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in WalkDir::new(root).contents_first(true).into_iter().flatten() {
        let path = entry.path();
        if entry.file_type().is_dir() {
            if path != root {
                // Only succeeds when the directory emptied out above.
                let _ = fs::remove_dir(path);
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(hash) = BlobHash::parse(&name) {
            if retained.contains(hash.as_str()) && blobs.path_for(&hash) == path {
                continue;
            }
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(path) {
            Ok(()) => {
                files += 1;
                bytes += size;
            }
            Err(e) => warn!("[store] could not remove orphaned blob {}: {e}", path.display()),
        }
    }
    Ok((files, bytes))
}

/// Write the human-readable note that says what the rebuild kept and what it
/// could not, next to the store file. Returns the path it wrote.
///
/// Only written when outbox rows were involved: a rebuild that touched nothing
/// a user submitted is a cache refill and needs no paperwork.
pub(super) fn write_notice(path: &Path, reason: &str, report: &RebuildReport) -> Option<PathBuf> {
    if report.carried.is_empty() && report.lost.is_empty() {
        return None;
    }
    let dir = path.parent()?;
    let now = chrono::Utc::now();
    let notice = dir.join(format!(
        "store-rebuild-{}.txt",
        now.format("%Y%m%dT%H%M%SZ")
    ));

    let mut text = format!(
        "mailypoppins rebuilt {} on {}.\n\n\
         Why: {reason}.\n\n\
         The store is a cache in front of the mail server, so the messages it held come back on \
         the next sync.\n\
         The outbox is not a cache, so its unfinished rows were carried across the rebuild.\n",
        path.display(),
        now.format("%Y-%m-%d %H:%M:%S UTC"),
    );
    if !report.carried.is_empty() {
        text.push_str("\nCarried into the rebuilt store, and listed by `mp outbox list`:\n");
        for row in &report.carried {
            text.push_str(&format!("  {row}\n"));
        }
    }
    if !report.lost.is_empty() {
        text.push_str("\nDiscarded, because they could not be carried:\n");
        for (row, why) in &report.lost {
            text.push_str(&format!("  {row}: {why}\n"));
        }
    }
    if report.swept_files > 0 {
        text.push_str(&format!(
            "\nOrphaned blob files removed: {} ({} bytes).\n",
            report.swept_files, report.swept_bytes
        ));
    }

    match fs::write(&notice, text) {
        Ok(()) => Some(notice),
        Err(e) => {
            warn!("[store] could not write {}: {e}", notice.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{schema, Store};
    use tempfile::{tempdir, TempDir};

    /// An account directory laid out the way the real one is: the store file
    /// and its `blobs/` sibling.
    fn account_dir() -> (TempDir, PathBuf, BlobStore) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let blobs = BlobStore::new(dir.path().join("blobs"));
        (dir, path, blobs)
    }

    fn blob_files(root: &Path) -> Vec<String> {
        if !root.exists() {
            return Vec::new();
        }
        let mut names: Vec<String> = WalkDir::new(root)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn notice_file(dir: &Path) -> Option<PathBuf> {
        fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("store-rebuild-"))
            })
    }

    /// Enqueue a submission the way `crate::outbox` does: blob first, then the
    /// row and its reference in one transaction.
    fn enqueue(store: &Store, blobs: &BlobStore, message_id: &str, state: &str, raw: &[u8]) {
        let hash = blobs.write(raw).unwrap();
        let tx = store.conn().unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO outbox (account, target_mailbox, message_id, raw_blob, state, attempts,
                                 created, updated, submission_started_at, envelope)
             VALUES ('alice', 'sent', ?1, ?2, ?3, 2, 100, 200, 300, 'from:alice@example.com')",
            rusqlite::params![message_id, hash.as_str(), state],
        )
        .unwrap();
        blobs.acquire(&tx, &hash, raw.len() as u64).unwrap();
        tx.commit().unwrap();
    }

    /// The columns of a carried row, so one assertion covers all of them.
    #[derive(Debug, PartialEq)]
    struct Carried {
        account: String,
        target_mailbox: Option<String>,
        state: String,
        attempts: i64,
        created: i64,
        updated: i64,
        submission_started_at: Option<i64>,
        envelope: Option<String>,
    }

    impl Carried {
        fn load(store: &Store, message_id: &str) -> Self {
            store
                .conn()
                .query_row(
                    "SELECT account, target_mailbox, state, attempts, created, updated,
                            submission_started_at, envelope
                     FROM outbox WHERE message_id = ?1",
                    [message_id],
                    |r| {
                        Ok(Self {
                            account: r.get(0)?,
                            target_mailbox: r.get(1)?,
                            state: r.get(2)?,
                            attempts: r.get(3)?,
                            created: r.get(4)?,
                            updated: r.get(5)?,
                            submission_started_at: r.get(6)?,
                            envelope: r.get(7)?,
                        })
                    },
                )
                .unwrap()
        }
    }

    /// Make the file unusable the way a schema bump does, which is the case
    /// #0066 was filed for.
    fn stamp_a_wrong_version(path: &Path) {
        let store = Store::open(path).unwrap();
        schema::set_meta(store.conn(), schema::META_SCHEMA_VERSION, "99").unwrap();
    }

    #[test]
    fn a_rebuild_carries_unfinished_outbox_rows_and_their_bytes() {
        let (dir, path, blobs) = account_dir();
        {
            let store = Store::open(&path).unwrap();
            enqueue(&store, &blobs, "<pending@example.com>", "pending_send", b"raw pending");
            enqueue(&store, &blobs, "<appending@example.com>", "sent_pending_append", b"raw appending");
            enqueue(&store, &blobs, "<parked@example.com>", "failed", b"raw parked");
            enqueue(&store, &blobs, "<finished@example.com>", "done", b"raw finished");
        }
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));

        let mut stmt = store
            .conn()
            .prepare("SELECT message_id FROM outbox ORDER BY message_id")
            .unwrap();
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![
                "<appending@example.com>",
                "<parked@example.com>",
                "<pending@example.com>"
            ],
            "the three unfinished rows survive and the done row does not"
        );

        assert_eq!(
            Carried::load(&store, "<pending@example.com>"),
            Carried {
                account: "alice".to_string(),
                target_mailbox: Some("sent".to_string()),
                state: "pending_send".to_string(),
                attempts: 2,
                created: 100,
                updated: 200,
                submission_started_at: Some(300),
                envelope: Some("from:alice@example.com".to_string()),
            },
            "every column of a carried row comes across, the exactly-once marker included"
        );

        // Their bytes survive with a fresh reference; the done row's do not.
        assert_eq!(
            blob_files(blobs.root()).len(),
            3,
            "only the carried rows' blobs are kept"
        );
        for raw in [&b"raw pending"[..], b"raw appending", b"raw parked"] {
            let hash = BlobHash::of(raw);
            assert!(blobs.contains(&hash), "carried bytes must survive the sweep");
            assert_eq!(super::super::blobs::refcount(store.conn(), &hash).unwrap(), 1);
        }
        assert!(
            !blobs.contains(&BlobHash::of(b"raw finished")),
            "a done row's bytes are swept with the rest"
        );

        let notice = notice_file(dir.path()).expect("a rebuild touching the outbox writes a note");
        let text = fs::read_to_string(notice).unwrap();
        assert!(text.contains("<pending@example.com> (pending_send)"), "{text}");
        assert!(text.contains("Carried into the rebuilt store"), "{text}");
    }

    #[test]
    fn a_rebuild_leaves_no_orphaned_blob_files() {
        let (dir, path, blobs) = account_dir();
        {
            let store = Store::open(&path).unwrap();
            let raw = b"a message body";
            let hash = blobs.write(raw).unwrap();
            store
                .conn()
                .execute(
                    "INSERT INTO messages (account, mailbox, uid, message_id, raw_blob)
                     VALUES ('alice', 'inbox', 1, '<m@example.com>', ?1)",
                    [hash.as_str()],
                )
                .unwrap();
            blobs.acquire(store.conn(), &hash, raw.len() as u64).unwrap();
            // An interrupted write leaves a temp sibling; it is orphaned too.
            let dir = blobs.path_for(&hash).parent().unwrap().to_path_buf();
            fs::write(dir.join(format!(".{hash}.tmp.4242.0")), b"half").unwrap();
        }
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        assert!(
            blob_files(blobs.root()).is_empty(),
            "a rebuilt store must not leave blob files nothing references"
        );
        let blob_rows: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blob_rows, 0);
        assert!(
            notice_file(dir.path()).is_none(),
            "a rebuild that touched no outbox row needs no note"
        );
    }

    #[test]
    fn a_row_whose_bytes_are_gone_is_named_rather_than_dropped_silently() {
        let (dir, path, blobs) = account_dir();
        {
            let store = Store::open(&path).unwrap();
            enqueue(&store, &blobs, "<orphan@example.com>", "pending_send", b"raw orphan");
        }
        // Retention, a manual cleanup, a half-restored backup: the row points
        // at bytes that are not there any more.
        fs::remove_file(blobs.path_for(&BlobHash::of(b"raw orphan"))).unwrap();
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        let rows: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "a row with no bytes cannot be carried");

        let notice = notice_file(dir.path()).expect("a discarded row must leave a note");
        let text = fs::read_to_string(notice).unwrap();
        assert!(text.contains("Discarded, because they could not be carried"), "{text}");
        assert!(text.contains("<orphan@example.com> (pending_send)"), "{text}");
        assert!(text.contains("no longer in the blob store"), "{text}");
    }

    #[test]
    fn salvage_reads_an_outbox_of_an_older_shape() {
        let (_dir, path, blobs) = account_dir();
        let raw = b"raw from an older schema";
        let hash = blobs.write(raw).unwrap();

        // A v3-era file: fewer columns, and none of the ones added since.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL,
                     attempts   INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '3')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO outbox (account, message_id, raw_blob, state, attempts)
                 VALUES ('alice', '<old@example.com>', ?1, 'pending_send', 1)",
                [hash.as_str()],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let (message_id, state, attempts, envelope, marker): (
            String,
            String,
            i64,
            Option<String>,
            Option<i64>,
        ) = store
            .conn()
            .query_row(
                "SELECT message_id, state, attempts, envelope, submission_started_at FROM outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(message_id, "<old@example.com>");
        assert_eq!(state, "pending_send");
        assert_eq!(attempts, 1);
        assert_eq!(envelope, None, "a column the old file never had is empty");
        assert_eq!(marker, None, "and so is the exactly-once marker");
        assert!(blobs.contains(&hash), "its bytes survive the sweep");
    }

    #[test]
    fn an_unreadable_state_is_parked_for_a_human_rather_than_re_sent() {
        let (_dir, path, blobs) = account_dir();
        let raw = b"raw with a bad state";
        let hash = blobs.write(raw).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO outbox (account, message_id, raw_blob, state)
                 VALUES ('alice', '<weird@example.com>', ?1, 'almost_sent')",
                [hash.as_str()],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let (state, last_error): (String, Option<String>) = store
            .conn()
            .query_row("SELECT state, last_error FROM outbox", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(state, "failed", "an unknown state must never be re-submitted");
        assert!(
            last_error.unwrap().contains("almost_sent"),
            "the note on the row says where it came from"
        );
    }

    #[test]
    fn a_file_that_is_not_a_database_salvages_nothing_and_still_rebuilds() {
        let (dir, path, blobs) = account_dir();
        blobs.write(b"an orphan from the dead file").unwrap();
        fs::write(&path, b"not a database, just bytes").unwrap();

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));
        assert!(blob_files(blobs.root()).is_empty(), "the orphan is swept");
        assert!(notice_file(dir.path()).is_none());
    }
}
