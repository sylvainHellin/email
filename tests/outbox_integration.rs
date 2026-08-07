//! The durable outbox (#0037 item 5).
//!
//! Every test here is offline and deterministic: the state machine is driven
//! directly and the Sent mailbox is a [`FakeSent`], which is what makes the two
//! kill-process acceptance criteria testable at all. A "crash" is simply the
//! test stopping between two committed transitions and reopening the store,
//! which is exactly what a `kill -9` leaves behind: WAL-committed rows and
//! nothing in memory.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use email::config::{
    appends_to_sent, server_saves_to_sent, AccountConfig, AuthMethod, ImapSettings, SaveToSent,
    SmtpSettings,
};
use email::outbox::{
    self, Envelope, OutboxState, RecipientVerdicts, SentMailbox, SubmitOutcome,
};
use email::send::RecipientRole;
use email::store::{BlobStore, Store};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ACCOUNT: &str = "alice";
const SENT: &str = "Sent";

fn raw(message_id: &str) -> Vec<u8> {
    format!(
        "From: alice@example.com\r\n\
         To: bob@example.com\r\n\
         Subject: Durable\r\n\
         Date: Mon, 01 Jan 2024 12:00:00 +0000\r\n\
         Message-ID: {message_id}\r\n\
         \r\n\
         Body.\r\n"
    )
    .into_bytes()
}

/// An account directory: the store and the blob root, laid out as the real one.
struct Account {
    _dir: TempDir,
    path: PathBuf,
    blobs_root: PathBuf,
}

impl Account {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let blobs_root = dir.path().join("blobs");
        Self {
            _dir: dir,
            path,
            blobs_root,
        }
    }

    /// Open the store fresh, the way a restarted process would.
    fn open(&self) -> (Store, BlobStore) {
        (
            Store::open(&self.path).unwrap(),
            BlobStore::new(&self.blobs_root),
        )
    }
}

/// An in-memory Sent mailbox.
///
/// Records every APPEND so a test can assert "exactly one copy", and can be
/// told to fail the next APPEND (a clean failure) or to accept it while losing
/// the acknowledgement (the ambiguous case that makes a retry dangerous).
#[derive(Default)]
struct FakeSent {
    /// mailbox -> appended (uid, message_id)
    appended: RefCell<HashMap<String, Vec<(u32, String)>>>,
    next_uid: RefCell<u32>,
    /// Fail the next APPEND without storing anything.
    fail_next: RefCell<bool>,
    /// Store the next APPEND but report a failure, as a dropped connection
    /// after the server already filed the message would.
    swallow_ack_next: RefCell<bool>,
    /// Make the dedup search fail rather than answer.
    search_fails: RefCell<bool>,
    searches: RefCell<usize>,
    appends: RefCell<usize>,
}

impl FakeSent {
    fn new() -> Self {
        Self {
            next_uid: RefCell::new(100),
            ..Default::default()
        }
    }

    fn copies(&self, message_id: &str) -> usize {
        self.appended
            .borrow()
            .values()
            .flatten()
            .filter(|(_, mid)| mid == message_id)
            .count()
    }

    fn store(&self, mailbox: &str, message_id: &str) -> u32 {
        let mut uid = self.next_uid.borrow_mut();
        let assigned = *uid;
        *uid += 1;
        self.appended
            .borrow_mut()
            .entry(mailbox.to_string())
            .or_default()
            .push((assigned, message_id.to_string()));
        assigned
    }
}

