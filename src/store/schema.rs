//! Schema v4 for the per-account store.
//!
//! There is no migrator and there never will be one: the store is a cache in
//! front of IMAP, so a version mismatch is answered by dropping the file and
//! rebuilding it (see [`super::Store::open`]). That is why every statement
//! below is a plain `CREATE`, and why the only stateful thing in the file is
//! the `schema_version` row in `meta`. The one table that is not a cache,
//! `outbox`, is carried across that rebuild rather than recreated from a
//! server that never had it; [`super::rebuild`] owns that half.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Version stamped into `meta.schema_version`. Bump this whenever any
/// statement in [`SCHEMA_SQL`] changes; every existing store is then dropped
/// and rebuilt on the next open, keeping only what [`super::rebuild`] carries
/// across.
///
/// v2 added `message_blobs` for the ingest path (#0037 unit 4a), then
/// `outbox.submission_started_at` and the `html` blob kind in the #0037 review
/// pass. v3 turned `messages_fts` from an external-content index into a
/// contentless one with `contentless_delete=1` (#0038 unit B), which is a
/// change to the virtual table's declaration and therefore a real version
/// bump. v4 is the #0054 bundle: `sync_cursors` splits its UID out of
/// `highest_modseq` into `last_uid`, `sync_cursors` and `pending_ops` gain the
/// `account` column the rest of the schema carries, `pending_ops` gains
/// `updated`, and the two write-only columns (`messages.mtime`,
/// `mailboxes.unread_count`) come out. v5 adds `sync_cursors.arrival_mark`, the
/// one piece of sync state a pass has to hand to the next one (#0072).
pub const SCHEMA_VERSION: i64 = 5;

/// `meta` key holding [`SCHEMA_VERSION`].
pub const META_SCHEMA_VERSION: &str = "schema_version";

/// `meta` key holding the crate version that last created the file. Purely
/// informational (it makes a stale store obvious in a bug report).
pub const META_APP_VERSION: &str = "app_version";

/// Tables that must exist for a store to count as a valid v4 file. Checked on
/// open so that a file which is stamped v4 but structurally incomplete (a
/// half-written create, a hand-edited database) is rebuilt rather than used.
pub const REQUIRED_TABLES: &[&str] = &[
    "meta",
    "mailboxes",
    "messages",
    "message_blobs",
    "blobs",
    "drafts",
    "outbox",
    "sync_cursors",
    "pending_ops",
    "messages_fts",
];

/// `(table, column)` pairs that must exist too.
///
/// The table list alone cannot see a schema that gained a *column* without a
/// version bump, which is exactly what the #0037 review pass did to `outbox`.
/// The v4 bump makes every one of those stores unusable by version anyway, so
/// the list is empty again; it fills up the next time a column is added
/// without a bump.
pub const REQUIRED_COLUMNS: &[(&str, &str)] = &[];

