//! The durable outbox: submission state that survives a kill -9 (#0037 item 5).
//!
//! Best-effort "SMTP, then APPEND to Sent and hope" is exactly the design that
//! loses sent mail in Thunderbird, mutt and aerc
//! (`.agents/research/sent-folder-durability-in-mail-clients.md`). The store
//! makes the alternative cheap: the raw RFC822 bytes and a row describing what
//! must happen to them are committed *before* the SMTP conversation starts, so
//! every crash window has a defined recovery.
//!
//! ## The state machine
//!
//! ```text
//!            enqueue                submit 250              append acked
//!   (nothing) ------> pending_send ------------> sent_pending_append ------> done
//!                          |                            ^     |
//!         clean pre-submission failure                  |     | append failed
//!                          '--> stays pending_send      '-----'  (attempts += 1)
//!                          |
//!            ambiguous SMTP failure --> failed  (manual inspection, never re-sent)
//! ```
//!
//! Every arrow is one committed transaction, and each state answers exactly one
//! question about a process that died mid-send:
//!
//! - `pending_send`: SMTP has provably not been accepted, so the resume path
//!   submits. A crash between the commit and the SMTP conversation lands here,
//!   and re-sending is correct because the server never saw the message.
//! - `sent_pending_append`: the server returned 250 and owns the message now.
//!   SMTP must never run again for this row; only the APPEND is retried. A crash
//!   between the 250 and the APPEND lands here.
//! - `failed`: an *ambiguous* SMTP failure, where the message may or may not
//!   have been accepted. Automatic recovery cannot be safe in either direction,
//!   so the row stops here for a human. It is never auto re-sent.
//! - `done`: the message is in the Sent mailbox (or the account does not want a
//!   local APPEND at all, see [`crate::config::appends_to_sent`]).
//!
//! ## Exactly once
//!
//! SMTP runs at most once per row: [`record_submission`] is the only writer that
//! leaves `pending_send`, and the driver only submits rows still in that state.
//! The APPEND is idempotent by construction instead: a retry (`attempts > 0`)
//! first runs `UID SEARCH HEADER MESSAGE-ID` in the Sent mailbox and skips the
//! APPEND on a hit, because the previous attempt may have been ambiguous in the
//! same way SMTP can be. `APPENDUID` is the definitive acknowledgement and its
//! UID is stored on the row.
//!
//! ## Blob refcounting
//!
//! The raw bytes live in the content-addressed blob store like any other blob,
//! and the outbox row holds a plain `blobs`-table reference taken by
//! [`crate::store::BlobStore::acquire`] with **no `message_blobs` row**: that
//! table's foreign key targets `messages`, and an outbox row is a submission,
//! not a message. The reference is released when the row reaches `done`, and
//! when a `failed` row is explicitly discarded with [`discard`] -- never on the
//! transition into `failed` itself, because the whole point of that state is
//! that a human can still read the bytes that did not make it.
//!
//! ## Transport seam
//!
//! [`SentMailbox`] is the only thing this module knows about IMAP. The live
//! implementation is [`crate::imap_client::ImapSentMailbox`]; tests drive the
//! state machine against an in-memory fake, so every crash window is covered
//! offline and deterministically.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use log::{info, warn};
use rusqlite::OptionalExtension;

use crate::store::blobs::BlobHash;
use crate::store::{BlobStore, Store};
use crate::timing::TimingSpan;

/// Where a row is in the send state machine. The same four strings the schema's
/// `CHECK` constraint accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxState {
    /// Committed, not yet submitted. Safe to submit.
    PendingSend,
    /// SMTP returned 250; only the APPEND is outstanding.
    SentPendingAppend,
    /// In the Sent mailbox, or deliberately not saved there.
    Done,
    /// Ambiguous SMTP failure. Parked for manual inspection, never re-sent.
    Failed,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxState::PendingSend => "pending_send",
            OutboxState::SentPendingAppend => "sent_pending_append",
            OutboxState::Done => "done",
            OutboxState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending_send" => Some(OutboxState::PendingSend),
            "sent_pending_append" => Some(OutboxState::SentPendingAppend),
            "done" => Some(OutboxState::Done),
            "failed" => Some(OutboxState::Failed),
            _ => None,
        }
    }

    /// True for the two states that still need work from the driver.
    pub fn is_open(self) -> bool {
        matches!(self, OutboxState::PendingSend | OutboxState::SentPendingAppend)
    }
}

