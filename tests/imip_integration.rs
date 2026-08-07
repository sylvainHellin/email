//! End-to-end iMIP receive tests (ticket #0027): raw RFC822 MIME -> parse ->
//! store ingest. Verifies that `text/calendar` parts are classified as invites
//! or as ordinary attachments, and that the invite payload reaches the store as
//! the `invite.ics` attachment blob without a duplicate copy.
//!
//! Rewritten for #0037 unit 4a: the assertions used to read the `.md` file and
//! its sidecar, which no ingest writes any more, so they now read the
//! `message_blobs` rows and the blob bytes. The classification being asserted
//! is unchanged.

use mailypoppins::parse::{parse_rfc822_to_fetched_email, CALENDAR_SIDECAR_NAME};
use tempfile::tempdir;

/// Ingest one parsed message into a throwaway store and expose what the row
/// references, so the iMIP classification can be asserted on what actually
/// landed instead of on a `.md` file that is no longer written (#0037).
struct Ingested {
    _tmp: tempfile::TempDir,
    store: mailypoppins::store::Store,
    blobs: mailypoppins::store::BlobStore,
    row: i64,
}

fn ingest(email: &mailypoppins::parse::FetchedEmail, mailbox: &str) -> Ingested {
    let tmp = tempdir().unwrap();
    let store = mailypoppins::store::Store::open(tmp.path().join("store.sqlite3")).unwrap();
    let blobs = mailypoppins::store::BlobStore::new(tmp.path().join("blobs"));
    let outcome = mailypoppins::ingest::ingest_message(
        &store,
        &blobs,
        &mailypoppins::ingest::IngestInput {
            account: "acct",
            mailbox,
            uid: 1,
            email,
            raw: None,
        },
    )
    .unwrap();
    Ingested { _tmp: tmp, store, blobs, row: outcome.row_id }
}

impl Ingested {
    /// Attachment blob filenames in ingest order.
    fn attachment_names(&self) -> Vec<String> {
        let mut stmt = self
            .store
            .conn()
            .prepare(
                "SELECT filename FROM message_blobs
                 WHERE message_row = ?1 AND kind = 'attachment' ORDER BY ordinal",
            )
            .unwrap();
        let rows = stmt
            .query_map([self.row], |r| r.get::<_, Option<String>>(0))
            .unwrap();
        rows.map(|r| r.unwrap().unwrap_or_default()).collect()
    }

    /// Bytes of the attachment blob stored under `filename`.
    fn attachment_bytes(&self, filename: &str) -> Option<Vec<u8>> {
        let hash: Option<String> = self
            .store
            .conn()
            .query_row(
                "SELECT hash FROM message_blobs
                 WHERE message_row = ?1 AND kind = 'attachment' AND filename = ?2",
                (self.row, filename),
                |r| r.get(0),
            )
            .ok();
        hash.map(|h| {
            self.blobs
                .read(&mailypoppins::store::blobs::BlobHash::parse(&h).unwrap())
                .unwrap()
        })
    }
}

// Outlook-style: multipart/alternative with an inline text/calendar REQUEST
// using a TZID (Europe/Berlin -> +02:00 in July).
const OUTLOOK_INLINE_REQUEST: &str = "\
From: Chair <chair@tum.de>\r
To: me@example.com\r
Subject: LOC Day planning\r
Date: Mon, 13 Jul 2026 09:00:00 +0000\r
Message-ID: <outlook-1@tum.de>\r
MIME-Version: 1.0\r
Content-Type: multipart/alternative; boundary=\"BOUND\"\r
\r
--BOUND\r
Content-Type: text/plain; charset=UTF-8\r
\r
You are invited to LOC Day planning.\r
--BOUND\r
Content-Type: text/calendar; method=REQUEST; charset=UTF-8\r
Content-Transfer-Encoding: 7bit\r
\r
BEGIN:VCALENDAR\r
PRODID:-//Microsoft Corporation//Outlook 16.0 MIMEDIR//EN\r
VERSION:2.0\r
METHOD:REQUEST\r
BEGIN:VEVENT\r
UID:outlook-uid-1@tum.de\r
SEQUENCE:2\r
SUMMARY:LOC Day planning\r
DTSTART;TZID=Europe/Berlin:20260720T140000\r
DTEND;TZID=Europe/Berlin:20260720T150000\r
LOCATION:Room 4.12\r
ORGANIZER;CN=Chair:mailto:chair@tum.de\r
ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@example.com\r
END:VEVENT\r
END:VCALENDAR\r
--BOUND--\r
";

