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
//!                          |  ^                         ^     |
//!         clean pre-submission failure                  |     | append failed
//!                          |  |  (attempts += 1,        '-----'  (attempts += 1)
//!                          |  |   marker cleared)
//!                          |  '------------------ retry (operator, from failed)
//!                          |
//!            ambiguous SMTP failure --> failed  (manual inspection, never re-sent)
//!            crash after the marker --> failed  (same rule, decided on resume)
//! ```
//!
//! Every arrow is one committed transaction, and each state answers exactly one
//! question about a process that died mid-send:
//!
//! - `pending_send`: SMTP has provably not been accepted, so the resume path
//!   submits. A crash between the commit and the SMTP conversation lands here,
//!   and re-sending is correct because the server never saw the message.
//!   "Provably" is what `submission_started_at` buys: the sender commits that
//!   marker immediately before it opens the SMTP session, so a `pending_send`
//!   row found on restart with a NULL marker was never attempted, while one
//!   with a marker died somewhere inside the conversation and is as ambiguous
//!   as a dropped connection. [`sweep_pending_sends`] hands the first kind
//!   back for submission and moves the second to `failed`.
//! - `sent_pending_append`: the server returned 250 and owns the message now.
//!   SMTP must never run again for this row; only the APPEND is retried. A crash
//!   between the 250 and the APPEND lands here.
//! - `failed`: an *ambiguous* SMTP failure, where the message may or may not
//!   have been accepted. Automatic recovery cannot be safe in either direction,
//!   so the row stops here for a human. It is never auto re-sent.
//! - `done`: the message is in the Sent mailbox (or the account does not want a
//!   local APPEND at all, see [`crate::config::appends_to_sent`]).
//!
//! ## One verdict per recipient
//!
//! SMTP here is one conversation per recipient ([`crate::send::submit`]), so
//! "the message was accepted" is not a fact about the message: a submission can
//! end with one recipient holding it, one refused for good and one still to
//! try. That is what [`SubmitOutcome::PerRecipient`] carries and what the
//! [`Envelope`] records, next to the addresses themselves (#0063). The four
//! states above still describe the row, and the verdicts decide which one it
//! moves to:
//!
//! - a recipient with no verdict parks the row in `failed`, whatever the
//!   others did;
//! - a recipient that can still be tried keeps the row in `pending_send`, one
//!   attempt older, and the next pass attempts only the recipients that are
//!   still outstanding;
//! - once nothing is outstanding the row goes on to `sent_pending_append` (or
//!   straight to `done`) if anybody took the message, and to `failed` if the
//!   server refused them all.
//!
//! A message that reached some of its recipients and not others keeps a note in
//! `last_error` for good, which is what keeps the row in
//! [`unfinished_rows`] after it is `done`: an operator has to be told which
//! recipient never got it, and there is nowhere else to tell them.
//!
//! ## Exactly once
//!
//! SMTP runs at most once per row *and recipient*: [`record_submission`] is the
//! only writer that leaves `pending_send`, the marker turns "we may have
//! submitted" into a committed fact, the driver only submits rows still in that
//! state with no marker, and a resubmission attempts
//! [`Envelope::outstanding`] rather than the whole address list, so a recipient
//! that answered 250 is never spoken to twice. [`retry`] re-arms a `failed` row
//! for a human who has established that the message did not arrive; it clears
//! the marker, so the row is again a single-attempt row rather than a second
//! attempt on top of an unknown first, and it inherits the same delivered set.
//! The APPEND is idempotent by construction instead: a retry (`attempts > 0`)
//! first runs `UID SEARCH HEADER MESSAGE-ID` in the Sent mailbox and skips the
//! APPEND on a hit, because the previous attempt may have been ambiguous in the
//! same way SMTP can be. `APPENDUID` is the definitive acknowledgement and its
//! UID is stored on the row.
//!
//! ## The admission gate
//!
//! Nothing above helps if the same message is queued twice, and a draft is one
//! message however many times send is pressed: every build mints a fresh
//! `Message-ID`, so a second submission looks unrelated to this state machine
//! and to the Sent dedup search. The envelope therefore carries the draft it
//! was built from, and [`enqueue`] refuses a draft that already has a row the
//! outbox owns ([`AlreadyInFlight`]). The in-process half of the gate lives
//! with the caller that races with itself, [`crate::send::send_draft`].
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
//! ## Surviving a store rebuild
//!
//! The rest of the store is dropped and rebuilt when its file goes bad or its
//! schema version moves, because the server holds it back. Nothing holds an
//! outbox row back, so it is carried across the rebuild instead, with its raw
//! blob (#0066, [`crate::store::rebuild`]). A row that cannot be carried is
//! named in a note file next to the store rather than discarded silently.
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
    /// When the sender last committed "the SMTP session is about to open".
    /// `None` means the transport was provably never entered for this row.
    pub submission_started_at: Option<i64>,
    /// The envelope the row was enqueued with, when it is readable.
    pub envelope: Option<Envelope>,
}

