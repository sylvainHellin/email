//! Store-only ingest: fetched message to one `messages` row plus its blobs.
//!
//! This is the only writer on the receive path, and it writes no `.md`.
//! Everything a fetched message carries ends up in exactly two places: the
//! per-account `store.sqlite3` row and the content-addressed blob store.
//!
//! ## Bodies, and the HTML the Graph path would otherwise lose
//!
//! Every message stores its plain-text body as a `body` blob. An IMAP message
//! also stores the RFC822 bytes as a `raw` blob, so any richer rendition can be
//! re-derived from them. Graph never returns RFC822, so its HTML part is
//! stored as an `html` blob instead; without it the HTML the sender wrote is
//! gone the moment the message is ingested.
//!
//! ## Transaction shape
//!
//! Blob *files* are written before the transaction opens and blob *references*
//! are taken inside it, which is the contract documented on
//! [`BlobStore::acquire`](crate::store::BlobStore::acquire): an unreferenced
//! blob file is a harmless orphan that a sweep reclaims, while a row pointing
//! at a missing blob is a hole in the read path. One transaction per message,
//! so a crash leaves whole messages behind, never half of one.
//!
//! ## Identity, and what re-ingest does
//!
//! Identity is `(account, mailbox, uid)`. Re-ingesting the same UID is an
//! UPSERT on that unique constraint: the row keeps its `id`, its thread
//! assignment and any local-only state, and blob references are re-pointed
//! only for the kinds whose content actually changed (new references are
//! acquired before old ones are released, so a hash shared by both versions
//! never touches zero and never gets unlinked mid-transaction).
//!
//! After a UIDVALIDITY reset the same message reappears under a new UID. The
//! non-unique `messages_message_id` index finds the prior row in the same
//! mailbox, and ingest updates its `uid` in place rather than inserting a
//! second row, so the thread assignment and the blob references survive the
//! renumbering.
//!
//! ## Synthesised Message-ID
//!
//! A message with no `Message-ID` header gets `sha256-<hex16>@local.invalid`,
//! where `<hex16>` is the first 16 lowercase hex characters of a SHA-256 over
//! bytes that are fixed as follows, so the same message always synthesises the
//! same id:
//!
//! - when the ingest holds the raw RFC822 bytes (every IMAP fetch), the digest
//!   is over those bytes exactly as they came off the wire;
//! - when it does not (the Graph path never returns RFC822), the digest is
//!   over the canonical envelope string
//!   `from \n to \n cc \n subject \n date \n body_text`, UTF-8, with `cc`
//!   rendered as the empty string when absent and no trailing newline.
//!
//! ## FTS
//!
//! `messages_fts` is contentless with `contentless_delete=1` (see the schema
//! doc comment), so there is nothing to rebuild from and ingest maintains the
//! index explicitly: delete the previous entry by rowid, insert the new one,
//! both inside the message's transaction.
//!
//! The delete needs the rowid and nothing else, which is what fixes the #0037
//! known issue: while the index was external-content, undoing an entry meant
//! replaying the *old* column values, and re-ingest of a message whose
//! previous body blob had been evicted could not produce them, so it skipped
//! the delete and left the row indexed twice.

use anyhow::{Context, Result};
use log::{info, warn};
use rusqlite::{OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

use crate::parse::FetchedEmail;
use crate::store::blobs::BlobHash;
use crate::store::{BlobStore, Store};
use crate::timing::TimingSpan;
use crate::types::MessageFlags;

/// How many characters of the body are kept in the `snippet` column.
const SNIPPET_CHARS: usize = 200;

/// One message handed to [`ingest_message`].
pub struct IngestInput<'a> {
    pub account: &'a str,
    pub mailbox: &'a str,
    /// IMAP UID, or the synthetic uid the Graph path derives from the Graph
    /// message id (see [`graph_uid`]).
    pub uid: i64,
    /// The parsed message.
    pub email: &'a FetchedEmail,
    /// Raw RFC822 bytes when the backend has them (IMAP); `None` for Graph,
    /// whose API never returns the original MIME.
    pub raw: Option<&'a [u8]>,
}

/// What ingest did with one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    /// `messages.id` of the row that now holds the message.
    pub row_id: i64,
    /// The `Message-ID`, synthesised when the header was missing.
    pub message_id: String,
    /// Thread the message was assigned to.
    pub thread_id: String,
    /// True when a new row was inserted, false when an existing row was updated.
    pub inserted: bool,
    /// True when the row was found through the `message_id` index under a
    /// different UID, i.e. a UIDVALIDITY reset was absorbed.
    pub uid_rebound: bool,
}

/// The blob kinds a message row can reference.
const KIND_BODY: &str = "body";
const KIND_HTML: &str = "html";
const KIND_RAW: &str = "raw";
const KIND_ATTACHMENT: &str = "attachment";

/// A blob reference about to be written for one message.
struct BlobRef {
    kind: &'static str,
    ordinal: i64,
    hash: BlobHash,
    filename: Option<String>,
    size: u64,
}

