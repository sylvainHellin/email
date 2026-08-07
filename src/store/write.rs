//! The write path: the optimistic local half of a flag, move or delete.
//!
//! Ingest owns the receive path ([`crate::ingest`]); this module owns the
//! mutations the *user* makes. Both write the same rows, and the split is
//! about direction rather than about SQL: what arrives from the server is
//! ingest's, what the user does to a message is here.
//!
//! ## Optimistic, then fire-and-forget
//!
//! Every function applies the change to the store immediately and returns the
//! coordinates the row had before it (a [`MutatedRow`]), which is what the
//! caller hands to the server op it fires next and what it puts back when that
//! op fails. The durable queue that would survive a crash between the two is
//! [#0039](../../docs/tickets/0039-pending-ops-queue.md); until it lands this
//! is exactly the durability the pre-store build had, where the local half was
//! a file move and the rollback was moving the file back.
//!
//! ## Moves renumber the uid
//!
//! `messages` is unique on `(account, mailbox, uid)`, so a row cannot carry its
//! source UID into a destination that already holds that number. A moved row
//! therefore gets `uid = -id`: negative is a value no backend produces (IMAP
//! UIDs are unsigned, and [`crate::ingest::graph_uid`] clears the sign bit) and
//! the row id makes it unique, so it reads as "moved locally, not yet seen
//! there by a sync". The next sync of the destination finds the row through the
//! `message_id` index and writes the real UID over it, which is the same rebind
//! a UIDVALIDITY reset takes.
//!
//! ## Delete removes the row
//!
//! There is no tombstone. The store is a droppable cache in front of the
//! server, so the honest answer to "the server delete failed" is the one the
//! status line already gives, `Sync (F) to fix?`: the UID is no longer in the
//! mailbox's skip list, the next sync refetches it and ingest re-inserts the
//! row. A tombstone would be local state that the server cannot contradict,
//! i.e. a second source of truth, and the durable form of that intent is the
//! `pending_ops` row of [#0039](../../docs/tickets/0039-pending-ops-queue.md).

use anyhow::{Context, Result};
use log::warn;
use rusqlite::{OptionalExtension, Transaction};

use crate::store::blobs::BlobHash;
use crate::store::{BlobStore, Store};
use crate::types::MessageFlags;

/// Where a row was before a mutation moved or removed it.
///
/// Carries the `Message-ID` because that is how every server op names the
/// message, and the `(mailbox, uid)` pair because that is what a rollback has
/// to put back.
///
/// Serializable so the durable `pending_ops` queue can persist a move's
/// rollback coordinates in its row payload and restore them if the server op
/// fails after a crash (#0039).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MutatedRow {
    pub id: i64,
    pub message_id: String,
    pub mailbox: String,
    pub uid: i64,
}

/// The `(message_id, mailbox, uid)` of one row, or `None` when the row is gone.
pub fn row_coordinates(store: &Store, id: i64) -> Result<Option<MutatedRow>> {
    store
        .conn()
        .query_row(
            "SELECT id, message_id, mailbox, uid FROM messages WHERE id = ?1",
            [id],
            |row| {
                Ok(MutatedRow {
                    id: row.get(0)?,
                    message_id: row.get(1)?,
                    mailbox: row.get(2)?,
                    uid: row.get(3)?,
                })
            },
        )
        .optional()
        .context("reading a message row's coordinates")
}

/// Move a row into `dest_mailbox`, returning where it came from.
///
/// The uid becomes `-id` (see the module docs). `Ok(None)` means the row was
/// already gone, which is a no-op rather than an error: two mutations racing on
/// the same message is a user action, not a bug.
pub fn move_row(store: &Store, id: i64, dest_mailbox: &str) -> Result<Option<MutatedRow>> {
    let Some(previous) = row_coordinates(store, id)? else {
        return Ok(None);
    };
    store
        .conn()
        .execute(
            "UPDATE messages SET mailbox = ?2, uid = ?3 WHERE id = ?1",
            rusqlite::params![id, dest_mailbox, -id],
        )
        .context("moving a message row")?;
    Ok(Some(previous))
}

