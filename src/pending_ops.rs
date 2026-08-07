//! The durable mutation queue: a flag, move or delete that survives a kill -9.
//!
//! Stage 3 of the data-access-layer redesign (#0039). Archive, delete, move and
//! mark-read / mark-unread change server-side state the server still holds, so
//! losing one loses a flag change or delays a move, never a message. Send is not
//! a kind here: it has the stricter [`crate::outbox`] state machine, because
//! SMTP is not retryable the way a flag toggle is.
//!
//! This is the mutation twin of the outbox, and it follows the same durability
//! patterns rather than inventing parallel ones:
//!
//! - the local write and the queue row commit in **one** transaction
//!   ([`apply_move`], [`apply_delete`], [`apply_set_read`], [`apply_set_flagged`]),
//!   so a crash can never leave the store optimistically changed with no durable
//!   record of the op it owes the server, nor an op row for a change that never
//!   landed locally;
//! - a background engine drains the queue ([`drain`]) with exponential backoff
//!   (the same [`crate::outbox::backoff_secs`] curve), on startup and on the
//!   sync tick, exactly where [`crate::send::resume_outbox`] already runs;
//! - replay is exactly-once for the local half by construction: the local write
//!   happened once, inside the commit, and the drain **never re-applies it**. It
//!   runs only the server op, and the drain **converges** that op on replay. A
//!   move, delete or flag whose server half already landed before the crash
//!   replays against a message the source folder no longer holds; both backends
//!   report that as a typed [`crate::ops::NotFoundOnServer`], which the drain
//!   treats as success and retires the row. So a crash between the server op and
//!   the row's retirement re-runs the op and converges, rather than parking a
//!   succeeded op as `failed` and rolling the local state back under it;
//! - two drains racing on these rows is the destructive case the engine
//!   advisory lock ([`crate::engine_lock`], #0061) exists to exclude, so a live
//!   drain runs under that lock ([`drain_account`]).
//!
//! ## States
//!
//! Simpler than the outbox, because a mutation is single-phase (there is no
//! APPEND-to-Sent second leg):
//!
//! - `queued`: the local write is committed and the server op is owed. The
//!   drain attempts it under backoff.
//! - a successful op **deletes** the row: a done mutation has nothing an
//!   operator can act on, unlike a `done` send that may carry a partial-delivery
//!   note, so there is nothing to retain.
//! - `failed`: the op was refused past its retry budget. The row is kept so the
//!   failure is visible, and the local state is rolled back to the server's:
//!   a moved row goes home, a flag toggle is undone. A delete has nothing to
//!   restore (the row is gone), so it converges when the next sync refetches the
//!   UID the server never dropped, which is the same answer
//!   [`crate::store::write`] already gives.
//!
//! ## Addressing
//!
//! `target_message_id` is the `messages` row id, for correlation with the store
//! (#0039 amendment); the full server addressing rides in the JSON payload as a
//! [`ServerOp`], which names the message by `Message-ID`. The row already knows
//! the exact `(mailbox, uid)`, and carrying the uid on the op to skip the
//! full-mailbox `UID SEARCH HEADER` is a latency refinement the amendment notes;
//! this first cut keeps the Message-ID addressing the outbox's Sent dedup relies
//! on, because it is the proven seam and needs no widening of the `imap_client`
//! op signatures.

use std::future::Future;

use anyhow::{Context, Result};
use log::{info, warn};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::ops::{run_op, Backend, ServerOp};
use crate::outbox::{backoff_secs, unix_now};
use crate::store::write::MutatedRow;
use crate::store::{BlobStore, Store};

/// How many times the drain attempts a queued op before it gives up, rolls the
/// local state back and parks the row as `failed`.
///
/// The server ops (`imap_client` move / delete / flag, and their Graph twins)
/// return a plain error without a permanent-vs-transient verdict, so unlike the
/// outbox's SMTP classification the queue cannot tell a 5xx refusal from a
/// dropped connection. It therefore retries a bounded number of times under
/// backoff and then surfaces the failure, which is the honest reading of "we
/// asked and it did not take". A standing refusal (a delete the server rejects)
/// surfaces after the budget rather than on the first attempt.
pub const MAX_ATTEMPTS: i64 = 5;