/// The SMTP envelope a resumed submission needs, and what became of each
/// recipient in it.
///
/// Stored on the row rather than recovered from the message bytes because the
/// two are not the same thing: lettre drops the `Bcc` header when it builds the
/// message (that is what makes a Bcc blind), so a submission rebuilt from
/// headers alone would silently lose every blind recipient. The envelope is
/// the sender's own record of who the message is going to.
///
/// It is also the sender's record of who already has it (#0063). SMTP is one
/// conversation per recipient here (see [`crate::send::submit`]), so a
/// submission can end with some recipients holding the message and others not,
/// and the row has to survive a restart knowing which is which: `delivered` is
/// what a retry must skip, `rejected` is what the user has to be told about.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Envelope {
    /// Envelope sender, as the `MAIL FROM` address is derived from it.
    pub from: String,
    /// Every recipient with its header role, in the order they were built.
    pub recipients: Vec<(String, crate::send::RecipientRole)>,
    /// The recipients whose own `RCPT TO`/`DATA` ended in a 250. Never
    /// submitted again, by any path, which is what keeps a retry from
    /// delivering twice.
    pub delivered: Vec<String>,
    /// The recipients the server refused for good, with the reason it gave.
    /// Never submitted again either: a 5xx does not become a 250 by waiting.
    pub rejected: Vec<(String, String)>,
    /// The draft this submission was built from, when it came from one. The
    /// durable half of the admission gate: a second send of the same draft
    /// finds this row instead of enqueuing a second copy (#0063).
    pub draft_key: Option<String>,
}

