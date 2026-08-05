//! The read path: mailbox listings, per-mailbox counts and Message-ID lookups.
//!
//! Everything the TUI and `mp dump-mailbox` show comes from here, and nothing
//! here touches the filesystem tree. There is deliberately no fallback to a
//! directory walk: after [#0037](../../docs/tickets/0037-sqlite-store-engine-skeleton.md)
//! nothing writes `.md`, so a row that is missing is a bug in ingest, and a
//! walk that quietly produced the message anyway would hide it.
//!
//! ## Ordering
//!
//! Listings are ordered in SQL by `date_sort DESC`, with the row `id` as the
//! tiebreaker so two messages that share a timestamp keep a stable, total
//! order across runs. `date_sort` is the unix timestamp ingest derived from
//! the `Date:` header, and `0` is its "unparseable or absent" marker (see
//! [`crate::ingest`]); undated mail therefore sorts last, which is where the
//! pre-store build put it too.
//!
//! ## Blobs
//!
//! A row carries a `body_blob` hash, not the body, and the listing functions
//! never resolve one: a mailbox load is rows only (#0038 scope item 5). The
//! body is fetched when something actually needs it, by [`load_body`] for the
//! previewed message and by [`load_bodies`] for the one batch that needs the
//! whole mailbox at once (the body-search index).
//!
//! Both degrade an unreadable blob to an empty body rather than an error: the
//! retention sweep is allowed to evict a body, and an evicted body must not
//! blank a list or fail a search.
//!
//! ## Invites
//!
//! An invite is a row with an attachment blob named [`CALENDAR_SIDECAR_NAME`],
//! and the listing carries that as a boolean column computed by an `EXISTS`
//! subquery ([`MessageRow::is_invite`]). The badge therefore costs one index
//! probe per row inside the query that was already running, and no blob read:
//! the ics is only fetched where its contents are actually rendered, by
//! [`load_invite_ics`] for the previewed message and by [`list_invites`] for
//! the agenda (#0038 scope item 6).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use log::warn;
use rusqlite::OptionalExtension;

use crate::store::blobs::BlobHash;
use crate::store::{BlobStore, Store};

/// The sidecar name ingest gives the iMIP payload of an invite. It is an
/// attachment blob like any other, but it is not user-facing attachment: the
/// pre-store build kept it beside the message rather than in the
/// `attachments:` list, and the read path keeps that distinction.
pub use crate::parse::CALENDAR_SIDECAR_NAME;

/// One `messages` row, in the shape the read path wants it.
///
/// Field names follow the columns rather than the display layer, so the
/// mapping into an `EmailEntry` or an envelope record stays visible at the
/// call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    /// `messages.id`, the synthetic primary key. This is the identity every
    /// in-process reference holds (see `MessageRef` in the TUI): it survives a
    /// move and a UIDVALIDITY renumbering, which `(mailbox, uid)` does not.
    pub id: i64,
    pub mailbox: String,
    pub uid: i64,
    pub message_id: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    /// The `Date:` header verbatim, as ingest stored it. The display and sort
    /// strings the TUI and the dump use are derived from it by
    /// `tui::app::resolve_date`, so both stacks apply the same rule.
    pub date_display: Option<String>,
    /// IMAP flag string, `\Seen` or empty.
    pub flags: Option<String>,
    pub has_attachments: bool,
    pub body_blob: Option<String>,
    /// True when the row carries an iMIP payload, i.e. an attachment blob
    /// named [`CALENDAR_SIDECAR_NAME`]. Computed in SQL so a mailbox listing
    /// can draw the invite badge without reading a single blob.
    pub is_invite: bool,
}

impl MessageRow {
    /// True when the server flagged the message as read.
    pub fn is_read(&self) -> bool {
        self.flags
            .as_deref()
            .is_some_and(|f| f.contains("\\Seen"))
    }
}

