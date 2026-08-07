//! Behavioural oracles for the receive/parse path (#0049, unit 0b).
//!
//! The data-access-layer rewrite deletes the `.md` writer, and with it the
//! byte-identity oracle. These tests are the replacement for three areas the
//! test-suite audit listed as having no oracle at all: RFC 2047 encoded-word
//! headers, non-UTF-8 body charsets, and malformed MIME.
//!
//! Every test carries one of two tags, and the tag is the point of the file:
//!
//! - `parity`: the recorded behaviour is correct. The new build must reproduce
//!   it.
//! - `known-bug`: the recorded behaviour is wrong. The comment states the
//!   target, and the assertion pins today's output so the change is visible
//!   when it happens. Do not "fix" these here; this unit captures the current
//!   build, it does not change it.
//!
//! Fixtures are inline byte literals on purpose: the non-UTF-8 payloads are
//! written as the exact octets a sender would put on the wire, so no encoding
//! crate is needed to produce them and nothing about the fixture depends on the
//! developer's locale.

use mailypoppins::parse::{parse_rfc822_to_fetched_email, FetchedEmail};

/// Assemble a raw RFC822 message. `headers` must end with CRLF; the blank line
/// that terminates the header block is added here.
fn message(headers: &str, body: &[u8]) -> Vec<u8> {
    let mut raw = headers.as_bytes().to_vec();
    raw.extend_from_slice(b"\r\n");
    raw.extend_from_slice(body);
    raw
}

fn parse(raw: &[u8]) -> FetchedEmail {
    parse_rfc822_to_fetched_email(raw).expect("fixture must parse")
}

/// One fixture ingested into a throwaway store, with accessors for the row and
/// its blobs. This is the store-side replacement for reading the `.md` file
/// the old save path wrote.
struct IngestedRow {
    _tmp: tempfile::TempDir,
    store: mailypoppins::store::Store,
    blobs: mailypoppins::store::BlobStore,
    row: i64,
}

fn ingest_raw(raw: &[u8]) -> IngestedRow {
    let tmp = tempfile::tempdir().unwrap();
    let store = mailypoppins::store::Store::open(tmp.path().join("store.sqlite3")).unwrap();
    let blobs = mailypoppins::store::BlobStore::new(tmp.path().join("blobs"));
    let fetched = parse(raw);
    let outcome = mailypoppins::ingest::ingest_message(
        &store,
        &blobs,
        &mailypoppins::ingest::IngestInput {
            account: "acct",
            mailbox: "inbox",
            uid: 1,
            email: &fetched,
            raw: Some(raw),
        },
    )
    .unwrap();
    IngestedRow { _tmp: tmp, store, blobs, row: outcome.row_id }
}

impl IngestedRow {
    fn text(&self, column: &str) -> String {
        self.store
            .conn()
            .query_row(
                &format!("SELECT IFNULL({column}, '') FROM messages WHERE id = ?1"),
                [self.row],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    }

    fn blob(&self, column: &str) -> Vec<u8> {
        let hash = self.text(column);
        self.blobs
            .read(&mailypoppins::store::blobs::BlobHash::parse(&hash).unwrap())
            .unwrap()
    }

    fn body(&self) -> String {
        String::from_utf8(self.blob("body_blob")).unwrap()
    }

    fn raw(&self) -> Vec<u8> {
        self.blob("raw_blob")
    }
}

// ---------------------------------------------------------------------------
// 1. RFC 2047 encoded-word headers
// ---------------------------------------------------------------------------

/// parity. UTF-8 + base64 ("B") encoded-word in Subject.
#[test]
fn rfc2047_utf8_base64_subject_decodes() {
    let raw = message(
        "From: a@example.com\r\n\
         To: c@example.com\r\n\
         Subject: =?UTF-8?B?R3LDvMOfZSBhdXMgTcO8bmNoZW4=?=\r\n",
        b"body\r\n",
    );
    let f = parse(&raw);
    assert_eq!(f.subject, "Grüße aus München");
    assert_eq!(f.from, "a@example.com");
    assert_eq!(f.to, "c@example.com");
}

/// parity. UTF-8 + quoted-printable ("Q") encoded-word, including the RFC 2047
/// rule that `_` stands for a space (not for U+005F).
#[test]
fn rfc2047_utf8_q_subject_decodes_underscore_as_space() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: =?UTF-8?Q?Gr=C3=BC=C3=9Fe_aus_M=C3=BCnchen?=\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&raw).subject, "Grüße aus München");
}

