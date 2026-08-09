//! The sync orchestration, written once against [`SyncBackend`] (#0059).
//!
//! Everything here used to live inside `imap_client::store_sync::sync_mailboxes`
//! between the network calls, which is why none of it had a test: driving it
//! meant standing up an IMAP server. The transport is now a trait parameter, so
//! the loop below runs offline against a fake backend and the properties it is
//! responsible for, the arrival mark, the ingest-failure bound, the flag
//! application, the cursor, and the deferred prune pass, are pinned through the
//! real code path rather than re-walked by hand in a composition test.
//!
//! What it is *not*: a Graph orchestrator. `graph.rs` still runs its own loop
//! (the parity half of #0059 is parked with the Graph backend), and its #0074
//! bookkeeping mirrors the one here by hand as it did before.

use anyhow::Result;
use log::{info, warn};

use super::{FreshObservation, MailboxFetch, SyncBackend, SyncResult, SyncTarget};
use crate::ingest::{self, IngestInput, MailboxCursor};
use crate::parse::parse_rfc822_to_fetched_email;
use crate::store::{BlobStore, Store};
use crate::timing::TimingSpan;
use crate::types::MailboxRole;

/// Everything one sync pass needs that is not the transport: where to write,
/// what to sync and how much of it.
///
/// A struct rather than six parameters so [`run_sync`] stays readable and the
/// caller cannot swap `account` for `limit` silently.
pub struct SyncRun<'a> {
    pub store: &'a Store,
    pub blobs: &'a BlobStore,
    pub account: &'a str,
    pub targets: &'a [SyncTarget],
    /// How many of the newest UIDs per mailbox the pass may download.
    pub limit: usize,
    /// Count what would be ingested without touching the store or the blobs.
    pub dry_run: bool,
}

/// The IMAP half of [`ingest::note_ingest_failure`]: the same give-up bound,
/// over this path's `u32` UIDs. `false` means the arrival mark no longer has to
/// stay below the UID (#0074).
fn note_ingest_failure(
    store: &Store,
    account: &str,
    mailbox: &str,
    server_name: &str,
    uid: u32,
    error: &str,
) -> bool {
    ingest::note_ingest_failure(store, account, mailbox, server_name, uid as i64, error)
}

/// The arrival mark a pass must persist once ingest is done, given the mark the
/// download reported and the UIDs the ingest failed to write (#0074).
///
/// The backend's own coverage measures what the pass *downloaded*, which is one
/// step short of what it owes the next pass: a message fetched and then not
/// written is as absent from the store as one never fetched, yet it reads as
/// covered, the pass reports itself complete, persists no mark, and the next
/// pass stands on a floor above a message the server still lists. The mark is
/// therefore lowered here to just under the lowest unwritten UID, which is what
/// makes that message an arrival again next pass and keeps the gate shut until
/// some pass writes it.
///
/// `unmet` holds only the failures still worth retrying;
/// [`ingest::note_ingest_failure`] drops a UID out of it once it has failed
/// [`ingest::MAX_INGEST_ATTEMPTS`] times, so a message the store rejects
/// deterministically cannot hold the mark down for good.
///
/// Saturating at 0 rather than wrapping: a UID of 1 that will not ingest leaves
/// a mark of 0, meaning every listed UID is an arrival, which is the correct
/// reading when the very bottom of the mailbox is missing.
pub(crate) fn mark_below_unmet(pending: Option<u32>, unmet: &[u32]) -> Option<u32> {
    let Some(lowest) = unmet.iter().copied().min() else {
        return pending;
    };
    let owed = lowest.saturating_sub(1);
    Some(pending.map_or(owed, |mark| mark.min(owed)))
}

