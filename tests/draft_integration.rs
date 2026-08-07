use email::draft::{
    create_forward_draft_from, create_reply_draft_from, find_drafts, mark_as_approved,
    mark_as_draft, mark_draft_sent, parse_email_draft, validate_draft, SourceMessage,
};
use email::types::EmailStatus;
use std::fs;
use tempfile::tempdir;

/// The source a reply or a forward is built from.
///
/// Received mail is a store row, not a file (#0038), so the builders take a
/// [`SourceMessage`] and this is what `draft::source_from_row` hands them:
/// these tests pin the formatting of the draft, not where the source came
/// from.
fn source(from: &str, to: &str, subject: &str, body: &str) -> SourceMessage {
    SourceMessage {
        from: from.to_string(),
        to: to.to_string(),
        cc: None,
        subject: subject.to_string(),
        message_id: Some("<source@example.com>".to_string()),
        date: Some("Mon, 01 Jan 2024 12:00:00 +0000".to_string()),
        body: body.to_string(),
        attachments: Vec::new(),
        html: None,
    }
}

fn write_draft(dir: &std::path::Path, filename: &str, to: &str, subject: &str, body: &str, status: &str) -> std::path::PathBuf {
    let path = dir.join(filename);
    let content = format!(
        "---\nfrom: \"me@example.com\"\nto: \"{to}\"\nsubject: \"{subject}\"\nstatus: {status}\n---\n\n{body}"
    );
    fs::write(&path, content).unwrap();
    path
}

// -----------------------------------------------------------------------
// Reply drafts
// -----------------------------------------------------------------------

#[test]
fn test_create_reply_draft() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let source = source("alice@example.com", "me@example.com", "Hello", "Original body");
    let draft_path =
        create_reply_draft_from(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

    assert!(draft_path.exists());
    let content = fs::read_to_string(&draft_path).unwrap();

    // Check frontmatter fields
    assert!(content.contains("to: \"alice@example.com\""));
    assert!(content.contains("subject: \"Re: Hello\""));
    assert!(content.contains("status: draft"));
    assert!(content.contains("from: \"me@example.com\""));

    // Check body structure
    assert!(content.contains("{{SIGNATURE}}"));
    assert!(content.contains("> Original body"));
    assert!(content.contains("alice@example.com wrote:"));
}

/// A reply names the message it answers, and the value survives the parse
/// (#TKT-0051). This is what the post-send hook reads to put `\Answered` on
/// the source; a draft that never goes out therefore claims nothing.
#[test]
fn a_reply_draft_records_the_message_it_answers() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let source = source("alice@example.com", "me@example.com", "Hello", "Original body");
    let draft_path =
        create_reply_draft_from(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

    let content = fs::read_to_string(&draft_path).unwrap();
    assert!(content.contains("in_reply_to: \"<source@example.com>\""));
    assert!(!content.contains("forwarded_from:"));

    let draft = parse_email_draft(&draft_path).unwrap();
    assert_eq!(
        draft.frontmatter.in_reply_to.as_deref(),
        Some("<source@example.com>")
    );
    assert_eq!(draft.frontmatter.forwarded_from, None);
}

/// The forward half of the same record (#TKT-0051).
#[test]
fn a_forward_draft_records_the_message_it_forwards() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let source = source("alice@example.com", "me@example.com", "Hello", "Forward me");
    let draft_path =
        create_forward_draft_from(&source, "me@example.com", Some(drafts.as_path())).unwrap();

    let draft = parse_email_draft(&draft_path).unwrap();
    assert_eq!(
        draft.frontmatter.forwarded_from.as_deref(),
        Some("<source@example.com>")
    );
    assert_eq!(draft.frontmatter.in_reply_to, None);
}

/// A source with no `Message-ID` (a server-search hit that carried none)
/// writes no key at all rather than an empty one, and the draft still parses.
#[test]
fn a_source_without_a_message_id_leaves_the_key_out() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let mut source = source("alice@example.com", "me@example.com", "Hello", "Body");
    source.message_id = None;
    let draft_path =
        create_reply_draft_from(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

    let content = fs::read_to_string(&draft_path).unwrap();
    assert!(!content.contains("in_reply_to:"));
    assert_eq!(
        parse_email_draft(&draft_path).unwrap().frontmatter.in_reply_to,
        None
    );
}

