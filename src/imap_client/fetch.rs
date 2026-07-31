use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use log::info;

use super::{ImapSession, search::{FetchCriteria, build_imap_search_query}, open_imap_session};
use crate::config::ImapConfig;
use crate::parse::{compress_uid_set, parse_rfc822_to_fetched_email, FetchedEmail};
use crate::timing::TimingSpan;

/// Extract Message-ID from raw header bytes (from BODY.PEEK[HEADER.FIELDS (Message-ID)]).
pub(super) fn parse_message_id_from_header_bytes(header_bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(header_bytes).ok()?;
    for line in text.lines() {
        let trimmed = line.trim();
        // Case-insensitive prefix check: some servers use MESSAGE-ID, MESSAGE-Id, etc.
        if trimmed.len() >= 11 && trimmed.as_bytes()[10] == b':' {
            let prefix = &trimmed[..10];
            if prefix.eq_ignore_ascii_case("Message-ID") {
                let id = trimmed[11..].trim();
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// Fetch only Message-IDs from a mailbox on an existing session (lightweight, no body download).
pub async fn fetch_server_message_ids_on_session(
    session: &mut ImapSession,
    mailbox: &str,
) -> Result<HashSet<String>> {
    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select mailbox '{}': {}", mailbox, e))?;

    let uids = session
        .uid_search("ALL")
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        return Ok(HashSet::new());
    }

    let mut uid_list: Vec<u32> = uids.into_iter().collect();
    uid_list.sort();
    let uid_set = compress_uid_set(&uid_list);

    let fetched: Vec<_> = session
        .uid_fetch(&uid_set, "BODY.PEEK[HEADER.FIELDS (Message-ID)]")
        .await
        .map_err(|e| anyhow!("Failed to fetch Message-IDs: {}", e))?
        .try_collect()
        .await
        .map_err(|e| anyhow!("Failed to collect Message-IDs: {}", e))?;

    let mut ids = HashSet::new();
    for msg in fetched.iter() {
        if let Some(header_bytes) = msg.header() {
            if let Some(mid) = parse_message_id_from_header_bytes(header_bytes) {
                ids.insert(mid);
            }
        }
    }

    Ok(ids)
}

/// Fetch only Message-IDs from a mailbox (lightweight, no body download).
/// Opens and closes its own IMAP session.
pub async fn fetch_server_message_ids(
    imap_config: &ImapConfig,
    mailbox: &str,
) -> Result<HashSet<String>> {
    info!("Fetching Message-IDs from mailbox '{}'", mailbox);
    let mut session = open_imap_session(imap_config).await?;
    let ids = fetch_server_message_ids_on_session(&mut session, mailbox).await?;
    session.logout().await.ok();
    Ok(ids)
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

/// Two-pass fetch: first fetch Message-ID headers + FLAGS (lightweight), then only
/// download full RFC822 for emails not already present locally.
/// Returns (new_emails, num_already_known_in_window, read_flags_for_known_emails).
/// The third element maps message_id -> is_seen for emails that already exist locally,
/// enabling the caller to sync read status from the server.
///
/// Pass 1 always covers the full `limit` window, even when nothing is new:
/// the collected \Seen flags are the only server->local read-status channel,
/// so shrinking the window silently drops flag changes made in other clients
/// (ticket #0004). A former "adaptive probe" fast path that returned early
/// after checking only the newest 10 UIDs did exactly that and was removed;
/// pass 2 (body download) is still skipped entirely when nothing is new.
pub async fn fetch_new_emails_on_session(
    session: &mut ImapSession,
    mailbox: &str,
    limit: Option<usize>,
    known_ids: &HashSet<String>,
) -> Result<(Vec<FetchedEmail>, usize, HashMap<String, bool>, Option<super::sync::MailboxState>)> {
    let mut span = TimingSpan::with_context("fetch_new_emails", mailbox.to_string());

    let imap_mailbox = session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select mailbox '{}': {}", mailbox, e))?;
    span.mark("select");
    let mb_state = Some(super::sync::MailboxState {
        uid_validity: imap_mailbox.uid_validity,
        uid_next: imap_mailbox.uid_next,
        exists: imap_mailbox.exists,
    });

    let uids = session
        .uid_search("ALL")
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;
    span.mark("uid_search");

    if uids.is_empty() {
        return Ok((Vec::new(), 0, HashMap::new(), mb_state));
    }

    let mut uid_list: Vec<u32> = uids.into_iter().collect();
    uid_list.sort();

    let selected_uids: Vec<u32> = match limit {
        Some(n) => uid_list.into_iter().rev().take(n).collect(),
        None => uid_list,
    };

    if selected_uids.is_empty() {
        return Ok((Vec::new(), 0, HashMap::new(), mb_state));
    }

    let checked_count = selected_uids.len();

    // Pass 1: Fetch Message-ID headers + FLAGS (~100 bytes/msg)
    let uid_set = compress_uid_set(&selected_uids);
    let header_fetched: Vec<_> = session
        .uid_fetch(&uid_set, "(UID BODY.PEEK[HEADER.FIELDS (Message-ID)] FLAGS)")
        .await
        .map_err(|e| anyhow!("Failed to fetch Message-ID headers: {}", e))?
        .try_collect()
        .await
        .map_err(|e| anyhow!("Failed to collect Message-ID headers: {}", e))?;
    span.mark("pass1_headers");

    // Identify UIDs whose Message-ID is not in known_ids,
    // and collect \Seen flags for known emails.
    let mut new_uids: Vec<u32> = Vec::new();
    let mut known_flags: HashMap<String, bool> = HashMap::new();
    for msg in header_fetched.iter() {
        let uid = match msg.uid {
            Some(u) => u,
            None => continue,
        };
        let mid = msg.header().and_then(parse_message_id_from_header_bytes);
        match mid {
            Some(ref id) if known_ids.contains(id) => {
                // Already known locally -- capture \Seen flag for read status sync
                let is_seen = msg.flags().any(|f| matches!(f, async_imap::types::Flag::Seen));
                known_flags.insert(id.clone(), is_seen);
            }
            _ => {
                new_uids.push(uid);
            }
        }
    }

    let skipped = checked_count - new_uids.len();

    if new_uids.is_empty() {
        return Ok((Vec::new(), skipped, known_flags, mb_state));
    }
    span.mark("diff_new_uids");

    info!(
        "Two-pass fetch for '{}': {} new, {} skipped (out of {} checked)",
        mailbox,
        new_uids.len(),
        skipped,
        checked_count
    );

    // Pass 2: Fetch full message body only for genuinely new messages
    let new_uid_set = compress_uid_set(&new_uids);
    let fetched: Vec<_> = session
        .uid_fetch(&new_uid_set, "(BODY.PEEK[] FLAGS)")
        .await
        .map_err(|e| anyhow!("Failed to fetch emails: {}", e))?
        .try_collect()
        .await
        .map_err(|e| anyhow!("Failed to collect emails: {}", e))?;
    span.mark("pass2_bodies");

    let mut emails = Vec::new();
    for msg in fetched.iter() {
        let body_raw = msg.body().unwrap_or_default();
        if let Some(mut email) = parse_rfc822_to_fetched_email(body_raw) {
            email.is_read = msg.flags().any(|f| matches!(f, async_imap::types::Flag::Seen));
            emails.push(email);
        }
    }
    span.mark("parse_rfc822");

    Ok((emails, skipped, known_flags, mb_state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_message_id_from_header_bytes_standard() {
        let header = b"Message-ID: <abc123@mail.example.com>";
        assert_eq!(
            parse_message_id_from_header_bytes(header),
            Some("<abc123@mail.example.com>".to_string())
        );
    }

    #[test]
    fn test_parse_message_id_from_header_bytes_case_insensitive() {
        let header = b"Message-Id: <def456@mail.example.com>\nOther: header";
        assert_eq!(
            parse_message_id_from_header_bytes(header),
            Some("<def456@mail.example.com>".to_string())
        );

        let header = b"message-id: <xyz@mail.example.com>";
        assert_eq!(
            parse_message_id_from_header_bytes(header),
            Some("<xyz@mail.example.com>".to_string())
        );
    }

    #[test]
    fn test_parse_message_id_from_header_bytes_with_whitespace() {
        let header = b"Message-ID:   <whitespace@mail.example.com>  ";
        assert_eq!(
            parse_message_id_from_header_bytes(header),
            Some("<whitespace@mail.example.com>".to_string())
        );
    }

    #[test]
    fn test_parse_message_id_from_header_bytes_not_found() {
        let header = b"From: alice@example.com\nSubject: Test\nDate: Mon, 1 Jan 2024";
        assert_eq!(parse_message_id_from_header_bytes(header), None);
    }

    #[test]
    fn test_parse_message_id_from_header_bytes_empty_value() {
        let header = b"Message-ID: ";
        assert_eq!(parse_message_id_from_header_bytes(header), None);

        let header = b"Message-ID:";
        assert_eq!(parse_message_id_from_header_bytes(header), None);
    }

    #[test]
    fn test_parse_message_id_from_header_bytes_utf8() {
        let header = b"Message-ID: <utf8-test-\xc2\xb5@mail.example.com>";
        assert!(parse_message_id_from_header_bytes(header).is_some());
    }
}
