//! The CLI read surface over the store: `mp show` and `mp list-messages` (#0062).
//!
//! After `mp sync`, received mail was reachable only through the TUI or through
//! `mp dump-mailbox`, which exists as the parity oracle and emits NDJSON for a
//! diff, not for a person. These two commands close that gap with the queries
//! `src/store/read.rs` already has; nothing here adds a query, and nothing here
//! touches the network. What the store holds is what they print.
//!
//! Both render to a `String` rather than printing, so the layout is testable
//! without a subprocess and `main.rs` keeps one `println!` per command.

use anyhow::Result;
use colored::*;
use serde::Serialize;

use crate::selector::Selector;
use crate::store::read::{self, MessageRow};
use crate::store::{BlobStore, Store};

/// Width of the rules `mp list` draws, so the two listings line up.
const RULE: usize = 72;

/// What `mp show --json` emits: one object, the message and its body.
///
/// Deliberately not [`crate::dump::EnvelopeRecord`]: the dump is a *parity
/// oracle* whose shape is pinned against the file era and must stay
/// byte-stable, so hanging a body off it would make one contract serve two
/// masters. This record is free to carry what a reader of one message wants.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ShownMessage {
    pub selector: String,
    pub account: String,
    pub mailbox: String,
    pub message_id: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    pub flags: Vec<String>,
    pub invite: bool,
    pub attachments: Vec<ShownAttachment>,
    /// The stored plain-text body. `null` when the store has no readable body
    /// for the row: an evicted blob, or a message ingested without one. A
    /// reader can tell that from an empty body, which is a message that was
    /// sent empty.
    pub body: Option<String>,
}

/// One attachment of a shown message.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShownAttachment {
    pub name: String,
    pub size: u64,
}

/// A stored header with the empty string read as absent, the rule ingest's
/// columns need everywhere: an absent header arrives as `""`, not as SQL NULL.
fn present(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(str::to_string)
}

/// The flag tokens of a row, in a fixed order so two runs agree.
fn flag_names(row: &MessageRow) -> Vec<String> {
    let flags = row.flags();
    let mut out = Vec::new();
    for (on, name) in [
        (flags.seen, "read"),
        (flags.answered, "answered"),
        (flags.forwarded, "forwarded"),
        (flags.flagged, "flagged"),
    ] {
        if on {
            out.push(name.to_string());
        }
    }
    out
}

/// Collect one message for `mp show`, body included.
///
/// The body is read through [`read::load_body`], which degrades: a row whose
/// blob is gone answers `Some("")` rather than failing, and a reference to a
/// row that no longer exists answers `None`. Both arrive here as `None` in the
/// record, because from a reader's side they are the same fact -- the store
/// cannot show you this body -- and neither is worth a backtrace.
pub fn shown_message(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    row: &MessageRow,
) -> ShownMessage {
    let attachments = read::attachments_for(store, row.id)
        .unwrap_or_else(|e| {
            log::warn!("[show] attachments of message {}: {e:#}", row.id);
            Vec::new()
        })
        .into_iter()
        .map(|att| ShownAttachment {
            name: att.name,
            size: att.size,
        })
        .collect();
    ShownMessage {
        selector: Selector::for_message(account, row).to_string(),
        account: account.to_string(),
        mailbox: row.mailbox.clone(),
        message_id: row.message_id.clone(),
        from: present(row.from.as_deref()),
        to: present(row.to.as_deref()),
        cc: present(row.cc.as_deref()),
        subject: present(row.subject.as_deref()),
        date: present(row.date_display.as_deref()),
        flags: flag_names(row),
        invite: row.is_invite,
        attachments,
        body: read::load_body(store, blobs, row.id).filter(|body| !body.is_empty()),
    }
}

