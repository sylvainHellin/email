//! iCalendar (iMIP) receive-side parsing.
//!
//! Turns a raw `text/calendar` payload (the sidecar `.ics` bytes) into an
//! [`ParsedEvent`] that the email writer serializes into the `event:`
//! frontmatter block (see `docs/plans/calendar-invites.md`, D2/D5/D6).
//!
//! Parsing is strictly best-effort: any failure returns `None` and the caller
//! still persists the sidecar `.ics` and the email itself. A malformed invite
//! must never fail an email save.

use std::str::FromStr;

use chrono::TimeZone;
use icalendar::{Calendar, CalendarComponent, Component, DatePerhapsTime, EventLike, PartStat};

use crate::types::{EventAttendee, EventFrontmatter};

/// A parsed calendar attendee with its participation status.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedAttendee {
    pub address: String,
    pub status: String,
}

/// The subset of a `VEVENT` we surface into frontmatter.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub uid: Option<String>,
    /// `RECURRENCE-ID` when the payload addresses a single occurrence of a
    /// recurring series rather than the whole series (#0031). Normalised the
    /// same way `DTSTART` is, so a CANCEL naming the instance in UTC matches a
    /// REQUEST naming it with a `TZID`. `None` means "the whole series", and
    /// `(uid, recurrence_id)` is the identity a CANCEL/update applies to.
    pub recurrence_id: Option<String>,
    pub method: Option<String>,
    pub sequence: u32,
    /// `DTSTAMP` as an RFC3339 UTC string, when present. Used by REPLY
    /// reconciliation (#0030) as the latest-wins tiebreaker between multiple
    /// replies from the same attendee within one sequence.
    pub dtstamp: Option<String>,
    pub summary: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub location: Option<String>,
    pub organizer: Option<String>,
    pub recurrence: Option<String>,
    pub attendees: Vec<ParsedAttendee>,
}

/// Detect whether a MIME content-type is an iCalendar payload.
pub fn is_calendar_mimetype(mimetype: &str) -> bool {
    mimetype.eq_ignore_ascii_case("text/calendar")
        || mimetype.eq_ignore_ascii_case("application/ics")
}

/// Detect whether a filename looks like an `.ics` calendar attachment
/// (used for the Graph path, where parts arrive as named attachments).
pub fn is_ics_filename(name: &str) -> bool {
    name.rsplit('.')
        .next()
        .map(|ext| ext.eq_ignore_ascii_case("ics"))
        .unwrap_or(false)
}

/// Whether raw `.ics` bytes are an actual iMIP invite, i.e. they parse as a
/// `VCALENDAR` carrying a `METHOD` property (REQUEST/REPLY/CANCEL/...).
///
/// This is the decisive test for lifting a calendar part to the `invite.ics`
/// sidecar plus an `event:` block. A plain `.ics` export (no `METHOD`) is not
/// an invite and stays a regular attachment. Never panics.
pub fn is_imip_invite(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Ok(calendar) = Calendar::from_str(text) else {
        return false;
    };
    calendar
        .property_value("METHOD")
        .map(|m| !m.trim().is_empty())
        .unwrap_or(false)
}

/// Parse raw `.ics` bytes into a [`ParsedEvent`]. Returns `None` when the bytes
/// are not valid UTF-8, do not parse as a `VCALENDAR`, or contain no `VEVENT`.
///
/// Never panics; every extraction step degrades to a missing field.
pub fn parse_ics(bytes: &[u8]) -> Option<ParsedEvent> {
    let text = std::str::from_utf8(bytes).ok()?;
    let calendar = Calendar::from_str(text).ok()?;

    let method = calendar
        .property_value("METHOD")
        .map(|m| m.trim().to_uppercase());

    // First VEVENT in the VCALENDAR (v1: whole-series, no per-occurrence).
    let event = calendar.components.iter().find_map(|c| match c {
        CalendarComponent::Event(e) => Some(e),
        _ => None,
    })?;

    let uid = event.get_uid().map(|s| s.to_string());
    let recurrence_id = event
        .properties()
        .get("RECURRENCE-ID")
        .and_then(DatePerhapsTime::from_property)
        .and_then(format_date_perhaps_time);
    let sequence = event.get_sequence().unwrap_or(0);
    let dtstamp = event
        .get_timestamp()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let summary = event.get_summary().map(|s| s.to_string());
    let location = event.get_location().map(|s| s.to_string());
    let organizer = event
        .property_value("ORGANIZER")
        .map(strip_mailto)
        .map(|s| s.to_string());

    let start = event.get_start().and_then(format_date_perhaps_time);
    let end = event.get_end().and_then(format_date_perhaps_time);

    let recurrence = event
        .property_value("RRULE")
        .map(humanize_rrule)
        .filter(|s| !s.is_empty());

    let attendees = event
        .get_attendees()
        .into_iter()
        .map(|att| ParsedAttendee {
            address: strip_mailto(&att.cal_address).to_string(),
            status: partstat_to_str(att.part_stat).to_string(),
        })
        .filter(|a| !a.address.is_empty())
        .collect();

    Some(ParsedEvent {
        uid,
        recurrence_id,
        method,
        sequence,
        dtstamp,
        summary,
        start,
        end,
        location,
        organizer,
        recurrence,
        attendees,
    })
}

