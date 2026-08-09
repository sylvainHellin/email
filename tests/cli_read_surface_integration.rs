//! Contract for `mp show` and `mp list-messages`, the CLI read surface (#0062).
//!
//! The rendering is unit-tested in `src/read_cmd.rs`; what this pins is the
//! wiring the unit tests cannot see -- that the commands resolve a selector
//! through the same grammar every other received command takes, open the store
//! of the account the *selector* names rather than the default one (the #0073
//! follow-up rule), read the store and nothing else, and degrade instead of
//! erroring when the store cannot answer.
//!
//! `HOME`, `MAILYPOPPINS_CONFIG_DIR` and `MAILYPOPPINS_DATA_DIR` point at a
//! temporary tree, so nothing here reads the real mailstore or config, and no
//! path in either command opens a socket.

use std::fs;
use std::path::Path;
use std::process::Command;

use mailypoppins::ingest::{ingest_message, IngestInput};
use mailypoppins::parse::{AttachmentData, FetchedEmail};
use mailypoppins::store::{BlobStore, Store};
use tempfile::TempDir;

const MP: &str = env!("CARGO_BIN_EXE_mp");

fn email(from: &str, subject: &str, date: &str, body: &str) -> FetchedEmail {
    FetchedEmail {
        from: from.to_string(),
        to: "sylvain@example.com".to_string(),
        cc: None,
        subject: subject.to_string(),
        date: date.to_string(),
        body_text: body.to_string(),
        html_body: None,
        has_attachments: false,
        message_id: Some(format!("<{subject}@example.com>")),
        attachments: Vec::new(),
        flags: Default::default(),
        calendar_ics: None,
        event: None,
    }
}

fn ingest(data: &Path, account: &str, mailbox: &str, uid: i64, message: &FetchedEmail) {
    let account_dir = data.join("accounts").join(account);
    fs::create_dir_all(&account_dir).expect("account dir");
    let store = Store::open(account_dir.join("store.sqlite3")).expect("store");
    let blobs = BlobStore::new(account_dir.join("blobs"));
    ingest_message(
        &store,
        &blobs,
        &IngestInput {
            account,
            mailbox,
            uid,
            email: message,
            raw: None,
        },
    )
    .expect("ingest");
}

/// Two accounts, so the account-from-selector rule has something to get wrong.
fn fixture_tree() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let data = tmp.path().join("data");

    let config = r#"
[[accounts]]
name = "alpha"
default_from = "alpha@example.com"

[accounts.mailboxes.inbox]
server = "INBOX"

[[accounts]]
name = "beta"
default_from = "beta@example.com"
"#;
    let config_path = tmp.path().join("config/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).expect("config dir");
    fs::write(&config_path, config).expect("config");

    let mut reported = email(
        "Ivana <ivana@example.com>",
        "Bericht",
        "Thu, 2 Jul 2026 13:57:30 +0200",
        "the body of the report\n",
    );
    reported.cc = Some("petzold@example.com".to_string());
    reported.flags = mailypoppins::types::MessageFlags::seen(true);
    reported.has_attachments = true;
    reported.attachments = vec![AttachmentData {
        filename: "notes.pdf".to_string(),
        content: b"%PDF-1.4 notes".to_vec(),
        content_id: None,
    }];
    ingest(&data, "alpha", "inbox", 1, &reported);

    ingest(
        &data,
        "alpha",
        "inbox",
        2,
        &email("bot@example.com", "Kickoff", "Wed, 1 Jul 2026 09:00:00 +0200", "kickoff\n"),
    );
    ingest(
        &data,
        "alpha",
        "sent",
        1,
        &email("alpha@example.com", "Re-Bericht", "Mon, 29 Jun 2026 12:00:00 +0000", "sent\n"),
    );

    // The other account's message: only a selector that names `beta` may reach
    // it, and it must not appear in any of alpha's listings.
    ingest(
        &data,
        "beta",
        "inbox",
        1,
        &email("someone@example.com", "Only-In-Beta", "Fri, 1 May 2026 05:00:00 +0000", "beta body\n"),
    );

    tmp
}

/// Run `mp` against the fixture tree; returns `(status ok, stdout, stderr)`.
fn run(tmp: &TempDir, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(MP)
        .args(args)
        .env("HOME", tmp.path())
        .env("MAILYPOPPINS_CONFIG_DIR", tmp.path().join("config"))
        .env("MAILYPOPPINS_DATA_DIR", tmp.path().join("data"))
        .env("NO_COLOR", "1")
        .output()
        .expect("mp must run");
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout is UTF-8"),
        String::from_utf8(out.stderr).expect("stderr is UTF-8"),
    )
}

/// `mp show` prints the stored body, the headers around it and the canonical
/// selector, from a bare Message-ID key.
#[test]
fn show_prints_one_message_from_a_bare_key() {
    let tmp = fixture_tree();
    let (ok, stdout, stderr) = run(&tmp, &["show", "Bericht@example.com"]);
    assert!(ok, "mp show failed: {stderr}");

    assert!(stdout.contains("From: Ivana <ivana@example.com>"), "{stdout}");
    assert!(stdout.contains("Cc: petzold@example.com"), "{stdout}");
    assert!(stdout.contains("Subject: Bericht"), "{stdout}");
    assert!(stdout.contains("Selector: mp://alpha/inbox/Bericht@example.com"), "{stdout}");
    assert!(stdout.contains("Flags: read"), "{stdout}");
    assert!(stdout.contains("notes.pdf (14 B)"), "{stdout}");
    assert!(stdout.trim_end().ends_with("the body of the report"), "{stdout}");
}