/// `mp show <selector>`: one message, headers then body.
///
/// The layout is the headers block `mp fetch --full` prints, with the selector
/// and mailbox added because this command is addressed by selector and the
/// answer should paste into the next one.
///
/// Not `read::render_markdown`, although #0075 built it and it is the obvious
/// candidate: it wraps the headers in a `---` YAML frontmatter and then appends
/// the body verbatim, so a message whose body opens with `---` reads back as
/// though the frontmatter continued (#0062 scope item 5). A human read surface
/// that is not meant to be parsed should not invent a document format it does
/// not escape; `--json` is the parseable answer, and it cannot be ambiguous.
pub fn render_show(message: &ShownMessage) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };

    line(format!(
        "{}: {}",
        "From".bold().green(),
        message.from.as_deref().unwrap_or("(unknown sender)")
    ));
    line(format!(
        "{}: {}",
        "To".bold().blue(),
        message.to.as_deref().unwrap_or("(no recipient)")
    ));
    if let Some(cc) = message.cc.as_deref() {
        line(format!("{}: {}", "Cc".bold().blue(), cc));
    }
    line(format!(
        "{}: {}",
        "Subject".bold().yellow(),
        message.subject.as_deref().unwrap_or("(no subject)")
    ));
    if let Some(date) = message.date.as_deref() {
        line(format!("{}: {}", "Date".bold().magenta(), date));
    }
    line(format!("{}: {}", "Mailbox".bold(), message.mailbox));
    line(format!("{}: {}", "Selector".bold(), message.selector));
    if !message.flags.is_empty() {
        line(format!("{}: {}", "Flags".bold(), message.flags.join(", ")));
    }
    if message.invite {
        line("[calendar invitation]".yellow().to_string());
    }
    if !message.attachments.is_empty() {
        line(format!("{}:", "Attachments".bold()));
        for att in &message.attachments {
            line(format!("  {} ({})", att.name, human_size(att.size)));
        }
    }

    line("\u{2500}".repeat(RULE));
    match message.body.as_deref() {
        Some(body) => line(body.trim_end().to_string()),
        // The degrade the acceptance criteria ask for: a body the store cannot
        // produce is a sentence, not an error, because the row is still real
        // and everything above this line is still true.
        None => line(
            "(no stored body: it was evicted or never ingested; `mp sync` fetches it again)"
                .dimmed()
                .to_string(),
        ),
    }
    out
}

/// `mp list-messages`: one line per message, plus its subject, per mailbox.
///
/// `rows` is one `(mailbox label, messages)` group per mailbox in listing
/// order, already truncated to the limit by the caller, and `total` is how many
/// the store holds for that group before truncation, so the header can say what
/// was left out.
///
/// The shape follows `mp list` (the drafts listing) deliberately: the same
/// rules, the same `[status] selector -> counterpart` line and the same dimmed
/// subject underneath, because a user who has read one has read both.
pub fn render_list(account: &str, groups: &[(String, usize, Vec<MessageRow>)]) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };

    let shown: usize = groups.iter().map(|(_, _, rows)| rows.len()).sum();
    if shown == 0 {
        return format!("No messages in the local store for {account}\n");
    }

    for (label, total, rows) in groups {
        if rows.is_empty() {
            continue;
        }
        line(String::new());
        line(format!(
            "{} ({} of {}):",
            label.bold(),
            rows.len(),
            total
        ));
        line("\u{2500}".repeat(RULE));
        for row in rows {
            let flags = row.flags();
            let status = if flags.seen {
                "read".dimmed()
            } else {
                "unread".yellow()
            };
            line(format!(
                "[{}] {} \u{2192} {}",
                status,
                Selector::for_message(account, row),
                row.from.as_deref().unwrap_or("(unknown sender)")
            ));
            let mut second = String::new();
            if let Some(date) = present(row.date_display.as_deref()) {
                second.push_str(&date);
                second.push_str("  ");
            }
            second.push_str(row.subject.as_deref().unwrap_or("(no subject)"));
            if row.has_attachments {
                second.push_str("  [attachments]");
            }
            line(format!("      {}", second.dimmed()));
        }
    }
    line("\u{2500}".repeat(RULE));
    let total: usize = groups.iter().map(|(_, total, _)| total).sum();
    line(format!("Shown: {shown} | In the store: {total}"));
    out
}