/// The columns [`MessageRow`] needs, in the order [`row_from_sql`] reads them.
///
/// The last one is not a column: it is the invite predicate, evaluated by the
/// `message_blobs` primary key rather than by a blob read. It is spelled once
/// here so every listing answers the same question the same way.
fn row_columns() -> String {
    format!(
        "id, mailbox, uid, message_id, from_, to_, cc, subject, \
         date_display, flags, has_attachments, body_blob, \
         EXISTS (SELECT 1 FROM message_blobs b \
                 WHERE b.message_row = messages.id AND b.kind = 'attachment' \
                   AND b.filename = '{CALENDAR_SIDECAR_NAME}')"
    )
}

fn row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: row.get(0)?,
        mailbox: row.get(1)?,
        uid: row.get(2)?,
        message_id: row.get(3)?,
        from: row.get(4)?,
        to: row.get(5)?,
        cc: row.get(6)?,
        subject: row.get(7)?,
        date_display: row.get(8)?,
        flags: row.get(9)?,
        has_attachments: row.get::<_, i64>(10)? != 0,
        body_blob: row.get(11)?,
        is_invite: row.get::<_, i64>(12)? != 0,
    })
}

/// Every message in one mailbox, newest first.
///
/// `mailbox` is the role or slug ingest recorded (`inbox`, `sent`, `archive`,
/// or the slugified name of an extra mailbox), which is the same leaf
/// `config::mailbox_dir` builds. A mailbox that was never synced returns an
/// empty list, not an error: it is a mailbox with no mail yet.
pub fn list_mailbox(store: &Store, account: &str, mailbox: &str) -> Result<Vec<MessageRow>> {
    let columns = row_columns();
    let sql = format!(
        "SELECT {columns} FROM messages
         WHERE account = ?1 AND mailbox = ?2
         ORDER BY date_sort DESC, id DESC"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let rows = stmt.query_map((account, mailbox), row_from_sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("reading a message row")?);
    }
    Ok(out)
}

/// Every message of one account, newest first within each mailbox.
///
/// Used by the envelope dump, which needs the whole account in one pass and
/// applies its own total order afterwards.
pub fn list_account(store: &Store, account: &str) -> Result<Vec<MessageRow>> {
    let columns = row_columns();
    let sql = format!(
        "SELECT {columns} FROM messages
         WHERE account = ?1
         ORDER BY mailbox ASC, date_sort DESC, id DESC"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let rows = stmt.query_map([account], row_from_sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("reading a message row")?);
    }
    Ok(out)
}

/// Message count per mailbox for one account, as one grouped query.
///
/// Replaces the second directory walk the pre-store build ran at startup
/// (`count_all_emails`). Mailboxes with no rows are absent from the map;
/// callers index by name and treat a miss as zero, which keeps the result
/// aligned with a mailbox list that includes never-synced mailboxes.
pub fn mailbox_counts(store: &Store, account: &str) -> Result<HashMap<String, usize>> {
    let mut stmt = store.conn().prepare(
        "SELECT mailbox, COUNT(*) FROM messages WHERE account = ?1 GROUP BY mailbox",
    )?;
    let rows = stmt.query_map([account], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (mailbox, count) = row.context("reading a mailbox count")?;
        out.insert(mailbox, count.max(0) as usize);
    }
    Ok(out)
}

/// The rows an account holds for one `Message-ID`, in `(mailbox, uid)` order.
///
/// This is the store-side replacement for the `message_id_index` the TUI used
/// to build by walking every mailbox directory at startup: the same question,
/// answered by the non-unique `messages_message_id` index at the moment it is
/// asked. More than one row is normal and not an error, because the same
/// message can sit in several mailboxes (a copy, an archived original).
pub fn find_by_message_id(
    store: &Store,
    account: &str,
    message_id: &str,
) -> Result<Vec<MessageRow>> {
    let columns = row_columns();
    let sql = format!(
        "SELECT {columns} FROM messages
         WHERE account = ?1 AND message_id = ?2
         ORDER BY mailbox ASC, uid ASC"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let rows = stmt.query_map((account, message_id), row_from_sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("reading a message row")?);
    }
    Ok(out)
}

