//! The `mp://` selector contract, end to end through the binary (#0050).
//!
//! This file used to pin the opposite property: every path-taking command
//! declined, because `.md` messages were gone (#0038) and nothing could name a
//! message yet. The selector contract is what replaced that stop-gate, so the
//! tests are now positive, with one decline kept: a filesystem path where a
//! selector is expected is still refused, and refused with the reason rather
//! than searched.
//!
//! The binary is run rather than the library called, because the contract is
//! the CLI surface: exit codes, the selectors printed on stdout and the errors
//! printed on stderr. `HOME` and `MAILYPOPPINS_DATA_DIR` point into a temp
//! tree, so nothing here reads the real mailstore or the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use email::ingest::{ingest_message, IngestInput};
use email::parse::FetchedEmail;
use email::selector::{self, Namespace};
use email::store::{BlobStore, Store};
use tempfile::TempDir;

const MP: &str = env!("CARGO_BIN_EXE_mp");
const ACCOUNT: &str = "work";

/// A temp `HOME` plus data directory with one configured account, which is all
/// any command here needs: no password, no server, no network.
struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let config_dir = tmp.path().join("home/.config/email");
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::write(
            config_dir.join("config.toml"),
            format!("[[accounts]]\nname = \"{ACCOUNT}\"\ndefault_from = \"me@example.com\"\n"),
        )
        .expect("write config");
        Self { tmp }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn data(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    fn drafts_dir(&self) -> PathBuf {
        self.data().join("accounts").join(ACCOUNT).join("drafts")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(MP)
            .args(args)
            .env("HOME", self.home())
            .env("MAILYPOPPINS_DATA_DIR", self.data())
            .output()
            .expect("mp must run")
    }

    /// stdout of a command that must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "mp {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// stderr of a command that must fail.
    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "mp {args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// Ingest one message into `mailbox` of the account's store, the way a
    /// sync does. Paths are built by hand rather than through `config`,
    /// because the test process must not mutate its own environment: the
    /// integration tests of one binary share it.
    fn ingest(&self, mailbox: &str, uid: i64, message_id: &str, subject: &str) {
        let account_dir = self.data().join("accounts").join(ACCOUNT);
        fs::create_dir_all(&account_dir).expect("account dir");
        let store = Store::open(account_dir.join("store.sqlite3")).expect("store");
        let blobs = BlobStore::new(account_dir.join("blobs"));
        let email = FetchedEmail {
            from: "Sender <sender@example.com>".to_string(),
            to: "me@example.com".to_string(),
            cc: None,
            subject: subject.to_string(),
            date: "Mon, 01 Jan 2026 09:00:00 +0000".to_string(),
            body_text: format!("body of {subject}"),
            html_body: None,
            has_attachments: false,
            message_id: Some(format!("<{message_id}>")),
            attachments: Vec::new(),
            is_read: false,
            calendar_ics: None,
            event: None,
        };
        ingest_message(
            &store,
            &blobs,
            &IngestInput {
                account: ACCOUNT,
                mailbox,
                uid,
                email: &email,
                raw: None,
            },
        )
        .expect("ingest");
    }
}

/// Write a draft the way an agent does: straight into `drafts/`, with no id
/// and without telling the application.
fn external_draft(dir: &Path, name: &str, subject: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("drafts dir");
    let path = dir.join(name);
    fs::write(
        &path,
        format!("---\nto: a@example.com\nsubject: {subject}\nstatus: draft\n---\n\nWritten by an agent\n"),
    )
    .expect("write draft");
    path
}