/// Two recipient strings that name the same recipient.
///
/// Compared as written rather than by parsed address: the strings in
/// `recipients` are the ones the send path attempted, so the verdicts come
/// back in exactly that spelling. Case and surrounding space are the only
/// slack allowed.
fn same_recipient(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Flatten a value into one line, so it cannot forge an encoding line break.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

impl Envelope {
    /// One `role:address` per line, `from` first.
    ///
    /// A hand-rolled encoding rather than JSON because the shape is a handful
    /// of fields of plain text and an address can hold neither a newline nor a
    /// colon before its role prefix. The per-recipient verdicts are further
    /// lines of the same shape, so a file written by an older build decodes as
    /// "nothing recorded yet", which is the truth about it.
    pub fn encode(&self) -> String {
        let mut out = format!("from:{}", one_line(&self.from));
        for (addr, role) in &self.recipients {
            out.push('\n');
            out.push_str(match role {
                crate::send::RecipientRole::To => "to:",
                crate::send::RecipientRole::Cc => "cc:",
                crate::send::RecipientRole::Bcc => "bcc:",
            });
            out.push_str(&one_line(addr));
        }
        for addr in &self.delivered {
            out.push_str("\ndelivered:");
            out.push_str(&one_line(addr));
        }
        for (addr, reason) in &self.rejected {
            out.push_str("\nrejected:");
            out.push_str(&one_line(addr));
            out.push('\t');
            out.push_str(&one_line(reason));
        }
        if let Some(key) = &self.draft_key {
            out.push_str("\ndraft:");
            out.push_str(&one_line(key));
        }
        out
    }

    /// Parse [`Envelope::encode`]. Unknown lines are skipped rather than
    /// failing the row: a readable partial envelope is still better than
    /// refusing to load a submission that has to be shown to a human.
    pub fn decode(text: &str) -> Self {
        use crate::send::RecipientRole;
        let mut env = Envelope::default();
        for line in text.lines() {
            let Some((role, addr)) = line.split_once(':') else {
                continue;
            };
            match role {
                "from" => env.from = addr.to_string(),
                "to" => env.recipients.push((addr.to_string(), RecipientRole::To)),
                "cc" => env.recipients.push((addr.to_string(), RecipientRole::Cc)),
                "bcc" => env.recipients.push((addr.to_string(), RecipientRole::Bcc)),
                "delivered" => env.delivered.push(addr.to_string()),
                "rejected" => {
                    let (addr, reason) = addr.split_once('\t').unwrap_or((addr, ""));
                    env.rejected.push((addr.to_string(), reason.to_string()));
                }
                "draft" => env.draft_key = Some(addr.to_string()),
                _ => {}
            }
        }
        env
    }

    /// True when there is enough here to submit from.
    pub fn is_submittable(&self) -> bool {
        !self.from.is_empty() && !self.recipients.is_empty()
    }

    pub fn is_delivered(&self, addr: &str) -> bool {
        self.delivered.iter().any(|a| same_recipient(a, addr))
    }

    pub fn is_rejected(&self, addr: &str) -> bool {
        self.rejected.iter().any(|(a, _)| same_recipient(a, addr))
    }

    /// The recipients that have neither taken the message nor been refused it.
    /// Exactly what a resubmission may attempt, and nothing else.
    pub fn outstanding(&self) -> Vec<(String, crate::send::RecipientRole)> {
        self.recipients
            .iter()
            .filter(|(addr, _)| !self.is_delivered(addr) && !self.is_rejected(addr))
            .cloned()
            .collect()
    }

    /// Record a 250. Idempotent, and it wins over an earlier rejection: a
    /// recipient that has the message must never be attempted again whatever
    /// else was said about it.
    pub fn record_delivered(&mut self, addr: &str) {
        self.rejected.retain(|(a, _)| !same_recipient(a, addr));
        if !self.is_delivered(addr) {
            self.delivered.push(addr.to_string());
        }
    }

    /// Record a refusal that will not change on a retry. Ignored for a
    /// recipient that already took the message.
    pub fn record_rejected(&mut self, addr: &str, reason: &str) {
        if self.is_delivered(addr) || self.is_rejected(addr) {
            return;
        }
        self.rejected.push((addr.to_string(), reason.to_string()));
    }

    /// The line a message that reached some but not all of its recipients
    /// carries for the rest of its life, in `last_error` and therefore in
    /// `mp outbox list`. `None` when nobody was refused.
    pub fn partial_note(&self) -> Option<String> {
        if self.rejected.is_empty() {
            return None;
        }
        let refused = self
            .rejected
            .iter()
            .map(|(addr, reason)| {
                if reason.is_empty() {
                    addr.clone()
                } else {
                    format!("{addr} ({reason})")
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "delivered to {} of {} recipient(s); never delivered to {refused}",
            self.delivered.len(),
            self.recipients.len().max(self.delivered.len() + self.rejected.len()),
        ))
    }
}

/// What one submission pass did to each recipient it attempted (#0063).
///
/// The four buckets are the four answers the state machine can act on, and
/// every recipient of the pass lands in exactly one of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecipientVerdicts {
    /// A 250 of its own. Terminal for that recipient.
    pub delivered: Vec<String>,
    /// A refusal that will not change (a 5xx, an address the transport cannot
    /// even form an envelope from). Terminal for that recipient.
    pub rejected: Vec<(String, String)>,
    /// A refusal that may well change (a 4xx, no connection, no credentials).
    /// The recipient stays outstanding and is attempted again under backoff.
    pub retryable: Vec<(String, String)>,
    /// No verdict came back, so the recipient may or may not hold the message.
    /// Parks the whole row for a human, and is never attempted automatically.
    pub ambiguous: Vec<(String, String)>,
}

impl RecipientVerdicts {
    /// One line naming what happened, for `last_error`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let list = |what: &str, rs: &[(String, String)]| {
            let detail = rs
                .iter()
                .map(|(addr, reason)| format!("{addr}: {reason}"))
                .collect::<Vec<_>>()
                .join("; ");
            format!("{what} {detail}")
        };
        if !self.delivered.is_empty() {
            parts.push(format!("delivered to {}", self.delivered.join(", ")));
        }
        if !self.rejected.is_empty() {
            parts.push(list("refused for", &self.rejected));
        }
        if !self.retryable.is_empty() {
            parts.push(list("not yet delivered to", &self.retryable));
        }
        if !self.ambiguous.is_empty() {
            parts.push(list("no verdict for", &self.ambiguous));
        }
        if parts.is_empty() {
            "no recipients were attempted".to_string()
        } else {
            parts.join(" | ")
        }
    }
}

