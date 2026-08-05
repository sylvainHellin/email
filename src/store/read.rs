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
//! A row carries a `body_blob` hash, not the body. [`load_bodies`] resolves
//! those hashes eagerly for now because the list and the preview share one
//! `EmailEntry`; the lazy-body split is #0038 scope item 5.

use std::collections::HashMap;

use anyhow::{Context, Result};
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
const ROW_COLUMNS: &str = "id, mailbox, uid, message_id, from_, to_, cc, subject, \
                           date_display, flags, has_attachments, body_blob";

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
    })
}

/// Every message in one mailbox, newest first.
///
/// `mailbox` is the role or slug ingest recorded (`inbox`, `sent`, `archive`,
/// or the slugified name of an extra mailbox), which is the same leaf
/// `config::mailbox_dir` builds. A mailbox that was never synced returns an
/// empty list, not an error: it is a mailbox with no mail yet.
pub fn list_mailbox(store: &Store, account: &str, mailbox: &str) -> Result<Vec<MessageRow>> {
    let sql = format!(
        "SELECT {ROW_COLUMNS} FROM messages
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
    let sql = format!(
        "SELECT {ROW_COLUMNS} FROM messages
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
    let sql = format!(
        "SELECT {ROW_COLUMNS} FROM messages
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
    let sql = format!("SELECT {ROW_COLUMNS} FROM messages WHERE id = ?1");
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

/// True when the message carries an iMIP payload, i.e. ingest stored an
/// attachment blob under [`CALENDAR_SIDECAR_NAME`].
///
/// The same predicate the pre-store build expressed as "the file has an
/// `event:` frontmatter block", asked of the store instead. Parsing that ics
/// into a rendered event is the calendar flip, #0038 scope item 6.
pub fn is_invite(store: &Store, message_row: i64) -> Result<bool> {
    let count: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM message_blobs
         WHERE message_row = ?1 AND kind = 'attachment' AND filename = ?2",
        rusqlite::params![message_row, CALENDAR_SIDECAR_NAME],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Resolve the body blobs of a batch of rows, keyed by row id.
///
/// A blob that cannot be read is reported as an empty body and logged, not
/// propagated: the retention sweep is allowed to evict a body, and one evicted
/// body must not blank the whole mailbox list.
pub fn load_bodies(blobs: &BlobStore, rows: &[MessageRow]) -> HashMap<i64, String> {
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let body = row
            .body_blob
            .as_deref()
            .and_then(|h| match BlobHash::parse(h) {
                Ok(h) => Some(h),
                Err(e) => {
                    warn!("[store] message {}: {e:#}", row.id);
                    None
                }
            })
            .and_then(|h| match blobs.read(&h) {
                Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                Err(e) => {
                    warn!("[store] message {}: body blob unreadable: {e:#}", row.id);
                    None
                }
            })
            .unwrap_or_default();
        out.insert(row.id, body);
    }
    out
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
        let rows = list_mailbox(&fx.store, "alice", "inbox").unwrap();
        let bodies = load_bodies(&fx.blobs, &rows);
        assert_eq!(bodies.get(&rows[0].id).unwrap(), "body of hello");
    }

    /// An unreadable body blob yields an empty body for that one row instead
    /// of failing the whole listing: retention is allowed to evict a body.
    #[test]
    fn an_unreadable_body_blob_does_not_blank_the_list() {
        let fx = fixture();
        ingest(&fx, "inbox", 1, &email("kept", "Mon, 01 Jan 2024 09:00:00 +0000"));
        let mut rows = list_mailbox(&fx.store, "alice", "inbox").unwrap();
        rows[0].body_blob = Some("not-a-hash".to_string());
        let bodies = load_bodies(&fx.blobs, &rows);
        assert_eq!(bodies.get(&rows[0].id).unwrap(), "");
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

        assert!(is_invite(&fx.store, id).unwrap());
        let atts = attachments_for(&fx.store, id).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "agenda.pdf");
        assert_eq!(atts[0].size, 8);

        let plain = ingest(&fx, "inbox", 2, &email("plain", "Mon, 01 Jan 2024 09:00:00 +0000"));
        assert!(!is_invite(&fx.store, plain).unwrap());
        assert!(attachments_for(&fx.store, plain).unwrap().is_empty());
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
