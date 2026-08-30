//! iMIP invite reconciliation over the store (#0030, moved onto rows and blobs
//! by [#0038](../docs/tickets/0038-read-path-to-db.md) scope item 6).
//!
//! An organizer's invitation is a `METHOD:REQUEST` message; an attendee's
//! answer is a `METHOD:REPLY` carrying that attendee's `PARTSTAT`. Both arrive
//! as ordinary mail and both are ingested as an ordinary row with an
//! `invite.ics` attachment blob. Reconciliation folds the replies onto the
//! invite so the organizer sees who responded, and so an attendee sees their
//! own answer on the invite they answered.
//!
//! # Derived, not stored
//!
//! Nothing here writes. The pre-store build rewrote `event.attendees[].status`
//! into the invite's `.md` frontmatter, but the store is a cache in front of
//! the server (a schema mismatch drops the whole file, `crate::store::schema`),
//! so a persisted fold would be a second source of truth that can drift from
//! the blobs it was computed from, and it buys nothing that recomputing does
//! not. The fold therefore runs where the answer is displayed, over the same
//! rows every time:
//!
//! - **Idempotent** by construction: there is no state to converge.
//! - **Multi-machine consistent** by construction: two machines holding the
//!   same messages compute the same statuses with no machine-to-machine sync.
//! - **Cheap**: an invite is a rare row, the query is one index-driven join,
//!   and each ics is a few kilobytes.
//!
//! `mp calendar rebuild` therefore reports what the fold resolves instead of
//! rewriting files, and [`ReconcileReport`] is a report rather than a diff.
//!
//! # Algorithm
//!
//! 1. [`load_invites`] reads every row of the account that carries an
//!    `invite.ics` blob and parses it. The ics is the only source: there is no
//!    frontmatter cache left to drift from it, which also removes the
//!    attachment-`.md` forgery surface TKT-0047 described (there is no `.md`
//!    on disk to walk, and an attachment blob is not a message row).
//! 2. [`fold_replies`] indexes the REPLYs by UID and attendee address
//!    (case-insensitive). The winner per address is the reply with the highest
//!    `(sequence, dtstamp)`: a newer sequence supersedes, ties break on the
//!    later `DTSTAMP`.
//! 3. [`apply_replies`] writes the winning `PARTSTAT`s onto an invite's
//!    in-memory attendee list, skipping replies older than the invite's own
//!    sequence. An address that was never invited is ignored: the attendee
//!    list belongs to the organizer's invitation.

use std::collections::HashMap;

use crate::calendar::ParsedEvent;
use crate::store::read::{self, MessageRow};
use crate::store::{BlobStore, Store};
use crate::types::EventFrontmatter;

/// One stored message carrying an iMIP payload: its row identity and the
/// parsed contents of its `invite.ics` blob.
#[derive(Debug, Clone)]
pub struct InviteMessage {
    /// `messages.id` of the row the payload came from.
    pub row_id: i64,
    /// The mailbox the row sits in. `sent` is what makes us the organizer of
    /// a REQUEST, the way the Sent *directory* used to.
    pub mailbox: String,
    /// The row's UID, used only as the final identity tiebreak.
    pub uid: i64,
    /// The email subject, the agenda's fallback title when the event carries
    /// no `SUMMARY`.
    pub subject: Option<String>,
    /// The parsed ics. Authoritative for UID, SEQUENCE, DTSTAMP and every
    /// displayed field.
    pub parsed: ParsedEvent,
}

impl InviteMessage {
    /// The upper-cased `METHOD`, empty when the payload carried none.
    pub fn method(&self) -> String {
        self.parsed
            .method
            .as_deref()
            .unwrap_or_default()
            .to_uppercase()
    }

    /// The trimmed iCal UID, `None` when it is absent or empty. An invite
    /// without one is still a real event; it just cannot be deduped or
    /// matched to a reply.
    pub fn uid(&self) -> Option<&str> {
        self.parsed
            .uid
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
    }

    /// `DTSTAMP` as a lexicographically comparable string, empty when unknown.
    pub fn dtstamp(&self) -> &str {
        self.parsed.dtstamp.as_deref().unwrap_or_default()
    }

    /// The normalised `RECURRENCE-ID`, `None` when the payload addresses the
    /// whole series (#0031).
    pub fn recurrence_id(&self) -> Option<&str> {
        self.parsed
            .recurrence_id
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
    }
}

/// A single REPLY observation: one attendee's `PARTSTAT` for one UID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyObs {
    /// Lowercased attendee address.
    pub address: String,
    /// Our lowercase status vocabulary (`accepted`, `declined`, ...).
    pub status: String,
    pub sequence: u32,
    /// RFC3339 UTC `DTSTAMP`, or empty when the source omitted it. Compared
    /// lexicographically, which is chronological for RFC3339 UTC strings.
    pub dtstamp: String,
}