// Google-style: multipart/mixed with a text/plain body plus a named
// text/calendar attachment (invite.ics), UTC times and an RRULE.
const GOOGLE_ICS_ATTACHMENT_REQUEST: &str = "\
From: Host <host@gmail.com>\r
To: me@example.com\r
Subject: Weekly sync\r
Date: Mon, 13 Jul 2026 09:00:00 +0000\r
Message-ID: <google-1@gmail.com>\r
MIME-Version: 1.0\r
Content-Type: multipart/mixed; boundary=\"MIX\"\r
\r
--MIX\r
Content-Type: text/plain; charset=UTF-8\r
\r
You have been invited to Weekly sync.\r
--MIX\r
Content-Type: text/calendar; charset=UTF-8; method=REQUEST; name=\"invite.ics\"\r
Content-Disposition: attachment; filename=\"invite.ics\"\r
\r
BEGIN:VCALENDAR\r
PRODID:-//Google Inc//Google Calendar 70.9054//EN\r
VERSION:2.0\r
METHOD:REQUEST\r
BEGIN:VEVENT\r
UID:google-uid-1@google.com\r
SEQUENCE:0\r
SUMMARY:Weekly sync\r
DTSTART:20260720T120000Z\r
DTEND:20260720T130000Z\r
RRULE:FREQ=WEEKLY;BYDAY=MO\r
ORGANIZER:mailto:host@gmail.com\r
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r
END:VEVENT\r
END:VCALENDAR\r
--MIX--\r
";

// Malformed calendar part: valid MIME, unparseable ICS body.
const MALFORMED_INVITE: &str = "\
From: Someone <someone@example.com>\r
To: me@example.com\r
Subject: Broken invite\r
Date: Mon, 13 Jul 2026 09:00:00 +0000\r
Message-ID: <malformed-1@example.com>\r
MIME-Version: 1.0\r
Content-Type: multipart/alternative; boundary=\"B\"\r
\r
--B\r
Content-Type: text/plain; charset=UTF-8\r
\r
Body text.\r
--B\r
Content-Type: text/calendar; method=REQUEST; charset=UTF-8\r
\r
this is not a valid vcalendar payload at all\r
--B--\r
";

// An iMIP invite (REQUEST) PLUS a second, user-shared calendar export attached
// as a separate `.ics` document. Only the invite should be lifted to the
// sidecar; the export must survive as a regular attachment with its filename.
const INVITE_PLUS_SHARED_ICS: &str = "\
From: Host <host@gmail.com>\r
To: me@example.com\r
Subject: Weekly sync + my calendar\r
Date: Mon, 13 Jul 2026 09:00:00 +0000\r
Message-ID: <invite-plus-1@gmail.com>\r
MIME-Version: 1.0\r
Content-Type: multipart/mixed; boundary=\"MIX\"\r
\r
--MIX\r
Content-Type: text/plain; charset=UTF-8\r
\r
You have been invited to Weekly sync. Here's my calendar too.\r
--MIX\r
Content-Type: text/calendar; charset=UTF-8; method=REQUEST; name=\"invite.ics\"\r
Content-Disposition: attachment; filename=\"invite.ics\"\r
\r
BEGIN:VCALENDAR\r
VERSION:2.0\r
METHOD:REQUEST\r
BEGIN:VEVENT\r
UID:invite-plus-uid@google.com\r
SEQUENCE:0\r
SUMMARY:Weekly sync\r
DTSTART:20260720T120000Z\r
DTEND:20260720T130000Z\r
ORGANIZER:mailto:host@gmail.com\r
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r
END:VEVENT\r
END:VCALENDAR\r
--MIX\r
Content-Type: application/octet-stream; name=\"my-calendar.ics\"\r
Content-Disposition: attachment; filename=\"my-calendar.ics\"\r
\r
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//Me//Export//EN\r
BEGIN:VEVENT\r
UID:my-export-1@example.com\r
SUMMARY:Dentist\r
DTSTART:20260801T090000Z\r
DTEND:20260801T093000Z\r
END:VEVENT\r
END:VCALENDAR\r
--MIX--\r
";

