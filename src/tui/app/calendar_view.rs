//! Local calendar loader (#0034): build the Calendar view's agenda rows from
//! the `.md` files the iMIP traffic already produced.
//!
//! This is deliberately local-first and **blind to Outlook-created events**:
//! only invitations that arrived (or were sent) by email exist on disk, so an
//! event created directly in Outlook is invisible here until the Graph sync
//! backend lands (#0036). The UI states that caveat in the pane.
//!
//! The walk mirrors `crate::reconcile::build_index`: every `.md` under the
//! account root, frontmatter-parsed, with the sidecar `invite.ics` treated as
//! authoritative for UID/SEQUENCE (the `event:` block is a lossy cache). It is
//! a free function taking only a path so it stays unit-testable and could be
//! moved off the UI thread unchanged.

use std::collections::HashMap;
use std::path::Path;

use gray_matter::engine::YAML;
use gray_matter::Matter;
use serde::Deserialize;

use super::types::CalendarEvent;

/// The frontmatter subset the calendar walk needs.
#[derive(Debug, Deserialize, Default)]
struct CalendarFrontmatter {
    subject: Option<String>,
    #[serde(default)]
    event: Option<crate::types::EventFrontmatter>,
}

/// A REQUEST candidate before dedup, with its identity/tiebreak keys.
struct Candidate {
    row: CalendarEvent,
    /// Authoritative sequence (sidecar-first).
    sequence: u32,
    /// Authoritative `DTSTAMP`, empty when unknown.
    dtstamp: String,
}

impl Candidate {
    /// Latest-wins ordering key: higher sequence, then later DTSTAMP, then our
    /// own sent copy, then the path (so ties are resolved deterministically
    /// rather than by walk order).
    ///
    /// The sent-copy component is load-bearing, not cosmetic: a self-invited
    /// event has one `DTSTAMP` shared by every copy, so without it the winner
    /// is whichever mailbox name sorts last and `is_organizer` flips for any
    /// custom mailbox sorting after `sent` (`team/` beat `sent/`).
    fn rank(&self) -> (u32, &str, bool, &Path) {
        (
            self.sequence,
            self.dtstamp.as_str(),
            self.row.is_organizer,
            self.row.path.as_path(),
        )
    }
}

/// True for paths inside an attachment directory, in either layout `parse.rs`
/// writes: the per-email `<stem>_attachments/` sidecar dir and the account-wide
/// `attachments/<message-id>/` mirror.
///
/// Those files are *inbound, sender-controlled content*. A `.md` attached to an
/// email carries frontmatter an attacker chooses, so ingesting it would let a
/// crafted attachment spoof an agenda row, displace a real invite (same UID,
/// higher sequence) or strike it through with a forged `METHOD:CANCEL`. The
/// invite's own `invite.ics` sidecar still lives there and is still read, but
/// only through [`authoritative_ids`], keyed off a real email's path.
fn is_attachment_path(path: &Path) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            name == "attachments" || name.ends_with("_attachments")
        }
        _ => false,
    })
}