/// Every `mp://` selector printed on stdout, in order.
fn selectors_in(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|word| word.starts_with(selector::SCHEME))
        .map(|word| word.trim_end_matches(&[',', '.'][..]).to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The decline that stays
// ---------------------------------------------------------------------------

/// No command accepts a filesystem path where a selector is expected, and the
/// refusal names the mistake instead of reporting "no match for ./x.md".
///
/// This is the one property carried over from the file-decline era, and it is
/// the reason `mp path` exists: there is exactly one direction between the two
/// namings, and it is selector to path.
#[test]
fn a_path_is_refused_where_a_selector_is_expected() {
    let fx = Fixture::new();
    let path = external_draft(&fx.drafts_dir(), "2026-07-01-note.md", "Note");
    let path = path.to_string_lossy().into_owned();

    for args in [
        vec!["send", path.as_str()],
        vec!["mark-approved", path.as_str()],
        vec!["mark-draft", path.as_str()],
        vec!["validate", path.as_str()],
        vec!["path", path.as_str()],
        vec!["edit", path.as_str()],
        vec!["archive", path.as_str()],
        vec!["delete", path.as_str()],
        vec!["open", path.as_str()],
        vec!["save", path.as_str()],
        vec!["reply", path.as_str()],
        vec!["forward", path.as_str()],
        vec!["invite", "accept", path.as_str()],
        // A relative path, and one that does not exist at all.
        vec!["archive", "./inbox/message.md"],
        vec!["send", "drafts/gone.md"],
    ] {
        let stderr = fx.err(&args);
        assert!(
            stderr.contains("looks like a filesystem path"),
            "mp {args:?} must name the mistake: {stderr}"
        );
        assert!(
            !stderr.contains("No such file or directory"),
            "mp {args:?} must not surface a raw I/O error: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Printed selectors round-trip
// ---------------------------------------------------------------------------

/// Every selector any command prints parses back, and `mp path` turns one into
/// a path that exists. Those are two of the ticket's acceptance criteria and
/// they are the whole reason the printed form is the fully qualified one.
#[test]
fn printed_selectors_round_trip_and_mp_path_lands_on_a_real_file() {
    let fx = Fixture::new();
    let printed = fx.ok(&["new", "quarterly-update"]);
    let created = selectors_in(&printed);
    assert_eq!(created.len(), 1, "mp new prints one selector: {printed}");
    let selector = &created[0];

    // It parses, and it parses into the account and mailbox it claims.
    let query = selector::parse_in(selector, Namespace::Drafts, ACCOUNT, None)
        .expect("a printed selector parses");
    assert_eq!(query.account, ACCOUNT);
    assert_eq!(query.mailbox.as_deref(), Some(selector::DRAFTS_MAILBOX));

    // `mp path` is the only edge back to the filesystem, and it lands on the
    // file `mp new` wrote.
    let path = PathBuf::from(fx.ok(&["path", selector]).trim());
    assert!(path.exists(), "{} must exist", path.display());
    assert_eq!(path.parent(), Some(fx.drafts_dir().as_path()));

    // The listing prints the same selector, and it parses too.
    let listed = selectors_in(&fx.ok(&["list"]));
    assert_eq!(listed, vec![selector.clone()]);
    for listed in &listed {
        selector::parse(listed).expect("a listed selector parses");
    }

    // The elided forms name the same draft as the canonical one.
    let key = query.key;
    assert_eq!(fx.ok(&["path", &key]).trim(), path.to_string_lossy());
    assert_eq!(
        fx.ok(&["path", &format!("drafts/{key}")]).trim(),
        path.to_string_lossy()
    );
}

/// Renaming a draft file keeps its selector working, because identity is the
/// `id:` field and not the filename. This is the property that makes it safe
/// for an agent to reorganise `drafts/`.
#[test]
fn renaming_a_draft_keeps_its_selector_working() {
    let fx = Fixture::new();
    external_draft(&fx.drafts_dir(), "before.md", "Renamed later");
    let selector = selectors_in(&fx.ok(&["list"]))
        .pop()
        .expect("the agent's draft is listed");

    let before = PathBuf::from(fx.ok(&["path", &selector]).trim());
    let after = fx.drafts_dir().join("completely-different-name.md");
    fs::rename(&before, &after).expect("rename");

    assert_eq!(fx.ok(&["path", &selector]).trim(), after.to_string_lossy());
    assert!(
        fx.ok(&["list"]).contains(&selector),
        "the renamed draft keeps its place in the listing"
    );
}

// ---------------------------------------------------------------------------
// TKT-0045: freshness
// ---------------------------------------------------------------------------

/// A draft created by another process appears in `mp list` within one second
/// and without a restart, which is the [TKT-0045] scenario and this ticket's
/// acceptance test for it.
///
/// Two halves, because the product has two readers. `mp list` is a fresh
/// process that refreshes the index at engine start, so its bound is its own
/// startup. The TUI is a long-running process, and what it polls is
/// [`email::store::drafts::fingerprint`]; the loop below is that poll, run at
/// the same one-second budget the event loop gives it.
#[test]
fn an_externally_written_draft_shows_up_within_one_second() {
    let fx = Fixture::new();
    external_draft(&fx.drafts_dir(), "first.md", "First");
    assert_eq!(selectors_in(&fx.ok(&["list"])).len(), 1);

    let before = email::store::drafts::fingerprint(&fx.drafts_dir());
    let started = Instant::now();
    external_draft(&fx.drafts_dir(), "second.md", "Second");

    // The long-running reader: the stat-only poll notices, inside its budget.
    let mut noticed = None;
    while started.elapsed() < Duration::from_secs(1) {
        let now = email::store::drafts::fingerprint(&fx.drafts_dir());
        if now != before {
            noticed = Some(started.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let noticed = noticed.expect("the poll must see the new draft within one second");
    assert!(noticed < Duration::from_secs(1));

    // The fresh reader: it is listed, with its subject, and the first draft is
    // still there.
    let listing = fx.ok(&["list"]);
    assert_eq!(selectors_in(&listing).len(), 2, "{listing}");
    assert!(listing.contains("Second"), "{listing}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the whole scenario must fit in the one-second budget"
    );
}

// ---------------------------------------------------------------------------
// Ambiguity
// ---------------------------------------------------------------------------

/// The same message in two mailboxes, the cross-mailbox copy, is never
/// resolved by a rule the user cannot see: the error lists both fully
/// qualified selectors and `--mailbox` picks one.
#[test]
fn an_ambiguous_key_lists_both_selectors_and_mailbox_resolves_it() {
    let fx = Fixture::new();
    let key = "shared@example.com";
    fx.ingest("inbox", 1, key, "Copied everywhere");
    fx.ingest("archive", 1, key, "Copied everywhere");

    let stderr = fx.err(&["save", key]);
    assert!(
        stderr.contains("--mailbox"),
        "the ambiguity must name the flag that resolves it: {stderr}"
    );
    for mailbox in ["inbox", "archive"] {
        let candidate = format!("mp://{ACCOUNT}/{mailbox}/{}", selector::encode(key));
        assert!(
            stderr.contains(&candidate),
            "the ambiguity must list {candidate}: {stderr}"
        );
    }

    // Named, it resolves: the failure is now about the message itself (it has
    // no attachments to save), and it names the one selector it settled on.
    let stderr = fx.err(&["save", "--mailbox", "inbox", key]);
    assert!(
        stderr.contains("has no attachments"),
        "--mailbox must resolve the ambiguity: {stderr}"
    );
    assert!(
        stderr.contains(&format!("mp://{ACCOUNT}/inbox/{}", selector::encode(key))),
        "the resolved selector is reported in full: {stderr}"
    );
    // The selector's own mailbox segment does the same job.
    let stderr = fx.err(&["save", &format!("archive/{key}")]);
    assert!(stderr.contains("has no attachments"), "{stderr}");
    assert!(
        stderr.contains(&format!("mp://{ACCOUNT}/archive/{}", selector::encode(key))),
        "{stderr}"
    );
}

/// A key that matches nothing names the namespace it searched, so "no such
/// message" is distinguishable from "you asked the wrong index".
#[test]
fn an_unknown_key_names_the_namespace_it_searched() {
    let fx = Fixture::new();
    fx.ingest("inbox", 1, "known@example.com", "Known");

    let stderr = fx.err(&["save", "unknown@example.com"]);
    assert!(stderr.contains("received mail"), "{stderr}");

    let stderr = fx.err(&["send", "0123456789abcdef"]);
    assert!(stderr.contains("drafts"), "{stderr}");
}
