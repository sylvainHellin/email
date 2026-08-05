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
    /// this external-content index (see the schema doc comment).
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
        deltalink: None,
    };
    email::ingest::record_mailbox_cursor(&f.store, "acct", "inbox", &cursor).unwrap();

    let loaded = email::ingest::load_mailbox_cursor(&f.store, "inbox").unwrap().unwrap();
    assert_eq!(loaded.uidvalidity, Some(42));
    assert_eq!(loaded.last_uid, Some(1234));

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
        &email::ingest::MailboxCursor { uidvalidity: Some(43), last_uid: Some(1), ..cursor },
    )
    .unwrap();
    let cursors: i64 = f
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM sync_cursors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cursors, 1);
    assert_eq!(
        email::ingest::load_mailbox_cursor(&f.store, "inbox").unwrap().unwrap().uidvalidity,
        Some(43)
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
