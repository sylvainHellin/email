//! Per-message server ops: archive, move, delete and the `\Seen` flag.
//!
//! Every one of them names its target by `Message-ID` and touches nothing
//! locally: the local half of a mutation is the store row the caller wrote
//! optimistically (`crate::store::write`), and the result here is what tells it
//! whether to keep that write or put it back.

use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use log::info;

use super::search::message_id_search_term;
use super::{pool, ImapSession};
use crate::config::ImapConfig;

/// Run one op on a session borrowed from the persistent pool (#0041).
///
/// Every public entry point below is this wrapper around an `_on_session`
/// function, which is what stopped each queued mutation from paying its own
/// TCP + TLS + LOGIN. The three rules the wrapper enforces once, so no call
/// site has to remember them:
///
/// - the op always `SELECT`s: the connection arrives selected on whatever the
///   previous borrower was doing, or on nothing at all;
/// - a failed op poisons the session rather than returning it, because a
///   command whose response was not read to the end leaves bytes in the stream
///   that the next borrower would misread as its own answer;
/// - the session is *not* logged out; dropping the guard returns it.
///
/// The typed [`crate::ops::NotFoundOnServer`] travels through as an ordinary
/// error. That does poison a perfectly healthy connection, which costs one
/// reconnect on a path that is already rare (a replayed op whose target has
/// moved), and is the conservative side of the trade.
async fn with_pooled<F, T>(imap_config: &ImapConfig, op: F) -> Result<T>
where
    F: AsyncFnOnce(&mut ImapSession) -> Result<T>,
{
    let mut pooled = pool::checkout(imap_config).await?;
    let out = op(pooled.session()).await;
    pooled.check(out)
}

/// Move an email to a different mailbox on the IMAP server (#0018).
pub async fn move_email_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    source_mailbox: &str,
    dest_mailbox: &str,
) -> Result<()> {
    with_pooled(imap_config, async |s: &mut ImapSession| {
        move_email_on_session(s, message_id, source_mailbox, dest_mailbox).await
    })
    .await
}

/// Delete an email on the server: SELECT, find by Message-ID, \Deleted, EXPUNGE.
pub async fn delete_email_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    source_mailbox: &str,
) -> Result<()> {
    with_pooled(imap_config, async |s: &mut ImapSession| {
        delete_email_on_session(s, message_id, source_mailbox).await
    })
    .await
}

/// Mark an email as read (`\Seen`) on the IMAP server.
pub async fn mark_read_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    with_pooled(imap_config, async |s: &mut ImapSession| {
        mark_read_on_session(s, message_id, mailbox).await
    })
    .await
}

/// Mark an email as unread (remove `\Seen`) on the IMAP server.
pub async fn mark_unread_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    with_pooled(imap_config, async |s: &mut ImapSession| {
        mark_unread_on_session(s, message_id, mailbox).await
    })
    .await
}

/// Add one flag to a message on the server, by `Message-ID` (#TKT-0051).
pub async fn add_flag_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
    flag: &str,
) -> Result<()> {
    with_pooled(imap_config, async |s: &mut ImapSession| {
        add_flag_on_session(s, message_id, mailbox, flag).await
    })
    .await
}

/// Remove one flag from a message on the server, by `Message-ID` (#0007).
pub async fn remove_flag_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
    flag: &str,
) -> Result<()> {
    with_pooled(imap_config, async |s: &mut ImapSession| {
        remove_flag_on_session(s, message_id, mailbox, flag).await
    })
    .await
}

