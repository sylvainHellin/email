//! Full-text search over the store, end to end (#0043).
//!
//! Offline, against a store and a blob directory in a tempdir. What is asserted
//! here is the pair the ticket asks for: that a query returns the right rows in
//! a useful order, and that the index behind it never drifts from the rows it
//! describes -- across ingest, re-ingest of the same UID, a UIDVALIDITY rebind,
//! a delete and a prune.

use mailypoppins::ingest::{ingest_message, prune_vanished, IngestInput};
use mailypoppins::parse::FetchedEmail;
use mailypoppins::store::search::{fts_expression, index_drift, search};
use mailypoppins::store::{BlobStore, Store};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    store: Store,
    blobs: BlobStore,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("store.sqlite3")).unwrap();
        let blobs = BlobStore::new(tmp.path().join("blobs"));
        Self { _tmp: tmp, store, blobs }
    }

    fn ingest(&self, mailbox: &str, uid: i64, email: &FetchedEmail) -> i64 {
        ingest_message(
            &self.store,
            &self.blobs,
            &IngestInput { account: "acct", mailbox, uid, email, raw: None },
        )
        .unwrap()
        .row_id
    }

    fn hits(&self, query: &str) -> Vec<i64> {
        search(&self.store, "acct", query, None, 20)
            .unwrap()
            .into_iter()
            .map(|h| h.row.id)
            .collect()
    }

    fn hits_in(&self, query: &str, mailbox: &str) -> Vec<i64> {
        search(&self.store, "acct", query, Some(mailbox), 20)
            .unwrap()
            .into_iter()
            .map(|h| h.row.id)
            .collect()
    }
}

fn email(message_id: &str, from: &str, subject: &str, body: &str) -> FetchedEmail {
    FetchedEmail {
        from: from.into(),
        to: "me@example.com".into(),
        cc: None,
        reply_to: None,
        bcc: None,
        subject: subject.into(),
        date: "Thu, 7 Aug 2026 10:00:00 +0000".into(),
        body_text: body.into(),
        html_body: None,
        has_attachments: false,
        message_id: Some(format!("<{message_id}@example.com>")),
        attachments: Vec::new(),
        flags: Default::default(),
        calendar_ics: None,
        event: None,
    }
}

/// The base assertion of the whole feature: a word in a body that was never
/// written to a file is found by a `SELECT`.
#[test]
fn a_word_in_the_body_is_found() {
    let f = Fixture::new();
    let row = f.ingest(
        "inbox",
        1,
        &email("a", "Ada <ada@example.com>", "Hello", "the quarterly ledger is attached"),
    );
    f.ingest("inbox", 2, &email("b", "Bob <bob@example.com>", "Lunch", "pizza?"));
    assert_eq!(f.hits("ledger"), vec![row]);
    assert!(f.hits("aardvark").is_empty());
}

/// Subject and sender are indexed columns too, and the field filters address
/// them individually.
#[test]
fn subject_and_sender_are_searchable_and_addressable() {
    let f = Fixture::new();
    let row = f.ingest(
        "inbox",
        1,
        &email("a", "Ada Lovelace <ada@example.com>", "Quarterly report", "nothing here"),
    );
    // A decoy whose *body* says the word the subject filter must not match.
    let decoy = f.ingest(
        "inbox",
        2,
        &email("b", "Bob <bob@example.com>", "Lunch", "quarterly pizza budget"),
    );

    assert_eq!(f.hits("subject:quarterly"), vec![row]);
    assert_eq!(f.hits("body:quarterly"), vec![decoy]);
    assert_eq!(f.hits("from:lovelace"), vec![row]);
    // Unfiltered, both match; ranking is asserted separately.
    assert_eq!(f.hits("quarterly").len(), 2);
}

/// Terms are AND-ed, which is what makes a second word narrow a search rather
/// than widen it.
#[test]
fn several_terms_all_have_to_match() {
    let f = Fixture::new();
    let both = f.ingest("inbox", 1, &email("a", "a@example.com", "Trip", "berlin in october"));
    f.ingest("inbox", 2, &email("b", "b@example.com", "Trip", "berlin in march"));
    assert_eq!(f.hits("berlin october"), vec![both]);
}