/// Build the frontmatter `event:` block from a [`ParsedEvent`].
/// `rsvp` starts unset (`needs-action`) — the user has not yet responded.
pub fn event_frontmatter(parsed: &ParsedEvent) -> EventFrontmatter {
    EventFrontmatter {
        uid: parsed.uid.clone(),
        recurrence_id: parsed.recurrence_id.clone(),
        method: parsed.method.clone(),
        sequence: parsed.sequence,
        summary: parsed.summary.clone(),
        start: parsed.start.clone(),
        end: parsed.end.clone(),
        location: parsed.location.clone(),
        organizer: parsed.organizer.clone(),
        rsvp: "needs-action".to_string(),
        recurrence: parsed.recurrence.clone().unwrap_or_default(),
        // Derived state, folded in by `crate::reconcile::fold_status` where
        // the whole account's payloads are visible; a single ics cannot know
        // whether it was cancelled or superseded later.
        cancelled: false,
        superseded: false,
        cancelled_instances: Vec::new(),
        attendees: parsed
            .attendees
            .iter()
            .map(|a| EventAttendee {
                address: a.address.clone(),
                status: a.status.clone(),
            })
            .collect(),
    }
}

/// Strip a leading `mailto:` (case-insensitive) from a CAL-ADDRESS.
fn strip_mailto(addr: &str) -> &str {
    let trimmed = addr.trim();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("mailto:") {
        trimmed[7..].trim()
    } else {
        trimmed
    }
}

/// Map an icalendar `PartStat` to our lowercase status vocabulary.
/// Unknown / missing statuses degrade to `needs-action`.
fn partstat_to_str(part_stat: Option<PartStat>) -> &'static str {
    match part_stat {
        Some(PartStat::Accepted) => "accepted",
        Some(PartStat::Declined) => "declined",
        Some(PartStat::Tentative) => "tentative",
        _ => "needs-action",
    }
}

/// Render a `DTSTART`/`DTEND` value as an RFC3339 string.
///
/// - UTC times → `...Z`-equivalent offset (`+00:00`).
/// - `TZID` times → resolved against the IANA zone, preserving the wall-clock
///   offset (e.g. `+02:00`). If the `TZID` is unknown, fall back to the naive
///   local time with no offset (still a useful, if offset-less, timestamp).
/// - Floating times → naive local time, no offset.
/// - Date-only values → midnight naive local time.
fn format_date_perhaps_time(dpt: DatePerhapsTime) -> Option<String> {
    use icalendar::CalendarDateTime;
    match dpt {
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(dt)) => {
            Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        }
        DatePerhapsTime::DateTime(CalendarDateTime::Floating(naive)) => {
            Some(naive.format("%Y-%m-%dT%H:%M:%S").to_string())
        }
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone { date_time, tzid }) => {
            if let Ok(tz) = tzid.parse::<chrono_tz::Tz>() {
                if let Some(zoned) = tz.from_local_datetime(&date_time).single() {
                    return Some(zoned.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                }
            }
            // Unknown zone: keep the wall-clock time without an offset.
            Some(date_time.format("%Y-%m-%dT%H:%M:%S").to_string())
        }
        DatePerhapsTime::Date(date) => {
            Some(date.format("%Y-%m-%dT00:00:00").to_string())
        }
    }
}