/// Ingest one message into the store.
///
/// Writes every blob file first, then does all database work in a single
/// transaction. See the module docs for the identity and FTS contracts.
pub fn ingest_message(
    store: &Store,
    blobs: &BlobStore,
    input: &IngestInput<'_>,
) -> Result<IngestOutcome> {
    let mut span = TimingSpan::with_context(
        "store_ingest",
        format!("{}/{}#{}", input.account, input.mailbox, input.uid),
    );

    let email = input.email;
    let message_id = resolve_message_id(email, input.raw);
    let body_bytes = email.body_text.as_bytes();

    // Blob files first: outside the transaction, idempotent, content-addressed.
    let mut refs: Vec<BlobRef> = Vec::new();
    refs.push(BlobRef {
        kind: KIND_BODY,
        ordinal: 0,
        hash: blobs.write(body_bytes)?,
        filename: None,
        size: body_bytes.len() as u64,
    });
    match (input.raw, email.html_body.as_deref()) {
        (Some(raw), _) => {
            refs.push(BlobRef {
                kind: KIND_RAW,
                ordinal: 0,
                hash: blobs.write(raw)?,
                filename: None,
                size: raw.len() as u64,
            });
        }
        (None, Some(html)) => {
            // No RFC822 to fall back on (the Graph path), so the HTML part is
            // kept as its own blob: it is the body the sender actually wrote,
            // and the plain-text rendition beside it is a downgrade the read
            // path should not be forced to display (#0038).
            let bytes = html.as_bytes();
            refs.push(BlobRef {
                kind: KIND_HTML,
                ordinal: 0,
                hash: blobs.write(bytes)?,
                filename: None,
                size: bytes.len() as u64,
            });
        }
        (None, None) => {}
    }
    let mut ordinal = 0i64;
    if let Some(ics) = email.calendar_ics.as_deref() {
        // The iMIP payload is an attachment blob like any other, under the
        // sidecar name the rest of the codebase already knows it by, so the
        // read path can find an invite without re-walking the MIME tree.
        refs.push(BlobRef {
            kind: KIND_ATTACHMENT,
            ordinal,
            hash: blobs.write(ics)?,
            filename: Some(crate::parse::CALENDAR_SIDECAR_NAME.to_string()),
            size: ics.len() as u64,
        });
        ordinal += 1;
    }
    for att in &email.attachments {
        refs.push(BlobRef {
            kind: KIND_ATTACHMENT,
            ordinal,
            hash: blobs.write(&att.content)?,
            filename: Some(att.filename.clone()),
            size: att.content.len() as u64,
        });
        ordinal += 1;
    }
    span.mark("blobs_written");

    let (in_reply_to, references) = input.raw.map(threading_headers).unwrap_or((None, None));

    let tx = store
        .conn()
        .unchecked_transaction()
        .context("opening ingest transaction")?;
    let outcome = ingest_in_tx(&tx, blobs, input, &message_id, &in_reply_to, &references, &refs)?;
    tx.commit().context("committing ingest transaction")?;
    span.mark("committed");

    Ok(outcome)
}

/// The database half of [`ingest_message`], so the transaction boundary stays
/// visible in one place.
#[allow(clippy::too_many_arguments)]
fn ingest_in_tx(
    tx: &Transaction<'_>,
    blobs: &BlobStore,
    input: &IngestInput<'_>,
    message_id: &str,
    in_reply_to: &Option<String>,
    references: &Option<String>,
    refs: &[BlobRef],
) -> Result<IngestOutcome> {
    let email = input.email;

    // 1. Find the row this message belongs in: by identity first, then
    //    through the message_id index (UIDVALIDITY reset). Its id and its
    //    thread are all that is carried forward; the old envelope values are
    //    not needed anywhere since the FTS delete became rowid-only.
    let existing: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT id, thread_id
             FROM messages WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
            (input.account, input.mailbox, input.uid),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("looking up the message by identity")?;

    let mut uid_rebound = false;
    let existing = match existing {
        Some(found) => Some(found),
        None => {
            let by_mid = tx
                .query_row(
                    "SELECT id, thread_id
                     FROM messages
                     WHERE account = ?1 AND mailbox = ?2 AND message_id = ?3
                     ORDER BY id LIMIT 1",
                    (input.account, input.mailbox, message_id),
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .context("looking up the message through the message_id index")?;
            uid_rebound = by_mid.is_some();
            by_mid
        }
    };

    // 2. Thread assignment: an existing row keeps the thread it was put in,
    //    a new one inherits from its parent and otherwise starts its own.
    let thread_id = match existing.as_ref().and_then(|(_, thread)| thread.clone()) {
        Some(thread) => thread,
        None => resolve_thread_id(tx, input.account, message_id, in_reply_to, references)?,
    };

    let date_sort = date_sort_for(&email.date);
    let snippet = snippet_for(&email.body_text);
    let flags = email.flags.to_flag_string();
    let body_blob = refs
        .iter()
        .find(|r| r.kind == KIND_BODY)
        .map(|r| r.hash.as_str().to_string());
    let raw_blob = refs
        .iter()
        .find(|r| r.kind == KIND_RAW)
        .map(|r| r.hash.as_str().to_string());
    let size: i64 = input
        .raw
        .map(|r| r.len() as i64)
        .unwrap_or_else(|| refs.iter().map(|r| r.size as i64).sum());
    let has_attachments =
        email.has_attachments || refs.iter().any(|r| r.kind == KIND_ATTACHMENT);

    // 3. Write the row.
    let row_id = match existing.as_ref() {
        Some((id, _)) => {
            tx.execute(
                "UPDATE messages SET
                    uid = ?2, message_id = ?3, from_ = ?4, to_ = ?5, cc = ?6, subject = ?7,
                    date_sort = ?8, date_display = ?9, flags = ?10, in_reply_to = ?11,
                    references_ = ?12, thread_id = ?13, snippet = ?14, has_attachments = ?15,
                    body_blob = ?16, raw_blob = ?17, size = ?18
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    input.uid,
                    message_id,
                    email.from,
                    email.to,
                    email.cc,
                    email.subject,
                    date_sort,
                    email.date,
                    flags,
                    in_reply_to,
                    references,
                    thread_id,
                    snippet,
                    has_attachments as i64,
                    body_blob,
                    raw_blob,
                    size,
                ],
            )
            .context("updating the message row")?;
            *id
        }
        None => {
            tx.execute(
                "INSERT INTO messages (
                    account, mailbox, uid, message_id, from_, to_, cc, subject,
                    date_sort, date_display, flags, in_reply_to, references_, thread_id,
                    snippet, has_attachments, body_blob, raw_blob, size
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                    ?17, ?18, ?19
                 )",
                rusqlite::params![
                    input.account,
                    input.mailbox,
                    input.uid,
                    message_id,
                    email.from,
                    email.to,
                    email.cc,
                    email.subject,
                    date_sort,
                    email.date,
                    flags,
                    in_reply_to,
                    references,
                    thread_id,
                    snippet,
                    has_attachments as i64,
                    body_blob,
                    raw_blob,
                    size,
                ],
            )
            .context("inserting the message row")?;
            tx.last_insert_rowid()
        }
    };

    // 4. Remove the previous FTS entry. A contentless-delete index undoes an
    //    entry from its rowid alone, so this holds whatever became of the old
    //    body blob (see the module docs). Unconditional: a rowid that carries
    //    no entry is a no-op delete.
    tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [row_id])
        .context("removing the previous FTS row")?;

    // 5. Re-point blob references. Acquire first, release second: a hash that
    //    both versions share must never pass through refcount zero, because
    //    `release` unlinks the file the moment it does.
    let previous = load_blob_refs(tx, row_id)?;
    for r in refs {
        blobs.acquire(tx, &r.hash, r.size)?;
    }
    tx.execute(
        "DELETE FROM message_blobs WHERE message_row = ?1",
        [row_id],
    )
    .context("clearing the previous blob references")?;
    for r in refs {
        tx.execute(
            "INSERT INTO message_blobs (message_row, kind, ordinal, hash, filename, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![row_id, r.kind, r.ordinal, r.hash.as_str(), r.filename, r.size as i64],
        )
        .context("recording a blob reference")?;
    }
    for hash in previous {
        blobs.release(tx, &hash)?;
    }

    // 6. Index the new content.
    tx.execute(
        "INSERT INTO messages_fts (rowid, subject, from_, body_text) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![row_id, email.subject, email.from, email.body_text],
    )
    .context("inserting the FTS row")?;

    Ok(IngestOutcome {
        row_id,
        message_id: message_id.to_string(),
        thread_id,
        inserted: existing.is_none(),
        uid_rebound,
    })
}