/// One row addressed by its synthetic id.
pub fn find_by_id(store: &Store, id: i64) -> Result<Option<MessageRow>> {
    let columns = row_columns();
    let sql = format!("SELECT {columns} FROM messages WHERE id = ?1");
    let row = store
        .conn()
        .query_row(&sql, [id], row_from_sql)
        .optional()
        .context("reading a message row by id")?;
    Ok(row)
}

/// One attachment of a message: the name it was sent under and its byte length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRow {
    pub name: String,
    pub size: u64,
    pub hash: String,
}

/// The user-facing attachments of one message, in ingest order.
///
/// The iMIP sidecar is excluded: ingest stores it as an attachment blob so the
/// read path can find an invite without re-walking the MIME tree, but the
/// pre-store build never listed it as an attachment either, and surfacing it
/// as one would be a visible change rather than a storage detail. Use
/// [`is_invite`] for that bit.
pub fn attachments_for(store: &Store, message_row: i64) -> Result<Vec<AttachmentRow>> {
    let mut stmt = store.conn().prepare(
        "SELECT filename, size, hash FROM message_blobs
         WHERE message_row = ?1 AND kind = 'attachment'
         ORDER BY ordinal ASC",
    )?;
    let rows = stmt.query_map([message_row], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (name, size, hash) = row.context("reading an attachment reference")?;
        let Some(name) = name else { continue };
        if name == CALENDAR_SIDECAR_NAME {
            continue;
        }
        out.push(AttachmentRow {
            name,
            size: size.unwrap_or(0).max(0) as u64,
            hash,
        });
    }
    Ok(out)
}

/// Every invite of one account: the row plus the hash of its ics blob.
///
/// This is the agenda's and the reconciler's source (#0038 scope item 6). It
/// replaced a walk of every `.md` under the account root, so the shape is
/// deliberately the whole account in one query: an invite is a rare row, and
/// both callers need every mailbox at once to collapse the Inbox / Sent /
/// Archive copies of one event into a single agenda row.
///
/// Ordered by `(mailbox, uid)`, which is the identity tiebreak both callers
/// use once sequence and `DTSTAMP` have tied, so the result is stable across
/// runs without a sort at the call site.
pub fn list_invites(store: &Store, account: &str) -> Result<Vec<(MessageRow, String)>> {
    let columns = row_columns();
    let sql = format!(
        "SELECT {columns}, b.hash FROM messages
         JOIN message_blobs b ON b.message_row = messages.id
         WHERE account = ?1 AND b.kind = 'attachment' AND b.filename = ?2
         ORDER BY mailbox ASC, uid ASC"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let rows = stmt.query_map((account, CALENDAR_SIDECAR_NAME), |row| {
        Ok((row_from_sql(row)?, row.get::<_, String>(13)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("reading an invite row")?);
    }
    Ok(out)
}

/// The raw ics bytes of one message, or `None` when it carries no iMIP
/// payload (or the blob is unreadable, which is logged).
///
/// One row, one blob: this is what the preview pane calls for the message
/// under the cursor, so the event card is paid for by the message on screen
/// and not by the mailbox behind it.
pub fn load_invite_ics(store: &Store, blobs: &BlobStore, message_row: i64) -> Option<Vec<u8>> {
    let hash: String = store
        .conn()
        .query_row(
            "SELECT hash FROM message_blobs
             WHERE message_row = ?1 AND kind = 'attachment' AND filename = ?2",
            rusqlite::params![message_row, CALENDAR_SIDECAR_NAME],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or_else(|e| {
            warn!("[store] reading the ics hash of message {message_row}: {e:#}");
            None
        })?;
    read_blob(blobs, message_row, &hash)
}

/// Read one blob by its hash string, degrading to `None` with a log line.
pub fn read_blob(blobs: &BlobStore, message_row: i64, hash: &str) -> Option<Vec<u8>> {
    let hash = match BlobHash::parse(hash) {
        Ok(h) => h,
        Err(e) => {
            warn!("[store] message {message_row}: {e:#}");
            return None;
        }
    };
    match blobs.read(&hash) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            warn!("[store] message {message_row}: blob unreadable: {e:#}");
            None
        }
    }
}