/// Put a moved row back where [`move_row`] found it, after a failed server op.
pub fn restore_row(store: &Store, previous: &MutatedRow) -> Result<()> {
    store
        .conn()
        .execute(
            "UPDATE messages SET mailbox = ?2, uid = ?3 WHERE id = ?1",
            rusqlite::params![previous.id, previous.mailbox, previous.uid],
        )
        .context("restoring a moved message row")?;
    Ok(())
}

/// Delete a row, its FTS entry and its blob references, returning where it was.
///
/// `message_blobs` goes with the row through `ON DELETE CASCADE`, but the
/// refcounts are decremented explicitly, exactly as ingest does when it
/// re-points a reference: the cascade removes the list, never the count.
pub fn delete_row(store: &Store, blobs: &BlobStore, id: i64) -> Result<Option<MutatedRow>> {
    let Some(previous) = row_coordinates(store, id)? else {
        return Ok(None);
    };
    let tx = store
        .conn()
        .unchecked_transaction()
        .context("opening a delete transaction")?;
    let hashes = blob_refs(&tx, id)?;
    tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [id])
        .context("removing the FTS entry")?;
    tx.execute("DELETE FROM messages WHERE id = ?1", [id])
        .context("deleting the message row")?;
    for hash in hashes {
        blobs.release(&tx, &hash)?;
    }
    tx.commit().context("committing a delete transaction")?;
    Ok(Some(previous))
}

/// Delete the row a `(account, mailbox, uid)` triple names, if it is there.
///
/// Same delete as [`delete_row`], reached from the identity the *server* uses
/// rather than from a row id: the sync prune knows that a UID has left a
/// mailbox, never which local row held it. `Ok(None)` means the store does not
/// hold that UID, which is a no-op rather than an error.
pub fn delete_by_uid(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    mailbox: &str,
    uid: i64,
) -> Result<Option<MutatedRow>> {
    let id: Option<i64> = store
        .conn()
        .query_row(
            "SELECT id FROM messages WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
            rusqlite::params![account, mailbox, uid],
            |row| row.get(0),
        )
        .optional()
        .context("looking up a message row by uid")?;
    match id {
        Some(id) => delete_row(store, blobs, id),
        None => Ok(None),
    }
}

/// Set (or clear) `\Seen` on a row, leaving the history bits alone. Returns
/// true when the flags changed.
///
/// The read bit is only one of the three the column carries since #TKT-0051,
/// so this reads the row's flags and writes them back with that one bit
/// replaced. Overwriting the column with `\Seen` (what it did while `\Seen`
/// was the only flag there was) would erase an `\Answered` the moment the user
/// marked a replied-to message unread.
pub fn set_read(store: &Store, id: i64, read: bool) -> Result<bool> {
    update_flags(store, id, |flags| flags.with_seen(read))
}

/// Set (or clear) `\Flagged` on a row, leaving the other bits alone (#0007).
/// Returns true when the flags changed. The `\Flagged` star is orthogonal to
/// the read/answered/forwarded axis, so it rides the same column through a
/// read-modify-write rather than overwriting it.
pub fn set_flagged(store: &Store, id: i64, flagged: bool) -> Result<bool> {
    update_flags(store, id, |flags| flags.with_flagged(flagged))
}

/// Record that a reply to this row has gone out (#TKT-0051). Idempotent.
pub fn set_answered(store: &Store, id: i64) -> Result<bool> {
    update_flags(store, id, |flags| MessageFlags {
        answered: true,
        ..flags
    })
}

/// Record that this row has been forwarded (#TKT-0051). Idempotent.
pub fn set_forwarded(store: &Store, id: i64) -> Result<bool> {
    update_flags(store, id, |flags| MessageFlags {
        forwarded: true,
        ..flags
    })
}

