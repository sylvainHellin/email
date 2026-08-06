//! Reads the account's message rows and builds or updates a `ContactIndex`.
//!
//! The full rebuild reads `messages` through `store::read`, the same listing
//! shape the TUI and `mp dump-mailbox` use: every row already carries `from`,
//! `to`, `cc`, `mailbox` and the `Date:` header, which is exactly what the
//! ranker needs. Before [#0053](../../docs/tickets/0053-contacts-rebuild-data-loss.md)
//! this walked a `.md` tree that the store cutover deleted, so a rebuild found
//! nothing and the caller cached the nothing over months of accumulated
//! frecency.

use crate::config::AccountConfig;
use crate::contacts::filter::is_usable_address;
use crate::contacts::rank::{update_from_observation, Observation, ObservationField};
use crate::contacts::types::{Contact, ContactIndex};
use crate::store::read::{self, MessageRow};
use crate::store::Store;
use anyhow::Result;
use chrono::Utc;
use mailparse::addrparse;
use std::collections::HashMap;

/// Public observation kind used by incremental-update hooks (send/sync).
#[derive(Debug, Clone, Copy)]
pub enum ObservedIn {
    /// Recipient was in the `to:` field of a sent message.
    SentTo,
    /// Recipient was in the `cc:` or `bcc:` field of a sent message.
    SentCc,
    /// Address was observed in an inbox/archive message.
    Inbox,
}

/// Build a full `ContactIndex` from the account's message store.
///
/// An account that has never synced has no store, and that is not an error:
/// the index comes back empty and the caller decides what to do with it (see
/// `cache::save_rebuilt_cache`, which refuses to persist an empty rebuild over
/// a populated cache).
pub fn build_index_for_account(account: &AccountConfig) -> Result<ContactIndex> {
    match crate::tui::app::open_store(&account.name) {
        Some(store) => build_index_from_store(&store, account),
        None => Ok(empty_index(account)),
    }
}

/// Build a full `ContactIndex` from an already-open store.
pub(crate) fn build_index_from_store(
    store: &Store,
    account: &AccountConfig,
) -> Result<ContactIndex> {
    let mut index = empty_index(account);
    let self_addr = account.default_from.to_ascii_lowercase();

    for row in read::list_account(store, &account.name)? {
        let role = mailbox_role(&row.mailbox);
        let observed_at = row
            .date_display
            .as_deref()
            .and_then(parse_date_to_rfc3339)
            .unwrap_or_else(|| Utc::now().to_rfc3339());
        for (field, raw) in header_fields(&row) {
            let Some(raw) = raw else { continue };
            if raw.trim().is_empty() {
                continue;
            }
            process_header(
                &mut index.contacts,
                raw,
                field,
                role,
                &observed_at,
                &self_addr,
            );
        }
    }

    Ok(index)
}

fn empty_index(account: &AccountConfig) -> ContactIndex {
    ContactIndex {
        account: account.name.clone(),
        contacts: HashMap::new(),
        built_at: Utc::now().to_rfc3339(),
    }
}

/// The from/to/cc headers of one row, in the order the ranker sees them.
fn header_fields(row: &MessageRow) -> [(ObservationField, Option<&str>); 3] {
    [
        (ObservationField::From, row.from.as_deref()),
        (ObservationField::To, row.to.as_deref()),
        (ObservationField::Cc, row.cc.as_deref()),
    ]
}

/// The ranking role of a `messages.mailbox` value.
///
/// The column holds the role name for the four mapped mailboxes and a
/// slugified server name for anything else, so every unmapped mailbox folds
/// into `extra` exactly as the per-directory walk did. Only `sent` changes the
/// ranking (see `rank::update_from_observation`); the rest all count as
/// received.
fn mailbox_role(mailbox: &str) -> &'static str {
    match mailbox {
        "sent" => "sent",
        "inbox" => "inbox",
        "archive" => "archive",
        _ => "extra",
    }
}

fn process_header(
    contacts: &mut HashMap<String, Contact>,
    raw: &str,
    field: ObservationField,
    role: &'static str,
    observed_at: &str,
    self_addr: &str,
) {
    let Ok(parsed_addrs) = addrparse(raw) else {
        return;
    };
    for info in parsed_addrs.iter() {
        for (addr, name) in flatten_addr(info) {
            let addr_lc = addr.to_ascii_lowercase();
            if addr_lc == self_addr {
                continue;
            }
            if !is_usable_address(&addr_lc) {
                continue;
            }
            let obs = Observation {
                address: addr_lc,
                display_name: name,
                mailbox_role: role,
                field,
                observed_at: observed_at.to_string(),
            };
            update_from_observation(contacts, obs);
        }
    }
}

fn flatten_addr(info: &mailparse::MailAddr) -> Vec<(String, String)> {
    match info {
        mailparse::MailAddr::Single(s) => {
            vec![(s.addr.clone(), s.display_name.clone().unwrap_or_default())]
        }
        mailparse::MailAddr::Group(g) => g
            .addrs
            .iter()
            .map(|s| (s.addr.clone(), s.display_name.clone().unwrap_or_default()))
            .collect(),
    }
}