/// Every blob hash currently referenced by a message row.
fn load_blob_refs(tx: &Transaction<'_>, row_id: i64) -> Result<Vec<BlobHash>> {
    let mut stmt = tx.prepare("SELECT hash FROM message_blobs WHERE message_row = ?1")?;
    let rows = stmt.query_map([row_id], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for hash in rows {
        match BlobHash::parse(&hash?) {
            Ok(h) => out.push(h),
            Err(e) => warn!("[ingest] ignoring unparseable blob reference: {e:#}"),
        }
    }
    Ok(out)
}

/// The thread a new message joins: its parent's thread when `In-Reply-To` or
/// the last `References` entry resolves to a row in this account, else a new
/// thread rooted at its own Message-ID.
fn resolve_thread_id(
    tx: &Transaction<'_>,
    account: &str,
    message_id: &str,
    in_reply_to: &Option<String>,
    references: &Option<String>,
) -> Result<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(irt) = in_reply_to {
        candidates.extend(split_message_ids(irt));
    }
    if let Some(refs) = references {
        // Nearest ancestor first: the last entry of References is the parent.
        let mut ids = split_message_ids(refs);
        ids.reverse();
        candidates.extend(ids);
    }

    for candidate in candidates {
        let thread: Option<Option<String>> = tx
            .query_row(
                "SELECT thread_id FROM messages WHERE account = ?1 AND message_id = ?2
                 ORDER BY id LIMIT 1",
                (account, candidate.as_str()),
                |row| row.get(0),
            )
            .optional()
            .context("resolving a parent message for threading")?;
        if let Some(Some(thread)) = thread {
            return Ok(thread);
        }
    }
    Ok(message_id.to_string())
}

/// Split a `References` / `In-Reply-To` header value into bracketed ids.
fn split_message_ids(value: &str) -> Vec<String> {
    value
        .split('>')
        .filter_map(|part| {
            let start = part.find('<')?;
            Some(format!("{}>", &part[start..]))
        })
        .collect()
}

/// `In-Reply-To` and `References` from the raw bytes.
///
/// Read here rather than carried on [`FetchedEmail`] because only ingest needs
/// them, and the second parse costs far less than widening a struct every
/// fetch path and test constructs.
fn threading_headers(raw: &[u8]) -> (Option<String>, Option<String>) {
    use mailparse::MailHeaderMap;
    match mailparse::parse_mail(raw) {
        Ok(parsed) => (
            parsed.headers.get_first_value("In-Reply-To"),
            parsed.headers.get_first_value("References"),
        ),
        Err(_) => (None, None),
    }
}

/// The message's `Message-ID`, or a synthesised one. See the module docs for
/// exactly which bytes are hashed.
pub fn resolve_message_id(email: &FetchedEmail, raw: Option<&[u8]>) -> String {
    match email.message_id.as_deref() {
        Some(mid) if !mid.trim().is_empty() => mid.trim().to_string(),
        _ => synthesize_message_id(email, raw),
    }
}

/// `sha256-<hex16>@local.invalid` over the raw bytes, or over the canonical
/// envelope when there are none.
pub fn synthesize_message_id(email: &FetchedEmail, raw: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    match raw {
        Some(raw) => hasher.update(raw),
        None => hasher.update(canonical_envelope(email).as_bytes()),
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for b in digest.iter().take(8) {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("<sha256-{hex}@local.invalid>")
}

/// The exact bytes hashed when no raw message is available.
fn canonical_envelope(email: &FetchedEmail) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        email.from,
        email.to,
        email.cc.as_deref().unwrap_or(""),
        email.subject,
        email.date,
        email.body_text
    )
}