/// Render an `RRULE` value into a short, human-readable summary (display-only,
/// D6). This is deliberately shallow — it covers the common FREQ/INTERVAL/BYDAY
/// cases and falls back to the raw rule for anything unrecognised.
fn humanize_rrule(rrule: &str) -> String {
    let mut freq = None;
    let mut interval: u32 = 1;
    let mut byday: Option<String> = None;
    let mut count: Option<String> = None;
    let mut until: Option<String> = None;

    for part in rrule.split(';') {
        let mut kv = part.splitn(2, '=');
        let key = kv.next().unwrap_or("").trim().to_uppercase();
        let val = kv.next().unwrap_or("").trim().to_string();
        match key.as_str() {
            "FREQ" => freq = Some(val.to_uppercase()),
            "INTERVAL" => interval = val.parse().unwrap_or(1),
            "BYDAY" => byday = Some(val.to_uppercase()),
            "COUNT" => count = Some(val),
            "UNTIL" => until = Some(val),
            _ => {}
        }
    }

    let Some(freq) = freq else {
        return rrule.to_string();
    };

    let base = match freq.as_str() {
        "DAILY" => {
            if interval == 1 {
                "Daily".to_string()
            } else {
                format!("Every {} days", interval)
            }
        }
        "WEEKLY" => {
            let unit = if interval == 1 {
                "Weekly".to_string()
            } else {
                format!("Every {} weeks", interval)
            };
            match byday.as_deref() {
                Some(days) if !days.is_empty() => format!("{} on {}", unit, humanize_days(days)),
                _ => unit,
            }
        }
        "MONTHLY" => {
            if interval == 1 {
                "Monthly".to_string()
            } else {
                format!("Every {} months", interval)
            }
        }
        "YEARLY" => {
            if interval == 1 {
                "Yearly".to_string()
            } else {
                format!("Every {} years", interval)
            }
        }
        _ => return rrule.to_string(),
    };

    if let Some(count) = count {
        format!("{} ({} times)", base, count)
    } else if let Some(until) = until {
        format!("{} until {}", base, until)
    } else {
        base
    }
}

