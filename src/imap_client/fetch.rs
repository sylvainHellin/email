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
    /// UIDs the store holds for this mailbox that the server no longer lists,
    /// restricted to the numeric range the window covers. See
    /// [`vanished_uids`].
    pub vanished: Vec<u32>,
    /// True when the server's UIDVALIDITY no longer matches the stored one, so
    /// this fetch deliberately skipped nothing and redownloaded the window.
    pub uidvalidity_reset: bool,
}

/// The UIDs the store holds that the server did not list, clamped to the
/// window's own numeric range.
///
/// The clamp is the whole safety argument. `UID SEARCH ALL` returns the entire
/// mailbox, but the window is only its last `limit` UIDs, so `known − window`
/// on a 12k mailbox with `-n 50` is 12k UIDs that are merely *older* than the
/// window, not gone. Only a known UID that falls between the window's lowest
/// and highest UID is provably absent from the server: the server listed every
/// UID in that range and this one was not among them.
///
/// Negative UIDs are skipped: they are the `-id` sentinel a local move parks a
/// row on (see [`crate::store::write`]), a row waiting for the destination's
/// next sync to give it a real UID, not something the server ever knew about.
///
/// The same clamp saves the other placeholder, from the other end of the
/// number line: a Sent copy appended without an `APPENDUID` is stored under
/// [`crate::ingest::graph_uid`], a 63-bit hash of the Message-ID that is
/// always far above any real `hi`, so it falls outside the window's range and
/// survives until a real sync rebinds it to the server's UID.
pub fn vanished_uids(known: &std::collections::HashSet<i64>, window: &[u32]) -> Vec<u32> {
    let (Some(&lo), Some(&hi)) = (window.iter().min(), window.iter().max()) else {
        return Vec::new();
    };
    let listed: std::collections::HashSet<u32> = window.iter().copied().collect();
    let mut out: Vec<u32> = known
        .iter()
        .filter(|&&uid| uid >= lo as i64 && uid <= hi as i64)
        .map(|&uid| uid as u32)
        .filter(|uid| !listed.contains(uid))
        .collect();
    out.sort_unstable();
    out
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
    let empty = |state: MailboxState| StoreFetch {
        messages: Vec::new(),
        skipped: 0,
        known_flags: Vec::new(),
        state,
        vanished: Vec::new(),
        uidvalidity_reset,
    };

    let uids = session
        .uid_search("ALL")
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;
    span.mark("uid_search");
    if uids.is_empty() {
        return Ok(empty(state));
    }

    let mut uid_list: Vec<u32> = uids.into_iter().collect();
    uid_list.sort_unstable();
    let mut window: Vec<u32> = match limit {
        Some(n) => uid_list.into_iter().rev().take(n).collect(),
        None => uid_list,
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
    // A UIDVALIDITY reset empties `known_uids`, so this is empty too: the
    // server renumbering says nothing about which messages are gone, and the
    // rows are about to be rebound through their Message-IDs.
    let vanished = vanished_uids(&known_uids, &window);
    if !vanished.is_empty() {
        info!(
            "Store fetch for '{}': {} row(s) inside the window range are no longer on the server",
            mailbox,
            vanished.len()
        );
    }

    if new_uids.is_empty() {
        return Ok(StoreFetch {
            messages: Vec::new(),
            skipped,
            known_flags,
            state,
            vanished,
            uidvalidity_reset,
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

    Ok(StoreFetch {
        messages: out,
        skipped,
        known_flags,
        state,
        vanished,
        uidvalidity_reset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A UID inside the window's range that the server did not list is gone.
    #[test]
    fn a_uid_missing_from_the_middle_of_the_window_is_vanished() {
        let known = HashSet::from([10, 11, 12, 13]);
        assert_eq!(vanished_uids(&known, &[10, 12, 13]), vec![11]);
    }

    /// The clamp: with a small `-n` the window is the tail of the mailbox, and
    /// everything older than it is outside the range the server proved
    /// anything about. Without this the first quick sync would delete the
    /// whole archive.
    #[test]
    fn uids_below_the_window_survive() {
        let known: HashSet<i64> = (1..=100).collect();
        assert_eq!(vanished_uids(&known, &[98, 99, 100]), Vec::<u32>::new());
    }

    /// Symmetrically, a UID above the window's highest is not covered either:
    /// only a row that was optimistically written ahead of the server can be
    /// there, and the next fetch will list it.
    #[test]
    fn uids_above_the_window_survive() {
        let known = HashSet::from([5, 6, 7, 42]);
        assert_eq!(vanished_uids(&known, &[5, 7]), vec![6]);
    }

    /// A UIDVALIDITY reset hands `resolve` an empty known set, which must
    /// produce an empty prune: a renumbering says nothing about what is gone.
    #[test]
    fn an_empty_known_set_prunes_nothing() {
        assert_eq!(vanished_uids(&HashSet::new(), &[1, 2, 3]), Vec::<u32>::new());
    }

    /// An empty window is "the server told us nothing", not "the mailbox is
    /// gone".
    #[test]
    fn an_empty_window_prunes_nothing() {
        let known = HashSet::from([1, 2, 3]);
        assert_eq!(vanished_uids(&known, &[]), Vec::<u32>::new());
    }

    /// The negative sentinel of a locally moved row is not a server UID and
    /// must never be pruned, whatever the window covers.
    #[test]
    fn a_locally_moved_rows_sentinel_uid_is_never_pruned() {
        let known = HashSet::from([-7, 4, 5]);
        assert_eq!(vanished_uids(&known, &[4, 5]), Vec::<u32>::new());
    }
}
