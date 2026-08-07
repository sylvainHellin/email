//! Per-message server ops: archive, move, delete and the `\Seen` flag.
//!
//! Every one of them names its target by `Message-ID` and touches nothing
//! locally: the local half of a mutation is the store row the caller wrote
//! optimistically (`crate::store::write`), and the result here is what tells it
//! whether to keep that write or put it back.

use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use log::info;

use super::open_imap_session;
use super::search::message_id_search_term;
use crate::config::ImapConfig;

/// Move an email to a different mailbox on the IMAP server (#0018).
///
/// SELECTs `source_mailbox`, finds the message by Message-ID, then
/// UID COPY + \Deleted + EXPUNGE -- the same machinery as archiving
/// (which is a move with a fixed destination). COPY+EXPUNGE is used
/// instead of MOVE so servers without the MOVE extension work too;
/// COPY preserves flags, so read/unread state survives the move.
pub async fn move_email_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    source_mailbox: &str,
    dest_mailbox: &str,
) -> Result<()> {
    info!(
        "Moving email on server: Message-ID={} {} -> {}",
        message_id, source_mailbox, dest_mailbox
    );
    let mut session = open_imap_session(imap_config).await?;

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
        session.logout().await.ok();
        return Err(anyhow!(
            "Email with Message-ID {} not found in {} on server",
            message_id,
            source_mailbox
        ));
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

    session.logout().await.ok();
    Ok(())
}

/// Delete an email on the server: SELECT, find by Message-ID, \Deleted, EXPUNGE.
///
/// `source_mailbox` is the server-side folder the message is in. It used to be
/// hardcoded to `INBOX`, which was only ever right because the file build could
/// not tell where a message lived; the store row knows its mailbox, so the
/// caller passes the folder it maps to.
pub async fn delete_email_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    source_mailbox: &str,
) -> Result<()> {
    info!("Deleting email on server: Message-ID={}", message_id);
    let mut session = open_imap_session(imap_config).await?;

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
        session.logout().await.ok();
        return Err(anyhow!(
            "Email with Message-ID {} not found in {} on server",
            message_id,
            source_mailbox
        ));
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

    session.logout().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Read / unread (\Seen flag)
// ---------------------------------------------------------------------------

/// Mark an email as read (\Seen) on the IMAP server.
pub async fn mark_read_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    info!("Marking email as read on server: Message-ID={}", message_id);
    let mut session = open_imap_session(imap_config).await?;

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
        session.logout().await.ok();
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

    session.logout().await.ok();
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
pub async fn add_flag_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
    flag: &str,
) -> Result<()> {
    info!("Adding {flag} on server: Message-ID={message_id} in {mailbox}");
    let mut session = open_imap_session(imap_config).await?;

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
        session.logout().await.ok();
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

    session.logout().await.ok();
    Ok(())
}

/// Remove one flag from a message on the server, by `Message-ID` (#0007).
///
/// The clear half of [`add_flag_on_server`], used to unflag a message: `-FLAGS`
/// so only the named flag is touched. A message the search does not find is not
/// an error, same as the add: the row may have moved or been deleted elsewhere,
/// and the next sync restates whatever the server holds either way.
pub async fn remove_flag_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
    flag: &str,
) -> Result<()> {
    info!("Removing {flag} on server: Message-ID={message_id} in {mailbox}");
    let mut session = open_imap_session(imap_config).await?;

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
        session.logout().await.ok();
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

    session.logout().await.ok();
    Ok(())
}

/// Mark an email as unread (remove \Seen) on the IMAP server.
pub async fn mark_unread_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    info!("Marking email as unread on server: Message-ID={}", message_id);
    let mut session = open_imap_session(imap_config).await?;

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
        session.logout().await.ok();
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

    session.logout().await.ok();
    Ok(())
}