/// Walk `account_root` and return the agenda rows for the Calendar view,
/// sorted by start instant with undated events last.
///
/// Semantics:
/// - only `METHOD:REQUEST` messages are agenda rows (a `REPLY` is an attendee
///   response to one of our invites, already folded into the REQUEST's
///   `attendees[]` by `mp calendar rebuild`);
/// - one row per iCal UID, keeping the highest `(sequence, dtstamp)` copy, so
///   the Sent/Inbox/Archive copies of one event collapse into one row;
/// - events with no usable UID fall back to path identity (they are still real
///   events, they just cannot be deduped);
/// - a `METHOD:CANCEL` message tags its UID as cancelled when its sequence is
///   at least the surviving REQUEST's; the row stays visible (display only --
///   the cancellation *semantics* are #0031).
///
/// Never panics: unreadable files, malformed YAML and missing directories are
/// skipped, and every field degrades to a missing value.
pub fn load_events_for_account(account_root: &Path) -> Vec<CalendarEvent> {
    if !account_root.is_dir() {
        return Vec::new();
    }
    // Our own sent invites live under `<account_root>/sent` (the fixed role dir
    // from `config::mailbox_dir`); that is what makes us the organizer.
    let sent_root = account_root.join("sent");

    let mut candidates: HashMap<String, Candidate> = HashMap::new();
    // uid -> highest CANCEL sequence seen.
    let mut cancels: HashMap<String, u32> = HashMap::new();
    let matter = Matter::<YAML>::new();

    for entry in walkdir::WalkDir::new(account_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        if is_attachment_path(path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let parsed = matter.parse(&content);
        let fm: CalendarFrontmatter = parsed
            .data
            .and_then(|d| d.deserialize().ok())
            .unwrap_or_default();
        let Some(event) = fm.event else {
            continue;
        };
        let method = event.method.as_deref().unwrap_or("").to_uppercase();
        let (uid, sequence, dtstamp) = authoritative_ids(path, &event);
        let uid = uid.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());

        match method.as_str() {
            "CANCEL" => {
                if let Some(uid) = uid {
                    let slot = cancels.entry(uid).or_insert(sequence);
                    *slot = (*slot).max(sequence);
                }
                continue;
            }
            "REQUEST" => {}
            // REPLY (an attendee's response) and anything unrecognised are not
            // agenda rows.
            _ => continue,
        }

        let start = normalize_stamp(event.start.as_deref());
        let end = normalize_stamp(event.end.as_deref());
        // An all-day event with no explicit end stays "upcoming" through the
        // end of its own local day rather than expiring at local midnight.
        let end_sort = match (end.sort.is_empty(), start.all_day) {
            (true, true) => plus_one_day(&start.sort),
            _ => end.sort,
        };
        let (start_sort, start_display) = (start.sort, start.display);
        let row = CalendarEvent {
            path: path.to_path_buf(),
            subject: fm.subject.unwrap_or_else(|| "(no subject)".to_string()),
            start_sort,
            end_sort,
            start_display,
            is_organizer: path.starts_with(&sent_root),
            cancelled: false,
            event,
        };
        let key = uid.unwrap_or_else(|| path.to_string_lossy().to_string());
        let candidate = Candidate { row, sequence, dtstamp };
        match candidates.get(&key) {
            Some(existing) if existing.rank() >= candidate.rank() => {}
            _ => {
                candidates.insert(key, candidate);
            }
        }
    }

    let mut events: Vec<CalendarEvent> = candidates
        .into_iter()
        .map(|(key, mut candidate)| {
            candidate.row.cancelled = cancels
                .get(&key)
                .is_some_and(|&cancel_seq| cancel_seq >= candidate.sequence);
            candidate.row
        })
        .collect();

    // Chronological, undated last; path as the final tiebreak so the order is
    // stable across runs.
    events.sort_by(|a, b| {
        let a_key = (a.start_sort.is_empty(), a.start_sort.as_str(), a.path.as_path());
        let b_key = (b.start_sort.is_empty(), b.start_sort.as_str(), b.path.as_path());
        a_key.cmp(&b_key)
    });
    events
}

