use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Collapse consecutive hyphens and trim leading/trailing hyphens.
/// Used by slugify functions across multiple modules.
pub(crate) fn collapse_hyphens(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut prev_hyphen = false;
    for c in input.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            prev_hyphen = false;
            result.push(c);
        }
    }
    result.trim_matches('-').to_string()
}

/// The three states a draft moves through: written, approved, submitted.
///
/// It is a *draft* state and nothing else. The `Inbox` and `Archived` variants
/// it used to carry described where a `.md` file sat in the mailbox tree the
/// store cutover deleted, and no draft was ever written with one; the status a
/// received message shows in the headers pane is derived from the mailbox it
/// was listed from instead (`tui::app::status_for_mailbox`), which is now the
/// only place that derivation happens (#0064).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmailStatus {
    Draft,
    Approved,
    Sent,
}

impl std::fmt::Display for EmailStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmailStatus::Draft => write!(f, "draft"),
            EmailStatus::Approved => write!(f, "approved"),
            EmailStatus::Sent => write!(f, "sent"),
        }
    }
}

/// What has happened to a received message: the second status axis (#TKT-0051).
///
/// Orthogonal to [`EmailStatus`], which is the *draft* lifecycle and says
/// nothing about received mail. This one is the history of a message that
/// arrived: it was read, it was answered, it was forwarded, and any
/// combination of the three can be true at once. That is why it is a set of
/// booleans rather than an enum: collapsing it into one value is a display
/// decision (`tui::ui::list`), not a storage one.
///
/// The storage is `messages.flags`, the IMAP flag string the column already
/// held, so the axis costs no schema change: the tokens are `\Seen`,
/// `\Answered` and the `$Forwarded` keyword (RFC 5788), which is what every
/// other client writes. The server is truth for all three on the IMAP path
/// (sync pass 1 fetches `FLAGS` over the whole window), so a store written by
/// a build that only knew `\Seen` heals itself on the next sync instead of
/// needing a migration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageFlags {
    pub seen: bool,
    pub answered: bool,
    pub forwarded: bool,
    /// The `\Flagged` system flag: the user marked this message important
    /// (#0007). Orthogonal to the read/answered/forwarded history, since a
    /// message can be flagged and unread at once, which is why it is a fourth
    /// bit rather than a fourth value of a collapsed state.
    pub flagged: bool,
}

/// The `\Seen` token, as stored and as IMAP spells it.
pub const FLAG_SEEN: &str = "\\Seen";
/// The `\Answered` token.
pub const FLAG_ANSWERED: &str = "\\Answered";
/// The `\Flagged` system flag, IMAP's star (#0007).
pub const FLAG_FLAGGED: &str = "\\Flagged";
/// The forwarded keyword. `$Forwarded` is the registered spelling (RFC 5788)
/// and what Thunderbird, Apple Mail and Dovecot use; `\Forwarded` is read back
/// as the same thing because a few servers hand it over that way, but it is
/// never written.
pub const FLAG_FORWARDED: &str = "$Forwarded";

impl MessageFlags {
    /// Only the read bit, which is all the Graph path can answer.
    pub fn seen(seen: bool) -> Self {
        MessageFlags {
            seen,
            ..Default::default()
        }
    }

    /// Read a stored (or server-sent) flag string. Unknown tokens are ignored:
    /// the column is not the client's to curate.
    pub fn parse(flags: &str) -> Self {
        let mut out = MessageFlags::default();
        for token in flags.split_whitespace() {
            if token.eq_ignore_ascii_case(FLAG_SEEN) {
                out.seen = true;
            } else if token.eq_ignore_ascii_case(FLAG_ANSWERED) {
                out.answered = true;
            } else if token.eq_ignore_ascii_case(FLAG_FLAGGED) {
                out.flagged = true;
            } else if token.eq_ignore_ascii_case(FLAG_FORWARDED)
                || token.eq_ignore_ascii_case("\\Forwarded")
            {
                out.forwarded = true;
            }
        }
        out
    }