impl SentMailbox for FakeSent {
    async fn search_message_id(
        &mut self,
        mailbox: &str,
        message_id: &str,
    ) -> anyhow::Result<Vec<u32>> {
        *self.searches.borrow_mut() += 1;
        if *self.search_fails.borrow() {
            return Err(anyhow::anyhow!("SEARCH unavailable"));
        }
        Ok(self
            .appended
            .borrow()
            .get(mailbox)
            .map(|rows| {
                rows.iter()
                    .filter(|(_, mid)| mid == message_id)
                    .map(|(uid, _)| *uid)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn append(&mut self, mailbox: &str, raw: &[u8]) -> anyhow::Result<Option<u32>> {
        *self.appends.borrow_mut() += 1;
        let message_id = email::send::message_id_of(raw);
        if *self.fail_next.borrow() {
            *self.fail_next.borrow_mut() = false;
            return Err(anyhow::anyhow!("APPEND refused"));
        }
        let uid = self.store(mailbox, &message_id);
        if *self.swallow_ack_next.borrow() {
            // The copy is filed but the acknowledgement never comes back.
            *self.swallow_ack_next.borrow_mut() = false;
            return Err(anyhow::anyhow!("connection reset after APPEND"));
        }
        Ok(Some(uid))
    }
}

/// The envelope every fixture is queued with: one visible recipient and one
/// blind one, because the blind one is the part the message bytes cannot carry.
fn envelope() -> Envelope {
    Envelope {
        from: "alice@example.com".into(),
        recipients: vec![
            ("bob@example.com".into(), RecipientRole::To),
            ("blind@example.com".into(), RecipientRole::Bcc),
        ],
        ..Default::default()
    }
}

/// A row that has been queued but never submitted, as the moment before SMTP.
fn enqueue(account: &Account, message_id: &str, target: Option<&str>) -> i64 {
    let (store, blobs) = account.open();
    outbox::enqueue(
        &store,
        &blobs,
        ACCOUNT,
        target,
        message_id,
        &raw(message_id),
        &envelope(),
    )
    .unwrap()
}

fn state_of(account: &Account, id: i64) -> OutboxState {
    let (store, _) = account.open();
    outbox::load(&store, id).unwrap().unwrap().state
}

fn imap_account(host: &str) -> AccountConfig {
    AccountConfig {
        name: ACCOUNT.to_string(),
        imap: ImapSettings {
            host: host.to_string(),
            ..Default::default()
        },
        smtp: SmtpSettings {
            host: host.to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Acceptance criterion 1: kill between the outbox commit and SMTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_before_smtp_leaves_the_row_submittable_and_sends_once() {
    let account = Account::new();
    let mid = "<crash-before-smtp@example.com>";
    let id = enqueue(&account, mid, Some(SENT));

    // ---- kill -9 here: nothing else ran. ----

    // On restart the row is still `pending_send`: SMTP provably never saw the
    // message, so re-sending it is the correct move rather than a duplicate.
    assert_eq!(state_of(&account, id), OutboxState::PendingSend);

    // The APPEND driver does not submit (SMTP belongs to the send path that
    // owns the credentials), it reports the row as awaiting submission.
    let (store, blobs) = account.open();
    let mut sent = FakeSent::new();
    let drained = outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(drained.awaiting_submission, 1);
    assert_eq!(*sent.appends.borrow(), 0, "nothing to append yet");
    assert_eq!(state_of(&account, id), OutboxState::PendingSend);

    // The resume sweep hands the row back for submission: no marker means the
    // transport was provably never entered, so sending it is not a duplicate.
    let sweep = outbox::sweep_pending_sends(&store, ACCOUNT).unwrap();
    assert!(sweep.stranded.is_empty());
    assert_eq!(sweep.resubmittable.len(), 1);
    assert_eq!(sweep.resubmittable[0].id, id);
    assert!(
        sweep.resubmittable[0].submission_started_at.is_none(),
        "a never-attempted row carries no marker"
    );

    // The send path marks, submits, records: exactly once, and the copy follows.
    outbox::mark_submission_started(&store, id).unwrap();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();
    assert_eq!(state_of(&account, id), OutboxState::SentPendingAppend);
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(state_of(&account, id), OutboxState::Done);
    assert_eq!(sent.copies(mid), 1, "exactly one copy in Sent");

    // A second submission result for the same row is refused, so a caller that
    // retries the whole send cannot produce a second SMTP delivery.
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();
    assert_eq!(state_of(&account, id), OutboxState::Done);
}

/// The crash window the marker exists for: the process died *inside* the SMTP
/// conversation, so nobody knows whether the message was delivered. Automatic
/// recovery cannot be safe in either direction, so the row is parked.
#[tokio::test]
async fn a_crash_after_the_marker_fails_the_row_and_is_never_auto_re_sent() {
    let account = Account::new();
    let mid = "<crash-inside-smtp@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    {
        let (store, _) = account.open();
        outbox::mark_submission_started(&store, id).unwrap();
    }

    // ---- kill -9 here: the marker is committed, no verdict ever came back. ----

    assert_eq!(state_of(&account, id), OutboxState::PendingSend);
    let (store, blobs) = account.open();
    let sweep = outbox::sweep_pending_sends(&store, ACCOUNT).unwrap();

    assert!(
        sweep.resubmittable.is_empty(),
        "an attempted submission must never be handed back for an automatic re-send"
    );
    assert_eq!(sweep.stranded, vec![id]);
    assert_eq!(state_of(&account, id), OutboxState::Failed);
    let row = outbox::load(&store, id).unwrap().unwrap();
    assert!(row.last_error.unwrap().contains("never returned a verdict"));

    // Every later resume leaves it alone, and its bytes stay readable.
    let sweep = outbox::sweep_pending_sends(&store, ACCOUNT).unwrap();
    assert!(sweep.resubmittable.is_empty() && sweep.stranded.is_empty());
    let mut sent = FakeSent::new();
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(*sent.appends.borrow(), 0);
    assert_eq!(state_of(&account, id), OutboxState::Failed);
    assert_eq!(blobs.read(&row.raw_blob).unwrap(), raw(mid));
}

/// `mp outbox retry`: the only way out of `failed`, and it puts the row back in
/// the never-attempted shape rather than stacking a second attempt on an
/// unknown first one.
#[test]
fn retrying_a_failed_row_re_arms_it_as_never_attempted() {
    let account = Account::new();
    let id = enqueue(&account, "<retry-me@example.com>", Some(SENT));
    let (store, blobs) = account.open();
    outbox::mark_submission_started(&store, id).unwrap();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Ambiguous("lost".into()))
        .unwrap();
    assert_eq!(state_of(&account, id), OutboxState::Failed);

    outbox::retry(&store, id).unwrap();

    let row = outbox::load(&store, id).unwrap().unwrap();
    assert_eq!(row.state, OutboxState::PendingSend);
    assert_eq!(row.submission_started_at, None, "the marker must be cleared");
    assert_eq!(row.last_error, None);
    let sweep = outbox::sweep_pending_sends(&store, ACCOUNT).unwrap();
    assert_eq!(sweep.resubmittable.len(), 1, "the next resume submits it once");

    // Only from `failed`: a row that is mid-flight cannot be re-armed under the
    // send path's feet.
    outbox::mark_submission_started(&store, id).unwrap();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();
    assert!(outbox::retry(&store, id).is_err());
}

/// The envelope is stored because the message bytes cannot carry it: lettre
/// drops the `Bcc` header when it builds, so a submission resumed from headers
/// alone would silently lose every blind recipient.
#[test]
fn the_stored_envelope_keeps_the_blind_recipients_a_resumed_send_needs() {
    let account = Account::new();
    let mid = "<bcc-survives@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, _) = account.open();

    // The bytes on the wire hold no trace of the blind recipient.
    let row = outbox::load(&store, id).unwrap().unwrap();
    assert!(!String::from_utf8_lossy(&raw(mid)).contains("blind@example.com"));

    let envelope = row.envelope.expect("the row must carry its envelope");
    assert!(envelope.is_submittable());
    assert_eq!(envelope, self::envelope());
    assert!(envelope
        .recipients
        .iter()
        .any(|(addr, role)| addr == "blind@example.com" && *role == RecipientRole::Bcc));

    // And it round-trips through its stored form unchanged.
    assert_eq!(Envelope::decode(&envelope.encode()), envelope);
}

// ---------------------------------------------------------------------------
// Acceptance criterion 2: kill between SMTP 250 and the APPEND
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_between_smtp_and_append_completes_the_append_on_resume() {
    let account = Account::new();
    let mid = "<crash-before-append@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    {
        let (store, blobs) = account.open();
        outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();
    }

    // ---- kill -9 here: the 250 is committed, the APPEND never ran. ----

    assert_eq!(state_of(&account, id), OutboxState::SentPendingAppend);

    let (store, blobs) = account.open();
    let mut sent = FakeSent::new();
    let drained = outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();

    assert_eq!(drained.completed, 1);
    assert_eq!(state_of(&account, id), OutboxState::Done);
    assert_eq!(sent.copies(mid), 1, "the Sent mailbox holds exactly one copy");

    // The sent message is in the local store too, so Sent shows it without
    // waiting for the next sync.
    let local: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE account = ?1 AND mailbox = 'sent' \
             AND message_id = ?2",
            (ACCOUNT, mid),
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(local, 1);
}

#[tokio::test]
async fn an_ambiguous_append_is_deduped_rather_than_duplicated_on_retry() {
    let account = Account::new();
    let mid = "<ambiguous-append@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();

    // First attempt: the server files the copy, then the connection dies
    // before the acknowledgement. This is the case that duplicates Sent items
    // in every best-effort client.
    let mut sent = FakeSent::new();
    *sent.swallow_ack_next.borrow_mut() = true;
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(state_of(&account, id), OutboxState::SentPendingAppend);
    assert_eq!(sent.copies(mid), 1);

    // Retry, past the backoff: the dedup search sees the copy and the APPEND
    // is skipped.
    let later = outbox::unix_now() + outbox::BACKOFF_MAX_SECS + 1;
    let appends_before = *sent.appends.borrow();
    let drained = outbox::drain(&store, &blobs, ACCOUNT, &mut sent, later)
        .await
        .unwrap();

    assert_eq!(drained.deduped, 1, "the retry must skip the APPEND");
    assert_eq!(*sent.appends.borrow(), appends_before, "no second APPEND");
    assert_eq!(sent.copies(mid), 1, "exactly one copy in Sent");
    assert_eq!(state_of(&account, id), OutboxState::Done);
}

#[tokio::test]
async fn a_retry_whose_dedup_search_fails_does_not_append() {
    let account = Account::new();
    let mid = "<search-broken@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();

    let mut sent = FakeSent::new();
    *sent.fail_next.borrow_mut() = true;
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();

    // Now the search itself is unavailable: "cannot tell" must not become
    // "append anyway".
    *sent.search_fails.borrow_mut() = true;
    let later = outbox::unix_now() + outbox::BACKOFF_MAX_SECS + 1;
    let appends_before = *sent.appends.borrow();
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, later)
        .await
        .unwrap();

    assert_eq!(*sent.appends.borrow(), appends_before);
    assert_eq!(state_of(&account, id), OutboxState::SentPendingAppend);
}

#[tokio::test]
async fn the_appended_uid_is_stored_on_the_row() {
    let account = Account::new();
    let mid = "<uid-stored@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();

    let mut sent = FakeSent::new();
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();

    let row = outbox::load(&store, id).unwrap().unwrap();
    assert_eq!(row.state, OutboxState::Done);
    assert_eq!(row.appended_uid, Some(100), "APPENDUID must land on the row");
}

// ---------------------------------------------------------------------------
// SMTP exactly once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_ambiguous_smtp_failure_fails_the_row_and_is_never_re_sent() {
    let account = Account::new();
    let mid = "<ambiguous-smtp@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();

    outbox::record_submission(
        &store,
        &blobs,
        id,
        &SubmitOutcome::Ambiguous("connection reset after DATA".into()),
    )
    .unwrap();

    let row = outbox::load(&store, id).unwrap().unwrap();
    assert_eq!(row.state, OutboxState::Failed);
    assert!(row.last_error.unwrap().contains("connection reset"));

    // The driver leaves it alone: no APPEND, no re-send, no state change.
    let mut sent = FakeSent::new();
    let drained = outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(drained, Default::default());
    assert_eq!(*sent.appends.borrow(), 0);
    assert_eq!(state_of(&account, id), OutboxState::Failed);

    // And the raw bytes are still readable, which is the point of `failed`.
    let row = outbox::load(&store, id).unwrap().unwrap();
    assert_eq!(blobs.read(&row.raw_blob).unwrap(), raw(mid));
}

#[test]
fn a_clean_pre_submission_failure_stays_submittable_and_backs_off() {
    let account = Account::new();
    let id = enqueue(&account, "<clean-failure@example.com>", Some(SENT));
    let (store, blobs) = account.open();
    outbox::mark_submission_started(&store, id).unwrap();

    let state = outbox::record_submission(
        &store,
        &blobs,
        id,
        &SubmitOutcome::CleanPreSubmission("could not resolve smtp.example.com".into()),
    )
    .unwrap();

    assert_eq!(state, OutboxState::PendingSend);
    let row = outbox::load(&store, id).unwrap().unwrap();
    assert!(row.last_error.unwrap().contains("could not resolve"));
    // The failure proves nothing was delivered, so the marker goes back to
    // NULL: the row is a never-attempted row again, not an ambiguous one.
    assert_eq!(row.submission_started_at, None);
    // And the attempt is counted, so the automatic resubmission on the next
    // resume waits instead of hammering a server that is refusing the message.
    assert_eq!(row.attempts, 1);
    assert!(outbox::backoff_secs(row.attempts) > 0);
    assert_eq!(
        outbox::sweep_pending_sends(&store, ACCOUNT).unwrap().resubmittable.len(),
        1
    );
}

/// `record_append` refuses a result for a row that is not waiting for an
/// APPEND. Without the guard a second call would run the completion path twice
/// and release the raw blob twice, unlinking bytes the ingested message still
/// references.
#[tokio::test]
async fn a_second_append_result_cannot_release_the_blob_twice() {
    let account = Account::new();
    let mid = "<double-append@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();

    let mut sent = FakeSent::new();
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    let hash = outbox::load(&store, id).unwrap().unwrap().raw_blob;
    assert_eq!(state_of(&account, id), OutboxState::Done);
    assert_eq!(
        email::store::blobs::refcount(store.conn(), &hash).unwrap(),
        1,
        "the ingested message owns the reference now"
    );

    let state = outbox::record_append(
        &store,
        &blobs,
        id,
        &email::outbox::AppendOutcome::Appended { uid: Some(999) },
    )
    .unwrap();

    assert_eq!(state, OutboxState::Done);
    assert_eq!(
        email::store::blobs::refcount(store.conn(), &hash).unwrap(),
        1,
        "a repeated result must not decrement the reference again"
    );
    assert!(blobs.contains(&hash), "and the bytes must still be there");
    assert_eq!(
        outbox::load(&store, id).unwrap().unwrap().appended_uid,
        Some(100),
        "the original acknowledgement stands"
    );
}

// ---------------------------------------------------------------------------
// Blob refcounting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_raw_blob_reference_moves_to_the_message_and_survives_completion() {
    let account = Account::new();
    let mid = "<refcount@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();

    let row = outbox::load(&store, id).unwrap().unwrap();
    let hash = row.raw_blob.clone();
    assert_eq!(
        email::store::blobs::refcount(store.conn(), &hash).unwrap(),
        1,
        "the outbox row holds a plain blobs-table reference"
    );
    // No `message_blobs` row: that table's FK targets `messages`, and an
    // outbox row is a submission, not a message.
    let refs: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM message_blobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(refs, 0);

    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();
    let mut sent = FakeSent::new();
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();

    // The outbox reference is released, the ingested message now owns one, and
    // the file never passed through refcount zero (it is still readable).
    assert_eq!(
        email::store::blobs::refcount(store.conn(), &hash).unwrap(),
        1
    );
    assert!(blobs.contains(&hash));
    assert_eq!(blobs.read(&hash).unwrap(), raw(mid));
}

#[test]
fn discarding_a_failed_row_releases_its_bytes() {
    let account = Account::new();
    let mid = "<discard@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Ambiguous("lost".into()))
        .unwrap();

    let hash = outbox::load(&store, id).unwrap().unwrap().raw_blob;
    assert!(blobs.contains(&hash), "a failed row keeps its bytes");

    outbox::discard(&store, &blobs, id).unwrap();
    assert!(outbox::load(&store, id).unwrap().is_none());
    assert!(!blobs.contains(&hash), "discard is what releases them");
}

// ---------------------------------------------------------------------------
// Counts, backoff and the no-append path
// ---------------------------------------------------------------------------

#[test]
fn counts_split_open_rows_from_failed_ones() {
    let account = Account::new();
    let (store, blobs) = account.open();
    let a = enqueue(&account, "<a@example.com>", Some(SENT));
    let b = enqueue(&account, "<b@example.com>", Some(SENT));
    let c = enqueue(&account, "<c@example.com>", Some(SENT));

    outbox::record_submission(&store, &blobs, b, &SubmitOutcome::Accepted).unwrap();
    outbox::record_submission(&store, &blobs, c, &SubmitOutcome::Ambiguous("lost".into())).unwrap();

    let counts = outbox::counts(&store, ACCOUNT).unwrap();
    assert_eq!(counts.open, 2, "{a} is pending_send and {b} pending append");
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.total(), 3);
}