/// A quoted phrase matches adjacency, not the same words anywhere.
#[test]
fn a_phrase_matches_adjacent_words_only() {
    let f = Fixture::new();
    let phrase = f.ingest(
        "inbox",
        1,
        &email("a", "a@example.com", "One", "please sign the lease agreement today"),
    );
    f.ingest(
        "inbox",
        2,
        &email("b", "b@example.com", "Two", "the agreement is not a lease"),
    );
    assert_eq!(f.hits("\"lease agreement\""), vec![phrase]);
    // Unquoted, both documents carry both words.
    assert_eq!(f.hits("lease agreement").len(), 2);
}

/// A trailing `*` is a prefix query; without it the term is a whole token.
#[test]
fn a_trailing_star_matches_a_prefix() {
    let f = Fixture::new();
    let row = f.ingest(
        "inbox",
        1,
        &email("a", "a@example.com", "Bill", "the invoicing run finished"),
    );
    assert_eq!(f.hits("invoic*"), vec![row]);
    assert!(
        f.hits("invoic").is_empty(),
        "a bare term must not match a longer token"
    );
}

/// Non-ASCII text survives the round trip through the tokenizer, and so does a
/// query typed with the same accents.
#[test]
fn unicode_terms_round_trip() {
    let f = Fixture::new();
    let row = f.ingest(
        "inbox",
        1,
        &email("a", "Émile <emile@example.com>", "Réunion d'équipe", "on se réunit à Zürich"),
    );
    assert_eq!(f.hits("Zürich"), vec![row]);
    assert_eq!(f.hits("réunit"), vec![row]);
    assert_eq!(f.hits("subject:équipe"), vec![row]);
    // Case folding is the tokenizer's, so an uppercase query is the same query.
    assert_eq!(f.hits("ZÜRICH"), vec![row]);
}

/// Punctuation a user types is text, not FTS5 syntax. Every one of these is a
/// syntax error when handed to `MATCH` unquoted.
#[test]
fn punctuation_in_a_query_is_not_a_syntax_error() {
    let f = Fixture::new();
    f.ingest("inbox", 1, &email("a", "a@example.com", "Build", "we ship c++ and rust"));
    for query in ["c++", "(c++)", "rust -", "ship AND", "ship\" OR rust"] {
        assert!(
            search(&f.store, "acct", query, None, 20).is_ok(),
            "query {query:?} must not fail"
        );
    }
    // A query with nothing indexable in it is refused rather than run.
    assert!(search(&f.store, "acct", "  ??  ", None, 20).is_err());
    assert!(search(&f.store, "acct", "\"", None, 20).is_err());
    assert!(fts_expression("??").is_err());
}

/// Rank puts the subject hit above the body hit, which is the whole reason the
/// bm25 weights are not uniform.
#[test]
fn a_subject_hit_outranks_a_body_hit() {
    let f = Fixture::new();
    let buried = f.ingest(
        "inbox",
        1,
        &email("a", "a@example.com", "Lunch", "somewhere in the thread: invoice"),
    );
    let titled = f.ingest("inbox", 2, &email("b", "b@example.com", "Invoice 42", "attached"));
    assert_eq!(f.hits("invoice"), vec![titled, buried]);
}

/// The search covers every mailbox at once, and `--mailbox` narrows it.
#[test]
fn search_spans_mailboxes_and_can_be_scoped_to_one() {
    let f = Fixture::new();
    let inbox = f.ingest("inbox", 1, &email("a", "a@example.com", "Contract", "signed"));
    let archive = f.ingest("archive", 1, &email("b", "b@example.com", "Contract", "signed"));
    let mut all = f.hits("contract");
    all.sort_unstable();
    assert_eq!(all, vec![inbox, archive].tap_sorted());
    assert_eq!(f.hits_in("contract", "archive"), vec![archive]);
    assert_eq!(f.hits_in("contract", "inbox"), vec![inbox]);
}

trait TapSorted {
    fn tap_sorted(self) -> Vec<i64>;
}
impl TapSorted for Vec<i64> {
    fn tap_sorted(mut self) -> Vec<i64> {
        self.sort_unstable();
        self
    }
}

/// Another account's rows are invisible, even though one file holds one
/// account today: the query is account-scoped like every other store query.
#[test]
fn hits_are_scoped_to_the_account() {
    let f = Fixture::new();
    let mine = f.ingest("inbox", 1, &email("a", "a@example.com", "Ledger", "mine"));
    ingest_message(
        &f.store,
        &f.blobs,
        &IngestInput {
            account: "other",
            mailbox: "inbox",
            uid: 1,
            email: &email("b", "b@example.com", "Ledger", "theirs"),
            raw: None,
        },
    )
    .unwrap();
    assert_eq!(f.hits("ledger"), vec![mine]);
}

