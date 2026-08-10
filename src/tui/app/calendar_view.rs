//! Local calendar loader (#0034): build the Calendar view's agenda rows from
//! the iMIP messages the store already holds.
//!
//! This is deliberately local-first and **blind to Outlook-created events**:
//! only invitations that arrived (or were sent) by email exist locally, so an
//! event created directly in Outlook is invisible here until the Graph sync
//! backend lands (#0036). The UI states that caveat in the pane.
//!
//! Since #0038 scope item 6 the source is the store, not the `.md` tree: the
//! rows that carry an `invite.ics` attachment blob, with that blob parsed as
//! the single authority for UID, SEQUENCE, DTSTAMP and every displayed field.
//! There is no frontmatter cache left to drift from it, and no directory walk
//! to be fooled by a sender-controlled `.md` attachment (TKT-0047's exposure
//! is gone by construction; #0040 closes the ticket formally).
//!
//! Attendee statuses and our own RSVP are folded in from the REPLY rows by
//! [`crate::reconcile`], which computes them rather than storing them. The
//! whole agenda is therefore rebuilt from rows and blobs on demand, and is
//! kept in `CalendarView` until the user refreshes it (`r`) or switches
//! account, exactly as the walk-based build was.
//!
//! Cost: one indexed join over `messages` plus one blob read per invite row.
//! Invites are a rare row, the blobs are a few kilobytes, and none of it runs
//! at cold start: the agenda is built the first time the Calendar view is
//! opened, never during `App::new`.

use std::collections::HashMap;

use super::types::{CalendarEvent, MessageRef};
use crate::reconcile::{self, InviteMessage};
use crate::store::{BlobStore, Store};

/// The mailbox role our own sent mail lives in. A REQUEST found there is one
/// we sent, which is what makes us the organizer (the pre-store build asked
/// the same question of the `sent/` directory).
const SENT_MAILBOX: &str = "sent";

/// A REQUEST candidate before dedup, with its identity and tiebreak keys.
struct Candidate {
    row: CalendarEvent,
    /// `SEQUENCE` from the ics.
    sequence: u32,
    /// `DTSTAMP` from the ics, empty when unknown.
    dtstamp: String,
    /// The row's `(mailbox, uid)`, the final deterministic tiebreak.
    ident: (String, i64),
}

impl Candidate {
    /// Latest-wins ordering key: higher sequence, then later DTSTAMP, then our
    /// own sent copy, then `(mailbox, uid)` so ties resolve deterministically
    /// rather than by query order.
    ///
    /// The sent-copy component is load-bearing, not cosmetic: a self-invited
    /// event has one `DTSTAMP` shared by every copy, so without it the winner
    /// is whichever mailbox name sorts last and `is_organizer` flips for any
    /// custom mailbox sorting after `sent` (`team` beat `sent`).
    fn rank(&self) -> (u32, &str, bool, &str, i64) {
        (
            self.sequence,
            self.dtstamp.as_str(),
            self.row.is_organizer,
            self.ident.0.as_str(),
            self.ident.1,
        )
    }
}