/// UID -> attendee address -> the winning reply for that attendee.
pub type ReplyIndex = HashMap<String, HashMap<String, ReplyObs>>;

/// What one reconciliation pass saw and resolved. A report, not a diff:
/// nothing is written, so there is no "updated" count to give.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// REQUEST invites read from the store.
    pub invites_seen: usize,
    /// REPLY messages read from the store.
    pub replies_seen: usize,
    /// Attendee statuses the fold resolved onto an invite, counted once per
    /// (invite, attendee) pair.
    pub resolved: usize,
    /// Invites a `METHOD:CANCEL` tombstoned (#0031). They stay listed and
    /// readable; nothing is deleted.
    pub cancelled: usize,
}

/// Every invite of one account, parsed, in `(mailbox, uid)` order.
///
/// Rows whose ics blob is unreadable (retention evicted it) or unparseable are
/// skipped rather than failing the pass: one bad payload must not empty an
/// agenda. A store that cannot be queried yields an empty list, which is what
/// an account that has never synced looks like anyway.
pub fn load_invites(store: &Store, blobs: &BlobStore, account: &str) -> Vec<InviteMessage> {
    let rows = match read::list_invites(store, account) {
        Ok(rows) => rows,
        Err(e) => {
            log::warn!("[reconcile] listing invites for {account} failed: {e:#}");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|(row, hash)| invite_from_row(blobs, row, &hash))
        .collect()
}

/// Parse one invite row's ics blob into an [`InviteMessage`].
fn invite_from_row(blobs: &BlobStore, row: MessageRow, hash: &str) -> Option<InviteMessage> {
    let bytes = read::read_blob(blobs, row.id, hash)?;
    let parsed = crate::calendar::parse_ics(&bytes)?;
    Some(InviteMessage {
        row_id: row.id,
        mailbox: row.mailbox,
        uid: row.uid,
        subject: row.subject,
        parsed,
    })
}

/// Index the REPLYs of `invites` by UID and attendee, keeping the winner per
/// attendee (highest `(sequence, dtstamp)`).
///
/// A REPLY carries exactly the replying attendee, so the first attendee with a
/// usable address is the observation; a REPLY with no attendee at all is not
/// an observation and is skipped.
pub fn fold_replies(invites: &[InviteMessage]) -> ReplyIndex {
    let mut replies: ReplyIndex = HashMap::new();
    for invite in invites {
        if invite.method() != "REPLY" {
            continue;
        }
        let (Some(uid), Some(obs)) = (invite.uid(), reply_obs(invite)) else {
            continue;
        };
        let by_addr = replies.entry(uid.to_string()).or_default();
        match by_addr.get(&obs.address) {
            Some(existing) if !supersedes(&obs, existing) => {}
            _ => {
                by_addr.insert(obs.address.clone(), obs);
            }
        }
    }
    replies
}

/// The replying attendee's `(address, status)` from a REPLY payload.
fn reply_obs(invite: &InviteMessage) -> Option<ReplyObs> {
    let att = invite
        .parsed
        .attendees
        .iter()
        .find(|a| !a.address.trim().is_empty())?;
    Some(ReplyObs {
        address: att.address.trim().to_lowercase(),
        status: att.status.clone(),
        sequence: invite.parsed.sequence,
        dtstamp: invite.dtstamp().to_string(),
    })
}

/// Whether reply `a` supersedes reply `b` for the same attendee and UID: a
/// newer sequence wins, and within a sequence the later `DTSTAMP` wins.
fn supersedes(a: &ReplyObs, b: &ReplyObs) -> bool {
    (a.sequence, a.dtstamp.as_str()) > (b.sequence, b.dtstamp.as_str())
}

/// Apply the winning replies for one UID onto an invite's attendee list, and
/// return how many statuses were resolved.
///
/// `sequence` is the invite's own: a reply for an older sequence answered a
/// version of the event that no longer exists and is dropped.
pub fn apply_replies(
    event: &mut EventFrontmatter,
    sequence: u32,
    by_addr: Option<&HashMap<String, ReplyObs>>,
) -> usize {
    let Some(by_addr) = by_addr else { return 0 };
    let mut resolved = 0;
    for attendee in &mut event.attendees {
        let key = attendee.address.trim().to_lowercase();
        let Some(obs) = by_addr.get(&key) else {
            continue;
        };
        if obs.sequence < sequence {
            continue;
        }
        attendee.status = obs.status.clone();
        resolved += 1;
    }
    resolved
}

/// The cancellations recorded for one iCal UID (#0031).
#[derive(Debug, Default, Clone)]
struct UidCancels {
    /// Highest `SEQUENCE` of a whole-series `METHOD:CANCEL` (no
    /// `RECURRENCE-ID`) seen for this UID.
    series: Option<u32>,
    /// `RECURRENCE-ID` -> highest `SEQUENCE` of a single-occurrence CANCEL.
    instances: std::collections::BTreeMap<String, u32>,
}

/// The account-wide `(UID, RECURRENCE-ID)` version chain: which identities were
/// cancelled, and which copy of each is the current one (#0031).
///
/// Derived like everything else here: computed from the ics blobs on every
/// pass, never stored. Arrival order does not matter -- a CANCEL that reaches
/// the mailbox before its REQUEST is folded exactly like one that follows it,
/// because both are just rows when the fold runs.
#[derive(Debug, Default, Clone)]
pub struct StatusIndex {
    /// UID -> its cancellations.
    cancels: HashMap<String, UidCancels>,
    /// `(UID, RECURRENCE-ID or "")` -> the highest `(sequence, dtstamp)` seen
    /// among the REQUESTs for that identity.
    latest: HashMap<(String, String), (u32, String)>,
}

/// Fold every CANCEL and REQUEST of `invites` into a [`StatusIndex`].
///
/// Payloads with no usable UID are skipped: without one there is no identity
/// to cancel or supersede against, and an unrelated event must never be
/// tombstoned by a UID-less CANCEL.
pub fn fold_status(invites: &[InviteMessage]) -> StatusIndex {
    let mut index = StatusIndex::default();
    for invite in invites {
        let Some(uid) = invite.uid() else { continue };
        let seq = invite.parsed.sequence;
        match invite.method().as_str() {
            "CANCEL" => {
                let entry = index.cancels.entry(uid.to_string()).or_default();
                match invite.recurrence_id() {
                    // A `RECURRENCE-ID` scopes the cancellation to that one
                    // occurrence; the rest of the series lives on.
                    Some(rid) => {
                        let slot = entry.instances.entry(rid.to_string()).or_insert(seq);
                        *slot = (*slot).max(seq);
                    }
                    None => entry.series = Some(entry.series.unwrap_or(seq).max(seq)),
                }
            }
            "REQUEST" => {
                let key = (
                    uid.to_string(),
                    invite.recurrence_id().unwrap_or_default().to_string(),
                );
                let rank = (seq, invite.dtstamp().to_string());
                let slot = index.latest.entry(key).or_insert_with(|| rank.clone());
                if rank > *slot {
                    *slot = rank;
                }
            }
            _ => {}
        }
    }
    index
}

impl StatusIndex {
    /// Mark one event with what the account-wide fold knows about it:
    /// `cancelled`, `superseded`, and the individually cancelled occurrences
    /// of a series.
    ///
    /// `dtstamp` is the copy's own, the tiebreak within one `SEQUENCE`.
    ///
    /// Sequence rules (iTIP, RFC 5546 §3.2):
    /// - a CANCEL cancels a copy whose `SEQUENCE` is at or below its own; a
    ///   *stale* CANCEL (lower sequence than the surviving REQUEST) is a
    ///   cancellation of a version that was already replaced, and is ignored;
    /// - a REQUEST is superseded only by a strictly greater
    ///   `(SEQUENCE, DTSTAMP)`, so a re-delivered or replayed copy at an equal
    ///   or lower version never displaces the newer state.
    pub fn apply(&self, event: &mut EventFrontmatter, dtstamp: &str) {
        let Some(uid) = event.uid.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
            return;
        };
        let rid = event.recurrence_id.clone();
        let seq = event.sequence;
        if let Some(cancels) = self.cancels.get(uid) {
            let series_cancelled = cancels.series.is_some_and(|c| c >= seq);
            let instance_cancelled = rid
                .as_deref()
                .and_then(|r| cancels.instances.get(r))
                .is_some_and(|&c| c >= seq);
            event.cancelled = series_cancelled || instance_cancelled;
            // Only the series row reports per-occurrence cancellations; an
            // occurrence payload reports its own state in `cancelled`.
            if rid.is_none() {
                event.cancelled_instances = cancels
                    .instances
                    .iter()
                    .filter(|(_, &c)| c >= seq)
                    .map(|(r, _)| r.clone())
                    .collect();
            }
        }
        let key = (uid.to_string(), rid.unwrap_or_default());
        event.superseded = self
            .latest
            .get(&key)
            .is_some_and(|latest| *latest > (seq, dtstamp.to_string()));
    }
}

