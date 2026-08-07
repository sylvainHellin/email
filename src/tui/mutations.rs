//! The two halves of a mutation, and the seam between them.
//!
//! A flag, a move, an archive or a delete is one local write and one server op.
//! This module owns the pairing: [`prepare_move`], [`prepare_delete`],
//! [`prepare_read_flag`] and [`prepare_flag`] apply the local write to the store immediately and
//! hand back the [`ServerOp`] the caller fires in the background, together with
//! the row's previous coordinates so a failed op can be rolled back.
//!
//! Splitting it out of `actions.rs` is what makes the pairing testable: an
//! action arm needs a live terminal and a background channel, while a prepare
//! function needs only a store, so a test can assert both halves at once (the
//! row changed, and *this* op was dispatched) over an ingested fixture.
//!
//! Durability is unchanged from the pre-store build: the local write lands
//! first, the server op is fire-and-forget, and a crash between them loses the
//! op. The queue that fixes that is
//! [#0039](../../docs/tickets/0039-pending-ops-queue.md).

use anyhow::Result;

use super::app::MessageRef;
use crate::config::{GraphConfig, ImapConfig};
use crate::store::write::{self, MutatedRow};
use crate::store::{open_store, BlobStore, Store};

/// What the background thread will ask the server to do.
///
/// Every variant names the message by `Message-ID`, which is the only handle
/// both backends share and the one that survives the local row moving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServerOp {
    /// Move between server folders. Archiving is this with the archive folder
    /// as destination, exactly as `move_email_on_server` has always framed it.
    Move {
        message_id: String,
        source_mailbox: String,
        dest_mailbox: String,
    },
    Delete {
        message_id: String,
        source_mailbox: String,
    },
    SetRead {
        message_id: String,
        mailbox: String,
        read: bool,
    },
    /// Toggle the `\Flagged` star (#0007). Like [`ServerOp::SetRead`], the
    /// server is truth on the IMAP path, so a local toggle needs a UID STORE
    /// write the next sync restates.
    SetFlagged {
        message_id: String,
        mailbox: String,
        flagged: bool,
    },
}

/// One mutation that has already been applied locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepared {
    /// The row's coordinates *before* the local write, for the rollback.
    pub previous: MutatedRow,
    /// The op to fire at the server.
    pub op: ServerOp,
}

impl Prepared {
    /// The `messages.id` this mutation applied to.
    pub fn msg(&self) -> MessageRef {
        MessageRef::new(self.previous.id)
    }
}

/// Move rows into `dest_mailbox` (the store's mailbox key) and return the ops
/// that carry them to `dest_server` on the server.
///
/// Rows that are already gone are skipped rather than reported: a message the
/// store no longer holds cannot be moved, and a second mutation racing the
/// first is a user action rather than a bug.
pub(crate) fn prepare_move(
    store: &Store,
    msgs: &[MessageRef],
    dest_mailbox: &str,
    source_server: &str,
    dest_server: &str,
) -> Vec<Prepared> {
    let mut out = Vec::with_capacity(msgs.len());
    for msg in msgs {
        match write::move_row(store, msg.row_id(), dest_mailbox) {
            Ok(Some(previous)) => {
                let op = ServerOp::Move {
                    message_id: previous.message_id.clone(),
                    source_mailbox: source_server.to_string(),
                    dest_mailbox: dest_server.to_string(),
                };
                out.push(Prepared { previous, op });
            }
            Ok(None) => log::warn!("[store] {msg} has no row to move"),
            Err(e) => log::warn!("[store] moving {msg} failed: {e:#}"),
        }
    }
    out
}

/// Delete rows and return the ops that delete them on the server.
pub(crate) fn prepare_delete(
    store: &Store,
    blobs: &BlobStore,
    msgs: &[MessageRef],
    source_server: &str,
) -> Vec<Prepared> {
    let mut out = Vec::with_capacity(msgs.len());
    for msg in msgs {
        match write::delete_row(store, blobs, msg.row_id()) {
            Ok(Some(previous)) => {
                let op = ServerOp::Delete {
                    message_id: previous.message_id.clone(),
                    source_mailbox: source_server.to_string(),
                };
                out.push(Prepared { previous, op });
            }
            Ok(None) => log::warn!("[store] {msg} has no row to delete"),
            Err(e) => log::warn!("[store] deleting {msg} failed: {e:#}"),
        }
    }
    out
}

