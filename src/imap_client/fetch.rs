use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use log::{info, warn};

use super::{ImapSession, search::{FetchCriteria, build_imap_search_query}, open_imap_session};
use crate::config::ImapConfig;
use crate::ingest::KnownUids;
use crate::parse::{compress_uid_set, parse_rfc822_to_fetched_email, FetchedEmail};
use crate::sync::{FetchedRaw, MailboxFetch, MailboxState};
use crate::timing::TimingSpan;
use crate::types::MessageFlags;

/// The status axes as the server states them: `\Seen`, `\Answered` and the
/// `$Forwarded` keyword (#TKT-0051), plus `\Flagged` (#0007), read off one
/// `FETCH FLAGS` response.
///
/// A keyword arrives as [`async_imap::types::Flag::Custom`], so it goes
/// through [`MessageFlags::parse`] rather than a match arm, which is also what
/// keeps the two spellings of the forwarded keyword in one place.
fn flags_of(msg: &async_imap::types::Fetch) -> MessageFlags {
    let mut out = MessageFlags::default();
    for flag in msg.flags() {
        match flag {
            async_imap::types::Flag::Seen => out.seen = true,
            async_imap::types::Flag::Answered => out.answered = true,
            async_imap::types::Flag::Flagged => out.flagged = true,
            async_imap::types::Flag::Custom(name) => {
                out.forwarded |= MessageFlags::parse(&name).forwarded;
            }
            _ => {}
        }
    }
    out
}

/// Fetch emails on an existing session using search criteria and optional limit.
pub async fn fetch_emails_on_session(
    session: &mut ImapSession,
    criteria: &FetchCriteria,
    mailbox: &str,
    limit: Option<usize>,
) -> Result<Vec<FetchedEmail>> {
    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select mailbox '{}': {}", mailbox, e))?;

    let query = build_imap_search_query(criteria);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let mut uid_list: Vec<u32> = uids.into_iter().collect();
    uid_list.sort();
    let selected_uids: Vec<u32> = match limit {
        Some(n) => uid_list.into_iter().rev().take(n).collect(),
        None => uid_list,
    };

    let uid_set = compress_uid_set(&selected_uids);

    let fetched: Vec<_> = session
        .uid_fetch(&uid_set, "(BODY.PEEK[] FLAGS)")
        .await
        .map_err(|e| anyhow!("Failed to fetch emails: {}", e))?
        .try_collect()
        .await
        .map_err(|e| anyhow!("Failed to collect emails: {}", e))?;

    let mut emails = Vec::new();
    for msg in fetched.iter() {
        let body_raw = msg.body().unwrap_or_default();
        if let Some(mut email) = parse_rfc822_to_fetched_email(body_raw) {
            email.flags = flags_of(msg);
            emails.push(email);
        }
    }

    // `HEADER "Message-ID"` is a substring match on the server side; make the
    // lookup exact here so every caller of this seam gets the same guarantee.
    super::search::retain_exact_message_id(&mut emails, criteria);

    Ok(emails)
}

/// Fetch emails from an IMAP server. Opens and closes its own session.
pub async fn fetch_emails(
    imap_config: &ImapConfig,
    criteria: &FetchCriteria,
    mailbox: &str,
    limit: Option<usize>,
) -> Result<Vec<FetchedEmail>> {
    info!(
        "Fetching emails from mailbox '{}' (limit: {:?})",
        mailbox, limit
    );
    let mut session = open_imap_session(imap_config).await?;
    let emails = fetch_emails_on_session(&mut session, criteria, mailbox, limit).await?;
    session.logout().await.ok();
    Ok(emails)
}

// ---------------------------------------------------------------------------
// Store ingest fetch
// ---------------------------------------------------------------------------

// The store fetch's result types live in `crate::sync` since #0059: they are
// the engine's contract with every backend, not this module's private shape.
// See `MailboxFetch`, `FetchedRaw` and `MailboxState`.