/// A stable positive uid for a backend that has no UIDs of its own: the first
/// 8 bytes of the SHA-256 of the message's `Message-ID` (the synthesised one
/// when the header is absent), cleared of the sign bit. Graph identifies a
/// message by `internetMessageId` on every endpoint this client uses, so this
/// is as stable an identity as the UID it stands in for.
pub fn graph_uid(message_id: &str) -> i64 {
    let digest = Sha256::digest(message_id.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    (i64::from_be_bytes(bytes) & i64::MAX) as i64
}

/// Sortable timestamp for a `Date:` header; `0` when it cannot be parsed, so
/// an undated message sorts to the bottom instead of disappearing.
fn date_sort_for(date: &str) -> i64 {
    chrono::DateTime::parse_from_rfc2822(date)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// List-preview text: the first [`SNIPPET_CHARS`] characters of the body with
/// runs of whitespace collapsed.
fn snippet_for(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(SNIPPET_CHARS).collect()
}

// ---------------------------------------------------------------------------
// Mailbox and cursor bookkeeping
// ---------------------------------------------------------------------------

/// What a fetch learned about a mailbox from the server, and what the next
/// incremental fetch needs to resume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxCursor {
    pub uidvalidity: Option<i64>,
    /// Highest UID seen by the fetch that recorded this cursor.
    pub last_uid: Option<i64>,
    pub uidnext: Option<i64>,
    pub exists: Option<i64>,
    /// CONDSTORE `HIGHESTMODSEQ`, and never anything else: it held a UID until
    /// #0054 split the column, which would have made `CHANGEDSINCE` return
    /// nothing and no error. Unused until #0041 issues that fetch.
    pub highest_modseq: Option<i64>,
    /// Graph `deltaLink` (unused until the delta fetch lands, see
    /// `TODO(#0037-4b-or-0038)` in `src/graph.rs`).
    pub deltalink: Option<String>,
    /// The IMAP arrival mark (#0072): the UID above which this mailbox still
    /// owes the store an arrival, so the prune gate stays shut until a pass
    /// reaches through it. `None` means nothing is owed. Always `None` on the
    /// Graph path, whose coverage is by-id rather than positional.
    pub arrival_mark: Option<i64>,
}

/// Record what ingest knows about a mailbox: the `mailboxes` row the read path
/// will list from, and the `sync_cursors` row the next fetch resumes from.
///
/// A UIDVALIDITY change is *not* handled by wiping the mailbox: the messages
/// reappear under new UIDs and ingest rebinds each row through the
/// `message_id` index, which is what keeps thread assignments and blob
/// references across a renumbering.
pub fn record_mailbox_cursor(
    store: &Store,
    account: &str,
    mailbox: &str,
    cursor: &MailboxCursor,
) -> Result<()> {
    let conn = store.conn();
    conn.execute(
        "INSERT INTO mailboxes (account, name, uidvalidity, uidnext, exists_count)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (account, name) DO UPDATE SET
            uidvalidity = excluded.uidvalidity,
            uidnext = excluded.uidnext,
            exists_count = excluded.exists_count",
        rusqlite::params![account, mailbox, cursor.uidvalidity, cursor.uidnext, cursor.exists],
    )
    .context("recording the mailbox row")?;

    conn.execute(
        "INSERT INTO sync_cursors
            (account, mailbox, uidvalidity, last_uid, highest_modseq, deltalink, arrival_mark)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (account, mailbox) DO UPDATE SET
            uidvalidity = excluded.uidvalidity,
            last_uid = excluded.last_uid,
            highest_modseq = excluded.highest_modseq,
            deltalink = excluded.deltalink,
            arrival_mark = excluded.arrival_mark",
        rusqlite::params![
            account,
            mailbox,
            cursor.uidvalidity,
            cursor.last_uid,
            cursor.highest_modseq,
            cursor.deltalink,
            cursor.arrival_mark
        ],
    )
    .context("recording the sync cursor")?;
    Ok(())
}

/// The cursor a previous fetch left for `(account, mailbox)`, if any.
pub fn load_mailbox_cursor(
    store: &Store,
    account: &str,
    mailbox: &str,
) -> Result<Option<MailboxCursor>> {
    let cursor = store
        .conn()
        .query_row(
            "SELECT uidvalidity, last_uid, highest_modseq, deltalink, arrival_mark
             FROM sync_cursors
             WHERE account = ?1 AND mailbox = ?2",
            [account, mailbox],
            |row| {
                Ok(MailboxCursor {
                    uidvalidity: row.get(0)?,
                    last_uid: row.get(1)?,
                    uidnext: None,
                    exists: None,
                    highest_modseq: row.get(2)?,
                    deltalink: row.get(3)?,
                    arrival_mark: row.get(4)?,
                })
            },
        )
        .optional()
        .context("loading the sync cursor")?;
    Ok(cursor)
}

/// The Message-IDs already ingested for `(account, mailbox)`. The Graph path
/// dedups on this rather than on UIDs, because Graph has none.
pub fn known_message_ids(
    store: &Store,
    account: &str,
    mailbox: &str,
) -> Result<std::collections::HashSet<String>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT message_id FROM messages WHERE account = ?1 AND mailbox = ?2")?;
    let rows = stmt.query_map((account, mailbox), |row| row.get::<_, String>(0))?;
    let mut out = std::collections::HashSet::new();
    for id in rows {
        out.insert(id?);
    }
    Ok(out)
}