#[tokio::test]
async fn a_row_within_its_backoff_window_is_not_retried() {
    let account = Account::new();
    let id = enqueue(&account, "<backoff@example.com>", Some(SENT));
    let (store, blobs) = account.open();
    outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();

    let mut sent = FakeSent::new();
    *sent.fail_next.borrow_mut() = true;
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    let attempts = outbox::load(&store, id).unwrap().unwrap().attempts;
    assert_eq!(attempts, 1);

    // Immediately after the failure the row is still inside its backoff.
    let appends_before = *sent.appends.borrow();
    let drained = outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(drained.still_open, 1);
    assert_eq!(*sent.appends.borrow(), appends_before);

    // Past it, the retry runs.
    let later = outbox::unix_now() + outbox::backoff_secs(1) + 1;
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, later)
        .await
        .unwrap();
    assert_eq!(state_of(&account, id), OutboxState::Done);
}

#[test]
fn a_row_with_no_target_mailbox_completes_on_the_250() {
    let account = Account::new();
    let mid = "<server-saves@example.com>";
    let id = enqueue(&account, mid, None);
    let (store, blobs) = account.open();

    let state = outbox::record_submission(&store, &blobs, id, &SubmitOutcome::Accepted).unwrap();
    assert_eq!(state, OutboxState::Done, "no APPEND is ever attempted");

    // The local copy is still ingested, so Sent shows it immediately.
    let local: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE mailbox = 'sent' AND message_id = ?1",
            [mid],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(local, 1);
}