/// Convert common email date formats to RFC-3339. Returns `None` if parsing fails.
fn parse_date_to_rfc3339(s: &str) -> Option<String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.to_rfc3339());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(dt.to_rfc3339());
    }
    None
}

/// Incremental update: merge a batch of address observations into an existing
/// index. Hook point for send-post-success and sync-post-new-message updates.
///
/// Caller is responsible for persisting the index via `cache::save_cache`
/// after calling this.
pub fn observe(
    index: &mut ContactIndex,
    self_addr: &str,
    observations: &[(ObservedIn, &str)],
    observed_at: &str,
) -> Result<()> {
    let self_lc = self_addr.to_ascii_lowercase();
    for (kind, raw_header) in observations {
        if raw_header.trim().is_empty() {
            continue;
        }
        let Ok(parsed) = addrparse(raw_header) else {
            continue;
        };
        let (role, field): (&'static str, ObservationField) = match kind {
            ObservedIn::SentTo => ("sent", ObservationField::To),
            ObservedIn::SentCc => ("sent", ObservationField::Cc),
            ObservedIn::Inbox => ("inbox", ObservationField::From),
        };
        for info in parsed.iter() {
            for (addr, name) in flatten_addr(info) {
                let addr_lc = addr.to_ascii_lowercase();
                if addr_lc == self_lc || !is_usable_address(&addr_lc) {
                    continue;
                }
                let obs = Observation {
                    address: addr_lc,
                    display_name: name,
                    mailbox_role: role,
                    field,
                    observed_at: observed_at.to_string(),
                };
                update_from_observation(&mut index.contacts, obs);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{ingest_message, IngestInput};
    use crate::parse::FetchedEmail;
    use crate::store::BlobStore;
    use tempfile::TempDir;

    fn empty_index() -> ContactIndex {
        ContactIndex {
            account: "test".into(),
            contacts: HashMap::new(),
            built_at: Utc::now().to_rfc3339(),
        }
    }

    /// A store plus its blob store, both under one temp directory. No mailbox
    /// tree exists anywhere near it: the rebuild reads rows only.
    struct Fixture {
        _dir: TempDir,
        store: Store,
        blobs: BlobStore,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        Fixture {
            _dir: dir,
            store,
            blobs,
        }
    }

    fn account() -> AccountConfig {
        AccountConfig {
            name: "alice".into(),
            default_from: "me@example.com".into(),
            ..Default::default()
        }
    }

    fn email(from: &str, to: &str, cc: Option<&str>, date: &str) -> FetchedEmail {
        FetchedEmail {
            from: from.into(),
            to: to.into(),
            cc: cc.map(|s| s.into()),
            subject: "Subject".into(),
            date: date.into(),
            body_text: "body".into(),
            html_body: None,
            has_attachments: false,
            message_id: Some(format!("<{from}-{date}@example.com>")),
            attachments: Vec::new(),
            is_read: false,
            calendar_ics: None,
            event: None,
        }
    }

    /// Ingest through the real ingest API, so the fixture rows are exactly the
    /// rows the sync path writes.
    fn ingest(fx: &Fixture, mailbox: &str, uid: i64, email: &FetchedEmail) {
        ingest_message(
            &fx.store,
            &fx.blobs,
            &IngestInput {
                account: "alice",
                mailbox,
                uid,
                email,
                raw: None,
            },
        )
        .unwrap();
    }

    /// #0053: the rebuild reads the store, so two ingested messages produce
    /// two contacts with no mailbox tree present.
    #[test]
    fn rebuild_finds_both_senders_of_a_two_message_store() {
        let fx = fixture();
        ingest(
            &fx,
            "inbox",
            1,
            &email(
                "Alice <alice@example.com>",
                "me@example.com",
                None,
                "Mon, 05 Jan 2026 12:00:00 +0000",
            ),
        );
        ingest(
            &fx,
            "inbox",
            2,
            &email(
                "Bob <bob@example.com>",
                "me@example.com",
                None,
                "Tue, 06 Jan 2026 12:00:00 +0000",
            ),
        );

        let index = build_index_from_store(&fx.store, &account()).unwrap();

        assert_eq!(index.contacts.len(), 2);
        let alice = index.contacts.get("alice@example.com").expect("alice");
        assert_eq!(alice.display_name, "Alice");
        assert_eq!(alice.received, 1);
        assert_eq!(alice.sent_to, 0);
        assert_eq!(alice.last_seen, "2026-01-05T12:00:00+00:00");
        let bob = index.contacts.get("bob@example.com").expect("bob");
        assert_eq!(bob.received, 1);
        // The self address is filtered out of every field.
        assert!(!index.contacts.contains_key("me@example.com"));
    }

    /// The role comes from the row's `mailbox` column: a `sent` row bumps
    /// sent_to/sent_cc, an archive row counts as received.
    #[test]
    fn the_row_mailbox_decides_the_observation_role() {
        let fx = fixture();
        ingest(
            &fx,
            "sent",
            1,
            &email(
                "me@example.com",
                "Carol <carol@example.com>",
                Some("Dave <dave@example.com>"),
                "Wed, 07 Jan 2026 12:00:00 +0000",
            ),
        );
        ingest(
            &fx,
            "archive",
            1,
            &email(
                "Erin <erin@example.com>",
                "me@example.com",
                None,
                "Thu, 08 Jan 2026 12:00:00 +0000",
            ),
        );

        let index = build_index_from_store(&fx.store, &account()).unwrap();

        assert_eq!(index.contacts.get("carol@example.com").unwrap().sent_to, 1);
        assert_eq!(index.contacts.get("dave@example.com").unwrap().sent_cc, 1);
        assert_eq!(index.contacts.get("erin@example.com").unwrap().received, 1);
    }

    /// A store with no rows for this account builds an empty index rather than
    /// failing; the caller's guard decides what that means.
    #[test]
    fn an_empty_store_builds_an_empty_index() {
        let fx = fixture();
        let index = build_index_from_store(&fx.store, &account()).unwrap();
        assert!(index.contacts.is_empty());
        assert_eq!(index.account, "alice");
    }

    /// Roles map verbatim for the three ranked mailboxes; a slugified server
    /// name folds into `extra`, as the per-directory walk did.
    #[test]
    fn mailbox_role_folds_unmapped_mailboxes_into_extra() {
        assert_eq!(mailbox_role("sent"), "sent");
        assert_eq!(mailbox_role("inbox"), "inbox");
        assert_eq!(mailbox_role("archive"), "archive");
        assert_eq!(mailbox_role("some-folder"), "extra");
    }

    #[test]
    fn observe_bumps_sent_to_counter() {
        let mut index = empty_index();
        observe(
            &mut index,
            "me@example.com",
            &[(ObservedIn::SentTo, "Alice <alice@example.com>")],
            "2026-04-08T00:00:00Z",
        )
        .unwrap();

        let c: &Contact = index
            .contacts
            .get("alice@example.com")
            .expect("alice added");
        assert_eq!(c.sent_to, 1);
        assert_eq!(c.sent_cc, 0);
        assert_eq!(c.received, 0);
        assert_eq!(c.display_name, "Alice");
    }

    #[test]
    fn observe_skips_self_address() {
        let mut index = empty_index();
        observe(
            &mut index,
            "me@example.com",
            &[(ObservedIn::SentTo, "me@example.com, bob@example.com")],
            "2026-04-08T00:00:00Z",
        )
        .unwrap();

        assert!(!index.contacts.contains_key("me@example.com"));
        assert!(index.contacts.contains_key("bob@example.com"));
    }

    #[test]
    fn observe_skips_noreply() {
        let mut index = empty_index();
        observe(
            &mut index,
            "me@example.com",
            &[(ObservedIn::Inbox, "no-reply@example.com")],
            "2026-04-08T00:00:00Z",
        )
        .unwrap();

        assert!(index.contacts.is_empty());
    }

    #[test]
    fn observe_accumulates_counts_across_calls() {
        let mut index = empty_index();
        for _ in 0..3 {
            observe(
                &mut index,
                "me@example.com",
                &[(ObservedIn::SentTo, "alice@example.com")],
                "2026-04-08T00:00:00Z",
            )
            .unwrap();
        }
        assert_eq!(index.contacts.get("alice@example.com").unwrap().sent_to, 3);
    }

    #[test]
    fn observe_updates_last_seen_with_newer_timestamp() {
        let mut index = empty_index();
        observe(
            &mut index,
            "me@example.com",
            &[(ObservedIn::SentTo, "Old Name <alice@example.com>")],
            "2025-01-01T00:00:00Z",
        )
        .unwrap();
        observe(
            &mut index,
            "me@example.com",
            &[(ObservedIn::SentTo, "New Name <alice@example.com>")],
            "2026-04-08T00:00:00Z",
        )
        .unwrap();

        let c = index.contacts.get("alice@example.com").unwrap();
        assert_eq!(c.display_name, "New Name");
        assert_eq!(c.sent_to, 2);
    }

    #[test]
    fn observe_handles_multi_recipient_header() {
        let mut index = empty_index();
        observe(
            &mut index,
            "me@example.com",
            &[(
                ObservedIn::SentTo,
                "\"A User\" <a@x.com>, B User <b@x.com>, c@x.com",
            )],
            "2026-04-08T00:00:00Z",
        )
        .unwrap();

        assert_eq!(index.contacts.len(), 3);
        assert_eq!(
            index.contacts.get("a@x.com").unwrap().display_name,
            "A User"
        );
        assert_eq!(
            index.contacts.get("b@x.com").unwrap().display_name,
            "B User"
        );
    }
}
