use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use log::{info, warn};

use super::{ImapSession, search::{FetchCriteria, build_imap_search_query}, open_imap_session};
use crate::config::ImapConfig;
use crate::ingest::KnownUids;
use crate::parse::{compress_uid_set, parse_rfc822_to_fetched_email, FetchedEmail};
use crate::timing::TimingSpan;

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
            email.is_read = msg.flags().any(|f| matches!(f, async_imap::types::Flag::Seen));
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

/// One message downloaded for ingest, with the identity the store keys on.
pub struct FetchedRaw {
    pub uid: u32,
    pub raw: Vec<u8>,
    pub is_read: bool,
}

/// What the SELECT response said about the mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxState {
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub exists: u32,
}

/// What one store fetch brought back.
pub struct StoreFetch {
    /// Messages the store does not hold yet, with their bodies.
    pub messages: Vec<FetchedRaw>,
    /// How many UIDs in the window the store already held.
    pub skipped: usize,
    /// The `\Seen` state of those already-held UIDs, the only server-to-local
    /// read-status channel (#0004).
    pub known_flags: Vec<(u32, bool)>,
    /// What SELECT said about the mailbox.
    pub state: MailboxState,
    /// UIDs the store holds for this mailbox that the server no longer lists.
    /// See [`vanished_uids`].
    pub vanished: Vec<u32>,
    /// True when the server's UIDVALIDITY no longer matches the stored one, so
    /// this fetch deliberately skipped nothing and redownloaded the window.
    pub uidvalidity_reset: bool,
    /// True when `UID SEARCH ALL` listed at least as many UIDs as `SELECT`
    /// announced in `EXISTS`, i.e. the enumeration [`vanished`] was computed
    /// from is the whole mailbox rather than a short answer.
    ///
    /// [`vanished`]: StoreFetch::vanished
    pub enumeration_complete: bool,
    /// True when this pass did not ingest every message that arrived in the
    /// mailbox since the store last saw it: the `limit` window cut some off,
    /// a body did not come back, or the caller failed to ingest one. Backlog
    /// *older* than what the store already holds does not count; see
    /// [`arrivals_missed`].
    pub download_incomplete: bool,
}

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
/// coverage flags on [`StoreFetch`] and by [`crate::ingest::pass_may_prune`].
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

/// Whether any message that arrived since the store's high-water mark was not
/// ingested by this pass.
///
/// "Arrival" is the load-bearing word. A quick sync downloads the last `limit`
/// UIDs and deliberately ignores the backlog below them, so measuring coverage
/// as `everything the folder holds` would report every capped pass as short and
/// suspend the prune forever on any mailbox bigger than the window. What the
/// prune actually depends on is that the copy of a message *moved into* this
/// mailbox landed, and a move issues a fresh UID at the top of the folder;
/// anything above `high_water` is such an arrival, and all of them must be in.
fn arrivals_missed(
    listed: &[u32],
    known: &std::collections::HashSet<i64>,
    ceiling: u32,
    ingested: &[u32],
) -> bool {
    let mark = high_water(known, ceiling);
    let ingested: std::collections::HashSet<u32> = ingested.iter().copied().collect();
    listed
        .iter()
        .filter(|&&uid| uid > mark)
        .filter(|&&uid| !known.contains(&(uid as i64)))
        .any(|uid| !ingested.contains(uid))
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
) -> Result<StoreFetch> {
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
    let (known_uids, uidvalidity_reset) = known.resolve(state.uid_validity);
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

    // The prune's top clamp, and the line that separates a server UID from a
    // locally written placeholder. `UIDNEXT` is what SELECT promised; a server
    // that withholds it leaves the highest UID it did list.
    let ceiling = state
        .uid_next
        .map(|n| n.saturating_sub(1))
        .or_else(|| listed.last().copied())
        .unwrap_or(0);
    // A listing shorter than the EXISTS the same SELECT announced is a partial
    // answer, and a partial answer reads exactly like a mass deletion.
    let enumeration_complete = listed.len() >= state.exists as usize;
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

    let empty = |state: MailboxState| StoreFetch {
        messages: Vec::new(),
        skipped: 0,
        known_flags: Vec::new(),
        state,
        vanished: vanished.clone(),
        uidvalidity_reset,
        enumeration_complete,
        download_incomplete: false,
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
    let mut known_flags: Vec<(u32, bool)> = Vec::new();
    for msg in flagged.iter() {
        let Some(uid) = msg.uid else { continue };
        let is_seen = msg.flags().any(|f| matches!(f, async_imap::types::Flag::Seen));
        if known_uids.contains(&(uid as i64)) {
            known_flags.push((uid, is_seen));
        } else {
            new_uids.push(uid);
        }
    }
    let skipped = known_flags.len();

    if new_uids.is_empty() {
        return Ok(StoreFetch {
            messages: Vec::new(),
            skipped,
            known_flags,
            state,
            vanished,
            uidvalidity_reset,
            enumeration_complete,
            download_incomplete: arrivals_missed(&listed, &known_uids, ceiling, &[]),
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
            is_read: msg.flags().any(|f| matches!(f, async_imap::types::Flag::Seen)),
        });
    }

    let downloaded: Vec<u32> = out.iter().map(|m| m.uid).collect();
    Ok(StoreFetch {
        messages: out,
        skipped,
        known_flags,
        state,
        vanished,
        uidvalidity_reset,
        enumeration_complete,
        download_incomplete: arrivals_missed(&listed, &known_uids, ceiling, &downloaded),
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
        assert!(!arrivals_missed(&listed, &known, 51, &[]));
    }

    /// A message that arrived above the mark and was not downloaded is: the
    /// bulk-move case where the destination window could not hold every copy.
    #[test]
    fn an_arrival_the_window_cut_off_is_a_missed_arrival() {
        let known = HashSet::from([50]);
        assert!(arrivals_missed(&[50, 51, 52], &known, 52, &[52]));
        assert!(!arrivals_missed(&[50, 51, 52], &known, 52, &[51, 52]));
    }

    /// A first sync of a mailbox bigger than the window has no mark to stand
    /// on, so every listed UID is an arrival and the pass is short. Nothing to
    /// prune then anyway, and the flag keeps it that way.
    #[test]
    fn a_first_sync_of_a_capped_mailbox_is_incomplete() {
        let known = HashSet::new();
        assert!(arrivals_missed(&[1, 2, 3], &known, 3, &[2, 3]));
    }
}