// ---------------------------------------------------------------------------
// save_to_sent
// ---------------------------------------------------------------------------

#[test]
fn save_to_sent_auto_skips_the_accounts_whose_server_saves() {
    // Gmail, Graph and Proton file their own copy.
    for host in ["imap.gmail.com", "smtp.googlemail.com", "127.0.0.1.protonmail"] {
        let account = imap_account(host);
        assert!(server_saves_to_sent(&account), "{host} should be detected");
        assert!(!appends_to_sent(&account), "{host} must not be appended to");
    }
    let mut graph = imap_account("outlook.office365.com");
    graph.auth_method = AuthMethod::Graph;
    assert!(server_saves_to_sent(&graph));
    assert!(!appends_to_sent(&graph));

    // Generic IMAP does not, so the client saves the copy.
    let generic = imap_account("mail.example.com");
    assert!(!server_saves_to_sent(&generic));
    assert!(appends_to_sent(&generic));
}

#[test]
fn save_to_sent_overrides_win_over_the_detection() {
    let mut gmail = imap_account("imap.gmail.com");
    gmail.save_to_sent = SaveToSent::Always;
    assert!(appends_to_sent(&gmail), "always must override the detection");

    let mut generic = imap_account("mail.example.com");
    generic.save_to_sent = SaveToSent::Never;
    assert!(!appends_to_sent(&generic), "never must override it too");
}

