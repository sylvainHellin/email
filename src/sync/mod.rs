//! The transport-independent half of the receive path: the sync types every
//! backend speaks, and the [`SyncBackend`] seam the engine is driven through.
//!
//! Before #0059 these types lived in `imap_client`, and `graph.rs` imported
//! them from there: the abstraction existed, spelled as a dependency on the
//! other transport's module. They live here now, and the orchestration that
//! consumes them lives in [`engine`], so it can be driven offline by a fake
//! backend in tests (`engine`'s test module) instead of only against a server.
//!
//! What the seam is *for*, beyond the tests:
//!
//! - [`#0041`] wants one IMAP session held across mailboxes and a CONDSTORE
//!   `HIGHESTMODSEQ` remembered between passes. Both are backend state, and
//!   the trait takes `&mut self` so a backend may keep them.
//! - [`#0042`] wants Graph's `deltaLink` remembered between passes, which is
//!   the same shape of state behind the same `&mut self`.
//!
//! Neither is implemented here. The parity half of #0059, one orchestration
//! serving both transports, is parked with the Graph backend: `graph.rs` still
//! runs its own loop, and the engine below is driven by the IMAP backend only.
//!
//! [`#0041`]: https://github.com/sylvainhellin/mailypoppins/blob/main/docs/tickets/0041-persistent-conn-condstore.md
//! [`#0042`]: https://github.com/sylvainhellin/mailypoppins/blob/main/docs/tickets/0042-graph-delta-sync.md

pub mod engine;

use anyhow::Result;

use crate::ingest::KnownUids;
use crate::types::{MailboxRole, MessageFlags};

pub use engine::run_sync;

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

/// One message downloaded for ingest, with the identity the store keys on.
pub struct FetchedRaw {
    pub uid: u32,
    pub raw: Vec<u8>,
    /// What the server says has happened to it: read, answered, forwarded.
    pub flags: MessageFlags,
}

/// What the SELECT response said about the mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxState {
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub exists: u32,
}

/// What one backend fetch of one mailbox brought back: everything the engine
/// needs to ingest, re-flag, prune and record a cursor, and nothing about how
/// it was obtained.
pub struct MailboxFetch {
    /// Messages the store does not hold yet, with their bodies.
    pub messages: Vec<FetchedRaw>,
    /// How many UIDs in the window the store already held.
    pub skipped: usize,
    /// The flags of those already-held UIDs, the only server-to-local channel
    /// for the second status axis (#0004, #TKT-0051): read, answered and
    /// forwarded all arrive here and nowhere else.
    pub known_flags: Vec<(u32, MessageFlags)>,
    /// What SELECT said about the mailbox.
    pub state: MailboxState,
    /// UIDs the store holds for this mailbox that the server no longer lists.
    /// See [`crate::imap_client::vanished_uids`].
    pub vanished: Vec<u32>,
    /// True when the server's UIDVALIDITY no longer matches the stored one, so
    /// this fetch deliberately skipped nothing and redownloaded the window.
    pub uidvalidity_reset: bool,
    /// True when the enumeration [`vanished`] was computed from is the whole
    /// mailbox rather than a short answer.
    ///
    /// [`vanished`]: MailboxFetch::vanished
    pub enumeration_complete: bool,
    /// True when this pass did not ingest every message that arrived in the
    /// mailbox since the store last saw it: the `limit` window cut some off,
    /// a body did not come back, or the caller failed to ingest one. Backlog
    /// *older* than what the store already holds does not count.
    pub download_incomplete: bool,
    /// The arrival mark to persist for this mailbox: `Some(mark)` while an
    /// arrival above it is still missing, `None` once every arrival is in.
    /// Carried back in by the next fetch so the gate cannot open on a mark that
    /// this pass's own ingest raised (#0072).
    pub pending_arrival_mark: Option<u32>,
    /// The CONDSTORE `HIGHESTMODSEQ` to record as the next pass's resume point,
    /// or `None` for "this fetch cannot vouch for one", which the cursor UPSERT
    /// reads as "keep whatever is stored" (#0041).
    ///
    /// `None` is the answer for every backend and every path without CONDSTORE,
    /// and for a CONDSTORE pass whose window did not cover the whole mailbox:
    /// a modseq claims every flag in the mailbox was correct as of that point,
    /// and only a pass that looked at all of them may claim it.
    pub highest_modseq: Option<i64>,
}

/// The per-transport half of a sync: fetch the window of every target.
///
/// Everything else, ingest, arrival marks, ingest-failure bookkeeping, flags,
/// cursors and the deferred prune pass, is the engine's ([`run_sync`]), written
/// once and tested against a fake implementation of this trait.
///
/// The trait is deliberately one method wide. It is not the "list folders,
/// move, delete, set read" surface the ticket sketched: those ops already have
/// their own seam in [`crate::ops::run_op`], and widening this one would be
/// generality no consumer has asked for.
///
/// Two contracts a backend must honour:
///
/// - one result per target, in the order the targets were given, so the engine
///   can zip them and keep the #0072 prune ordering (inbox before archive
///   before sent) whatever order the fetches finished in;
/// - a per-target failure is an `Err` *element*, not an `Err` return: the
///   engine treats it as the strongest form of partial pass and keeps going,
///   whereas an `Err` return fails the whole sync.
///
/// `&mut self` is where a backend keeps what outlives one mailbox: a persistent
/// session and its `HIGHESTMODSEQ` (#0041), a `deltaLink` (#0042). Today's
/// [`crate::imap_client::ImapBackend`] keeps only its config.
pub trait SyncBackend {
    /// Fetch every target's window. `knowns` is parallel to `targets`: the
    /// store's skip list for each, already read under the single-writer
    /// discipline before any network call.
    fn fetch_targets(
        &mut self,
        targets: &[SyncTarget],
        limit: usize,
        knowns: Vec<KnownUids>,
    ) -> impl std::future::Future<Output = Vec<Result<MailboxFetch>>>;
}
