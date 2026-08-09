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
use log::info;

use super::fetch::fetch_new_raw_on_session;
use super::open_imap_session;
use crate::config::ImapConfig;
use crate::ingest::KnownUids;
use crate::store::{BlobStore, Store};
use crate::sync::engine::{run_sync, SyncRun};
use crate::sync::{MailboxFetch, SyncBackend, SyncResult, SyncTarget};
use crate::timing::TimingSpan;

/// The IMAP transport behind [`crate::sync::SyncBackend`] (#0059).
///
/// It owns what a fetch needs and nothing the engine owns: no store handle, no
/// cursors, no ingest bookkeeping. Today that is only the config, because every
/// pass opens fresh sessions and drops them; the struct exists so #0041 has
/// somewhere to keep a session and its `HIGHESTMODSEQ` across mailboxes and
/// across passes without touching the engine.
pub struct ImapBackend<'a> {
    config: &'a ImapConfig,
}

impl<'a> ImapBackend<'a> {
    pub fn new(config: &'a ImapConfig) -> Self {
        Self { config }
    }
}

impl SyncBackend for ImapBackend<'_> {
    /// Fetch every mailbox in parallel, one IMAP session each (#0005).
    ///
    /// IMAP allows one SELECTed mailbox per connection, so the old single
    /// session paid `N * latency` for N mailboxes; N connections overlap that
    /// latency. `buffered` caps how many run at once (servers throttle) and,
    /// crucially, yields results in target order however the fetches finish, so
    /// the engine's ordered ingest is unaffected by completion order. Swapping
    /// in `buffer_unordered` would break the #0072 prune ordering; the test
    /// below pins that property.
    async fn fetch_targets(
        &mut self,
        targets: &[SyncTarget],
        limit: usize,
        knowns: Vec<KnownUids>,
    ) -> Vec<Result<MailboxFetch>> {
        use futures::stream::StreamExt;
        let concurrency = self.config.fetch_concurrency.clamp(1, 8);
        let config = self.config;
        futures::stream::iter(targets.iter().zip(knowns).map(|(target, known)| async move {
            let mut session = open_imap_session(config).await?;
            let out =
                fetch_new_raw_on_session(&mut session, &target.server_name, Some(limit), known)
                    .await;
            session.logout().await.ok();
            out
        }))
        .buffered(concurrency)
        .collect()
        .await
    }
}

/// Fetch every target mailbox and ingest what comes back.
///
/// Since #0059 this is the wiring only: it opens the store, builds the IMAP
/// backend and hands both to [`crate::sync::run_sync`], which owns the
/// orchestration (ingest, arrival marks, flags, cursors, the deferred prune
/// pass) and is tested offline against a fake backend.
///
/// `dry_run` fetches and counts what *would* be ingested without touching the
/// store or the blob directory.
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
    let mut backend = ImapBackend::new(imap_config);

    run_sync(
        &mut backend,
        &SyncRun {
            store: &store,
            blobs: &blobs,
            account: account_name,
            targets,
            limit,
            dry_run,
        },
        &mut span,
    )
    .await
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

    // The orchestration tests this module used to hold moved to
    // `crate::sync::engine` with the loop itself (#0059), where they now run
    // through the real code path against a fake backend instead of re-walking
    // its calls by hand. What stays here is the one property that belongs to
    // *this* transport: the fetch's completion order.

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

    /// The load-bearing property of the #0005 parallel fetch, and the contract
    /// [`super::ImapBackend`] owes [`crate::sync::SyncBackend`]: `buffered`
    /// yields results in *input* (target) order regardless of which fetch
    /// finishes first, so the engine's serial ingest still processes inbox
    /// before archive before sent, and the #0072 prune ordering is untouched.
    /// Swapping in `buffer_unordered` would return `4,3,2,1,0` here and break
    /// that.
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
