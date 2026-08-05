use std::collections::HashSet;

use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use log::info;

use super::{ImapSession, search::{FetchCriteria, build_imap_search_query}, open_imap_session};
use crate::config::ImapConfig;
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
/// Returns the new messages, the number of UIDs already held, the `\Seen`
/// state of those already-held UIDs, and the mailbox state from SELECT.
pub async fn fetch_new_raw_on_session(
    session: &mut ImapSession,
    mailbox: &str,
    limit: Option<usize>,
    known_uids: &HashSet<i64>,
) -> Result<(Vec<FetchedRaw>, usize, Vec<(u32, bool)>, MailboxState)> {
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

    let uids = session
        .uid_search("ALL")
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;
    span.mark("uid_search");
    if uids.is_empty() {
        return Ok((Vec::new(), 0, Vec::new(), state));
    }

    let mut uid_list: Vec<u32> = uids.into_iter().collect();
    uid_list.sort_unstable();
    let mut window: Vec<u32> = match limit {
        Some(n) => uid_list.into_iter().rev().take(n).collect(),
        None => uid_list,
    };
    window.sort_unstable();
    if window.is_empty() {
        return Ok((Vec::new(), 0, Vec::new(), state));
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
        return Ok((Vec::new(), skipped, known_flags, state));
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

    Ok((out, skipped, known_flags, state))
}