/// Read the authoritative UID / SEQUENCE / DTSTAMP for an invite email: the
/// sidecar `invite.ics` when it parses, the `event:` frontmatter cache
/// otherwise. Mirrors `reconcile::authoritative_ids` (the frontmatter block is
/// a lossy cache and can drift from the sidecar).
fn authoritative_ids(
    md_path: &Path,
    fm_event: &crate::types::EventFrontmatter,
) -> (Option<String>, u32, String) {
    let sidecar =
        crate::parse::attachments_dir_for(md_path).join(crate::parse::CALENDAR_SIDECAR_NAME);
    if let Ok(bytes) = std::fs::read(&sidecar) {
        if let Some(parsed) = crate::calendar::parse_ics(&bytes) {
            let uid = parsed.uid.or_else(|| fm_event.uid.clone());
            return (uid, parsed.sequence, parsed.dtstamp.unwrap_or_default());
        }
    }
    (fm_event.uid.clone(), fm_event.sequence, String::new())
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
    use std::path::PathBuf;

    /// Write an invite `.md` with an `event:` block into `dir`.
    #[allow(clippy::too_many_arguments)]
    fn write_invite(
        dir: &Path,
        filename: &str,
        subject: &str,
        uid: &str,
        method: &str,
        sequence: u32,
        start: Option<&str>,
    ) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let start_line = match start {
            Some(s) => format!("  start: \"{s}\"\n"),
            None => String::new(),
        };
        let body = format!(
            "---\nfrom: Org <org@example.com>\nto: me@example.com\n\
             subject: \"{subject}\"\nevent:\n  uid: {uid}\n  method: {method}\n  \
             sequence: {sequence}\n  summary: \"{subject}\"\n{start_line}---\n\nbody\n"
        );
        let path = dir.join(filename);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn loads_request_invites_from_account_root() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "a.md",
            "Standup",
            "uid-a",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        write_invite(
            &tmp.path().join("archive"),
            "b.md",
            "Retro",
            "uid-b",
            "REQUEST",
            0,
            Some("2026-08-02T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.summary.as_deref(), Some("Standup"));
        assert_eq!(events[1].event.summary.as_deref(), Some("Retro"));
    }

    #[test]
    fn ignores_non_invite_emails() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("plain.md"),
            "---\nfrom: a@b.com\nto: me@x.com\nsubject: \"Re: Plan\"\n---\n\nhi\n",
        )
        .unwrap();
        assert!(load_events_for_account(tmp.path()).is_empty());
    }

    #[test]
    fn ignores_reply_method() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "reply.md",
            "Standup",
            "uid-a",
            "REPLY",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        assert!(load_events_for_account(tmp.path()).is_empty());
    }

    #[test]
    fn dedups_same_uid_keeping_highest_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("sent"),
            "v0.md",
            "Planning",
            "uid-dup",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        write_invite(
            &tmp.path().join("inbox"),
            "v1.md",
            "Planning (moved)",
            "uid-dup",
            "REQUEST",
            1,
            Some("2026-08-01T15:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1, "one row per UID");
        assert_eq!(events[0].event.sequence, 1);
        assert_eq!(events[0].event.summary.as_deref(), Some("Planning (moved)"));
    }

    /// Our own sent invites make us the organizer (no own-RSVP, no RSVP key).
    #[test]
    fn sent_copies_are_flagged_as_organizer() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("sent"),
            "mine.md",
            "My meeting",
            "uid-mine",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        write_invite(
            &tmp.path().join("inbox"),
            "theirs.md",
            "Their meeting",
            "uid-theirs",
            "REQUEST",
            0,
            Some("2026-08-02T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        let mine = events.iter().find(|e| e.subject == "My meeting").unwrap();
        let theirs = events.iter().find(|e| e.subject == "Their meeting").unwrap();
        assert!(mine.is_organizer);
        assert!(!theirs.is_organizer);
    }

    /// A CANCEL at or above the REQUEST's sequence tags the row; the event is
    /// still listed (display only -- #0031 owns the semantics).
    #[test]
    fn cancel_message_tags_the_uid_as_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "req.md",
            "Doomed",
            "uid-c",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        write_invite(
            &tmp.path().join("inbox"),
            "cancel.md",
            "Cancelled: Doomed",
            "uid-c",
            "CANCEL",
            1,
            Some("2026-08-01T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1, "the CANCEL is not its own agenda row");
        assert!(events[0].cancelled);
    }

    /// The boundary the doc comment promises: a CANCEL at *exactly* the
    /// REQUEST's sequence still tags it (`>=`, not `>`).
    #[test]
    fn cancel_at_the_same_sequence_tags_the_request() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "req.md",
            "Weekly",
            "uid-eq",
            "REQUEST",
            2,
            Some("2026-08-01T09:00:00+00:00"),
        );
        write_invite(
            &tmp.path().join("inbox"),
            "cancel.md",
            "Cancelled: Weekly",
            "uid-eq",
            "CANCEL",
            2,
            Some("2026-08-01T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1);
        assert!(
            events[0].cancelled,
            "cancel_seq == request_seq must still tag the row"
        );
    }

    /// A stale CANCEL (older sequence than the surviving REQUEST) does not tag
    /// the rescheduled event.
    #[test]
    fn stale_cancel_does_not_tag_a_newer_request() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "cancel.md",
            "Cancelled: Weekly",
            "uid-s",
            "CANCEL",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        write_invite(
            &tmp.path().join("inbox"),
            "req.md",
            "Weekly",
            "uid-s",
            "REQUEST",
            2,
            Some("2026-08-08T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1);
        assert!(!events[0].cancelled);
    }

    /// Two events on the same calendar day in different zones must order by
    /// actual instant, not by wallclock (mirrors `resolve_date`, #0024).
    #[test]
    fn sorts_by_start_instant_not_wallclock() {
        let tmp = tempfile::tempdir().unwrap();
        // 10:00+02:00 == 08:00 UTC (earlier instant)
        write_invite(
            &tmp.path().join("inbox"),
            "early.md",
            "Early",
            "uid-e",
            "REQUEST",
            0,
            Some("2026-08-01T10:00:00+02:00"),
        );
        // 09:30+00:00 == 09:30 UTC (later instant)
        write_invite(
            &tmp.path().join("inbox"),
            "late.md",
            "Late",
            "uid-l",
            "REQUEST",
            0,
            Some("2026-08-01T09:30:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        let names: Vec<&str> = events.iter().map(|e| e.subject.as_str()).collect();
        assert_eq!(names, vec!["Early", "Late"]);
        // The display keeps the event's own offset.
        assert_eq!(events[0].start_display, "2026-08-01 10:00");
    }

    #[test]
    fn handles_missing_start_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "dated.md",
            "Dated",
            "uid-d",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        write_invite(
            &tmp.path().join("inbox"),
            "undated.md",
            "Undated",
            "uid-u",
            "REQUEST",
            0,
            None,
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].subject, "Undated", "undated events sort last");
        assert!(events[1].start_sort.is_empty());
        assert!(events[1].start_display.is_empty());
    }

    #[test]
    fn handles_malformed_frontmatter_and_missing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("broken.md"), "---\nnot: [valid\n---\nbody").unwrap();
        std::fs::write(inbox.join("nofm.md"), "just a body, no frontmatter").unwrap();
        assert!(load_events_for_account(tmp.path()).is_empty());
        // A missing account root yields no events rather than panicking.
        assert!(load_events_for_account(&tmp.path().join("nope")).is_empty());
    }

    /// All-day (`VALUE=DATE`) invites serialise as midnight wallclock; they get
    /// a date-only display, a UTC-normalised sort key, and an implicit end at
    /// the start of the next local day.
    #[test]
    fn all_day_events_display_as_a_bare_date() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "allday.md",
            "Offsite",
            "uid-a",
            "REQUEST",
            0,
            Some("2026-08-01T00:00:00"),
        );
        let events = load_events_for_account(tmp.path());
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

    /// A hand-edited all-day date at the far end of chrono's range must not
    /// panic in `plus_one_day` (the implicit-end `+1 day` used to overflow);
    /// it degrades to an unchanged end key and the event still loads.
    #[test]
    fn far_future_all_day_event_does_not_overflow() {
        // TZ-independent core: one day past this key does not exist.
        let max_key = chrono::NaiveDateTime::MAX
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert_eq!(plus_one_day(&max_key), max_key);

        // End-to-end: the reviewer's reproducer (panicked under TZ=UTC).
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("inbox"),
            "doom.md",
            "Heat death planning",
            "uid-far",
            "REQUEST",
            0,
            Some("+262142-12-31T00:00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1);
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
    /// day. It serialises as offset-less local midnight, so before the fix its
    /// key sorted before a UTC `now` and the row vanished from 00:00 onward.
    #[test]
    fn todays_all_day_event_stays_upcoming() {
        let tmp = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().date_naive();
        write_invite(
            &tmp.path().join("inbox"),
            "today.md",
            "Offsite",
            "uid-today",
            "REQUEST",
            0,
            Some(&today.format("%Y-%m-%dT00:00:00").to_string()),
        );
        // Yesterday's all-day event is over and must not linger.
        write_invite(
            &tmp.path().join("inbox"),
            "yesterday.md",
            "Past offsite",
            "uid-yesterday",
            "REQUEST",
            0,
            Some(
                &(today - chrono::Duration::days(1))
                    .format("%Y-%m-%dT00:00:00")
                    .to_string(),
            ),
        );
        let mut app = crate::tui::app::App::default_for_tests();
        app.calendar_view.events = load_events_for_account(tmp.path());
        app.calendar_view.loaded = true;
        app.recompute_calendar_visible();
        let visible: Vec<&str> = app
            .calendar_view
            .visible
            .iter()
            .map(|&i| app.calendar_view.events[i].subject.as_str())
            .collect();
        assert_eq!(visible, vec!["Offsite"]);
    }

    /// A floating (offset-less) event that already finished must leave the
    /// upcoming scope on time. Before the fix its wallclock key was compared
    /// against a UTC `now`, so in a positive-offset zone it lingered for the
    /// length of the offset. Vacuous on a UTC machine, sharp everywhere else.
    #[test]
    fn a_finished_floating_event_leaves_the_upcoming_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::Local::now().naive_local();
        write_invite(
            &tmp.path().join("inbox"),
            "past.md",
            "Just finished",
            "uid-past",
            "REQUEST",
            0,
            Some(
                &(now - chrono::Duration::minutes(45))
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            ),
        );
        write_invite(
            &tmp.path().join("inbox"),
            "soon.md",
            "Starting soon",
            "uid-soon",
            "REQUEST",
            0,
            Some(
                &(now + chrono::Duration::minutes(45))
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            ),
        );
        let mut app = crate::tui::app::App::default_for_tests();
        app.calendar_view.events = load_events_for_account(tmp.path());
        app.calendar_view.loaded = true;
        app.recompute_calendar_visible();
        let visible: Vec<&str> = app
            .calendar_view
            .visible
            .iter()
            .map(|&i| app.calendar_view.events[i].subject.as_str())
            .collect();
        assert_eq!(visible, vec!["Starting soon"]);
    }

    /// The sidecar `.ics` is authoritative for identity: an invite whose
    /// frontmatter UID drifted from the sidecar dedups on the sidecar's UID.
    #[test]
    fn prefers_sidecar_uid_over_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let a = write_invite(
            &inbox,
            "a.md",
            "Sync",
            "stale-uid",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        let b = write_invite(
            &tmp.path().join("archive"),
            "b.md",
            "Sync",
            "real-uid",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        // Give the drifted copy a sidecar carrying the real UID.
        let att = crate::parse::attachments_dir_for(&a);
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(
            att.join(crate::parse::CALENDAR_SIDECAR_NAME),
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
             UID:real-uid\r\nSEQUENCE:3\r\nSUMMARY:Sync\r\n\
             DTSTART:20260801T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        )
        .unwrap();
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1, "sidecar UID collapses the two copies");
        // Sequence 3 from the sidecar beats the frontmatter copy's 0.
        assert_eq!(events[0].path, a);
        assert_ne!(events[0].path, b);
    }

    /// `.md` files that arrived as *email attachments* are sender-controlled
    /// content, not our mail: they must never become agenda rows, displace a
    /// real invite (same UID, huge sequence) or strike one through.
    #[test]
    fn attachment_markdown_cannot_hijack_a_real_invite() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let real = write_invite(
            &inbox,
            "invite.md",
            "Board meeting",
            "real-uid",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        // Layout 1: the per-email `<stem>_attachments/` sidecar dir.
        write_invite(
            &inbox.join("spam_attachments"),
            "notes.md",
            "Board meeting (MOVED to attacker room)",
            "real-uid",
            "REQUEST",
            u32::MAX,
            Some("2026-08-01T20:00:00+00:00"),
        );
        // Layout 2: the account-wide `attachments/<message-id>/` mirror.
        write_invite(
            &tmp.path().join("attachments").join("mid-1"),
            "cancel.md",
            "Cancelled: Board meeting",
            "real-uid",
            "CANCEL",
            u32::MAX,
            Some("2026-08-01T09:00:00+00:00"),
        );
        // And a fabricated event with a fresh UID: no phantom row either.
        write_invite(
            &inbox.join("spam_attachments"),
            "phantom.md",
            "Free money",
            "phantom-uid",
            "REQUEST",
            0,
            Some("2026-08-02T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1, "only the real invite is an agenda row");
        assert_eq!(events[0].path, real);
        assert_eq!(events[0].event.summary.as_deref(), Some("Board meeting"));
        assert!(!events[0].cancelled, "an attached CANCEL must not strike it");
    }

    /// On an equal-`(sequence, dtstamp)` tie the sent copy wins, so
    /// `is_organizer` cannot flip on a mailbox name that sorts after `sent`.
    /// A self-invited event shares one DTSTAMP across every copy, so this tie
    /// is the normal case, not an exotic one.
    #[test]
    fn sent_copy_wins_an_equal_rank_tie() {
        let tmp = tempfile::tempdir().unwrap();
        write_invite(
            &tmp.path().join("sent"),
            "x.md",
            "All hands",
            "uid-tie",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        // `team` sorts after `sent`, so plain path order would pick this copy.
        write_invite(
            &tmp.path().join("team"),
            "x.md",
            "All hands",
            "uid-tie",
            "REQUEST",
            0,
            Some("2026-08-01T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1);
        assert!(
            events[0].is_organizer,
            "our sent copy must win the tie, got {}",
            events[0].path.display()
        );
    }

    /// A sidecar that does not parse falls back to the frontmatter identity
    /// rather than dropping the event or losing its UID.
    #[test]
    fn malformed_sidecar_falls_back_to_frontmatter_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let a = write_invite(
            &inbox,
            "a.md",
            "Sync",
            "fm-uid",
            "REQUEST",
            2,
            Some("2026-08-01T09:00:00+00:00"),
        );
        let att = crate::parse::attachments_dir_for(&a);
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(
            att.join(crate::parse::CALENDAR_SIDECAR_NAME),
            b"not an ics at all",
        )
        .unwrap();
        // A second copy of the same frontmatter UID at a lower sequence: it
        // dedups away only if the fallback UID survived.
        write_invite(
            &tmp.path().join("archive"),
            "b.md",
            "Sync",
            "fm-uid",
            "REQUEST",
            1,
            Some("2026-08-01T09:00:00+00:00"),
        );
        let events = load_events_for_account(tmp.path());
        assert_eq!(events.len(), 1, "frontmatter UID still dedups the copies");
        assert_eq!(events[0].path, a);
        assert_eq!(events[0].event.sequence, 2);
    }

    /// Invites with no UID at all are kept (keyed by path) rather than dropped.
    #[test]
    fn keeps_uidless_invites_keyed_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        for name in ["one.md", "two.md"] {
            std::fs::write(
                inbox.join(name),
                "---\nfrom: a@b.com\nto: me@x.com\nsubject: \"No UID\"\n\
                 event:\n  method: REQUEST\n  summary: \"No UID\"\n  \
                 start: \"2026-08-01T09:00:00+00:00\"\n---\n\nbody\n",
            )
            .unwrap();
        }
        assert_eq!(load_events_for_account(tmp.path()).len(), 2);
    }
}