/// Set the read flag on rows and return the ops that mirror it to the server.
pub(crate) fn prepare_read_flag(
    store: &Store,
    msgs: &[MessageRef],
    read: bool,
    server_mailbox: &str,
) -> Vec<Prepared> {
    let mut out = Vec::with_capacity(msgs.len());
    for msg in msgs {
        let previous = match write::row_coordinates(store, msg.row_id()) {
            Ok(Some(row)) => row,
            Ok(None) => {
                log::warn!("[store] {msg} has no row to flag");
                continue;
            }
            Err(e) => {
                log::warn!("[store] reading {msg} failed: {e:#}");
                continue;
            }
        };
        if let Err(e) = write::set_read(store, msg.row_id(), read) {
            log::warn!("[store] flagging {msg} failed: {e:#}");
            continue;
        }
        let op = ServerOp::SetRead {
            message_id: previous.message_id.clone(),
            mailbox: server_mailbox.to_string(),
            read,
        };
        out.push(Prepared { previous, op });
    }
    out
}

/// Set the `\Flagged` star on rows and return the ops that mirror it to the
/// server (#0007). The local write lands first, exactly as the read toggle
/// does; the star rides the same `flags` column through a read-modify-write.
pub(crate) fn prepare_flag(
    store: &Store,
    msgs: &[MessageRef],
    flagged: bool,
    server_mailbox: &str,
) -> Vec<Prepared> {
    let mut out = Vec::with_capacity(msgs.len());
    for msg in msgs {
        let previous = match write::row_coordinates(store, msg.row_id()) {
            Ok(Some(row)) => row,
            Ok(None) => {
                log::warn!("[store] {msg} has no row to flag");
                continue;
            }
            Err(e) => {
                log::warn!("[store] reading {msg} failed: {e:#}");
                continue;
            }
        };
        if let Err(e) = write::set_flagged(store, msg.row_id(), flagged) {
            log::warn!("[store] starring {msg} failed: {e:#}");
            continue;
        }
        let op = ServerOp::SetFlagged {
            message_id: previous.message_id.clone(),
            mailbox: server_mailbox.to_string(),
            flagged,
        };
        out.push(Prepared { previous, op });
    }
    out
}

/// Put a moved row back after its server op failed.
///
/// A delete has no counterpart here on purpose: the row is gone and there is
/// nothing to restore it from. The next sync of the mailbox refetches the UID
/// and ingest re-inserts it, which is what the failure status already tells the
/// user to do (see `crate::store::write`).
pub(crate) fn rollback_move(account: &str, previous: &MutatedRow) {
    let Some(store) = open_store(account) else {
        log::warn!("[store] no store to roll {} back into", previous.id);
        return;
    };
    if let Err(e) = write::restore_row(&store, previous) {
        log::warn!("[store] rolling back message #{} failed: {e:#}", previous.id);
    }
}

/// Put a read flag back after its server op failed.
pub(crate) fn rollback_read_flag(account: &str, msg: MessageRef, read: bool) {
    let Some(store) = open_store(account) else {
        return;
    };
    if let Err(e) = write::set_read(&store, msg.row_id(), read) {
        log::warn!("[store] rolling back the read flag on {msg} failed: {e:#}");
    }
}

/// Put the `\Flagged` star back after its server op failed (#0007).
pub(crate) fn rollback_flag(account: &str, msg: MessageRef, flagged: bool) {
    let Some(store) = open_store(account) else {
        return;
    };
    if let Err(e) = write::set_flagged(&store, msg.row_id(), flagged) {
        log::warn!("[store] rolling back the flag on {msg} failed: {e:#}");
    }
}

/// The backend a batch of ops runs against, resolved on the UI thread before
/// the spawn so a missing config never leaves a half-applied mutation behind.
pub(crate) enum Backend {
    Imap(Box<ImapConfig>),
    Graph(Box<GraphConfig>),
}