/// Build the agenda rows for one account from its store, sorted by start
/// instant with undated events last.
///
/// `self_address` is the account's own address, used to resolve our own RSVP
/// out of the REPLY we sent (see [`crate::reconcile::own_rsvp`]).
///
/// Semantics, unchanged from the walk-based build:
/// - only `METHOD:REQUEST` messages are agenda rows (a `REPLY` is an attendee
///   response, folded into the REQUEST's `attendees[]` instead);
/// - one row per iCal UID, keeping the highest `(sequence, dtstamp)` copy, so
///   the Sent/Inbox/Archive copies of one event collapse into one row;
/// - events with no usable UID fall back to row identity (they are still real
///   events, they just cannot be deduped);
/// - a `METHOD:CANCEL` message tags its identity as cancelled when its sequence
///   is at least the surviving REQUEST's; the row **stays visible**, marked
///   cancelled, rather than being deleted (#0031: a tombstone the user can
///   still read, never silent data loss);
/// - identity is `(UID, RECURRENCE-ID)`: a CANCEL naming one occurrence of a
///   series cancels that occurrence only, and is listed on the series row as a
///   cancelled occurrence instead of tombstoning the whole series (#0031).
///
/// Never panics: an unreadable blob, an unparseable payload and an account
/// with no invites all degrade to fewer rows, and every field degrades to a
/// missing value.
pub fn load_events_for_account(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    self_address: &str,
) -> Vec<CalendarEvent> {
    let invites = reconcile::load_invites(store, blobs, account);
    let replies = reconcile::fold_replies(&invites);
    // Cancellations and the version chain, folded over the whole account so
    // arrival order does not matter: a CANCEL ingested before its REQUEST
    // tombstones it just the same (#0031).
    let status = reconcile::fold_status(&invites);

    let mut candidates: HashMap<String, Candidate> = HashMap::new();

    for invite in &invites {
        // Identity is (UID, RECURRENCE-ID): an occurrence override is its own
        // agenda row, not a duplicate of the series.
        let uid = invite.uid().map(|uid| match invite.recurrence_id() {
            Some(rid) => format!("{uid}\u{0}{rid}"),
            None => uid.to_string(),
        });
        // REPLY (an attendee's response), CANCEL (folded into `status`) and
        // anything unrecognised are not agenda rows.
        if invite.method() != "REQUEST" {
            continue;
        }

        let candidate = candidate_from(invite, self_address, &replies, &status);
        let key = uid.unwrap_or_else(|| format!("row:{}", invite.row_id));
        match candidates.get(&key) {
            Some(existing) if existing.rank() >= candidate.rank() => {}
            _ => {
                candidates.insert(key, candidate);
            }
        }
    }

    let mut events: Vec<CalendarEvent> =
        candidates.into_values().map(|candidate| candidate.row).collect();

    // Chronological, undated last; the row reference as the final tiebreak so
    // the order is stable across runs.
    events.sort_by(|a, b| {
        let a_key = (a.start_sort.is_empty(), &a.start_sort, a.msg);
        let b_key = (b.start_sort.is_empty(), &b.start_sort, b.msg);
        a_key.cmp(&b_key)
    });
    events
}

/// Turn one REQUEST into an agenda candidate, with the replies folded in.
fn candidate_from(
    invite: &InviteMessage,
    self_address: &str,
    replies: &reconcile::ReplyIndex,
    status: &reconcile::StatusIndex,
) -> Candidate {
    let mut event = crate::calendar::event_frontmatter(&invite.parsed);
    let by_addr = invite.uid().and_then(|uid| replies.get(uid));
    reconcile::apply_replies(&mut event, invite.parsed.sequence, by_addr);
    event.rsvp = reconcile::own_rsvp(&event, self_address, by_addr);
    status.apply(&mut event, invite.dtstamp());
    let cancelled = event.cancelled;

    let start = normalize_stamp(event.start.as_deref());
    let end = normalize_stamp(event.end.as_deref());
    // An all-day event with no explicit end stays "upcoming" through the end
    // of its own local day rather than expiring at local midnight.
    let end_sort = match (end.sort.is_empty(), start.all_day) {
        (true, true) => plus_one_day(&start.sort),
        _ => end.sort,
    };
    Candidate {
        row: CalendarEvent {
            msg: MessageRef::new(invite.row_id),
            subject: invite
                .subject
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(no subject)".to_string()),
            start_sort: start.sort,
            end_sort,
            start_display: start.display,
            is_organizer: invite.mailbox == SENT_MAILBOX,
            cancelled,
            event,
        },
        sequence: invite.parsed.sequence,
        dtstamp: invite.dtstamp().to_string(),
        ident: (invite.mailbox.clone(), invite.uid),
    }
}

/// A normalised `event:` timestamp: a UTC sort key, a human display string,
/// and whether the value carried no time of day (an all-day event).
#[derive(Debug, Default, PartialEq, Eq)]
struct Stamp {
    /// UTC-normalised `YYYY-MM-DDTHH:MM:SS` key, empty when unparseable.
    sort: String,
    /// Human-readable start in the event's own offset (or local wallclock).
    display: String,
    /// True for a `VALUE=DATE` / midnight-wallclock value: no time of day.
    all_day: bool,
}