/// Refusal marker: this draft already has a submission the outbox owns, so a
/// second one would be a second copy in the recipient's mailbox (#0063).
///
/// Carried as an error rather than as a silent no-op because the caller has a
/// user in front of it who pressed send and is owed an answer.
#[derive(Debug)]
pub struct AlreadyInFlight(pub String);

impl std::fmt::Display for AlreadyInFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AlreadyInFlight {}

/// True when an error is the admission gate refusing a duplicate send.
pub fn is_already_in_flight(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.is::<AlreadyInFlight>())
}

/// True when an error is the store being held by another writer.
///
/// The distinction the send path needs (#0063 review): a store that will not
/// open is a store that cannot answer "is this draft already in flight", and
/// the send may proceed without a durable record. A *busy* store is one that
/// can answer and has not been asked yet, so proceeding would drive past an
/// admission gate another process is holding. That one is retryable, not a
/// licence to send.
pub fn is_store_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        matches!(
            c.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
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
    /// a dropped connection, a timeout. The row goes to `failed` for a human.
    Ambiguous(String),
    /// One verdict per recipient, from a transport that talks to them one at a
    /// time (#0063). The row transition is derived from the whole set, and the
    /// verdicts themselves are committed to the envelope so a later pass knows
    /// who already has the message.
    PerRecipient(RecipientVerdicts),
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
///
/// An envelope that names a draft is admitted at most once at a time (#0063):
/// if that draft already has a row the outbox owns, this refuses with
/// [`AlreadyInFlight`] instead of queuing a second copy of the same message.
pub fn enqueue(
    store: &Store,
    blobs: &BlobStore,
    account: &str,
    target_mailbox: Option<&str>,
    message_id: &str,
    raw: &[u8],
    envelope: &Envelope,
) -> Result<i64> {
    let mut span = TimingSpan::with_context("outbox_enqueue", format!("{} bytes", raw.len()));
    // Before the blob is written, so a refused send leaves nothing behind.
    refuse_a_second_submission(store.conn(), account, envelope)?;
    let hash = blobs.write(raw)?;
    span.mark("blob_written");

    let now = unix_now();
    // IMMEDIATE, not the default DEFERRED: this transaction reads to decide
    // whether it may write, and under WAL a deferred read taken before a
    // competing enqueue commits turns the INSERT into `SQLITE_BUSY_SNAPSHOT`.
    // The loser of that race must see the winner's row and be refused by the
    // gate, not fail on the write (#0063 review).
    let tx = store
        .immediate_transaction()
        .context("opening the outbox enqueue transaction")?;
    // Again inside the transaction, where a concurrent enqueue that has
    // already committed is visible.
    refuse_a_second_submission(&tx, account, envelope)?;
    tx.execute(
        "INSERT INTO outbox (
            account, target_mailbox, message_id, raw_blob, state, attempts,
            last_error, appended_uid, created, updated, submission_started_at, envelope
         ) VALUES (?1, ?2, ?3, ?4, 'pending_send', 0, NULL, NULL, ?5, ?5, NULL, ?6)",
        rusqlite::params![
            account,
            target_mailbox,
            message_id,
            hash.as_str(),
            now,
            envelope.encode()
        ],
    )
    .context("inserting the outbox row")?;
    let id = tx.last_insert_rowid();
    blobs.acquire(&tx, &hash, raw.len() as u64)?;
    tx.commit().context("committing the outbox row")?;
    span.mark("committed");

    info!("[outbox] queued {message_id} as row {id} for {account}");
    Ok(id)
}