/// parity. ISO-8859-1 encoded-words, both encodings, decode to the same text.
#[test]
fn rfc2047_iso8859_1_subject_decodes_in_both_encodings() {
    let q = message(
        "From: a@example.com\r\n\
         Subject: =?ISO-8859-1?Q?Gr=FC=DFe_aus_M=FCnchen?=\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&q).subject, "Grüße aus München");

    // Same string, base64 of the latin-1 octets, lowercase charset label.
    let b = message(
        "From: a@example.com\r\n\
         Subject: =?iso-8859-1?B?R3L832UgYXVzIE38bmNoZW4=?=\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&b).subject, "Grüße aus München");
}

/// parity. The label `ISO-8859-1` is decoded as windows-1252, so the bytes
/// 0x80..0x9F come out as the printable characters real senders mean (curly
/// quotes, euro sign) instead of C1 control characters. This is what browsers
/// and the WHATWG encoding standard do, and it is what mail clients need.
#[test]
fn rfc2047_iso8859_1_label_decodes_c1_bytes_as_windows_1252() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: =?ISO-8859-1?Q?=93smart=94_caf=E9?=\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&raw).subject, "\u{201c}smart\u{201d} café");
}

/// parity. An encoded-word in a display name decodes, and the angle-addr is
/// preserved verbatim next to it.
#[test]
fn rfc2047_display_name_decodes_and_keeps_the_address() {
    let latin1 = message(
        "From: =?ISO-8859-1?Q?J=FCrgen_M=FCller?= <juergen@example.de>\r\n\
         Subject: hi\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&latin1).from, "Jürgen Müller <juergen@example.de>");

    let utf8 = message(
        "From: =?UTF-8?B?5bGx55Sw5aSq6YOO?= <taro@example.jp>\r\n\
         Subject: hi\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&utf8).from, "山田太郎 <taro@example.jp>");
}

/// parity. Adjacent encoded-words separated by a fold join with no space
/// between them (RFC 2047 section 6.2), so the sender's own `_` supplies the
/// only space.
#[test]
fn rfc2047_folded_encoded_words_drop_the_fold_whitespace() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: =?UTF-8?Q?Gr=C3=BC=C3=9Fe?=\r\n\
         \t=?UTF-8?Q?_aus_M=C3=BCnchen?=\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&raw).subject, "Grüße aus München");
}

/// parity. Plain ASCII around an encoded-word survives untouched, spaces
/// included. This is the shape of every non-English reply subject.
#[test]
fn rfc2047_encoded_word_mixed_with_plain_ascii() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: Re: =?UTF-8?Q?Caf=C3=A9?= meeting\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&raw).subject, "Re: Café meeting");
}

/// parity. An encoded-word that cannot be decoded (broken base64, or a charset
/// nobody knows) is left as literal text, per RFC 2047 section 6.3. Ugly, but
/// lossless: the user sees the raw token instead of a silently emptied field.
#[test]
fn rfc2047_undecodable_encoded_words_are_left_verbatim() {
    let bad_base64 = message(
        "From: a@example.com\r\n\
         Subject: =?UTF-8?B?!!!not-base64!!!?=\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&bad_base64).subject, "=?UTF-8?B?!!!not-base64!!!?=");

    let unknown_charset = message(
        "From: a@example.com\r\n\
         Subject: =?X-UNKNOWN?Q?caf=E9?=\r\n",
        b"body\r\n",
    );
    assert_eq!(parse(&unknown_charset).subject, "=?X-UNKNOWN?Q?caf=E9?=");
}

/// known-bug. Raw 8-bit bytes in a header (no encoded-word at all, which plenty
/// of senders still emit) are decoded as strict ISO-8859-1, so 0x80..0x9F
/// become C1 control characters: 0x93/0x94 land as U+0093/U+0094 instead of the
/// curly quotes the sender meant. The same bytes inside an `=?ISO-8859-1?...?=`
/// encoded-word, and the same bytes in a body, do come out as curly quotes
/// (see the two tests above), so the three paths disagree.
/// Target: decode raw 8-bit header bytes as windows-1252 too, matching the
/// encoded-word and body paths and what every other mail client does.
#[test]
fn raw_8bit_header_bytes_are_decoded_as_strict_latin1_not_windows_1252() {
    let mut raw = b"From: a@example.com\r\nSubject: caf\xe9 \x93raw\x94\r\n".to_vec();
    raw.extend_from_slice(b"\r\nbody\r\n");
    // Recorded, not endorsed: U+0093/U+0094 are invisible control characters.
    assert_eq!(parse(&raw).subject, "café \u{93}raw\u{94}");
}

/// parity. Encoded-word and RFC 2231 attachment filenames both decode, and the
/// decoded name is what reaches `AttachmentData`.
#[test]
fn rfc2047_and_rfc2231_attachment_filenames_decode() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: att\r\n\
         Content-Type: multipart/mixed; boundary=\"B\"\r\n",
        b"--B\r\n\
          Content-Type: text/plain\r\n\
          \r\n\
          hi\r\n\
          --B\r\n\
          Content-Type: application/pdf\r\n\
          Content-Disposition: attachment; filename=\"=?UTF-8?Q?Angebot_M=C3=BCnchen.pdf?=\"\r\n\
          \r\n\
          FIRST\r\n\
          --B\r\n\
          Content-Type: application/pdf\r\n\
          Content-Disposition: attachment; filename*=UTF-8''Angebot%20M%C3%BCnchen%202.pdf\r\n\
          \r\n\
          SECOND\r\n\
          --B--\r\n",
    );
    let f = parse(&raw);
    let names: Vec<&str> = f.attachments.iter().map(|a| a.filename.as_str()).collect();
    assert_eq!(names, vec!["Angebot München.pdf", "Angebot München 2.pdf"]);
}