#[test]
fn test_create_reply_draft_already_re_prefix() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let source = source("alice@example.com", "me@example.com", "Re: Hello", "Body");
    let draft_path =
        create_reply_draft_from(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

    let content = fs::read_to_string(&draft_path).unwrap();
    // Should not double the Re: prefix
    assert!(content.contains("subject: \"Re: Hello\""));
    assert!(!content.contains("Re: Re:"));
}

#[test]
fn test_create_reply_all_draft() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    // Source with a CC line.
    let mut source = source(
        "alice@example.com",
        "me@example.com, bob@example.com",
        "Meeting",
        "Meeting notes",
    );
    source.cc = Some("carol@example.com".to_string());

    let draft_path =
        create_reply_draft_from(&source, true, "me@example.com", Some(drafts.as_path())).unwrap();
    let draft_content = fs::read_to_string(&draft_path).unwrap();

    // Reply-all should have CC with bob and carol but not self
    assert!(draft_content.contains("to: \"alice@example.com\""));
    assert!(draft_content.contains("cc:"));
    assert!(draft_content.contains("bob@example.com"));
    assert!(draft_content.contains("carol@example.com"));
    assert!(!draft_content.contains("cc: \"me@example.com"));
}

// -----------------------------------------------------------------------
// Forward drafts
// -----------------------------------------------------------------------

#[test]
fn test_create_forward_draft() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let source = source("alice@example.com", "me@example.com", "Hello", "Forward me");
    let draft_path =
        create_forward_draft_from(&source, "me@example.com", Some(drafts.as_path())).unwrap();

    let content = fs::read_to_string(&draft_path).unwrap();

    assert!(content.contains("subject: \"Fwd: Hello\""));
    assert!(content.contains("to: \"\""));
    assert!(content.contains("{{SIGNATURE}}"));
    assert!(content.contains("Forwarded message"));
    assert!(content.contains("From: alice@example.com"));
    assert!(content.contains("Forward me"));
}

#[test]
fn test_forward_with_attachments() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    // The caller materialised the row's attachment blobs into files and hands
    // over their paths, which is what `source_from_row` does for a forward.
    let materialised = tmp.path().join("attachments");
    fs::create_dir_all(&materialised).unwrap();
    let report = materialised.join("report.pdf");
    fs::write(&report, b"fake pdf").unwrap();

    let mut source = source(
        "alice@example.com",
        "me@example.com",
        "With attachment",
        "See attached",
    );
    source.attachments = vec![report.clone()];

    let draft_path =
        create_forward_draft_from(&source, "me@example.com", Some(drafts.as_path())).unwrap();
    let draft_content = fs::read_to_string(&draft_path).unwrap();

    // Forward should reference the attachment by the path it was given.
    assert!(draft_content.contains("attachments:"));
    assert!(draft_content.contains(report.to_string_lossy().as_ref()));
}

/// Ticket #0006: forwarding a message and then losing the source mailbox must
/// not invalidate the attachment paths in the draft. The forward references
/// the per-account stable attachments mirror, which is where the store-backed
/// source (`draft::source_from_row`) materialises a row's blobs, so it
/// survives the source row being archived, evicted or deleted.
#[test]
fn test_forward_then_archive_source_keeps_attachment_resolvable() {
    let tmp = tempdir().unwrap();
    let account = tmp.path().join("account");
    let drafts = account.join("drafts");

    let stable = email::parse::stable_attachments_dir(&account, "<att-archive@example.com>");
    fs::create_dir_all(&stable).unwrap();
    fs::write(stable.join("report.pdf"), b"fake pdf").unwrap();

    let mut source = source(
        "alice@example.com",
        "me@example.com",
        "With attachment",
        "See attached",
    );
    source.attachments = vec![stable.join("report.pdf")];

    let draft_path =
        create_forward_draft_from(&source, "me@example.com", Some(drafts.as_path())).unwrap();

    // The source's own mailbox goes away; the mirror does not.
    let inbox = account.join("inbox");
    fs::create_dir_all(&inbox).unwrap();
    fs::remove_dir_all(&inbox).unwrap();

    // Re-parse the draft and resolve every attachment path on disk.
    let draft = parse_email_draft(&draft_path).unwrap();
    let attachments = draft.frontmatter.attachments.expect("attachments");
    assert!(!attachments.is_empty());
    for path in &attachments {
        assert!(
            std::path::Path::new(path).exists(),
            "attachment must still be readable after the source is gone: {}",
            path
        );
        let bytes = fs::read(path).unwrap();
        assert_eq!(bytes, b"fake pdf");
    }
}