// A single non-iMIP `.ics` calendar export (no METHOD property). It is not an
// invite: it must be stored as a regular attachment, keeping its filename, with
// no sidecar and no event block.
const SHARED_ICS_EXPORT_ONLY: &str = "\
From: Friend <friend@example.com>\r
To: me@example.com\r
Subject: My calendar export\r
Date: Mon, 13 Jul 2026 09:00:00 +0000\r
Message-ID: <export-only-1@example.com>\r
MIME-Version: 1.0\r
Content-Type: multipart/mixed; boundary=\"EXP\"\r
\r
--EXP\r
Content-Type: text/plain; charset=UTF-8\r
\r
Here is my calendar.\r
--EXP\r
Content-Type: text/calendar; charset=UTF-8; name=\"schedule.ics\"\r
Content-Disposition: attachment; filename=\"schedule.ics\"\r
\r
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//Me//Export//EN\r
BEGIN:VEVENT\r
UID:export-uid-1@example.com\r
SUMMARY:Standup\r
DTSTART:20260720T090000Z\r
DTEND:20260720T091500Z\r
END:VEVENT\r
END:VCALENDAR\r
--EXP--\r
";

// Plain multipart email with no calendar part.
const PLAIN_MULTIPART: &str = "\
From: Bob <bob@example.com>\r
To: me@example.com\r
Subject: Just a note\r
Date: Mon, 13 Jul 2026 09:00:00 +0000\r
Message-ID: <plain-1@example.com>\r
MIME-Version: 1.0\r
Content-Type: multipart/alternative; boundary=\"P\"\r
\r
--P\r
Content-Type: text/plain; charset=UTF-8\r
\r
Hello there.\r
--P\r
Content-Type: text/html; charset=UTF-8\r
\r
<p>Hello there.</p>\r
--P--\r
";

#[test]
fn outlook_inline_request_saves_sidecar_and_event() {

    let email = parse_rfc822_to_fetched_email(OUTLOOK_INLINE_REQUEST.as_bytes()).unwrap();
    assert!(email.calendar_ics.is_some());
    let ev = email.event.clone().expect("event parsed");
    assert_eq!(ev.method.as_deref(), Some("REQUEST"));
    assert_eq!(ev.uid.as_deref(), Some("outlook-uid-1@tum.de"));
    assert_eq!(ev.sequence, 2);
    assert_eq!(ev.start.as_deref(), Some("2026-07-20T14:00:00+02:00"));
    assert_eq!(ev.end.as_deref(), Some("2026-07-20T15:00:00+02:00"));
    assert_eq!(ev.location.as_deref(), Some("Room 4.12"));
    assert_eq!(ev.organizer.as_deref(), Some("chair@tum.de"));
    assert_eq!(ev.rsvp, "needs-action");
    assert_eq!(ev.attendees.len(), 1);

    // The invite lands as the sidecar-named attachment blob, and as the only
    // attachment: the inline calendar part is never stored twice.
    let ingested = ingest(&email, "inbox");
    assert_eq!(ingested.attachment_names(), vec![CALENDAR_SIDECAR_NAME.to_string()]);
    let ics = String::from_utf8(
        ingested.attachment_bytes(CALENDAR_SIDECAR_NAME).expect("sidecar blob"),
    )
    .unwrap();
    assert!(ics.contains("UID:outlook-uid-1@tum.de"));
    assert!(ics.contains("METHOD:REQUEST"));
}

