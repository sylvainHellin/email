//! Store-only ingest, end to end (#0037 unit 4a).
//!
//! Everything here runs offline against a store and a blob directory in a
//! tempdir. The fixtures are the #0049 parity corpus, re-pointed from the `.md`
//! file the old writer produced at the `messages` row and the blobs it
//! references, which is the oracle the ticket replaced "byte-identical to the
//! `.md`" with.
//!
//! Tags follow #0049: `parity` means the recorded behaviour is correct and must
//! be reproduced; `known-bug` means it is wrong and the comment names the
//! target. This unit changes no parser, so every `known-bug` here still records
//! today's output.

use std::collections::HashSet;

use email::ingest::{ingest_message, IngestInput, IngestOutcome};
use email::parse::{parse_rfc822_to_fetched_email, FetchedEmail};
use email::store::{blobs::BlobHash, BlobStore, Store};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture harness
// ---------------------------------------------------------------------------

struct Fixture {
    _tmp: TempDir,
    store: Store,
    blobs: BlobStore,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("store.sqlite3")).unwrap();
        let blobs = BlobStore::new(tmp.path().join("blobs"));
        Self { _tmp: tmp, store, blobs }
    }

    fn ingest_raw(&self, mailbox: &str, uid: i64, raw: &[u8]) -> IngestOutcome {
        let email = parse_rfc822_to_fetched_email(raw).expect("fixture must parse");
        self.ingest(mailbox, uid, &email, Some(raw))
    }

    fn ingest(
        &self,
        mailbox: &str,
        uid: i64,
        email: &FetchedEmail,
        raw: Option<&[u8]>,
    ) -> IngestOutcome {
        ingest_message(
            &self.store,
            &self.blobs,
            &IngestInput { account: "acct", mailbox, uid, email, raw },
        )
        .unwrap()
    }

    fn text(&self, row: i64, column: &str) -> String {
        self.store
            .conn()
            .query_row(
                &format!("SELECT IFNULL({column}, '') FROM messages WHERE id = ?1"),
                [row],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    }

    fn int(&self, row: i64, column: &str) -> i64 {
        self.store
            .conn()
            .query_row(
                &format!("SELECT IFNULL({column}, 0) FROM messages WHERE id = ?1"),
                [row],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn blob(&self, row: i64, column: &str) -> Vec<u8> {
        let hash = self.text(row, column);
        self.blobs.read(&BlobHash::parse(&hash).unwrap()).unwrap()
    }

    fn body(&self, row: i64) -> String {
        String::from_utf8(self.blob(row, "body_blob")).unwrap()
    }

    fn message_rows(&self) -> i64 {
        self.store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    }

    /// `(kind, ordinal, hash, filename)` of every blob the row references.
    fn blob_refs(&self, row: i64) -> Vec<(String, i64, String, String)> {
        let mut stmt = self
            .store
            .conn()
            .prepare(
                "SELECT kind, ordinal, hash, IFNULL(filename, '') FROM message_blobs
                 WHERE message_row = ?1 ORDER BY kind, ordinal",
            )
            .unwrap();
        let rows = stmt
            .query_map([row], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn refcount(&self, hash: &str) -> i64 {
        self.store
            .conn()
            .query_row(
                "SELECT IFNULL((SELECT refcount FROM blobs WHERE hash = ?1), 0)",
                [hash],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Row ids returned by an FTS query, which is the only usable shape for
    /// this contentless index (see the schema doc comment).
    fn fts_hits(&self, query: &str) -> HashSet<i64> {
        let mut stmt = self
            .store
            .conn()
            .prepare("SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1")
            .unwrap();
        let rows = stmt.query_map([query], |r| r.get::<_, i64>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }
}

fn message(headers: &str, body: &[u8]) -> Vec<u8> {
    let mut raw = headers.as_bytes().to_vec();
    raw.extend_from_slice(b"\r\n");
    raw.extend_from_slice(body);
    raw
}

// ---------------------------------------------------------------------------
// 1. The #0049 parity corpus, decoded through ingest
// ---------------------------------------------------------------------------

/// parity. RFC 2047 encoded words in Subject and From decode before they reach
/// the row, in both UTF-8 and latin-1, and `_` is a space in a "Q" word.
#[test]
fn rfc2047_headers_reach_the_row_decoded() {
    let f = Fixture::new();

    let utf8 = message(
        "From: a@example.com\r\n\
         To: c@example.com\r\n\
         Subject: =?UTF-8?B?R3LDvMOfZSBhdXMgTcO8bmNoZW4=?=\r\n\
         Message-ID: <u1@example.com>\r\n",
        b"body\r\n",
    );
    let row = f.ingest_raw("inbox", 1, &utf8).row_id;
    assert_eq!(f.text(row, "subject"), "Grüße aus München");
    assert_eq!(f.text(row, "from_"), "a@example.com");
    assert_eq!(f.text(row, "to_"), "c@example.com");

    let latin1 = message(
        "From: =?ISO-8859-1?Q?J=FCrgen_M=FCller?= <juergen@example.de>\r\n\
         Subject: =?ISO-8859-1?Q?Gr=FC=DFe_aus_M=FCnchen?=\r\n\
         Message-ID: <u2@example.de>\r\n",
        b"body\r\n",
    );
    let row = f.ingest_raw("inbox", 2, &latin1).row_id;
    assert_eq!(f.text(row, "from_"), "Jürgen Müller <juergen@example.de>");
    assert_eq!(f.text(row, "subject"), "Grüße aus München");
}

/// parity. `ISO-8859-1` is decoded as windows-1252, so 0x80..0x9F come out as
/// the printable characters real senders mean.
#[test]
fn iso8859_1_label_decodes_c1_bytes_as_windows_1252() {
    let f = Fixture::new();
    let raw = message(
        "From: a@example.com\r\n\
         Subject: =?ISO-8859-1?Q?=93smart=94_caf=E9?=\r\n\
         Message-ID: <c1@example.com>\r\n",
        b"body\r\n",
    );
    let row = f.ingest_raw("inbox", 1, &raw).row_id;
    assert_eq!(f.text(row, "subject"), "\u{201c}smart\u{201d} café");
}

/// known-bug (unchanged by this unit). Raw 8-bit header bytes with no
/// encoded-word are decoded as strict ISO-8859-1, so 0x93/0x94 land as the
/// invisible C1 controls U+0093/U+0094 instead of curly quotes. The store keeps
/// exactly what the parser produced, so the bug is now visible in the row.
/// Target: decode raw 8-bit header bytes as windows-1252, like the
/// encoded-word and body paths already do.
#[test]
fn raw_8bit_header_bytes_still_land_as_c1_controls() {
    let f = Fixture::new();
    let mut raw = b"From: a@example.com\r\nSubject: caf\xe9 \x93raw\x94\r\n".to_vec();
    raw.extend_from_slice(b"Message-ID: <r1@example.com>\r\n\r\nbody\r\n");
    let row = f.ingest_raw("inbox", 1, &raw).row_id;
    assert_eq!(f.text(row, "subject"), "café \u{93}raw\u{94}");
}

/// parity. Non-UTF-8 bodies decode into the body blob: latin-1, windows-1252,
/// Shift_JIS, and the very common windows-1252-mislabelled-as-latin-1 case.
#[test]
fn non_utf8_bodies_decode_into_the_body_blob() {
    let f = Fixture::new();

    let cases: Vec<(&str, &[u8], &str)> = vec![
        (
            "iso-8859-1",
            &b"Gr\xfc\xdfe aus M\xfcnchen\r\n"[..],
            "Grüße aus München\r\n",
        ),
        (
            "windows-1252",
            &b"\x93smart quotes\x94 and \x80 euro \x85\r\n"[..],
            "\u{201c}smart quotes\u{201d} and € euro …\r\n",
        ),
        (
            "Shift_JIS",
            &b"\x93\xfa\x96{\x8c\xea\x82\xcc\x83\x81\x81[\x83\x8b\r\n"[..],
            "日本語のメール\r\n",
        ),
        ("iso-8859-1", &b"\x93smart\x94\r\n"[..], "\u{201c}smart\u{201d}\r\n"),
    ];

    for (uid, (charset, body, expected)) in cases.iter().enumerate() {
        let raw = message(
            &format!(
                "From: a@example.com\r\n\
                 Subject: charset\r\n\
                 Message-ID: <b{uid}@example.com>\r\n\
                 Content-Type: text/plain; charset={charset}\r\n"
            ),
            body,
        );
        let row = f.ingest_raw("inbox", uid as i64 + 1, &raw).row_id;
        assert_eq!(&f.body(row), expected, "charset {charset}");
    }
}

/// parity. A truncated multipart keeps the earlier part as the body and the
/// partial trailing part as an attachment blob, bytes intact.
#[test]
fn truncated_multipart_keeps_the_partial_attachment_blob() {
    let f = Fixture::new();
    let raw = message(
        "From: a@example.com\r\n\
         Subject: trunc\r\n\
         Message-ID: <t1@example.com>\r\n\
         Content-Type: multipart/mixed; boundary=\"XX\"\r\n",
        b"--XX\r\n\
          Content-Type: text/plain\r\n\
          \r\n\
          first part text\r\n\
          --XX\r\n\
          Content-Type: application/pdf\r\n\
          Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
          \r\n\
          trunc",
    );
    let row = f.ingest_raw("inbox", 1, &raw).row_id;

    assert_eq!(f.body(row), "first part text\r\n");
    assert_eq!(f.int(row, "has_attachments"), 1);
    let refs = f.blob_refs(row);
    let attachments: Vec<_> = refs.iter().filter(|r| r.0 == "attachment").collect();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].3, "doc.pdf");
    assert_eq!(
        f.blobs.read(&BlobHash::parse(&attachments[0].2).unwrap()).unwrap(),
        b"trunc"
    );
}

/// known-bug (unchanged by this unit). A `multipart/mixed` with no `boundary=`
/// parameter still loses its whole body, so the row's body blob is empty and
/// the snippet is blank. Target: fall back to treating the entity body as text
/// so the content stays visible.
#[test]
fn multipart_without_boundary_still_ingests_an_empty_body() {
    let f = Fixture::new();
    let raw = message(
        "From: a@example.com\r\n\
         Subject: noboundary\r\n\
         Message-ID: <n1@example.com>\r\n\
         Content-Type: multipart/mixed\r\n",
        b"--XX\r\nContent-Type: text/plain\r\n\r\nhello\r\n--XX--\r\n",
    );
    let row = f.ingest_raw("inbox", 1, &raw).row_id;
    assert_eq!(f.body(row), "");
    assert_eq!(f.text(row, "snippet"), "");
    // The bytes are not lost: the raw blob still holds the whole message, so a
    // parser fix can re-derive the body without a re-fetch.
    assert_eq!(f.blob(row, "raw_blob"), raw);
}

/// known-bug (unchanged by this unit). A nested `message/rfc822` part is
/// dropped: no body text, no attachment blob. Target: keep the forwarded
/// message reachable, as an `.eml` attachment or inline.
#[test]
fn nested_message_rfc822_is_still_dropped() {
    let f = Fixture::new();
    let raw = message(
        "From: outer@example.com\r\n\
         Subject: fwd\r\n\
         Message-ID: <f1@example.com>\r\n\
         Content-Type: multipart/mixed; boundary=\"B\"\r\n",
        b"--B\r\n\
          Content-Type: text/plain\r\n\
          \r\n\
          see attached\r\n\
          --B\r\n\
          Content-Type: message/rfc822\r\n\
          \r\n\
          From: inner@example.org\r\n\
          Subject: inner\r\n\
          Content-Type: text/plain\r\n\
          \r\n\
          inner body\r\n\
          --B--\r\n",
    );
    let row = f.ingest_raw("inbox", 1, &raw).row_id;
    assert_eq!(f.body(row), "see attached\r\n");
    assert_eq!(f.int(row, "has_attachments"), 0);
    assert!(f.blob_refs(row).iter().all(|r| r.0 != "attachment"));
}

/// parity. Degenerate input still produces a row rather than a dropped
/// message: placeholder headers, an empty body, and a synthesised Message-ID.
#[test]
fn headerless_input_still_yields_a_row() {
    let f = Fixture::new();
    let row = f.ingest_raw("inbox", 1, b"just a body with no headers\r\n").row_id;
    assert_eq!(f.text(row, "from_"), "(unknown)");
    assert_eq!(f.text(row, "subject"), "(no subject)");
    assert_eq!(f.text(row, "date_display"), "(unknown date)");
    assert_eq!(f.int(row, "date_sort"), 0);
    assert!(f.text(row, "message_id").ends_with("@local.invalid>"));
}

// ---------------------------------------------------------------------------
// 2. Message-ID synthesis
// ---------------------------------------------------------------------------

/// parity. A missing Message-ID is synthesised as `sha256-<hex16>@local.invalid`
/// over the raw bytes, deterministically: the same message ingested into a
/// fresh store twice gets the same id, and a different message gets a
/// different one.
#[test]
fn synthesised_message_ids_are_deterministic() {
    let raw = message("From: a@example.com\r\nSubject: no id\r\n", b"body\r\n");
    let other = message("From: a@example.com\r\nSubject: no id\r\n", b"other\r\n");

    let first = Fixture::new();
    let a = first.ingest_raw("inbox", 1, &raw);
    let second = Fixture::new();
    let b = second.ingest_raw("inbox", 99, &raw);
    let c = second.ingest_raw("inbox", 100, &other);

    assert_eq!(a.message_id, b.message_id, "same bytes, same synthesised id");
    assert_ne!(a.message_id, c.message_id, "different bytes, different id");

    let mid = a.message_id;
    assert!(mid.starts_with("<sha256-"), "{mid}");
    assert!(mid.ends_with("@local.invalid>"), "{mid}");
    assert_eq!(mid.len(), "<sha256-".len() + 16 + "@local.invalid>".len());
    // Non-null and usable as an identity: it is what the row stores.
    assert_eq!(first.text(a.row_id, "message_id"), mid);
}

// ---------------------------------------------------------------------------
// 3. Re-ingest: UPSERT, blob refs, FTS
// ---------------------------------------------------------------------------

/// Regression for the known issue #0037 left behind and #0038 unit B fixed:
/// a re-ingest whose *previous* body blob is unreadable used to skip the FTS
/// delete, because an external-content index can only undo an entry by
/// replaying the old column values. The row ended up indexed twice, once under
/// the old terms and once under the new ones. `messages_fts` is contentless
/// with `contentless_delete=1` now, so the delete needs the rowid and nothing
/// else, and the eviction cannot desynchronise the index.
#[test]
fn reingest_after_the_old_body_blob_is_evicted_leaves_one_fts_entry() {
    let f = Fixture::new();
    let first = message(
        "From: a@example.com\r\n\
         Subject: original subject\r\n\
         Message-ID: <ev1@example.com>\r\n",
        b"first body text\r\n",
    );
    let row = f.ingest_raw("inbox", 11, &first).row_id;
    assert_eq!(f.fts_hits("original"), HashSet::from([row]));

    // Evict the body blob the way a retention sweep would: unlink the file and
    // leave the row pointing at it.
    let hash = f.text(row, "body_blob");
    std::fs::remove_file(f.blobs.path_for(&BlobHash::parse(&hash).unwrap())).unwrap();

    let second = message(
        "From: a@example.com\r\n\
         Subject: corrected subject\r\n\
         Message-ID: <ev1@example.com>\r\n",
        b"second body text\r\n",
    );
    let again = f.ingest_raw("inbox", 11, &second);
    assert_eq!(again.row_id, row, "still one row");
    assert_eq!(f.message_rows(), 1);

    assert!(
        f.fts_hits("original").is_empty(),
        "the entry built from the evicted body must still be deleted"
    );
    assert_eq!(f.fts_hits("corrected"), HashSet::from([row]));
    assert_eq!(f.fts_hits("\"second body text\""), HashSet::from([row]));
    let entries: i64 = f
        .store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'corrected'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(entries, 1, "exactly one FTS entry for the row");
}

/// Re-ingesting the same UID updates the row instead of inserting a second one,
/// and the FTS entry follows the new content instead of accumulating.
#[test]
fn reingesting_a_uid_upserts_and_keeps_fts_in_step() {
    let f = Fixture::new();
    let first = message(
        "From: a@example.com\r\n\
         Subject: original subject\r\n\
         Message-ID: <up1@example.com>\r\n",
        b"first body text\r\n",
    );
    let outcome = f.ingest_raw("inbox", 7, &first);
    assert!(outcome.inserted);
    let row = outcome.row_id;

    assert_eq!(f.fts_hits("original"), HashSet::from([row]));
    assert_eq!(f.fts_hits("\"first body text\""), HashSet::from([row]));

    // Same UID, new content (a corrected fetch, or a re-download).
    let second = message(
        "From: a@example.com\r\n\
         Subject: corrected subject\r\n\
         Message-ID: <up1@example.com>\r\n",
        b"second body text\r\n",
    );
    let again = f.ingest_raw("inbox", 7, &second);
    assert!(!again.inserted, "a re-ingest must not insert a second row");
    assert_eq!(again.row_id, row, "the row id is stable across re-ingest");
    assert_eq!(f.message_rows(), 1);
    assert_eq!(f.text(row, "subject"), "corrected subject");
    assert_eq!(f.body(row), "second body text\r\n");

    // FTS follows: the old terms are gone, the new ones hit, and there is
    // exactly one entry for the row.
    assert!(f.fts_hits("original").is_empty(), "stale FTS entry survived");
    assert_eq!(f.fts_hits("corrected"), HashSet::from([row]));
    assert_eq!(f.fts_hits("\"second body text\""), HashSet::from([row]));
    let fts_rows: i64 = f
        .store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'corrected'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_rows, 1);
}

/// Re-ingest releases the references it replaced and keeps the ones it did not,
/// so nothing leaks and a still-referenced blob is never unlinked.
#[test]
fn reingest_releases_only_the_references_that_changed() {
    let f = Fixture::new();
    let with_attachment = |body: &str| {
        message(
            "From: a@example.com\r\n\
             Subject: att\r\n\
             Message-ID: <ref1@example.com>\r\n\
             Content-Type: multipart/mixed; boundary=\"B\"\r\n",
            format!(
                "--B\r\n\
                 Content-Type: text/plain\r\n\
                 \r\n\
                 {body}\r\n\
                 --B\r\n\
                 Content-Type: application/pdf\r\n\
                 Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
                 \r\n\
                 PDFBYTES\r\n\
                 --B--\r\n"
            )
            .as_bytes(),
        )
    };

    let row = f.ingest_raw("inbox", 3, &with_attachment("first")).row_id;
    let before = f.blob_refs(row);
    let old_body = before.iter().find(|r| r.0 == "body").unwrap().2.clone();
    let attachment = before.iter().find(|r| r.0 == "attachment").unwrap().2.clone();
    assert_eq!(f.refcount(&old_body), 1);
    assert_eq!(f.refcount(&attachment), 1);

    f.ingest_raw("inbox", 3, &with_attachment("second"));
    let after = f.blob_refs(row);
    let new_body = after.iter().find(|r| r.0 == "body").unwrap().2.clone();

    assert_ne!(new_body, old_body, "a changed body is a different blob");
    assert_eq!(f.refcount(&old_body), 0, "the replaced body blob leaked");
    assert!(
        !f.blobs.contains(&BlobHash::parse(&old_body).unwrap()),
        "the last reference should have unlinked the file"
    );
    assert_eq!(f.refcount(&new_body), 1);
    assert_eq!(
        f.refcount(&attachment),
        1,
        "an unchanged attachment must keep exactly one reference"
    );
    assert!(
        f.blobs.contains(&BlobHash::parse(&attachment).unwrap()),
        "an unchanged attachment blob must never be unlinked"
    );
    assert_eq!(after.len(), before.len(), "the reference list must not grow");
}

// ---------------------------------------------------------------------------
// 4. UIDVALIDITY reset
// ---------------------------------------------------------------------------

/// A simulated UIDVALIDITY reset: the same message reappears under a new UID.
/// Ingest finds the prior row through the `message_id` index, keeps its row id,
/// thread assignment and blob references, and updates the UID in place.
#[test]
fn uidvalidity_reset_rebinds_the_row_and_keeps_thread_and_refs() {
    let f = Fixture::new();

    let parent = message(
        "From: a@example.com\r\n\
         Subject: thread root\r\n\
         Message-ID: <root@example.com>\r\n",
        b"root body\r\n",
    );
    let reply = message(
        "From: b@example.com\r\n\
         Subject: Re: thread root\r\n\
         Message-ID: <reply@example.com>\r\n\
         In-Reply-To: <root@example.com>\r\n",
        b"reply body\r\n",
    );

    let root = f.ingest_raw("inbox", 10, &parent);
    let before = f.ingest_raw("inbox", 11, &reply);
    assert_eq!(before.thread_id, root.message_id, "the reply joins its parent's thread");
    let refs_before = f.blob_refs(before.row_id);

    // The server renumbers: same message, new UID, nothing else changed.
    let after = f.ingest_raw("inbox", 5001, &reply);

    assert!(after.uid_rebound, "the reset should have been absorbed");
    assert!(!after.inserted);
    assert_eq!(after.row_id, before.row_id, "the row identity must survive");
    assert_eq!(f.message_rows(), 2, "renumbering must not duplicate the message");
    assert_eq!(f.int(after.row_id, "uid"), 5001, "the uid must be updated in place");
    assert_eq!(after.thread_id, root.message_id, "the thread assignment must survive");
    assert_eq!(f.blob_refs(after.row_id), refs_before, "blob references must survive");
    for (_, _, hash, _) in refs_before {
        assert_eq!(f.refcount(&hash), 1, "a rebind must not double-count a reference");
    }
}

/// The sync path's half of a UIDVALIDITY reset (#0037 review).
///
/// Rebinding only works if the bodies are downloaded again in the first place,
/// and they are not: the fetch skips every UID the store already holds, and
/// after a renumbering those numbers have been handed to *different* messages.
/// [`KnownUids::resolve`] is what catches that, by throwing the skip list away
/// when the server's UIDVALIDITY no longer matches the stored cursor.
#[test]
fn a_uidvalidity_reset_refetches_the_window_and_rebinds_what_moved() {
    let f = Fixture::new();
    let root = message(
        "From: a@example.com\r\n\
         Subject: thread root\r\n\
         Message-ID: <reset-root@example.com>\r\n",
        b"root body\r\n",
    );
    let reply = message(
        "From: b@example.com\r\n\
         Subject: Re: thread root\r\n\
         Message-ID: <reset-reply@example.com>\r\n\
         In-Reply-To: <reset-root@example.com>\r\n",
        b"reply body\r\n",
    );
    f.ingest_raw("inbox", 1, &root);
    let before = f.ingest_raw("inbox", 2, &reply);
    let refs_before = f.blob_refs(before.row_id);
    email::ingest::record_mailbox_cursor(
        &f.store,
        "acct",
        "inbox",
        &email::ingest::MailboxCursor {
            uidvalidity: Some(1),
            last_uid: Some(2),
            uidnext: Some(3),
            exists: Some(2),
            highest_modseq: None,
            deltalink: None,
        },
    )
    .unwrap();

    // The server renumbers. UID 1 now holds a different message entirely, and
    // the reply has moved to UID 9.
    let known = email::ingest::known_uids_with_cursor(&f.store, "acct", "inbox").unwrap();
    assert_eq!(known.uids, HashSet::from([1, 2]));
    assert_eq!(known.uidvalidity, Some(1));
    let (skip, reset) = known.resolve(Some(2));
    assert!(reset, "the UIDVALIDITY change must be detected");
    assert!(
        skip.is_empty(),
        "a recycled UID must not be treated as a body the store already holds"
    );

    // So pass 2 downloads both, and ingest sorts out what each one is.
    let stranger = message(
        "From: c@example.com\r\n\
         Subject: brand new\r\n\
         Message-ID: <reset-stranger@example.com>\r\n",
        b"stranger body\r\n",
    );
    let recycled = f.ingest_raw("inbox", 1, &stranger);
    let moved = f.ingest_raw("inbox", 9, &reply);

    assert_eq!(
        f.body(recycled.row_id),
        "stranger body\r\n",
        "the recycled UID must carry the body that was refetched for it"
    );
    assert!(moved.uid_rebound, "the moved message is rebound, not duplicated");
    assert_eq!(moved.row_id, before.row_id);
    assert_eq!(moved.thread_id, before.thread_id, "the thread must survive");
    assert_eq!(f.blob_refs(moved.row_id), refs_before, "and so must the blob refs");
    assert_eq!(f.message_rows(), 2, "a reset must not duplicate the mailbox");

    // Once the new cursor is recorded, the next sync skips normally again.
    email::ingest::record_mailbox_cursor(
        &f.store,
        "acct",
        "inbox",
        &email::ingest::MailboxCursor {
            uidvalidity: Some(2),
            last_uid: Some(9),
            uidnext: Some(10),
            exists: Some(2),
            highest_modseq: None,
            deltalink: None,
        },
    )
    .unwrap();
    let (skip, reset) = email::ingest::known_uids_with_cursor(&f.store, "acct", "inbox")
        .unwrap()
        .resolve(Some(2));
    assert!(!reset);
    assert_eq!(skip, HashSet::from([1, 9]));

    // A first sync (no stored cursor) and a server that reports no UIDVALIDITY
    // are both "cannot tell", and must not throw the skip list away.
    let (skip, reset) = email::ingest::known_uids_with_cursor(&f.store, "acct", "inbox")
        .unwrap()
        .resolve(None);
    assert!(!reset);
    assert_eq!(skip.len(), 2);
    let (skip, reset) = email::ingest::KnownUids {
        uids: HashSet::from([7]),
        uidvalidity: None,
    }
    .resolve(Some(42));
    assert!(!reset);
    assert_eq!(skip, HashSet::from([7]));
}

/// The same Message-ID in another mailbox is a copy, not the same row: identity
/// is `(account, mailbox, uid)`, and the `message_id` index is deliberately
/// non-unique.
#[test]
fn the_same_message_in_two_mailboxes_is_two_rows() {
    let f = Fixture::new();
    let raw = message(
        "From: a@example.com\r\n\
         Subject: copied\r\n\
         Message-ID: <copy@example.com>\r\n",
        b"body\r\n",
    );
    let inbox = f.ingest_raw("inbox", 1, &raw);
    let archive = f.ingest_raw("archive", 1, &raw);

    assert!(archive.inserted);
    assert_ne!(inbox.row_id, archive.row_id);
    assert_eq!(f.message_rows(), 2);
    // Both rows reference the same deduped blobs, counted twice.
    let hash = f.text(inbox.row_id, "body_blob");
    assert_eq!(f.text(archive.row_id, "body_blob"), hash);
    assert_eq!(f.refcount(&hash), 2);
}

// ---------------------------------------------------------------------------
// 5. Cursors
// ---------------------------------------------------------------------------

/// The cursor a fetch records is what the next one resumes from.
#[test]
fn mailbox_cursors_round_trip() {
    let f = Fixture::new();
    let cursor = email::ingest::MailboxCursor {
        uidvalidity: Some(42),
        last_uid: Some(1234),
        uidnext: Some(1235),
        exists: Some(56),
        highest_modseq: Some(7),
        deltalink: None,
    };
    email::ingest::record_mailbox_cursor(&f.store, "acct", "inbox", &cursor).unwrap();

    let loaded = email::ingest::load_mailbox_cursor(&f.store, "acct", "inbox").unwrap().unwrap();
    assert_eq!(loaded.uidvalidity, Some(42));
    assert_eq!(loaded.last_uid, Some(1234));
    assert_eq!(loaded.highest_modseq, Some(7));

    // The UID and the modseq live in their own columns (#0054): a UID written
    // into `highest_modseq` would make a later CHANGEDSINCE fetch return
    // nothing and no error.
    let (stored_uid, stored_modseq): (i64, i64) = f
        .store
        .conn()
        .query_row(
            "SELECT last_uid, highest_modseq FROM sync_cursors
             WHERE account = 'acct' AND mailbox = 'inbox'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((stored_uid, stored_modseq), (1234, 7));

    // The mailbox row carries what the read path lists from.
    let (uidvalidity, uidnext, exists): (i64, i64, i64) = f
        .store
        .conn()
        .query_row(
            "SELECT uidvalidity, uidnext, exists_count FROM mailboxes
             WHERE account = 'acct' AND name = 'inbox'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((uidvalidity, uidnext, exists), (42, 1235, 56));

    // Recording again updates in place rather than inserting a second cursor.
    email::ingest::record_mailbox_cursor(
        &f.store,
        "acct",
        "inbox",
        &email::ingest::MailboxCursor {
            uidvalidity: Some(43),
            last_uid: Some(1),
            ..cursor.clone()
        },
    )
    .unwrap();
    let cursors: i64 = f
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM sync_cursors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cursors, 1);
    assert_eq!(
        email::ingest::load_mailbox_cursor(&f.store, "acct", "inbox").unwrap().unwrap().uidvalidity,
        Some(43)
    );

    // The row is keyed on (account, mailbox), like every other table.
    email::ingest::record_mailbox_cursor(&f.store, "other", "inbox", &cursor).unwrap();
    let cursors: i64 = f
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM sync_cursors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cursors, 2);
    assert_eq!(
        email::ingest::load_mailbox_cursor(&f.store, "other", "inbox").unwrap().unwrap().uidvalidity,
        Some(42),
        "another account's cursor for the same mailbox name is a separate row"
    );
    assert_eq!(
        email::ingest::load_mailbox_cursor(&f.store, "acct", "inbox").unwrap().unwrap().uidvalidity,
        Some(43),
        "writing another account's cursor leaves the first account's row alone"
    );
}

/// `known_uids` is what lets an incremental fetch skip bodies it already holds.
#[test]
fn known_uids_reports_what_the_store_holds() {
    let f = Fixture::new();
    let raw = |n: u8| {
        message(
            &format!("From: a@example.com\r\nSubject: m{n}\r\nMessage-ID: <k{n}@x.com>\r\n"),
            b"body\r\n",
        )
    };
    f.ingest_raw("inbox", 1, &raw(1));
    f.ingest_raw("inbox", 4, &raw(4));
    f.ingest_raw("archive", 9, &raw(9));

    assert_eq!(
        email::ingest::known_uids(&f.store, "acct", "inbox").unwrap(),
        HashSet::from([1, 4])
    );
    assert_eq!(
        email::ingest::known_uids(&f.store, "acct", "archive").unwrap(),
        HashSet::from([9])
    );
    assert!(email::ingest::known_uids(&f.store, "other", "inbox").unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// 6. The Graph shape: no raw bytes, synthetic uid
// ---------------------------------------------------------------------------

/// A message ingested without raw bytes (the Graph path) gets a body blob, no
/// raw blob, and a uid derived from its Message-ID, and re-ingesting it is
/// still an UPSERT.
#[test]
fn a_message_without_raw_bytes_ingests_and_upserts() {
    let f = Fixture::new();
    let email = FetchedEmail {
        from: "a@example.com".into(),
        to: "b@example.com".into(),
        cc: None,
        subject: "graph message".into(),
        date: "Mon, 01 Jan 2024 12:00:00 +0000".into(),
        body_text: "graph body".into(),
        html_body: None,
        has_attachments: false,
        message_id: Some("<g1@example.com>".into()),
        attachments: Vec::new(),
        is_read: true,
        calendar_ics: None,
        event: None,
    };
    let uid = email::ingest::graph_uid("<g1@example.com>");
    assert!(uid > 0);

    let outcome = f.ingest("inbox", uid, &email, None);
    assert!(outcome.inserted);
    assert_eq!(f.body(outcome.row_id), "graph body");
    assert_eq!(f.text(outcome.row_id, "raw_blob"), "", "Graph has no RFC822");
    assert_eq!(f.text(outcome.row_id, "flags"), "\\Seen");
    assert!(f.blob_refs(outcome.row_id).iter().all(|r| r.0 != "raw"));

    let again = f.ingest("inbox", uid, &email, None);
    assert!(!again.inserted);
    assert_eq!(f.message_rows(), 1);
}

/// A Graph message keeps its HTML part.
///
/// There are no RFC822 bytes to re-derive it from, so without an `html` blob
/// the body the sender actually wrote is gone the moment the message is
/// ingested and the read path (#0038) can only ever show the plain-text
/// downgrade. An IMAP message needs no such blob: its `raw` blob holds the
/// whole MIME tree.
#[test]
fn a_graph_message_keeps_its_html_body_as_a_blob() {
    let f = Fixture::new();
    let html = "<html><body><p>graph <b>body</b></p></body></html>";
    let mut email = FetchedEmail {
        from: "a@example.com".into(),
        to: "b@example.com".into(),
        cc: None,
        subject: "graph html".into(),
        date: "Mon, 01 Jan 2024 12:00:00 +0000".into(),
        body_text: "graph body".into(),
        html_body: Some(html.to_string()),
        has_attachments: false,
        message_id: Some("<g-html@example.com>".into()),
        attachments: Vec::new(),
        is_read: true,
        calendar_ics: None,
        event: None,
    };

    let outcome = f.ingest("inbox", email::ingest::graph_uid("<g-html@example.com>"), &email, None);
    let refs = f.blob_refs(outcome.row_id);
    let (_, _, hash, _) = refs
        .iter()
        .find(|r| r.0 == "html")
        .expect("the HTML part must be persisted");
    assert_eq!(
        String::from_utf8(f.blobs.read(&BlobHash::parse(hash).unwrap()).unwrap()).unwrap(),
        html
    );
    assert_eq!(f.refcount(hash), 1);

    // Re-ingesting with new HTML re-points the reference and releases the old.
    let old_hash = hash.clone();
    email.html_body = Some("<html><body><p>edited</p></body></html>".into());
    let again = f.ingest("inbox", email::ingest::graph_uid("<g-html@example.com>"), &email, None);
    let refs = f.blob_refs(again.row_id);
    let (_, _, new_hash, _) = refs.iter().find(|r| r.0 == "html").unwrap();
    assert_ne!(new_hash, &old_hash);
    assert_eq!(f.refcount(&old_hash), 0, "the superseded HTML is released");

    // An IMAP message stores the raw bytes instead, and no html blob.
    let raw = message(
        "From: a@example.com\r\nSubject: imap\r\nMessage-ID: <imap-html@example.com>\r\n",
        b"body\r\n",
    );
    let imap = f.ingest_raw("inbox", 5, &raw);
    let kinds: Vec<String> = f.blob_refs(imap.row_id).into_iter().map(|r| r.0).collect();
    assert!(kinds.contains(&"raw".to_string()));
    assert!(!kinds.contains(&"html".to_string()));
}

// ---------------------------------------------------------------------------
// 7. The prune: rows the server no longer lists
// ---------------------------------------------------------------------------

/// `vanished_uids` is the diff a fetch computes; `prune_vanished` is what the
/// sync does with it. Both halves run here so the tests read like one sync
/// pass without needing a server.
fn sync_prune(f: &Fixture, mailbox: &str, listed: &[u32]) -> usize {
    let vanished = fetch_diff(f, mailbox, listed);
    email::ingest::prune_vanished(&f.store, &f.blobs, "acct", mailbox, &vanished)
}

/// The diff half on its own, for the test that has to hold a prune back until
/// every target mailbox has been ingested, the way the sync does.
///
/// `listed` is the server's whole `UID SEARCH ALL` answer, not the download
/// window (#0072). The ceiling stands in for `UIDNEXT - 1`, one above the
/// highest UID either side knows about.
fn fetch_diff(f: &Fixture, mailbox: &str, listed: &[u32]) -> Vec<u32> {
    let known = email::ingest::known_uids(&f.store, "acct", mailbox).unwrap();
    let ceiling = known
        .iter()
        .filter(|&&uid| uid > 0 && uid < u32::MAX as i64)
        .map(|&uid| uid as u32)
        .chain(listed.iter().copied())
        .max()
        .unwrap_or(0);
    email::imap_client::vanished_uids(&known, listed, ceiling)
}

fn plain(name: &str) -> Vec<u8> {
    message(
        &format!("From: a@example.com\r\nSubject: {name}\r\nMessage-ID: <{name}@example.com>\r\n"),
        format!("{name} body\r\n").as_bytes(),
    )
}

/// A UID the server did not list is gone from the mailbox: the row goes, and
/// the blobs it was holding lose their reference. Before this the row was
/// immortal, because no sync path ever computed "store UID not on the server".
#[test]
fn a_uid_missing_from_the_listing_loses_its_row_and_its_blob_refs() {
    let f = Fixture::new();
    let gone = f.ingest_raw("inbox", 2, &plain("gone"));
    let stays = f.ingest_raw("inbox", 3, &plain("stays"));
    let gone_hash = f.text(gone.row_id, "body_blob");
    assert_eq!(f.refcount(&gone_hash), 1);

    // The server lists 1..=3 minus 2.
    f.ingest_raw("inbox", 1, &plain("first"));
    assert_eq!(sync_prune(&f, "inbox", &[1, 3]), 1);

    assert_eq!(f.message_rows(), 2);
    assert!(email::store::read::find_by_id(&f.store, gone.row_id).unwrap().is_none());
    assert!(email::store::read::find_by_id(&f.store, stays.row_id).unwrap().is_some());
    assert!(f.blob_refs(gone.row_id).is_empty(), "the reference list outlived its row");
    assert_eq!(f.refcount(&gone_hash), 0, "the pruned row kept its blob alive");
    let fts: i64 = f
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM messages_fts WHERE rowid = ?1", [gone.row_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fts, 0);
}

/// #0072, the reported defect: the *oldest* inbox message is archived in
/// another client, which is what everyone does first. It is below every UID the
/// server still lists, so the old window-range clamp could never reach it and
/// the row was immortal however often the user pressed `s`.
#[test]
fn the_oldest_uid_archived_elsewhere_is_pruned() {
    let f = Fixture::new();
    for uid in 1..=6 {
        f.ingest_raw("inbox", uid, &plain(&format!("m{uid}")));
    }

    // The server enumerated the whole mailbox and UID 1 was not in it.
    assert_eq!(sync_prune(&f, "inbox", &[2, 3, 4, 5, 6]), 1);
    assert_eq!(
        email::ingest::known_uids(&f.store, "acct", "inbox").unwrap(),
        HashSet::from([2, 3, 4, 5, 6])
    );

    // A hole in the middle is the same diff, not a different rule.
    assert_eq!(sync_prune(&f, "inbox", &[2, 3, 5, 6]), 1);
    assert_eq!(f.message_rows(), 4);
}

/// The clamp that survives the widening: a row above `UIDNEXT - 1` was written
/// by this client, not by the server. The Sent copy appended without an
/// `APPENDUID` lives there, under a `graph_uid` hash, and the server was never
/// asked about it.
#[test]
fn a_row_above_the_ceiling_survives_a_listing_that_omits_it() {
    let f = Fixture::new();
    for uid in 1..=3 {
        f.ingest_raw("sent", uid, &plain(&format!("m{uid}")));
    }
    let placeholder = email::ingest::graph_uid("<not-yet-filed@example.com>");
    f.ingest_raw("sent", placeholder, &plain("not-yet-filed"));

    let known = email::ingest::known_uids(&f.store, "acct", "sent").unwrap();
    let vanished = email::imap_client::vanished_uids(&known, &[1, 2, 3], 3);
    assert!(vanished.is_empty(), "the placeholder is not a server UID");
    assert_eq!(f.message_rows(), 4);
}

/// A UIDVALIDITY reset empties the known set, so there is nothing to prune: a
/// renumbering says nothing about which messages are gone, and the rows are
/// about to be rebound through their Message-IDs.
#[test]
fn a_uidvalidity_reset_prunes_nothing() {
    let f = Fixture::new();
    for uid in 1..=3 {
        f.ingest_raw("inbox", uid, &plain(&format!("m{uid}")));
    }
    email::ingest::record_mailbox_cursor(
        &f.store,
        "acct",
        "inbox",
        &email::ingest::MailboxCursor {
            uidvalidity: Some(1),
            last_uid: Some(3),
            uidnext: Some(4),
            exists: Some(3),
            highest_modseq: None,
            deltalink: None,
        },
    )
    .unwrap();

    // The server renumbered: UIDs 90..=92 now hold the same three messages.
    let known = email::ingest::known_uids_with_cursor(&f.store, "acct", "inbox").unwrap();
    let (resolved, reset) = known.resolve(Some(2));
    assert!(reset);
    let vanished = email::imap_client::vanished_uids(&resolved, &[90, 91, 92], 92);
    assert!(vanished.is_empty(), "a reset must never prune");
    assert_eq!(
        email::ingest::prune_vanished(&f.store, &f.blobs, "acct", "inbox", &vanished),
        0
    );
    assert_eq!(f.message_rows(), 3);
}

/// The reported defect, end to end: a message archived in another client leaves
/// the inbox window and reappears in the archive. Ingest inserts the archive
/// copy (identity is per mailbox), the prune drops the inbox row, and the user
/// sees the message exactly once, in the archive.
///
/// The order below is the sync's own, and it is the point of the test: targets
/// are synced inbox, archive, sent, so each target's diff is computed against
/// the store as its fetch found it, every target is ingested, and only then do
/// the prunes go. Pruning inside the per-target loop instead deleted the inbox
/// row before the archive pass ran, and the message spent that window with no
/// row anywhere, its body blob at refcount zero and unlinked from disk.
#[test]
fn a_message_archived_elsewhere_ends_up_in_the_archive_only() {
    let f = Fixture::new();
    let raw = plain("archived-elsewhere");
    let inbox = f.ingest_raw("inbox", 7, &raw);
    f.ingest_raw("inbox", 8, &plain("other"));
    let hash = f.text(inbox.row_id, "body_blob");
    let blob = BlobHash::parse(&hash).unwrap();
    let rows_for_message = || -> i64 {
        f.store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE message_id = ?1",
                ["<archived-elsewhere@example.com>"],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(rows_for_message(), 1);
    assert_eq!(f.refcount(&hash), 1);
    assert!(f.blobs.contains(&blob));

    // Inbox pass: the server stopped listing UID 7, which the fetch reports
    // and the sync holds back.
    let inbox_vanished = fetch_diff(&f, "inbox", &[8]);
    assert_eq!(inbox_vanished, vec![7]);

    // Archive pass: its own diff, then the ingest of the moved copy at its new
    // UID. The message now has two rows, never fewer.
    let archive_vanished = fetch_diff(&f, "archive", &[31]);
    assert!(archive_vanished.is_empty());
    let archived = f.ingest_raw("archive", 31, &raw);
    assert!(archived.inserted);
    assert_eq!(rows_for_message(), 2);
    assert_eq!(f.refcount(&hash), 2, "both rows reference the deduped body blob");
    assert!(f.blobs.contains(&blob));

    // Prune pass, after every target has been ingested.
    assert_eq!(
        email::ingest::prune_vanished(&f.store, &f.blobs, "acct", "inbox", &inbox_vanished),
        1
    );
    assert_eq!(
        email::ingest::prune_vanished(&f.store, &f.blobs, "acct", "archive", &archive_vanished),
        0
    );
    assert_eq!(rows_for_message(), 1, "the archive row, and it never went to zero");
    assert_eq!(f.refcount(&hash), 1, "the inbox row released its reference");
    assert!(f.blobs.contains(&blob), "the body blob was never unlinked");

    assert_eq!(f.message_rows(), 2, "the archived message plus the untouched one");
    let mailboxes: Vec<String> = email::store::read::list_mailbox(&f.store, "acct", "inbox")
        .unwrap()
        .iter()
        .map(|e| e.message_id.clone())
        .collect();
    assert_eq!(mailboxes, vec!["<other@example.com>".to_string()]);
    let archive: Vec<String> = email::store::read::list_mailbox(&f.store, "acct", "archive")
        .unwrap()
        .iter()
        .map(|e| e.message_id.clone())
        .collect();
    assert_eq!(archive, vec!["<archived-elsewhere@example.com>".to_string()]);
}

/// A Graph message, whose identity is its `Message-ID` and whose UID is the
/// hash of it.
fn graph_email(message_id: &str, is_read: bool) -> FetchedEmail {
    FetchedEmail {
        from: "a@example.com".into(),
        to: "b@example.com".into(),
        cc: None,
        subject: message_id.to_string(),
        date: "Mon, 01 Jan 2024 12:00:00 +0000".into(),
        body_text: format!("{message_id} body"),
        html_body: None,
        has_attachments: false,
        message_id: Some(message_id.to_string()),
        attachments: Vec::new(),
        is_read,
        calendar_ics: None,
        event: None,
    }
}

/// The Graph half of `a_message_archived_elsewhere_ends_up_in_the_archive_only`
/// (#0055): a message archived in Outlook web leaves the inbox enumeration and
/// appears in the archive one, and the same prune-after-every-ingest ordering
/// leaves exactly the archive row. Graph UIDs are 63-bit hashes rather than
/// IMAP's `u32`, which is the width the prune has to take.
#[test]
fn a_graph_message_archived_on_the_server_is_pruned_from_the_inbox() {
    let f = Fixture::new();
    let moved = "<moved@example.com>";
    let stays = "<stays@example.com>";
    let moved_uid = email::ingest::graph_uid(moved);
    let stays_uid = email::ingest::graph_uid(stays);
    f.ingest("inbox", moved_uid, &graph_email(moved, false), None);
    f.ingest("inbox", stays_uid, &graph_email(stays, false), None);

    // Archive pass ingests the moved copy first, exactly as the sync does.
    assert!(f.ingest("archive", moved_uid, &graph_email(moved, false), None).inserted);
    assert_eq!(f.message_rows(), 3);

    // Inbox prune: the server enumeration no longer lists the moved message.
    assert_eq!(
        email::ingest::prune_vanished(&f.store, &f.blobs, "acct", "inbox", &[moved_uid]),
        1
    );
    assert_eq!(f.message_rows(), 2);

    let inbox: Vec<String> = email::store::read::list_mailbox(&f.store, "acct", "inbox")
        .unwrap()
        .iter()
        .map(|e| e.message_id.clone())
        .collect();
    assert_eq!(inbox, vec![stays.to_string()]);
    let archive: Vec<String> = email::store::read::list_mailbox(&f.store, "acct", "archive")
        .unwrap()
        .iter()
        .map(|e| e.message_id.clone())
        .collect();
    assert_eq!(archive, vec![moved.to_string()]);
}

/// #0065 item 1. A Graph send files its own copy locally under
/// `graph_uid(<our Message-ID>)`, but `sendMail` transmits no `Message-ID`, so
/// Exchange stamps its own and the Sent enumeration never lists ours. The row
/// is therefore in every vanished set the sync computes, and deleting it
/// releases the raw MIME blob for good: Graph never returns RFC822, so nothing
/// can fetch it back and "show source" for that message dies with it.
///
/// The age guard is what carries the copy through the window where the server
/// has not filed the item yet, without making the row immortal: once it is
/// older than one poll cycle the server's own copy is in the store and ours is
/// a duplicate, which is exactly what the prune is for.
#[test]
fn a_just_sent_graph_copy_survives_the_prune_that_never_listed_it() {
    let f = Fixture::new();
    let mid = "<local-send@example.com>";
    let raw = message(
        &format!(
            "From: me@example.com\r\nTo: b@example.com\r\nSubject: just sent\r\n\
             Date: {}\r\nMessage-ID: {mid}\r\n",
            chrono::Utc::now().to_rfc2822(),
        ),
        b"sent body\r\n",
    );
    email::outbox::ingest_sent_copy(&f.store, &f.blobs, "acct", "sent", &raw, mid, None).unwrap();

    let uid = email::ingest::graph_uid(mid);
    let row: i64 = f
        .store
        .conn()
        .query_row("SELECT id FROM messages WHERE uid = ?1", [uid], |r| r.get(0))
        .unwrap();
    let raw_hash = f.text(row, "raw_blob");
    assert!(!raw_hash.is_empty(), "the local copy is the only MIME there is");

    // The pass's diff: the folder listed the server's id, so our row is
    // "vanished" from it.
    let vanished = vec![uid];
    let now = email::outbox::unix_now();
    let prunable = email::ingest::prunable_uids(&f.store, "acct", "sent", &vanished, now);
    assert!(prunable.is_empty(), "a copy the server may not have filed yet");
    assert_eq!(
        email::ingest::prune_vanished(&f.store, &f.blobs, "acct", "sent", &prunable),
        0
    );
    assert_eq!(f.message_rows(), 1);
    assert_eq!(f.refcount(&raw_hash), 1);
    assert!(f.blobs.contains(&BlobHash::parse(&raw_hash).unwrap()));

    // One poll cycle on, the row is prunable again and the duplicate goes.
    let later = now + email::ingest::PRUNE_MIN_AGE_SECS + 1;
    assert_eq!(
        email::ingest::prunable_uids(&f.store, "acct", "sent", &vanished, later),
        vec![uid]
    );
    assert_eq!(
        email::ingest::prune_vanished(&f.store, &f.blobs, "acct", "sent", &[uid]),
        1
    );
    assert_eq!(f.message_rows(), 0);
    assert_eq!(f.refcount(&raw_hash), 0);
}

/// The guard is a delay, not an exemption, and it is not a licence for the rest
/// of the mailbox: a row that is genuinely gone and older than the window is
/// still pruned in the same call that holds the fresh one back.
#[test]
fn the_prune_age_guard_holds_back_only_the_fresh_row() {
    let f = Fixture::new();
    let old = "<old@example.com>";
    let fresh = "<fresh@example.com>";
    let mut old_email = graph_email(old, false);
    old_email.date = "Mon, 01 Jan 2024 12:00:00 +0000".into();
    let mut fresh_email = graph_email(fresh, false);
    fresh_email.date = chrono::Utc::now().to_rfc2822();
    f.ingest("inbox", email::ingest::graph_uid(old), &old_email, None);
    f.ingest("inbox", email::ingest::graph_uid(fresh), &fresh_email, None);

    let vanished = vec![email::ingest::graph_uid(old), email::ingest::graph_uid(fresh)];
    let prunable = email::ingest::prunable_uids(
        &f.store,
        "acct",
        "inbox",
        &vanished,
        email::outbox::unix_now(),
    );
    assert_eq!(prunable, vec![email::ingest::graph_uid(old)]);
    assert_eq!(
        email::ingest::prune_vanished(&f.store, &f.blobs, "acct", "inbox", &prunable),
        1
    );
    assert_eq!(f.message_rows(), 1);
}

/// The whole folder's read flags in one transaction: every changed row lands,
/// a UID the store does not hold is a no-op rather than an error, and the
/// return value counts only the rows that actually changed.
#[test]
fn a_pass_of_server_read_flags_applies_in_one_transaction() {
    let f = Fixture::new();
    let read_already = "<read@example.com>";
    let unread = "<unread@example.com>";
    f.ingest("inbox", email::ingest::graph_uid(read_already), &graph_email(read_already, true), None);
    f.ingest("inbox", email::ingest::graph_uid(unread), &graph_email(unread, false), None);

    let updated = email::ingest::apply_seen_flags(
        &f.store,
        "acct",
        "inbox",
        [
            (email::ingest::graph_uid(read_already), true),
            (email::ingest::graph_uid(unread), true),
            (email::ingest::graph_uid("<never-ingested@example.com>"), true),
        ],
    );
    assert_eq!(updated, 1, "only the row whose flags changed");

    let flags: Vec<String> = email::store::read::list_mailbox(&f.store, "acct", "inbox")
        .unwrap()
        .iter()
        .map(|e| e.flags.clone().unwrap_or_default())
        .collect();
    assert_eq!(flags, vec!["\\Seen".to_string(), "\\Seen".to_string()]);
}
