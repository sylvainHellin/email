//! The TUI's entry into the durable mutation queue.
//!
//! A flag, a move, an archive or a delete is one local write and one server op.
//! [`queue_move`], [`queue_delete`], [`queue_read_flag`] and [`queue_flag`]
//! commit the local store change and the owed [`ServerOp`] in one transaction
//! through [`crate::pending_ops`] (#0039), then hand back the rows they touched
//! so the caller can drop them from the list it is showing. The server op is
//! retired later by the background drain at the sync/fetch resume point, and a
//! refusal is rolled back there, so the TUI no longer spawns a per-op server
//! thread and keeps no rollback of its own: the queue owns both.
//!
//! Splitting it out of `actions.rs` keeps the pairing testable: an action arm
//! needs a live terminal, while a queue function needs only a store, so a test
//! can assert both halves at once (the row changed, and *this* op was queued)
//! over an ingested fixture.

use super::app::MessageRef;
use crate::ops::ServerOp;
use crate::pending_ops;
use crate::store::write;
use crate::store::{BlobStore, Store};

/// The Message-ID a queued op names, read off the row before the op is built.
///
/// The durable `apply_*` functions read the row's coordinates themselves inside
/// their commit, but they take the [`ServerOp`] ready-made, and the op names the
/// message by Message-ID; this is the one lightweight read that supplies it.
/// `None` (with a log line) when the row is already gone, which is a skip rather
/// than an error.
fn message_id_of(store: &Store, msg: MessageRef, what: &str) -> Option<String> {
    match write::row_coordinates(store, msg.row_id()) {
        Ok(Some(row)) => Some(row.message_id),
        Ok(None) => {
            log::warn!("[store] {msg} has no row to {what}");
            None
        }
        Err(e) => {
            log::warn!("[store] reading {msg} to {what} failed: {e:#}");
            None
        }
    }
}

/// Move rows into `dest_mailbox` (the store's mailbox key) and queue the server
/// moves that carry them to `dest_server`. Returns the rows actually moved, for
/// the list update.
///
/// Rows that are already gone are skipped rather than reported: a message the
/// store no longer holds cannot be moved, and a second mutation racing the
/// first is a user action rather than a bug.
pub(crate) fn queue_move(
    store: &Store,
    account: &str,
    msgs: &[MessageRef],
    dest_mailbox: &str,
    source_server: &str,
    dest_server: &str,
) -> Vec<MessageRef> {
    let mut moved = Vec::with_capacity(msgs.len());
    for msg in msgs {
        let Some(message_id) = message_id_of(store, *msg, "move") else {
            continue;
        };
        let op = ServerOp::Move {
            message_id,
            source_mailbox: source_server.to_string(),
            dest_mailbox: dest_server.to_string(),
        };
        match pending_ops::apply_move(store, account, msg.row_id(), dest_mailbox, op) {
            Ok(Some(_)) => moved.push(*msg),
            Ok(None) => log::warn!("[store] {msg} has no row to move"),
            Err(e) => log::warn!("[store] queuing a move for {msg} failed: {e:#}"),
        }
    }
    moved
}

/// Delete rows and queue the server deletes. Returns the rows actually removed.
pub(crate) fn queue_delete(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    msgs: &[MessageRef],
    source_server: &str,
) -> Vec<MessageRef> {
    let mut deleted = Vec::with_capacity(msgs.len());
    for msg in msgs {
        let Some(message_id) = message_id_of(store, *msg, "delete") else {
            continue;
        };
        let op = ServerOp::Delete {
            message_id,
            source_mailbox: source_server.to_string(),
        };
        match pending_ops::apply_delete(store, blobs, account, msg.row_id(), op) {
            Ok(Some(_)) => deleted.push(*msg),
            Ok(None) => log::warn!("[store] {msg} has no row to delete"),
            Err(e) => log::warn!("[store] queuing a delete for {msg} failed: {e:#}"),
        }
    }
    deleted
}

/// Set the read flag on rows and queue the server ops that mirror it. Returns
/// the rows actually flagged.
pub(crate) fn queue_read_flag(
    store: &Store,
    account: &str,
    msgs: &[MessageRef],
    read: bool,
    server_mailbox: &str,
) -> Vec<MessageRef> {
    let mut out = Vec::with_capacity(msgs.len());
    for msg in msgs {
        let Some(message_id) = message_id_of(store, *msg, "flag") else {
            continue;
        };
        let op = ServerOp::SetRead {
            message_id,
            mailbox: server_mailbox.to_string(),
            read,
        };
        match pending_ops::apply_set_read(store, account, msg.row_id(), read, op) {
            Ok(Some(_)) => out.push(*msg),
            Ok(None) => log::warn!("[store] {msg} has no row to flag"),
            Err(e) => log::warn!("[store] queuing a read flag for {msg} failed: {e:#}"),
        }
    }
    out
}

