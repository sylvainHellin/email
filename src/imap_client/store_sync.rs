//! IMAP sync, store-only.
//!
//! Replaces the `.md`-writing orchestrator: one IMAP session per sync, and
//! every message that comes back goes through [`crate::ingest`] into the
//! per-account `store.sqlite3` plus its blob store. Nothing is written to the
//! mailbox directories.
//!
//! Three things the old path did are gone with it, because the store makes
//! them unnecessary rather than merely cheaper:
//!
//! - the local Message-ID scan that decided what was new (identity is the UID,
//!   and the store answers "which UIDs do I hold" with one query);
//! - the dedup pass over the mailbox directory (the unique constraint on
//!   `(account, mailbox, uid)` makes a duplicate impossible);
//! - the EXISTS/UIDNEXT reconciliation heuristic and its `mailbox-states.json`
//!   cache (superseded by `sync_cursors`; a row the server no longer lists is
//!   now pruned from the fetch's own window, see
//!   [`crate::imap_client::vanished_uids`]).

use anyhow::{anyhow, Result};
use log::{info, warn};

use super::fetch::fetch_new_raw_on_session;
use super::open_imap_session;
use crate::config::ImapConfig;
use crate::ingest::{self, IngestInput, MailboxCursor};
use crate::parse::parse_rfc822_to_fetched_email;
use crate::store::{BlobStore, Store};
use crate::timing::TimingSpan;

/// A mailbox to sync: the configured role and the name on the server.
///
/// The local directory and `.md` status the old struct carried are gone: the
/// ingest path has no filesystem destination.
#[derive(Debug, Clone)]
pub struct SyncTarget {
    pub role: String,
    pub server_name: String,
}

/// One fresh address observation captured from a newly-ingested message.
/// Consumed by the contacts-index hook after a successful sync.
#[derive(Debug, Clone)]
pub struct FreshObservation {
    /// Mailbox role: "inbox", "archive", "sent", or "extra".
    pub role: String,
    pub from: String,
    pub to: String,
    pub cc: Option<String>,
    /// RFC-2822 date header from the email, or empty if unavailable.
    pub date: String,
}

/// Results from a sync run.
#[derive(Debug, Default)]
pub struct SyncResult {
    /// Messages ingested as new rows.
    pub saved: usize,
    /// Messages the store already held (pass 1 hits).
    pub skipped: usize,
    /// Rows whose `\Seen` flag was updated from the server.
    pub read_updated: usize,
    /// Rows deleted because the server no longer lists their UID inside the
    /// window the fetch covered (a message archived, moved or deleted in
    /// another client).
    pub pruned: usize,
    /// Rows rebound to a new UID after a UIDVALIDITY reset.
    pub uid_rebound: usize,
    /// Mailboxes whose server-side UIDVALIDITY no longer matched the stored
    /// cursor, and were therefore refetched in full.
    pub uidvalidity_resets: usize,
    /// Address observations from newly-ingested messages, ready to be merged
    /// into the contacts index by the caller. Empty on `dry_run`.
    pub fresh_observations: Vec<FreshObservation>,
    /// Sender + subject of every genuinely new inbox message (#0009).
    pub new_inbox_mail: Vec<crate::notify::NewMailMeta>,
}