/// parity. End to end: a latin-1 encoded-word Subject and display name survive
/// the decode and the store ingest, and come back out of the `messages` row
/// unchanged. The `.md` era asserted the YAML quoting of a subject containing
/// a colon and the slugged filename; neither exists any more (#0037), and the
/// column round-trip is the surviving contract.
#[test]
fn rfc2047_headers_survive_ingest() {
    let mut raw = b"From: =?ISO-8859-1?Q?J=FCrgen_M=FCller?= <juergen@example.de>\r\n\
                    To: c@example.com\r\n\
                    Subject: =?UTF-8?B?R3LDvMOfZTogTcO8bmNoZW4=?=\r\n\
                    Date: Mon, 01 Jan 2024 12:00:00 +0000\r\n\
                    Message-ID: <m1@example.de>\r\n\
                    Content-Type: text/plain; charset=iso-8859-1\r\n\r\n"
        .to_vec();
    raw.extend_from_slice(b"Gr\xfc\xdfe\r\n");

    let store = ingest_raw(&raw);
    assert_eq!(store.text("from_"), "Jürgen Müller <juergen@example.de>");
    assert_eq!(store.text("subject"), "Grüße: München");
    assert_eq!(store.text("message_id"), "<m1@example.de>");
    assert_eq!(store.body(), "Grüße\r\n");
    assert_eq!(store.text("snippet"), "Grüße");
}

// ---------------------------------------------------------------------------
// 2. Non-UTF-8 body charsets
// ---------------------------------------------------------------------------

/// parity. `charset=iso-8859-1` on an 8-bit body decodes to the right text.
/// Note the body keeps its CRLF line endings; the `.md` writer stores them
/// verbatim.
#[test]
fn body_iso8859_1_decodes() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: latin1\r\n\
         Content-Type: text/plain; charset=iso-8859-1\r\n",
        b"Gr\xfc\xdfe aus M\xfcnchen\r\n",
    );
    assert_eq!(parse(&raw).body_text, "Grüße aus München\r\n");
}

/// parity. windows-1252 specifics: curly quotes, euro sign, ellipsis.
#[test]
fn body_windows_1252_decodes() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: cp1252\r\n\
         Content-Type: text/plain; charset=windows-1252\r\n",
        b"\x93smart quotes\x94 and \x80 euro \x85\r\n",
    );
    assert_eq!(
        parse(&raw).body_text,
        "\u{201c}smart quotes\u{201d} and € euro …\r\n"
    );
}

/// parity. Shift_JIS decodes, so the double-byte range is handled and not
/// mangled byte-by-byte.
#[test]
fn body_shift_jis_decodes() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: sjis\r\n\
         Content-Type: text/plain; charset=Shift_JIS\r\n",
        b"\x93\xfa\x96{\x8c\xea\x82\xcc\x83\x81\x81[\x83\x8b\r\n",
    );
    assert_eq!(parse(&raw).body_text, "日本語のメール\r\n");
}

