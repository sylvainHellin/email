use email::draft::{
    create_forward_draft, create_reply_draft, find_drafts, mark_as_approved, mark_as_draft,
    mark_draft_sent, parse_email_draft, validate_draft,
};
use email::types::EmailStatus;
use std::fs;
use tempfile::tempdir;

/// Write a minimal inbox email file and return its path.
fn write_inbox_email(dir: &std::path::Path, filename: &str, from: &str, subject: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(filename);
    let content = format!(
        "---\nfrom: \"{from}\"\nto: \"me@example.com\"\nsubject: \"{subject}\"\ndate: \"Mon, 01 Jan 2024 12:00:00 +0000\"\nmessage_id: \"<{filename}@example.com>\"\nstatus: inbox\nhas_attachments: false\n---\n\n{body}"
    );
    fs::write(&path, content).unwrap();
    path
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
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    let source = write_inbox_email(&inbox, "email.md", "alice@example.com", "Hello", "Original body");

    let draft_path = create_reply_draft(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

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

#[test]
fn test_create_reply_draft_already_re_prefix() {
    let tmp = tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    let source = write_inbox_email(&inbox, "email.md", "alice@example.com", "Re: Hello", "Body");
    let draft_path = create_reply_draft(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

    let content = fs::read_to_string(&draft_path).unwrap();
    // Should not double the Re: prefix
    assert!(content.contains("subject: \"Re: Hello\""));
    assert!(!content.contains("Re: Re:"));
}

#[test]
fn test_create_reply_all_draft() {
    let tmp = tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    // Source email with CC
    let path = inbox.join("email.md");
    let content = "---\nfrom: \"alice@example.com\"\nto: \"me@example.com, bob@example.com\"\ncc: \"carol@example.com\"\nsubject: \"Meeting\"\ndate: \"Mon, 01 Jan 2024 12:00:00 +0000\"\nmessage_id: \"<meeting@example.com>\"\nstatus: inbox\nhas_attachments: false\n---\n\nMeeting notes";
    fs::write(&path, content).unwrap();

    let draft_path = create_reply_draft(&path, true, "me@example.com", Some(drafts.as_path())).unwrap();
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
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    let source = write_inbox_email(&inbox, "email.md", "alice@example.com", "Hello", "Forward me");

    let draft_path = create_forward_draft(&source, "me@example.com", Some(drafts.as_path())).unwrap();

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
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    // Create source email with attachments
    let source = inbox.join("email.md");
    let att_dir = inbox.join("email_attachments");
    fs::create_dir_all(&att_dir).unwrap();
    fs::write(att_dir.join("report.pdf"), b"fake pdf").unwrap();

    let content = "---\nfrom: \"alice@example.com\"\nto: \"me@example.com\"\nsubject: \"With attachment\"\ndate: \"Mon, 01 Jan 2024 12:00:00 +0000\"\nmessage_id: \"<att@example.com>\"\nstatus: inbox\nhas_attachments: true\nattachments:\n  - \"report.pdf\"\n---\n\nSee attached";
    fs::write(&source, content).unwrap();

    let draft_path = create_forward_draft(&source, "me@example.com", Some(drafts.as_path())).unwrap();
    let draft_content = fs::read_to_string(&draft_path).unwrap();

    // Forward should reference the attachment
    assert!(draft_content.contains("attachments:"));
    assert!(draft_content.contains("report.pdf"));
}

/// Ticket #0006: forwarding an email and then archiving the source must not
/// invalidate the attachment paths in the draft. The forward draft should
/// reference the per-account stable attachments mirror, which survives a
/// move of the source out of the inbox.
#[test]
fn test_forward_then_archive_source_keeps_attachment_resolvable() {
    let tmp = tempdir().unwrap();
    let account = tmp.path().join("account");
    let inbox = account.join("inbox");
    let archive = account.join("archive");
    let drafts = account.join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&archive).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    let source = inbox.join("email.md");
    let att_dir = inbox.join("email_attachments");
    fs::create_dir_all(&att_dir).unwrap();
    fs::write(att_dir.join("report.pdf"), b"fake pdf").unwrap();

    let content = "---\nfrom: \"alice@example.com\"\nto: \"me@example.com\"\nsubject: \"With attachment\"\ndate: \"Mon, 01 Jan 2024 12:00:00 +0000\"\nmessage_id: \"<att-archive@example.com>\"\nstatus: inbox\nhas_attachments: true\nattachments:\n  - \"report.pdf\"\n---\n\nSee attached";
    fs::write(&source, content).unwrap();

    let draft_path =
        create_forward_draft(&source, "me@example.com", Some(drafts.as_path())).unwrap();

    // Move the source into archive, per-mailbox _attachments/ included.
    let dest = archive.join(source.file_name().unwrap());
    fs::rename(&source, &dest).unwrap();
    fs::rename(&att_dir, archive.join("email_attachments")).unwrap();
    assert!(!source.exists());

    // Re-parse the draft and resolve every attachment path on disk.
    let draft = parse_email_draft(&draft_path).unwrap();
    let attachments = draft.frontmatter.attachments.expect("attachments");
    assert!(!attachments.is_empty());
    for path in &attachments {
        assert!(
            std::path::Path::new(path).exists(),
            "attachment must still be readable after archive: {}",
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
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    let source = write_inbox_email(&inbox, "email.md", "alice@example.com", "HTML email", "Plain text body");

    // Write companion HTML
    let html_path = inbox.join("email.html");
    fs::write(&html_path, "<p>Rich HTML body</p>").unwrap();

    let draft_path = create_reply_draft(&source, false, "me@example.com", Some(drafts.as_path())).unwrap();

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

#[test]
fn test_mark_as_draft_inbox_fails() {
    let tmp = tempdir().unwrap();
    // write_draft expects a draft-shaped frontmatter; inbox emails are
    // shaped slightly differently (`from:` instead of `to:`), but for
    // mark_as_draft's status guard it's the parsed status that matters.
    let path = write_draft(tmp.path(), "draft.md", "alice@example.com", "Test", "Body", "inbox");

    let result = mark_as_draft(&path);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("Only approved drafts"), "unexpected error: {err}");
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
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    let source = write_inbox_email(&inbox, "email.md", "alice@example.com", "Fwd: Original", "Body");
    let draft_path = create_forward_draft(&source, "me@example.com", Some(drafts.as_path())).unwrap();

    let content = fs::read_to_string(&draft_path).unwrap();
    // Should not double the Fwd: prefix
    assert!(content.contains("subject: \"Fwd: Original\""));
    assert!(!content.contains("Fwd: Fwd:"));
}

#[test]
fn test_forward_draft_with_companion_html() {
    let tmp = tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    let source = write_inbox_email(&inbox, "email.md", "alice@example.com", "Hello", "Body");
    let html_path = inbox.join("email.html");
    fs::write(&html_path, "<p>Rich content</p>").unwrap();

    let draft_path = create_forward_draft(&source, "me@example.com", Some(drafts.as_path())).unwrap();

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
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    // All recipients are self -- CC should be absent
    let path = inbox.join("email.md");
    let content = "---\nfrom: \"alice@example.com\"\nto: \"me@example.com\"\nsubject: \"Solo\"\ndate: \"Mon, 01 Jan 2024 12:00:00 +0000\"\nmessage_id: \"<solo@example.com>\"\nstatus: inbox\nhas_attachments: false\n---\n\nBody";
    fs::write(&path, content).unwrap();

    let draft_path = create_reply_draft(&path, true, "me@example.com", Some(drafts.as_path())).unwrap();
    let draft_content = fs::read_to_string(&draft_path).unwrap();

    // No cc line should appear since the only other recipient is self
    assert!(!draft_content.contains("cc:"));
}

#[test]
fn test_reply_deduplicates_cc_addresses() {
    let tmp = tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    let drafts = tmp.path().join("drafts");
    fs::create_dir_all(&inbox).unwrap();
    fs::create_dir_all(&drafts).unwrap();

    // bob appears in both To and CC
    let path = inbox.join("email.md");
    let content = "---\nfrom: \"alice@example.com\"\nto: \"me@example.com, bob@example.com\"\ncc: \"bob@example.com, carol@example.com\"\nsubject: \"Dupes\"\ndate: \"Mon, 01 Jan 2024 12:00:00 +0000\"\nmessage_id: \"<dupes@example.com>\"\nstatus: inbox\nhas_attachments: false\n---\n\nBody";
    fs::write(&path, content).unwrap();

    let draft_path = create_reply_draft(&path, true, "me@example.com", Some(drafts.as_path())).unwrap();
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
            event: None,
        },
        body_markdown: "Body".to_string(),
    };

    let warnings = validate_draft(&draft).unwrap();
    assert!(warnings.is_empty());
}