/// Apply the server's flags to a row that is already in the store (#TKT-0051).
///
/// `resolve` is handed the flags the row carries now and returns the ones it
/// should carry: which of the three bits a backend is entitled to answer for
/// is the caller's business, not this function's. IMAP fetches the whole
/// `FLAGS` set and replaces it; Graph only knows `isRead` and merges that one
/// bit in, so its passes cannot erase an `\Answered` it was never told about.
///
/// Returns true when the stored string actually changed, and false for a UID
/// the store does not hold. The server is truth here, so there is no cutoff
/// guard: a local flag change becomes a `pending_ops` entry (#0039) rather
/// than a file the sync must not clobber.
///
/// Both statements go through `prepare_cached`: this runs once per UID in the
/// sync window on every pass, so re-preparing them per row would be two fresh
/// SQL compiles per already-held message on the path #0005 and the latency
/// work are about.
pub fn apply_flag(
    store: &Store,
    account: &str,
    mailbox: &str,
    uid: i64,
    resolve: impl FnOnce(MessageFlags) -> MessageFlags,
) -> Result<bool> {
    let conn = store.conn();
    let current: Option<Option<String>> = conn
        .prepare_cached("SELECT flags FROM messages WHERE account = ?1 AND mailbox = ?2 AND uid = ?3")
        .context("preparing the flag read")?
        .query_row(rusqlite::params![account, mailbox, uid], |row| row.get(0))
        .optional()
        .context("reading a row's flags")?;
    let Some(current) = current else {
        return Ok(false);
    };
    let flags = resolve(MessageFlags::parse(current.as_deref().unwrap_or_default())).to_flag_string();
    let changed = conn
        .prepare_cached(
            "UPDATE messages SET flags = ?4
             WHERE account = ?1 AND mailbox = ?2 AND uid = ?3 AND IFNULL(flags, '') <> ?4",
        )
        .context("preparing the flag update")?
        .execute(rusqlite::params![account, mailbox, uid, flags])
        .context("applying server flags")?;
    Ok(changed > 0)
}

/// Apply a whole mailbox's worth of server flag sets in one transaction,
/// returning how many rows actually changed. The IMAP entry point: the server
/// listed every flag it holds, so what it listed is what the row carries.
///
/// Every sync pass hands over the flags of every message the server listed, so
/// this is O(mailbox) `UPDATE`s per pass. In autocommit each one is its own
/// fsync; one transaction makes the pass a single commit.
///
/// Best-effort in the same sense as [`prune_vanished`]: a row that refuses to
/// update is logged and the rest still go. A commit that fails loses the whole
/// pass's flag updates, which the next sync recomputes from the server, so it
/// is logged and reported as zero rather than returned as an error.
pub fn apply_flags(
    store: &Store,
    account: &str,
    mailbox: &str,
    flags: impl IntoIterator<Item = (i64, MessageFlags)>,
) -> usize {
    apply_flag_updates(
        store,
        account,
        mailbox,
        flags.into_iter().map(|(uid, server)| (uid, move |_| server)),
    )
}

/// Apply a whole mailbox's worth of server `\Seen` states in one transaction,
/// leaving the history bits (#TKT-0051) alone. The Graph entry point: Graph
/// answers `isRead` and nothing else, so a pass that wrote the whole flag set
/// would erase every `\Answered` and `$Forwarded` the store holds.
pub fn apply_seen_flags(
    store: &Store,
    account: &str,
    mailbox: &str,
    flags: impl IntoIterator<Item = (i64, bool)>,
) -> usize {
    apply_flag_updates(
        store,
        account,
        mailbox,
        flags
            .into_iter()
            .map(|(uid, seen)| (uid, move |have: MessageFlags| have.with_seen(seen))),
    )
}

/// The shared transaction both entry points above run in.
fn apply_flag_updates<F>(
    store: &Store,
    account: &str,
    mailbox: &str,
    updates: impl IntoIterator<Item = (i64, F)>,
) -> usize
where
    F: FnOnce(MessageFlags) -> MessageFlags,
{
    let tx = match store.conn().unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            warn!("Failed to open the flag transaction for '{mailbox}': {e:#}");
            return 0;
        }
    };
    let mut updated = 0;
    for (uid, resolve) in updates {
        match apply_flag(store, account, mailbox, uid, resolve) {
            Ok(true) => updated += 1,
            Ok(false) => {}
            Err(e) => warn!("Failed to apply the flags of UID {uid}: {e:#}"),
        }
    }
    if let Err(e) = tx.commit() {
        warn!("Failed to commit the flags for '{mailbox}': {e:#}");
        return 0;
    }
    updated
}

/// How close to now a row's own `Date:` may be before the prune leaves it
/// alone: 15 minutes, the watcher's longest poll delay
/// ([`crate::outbox::backoff_secs`]'s ceiling), so a row survives at least one
/// full poll cycle at the worst backoff before anything can delete it.
pub const PRUNE_MIN_AGE_SECS: i64 = 900;

/// Whether a row dated `date_sort` is too fresh for a prune to touch, given the
/// current time.
///
/// The window is symmetric because the input is the message's own `Date:`
/// header, not an ingest timestamp: a sender's clock running fast puts a
/// just-arrived message slightly in the future, and that message needs the same
/// protection. Symmetry also keeps the guard from being permanent -- a header
/// dated 2099 is far outside the window in the other direction, so it is
/// prunable rather than immortal. An unparseable date stores `0`, which is
/// prunable, matching the rest of the code's treatment of it as "oldest".
fn too_fresh_to_prune(date_sort: i64, now: i64) -> bool {
    (now - date_sort).abs() < PRUNE_MIN_AGE_SECS
}