#[test]
fn save_to_sent_round_trips_through_the_config_file() {
    let toml = r#"
[[accounts]]
name = "work"
save_to_sent = "always"

[[accounts]]
name = "gmail"
save_to_sent = "never"

[[accounts]]
name = "default"
"#;
    let config: email::config::GlobalConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.accounts[0].save_to_sent, SaveToSent::Always);
    assert_eq!(config.accounts[1].save_to_sent, SaveToSent::Never);
    assert_eq!(
        config.accounts[2].save_to_sent,
        SaveToSent::Auto,
        "an unset flag defaults to auto"
    );
}

// ---------------------------------------------------------------------------
// Partial recipient verdicts (#0063)
// ---------------------------------------------------------------------------

/// The two recipients of [`envelope`], as a submission that reached one of
/// them and was refused for good by the other.
fn one_of_two(delivered: &str, rejected: &str, reason: &str) -> SubmitOutcome {
    SubmitOutcome::PerRecipient(RecipientVerdicts {
        delivered: vec![delivered.to_string()],
        rejected: vec![(rejected.to_string(), reason.to_string())],
        ..Default::default()
    })
}

fn envelope_of(account: &Account, id: i64) -> Envelope {
    let (store, _) = account.open();
    outbox::load(&store, id)
        .unwrap()
        .unwrap()
        .envelope
        .expect("every queued row carries its envelope")
}