/// Read one row's flags, apply `f`, write the canonical string back. Returns
/// true when the stored string actually changed, which is what keeps a no-op
/// toggle from counting as a mutation.
fn update_flags(
    store: &Store,
    id: i64,
    f: impl FnOnce(MessageFlags) -> MessageFlags,
) -> Result<bool> {
    let current: Option<String> = store
        .conn()
        .query_row("SELECT flags FROM messages WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .context("reading a message's flags")?
        .flatten();
    let flags = f(MessageFlags::parse(current.as_deref().unwrap_or_default())).to_flag_string();
    let changed = store
        .conn()
        .execute(
            "UPDATE messages SET flags = ?2 WHERE id = ?1 AND IFNULL(flags, '') <> ?2",
            rusqlite::params![id, flags],
        )
        .context("setting a message's flags")?;
    Ok(changed > 0)
}

/// Every blob hash a row references. An unparseable one is logged and skipped
/// rather than aborting the delete: the row must go either way, and a hash that
/// cannot be parsed is a refcount that was already unreachable.
fn blob_refs(tx: &Transaction<'_>, id: i64) -> Result<Vec<BlobHash>> {
    let mut stmt = tx.prepare("SELECT hash FROM message_blobs WHERE message_row = ?1")?;
    let rows = stmt.query_map([id], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for hash in rows {
        match BlobHash::parse(&hash?) {
            Ok(h) => out.push(h),
            Err(e) => warn!("[store] ignoring unparseable blob reference on delete: {e:#}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::tests::{fixture, invite_ics};
    use crate::store::read;

    /// A move rewrites the mailbox and parks the uid on the negative sentinel,
    /// which is what keeps `UNIQUE (account, mailbox, uid)` satisfiable when
    /// the destination already holds the source's UID.
    #[test]
    fn a_move_rewrites_the_mailbox_and_frees_the_uid() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 7, "Move me");
        // The destination already holds uid 7: the row cannot carry it over.
        fx.ingest_plain("archive", 7, "Incumbent");

        let previous = move_row(&fx.store, id, "archive").unwrap().unwrap();

        assert_eq!(previous.mailbox, "inbox");
        assert_eq!(previous.uid, 7);
        let now = row_coordinates(&fx.store, id).unwrap().unwrap();
        assert_eq!(now.mailbox, "archive");
        assert_eq!(now.uid, -id, "a moved row parks on the negative sentinel");
        assert_eq!(read::list_mailbox(&fx.store, "alice", "inbox").unwrap().len(), 0);
        assert_eq!(read::list_mailbox(&fx.store, "alice", "archive").unwrap().len(), 2);
    }

    /// A refused server op puts the row back exactly where it was, which is the
    /// store equivalent of the file build moving the file back.
    #[test]
    fn a_rollback_puts_the_row_back_in_its_mailbox_and_uid() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 3, "There and back");

        let previous = move_row(&fx.store, id, "archive").unwrap().unwrap();
        restore_row(&fx.store, &previous).unwrap();

        let now = row_coordinates(&fx.store, id).unwrap().unwrap();
        assert_eq!((now.mailbox.as_str(), now.uid), ("inbox", 3));
    }

    /// A delete takes the row, its FTS entry and its blob references with it,
    /// and releases the refcounts the row was holding.
    #[test]
    fn a_delete_removes_the_row_its_index_entry_and_its_blob_references() {
        let fx = fixture();
        let id = fx.ingest_invite("inbox", 1, "Standup", &invite_ics("uid-a", 0, &["a@x.com"]));
        let hashes: Vec<String> = fx
            .store
            .conn()
            .prepare("SELECT hash FROM message_blobs WHERE message_row = ?1")
            .unwrap()
            .query_map([id], |row| row.get(0))
            .unwrap()
            .map(|h| h.unwrap())
            .collect();
        assert!(!hashes.is_empty(), "fixture wrote no blobs");

        let previous = delete_row(&fx.store, &fx.blobs, id).unwrap().unwrap();

        assert_eq!(previous.mailbox, "inbox");
        assert!(row_coordinates(&fx.store, id).unwrap().is_none());
        assert!(read::find_by_id(&fx.store, id).unwrap().is_none());
        let fts: i64 = fx
            .store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages_fts WHERE rowid = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(fts, 0, "the FTS entry outlived its row");
        let refs: i64 = fx
            .store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM message_blobs WHERE message_row = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refs, 0, "the blob reference list outlived its row");
        for hash in &hashes {
            let parsed = crate::store::BlobHash::parse(hash).unwrap();
            assert_eq!(
                crate::store::blobs::refcount(fx.store.conn(), &parsed).unwrap(),
                0,
                "blob {hash} kept a reference from a deleted row"
            );
        }
    }

    /// Why nothing may hold a `MessageRef` across a delete: the row id is not
    /// reserved once the row is gone, and SQLite hands the same number to the
    /// next INSERT. A reference kept across the boundary therefore does not
    /// merely miss, it can name a *different* message.
    ///
    /// This is the hazard `App::remove_selected_from_list*` scrubs the
    /// selection for, and the reason the delete arm rebuilds the list from the
    /// prepared set rather than from what it held before.
    #[test]
    fn a_deleted_row_id_can_be_handed_to_the_next_message() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 5, "Gone");
        delete_row(&fx.store, &fx.blobs, id).unwrap();
        assert!(read::find_by_id(&fx.store, id).unwrap().is_none());

        let reused = fx.ingest_plain("inbox", 9, "Someone else");

        assert_eq!(reused, id, "SQLite reuses the id of a deleted row");
        assert_eq!(
            read::find_by_id(&fx.store, id).unwrap().unwrap().message_id,
            "<inbox-9@example.com>",
            "the old reference now resolves to a different message"
        );
    }

    /// The read flag round-trips, and reports whether it actually changed.
    #[test]
    fn setting_the_read_flag_writes_seen_and_reports_the_change() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Unread");
        assert!(!read::find_by_id(&fx.store, id).unwrap().unwrap().is_read());

        assert!(set_read(&fx.store, id, true).unwrap());
        assert!(read::find_by_id(&fx.store, id).unwrap().unwrap().is_read());
        assert!(!set_read(&fx.store, id, true).unwrap(), "no change to report");

        assert!(set_read(&fx.store, id, false).unwrap());
        assert!(!read::find_by_id(&fx.store, id).unwrap().unwrap().is_read());
    }

    /// Marking a replied-to message unread must not forget that it was
    /// replied to: the two axes are orthogonal, and the read one is written
    /// far more often (#TKT-0051).
    #[test]
    fn toggling_read_leaves_the_answered_and_forwarded_bits_alone() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 2, "Answered and forwarded");
        assert!(set_answered(&fx.store, id).unwrap());
        assert!(set_forwarded(&fx.store, id).unwrap());
        assert!(!set_answered(&fx.store, id).unwrap(), "idempotent");

        set_read(&fx.store, id, true).unwrap();
        set_read(&fx.store, id, false).unwrap();

        let row = read::find_by_id(&fx.store, id).unwrap().unwrap();
        assert!(!row.is_read());
        assert!(row.is_answered());
        assert!(row.is_forwarded());
    }

    /// A row that is already gone is a no-op, not an error: two mutations
    /// racing on one message is a user action.
    #[test]
    fn mutating_a_missing_row_is_a_no_op() {
        let fx = fixture();
        assert!(move_row(&fx.store, 404, "archive").unwrap().is_none());
        assert!(delete_row(&fx.store, &fx.blobs, 404).unwrap().is_none());
        assert!(delete_by_uid(&fx.store, &fx.blobs, "alice", "inbox", 404)
            .unwrap()
            .is_none());
        assert!(!set_read(&fx.store, 404, true).unwrap());
    }

    /// The server's identity for a message is `(account, mailbox, uid)`, and
    /// the same UID in another mailbox is a different message: a delete
    /// reached from a UID must not cross the mailbox boundary.
    #[test]
    fn deleting_by_uid_takes_only_the_row_in_that_mailbox() {
        let fx = fixture();
        let inbox = fx.ingest_plain("inbox", 7, "Gone from the inbox");
        let archive = fx.ingest_plain("archive", 7, "Still in the archive");

        let previous = delete_by_uid(&fx.store, &fx.blobs, "alice", "inbox", 7)
            .unwrap()
            .unwrap();

        assert_eq!(
            (previous.id, previous.mailbox.as_str(), previous.uid),
            (inbox, "inbox", 7)
        );
        assert!(row_coordinates(&fx.store, inbox).unwrap().is_none());
        assert!(row_coordinates(&fx.store, archive).unwrap().is_some());
    }
}
