//! Path-free envelope dump of the local `.md` tree (`mp dump-mailbox --json`).
//!
//! Written for #0049 unit 0c, as the parity harness for the data-access-layer
//! redesign: the complete nuke removes the byte-identity oracle, so this
//! command records one normalised record per message from the current
//! file-based stack. The new SQLite store must be able to emit the same
//! records from the database, which is why nothing here carries a filesystem
//! path: a path is the one thing the two stacks cannot agree on.
//!
//! # Record shape
//!
//! One JSON object per message, in this field order (NDJSON, one object per
//! line, LF-terminated, compact):
//!
//! - `account`: account name as configured in `[[accounts]]`.
//! - `mailbox`: the role or slugified server name (`inbox`, `drafts`, `sent`,
//!   `archive`, or the slug of an `extra` mailbox), i.e. the leaf that
//!   `config::mailbox_dir` builds. Never a path.
//! - `message_id`: the `message_id:` frontmatter value verbatim, angle
//!   brackets included, or `null` when the file has none. Deliberately not
//!   synthesized: synthesizing an identity for identity-less mail is the new
//!   stack's behaviour, and recording it here would launder it into the
//!   oracle.
//! - `from`, `to`, `cc`, `subject`: frontmatter values verbatim (no display
//!   name extraction, no `(no subject)` placeholder), `null` when absent.
//! - `date_sort`: the current build's sort key, produced by
//!   `tui::app::resolve_date` (UTC `%Y-%m-%dT%H:%M:%S`, falling back to the
//!   date encoded in the filename, then to the empty string). The fallback reads the
//!   file *name*, never the directory, so the value stays reproducible from a
//!   store that keeps the original filename or the parsed date.
//! - `flags`: sorted, deduplicated state tokens from the closed set
//!   `approved`, `draft`, `seen`. `seen` comes from `read: true`, `draft` and
//!   `approved` from `status:`. The current build tracks no other per-message
//!   flag locally (no answered, no flagged), so the list is short by
//!   construction rather than by omission.
//! - `attachments`: array of `{"name", "size"}`, sorted by name then size.
//!   Names come from the `attachments:` frontmatter list (the record the
//!   stack itself keeps), reduced to their file name: sent and draft mail
//!   stores the *source* path of an outgoing attachment there
//!   (`/tmp/briefing.mp3`), and a path is exactly what must not reach the
//!   output. `size` is the byte length of the matching file in the sibling
//!   `<stem>_attachments/` directory, or `null` when that file is missing
//!   (which is the normal case for those outgoing entries). A message with
//!   `has_attachments: true` but no `attachments:` list dumps an empty array,
//!   which is exactly what the current build knows.
//! - `invite`: `true` when the file carries an `event:` frontmatter block
//!   (same predicate as `EmailEntry::is_invite`).
//!
//! # Ordering
//!
//! Records are sorted by `(account, mailbox, date_sort, message_id, subject,
//! file name)`, with absent values sorting as the empty string. The file name
//! is the final tiebreaker only: it is unique within a mailbox directory, so
//! the order is total even for two messages that agree on everything else, and
//! it is never emitted. Two runs over an unchanged tree are byte-identical;
//! nothing in the output depends on the wallclock of the run.
//!
//! Offline by construction: this module reads the local tree and nothing else.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gray_matter::engine::YAML;
use gray_matter::Matter;
use serde::{Deserialize, Serialize};

use crate::config::AccountConfig;
use crate::parse::attachments_dir_for;
use crate::tui::app::{build_mailboxes, resolve_date};

/// One attachment, name and size only.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttachmentRecord {
    pub name: String,
    /// Byte length on disk, `null` when the file is not present.
    pub size: Option<u64>,
}

/// One message envelope, normalised and free of filesystem paths.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EnvelopeRecord {
    pub account: String,
    pub mailbox: String,
    pub message_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub date_sort: String,
    pub flags: Vec<String>,
    pub attachments: Vec<AttachmentRecord>,
    pub invite: bool,
}