/// Materialise one message's attachments into `dest`, returning the files
/// written.
///
/// Attachments are blobs in the account's content-addressed store (#0037), so
/// this is the one place that turns them back into files: `mp save`, `mp open`
/// and the forward path that needs real paths in a draft's `attachments:`
/// list all come through here.
///
/// A missing blob is an error rather than a skipped file: a forward that
/// silently dropped an attachment would be a worse answer than one that says
/// which blob is gone.
pub fn materialise_attachments(
    store: &Store,
    blobs: &BlobStore,
    row_id: i64,
    dest: &Path,
) -> Result<Vec<PathBuf>> {
    let attachments = attachments_for(store, row_id)?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut written = Vec::new();
    for att in attachments {
        let Some(bytes) = read_blob(blobs, row_id, &att.hash) else {
            return Err(anyhow!(
                "the blob for attachment {} is missing or unreadable",
                att.name
            ));
        };
        let out = dest.join(&att.name);
        std::fs::write(&out, &bytes).with_context(|| format!("writing {}", out.display()))?;
        written.push(out);
    }
    Ok(written)
}

/// The body of one message, or `None` when the row itself is gone.
///
/// `Some("")` and `None` are different answers: the first is a row whose body
/// blob is unreadable (evicted, or never written), the second is a reference
/// to a row that no longer exists, which is a caller-side staleness bug rather
/// than a storage state.
pub fn load_body(store: &Store, blobs: &BlobStore, id: i64) -> Option<String> {
    let hash: Option<String> = store
        .conn()
        .query_row("SELECT body_blob FROM messages WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .unwrap_or_else(|e| {
            warn!("[store] reading the body hash of message {id}: {e:#}");
            None
        })?;
    Some(blob_text(blobs, id, hash.as_deref()))
}