/// Expand a `BYDAY` list (e.g. `MO,WE`) into readable day names.
fn humanize_days(days: &str) -> String {
    days.split(',')
        .map(|d| {
            // Strip any ordinal prefix (e.g. `2MO` → Monday).
            let code: String = d.trim().chars().filter(|c| c.is_alphabetic()).collect();
            match code.as_str() {
                "MO" => "Monday",
                "TU" => "Tuesday",
                "WE" => "Wednesday",
                "TH" => "Thursday",
                "FR" => "Friday",
                "SA" => "Saturday",
                "SU" => "Sunday",
                other => other,
            }
            .to_string()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTLOOK_REQUEST: &str = "\
BEGIN:VCALENDAR\r
PRODID:-//Microsoft Corporation//Outlook 16.0 MIMEDIR//EN\r
VERSION:2.0\r
METHOD:REQUEST\r
BEGIN:VTIMEZONE\r
TZID:W. Europe Standard Time\r
END:VTIMEZONE\r
BEGIN:VEVENT\r
UID:040000008200E00074C5B7101A82E00800000000@outlook.com\r
SEQUENCE:2\r
SUMMARY:LOC Day planning\r
DTSTART;TZID=W. Europe Standard Time:20260720T140000\r
DTEND;TZID=W. Europe Standard Time:20260720T150000\r
LOCATION:Room 4.12\r
ORGANIZER;CN=Chair:mailto:chair@tum.de\r
ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:a@example.com\r
ATTENDEE;PARTSTAT=ACCEPTED:mailto:b@example.com\r
END:VEVENT\r
END:VCALENDAR\r
";

    // Google-style: UTC times (Z suffix), RRULE, no TZID.
    const GOOGLE_REQUEST: &str = "\
BEGIN:VCALENDAR\r
PRODID:-//Google Inc//Google Calendar 70.9054//EN\r
VERSION:2.0\r
METHOD:REQUEST\r
BEGIN:VEVENT\r
UID:abc123@google.com\r
SEQUENCE:0\r
SUMMARY:Weekly sync\r
DTSTART:20260720T120000Z\r
DTEND:20260720T130000Z\r
RRULE:FREQ=WEEKLY;BYDAY=MO\r
ORGANIZER:mailto:host@gmail.com\r
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:guest@example.com\r
END:VEVENT\r
END:VCALENDAR\r
";

    const REPLY: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
METHOD:REPLY\r
BEGIN:VEVENT\r
UID:abc123@google.com\r
SEQUENCE:0\r
SUMMARY:Weekly sync\r
DTSTART:20260720T120000Z\r
ORGANIZER:mailto:host@gmail.com\r
ATTENDEE;PARTSTAT=ACCEPTED:mailto:guest@example.com\r
END:VEVENT\r
END:VCALENDAR\r
";

    const CANCEL: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
METHOD:CANCEL\r
BEGIN:VEVENT\r
UID:abc123@google.com\r
SEQUENCE:1\r
STATUS:CANCELLED\r
SUMMARY:Weekly sync\r
END:VEVENT\r
END:VCALENDAR\r
";

    #[test]
    fn parse_outlook_request_tzid() {
        let ev = parse_ics(OUTLOOK_REQUEST.as_bytes()).unwrap();
        assert_eq!(ev.method.as_deref(), Some("REQUEST"));
        assert_eq!(
            ev.uid.as_deref(),
            Some("040000008200E00074C5B7101A82E00800000000@outlook.com")
        );
        assert_eq!(ev.sequence, 2);
        assert_eq!(ev.summary.as_deref(), Some("LOC Day planning"));
        assert_eq!(ev.location.as_deref(), Some("Room 4.12"));
        assert_eq!(ev.organizer.as_deref(), Some("chair@tum.de"));
        // W. Europe Standard Time is not a valid IANA zone, so we keep the
        // naive wall-clock time (no offset) rather than dropping the field.
        assert_eq!(ev.start.as_deref(), Some("2026-07-20T14:00:00"));
        assert_eq!(ev.end.as_deref(), Some("2026-07-20T15:00:00"));
        assert_eq!(ev.attendees.len(), 2);
        assert_eq!(ev.attendees[0].address, "a@example.com");
        assert_eq!(ev.attendees[0].status, "needs-action");
        assert_eq!(ev.attendees[1].address, "b@example.com");
        assert_eq!(ev.attendees[1].status, "accepted");
    }

    #[test]
    fn parse_outlook_iana_tzid_has_offset() {
        // Same as Outlook but with a real IANA zone id, to prove offset output.
        let ics = OUTLOOK_REQUEST
            .replace("TZID:W. Europe Standard Time", "TZID:Europe/Berlin")
            .replace(
                "DTSTART;TZID=W. Europe Standard Time",
                "DTSTART;TZID=Europe/Berlin",
            )
            .replace(
                "DTEND;TZID=W. Europe Standard Time",
                "DTEND;TZID=Europe/Berlin",
            );
        let ev = parse_ics(ics.as_bytes()).unwrap();
        assert_eq!(ev.start.as_deref(), Some("2026-07-20T14:00:00+02:00"));
        assert_eq!(ev.end.as_deref(), Some("2026-07-20T15:00:00+02:00"));
    }

    #[test]
    fn parse_google_request_utc_and_rrule() {
        let ev = parse_ics(GOOGLE_REQUEST.as_bytes()).unwrap();
        assert_eq!(ev.method.as_deref(), Some("REQUEST"));
        assert_eq!(ev.uid.as_deref(), Some("abc123@google.com"));
        // UTC renders with the canonical `Z` designator.
        assert_eq!(ev.start.as_deref(), Some("2026-07-20T12:00:00Z"));
        assert_eq!(ev.end.as_deref(), Some("2026-07-20T13:00:00Z"));
        assert_eq!(ev.recurrence.as_deref(), Some("Weekly on Monday"));
        assert_eq!(ev.organizer.as_deref(), Some("host@gmail.com"));
    }

    #[test]
    fn parse_reply_preserved() {
        let ev = parse_ics(REPLY.as_bytes()).unwrap();
        assert_eq!(ev.method.as_deref(), Some("REPLY"));
        assert_eq!(ev.uid.as_deref(), Some("abc123@google.com"));
        assert_eq!(ev.attendees[0].status, "accepted");
    }

    #[test]
    fn parse_dtstamp_for_reply_tiebreak() {
        // DTSTAMP is surfaced as an RFC3339 UTC string for #0030's
        // latest-wins reconciliation; absence degrades to None.
        let with = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:u@x\r\nSEQUENCE:0\r\nDTSTAMP:20260710T120000Z\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:a@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let ev = parse_ics(with.as_bytes()).unwrap();
        assert_eq!(ev.dtstamp.as_deref(), Some("2026-07-10T12:00:00Z"));

        let without = REPLY; // the REPLY fixture carries no DTSTAMP
        let ev2 = parse_ics(without.as_bytes()).unwrap();
        assert_eq!(ev2.dtstamp, None);
    }

    #[test]
    fn parse_cancel_preserved() {
        let ev = parse_ics(CANCEL.as_bytes()).unwrap();
        assert_eq!(ev.method.as_deref(), Some("CANCEL"));
        assert_eq!(
            ev.recurrence_id, None,
            "a whole-series CANCEL carries no RECURRENCE-ID"
        );
        assert_eq!(ev.uid.as_deref(), Some("abc123@google.com"));
        assert_eq!(ev.sequence, 1);
    }

    #[test]
    fn malformed_ics_returns_none() {
        assert!(parse_ics(b"this is not a calendar").is_none());
        assert!(parse_ics(b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n").is_none()); // no VEVENT
        assert!(parse_ics(&[0xff, 0xfe, 0x00]).is_none()); // invalid UTF-8
    }

    #[test]
    fn event_frontmatter_defaults_rsvp() {
        let ev = parse_ics(GOOGLE_REQUEST.as_bytes()).unwrap();
        let fm = event_frontmatter(&ev);
        assert_eq!(fm.rsvp, "needs-action");
        assert_eq!(fm.method.as_deref(), Some("REQUEST"));
        assert_eq!(fm.attendees.len(), 1);
    }

    #[test]
    fn humanize_rrule_variants() {
        assert_eq!(humanize_rrule("FREQ=WEEKLY;BYDAY=MO"), "Weekly on Monday");
        assert_eq!(
            humanize_rrule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE"),
            "Every 2 weeks on Monday, Wednesday"
        );
        assert_eq!(humanize_rrule("FREQ=DAILY"), "Daily");
        assert_eq!(humanize_rrule("FREQ=DAILY;INTERVAL=3"), "Every 3 days");
        assert_eq!(humanize_rrule("FREQ=MONTHLY"), "Monthly");
        assert_eq!(humanize_rrule("FREQ=YEARLY"), "Yearly");
        assert_eq!(humanize_rrule("FREQ=WEEKLY;COUNT=5"), "Weekly (5 times)");
        // Unrecognised freq falls back to the raw rule.
        assert_eq!(humanize_rrule("FREQ=SECONDLY"), "FREQ=SECONDLY");
    }

    #[test]
    fn is_calendar_mimetype_detects() {
        assert!(is_calendar_mimetype("text/calendar"));
        assert!(is_calendar_mimetype("TEXT/CALENDAR"));
        assert!(is_calendar_mimetype("application/ics"));
        assert!(!is_calendar_mimetype("text/plain"));
    }

    #[test]
    fn is_ics_filename_detects() {
        assert!(is_ics_filename("invite.ics"));
        assert!(is_ics_filename("meeting.ICS"));
        assert!(!is_ics_filename("report.pdf"));
    }

    #[test]
    fn is_imip_invite_requires_method_property() {
        let invite = "BEGIN:VCALENDAR\nVERSION:2.0\nMETHOD:REQUEST\nBEGIN:VEVENT\nUID:1\nEND:VEVENT\nEND:VCALENDAR\n";
        let export = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:1\nEND:VEVENT\nEND:VCALENDAR\n";
        assert!(is_imip_invite(invite.as_bytes()), "VCALENDAR with METHOD is an invite");
        assert!(!is_imip_invite(export.as_bytes()), "no METHOD -> not an invite");
        assert!(!is_imip_invite(b"this is not a vcalendar at all"));
        assert!(!is_imip_invite(&[0xff, 0xfe]), "invalid UTF-8 -> not an invite");
    }

    /// #0031: a single-occurrence payload carries a `RECURRENCE-ID`, and it is
    /// normalised exactly like `DTSTART`, so the same instance named in UTC
    /// and with a `TZID` is one identity.
    #[test]
    fn parses_recurrence_id_and_normalises_it_like_dtstart() {
        let utc = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\n\
                   UID:series@x\r\nSEQUENCE:1\r\nRECURRENCE-ID:20260720T120000Z\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        let zoned = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
                     UID:series@x\r\nSEQUENCE:1\r\n\
                     RECURRENCE-ID;TZID=Europe/Berlin:20260720T140000\r\n\
                     END:VEVENT\r\nEND:VCALENDAR\r\n";
        let utc = parse_ics(utc.as_bytes()).unwrap();
        let zoned = parse_ics(zoned.as_bytes()).unwrap();
        assert_eq!(utc.recurrence_id.as_deref(), Some("2026-07-20T12:00:00Z"));
        assert_eq!(
            zoned.recurrence_id.as_deref(),
            Some("2026-07-20T14:00:00+02:00"),
            "the zoned form keeps its own offset, as DTSTART does"
        );
    }

}
