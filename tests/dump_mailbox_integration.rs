//! Contract for `mp dump-mailbox --json`, the envelope oracle (#0049 unit 0c,
//! flipped onto the store by #0038 unit A).
//!
//! parity, all of it. The dump is the only oracle that survives the nuke of
//! the `.md` stack: the records below were recorded from the file build and
//! the store build must emit the same ones. The expected NDJSON is written out
//! in full rather than snapshotted, so the record shape, the field order, the
//! null handling and the sort key are readable in one place and a change to
//! any of them is a deliberate edit.
//!
//! Every intended difference from the pre-nuke output is one line in
//! [docs/dump-allow-list.md](../docs/dump-allow-list.md); the ones this
//! fixture exercises are called out at the message that exercises them.
//!
//! The fixture is written through the real ingest API rather than by inserting
//! rows, so what is dumped is what a sync produces. The binary is then run
//! rather than the library called, because the CLI surface (`--json` required,
//! `-A` selecting one account, `--mailbox` filtering) is part of the contract.
//! `HOME` and `MAILYPOPPINS_DATA_DIR` point at a temporary tree, so the test
//! never reads the real mailstore and never touches the network.

use std::fs;
use std::path::Path;
use std::process::Command;

use email::ingest::{ingest_message, IngestInput};
use email::parse::{AttachmentData, FetchedEmail};
use email::store::{BlobStore, Store};
use tempfile::TempDir;

const MP: &str = env!("CARGO_BIN_EXE_mp");

fn email(from: &str, to: &str, subject: &str, date: &str) -> FetchedEmail {
    FetchedEmail {
        from: from.to_string(),
        to: to.to_string(),
        cc: None,
        subject: subject.to_string(),
        date: date.to_string(),
        body_text: format!("Body of {subject}.\n"),
        html_body: None,
        has_attachments: false,
        message_id: None,
        attachments: Vec::new(),
        flags: Default::default(),
        calendar_ics: None,
        event: None,
    }
}

/// Ingest one message into `account`'s store, as a sync would.
///
/// The store and blob paths are built from `data` explicitly rather than
/// through `config::store_path`, which reads `MAILYPOPPINS_DATA_DIR`: these
/// tests run in one process and each has its own temp tree, so a process-wide
/// env var would let two fixtures write into each other.
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

/// Lay down a config with two accounts and an ingested store for each, and
/// return the temp dir holding both.
fn fixture_tree() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();
    let data = home.join("data");

    let config = r#"
[[accounts]]
name = "alpha"
default_from = "alpha@example.com"

[accounts.mailboxes.inbox]
server = "INBOX"

[[accounts.mailboxes.extra]]
server = "Team/Reports"

[[accounts]]
name = "beta"
default_from = "beta@example.com"