/// The complete schema. Follows the sketch in `docs/plans/data-access-layer.md`.
///
/// Identity notes that are load-bearing rather than cosmetic:
///
/// - Every per-account, row-scoped table carries `account`, although one store
///   file only ever holds one account. The redundancy is deliberate (#0054):
///   it keeps a future shared database a schema change rather than a rewrite
///   of every query, and it is cheaper to carry the column than to explain per
///   table why this one is exempt. `sync_cursors` and `pending_ops` were the
///   two exceptions among those tables and are no longer. The convention does
///   not reach `meta` (file-scoped), `blobs` (content-addressed and shared by
///   construction), `message_blobs` (scoped by its `messages` foreign key) or
///   `messages_fts` (an index keyed on the `messages` rowid).
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
///   `submission_started_at` is the exactly-once marker: it is committed
///   immediately before the SMTP session opens, so a `pending_send` row found
///   on restart says whether the transport was ever entered (see
///   [`crate::outbox`]). `envelope` is who the message is actually going to,
///   which the message bytes cannot answer: lettre drops the `Bcc` header when
///   it builds, so a resumed submission rebuilt from headers would lose every
///   blind recipient.
/// - `blobs` is the refcount index for the content-addressed blob store in
///   [`super::blobs`]. It lives here rather than on disk so a reference can be
///   taken in the same transaction as the `messages` / `outbox` row that
///   carries the hash; the file itself is the disposable side of the pair.
/// - `message_blobs` is the per-message list of blob references: one row per
///   `(message, kind, ordinal)`, where `kind` is `body`, `html`, `raw` or
///   `attachment`. `html` only appears for a backend that returns no RFC822
///   (Graph), where losing the HTML part would mean losing the body the user
///   actually wrote; an IMAP message keeps its `raw` blob and needs no second
///   copy. It is the *source of truth* for refcounting, and
///   `messages.body_blob` / `messages.raw_blob` are a convenience
///   denormalisation of the two singleton kinds. It exists because retention
///   evicts attachment blobs and body blobs on separate horizons, so eviction
///   and refcount auditing must be able to enumerate one row's blob
///   references with a query rather than a parse loop over a JSON column; it
///   is also what lets re-ingest release exactly the references whose content
///   actually changed. `ON DELETE CASCADE` keeps the list from outliving its
///   message, but the *refcount* is only decremented by an explicit
///   [`super::blobs::BlobStore::release`], never by the cascade.
/// - `messages_fts` is *contentless* (`content=''`) with `contentless_delete=1`,
///   over `subject`, `from_` and `body_text`. It was external-content over
///   `messages` until #0038 unit B, which is where the amendment comes from:
///   `messages` has no `body_text` column (the body lives in a blob), so the
///   index was already written by hand and never readable back, and the
///   external-content `'delete'` command needs the *old* column values to undo
///   an entry. Re-ingest could not always produce them (an evicted body blob),
///   so it skipped the delete and left the row indexed twice (the #0037 known
///   issue). `contentless_delete=1` makes `DELETE FROM messages_fts WHERE
///   rowid = ?` legal without any column values, which is the whole fix.
///   What stays true either way: only `rowid`-returning `MATCH` queries are
///   usable (`snippet()`, `highlight()` and selecting a column value all fail),
///   and there is nothing to rebuild from, because a store that loses its index
///   is dropped and refilled by the next sync.
/// - `sync_cursors.arrival_mark` is the only column here a *later* pass reads
///   back: the UID above which the mailbox still owes the store a message the
///   server lists (a bulk move whose destination copies did not fit the
///   download window). Recomputing it from the stored UIDs cannot work, since
///   ingesting the top of the window raises that high-water mark above the
///   stragglers and declares the pass complete, which is exactly the loss the
///   prune gate exists to prevent (#0072). NULL means nothing is owed. Written
///   by the IMAP pull only; the Graph pull downloads by id, not by position,
///   and leaves it NULL.
/// - `sync_cursors` keeps the two resume points apart, because they are not
///   the same number: `last_uid` is the highest UID the recording fetch saw,
///   while `highest_modseq` is a CONDSTORE modification sequence. Both are
///   write-only today. The IMAP pull resumes from `known_uids_with_cursor`
///   (the stored UID set plus uidvalidity), not from `last_uid`; `last_uid` is
///   what #0041 and #0059 will resume from once the pull is a delta rather
///   than a window. `highest_modseq` stays NULL until #0041 issues
///   `CHANGEDSINCE`, as does `deltalink` (Graph's `deltaLink`, waiting on
///   #0042). They were one column until #0054, which stored a UID in
///   `highest_modseq`: a UID-sized number read as a modseq makes the server
///   return nothing and no error, which is the #0004 failure mode with no
///   symptom.
/// - `pending_ops` is the durable mutation queue #0039 will drain; only its
///   shape lives here so far. `updated` is the last-attempt timestamp the
///   backoff is a function of (`updated + backoff_secs(attempts) > now`, the
///   formula `outbox` already uses), which `created` cannot answer.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE mailboxes (
    account      TEXT NOT NULL,
    name         TEXT NOT NULL,
    uidvalidity  INTEGER,
    -- uidnext and exists_count are what the server last reported: sync
    -- diagnostics, not inputs to any decision the client takes.
    uidnext      INTEGER,
    exists_count INTEGER,
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
    -- size will be the retention input: eviction, when it lands (#0060), picks
    -- its victims by it. Nothing evicts anything today.
    size             INTEGER,
    UNIQUE (account, mailbox, uid)
);

CREATE INDEX messages_message_id ON messages (message_id);

CREATE TABLE message_blobs (
    message_row INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    kind        TEXT NOT NULL CHECK (kind IN ('body', 'html', 'raw', 'attachment')),
    ordinal     INTEGER NOT NULL DEFAULT 0,
    hash        TEXT NOT NULL,
    filename    TEXT,
    size        INTEGER,
    PRIMARY KEY (message_row, kind, ordinal)
);

CREATE INDEX message_blobs_hash ON message_blobs (hash);

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
    updated        INTEGER,
    submission_started_at INTEGER,
    envelope       TEXT
);

CREATE INDEX outbox_state ON outbox (state);

CREATE TABLE sync_cursors (
    account        TEXT NOT NULL,
    mailbox        TEXT NOT NULL,
    uidvalidity    INTEGER,
    last_uid       INTEGER,
    -- A CONDSTORE modification sequence and nothing else; NULL until #0041.
    highest_modseq INTEGER,
    -- Graph deltaLink; NULL until #0042.
    deltalink      TEXT,
    -- IMAP arrival mark (#0072): the UID above which this mailbox still owes
    -- the store an arrival, which keeps the prune gate shut. NULL when it does
    -- not, and on the Graph path.
    arrival_mark   INTEGER,
    PRIMARY KEY (account, mailbox)
);

CREATE TABLE pending_ops (
    id                INTEGER PRIMARY KEY,
    account           TEXT NOT NULL,
    kind              TEXT NOT NULL,
    target_message_id INTEGER,
    payload           TEXT,
    state             TEXT,
    attempts          INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    created           INTEGER,
    updated           INTEGER
);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject,
    from_,
    body_text,
    content='',
    contentless_delete=1
);
"#;

/// Create every schema object and stamp the version. Runs in one transaction
/// so a crash mid-create leaves no half-built file behind.
pub fn create(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!("BEGIN;{SCHEMA_SQL}COMMIT;"))
        .with_context(|| format!("creating store schema v{SCHEMA_VERSION}"))?;
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

/// True when every table in [`REQUIRED_TABLES`] and every column in
/// [`REQUIRED_COLUMNS`] exists.
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
    for (table, column) in REQUIRED_COLUMNS {
        if !column_present(conn, table, column)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// True when `table` has a column named `column`.
fn column_present(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2")?;
    let count: i64 = stmt.query_row((table, column), |row| row.get(0))?;
    Ok(count > 0)
}