    /// The canonical stored form: the tokens that are set, in one fixed order,
    /// so an unchanged flag set compares equal as a string and the `IFNULL(flags,
    /// '') <> ?` guard in the store keeps meaning "actually changed".
    pub fn to_flag_string(self) -> String {
        let mut out: Vec<&str> = Vec::with_capacity(4);
        if self.seen {
            out.push(FLAG_SEEN);
        }
        if self.answered {
            out.push(FLAG_ANSWERED);
        }
        if self.flagged {
            out.push(FLAG_FLAGGED);
        }
        if self.forwarded {
            out.push(FLAG_FORWARDED);
        }
        out.join(" ")
    }

    /// This set with the read bit replaced, the others untouched. What the
    /// Graph path and the read/unread toggle apply, so neither can clobber a
    /// history bit it knows nothing about.
    pub fn with_seen(self, seen: bool) -> Self {
        MessageFlags { seen, ..self }
    }

    /// This set with the `\Flagged` bit replaced, the others untouched. What
    /// the flag/star toggle applies (#0007).
    pub fn with_flagged(self, flagged: bool) -> Self {
        MessageFlags { flagged, ..self }
    }
}

/// The role a mailbox plays for an account, and the key its messages carry in
/// the `messages.mailbox` column.
///
/// Three roles are mapped in config and named by the product (`inbox`,
/// `archive`, `sent`); every other configured mailbox is
/// [`MailboxRole::Other`] and keeps its server name. The string form is
/// canonical: it is what ingest writes, what the `mp://<account>/<mailbox>/<key>`
/// selector carries, and what the sidebar counts group by.
///
/// Parsing is case-insensitive on the three named roles, which is what the
/// half-dozen `eq_ignore_ascii_case("inbox")` comparisons this type replaced
/// were each doing locally (#0064). `mp sync --mailbox INBOX` therefore files
/// its rows under `inbox`, where the sidebar and the selector look for them,
/// instead of under a second `INBOX` key nothing lists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MailboxRole {
    Inbox,
    Archive,
    Sent,
    /// A configured mailbox with no product role, keyed by its server name.
    ///
    /// The name is kept verbatim, because it is the key its rows already carry
    /// in the store; a mailbox whose server name happens to spell one of the
    /// three roles is the one place where reading a stored key back yields the
    /// role rather than this arm, and it costs nothing: config resolves that
    /// name to the mapped mailbox either way.
    Other(String),
}

impl MailboxRole {
    /// The canonical key: what the store holds and what selectors print.
    pub fn as_str(&self) -> &str {
        match self {
            MailboxRole::Inbox => "inbox",
            MailboxRole::Archive => "archive",
            MailboxRole::Sent => "sent",
            MailboxRole::Other(name) => name,
        }
    }

    pub fn is_inbox(&self) -> bool {
        matches!(self, MailboxRole::Inbox)
    }

    pub fn is_sent(&self) -> bool {
        matches!(self, MailboxRole::Sent)
    }
}

impl From<&str> for MailboxRole {
    fn from(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "inbox" => MailboxRole::Inbox,
            "archive" => MailboxRole::Archive,
            "sent" => MailboxRole::Sent,
            _ => MailboxRole::Other(name.to_string()),
        }
    }
}

impl std::fmt::Display for MailboxRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single attendee within an `event:` frontmatter block.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct EventAttendee {
    pub address: String,
    /// needs-action | accepted | tentative | declined
    pub status: String,
}

/// The nested `event:` frontmatter block populated when an email carries an
/// iMIP calendar invitation. The sidecar `.ics` is the source of truth; this
/// block is a render/query cache (see `docs/plans/calendar-invites.md`, D2).
///
/// Every field is optional or defaulted so that emails without an `event:`
/// block (the vast majority) round-trip unchanged.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct EventFrontmatter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// REQUEST | REPLY | CANCEL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default)]
    pub sequence: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// RFC3339 with offset where the source carried a resolvable timezone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organizer: Option<String>,
    /// Own RSVP status: needs-action | accepted | tentative | declined.
    #[serde(default)]
    pub rsvp: String,
    /// Human-readable RRULE summary, empty when the event does not recur (D6).
    #[serde(default)]
    pub recurrence: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<EventAttendee>,
}