/// The UIDs the store holds that the server did not list, up to `ceiling`.
///
/// `listed` is the *whole* mailbox as `UID SEARCH ALL` returned it, not the
/// download window: the enumeration is complete even when the fetch is capped,
/// so a message archived, moved or deleted in another client is absent from it
/// whatever its UID. Diffing against the window instead was #0072, where
/// archiving the oldest mail elsewhere left rows below the window's bottom
/// that nothing could ever reach.
///
/// Two kinds of known UID are still exempt, both of them locally written rather
/// than server-issued:
///
/// - negative UIDs, the `-id` sentinel a local move parks a row on (see
///   [`crate::store::write`]), waiting for the destination's next sync to give
///   it a real UID;
/// - anything above `ceiling`, which the caller sets to `UIDNEXT - 1`. A Sent
///   copy appended without an `APPENDUID` is stored under
///   [`crate::ingest::graph_uid`], a 63-bit hash of the Message-ID that no
///   server would ever assign, and a row written optimistically ahead of the
///   server sits just above the same line. `UIDNEXT` is the right ceiling
///   rather than `max(listed)` because it does not drop when the newest
///   message is the one that was deleted.
///
/// Whether the result may be *applied* is a separate question, answered by the
/// coverage flags on [`MailboxFetch`] and by [`crate::ingest::pass_may_prune`].
pub fn vanished_uids(
    known: &std::collections::HashSet<i64>,
    listed: &[u32],
    ceiling: u32,
) -> Vec<u32> {
    let listed: std::collections::HashSet<u32> = listed.iter().copied().collect();
    let mut out: Vec<u32> = known
        .iter()
        .filter(|&&uid| uid > 0 && uid <= ceiling as i64)
        .map(|&uid| uid as u32)
        .filter(|uid| !listed.contains(uid))
        .collect();
    out.sort_unstable();
    out
}

/// The highest UID `known` holds that a server could have issued, i.e. the
/// mailbox's local high-water mark.
///
/// Placeholder rows are excluded by the same `ceiling` the prune uses, so a
/// `graph_uid` hash cannot push the mark to 2^62 and make every real arrival
/// look old.
fn high_water(known: &std::collections::HashSet<i64>, ceiling: u32) -> u32 {
    known
        .iter()
        .filter(|&&uid| uid > 0 && uid <= ceiling as i64)
        .map(|&uid| uid as u32)
        .max()
        .unwrap_or(0)
}

/// What one pass learned about the arrivals it was supposed to bring in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrivalCoverage {
    /// True when at least one arrival above the mark is still not in the store
    /// after this pass. Suspends the prune (see [`crate::ingest::pass_may_prune`]).
    pub incomplete: bool,
    /// The mark to carry into the next pass, or `None` when every arrival is
    /// in and the next pass may derive its own mark from the store again.
    pub pending_mark: Option<u32>,
}

/// The line above which every listed UID must be in the store for the pass to
/// count as complete, or `None` when the mailbox has no line to stand on.
///
/// The floor is what the mailbox is known to have held: the store's own
/// high-water mark, or the top the last recorded pass saw (`sync_cursors.last_uid`,
/// `prior`) when the rows themselves are gone. A UID at or below it existed
/// before the last pass and was already passed over as backlog, so it is not an
/// arrival now. `prior` is clamped to the same `ceiling` the rest of the module
/// uses, which is what keeps a placeholder UID from lifting the floor to 2^62.
///
/// With a carried mark the answer is the lower of the two, which is the whole
/// point: a pass that ingested the top of the window raises the floor above the
/// arrivals it could *not* reach, and deriving the mark afresh would put them
/// below it and declare the pass complete (#0072 review note 1).
///
/// `None` is *first contact*: no carried mark, no cursor row, no rows. Nothing
/// about this mailbox is known yet, so nothing the server lists is an arrival
/// and there is no mark worth handing to the next pass. Persisting one there is
/// what turned a first capped sync of a large mailbox into a mark of `0` that
/// no positional window could ever meet, suspending the prune account-wide
/// until a manual full sync (#0072 sweep review B1).
fn arrival_mark(
    known: &std::collections::HashSet<i64>,
    ceiling: u32,
    carried: Option<u32>,
    prior: Option<u32>,
) -> Option<u32> {
    let prior = prior.map(|uid| uid.min(ceiling));
    let floor = high_water(known, ceiling).max(prior.unwrap_or(0));
    match carried {
        Some(mark) => Some(mark.min(floor)),
        // A cursor row is history even when it records a top of 0 and every
        // local row is gone: a mailbox emptied and then bulk-moved into still
        // has to defer, which is why the cursor is consulted rather than the
        // rows alone.
        None if prior.is_none() && floor == 0 => None,
        None => Some(floor),
    }
}