/// Set the `\Flagged` star on rows and queue the server ops that mirror it
/// (#0007). Returns the rows actually starred.
pub(crate) fn queue_flag(
    store: &Store,
    account: &str,
    msgs: &[MessageRef],
    flagged: bool,
    server_mailbox: &str,
) -> Vec<MessageRef> {
    let mut out = Vec::with_capacity(msgs.len());
    for msg in msgs {
        let Some(message_id) = message_id_of(store, *msg, "flag") else {
            continue;
        };
        let op = ServerOp::SetFlagged {
            message_id,
            mailbox: server_mailbox.to_string(),
            flagged,
        };
        match pending_ops::apply_set_flagged(store, account, msg.row_id(), flagged, op) {
            Ok(Some(_)) => out.push(*msg),
            Ok(None) => log::warn!("[store] {msg} has no row to flag"),
            Err(e) => log::warn!("[store] queuing a flag for {msg} failed: {e:#}"),
        }
    }
    out
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

    /// The one queued op of an account, when a test expects exactly one.
    fn only_queued_op(fx: &Fixture) -> ServerOp {
        let queued = pending_ops::queued_ops(&fx.store, "alice").unwrap();
        assert_eq!(queued.len(), 1, "expected exactly one queued op");
        queued[0].op.clone()
    }

    /// Archive is a move with a fixed destination: the row lands in the archive
    /// mailbox immediately, and the queued op names the message by Message-ID
    /// with the two *server* folders, not the store's mailbox keys.
    #[test]
    fn archiving_moves_the_row_and_queues_a_server_move() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");

        let moved = queue_move(&fx.store, "alice", &refs(&[id]), "archive", "INBOX", "Archive");

        assert_eq!(mailbox_of(&fx, id).as_deref(), Some("archive"));
        assert_eq!(moved, vec![MessageRef::new(id)]);
        assert_eq!(
            only_queued_op(&fx),
            ServerOp::Move {
                message_id: "<inbox-1@example.com>".to_string(),
                source_mailbox: "INBOX".to_string(),
                dest_mailbox: "Archive".to_string(),
            }
        );
    }

    /// Delete removes the row and queues a server delete against the folder the
    /// message was actually in, rather than the hardcoded INBOX the file build
    /// had to assume.
    #[test]
    fn deleting_removes_the_row_and_queues_a_server_delete() {
        let fx = fixture();
        let id = fx.ingest_plain("archive", 4, "Junk");

        let deleted = queue_delete(&fx.store, &fx.blobs, "alice", &refs(&[id]), "Archive");

        assert_eq!(deleted, vec![MessageRef::new(id)]);
        assert!(mailbox_of(&fx, id).is_none(), "the row survived the delete");
        assert_eq!(
            only_queued_op(&fx),
            ServerOp::Delete {
                message_id: "<archive-4@example.com>".to_string(),
                source_mailbox: "Archive".to_string(),
            }
        );
    }

    /// The flag lands on the row first, and the queued op carries the new state
    /// and the folder to apply it in.
    #[test]
    fn flagging_writes_seen_and_queues_the_matching_server_op() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 2, "Unread");

        queue_read_flag(&fx.store, "alice", &refs(&[id]), true, "INBOX");

        assert!(read::find_by_id(&fx.store, id).unwrap().unwrap().is_read());
        assert_eq!(
            only_queued_op(&fx),
            ServerOp::SetRead {
                message_id: "<inbox-2@example.com>".to_string(),
                mailbox: "INBOX".to_string(),
                read: true,
            }
        );
    }

    /// The star lands on the row first and the queued op carries the new state
    /// and the folder to apply it in (#0007). Flagging leaves the read bit
    /// alone.
    #[test]
    fn flagging_writes_the_star_and_queues_the_matching_server_op() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 7, "Important");

        queue_flag(&fx.store, "alice", &refs(&[id]), true, "INBOX");

        let row = read::find_by_id(&fx.store, id).unwrap().unwrap();
        assert!(row.is_flagged());
        assert!(!row.is_read(), "flagging must not touch the read bit");
        assert_eq!(
            only_queued_op(&fx),
            ServerOp::SetFlagged {
                message_id: "<inbox-7@example.com>".to_string(),
                mailbox: "INBOX".to_string(),
                flagged: true,
            }
        );
    }

    /// A batch moves every row and queues one op per message: the store shows
    /// the whole selection archived and the queue owes one op each.
    #[test]
    fn a_batch_moves_every_row_and_queues_one_op_each() {
        let fx = fixture();
        let ids: Vec<i64> = (1..=3)
            .map(|uid| fx.ingest_plain("inbox", uid, &format!("Mail {uid}")))
            .collect();

        let moved = queue_move(&fx.store, "alice", &refs(&ids), "archive", "INBOX", "Archive");

        assert_eq!(moved.len(), 3);
        for id in &ids {
            assert_eq!(mailbox_of(&fx, *id).as_deref(), Some("archive"));
        }
        assert_eq!(read::list_mailbox(&fx.store, "alice", "inbox").unwrap().len(), 0);
        assert_eq!(pending_ops::queued_ops(&fx.store, "alice").unwrap().len(), 3);
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

        queue_move(&fx.store, "alice", &refs(&[id]), "archive", "INBOX", "Archive");
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

        queue_delete(&fx.store, &fx.blobs, "alice", &refs(&[id]), "INBOX");
        let rebuilt = calendar_view::load_events_for_account(&fx.store, &fx.blobs, "alice", "");

        assert!(rebuilt.is_empty(), "a deleted invite stayed on the agenda");
        assert_eq!(stale.len(), 1, "the pre-mutation snapshot is the stale one");
    }

    /// A reference to a row that is gone is skipped rather than reported: it
    /// queues no op, so nothing is owed to the server for a message the store
    /// no longer holds.
    #[test]
    fn a_dead_reference_queues_nothing() {
        let fx = fixture();
        assert!(queue_move(&fx.store, "alice", &refs(&[404]), "archive", "INBOX", "Archive").is_empty());
        assert!(queue_delete(&fx.store, &fx.blobs, "alice", &refs(&[404]), "INBOX").is_empty());
        assert!(queue_read_flag(&fx.store, "alice", &refs(&[404]), true, "INBOX").is_empty());
        assert!(queue_flag(&fx.store, "alice", &refs(&[404]), true, "INBOX").is_empty());
        assert!(pending_ops::queued_ops(&fx.store, "alice").unwrap().is_empty());
    }
}