/// The durable half of the admission gate (#0063).
///
/// A draft is one message however many times the user presses send: the TUI
/// can reach [`crate::send::send_draft`] twice for the same draft (the cursor
/// send and the approved batch it is also in), and a draft whose file could
/// not be retired after a send is still sitting there to be sent again. Every
/// build mints a fresh `Message-ID`, so neither the outbox nor the Sent dedup
/// search would see the second one as the same message; the draft key is what
/// does.
///
/// Only rows the outbox still owns count. A `failed` row is a human's problem
/// and a `done` row is finished, so neither blocks a deliberate re-send.
fn refuse_a_second_submission(
    conn: &rusqlite::Connection,
    account: &str,
    envelope: &Envelope,
) -> Result<()> {
    let Some(key) = envelope.draft_key.as_deref() else {
        return Ok(());
    };
    // Compared flattened on both sides: the stored key went through `one_line`
    // on the way in, so a path-fallback key holding a tab or a trailing space
    // would never compare equal to its own encoding (#0063 review).
    let flattened = one_line(key);
    let mut stmt = conn.prepare(
        "SELECT id, state, envelope FROM outbox
         WHERE account = ?1 AND state IN ('pending_send', 'sent_pending_append')
         ORDER BY id",
    )?;
    let rows = stmt.query_map([account], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, state, encoded) = row?;
        let holds_the_draft = encoded
            .as_deref()
            .map(Envelope::decode)
            .and_then(|env| env.draft_key)
            .is_some_and(|other| one_line(&other) == flattened);
        if holds_the_draft {
            return Err(anyhow::Error::new(AlreadyInFlight(format!(
                "this draft is already in the outbox as row {id} ({state}); \
                 it is sent once, not twice"
            ))));
        }
    }
    Ok(())
}

/// Commit "the SMTP session is about to open" for a `pending_send` row.
///
/// The marker is what makes the resume path decidable: without it a
/// `pending_send` row found after a crash could equally be one that never
/// reached the transport (safe to send) and one that died between the first
/// byte and the 250 (a possible duplicate). Called immediately before the
/// submission and committed on its own, so the window it does not cover is the
/// one instruction between the commit and the connect.
///
/// Refuses any state but `pending_send`, so a caller that re-runs a send cannot
/// re-arm a row that has already left the transport.
pub fn mark_submission_started(store: &Store, id: i64) -> Result<()> {
    let now = unix_now();
    let changed = store
        .conn()
        .execute(
            "UPDATE outbox SET submission_started_at = ?2, updated = ?2
             WHERE id = ?1 AND state = 'pending_send'",
            rusqlite::params![id, now],
        )
        .context("marking the outbox row as entering submission")?;
    if changed == 0 {
        warn!("[outbox] row {id} is not pending_send; not marking a submission start");
    }
    Ok(())
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
            // The row stays submittable, so it must also stay *decidable*: the
            // marker goes back to NULL because this failure proves the server
            // never took the message, and `attempts` is bumped so the automatic
            // resubmission on the next resume backs off instead of hammering a
            // server that is refusing the message for a standing reason.
            store
                .conn()
                .execute(
                    "UPDATE outbox SET attempts = attempts + 1, last_error = ?2, updated = ?3,
                     submission_started_at = NULL WHERE id = ?1",
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
        SubmitOutcome::PerRecipient(verdicts) => {
            record_per_recipient(store, blobs, row, verdicts, now)
        }
    }
}

