//! Contract for `mp dump-mailbox --json`, the pre-nuke envelope oracle
//! (#0049, unit 0c).
//!
//! parity, all of it. The dump is the only oracle left once the file-based
//! stack is nuked: the new SQLite store must be able to emit exactly these
//! records, byte for byte, from the database. The expected NDJSON below is
//! therefore written out in full rather than snapshotted, so the record shape,
//! the field order, the null handling and the sort key are all readable in one
//! place and a change to any of them is a deliberate edit.
//!
//! The binary is run rather than the library called, because the CLI surface
//! (`--json` required, `-A` selecting one account, `--mailbox` filtering) is
//! part of the contract. `HOME` and `MAILYPOPPINS_DATA_DIR` point at a
//! temporary tree, so the test never reads the real mailstore and never
//! touches the network.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const MP: &str = env!("CARGO_BIN_EXE_mp");

/// Lay down a config with two accounts and a fixture mail tree, and return the
/// temp dir holding both (dropped by the caller, which unlinks the tree).
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

    let alpha = data.join("accounts/alpha");
    let beta = data.join("accounts/beta");

    // Read, with cc, a unicode subject, two attachments of which only one is
    // on disk, and a message-id.
    write(
        &alpha.join("inbox/2026-07-02-1357_ivana_bericht.md"),
        "---\n\
         from: Ivana Hecimovic <ivana@example.com>\n\
         to: Sylvain Hellin <sylvain@example.com>\n\
         cc: '\"Prof. Petzold\" <petzold@example.com>'\n\
         subject: 'Bericht über Anträge'\n\
         date: Thu, 2 Jul 2026 13:57:30 +0200\n\
         message_id: <f7ef260c@example.com>\n\
         status: inbox\n\
         has_attachments: true\n\
         attachments:\n\
         - notes.pdf\n\
         - agenda.txt\n\
         read: true\n\
         ---\n\n\
         Body.\n",
    );
    write(
        &alpha.join("inbox/2026-07-02-1357_ivana_bericht_attachments/agenda.txt"),
        "abc",
    );

    // Unread invite with no message-id: the dump records the absence instead
    // of synthesizing an identity.
    write(
        &alpha.join("inbox/2026-07-01-0900_organizer_kickoff.md"),
        "---\n\
         from: Organizer <organizer@example.com>\n\
         to: sylvain@example.com\n\
         subject: Kickoff\n\
         date: Wed, 1 Jul 2026 09:00:00 +0200\n\
         status: inbox\n\
         read: false\n\
         event:\n\
        \x20 uid: evt-1\n\
        \x20 method: REQUEST\n\
        \x20 sequence: 0\n\
        \x20 summary: Kickoff\n\
        \x20 rsvp: needs-action\n\
        \x20 recurrence: ''\n\
         ---\n\n\
         Invite body.\n",
    );

    // Not valid UTF-8: dropped, exactly as `load_emails` drops it.
    fs::write(
        alpha.join("inbox/2026-07-03-1000_broken_bytes.md"),
        [b'-', b'-', b'-', b'\n', 0xff, 0xfe, b'\n'],
    )
    .expect("write broken file");

    // Ties on date_sort and message-id: subject then filename order them.
    write(
        &alpha.join("inbox/2026-07-04-1000_b_same.md"),
        "---\nfrom: b@example.com\nsubject: Same second\ndate: Sat, 4 Jul 2026 10:00:00 +0000\nstatus: inbox\n---\n\nb\n",
    );
    write(
        &alpha.join("inbox/2026-07-04-1000_a_same.md"),
        "---\nfrom: a@example.com\nsubject: Same second\ndate: Sat, 4 Jul 2026 10:00:00 +0000\nstatus: inbox\n---\n\na\n",
    );

    // Drafts: status flags, and no date at all (the filename fallback).
    write(
        &alpha.join("drafts/2026-06-30-0815_reply-to-ivana.md"),
        "---\nto: ivana@example.com\nsubject: 'Re: Bericht'\nstatus: draft\n---\n\nDraft body.\n",
    );
    write(
        &alpha.join("drafts/ready.md"),
        "---\nto: ivana@example.com\nsubject: Ready to go\nstatus: approved\n---\n\nApproved body.\n",
    );

    // Sent: sent_at instead of date, and an attachment recorded as the source
    // path it was sent from (what outgoing mail really stores).
    write(
        &alpha.join("sent/2026-06-29-1200_ivana_re-bericht.md"),
        "---\nfrom: sylvain@example.com\nto: ivana@example.com\nsubject: 'Re: Bericht'\nsent_at: 2026-06-29T12:00:00Z\nstatus: sent\nmessage_id: <sent-1@example.com>\nhas_attachments: true\nattachments:\n- /tmp/outgoing/report.pdf\n---\n\nSent body.\n",
    );

    // An `extra` mailbox: the slug of the server name is the mailbox id.
    write(
        &alpha.join("team-reports/2026-06-28-0700_bot_weekly.md"),
        "---\nfrom: bot@example.com\nto: sylvain@example.com\nsubject: Weekly\ndate: Sun, 28 Jun 2026 07:00:00 +0000\nstatus: inbox\nmessage_id: <weekly-1@example.com>\nread: true\n---\n\nWeekly body.\n",
    );

    // Second account, so account ordering is exercised.
    write(
        &beta.join("archive/2026-05-01-0500_someone_old.md"),
        "---\nfrom: someone@example.com\nto: beta@example.com\nsubject: Old thread\ndate: Fri, 1 May 2026 05:00:00 +0000\nstatus: archived\nmessage_id: <old-1@example.com>\nread: true\n---\n\nOld body.\n",
    );

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
const EXPECTED: &str = concat!(
    r#"{"account":"alpha","mailbox":"drafts","message_id":null,"from":null,"to":"ivana@example.com","cc":null,"subject":"Ready to go","date_sort":"","flags":["approved"],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"drafts","message_id":null,"from":null,"to":"ivana@example.com","cc":null,"subject":"Re: Bericht","date_sort":"2026-06-30T08:15:00","flags":["draft"],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":null,"from":"Organizer <organizer@example.com>","to":"sylvain@example.com","cc":null,"subject":"Kickoff","date_sort":"2026-07-01T07:00:00","flags":[],"attachments":[],"invite":true}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":"<f7ef260c@example.com>","from":"Ivana Hecimovic <ivana@example.com>","to":"Sylvain Hellin <sylvain@example.com>","cc":"\"Prof. Petzold\" <petzold@example.com>","subject":"Bericht über Anträge","date_sort":"2026-07-02T11:57:30","flags":["seen"],"attachments":[{"name":"agenda.txt","size":3},{"name":"notes.pdf","size":null}],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":null,"from":"a@example.com","to":null,"cc":null,"subject":"Same second","date_sort":"2026-07-04T10:00:00","flags":[],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"inbox","message_id":null,"from":"b@example.com","to":null,"cc":null,"subject":"Same second","date_sort":"2026-07-04T10:00:00","flags":[],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"sent","message_id":"<sent-1@example.com>","from":"sylvain@example.com","to":"ivana@example.com","cc":null,"subject":"Re: Bericht","date_sort":"2026-06-29T12:00:00","flags":[],"attachments":[{"name":"report.pdf","size":null}],"invite":false}"#,
    "\n",
    r#"{"account":"alpha","mailbox":"team-reports","message_id":"<weekly-1@example.com>","from":"bot@example.com","to":"sylvain@example.com","cc":null,"subject":"Weekly","date_sort":"2026-06-28T07:00:00","flags":["seen"],"attachments":[],"invite":false}"#,
    "\n",
    r#"{"account":"beta","mailbox":"archive","message_id":"<old-1@example.com>","from":"someone@example.com","to":"beta@example.com","cc":null,"subject":"Old thread","date_sort":"2026-05-01T05:00:00","flags":["seen"],"attachments":[],"invite":false}"#,
    "\n",
);