/// Turn an `event:` start/end string into a [`Stamp`].
///
/// `start`/`end` are strings, not typed: `calendar::format_date_perhaps_time`
/// emits RFC3339-with-offset when a timezone was resolvable, an offset-less
/// wallclock for floating/unknown-TZ times, and midnight for all-day
/// (`VALUE=DATE`) events. Each form is tried in turn; anything else yields
/// empty strings (the row sorts last and shows no date) rather than panicking.
///
/// The sort key is always a UTC instant, including for the offset-less forms:
/// those are wallclock times, so they are resolved through the machine's local
/// zone (the same rule `invite.rs` uses for user-entered times). Without that
/// step a floating key would be compared against a UTC `now`, hiding a 09:00
/// event from "upcoming" hours early, and would order wrongly against events
/// that *do* carry an offset. The display keeps the event's own offset,
/// matching what other clients show (same rule as `resolve_date`, #0024).
fn normalize_stamp(value: Option<&str>) -> Stamp {
    let Some(raw) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Stamp::default();
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Stamp {
            sort: dt
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            display: dt.format("%Y-%m-%d %H:%M").to_string(),
            all_day: false,
        };
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        let all_day = naive.time() == chrono::NaiveTime::MIN;
        let display = if all_day {
            naive.format("%Y-%m-%d").to_string()
        } else {
            naive.format("%Y-%m-%d %H:%M").to_string()
        };
        return Stamp {
            sort: local_wallclock_to_utc_key(naive),
            display,
            all_day,
        };
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Stamp {
            sort: local_wallclock_to_utc_key(date.and_time(chrono::NaiveTime::MIN)),
            display: date.format("%Y-%m-%d").to_string(),
            all_day: true,
        };
    }
    Stamp::default()
}

/// Resolve an offset-less wallclock through the local zone into a UTC sort key.
/// DST edges degrade rather than fail: an ambiguous time takes the earliest
/// candidate (as `invite.rs` does) and a nonexistent one keeps the wallclock.
fn local_wallclock_to_utc_key(naive: chrono::NaiveDateTime) -> String {
    use chrono::TimeZone;
    let utc = match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.naive_utc(),
        chrono::LocalResult::Ambiguous(earliest, _) => earliest.naive_utc(),
        chrono::LocalResult::None => naive,
    };
    utc.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// One day after a `YYYY-MM-DDTHH:MM:SS` sort key, used as the implicit end of