/// Raw frontmatter fields this dump needs. Deliberately a local copy of the
/// TUI's `Frontmatter` (see `tui::app::types`): the load path is about to be
/// replaced wholesale, and coupling the oracle to it would mean the oracle
/// changes with the thing it measures.
#[derive(Debug, Deserialize, Default)]
struct DumpFrontmatter {
    from: Option<String>,
    to: Option<String>,
    cc: Option<String>,
    subject: Option<String>,
    status: Option<String>,
    date: Option<String>,
    sent_at: Option<String>,
    message_id: Option<String>,
    attachments: Option<Vec<String>>,
    read: Option<bool>,
    #[serde(default)]
    event: Option<crate::types::EventFrontmatter>,
}

/// Collect envelope records for every account in `accounts`, restricted to the
/// mailboxes named in `mailbox_filter` when that filter is non-empty (matched
/// case-insensitively against both the mailbox id and its sidebar label).
/// Mailbox directories that do not exist contribute nothing.
pub fn collect_records(accounts: &[AccountConfig], mailbox_filter: &[String]) -> Vec<EnvelopeRecord> {
    let mut rows: Vec<(SortKey, EnvelopeRecord)> = Vec::new();

    for account in accounts {
        for mailbox in build_mailboxes(account) {
            let id = mailbox_id(&mailbox.dir);
            if !mailbox_selected(&id, &mailbox.label, mailbox_filter) {
                continue;
            }
            for path in mailbox_files(&mailbox.dir) {
                if let Some(record) = read_record(&account.name, &id, &path) {
                    rows.push((sort_key(&record, &path), record));
                }
            }
        }
    }

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, record)| record).collect()
}

/// Serialize records as NDJSON: one compact JSON object per line, each line
/// terminated by `\n` (so the output ends with a newline and concatenates
/// cleanly).
pub fn to_ndjson(records: &[EnvelopeRecord]) -> String {
    let mut out = String::new();
    for record in records {
        // Serializing a plain struct of owned strings cannot fail.
        out.push_str(&serde_json::to_string(record).expect("envelope record serializes"));
        out.push('\n');
    }
    out
}

/// `(account, mailbox, date_sort, message_id, subject, file name)`.
type SortKey = (String, String, String, String, String, String);

fn sort_key(record: &EnvelopeRecord, path: &Path) -> SortKey {
    (
        record.account.clone(),
        record.mailbox.clone(),
        record.date_sort.clone(),
        record.message_id.clone().unwrap_or_default(),
        record.subject.clone().unwrap_or_default(),
        path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
    )
}

/// The mailbox identifier: the directory leaf that `config::mailbox_dir`
/// builds (`inbox`, `sent`, or a slugified server name). A name, not a path.
fn mailbox_id(dir: &Path) -> String {
    dir.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// The file name of an `attachments:` entry. Outgoing mail stores the source
/// path of the attachment (`/tmp/audio/briefing.mp3`), so the raw value can be
/// absolute; the file name is the part both stacks can agree on. Entries that
/// are already bare names pass through unchanged.
fn attachment_name(raw: &str) -> String {
    Path::new(raw)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.to_string())
}

fn mailbox_selected(id: &str, label: &str, filter: &[String]) -> bool {
    filter.is_empty()
        || filter
            .iter()
            .any(|want| want.eq_ignore_ascii_case(id) || want.eq_ignore_ascii_case(label))
}

/// Top-level `.md` files of a mailbox directory, in unspecified order (the
/// caller sorts). Mirrors `load_emails`: depth 1, `.md` only.
fn mailbox_files(dir: &Path) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return Vec::new();
    }
    walkdir::WalkDir::new(dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "md"))
        .map(|e| e.into_path())
        .collect()
}