/// parity: the exact serialized dump of the whole fixture tree. This is the
/// record-shape contract, including null message-ids (never synthesized),
/// verbatim header values, attachment sizes (null when the file is gone), the
/// invite flag and the absence of any filesystem path.
#[test]
fn dump_mailbox_emits_the_recorded_envelope_shape() {
    let tmp = fixture_tree();
    let out = dump(&tmp, &["dump-mailbox", "--json"]);
    assert_eq!(out, EXPECTED);
    assert!(
        !out.contains(&tmp.path().to_string_lossy().to_string()),
        "dump must not contain filesystem paths"
    );
    assert!(!out.contains("/tmp/outgoing"), "attachment source paths must not leak");
}

/// parity: two consecutive runs over an unchanged tree are byte-identical, so
/// a diff against the new build can only mean a behaviour change.
#[test]
fn dump_mailbox_is_deterministic_across_runs() {
    let tmp = fixture_tree();
    let first = dump(&tmp, &["dump-mailbox", "--json"]);
    let second = dump(&tmp, &["dump-mailbox", "--json"]);
    assert_eq!(first, second);
}

/// parity: `-A` restricts the dump to one account, `--mailbox` to the named
/// mailboxes (role, slug or sidebar label, case-insensitive).
#[test]
fn dump_mailbox_honours_account_and_mailbox_selectors() {
    let tmp = fixture_tree();

    let beta = dump(&tmp, &["-A", "beta", "dump-mailbox", "--json"]);
    assert_eq!(beta.lines().count(), 1);
    assert!(beta.contains(r#""account":"beta""#));

    let inbox = dump(&tmp, &["-A", "alpha", "dump-mailbox", "--json", "--mailbox", "INBOX"]);
    assert_eq!(inbox.lines().count(), 4);
    assert!(inbox.lines().all(|l| l.contains(r#""mailbox":"inbox""#)));

    let extra = dump(
        &tmp,
        &["-A", "alpha", "dump-mailbox", "--json", "--mailbox", "Team/Reports"],
    );
    assert_eq!(extra.lines().count(), 1);
    assert!(extra.contains(r#""mailbox":"team-reports""#));

    let two = dump(
        &tmp,
        &["-A", "alpha", "dump-mailbox", "--json", "--mailbox", "sent", "--mailbox", "drafts"],
    );
    assert_eq!(two.lines().count(), 3);
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