[beta.mailboxes]
"#;
    write(&home.join(".config/email/config.toml"), config);

    // Read, with cc, a unicode subject, two attachments and a message-id.
    // Allow-list: both attachment sizes are present, because an attachment in
    // the store is a blob. The file build recorded `null` for an attachment
    // named in frontmatter whose file was not on disk.
    let mut bericht = email(
        "Ivana Hecimovic <ivana@example.com>",
        "Sylvain Hellin <sylvain@example.com>",
        "Bericht über Anträge",
        "Thu, 2 Jul 2026 13:57:30 +0200",
    );
    bericht.cc = Some("\"Prof. Petzold\" <petzold@example.com>".to_string());
    bericht.message_id = Some("<f7ef260c@example.com>".to_string());
    bericht.flags = email::types::MessageFlags::seen(true);
    bericht.has_attachments = true;
    bericht.attachments = vec![
        AttachmentData {
            filename: "notes.pdf".to_string(),
            content: b"%PDF-1.4 notes".to_vec(),
            content_id: None,
        },
        AttachmentData {
            filename: "agenda.txt".to_string(),
            content: b"abc".to_vec(),
            content_id: None,
        },
    ];
    ingest(&data, "alpha", "inbox", 1, &bericht);

    // Unread invite with no Message-ID header.
    // Allow-list: ingest synthesizes an id, where the file build recorded
    // `null`. The iMIP payload sets `invite` and is not an attachment.
    let mut kickoff = email(
        "Organizer <organizer@example.com>",
        "sylvain@example.com",
        "Kickoff",
        "Wed, 1 Jul 2026 09:00:00 +0200",
    );
    kickoff.calendar_ics = Some(
        b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:evt-1\r\nSUMMARY:Kickoff\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            .to_vec(),
    );
    ingest(&data, "alpha", "inbox", 2, &kickoff);

    // Two messages in the same second: the sort key falls through to
    // message_id (both synthetic here), then subject, then uid. Two rows that
    // agree all the way down to uid cannot be built through ingest -- the same
    // envelope in the same mailbox is one row, re-bound to the new uid -- so
    // uid is the tiebreaker that makes the order total, not one a fixture can
    // exercise.
    ingest(
        &data,
        "alpha",
        "inbox",
        4,
        &email("b@example.com", "sylvain@example.com", "Same second", "Sat, 4 Jul 2026 10:00:00 +0000"),
    );
    ingest(
        &data,
        "alpha",
        "inbox",
        3,
        &email("a@example.com", "sylvain@example.com", "Same second", "Sat, 4 Jul 2026 10:00:00 +0000"),
    );

    // No usable `Date:` header at all.
    // Allow-list: `date_sort` is empty. The file build fell back to the date
    // encoded in the file name, which no longer exists.
    ingest(
        &data,
        "alpha",
        "inbox",
        5,
        &email("undated@example.com", "sylvain@example.com", "No date header", "(unknown date)"),
    );

    // Sent, with an attachment. The file build stored the *source* path of an
    // outgoing attachment (`/tmp/outgoing/report.pdf`) and reduced it to a
    // file name; the store holds the blob and its name.
    let mut reply = email(
        "sylvain@example.com",
        "ivana@example.com",
        "Re: Bericht",
        "Mon, 29 Jun 2026 12:00:00 +0000",
    );
    reply.message_id = Some("<sent-1@example.com>".to_string());
    reply.has_attachments = true;
    reply.attachments = vec![AttachmentData {
        filename: "report.pdf".to_string(),
        content: b"%PDF-1.4".to_vec(),
        content_id: None,
    }];
    ingest(&data, "alpha", "sent", 1, &reply);

    // An `extra` mailbox: the server name is the mailbox key, verbatim, which
    // is what the sync path hands to ingest (#0064).
    let mut weekly = email(
        "bot@example.com",
        "sylvain@example.com",
        "Weekly",
        "Sun, 28 Jun 2026 07:00:00 +0000",
    );
    weekly.message_id = Some("<weekly-1@example.com>".to_string());
    weekly.flags = email::types::MessageFlags::seen(true);
    ingest(&data, "alpha", "Team/Reports", 1, &weekly);

    // Second account, so account ordering is exercised.
    let mut old = email(
        "someone@example.com",
        "beta@example.com",
        "Old thread",
        "Fri, 1 May 2026 05:00:00 +0000",
    );
    old.message_id = Some("<old-1@example.com>".to_string());
    old.flags = email::types::MessageFlags::seen(true);
    ingest(&data, "beta", "archive", 1, &old);

    // Drafts contribute nothing: they have no `messages` rows until #0050
    // indexes them. The file build dumped two draft records here; that is the
    // documented stop-gate state, not a lost record.

    tmp
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    fs::write(path, content).expect("write fixture");
}

/// Run `mp` against the fixture tree and return stdout. Only stdout is the
/// contract: stderr carries config/secrets warnings that depend on the
/// environment.
fn dump(tmp: &TempDir, args: &[&str]) -> String {
    let out = Command::new(MP)
        .args(args)
        .env("HOME", tmp.path())
        .env("MAILYPOPPINS_DATA_DIR", tmp.path().join("data"))
        .output()
        .expect("mp must run");
    assert!(out.status.success(), "mp {args:?} failed: {out:?}");
    String::from_utf8(out.stdout).expect("dump is UTF-8")
}

/// The full expected dump: every field, in order, for every fixture message.
///
/// The `sha256-...@local.invalid` id is the synthetic identity ingest gives
/// mail with no `Message-ID:` header (the sha256 prefix of its canonical
/// envelope); it is written out literally so a change to that derivation
/// shows up here as a deliberate edit.
const EXPECTED: &str = concat!(
    r#"{"account":"alpha","mailbox":"Team/Reports","message_id":"<weekly-1@example.com>","from":"bot@example.com","to":"sylvain@example.com","cc":null,"subject":"Weekly","date_sort":"2026-06-28T07:00:00","flags":["seen"],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":"<sha256-9a19496e192edba7@local.invalid>","from":"undated@example.com","to":"sylvain@example.com","cc":null,"subject":"No date header","date_sort":"","flags":[],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":"<sha256-50b9b217cc3bf6f3@local.invalid>","from":"Organizer <organizer@example.com>","to":"sylvain@example.com","cc":null,"subject":"Kickoff","date_sort":"2026-07-01T07:00:00","flags":[],"attachments":[],"invite":true}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":"<f7ef260c@example.com>","from":"Ivana Hecimovic <ivana@example.com>","to":"Sylvain Hellin <sylvain@example.com>","cc":"\"Prof. Petzold\" <petzold@example.com>","subject":"Bericht über Anträge","date_sort":"2026-07-02T11:57:30","flags":["seen"],"attachments":[{"name":"agenda.txt","size":3},{"name":"notes.pdf","size":14}],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":"<sha256-56189be5b91b92d9@local.invalid>","from":"a@example.com","to":"sylvain@example.com","cc":null,"subject":"Same second","date_sort":"2026-07-04T10:00:00","flags":[],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":"<sha256-a8c9281df15da4b3@local.invalid>","from":"b@example.com","to":"sylvain@example.com","cc":null,"subject":"Same second","date_sort":"2026-07-04T10:00:00","flags":[],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"sent","message_id":"<sent-1@example.com>","from":"sylvain@example.com","to":"ivana@example.com","cc":null,"subject":"Re: Bericht","date_sort":"2026-06-29T12:00:00","flags":[],"attachments":[{"name":"report.pdf","size":8}],"invite":false}"#,
    "\n",
    r#"{"account":"beta","mailbox":"archive","message_id":"<old-1@example.com>","from":"someone@example.com","to":"beta@example.com","cc":null,"subject":"Old thread","date_sort":"2026-05-01T05:00:00","flags":["seen"],"attachments":[],"invite":false}"#,
    "\n",
);