/// Run one op, index-aligned results for a whole batch.
///
/// IMAP batches a homogeneous list over a single connection, which is what the
/// pre-store build did for archive and delete; Graph has no session to share,
/// so it runs them in order.
pub(crate) async fn run_ops(backend: &Backend, ops: &[ServerOp]) -> Vec<Result<()>> {
    match backend {
        Backend::Graph(config) => {
            let mut out = Vec::with_capacity(ops.len());
            for op in ops {
                out.push(run_graph(config, op).await);
            }
            out
        }
        Backend::Imap(config) => match homogeneous(ops) {
            Some(Homogeneous::Move { source, dest }) if ops.len() > 1 => {
                let ids: Vec<String> = ops.iter().map(message_id_of).collect();
                crate::imap_client::batch_move_on_server(config, &ids, source, dest).await
            }
            Some(Homogeneous::Delete { source }) if ops.len() > 1 => {
                let ids: Vec<String> = ops.iter().map(message_id_of).collect();
                crate::imap_client::batch_delete_on_server(config, &ids, source).await
            }
            _ => {
                let mut out = Vec::with_capacity(ops.len());
                for op in ops {
                    out.push(run_imap(config, op).await);
                }
                out
            }
        },
    }
}

async fn run_imap(config: &ImapConfig, op: &ServerOp) -> Result<()> {
    match op {
        ServerOp::Move {
            message_id,
            source_mailbox,
            dest_mailbox,
        } => {
            crate::imap_client::move_email_on_server(
                config,
                message_id,
                source_mailbox,
                dest_mailbox,
            )
            .await
        }
        ServerOp::Delete {
            message_id,
            source_mailbox,
        } => crate::imap_client::delete_email_on_server(config, message_id, source_mailbox).await,
        ServerOp::SetRead {
            message_id,
            mailbox,
            read,
        } => {
            if *read {
                crate::imap_client::mark_read_on_server(config, message_id, mailbox).await
            } else {
                crate::imap_client::mark_unread_on_server(config, message_id, mailbox).await
            }
        }
        ServerOp::SetFlagged {
            message_id,
            mailbox,
            flagged,
        } => {
            let flag = crate::types::FLAG_FLAGGED;
            if *flagged {
                crate::imap_client::add_flag_on_server(config, message_id, mailbox, flag).await
            } else {
                crate::imap_client::remove_flag_on_server(config, message_id, mailbox, flag).await
            }
        }
    }
}

async fn run_graph(config: &GraphConfig, op: &ServerOp) -> Result<()> {
    match op {
        ServerOp::Move {
            message_id,
            dest_mailbox,
            ..
        } => crate::graph::move_message_graph(config, message_id, dest_mailbox).await,
        ServerOp::Delete { message_id, .. } => {
            crate::graph::delete_message_graph(config, message_id).await
        }
        ServerOp::SetRead {
            message_id, read, ..
        } => crate::graph::mark_read_graph(config, message_id, *read).await,
        // Graph stays seen-only (#0007): the star is parked locally, honoured
        // by the store and the list, but never mirrored to Graph. A no-op Ok
        // rather than an error, so the optimistic local write stands.
        ServerOp::SetFlagged { message_id, .. } => {
            log::debug!("[graph] flag parked locally for {message_id}; Graph is seen-only");
            Ok(())
        }
    }
}

/// A batch that can share one IMAP session: same kind, same folders.
enum Homogeneous<'a> {
    Move { source: &'a str, dest: &'a str },
    Delete { source: &'a str },
}

fn homogeneous(ops: &[ServerOp]) -> Option<Homogeneous<'_>> {
    let first = ops.first()?;
    match first {
        ServerOp::Move {
            source_mailbox,
            dest_mailbox,
            ..
        } => ops
            .iter()
            .all(|op| {
                matches!(op, ServerOp::Move { source_mailbox: s, dest_mailbox: d, .. }
                    if s == source_mailbox && d == dest_mailbox)
            })
            .then_some(Homogeneous::Move {
                source: source_mailbox,
                dest: dest_mailbox,
            }),
        ServerOp::Delete { source_mailbox, .. } => ops
            .iter()
            .all(|op| matches!(op, ServerOp::Delete { source_mailbox: s, .. } if s == source_mailbox))
            .then_some(Homogeneous::Delete {
                source: source_mailbox,
            }),
        ServerOp::SetRead { .. } | ServerOp::SetFlagged { .. } => None,
    }
}