/// `--json` is the parseable half, and the one output nothing can misread: the
/// body is a JSON string, so a body of `---` cannot be mistaken for a fence.
#[test]
fn show_json_carries_the_envelope_and_the_body() {
    let tmp = fixture_tree();
    let (ok, stdout, stderr) = run(&tmp, &["show", "--json", "inbox/Bericht@example.com"]);
    assert!(ok, "mp show --json failed: {stderr}");

    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(value["selector"], "mp://alpha/inbox/Bericht@example.com");
    assert_eq!(value["account"], "alpha");
    assert_eq!(value["mailbox"], "inbox");
    assert_eq!(value["subject"], "Bericht");
    assert_eq!(value["flags"][0], "read");
    assert_eq!(value["attachments"][0]["name"], "notes.pdf");
    assert_eq!(value["body"], "the body of the report\n");
}

/// The #0073 follow-up rule: the account named in the selector wins over the
/// default, so a cross-account selector opens the right store instead of
/// reporting a phantom miss against the first configured account.
#[test]
fn a_selector_naming_another_account_reads_that_accounts_store() {
    let tmp = fixture_tree();
    let (ok, stdout, stderr) = run(&tmp, &["show", "mp://beta/inbox/Only-In-Beta@example.com"]);
    assert!(ok, "mp show across accounts failed: {stderr}");
    assert!(stdout.contains("Selector: mp://beta/inbox/Only-In-Beta@example.com"), "{stdout}");
    assert!(stdout.trim_end().ends_with("beta body"), "{stdout}");
}

/// A selector that resolves to nothing is a named miss, not a backtrace.
#[test]
fn a_missing_message_is_a_clear_error() {
    let tmp = fixture_tree();
    let (ok, _, stderr) = run(&tmp, &["show", "nothing-here@example.com"]);
    assert!(!ok, "a miss must exit non-zero");
    assert!(stderr.contains("nothing-here@example.com"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// `mp list-messages -A alpha --mailbox inbox` lists that mailbox and nothing
/// else, in the store's own order, which is the order the TUI list shows.
#[test]
fn list_messages_lists_one_mailbox_of_one_account() {
    let tmp = fixture_tree();
    let (ok, stdout, stderr) = run(&tmp, &["list-messages", "-A", "alpha", "--mailbox", "inbox"]);
    assert!(ok, "mp list-messages failed: {stderr}");

    assert!(stdout.contains("Inbox (2 of 2):"), "{stdout}");
    assert!(stdout.contains("mp://alpha/inbox/Bericht@example.com"), "{stdout}");
    assert!(stdout.contains("mp://alpha/inbox/Kickoff@example.com"), "{stdout}");
    assert!(!stdout.contains("Re-Bericht"), "the sent mailbox is not listed: {stdout}");
    assert!(!stdout.contains("Only-In-Beta"), "another account is never listed: {stdout}");
    assert!(stdout.contains("Shown: 2 | In the store: 2"), "{stdout}");

    // The order is the store's, which `read::list_mailbox` and the TUI share.
    let store = Store::open(
        tmp.path().join("data/accounts/alpha/store.sqlite3"),
    )
    .expect("store");
    let rows = mailypoppins::store::read::list_mailbox(&store, "alpha", "inbox").expect("rows");
    let printed: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.split("mp://alpha/inbox/").nth(1))
        .filter_map(|rest| rest.split(' ').next())
        .collect();
    let expected: Vec<String> = rows
        .iter()
        .map(|row| row.message_id.trim_matches(['<', '>']).to_string())
        .collect();
    assert_eq!(printed, expected, "the listing follows the store's order");
}

/// Without `--mailbox` every mailbox of the account is listed, grouped, and the
/// limit applies per mailbox so a busy inbox cannot hide the others.
#[test]
fn list_messages_groups_every_mailbox_and_limits_per_group() {
    let tmp = fixture_tree();
    let (ok, stdout, stderr) = run(&tmp, &["list-messages", "-n", "1"]);
    assert!(ok, "mp list-messages failed: {stderr}");

    assert!(stdout.contains("Inbox (1 of 2):"), "{stdout}");
    assert!(stdout.contains("Sent (1 of 1):"), "the limit is per mailbox: {stdout}");
    assert!(stdout.contains("Shown: 2 | In the store: 3"), "{stdout}");
}

/// An unknown mailbox names what it could have been rather than printing an
/// empty listing that reads like an empty mailbox.
#[test]
fn an_unknown_mailbox_is_an_error_that_names_the_known_ones() {
    let tmp = fixture_tree();
    let (ok, _, stderr) = run(&tmp, &["list-messages", "--mailbox", "nope"]);
    assert!(!ok);
    assert!(stderr.contains("not a mailbox of alpha"), "{stderr}");
    assert!(stderr.contains("inbox"), "{stderr}");
}

/// A mailbox the store holds no rows for is an empty listing, not an error:
/// the account has a store, this mailbox has simply never received anything.
#[test]
fn an_empty_mailbox_is_not_an_error() {
    let tmp = fixture_tree();
    let (ok, _, stderr) = run(&tmp, &["list-messages", "-A", "beta", "--mailbox", "sent"]);
    assert!(ok, "an empty mailbox is not an error: {stderr}");
}
