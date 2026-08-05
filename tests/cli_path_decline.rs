//! The `#0050` boundary: every path-taking CLI command declines, non-zero.
//!
//! `.md` messages are gone (#0038), so `mp archive`, `mp delete`, `mp open`,
//! `mp save`, `mp reply` and `mp forward` have nothing to address until the
//! selector contract lands. The failure mode this pins is not the wording, it
//! is the *shape*: none of them may report success, and none may fail with an
//! incidental I/O error that reads like the command worked on something.
//!
//! Before this test, `mp open` and `mp save` called `list_attachments` on the
//! `<stem>_attachments/` tree ingest stopped writing, found nothing, printed
//! "No attachments found" and exited 0 for a message that has attachments;
//! `mp reply` and `mp forward` died inside the draft builder with a bare
//! "No such file or directory". Both are false or misleading successes.
//!
//! The binary is run rather than the library called, because the exit code is
//! half the contract. `HOME` and `MAILYPOPPINS_DATA_DIR` point at a temp tree,
//! so nothing here reads the real mailstore or the network: the declines land
//! before any config-dependent work, which is itself part of the contract.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const MP: &str = env!("CARGO_BIN_EXE_mp");

/// Run `mp <args>` in an empty home and return (exit code, stderr).
fn run(home: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(MP)
        .args(args)
        .env("HOME", home)
        .env("MAILYPOPPINS_DATA_DIR", home.join("data"))
        .output()
        .expect("mp must run");
    (
        out.status.code().expect("mp exited by signal"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A `.md` message with a populated `<stem>_attachments/` sibling, i.e. exactly
/// the layout `list_attachments` used to walk. The declines must not look at
/// it: this is the file whose existence made the old `mp open` exit 0.
fn message_with_attachments(dir: &Path) -> String {
    let path = dir.join("email.md");
    fs::write(
        &path,
        "---\nfrom: \"a@example.com\"\nsubject: \"With attachment\"\nhas_attachments: true\nattachments:\n  - \"report.pdf\"\n---\n\nSee attached\n",
    )
    .expect("write message");
    let atts = dir.join("email_attachments");
    fs::create_dir_all(&atts).expect("attachments dir");
    fs::write(atts.join("report.pdf"), b"%PDF-1.4 report").expect("write attachment");
    path.to_string_lossy().into_owned()
}

/// Every path-taking command declines with the #0050 boundary line and a
/// non-zero exit, whether or not the path it was handed exists.
#[test]
fn path_taking_commands_decline_with_the_selector_boundary() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();
    let existing = message_with_attachments(home);

    let cases: Vec<Vec<&str>> = vec![
        vec!["archive", &existing],
        vec!["delete", &existing],
        vec!["open", &existing],
        vec!["save", &existing],
        vec!["reply", &existing],
        vec!["reply", "--all", &existing],
        vec!["forward", &existing],
        // The interactive forms used to walk the inbox tree to offer a pick.
        vec!["reply"],
        vec!["forward"],
        // A path that never existed declines the same way.
        vec!["open", "does-not-exist.md"],
        vec!["save", "does-not-exist.md"],
    ];

    for args in cases {
        let (code, stderr) = run(home, &args);
        assert_eq!(code, 1, "mp {args:?} must fail: {stderr}");
        assert!(
            stderr.contains("#0050"),
            "mp {args:?} must name the selector contract: {stderr}"
        );
        assert!(
            stderr.contains("mail is stored in the account database"),
            "mp {args:?} must give the boundary line: {stderr}"
        );
    }
}

/// The attachment commands decline *before* reading anything, so the dead
/// `_attachments/` tree can never turn an unaddressable message into a
/// successful no-op.
#[test]
fn attachment_commands_never_report_no_attachments_found() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();
    let existing = message_with_attachments(home);

    for args in [
        vec!["open", existing.as_str()],
        vec!["save", existing.as_str()],
    ] {
        let out = Command::new(MP)
            .args(&args)
            .env("HOME", home)
            .env("MAILYPOPPINS_DATA_DIR", home.join("data"))
            .output()
            .expect("mp must run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success(), "mp {args:?} must not succeed");
        assert!(
            !stdout.contains("No attachments found"),
            "mp {args:?} must not claim the message has no attachments: {stdout}"
        );
        assert!(
            stdout.is_empty(),
            "mp {args:?} must print nothing on stdout: {stdout}"
        );
    }
}

/// The draft commands decline instead of failing inside the draft builder: an
/// I/O error from a path the user did not choose is not an answer.
#[test]
fn draft_commands_decline_instead_of_failing_on_a_missing_file() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();

    for args in [
        vec!["reply", "gone.md"],
        vec!["forward", "gone.md"],
    ] {
        let (code, stderr) = run(home, &args);
        assert_eq!(code, 1, "mp {args:?} must fail: {stderr}");
        assert!(
            !stderr.contains("No such file or directory"),
            "mp {args:?} must not surface a raw I/O error: {stderr}"
        );
        assert!(
            stderr.contains("mp-legacy is the working fallback"),
            "mp {args:?} must name the fallback that works today: {stderr}"
        );
    }
}