/// Whether any message that arrived since the mailbox's arrival mark is still
/// not in the store after this pass, and the mark the next pass must use.
///
/// "Arrival" is the load-bearing word. A quick sync downloads the last `limit`
/// UIDs and deliberately ignores the backlog below them, so measuring coverage
/// as `everything the folder holds` would report every capped pass as short and
/// suspend the prune forever on any mailbox bigger than the window. What the
/// prune actually depends on is that the copy of a message *moved into* this
/// mailbox landed, and a move issues a fresh UID at the top of the folder;
/// anything above the mark is such an arrival, and all of them must be in.
///
/// The mark is *persisted* (`sync_cursors.arrival_mark`) rather than recomputed,
/// because an unmet arrival outlives the pass that missed it: bulk-move 300
/// messages into a mailbox a 100-UID window can only take the top of, and pass 2
/// would otherwise stand on a high-water mark the 200 stragglers sit below, open
/// the gate, and prune the source rows of copies that were never ingested. The
/// mark therefore stays put until a pass actually reaches through it, which any
/// full sync does; it also clears when the stragglers stop being listed (deleted
/// on the server), so it cannot deadlock.
///
/// First contact ([`arrival_mark`] returning `None`) is the one case that hands
/// nothing on. The pass is still reported short whenever it could not take the
/// whole listing, which costs one conservative pass and nothing more, because
/// the cursor it writes gives the next pass a real floor to stand on.
fn arrival_coverage(
    listed: &[u32],
    known: &std::collections::HashSet<i64>,
    ceiling: u32,
    ingested: &[u32],
    carried: Option<u32>,
    prior: Option<u32>,
) -> ArrivalCoverage {
    let mark = arrival_mark(known, ceiling, carried, prior);
    let ingested: std::collections::HashSet<u32> = ingested.iter().copied().collect();
    let incomplete = listed
        .iter()
        .filter(|&&uid| uid > mark.unwrap_or(0))
        .filter(|&&uid| !known.contains(&(uid as i64)))
        .any(|uid| !ingested.contains(uid));
    ArrivalCoverage {
        incomplete,
        pending_mark: mark.filter(|_| incomplete),
    }
}

/// Whether the `UID SEARCH ALL` listing is the whole mailbox.
///
/// A listing shorter than the `EXISTS` the same SELECT announced is a partial
/// answer, and a partial answer reads exactly like a mass deletion. Two
/// independent server statements have to agree before anything is pruned, which
/// is what bounds the blast radius of the diff to something a single malformed
/// response cannot widen.
///
/// A listing *longer* than `EXISTS` is not a contradiction worth acting on:
/// messages can arrive between the two responses, and every extra UID can only
/// keep a row alive.
fn enumeration_complete(listed_len: usize, exists: u32) -> bool {
    listed_len >= exists as usize
}

/// The prune's top clamp, and the line that separates a server UID from a
/// locally written placeholder.
///
/// `UIDNEXT` is what SELECT promised and is preferred over `max(listed)`
/// because it does not drop when the newest message is the one that was
/// deleted. A server that withholds it leaves the highest UID it did list;
/// a server that withholds both leaves 0, which prunes nothing at all.
///
/// `listed` must be sorted ascending, as it is at every call site.
fn ceiling(uid_next: Option<u32>, listed: &[u32]) -> u32 {
    uid_next
        .map(|n| n.saturating_sub(1))
        .or_else(|| listed.last().copied())
        .unwrap_or(0)
}