/// The HTML rendition of one message, or `None` when it has none.
///
/// This is what the quoted companion of a reply or a forward is built from:
/// the pre-store build wrote a `.html` file beside every received `.md` and
/// `mp reply` copied it, so without this the store build would send a
/// plain-text-only quote where the file build sent the sender's own markup.
///
/// Two shapes carry it, because ingest stores whichever it was given: the
/// Graph path has no RFC822 and writes an `html` blob of its own, the IMAP
/// path writes the raw message and the HTML part lives inside it. The blob is
/// preferred because it needs no parse; the raw is parsed only when there is
/// no blob, and only for the one message being replied to.
pub fn load_html(store: &Store, blobs: &BlobStore, message_row: i64) -> Option<String> {
    let hash: Option<String> = store
        .conn()
        .query_row(
            "SELECT hash FROM message_blobs
             WHERE message_row = ?1 AND kind = 'html' ORDER BY ordinal LIMIT 1",
            [message_row],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or_else(|e| {
            warn!("[store] reading the html hash of message {message_row}: {e:#}");
            None
        });
    if let Some(hash) = hash {
        if let Some(bytes) = read_blob(blobs, message_row, &hash) {
            return Some(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    let raw_hash: Option<String> = store
        .conn()
        .query_row(
            "SELECT hash FROM message_blobs
             WHERE message_row = ?1 AND kind = 'raw' ORDER BY ordinal LIMIT 1",
            [message_row],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or_else(|e| {
            warn!("[store] reading the raw hash of message {message_row}: {e:#}");
            None
        });
    let raw = read_blob(blobs, message_row, raw_hash.as_deref()?)?;
    crate::parse::parse_rfc822_to_fetched_email(&raw).and_then(|email| email.html_body)
}

/// Resolve the body blobs of a batch of messages, keyed by `messages.id`.
///
/// One prepared statement, one blob read per id: the batch shape exists for
/// the body-search index, which needs every body of a mailbox at once and is
/// built once per list generation rather than per keystroke. An id with no row
/// is absent from the map; an unreadable blob maps to an empty body.
pub fn load_bodies(store: &Store, blobs: &BlobStore, ids: &[i64]) -> HashMap<i64, String> {
    let mut out = HashMap::with_capacity(ids.len());
    let mut stmt = match store
        .conn()
        .prepare("SELECT body_blob FROM messages WHERE id = ?1")
    {
        Ok(stmt) => stmt,
        Err(e) => {
            warn!("[store] preparing the body-blob query: {e:#}");
            return out;
        }
    };
    for &id in ids {
        let hash: Option<Option<String>> = stmt
            .query_row([id], |row| row.get::<_, Option<String>>(0))
            .optional()
            .unwrap_or_else(|e| {
                warn!("[store] reading the body hash of message {id}: {e:#}");
                None
            });
        let Some(hash) = hash else { continue };
        out.insert(id, blob_text(blobs, id, hash.as_deref()));
    }
    out
}

/// Read one body blob as text, degrading to the empty string.
///
/// A blob that cannot be read is reported as an empty body and logged, not
/// propagated: the retention sweep is allowed to evict a body, and one evicted
/// body must not blank the whole mailbox list.
fn blob_text(blobs: &BlobStore, id: i64, hash: Option<&str>) -> String {
    hash.and_then(|h| read_blob(blobs, id, h))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{ingest_message, IngestInput};
    use crate::parse::FetchedEmail;
    use tempfile::TempDir;

    /// A store plus its blob store, both under one temp directory.
    struct Fixture {
        _dir: TempDir,
        store: Store,
        blobs: BlobStore,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        Fixture {
            _dir: dir,
            store,
            blobs,
        }
    }

    fn email(subject: &str, date: &str) -> FetchedEmail {
        FetchedEmail {
            from: "Ada Lovelace <ada@example.com>".into(),
            to: "b@example.com".into(),
            cc: None,
            subject: subject.into(),
            date: date.into(),
            body_text: format!("body of {subject}"),
            html_body: None,
            has_attachments: false,
            message_id: Some(format!("<{subject}@example.com>")),
            attachments: Vec::new(),
            is_read: false,
            calendar_ics: None,
            event: None,
        }
    }

    /// Ingest one message through the real ingest API, so the fixture rows are
    /// exactly the rows the sync path writes.
    fn ingest(fx: &Fixture, mailbox: &str, uid: i64, email: &FetchedEmail) -> i64 {
        ingest_message(
            &fx.store,
            &fx.blobs,
            &IngestInput {
                account: "alice",
                mailbox,
                uid,
                email,
                raw: None,
            },
        )
        .unwrap()
        .row_id
    }

    #[test]
    fn a_mailbox_lists_newest_first() {
        let fx = fixture();
        ingest(&fx, "inbox", 1, &email("older", "Mon, 01 Jan 2024 09:00:00 +0000"));
        ingest(&fx, "inbox", 2, &email("newer", "Mon, 01 Jan 2024 17:00:00 +0000"));
        ingest(&fx, "archive", 3, &email("elsewhere", "Mon, 01 Jan 2024 12:00:00 +0000"));

        let rows = list_mailbox(&fx.store, "alice", "inbox").unwrap();
        let subjects: Vec<_> = rows.iter().map(|r| r.subject.clone().unwrap()).collect();
        assert_eq!(subjects, vec!["newer", "older"]);
    }

    /// Undated mail sorts last rather than disappearing, and two runs agree:
    /// the `id` tiebreaker makes the order total even when `date_sort` ties.
    #[test]
    fn ordering_is_total_and_undated_mail_sorts_last() {
        let fx = fixture();
        ingest(&fx, "inbox", 1, &email("tie-a", "Mon, 01 Jan 2024 09:00:00 +0000"));
        ingest(&fx, "inbox", 2, &email("tie-b", "Mon, 01 Jan 2024 09:00:00 +0000"));
        ingest(&fx, "inbox", 3, &email("undated", "not a date"));

        let first: Vec<_> = list_mailbox(&fx.store, "alice", "inbox")
            .unwrap()
            .into_iter()
            .map(|r| r.subject.unwrap())
            .collect();
        let second: Vec<_> = list_mailbox(&fx.store, "alice", "inbox")
            .unwrap()
            .into_iter()
            .map(|r| r.subject.unwrap())
            .collect();
        assert_eq!(first, second, "the order must not vary between runs");
        assert_eq!(first, vec!["tie-b", "tie-a", "undated"]);
    }

    #[test]
    fn counts_group_by_mailbox_and_omit_empty_ones() {
        let fx = fixture();
        ingest(&fx, "inbox", 1, &email("a", "Mon, 01 Jan 2024 09:00:00 +0000"));
        ingest(&fx, "inbox", 2, &email("b", "Mon, 01 Jan 2024 09:00:00 +0000"));
        ingest(&fx, "archive", 1, &email("c", "Mon, 01 Jan 2024 09:00:00 +0000"));

        let counts = mailbox_counts(&fx.store, "alice").unwrap();
        assert_eq!(counts.get("inbox"), Some(&2));
        assert_eq!(counts.get("archive"), Some(&1));
        assert_eq!(counts.get("sent"), None, "an empty mailbox has no row");
        assert!(mailbox_counts(&fx.store, "nobody").unwrap().is_empty());
    }

    /// The cross-mailbox lookup the deleted `build_message_id_index` startup
    /// walk used to answer. The same message in two mailboxes is two rows, and
    /// both come back.
    #[test]
    fn a_message_id_resolves_across_mailboxes() {
        let fx = fixture();
        let mut e = email("copy", "Mon, 01 Jan 2024 09:00:00 +0000");
        e.message_id = Some("<shared@example.com>".into());
        ingest(&fx, "inbox", 1, &e);
        ingest(&fx, "archive", 7, &e);

        let hits = find_by_message_id(&fx.store, "alice", "<shared@example.com>").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].mailbox, "archive");
        assert_eq!(hits[1].mailbox, "inbox");
        assert!(find_by_message_id(&fx.store, "alice", "<nope@x>")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn bodies_come_back_from_the_blob_store() {
        let fx = fixture();
        ingest(&fx, "inbox", 1, &email("hello", "Mon, 01 Jan 2024 09:00:00 +0000"));
        ingest(&fx, "inbox", 2, &email("second", "Mon, 01 Jan 2024 10:00:00 +0000"));
        let rows = list_mailbox(&fx.store, "alice", "inbox").unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

        let bodies = load_bodies(&fx.store, &fx.blobs, &ids);
        assert_eq!(bodies.len(), 2);
        for row in &rows {
            let expected = format!("body of {}", row.subject.as_deref().unwrap());
            assert_eq!(bodies.get(&row.id).unwrap(), &expected);
            assert_eq!(
                load_body(&fx.store, &fx.blobs, row.id).unwrap(),
                expected,
                "the single read must agree with the batch"
            );
        }
    }

    /// The quoted companion of a reply or a forward comes from here, so both
    /// shapes ingest writes have to answer: the Graph path's own `html` blob
    /// and the IMAP path's raw message, whose HTML part is inside it.
    #[test]
    fn the_html_rendition_is_read_from_the_blob_or_from_the_raw_message() {
        let fx = fixture();

        // Graph shape: no RFC822, so ingest wrote an `html` blob.
        let mut graph = email("graph", "Mon, 01 Jan 2024 09:00:00 +0000");
        graph.html_body = Some("<p>markup the sender wrote</p>".to_string());
        let graph_id = ingest(&fx, "inbox", 1, &graph);
        assert_eq!(
            load_html(&fx.store, &fx.blobs, graph_id).as_deref(),
            Some("<p>markup the sender wrote</p>")
        );

        // IMAP shape: the raw message carries the HTML part.
        let raw = b"From: ada@example.com\r\nTo: b@example.com\r\nSubject: raw\r\n\
Message-ID: <raw@example.com>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\n\
Content-Type: text/html; charset=utf-8\r\n\r\n<p>html inside the raw</p>\r\n";
        let raw_id = ingest_message(
            &fx.store,
            &fx.blobs,
            &IngestInput {
                account: "alice",
                mailbox: "inbox",
                uid: 2,
                email: &crate::parse::parse_rfc822_to_fetched_email(raw).unwrap(),
                raw: Some(raw),
            },
        )
        .unwrap()
        .row_id;
        let html = load_html(&fx.store, &fx.blobs, raw_id).expect("the raw message has html");
        assert!(html.contains("html inside the raw"), "{html}");

        // A plain-text message has none, and says so rather than inventing one.
        let plain_id = ingest(&fx, "inbox", 3, &email("plain", "Mon, 01 Jan 2024 11:00:00 +0000"));
        assert_eq!(load_html(&fx.store, &fx.blobs, plain_id), None);
    }

    /// A reference to a row that no longer exists is `None`, not an empty
    /// body: the caller is holding a stale id, which is a different problem
    /// from an evicted blob.
    #[test]
    fn a_missing_row_reads_back_as_none() {
        let fx = fixture();
        let id = ingest(&fx, "inbox", 1, &email("x", "Mon, 01 Jan 2024 09:00:00 +0000"));
        assert_eq!(load_body(&fx.store, &fx.blobs, id + 999), None);
        assert!(load_bodies(&fx.store, &fx.blobs, &[id + 999]).is_empty());
    }

    /// An unreadable body blob yields an empty body for that one row instead
    /// of failing the whole listing: retention is allowed to evict a body.
    #[test]
    fn an_unreadable_body_blob_does_not_blank_the_list() {
        let fx = fixture();
        let id = ingest(&fx, "inbox", 1, &email("kept", "Mon, 01 Jan 2024 09:00:00 +0000"));
        fx.store
            .conn()
            .execute("UPDATE messages SET body_blob = 'not-a-hash' WHERE id = ?1", [id])
            .unwrap();

        assert_eq!(load_body(&fx.store, &fx.blobs, id).unwrap(), "");
        assert_eq!(load_bodies(&fx.store, &fx.blobs, &[id]).get(&id).unwrap(), "");
        assert_eq!(
            list_mailbox(&fx.store, "alice", "inbox").unwrap().len(),
            1,
            "the listing itself is untouched by the unreadable blob"
        );
    }

    /// The iMIP sidecar is an attachment blob but not a user-facing
    /// attachment, exactly as the pre-store build had it: it lived in the
    /// `_attachments/` directory and never in the `attachments:` list.
    #[test]
    fn the_invite_sidecar_is_a_flag_not_an_attachment() {
        let fx = fixture();
        let mut e = email("invite", "Mon, 01 Jan 2024 09:00:00 +0000");
        e.calendar_ics = Some("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into());
        e.attachments = vec![crate::parse::AttachmentData {
            filename: "agenda.pdf".into(),
            content: b"%PDF-1.4".to_vec(),
            content_id: None,
        }];
        let id = ingest(&fx, "inbox", 1, &e);

        assert!(find_by_id(&fx.store, id).unwrap().unwrap().is_invite);
        let atts = attachments_for(&fx.store, id).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "agenda.pdf");
        assert_eq!(atts[0].size, 8);

        let plain = ingest(&fx, "inbox", 2, &email("plain", "Mon, 01 Jan 2024 09:00:00 +0000"));
        assert!(!find_by_id(&fx.store, plain).unwrap().unwrap().is_invite);
        assert!(attachments_for(&fx.store, plain).unwrap().is_empty());
    }

    /// The invite flag rides on the listing itself, so the badge costs no
    /// blob read: the whole mailbox comes back with the predicate answered.
    #[test]
    fn the_listing_carries_the_invite_flag() {
        let fx = fixture();
        let mut e = email("invite", "Mon, 01 Jan 2024 10:00:00 +0000");
        e.calendar_ics = Some("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into());
        ingest(&fx, "inbox", 1, &e);
        ingest(&fx, "inbox", 2, &email("plain", "Mon, 01 Jan 2024 09:00:00 +0000"));

        let rows = list_mailbox(&fx.store, "alice", "inbox").unwrap();
        assert_eq!(rows[0].subject.as_deref(), Some("invite"));
        assert!(rows[0].is_invite);
        assert!(!rows[1].is_invite);
    }

    /// The agenda's source: every invite of the account, whatever mailbox it
    /// sits in, with the bytes that came off the wire.
    #[test]
    fn invites_come_back_across_mailboxes_with_their_ics() {
        let fx = fixture();
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n";
        let mut e = email("invite", "Mon, 01 Jan 2024 09:00:00 +0000");
        e.calendar_ics = Some(ics.into());
        ingest(&fx, "inbox", 1, &e);
        ingest(&fx, "sent", 4, &e);
        ingest(&fx, "inbox", 2, &email("plain", "Mon, 01 Jan 2024 09:00:00 +0000"));

        let invites = list_invites(&fx.store, "alice").unwrap();
        let boxes: Vec<&str> = invites.iter().map(|(r, _)| r.mailbox.as_str()).collect();
        assert_eq!(boxes, vec!["inbox", "sent"], "ordered by (mailbox, uid)");
        for (row, hash) in &invites {
            assert_eq!(
                read_blob(&fx.blobs, row.id, hash).unwrap(),
                ics.as_bytes(),
                "the ics bytes are the ones ingest stored"
            );
            assert_eq!(
                load_invite_ics(&fx.store, &fx.blobs, row.id).unwrap(),
                ics.as_bytes(),
                "the single read must agree with the batch"
            );
        }
        assert!(list_invites(&fx.store, "nobody").unwrap().is_empty());
    }

    /// A message with no iMIP payload has no ics to load, and an unreadable
    /// blob degrades to the same `None` rather than to a panic.
    #[test]
    fn a_message_without_an_ics_reads_back_as_none() {
        let fx = fixture();
        let plain = ingest(&fx, "inbox", 1, &email("plain", "Mon, 01 Jan 2024 09:00:00 +0000"));
        assert_eq!(load_invite_ics(&fx.store, &fx.blobs, plain), None);
        assert_eq!(read_blob(&fx.blobs, plain, "not-a-hash"), None);
    }

    #[test]
    fn a_row_is_addressable_by_its_synthetic_id() {
        let fx = fixture();
        let id = ingest(&fx, "inbox", 1, &email("x", "Mon, 01 Jan 2024 09:00:00 +0000"));
        let row = find_by_id(&fx.store, id).unwrap().unwrap();
        assert_eq!(row.subject.as_deref(), Some("x"));
        assert_eq!(find_by_id(&fx.store, id + 999).unwrap(), None);
    }

    #[test]
    fn the_seen_flag_reads_back_off_the_row() {
        let fx = fixture();
        let mut e = email("read", "Mon, 01 Jan 2024 09:00:00 +0000");
        e.is_read = true;
        ingest(&fx, "inbox", 1, &e);
        ingest(&fx, "inbox", 2, &email("unread", "Mon, 01 Jan 2024 08:00:00 +0000"));

        let rows = list_mailbox(&fx.store, "alice", "inbox").unwrap();
        assert!(rows[0].is_read(), "the \\Seen row must read back as read");
        assert!(!rows[1].is_read());
    }
}
