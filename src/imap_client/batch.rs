//! Batched archive and delete: many messages over one IMAP connection.
//!
//! The per-message ops in [`super::ops`] open a session each, which is fine for
//! one keystroke and wasteful for a selection of forty. These take the whole
//! list, open one session, run one SEARCH + STORE per message and end with a
//! single EXPUNGE.
//!
//! They act on the server only. The local half of a mutation is the store row
//! the caller already wrote optimistically (`crate::store::write`), so a result
//! here is what tells the caller whether to keep that write or put it back.

use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use log::info;

use super::{open_imap_session, ImapSession};
use crate::config::ImapConfig;

/// SEARCH + UID COPY + UID STORE \Deleted on an existing session.
/// Does NOT EXPUNGE -- caller batches a single EXPUNGE at the end.
async fn move_single_on_session(
    session: &mut ImapSession,
    message_id: &str,
    dest_mailbox: &str,
) -> Result<()> {
    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        info!(
            "Message-ID {} not found in the source mailbox (already moved on server), skipping",
            message_id
        );
        return Ok(());
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

    Ok(())
}

/// SEARCH + UID STORE \Deleted on an existing session.
/// Does NOT EXPUNGE -- caller batches a single EXPUNGE at the end.
async fn delete_single_on_session(session: &mut ImapSession, message_id: &str) -> Result<()> {
    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        info!(
            "Message-ID {} not found in the source mailbox (already removed on server), skipping",
            message_id
        );
        return Ok(());
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

    Ok(())
}

/// Move every message to `dest_mailbox` over one connection, index-aligned with
/// `message_ids`. Archiving is this with the archive folder as destination.
pub async fn batch_move_on_server(
    imap_config: &ImapConfig,
    message_ids: &[String],
    source_mailbox: &str,
    dest_mailbox: &str,
) -> Vec<Result<()>> {
    run_batch(
        imap_config,
        message_ids,
        source_mailbox,
        BatchOp::Move { dest: dest_mailbox },
    )
    .await
}

/// Delete every message over one connection, index-aligned with `message_ids`.
pub async fn batch_delete_on_server(
    imap_config: &ImapConfig,
    message_ids: &[String],
    source_mailbox: &str,
) -> Vec<Result<()>> {
    run_batch(imap_config, message_ids, source_mailbox, BatchOp::Delete).await
}

/// Which per-message op [`run_batch`] runs on the open session.
#[derive(Debug, Clone, Copy)]
enum BatchOp<'a> {
    Move { dest: &'a str },
    Delete,
}

/// The shape both batches share: open once, SELECT once, run `op` per message,
/// EXPUNGE once when anything succeeded. A connection or SELECT failure is
/// reported against every message rather than swallowed, so the caller rolls
/// back all of them.
async fn run_batch(
    imap_config: &ImapConfig,
    message_ids: &[String],
    source_mailbox: &str,
    op: BatchOp<'_>,
) -> Vec<Result<()>> {
    if message_ids.is_empty() {
        return Vec::new();
    }

    let mut session = match open_imap_session(imap_config).await {
        Ok(session) => session,
        Err(e) => {
            return message_ids
                .iter()
                .map(|_| Err(anyhow!("IMAP connection failed: {}", e)))
                .collect()
        }
    };

    if let Err(e) = session.select(source_mailbox).await {
        let msg = format!("Failed to select {}: {}", source_mailbox, e);
        session.logout().await.ok();
        return message_ids.iter().map(|_| Err(anyhow!("{}", msg))).collect();
    }

    let mut results: Vec<Result<()>> = Vec::with_capacity(message_ids.len());
    for message_id in message_ids {
        results.push(match op {
            BatchOp::Move { dest } => {
                move_single_on_session(&mut session, message_id, dest).await
            }
            BatchOp::Delete => delete_single_on_session(&mut session, message_id).await,
        });
    }

    if results.iter().any(Result::is_ok) {
        match session.expunge().await {
            Ok(stream) => {
                if let Err(e) = stream.try_collect::<Vec<_>>().await {
                    info!("Batch EXPUNGE collect failed (non-fatal): {}", e);
                }
            }
            Err(e) => info!("Batch EXPUNGE failed (non-fatal): {}", e),
        }
    }
    session.logout().await.ok();

    results
}