/// Read a string field that may be written as a bare key (YAML null) as the
/// empty string. Paired with `#[serde(default)]`, which covers the field being
/// absent altogether.
fn null_as_empty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct EmailFrontmatter {
    /// Stable identity of a draft (#0050, DAL decision C). `mp new` writes it,
    /// the drafts index assigns one to any agent-written file that lacks it,
    /// and it is the key of every `mp://<account>/drafts/<key>` selector. It
    /// survives a rename, which is exactly what the filename cannot do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub cc: Option<String>,
    #[serde(default)]
    pub bcc: Option<String>,
    /// The subject line. Tolerant of a bare `subject:` key (YAML null) and of
    /// the field being absent, both of which read as the empty string (#0050).
    ///
    /// Agents write drafts into `drafts/` and the index assigns them ids, so a
    /// field that failed to deserialize made such a draft *invisible*: skipped
    /// by the index with a log line nobody reads, absent from `mp list` and
    /// from the TUI. The empty subject is not thereby accepted as sendable;
    /// [`crate::draft::validate_draft`] still refuses it, which moves the
    /// diagnosis from a silent skip to `mp validate` saying what is missing.
    /// Tolerance is scoped to this one field: genuinely malformed YAML still
    /// fails to parse and is still skipped with a log line.
    #[serde(default, deserialize_with = "null_as_empty_string")]
    pub subject: String,
    pub status: EmailStatus,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<String>>,
    #[serde(default)]
    pub sent_at: Option<String>,
    #[serde(default)]
    pub sent_via: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    /// The `Message-ID` of the message this draft answers (#TKT-0051).
    ///
    /// Written by the reply builder and read once, by the post-send hook that
    /// puts `\Answered` on the source: the second status axis is only honest
    /// if it is set when the reply actually goes out, not when the draft is
    /// opened. Optional and absent from every other draft, so a draft written
    /// before this field existed parses unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// The `Message-ID` of the message this draft forwards (#TKT-0051). The
    /// forward half of [`EmailFrontmatter::in_reply_to`]; the two are mutually
    /// exclusive by construction, since a draft is built as one or the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_from: Option<String>,
    /// The `date:` line the draft skeleton writes. Carried so the drafts index
    /// can list it; nothing in the send path reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<EventFrontmatter>,
}