/// `mp search --local`: the ranked hits of the FTS5 index (#0043).
///
/// Flat and best-first rather than grouped by mailbox, because the ranking is
/// the answer: a full-tree search that regrouped its hits would hide which of
/// them the index thought was the closest. Each hit prints the two lines
/// `mp list-messages` prints, with the mailbox in front of the sender so the
/// scope of a hit is readable without parsing the selector, plus the body when
/// `bodies` carries one (`--full`).
pub fn render_search(
    account: &str,
    query: &str,
    hits: &[(MessageRow, Option<String>)],
) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };

    if hits.is_empty() {
        return format!("No local matches for {query:?} in {account}\n");
    }

    line(String::new());
    line(format!(
        "{} in {} ({} hit{}):",
        format!("{query:?}").bold(),
        account,
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    ));
    line("\u{2500}".repeat(RULE));
    for (row, body) in hits {
        let flags = row.flags();
        let status = if flags.seen {
            "read".dimmed()
        } else {
            "unread".yellow()
        };
        line(format!(
            "[{}] {} \u{2192} {}",
            status,
            Selector::for_message(account, row),
            row.from.as_deref().unwrap_or("(unknown sender)")
        ));
        let mut second = String::new();
        if let Some(date) = present(row.date_display.as_deref()) {
            second.push_str(&date);
            second.push_str("  ");
        }
        second.push_str(row.subject.as_deref().unwrap_or("(no subject)"));
        if row.has_attachments {
            second.push_str("  [attachments]");
        }
        line(format!("      {}", second.dimmed()));
        if let Some(body) = body {
            line("\u{2500}".repeat(RULE));
            line(body.trim_end().to_string());
            line("\u{2500}".repeat(RULE));
        }
    }
    line("\u{2500}".repeat(RULE));
    line(format!("Shown: {} (best match first)", hits.len()));
    out
}

/// A byte count in the units a person reads attachment sizes in.
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    match bytes {
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.1} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