/// Whether a pass may apply the prunes it collected, given `(enumeration was
/// complete, download was incomplete)` for every target it synced.
///
/// One partial target suspends the prune for *all* of them, which is the whole
/// point: the reason an inbox row may be deleted is that some other target
/// ingested the copy the message moved to, so a target that did not finish
/// invalidates every other target's diff as well. A full sync
/// (`limit = usize::MAX`) never truncates, so the deferred prunes are applied
/// by the next uncapped pass; nothing is lost, only postponed (#0065).
///
/// Both backends gate on this. What the two flags *mean* is backend-specific:
/// Graph pages a folder and counts every message it found but could not fetch
/// ([`crate::graph::sync_mailboxes_graph`]), while IMAP enumerates the whole
/// mailbox in one `UID SEARCH ALL` and counts only the arrivals above its
/// high-water mark, because its window is positional and the backlog under it
/// is skipped by design (#0072).
pub fn pass_may_prune(coverage: &[(bool, bool)]) -> bool {
    coverage
        .iter()
        .all(|&(complete, truncated)| complete && !truncated)
}

/// How many passes may download a message and fail to write it before the pass
/// stops holding the prune gate shut for it (#0074).
///
/// Three, because the failures worth retrying are transient (a locked store, a
/// blob write that lost a race) and clear on the next pass, while the ones that
/// do not clear -- a MIME the parser rejects, a body the store will not take --
/// are deterministic and will fail identically for as long as the server keeps
/// listing the message. Retrying forever is the deadlock #0072 closed from the
/// other side: one unwritable message would suspend the prune for the whole
/// account until someone noticed.
pub const MAX_INGEST_ATTEMPTS: i64 = 3;

/// Count one failed attempt to write `uid`, and answer whether the message is
/// still owed, i.e. whether it must keep the arrival mark below itself.
///
/// `false` is the give-up answer: the message has failed
/// [`MAX_INGEST_ATTEMPTS`] times, so it stops counting against coverage and the
/// gate may open over it. The row stays, so the give-up is visible in the file
/// and a later success still clears it.
///
/// A store that cannot even record the failure answers `true`, the conservative
/// side: an unrecorded attempt can never reach the bound, so the worst case is
/// the pre-#0074 behaviour of retrying, never a gate opened over a hole.
pub fn record_ingest_failure(
    store: &Store,
    account: &str,
    mailbox: &str,
    uid: i64,
    error: &str,
) -> bool {
    let conn = store.conn();
    let attempts: rusqlite::Result<i64> = conn.query_row(
        "INSERT INTO ingest_failures (account, mailbox, uid, attempts, last_error, updated)
         VALUES (?1, ?2, ?3, 1, ?4, ?5)
         ON CONFLICT (account, mailbox, uid) DO UPDATE SET
            attempts = ingest_failures.attempts + 1,
            last_error = excluded.last_error,
            updated = excluded.updated
         RETURNING attempts",
        rusqlite::params![account, mailbox, uid, error, crate::outbox::unix_now()],
        |row| row.get(0),
    );
    match attempts {
        Ok(attempts) => attempts < MAX_INGEST_ATTEMPTS,
        Err(e) => {
            warn!("Failed to record the ingest failure of UID {uid} in '{mailbox}': {e}");
            true
        }
    }
}

/// Forget the failure history of `uid`, because a pass has now written it.
///
/// Called on every successful ingest rather than only after a known failure:
/// the row is keyed by UID, and a UIDVALIDITY reset or a rebind can hand the
/// same UID to a different message, which must not inherit anyone's attempts.
pub fn clear_ingest_failure(store: &Store, account: &str, mailbox: &str, uid: i64) {
    if let Err(e) = store.conn().execute(
        "DELETE FROM ingest_failures WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
        rusqlite::params![account, mailbox, uid],
    ) {
        warn!("Failed to clear the ingest failure of UID {uid} in '{mailbox}': {e}");
    }
}

/// How many passes have failed to write `uid`. `0` when nothing is recorded.
pub fn ingest_failure_attempts(store: &Store, account: &str, mailbox: &str, uid: i64) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT attempts FROM ingest_failures
             WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
            rusqlite::params![account, mailbox, uid],
            |row| row.get(0),
        )
        .unwrap_or(0)
}

/// Of `vanished`, the UIDs whose rows are old enough for the prune to delete.
///
/// This is the answer to a row the server has never listed under the identity
/// the store gave it. The shape came from Graph, and IMAP runs it too since
/// #0072 widened its diff to the whole mailbox: the placeholder a Sent copy is
/// filed under is no longer kept safe by a window clamp alone. A Graph send
/// transmits no `Message-ID`
/// (Graph's `sendMail` takes JSON and Exchange stamps its own
/// `internetMessageId`), so [`crate::outbox::ingest_sent_copy`] files the local
/// copy under [`graph_uid`] of *our* id, which no Sent enumeration will ever
/// return. Every pass therefore sees it as vanished, and pruning it releases
/// the raw MIME blob -- the only copy, because Graph never returns RFC822, so
/// "show source" for that message dies with it (#0065).
///
/// The guard, rather than exempting the `sent` role: the local row is a
/// duplicate of the server's copy once Exchange files it, and the prune is what
/// clears it. Exempting the role would trade a rare permanent data loss for one
/// permanent duplicate row per message ever sent through Graph. Delaying the
/// prune by one poll cycle keeps both properties: the copy survives the window
/// where the server has not filed it yet, and a later pass still tidies up.
///
/// The age is the row's `date_sort` (its `Date:` header) because the store has
/// no ingest timestamp; for a message this client just composed the two are the
/// same instant. The cost is that a message *received* in the last few minutes
/// and deleted on the server keeps its row for one more cycle, which the next
/// pass corrects. That equivalence is a coupling worth naming: it holds only
/// while a locally-composed message is ingested at the moment it is dated, so a
/// future offline or retry queue on the Graph send path (#0063) would ingest a
/// copy dated hours earlier, past the window, and hand the guard back the exact
/// data loss it exists to prevent. A real ingest timestamp is the fix at that
/// point.
///
/// A row whose date cannot be read (a query error, as opposed to no such row)
/// is held back rather than pruned: the guard is about not deleting a message
/// on incomplete information, and a failed lookup is the least complete
/// information there is (#0065 follow-up).
pub fn prunable_uids(
    store: &Store,
    account: &str,
    mailbox: &str,
    vanished: &[i64],
    now: i64,
) -> Vec<i64> {
    let mut prunable = Vec::with_capacity(vanished.len());
    let mut held_back = 0;
    for &uid in vanished {
        let date_sort: i64 = match store.conn().query_row(
            "SELECT IFNULL(date_sort, 0) FROM messages
             WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
            rusqlite::params![account, mailbox, uid],
            |row| row.get(0),
        ) {
            Ok(date_sort) => date_sort,
            // No row: nothing to hold back, and the delete is a no-op.
            Err(rusqlite::Error::QueryReturnedNoRows) => 0,
            Err(e) => {
                warn!(
                    "Could not read the date of uid {uid} in '{mailbox}' ({e}); \
                     holding it back from the prune"
                );
                held_back += 1;
                continue;
            }
        };
        if too_fresh_to_prune(date_sort, now) {
            held_back += 1;
        } else {
            prunable.push(uid);
        }
    }
    if held_back > 0 {
        info!(
            "Held {held_back} freshly-dated row(s) back from the prune of '{mailbox}': \
             a just-sent or just-arrived message the server may not have filed yet"
        );
    }
    prunable
}