#[derive(Debug)]
pub struct EmailDraft {
    pub path: PathBuf,
    pub frontmatter: EmailFrontmatter,
    pub body_markdown: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_email_status_serde_roundtrip() {
        for status in [EmailStatus::Draft, EmailStatus::Approved, EmailStatus::Sent] {
            let yaml = serde_yaml::to_string(&status).unwrap();
            let back: EmailStatus = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn test_email_status_display() {
        assert_eq!(EmailStatus::Draft.to_string(), "draft");
        assert_eq!(EmailStatus::Approved.to_string(), "approved");
        assert_eq!(EmailStatus::Sent.to_string(), "sent");
    }

    /// The second axis is a set, not an enum: a message can be read, answered
    /// and forwarded at once, and the stored string round-trips all three.
    #[test]
    fn message_flags_round_trip_through_the_stored_string() {
        let all = MessageFlags {
            seen: true,
            answered: true,
            forwarded: true,
            flagged: true,
        };
        assert_eq!(all.to_flag_string(), "\\Seen \\Answered \\Flagged $Forwarded");
        assert_eq!(MessageFlags::parse(&all.to_flag_string()), all);
        assert_eq!(MessageFlags::default().to_flag_string(), "");
        assert_eq!(MessageFlags::parse(""), MessageFlags::default());
    }

    /// A store written before this axis existed holds `\Seen` or nothing, and
    /// reads back as exactly the read bit it always meant.
    #[test]
    fn a_pre_axis_flag_string_reads_as_the_read_bit_alone() {
        assert_eq!(MessageFlags::parse("\\Seen"), MessageFlags::seen(true));
    }

    /// Flags this build does not own are not the client's to curate: `\Draft`
    /// and `\Recent` must survive being read past, not be misread as something
    /// else.
    #[test]
    fn unknown_flags_are_ignored_rather_than_guessed_at() {
        assert_eq!(
            MessageFlags::parse("\\Draft \\Recent"),
            MessageFlags::default()
        );
    }

    /// `\Flagged` is the star (#0007): it parses to the flagged bit, round-trips
    /// through the stored string, and rides beside the read bit without
    /// disturbing it.
    #[test]
    fn the_flagged_bit_parses_and_round_trips() {
        assert!(MessageFlags::parse("\\Flagged").flagged);
        let flagged = MessageFlags::default().with_flagged(true);
        assert_eq!(flagged.to_flag_string(), "\\Flagged");
        assert_eq!(MessageFlags::parse(&flagged.to_flag_string()), flagged);
        let seen_flagged = MessageFlags::seen(true).with_flagged(true);
        assert!(seen_flagged.seen && seen_flagged.flagged);
        assert!(!seen_flagged.with_flagged(false).flagged);
        assert!(seen_flagged.with_flagged(false).seen);
    }

    /// Servers disagree on the spelling of the forwarded keyword; both are
    /// read, one is written.
    #[test]
    fn both_spellings_of_the_forwarded_keyword_are_read() {
        assert!(MessageFlags::parse("$Forwarded").forwarded);
        assert!(MessageFlags::parse("\\Forwarded").forwarded);
        assert!(MessageFlags::parse("$forwarded").forwarded);
        assert_eq!(
            MessageFlags {
                forwarded: true,
                ..Default::default()
            }
            .to_flag_string(),
            "$Forwarded"
        );
    }

    /// The read/unread toggle and the Graph path answer one bit; the history
    /// bits they know nothing about stay where they were.
    #[test]
    fn setting_the_read_bit_leaves_the_history_bits_alone() {
        let answered = MessageFlags {
            seen: true,
            answered: true,
            forwarded: false,
            flagged: false,
        };
        let unread = answered.with_seen(false);
        assert!(!unread.seen);
        assert!(unread.answered);
    }

    /// The file-era placement states are gone from the type, so a draft that
    /// carries one is a draft this build refuses to read rather than one it
    /// silently reinterprets. Nothing writes them: the receive path stopped
    /// writing `.md` at the store cutover, and no draft was ever created with
    /// one (#0064).
    #[test]
    fn the_file_era_placement_states_no_longer_deserialize() {
        for legacy in ["inbox", "archived"] {
            let parsed: Result<EmailStatus, _> = serde_yaml::from_str(legacy);
            assert!(parsed.is_err(), "'{legacy}' is not a draft state");
        }
    }

    #[test]
    fn the_named_roles_parse_in_any_case_and_print_canonically() {
        for spelling in ["inbox", "INBOX", "Inbox"] {
            assert_eq!(MailboxRole::from(spelling), MailboxRole::Inbox);
        }
        assert_eq!(MailboxRole::from("Archive"), MailboxRole::Archive);
        assert_eq!(MailboxRole::from("SENT"), MailboxRole::Sent);
        assert_eq!(MailboxRole::Inbox.to_string(), "inbox");
        assert_eq!(MailboxRole::Archive.as_str(), "archive");
        assert_eq!(MailboxRole::Sent.as_str(), "sent");
    }

    /// An unmapped mailbox keeps its server name verbatim: that name is the
    /// store key its rows already carry, so folding its case would orphan them.
    #[test]
    fn an_unmapped_mailbox_keeps_its_server_name() {
        let role = MailboxRole::from("INBOX.Archive");
        assert_eq!(role, MailboxRole::Other("INBOX.Archive".to_string()));
        assert_eq!(role.as_str(), "INBOX.Archive");
        assert!(!role.is_inbox());
        assert!(!role.is_sent());
    }

    #[test]
    fn test_email_frontmatter_serde_roundtrip() {
        let fm = EmailFrontmatter {
            id: None,
            date: None,
            to: Some("alice@example.com".to_string()),
            cc: Some("bob@example.com".to_string()),
            bcc: None,
            subject: "Test Subject".to_string(),
            status: EmailStatus::Draft,
            from: Some("sender@example.com".to_string()),
            reply_to: None,
            attachments: Some(vec!["file.pdf".to_string()]),
            sent_at: None,
            sent_via: None,
            message_id: Some("<test@example.com>".to_string()),
            in_reply_to: None,
            forwarded_from: None,
            event: None,
        };
        let yaml = serde_yaml::to_string(&fm).unwrap();
        let back: EmailFrontmatter = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.to, fm.to);
        assert_eq!(back.cc, fm.cc);
        assert_eq!(back.subject, fm.subject);
        assert_eq!(back.status, fm.status);
        assert_eq!(back.from, fm.from);
        assert_eq!(back.attachments, fm.attachments);
        assert_eq!(back.message_id, fm.message_id);
    }
}