/// parity: the exact serialized dump of the whole fixture store. This is the
/// record-shape contract, including verbatim header values, attachment sizes,
/// the invite flag and the absence of any filesystem path.
#[test]
fn dump_mailbox_emits_the_recorded_envelope_shape() {
    let tmp = fixture_tree();
    let out = dump(&tmp, &["dump-mailbox", "--json"]);
    assert_eq!(out, EXPECTED);
    assert!(
        !out.contains(&tmp.path().to_string_lossy().to_string()),
        "dump must not contain filesystem paths"
    );
}

/// parity: two consecutive runs over an unchanged store are byte-identical, so
/// a diff against another build can only mean a behaviour change.
#[test]
fn dump_mailbox_is_deterministic_across_runs() {
    let tmp = fixture_tree();
    let first = dump(&tmp, &["dump-mailbox", "--json"]);
    let second = dump(&tmp, &["dump-mailbox", "--json"]);
    assert_eq!(first, second);
}

/// parity: `-A` restricts the dump to one account, `--mailbox` to the named
/// mailboxes (role, server name or sidebar label, case-insensitive).
#[test]
fn dump_mailbox_honours_account_and_mailbox_selectors() {
    let tmp = fixture_tree();

    let beta = dump(&tmp, &["-A", "beta", "dump-mailbox", "--json"]);
    assert_eq!(beta.lines().count(), 1);
    assert!(beta.contains(r#""account":"beta""#));

    let inbox = dump(&tmp, &["-A", "alpha", "dump-mailbox", "--json", "--mailbox", "INBOX"]);
    assert_eq!(inbox.lines().count(), 5);
    assert!(inbox.lines().all(|l| l.contains(r#""mailbox":"inbox""#)));

    let extra = dump(
        &tmp,
        &["-A", "alpha", "dump-mailbox", "--json", "--mailbox", "Team/Reports"],
    );
    assert_eq!(extra.lines().count(), 1);
    assert!(extra.contains(r#""mailbox":"Team/Reports""#));

    let two = dump(
        &tmp,
        &["-A", "alpha", "dump-mailbox", "--json", "--mailbox", "sent", "--mailbox", "drafts"],
    );
    assert_eq!(two.lines().count(), 1, "drafts have no rows until #0050");
}

/// The second status axis reaches the dump (#TKT-0051), which is where a
/// scripted check reads it: `answered` and `forwarded` sit beside `seen` in
/// the same sorted token set.
///
/// Outside the parity record above, which was captured from a build that had
/// no such axis: the flags are set through the same store API the post-send
/// hook uses, on the fixture the other tests dump unchanged.
#[test]
fn dump_mailbox_reports_the_answered_and_forwarded_flags() {
    let tmp = fixture_tree();
    let account_dir = tmp.path().join("data").join("accounts").join("alpha");
    let store = Store::open(account_dir.join("store.sqlite3")).expect("store");
    let answered = email::store::read::find_by_message_id(&store, "alpha", "<f7ef260c@example.com>")
        .expect("lookup")
        .remove(0);
    email::store::write::set_answered(&store, answered.id).expect("set answered");
    let forwarded = email::store::read::find_by_message_id(&store, "alpha", "<weekly-1@example.com>")
        .expect("lookup")
        .remove(0);
    email::store::write::set_forwarded(&store, forwarded.id).expect("set forwarded");
    drop(store);

    let out = dump(&tmp, &["-A", "alpha", "dump-mailbox", "--json"]);
    let answered_line = out
        .lines()
        .find(|l| l.contains("<f7ef260c@example.com>"))
        .expect("the answered message is dumped");
    assert!(
        answered_line.contains(r#""flags":["answered","seen"]"#),
        "{answered_line}"
    );
    let forwarded_line = out
        .lines()
        .find(|l| l.contains("<weekly-1@example.com>"))
        .expect("the forwarded message is dumped");
    assert!(
        forwarded_line.contains(r#""flags":["forwarded","seen"]"#),
        "{forwarded_line}"
    );
}

/// parity: the output format is pinned by a required flag, so a future default
/// cannot silently change what a dump means.
#[test]
fn dump_mailbox_requires_the_json_flag() {
    let tmp = fixture_tree();
    let out = Command::new(MP)
        .args(["dump-mailbox"])
        .env("HOME", tmp.path())
        .env("MAILYPOPPINS_DATA_DIR", tmp.path().join("data"))
        .output()
        .expect("mp must run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--json"));
}