/// Move an email to a different mailbox on the IMAP server (#0018).
///
/// SELECTs `source_mailbox`, finds the message by Message-ID, then
/// UID COPY + \Deleted + EXPUNGE -- the same machinery as archiving
/// (which is a move with a fixed destination). COPY+EXPUNGE is used
/// instead of MOVE so servers without the MOVE extension work too;
/// COPY preserves flags, so read/unread state survives the move.
async fn move_email_on_session(
    session: &mut ImapSession,
    message_id: &str,
    source_mailbox: &str,
    dest_mailbox: &str,
) -> Result<()> {
    info!(
        "Moving email on server: Message-ID={} {} -> {}",
        message_id, source_mailbox, dest_mailbox
    );
    session
        .select(source_mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", source_mailbox, e))?;

    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        // A typed not-found, so the durable queue's drain can converge a
        // replay whose move already landed (#0039 review); a direct CLI/TUI
        // caller still sees the same message through `Display`.
        return Err(crate::ops::NotFoundOnServer {
            message_id: message_id.to_string(),
            mailbox: Some(source_mailbox.to_string()),
        }
        .into());
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    let uid_str = uid.to_string();

    session
        .uid_copy(&uid_str, dest_mailbox)
        .await
        .map_err(|e| anyhow!("Failed to copy email to {}: {}", dest_mailbox, e))?;

    session
        .uid_store(&uid_str, "+FLAGS (\\Deleted)")
        .await
        .map_err(|e| anyhow!("Failed to mark email as deleted: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    session
        .expunge()
        .await
        .map_err(|e| anyhow!("Failed to expunge: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect expunge response: {}", e))?;

    Ok(())
}

/// Delete an email on the server: SELECT, find by Message-ID, \Deleted, EXPUNGE.
///
/// `source_mailbox` is the server-side folder the message is in. It used to be
/// hardcoded to `INBOX`, which was only ever right because the file build could
/// not tell where a message lived; the store row knows its mailbox, so the
/// caller passes the folder it maps to.
async fn delete_email_on_session(
    session: &mut ImapSession,
    message_id: &str,
    source_mailbox: &str,
) -> Result<()> {
    info!("Deleting email on server: Message-ID={}", message_id);
    session
        .select(source_mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", source_mailbox, e))?;

    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        // Typed not-found: the queue drain converges a replay whose delete
        // already landed (#0039 review), while a direct caller still errors.
        return Err(crate::ops::NotFoundOnServer {
            message_id: message_id.to_string(),
            mailbox: Some(source_mailbox.to_string()),
        }
        .into());
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    let uid_str = uid.to_string();

    session
        .uid_store(&uid_str, "+FLAGS (\\Deleted)")
        .await
        .map_err(|e| anyhow!("Failed to mark email as deleted: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    session
        .expunge()
        .await
        .map_err(|e| anyhow!("Failed to expunge: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect expunge response: {}", e))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read / unread (\Seen flag)
// ---------------------------------------------------------------------------

/// Mark an email as read (\Seen) on the IMAP server.
async fn mark_read_on_session(
    session: &mut ImapSession,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    info!("Marking email as read on server: Message-ID={}", message_id);
    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", mailbox, e))?;

    let query = message_id_search_term(message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        return Ok(()); // Not found on server -- not an error
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    session
        .uid_store(&uid.to_string(), "+FLAGS (\\Seen)")
        .await
        .map_err(|e| anyhow!("Failed to set \\Seen flag: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    Ok(())
}

/// Add one flag to a message on the server, by `Message-ID` (#TKT-0051).
///
/// The write half of the second status axis: `\Answered` when a reply goes
/// out, `$Forwarded` when a forward does. `+FLAGS` rather than `FLAGS`, so
/// nothing the server already holds is cleared, and a message the search does
/// not find is not an error -- the row may have been moved or deleted
/// elsewhere since the draft was written, and the message still went out.
///
/// A server that refuses the keyword (`PERMANENTFLAGS` without `\*`) fails
/// here, which the caller logs and lives with: the next sync overwrites the
/// local bit with what the server believes, so the two never drift silently.
async fn add_flag_on_session(
    session: &mut ImapSession,
    message_id: &str,
    mailbox: &str,
    flag: &str,
) -> Result<()> {
    info!("Adding {flag} on server: Message-ID={message_id} in {mailbox}");
    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", mailbox, e))?;

    let query = message_id_search_term(message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        return Ok(());
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    session
        .uid_store(&uid.to_string(), &format!("+FLAGS ({flag})"))
        .await
        .map_err(|e| anyhow!("Failed to set the {} flag: {}", flag, e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    Ok(())
}

/// Remove one flag from a message on the server, by `Message-ID` (#0007).
///
/// The clear half of [`add_flag_on_server`], used to unflag a message: `-FLAGS`
/// so only the named flag is touched. A message the search does not find is not
/// an error, same as the add: the row may have moved or been deleted elsewhere,
/// and the next sync restates whatever the server holds either way.
async fn remove_flag_on_session(
    session: &mut ImapSession,
    message_id: &str,
    mailbox: &str,
    flag: &str,
) -> Result<()> {
    info!("Removing {flag} on server: Message-ID={message_id} in {mailbox}");
    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", mailbox, e))?;

    let query = message_id_search_term(message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        return Ok(());
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    session
        .uid_store(&uid.to_string(), &format!("-FLAGS ({flag})"))
        .await
        .map_err(|e| anyhow!("Failed to remove the {} flag: {}", flag, e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    Ok(())
}

/// Mark an email as unread (remove \Seen) on the IMAP server.
async fn mark_unread_on_session(
    session: &mut ImapSession,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    info!("Marking email as unread on server: Message-ID={}", message_id);
    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", mailbox, e))?;

    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        return Ok(());
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    session
        .uid_store(&uid.to_string(), "-FLAGS (\\Seen)")
        .await
        .map_err(|e| anyhow!("Failed to remove \\Seen flag: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    Ok(())
}
