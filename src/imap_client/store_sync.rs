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
//!   pruned from the fetch's own enumeration of the mailbox, see
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
use crate::types::MailboxRole;

/// A mailbox to sync: the configured role and the name on the server.
///
/// The local directory and `.md` status the old struct carried are gone: the
/// ingest path has no filesystem destination. The role is a [`MailboxRole`]
/// rather than a bare string, so `--mailbox INBOX` and the configured inbox
/// are the same target and file their rows under one key (#0064).
#[derive(Debug, Clone)]
pub struct SyncTarget {
    pub role: MailboxRole,
    pub server_name: String,
}

/// One fresh address observation captured from a newly-ingested message.
/// Consumed by the contacts-index hook after a successful sync.
#[derive(Debug, Clone)]
pub struct FreshObservation {
    /// The mailbox the message was ingested into.
    pub role: MailboxRole,
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
    /// Rows whose flags were updated from the server: read, answered or
    /// forwarded (#TKT-0051).
    pub flags_updated: usize,
    /// Rows deleted because the server no longer lists their UID (a message
    /// archived, moved or deleted in another client).
    pub pruned: usize,
    /// Rows this pass found vanished but did not delete, because some mailbox
    /// it touched came back short and the diff cannot be trusted until one
    /// does not (see [`crate::ingest::pass_may_prune`]).
    pub prunes_deferred: usize,
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

    let mut result = SyncResult::default();
    // Every prune this run will apply, collected here and applied after the
    // loop: see the second pass below for why it cannot run per target.
    let mut prunes: Vec<(MailboxRole, Vec<u32>)> = Vec::new();
    // `(enumeration complete, download short)` per target, which decides
    // whether the prunes above may be applied at all (#0072). Mirrors the
    // Graph backend; the shared gate is `ingest::pass_may_prune`.
    let mut coverage: Vec<(bool, bool)> = Vec::with_capacity(targets.len());

    // Phase 1: read the store's skip list for every target, serially. These
    // are single-reader queries and cheap; holding them all in hand lets the
    // network fetch below run without touching the store (single-writer
    // discipline: nothing concurrent reads or writes SQLite).
    //
    // The skip list travels with the UIDVALIDITY it was recorded under, so the
    // fetch can throw it away when the server has renumbered; carrying it
    // across a reset would skip bodies that were never downloaded.
    let mut knowns = Vec::with_capacity(targets.len());
    for target in targets {
        knowns.push(ingest::known_uids_with_cursor(
            &store,
            account_name,
            target.role.as_str(),
        )?);
    }

    // Phase 2: fetch every mailbox in parallel, one IMAP session each (#0005).
    // IMAP allows one SELECTed mailbox per connection, so the old single
    // session paid `N * latency` for N mailboxes; N connections overlap that
    // latency. `buffered` caps how many run at once (servers throttle) and,
    // crucially, yields results in target order however the fetches finish, so
    // the ordered ingest below is unaffected by completion order.
    let concurrency = imap_config.fetch_concurrency.clamp(1, 8);
    let fetched_results: Vec<Result<super::fetch::StoreFetch>> = {
        use futures::stream::StreamExt;
        futures::stream::iter(targets.iter().zip(knowns).map(|(target, known)| async move {
            let mut session = open_imap_session(imap_config).await?;
            let out =
                fetch_new_raw_on_session(&mut session, &target.server_name, Some(limit), known)
                    .await;
            session.logout().await.ok();
            out
        }))
        .buffered(concurrency)
        .collect()
        .await
    };
    span.mark("fetch");