/// `queued`, the one non-terminal state.
pub const STATE_QUEUED: &str = "queued";
/// `failed`, kept for visibility after the retry budget is spent.
pub const STATE_FAILED: &str = "failed";

/// How to undo a mutation's local half when its server op is refused for good.
///
/// Stored in the row payload beside the [`ServerOp`], so a rollback after a
/// crash has everything it needs without a second query against a row the
/// mutation may have moved or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rollback {
    /// Put a moved row back in its old mailbox and uid.
    Move(MutatedRow),
    /// Restore a row's full flag string (read and starred toggles both ride the
    /// same column, so the whole string is what round-trips cleanly).
    Flags { id: i64, flags: String },
    /// A delete has nothing to restore: the row is gone and the server still
    /// holds the message, so the next sync refetches it.
    None,
}

/// The JSON payload of a `pending_ops` row: the op to run and how to undo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Payload {
    op: ServerOp,
    rollback: Rollback,
}

/// One decoded `pending_ops` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub id: i64,
    pub account: String,
    pub kind: String,
    pub target_message_id: Option<i64>,
    pub op: ServerOp,
    pub rollback: Rollback,
    pub state: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created: i64,
    pub updated: i64,
}

// ---------------------------------------------------------------------------
// Enqueue, atomic with the local write
// ---------------------------------------------------------------------------

/// Insert a `queued` row inside an existing transaction.
///
/// This is the durability primitive: the caller opens one transaction, applies
/// the local store change and calls this in the same transaction, so the two
/// commit together. The `apply_*` functions below are the composed callers;
/// this is exposed so a future call site can compose its own local write with
/// the enqueue when it needs to.
pub fn enqueue(
    conn: &Connection,
    account: &str,
    target_message_id: Option<i64>,
    op: &ServerOp,
    rollback: &Rollback,
) -> Result<i64> {
    let payload = serde_json::to_string(&Payload {
        op: op.clone(),
        rollback: rollback.clone(),
    })
    .context("encoding the pending op payload")?;
    let now = unix_now();
    conn.execute(
        "INSERT INTO pending_ops (
            account, kind, target_message_id, payload, state, attempts,
            last_error, created, updated
         ) VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?6)",
        rusqlite::params![account, op.kind(), target_message_id, payload, STATE_QUEUED, now],
    )
    .context("inserting the pending op row")?;
    Ok(conn.last_insert_rowid())
}

/// Move a row into `dest_mailbox` and enqueue the server move, atomically.
///
/// Returns the row's previous coordinates (what a caller hands its UI) and the
/// queued op id. `Ok(None)` when the row is already gone, which is a no-op, not
/// an error.
pub fn apply_move(
    store: &Store,
    account: &str,
    id: i64,
    dest_mailbox: &str,
    op: ServerOp,
) -> Result<Option<(MutatedRow, i64)>> {
    let tx = store
        .immediate_transaction()
        .context("opening the move-and-enqueue transaction")?;
    let Some(previous) = coordinates_in(&tx, id)? else {
        return Ok(None);
    };
    tx.execute(
        "UPDATE messages SET mailbox = ?2, uid = ?3 WHERE id = ?1",
        rusqlite::params![id, dest_mailbox, -id],
    )
    .context("moving the message row")?;
    let op_id = enqueue(&tx, account, Some(id), &op, &Rollback::Move(previous.clone()))?;
    tx.commit().context("committing the move and its op")?;
    info!("[pending_ops] queued a {} for row {id} as op {op_id}", op.kind());
    Ok(Some((previous, op_id)))
}

/// Delete a row and enqueue the server delete, atomically.
pub fn apply_delete(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    id: i64,
    op: ServerOp,
) -> Result<Option<(MutatedRow, i64)>> {
    let tx = store
        .immediate_transaction()
        .context("opening the delete-and-enqueue transaction")?;
    let Some(previous) = coordinates_in(&tx, id)? else {
        return Ok(None);
    };
    let hashes = blob_refs_in(&tx, id)?;
    tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [id])
        .context("removing the FTS entry")?;
    tx.execute("DELETE FROM messages WHERE id = ?1", [id])
        .context("deleting the message row")?;
    for hash in &hashes {
        blobs.release(&tx, hash)?;
    }
    let op_id = enqueue(&tx, account, Some(id), &op, &Rollback::None)?;
    tx.commit().context("committing the delete and its op")?;
    info!("[pending_ops] queued a delete for row {id} as op {op_id}");
    Ok(Some((previous, op_id)))
}