impl std::fmt::Display for OutboxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `outbox` row.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: i64,
    pub account: String,
    /// The Sent mailbox to APPEND into, or `None` when this account saves no
    /// local copy (Gmail, Graph, Proton, or an explicit `save_to_sent = never`).
    pub target_mailbox: Option<String>,
    pub message_id: String,
    pub raw_blob: BlobHash,
    pub state: OutboxState,
    /// APPEND attempts made so far. SMTP has no counter: it runs at most once.
    pub attempts: i64,
    pub last_error: Option<String>,
    pub appended_uid: Option<i64>,
    pub created: i64,
    pub updated: i64,
}

/// What the SMTP conversation did, from the state machine's point of view.
///
/// The distinction that matters is not "worked / did not work" but "does the
/// server possibly hold the message". Everything before the message bytes are
/// on the wire is a clean failure (no copy exists, retry is safe); everything
/// from there on is ambiguous (a copy may exist, retry may duplicate).
#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    /// 250, for at least one recipient.
    Accepted,
    /// The failure happened before submission could have been accepted:
    /// no transport, no auth, a rejected recipient address, an unparseable
    /// envelope. Nothing was delivered, so the row stays submittable.
    CleanPreSubmission(String),
    /// The failure leaves it unknown whether the server accepted the message:
    /// a dropped connection, a timeout, a partial per-recipient result. The row
    /// goes to `failed` for a human.
    Ambiguous(String),
}

/// What the APPEND attempt did.
#[derive(Debug, Clone)]
pub enum AppendOutcome {
    /// `APPENDUID` (or, on a server without UIDPLUS, the tagged `OK` plus a
    /// lookup) acknowledged the copy.
    Appended { uid: Option<i64> },
    /// The dedup search found the message already in the Sent mailbox, so the
    /// APPEND was skipped. This is what makes a retry after an ambiguous
    /// APPEND safe.
    AlreadyPresent { uid: Option<i64> },
    /// The copy is not there and the row stays retryable.
    Failed(String),
}

/// Marker error for a submission that never produced a verdict.
///
/// Attached by a transport (see [`crate::graph::GraphClient::send_mail`]) when
/// the request went out but no answer came back, so the message may or may not
/// have been accepted. [`classify_submission_error`] turns it into
/// [`SubmitOutcome::Ambiguous`], which parks the row in `failed` instead of
/// risking a duplicate.
#[derive(Debug)]
pub struct AmbiguousSubmission(pub String);

impl std::fmt::Display for AmbiguousSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server gave no verdict: {}", self.0)
    }
}

impl std::error::Error for AmbiguousSubmission {}

/// Read a failed submission's error chain: ambiguous when it carries an
/// [`AmbiguousSubmission`], a clean pre-submission failure otherwise.
pub fn classify_submission_error(err: &anyhow::Error) -> SubmitOutcome {
    if err.chain().any(|c| c.is::<AmbiguousSubmission>()) {
        SubmitOutcome::Ambiguous(format!("{err:#}"))
    } else {
        SubmitOutcome::CleanPreSubmission(format!("{err:#}"))
    }
}