/// Fetch every target mailbox on one session and ingest what comes back.
///
/// `dry_run` opens the session and counts what *would* be ingested without
/// touching the store or the blob directory.
pub async fn sync_mailboxes(
    imap_config: &ImapConfig,
    account_name: &str,
    targets: &[SyncTarget],
    limit: usize,
    dry_run: bool,
) -> Result<SyncResult> {
    info!(
        "sync_mailboxes: account={account_name}, {} targets, limit={limit}, dry_run={dry_run}",
        targets.len(),
    );
    let span_label = if limit < usize::MAX {
        "sync_mailboxes:quick"
    } else {
        "sync_mailboxes:full"
    };
    let mut span = TimingSpan::with_context(span_label, format!("{} targets", targets.len()));

    let store = Store::open_account(account_name)?;
    let blobs = BlobStore::for_account(account_name);
    let mut session = open_imap_session(imap_config).await?;
    span.mark("session_open");

    let mut result = SyncResult::default();
    // Every prune this run will apply, collected here and applied after the
    // loop: see the second pass below for why it cannot run per target.
    let mut prunes: Vec<(String, Vec<u32>)> = Vec::new();

    for target in targets {
        // The skip list travels with the UIDVALIDITY it was recorded under, so
        // the fetch can throw it away when the server has renumbered; carrying
        // it across a reset would skip bodies that were never downloaded.
        let known = ingest::known_uids_with_cursor(&store, account_name, &target.role)?;
        let fetched =
            fetch_new_raw_on_session(&mut session, &target.server_name, Some(limit), known).await;

        let fetched = match fetched {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "Failed to sync mailbox '{}': {}. Continuing with next.",
                    target.server_name, e
                );
                continue;
            }
        };
        let new_messages = fetched.messages;
        let state = fetched.state;
        result.skipped += fetched.skipped;
        if fetched.uidvalidity_reset {
            result.uidvalidity_resets += 1;
        }

        if dry_run {
            result.saved += new_messages.len();
            continue;
        }

        for message in &new_messages {
            let Some(mut email) = parse_rfc822_to_fetched_email(&message.raw) else {
                warn!(
                    "Skipping UID {} in '{}': the message did not parse",
                    message.uid, target.server_name
                );
                continue;
            };
            email.is_read = message.is_read;

            let outcome = ingest::ingest_message(
                &store,
                &blobs,
                &IngestInput {
                    account: account_name,
                    mailbox: &target.role,
                    uid: message.uid as i64,
                    email: &email,
                    raw: Some(&message.raw),
                },
            );
            match outcome {
                Ok(outcome) => {
                    if outcome.inserted {
                        result.saved += 1;
                    }
                    if outcome.uid_rebound {
                        result.uid_rebound += 1;
                    }
                    if outcome.inserted && target.role.eq_ignore_ascii_case("inbox") {
                        result.new_inbox_mail.push(crate::notify::NewMailMeta::new(
                            &email.from,
                            &email.subject,
                        ));
                    }
                    result.fresh_observations.push(FreshObservation {
                        role: target.role.clone(),
                        from: email.from.clone(),
                        to: email.to.clone(),
                        cc: email.cc.clone(),
                        date: email.date.clone(),
                    });
                }
                Err(e) => warn!(
                    "Failed to ingest UID {} from '{}': {:#}",
                    message.uid, target.server_name, e
                ),
            }
        }

        for (uid, is_read) in fetched.known_flags {
            match ingest::apply_seen_flag(&store, account_name, &target.role, uid as i64, is_read) {
                Ok(true) => result.read_updated += 1,
                Ok(false) => {}
                Err(e) => warn!("Failed to apply the read flag for UID {uid}: {e:#}"),
            }
        }

        // The other half of the same diff: the UIDs the store holds inside
        // the window's range that the server did not list. Held back until
        // every target has been ingested (see the second pass below).
        if !fetched.vanished.is_empty() {
            prunes.push((target.role.clone(), fetched.vanished));
        }

        let highest_uid = new_messages.iter().map(|m| m.uid as i64).max();
        ingest::record_mailbox_cursor(
            &store,
            account_name,
            &target.role,
            &MailboxCursor {
                uidvalidity: state.uid_validity.map(|v| v as i64),
                last_uid: highest_uid.or_else(|| state.uid_next.map(|n| n as i64 - 1)),
                uidnext: state.uid_next.map(|v| v as i64),
                exists: Some(state.exists as i64),
                highest_modseq: None,
                deltalink: None,
            },
        )?;
    }

    span.mark("ingest");

    // Second pass: every prune, after every target has been ingested.
    //
    // Targets are synced in order (inbox, archive, sent), so pruning inside
    // the loop deletes the inbox row of a message archived in another client
    // *before* the archive pass ingests it: a window in which the store holds
    // no row for that message at all, its blobs drop to refcount zero and are
    // unlinked, and a failed archive fetch (the `continue` above) loses it
    // locally until a later sync. Applying the prunes here means the
    // destination row already exists when the source row goes.
    for (role, vanished) in &prunes {
        result.pruned += ingest::prune_vanished(&store, &blobs, account_name, role, vanished);
    }
    span.mark("prune");

    session.logout().await.ok();
    span.mark("logout");

    Ok(result)
}

// ---------------------------------------------------------------------------
// list_mailboxes
// ---------------------------------------------------------------------------

pub async fn list_mailboxes(imap_config: &ImapConfig) -> Result<Vec<String>> {
    use futures::TryStreamExt;

    let mut session = open_imap_session(imap_config).await?;

    let mailboxes: Vec<_> = session
        .list(None, Some("*"))
        .await
        .map_err(|e| anyhow!("Failed to list mailboxes: {}", e))?
        .try_collect()
        .await
        .map_err(|e| anyhow!("Failed to collect mailboxes: {}", e))?;

    let names: Vec<String> = mailboxes.iter().map(|m| m.name().to_string()).collect();

    session.logout().await.ok();
    Ok(names)
}