fn message_id_of(op: &ServerOp) -> String {
    match op {
        ServerOp::Move { message_id, .. }
        | ServerOp::Delete { message_id, .. }
        | ServerOp::SetRead { message_id, .. }
        | ServerOp::SetFlagged { message_id, .. } => message_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::tests::{fixture, invite_ics, Fixture};
    use crate::store::read;
    use crate::tui::app::calendar_view;

    fn refs(ids: &[i64]) -> Vec<MessageRef> {
        ids.iter().copied().map(MessageRef::new).collect()
    }

    fn mailbox_of(fx: &Fixture, id: i64) -> Option<String> {
        read::find_by_id(&fx.store, id).unwrap().map(|r| r.mailbox)
    }

    /// Archive is a move with a fixed destination: the row lands in the archive
    /// mailbox immediately, and the op the caller fires names the message by
    /// Message-ID with the two *server* folders, not the store's mailbox keys.
    #[test]
    fn archiving_moves_the_row_and_dispatches_a_server_move() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");

        let prepared = prepare_move(&fx.store, &refs(&[id]), "archive", "INBOX", "Archive");

        assert_eq!(mailbox_of(&fx, id).as_deref(), Some("archive"));
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].msg(), MessageRef::new(id));
        assert_eq!(
            prepared[0].op,
            ServerOp::Move {
                message_id: "<inbox-1@example.com>".to_string(),
                source_mailbox: "INBOX".to_string(),
                dest_mailbox: "Archive".to_string(),
            }
        );
    }

    /// Delete removes the row and dispatches a server delete against the folder
    /// the message was actually in, rather than the hardcoded INBOX the file
    /// build had to assume.
    #[test]
    fn deleting_removes_the_row_and_dispatches_a_server_delete() {
        let fx = fixture();
        let id = fx.ingest_plain("archive", 4, "Junk");

        let prepared = prepare_delete(&fx.store, &fx.blobs, &refs(&[id]), "Archive");

        assert!(mailbox_of(&fx, id).is_none(), "the row survived the delete");
        assert_eq!(
            prepared[0].op,
            ServerOp::Delete {
                message_id: "<archive-4@example.com>".to_string(),
                source_mailbox: "Archive".to_string(),
            }
        );
    }

    /// The flag lands on the row first, and the op carries the new state and
    /// the folder to apply it in.
    #[test]
    fn flagging_writes_seen_and_dispatches_the_matching_server_op() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 2, "Unread");

        let prepared = prepare_read_flag(&fx.store, &refs(&[id]), true, "INBOX");

        assert!(read::find_by_id(&fx.store, id).unwrap().unwrap().is_read());
        assert_eq!(
            prepared[0].op,
            ServerOp::SetRead {
                message_id: "<inbox-2@example.com>".to_string(),
                mailbox: "INBOX".to_string(),
                read: true,
            }
        );

        let prepared = prepare_read_flag(&fx.store, &refs(&[id]), false, "INBOX");
        assert!(!read::find_by_id(&fx.store, id).unwrap().unwrap().is_read());
        assert!(matches!(prepared[0].op, ServerOp::SetRead { read: false, .. }));
    }

    /// The star lands on the row first and the op carries the new state and the
    /// folder to apply it in (#0007). Flagging leaves the read bit alone.
    #[test]
    fn flagging_writes_the_star_and_dispatches_the_matching_server_op() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 7, "Important");

        let prepared = prepare_flag(&fx.store, &refs(&[id]), true, "INBOX");

        let row = read::find_by_id(&fx.store, id).unwrap().unwrap();
        assert!(row.is_flagged());
        assert!(!row.is_read(), "flagging must not touch the read bit");
        assert_eq!(
            prepared[0].op,
            ServerOp::SetFlagged {
                message_id: "<inbox-7@example.com>".to_string(),
                mailbox: "INBOX".to_string(),
                flagged: true,
            }
        );

        let prepared = prepare_flag(&fx.store, &refs(&[id]), false, "INBOX");
        assert!(!read::find_by_id(&fx.store, id).unwrap().unwrap().is_flagged());
        assert!(matches!(prepared[0].op, ServerOp::SetFlagged { flagged: false, .. }));
    }

    /// A batch applies every row and produces one op per message, in order.
    /// The ops are homogeneous, which is what lets IMAP run them over a single
    /// connection.
    #[test]
    fn a_batch_moves_every_row_and_dispatches_one_op_each() {
        let fx = fixture();
        let ids: Vec<i64> = (1..=3)
            .map(|uid| fx.ingest_plain("inbox", uid, &format!("Mail {uid}")))
            .collect();

        let prepared = prepare_move(&fx.store, &refs(&ids), "archive", "INBOX", "Archive");

        assert_eq!(prepared.len(), 3);
        for id in &ids {
            assert_eq!(mailbox_of(&fx, *id).as_deref(), Some("archive"));
        }
        assert_eq!(read::list_mailbox(&fx.store, "alice", "inbox").unwrap().len(), 0);
        let ops: Vec<ServerOp> = prepared.iter().map(|p| p.op.clone()).collect();
        assert!(
            matches!(homogeneous(&ops), Some(Homogeneous::Move { source: "INBOX", dest: "Archive" })),
            "a uniform batch must be runnable over one connection"
        );
    }

    /// Mixed folders are not batchable: two moves out of different mailboxes
    /// cannot share one SELECT, so they fall back to one session each.
    #[test]
    fn a_mixed_batch_is_not_run_over_one_connection() {
        let fx = fixture();
        let a = fx.ingest_plain("inbox", 1, "From inbox");
        let b = fx.ingest_plain("archive", 1, "From archive");

        let mut ops: Vec<ServerOp> = prepare_move(&fx.store, &refs(&[a]), "archive", "INBOX", "Archive")
            .into_iter()
            .map(|p| p.op)
            .collect();
        ops.extend(
            prepare_move(&fx.store, &refs(&[b]), "inbox", "Archive", "INBOX")
                .into_iter()
                .map(|p| p.op),
        );

        assert!(homogeneous(&ops).is_none());
    }

    /// Moving an invite is what the Calendar view has to hear about: the
    /// agenda is a snapshot of the invite rows, so the copy taken before the
    /// move still points at the old mailbox and only a rebuild agrees with the
    /// store. This is the refresh the mutation arms run (`bg.rs` runs the same
    /// one after an RSVP).
    #[test]
    fn moving_an_invite_changes_what_a_rebuilt_agenda_reads() {
        let fx = fixture();
        let id = fx.ingest_invite("inbox", 1, "Standup", &invite_ics("uid-a", 0, &["a@x.com"]));
        let stale = calendar_view::load_events_for_account(&fx.store, &fx.blobs, "alice", "");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].msg, MessageRef::new(id));

        prepare_move(&fx.store, &refs(&[id]), "archive", "INBOX", "Archive");
        let rebuilt = calendar_view::load_events_for_account(&fx.store, &fx.blobs, "alice", "");

        assert_eq!(rebuilt.len(), 1, "the invite is still on the agenda");
        assert_eq!(mailbox_of(&fx, id).as_deref(), Some("archive"));
    }

    /// Deleting an invite is the case a stale agenda gets visibly wrong: the
    /// row is gone, so a rebuilt agenda drops the event while the snapshot
    /// taken before the mutation still lists it.
    #[test]
    fn deleting_an_invite_drops_it_from_a_rebuilt_agenda() {
        let fx = fixture();
        let id = fx.ingest_invite("inbox", 1, "Standup", &invite_ics("uid-a", 0, &["a@x.com"]));
        let stale = calendar_view::load_events_for_account(&fx.store, &fx.blobs, "alice", "");
        assert_eq!(stale.len(), 1);

        prepare_delete(&fx.store, &fx.blobs, &refs(&[id]), "INBOX");
        let rebuilt = calendar_view::load_events_for_account(&fx.store, &fx.blobs, "alice", "");

        assert!(rebuilt.is_empty(), "a deleted invite stayed on the agenda");
        assert_eq!(stale.len(), 1, "the pre-mutation snapshot is the stale one");
    }

    /// A reference to a row that is gone is skipped rather than reported: it
    /// produces no op, so nothing is fired at the server for a message the
    /// store no longer holds.
    #[test]
    fn a_dead_reference_prepares_nothing() {
        let fx = fixture();
        assert!(prepare_move(&fx.store, &refs(&[404]), "archive", "INBOX", "Archive").is_empty());
        assert!(prepare_delete(&fx.store, &fx.blobs, &refs(&[404]), "INBOX").is_empty());
        assert!(prepare_read_flag(&fx.store, &refs(&[404]), true, "INBOX").is_empty());
        assert!(prepare_flag(&fx.store, &refs(&[404]), true, "INBOX").is_empty());
    }
}