/// parity. The extremely common mislabelling (windows-1252 bytes declared as
/// iso-8859-1) renders as the sender intended, for the same reason as the
/// header case above.
#[test]
fn body_windows_1252_mislabelled_as_iso8859_1_still_decodes() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: mislabelled\r\n\
         Content-Type: text/plain; charset=iso-8859-1\r\n",
        b"\x93smart\x94\r\n",
    );
    assert_eq!(parse(&raw).body_text, "\u{201c}smart\u{201d}\r\n");
}

/// parity. No `charset=` at all with 8-bit bytes: falls back to windows-1252
/// rather than to UTF-8 (where those bytes would be invalid) or U+FFFD.
#[test]
fn body_without_charset_falls_back_to_windows_1252() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: nocharset\r\n",
        b"caf\xe9\r\n",
    );
    assert_eq!(parse(&raw).body_text, "café\r\n");
}

/// parity. Charset decoding happens before transfer-decoding is undone, so a
/// quoted-printable latin-1 body decodes in both layers.
#[test]
fn body_quoted_printable_latin1_decodes() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: qp\r\n\
         Content-Type: text/plain; charset=iso-8859-1\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n",
        b"Gr=FC=DFe aus M=FCnchen\r\n",
    );
    assert_eq!(parse(&raw).body_text, "Grüße aus München\r\n");
}

/// parity. A latin-1 HTML body is decoded for the plain-text projection that
/// the store keeps as the body blob.
///
/// The `.md` era also wrote a companion `.html` whose stale
/// `<meta charset="iso-8859-1">` was rewritten to UTF-8; ingest writes no
/// companion file (#0037), so what is asserted here is the decoded projection
/// that reaches the row. The HTML itself is recoverable from the raw blob.
#[test]
fn html_body_iso8859_1_is_decoded_for_the_body_blob() {
    let mut raw = b"From: a@example.com\r\n\
                    Subject: html\r\n\
                    Date: Mon, 01 Jan 2024 12:00:00 +0000\r\n\
                    Message-ID: <h1@example.com>\r\n\
                    Content-Type: text/html; charset=iso-8859-1\r\n\r\n"
        .to_vec();
    raw.extend_from_slice(
        b"<html><head><meta charset=\"iso-8859-1\"></head><body><p>Gr\xfc\xdfe</p></body></html>\r\n",
    );

    let fetched = parse(&raw);
    assert_eq!(fetched.body_text, "Grüße\n");

    let store = ingest_raw(&raw);
    assert_eq!(store.body(), "Grüße\n");
    // The raw blob is byte-identical to what came off the wire, so the HTML
    // part is never lost even though no `.html` companion is written.
    assert_eq!(store.raw(), raw);
}

// ---------------------------------------------------------------------------
// 3. Malformed MIME
// ---------------------------------------------------------------------------

/// parity. A multipart cut off mid-part still yields the complete earlier part,
/// and the truncated trailing part is kept as an attachment with the bytes that
/// did arrive. Partial data beats no data here.
#[test]
fn truncated_multipart_keeps_earlier_parts_and_the_partial_attachment() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: trunc\r\n\
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
    let f = parse(&raw);
    assert_eq!(f.body_text, "first part text\r\n");
    assert!(f.has_attachments);
    assert_eq!(f.attachments.len(), 1);
    assert_eq!(f.attachments[0].filename, "doc.pdf");
    assert_eq!(f.attachments[0].content, b"trunc");
}

/// known-bug. `Content-Type: multipart/mixed` with no `boundary=` parameter
/// produces an entirely empty body: the text the sender wrote is dropped and
/// the user sees a blank email.
/// Target: when a multipart declares no boundary, fall back to treating the
/// entity body as text (or sniff `--<token>` lines), so the content stays
/// visible. Nothing should silently reduce a message to nothing.
#[test]
fn multipart_without_boundary_parameter_loses_the_whole_body() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: noboundary\r\n\
         Content-Type: multipart/mixed\r\n",
        b"--XX\r\n\
          Content-Type: text/plain\r\n\
          \r\n\
          hello\r\n\
          --XX--\r\n",
    );
    let f = parse(&raw);
    assert_eq!(f.body_text, "");
    assert_eq!(f.html_body, None);
    assert!(!f.has_attachments);
}

/// known-bug. Same failure from the other direction: a declared boundary that
/// never appears in the body. Everything after the headers is discarded.
/// Target: same as above, the raw entity body must remain reachable.
#[test]
fn multipart_whose_boundary_never_appears_loses_the_whole_body() {
    let raw = message(
        "From: a@example.com\r\n\
         Subject: never-opened\r\n\
         Content-Type: multipart/alternative; boundary=\"ZZ\"\r\n",
        b"no parts here at all\r\n",
    );
    assert_eq!(parse(&raw).body_text, "");
}