#[test]
fn google_ics_attachment_request_saves_sidecar_and_event() {

    let email = parse_rfc822_to_fetched_email(GOOGLE_ICS_ATTACHMENT_REQUEST.as_bytes()).unwrap();
    let ev = email.event.clone().expect("event parsed");
    assert_eq!(ev.uid.as_deref(), Some("google-uid-1@google.com"));
    assert_eq!(ev.start.as_deref(), Some("2026-07-20T12:00:00Z"));
    assert_eq!(ev.recurrence, "Weekly on Monday");

    // The `.ics` attachment becomes the sidecar blob, never a second copy.
    let ingested = ingest(&email, "inbox");
    assert_eq!(ingested.attachment_names(), vec![CALENDAR_SIDECAR_NAME.to_string()]);
}

#[test]
fn malformed_calendar_part_stays_a_regular_attachment() {
    // This fixture's payload does not parse as a VCALENDAR and carries no
    // `METHOD` property (the `method=REQUEST` lives only in the MIME
    // Content-Type header, not the body). Under the corrected classification an
    // iMIP invite is the first calendar part whose PAYLOAD is a VCALENDAR with a
    // METHOD, so this part is NOT an invite: no sidecar, no event block. Its
    // bytes are preserved as a regular calendar attachment instead (previously
    // it was lifted to the sidecar -- expectation intentionally changed; the new
    // behavior loses no bytes and is more correct).

    let email = parse_rfc822_to_fetched_email(MALFORMED_INVITE.as_bytes()).unwrap();
    assert!(email.calendar_ics.is_none(), "not lifted to a sidecar");
    assert!(email.event.is_none());
    assert_eq!(email.attachments.len(), 1, "preserved as a regular attachment");
    // Inline calendar part had no filename, so a `.ics` name is synthesized.
    assert!(
        email.attachments[0].filename.ends_with(".ics"),
        "synthesized .ics filename, got {}",
        email.attachments[0].filename
    );
    assert_eq!(
        email.attachments[0].content,
        b"this is not a valid vcalendar payload at all\r\n",
        "original bytes preserved intact"
    );

    let ingested = ingest(&email, "inbox");
    assert!(
        ingested.attachment_bytes(CALENDAR_SIDECAR_NAME).is_none(),
        "no sidecar blob for a non-invite calendar part"
    );
    assert_eq!(ingested.attachment_names().len(), 1);
}

#[test]
fn invite_plus_shared_ics_lifts_invite_and_keeps_document() {

    let email = parse_rfc822_to_fetched_email(INVITE_PLUS_SHARED_ICS.as_bytes()).unwrap();
    // The invite is lifted to the sidecar and parsed into an event block.
    let ev = email.event.clone().expect("invite parsed");
    assert_eq!(ev.uid.as_deref(), Some("invite-plus-uid@google.com"));
    assert!(email.calendar_ics.is_some());
    // The second, non-invite `.ics` document survives as a regular attachment
    // with its original filename and full bytes -- previously it was lost.
    assert_eq!(email.attachments.len(), 1, "shared .ics preserved");
    assert_eq!(email.attachments[0].filename, "my-calendar.ics");
    assert!(
        email.attachments[0].content.starts_with(b"BEGIN:VCALENDAR"),
        "document bytes preserved"
    );
    assert!(
        email.attachments[0].content.windows(b"my-export-1@example.com".len())
            .any(|w| w == b"my-export-1@example.com"),
        "the shared export, not the invite, is the attachment"
    );

    let ingested = ingest(&email, "inbox");
    assert_eq!(
        ingested.attachment_names(),
        vec![CALENDAR_SIDECAR_NAME.to_string(), "my-calendar.ics".to_string()],
        "invite first as the sidecar, then the shared document"
    );
    assert!(ingested
        .attachment_bytes("my-calendar.ics")
        .unwrap()
        .starts_with(b"BEGIN:VCALENDAR"));
}