/// an all-day event so it stays "upcoming" through its own local day instead of
/// expiring at local midnight. Returns the input unchanged if it does not parse.
///
/// A flat 24 h, not a calendar day: across a DST boundary the implicit end is
/// an hour off, which only shifts when a *finished* all-day event leaves the
/// upcoming scope. Overflow at the far end of the date range (a hand-edited
/// six-digit year) degrades to the input unchanged instead of panicking.
fn plus_one_day(sort_key: &str) -> String {
    match chrono::NaiveDateTime::parse_from_str(sort_key, "%Y-%m-%dT%H:%M:%S") {
        Ok(naive) => naive
            .checked_add_signed(chrono::Duration::days(1))
            .map(|next| next.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_else(|| sort_key.to_string()),
        Err(_) => sort_key.to_string(),
    }
}

/// The current instant as a `start_sort`-comparable UTC key.
pub fn now_sort_key() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::tests::{fixture, reply_ics, Fixture};

    /// A VEVENT payload for one ingested message.
    ///
    /// `dtstart` is the text that follows `DTSTART` in the property line, so a
    /// test can pick the exact iCalendar time form whose handling it is about:
    /// `":20260801T093000Z"` (UTC), `";TZID=Europe/Berlin:20260801T100000"`
    /// (zoned), `":20260801T090000"` (floating), `";VALUE=DATE:20260801"`
    /// (all-day), or `None` for an event with no start at all.
    ///
    /// `DTSTAMP` is fixed, so two copies of one event tie on `(sequence,
    /// dtstamp)` exactly as a self-invited event's copies do in the wild.
    fn event_ics(
        method: &str,
        uid: Option<&str>,
        sequence: u32,
        summary: &str,
        dtstart: Option<&str>,
    ) -> String {
        let uid_line = uid.map(|u| format!("UID:{u}\r\n")).unwrap_or_default();
        let start_line = dtstart
            .map(|d| format!("DTSTART{d}\r\n"))
            .unwrap_or_default();
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\n{uid_line}\
             SEQUENCE:{sequence}\r\nDTSTAMP:20260701T090000Z\r\nSUMMARY:{summary}\r\n{start_line}\
             ORGANIZER:mailto:org@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// A single-occurrence payload: `RECURRENCE-ID` naming one instance of the
    /// `uid` series, with a start on that instance.
    fn occurrence_ics(method: &str, uid: &str, sequence: u32, recurrence_id: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             SEQUENCE:{sequence}\r\nDTSTAMP:20260701T090000Z\r\nRECURRENCE-ID:{recurrence_id}\r\n\
             SUMMARY:Weekly (moved)\r\nDTSTART:{recurrence_id}\r\n\
             ORGANIZER:mailto:org@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// The agenda of the fixture account, with no own address to resolve.
    fn agenda(fx: &Fixture) -> Vec<CalendarEvent> {
        load_events_for_account(&fx.store, &fx.blobs, "alice", "")
    }

    /// Ingest one REQUEST and return the agenda it produces.
    fn agenda_of(mailbox: &str, uid: i64, ics: &str) -> Vec<CalendarEvent> {
        let fx = fixture();
        fx.ingest_invite(mailbox, uid, "Subject", ics);
        agenda(&fx)
    }

    fn subjects(events: &[CalendarEvent]) -> Vec<&str> {
        events.iter().map(|e| e.subject.as_str()).collect()
    }

    #[test]
    fn loads_request_invites_from_every_mailbox() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Standup",
            &event_ics("REQUEST", Some("uid-a"), 0, "Standup", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "archive",
            1,
            "Retro",
            &event_ics("REQUEST", Some("uid-b"), 0, "Retro", Some(":20260802T090000Z")),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.summary.as_deref(), Some("Standup"));
        assert_eq!(events[1].event.summary.as_deref(), Some("Retro"));
    }

    /// A message with no iMIP payload is not an invite row at all, so it never
    /// reaches the agenda (the listing query filters it out before any blob
    /// read).
    #[test]
    fn ignores_messages_without_an_ics() {
        let fx = fixture();
        fx.ingest_plain("inbox", 1, "Re: Plan");
        assert!(agenda(&fx).is_empty());
    }

    #[test]
    fn ignores_reply_method() {
        let events = agenda_of(
            "inbox",
            1,
            &event_ics("REPLY", Some("uid-a"), 0, "Standup", Some(":20260801T090000Z")),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn dedups_same_uid_keeping_highest_sequence() {
        let fx = fixture();
        fx.ingest_invite(
            "sent",
            1,
            "Planning",
            &event_ics(
                "REQUEST",
                Some("uid-dup"),
                0,
                "Planning",
                Some(":20260801T090000Z"),
            ),
        );
        fx.ingest_invite(
            "inbox",
            1,
            "Planning (moved)",
            &event_ics(
                "REQUEST",
                Some("uid-dup"),
                1,
                "Planning (moved)",
                Some(":20260801T150000Z"),
            ),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1, "one row per UID");
        assert_eq!(events[0].event.sequence, 1);
        assert_eq!(events[0].event.summary.as_deref(), Some("Planning (moved)"));
    }

    /// A REQUEST sitting in `sent` is one we sent, which is what makes us the
    /// organizer (the pre-store build asked the same of the `sent/` directory).
    #[test]
    fn sent_copies_are_flagged_as_organizer() {
        let fx = fixture();
        fx.ingest_invite(
            "sent",
            1,
            "My meeting",
            &event_ics(
                "REQUEST",
                Some("uid-mine"),
                0,
                "My meeting",
                Some(":20260801T090000Z"),
            ),
        );
        fx.ingest_invite(
            "inbox",
            1,
            "Their meeting",
            &event_ics(
                "REQUEST",
                Some("uid-theirs"),
                0,
                "Their meeting",
                Some(":20260802T090000Z"),
            ),
        );
        let events = agenda(&fx);
        let mine = events.iter().find(|e| e.subject == "My meeting").unwrap();
        let theirs = events.iter().find(|e| e.subject == "Their meeting").unwrap();
        assert!(mine.is_organizer);
        assert!(!theirs.is_organizer);
    }

    /// On an equal-`(sequence, dtstamp)` tie the sent copy wins, so
    /// `is_organizer` cannot flip on a mailbox name that sorts after `sent`.
    /// A self-invited event shares one DTSTAMP across every copy, so this tie
    /// is the normal case, not an exotic one.
    #[test]
    fn sent_copy_wins_an_equal_rank_tie() {
        let fx = fixture();
        let ics = event_ics(
            "REQUEST",
            Some("uid-tie"),
            0,
            "All hands",
            Some(":20260801T090000Z"),
        );
        fx.ingest_invite("sent", 1, "All hands", &ics);
        // `team` sorts after `sent`, so plain mailbox order would pick this copy.
        fx.ingest_invite("team", 1, "All hands", &ics);
        let events = agenda(&fx);
        assert_eq!(events.len(), 1);
        assert!(
            events[0].is_organizer,
            "our sent copy must win the tie, got {}",
            events[0].msg
        );
    }

    /// A CANCEL at or above the REQUEST's sequence tags the row; the event is
    /// still listed (display only -- #0031 owns the semantics).
    #[test]
    fn cancel_message_tags_the_uid_as_cancelled() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Doomed",
            &event_ics("REQUEST", Some("uid-c"), 0, "Doomed", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Cancelled: Doomed",
            &event_ics("CANCEL", Some("uid-c"), 1, "Doomed", Some(":20260801T090000Z")),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1, "the CANCEL is not its own agenda row");
        assert!(events[0].cancelled);
    }

    /// The boundary the doc comment promises: a CANCEL at *exactly* the
    /// REQUEST's sequence still tags it (`>=`, not `>`).
    #[test]
    fn cancel_at_the_same_sequence_tags_the_request() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Weekly",
            &event_ics("REQUEST", Some("uid-eq"), 2, "Weekly", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Cancelled: Weekly",
            &event_ics("CANCEL", Some("uid-eq"), 2, "Weekly", Some(":20260801T090000Z")),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1);
        assert!(
            events[0].cancelled,
            "cancel_seq == request_seq must still tag the row"
        );
    }

    /// An occurrence-level CANCEL (`RECURRENCE-ID`) must not tombstone the
    /// series row: the weekly meeting lives on, minus one occurrence, which
    /// the row reports so the user can see what was dropped (#0031).
    #[test]
    fn an_occurrence_cancel_leaves_the_series_row_live() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Weekly",
            &event_ics("REQUEST", Some("uid-r"), 0, "Weekly", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Cancelled: Weekly",
            &occurrence_ics("CANCEL", "uid-r", 1, "20260808T090000Z"),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1, "the CANCEL is not its own agenda row");
        assert!(!events[0].cancelled, "the series survives");
        assert_eq!(
            events[0].event.cancelled_instances,
            vec!["2026-08-08T09:00:00Z".to_string()]
        );
    }

    /// An occurrence override (`RECURRENCE-ID` on a REQUEST) is its own agenda
    /// row, cancellable on its own, and does not collapse into the series.
    #[test]
    fn an_occurrence_override_is_its_own_row() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Weekly",
            &event_ics("REQUEST", Some("uid-o"), 0, "Weekly", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Weekly (moved)",
            &occurrence_ics("REQUEST", "uid-o", 0, "20260808T090000Z"),
        );
        fx.ingest_invite(
            "inbox",
            3,
            "Cancelled: Weekly",
            &occurrence_ics("CANCEL", "uid-o", 1, "20260808T090000Z"),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 2, "series and override are separate rows");
        let series = events.iter().find(|e| e.subject == "Weekly").unwrap();
        let override_row = events
            .iter()
            .find(|e| e.subject == "Weekly (moved)")
            .unwrap();
        assert!(!series.cancelled);
        assert!(override_row.cancelled, "the occurrence itself is cancelled");
    }

    /// A malformed CANCEL costs itself and nothing else: the event it names
    /// stays live and listed rather than the whole agenda failing (#0031).
    #[test]
    fn a_malformed_cancel_leaves_the_agenda_intact() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Weekly",
            &event_ics("REQUEST", Some("uid-m"), 0, "Weekly", Some(":20260801T090000Z")),
        );
        fx.ingest_invite("inbox", 2, "Cancelled: Weekly", "METHOD:CANCEL but broken");
        let events = agenda(&fx);
        assert_eq!(subjects(&events), vec!["Weekly"]);
        assert!(!events[0].cancelled);
    }

    /// A CANCEL ingested before the REQUEST it cancels still tombstones it:
    /// the fold sees rows, not an arrival order (#0031).
    #[test]
    fn a_cancel_before_the_request_still_tombstones_the_row() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Cancelled: Doomed",
            &event_ics("CANCEL", Some("uid-ooo"), 0, "Doomed", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Doomed",
            &event_ics("REQUEST", Some("uid-ooo"), 0, "Doomed", Some(":20260801T090000Z")),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1, "the event is kept, not deleted");
        assert!(events[0].cancelled);
        assert!(events[0].event.cancelled, "the shared card says so too");
    }

    /// A re-issued invite at a higher SEQUENCE replaces the stored copy, and a
    /// replay of the older one (ingested afterwards) does not clobber it back.
    #[test]
    fn a_higher_sequence_replaces_and_a_replay_does_not_clobber() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Weekly",
            &event_ics("REQUEST", Some("uid-up"), 0, "Weekly", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Weekly (moved)",
            &event_ics(
                "REQUEST",
                Some("uid-up"),
                3,
                "Weekly (moved)",
                Some(":20260802T090000Z"),
            ),
        );
        // The same old version delivered again, into another mailbox.
        fx.ingest_invite(
            "archive",
            3,
            "Weekly",
            &event_ics("REQUEST", Some("uid-up"), 0, "Weekly", Some(":20260801T090000Z")),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.sequence, 3);
        assert_eq!(events[0].event.summary.as_deref(), Some("Weekly (moved)"));
        assert!(!events[0].event.superseded, "the winner is the current one");
    }

    /// A stale CANCEL (older sequence than the surviving REQUEST) does not tag
    /// the rescheduled event.
    #[test]
    fn stale_cancel_does_not_tag_a_newer_request() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Cancelled: Weekly",
            &event_ics("CANCEL", Some("uid-s"), 0, "Weekly", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Weekly",
            &event_ics("REQUEST", Some("uid-s"), 2, "Weekly", Some(":20260808T090000Z")),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1);
        assert!(!events[0].cancelled);
    }

    /// Two events on the same calendar day in different zones must order by
    /// actual instant, not by wallclock (mirrors `resolve_date`, #0024).
    #[test]
    fn sorts_by_start_instant_not_wallclock() {
        let fx = fixture();
        // 10:00 Europe/Berlin == 08:00 UTC in August (the earlier instant).
        fx.ingest_invite(
            "inbox",
            1,
            "Early",
            &event_ics(
                "REQUEST",
                Some("uid-e"),
                0,
                "Early",
                Some(";TZID=Europe/Berlin:20260801T100000"),
            ),
        );
        // 09:30 UTC (the later instant).
        fx.ingest_invite(
            "inbox",
            2,
            "Late",
            &event_ics("REQUEST", Some("uid-l"), 0, "Late", Some(":20260801T093000Z")),
        );
        let events = agenda(&fx);
        assert_eq!(subjects(&events), vec!["Early", "Late"]);
        // The display keeps the event's own offset.
        assert_eq!(events[0].start_display, "2026-08-01 10:00");
    }

    #[test]
    fn handles_missing_start_without_panicking() {
        let fx = fixture();
        fx.ingest_invite(
            "inbox",
            1,
            "Dated",
            &event_ics("REQUEST", Some("uid-d"), 0, "Dated", Some(":20260801T090000Z")),
        );
        fx.ingest_invite(
            "inbox",
            2,
            "Undated",
            &event_ics("REQUEST", Some("uid-u"), 0, "Undated", None),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].subject, "Undated", "undated events sort last");
        assert!(events[1].start_sort.is_empty());
        assert!(events[1].start_display.is_empty());
    }

    /// An unparseable payload costs its own row and nothing else, and an
    /// account that has never synced yields an empty agenda rather than an
    /// error.
    #[test]
    fn unparseable_payloads_and_empty_accounts_yield_no_rows() {
        let fx = fixture();
        assert!(agenda(&fx).is_empty(), "no invites yet");
        fx.ingest_invite("inbox", 1, "Broken", "not an ics at all");
        assert!(agenda(&fx).is_empty());
        fx.ingest_invite(
            "inbox",
            2,
            "Real",
            &event_ics("REQUEST", Some("uid-ok"), 0, "Real", Some(":20260801T090000Z")),
        );
        assert_eq!(subjects(&agenda(&fx)), vec!["Real"]);
    }

    /// Invites with no UID at all are kept (keyed by row) rather than dropped
    /// or collapsed into each other.
    #[test]
    fn keeps_uidless_invites_keyed_by_row() {
        let fx = fixture();
        for uid in [1, 2] {
            fx.ingest_invite(
                "inbox",
                uid,
                "No UID",
                &event_ics("REQUEST", None, 0, "No UID", Some(":20260801T090000Z")),
            );
        }
        assert_eq!(agenda(&fx).len(), 2);
    }

    /// All-day (`VALUE=DATE`) invites get a date-only display, a UTC-normalised
    /// sort key, and an implicit end at the start of the next local day.
    #[test]
    fn all_day_events_display_as_a_bare_date() {
        let events = agenda_of(
            "inbox",
            1,
            &event_ics(
                "REQUEST",
                Some("uid-a"),
                0,
                "Offsite",
                Some(";VALUE=DATE:20260801"),
            ),
        );
        assert_eq!(events[0].start_display, "2026-08-01");
        // Local midnight, expressed as a UTC instant.
        assert_eq!(
            events[0].start_sort,
            local_wallclock_to_utc_key(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
                    .unwrap()
                    .and_time(chrono::NaiveTime::MIN)
            )
        );
        assert_eq!(events[0].end_sort, plus_one_day(&events[0].start_sort));
    }

    /// A date at the far end of chrono's range must not panic in
    /// `plus_one_day` (the implicit-end `+1 day` used to overflow); it degrades
    /// to an unchanged end key.
    #[test]
    fn far_future_all_day_key_does_not_overflow() {
        let max_key = chrono::NaiveDateTime::MAX
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert_eq!(plus_one_day(&max_key), max_key);
    }

    /// An offset-less wallclock is a *local* time, so it must normalise to the
    /// same UTC key as the same instant written with its offset. Without that
    /// step the key is compared against a UTC `now` and the upcoming filter
    /// skews by the local offset (up to ±14 h), and floating events order
    /// wrongly against events that do carry an offset.
    #[test]
    fn floating_wallclock_normalises_through_the_local_zone() {
        use chrono::{Offset, TimeZone};
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let offset = chrono::Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .offset()
            .fix();
        let with_offset = format!("2026-08-01T09:00:00{offset}");
        assert_eq!(
            normalize_stamp(Some("2026-08-01T09:00:00")).sort,
            normalize_stamp(Some(&with_offset)).sort,
            "floating 09:00 must be the same instant as local 09:00"
        );
    }

    /// An all-day event for *today* stays in the default upcoming scope all
    /// day. It normalises as offset-less local midnight, so without the
    /// implicit end its key sorts before a UTC `now` and the row vanishes from
    /// 00:00 onward.
    #[test]
    fn todays_all_day_event_stays_upcoming() {
        let fx = fixture();
        let today = chrono::Local::now().date_naive();
        fx.ingest_invite(
            "inbox",
            1,
            "Offsite",
            &event_ics(
                "REQUEST",
                Some("uid-today"),
                0,
                "Offsite",
                Some(&format!(";VALUE=DATE:{}", today.format("%Y%m%d"))),
            ),
        );
        // Yesterday's all-day event is over and must not linger.
        fx.ingest_invite(
            "inbox",
            2,
            "Past offsite",
            &event_ics(
                "REQUEST",
                Some("uid-yesterday"),
                0,
                "Past offsite",
                Some(&format!(
                    ";VALUE=DATE:{}",
                    (today - chrono::Duration::days(1)).format("%Y%m%d")
                )),
            ),
        );
        assert_eq!(upcoming(&fx), vec!["Offsite"]);
    }

    /// A floating (offset-less) event that already finished must leave the
    /// upcoming scope on time. Compared against a UTC `now` without the local
    /// normalisation it lingers for the length of the local offset: vacuous on
    /// a UTC machine, sharp everywhere else.
    #[test]
    fn a_finished_floating_event_leaves_the_upcoming_scope() {
        let fx = fixture();
        let now = chrono::Local::now().naive_local();
        for (uid, subject, minutes) in [
            ("uid-past", "Just finished", -45),
            ("uid-soon", "Starting soon", 45),
        ] {
            let start = (now + chrono::Duration::minutes(minutes))
                .format("%Y%m%dT%H%M%S")
                .to_string();
            fx.ingest_invite(
                "inbox",
                if minutes < 0 { 1 } else { 2 },
                subject,
                &event_ics("REQUEST", Some(uid), 0, subject, Some(&format!(":{start}"))),
            );
        }
        assert_eq!(upcoming(&fx), vec!["Starting soon"]);
    }

    /// The subjects the Calendar view's default (upcoming-only) scope shows.
    fn upcoming(fx: &Fixture) -> Vec<String> {
        let mut app = crate::tui::app::App::default_for_tests();
        app.calendar_view.events = agenda(fx);
        app.calendar_view.loaded = true;
        app.recompute_calendar_visible();
        app.calendar_view
            .visible
            .iter()
            .map(|&i| app.calendar_view.events[i].subject.clone())
            .collect()
    }

    /// The organizer's view: an attendee's REPLY lands on the agenda row's
    /// attendee list without anything being written.
    #[test]
    fn a_reply_folds_onto_the_agenda_row() {
        let fx = fixture();
        fx.ingest_invite(
            "sent",
            1,
            "Plan",
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
             UID:uid-fold\r\nSEQUENCE:0\r\nDTSTAMP:20260701T090000Z\r\nSUMMARY:Plan\r\n\
             DTSTART:20260801T090000Z\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:a@example.com\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(agenda(&fx)[0].event.attendees[0].status, "needs-action");

        fx.ingest_invite(
            "inbox",
            2,
            "Accepted: Plan",
            &reply_ics(
                "uid-fold",
                0,
                "a@example.com",
                "ACCEPTED",
                "20260710T120000Z",
            ),
        );
        let events = agenda(&fx);
        assert_eq!(events.len(), 1, "the REPLY is not its own agenda row");
        assert_eq!(events[0].event.attendees[0].status, "accepted");
    }

    /// The attendee's view, end to end over the store: the REQUEST arrives,
    /// our RSVP is sent, and the sent-copy REPLY the outbox ingests is where
    /// our own `PARTSTAT` comes from. Nothing is written to the invite: the
    /// agenda derives `rsvp` from that REPLY on every rebuild.
    #[test]
    fn our_own_rsvp_reaches_the_agenda_through_the_sent_reply() {
        let fx = fixture();
        let request = fx.ingest_invite(
            "inbox",
            1,
            "Plan",
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
             UID:uid-rsvp\r\nSEQUENCE:0\r\nDTSTAMP:20260701T090000Z\r\nSUMMARY:Plan\r\n\
             DTSTART:20260801T090000Z\r\nORGANIZER:mailto:org@example.com\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let me = "me@example.com";
        let before = load_events_for_account(&fx.store, &fx.blobs, "alice", me);
        assert_eq!(before[0].event.rsvp, "needs-action");
        assert_eq!(before[0].msg, MessageRef::new(request));

        // What `outbox::ingest_sent_copy` does during the send itself.
        let reply = fx.ingest_invite(
            "sent",
            2,
            "Declined: Plan",
            &reply_ics("uid-rsvp", 0, me, "DECLINED", "20260710T120000Z"),
        );

        // The REPLY is a real invite row, and its blob carries the PARTSTAT.
        let row = crate::store::read::find_by_id(&fx.store, reply)
            .unwrap()
            .expect("the sent reply is a row");
        assert!(row.is_invite, "the sent copy carries an invite.ics blob");
        assert_eq!(row.mailbox, "sent");
        let ics = crate::store::read::load_invite_ics(&fx.store, &fx.blobs, reply)
            .expect("the reply's ics blob");
        let ics = String::from_utf8(ics).unwrap();
        assert!(ics.contains("METHOD:REPLY"), "{ics}");
        assert!(ics.contains("PARTSTAT=DECLINED"), "{ics}");

        // And the agenda derives our answer from it, with the invite row
        // untouched: still one agenda row, still the REQUEST's.
        let after = load_events_for_account(&fx.store, &fx.blobs, "alice", me);
        assert_eq!(after.len(), 1, "the REPLY is not a second agenda row");
        assert_eq!(after[0].msg, MessageRef::new(request));
        assert_eq!(after[0].event.rsvp, "declined");
        assert_eq!(after[0].event.attendees[0].status, "declined");
    }
}