/// Drop the rows of `mailbox` whose UIDs the server no longer lists, returning
/// how many went.
///
/// On the IMAP side the set comes from [`crate::imap_client::vanished_uids`],
/// which diffs against the mailbox's whole `UID SEARCH ALL` listing and clamps
/// only at `UIDNEXT - 1`, above which sit this client's own placeholder rows.
/// The Graph side enumerates the whole folder every pass and needs no clamp,
/// and its UIDs are the 63-bit [`graph_uid`] hashes rather than IMAP's `u32`:
/// hence the generic width. Both are gated by [`pass_may_prune`] and filtered
/// by [`prunable_uids`] before they get here.
///
/// Delete is the whole of it: there is no tombstone and no attempt to guess
/// where the message went. The store is a droppable cache in front of the
/// server (see [`crate::store::write`]), and a message archived in another
/// client is re-ingested under the destination mailbox by the same sync, so
/// deleting the source row leaves exactly the one row the server has. That
/// ordering is the caller's to keep: the sync ingests every target mailbox
/// first and prunes afterwards (see [`crate::imap_client::sync_mailboxes`]),
/// because pruning the inbox before the archive pass runs would leave the
/// message with no row anywhere. A row the user is previewing can go out from
/// under them, which is the row-id reuse hazard the write path already
/// documents; the list is rebuilt from the store after every sync, which is
/// where a stale reference is dropped.
///
/// Best-effort by construction: a row that refuses to delete is logged and the
/// rest still go, so there is no error to return.
pub fn prune_vanished<U>(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    mailbox: &str,
    vanished: &[U],
) -> usize
where
    U: Copy + Into<i64> + std::fmt::Display,
{
    let mut pruned = 0;
    for uid in vanished {
        match crate::store::write::delete_by_uid(store, blobs, account, mailbox, (*uid).into()) {
            Ok(Some(_)) => pruned += 1,
            Ok(None) => {}
            Err(e) => warn!("Failed to prune UID {uid} from '{mailbox}': {e:#}"),
        }
    }
    if pruned > 0 {
        info!("Pruned {pruned} row(s) from '{mailbox}': no longer listed there by the server");
    }
    pruned
}

/// What the store already holds for a mailbox, and the UIDVALIDITY it holds it
/// under.
///
/// The pair travels together because a UID means nothing on its own: after a
/// server-side renumbering the same numbers are handed out again to different
/// messages, so a skip list carried across a UIDVALIDITY change makes the fetch
/// skip bodies it has never seen while a stale row keeps the old content under
/// the recycled number. [`KnownUids::resolve`] is where that is caught.
#[derive(Debug, Clone, Default)]
pub struct KnownUids {
    /// UIDs the store holds for this mailbox.
    pub uids: std::collections::HashSet<i64>,
    /// UIDVALIDITY recorded by the fetch that last wrote those rows, when the
    /// store has a cursor for the mailbox at all.
    pub uidvalidity: Option<i64>,
    /// The arrival mark a previous pass left behind: a UID above which some
    /// message the server lists is still not in the store, so the prune stays
    /// suspended until a pass reaches through it (#0072). `None` when the last
    /// pass brought every arrival in. Meaningless across a UIDVALIDITY change,
    /// like the UID set it travels with.
    pub arrival_mark: Option<u32>,
    /// The top of the mailbox as of the last recorded pass (`sync_cursors.last_uid`),
    /// and `None` when the store has no cursor row for it at all.
    ///
    /// That `None` is the whole point: it is the only way to tell first contact
    /// from a mailbox whose local rows are simply gone, and the two must not be
    /// treated alike. A mailbox with history defers when messages land above its
    /// recorded top; one without has no arrivals at all, only backlog (#0072).
    /// A recorded top of 0 is history too, so this is `Some(0)` rather than
    /// `None` when the cursor exists but recorded no UID.
    pub prior_high_water: Option<u32>,
}

impl KnownUids {
    /// The UIDs the fetch may skip, given the UIDVALIDITY the server reported
    /// in its SELECT response, plus whether a reset was detected.
    ///
    /// A mismatch empties the skip list: every UID in the window is refetched,
    /// and ingest then either UPSERTs the row that holds the recycled UID or
    /// rebinds the message through the `message_id` index, keeping its thread
    /// assignment and blob references (see the module docs). Nothing is
    /// deleted here, because "the server renumbered" says nothing about which
    /// messages are gone; that is the reconcile pass's job (#0038).
    ///
    /// No stored UIDVALIDITY (a first sync) and no reported one (a server that
    /// omits it) are both "cannot tell", which leaves the skip list alone.
    pub fn resolve(mut self, server_uidvalidity: Option<u32>) -> (std::collections::HashSet<i64>, bool) {
        let reset = matches!(
            (self.uidvalidity, server_uidvalidity),
            (Some(stored), Some(seen)) if stored != seen as i64
        );
        if reset {
            self.uids.clear();
        }
        (self.uids, reset)
    }
}