#[test]
fn non_imip_ics_export_is_a_plain_attachment() {

    let email = parse_rfc822_to_fetched_email(SHARED_ICS_EXPORT_ONLY.as_bytes()).unwrap();
    // No METHOD -> not an invite: no sidecar, no event block.
    assert!(email.calendar_ics.is_none(), "no sidecar for a plain export");
    assert!(email.event.is_none(), "no event block for a plain export");
    // Kept as a regular attachment with its original filename.
    assert_eq!(email.attachments.len(), 1);
    assert_eq!(email.attachments[0].filename, "schedule.ics");

    let ingested = ingest(&email, "inbox");
    assert_eq!(ingested.attachment_names(), vec!["schedule.ics".to_string()]);
    assert!(ingested.attachment_bytes(CALENDAR_SIDECAR_NAME).is_none());
}

#[test]
fn plain_multipart_email_is_unchanged() {

    let email = parse_rfc822_to_fetched_email(PLAIN_MULTIPART.as_bytes()).unwrap();
    assert!(email.calendar_ics.is_none());
    assert!(email.event.is_none());
    assert!(!email.has_attachments);

    let ingested = ingest(&email, "inbox");
    assert!(
        ingested.attachment_names().is_empty(),
        "a plain email references no attachment blobs"
    );
    let has_attachments: i64 = ingested
        .store
        .conn()
        .query_row(
            "SELECT has_attachments FROM messages WHERE id = ?1",
            [ingested.row],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_attachments, 0);
}

// ---------------------------------------------------------------------------
// Send-side round-trip (ticket #0028)
//
// Build a METHOD:REQUEST invite the way `mp send --invite` does, assemble the
// exact iMIP MIME tree via the shared `build_invite_mime_body`, then feed the
// formatted bytes back through the #0027 receive path. This proves the sent
// invite is picked up as an event (frontmatter + sidecar) -- the anchor #0030
// reconciliation relies on.
// ---------------------------------------------------------------------------

#[test]
fn sent_invite_roundtrips_through_receive_parser() {
    use chrono::{TimeZone, Utc};
    use mailypoppins::invite::{build_invite_ics, generate_uid, InviteSpec};
    use mailypoppins::send::build_invite_mime_body;
    use lettre::message::Message;

    let organizer = "chair@tum.de";
    let uid = generate_uid(organizer);
    let spec = InviteSpec {
        uid: uid.clone(),
        organizer: organizer.to_string(),
        attendees: vec!["a@example.com".to_string(), "b@example.com".to_string()],
        // Text that exercises RFC 5545 escaping + folding across the wire.
        summary: "LOC Day planning, part 2; final".to_string(),
        start: Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap(),
        end: Utc.with_ymd_and_hms(2026, 7, 20, 13, 0, 0).unwrap(),
        location: Some("Room 4.12; TUM".to_string()),
        description: Some("Agenda:\n- item one\n- item two".to_string()),
    };
    let ics = build_invite_ics(&spec).unwrap();

    // Assemble the iMIP MIME message (same builder as the live send path).
    let body = build_invite_mime_body(
        "You are invited.",
        "<p>You are invited.</p>".to_string(),
        &ics,
    );
    let message: Message = Message::builder()
        .from("Chair <chair@tum.de>".parse().unwrap())
        .to("a@example.com".parse().unwrap())
        .cc("b@example.com".parse().unwrap())
        .subject(&spec.summary)
        .message_id(Some("<sent-invite-1@tum.de>".to_string()))
        .multipart(body)
        .unwrap();
    let raw = message.formatted();

    // Header tree sanity: the inline calendar part with method=REQUEST is the
    // contract; the application/ics attachment is the optional hardening.
    let raw_str = String::from_utf8_lossy(&raw);
    assert!(raw_str.contains("multipart/mixed"), "top-level mixed");
    assert!(raw_str.contains("multipart/alternative"), "alternative present");
    assert!(
        raw_str.contains("text/calendar")
            && raw_str.to_lowercase().contains("method=request"),
        "inline text/calendar; method=REQUEST is the contract"
    );
    assert!(
        raw_str.contains("application/ics"),
        "optional application/ics hardening part present"
    );

    // Round-trip through the #0027 receive path.
    let email = parse_rfc822_to_fetched_email(&raw).unwrap();
    let ev = email.event.clone().expect("sent invite parsed back as an event");
    assert_eq!(ev.method.as_deref(), Some("REQUEST"));
    assert_eq!(ev.uid.as_deref(), Some(uid.as_str()));
    assert_eq!(ev.sequence, 0);
    assert_eq!(ev.summary.as_deref(), Some("LOC Day planning, part 2; final"));
    assert_eq!(ev.location.as_deref(), Some("Room 4.12; TUM"));
    assert_eq!(ev.organizer.as_deref(), Some("chair@tum.de"));
    assert_eq!(ev.start.as_deref(), Some("2026-07-20T12:00:00Z"));
    assert_eq!(ev.end.as_deref(), Some("2026-07-20T13:00:00Z"));
    assert_eq!(ev.attendees.len(), 2);
    assert_eq!(ev.attendees[0].address, "a@example.com");
    assert_eq!(ev.attendees[1].address, "b@example.com");
    assert!(email.calendar_ics.is_some(), "sidecar bytes carried");

    // Ingest into the store and confirm the sidecar bytes are the ones kept.
    let ingested = ingest(&email, "sent");
    let ics = String::from_utf8(
        ingested.attachment_bytes(CALENDAR_SIDECAR_NAME).expect("sidecar blob"),
    )
    .unwrap();
    assert!(ics.contains(&format!("UID:{uid}")));
    assert!(ics.contains("METHOD:REQUEST"));
}