/// The IMAP operations the outbox needs, and nothing else.
///
/// Kept as a trait so the state machine can be tested against an in-memory
/// fake: the two acceptance criteria are crash windows, and reproducing those
/// against a live server is neither offline nor deterministic.
pub trait SentMailbox {
    /// `UID SEARCH HEADER MESSAGE-ID "<id>"` in `mailbox`. An empty result
    /// means "not there"; an error means "could not tell", which the caller
    /// treats as a retryable failure rather than as a miss.
    fn search_message_id(
        &mut self,
        mailbox: &str,
        message_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u32>>> + Send;

    /// `APPEND` into `mailbox` with `\Seen`, returning the `APPENDUID` when the
    /// server offers one.
    fn append(
        &mut self,
        mailbox: &str,
        raw: &[u8],
    ) -> impl std::future::Future<Output = Result<Option<u32>>> + Send;
}

/// Base retry delay in seconds; each further attempt doubles it.
pub const BACKOFF_BASE_SECS: i64 = 30;
/// Ceiling on the retry delay, so a long-broken server settles at one attempt
/// per sync tick rather than drifting into never.
pub const BACKOFF_MAX_SECS: i64 = 900;

/// How long after `updated` a row with `attempts` failures may be retried.
pub fn backoff_secs(attempts: i64) -> i64 {
    if attempts <= 0 {
        return 0;
    }
    let shift = (attempts - 1).min(16) as u32;
    (BACKOFF_BASE_SECS.saturating_mul(1i64 << shift)).min(BACKOFF_MAX_SECS)
}

// ---------------------------------------------------------------------------
// Row lifecycle
// ---------------------------------------------------------------------------

/// Commit the raw message and its outbox row, *before* SMTP.
///
/// The blob file is written outside the transaction (idempotent and
/// content-addressed, per [`BlobStore::acquire`]'s contract) and the row plus
/// its blob reference go in together. When this returns, a crash can only lose
/// the SMTP attempt, never the message.
///
/// `target_mailbox` is `None` when the account saves no local copy; the row
/// then goes straight from `pending_send` to `done` on a 250.
pub fn enqueue(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    target_mailbox: Option<&str>,
    message_id: &str,
    raw: &[u8],
) -> Result<i64> {
    let mut span = TimingSpan::with_context("outbox_enqueue", format!("{} bytes", raw.len()));
    let hash = blobs.write(raw)?;
    span.mark("blob_written");

    let now = unix_now();
    let tx = store
        .conn()
        .unchecked_transaction()
        .context("opening the outbox enqueue transaction")?;
    tx.execute(
        "INSERT INTO outbox (
            account, target_mailbox, message_id, raw_blob, state, attempts,
            last_error, appended_uid, created, updated
         ) VALUES (?1, ?2, ?3, ?4, 'pending_send', 0, NULL, NULL, ?5, ?5)",
        rusqlite::params![account, target_mailbox, message_id, hash.as_str(), now],
    )
    .context("inserting the outbox row")?;
    let id = tx.last_insert_rowid();
    blobs.acquire(&tx, &hash, raw.len() as u64)?;
    tx.commit().context("committing the outbox row")?;
    span.mark("committed");

    info!("[outbox] queued {message_id} as row {id} for {account}");
    Ok(id)
}

/// Record what SMTP did. The only transition out of `pending_send`.
///
/// Returns the state the row is now in.
pub fn record_submission(
    store: &Store,
    blobs: &BlobStore,
    id: i64,
    outcome: &SubmitOutcome,
) -> Result<OutboxState> {
    let row = load(store, id)?
        .with_context(|| format!("outbox row {id} disappeared before its SMTP result"))?;
    if row.state != OutboxState::PendingSend {
        // Belt and braces: a second submission result for a row that already
        // left `pending_send` would be a double SMTP, so refuse to record it.
        warn!(
            "[outbox] ignoring an SMTP result for row {id}, already in state {}",
            row.state
        );
        return Ok(row.state);
    }

    let now = unix_now();
    match outcome {
        SubmitOutcome::Accepted => {
            if row.target_mailbox.is_none() {
                // Nothing to APPEND: the server saves the copy itself, or the
                // user asked for no local copy at all.
                finish_done(store, blobs, &row, None, now)?;
                Ok(OutboxState::Done)
            } else {
                store
                    .conn()
                    .execute(
                        "UPDATE outbox SET state = 'sent_pending_append', last_error = NULL,
                         updated = ?2 WHERE id = ?1",
                        rusqlite::params![id, now],
                    )
                    .context("marking the outbox row sent_pending_append")?;
                Ok(OutboxState::SentPendingAppend)
            }
        }
        SubmitOutcome::CleanPreSubmission(err) => {
            store
                .conn()
                .execute(
                    "UPDATE outbox SET last_error = ?2, updated = ?3 WHERE id = ?1",
                    rusqlite::params![id, err, now],
                )
                .context("recording a clean pre-submission failure")?;
            Ok(OutboxState::PendingSend)
        }
        SubmitOutcome::Ambiguous(err) => {
            store
                .conn()
                .execute(
                    "UPDATE outbox SET state = 'failed', last_error = ?2, updated = ?3
                     WHERE id = ?1",
                    rusqlite::params![id, err, now],
                )
                .context("marking the outbox row failed")?;
            warn!(
                "[outbox] row {id} ({}) failed ambiguously and will not be re-sent: {err}",
                row.message_id
            );
            Ok(OutboxState::Failed)
        }
    }
}