/// Serialize a shown message as pretty JSON, the `--json` answer.
pub fn to_json(message: &ShownMessage) -> Result<String> {
    Ok(serde_json::to_string_pretty(message)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{ingest_message, IngestInput};
    use crate::parse::FetchedEmail;
    use tempfile::TempDir;

    struct Fixture {
        _dir: TempDir,
        store: Store,
        blobs: BlobStore,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        Fixture { _dir: dir, store, blobs }
    }

    fn email(subject: &str, body: &str) -> FetchedEmail {
        FetchedEmail {
            from: "Ada Lovelace <ada@example.com>".into(),
            to: "b@example.com".into(),
            cc: None,
            subject: subject.into(),
            date: "Thu, 7 Aug 2026 10:00:00 +0000".into(),
            body_text: body.into(),
            html_body: None,
            has_attachments: false,
            message_id: Some(format!("<{subject}@example.com>")),
            attachments: Vec::new(),
            flags: Default::default(),
            calendar_ics: None,
            event: None,
        }
    }

    fn ingest(fx: &Fixture, mailbox: &str, uid: i64, e: &FetchedEmail) -> MessageRow {
        let outcome = ingest_message(
            &fx.store,
            &fx.blobs,
            &IngestInput {
                account: "acct",
                mailbox,
                uid,
                email: e,
                raw: None,
            },
        )
        .unwrap();
        read::find_by_id(&fx.store, outcome.row_id).unwrap().unwrap()
    }

    /// The acceptance criterion: `mp show` prints the body the TUI shows, which
    /// is the same `read::load_body` the preview pane reads.
    #[test]
    fn show_prints_the_stored_body_and_the_canonical_selector() {
        colored::control::set_override(false);
        let fx = fixture();
        let row = ingest(&fx, "inbox", 4, &email("hello", "the body\nsecond line"));

        let shown = shown_message(&fx.store, &fx.blobs, "acct", &row);
        assert_eq!(shown.body.as_deref(), Some("the body\nsecond line"));
        assert_eq!(shown.selector, "mp://acct/inbox/hello@example.com");

        let text = render_show(&shown);
        assert!(text.contains("From: Ada Lovelace <ada@example.com>"));
        assert!(text.contains("Subject: hello"));
        assert!(text.contains("Selector: mp://acct/inbox/hello@example.com"));
        assert!(text.ends_with("the body\nsecond line\n"));
    }

    /// A body whose first line is `---` is why this is not
    /// `read::render_markdown`: nothing downstream can mistake it for a
    /// frontmatter fence, because there is no frontmatter.
    #[test]
    fn a_body_that_opens_with_a_yaml_fence_is_not_ambiguous() {
        colored::control::set_override(false);
        let fx = fixture();
        let row = ingest(&fx, "inbox", 5, &email("fenced", "---\nfrom: forged@example.com\n---"));

        let text = render_show(&shown_message(&fx.store, &fx.blobs, "acct", &row));
        let rendered = read::render_markdown(&fx.store, &fx.blobs, &row);
        assert_eq!(
            rendered.matches("\n---\n").count(),
            3,
            "the markdown rendition really is ambiguous, which is what this avoids"
        );
        assert!(!text.starts_with("---"), "the show layout opens with headers, not a fence");
        assert!(text.contains("from: forged@example.com"), "the body is still printed in full");
    }

    /// An evicted body degrades to a sentence rather than an error, and says so
    /// in JSON with `null` instead of an empty string.
    #[test]
    fn an_evicted_body_degrades_to_a_message() {
        colored::control::set_override(false);
        let fx = fixture();
        let row = ingest(&fx, "inbox", 6, &email("evicted", "gone soon"));
        // Evict the blob the way retention would: the row survives, the bytes
        // do not.
        std::fs::remove_dir_all(fx.blobs.root()).unwrap();

        let shown = shown_message(&fx.store, &fx.blobs, "acct", &row);
        assert_eq!(shown.body, None);
        let text = render_show(&shown);
        assert!(text.contains("Subject: evicted"), "the envelope is still readable");
        assert!(text.contains("no stored body"));
    }

    /// The listing follows the store's own order (newest first), carries the
    /// selector that addresses each row, and reports what the limit left out.
    #[test]
    fn the_listing_names_every_row_by_selector_and_reports_the_truncation() {
        colored::control::set_override(false);
        let fx = fixture();
        for (uid, subject) in [(1, "oldest"), (2, "middle"), (3, "newest")] {
            let mut e = email(subject, "body");
            e.date = format!("Thu, {} Aug 2026 10:00:00 +0000", 5 + uid);
            ingest(&fx, "inbox", uid, &e);
        }
        let all = read::list_mailbox(&fx.store, "acct", "inbox").unwrap();
        assert_eq!(all.len(), 3);

        // The limit is applied by the caller over the store's own order, which
        // is the order the TUI list shows, so the listing is the first N of it
        // and the N+1th is absent.
        let text = render_list("acct", &[("Inbox".into(), all.len(), all[..2].to_vec())]);
        assert!(text.contains("Inbox (2 of 3):"));
        for row in &all[..2] {
            assert!(text.contains(&Selector::for_message("acct", row).to_string()));
        }
        assert!(
            !text.contains(&Selector::for_message("acct", &all[2]).to_string()),
            "the limit cuts from the end of the store's order, not from the middle"
        );
        assert!(text.contains("[unread]"));
        assert!(text.contains("Shown: 2 | In the store: 3"));
    }

    /// `mp search --local` prints its hits best-first, each addressable by the
    /// selector the next command takes, and shows the body only with `--full`.
    #[test]
    fn the_search_listing_is_ranked_selectors_and_optional_bodies() {
        colored::control::set_override(false);
        let fx = fixture();
        let first = ingest(&fx, "inbox", 1, &email("ledger", "the quarterly ledger"));
        let second = ingest(&fx, "archive", 2, &email("lunch", "ledger of pizzas"));

        let text = render_search(
            "acct",
            "ledger",
            &[(first.clone(), None), (second.clone(), None)],
        );
        assert!(text.contains("\"ledger\" in acct (2 hits):"));
        let at_first = text.find(&Selector::for_message("acct", &first).to_string());
        let at_second = text.find(&Selector::for_message("acct", &second).to_string());
        assert!(at_first < at_second, "hits print in the order they are ranked");
        assert!(text.contains("Shown: 2 (best match first)"));
        assert!(!text.contains("quarterly ledger"), "no body without --full");

        let full = render_search("acct", "ledger", &[(first, Some("the quarterly ledger".into()))]);
        assert!(full.contains("the quarterly ledger"));
        assert!(full.contains("(1 hit):"));
    }

    /// A query nothing matches says so rather than printing an empty frame.
    #[test]
    fn an_empty_search_says_so() {
        colored::control::set_override(false);
        let text = render_search("acct", "nothing", &[]);
        assert_eq!(text, "No local matches for \"nothing\" in acct\n");
    }

    /// An account with rows in no listed mailbox says so instead of printing an
    /// empty frame.
    #[test]
    fn an_empty_listing_says_so() {
        colored::control::set_override(false);
        let text = render_list("acct", &[("Inbox".into(), 0, Vec::new())]);
        assert_eq!(text, "No messages in the local store for acct\n");
    }
}