/// Fold one pass's per-recipient verdicts into the row (#0063).
///
/// The verdicts go into the envelope first, on their own commit: whatever the
/// row's state becomes, who holds the message is a fact about the world and
/// the next pass must not re-derive it. The crash window this leaves (verdicts
/// committed, state not) is the same one that has always sat between the 250
/// and its record, and it resolves the same way: the marker is still set, so
/// [`sweep_pending_sends`] parks the row for a human rather than re-sending.
///
/// The state then follows from what is left outstanding:
///
/// - a recipient with no verdict at all parks the row in `failed`, because a
///   message that may have been delivered is never re-sent automatically;
/// - recipients that can still be retried keep the row in `pending_send`, one
///   attempt older, so the backoff applies and only they are attempted again;
/// - otherwise every recipient is settled: the row goes on towards `done` when
///   at least one took the message, and to `failed` when the server refused
///   them all.
fn record_per_recipient(
    store: &Store,
    blobs: &BlobStore,
    mut row: OutboxRow,
    verdicts: &RecipientVerdicts,
    now: i64,
) -> Result<OutboxState> {
    let mut envelope = row.envelope.clone().unwrap_or_default();
    for addr in &verdicts.delivered {
        envelope.record_delivered(addr);
    }
    for (addr, reason) in &verdicts.rejected {
        envelope.record_rejected(addr, reason);
    }
    store
        .conn()
        .execute(
            "UPDATE outbox SET envelope = ?2, updated = ?3 WHERE id = ?1",
            rusqlite::params![row.id, envelope.encode(), now],
        )
        .context("recording the per-recipient verdicts")?;
    row.envelope = Some(envelope.clone());

    let summary = verdicts.summary();
    if !verdicts.ambiguous.is_empty() {
        store
            .conn()
            .execute(
                "UPDATE outbox SET state = 'failed', last_error = ?2, updated = ?3
                 WHERE id = ?1",
                rusqlite::params![row.id, summary, now],
            )
            .context("marking the outbox row failed")?;
        warn!(
            "[outbox] row {} ({}) has a recipient with no verdict and will not be re-sent: {summary}",
            row.id, row.message_id
        );
        return Ok(OutboxState::Failed);
    }

    if !envelope.outstanding().is_empty() {
        // Same shape as a clean pre-submission failure, and for the same
        // reason: nothing was accepted for these recipients, so the marker
        // goes back to NULL and the row stays decidable.
        store
            .conn()
            .execute(
                "UPDATE outbox SET attempts = attempts + 1, last_error = ?2, updated = ?3,
                 submission_started_at = NULL WHERE id = ?1",
                rusqlite::params![row.id, summary, now],
            )
            .context("recording a partly undelivered submission")?;
        return Ok(OutboxState::PendingSend);
    }

    if envelope.delivered.is_empty() {
        store
            .conn()
            .execute(
                "UPDATE outbox SET state = 'failed', last_error = ?2, updated = ?3
                 WHERE id = ?1",
                rusqlite::params![row.id, summary, now],
            )
            .context("marking the outbox row failed")?;
        warn!(
            "[outbox] row {} ({}) was refused by every recipient: {summary}",
            row.id, row.message_id
        );
        return Ok(OutboxState::Failed);
    }

    // At least one recipient holds the message, and nothing is outstanding:
    // the Sent copy is owed exactly as it is after a clean 250. What the
    // partial case adds is the note, which survives into `done` so the row
    // stays listed until a human has seen who did not get it.
    if row.target_mailbox.is_none() {
        finish_done(store, blobs, &row, None, now)?;
        Ok(OutboxState::Done)
    } else {
        store
            .conn()
            .execute(
                "UPDATE outbox SET state = 'sent_pending_append', last_error = ?2,
                 updated = ?3 WHERE id = ?1",
                rusqlite::params![row.id, envelope.partial_note(), now],
            )
            .context("marking the outbox row sent_pending_append")?;
        Ok(OutboxState::SentPendingAppend)
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
    if row.state != OutboxState::SentPendingAppend {
        // Same belt and braces as `record_submission`: only a row that is
        // actually waiting for an APPEND may be completed by one. A second
        // result for a row already `done` would run `finish_done` twice and
        // release the raw blob twice, unlinking bytes another reference still
        // points at.
        warn!(
            "[outbox] ignoring an APPEND result for row {id}, already in state {}",
            row.state
        );
        return Ok(row.state);
    }
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

    // `done` clears the error it took to get here, with one exception: a
    // message some recipients never got keeps that note for good, because it
    // is the only place the user is ever told (#0063).
    let note = row.envelope.as_ref().and_then(Envelope::partial_note);
    let tx = store
        .conn()
        .unchecked_transaction()
        .context("opening the outbox completion transaction")?;
    tx.execute(
        "UPDATE outbox SET state = 'done', last_error = ?4, appended_uid = ?2, updated = ?3
         WHERE id = ?1",
        rusqlite::params![row.id, uid, now, note],
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
                    last_error, appended_uid, created, updated, submission_started_at,
                    envelope
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
    rows_in_states(
        store,
        account,
        "state IN ('pending_send', 'sent_pending_append')",
    )
}

/// Every row that still has something to say, oldest first: what
/// `mp outbox list` shows.
///
/// `done` rows are the boring majority and say nothing an operator can act on,
/// so they are left out rather than paged over. The exception is a `done` row
/// that kept a note (#0063): a message one of its recipients never got is
/// exactly what an operator has to be told, and there is nowhere else to tell
/// them, so it stays listed until it is discarded.
pub fn unfinished_rows(store: &Store, account: &str) -> Result<Vec<OutboxRow>> {
    rows_in_states(store, account, "(state <> 'done' OR last_error IS NOT NULL)")
}

fn rows_in_states(store: &Store, account: &str, predicate: &str) -> Result<Vec<OutboxRow>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT id, account, target_mailbox, message_id, raw_blob, state, attempts,
                last_error, appended_uid, created, updated, submission_started_at,
                envelope
         FROM outbox
         WHERE account = ?1 AND {predicate}
         ORDER BY id",
    ))?;
    let rows = stmt.query_map([account], row_from_sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// What a resume pass decided about this account's `pending_send` rows.
#[derive(Debug, Default)]
pub struct PendingSends {
    /// Rows whose marker is NULL: the transport was never entered, so the send
    /// path may submit them exactly as if they had just been enqueued.
    pub resubmittable: Vec<OutboxRow>,
    /// Rows that carried a marker and have just been moved to `failed`. The
    /// message may or may not have been delivered, so nothing automatic can be
    /// safe; a human reads them with `mp outbox list`.
    pub stranded: Vec<i64>,
}

/// Classify (and part-resolve) the `pending_send` rows left by a crash.
///
/// Runs before any resubmission, on startup and on the sync tick. The stranded
/// rows are transitioned here, one committed transaction each; the
/// resubmittable ones are handed back because SMTP belongs to the caller that
/// owns the credentials (see [`crate::send::resume_outbox`]).
pub fn sweep_pending_sends(store: &Store, account: &str) -> Result<PendingSends> {
    let mut out = PendingSends::default();
    let now = unix_now();
    for row in open_rows(store, account)? {
        if row.state != OutboxState::PendingSend {
            continue;
        }
        match row.submission_started_at {
            None => out.resubmittable.push(row),
            Some(started) => {
                let err = format!(
                    "submission started at {started} and never returned a verdict; \
                     the message may or may not have been delivered"
                );
                store
                    .conn()
                    .execute(
                        "UPDATE outbox SET state = 'failed', last_error = ?2, updated = ?3
                         WHERE id = ?1 AND state = 'pending_send'",
                        rusqlite::params![row.id, err, now],
                    )
                    .context("failing a stranded submission")?;
                warn!(
                    "[outbox] row {} ({}) died inside its SMTP session; parked as failed, \
                     never auto re-sent",
                    row.id, row.message_id
                );
                out.stranded.push(row.id);
            }
        }
    }
    Ok(out)
}

/// Re-arm a `failed` row for one more submission.
///
/// The operator's half of the exactly-once rule: automatic recovery cannot know
/// whether an ambiguous submission arrived, a human can find out. The marker is
/// cleared, so the row goes back to being a never-attempted `pending_send` and
/// the next resume submits it exactly once.
pub fn retry(store: &Store, id: i64) -> Result<()> {
    let row = load(store, id)?.with_context(|| format!("no outbox row {id}"))?;
    if row.state != OutboxState::Failed {
        return Err(anyhow::anyhow!(
            "outbox row {id} is {}, and only a failed row can be retried",
            row.state
        ));
    }
    store
        .conn()
        .execute(
            "UPDATE outbox SET state = 'pending_send', last_error = NULL,
             submission_started_at = NULL, updated = ?2 WHERE id = ?1",
            rusqlite::params![id, unix_now()],
        )
        .context("re-arming a failed outbox row")?;
    info!("[outbox] row {id} ({}) re-armed for one more send", row.message_id);
    Ok(())
}

/// How many rows are not `done`, split into "still working" and "parked".
///
/// This is what the TUI badge renders, so it is one query rather than a load of
/// every row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboxCounts {
    pub open: usize,
    pub failed: usize,
    /// Rows that are `done` and still carry a note: the message went out, but
    /// not to every recipient it was addressed to (#0063). Parked in the sense
    /// that only a human can close them, by reading the row and discarding it.
    pub partial: usize,
}