/// Record what the APPEND did.
pub fn record_append(
    store: &Store,
    blobs: &BlobStore,
    id: i64,
    outcome: &AppendOutcome,
) -> Result<OutboxState> {
    let row = load(store, id)?
        .with_context(|| format!("outbox row {id} disappeared before its APPEND result"))?;
    let now = unix_now();
    match outcome {
        AppendOutcome::Appended { uid } | AppendOutcome::AlreadyPresent { uid } => {
            finish_done(store, blobs, &row, *uid, now)?;
            Ok(OutboxState::Done)
        }
        AppendOutcome::Failed(err) => {
            store
                .conn()
                .execute(
                    "UPDATE outbox SET attempts = attempts + 1, last_error = ?2, updated = ?3
                     WHERE id = ?1",
                    rusqlite::params![id, err, now],
                )
                .context("recording a failed APPEND")?;
            Ok(row.state)
        }
    }
}

/// The mailbox role a locally-ingested sent copy lands in.
pub const SENT_ROLE: &str = "sent";

/// `done`, the local ingest of the sent copy and the blob release.
///
/// Every path to `done` funnels through here, so the local `messages` row is
/// written exactly once per message however the row got there: appended by us,
/// found already present by the dedup search, or never appended at all because
/// the server saves the copy itself.
///
/// The ingest runs *before* the release, so the raw blob's refcount passes from
/// the outbox reference to the message's own reference without ever touching
/// zero (a zero would unlink the file, see [`BlobStore::release`]).
fn finish_done(
    store: &Store,
    blobs: &BlobStore,
    row: &OutboxRow,
    uid: Option<i64>,
    now: i64,
) -> Result<()> {
    match blobs.read(&row.raw_blob) {
        Ok(raw) => {
            if let Err(e) = ingest_sent_copy(
                store,
                blobs,
                &row.account,
                SENT_ROLE,
                &raw,
                &row.message_id,
                uid,
            ) {
                // A local copy that failed to materialise is a display
                // problem, not a delivery problem: the message is on the
                // server and the next Sent sync brings it in.
                warn!(
                    "[outbox] row {} was sent but could not be ingested locally: {e:#}",
                    row.id
                );
            }
        }
        Err(e) => warn!(
            "[outbox] row {} was sent but its raw blob is unreadable ({e:#}); \
             the local Sent copy waits for the next sync",
            row.id
        ),
    }

    let tx = store
        .conn()
        .unchecked_transaction()
        .context("opening the outbox completion transaction")?;
    tx.execute(
        "UPDATE outbox SET state = 'done', last_error = NULL, appended_uid = ?2, updated = ?3
         WHERE id = ?1",
        rusqlite::params![row.id, uid, now],
    )
    .context("marking the outbox row done")?;
    blobs.release(&tx, &row.raw_blob)?;
    tx.commit().context("committing the outbox completion")?;
    Ok(())
}

/// Drop a terminal row and its blob reference.
///
/// The one way a `failed` row's bytes are released: while the row is parked for
/// inspection the reference is deliberately held, so this has to be explicit.
pub fn discard(store: &Store, blobs: &BlobStore, id: i64) -> Result<()> {
    let Some(row) = load(store, id)? else {
        return Ok(());
    };
    let tx = store
        .conn()
        .unchecked_transaction()
        .context("opening the outbox discard transaction")?;
    tx.execute("DELETE FROM outbox WHERE id = ?1", [id])
        .context("deleting the outbox row")?;
    if row.state != OutboxState::Done {
        // A `done` row already released its reference.
        blobs.release(&tx, &row.raw_blob)?;
    }
    tx.commit().context("committing the outbox discard")?;
    Ok(())
}

/// One row by id.
pub fn load(store: &Store, id: i64) -> Result<Option<OutboxRow>> {
    let row = store
        .conn()
        .query_row(
            "SELECT id, account, target_mailbox, message_id, raw_blob, state, attempts,
                    last_error, appended_uid, created, updated
             FROM outbox WHERE id = ?1",
            [id],
            row_from_sql,
        )
        .optional()
        .context("loading an outbox row")?;
    Ok(row)
}