/// Re-ingesting the same UID replaces the indexed text instead of adding a
/// second entry for the row (the #0037 double-index bug, guarded from the
/// query side this time).
#[test]
fn reingesting_a_uid_replaces_what_is_indexed() {
    let f = Fixture::new();
    let row = f.ingest("inbox", 1, &email("a", "a@example.com", "First", "original wording"));
    assert_eq!(f.hits("original"), vec![row]);

    let same = f.ingest("inbox", 1, &email("a", "a@example.com", "Second", "corrected wording"));
    assert_eq!(same, row, "the UPSERT must keep the row id");
    assert!(f.hits("original").is_empty(), "stale text still matches");
    assert_eq!(f.hits("corrected"), vec![row]);
    assert_eq!(f.hits("wording"), vec![row], "one entry, not two");
    assert_eq!(index_drift(&f.store).unwrap(), (0, 0));
}

/// A UIDVALIDITY reset rebinds the row to a new UID in place; the index goes
/// with it and does not fork into two entries.
#[test]
fn a_uidvalidity_rebind_keeps_one_indexed_entry() {
    let f = Fixture::new();
    let row = f.ingest("inbox", 7, &email("a", "a@example.com", "Renumbered", "same content"));
    let rebound = f.ingest("inbox", 9001, &email("a", "a@example.com", "Renumbered", "same content"));
    assert_eq!(rebound, row);
    assert_eq!(f.hits("renumbered"), vec![row]);
    assert_eq!(index_drift(&f.store).unwrap(), (0, 0));
}

/// A pruned message stops being findable, in the same transaction that removed
/// its row.
#[test]
fn a_pruned_message_leaves_the_index() {
    let f = Fixture::new();
    let stays = f.ingest("inbox", 1, &email("a", "a@example.com", "Keep", "shared word"));
    let goes = f.ingest("inbox", 2, &email("b", "b@example.com", "Drop", "shared word"));
    assert_eq!(f.hits("shared").len(), 2);

    let pruned = prune_vanished(&f.store, &f.blobs, "acct", "inbox", &[2i64]);
    assert_eq!(pruned, 1);
    assert_eq!(f.hits("shared"), vec![stays]);
    assert!(f.hits("drop").is_empty());
    assert_ne!(stays, goes);
    assert_eq!(index_drift(&f.store).unwrap(), (0, 0));
}

/// The same for an explicit delete (`mp delete`, the TUI's `d`).
#[test]
fn a_deleted_row_leaves_the_index() {
    let f = Fixture::new();
    let row = f.ingest("inbox", 1, &email("a", "a@example.com", "Doomed", "delete me"));
    mailypoppins::store::write::delete_row(&f.store, &f.blobs, row).unwrap();
    assert!(f.hits("doomed").is_empty());
    assert_eq!(index_drift(&f.store).unwrap(), (0, 0));
}

/// A move changes the mailbox of a row without touching its text, so the hit
/// follows the message to its new scope rather than disappearing.
#[test]
fn a_moved_row_is_found_under_its_new_mailbox() {
    let f = Fixture::new();
    let row = f.ingest("inbox", 1, &email("a", "a@example.com", "Filed", "keep this one"));
    mailypoppins::store::write::move_row(&f.store, row, "archive").unwrap();
    assert!(f.hits_in("filed", "inbox").is_empty());
    assert_eq!(f.hits_in("filed", "archive"), vec![row]);
    assert_eq!(index_drift(&f.store).unwrap(), (0, 0));
}

/// `limit` is honoured, and it takes the best-ranked hits rather than an
/// arbitrary slice.
#[test]
fn the_limit_caps_the_ranked_hits() {
    let f = Fixture::new();
    for uid in 1..=5 {
        f.ingest(
            "inbox",
            uid,
            &email(&format!("m{uid}"), "a@example.com", "Body hit", "common term here"),
        );
    }
    let best = f.ingest("inbox", 6, &email("m6", "a@example.com", "Common", "subject hit"));
    let hits = search(&f.store, "acct", "common", None, 2).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].row.id, best);
}