/// known-bug. A nested `message/rfc822` part (every "forwarded as attachment"
/// mail) is unreachable: its text is not extracted into the body, and it is not
/// listed as an attachment either, so the forwarded message simply vanishes.
/// Target: keep the nested message reachable, either as an `.eml` attachment or
/// by rendering it inline under the covering text.
#[test]
fn nested_message_rfc822_is_dropped_entirely() {
    let raw = message(
        "From: outer@example.com\r\n\
         Subject: fwd\r\n\
         Content-Type: multipart/mixed; boundary=\"B\"\r\n",
        b"--B\r\n\
          Content-Type: text/plain\r\n\
          \r\n\
          see attached\r\n\
          --B\r\n\
          Content-Type: message/rfc822\r\n\
          \r\n\
          From: inner@example.org\r\n\
          Subject: =?UTF-8?Q?inner_caf=C3=A9?=\r\n\
          Content-Type: text/plain\r\n\
          \r\n\
          inner body\r\n\
          --B--\r\n",
    );
    let f = parse(&raw);
    assert_eq!(f.body_text, "see attached\r\n");
    assert!(!f.has_attachments);
    assert!(f.attachments.is_empty());
    assert!(!f.body_text.contains("inner body"));
}

/// parity. A 100 KB header does not blow up or slow the parse down; the header
/// is simply carried and the rest of the message parses normally.
#[test]
fn oversized_header_parses_without_panic() {
    let headers = format!(
        "From: a@example.com\r\nX-Big: {}\r\nSubject: oversized\r\n",
        "A".repeat(100_000)
    );
    let raw = message(&headers, b"body\r\n");
    let f = parse(&raw);
    assert_eq!(f.subject, "oversized");
    assert_eq!(f.body_text, "body\r\n");
}

/// parity. Degenerate inputs get placeholder headers instead of an error, which
/// is what keeps a junk message visible in the list rather than silently
/// skipped at save time.
#[test]
fn empty_and_headerless_input_yield_placeholders() {
    for raw in [&b""[..], &b"just a body with no headers\r\n"[..]] {
        let f = parse(raw);
        assert_eq!(f.from, "(unknown)");
        assert_eq!(f.to, "(unknown)");
        assert_eq!(f.subject, "(no subject)");
        assert_eq!(f.date, "(unknown date)");
        assert_eq!(f.body_text, "");
        assert_eq!(f.message_id, None);
    }
}

/// parity. Every truncation point of a realistic multipart message is parsed
/// without a panic. Three of them return `None` (the message is dropped whole),
/// and they are exactly the offsets where the input ends mid-way through the
/// CRLF CRLF that terminates a header block. Recorded rather than fixed: the
/// drop is total, so if the new build wants to keep the headers of a truncated
/// message it has to decide that deliberately.
#[test]
fn every_truncation_point_parses_without_panic() {
    let mut full = b"From: a@example.com\r\n\
                     To: c@example.com\r\n\
                     Subject: =?UTF-8?Q?Caf=C3=A9?=\r\n\
                     Date: Mon, 01 Jan 2024 12:00:00 +0000\r\n\
                     Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
                     --B\r\n\
                     Content-Type: text/plain; charset=iso-8859-1\r\n\r\n"
        .to_vec();
    full.extend_from_slice(b"Gr\xfc\xdfe\r\n");
    full.extend_from_slice(
        b"--B\r\n\
          Content-Type: application/pdf\r\n\
          Content-Disposition: attachment; filename=\"d.pdf\"\r\n\
          Content-Transfer-Encoding: base64\r\n\r\n\
          UERGQllURVM=\r\n\
          --B--\r\n",
    );

    let dropped: Vec<usize> = (0..=full.len())
        .filter(|&n| parse_rfc822_to_fetched_email(&full[..n]).is_none())
        .collect();
    assert_eq!(dropped.len(), 3, "dropped prefixes: {dropped:?}");
    for n in &dropped {
        assert!(
            full[..*n].ends_with(b"\r\n\r"),
            "prefix {n} is dropped for a reason other than a half-written header terminator"
        );
    }

    // The complete message is the oracle the truncations degrade from.
    let f = parse(&full);
    assert_eq!(f.subject, "Café");
    assert_eq!(f.body_text, "Grüße\r\n");
    assert_eq!(f.attachments.len(), 1);
    assert_eq!(f.attachments[0].content, b"PDFBYTES");
}
