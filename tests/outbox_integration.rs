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
    self, OutboxState, SentMailbox, SubmitOutcome,
};
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

    // The resume path does not submit (SMTP belongs to the send path that owns
    // the credentials), it reports the row as awaiting submission.
    let (store, blobs) = account.open();
    let mut sent = FakeSent::new();
    let drained = outbox::drain(&store, &blobs, ACCOUNT, &mut sent, outbox::unix_now())
        .await
        .unwrap();
    assert_eq!(drained.awaiting_submission, 1);
    assert_eq!(*sent.appends.borrow(), 0, "nothing to append yet");
    assert_eq!(state_of(&account, id), OutboxState::PendingSend);

    // The send path submits it, exactly once, and the copy follows.
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
fn a_clean_pre_submission_failure_stays_submittable() {
    let account = Account::new();
    let id = enqueue(&account, "<clean-failure@example.com>", Some(SENT));
    let (store, blobs) = account.open();

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