    // Phase 3: ingest serially, in target order. Every store write happens
    // here on one thread, so per-mailbox transaction boundaries cannot
    // interleave and the prune ordering (inbox before archive before sent)
    // holds exactly as it did on the single-session path.
    for (target, fetched) in targets.iter().zip(fetched_results) {
        let fetched = match fetched {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "Failed to sync mailbox '{}': {}. Continuing with next.",
                    target.server_name, e
                );
                // A target that did not sync at all is the strongest form of
                // partial pass: the copy that would justify another target's
                // deletion may be exactly what this fetch failed to bring in.
                coverage.push((false, false));
                continue;
            }
        };
        let new_messages = fetched.messages;
        let state = fetched.state;
        let pending_arrival_mark = fetched.pending_arrival_mark;
        result.skipped += fetched.skipped;
        if fetched.uidvalidity_reset {
            result.uidvalidity_resets += 1;
        }

        if dry_run {
            coverage.push((fetched.enumeration_complete, fetched.download_incomplete));
            result.saved += new_messages.len();
            continue;
        }

        // A message that was downloaded but not written is as absent from the
        // store as one that was never fetched, so it counts against this
        // target's coverage too.
        let mut ingest_failed = false;
        for message in &new_messages {
            let Some(mut email) = parse_rfc822_to_fetched_email(&message.raw) else {
                warn!(
                    "Skipping UID {} in '{}': the message did not parse",
                    message.uid, target.server_name
                );
                ingest_failed = true;
                continue;
            };
            email.flags = message.flags;

            let outcome = ingest::ingest_message(
                &store,
                &blobs,
                &IngestInput {
                    account: account_name,
                    mailbox: target.role.as_str(),
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
                    if outcome.inserted && target.role.is_inbox() {
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
                Err(e) => {
                    ingest_failed = true;
                    warn!(
                        "Failed to ingest UID {} from '{}': {:#}",
                        message.uid, target.server_name, e
                    );
                }
            }
        }
        coverage.push((
            fetched.enumeration_complete,
            fetched.download_incomplete || ingest_failed,
        ));

        // The IMAP server states the whole flag set, so it is truth for all
        // three bits of the second axis (#TKT-0051), not just for `\Seen`.
        result.flags_updated += ingest::apply_flags(
            &store,
            account_name,
            target.role.as_str(),
            fetched
                .known_flags
                .into_iter()
                .map(|(uid, flags)| (uid as i64, flags)),
        );

        // The other half of the same diff: the UIDs the store holds for this
        // mailbox that the server did not list. Held back until every target
        // has been ingested (see the second pass below).
        if !fetched.vanished.is_empty() {
            prunes.push((target.role.clone(), fetched.vanished));
        }

        let highest_uid = new_messages.iter().map(|m| m.uid as i64).max();
        ingest::record_mailbox_cursor(
            &store,
            account_name,
            target.role.as_str(),
            &MailboxCursor {
                uidvalidity: state.uid_validity.map(|v| v as i64),
                last_uid: highest_uid.or_else(|| state.uid_next.map(|n| n as i64 - 1)),
                uidnext: state.uid_next.map(|v| v as i64),
                exists: Some(state.exists as i64),
                highest_modseq: None,
                deltalink: None,
                // What this pass owes the next one: the mark below which the
                // gate must stay shut because an arrival the server lists is
                // still not in the store. Written even when it is None, which
                // is how a pass that caught up reopens the gate (#0072).
                arrival_mark: pending_arrival_mark.map(|m| m as i64),
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
    //
    // Whether they run at all is the coverage gate (#0072): one mailbox that
    // came back short invalidates every target's diff, because the argument
    // that lets an inbox row go is that another target ingested the copy the
    // message moved to.
    if ingest::pass_may_prune(&coverage) {
        let now = crate::outbox::unix_now();
        for (role, vanished) in &prunes {
            // The age guard is the other half: a row this client has just
            // written locally (a Sent copy the server has not filed yet) is in
            // every vanished set until the server's own copy shows up.
            let vanished: Vec<i64> = vanished.iter().map(|&uid| uid as i64).collect();
            let prunable =
                ingest::prunable_uids(&store, account_name, role.as_str(), &vanished, now);
            result.pruned +=
                ingest::prune_vanished(&store, &blobs, account_name, role.as_str(), &prunable);
        }
    } else {
        result.prunes_deferred = prunes.iter().map(|(_, v)| v.len()).sum();
        if result.prunes_deferred > 0 {
            info!(
                "IMAP sync: {} pending prune(s) deferred; this pass did not see every message",
                result.prunes_deferred,
            );
        }
    }
    span.mark("prune");

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

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A future that returns `Poll::Pending` `n` times before resolving to its
    /// value, rescheduling itself each time. Used to make the parallel fetches
    /// finish in a deliberately scrambled order.
    struct ReadyAfter {
        remaining: usize,
        value: usize,
    }

    impl Future for ReadyAfter {
        type Output = usize;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
            if self.remaining == 0 {
                Poll::Ready(self.value)
            } else {
                self.remaining -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// The load-bearing property of the #0005 parallel fetch: `buffered` yields
    /// results in *input* (target) order regardless of which fetch finishes
    /// first, so the serial ingest phase still processes inbox before archive
    /// before sent, and the #0072 prune ordering is untouched. Swapping in
    /// `buffer_unordered` would return `4,3,2,1,0` here and break that.
    #[test]
    fn buffered_fetch_yields_in_target_order_however_the_fetches_finish() {
        use futures::stream::StreamExt;
        let out: Vec<usize> = futures::executor::block_on(async {
            futures::stream::iter((0..5usize).map(|i| ReadyAfter {
                // input 0 stalls longest, input 4 finishes first
                remaining: (5 - i) * 2,
                value: i,
            }))
            .buffered(5)
            .collect()
            .await
        });
        assert_eq!(out, vec![0, 1, 2, 3, 4]);
    }
}