// ---------------------------------------------------------------------------
// Reconciliation over the store (tickets #0030, #0038 scope item 6)
//
// The full-rescan test that lived here drove the `.md` writer to lay a sent
// invite and an incoming REPLY out on disk. That writer is gone (#0037 unit
// 4a) and reconciliation now derives attendee statuses from the `invite.ics`
// blobs of the account's rows, so the end-to-end coverage runs the same MIME
// fixtures through ingest and then through `reconcile::load_invites` -- the
// entry point the Calendar view's agenda is built on.
// ---------------------------------------------------------------------------

/// One store shared by several ingested messages, so a whole account can be
/// assembled and then reconciled (the single-message `ingest` above opens a
/// fresh store per call).
struct Mailstore {
    _tmp: tempfile::TempDir,
    store: mailypoppins::store::Store,
    blobs: mailypoppins::store::BlobStore,
}

impl Mailstore {
    fn new() -> Self {
        let tmp = tempdir().unwrap();
        let store = mailypoppins::store::Store::open(tmp.path().join("store.sqlite3")).unwrap();
        let blobs = mailypoppins::store::BlobStore::new(tmp.path().join("blobs"));
        Mailstore { _tmp: tmp, store, blobs }
    }

    /// Ingest raw RFC822 bytes through the real parse + ingest path.
    fn ingest_raw(&self, raw: &[u8], mailbox: &str, uid: i64) -> i64 {
        let email = parse_rfc822_to_fetched_email(raw).unwrap();
        mailypoppins::ingest::ingest_message(
            &self.store,
            &self.blobs,
            &mailypoppins::ingest::IngestInput {
                account: "acct",
                mailbox,
                uid,
                email: &email,
                raw: None,
            },
        )
        .unwrap()
        .row_id
    }

    fn invites(&self) -> Vec<mailypoppins::reconcile::InviteMessage> {
        mailypoppins::reconcile::load_invites(&self.store, &self.blobs, "acct")
    }
}