/// Our own answer to an invite: the winning REPLY we sent for it, falling back
/// to whatever `PARTSTAT` the organizer's own copy already carries for us.
///
/// The fallback matters because the two are the same fact from two directions:
/// once the organizer has processed our reply their next REQUEST carries it,
/// and before we have replied at all it reads `needs-action`, which is exactly
/// the pre-store default. Our own sent reply lands in the store during the
/// send itself (`crate::outbox::ingest_sent_copy` runs from the append), so
/// this answers correctly without waiting for a sync.
pub fn own_rsvp(
    event: &EventFrontmatter,
    self_address: &str,
    by_addr: Option<&HashMap<String, ReplyObs>>,
) -> String {
    let key = self_address.trim().to_lowercase();
    if !key.is_empty() {
        if let Some(obs) = by_addr.and_then(|m| m.get(&key)) {
            return obs.status.clone();
        }
        if let Some(att) = event
            .attendees
            .iter()
            .find(|a| a.address.trim().eq_ignore_ascii_case(&key))
        {
            return att.status.clone();
        }
    }
    "needs-action".to_string()
}

/// Reconcile every invite of one account and report what the fold resolved.
///
/// The primitive behind `mp calendar rebuild`. It is a read: running it twice
/// reports the same numbers and changes nothing, because there is nothing to
/// change.
pub fn reconcile_account(store: &Store, blobs: &BlobStore, account: &str) -> ReconcileReport {
    let invites = load_invites(store, blobs, account);
    let replies = fold_replies(&invites);
    let status = fold_status(&invites);
    let mut report = ReconcileReport {
        replies_seen: invites.iter().filter(|i| i.method() == "REPLY").count(),
        ..Default::default()
    };
    for invite in invites.iter().filter(|i| i.method() == "REQUEST") {
        report.invites_seen += 1;
        let mut event = crate::calendar::event_frontmatter(&invite.parsed);
        let by_addr = invite.uid().and_then(|uid| replies.get(uid));
        report.resolved += apply_replies(&mut event, invite.parsed.sequence, by_addr);
        status.apply(&mut event, invite.dtstamp());
        if event.cancelled {
            report.cancelled += 1;
        }
    }
    report
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ingest::{ingest_message, IngestInput};
    use crate::parse::FetchedEmail;
    use tempfile::TempDir;

    /// A store plus its blob store under one temp directory, with the real
    /// ingest path as the only writer, so the fixture rows are the rows sync
    /// writes. Shared with the calendar loader's tests, which need the same
    /// "ingest an invite" primitive.
    pub(crate) struct Fixture {
        _dir: TempDir,
        pub(crate) store: Store,
        pub(crate) blobs: BlobStore,
    }

    pub(crate) fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        Fixture {
            _dir: dir,
            store,
            blobs,
        }
    }

    impl Fixture {
        /// Ingest one message carrying `ics` into `mailbox`; returns its row id.
        pub(crate) fn ingest_invite(
            &self,
            mailbox: &str,
            uid: i64,
            subject: &str,
            ics: &str,
        ) -> i64 {
            self.ingest(mailbox, uid, subject, Some(ics))
        }

        /// Ingest one ordinary message, with no iMIP payload at all.
        pub(crate) fn ingest_plain(&self, mailbox: &str, uid: i64, subject: &str) -> i64 {
            self.ingest(mailbox, uid, subject, None)
        }

        /// Ingest one ordinary message carrying a named attachment, so a test
        /// can hand the store sender-controlled bytes (TKT-0047).
        pub(crate) fn ingest_with_attachment(
            &self,
            mailbox: &str,
            uid: i64,
            subject: &str,
            filename: &str,
            content: &[u8],
        ) -> i64 {
            let mut email = self.email(mailbox, uid, subject, None);
            email.has_attachments = true;
            email.attachments = vec![crate::parse::AttachmentData {
                filename: filename.to_string(),
                content: content.to_vec(),
                content_id: None,
            }];
            self.ingest_email(mailbox, uid, &email)
        }

        fn ingest(&self, mailbox: &str, uid: i64, subject: &str, ics: Option<&str>) -> i64 {
            let email = self.email(mailbox, uid, subject, ics);
            self.ingest_email(mailbox, uid, &email)
        }

        fn ingest_email(&self, mailbox: &str, uid: i64, email: &FetchedEmail) -> i64 {
            ingest_message(
                &self.store,
                &self.blobs,
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

        fn email(
            &self,
            mailbox: &str,
            uid: i64,
            subject: &str,
            ics: Option<&str>,
        ) -> FetchedEmail {
            FetchedEmail {
                from: "Organizer <me@example.com>".into(),
                to: "a@example.com".into(),
                cc: None,
                reply_to: None,
                bcc: None,
                subject: subject.into(),
                date: "Mon, 20 Jul 2026 09:00:00 +0000".into(),
                body_text: "You are invited.".into(),
                html_body: None,
                has_attachments: false,
                message_id: Some(format!("<{mailbox}-{uid}@example.com>")),
                attachments: Vec::new(),
                flags: Default::default(),
                calendar_ics: ics.map(|s| s.as_bytes().to_vec()),
                event: None,
            }
        }
    }

    /// A `METHOD:REQUEST` payload with `NEEDS-ACTION` attendees.
    pub(crate) fn invite_ics(uid: &str, seq: u32, attendees: &[&str]) -> String {
        let mut s = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             SEQUENCE:{seq}\r\nSUMMARY:Plan\r\nDTSTART:20260720T120000Z\r\n\
             ORGANIZER:mailto:me@example.com\r\n"
        );
        for addr in attendees {
            s.push_str(&format!("ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:{addr}\r\n"));
        }
        s.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
        s
    }

    /// A `METHOD:REPLY` payload carrying one attendee's answer.
    pub(crate) fn reply_ics(
        uid: &str,
        seq: u32,
        addr: &str,
        partstat: &str,
        dtstamp: &str,
    ) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             SEQUENCE:{seq}\r\nDTSTAMP:{dtstamp}\r\nORGANIZER:mailto:me@example.com\r\n\
             ATTENDEE;PARTSTAT={partstat}:mailto:{addr}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// Fold the store's replies onto one invite and return its attendee list.
    fn statuses(fx: &Fixture, uid: &str) -> Vec<(String, String)> {
        let invites = load_invites(&fx.store, &fx.blobs, "alice");
        let replies = fold_replies(&invites);
        let request = invites
            .iter()
            .find(|i| i.method() == "REQUEST" && i.uid() == Some(uid))
            .expect("the REQUEST is in the store");
        let mut event = crate::calendar::event_frontmatter(&request.parsed);
        apply_replies(&mut event, request.parsed.sequence, replies.get(uid));
        event
            .attendees
            .into_iter()
            .map(|a| (a.address, a.status))
            .collect()
    }

    #[test]
    fn a_reply_flips_the_matching_attendee() {
        let fx = fixture();
        fx.ingest_invite("sent", 1, "Plan", &invite_ics("u1@x", 0, &["a@example.com"]));
        fx.ingest_invite(
            "inbox",
            2,
            "Re: Plan",
            &reply_ics("u1@x", 0, "a@example.com", "ACCEPTED", "20260710T120000Z"),
        );
        assert_eq!(
            statuses(&fx, "u1@x"),
            vec![("a@example.com".to_string(), "accepted".to_string())]
        );
    }

    #[test]
    fn addresses_match_case_insensitively() {
        let fx = fixture();
        fx.ingest_invite(
            "sent",
            1,
            "Plan",
            &invite_ics("u1@x", 0, &["Alice@Example.com"]),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Re: Plan",
            &reply_ics("u1@x", 0, "alice@example.com", "DECLINED", "20260710T120000Z"),
        );
        assert_eq!(statuses(&fx, "u1@x")[0].1, "declined");
    }

    #[test]
    fn the_latest_dtstamp_wins_within_a_sequence() {
        let fx = fixture();
        fx.ingest_invite("sent", 1, "Plan", &invite_ics("u1@x", 0, &["a@example.com"]));
        fx.ingest_invite(
            "inbox",
            2,
            "Re: Plan",
            &reply_ics("u1@x", 0, "a@example.com", "ACCEPTED", "20260710T090000Z"),
        );
        fx.ingest_invite(
            "inbox",
            3,
            "Re: Plan",
            &reply_ics("u1@x", 0, "a@example.com", "DECLINED", "20260711T090000Z"),
        );
        assert_eq!(statuses(&fx, "u1@x")[0].1, "declined");
    }

    #[test]
    fn a_newer_sequence_reply_wins_and_an_older_one_is_ignored() {
        let fx = fixture();
        // The invite was bumped to sequence 2, so a reply for sequence 1
        // answered a version of the event that no longer exists.
        fx.ingest_invite("sent", 1, "Plan", &invite_ics("u1@x", 2, &["a@example.com"]));
        fx.ingest_invite(
            "inbox",
            2,
            "Re: Plan",
            &reply_ics("u1@x", 1, "a@example.com", "ACCEPTED", "20260710T120000Z"),
        );
        assert_eq!(statuses(&fx, "u1@x")[0].1, "needs-action");

        fx.ingest_invite(
            "inbox",
            3,
            "Re: Plan",
            &reply_ics("u1@x", 3, "a@example.com", "TENTATIVE", "20260709T090000Z"),
        );
        assert_eq!(statuses(&fx, "u1@x")[0].1, "tentative");
    }

    #[test]
    fn a_reply_from_an_uninvited_address_is_ignored() {
        let fx = fixture();
        fx.ingest_invite("sent", 1, "Plan", &invite_ics("u1@x", 0, &["a@example.com"]));
        fx.ingest_invite(
            "inbox",
            2,
            "Re: Plan",
            &reply_ics(
                "u1@x",
                0,
                "stranger@example.com",
                "ACCEPTED",
                "20260710T120000Z",
            ),
        );
        assert_eq!(statuses(&fx, "u1@x")[0].1, "needs-action");
    }

    /// The report counts what it saw and resolved, and a second pass reports
    /// the same numbers: with nothing written there is nothing to converge.
    #[test]
    fn the_report_is_stable_across_passes() {
        let fx = fixture();
        fx.ingest_invite(
            "sent",
            1,
            "Plan",
            &invite_ics("u1@x", 0, &["a@example.com", "b@example.com"]),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Re: Plan",
            &reply_ics("u1@x", 0, "a@example.com", "ACCEPTED", "20260710T120000Z"),
        );
        fx.ingest_invite(
            "inbox",
            3,
            "Re: Plan",
            &reply_ics("u1@x", 0, "b@example.com", "DECLINED", "20260710T120000Z"),
        );

        let first = reconcile_account(&fx.store, &fx.blobs, "alice");
        assert_eq!(
            first,
            ReconcileReport {
                invites_seen: 1,
                replies_seen: 2,
                resolved: 2,
                cancelled: 0,
            }
        );
        assert_eq!(
            reconcile_account(&fx.store, &fx.blobs, "alice"),
            first,
            "a second pass must report the same numbers"
        );
    }

    /// Our own answer comes from our own sent REPLY, which the outbox ingests
    /// during the send, so the invite shows it without waiting for a sync.
    #[test]
    fn our_own_rsvp_comes_from_our_own_reply() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Plan",
            &invite_ics("u1@x", 0, &["me@example.com"]),
        );
        let invites = load_invites(&fx.store, &fx.blobs, "alice");
        let request = invites.iter().find(|i| i.method() == "REQUEST").unwrap();
        let event = crate::calendar::event_frontmatter(&request.parsed);
        assert_eq!(
            own_rsvp(&event, "me@example.com", None),
            "needs-action",
            "before we answer, the organizer's PARTSTAT for us stands"
        );

        fx.ingest_invite(
            "sent",
            2,
            "Declined: Plan",
            &reply_ics("u1@x", 0, "me@example.com", "DECLINED", "20260710T120000Z"),
        );
        let invites = load_invites(&fx.store, &fx.blobs, "alice");
        let replies = fold_replies(&invites);
        assert_eq!(
            own_rsvp(&event, "Me@Example.com", replies.get("u1@x")),
            "declined"
        );
    }

    /// An unreadable or unparseable ics costs its own row and nothing else.
    #[test]
    fn an_unreadable_ics_skips_only_that_invite() {
        let fx = fixture();
        let broken = fx.ingest_invite("inbox", 1, "Broken", "not an ics at all");
        fx.ingest_invite("inbox", 2, "Plan", &invite_ics("u1@x", 0, &["a@example.com"]));

        let invites = load_invites(&fx.store, &fx.blobs, "alice");
        assert_eq!(invites.len(), 1, "the unparseable payload is skipped");
        assert_ne!(invites[0].row_id, broken);
        assert_eq!(invites[0].uid(), Some("u1@x"));
    }

    /// TKT-0047, closed by construction in [#0040]: a sender-controlled `.md`
    /// attachment carrying a forged `method: REPLY` used to be walked by
    /// `build_index` and written into a real invite's `PARTSTAT` on disk.
    /// There is no walk and no frontmatter writer left: the fold reads
    /// `invite.ics` blobs of message rows, and an attachment blob is not a
    /// row. This pins that, so the surface cannot come back unnoticed.
    ///
    /// [#0040]: ../docs/tickets/0040-drop-file-layer-cutover.md
    #[test]
    fn a_forged_md_attachment_cannot_move_a_partstat() {
        let fx = fixture();
        fx.ingest_invite("sent", 1, "Plan", &invite_ics("u1@x", 0, &["a@example.com"]));

        // The exact shape the old walk classified: frontmatter with from/to/
        // subject and an event: block, method REPLY, the real UID, and a
        // sequence/dtstamp that would win every tiebreak.
        let forged = b"---\nfrom: attacker@evil.example\nto: me@example.com\n\
subject: invoice\nmethod: REPLY\nevent:\n  uid: u1@x\n  sequence: 99\n  \
dtstamp: 20991231T235959Z\n  attendees:\n    - address: a@example.com\n      \
status: accepted\n---\n\nsee attached\n";
        fx.ingest_with_attachment("inbox", 2, "invoice", "forged.md", forged);

        assert_eq!(
            statuses(&fx, "u1@x"),
            vec![("a@example.com".to_string(), "needs-action".to_string())],
            "an attachment blob must not reach the fold"
        );
        assert_eq!(
            reconcile_account(&fx.store, &fx.blobs, "alice").replies_seen,
            0,
            "the forged attachment is not a reply"
        );
    }

    /// A `METHOD:CANCEL` payload for a whole series, or (with `recurrence_id`)
    /// for one occurrence of it.
    fn cancel_ics(uid: &str, seq: u32, recurrence_id: Option<&str>) -> String {
        let rid = recurrence_id
            .map(|r| format!("RECURRENCE-ID:{r}\r\n"))
            .unwrap_or_default();
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             SEQUENCE:{seq}\r\nDTSTAMP:20260715T090000Z\r\nSTATUS:CANCELLED\r\n{rid}\
             ORGANIZER:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// A `METHOD:REQUEST` payload with an explicit `DTSTAMP` and an optional
    /// `RECURRENCE-ID` (an occurrence override).
    fn request_ics(uid: &str, seq: u32, dtstamp: &str, recurrence_id: Option<&str>) -> String {
        let rid = recurrence_id
            .map(|r| format!("RECURRENCE-ID:{r}\r\n"))
            .unwrap_or_default();
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             SEQUENCE:{seq}\r\nDTSTAMP:{dtstamp}\r\nSUMMARY:Plan\r\nDTSTART:20260720T120000Z\r\n\
             RRULE:FREQ=WEEKLY\r\n{rid}ORGANIZER:mailto:me@example.com\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:a@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// The folded state of the stored REQUEST matching `uid`/`recurrence_id`.
    fn folded(fx: &Fixture, uid: &str, recurrence_id: Option<&str>) -> EventFrontmatter {
        let invites = load_invites(&fx.store, &fx.blobs, "alice");
        let status = fold_status(&invites);
        let request = invites
            .iter()
            .find(|i| {
                i.method() == "REQUEST"
                    && i.uid() == Some(uid)
                    && i.recurrence_id() == recurrence_id
            })
            .expect("the REQUEST is in the store");
        let mut event = crate::calendar::event_frontmatter(&request.parsed);
        status.apply(&mut event, request.dtstamp());
        event
    }

    /// A whole-series CANCEL tombstones the event: it is marked cancelled and
    /// still readable, never removed.
    #[test]
    fn a_cancel_tombstones_the_whole_event() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Plan", &request_ics("u1@x", 0, "20260701T090000Z", None));
        fx.ingest_invite("inbox", 2, "Cancelled: Plan", &cancel_ics("u1@x", 1, None));
        let event = folded(&fx, "u1@x", None);
        assert!(event.cancelled);
        assert!(event.cancelled_instances.is_empty());
        assert_eq!(event.summary.as_deref(), Some("Plan"), "the event is kept");
        assert_eq!(
            reconcile_account(&fx.store, &fx.blobs, "alice").cancelled,
            1
        );
    }

    /// Arrival order is irrelevant: a CANCEL ingested *before* its REQUEST
    /// tombstones it just as one ingested after would.
    #[test]
    fn a_cancel_that_arrives_before_its_request_still_applies() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Cancelled: Plan", &cancel_ics("u2@x", 1, None));
        fx.ingest_invite("inbox", 2, "Plan", &request_ics("u2@x", 0, "20260701T090000Z", None));
        assert!(folded(&fx, "u2@x", None).cancelled);
    }

    /// A CANCEL naming one `RECURRENCE-ID` kills that occurrence only: the
    /// series stays live and lists the cancelled occurrence.
    #[test]
    fn an_occurrence_cancel_does_not_kill_the_series() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Plan", &request_ics("u3@x", 0, "20260701T090000Z", None));
        fx.ingest_invite(
            "inbox",
            2,
            "Cancelled: Plan",
            &cancel_ics("u3@x", 1, Some("20260727T120000Z")),
        );
        let series = folded(&fx, "u3@x", None);
        assert!(!series.cancelled, "the series survives an occurrence cancel");
        assert_eq!(
            series.cancelled_instances,
            vec!["2026-07-27T12:00:00Z".to_string()]
        );
    }

    /// The occurrence override itself is what the occurrence CANCEL cancels,
    /// and the series row beside it is untouched.
    #[test]
    fn an_occurrence_cancel_marks_only_that_occurrence() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Plan", &request_ics("u4@x", 0, "20260701T090000Z", None));
        fx.ingest_invite(
            "inbox",
            2,
            "Plan (moved)",
            &request_ics("u4@x", 0, "20260702T090000Z", Some("20260727T120000Z")),
        );
        fx.ingest_invite(
            "inbox",
            3,
            "Cancelled: Plan",
            &cancel_ics("u4@x", 0, Some("20260727T120000Z")),
        );
        assert!(folded(&fx, "u4@x", Some("2026-07-27T12:00:00Z")).cancelled);
        assert!(!folded(&fx, "u4@x", None).cancelled);
    }

    /// A stale CANCEL (a lower sequence than the surviving REQUEST) cancelled
    /// a version that has already been replaced, and must not tombstone the
    /// rescheduled event.
    #[test]
    fn a_stale_cancel_does_not_tombstone_a_newer_request() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Cancelled: Plan", &cancel_ics("u5@x", 0, None));
        fx.ingest_invite("inbox", 2, "Plan", &request_ics("u5@x", 2, "20260701T090000Z", None));
        assert!(!folded(&fx, "u5@x", None).cancelled);
    }

    /// A UID-less CANCEL has no identity to apply to and must never tombstone
    /// an unrelated event.
    #[test]
    fn a_uidless_cancel_tombstones_nothing() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Plan", &request_ics("u6@x", 0, "20260701T090000Z", None));
        fx.ingest_invite(
            "inbox",
            2,
            "Cancelled",
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\n\
             SEQUENCE:9\r\nDTSTAMP:20260715T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert!(!folded(&fx, "u6@x", None).cancelled);
    }

    /// A malformed CANCEL is skipped like any other unparseable payload: the
    /// event stays live, the other invites are unaffected, and the pass does
    /// not fail.
    #[test]
    fn a_malformed_cancel_degrades_to_no_cancellation() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Plan", &request_ics("u7@x", 0, "20260701T090000Z", None));
        fx.ingest_invite(
            "inbox",
            2,
            "Cancelled: Plan",
            "BEGIN:VCALENDAR\r\nMETHOD:CANCEL\r\nthis is not an ics\r\n",
        );
        assert!(!folded(&fx, "u7@x", None).cancelled);
        assert_eq!(
            reconcile_account(&fx.store, &fx.blobs, "alice"),
            ReconcileReport {
                invites_seen: 1,
                replies_seen: 0,
                resolved: 0,
                cancelled: 0,
            }
        );
    }

    /// A re-issue with a higher `SEQUENCE` supersedes the copy already stored;
    /// the new copy is not itself superseded.
    #[test]
    fn a_higher_sequence_request_supersedes_the_stored_one() {
        let fx = fixture();
        fx.ingest_invite("inbox", 1, "Plan", &request_ics("u8@x", 0, "20260701T090000Z", None));
        fx.ingest_invite("inbox", 2, "Plan (moved)", &request_ics("u8@x", 1, "20260702T090000Z", None));
        let invites = load_invites(&fx.store, &fx.blobs, "alice");
        let status = fold_status(&invites);
        for invite in invites.iter().filter(|i| i.method() == "REQUEST") {
            let mut event = crate::calendar::event_frontmatter(&invite.parsed);
            status.apply(&mut event, invite.dtstamp());
            assert_eq!(
                event.superseded,
                event.sequence == 0,
                "sequence {} superseded={}",
                event.sequence,
                event.superseded
            );
        }
    }

    /// The clobber guard: a re-delivered copy at a lower *or equal*
    /// `(SEQUENCE, DTSTAMP)` never marks the newer state as superseded, so a
    /// replayed old invite cannot displace what is stored.
    #[test]
    fn a_stale_or_equal_sequence_request_never_supersedes() {
        let fx = fixture();
        // sequence 2 is the current version.
        fx.ingest_invite("inbox", 1, "Plan", &request_ics("u9@x", 2, "20260703T090000Z", None));
        // A replay of sequence 1, and a duplicate of sequence 2 with the same
        // DTSTAMP (the same version delivered twice).
        fx.ingest_invite("archive", 2, "Plan", &request_ics("u9@x", 1, "20260705T090000Z", None));
        fx.ingest_invite("archive", 3, "Plan", &request_ics("u9@x", 2, "20260703T090000Z", None));

        let invites = load_invites(&fx.store, &fx.blobs, "alice");
        let status = fold_status(&invites);
        let current = invites
            .iter()
            .find(|i| i.parsed.sequence == 2 && i.mailbox == "inbox")
            .unwrap();
        let mut event = crate::calendar::event_frontmatter(&current.parsed);
        status.apply(&mut event, current.dtstamp());
        assert!(
            !event.superseded,
            "a lower/equal-version copy must not supersede the stored one"
        );
    }

    /// An account with no invites reconciles to an empty report, not an error.
    #[test]
    fn an_account_with_no_invites_reports_nothing() {
        let fx = fixture();
        assert_eq!(
            reconcile_account(&fx.store, &fx.blobs, "alice"),
            ReconcileReport::default()
        );
    }
}