// -----------------------------------------------------------------------
// Reply with companion HTML
// -----------------------------------------------------------------------

#[test]
fn test_reply_with_companion_html() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    // The html blob of the row (`store::read::load_html`) is what the source
    // carries; the reply wraps it as the draft's companion.
    let mut source = source(
        "alice@example.com",
        "me@example.com",
        "HTML email",
        "Plain text body",
    );
    source.html = Some("<p>Rich HTML body</p>".to_string());

    let draft_path =
        create_reply_draft_from(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

    // Draft should have a companion HTML with quoted content
    let draft_html = draft_path.with_extension("html");
    assert!(draft_html.exists());
    let html = fs::read_to_string(&draft_html).unwrap();
    assert!(html.contains("Rich HTML body"));
    assert!(html.contains("alice@example.com wrote:"));
}

// -----------------------------------------------------------------------
// Parse and validate drafts
// -----------------------------------------------------------------------

#[test]
fn test_parse_and_validate_draft() {
    let tmp = tempdir().unwrap();
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body content", "draft");

    let draft = parse_email_draft(&path).unwrap();
    assert_eq!(draft.frontmatter.to.as_deref(), Some("alice@example.com"));
    assert_eq!(draft.frontmatter.subject, "Test");
    assert_eq!(draft.frontmatter.status, EmailStatus::Draft);
    assert_eq!(draft.body_markdown, "Body content");

    let warnings = validate_draft(&draft).unwrap();
    assert!(warnings.is_empty());
}

// -----------------------------------------------------------------------
// Status transitions
// -----------------------------------------------------------------------

#[test]
fn test_mark_as_approved() {
    let tmp = tempdir().unwrap();
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", "draft");

    let msg = mark_as_approved(&path).unwrap();
    assert!(msg.contains("approved"));

    let draft = parse_email_draft(&path).unwrap();
    assert_eq!(draft.frontmatter.status, EmailStatus::Approved);
}

#[test]
fn test_mark_as_approved_already_approved() {
    let tmp = tempdir().unwrap();
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", "approved");

    let msg = mark_as_approved(&path).unwrap();
    assert!(msg.contains("Already approved"));
}

#[test]
fn test_mark_as_approved_sent_fails() {
    let tmp = tempdir().unwrap();
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", "sent");

    let result = mark_as_approved(&path);
    assert!(result.is_err());
}

#[test]
fn test_mark_as_draft_demotes_approved() {
    let tmp = tempdir().unwrap();
    let path = write_draft(
        tmp.path(),
        "draft.md",
        "alice@example.com",
        "Test",
        "Body",
        "approved",
    );

    let msg = mark_as_draft(&path).unwrap();
    assert!(msg.contains("Marked as draft"));

    let draft = parse_email_draft(&path).unwrap();
    assert_eq!(draft.frontmatter.status, EmailStatus::Draft);
}

#[test]
fn test_mark_as_draft_already_draft() {
    let tmp = tempdir().unwrap();
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", "draft");

    let msg = mark_as_draft(&path).unwrap();
    assert!(msg.contains("Already a draft"));

    // Status unchanged.
    let draft = parse_email_draft(&path).unwrap();
    assert_eq!(draft.frontmatter.status, EmailStatus::Draft);
}

#[test]
fn test_mark_as_draft_sent_fails() {
    let tmp = tempdir().unwrap();
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", "sent");

    let result = mark_as_draft(&path);
    assert!(result.is_err());
}

/// A file carrying one of the file-era placement statuses is refused, and now
/// refused one step earlier: `EmailStatus` narrowed to the three draft states
/// (#0064), so `inbox` and `archived` no longer deserialize at all. Nothing
/// writes such a file -- the receive path stopped writing `.md` at the store
/// cutover, and no draft was ever created with one.
#[test]
fn test_mark_as_draft_inbox_fails() {
    let tmp = tempdir().unwrap();
    for status in ["inbox", "archived"] {
        let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", status);

        let result = mark_as_draft(&path);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("Failed to parse frontmatter"), "unexpected error: {err}");
    }
}