/// Everything a fetch needs to decide what to skip: the stored UIDs and the
/// UIDVALIDITY they were recorded under.
pub fn known_uids_with_cursor(store: &Store, account: &str, mailbox: &str) -> Result<KnownUids> {
    let cursor = load_mailbox_cursor(store, account, mailbox)?;
    Ok(KnownUids {
        uids: known_uids(store, account, mailbox)?,
        uidvalidity: cursor.as_ref().and_then(|c| c.uidvalidity),
        arrival_mark: cursor
            .as_ref()
            .and_then(|c| c.arrival_mark)
            .and_then(|m| u32::try_from(m).ok()),
        prior_high_water: cursor.as_ref().map(|c| {
            c.last_uid
                .and_then(|uid| u32::try_from(uid).ok())
                .unwrap_or(0)
        }),
    })
}

/// The UIDs already ingested for `(account, mailbox)`, so a fetch can skip
/// bodies it already holds. This is the store-side replacement for the
/// Message-ID scan of the `.md` tree the old sync ran on every pass.
///
/// Callers on the sync path want [`known_uids_with_cursor`] instead: a bare UID
/// set cannot see a UIDVALIDITY reset.
pub fn known_uids(
    store: &Store,
    account: &str,
    mailbox: &str,
) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT uid FROM messages WHERE account = ?1 AND mailbox = ?2")?;
    let rows = stmt.query_map((account, mailbox), |row| row.get::<_, i64>(0))?;
    let mut out = std::collections::HashSet::new();
    for uid in rows {
        out.insert(uid?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(subject: &str) -> FetchedEmail {
        FetchedEmail {
            from: "a@example.com".into(),
            to: "b@example.com".into(),
            cc: None,
            subject: subject.into(),
            date: "Mon, 01 Jan 2024 12:00:00 +0000".into(),
            body_text: "body".into(),
            html_body: None,
            has_attachments: false,
            message_id: None,
            attachments: Vec::new(),
            flags: MessageFlags::default(),
            calendar_ics: None,
            event: None,
        }
    }

    #[test]
    fn synthesised_ids_are_deterministic_and_content_bound() {
        let e = email("hello");
        let raw = b"From: a@example.com\r\n\r\nbody\r\n";
        assert_eq!(
            synthesize_message_id(&e, Some(raw)),
            synthesize_message_id(&e, Some(raw))
        );
        assert_ne!(
            synthesize_message_id(&e, Some(raw)),
            synthesize_message_id(&e, Some(b"From: a@example.com\r\n\r\nother\r\n"))
        );
        // The envelope form is used only when there are no raw bytes, and the
        // two forms are different digests of different inputs.
        assert_ne!(
            synthesize_message_id(&e, None),
            synthesize_message_id(&e, Some(raw))
        );

        let id = synthesize_message_id(&e, None);
        assert!(id.starts_with("<sha256-"), "{id}");
        assert!(id.ends_with("@local.invalid>"), "{id}");
        assert_eq!(id.len(), "<sha256-".len() + 16 + "@local.invalid>".len());
    }

    #[test]
    fn a_present_header_wins_over_synthesis() {
        let mut e = email("hello");
        e.message_id = Some("  <real@example.com>  ".into());
        assert_eq!(resolve_message_id(&e, None), "<real@example.com>");
        e.message_id = Some("   ".into());
        assert!(resolve_message_id(&e, None).ends_with("@local.invalid>"));
    }

    #[test]
    fn graph_uids_are_stable_and_positive() {
        assert_eq!(graph_uid("AAMkAGI2"), graph_uid("AAMkAGI2"));
        assert_ne!(graph_uid("AAMkAGI2"), graph_uid("AAMkAGI3"));
        assert!(graph_uid("AAMkAGI2") >= 0);
    }

    #[test]
    fn message_ids_split_out_of_a_references_header() {
        assert_eq!(
            split_message_ids("<a@x> <b@x>\r\n <c@x>"),
            vec!["<a@x>", "<b@x>", "<c@x>"]
        );
        assert!(split_message_ids("garbage").is_empty());
    }

    /// The window the Graph prune leaves alone (#0065). Symmetric, so a
    /// sender's fast clock is covered, and bounded, so nothing is immortal.
    #[test]
    fn a_freshly_dated_row_is_held_back_from_the_prune() {
        let now = 1_800_000_000;
        assert!(too_fresh_to_prune(now, now), "a message dated this instant");
        assert!(too_fresh_to_prune(now - PRUNE_MIN_AGE_SECS + 1, now));
        assert!(!too_fresh_to_prune(now - PRUNE_MIN_AGE_SECS, now));
        assert!(!too_fresh_to_prune(now - 86_400, now));
        // Clock skew in both directions, and the absurd date that must not buy
        // a row permanent residence.
        assert!(too_fresh_to_prune(now + 60, now));
        assert!(!too_fresh_to_prune(now + 10 * 365 * 86_400, now));
        // An unparseable `Date:` stores 0, which is prunable like any old row.
        assert!(!too_fresh_to_prune(0, now));
    }

    #[test]
    fn snippets_collapse_whitespace_and_are_bounded() {
        assert_eq!(snippet_for("hello\r\n\r\n  world\t"), "hello world");
        assert_eq!(snippet_for(&"x ".repeat(400)).chars().count(), SNIPPET_CHARS);
    }
}