/// Only the fixtures classified as iMIP invites reach the calendar path. The
/// shared `.ics` export, the malformed calendar part and the plain email are
/// ordinary messages: they carry no `invite.ics` blob, so the invite listing
/// never sees them and no agenda row can come from them.
#[test]
fn only_imip_classified_messages_reach_the_calendar_path() {
    let ms = Mailstore::new();
    ms.ingest_raw(OUTLOOK_INLINE_REQUEST.as_bytes(), "inbox", 1);
    ms.ingest_raw(GOOGLE_ICS_ATTACHMENT_REQUEST.as_bytes(), "inbox", 2);
    ms.ingest_raw(SHARED_ICS_EXPORT_ONLY.as_bytes(), "inbox", 3);
    ms.ingest_raw(MALFORMED_INVITE.as_bytes(), "inbox", 4);
    ms.ingest_raw(PLAIN_MULTIPART.as_bytes(), "inbox", 5);

    let invites = ms.invites();
    let uids: Vec<Option<&str>> = invites.iter().map(|i| i.uid()).collect();
    assert_eq!(
        uids,
        vec![
            Some("outlook-uid-1@tum.de"),
            Some("google-uid-1@google.com")
        ],
        "only the two iMIP REQUESTs are invites"
    );
    assert!(invites.iter().all(|i| i.method() == "REQUEST"));
    // The zoned Outlook DTSTART survives the round trip through the blob.
    assert_eq!(invites[0].parsed.start.as_deref(), Some("2026-07-20T14:00:00+02:00"));
    assert_eq!(invites[0].parsed.sequence, 2);
}

/// The attendee's round trip, end to end over the store: an Outlook REQUEST is
/// ingested, the RSVP reply is built exactly as `send_rsvp` builds it, and its
/// sent copy is ingested the way the outbox does during the send. Our own
/// `PARTSTAT` is then derived from that sent REPLY, with nothing written to the
/// invitation, so a second pass reports the same numbers.
#[test]
fn an_rsvp_reply_reconciles_against_the_stored_invite() {
    use mailypoppins::invite::Rsvp;

    let ms = Mailstore::new();
    let request = ms.ingest_raw(OUTLOOK_INLINE_REQUEST.as_bytes(), "inbox", 1);

    // Read the invitation back out of the store, exactly as the TUI does.
    let ics = mailypoppins::store::read::load_invite_ics(&ms.store, &ms.blobs, request)
        .expect("the invite.ics blob");
    let ctx = mailypoppins::invite::reply_context_from_ics(&ics).unwrap();
    let reply_ics = mailypoppins::invite::build_reply_ics(&ctx, "me@example.com", Rsvp::Declined).unwrap();
    let built = mailypoppins::send::build_reply_message(
        "me@example.com",
        &ctx.organizer,
        "Declined: LOC Day planning",
        "Declined the invitation.",
        &reply_ics,
    )
    .unwrap();
    // What `outbox::ingest_sent_copy` files during the send itself.
    ms.ingest_raw(&built.raw, "sent", 1);

    let invites = ms.invites();
    assert_eq!(invites.len(), 2, "the REQUEST and our sent REPLY");
    let replies = mailypoppins::reconcile::fold_replies(&invites);
    let by_addr = replies.get("outlook-uid-1@tum.de");

    let request = invites
        .iter()
        .find(|i| i.method() == "REQUEST")
        .expect("the REQUEST");
    let mut event = mailypoppins::calendar::event_frontmatter(&request.parsed);
    mailypoppins::reconcile::apply_replies(&mut event, request.parsed.sequence, by_addr);
    assert_eq!(event.attendees[0].address, "me@example.com");
    assert_eq!(event.attendees[0].status, "declined");
    assert_eq!(
        mailypoppins::reconcile::own_rsvp(&event, "me@example.com", by_addr),
        "declined"
    );

    // The report is a read: two passes, same numbers, nothing written.
    let first = mailypoppins::reconcile::reconcile_account(&ms.store, &ms.blobs, "acct");
    assert_eq!(first.invites_seen, 1);
    assert_eq!(first.replies_seen, 1);
    assert_eq!(first.resolved, 1);
    assert_eq!(
        mailypoppins::reconcile::reconcile_account(&ms.store, &ms.blobs, "acct"),
        first
    );
}