/// Parse one file into a record. Returns `None` for files the current build
/// cannot read either (non-UTF-8 bytes), matching `load_emails`, which drops
/// them from the list.
fn read_record(account: &str, mailbox: &str, path: &Path) -> Option<EnvelopeRecord> {
    let content = std::fs::read_to_string(path).ok()?;
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(&content);
    let fm: DumpFrontmatter = parsed
        .data
        .and_then(|d| d.deserialize().ok())
        .unwrap_or_default();

    let (_display, date_sort) = resolve_date(&fm.date, &fm.sent_at, path);

    let mut flags: BTreeSet<String> = BTreeSet::new();
    if fm.read == Some(true) {
        flags.insert("seen".to_string());
    }
    match fm.status.as_deref() {
        Some("draft") => {
            flags.insert("draft".to_string());
        }
        Some("approved") => {
            flags.insert("approved".to_string());
        }
        _ => {}
    }

    let att_dir = attachments_dir_for(path);
    let mut attachments: Vec<AttachmentRecord> = fm
        .attachments
        .unwrap_or_default()
        .into_iter()
        .map(|raw| {
            let name = attachment_name(&raw);
            let size = std::fs::metadata(att_dir.join(&name)).ok().map(|m| m.len());
            AttachmentRecord { name, size }
        })
        .collect();
    attachments.sort();

    Some(EnvelopeRecord {
        account: account.to_string(),
        mailbox: mailbox.to_string(),
        message_id: fm.message_id,
        from: fm.from,
        to: fm.to,
        cc: fm.cc,
        subject: fm.subject,
        date_sort,
        flags: flags.into_iter().collect(),
        attachments,
        invite: fm.event.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// parity: the mailbox id is the directory leaf, which is the role or the
    /// slug the config builds, never a path.
    #[test]
    fn mailbox_id_is_the_directory_leaf() {
        assert_eq!(mailbox_id(Path::new("/data/accounts/tum/inbox")), "inbox");
        assert_eq!(mailbox_id(Path::new("/data/accounts/tum/some-folder")), "some-folder");
    }

    /// parity: an attachment recorded as a source path (sent and draft mail
    /// does this) dumps as its file name, so no filesystem path reaches the
    /// output.
    #[test]
    fn attachment_names_are_stripped_to_the_file_name() {
        assert_eq!(attachment_name("/tmp/audio/briefing.mp3"), "briefing.mp3");
        assert_eq!(attachment_name("report.pdf"), "report.pdf");
        assert_eq!(attachment_name(""), "");
        assert_eq!(attachment_name("/"), "/");
    }

    /// parity: an empty filter selects everything; a non-empty one matches the
    /// id or the sidebar label, case-insensitively.
    #[test]
    fn mailbox_filter_matches_id_or_label() {
        assert!(mailbox_selected("inbox", "Inbox", &[]));
        assert!(mailbox_selected("inbox", "Inbox", &["INBOX".to_string()]));
        assert!(mailbox_selected("inbox", "Inbox", &["inbox".to_string()]));
        assert!(!mailbox_selected("inbox", "Inbox", &["sent".to_string()]));
        assert!(mailbox_selected("some-folder", "Some/Folder", &["Some/Folder".to_string()]));
    }

    /// parity: NDJSON is one compact object per line, trailing newline
    /// included.
    #[test]
    fn ndjson_is_one_line_per_record() {
        let record = EnvelopeRecord {
            account: "a".to_string(),
            mailbox: "inbox".to_string(),
            message_id: None,
            from: Some("x@example.com".to_string()),
            to: None,
            cc: None,
            subject: Some("hi".to_string()),
            date_sort: "2026-01-01T00:00:00".to_string(),
            flags: vec!["seen".to_string()],
            attachments: vec![AttachmentRecord { name: "a.pdf".to_string(), size: Some(3) }],
            invite: false,
        };
        let out = to_ndjson(std::slice::from_ref(&record));
        assert_eq!(out.lines().count(), 1);
        assert!(out.ends_with('\n'));
        assert_eq!(
            out.trim_end(),
            r#"{"account":"a","mailbox":"inbox","message_id":null,"from":"x@example.com","to":null,"cc":null,"subject":"hi","date_sort":"2026-01-01T00:00:00","flags":["seen"],"attachments":[{"name":"a.pdf","size":3}],"invite":false}"#
        );
    }
}
