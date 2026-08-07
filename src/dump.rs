//! Path-free envelope dump of the message store (`mp dump-mailbox --json`).
//!
//! Written for #0049 unit 0c as the parity harness for the data-access-layer
//! redesign: it recorded one normalised record per message from the file-based
//! stack, and #0038 flipped its source to the SQLite store while keeping the
//! record shape and the sort contract identical. Nothing here carries a
//! filesystem path -- that was true when the source was a `.md` tree, and it
//! is what makes the flip invisible in the output.
//!
//! # Record shape
//!
//! One JSON object per message, in this field order (NDJSON, one object per
//! line, LF-terminated, compact):
//!
//! - `account`: account name as configured in `[[accounts]]`.
//! - `mailbox`: the role or server name (`inbox`, `drafts`, `sent`, `archive`,
//!   or the server name of an `extra` mailbox), i.e. `messages.mailbox` and
//!   the mailbox segment of an `mp://` selector. Never a path.
//! - `message_id`: `messages.message_id`, angle brackets included. See the
//!   allow-list ([docs/dump-allow-list.md](../docs/dump-allow-list.md)): the
//!   file build recorded `null` for mail with no `Message-ID:` header, while
//!   ingest synthesizes one, so identity-less mail now dumps its synthetic id.
//! - `from`, `to`, `cc`, `subject`: the stored header values verbatim (no
//!   display name extraction, no `(no subject)` placeholder), `null` when
//!   absent. A stored empty string is treated as absent: the store cannot
//!   tell "header missing" from "header empty" and the file build's `null` is
//!   the far commoner of the two.
//! - `date_sort`: the sort key `tui::app::resolve_date` derives from the
//!   stored `Date:` header (UTC `%Y-%m-%dT%H:%M:%S`), empty when the header is
//!   missing or unparseable. The file build had a filename-derived fallback in
//!   between; see the allow-list.
//! - `flags`: sorted, deduplicated state tokens from the closed set
//!   `answered`, `approved`, `draft`, `flagged`, `forwarded`, `seen`. `seen`,
//!   `answered` and `forwarded` are the three bits of the second status axis
//!   (#TKT-0051), and `flagged` is the `\Flagged` star (#0007), all read out
//!   of `messages.flags` (`\Seen`, `\Answered`, `\Flagged`,
//!   `$Forwarded`). `draft` and `approved` cannot occur on a `messages`
//!   row: drafts are local files in the `drafts` index, permanently outside
//!   this table and outside the dump contract (see the allow-list).
//! - `attachments`: array of `{"name", "size"}`, sorted by name then size,
//!   read from the attachment blobs of the row. `size` is the blob length and
//!   is therefore always present. The iMIP sidecar is excluded, exactly as the
//!   `attachments:` frontmatter list excluded it.
//! - `invite`: `true` when the message carries an iMIP payload, i.e. the row
//!   has an `invite.ics` attachment blob (`MessageRow::is_invite`).
//!
//! # Ordering
//!
//! Records are sorted by `(account, mailbox, date_sort, message_id, subject,
//! uid)`, with absent values sorting as the empty string. The uid is the final
//! tiebreaker only: it is unique within `(account, mailbox)`, so the order is
//! total even for two messages that agree on everything else, and like the
//! file name it replaced it is never emitted. Two runs over an unchanged store
//! are byte-identical; nothing in the output depends on the wallclock of the
//! run.
//!
//! Offline by construction: this module reads the local store and nothing else.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::config::AccountConfig;
use crate::store::read::{self, MessageRow};
use crate::store::{open_store, Store};
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