#[test]
fn test_mark_draft_sent() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&drafts).unwrap();

    let path = write_draft(&drafts, "draft.md", "alice@example.com", "Test", "Body", "approved");
    let draft = parse_email_draft(&path).unwrap();

    mark_draft_sent(&draft, Some("<msg123@example.com>")).unwrap();

    // The draft is marked in place: there is no local sent `.md` any more,
    // the Sent copy is the durable outbox's (#0037).
    assert!(path.exists());
    assert!(!tmp.path().join("sent").exists());

    let sent_draft = parse_email_draft(&path).unwrap();
    assert_eq!(sent_draft.frontmatter.status, EmailStatus::Sent);
    assert!(sent_draft.frontmatter.sent_at.is_some());
    assert_eq!(sent_draft.frontmatter.message_id, Some("<msg123@example.com>".to_string()));
}

// -----------------------------------------------------------------------
// find_drafts with status filter
// -----------------------------------------------------------------------

#[test]
fn test_find_drafts_with_status_filter() {
    let tmp = tempdir().unwrap();
    write_draft(tmp.path(), "a.md", "alice@example.com", "Draft A", "Body", "draft");
    write_draft(tmp.path(), "b.md", "bob@example.com", "Draft B", "Body", "approved");
    write_draft(tmp.path(), "c.md", "carol@example.com", "Draft C", "Body", "draft");

    let all = find_drafts(tmp.path(), None).unwrap();
    assert_eq!(all.len(), 3);

    let drafts_only = find_drafts(tmp.path(), Some(EmailStatus::Draft)).unwrap();
    assert_eq!(drafts_only.len(), 2);

    let approved_only = find_drafts(tmp.path(), Some(EmailStatus::Approved)).unwrap();
    assert_eq!(approved_only.len(), 1);
    assert_eq!(approved_only[0].frontmatter.to.as_deref(), Some("bob@example.com"));
}

#[test]
fn test_find_drafts_empty_dir() {
    let tmp = tempdir().unwrap();
    let drafts = find_drafts(tmp.path(), None).unwrap();
    assert!(drafts.is_empty());
}

#[test]
fn test_find_drafts_ignores_non_md_files() {
    let tmp = tempdir().unwrap();
    write_draft(tmp.path(), "a.md", "alice@example.com", "Draft", "Body", "draft");
    fs::write(tmp.path().join("notes.txt"), "not a draft").unwrap();
    fs::write(tmp.path().join("data.html"), "<p>html</p>").unwrap();

    let drafts = find_drafts(tmp.path(), None).unwrap();
    assert_eq!(drafts.len(), 1);
}

// -----------------------------------------------------------------------
// mark_draft_sent edge cases
// -----------------------------------------------------------------------

#[test]
fn test_mark_draft_sent_without_message_id() {
    let tmp = tempdir().unwrap();
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", "approved");
    let draft = parse_email_draft(&path).unwrap();

    mark_draft_sent(&draft, None).unwrap();

    let updated = parse_email_draft(&path).unwrap();
    assert_eq!(updated.frontmatter.status, EmailStatus::Sent);
    assert!(updated.frontmatter.message_id.is_none());
}

#[test]
fn test_mark_draft_sent_cleans_companion_html() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&drafts).unwrap();

    let path = write_draft(&drafts, "draft.md", "alice@example.com", "Test", "Body", "approved");
    let html_path = drafts.join("draft.html");
    fs::write(&html_path, "<p>companion</p>").unwrap();

    let draft = parse_email_draft(&path).unwrap();
    mark_draft_sent(&draft, None).unwrap();

    // Companion HTML should be cleaned up
    assert!(!html_path.exists());
}

// -----------------------------------------------------------------------
// parse_email_draft error cases
// -----------------------------------------------------------------------

#[test]
fn test_parse_email_draft_missing_file() {
    let result = parse_email_draft(std::path::Path::new("/nonexistent/file.md"));
    assert!(result.is_err());
}

#[test]
fn test_parse_email_draft_no_frontmatter() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("plain.md");
    fs::write(&path, "Just a plain markdown file with no frontmatter").unwrap();

    let result = parse_email_draft(&path);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("frontmatter"));
}

// -----------------------------------------------------------------------
// Forward draft edge cases
// -----------------------------------------------------------------------