/// Two-pass fetch for the store ingest path.
///
/// Pass 1 fetches `UID FLAGS` over the whole window, pass 2 downloads
/// `BODY.PEEK[]` only for UIDs the store does not hold yet. Identity is the
/// UID, so pass 1 no longer needs the `Message-ID` header the `.md` era
/// compared against a directory scan.
///
/// Pass 1 always covers the full `limit` window even when nothing is new: the
/// `\Seen` flags it collects are the only server-to-local read-status channel
/// (ticket #0004), so shrinking the window silently drops flag changes made in
/// other clients. Pass 2 is skipped entirely when nothing is new.
///
/// The skip list is resolved against the server's UIDVALIDITY *after* SELECT
/// (see [`KnownUids::resolve`]): a renumbering hands recycled UIDs to different
/// messages, so carrying the stored list across one would make pass 2 skip
/// bodies that were never downloaded.
pub async fn fetch_new_raw_on_session(
    session: &mut ImapSession,
    mailbox: &str,
    limit: Option<usize>,
    known: KnownUids,
) -> Result<MailboxFetch> {
    let mut span = TimingSpan::with_context("fetch_new_raw", mailbox.to_string());

    let imap_mailbox = session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select mailbox '{}': {}", mailbox, e))?;
    span.mark("select");
    let state = MailboxState {
        uid_validity: imap_mailbox.uid_validity,
        uid_next: imap_mailbox.uid_next,
        exists: imap_mailbox.exists,
    };

    let stored_uidvalidity = known.uidvalidity;
    let stored_arrival_mark = known.arrival_mark;
    let stored_prior_high_water = known.prior_high_water;
    let (known_uids, uidvalidity_reset) = known.resolve(state.uid_validity);
    // A mark and a recorded top are both UIDs, so a renumbering makes them
    // meaningless: they are dropped with the skip list they travelled with.
    let (carried_mark, prior_high_water) = if uidvalidity_reset {
        (None, None)
    } else {
        (stored_arrival_mark, stored_prior_high_water)
    };
    if uidvalidity_reset {
        warn!(
            "UIDVALIDITY for '{}' changed from {:?} to {:?}: refetching the whole window, \
             the rows are rebound through their Message-IDs",
            mailbox, stored_uidvalidity, state.uid_validity
        );
    }
    let uids = session
        .uid_search("ALL")
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;
    span.mark("uid_search");

    let mut listed: Vec<u32> = uids.into_iter().collect();
    listed.sort_unstable();

    let ceiling = ceiling(state.uid_next, &listed);
    let enumeration_complete = enumeration_complete(listed.len(), state.exists);
    if !enumeration_complete {
        warn!(
            "'{}' listed {} UID(s) but announced EXISTS {}: treating the enumeration as short, \
             nothing will be pruned this pass",
            mailbox,
            listed.len(),
            state.exists
        );
    }
    let vanished = if enumeration_complete {
        vanished_uids(&known_uids, &listed, ceiling)
    } else {
        Vec::new()
    };
    if !vanished.is_empty() {
        info!(
            "Store fetch for '{}': {} row(s) are no longer on the server",
            mailbox,
            vanished.len()
        );
    }

    // A pass that downloads nothing still has to answer the coverage question:
    // `mp sync -n 0` computes a whole vanished set and returns through here, so
    // hardcoding "complete" would force the gate open on a prune-only pass
    // (#0072 review note 3). An empty listing has no arrivals and stays open.
    let empty = |state: MailboxState| {
        let coverage = arrival_coverage(
            &listed,
            &known_uids,
            ceiling,
            &[],
            carried_mark,
            prior_high_water,
        );
        MailboxFetch {
            messages: Vec::new(),
            skipped: 0,
            known_flags: Vec::new(),
            state,
            vanished: vanished.clone(),
            uidvalidity_reset,
            enumeration_complete,
            download_incomplete: coverage.incomplete,
            pending_arrival_mark: coverage.pending_mark,
        }
    };

    if listed.is_empty() {
        return Ok(empty(state));
    }

    let mut window: Vec<u32> = match limit {
        Some(n) => listed.iter().rev().take(n).copied().collect(),
        None => listed.clone(),
    };
    window.sort_unstable();
    if window.is_empty() {
        return Ok(empty(state));
    }

    // Pass 1: UID + FLAGS over the whole window (~40 bytes per message).
    let window_set = compress_uid_set(&window);
    let flagged: Vec<_> = session
        .uid_fetch(&window_set, "(UID FLAGS)")
        .await
        .map_err(|e| anyhow!("Failed to fetch flags: {}", e))?
        .try_collect()
        .await
        .map_err(|e| anyhow!("Failed to collect flags: {}", e))?;
    span.mark("pass1_flags");

    let mut new_uids: Vec<u32> = Vec::new();
    let mut known_flags: Vec<(u32, MessageFlags)> = Vec::new();
    for msg in flagged.iter() {
        let Some(uid) = msg.uid else { continue };
        if known_uids.contains(&(uid as i64)) {
            known_flags.push((uid, flags_of(msg)));
        } else {
            new_uids.push(uid);
        }
    }
    let skipped = known_flags.len();

    if new_uids.is_empty() {
        let coverage = arrival_coverage(
            &listed,
            &known_uids,
            ceiling,
            &[],
            carried_mark,
            prior_high_water,
        );
        return Ok(MailboxFetch {
            messages: Vec::new(),
            skipped,
            known_flags,
            state,
            vanished,
            uidvalidity_reset,
            enumeration_complete,
            download_incomplete: coverage.incomplete,
            pending_arrival_mark: coverage.pending_mark,
        });
    }
    info!(
        "Store fetch for '{}': {} new, {} already ingested",
        mailbox,
        new_uids.len(),
        skipped
    );

    // Pass 2: full bodies for the new UIDs only.
    let new_set = compress_uid_set(&new_uids);
    let fetched: Vec<_> = session
        .uid_fetch(&new_set, "(UID BODY.PEEK[] FLAGS)")
        .await
        .map_err(|e| anyhow!("Failed to fetch emails: {}", e))?
        .try_collect()
        .await
        .map_err(|e| anyhow!("Failed to collect emails: {}", e))?;
    span.mark("pass2_bodies");

    let mut out = Vec::new();
    for msg in fetched.iter() {
        let Some(uid) = msg.uid else { continue };
        let Some(body) = msg.body() else { continue };
        out.push(FetchedRaw {
            uid,
            raw: body.to_vec(),
            flags: flags_of(msg),
        });
    }

    let downloaded: Vec<u32> = out.iter().map(|m| m.uid).collect();
    let coverage = arrival_coverage(
        &listed,
        &known_uids,
        ceiling,
        &downloaded,
        carried_mark,
        prior_high_water,
    );
    Ok(MailboxFetch {
        messages: out,
        skipped,
        known_flags,
        state,
        vanished,
        uidvalidity_reset,
        enumeration_complete,
        download_incomplete: coverage.incomplete,
        pending_arrival_mark: coverage.pending_mark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A UID the server no longer lists is gone.
    #[test]
    fn a_uid_missing_from_the_listing_is_vanished() {
        let known = HashSet::from([10, 11, 12, 13]);
        assert_eq!(vanished_uids(&known, &[10, 12, 13], 13), vec![11]);
    }

    /// #0072: the reported bug. The oldest inbox message is archived in another
    /// client, so it is below every UID the server still lists. The old
    /// window-range clamp could not see it; the full listing can.
    #[test]
    fn the_oldest_uid_archived_elsewhere_is_vanished() {
        let known: HashSet<i64> = (1..=83).collect();
        let listed: Vec<u32> = (2..=83).collect();
        assert_eq!(vanished_uids(&known, &listed, 83), vec![1]);
    }

    /// A capped download does not cap the diff: `UID SEARCH ALL` enumerated the
    /// whole mailbox even when only its last 50 UIDs were fetched, so the rows
    /// below the window are proved present, not merely unexamined.
    #[test]
    fn uids_below_the_download_window_are_kept_when_the_server_still_lists_them() {
        let known: HashSet<i64> = (1..=100).collect();
        let listed: Vec<u32> = (1..=100).collect();
        assert_eq!(vanished_uids(&known, &listed, 100), Vec::<u32>::new());
    }

    /// The one clamp that survives: above `UIDNEXT - 1` sit the locally written
    /// placeholders (a `graph_uid` hash, a row written ahead of the server),
    /// which the server was never asked about.
    #[test]
    fn uids_above_the_ceiling_survive() {
        let known = HashSet::from([5, 6, 7, 4_611_686_018_427_387_904]);
        assert_eq!(vanished_uids(&known, &[5, 7], 7), vec![6]);
    }

    /// A UIDVALIDITY reset hands `resolve` an empty known set, which must
    /// produce an empty prune: a renumbering says nothing about what is gone.
    #[test]
    fn an_empty_known_set_prunes_nothing() {
        assert_eq!(
            vanished_uids(&HashSet::new(), &[1, 2, 3], 3),
            Vec::<u32>::new()
        );
    }

    /// A mailbox the server lists as empty holds no rows either. The caller
    /// only reaches this with `EXISTS 0` agreeing with the empty listing.
    #[test]
    fn an_empty_listing_prunes_every_known_row() {
        let known = HashSet::from([1, 2, 3]);
        assert_eq!(vanished_uids(&known, &[], 3), vec![1, 2, 3]);
    }

    /// The negative sentinel of a locally moved row is not a server UID and
    /// must never be pruned.
    #[test]
    fn a_locally_moved_rows_sentinel_uid_is_never_pruned() {
        let known = HashSet::from([-7, 4, 5]);
        assert_eq!(vanished_uids(&known, &[4, 5], 5), Vec::<u32>::new());
    }

    /// The high-water mark ignores the placeholder above the ceiling, which
    /// would otherwise make every real arrival look older than the store.
    #[test]
    fn the_high_water_mark_ignores_placeholders() {
        let known = HashSet::from([-3, 4, 9, 4_611_686_018_427_387_904]);
        assert_eq!(high_water(&known, 20), 9);
    }

    /// Backlog below the high-water mark is not an arrival: a quick sync never
    /// promised to fetch it, so it must not suspend the prune.
    #[test]
    fn old_backlog_the_window_skipped_is_not_a_missed_arrival() {
        let known = HashSet::from([50, 51]);
        let listed: Vec<u32> = (1..=51).collect();
        let coverage = arrival_coverage(&listed, &known, 51, &[], None, Some(51));
        assert!(!coverage.incomplete);
        assert_eq!(coverage.pending_mark, None);
    }

    /// A message that arrived above the mark and was not downloaded is: the
    /// bulk-move case where the destination window could not hold every copy.
    #[test]
    fn an_arrival_the_window_cut_off_is_a_missed_arrival() {
        let known = HashSet::from([50]);
        assert!(arrival_coverage(&[50, 51, 52], &known, 52, &[52], None, Some(50)).incomplete);
        assert!(!arrival_coverage(&[50, 51, 52], &known, 52, &[51, 52], None, Some(50)).incomplete);
    }

    /// A first sync of a mailbox bigger than the window has no mark to stand
    /// on, so every listed UID is an arrival and the pass is short. Nothing to
    /// prune then anyway, and the flag keeps it that way.
    #[test]
    fn a_first_sync_of_a_capped_mailbox_is_incomplete() {
        let known = HashSet::new();
        assert!(arrival_coverage(&[1, 2, 3], &known, 3, &[2, 3], None, None).incomplete);
    }

    /// ...and it hands *no* mark to the next pass, which is the difference
    /// between one conservative pass and a permanently shut gate (#0072 sweep
    /// review B1).
    ///
    /// A capped first sync of a large mailbox used to persist a mark of 0: the
    /// carried mark is combined with `min`, so it could never rise again, and a
    /// mark of 0 demands the whole mailbox be in the store before any pass
    /// counts as complete, which a positional window never achieves. Because
    /// `pass_may_prune` needs *every* target complete, one such mailbox
    /// suspended the prune for the whole account until a manual full sync, and
    /// the schema v5 rebuild put every user in exactly that state.
    #[test]
    fn a_first_capped_sync_of_a_large_mailbox_hands_on_no_mark() {
        let listed: Vec<u32> = (1..=8000).collect();
        let window: Vec<u32> = (7951..=8000).collect();
        let nothing_known = HashSet::new();

        let first = arrival_coverage(&listed, &nothing_known, 8000, &window, None, None);
        assert!(
            first.incomplete,
            "one pass of conservatism is fine: the store has nothing to prune yet anyway"
        );
        assert_eq!(
            first.pending_mark, None,
            "the forbidden pre-fix answer is Some(0), a mark no window can ever meet"
        );

        // The next pass stands on the cursor the first one wrote (top of the
        // window) plus the rows it ingested, and prunes normally.
        let known: HashSet<i64> = (7951..=8000).map(i64::from).collect();
        let second = arrival_coverage(&listed, &known, 8000, &[], None, Some(8000));
        assert!(!second.incomplete, "the backlog below the window is not an arrival");
        assert_eq!(second.pending_mark, None);
    }

    /// The edge that keeps first contact from being "the store holds no rows":
    /// a mailbox emptied in another client and then bulk-moved into has no rows
    /// left, but its cursor still records the top it reached. That recorded top
    /// is the floor, so the 200 copies the window could not take still hold the
    /// gate shut.
    #[test]
    fn an_emptied_then_refilled_mailbox_defers_on_its_recorded_top() {
        let listed: Vec<u32> = (101..=400).collect();
        let window: Vec<u32> = (301..=400).collect();
        let nothing_known = HashSet::new();

        let coverage = arrival_coverage(&listed, &nothing_known, 400, &window, None, Some(100));
        assert!(
            coverage.incomplete,
            "101..=300 arrived above the recorded top and are not in the store"
        );
        assert_eq!(coverage.pending_mark, Some(100));
    }

    /// A pass that comes up short leaves the mark it stood on behind, so the
    /// next one can be held to the same line.
    #[test]
    fn a_short_pass_hands_its_mark_to_the_next_one() {
        let known = HashSet::from([50]);
        let coverage = arrival_coverage(&[50, 51, 52], &known, 52, &[52], None, Some(50));
        assert!(coverage.incomplete);
        assert_eq!(coverage.pending_mark, Some(50));
    }

    /// #0072 review note 1, the two-pass bulk move: 300 messages land in a
    /// mailbox whose quick sync only takes the top 100.
    ///
    /// Pass 1 defers, which the shipped gate already did. Pass 2 is where it
    /// used to fail: its own ingest raised `max(known)` to 400, which put the
    /// 200 stragglers *below* a freshly derived mark and opened the gate on
    /// rows whose copies were never fetched. The carried mark is what keeps it
    /// shut.
    #[test]
    fn the_pass_after_a_bulk_move_still_defers_while_arrivals_are_missing() {
        let listed: Vec<u32> = (1..=400).collect();
        let window: Vec<u32> = (301..=400).collect();

        // Pass 1: the store holds 1..=100, the move added 101..=400.
        let known_before: HashSet<i64> = (1..=100).collect();
        let pass1 = arrival_coverage(&listed, &known_before, 400, &window, None, Some(100));
        assert!(
            pass1.incomplete,
            "pass 1 could not reach 101..=300 and must defer"
        );
        assert_eq!(pass1.pending_mark, Some(100));

        // Pass 2: the store now also holds what pass 1 downloaded, and the
        // window has nothing new in it.
        let mut known_after = known_before.clone();
        known_after.extend((301..=400).map(i64::from));
        let derived = arrival_coverage(&listed, &known_after, 400, &[], None, Some(400));
        assert!(
            !derived.incomplete,
            "the pre-fix behaviour this test exists to forbid: a mark of 400 sees no arrival"
        );
        let carried =
            arrival_coverage(&listed, &known_after, 400, &[], pass1.pending_mark, Some(400));
        assert!(
            carried.incomplete,
            "101..=300 are still not in the store, so pass 2 must defer too"
        );
        assert_eq!(carried.pending_mark, Some(100));

        // A full sync brings the stragglers in and the gate opens.
        let everything: Vec<u32> = (101..=300).collect();
        let opened = arrival_coverage(
            &listed,
            &known_after,
            400,
            &everything,
            carried.pending_mark,
            Some(400),
        );
        assert!(!opened.incomplete);
        assert_eq!(opened.pending_mark, None, "the mark clears once it is met");
    }

    /// No mark can get stuck, including the lowest one there is. A mark of 0 is
    /// still reachable after the fix, and legitimately so: a mailbox that had
    /// never held a message when it was last synced records a top of 0, and a
    /// bulk move into it makes every copy an arrival. Ingesting through it
    /// clears it, which is the property that makes the whole mechanism safe to
    /// persist.
    #[test]
    fn any_persisted_mark_clears_once_a_pass_reaches_through_it() {
        let listed: Vec<u32> = (1..=300).collect();
        let known: HashSet<i64> = (201..=300).map(i64::from).collect();

        let stuck = arrival_coverage(&listed, &known, 300, &[], Some(0), Some(300));
        assert!(stuck.incomplete);
        assert_eq!(stuck.pending_mark, Some(0), "a mark of 0 holds the gate shut");

        let backlog: Vec<u32> = (1..=200).collect();
        let opened = arrival_coverage(&listed, &known, 300, &backlog, Some(0), Some(300));
        assert!(!opened.incomplete, "a full sync reaches through any mark");
        assert_eq!(opened.pending_mark, None);
    }

    /// The other way out of a carried mark: the arrivals that were never
    /// fetched are deleted on the server, so nothing owes the store anything
    /// and the gate opens without a full sync. The mark cannot deadlock.
    #[test]
    fn a_carried_mark_clears_when_the_missing_arrivals_stop_being_listed() {
        let known: HashSet<i64> = HashSet::from([1, 2, 3]);
        let coverage = arrival_coverage(&[1, 2, 3], &known, 10, &[], Some(1), Some(3));
        assert!(!coverage.incomplete);
        assert_eq!(coverage.pending_mark, None);
    }

    /// A carried mark never rises with the store: it is the lower of the two,
    /// which is the whole mechanism.
    #[test]
    fn the_carried_mark_wins_over_a_higher_high_water() {
        let known: HashSet<i64> = HashSet::from([10, 90]);
        assert_eq!(arrival_mark(&known, 100, Some(10), Some(90)), Some(10));
        assert_eq!(arrival_mark(&known, 100, None, Some(90)), Some(90));
        // A stale mark above the store's own high-water mark cannot loosen it.
        assert_eq!(arrival_mark(&known, 100, Some(95), Some(90)), Some(90));
    }

    /// The floor a pass stands on: the higher of what the store holds and what
    /// the cursor recorded, and `None` only when there is neither.
    #[test]
    fn the_mark_is_none_only_at_first_contact() {
        let nothing = HashSet::new();
        assert_eq!(arrival_mark(&nothing, 100, None, None), None);
        // A cursor row is history, even one that recorded no UID.
        assert_eq!(arrival_mark(&nothing, 100, None, Some(0)), Some(0));
        assert_eq!(arrival_mark(&nothing, 100, None, Some(40)), Some(40));
        // Rows without a cursor row still count: the store knows something.
        let known: HashSet<i64> = HashSet::from([40]);
        assert_eq!(arrival_mark(&known, 100, None, None), Some(40));
        // The recorded top only ever raises the floor, and is clamped to the
        // same ceiling that keeps a placeholder out of the high-water mark.
        assert_eq!(arrival_mark(&known, 100, None, Some(10)), Some(40));
        assert_eq!(arrival_mark(&known, 100, None, Some(4_000_000)), Some(100));
    }

    /// `mp sync -n 0` computes a full vanished set and downloads nothing. It
    /// must not report itself complete when the mailbox holds rows the store
    /// has never seen (#0072 review note 3).
    #[test]
    fn a_pass_that_downloads_nothing_is_not_complete() {
        let known: HashSet<i64> = HashSet::from([1, 2]);
        assert!(arrival_coverage(&[1, 2, 3], &known, 3, &[], None, Some(2)).incomplete);
    }

    /// ...and stays complete when the mailbox is genuinely empty, which is the
    /// one case the empty-listing return has to keep prunable.
    #[test]
    fn an_empty_listing_is_a_complete_pass() {
        let known: HashSet<i64> = HashSet::from([1, 2]);
        let coverage = arrival_coverage(&[], &known, 2, &[], None, Some(2));
        assert!(!coverage.incomplete);
        assert_eq!(coverage.pending_mark, None);
    }

    /// The enumeration gate: `UID SEARCH ALL` must account for every message
    /// `SELECT` announced before a single row is pruned.
    #[test]
    fn a_listing_shorter_than_exists_is_an_incomplete_enumeration() {
        assert!(!enumeration_complete(3, 4));
        assert!(enumeration_complete(4, 4));
        // A message that arrived between the two responses is not a short
        // answer, and can only keep rows alive.
        assert!(enumeration_complete(5, 4));
    }

    /// `EXISTS 0` with an empty listing is the one case where a complete
    /// enumeration prunes a whole mailbox, so it is pinned deliberately.
    #[test]
    fn an_empty_mailbox_enumerates_completely() {
        assert!(enumeration_complete(0, 0));
    }

    /// The ceiling comes from `UIDNEXT`, which does not drop when the newest
    /// message is the one that was deleted.
    #[test]
    fn the_ceiling_is_uidnext_minus_one() {
        assert_eq!(ceiling(Some(84), &[1, 2, 3]), 83);
    }

    /// A server that withholds `UIDNEXT` leaves the highest UID it did list,
    /// and one that lists neither leaves 0, which prunes nothing and makes
    /// every listed UID a placeholder rather than a prune candidate.
    #[test]
    fn the_ceiling_falls_back_to_the_listing_then_to_zero() {
        assert_eq!(ceiling(None, &[1, 2, 9]), 9);
        assert_eq!(ceiling(None, &[]), 0);
        assert_eq!(ceiling(Some(0), &[1, 2, 9]), 0);
    }
}