/// Collect envelope records for every account in `accounts`, restricted to the
/// mailboxes named in `mailbox_filter` when that filter is non-empty (matched
/// case-insensitively against both the mailbox id and its sidebar label).
/// Mailbox directories that do not exist contribute nothing.
pub fn collect_records(accounts: &[AccountConfig], mailbox_filter: &[String]) -> Vec<EnvelopeRecord> {
    let mut rows: Vec<(SortKey, EnvelopeRecord)> = Vec::new();

    for account in accounts {
        let Some(store) = open_store(&account.name) else {
            continue;
        };
        // The configured mailboxes decide what the filter can name and what a
        // label means; the rows themselves come from the store. A mailbox the
        // store has no rows for contributes nothing, exactly as a missing
        // directory did.
        let selected: Vec<String> = build_mailboxes(account)
            .into_iter()
            .filter_map(|mailbox| {
                mailbox_selected(&mailbox.id, &mailbox.label, mailbox_filter)
                    .then_some(mailbox.id)
            })
            .collect();

        let all = match read::list_account(&store, &account.name) {
            Ok(rows) => rows,
            Err(e) => {
                log::warn!("[dump] reading {}: {e:#}", account.name);
                continue;
            }
        };
        for row in all {
            if !selected.iter().any(|id| *id == row.mailbox) {
                continue;
            }
            let record = read_record(&store, &account.name, &row);
            rows.push((sort_key(&record, &row), record));
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

/// `(account, mailbox, date_sort, message_id, subject, uid)`.
type SortKey = (String, String, String, String, String, i64);

fn sort_key(record: &EnvelopeRecord, row: &MessageRow) -> SortKey {
    (
        record.account.clone(),
        record.mailbox.clone(),
        record.date_sort.clone(),
        record.message_id.clone().unwrap_or_default(),
        record.subject.clone().unwrap_or_default(),
        row.uid,
    )
}

fn mailbox_selected(id: &str, label: &str, filter: &[String]) -> bool {
    filter.is_empty()
        || filter
            .iter()
            .any(|want| want.eq_ignore_ascii_case(id) || want.eq_ignore_ascii_case(label))
}

/// A stored header value, with the empty string read as absent.
///
/// Ingest writes whatever the parser produced, and an absent header arrives as
/// an empty string rather than as SQL `NULL`. The file build recorded `null`
/// for an absent header and had no way to produce an empty one, so the empty
/// string maps back to `null` here.
fn header(value: Option<&String>) -> Option<String> {
    value.filter(|v| !v.is_empty()).cloned()
}

/// Turn one store row into a record.
fn read_record(store: &Store, account: &str, row: &MessageRow) -> EnvelopeRecord {
    // The same `resolve_date` the TUI applies, over the same stored `Date:`
    // header, so the two stacks cannot drift. The path argument is empty:
    // the filename fallback died with the filenames (see the allow-list).
    let (_display, date_sort) = resolve_date(&row.date_display, &None, Path::new(""));

    let mut flags: BTreeSet<String> = BTreeSet::new();
    let axis = row.flags();
    if axis.seen {
        flags.insert("seen".to_string());
    }
    if axis.answered {
        flags.insert("answered".to_string());
    }
    if axis.forwarded {
        flags.insert("forwarded".to_string());
    }
    if axis.flagged {
        flags.insert("flagged".to_string());
    }
    // `draft` and `approved` were frontmatter `status:` values. Nothing writes
    // them to a `messages` row: drafts live in the `drafts` index, outside
    // this table by design.

    let mut attachments: Vec<AttachmentRecord> = read::attachments_for(store, row.id)
        .unwrap_or_else(|e| {
            log::warn!("[dump] attachments of message {}: {e:#}", row.id);
            Vec::new()
        })
        .into_iter()
        .map(|att| AttachmentRecord {
            name: att.name,
            size: Some(att.size),
        })
        .collect();
    attachments.sort();

    EnvelopeRecord {
        account: account.to_string(),
        mailbox: row.mailbox.clone(),
        message_id: header(Some(&row.message_id)),
        from: header(row.from.as_ref()),
        to: header(row.to.as_ref()),
        cc: header(row.cc.as_ref()),
        subject: header(row.subject.as_ref()),
        date_sort,
        flags: flags.into_iter().collect(),
        attachments,
        invite: row.is_invite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