/// Drive one sync pass: read the skip lists, hand them to the backend, ingest
/// what comes back in target order, then apply the prunes.
///
/// The phases and their order are load-bearing and unchanged from the
/// pre-#0059 IMAP orchestrator:
///
/// 1. every target's skip list is read from the store serially, before any
///    network call, so the fetch never races the single SQLite writer;
/// 2. the backend fetches, in whatever order it likes, and hands the results
///    back in target order;
/// 3. ingest runs serially in target order, so per-mailbox transactions cannot
///    interleave and the prune ordering below holds;
/// 4. every prune runs after every target has been ingested, gated on the whole
///    pass's coverage.
pub async fn run_sync(
    backend: &mut impl SyncBackend,
    run: &SyncRun<'_>,
    span: &mut TimingSpan,
) -> Result<SyncResult> {
    let SyncRun { store, blobs, account, targets, limit, dry_run } = *run;

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
        knowns.push(ingest::known_uids_with_cursor(store, account, target.role.as_str())?);
    }

    // Phase 2: the transport. One result per target, in target order, whatever
    // order the backend actually fetched them in (see [`SyncBackend`]).
    let fetched_results = backend.fetch_targets(targets, limit, knowns).await;
    span.mark("fetch");

    // Phase 3: ingest serially, in target order.
    for (target, fetched) in targets.iter().zip(fetched_results) {
        let fetched: MailboxFetch = match fetched {
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
            // The retry counters are keyed by UID, and the server has just
            // renumbered them: every recorded attempt now points at a message
            // that no longer holds that UID, so it is dropped with the mark and
            // the skip list the refetch already discards (#0074 review).
            ingest::clear_mailbox_ingest_failures(store, account, target.role.as_str());
            // The CONDSTORE resume point goes with them, and has to go from
            // here: the cursor UPSERT carries a modseq forward precisely so an
            // ordinary full-window pass cannot erase it (#0041), which leaves
            // this as the one path that may. A modseq recorded under the old
            // UIDVALIDITY describes a mailbox that no longer exists.
            ingest::clear_mailbox_modseq(store, account, target.role.as_str());
        }

        if dry_run {
            coverage.push((fetched.enumeration_complete, fetched.download_incomplete));
            result.saved += new_messages.len();
            continue;
        }

        // A message that was downloaded but not written is as absent from the
        // store as one that was never fetched, so it counts against this
        // target's coverage too.
        //
        // The failed UIDs are collected rather than reduced to a flag, because
        // the pass owes the next one a mark below them (#0074): a flag lives
        // for this pass only, and the next pass would stand on a floor above
        // the message this one downloaded and dropped. `note_ingest_failure`
        // is what keeps that from being permanent for a message the store
        // rejects every time; a UID it has given up on is left out of `unmet`,
        // so it neither lowers the mark nor reports the pass short.
        //
        // One poisoned message never stops the batch: every failure `continue`s
        // to the next message, so the rest of the window is ingested normally
        // and only the prune is held back.
        let mut unmet: Vec<u32> = Vec::new();
        let mut note_failure = |uid: u32, error: &str| {
            if note_ingest_failure(store, account, target.role.as_str(), &target.server_name, uid, error)
            {
                unmet.push(uid);
            }
        };
        for message in &new_messages {
            let Some(mut email) = parse_rfc822_to_fetched_email(&message.raw) else {
                warn!(
                    "Skipping UID {} in '{}': the message did not parse",
                    message.uid, target.server_name
                );
                note_failure(message.uid, "the message did not parse");
                continue;
            };
            email.flags = message.flags;

            let outcome = ingest::ingest_message(
                store,
                blobs,
                &IngestInput {
                    account,
                    mailbox: target.role.as_str(),
                    uid: message.uid as i64,
                    email: &email,
                    raw: Some(&message.raw),
                },
            );
            match outcome {
                Ok(outcome) => {
                    ingest::clear_ingest_failure(
                        store,
                        account,
                        target.role.as_str(),
                        message.uid as i64,
                    );
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
                    warn!(
                        "Failed to ingest UID {} from '{}': {:#}",
                        message.uid, target.server_name, e
                    );
                    note_failure(message.uid, &format!("{e:#}"));
                }
            }
        }
        coverage.push((
            fetched.enumeration_complete,
            fetched.download_incomplete || !unmet.is_empty(),
        ));
        let pending_arrival_mark = mark_below_unmet(pending_arrival_mark, &unmet);

        // The IMAP server states the whole flag set, so it is truth for all
        // three bits of the second axis (#TKT-0051), not just for `\Seen`.
        result.flags_updated += ingest::apply_flags(
            store,
            account,
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
            store,
            account,
            target.role.as_str(),
            &MailboxCursor {
                uidvalidity: state.uid_validity.map(|v| v as i64),
                last_uid: highest_uid.or_else(|| state.uid_next.map(|n| n as i64 - 1)),
                uidnext: state.uid_next.map(|v| v as i64),
                exists: Some(state.exists as i64),
                // `None` here means "this pass has nothing to say about the
                // modseq", not "clear it": the UPSERT COALESCEs, so a
                // full-window pass leaves a CONDSTORE pass's resume point
                // alone (#0041).
                highest_modseq: fetched.highest_modseq,
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
            let prunable = ingest::prunable_uids(store, account, role.as_str(), &vanished, now);
            result.pruned +=
                ingest::prune_vanished(store, blobs, account, role.as_str(), &prunable);
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::sync::{FetchedRaw, MailboxState};
    use crate::types::MessageFlags;

    // -----------------------------------------------------------------------
    // The fake backend (#0059)
    // -----------------------------------------------------------------------

    /// A [`SyncBackend`] that answers from a script instead of a server.
    ///
    /// One entry per pass per target, keyed by server name, popped in order, so
    /// a test can say "this is what the server hands back on pass 1, this on
    /// pass 2" and drive the real engine over both. A target with nothing left
    /// scripted gets an error result, which is the engine's "this mailbox did
    /// not sync at all" path.
    #[derive(Default)]
    struct FakeBackend {
        passes: HashMap<String, Vec<Result<MailboxFetch>>>,
        /// Every `(target, limit, skip-list size)` the engine asked for, in
        /// call order: what the engine handed the transport.
        seen: Vec<(String, usize, usize)>,
    }

    impl FakeBackend {
        fn script(&mut self, server_name: &str, fetches: Vec<Result<MailboxFetch>>) {
            self.passes.insert(server_name.to_string(), fetches);
        }
    }

    impl SyncBackend for FakeBackend {
        async fn fetch_targets(
            &mut self,
            targets: &[SyncTarget],
            limit: usize,
            knowns: Vec<crate::ingest::KnownUids>,
        ) -> Vec<Result<MailboxFetch>> {
            let mut out = Vec::with_capacity(targets.len());
            for (target, known) in targets.iter().zip(knowns) {
                self.seen.push((target.server_name.clone(), limit, known.uids.len()));
                let next = self
                    .passes
                    .get_mut(&target.server_name)
                    .and_then(|queue| {
                        if queue.is_empty() {
                            None
                        } else {
                            Some(queue.remove(0))
                        }
                    })
                    .unwrap_or_else(|| Err(anyhow::anyhow!("nothing scripted")));
                out.push(next);
            }
            out
        }
    }

    fn raw(name: &str) -> Vec<u8> {
        format!(
            "From: a@example.com\r\nTo: b@example.com\r\nSubject: {name}\r\n\
             Message-ID: <{name}@example.com>\r\nDate: Thu, 7 Aug 2025 10:00:00 +0000\r\n\r\n\
             {name} body\r\n"
        )
        .into_bytes()
    }

    /// Bytes the ingest path refuses: a header block that opens with a
    /// continuation line, which is the one shape `mailparse` (and so
    /// [`parse_rfc822_to_fetched_email`]) rejects outright. It stands in for
    /// every "downloaded and not written" message in these tests.
    fn unparsable() -> Vec<u8> {
        b" not a header\r\nFrom: a@example.com\r\n\r\nbody\r\n".to_vec()
    }

    /// A fetch that saw the whole mailbox and downloaded everything it owed:
    /// the shape that opens the prune gate.
    fn fetch(messages: Vec<(u32, Vec<u8>)>) -> MailboxFetch {
        MailboxFetch {
            messages: messages
                .into_iter()
                .map(|(uid, raw)| FetchedRaw { uid, raw, flags: MessageFlags::default() })
                .collect(),
            skipped: 0,
            known_flags: Vec::new(),
            state: MailboxState { uid_validity: Some(7), uid_next: Some(200), exists: 2 },
            vanished: Vec::new(),
            uidvalidity_reset: false,
            enumeration_complete: true,
            download_incomplete: false,
            pending_arrival_mark: None,
            // #0041 added this field; the fake backend is a non-CONDSTORE
            // server, which is what every existing engine test assumed and
            // still asserts.
            highest_modseq: None,
        }
    }

    fn targets() -> Vec<SyncTarget> {
        vec![
            SyncTarget { role: MailboxRole::Inbox, server_name: "INBOX".into() },
            SyncTarget { role: MailboxRole::Archive, server_name: "Archive".into() },
        ]
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        store: Store,
        blobs: BlobStore,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let store = Store::open(tmp.path().join("store.sqlite3")).unwrap();
            let blobs = BlobStore::new(tmp.path().join("blobs"));
            Self { _tmp: tmp, store, blobs }
        }

        fn run(&self, backend: &mut FakeBackend, targets: &[SyncTarget]) -> SyncResult {
            self.run_with(backend, targets, usize::MAX, false)
        }

        fn run_with(
            &self,
            backend: &mut FakeBackend,
            targets: &[SyncTarget],
            limit: usize,
            dry_run: bool,
        ) -> SyncResult {
            let run = SyncRun {
                store: &self.store,
                blobs: &self.blobs,
                account: "acct",
                targets,
                limit,
                dry_run,
            };
            let mut span = TimingSpan::new("test");
            futures::executor::block_on(run_sync(backend, &run, &mut span)).unwrap()
        }

        fn rows(&self, mailbox: &str) -> Vec<i64> {
            let conn = self.store.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT uid FROM messages WHERE account = 'acct' AND mailbox = ?1 \
                     ORDER BY uid",
                )
                .unwrap();
            let out = stmt
                .query_map([mailbox], |row| row.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            out
        }

        fn modseq(&self, mailbox: &str) -> Option<i64> {
            ingest::load_mailbox_cursor(&self.store, "acct", mailbox)
                .unwrap()
                .and_then(|c| c.highest_modseq)
        }

        fn cursor_mark(&self, mailbox: &str) -> Option<u32> {
            ingest::known_uids_with_cursor(&self.store, "acct", mailbox).unwrap().arrival_mark
        }
    }

    // -----------------------------------------------------------------------
    // Engine tests: the loop itself, driven by the fake backend
    // -----------------------------------------------------------------------

    /// The baseline: what the backend hands back is ingested, counted and
    /// cursored, and the next pass's skip list is what the first pass wrote.
    #[test]
    fn the_engine_ingests_what_the_backend_returns_and_advances_the_cursor() {
        let fx = Fixture::new();
        let targets = targets();
        let mut backend = FakeBackend::default();
        backend.script("INBOX", vec![Ok(fetch(vec![(101, raw("one")), (102, raw("two"))]))]);
        backend.script("Archive", vec![Ok(fetch(vec![(55, raw("old"))]))]);

        let result = fx.run(&mut backend, &targets);

        assert_eq!(result.saved, 3);
        assert_eq!(result.new_inbox_mail.len(), 2, "only inbox arrivals notify");
        assert_eq!(result.fresh_observations.len(), 3, "every ingest feeds the contacts hook");
        assert_eq!(fx.rows("inbox"), vec![101, 102]);
        assert_eq!(fx.rows("archive"), vec![55]);
        // The cursor is the highest UID this pass ingested, and the next pass
        // is handed exactly the UIDs the store now holds.
        let known = ingest::known_uids_with_cursor(&fx.store, "acct", "inbox").unwrap();
        assert_eq!(known.prior_high_water, Some(102));
        assert_eq!(known.uidvalidity, Some(7));

        backend.script("INBOX", vec![Ok(fetch(vec![]))]);
        backend.script("Archive", vec![Ok(fetch(vec![]))]);
        fx.run(&mut backend, &targets);
        let asked = &backend.seen[2..];
        assert_eq!(
            asked,
            &[("INBOX".to_string(), usize::MAX, 2), ("Archive".to_string(), usize::MAX, 1)],
            "pass 2 hands the backend the skip list pass 1 wrote, in target order"
        );
    }

    /// #0074, through the real loop this time: a message the engine downloads
    /// and cannot write pulls the persisted arrival mark under itself, holds
    /// this pass's prune back, and is retried on the next pass, which writes it
    /// exactly once and reopens the gate.
    ///
    /// The pre-#0059 version of this test re-walked `note_ingest_failure`,
    /// `mark_below_unmet` and `record_mailbox_cursor` by hand and could only
    /// claim that the loop called them in that order; here the loop does.
    #[test]
    fn an_unwritable_message_holds_the_mark_down_and_the_retry_writes_it_once() {
        let fx = Fixture::new();
        let targets = vec![SyncTarget { role: MailboxRole::Inbox, server_name: "INBOX".into() }];
        let mut backend = FakeBackend::default();

        // Pass 0 seeds the row a later prune will delete, so `pruned` is
        // observable. Pass 1: UID 105 does not parse and cannot be written,
        // UID 104 lands beside it, and UID 90 is offered as vanished to show
        // the write failure holds the prune back too. Pass 2 hands 105 back
        // (it is not in the store, so a real backend re-downloads it) parsable.
        let mut poisoned = fetch(vec![(104, raw("below")), (105, unparsable())]);
        poisoned.vanished = vec![90];
        let mut good = fetch(vec![(105, raw("retried"))]);
        good.vanished = vec![90];
        backend.script(
            "INBOX",
            vec![Ok(fetch(vec![(90, raw("doomed"))])), Ok(poisoned), Ok(good)],
        );

        let seed = fx.run(&mut backend, &targets);
        assert_eq!(seed.saved, 1);

        let result = fx.run(&mut backend, &targets);

        assert_eq!(result.saved, 1, "the poisoned message does not stop the one beside it");
        assert_eq!(fx.rows("inbox"), vec![90, 104], "and 105 is simply not there");
        assert_eq!(
            fx.cursor_mark("inbox"),
            Some(104),
            "the mark sits below the message that was not written"
        );
        assert_eq!(result.pruned, 0, "and the same failure suspends this pass's prune");
        assert_eq!(result.prunes_deferred, 1);
        assert_eq!(ingest::ingest_failure_attempts(&fx.store, "acct", "inbox", 105), 1);

        let result = fx.run(&mut backend, &targets);

        assert_eq!(result.saved, 1);
        assert_eq!(fx.cursor_mark("inbox"), None, "a pass that wrote what it owed reopens the gate");
        assert_eq!(ingest::ingest_failure_attempts(&fx.store, "acct", "inbox", 105), 0);
        assert_eq!(result.pruned, 1, "and the reopened gate applies the prune it deferred");
        assert_eq!(fx.rows("inbox"), vec![104, 105], "written once, and the vanished row is gone");
    }

    /// #0074: the mark may not become a deadlock. A message the store rejects
    /// every pass is given up on after [`ingest::MAX_INGEST_ATTEMPTS`], and the
    /// pass after that stops reporting itself short, which is what stops one
    /// unwritable message from suspending the prune for the account for good.
    #[test]
    fn a_permanently_unwritable_message_stops_holding_the_prune_after_three_passes() {
        let fx = Fixture::new();
        let targets = vec![SyncTarget { role: MailboxRole::Inbox, server_name: "INBOX".into() }];
        let mut backend = FakeBackend::default();
        let poisoned = || {
            let mut f = fetch(vec![(105, unparsable())]);
            f.vanished = vec![90];
            Ok(f)
        };
        backend.script("INBOX", vec![poisoned(), poisoned(), poisoned(), poisoned()]);

        for pass in 1..ingest::MAX_INGEST_ATTEMPTS {
            let result = fx.run(&mut backend, &targets);
            assert_eq!(fx.cursor_mark("inbox"), Some(104), "pass {pass} still owes the message");
            assert_eq!(result.pruned, 0, "pass {pass} keeps the prune suspended");
        }

        // The last attempt is the give-up: the UID drops out of `unmet`, so it
        // neither lowers the mark nor reports the pass short from here on.
        let result = fx.run(&mut backend, &targets);
        assert_eq!(fx.cursor_mark("inbox"), None, "a given-up UID leaves no mark behind");
        assert_eq!(result.prunes_deferred, 0, "and no longer reports the pass short");
        assert_eq!(ingest::ingest_failure_attempts(&fx.store, "acct", "inbox", 105), 3);
    }

    /// #0074 review, through the loop: a UIDVALIDITY reset clears the mailbox's
    /// failure counts, because they are keyed by UID and the server has just
    /// renumbered them. The message that lands on the recycled UID gets its
    /// full three attempts rather than inheriting a give-up.
    #[test]
    fn a_uidvalidity_reset_clears_the_mailboxs_failure_counts() {
        let fx = Fixture::new();
        let targets = vec![SyncTarget { role: MailboxRole::Inbox, server_name: "INBOX".into() }];
        let mut backend = FakeBackend::default();
        let poisoned = || Ok(fetch(vec![(105, unparsable())]));
        let mut after_reset = fetch(vec![(105, unparsable())]);
        after_reset.uidvalidity_reset = true;
        backend.script("INBOX", vec![poisoned(), poisoned(), Ok(after_reset), poisoned()]);

        fx.run(&mut backend, &targets);
        fx.run(&mut backend, &targets);
        assert_eq!(ingest::ingest_failure_attempts(&fx.store, "acct", "inbox", 105), 2);

        let result = fx.run(&mut backend, &targets);
        assert_eq!(result.uidvalidity_resets, 1);
        assert_eq!(
            ingest::ingest_failure_attempts(&fx.store, "acct", "inbox", 105),
            1,
            "the reset wiped the count and this pass's own failure is the first again"
        );
        fx.run(&mut backend, &targets);
        assert_eq!(ingest::ingest_failure_attempts(&fx.store, "acct", "inbox", 105), 2);
        assert_eq!(fx.cursor_mark("inbox"), Some(104), "still retrying, so still owed");
    }

    /// #0072/#0055: prunes run after *every* target is ingested, so a message
    /// archived in another client has its archive row before its inbox row
    /// goes, and never spends a window with no row anywhere.
    #[test]
    fn a_message_moved_between_mailboxes_is_ingested_before_its_old_row_is_pruned() {
        let fx = Fixture::new();
        let targets = targets();
        let mut backend = FakeBackend::default();
        backend.script("INBOX", vec![Ok(fetch(vec![(101, raw("moved"))]))]);
        backend.script("Archive", vec![Ok(fetch(vec![]))]);
        fx.run(&mut backend, &targets);
        assert_eq!(fx.rows("inbox"), vec![101]);

        // Now the server says: gone from INBOX, present in Archive.
        let mut gone = fetch(vec![]);
        gone.vanished = vec![101];
        backend.script("INBOX", vec![Ok(gone)]);
        backend.script("Archive", vec![Ok(fetch(vec![(7, raw("moved"))]))]);
        let result = fx.run(&mut backend, &targets);

        assert_eq!(result.pruned, 1);
        assert!(fx.rows("inbox").is_empty(), "the inbox row goes");
        assert_eq!(fx.rows("archive"), vec![7], "and the archive row is already there");
    }

    /// The coverage gate is account-wide: one target that came back short
    /// suspends every target's prune, because the argument that lets an inbox
    /// row go is that another target ingested the copy it moved to.
    #[test]
    fn a_short_target_defers_every_targets_prune() {
        let fx = Fixture::new();
        let targets = targets();
        let mut backend = FakeBackend::default();
        backend.script("INBOX", vec![Ok(fetch(vec![(101, raw("one"))]))]);
        backend.script("Archive", vec![Ok(fetch(vec![(55, raw("two"))]))]);
        fx.run(&mut backend, &targets);

        let mut inbox_gone = fetch(vec![]);
        inbox_gone.vanished = vec![101];
        backend.script("INBOX", vec![Ok(inbox_gone)]);
        // The archive fetch failed outright, which is the strongest partial
        // pass: the engine keeps going and prunes nothing.
        backend.script("Archive", vec![Err(anyhow::anyhow!("connection reset"))]);
        let result = fx.run(&mut backend, &targets);

        assert_eq!(result.pruned, 0);
        assert_eq!(result.prunes_deferred, 1);
        assert_eq!(fx.rows("inbox"), vec![101], "the row stays until a pass sees everything");
        assert_eq!(fx.rows("archive"), vec![55], "and the failed target is left untouched");
    }

    /// `dry_run` counts and writes nothing: no rows, no cursor, no prune.
    #[test]
    fn a_dry_run_touches_neither_the_store_nor_the_blobs() {
        let fx = Fixture::new();
        let targets = vec![SyncTarget { role: MailboxRole::Inbox, server_name: "INBOX".into() }];
        let mut backend = FakeBackend::default();
        let mut f = fetch(vec![(101, raw("one")), (102, raw("two"))]);
        f.vanished = vec![90];
        backend.script("INBOX", vec![Ok(f)]);

        let result = fx.run_with(&mut backend, &targets, 50, true);

        assert_eq!(result.saved, 2, "it still reports what it would have ingested");
        assert!(fx.rows("inbox").is_empty());
        assert_eq!(result.pruned, 0);
        assert!(result.fresh_observations.is_empty(), "and feeds the contacts hook nothing");
        assert_eq!(backend.seen[0].1, 50, "the limit reaches the transport verbatim");
    }

    /// Flags are the second status axis (#TKT-0051) and arrive only on rows the
    /// store already holds, via `known_flags`.
    #[test]
    fn known_flags_from_the_backend_are_applied_to_rows_the_store_already_holds() {
        let fx = Fixture::new();
        let targets = vec![SyncTarget { role: MailboxRole::Inbox, server_name: "INBOX".into() }];
        let mut backend = FakeBackend::default();
        backend.script("INBOX", vec![Ok(fetch(vec![(101, raw("one"))]))]);
        fx.run(&mut backend, &targets);

        let mut flagged = fetch(vec![]);
        flagged.known_flags = vec![(101, MessageFlags { seen: true, answered: true, ..Default::default() })];
        flagged.skipped = 1;
        backend.script("INBOX", vec![Ok(flagged)]);
        let result = fx.run(&mut backend, &targets);

        assert_eq!(result.flags_updated, 1);
        assert_eq!(result.skipped, 1);
        let flags: String = fx
            .store
            .conn()
            .query_row(
                "SELECT flags FROM messages WHERE account = 'acct' AND mailbox = 'inbox' AND uid = 101",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(flags.contains("\\Seen") && flags.contains("\\Answered"));
    }

    /// #0041, end to end through the engine: the CONDSTORE resume point
    /// survives the passes that know nothing about it, and dies with a
    /// UIDVALIDITY reset.
    ///
    /// This is the loop's half of the carry-forward hazard. The engine writes
    /// whatever the fetch reported, `None` included, and `None` has to mean
    /// "nothing to say" rather than "clear it": a quick sync reports `None` on
    /// every mailbox bigger than its window, so the unconditional write would
    /// have erased the resume point on the very next pass and the delta would
    /// have flapped in and out of use forever, silently.
    #[test]
    fn a_condstore_modseq_survives_the_passes_that_cannot_vouch_for_one() {
        let fx = Fixture::new();
        let targets = vec![SyncTarget { role: MailboxRole::Inbox, server_name: "INBOX".into() }];
        let mut backend = FakeBackend::default();

        // Pass 1 is a full pass over a CONDSTORE server: it records a modseq.
        let mut condstore = fetch(vec![(101, raw("one"))]);
        condstore.highest_modseq = Some(90_060_115_205_545_359);
        // Pass 2 is an ordinary capped pass, which reports no modseq at all.
        let quick = fetch(vec![(102, raw("two"))]);
        // Pass 3 is the renumbering.
        let mut renumbered = fetch(vec![(1, raw("three"))]);
        renumbered.uidvalidity_reset = true;
        backend.script("INBOX", vec![Ok(condstore), Ok(quick), Ok(renumbered)]);

        fx.run(&mut backend, &targets);
        assert_eq!(
            fx.modseq("inbox"),
            Some(90_060_115_205_545_359),
            "the CONDSTORE pass records its resume point"
        );

        fx.run(&mut backend, &targets);
        assert_eq!(
            fx.modseq("inbox"),
            Some(90_060_115_205_545_359),
            "and a pass with nothing to say about it must not erase it"
        );
        // ...and the next fetch is handed it back as its resume point.
        let known = ingest::known_uids_with_cursor(&fx.store, "acct", "inbox").unwrap();
        assert_eq!(known.highest_modseq, Some(90_060_115_205_545_359));

        fx.run(&mut backend, &targets);
        assert_eq!(
            fx.modseq("inbox"),
            None,
            "a UIDVALIDITY reset is the one thing that clears it: the modseq \
             described a mailbox that no longer exists"
        );
        assert!(
            ingest::known_uids_with_cursor(&fx.store, "acct", "inbox")
                .unwrap()
                .highest_modseq
                .is_none(),
            "so the next pass does the full window rather than a delta"
        );
    }

    // -----------------------------------------------------------------------
    // The arrival-mark arithmetic itself
    // -----------------------------------------------------------------------

    /// #0074: what the download covered is not what the pass wrote. A UID that
    /// was fetched and not ingested pulls the persisted mark under itself, even
    /// when the download reported the pass complete.
    #[test]
    fn an_unwritten_uid_pulls_the_mark_below_itself() {
        // The complete-looking case the bug lived in: no mark, so the next pass
        // would have derived a floor above the message it never wrote.
        assert_eq!(mark_below_unmet(None, &[105]), Some(104));
        // The lowest one wins; everything above it is an arrival again too.
        assert_eq!(mark_below_unmet(None, &[110, 105, 107]), Some(104));
        // A mark the download already reported can only be lowered.
        assert_eq!(mark_below_unmet(Some(100), &[105]), Some(100));
        assert_eq!(mark_below_unmet(Some(200), &[105]), Some(104));
        // Nothing unwritten changes nothing, which is how the gate reopens.
        assert_eq!(mark_below_unmet(None, &[]), None);
        assert_eq!(mark_below_unmet(Some(100), &[]), Some(100));
        // The bottom of the mailbox: every listed UID is an arrival.
        assert_eq!(mark_below_unmet(None, &[1]), Some(0));
    }
}