/// The acceptance criterion: one of two recipients is rejected, and the row
/// reaches a terminal state that still names the one who never got it.
#[tokio::test]
async fn a_rejected_recipient_is_named_on_the_row_and_survives_a_restart() {
    let account = Account::new();
    let mid = "<one-of-two@example.com>";
    let id = enqueue(&account, mid, Some(SENT));
    let (store, blobs) = account.open();
    outbox::mark_submission_started(&store, id).unwrap();

    let state = outbox::record_submission(
        &store,
        &blobs,
        id,
        &one_of_two("bob@example.com", "blind@example.com", "550 no such mailbox"),
    )
    .unwrap();

    // One recipient holds the message, so the Sent copy is owed exactly as
    // after a clean 250; nothing is outstanding, so nothing is retried.
    assert_eq!(state, OutboxState::SentPendingAppend);
    let mut sent = FakeSent::new();
    outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(state_of(&account, id), OutboxState::Done);
    assert_eq!(sent.copies(mid), 1);

    // ---- restart: everything below is read out of the file. ----
    let (store, _) = account.open();
    let row = outbox::load(&store, id).unwrap().unwrap();
    let envelope = row.envelope.clone().unwrap();
    assert_eq!(envelope.delivered, vec!["bob@example.com".to_string()]);
    assert_eq!(
        envelope.rejected,
        vec![(
            "blind@example.com".to_string(),
            "550 no such mailbox".to_string()
        )]
    );
    assert!(
        envelope.outstanding().is_empty(),
        "a settled row has nothing left to attempt"
    );

    // A `done` row is normally silent; this one keeps its note, so it is still
    // listed and still counted as something a human has to close.
    let note = row.last_error.expect("a partial delivery keeps its note");
    assert!(note.contains("blind@example.com"), "{note}");
    let listed = outbox::unfinished_rows(&store, ACCOUNT).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    let counts = outbox::counts(&store, ACCOUNT).unwrap();
    assert_eq!(counts.partial, 1);
    assert_eq!(counts.open, 0);
    assert_eq!(counts.failed, 0);
    assert_eq!(counts.total(), 1);
}

