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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmailStatus {
    Draft,
    Approved,
    Sent,
    Inbox,
    Archived,
}

impl std::fmt::Display for EmailStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmailStatus::Draft => write!(f, "draft"),
            EmailStatus::Approved => write!(f, "approved"),
            EmailStatus::Sent => write!(f, "sent"),
            EmailStatus::Inbox => write!(f, "inbox"),
            EmailStatus::Archived => write!(f, "archived"),
        }
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

/// Frontmatter of a received message written as a `.md` file.
///
/// The receive path stopped writing these files at the store cutover, so
/// nothing in the crate parses one in production any more. It survives as the
/// deserialization target the `draft::set_event_rsvp` and
/// `draft::set_event_attendee_status` tests read their rewritten invite back
/// through; those two rewriters are themselves file-era leftovers (see #0057),
/// and this type goes when they do.
#[derive(Debug, Deserialize)]
pub struct InboxFrontmatter {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub cc: Option<String>,
    pub subject: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<String>>,
    #[serde(default)]
    pub read: Option<bool>,
    #[serde(default)]
    pub event: Option<EventFrontmatter>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_email_status_serde_roundtrip() {
        for status in [
            EmailStatus::Draft,
            EmailStatus::Approved,
            EmailStatus::Sent,
            EmailStatus::Inbox,
            EmailStatus::Archived,
        ] {
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
        assert_eq!(EmailStatus::Inbox.to_string(), "inbox");
        assert_eq!(EmailStatus::Archived.to_string(), "archived");
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

    #[test]
    fn test_inbox_frontmatter_deserialize() {
        let yaml = r#"
from: "alice@example.com"
to: "bob@example.com"
cc: "carol@example.com"
subject: "Meeting notes"
date: "Mon, 01 Jan 2024 12:00:00 +0000"
message_id: "<abc123@example.com>"
attachments:
  - "notes.pdf"
"#;
        let fm: InboxFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.from, "alice@example.com");
        assert_eq!(fm.to, "bob@example.com");
        assert_eq!(fm.cc, Some("carol@example.com".to_string()));
        assert_eq!(fm.subject, "Meeting notes");
        assert_eq!(fm.date, Some("Mon, 01 Jan 2024 12:00:00 +0000".to_string()));
        assert_eq!(fm.message_id, Some("<abc123@example.com>".to_string()));
        assert_eq!(fm.attachments, Some(vec!["notes.pdf".to_string()]));
    }

    #[test]
    fn test_inbox_frontmatter_minimal() {
        let yaml = r#"
from: "alice@example.com"
to: "bob@example.com"
subject: "Hi"
"#;
        let fm: InboxFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.from, "alice@example.com");
        assert!(fm.cc.is_none());
        assert!(fm.date.is_none());
        assert!(fm.message_id.is_none());
        assert!(fm.attachments.is_none());
    }

    #[test]
    fn test_inbox_frontmatter_read_true() {
        let yaml = r#"
from: "alice@example.com"
to: "bob@example.com"
subject: "Hi"
read: true
"#;
        let fm: InboxFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.read, Some(true));
    }

    #[test]
    fn test_inbox_frontmatter_read_false() {
        let yaml = r#"
from: "alice@example.com"
to: "bob@example.com"
subject: "Hi"
read: false
"#;
        let fm: InboxFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.read, Some(false));
    }

    #[test]
    fn test_inbox_frontmatter_read_missing_is_none() {
        let yaml = r#"
from: "alice@example.com"
to: "bob@example.com"
subject: "Hi"
"#;
        let fm: InboxFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.read, None);
    }
}