/// Every row in a non-terminal state, oldest first.
pub fn open_rows(store: &Store, account: &str) -> Result<Vec<OutboxRow>> {
    let mut stmt = store.conn().prepare(
        "SELECT id, account, target_mailbox, message_id, raw_blob, state, attempts,
                last_error, appended_uid, created, updated
         FROM outbox
         WHERE account = ?1 AND state IN ('pending_send', 'sent_pending_append')
         ORDER BY id",
    )?;
    let rows = stmt.query_map([account], row_from_sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// How many rows are not `done`, split into "still working" and "parked".
///
/// This is what the TUI badge renders, so it is one query rather than a load of
/// every row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboxCounts {
    pub open: usize,
    pub failed: usize,
}

impl OutboxCounts {
    pub fn total(self) -> usize {
        self.open + self.failed
    }
}

pub fn counts(store: &Store, account: &str) -> Result<OutboxCounts> {
    let mut stmt = store
        .conn()
        .prepare("SELECT state, COUNT(*) FROM outbox WHERE account = ?1 GROUP BY state")?;
    let rows = stmt.query_map([account], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut counts = OutboxCounts::default();
    for row in rows {
        let (state, n) = row?;
        match OutboxState::parse(&state) {
            Some(OutboxState::Failed) => counts.failed += n as usize,
            Some(s) if s.is_open() => counts.open += n as usize,
            _ => {}
        }
    }
    Ok(counts)
}

/// The counts for an account whose store may not exist yet. Never an error: a
/// badge is not worth failing a redraw over.
pub fn counts_for_account(account: &str) -> OutboxCounts {
    let path = crate::config::store_path(account);
    if !path.exists() {
        return OutboxCounts::default();
    }
    match Store::open(&path).and_then(|store| counts(&store, account)) {
        Ok(counts) => counts,
        Err(e) => {
            warn!("[outbox] could not read the outbox counts for {account}: {e:#}");
            OutboxCounts::default()
        }
    }
}

/// Map one selected row. A state or hash the schema should have made
/// impossible is reported as a column-type error rather than silently skipped,
/// so a corrupted store fails the read and gets rebuilt instead of quietly
/// losing a pending send.
fn row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRow> {
    let raw_blob: String = row.get(4)?;
    let state: String = row.get(5)?;
    let bad = |idx: usize, name: &str| {
        rusqlite::Error::InvalidColumnType(idx, name.to_string(), rusqlite::types::Type::Text)
    };
    Ok(OutboxRow {
        id: row.get(0)?,
        account: row.get(1)?,
        target_mailbox: row.get(2)?,
        message_id: row.get(3)?,
        raw_blob: BlobHash::parse(&raw_blob).map_err(|_| bad(4, "raw_blob"))?,
        state: OutboxState::parse(&state).ok_or_else(|| bad(5, "state"))?,
        attempts: row.get(6)?,
        last_error: row.get(7)?,
        appended_uid: row.get(8)?,
        created: row.get(9).unwrap_or(0),
        updated: row.get(10).unwrap_or(0),
    })
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The retry driver
// ---------------------------------------------------------------------------

/// What one [`drain`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainResult {
    /// Rows that reached `done`.
    pub completed: usize,
    /// Rows whose APPEND was skipped because the message was already in Sent.
    pub deduped: usize,
    /// Rows that are still open after this pass (backoff, or another failure).
    pub still_open: usize,
    /// Rows left in `pending_send` because they were never submitted and this
    /// pass does not submit (see [`drain`]).
    pub awaiting_submission: usize,
}

/// Drive every `sent_pending_append` row for `account` towards `done`.
///
/// This is the resume path: it runs on startup and on the normal sync tick, and
/// it drives the APPEND only. Rows in `pending_send` are counted and left
/// alone, because re-submitting them needs the SMTP transport and the account's
/// credentials, which the caller owns (see [`crate::send::resume_pending_sends`]).
///
/// Retries are safe by construction: a row that has already been attempted
/// (`attempts > 0`) runs the Message-ID dedup search first and skips the APPEND
/// on a hit, so an ambiguous earlier attempt cannot produce a second copy.
pub async fn drain<M: SentMailbox>(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    mailbox: &mut M,
    now: i64,
) -> Result<DrainResult> {
    let mut span = TimingSpan::with_context("outbox_drain", account.to_string());
    let mut result = DrainResult::default();

    for row in open_rows(store, account)? {
        if row.state == OutboxState::PendingSend {
            result.awaiting_submission += 1;
            continue;
        }
        if row.updated + backoff_secs(row.attempts) > now {
            result.still_open += 1;
            continue;
        }
        let Some(target) = row.target_mailbox.clone() else {
            // A row with no target should have gone straight to `done` on the
            // 250; heal it rather than retry forever.
            finish_done(store, blobs, &row, None, now)?;
            result.completed += 1;
            continue;
        };

        let outcome = append_once(blobs, mailbox, &row, &target).await;
        let deduped = matches!(outcome, AppendOutcome::AlreadyPresent { .. });
        match record_append(store, blobs, row.id, &outcome)? {
            OutboxState::Done => {
                result.completed += 1;
                if deduped {
                    result.deduped += 1;
                }
            }
            _ => result.still_open += 1,
        }
    }

    span.mark("drained");
    Ok(result)
}

/// One APPEND attempt for one row, dedup search included.
async fn append_once<M: SentMailbox>(
    blobs: &BlobStore,
    mailbox: &mut M,
    row: &OutboxRow,
    target: &str,
) -> AppendOutcome {
    if row.attempts > 0 {
        // The previous attempt may have been ambiguous (the copy landed but the
        // acknowledgement did not come back), so look before appending.
        match mailbox.search_message_id(target, &row.message_id).await {
            Ok(uids) if !uids.is_empty() => {
                info!(
                    "[outbox] row {} is already in {target}; skipping the APPEND",
                    row.id
                );
                return AppendOutcome::AlreadyPresent {
                    uid: uids.first().map(|u| *u as i64),
                };
            }
            Ok(_) => {}
            Err(e) => {
                // Could not tell. Appending now might duplicate, so do not.
                return AppendOutcome::Failed(format!("Sent dedup search failed: {e:#}"));
            }
        }
    }

    let raw = match blobs.read(&row.raw_blob) {
        Ok(raw) => raw,
        Err(e) => return AppendOutcome::Failed(format!("raw blob unreadable: {e:#}")),
    };
    match mailbox.append(target, &raw).await {
        Ok(uid) => AppendOutcome::Appended {
            uid: uid.map(|u| u as i64),
        },
        Err(e) => AppendOutcome::Failed(format!("APPEND failed: {e:#}")),
    }
}

/// Ingest an acknowledged sent message into the local store.
///
/// Without this the message is on the server but invisible locally until the
/// next Sent sync. The UID is the `APPENDUID` when the server gave one, and
/// otherwise a synthetic id derived from the Message-ID (the same trick the
/// Graph path uses, [`crate::ingest::graph_uid`]), which the next real sync
/// rebinds to the server UID through the `message_id` index.
pub fn ingest_sent_copy(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    mailbox_role: &str,
    raw: &[u8],
    message_id: &str,
    uid: Option<i64>,
) -> Result<()> {
    let Some(mut email) = crate::parse::parse_rfc822_to_fetched_email(raw) else {
        warn!("[outbox] the sent copy of {message_id} did not parse; not ingesting it");
        return Ok(());
    };
    email.is_read = true;
    crate::ingest::ingest_message(
        store,
        blobs,
        &crate::ingest::IngestInput {
            account,
            mailbox: mailbox_role,
            uid: uid.unwrap_or_else(|| crate::ingest::graph_uid(message_id)),
            email: &email,
            raw: Some(raw),
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_through_their_sql_spelling() {
        for state in [
            OutboxState::PendingSend,
            OutboxState::SentPendingAppend,
            OutboxState::Done,
            OutboxState::Failed,
        ] {
            assert_eq!(OutboxState::parse(state.as_str()), Some(state));
        }
        assert_eq!(OutboxState::parse("almost_sent"), None);
    }

    #[test]
    fn only_the_two_working_states_are_open() {
        assert!(OutboxState::PendingSend.is_open());
        assert!(OutboxState::SentPendingAppend.is_open());
        assert!(!OutboxState::Done.is_open());
        assert!(!OutboxState::Failed.is_open());
    }

    #[test]
    fn backoff_doubles_and_then_flattens() {
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(1), BACKOFF_BASE_SECS);
        assert_eq!(backoff_secs(2), BACKOFF_BASE_SECS * 2);
        assert_eq!(backoff_secs(3), BACKOFF_BASE_SECS * 4);
        assert_eq!(backoff_secs(99), BACKOFF_MAX_SECS);
        // Never longer than the ceiling, whatever the counter says.
        assert!(backoff_secs(i64::MAX) <= BACKOFF_MAX_SECS);
    }
}