/// Set (or clear) `\Seen` on a row and enqueue the server op, atomically.
pub fn apply_set_read(
    store: &Store,
    account: &str,
    id: i64,
    read: bool,
    op: ServerOp,
) -> Result<Option<i64>> {
    apply_flag_change(store, account, id, op, |flags| flags.with_seen(read))
}

/// Set (or clear) `\Flagged` on a row and enqueue the server op, atomically.
pub fn apply_set_flagged(
    store: &Store,
    account: &str,
    id: i64,
    flagged: bool,
    op: ServerOp,
) -> Result<Option<i64>> {
    apply_flag_change(store, account, id, op, |flags| flags.with_flagged(flagged))
}

/// The shared read-modify-write behind the two flag toggles: read the row's
/// flags, apply `f`, write the canonical string back and enqueue the op, all in
/// one transaction. The rollback carries the *old* string so a refused op
/// restores exactly what was there, including any orthogonal bit `f` left alone.
fn apply_flag_change(
    store: &Store,
    account: &str,
    id: i64,
    op: ServerOp,
    f: impl FnOnce(crate::types::MessageFlags) -> crate::types::MessageFlags,
) -> Result<Option<i64>> {
    let tx = store
        .immediate_transaction()
        .context("opening the flag-and-enqueue transaction")?;
    let current: Option<Option<String>> = tx
        .query_row("SELECT flags FROM messages WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .context("reading a message's flags")?;
    let Some(current) = current else {
        return Ok(None);
    };
    let old = current.unwrap_or_default();
    let new = f(crate::types::MessageFlags::parse(&old)).to_flag_string();
    tx.execute(
        "UPDATE messages SET flags = ?2 WHERE id = ?1",
        rusqlite::params![id, new],
    )
    .context("setting a message's flags")?;
    let op_id = enqueue(
        &tx,
        account,
        Some(id),
        &op,
        &Rollback::Flags { id, flags: old },
    )?;
    tx.commit().context("committing the flag change and its op")?;
    info!("[pending_ops] queued a {} for row {id} as op {op_id}", op.kind());
    Ok(Some(op_id))
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Every `queued` row for `account`, oldest first: what a drain works through.
pub fn queued_ops(store: &Store, account: &str) -> Result<Vec<PendingOp>> {
    rows_in_state(store, account, STATE_QUEUED)
}

/// Every `failed` row for `account`, oldest first: what the UI surfaces so a
/// refused mutation is not silent (#0039 scope item 3).
pub fn failed_ops(store: &Store, account: &str) -> Result<Vec<PendingOp>> {
    rows_in_state(store, account, STATE_FAILED)
}

/// `(queued, failed)` counts in one query, for a badge.
pub fn counts(store: &Store, account: &str) -> Result<(usize, usize)> {
    let mut stmt = store.conn().prepare(
        "SELECT state, COUNT(*) FROM pending_ops WHERE account = ?1 GROUP BY state",
    )?;
    let rows = stmt.query_map([account], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let (mut queued, mut failed) = (0usize, 0usize);
    for row in rows {
        let (state, n) = row?;
        match state.as_str() {
            STATE_QUEUED => queued = n as usize,
            STATE_FAILED => failed = n as usize,
            _ => {}
        }
    }
    Ok((queued, failed))
}

fn rows_in_state(store: &Store, account: &str, state: &str) -> Result<Vec<PendingOp>> {
    let mut stmt = store.conn().prepare(
        "SELECT id, account, kind, target_message_id, payload, state, attempts,
                last_error, created, updated
         FROM pending_ops WHERE account = ?1 AND state = ?2 ORDER BY id",
    )?;
    let rows = stmt.query_map(rusqlite::params![account, state], row_from_sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingOp> {
    let payload: Option<String> = row.get(4)?;
    let decoded: Payload = payload
        .as_deref()
        .and_then(|p| serde_json::from_str(p).ok())
        .ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(4, "payload".to_string(), rusqlite::types::Type::Text)
        })?;
    Ok(PendingOp {
        id: row.get(0)?,
        account: row.get(1)?,
        kind: row.get(2)?,
        target_message_id: row.get(3)?,
        op: decoded.op,
        rollback: decoded.rollback,
        state: row.get(5)?,
        attempts: row.get(6)?,
        last_error: row.get(7)?,
        created: row.get(8).unwrap_or(0),
        updated: row.get(9).unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------
// The drain
// ---------------------------------------------------------------------------

/// The server side of the drain, as a trait so the state machine is testable
/// against an in-memory fake. The live implementation is [`Backend`]; a test
/// drives every retry and rollback path offline.
///
/// An executor reports faithfully: a not-found on the server surfaces as an
/// [`Err`] carrying [`crate::ops::NotFoundOnServer`], and [`drain`] owns the
/// policy of treating that as a converged replay rather than a failure.
pub trait OpExecutor {
    fn execute(&mut self, op: &ServerOp) -> impl Future<Output = Result<()>> + Send;
}

impl OpExecutor for Backend {
    async fn execute(&mut self, op: &ServerOp) -> Result<()> {
        run_op(self, op).await
    }
}

/// What one [`drain`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainResult {
    /// Rows whose op succeeded and were retired.
    pub completed: usize,
    /// Rows parked as `failed` this pass, with their local state rolled back.
    pub failed: usize,
    /// Rows still `queued` after this pass (backoff not elapsed, or one more
    /// retryable failure short of the budget).
    pub still_open: usize,
}

/// Drive every `queued` row for `account` towards done or failed.
///
/// One row at a time, oldest first. A row whose backoff has not elapsed is
/// counted and left. A successful op retires the row; a failure bumps its
/// attempt count and, once the budget is spent, rolls the local state back and
/// parks the row as `failed`.
///
/// The drain **does not re-apply the local write**: that happened once, in the
/// transaction that enqueued the row. This is the guard against a duplicate
/// apply on replay after a crash.
///
/// A server op that comes back [`crate::ops::NotFoundOnServer`] is a converged
/// replay, not a failure: the op's earlier attempt already moved, deleted or
/// flagged the message, so the row is retired like any success. Every other
/// error is a genuine failure and is retried, then rolled back once the budget
/// is spent.
///
/// `pub(crate)`, not `pub`: the only lock-guarded entry point is
/// [`drain_account`], and draining outside that lock is the race the engine
/// advisory lock (#0061) exists to exclude.
pub(crate) async fn drain<E: OpExecutor>(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    exec: &mut E,
    now: i64,
) -> Result<DrainResult> {
    let mut result = DrainResult::default();
    for row in queued_ops(store, account)? {
        if row.updated + backoff_secs(row.attempts) > now {
            result.still_open += 1;
            continue;
        }
        match exec.execute(&row.op).await {
            Ok(()) => {
                retire(store, row.id)?;
                result.completed += 1;
            }
            Err(e) if crate::ops::NotFoundOnServer::is_in(&e) => {
                // The op already landed before the crash: the message is gone
                // from the source, which is exactly where a successful op
                // leaves it. Retire the row, do not roll the local state back.
                info!(
                    "[pending_ops] op {} ({}) target already gone on server; converged",
                    row.id, row.kind
                );
                retire(store, row.id)?;
                result.completed += 1;
            }
            Err(e) => {
                let err = format!("{e:#}");
                if row.attempts + 1 >= MAX_ATTEMPTS {
                    fail_and_roll_back(store, blobs, &row, &err)?;
                    result.failed += 1;
                } else {
                    bump_attempt(store, row.id, &err)?;
                    result.still_open += 1;
                }
            }
        }
    }
    Ok(result)
}

/// Take the engine lock and drain, or do nothing when another process holds it.
///
/// The live entry point: gating the drain on [`crate::engine_lock`] is what
/// keeps two processes (an open TUI and a `mp sync`) from running the same op
/// twice or racing a row's transitions (#0061). `Ok(None)` means another
/// process is the engine and is draining this account; the work still happens,
/// there.
pub async fn drain_account(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    backend: &mut Backend,
) -> Result<Option<DrainResult>> {
    match crate::engine_lock::EngineLock::try_acquire(account) {
        Ok(Some(_lock)) => {
            let result = drain(store, blobs, account, backend, unix_now()).await?;
            Ok(Some(result))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            warn!("[pending_ops] no engine lock for {account}, not draining: {e:#}");
            Ok(None)
        }
    }
}

/// Retire a row whose op succeeded. A done mutation has nothing to say, so the
/// row is deleted rather than parked.
fn retire(store: &Store, id: i64) -> Result<()> {
    store
        .conn()
        .execute("DELETE FROM pending_ops WHERE id = ?1", [id])
        .context("retiring a completed pending op")?;
    Ok(())
}

/// Record one more failed attempt on a still-retryable row.
fn bump_attempt(store: &Store, id: i64, err: &str) -> Result<()> {
    store
        .conn()
        .execute(
            "UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2, updated = ?3
             WHERE id = ?1",
            rusqlite::params![id, err, unix_now()],
        )
        .context("recording a failed pending-op attempt")?;
    Ok(())
}

/// Park a row as `failed` and roll its local state back to the server's.
fn fail_and_roll_back(store: &Store, blobs: &BlobStore, row: &PendingOp, err: &str) -> Result<()> {
    apply_rollback(store, blobs, &row.rollback)?;
    store
        .conn()
        .execute(
            "UPDATE pending_ops SET state = ?2, attempts = attempts + 1, last_error = ?3,
             updated = ?4 WHERE id = ?1",
            rusqlite::params![row.id, STATE_FAILED, err, unix_now()],
        )
        .context("parking a pending op as failed")?;
    warn!(
        "[pending_ops] op {} ({}) failed and was rolled back: {err}",
        row.id, row.kind
    );
    Ok(())
}

/// Undo a mutation's local half.
fn apply_rollback(store: &Store, _blobs: &BlobStore, rollback: &Rollback) -> Result<()> {
    match rollback {
        Rollback::Move(previous) => {
            store
                .conn()
                .execute(
                    "UPDATE messages SET mailbox = ?2, uid = ?3 WHERE id = ?1",
                    rusqlite::params![previous.id, previous.mailbox, previous.uid],
                )
                .context("rolling a moved row back")?;
        }
        Rollback::Flags { id, flags } => {
            store
                .conn()
                .execute(
                    "UPDATE messages SET flags = ?2 WHERE id = ?1",
                    rusqlite::params![id, flags],
                )
                .context("rolling a flag change back")?;
        }
        // A delete cannot be restored: the row is gone and the message is still
        // on the server, so convergence is the next sync refetching the UID.
        Rollback::None => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transaction-scoped helpers (no auto-commit, so they compose with `enqueue`)
// ---------------------------------------------------------------------------

/// A row's coordinates, read inside a transaction.
fn coordinates_in(conn: &Connection, id: i64) -> Result<Option<MutatedRow>> {
    conn.query_row(
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

/// Every blob hash a row references, read inside a transaction.
fn blob_refs_in(conn: &Connection, id: i64) -> Result<Vec<crate::store::blobs::BlobHash>> {
    let mut stmt = conn.prepare("SELECT hash FROM message_blobs WHERE message_row = ?1")?;
    let rows = stmt.query_map([id], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for hash in rows {
        match crate::store::blobs::BlobHash::parse(&hash?) {
            Ok(h) => out.push(h),
            Err(e) => warn!("[pending_ops] ignoring unparseable blob reference on delete: {e:#}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::tests::{fixture, invite_ics};
    use crate::store::read;
    use std::collections::VecDeque;

    /// A fake executor with a scripted verdict per call, recording every op it
    /// was asked to run so a test can assert exactly-once and idempotent replay.
    struct FakeExecutor {
        verdicts: VecDeque<Result<()>>,
        seen: Vec<ServerOp>,
    }

    impl FakeExecutor {
        fn always_ok() -> Self {
            FakeExecutor {
                verdicts: VecDeque::new(),
                seen: Vec::new(),
            }
        }
        fn scripted(verdicts: Vec<Result<()>>) -> Self {
            FakeExecutor {
                verdicts: verdicts.into(),
                seen: Vec::new(),
            }
        }
    }

    impl OpExecutor for FakeExecutor {
        async fn execute(&mut self, op: &ServerOp) -> Result<()> {
            self.seen.push(op.clone());
            self.verdicts
                .pop_front()
                .unwrap_or(Ok(()))
        }
    }

    fn move_op(message_id: &str) -> ServerOp {
        ServerOp::Move {
            message_id: message_id.to_string(),
            source_mailbox: "INBOX".to_string(),
            dest_mailbox: "Archive".to_string(),
        }
    }

    /// The typed not-found a real backend returns when the op's server half
    /// already landed and the message is gone from the source folder. Scripting
    /// this into the fake is what exercises the drain's convergence contract
    /// rather than an assumed-idempotent backend.
    fn not_found(message_id: &str) -> Result<()> {
        Err(crate::ops::NotFoundOnServer {
            message_id: message_id.to_string(),
            mailbox: Some("INBOX".to_string()),
        }
        .into())
    }

    /// Applying a move writes the row *and* the queue row in one transaction:
    /// the store shows the move, and a queued op is durably recorded.
    #[test]
    fn apply_move_commits_the_row_and_the_op_together() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");

        let (previous, _op_id) = apply_move(&fx.store, "alice", id, "archive", move_op("<inbox-1@example.com>"))
            .unwrap()
            .unwrap();

        assert_eq!(previous.mailbox, "inbox");
        assert_eq!(read::find_by_id(&fx.store, id).unwrap().unwrap().mailbox, "archive");
        let queued = queued_ops(&fx.store, "alice").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].kind, "move");
        assert_eq!(queued[0].target_message_id, Some(id));
        assert_eq!(queued[0].op, move_op("<inbox-1@example.com>"));
    }

    /// A successful drain retires the row: the store keeps the move and the
    /// queue is empty, with the server op run exactly once.
    #[tokio::test]
    async fn a_successful_drain_retires_the_row() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");
        apply_move(&fx.store, "alice", id, "archive", move_op("<inbox-1@example.com>")).unwrap();

        let mut exec = FakeExecutor::always_ok();
        let result = drain(&fx.store, &fx.blobs, "alice", &mut exec, unix_now() + 10).await.unwrap();

        assert_eq!(result.completed, 1);
        assert_eq!(exec.seen.len(), 1, "the op ran exactly once");
        assert!(queued_ops(&fx.store, "alice").unwrap().is_empty());
        assert_eq!(read::find_by_id(&fx.store, id).unwrap().unwrap().mailbox, "archive");
    }

    /// Crash-safety with the real not-found contract: the server move landed
    /// before the crash, so on restart the replay finds the message already
    /// gone from the source. The backend reports [`crate::ops::NotFoundOnServer`],
    /// and the drain must **converge** (retire the row, keep the optimistic
    /// move), not fail the succeeded op and roll it back. This is the #0039
    /// review blocker: a scripted `Ok` hides it, so the fake returns the true
    /// not-found here.
    #[tokio::test]
    async fn a_queued_op_replays_after_a_simulated_crash() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");
        apply_move(&fx.store, "alice", id, "archive", move_op("<inbox-1@example.com>")).unwrap();

        // The "crash": the process died after the server move but before the
        // drain retired the row. On restart the row is still queued and the
        // store still shows the optimistic move.
        assert_eq!(queued_ops(&fx.store, "alice").unwrap().len(), 1);
        assert_eq!(read::find_by_id(&fx.store, id).unwrap().unwrap().mailbox, "archive");

        let mut exec = FakeExecutor::scripted(vec![not_found("<inbox-1@example.com>")]);
        let result = drain(&fx.store, &fx.blobs, "alice", &mut exec, unix_now() + 10)
            .await
            .unwrap();

        assert_eq!(exec.seen.len(), 1, "replay runs the op once");
        assert_eq!(result.completed, 1, "a not-found replay converges, it does not fail");
        assert_eq!(result.failed, 0);
        assert_eq!(
            read::find_by_id(&fx.store, id).unwrap().unwrap().mailbox, "archive",
            "a converged replay keeps the move; it is not rolled back"
        );
        assert!(queued_ops(&fx.store, "alice").unwrap().is_empty());
        assert!(failed_ops(&fx.store, "alice").unwrap().is_empty());
    }

    /// The discrimination the blocker turns on: a single not-found converges on
    /// the first attempt (a succeeded op that crashed before retiring), while a
    /// genuine error is retried and rolled back once the budget is spent. One
    /// test pins both so a future change cannot make the drain swallow real
    /// failures as "converged".
    #[tokio::test]
    async fn not_found_converges_but_a_genuine_error_still_rolls_back() {
        // Not-found: converges at once, no rollback, no failed row.
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");
        apply_move(&fx.store, "alice", id, "archive", move_op("<inbox-1@example.com>")).unwrap();
        let mut exec = FakeExecutor::scripted(vec![not_found("<inbox-1@example.com>")]);
        let r = drain(&fx.store, &fx.blobs, "alice", &mut exec, unix_now() + 10).await.unwrap();
        assert_eq!((r.completed, r.failed), (1, 0));
        assert_eq!(read::find_by_id(&fx.store, id).unwrap().unwrap().mailbox, "archive");
        assert!(failed_ops(&fx.store, "alice").unwrap().is_empty());

        // Genuine error: retried to the budget, then rolled the row home and
        // parked as failed. A not-found must never be mistaken for this.
        let id2 = fx.ingest_plain("inbox", 2, "Other");
        apply_move(&fx.store, "alice", id2, "archive", move_op("<inbox-2@example.com>")).unwrap();
        let base = unix_now();
        for tick in 0..MAX_ATTEMPTS {
            let mut exec = FakeExecutor::scripted(vec![Err(anyhow::anyhow!("NO server refused"))]);
            drain(&fx.store, &fx.blobs, "alice", &mut exec, base + (tick + 1) * 10_000_000)
                .await
                .unwrap();
        }
        assert_eq!(
            read::find_by_id(&fx.store, id2).unwrap().unwrap().mailbox, "inbox",
            "a genuine refusal must still roll the row home"
        );
        assert_eq!(failed_ops(&fx.store, "alice").unwrap().len(), 1);
    }

    /// A transient failure keeps the row queued and backs off: a second drain
    /// at the same instant does not re-attempt it, and one past the backoff
    /// window does.
    #[tokio::test]
    async fn a_transient_failure_backs_off_and_then_retries() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");
        apply_move(&fx.store, "alice", id, "archive", move_op("<inbox-1@example.com>")).unwrap();

        let base = unix_now();
        let mut exec = FakeExecutor::scripted(vec![Err(anyhow::anyhow!("connection reset"))]);
        let r1 = drain(&fx.store, &fx.blobs, "alice", &mut exec, base + 5).await.unwrap();
        assert_eq!(r1.still_open, 1);
        let queued = queued_ops(&fx.store, "alice").unwrap();
        assert_eq!(queued[0].attempts, 1);
        assert!(queued[0].last_error.as_deref().unwrap().contains("connection reset"));

        // Same instant: backoff (30s after one failure) has not elapsed, so the
        // op is not attempted again.
        let mut exec2 = FakeExecutor::always_ok();
        let r2 = drain(&fx.store, &fx.blobs, "alice", &mut exec2, base + 5).await.unwrap();
        assert_eq!(r2.still_open, 1);
        assert!(exec2.seen.is_empty(), "backoff must hold the retry back");

        // Well past the backoff window: it retries and succeeds.
        let mut exec3 = FakeExecutor::always_ok();
        let r3 = drain(&fx.store, &fx.blobs, "alice", &mut exec3, base + 10_000_000).await.unwrap();
        assert_eq!(r3.completed, 1);
        assert_eq!(exec3.seen.len(), 1);
    }

    /// A move the server refuses past the retry budget parks the row as failed
    /// and rolls the local row back to where it was.
    #[tokio::test]
    async fn a_refused_move_fails_and_rolls_the_row_home() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 1, "Receipt");
        apply_move(&fx.store, "alice", id, "archive", move_op("<inbox-1@example.com>")).unwrap();
        assert_eq!(read::find_by_id(&fx.store, id).unwrap().unwrap().mailbox, "archive");

        // Fail every attempt; drive the drain past the budget with elapsed
        // backoff each time.
        let base = unix_now();
        let mut failed = false;
        for tick in 0..MAX_ATTEMPTS {
            let mut exec = FakeExecutor::scripted(vec![Err(anyhow::anyhow!("NO server refused"))]);
            let r = drain(&fx.store, &fx.blobs, "alice", &mut exec, base + (tick + 1) * 10_000_000)
                .await
                .unwrap();
            if r.failed == 1 {
                failed = true;
            }
        }
        assert!(failed, "the op never reached the failed state");

        let row = read::find_by_id(&fx.store, id).unwrap().unwrap();
        assert_eq!(row.mailbox, "inbox", "a refused move must roll the row home");
        assert!(queued_ops(&fx.store, "alice").unwrap().is_empty());
        let failed_rows = failed_ops(&fx.store, "alice").unwrap();
        assert_eq!(failed_rows.len(), 1);
        assert!(failed_rows[0].last_error.as_deref().unwrap().contains("refused"));
    }

    /// A read toggle stores the old flag string, so a refused op restores it
    /// without disturbing the orthogonal answered/forwarded bits.
    #[tokio::test]
    async fn a_refused_read_toggle_restores_the_flags() {
        let fx = fixture();
        let id = fx.ingest_plain("inbox", 2, "Unread");
        crate::store::write::set_answered(&fx.store, id).unwrap();

        let op = ServerOp::SetRead {
            message_id: "<inbox-2@example.com>".to_string(),
            mailbox: "INBOX".to_string(),
            read: true,
        };
        apply_set_read(&fx.store, "alice", id, true, op).unwrap();
        assert!(read::find_by_id(&fx.store, id).unwrap().unwrap().is_read());

        let base = unix_now();
        for tick in 0..MAX_ATTEMPTS {
            let mut exec = FakeExecutor::scripted(vec![Err(anyhow::anyhow!("STORE rejected"))]);
            drain(&fx.store, &fx.blobs, "alice", &mut exec, base + (tick + 1) * 10_000_000)
                .await
                .unwrap();
        }

        let row = read::find_by_id(&fx.store, id).unwrap().unwrap();
        assert!(!row.is_read(), "the read bit was not rolled back");
        assert!(row.is_answered(), "rollback disturbed an orthogonal bit");
    }

    /// A delete applies the row removal and the op together, and a refused
    /// delete parks as failed with nothing to restore (the row stays gone; the
    /// next sync refetches).
    #[tokio::test]
    async fn a_delete_queues_and_a_refusal_leaves_the_row_gone() {
        let fx = fixture();
        let id = fx.ingest_invite("inbox", 1, "Standup", &invite_ics("uid-a", 0, &["a@x.com"]));

        let op = ServerOp::Delete {
            message_id: "<inbox-1@example.com>".to_string(),
            source_mailbox: "INBOX".to_string(),
        };
        apply_delete(&fx.store, &fx.blobs, "alice", id, op).unwrap();
        assert!(read::find_by_id(&fx.store, id).unwrap().is_none());
        assert_eq!(queued_ops(&fx.store, "alice").unwrap().len(), 1);

        let base = unix_now();
        for tick in 0..MAX_ATTEMPTS {
            let mut exec = FakeExecutor::scripted(vec![Err(anyhow::anyhow!("delete refused"))]);
            drain(&fx.store, &fx.blobs, "alice", &mut exec, base + (tick + 1) * 10_000_000)
                .await
                .unwrap();
        }

        assert!(read::find_by_id(&fx.store, id).unwrap().is_none(), "a delete has nothing to restore");
        assert_eq!(failed_ops(&fx.store, "alice").unwrap().len(), 1);
        assert_eq!(counts(&fx.store, "alice").unwrap(), (0, 1));
    }

    /// Mutating a missing row is a no-op, not an error or a queued op.
    #[test]
    fn applying_to_a_missing_row_queues_nothing() {
        let fx = fixture();
        assert!(apply_move(&fx.store, "alice", 404, "archive", move_op("<x@x>")).unwrap().is_none());
        assert!(apply_delete(&fx.store, &fx.blobs, "alice", 404, ServerOp::Delete {
            message_id: "<x@x>".to_string(),
            source_mailbox: "INBOX".to_string(),
        }).unwrap().is_none());
        assert!(queued_ops(&fx.store, "alice").unwrap().is_empty());
    }
}