/// A recipient that can still be delivered to keeps the row submittable, and
/// the one that already answered 250 is not attempted again.
#[test]
fn only_the_undelivered_recipients_are_retried() {
    let account = Account::new();
    let id = enqueue(&account, "<retry-the-rest@example.com>", Some(SENT));
    let (store, blobs) = account.open();
    outbox::mark_submission_started(&store, id).unwrap();

    let state = outbox::record_submission(
        &store,
        &blobs,
        id,
        &SubmitOutcome::PerRecipient(RecipientVerdicts {
            delivered: vec!["bob@example.com".to_string()],
            retryable: vec![(
                "blind@example.com".to_string(),
                "451 try again later".to_string(),
            )],
            ..Default::default()
        }),
    )
    .unwrap();

    assert_eq!(state, OutboxState::PendingSend);
    let row = outbox::load(&store, id).unwrap().unwrap();
    assert_eq!(row.attempts, 1, "the backoff counter moved");
    assert_eq!(
        row.submission_started_at, None,
        "nothing was accepted for the outstanding recipient, so the row stays decidable"
    );
    let envelope = row.envelope.unwrap();
    assert_eq!(
        envelope.outstanding(),
        vec![("blind@example.com".to_string(), RecipientRole::Bcc)],
        "the delivered recipient is never submitted again"
    );

    // The resume hands it back, and the second pass settles it.
    let sweep = outbox::sweep_pending_sends(&store, ACCOUNT).unwrap();
    assert_eq!(sweep.resubmittable.len(), 1);
    outbox::mark_submission_started(&store, id).unwrap();
    let state = outbox::record_submission(
        &store,
        &blobs,
        id,
        &SubmitOutcome::PerRecipient(RecipientVerdicts {
            delivered: vec!["blind@example.com".to_string()],
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(state, OutboxState::SentPendingAppend);
    let envelope = envelope_of(&account, id);
    assert_eq!(
        envelope.delivered,
        vec![
            "bob@example.com".to_string(),
            "blind@example.com".to_string()
        ],
        "each recipient took the message exactly once"
    );
    assert_eq!(
        envelope.partial_note(),
        None,
        "everybody got it in the end, so there is nothing to report"
    );
}

/// Nothing was delivered and nothing will be: the row stops, visibly, without
/// a human having to notice it looping.
#[test]
fn a_message_every_recipient_refused_reaches_a_terminal_state() {
    let account = Account::new();
    let id = enqueue(&account, "<all-refused@example.com>", Some(SENT));
    let (store, blobs) = account.open();
    outbox::mark_submission_started(&store, id).unwrap();

    let state = outbox::record_submission(
        &store,
        &blobs,
        id,
        &SubmitOutcome::PerRecipient(RecipientVerdicts {
            rejected: vec![
                ("bob@example.com".to_string(), "550 unknown".to_string()),
                ("blind@example.com".to_string(), "550 unknown".to_string()),
            ],
            ..Default::default()
        }),
    )
    .unwrap();

    assert_eq!(state, OutboxState::Failed);
    let sweep = outbox::sweep_pending_sends(&store, ACCOUNT).unwrap();
    assert!(
        sweep.resubmittable.is_empty() && sweep.stranded.is_empty(),
        "a terminal row is not swept again"
    );
    let row = outbox::load(&store, id).unwrap().unwrap();
    assert!(row.last_error.unwrap().contains("bob@example.com"));
}

/// One recipient took the message, another never answered: the unknown one is
/// what decides, so the row parks for a human. What it took is recorded, so
/// the retry a human orders does not deliver twice.
#[test]
fn a_recipient_with_no_verdict_parks_the_row_and_a_retry_skips_the_delivered() {
    let account = Account::new();
    let id = enqueue(&account, "<no-verdict@example.com>", Some(SENT));
    let (store, blobs) = account.open();
    outbox::mark_submission_started(&store, id).unwrap();

    let state = outbox::record_submission(
        &store,
        &blobs,
        id,
        &SubmitOutcome::PerRecipient(RecipientVerdicts {
            delivered: vec!["bob@example.com".to_string()],
            ambiguous: vec![(
                "blind@example.com".to_string(),
                "connection reset after DATA".to_string(),
            )],
            ..Default::default()
        }),
    )
    .unwrap();

    assert_eq!(state, OutboxState::Failed);
    assert!(outbox::sweep_pending_sends(&store, ACCOUNT)
        .unwrap()
        .resubmittable
        .is_empty());

    // The human decides the blind recipient never got it and retries.
    outbox::retry(&store, id).unwrap();
    let envelope = envelope_of(&account, id);
    assert_eq!(
        envelope.outstanding(),
        vec![("blind@example.com".to_string(), RecipientRole::Bcc)],
        "the recipient that answered 250 is never in a retry"
    );
}

/// The crash window per-recipient recording opens: the verdicts are committed
/// and the state transition is not. The marker is still set, so the row is
/// parked rather than re-sent, and what was delivered is still known.
#[test]
fn a_crash_between_the_verdicts_and_the_transition_parks_the_row() {
    let account = Account::new();
    let id = enqueue(&account, "<crash-mid-record@example.com>", Some(SENT));
    {
        let (store, _) = account.open();
        outbox::mark_submission_started(&store, id).unwrap();
        // What `record_submission` commits first, on its own.
        let mut envelope = envelope_of(&account, id);
        envelope.record_delivered("bob@example.com");
        store
            .conn()
            .execute(
                "UPDATE outbox SET envelope = ?2 WHERE id = ?1",
                rusqlite::params![id, envelope.encode()],
            )
            .unwrap();
    }

    // ---- kill -9 here. ----
    let (store, _) = account.open();
    let sweep = outbox::sweep_pending_sends(&store, ACCOUNT).unwrap();
    assert_eq!(sweep.stranded, vec![id]);
    assert_eq!(state_of(&account, id), OutboxState::Failed);
    let envelope = envelope_of(&account, id);
    assert!(envelope.is_delivered("bob@example.com"));
    outbox::retry(&store, id).unwrap();
    assert_eq!(
        envelope_of(&account, id).outstanding(),
        vec![("blind@example.com".to_string(), RecipientRole::Bcc)],
        "even the re-armed row knows who already has it"
    );
}

/// The envelope encoding is what all of this survives a restart in, so it has
/// to round-trip, and a file written before #0063 has to read as "nothing
/// recorded yet" rather than fail.
#[test]
fn the_envelope_encoding_carries_the_verdicts_and_stays_backwards_readable() {
    let mut envelope = Envelope {
        from: "alice@example.com".into(),
        recipients: vec![
            ("\"Doe, Jane\" <jane@example.com>".into(), RecipientRole::To),
            ("blind@example.com".into(), RecipientRole::Bcc),
        ],
        draft_key: Some("id:2026-08-06-note".into()),
        ..Default::default()
    };
    envelope.record_delivered("\"Doe, Jane\" <jane@example.com>");
    envelope.record_rejected("blind@example.com", "550 no\nsuch\tmailbox");

    let decoded = Envelope::decode(&envelope.encode());
    assert_eq!(decoded.from, envelope.from);
    assert_eq!(decoded.recipients, envelope.recipients);
    assert_eq!(decoded.draft_key, envelope.draft_key);
    assert_eq!(
        decoded.delivered,
        vec!["\"Doe, Jane\" <jane@example.com>".to_string()]
    );
    assert_eq!(
        decoded.rejected,
        vec![(
            "blind@example.com".to_string(),
            "550 no such mailbox".to_string()
        )],
        "a reason cannot smuggle a line break into the encoding"
    );

    let old = Envelope::decode("from:alice@example.com\nto:bob@example.com");
    assert!(old.delivered.is_empty() && old.rejected.is_empty() && old.draft_key.is_none());
    assert_eq!(
        old.outstanding(),
        vec![("bob@example.com".to_string(), RecipientRole::To)],
        "a row queued before the verdicts existed is entirely outstanding"
    );
}

// ---------------------------------------------------------------------------
// The admission gate (#0063)
// ---------------------------------------------------------------------------

/// Queue a submission the way a draft send does: keyed on the draft.
fn enqueue_draft(account: &Account, message_id: &str, draft: &str) -> anyhow::Result<i64> {
    let (store, blobs) = account.open();
    outbox::enqueue(
        &store,
        &blobs,
        ACCOUNT,
        Some(SENT),
        message_id,
        &raw(message_id),
        &Envelope {
            draft_key: Some(draft.to_string()),
            ..envelope()
        },
    )
}

/// One draft is one message however many times send is pressed: every build
/// mints a fresh Message-ID, so the draft key is the only thing the second
/// submission shares with the first.
#[test]
fn a_second_submission_of_the_same_draft_is_refused_while_the_first_is_open() {
    let account = Account::new();
    let first = enqueue_draft(&account, "<first-build@example.com>", "id:note-1").unwrap();

    let refused = enqueue_draft(&account, "<second-build@example.com>", "id:note-1")
        .expect_err("the same draft must not be queued twice");
    assert!(outbox::is_already_in_flight(&refused), "{refused:#}");
    assert!(refused.to_string().contains(&first.to_string()));

    // A different draft is not affected.
    enqueue_draft(&account, "<other-draft@example.com>", "id:note-2").unwrap();

    // Nor is a submission that names no draft at all (an RSVP reply).
    enqueue(&account, "<rsvp@example.com>", Some(SENT));

    // Once the first row is out of the outbox's hands, a deliberate re-send is
    // the user's business again.
    let (store, blobs) = account.open();
    outbox::record_submission(&store, &blobs, first, &SubmitOutcome::Ambiguous("lost".into()))
        .unwrap();
    enqueue_draft(&account, "<third-build@example.com>", "id:note-1")
        .expect("a failed row is a human's problem, not a lock");
}
