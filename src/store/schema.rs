//! Schema v1 for the per-account store.
//!
//! There is no migrator and there never will be one: the store is a cache in
//! front of IMAP, so a version mismatch is answered by dropping the file and
//! rebuilding it empty (see [`super::Store::open`]). That is why every
//! statement below is a plain `CREATE`, and why the only stateful thing in the
//! file is the `schema_version` row in `meta`.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Version stamped into `meta.schema_version`. Bump this whenever any
/// statement in [`SCHEMA_SQL`] changes; every existing store is then dropped
/// and rebuilt on the next open.
pub const SCHEMA_VERSION: i64 = 1;

/// `meta` key holding [`SCHEMA_VERSION`].
pub const META_SCHEMA_VERSION: &str = "schema_version";

/// `meta` key holding the crate version that last created the file. Purely
/// informational (it makes a stale store obvious in a bug report).
pub const META_APP_VERSION: &str = "app_version";

/// Tables that must exist for a store to count as a valid v1 file. Checked on
/// open so that a file which is stamped v1 but structurally incomplete (a
/// half-written create, a hand-edited database) is rebuilt rather than used.
pub const REQUIRED_TABLES: &[&str] = &[
    "meta",
    "mailboxes",
    "messages",
    "blobs",
    "drafts",
    "outbox",
    "sync_cursors",
    "pending_ops",
    "messages_fts",
];

/// The complete schema. Follows the sketch in `docs/plans/data-access-layer.md`.
///
/// Identity notes that are load-bearing rather than cosmetic:
///
/// - `messages` has a synthetic `id` so a move or a UIDVALIDITY reset does not
///   invalidate references held elsewhere; `UNIQUE (account, mailbox, uid)` is
///   the real identity and the target of the ingest UPSERT.
/// - `messages_message_id` is deliberately non-unique. It serves threading,
///   idempotent re-ingest after cursor loss, cross-mailbox copy detection and
///   stale selector resolution, and is never on the hot ingest path.
/// - `drafts` is keyed by `(account, id)`, where `id` is the frontmatter field
///   written by `mp new`; the file on disk stays truth and this table is a
///   derived index.
/// - `outbox` carries the durable send state machine; the four states are
///   enforced by a CHECK so a typo in later code fails loudly.
/// - `blobs` is the refcount index for the content-addressed blob store in
///   [`super::blobs`]. It lives here rather than on disk so a reference can be
///   taken in the same transaction as the `messages` / `outbox` row that
///   carries the hash; the file itself is the disposable side of the pair.
/// - `messages_fts` is external-content over `messages`, as the plan sketches,
///   with `body_text` as a third indexed column. `messages` has no `body_text`
///   column (the body lives in a blob), so the index is written explicitly by
///   whoever ingests, and only `rowid`-returning `MATCH` queries are usable:
///   `snippet()`, `highlight()`, selecting a column value from the FTS table
///   and the `'rebuild'` command all fail with `no such column: T.body_text`.
///   Rebuilding from the content table is not needed anyway, because a store
///   that loses its index is dropped and refilled by the next sync.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE mailboxes (
    account      TEXT NOT NULL,
    name         TEXT NOT NULL,
    uidvalidity  INTEGER,
    uidnext      INTEGER,
    exists_count INTEGER,
    unread_count INTEGER,
    PRIMARY KEY (account, name)
);

CREATE TABLE messages (
    id               INTEGER PRIMARY KEY,
    account          TEXT NOT NULL,
    mailbox          TEXT NOT NULL,
    uid              INTEGER NOT NULL,
    message_id       TEXT NOT NULL,
    from_            TEXT,
    to_              TEXT,
    cc               TEXT,
    subject          TEXT,
    date_sort        INTEGER,
    date_display     TEXT,
    flags            TEXT,
    in_reply_to      TEXT,
    references_      TEXT,
    thread_id        TEXT,
    snippet          TEXT,
    has_attachments  INTEGER NOT NULL DEFAULT 0,
    body_blob        TEXT,
    raw_blob         TEXT,
    size             INTEGER,
    mtime            INTEGER,
    UNIQUE (account, mailbox, uid)
);

CREATE INDEX messages_message_id ON messages (message_id);

CREATE TABLE blobs (
    hash     TEXT PRIMARY KEY,
    size     INTEGER NOT NULL,
    refcount INTEGER NOT NULL DEFAULT 0 CHECK (refcount >= 0)
);

CREATE TABLE drafts (
    account  TEXT NOT NULL,
    id       TEXT NOT NULL,
    slug     TEXT,
    path     TEXT,
    mtime    INTEGER,
    size     INTEGER,
    status   TEXT,
    to_      TEXT,
    cc       TEXT,
    subject  TEXT,
    date     TEXT,
    snippet  TEXT,
    PRIMARY KEY (account, id)
);

CREATE TABLE outbox (
    id             INTEGER PRIMARY KEY,
    account        TEXT NOT NULL,
    target_mailbox TEXT,
    message_id     TEXT NOT NULL,
    raw_blob       TEXT NOT NULL,
    state          TEXT NOT NULL
                   CHECK (state IN ('pending_send', 'sent_pending_append', 'done', 'failed')),
    attempts       INTEGER NOT NULL DEFAULT 0,
    last_error     TEXT,
    appended_uid   INTEGER,
    created        INTEGER,
    updated        INTEGER
);

CREATE INDEX outbox_state ON outbox (state);

CREATE TABLE sync_cursors (
    mailbox        TEXT PRIMARY KEY,
    uidvalidity    INTEGER,
    highest_modseq INTEGER,
    deltalink      TEXT
);

CREATE TABLE pending_ops (
    id                INTEGER PRIMARY KEY,
    kind              TEXT NOT NULL,
    target_message_id INTEGER,
    payload           TEXT,
    state             TEXT,
    attempts          INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    created           INTEGER
);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject,
    from_,
    body_text,
    content='messages'
);
"#;

/// Create every v1 object and stamp the version. Runs in one transaction so a
/// crash mid-create leaves no half-built file behind.
pub fn create(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!("BEGIN;{SCHEMA_SQL}COMMIT;"))
        .context("creating store schema v1")?;
    set_meta(conn, META_SCHEMA_VERSION, &SCHEMA_VERSION.to_string())?;
    set_meta(conn, META_APP_VERSION, env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

/// Write a `meta` row, replacing any existing value for the key.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        (key, value),
    )
    .with_context(|| format!("writing meta key {key}"))?;
    Ok(())
}

/// Read a `meta` row. `Ok(None)` when the key is absent; an error when the
/// table itself is missing or unreadable.
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// The stamped schema version, or `None` when unstamped or unparseable.
pub fn stamped_version(conn: &Connection) -> Result<Option<i64>> {
    Ok(get_meta(conn, META_SCHEMA_VERSION)?.and_then(|v| v.parse::<i64>().ok()))
}

/// True when every table in [`REQUIRED_TABLES`] exists.
pub fn all_tables_present(conn: &Connection) -> Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type IN ('table') AND name = ?1",
    )?;
    for table in REQUIRED_TABLES {
        let count: i64 = stmt.query_row([table], |row| row.get(0))?;
        if count == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}