#[test]
fn test_forward_draft_already_fwd_prefix() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let source = source("alice@example.com", "me@example.com", "Fwd: Original", "Body");
    let draft_path =
        create_forward_draft_from(&source, "me@example.com", Some(drafts.as_path())).unwrap();

    let content = fs::read_to_string(&draft_path).unwrap();
    // Should not double the Fwd: prefix
    assert!(content.contains("subject: \"Fwd: Original\""));
    assert!(!content.contains("Fwd: Fwd:"));
}

#[test]
fn test_forward_draft_with_companion_html() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    let mut source = source("alice@example.com", "me@example.com", "Hello", "Body");
    source.html = Some("<p>Rich content</p>".to_string());

    let draft_path =
        create_forward_draft_from(&source, "me@example.com", Some(drafts.as_path())).unwrap();

    let draft_html = draft_path.with_extension("html");
    assert!(draft_html.exists());
    let html = fs::read_to_string(&draft_html).unwrap();
    assert!(html.contains("Rich content"));
    assert!(html.contains("Forwarded message"));
}

// -----------------------------------------------------------------------
// Reply edge cases
// -----------------------------------------------------------------------

#[test]
fn test_reply_all_excludes_self_from_cc() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    // All recipients are self -- CC should be absent
    let source = source("alice@example.com", "me@example.com", "Solo", "Body");

    let draft_path =
        create_reply_draft_from(&source, true, "me@example.com", Some(drafts.as_path())).unwrap();
    let draft_content = fs::read_to_string(&draft_path).unwrap();

    // No cc line should appear since the only other recipient is self
    assert!(!draft_content.contains("cc:"));
}

#[test]
fn test_reply_deduplicates_cc_addresses() {
    let tmp = tempdir().unwrap();
    let drafts = tmp.path().join("drafts");

    // bob appears in both To and CC
    let mut source = source(
        "alice@example.com",
        "me@example.com, bob@example.com",
        "Dupes",
        "Body",
    );
    source.cc = Some("bob@example.com, carol@example.com".to_string());

    let draft_path =
        create_reply_draft_from(&source, true, "me@example.com", Some(drafts.as_path())).unwrap();
    let draft_content = fs::read_to_string(&draft_path).unwrap();

    // bob should appear only once in cc
    let bob_count = draft_content.matches("bob@example.com").count();
    assert_eq!(bob_count, 1, "bob should appear exactly once in CC");
}

// -----------------------------------------------------------------------
// validate_draft with attachments
// -----------------------------------------------------------------------

#[test]
fn test_validate_draft_missing_attachment_warning() {
    let draft = email::types::EmailDraft {
        path: std::path::PathBuf::from("test.md"),
        frontmatter: email::types::EmailFrontmatter {
            id: None,
            date: None,
            to: Some("alice@example.com".to_string()),
            cc: None,
            bcc: None,
            subject: "Test".to_string(),
            status: EmailStatus::Draft,
            from: Some("me@example.com".to_string()),
            reply_to: None,
            attachments: Some(vec!["/nonexistent/file.pdf".to_string()]),
            sent_at: None,
            sent_via: None,
            message_id: None,
            in_reply_to: None,
            forwarded_from: None,
            event: None,
        },
        body_markdown: "Body".to_string(),
    };

    let warnings = validate_draft(&draft).unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Attachment not found"));
}

#[test]
fn test_validate_draft_existing_attachment_no_warning() {
    let tmp = tempdir().unwrap();
    let att_path = tmp.path().join("doc.pdf");
    fs::write(&att_path, b"fake pdf").unwrap();

    let draft = email::types::EmailDraft {
        path: std::path::PathBuf::from("test.md"),
        frontmatter: email::types::EmailFrontmatter {
            id: None,
            date: None,
            to: Some("alice@example.com".to_string()),
            cc: None,
            bcc: None,
            subject: "Test".to_string(),
            status: EmailStatus::Draft,
            from: Some("me@example.com".to_string()),
            reply_to: None,
            attachments: Some(vec![att_path.to_string_lossy().to_string()]),
            sent_at: None,
            sent_via: None,
            message_id: None,
            in_reply_to: None,
            forwarded_from: None,
            event: None,
        },
        body_markdown: "Body".to_string(),
    };

    let warnings = validate_draft(&draft).unwrap();
    assert!(warnings.is_empty());
}