impl OutboxCounts {
    pub fn total(self) -> usize {
        self.open + self.failed + self.partial
    }
}

pub fn counts(store: &Store, account: &str) -> Result<OutboxCounts> {
    let mut stmt = store.conn().prepare(
        "SELECT state, last_error IS NOT NULL, COUNT(*) FROM outbox
         WHERE account = ?1 GROUP BY state, last_error IS NOT NULL",
    )?;
    let rows = stmt.query_map([account], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, bool>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut counts = OutboxCounts::default();
    for row in rows {
        let (state, noted, n) = row?;
        match OutboxState::parse(&state) {
            Some(OutboxState::Failed) => counts.failed += n as usize,
            Some(OutboxState::Done) if noted => counts.partial += n as usize,
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
        submission_started_at: row.get(11).unwrap_or(None),
        envelope: row
            .get::<_, Option<String>>(12)
            .unwrap_or(None)
            .map(|text| Envelope::decode(&text)),
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
/// Half of the resume path: it runs on startup and on the normal sync tick, and
/// it drives the APPEND only. Rows in `pending_send` are counted and left
/// alone, because re-submitting them needs the SMTP transport and the account's
/// credentials, which the caller owns: [`crate::send::resume_outbox`] runs
/// [`sweep_pending_sends`] and the resubmission around this pass.
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
    email.flags = email.flags.with_seen(true);
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
