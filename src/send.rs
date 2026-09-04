use anyhow::{anyhow, Context, Result};
use lettre::{
    address::Envelope,
    message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use log::{debug, error, info, warn};
use pulldown_cmark::{html, Options, Parser as MdParser};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{AuthMethod, EmailSettings, SmtpConfig};
use crate::types::{EmailDraft, EmailStatus};

// ---------------------------------------------------------------------------
// Per-recipient sending types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecipientRole {
    To,
    Cc,
    Bcc,
}

impl std::fmt::Display for RecipientRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecipientRole::To => write!(f, "To"),
            RecipientRole::Cc => write!(f, "Cc"),
            RecipientRole::Bcc => write!(f, "Bcc"),
        }
    }
}

/// What the transport said about one recipient (#0063).
///
/// SMTP is one conversation per recipient here, so this is a verdict about
/// that recipient and not about the message: the four values are what the
/// outbox needs to decide whether that recipient may be attempted again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientVerdict {
    /// 250. The recipient has the message and is never attempted again.
    Delivered,
    /// A 5xx, or an address no envelope can be built from. Refused for good.
    Rejected,
    /// A 4xx, no connection, no credentials: nothing was accepted and the next
    /// pass may well succeed.
    Retryable,
    /// No verdict came back, so the recipient may or may not hold the message.
    Unknown,
}

#[derive(Debug)]
pub struct RecipientResult {
    pub address: String,
    pub role: RecipientRole,
    pub success: bool,
    pub error: Option<String>,
    /// What the transport said about this recipient, which is what the outbox
    /// records durably; see [`SendResult::submit_outcome`].
    pub verdict: RecipientVerdict,
}

impl RecipientResult {
    /// True when the failure leaves it unknown whether the server accepted the
    /// message. Drives the outbox's never-auto-re-send rule.
    pub fn ambiguous(&self) -> bool {
        self.verdict == RecipientVerdict::Unknown
    }
}

#[derive(Debug)]
pub struct SendResult {
    pub results: Vec<RecipientResult>,
}

impl SendResult {
    pub fn all_succeeded(&self) -> bool {
        self.results.iter().all(|r| r.success)
    }

    pub fn any_succeeded(&self) -> bool {
        self.results.iter().any(|r| r.success)
    }

    pub fn succeeded(&self) -> Vec<&RecipientResult> {
        self.results.iter().filter(|r| r.success).collect()
    }

    pub fn failed(&self) -> Vec<&RecipientResult> {
        self.results.iter().filter(|r| !r.success).collect()
    }

    /// How the durable outbox must read this result (#0037 item 5, #0063).
    ///
    /// One verdict per recipient, because that is what the per-recipient loop
    /// in [`submit`] actually produced: the same bytes go out in separate
    /// envelopes, so "the message was accepted" is not a fact about the
    /// message but about each recipient in turn. The outbox folds the set into
    /// a row state and, more to the point, remembers who took it, so a retry
    /// never delivers twice.
    pub fn submit_outcome(&self) -> crate::outbox::SubmitOutcome {
        let mut verdicts = crate::outbox::RecipientVerdicts::default();
        for r in &self.results {
            let reason = r.error.clone().unwrap_or_else(|| "unknown error".to_string());
            match r.verdict {
                RecipientVerdict::Delivered => verdicts.delivered.push(r.address.clone()),
                RecipientVerdict::Rejected => verdicts.rejected.push((r.address.clone(), reason)),
                RecipientVerdict::Retryable => verdicts.retryable.push((r.address.clone(), reason)),
                RecipientVerdict::Unknown => verdicts.ambiguous.push((r.address.clone(), reason)),
            }
        }
        crate::outbox::SubmitOutcome::PerRecipient(verdicts)
    }
}

/// What one recipient's SMTP failure means for that recipient (#0063).
///
/// The question is never "did it work" but "may this recipient be attempted
/// again", and SMTP answers it in three ways:
///
/// - a 5xx is the server refusing in words, and it will refuse again: the
///   recipient is rejected for good and the user has to be told;
/// - a 4xx, a client-side error (no credentials, no usable mechanism) or a TLS
///   failure all happen before any bytes could be accepted, and all of them
///   can be gone by the next attempt: retryable;
/// - anything else is a timeout or a connection that died somewhere in the
///   conversation, where the 250 may simply have been lost on the way back:
///   unknown, and never attempted again automatically.
fn smtp_failure_verdict(err: &lettre::transport::smtp::Error) -> RecipientVerdict {
    if err.is_timeout() {
        return RecipientVerdict::Unknown;
    }
    if err.is_permanent() {
        return RecipientVerdict::Rejected;
    }
    if err.is_transient() || err.is_client() || err.is_tls() {
        return RecipientVerdict::Retryable;
    }
    RecipientVerdict::Unknown
}

pub fn markdown_to_html(
    markdown: &str,
    config: &EmailSettings,
    signature: Option<&str>,
    quoted_html: Option<&str>,
) -> String {
    // The signature is Markdown (#0099, resolved by
    // `config::resolve_signature_markdown`). Draft sends pass `None` because the
    // signature was already spliced into the draft body at reply/forward/compose
    // time; only the send-time paths that have no editable draft (invites,
    // direct sends) pass `Some`. Append it to the body Markdown before rendering
    // so it goes through the same converter as the body and inherits the body
    // font, instead of being injected as pre-styled HTML (which rendered the
    // signature at a different size, and double-injected it on replies).
    // Drop the signature sentinel comment lines (#0106) before rendering. They
    // wrap the spliced block in the draft body so a re-splice can find it;
    // CommonMark would otherwise pass them through as raw HTML comments into the
    // sent message. Stripping the lines keeps the signature content and its
    // surrounding blank lines intact.
    let sanitized = crate::draft::strip_signature_sentinels(markdown);
    let markdown = sanitized.as_str();

    let owned_body;
    let markdown = match signature {
        Some(sig) if !sig.trim().is_empty() => {
            owned_body = format!("{}\n\n{}", markdown.trim_end(), sig.trim());
            owned_body.as_str()
        }
        _ => markdown,
    };

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = MdParser::new_ext(markdown, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);

    // The `{{SIGNATURE}}` marker (reply/forward drafts) no longer carries the
    // signature; it is purely the boundary where the quoted section begins.
    // Split there and wrap the quoted content in a styled <div> so email clients
    // (Apple Mail, Gmail) do not collapse the reply and signature behind "see
    // more". Replace <blockquote> with styled <div> in the quoted section for
    // the same reason. Regular drafts have no marker and render as-is.
    let body = if html_output.contains("{{SIGNATURE}}") {
        // pulldown-cmark wraps the placeholder in <p> tags; match that form first
        let marker = if html_output.contains("<p>{{SIGNATURE}}</p>") {
            "<p>{{SIGNATURE}}</p>"
        } else {
            "{{SIGNATURE}}"
        };
        let parts: Vec<&str> = html_output.splitn(2, marker).collect();
        let reply_part = parts[0];

        if let Some(original_html) = quoted_html {
            // Use original HTML instead of Markdown-converted blockquotes
            format!(
                "{}\n<div style=\"padding-top:1em\">\n{}\n</div>",
                reply_part.trim_end(),
                original_html,
            )
        } else {
            // Fallback: convert Markdown blockquotes to styled divs
            let quoted_part = if parts.len() > 1 { parts[1] } else { "" };
            let quoted_styled = quoted_part
                .replace("<blockquote>", "<div style=\"margin:0;padding:0 0 0 1em;border-left:2px solid #ccc\">")
                .replace("</blockquote>", "</div>");
            format!(
                "{}\n<div style=\"padding-top:1em\">\n{}\n</div>",
                reply_part.trim_end(),
                quoted_styled.trim_start()
            )
        }
    } else {
        html_output
    };

    // Wrap in basic HTML structure with styling from config. The font is set
    // both in the head <style> and as an inline style on the content wrapper:
    // Gmail and Outlook strip <style> blocks, so without the inline copy the
    // body would fall back to the client default while any inline-styled
    // fragment (a pasted quote) kept its own size.
    //
    // The inline copy lands in a double-quoted `style="..."` attribute, so any
    // literal double quote in the font stack (e.g. `"Times New Roman", serif`)
    // would prematurely close the attribute. Escape it to `&quot;` for that
    // context; the <style> block is not an attribute and takes the value raw.
    let font_family_attr = config.font_family.replace('"', "&quot;");
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<style>
body {{ font-family: {font_family}; font-size: {font_size}; line-height: 1.6; color: #000; }}
a {{ color: #0066cc; }}
p {{ margin: 0 0 1em 0; }}
blockquote {{ margin: 0.5em 0; padding: 0 0 0 1em; border-left: 2px solid #ccc; white-space: pre-wrap; }}
</style>
</head>
<body>
<div style="font-family: {font_family_attr}; font-size: {font_size}; line-height: 1.6; color: #000;">
{body}
</div>
</body>
</html>"#,
        font_family = config.font_family,
        font_family_attr = font_family_attr,
        font_size = config.font_size,
        body = body,
    )
}

/// Strip the `{{SIGNATURE}}` marker out of a plain-text body (#0102).
///
/// Since #0099 the signature Markdown is spliced into the draft body at
/// creation time and a bare `{{SIGNATURE}}` marker is left where the HTML send
/// path splices the rich-HTML quote (`draft.rs`). The HTML build consumes that
/// marker (`markdown_to_html`) and the TUI preview substitutes it for display
/// (`tui/ui/preview.rs`), but the `text/plain` alternative shipped
/// `body_markdown` verbatim, so a recipient reading the plain part saw the
/// literal `{{SIGNATURE}}` mid-message. Drop the marker and collapse the blank
/// lines it padded down to a single paragraph break so the plain text reads
/// naturally and no double blank line is left where it stood.
/// The plain-text body of a draft: the Markdown with the signature sentinel
/// comments (#0106) and the `{{SIGNATURE}}` quote marker (#0102) removed, so a
/// recipient reading the `text/plain` alternative sees neither.
fn plain_text_body(body_markdown: &str) -> String {
    strip_signature_marker(&crate::draft::strip_signature_sentinels(body_markdown))
}

fn strip_signature_marker(body: &str) -> String {
    const MARKER: &str = "{{SIGNATURE}}";
    let mut out = body.to_string();
    while let Some(idx) = out.find(MARKER) {
        let before = out[..idx].trim_end_matches(char::is_whitespace).to_string();
        let after = out[idx + MARKER.len()..]
            .trim_start_matches(char::is_whitespace)
            .to_string();
        out = if before.is_empty() {
            after
        } else if after.is_empty() {
            format!("{before}\n")
        } else {
            format!("{before}\n\n{after}")
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmailSettings;
    use insta::assert_snapshot;

    fn default_settings() -> EmailSettings {
        EmailSettings::default()
    }

    #[test]
    fn test_markdown_to_html_basic_paragraph() {
        let html = markdown_to_html("Hello **world**!\n\nSecond paragraph.", &default_settings(), None, None);
        assert_snapshot!(html);
    }

    #[test]
    fn test_resolve_attachment_paths_expands_folder() {
        let dir = tempfile::tempdir().unwrap();
        // Files land out of order and include a dotfile that must be skipped.
        fs::write(dir.path().join("b.pdf"), b"b").unwrap();
        fs::write(dir.path().join("a.pdf"), b"a").unwrap();
        fs::write(dir.path().join(".hidden"), b"x").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();

        let entries = vec![dir.path().to_string_lossy().to_string()];
        let resolved = resolve_attachment_paths(&entries).unwrap();

        let names: Vec<String> = resolved
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        // Sorted, dotfile and subdirectory dropped.
        assert_eq!(names, vec!["a.pdf".to_string(), "b.pdf".to_string()]);
    }

    #[test]
    fn test_resolve_attachment_paths_passes_files_through() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("one.pdf");
        fs::write(&file, b"one").unwrap();
        // A file entry and a non-existent entry are both returned unchanged;
        // the missing one fails later at read time, not here.
        let entries = vec![
            file.to_string_lossy().to_string(),
            "/no/such/file.pdf".to_string(),
        ];
        let resolved = resolve_attachment_paths(&entries).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0], file);
        assert_eq!(resolved[1], PathBuf::from("/no/such/file.pdf"));
    }

    #[test]
    fn test_markdown_to_html_with_signature_placeholder() {
        // Reply drafts carry the (Markdown) signature spliced into the body at
        // creation time (#0099); the send-time signature arg is None and the
        // `{{SIGNATURE}}` marker is only the quote boundary.
        let md = "My reply\n\n-- Best, Alice\n\n{{SIGNATURE}}\n\n> Original message";
        let html = markdown_to_html(md, &default_settings(), None, None);
        assert_snapshot!(html);
    }

    #[test]
    fn test_markdown_to_html_signature_with_quoted_html() {
        let md = "My reply\n\n-- Best, Alice\n\n{{SIGNATURE}}\n\n> Quoted text";
        let quoted = "<p>Original HTML content</p>";
        let html = markdown_to_html(md, &default_settings(), None, Some(quoted));
        assert_snapshot!(html);
    }

    #[test]
    fn test_markdown_to_html_signature_without_quoted_html() {
        let md = "My reply\n\n-- Best, Alice\n\n{{SIGNATURE}}\n\n> Quoted text";
        let html = markdown_to_html(md, &default_settings(), None, None);
        assert_snapshot!(html);
    }

    #[test]
    fn test_markdown_to_html_tables_and_links() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n\n[Link](https://example.com)\n\n~~strikethrough~~";
        let html = markdown_to_html(md, &default_settings(), None, None);
        assert_snapshot!(html);
    }

    #[test]
    fn test_markdown_to_html_empty_body() {
        let html = markdown_to_html("", &default_settings(), None, None);
        assert_snapshot!(html);
    }

    #[test]
    fn test_markdown_to_html_custom_font() {
        let settings = EmailSettings {
            font_family: "Georgia, serif".to_string(),
            font_size: "14px".to_string(),
            include_signature: true,
            send_hold_secs: 20,
        };
        let html = markdown_to_html("Hello", &settings, None, None);
        assert!(html.contains("Georgia, serif"));
        assert!(html.contains("14px"));
    }

    #[test]
    fn test_markdown_to_html_quoted_font_name_escapes_attribute() {
        let settings = EmailSettings {
            font_family: "\"Times New Roman\", serif".to_string(),
            font_size: "12px".to_string(),
            include_signature: true,
            send_hold_secs: 20,
        };
        let html = markdown_to_html("Hello", &settings, None, None);
        // The inline wrapper's style attribute must not be broken by the raw
        // double quotes in the font name; they are escaped to &quot;.
        assert!(
            html.contains("font-family: &quot;Times New Roman&quot;, serif;"),
            "attribute not escaped: {html}"
        );
        // The raw form survives in the <style> block (not an attribute context).
        assert!(html.contains("font-family: \"Times New Roman\", serif;"));
    }

    // -----------------------------------------------------------------------
    // SendResult methods
    // -----------------------------------------------------------------------

    fn make_result(successes: &[&str], failures: &[&str]) -> SendResult {
        let mut results = Vec::new();
        for addr in successes {
            results.push(RecipientResult {
                address: addr.to_string(),
                role: RecipientRole::To,
                success: true,
                error: None,
                verdict: RecipientVerdict::Delivered,
            });
        }
        for addr in failures {
            results.push(RecipientResult {
                address: addr.to_string(),
                role: RecipientRole::To,
                success: false,
                error: Some("SMTP error".to_string()),
                verdict: RecipientVerdict::Rejected,
            });
        }
        SendResult { results }
    }

    #[test]
    fn test_send_result_all_succeeded() {
        let r = make_result(&["a@example.com", "b@example.com"], &[]);
        assert!(r.all_succeeded());
        assert!(r.any_succeeded());
        assert_eq!(r.succeeded().len(), 2);
        assert!(r.failed().is_empty());
    }

    #[test]
    fn test_send_result_partial_failure() {
        let r = make_result(&["a@example.com"], &["b@example.com"]);
        assert!(!r.all_succeeded());
        assert!(r.any_succeeded());
        assert_eq!(r.succeeded().len(), 1);
        assert_eq!(r.failed().len(), 1);
    }

    #[test]
    fn test_send_result_all_failed() {
        let r = make_result(&[], &["a@example.com", "b@example.com"]);
        assert!(!r.all_succeeded());
        assert!(!r.any_succeeded());
        assert!(r.succeeded().is_empty());
        assert_eq!(r.failed().len(), 2);
    }

    #[test]
    fn test_send_result_empty() {
        let r = SendResult { results: vec![] };
        assert!(r.all_succeeded()); // vacuously true
        assert!(!r.any_succeeded());
    }

    // -----------------------------------------------------------------------
    // Per-recipient verdicts and the admission gate (#0063)
    // -----------------------------------------------------------------------

    fn recipient(address: &str, verdict: RecipientVerdict) -> RecipientResult {
        RecipientResult {
            address: address.to_string(),
            role: RecipientRole::To,
            success: verdict == RecipientVerdict::Delivered,
            error: (verdict != RecipientVerdict::Delivered).then(|| "why".to_string()),
            verdict,
        }
    }

    /// The outbox is handed one verdict per recipient, in the four buckets it
    /// acts on; nothing collapses a mixed result into "accepted".
    #[test]
    fn submit_outcome_hands_the_outbox_one_verdict_per_recipient() {
        let result = SendResult {
            results: vec![
                recipient("took-it@x.com", RecipientVerdict::Delivered),
                recipient("refused@x.com", RecipientVerdict::Rejected),
                recipient("later@x.com", RecipientVerdict::Retryable),
                recipient("silent@x.com", RecipientVerdict::Unknown),
            ],
        };
        let crate::outbox::SubmitOutcome::PerRecipient(verdicts) = result.submit_outcome() else {
            panic!("an SMTP result is always per recipient");
        };
        assert_eq!(verdicts.delivered, vec!["took-it@x.com".to_string()]);
        assert_eq!(verdicts.rejected[0].0, "refused@x.com");
        assert_eq!(verdicts.retryable[0].0, "later@x.com");
        assert_eq!(verdicts.ambiguous[0].0, "silent@x.com");
        assert!(result.results[3].ambiguous());
    }

    /// The status line of a send that reached some recipients says so, rather
    /// than reporting the outbox row's state as an unqualified success.
    #[test]
    fn a_partial_send_says_so_in_its_status_line() {
        let report = SendReport {
            send_result: SendResult {
                results: vec![
                    recipient("took-it@x.com", RecipientVerdict::Delivered),
                    recipient("refused@x.com", RecipientVerdict::Rejected),
                ],
            },
            state: Some(crate::outbox::OutboxState::Done),
            row_id: Some(1),
        };
        assert_eq!(report.status_line(), "partly delivered, see `mp outbox list`");
    }

    fn draft_with(id: Option<&str>, path: &str) -> EmailDraft {
        EmailDraft {
            path: std::path::PathBuf::from(path),
            frontmatter: crate::types::EmailFrontmatter {
                id: id.map(|s| s.to_string()),
                date: None,
                to: Some("bob@example.com".to_string()),
                cc: None,
                bcc: None,
                subject: "Hello".to_string(),
                status: EmailStatus::Approved,
                from: None,
                reply_to: None,
                attachments: None,
                sent_at: None,
                sent_via: None,
                message_id: None,
                in_reply_to: None,
                forwarded_from: None,
                signature: None,
                event: None,
            },
            body_markdown: String::new(),
        }
    }

    /// The second status axis is written from the draft that caused it: a
    /// reply names its source in `in_reply_to:`, a forward in
    /// `forwarded_from:`, and an ordinary draft names nothing (#TKT-0051).
    #[test]
    fn only_a_reply_or_a_forward_has_a_source_to_flag() {
        let mut reply = draft_with(None, "/drafts/r.md");
        reply.frontmatter.in_reply_to = Some("<src@example.com>".to_string());
        assert_eq!(source_to_flag(&reply), Some(("<src@example.com>", true)));

        let mut forward = draft_with(None, "/drafts/f.md");
        forward.frontmatter.forwarded_from = Some(" <src@example.com> ".to_string());
        assert_eq!(source_to_flag(&forward), Some(("<src@example.com>", false)));

        assert_eq!(source_to_flag(&draft_with(None, "/drafts/plain.md")), None);

        let mut blank = draft_with(None, "/drafts/blank.md");
        blank.frontmatter.in_reply_to = Some("   ".to_string());
        assert_eq!(source_to_flag(&blank), None);
    }

    /// The local half of the post-send hook flags *every* copy of the source,
    /// because one message is one row per mailbox and the archived copy is as
    /// likely to be the one on screen (#TKT-0051).
    #[test]
    fn sending_a_reply_flags_every_local_copy_of_its_source() {
        let fx = crate::reconcile::tests::fixture();
        let inbox = fx.ingest_plain("inbox", 7, "Question");
        // The same Message-ID filed in a second mailbox: `ingest_plain` keys
        // its id on `(mailbox, uid)`, so the archive copy is ingested under the
        // inbox row's identity by hand.
        fx.store
            .conn()
            .execute(
                "INSERT INTO messages (account, mailbox, uid, message_id, subject)
                 SELECT account, 'archive', 99, message_id, subject FROM messages WHERE id = ?1",
                [inbox],
            )
            .unwrap();

        let message_id = crate::store::read::find_by_id(&fx.store, inbox)
            .unwrap()
            .unwrap()
            .message_id;
        let rows =
            crate::store::read::find_by_message_id(&fx.store, "alice", &message_id).unwrap();
        let mailboxes = server_mailboxes_of(&crate::config::AccountConfig::default(), &rows);
        let outcome = crate::pending_ops::apply_post_send_flag(
            &fx.store,
            "alice",
            &message_id,
            true,
            &mailboxes,
        )
        .unwrap();

        assert_eq!(outcome.rows, 2, "both copies are found by Message-ID");
        for row in crate::store::read::find_by_message_id(&fx.store, "alice", &message_id).unwrap() {
            assert!(row.is_answered(), "{} was not flagged", row.mailbox);
            assert!(!row.is_forwarded());
        }
        // Two mailboxes, one queued op: the whole point of #0076 is that the
        // server half is one multi-mailbox op rather than one session each.
        let queued = crate::pending_ops::queued_ops(&fx.store, "alice").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].kind, "set_answered");
        match &queued[0].op {
            crate::ops::ServerOp::SetAnswered { mailboxes, .. } => {
                assert_eq!(mailboxes.len(), 2, "both server folders ride one op");
            }
            other => panic!("unexpected op {other:?}"),
        }
    }

    /// The distinct server folders are what ride the op: two local rows in the
    /// same server folder are one `SELECT`, not two.
    #[test]
    fn the_server_mailbox_list_is_deduplicated() {
        let fx = crate::reconcile::tests::fixture();
        let a = fx.ingest_plain("inbox", 1, "One");
        let b = fx.ingest_plain("inbox", 2, "Two");
        let rows: Vec<_> = [a, b]
            .into_iter()
            .map(|id| crate::store::read::find_by_id(&fx.store, id).unwrap().unwrap())
            .collect();
        let mailboxes = server_mailboxes_of(&crate::config::AccountConfig::default(), &rows);
        assert_eq!(mailboxes.len(), 1, "one server folder, one SELECT");
    }

    /// A forward sets the other bit, and neither write disturbs the read bit
    /// the row already carried.
    #[test]
    fn forwarding_sets_the_forwarded_bit_and_leaves_the_read_bit_alone() {
        let fx = crate::reconcile::tests::fixture();
        let id = fx.ingest_plain("inbox", 3, "Passing this on");
        crate::store::write::set_read(&fx.store, id, true).unwrap();

        let message_id = crate::store::read::find_by_id(&fx.store, id)
            .unwrap()
            .unwrap()
            .message_id;
        crate::pending_ops::apply_post_send_flag(&fx.store, "alice", &message_id, false, &[])
            .unwrap();

        let row = crate::store::read::find_by_id(&fx.store, id).unwrap().unwrap();
        assert!(row.is_forwarded());
        assert!(!row.is_answered());
        assert!(row.is_read(), "the read bit survives a history write");
        assert!(
            crate::pending_ops::queued_ops(&fx.store, "alice").unwrap().is_empty(),
            "no server mailboxes (a Graph account) queues nothing"
        );
    }

    /// A source the store does not hold flags nothing and says so, which is
    /// what keeps the hook silent for a reply to a message that has since been
    /// deleted.
    #[test]
    fn a_source_the_store_does_not_hold_flags_nothing() {
        let fx = crate::reconcile::tests::fixture();
        let outcome = crate::pending_ops::apply_post_send_flag(
            &fx.store,
            "alice",
            "<gone@example.com>",
            true,
            &["INBOX".to_string()],
        )
        .unwrap();
        assert_eq!(outcome.rows, 0);
        assert_eq!(outcome.op_id, None, "nothing local means nothing owed");
        assert!(crate::pending_ops::queued_ops(&fx.store, "alice").unwrap().is_empty());
    }

    /// The key is the frontmatter id when there is one, because that is what
    /// survives the rename a send performs.
    #[test]
    fn a_draft_is_keyed_by_its_id_and_falls_back_to_its_path() {
        let with_id = draft_with(Some("2026-08-06-note"), "/drafts/note.md");
        assert_eq!(draft_key(&with_id), "id:2026-08-06-note");
        let renamed = draft_with(Some("2026-08-06-note"), "/drafts/renamed.md");
        assert_eq!(draft_key(&renamed), draft_key(&with_id));
        let anonymous = draft_with(None, "/drafts/agent-wrote-this.md");
        assert_eq!(draft_key(&anonymous), "path:/drafts/agent-wrote-this.md");
    }

    /// The cheap half of the admission gate: two threads reaching `send_draft`
    /// for one draft, which is what the TUI does when the cursor draft is also
    /// in the approved batch.
    #[test]
    fn one_draft_admits_one_send_at_a_time() {
        let key = "id:only-once";
        let first = SendAdmission::claim("work", key).expect("the first send is admitted");
        assert!(
            SendAdmission::claim("work", key).is_none(),
            "a second send of the same draft is refused while the first runs"
        );
        assert!(
            SendAdmission::claim("work", "id:another").is_some(),
            "a different draft is not blocked"
        );
        assert!(
            SendAdmission::claim("personal", key).is_some(),
            "the same id on another account is another message, not a duplicate"
        );
        drop(first);
        assert!(
            SendAdmission::claim("work", key).is_some(),
            "the slot is released however the send returned"
        );
    }

    /// The `from:` that reaches SMTP is the one the build validated. An
    /// unquoted comma in the display name parses nowhere, so a draft carrying
    /// one used to enqueue and then fail every submission it would ever get
    /// (#0063 review).
    #[test]
    fn a_from_with_a_comma_in_the_display_name_survives_the_build() {
        let mut draft = draft_with(Some("comma-from"), "/drafts/comma.md");
        draft.frontmatter.from = Some("Doe, Jane <jane@example.com>".to_string());
        let built = build_draft_message(
            &draft,
            "fallback@example.com",
            &EmailSettings::default(),
            None,
            None,
        )
        .expect("the build accepts a display name it can quote");
        assert_eq!(built.from, "\"Doe, Jane\" <jane@example.com>");
        // What `submit` does with it before it opens a connection.
        let mailbox: Mailbox = built
            .from
            .parse()
            .expect("the stored from address parses on the submission path");
        assert_eq!(mailbox.email.to_string(), "jane@example.com");
    }

    /// The same trap on the RSVP path: an account whose address carries a
    /// display name with a comma builds a reply whose `from:` must still parse
    /// where `submit` derives `MAIL FROM` from it (#0063 review, second half).
    #[test]
    fn an_rsvp_reply_carries_the_from_it_validated() {
        let built = build_reply_message(
            "Doe, Jane <jane@example.com>",
            "organizer@example.com",
            "Accepted: Planning",
            "Accepted.",
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
        )
        .expect("the build accepts a display name it can quote");
        assert_eq!(built.from, "\"Doe, Jane\" <jane@example.com>");
        let mailbox: Mailbox = built
            .from
            .parse()
            .expect("the stored from address parses on the submission path");
        assert_eq!(mailbox.email.to_string(), "jane@example.com");
    }

    // -----------------------------------------------------------------------
    // strip_signature_marker (#0102)
    // -----------------------------------------------------------------------

    /// The marker is consumed by the HTML path and substituted in the TUI
    /// preview, but the `text/plain` alternative used to ship it verbatim. It
    /// must leave no trace, and the blank lines it padded collapse to a single
    /// paragraph break rather than a double one.
    #[test]
    fn strip_signature_marker_collapses_the_blank_lines_it_padded() {
        let body = "My reply\n\n-- \nBest, Alice\n\n{{SIGNATURE}}\n\nOn Mon wrote:\n> Quoted";
        let plain = strip_signature_marker(body);
        assert!(!plain.contains("{{SIGNATURE}}"), "marker survived: {plain:?}");
        // The signature Markdown (spliced in at draft time, #0099) stays; only
        // the marker and its padding go.
        assert_eq!(
            plain,
            "My reply\n\n-- \nBest, Alice\n\nOn Mon wrote:\n> Quoted"
        );
        assert!(!plain.contains("\n\n\n"), "double blank line left behind: {plain:?}");
    }

    /// The signature sentinels (#0106) never reach a recipient: the plain part
    /// drops the marker lines and keeps the signature content, and the HTML
    /// part carries no raw `<!-- mp:sig-... -->` comment.
    #[test]
    fn signature_sentinels_are_stripped_from_both_send_parts() {
        let body =
            "My note\n\n<!-- mp:sig-start -->\nBest,\nAlice\n<!-- mp:sig-end -->\n";
        let plain = plain_text_body(body);
        assert!(!plain.contains("mp:sig-start"), "sentinel in plain: {plain:?}");
        assert!(!plain.contains("mp:sig-end"), "sentinel in plain: {plain:?}");
        assert!(plain.contains("Best,"), "signature dropped from plain: {plain:?}");
        assert!(plain.contains("Alice"), "signature dropped from plain: {plain:?}");

        let html = markdown_to_html(body, &default_settings(), None, None);
        assert!(!html.contains("mp:sig-start"), "sentinel in html: {html}");
        assert!(!html.contains("mp:sig-end"), "sentinel in html: {html}");
        // The signature content still renders, on its own line (hard break).
        assert!(html.contains("Alice"), "signature dropped from html: {html}");
    }

    /// A body with no marker is returned untouched, so the non-reply send
    /// paths keep byte-for-byte identical plain text.
    #[test]
    fn strip_signature_marker_is_a_no_op_without_the_marker() {
        let body = "Just a plain note.\n\nSecond paragraph.\n";
        assert_eq!(strip_signature_marker(body), body);
    }

    /// The reply/forward plain part built by `build_draft_message` carries no
    /// `{{SIGNATURE}}` literal, on either the SMTP body or the invite path,
    /// while the reply text and the quoted original still ride along.
    #[test]
    fn a_sent_reply_plain_part_carries_no_signature_marker() {
        let mut draft = draft_with(Some("reply-marker"), "/drafts/reply.md");
        draft.body_markdown =
            "My reply\n\n\n-- \nBest, Alice\n\n{{SIGNATURE}}\n\nOn Mon, Alice wrote:\n> Original text"
                .to_string();

        // Non-invite path.
        let built = build_draft_message(
            &draft,
            "fallback@example.com",
            &EmailSettings::default(),
            None,
            None,
        )
        .expect("the reply draft builds");
        let raw = String::from_utf8_lossy(&built.raw);
        assert!(!raw.contains("{{SIGNATURE}}"), "marker leaked into the message bytes");
        assert!(raw.contains("My reply"), "reply text missing from the message");
        assert!(raw.contains("Original text"), "quoted text missing from the message");

        // Invite path: the same body feeds the invite alternative's plain part.
        let invited = build_draft_message(
            &draft,
            "fallback@example.com",
            &EmailSettings::default(),
            None,
            Some("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
        )
        .expect("the invite reply draft builds");
        let invited_raw = String::from_utf8_lossy(&invited.raw);
        assert!(
            !invited_raw.contains("{{SIGNATURE}}"),
            "marker leaked into the invite message bytes"
        );
    }

    /// The unified Markdown signature (spliced into the reply body at draft
    /// creation, #0099) is the single source for both outgoing parts: the
    /// plain part carries it once (as Markdown link syntax) with no marker, and
    /// the HTML part carries it once as a rendered anchor. No send-time HTML
    /// injection, so it can no longer appear twice.
    #[test]
    fn a_reply_carries_the_signature_once_in_each_part() {
        let settings = EmailSettings::default();
        let body =
            "My reply\n\n[Robin](mailto:robin@example.com)\n\n{{SIGNATURE}}\n\nOn Mon wrote:\n> Quoted";

        // Plain part: marker gone, signature exactly once.
        let plain = strip_signature_marker(body);
        assert!(!plain.contains("{{SIGNATURE}}"), "marker in plain part: {plain:?}");
        assert_eq!(
            plain.matches("mailto:robin@example.com").count(),
            1,
            "signature not exactly once in plain part: {plain:?}"
        );

        // HTML part: marker gone, no raw spliced `<` from the source, signature
        // exactly once as a rendered anchor.
        let html = markdown_to_html(body, &settings, None, None);
        assert!(!html.contains("{{SIGNATURE}}"), "marker in HTML part");
        assert_eq!(
            html.matches("mailto:robin@example.com").count(),
            1,
            "signature not exactly once in HTML part: {html}"
        );
        assert!(
            html.contains(r#"<a href="mailto:robin@example.com">Robin</a>"#),
            "signature link not rendered as an anchor: {html}"
        );
    }

    // -----------------------------------------------------------------------
    // RecipientRole display
    // -----------------------------------------------------------------------

    #[test]
    fn test_recipient_role_display() {
        assert_eq!(format!("{}", RecipientRole::To), "To");
        assert_eq!(format!("{}", RecipientRole::Cc), "Cc");
        assert_eq!(format!("{}", RecipientRole::Bcc), "Bcc");
    }

    // -----------------------------------------------------------------------
    // split_addresses
    // -----------------------------------------------------------------------

    #[test]
    fn split_addresses_simple() {
        let r = split_addresses("alice@x.com, bob@x.com");
        assert_eq!(r, vec!["alice@x.com", "bob@x.com"]);
    }

    #[test]
    fn split_addresses_quoted_comma_in_display_name() {
        let r = split_addresses("\"Doe, Jane\" <jane@example.com>, bob@x.com");
        assert_eq!(r, vec!["\"Doe, Jane\" <jane@example.com>", "bob@x.com"]);
    }

    #[test]
    fn split_addresses_single_quoted_name() {
        let r = split_addresses("\"Doe, Jane\" <jane@x.com>");
        assert_eq!(r, vec!["\"Doe, Jane\" <jane@x.com>"]);
    }

    #[test]
    fn split_addresses_empty() {
        let r = split_addresses("");
        assert!(r.is_empty());
    }

    #[test]
    fn split_addresses_whitespace_only() {
        let r = split_addresses("   ");
        assert!(r.is_empty());
    }

    #[test]
    fn split_addresses_lettre_parses_quoted_name() {
        use lettre::message::Mailbox;
        let addr = "\"Doe, Jane\" <jane@example.com>";
        let mbox: Mailbox = addr.parse().expect("lettre should parse quoted display name");
        assert_eq!(mbox.email.to_string(), "jane@example.com");
    }

    // -----------------------------------------------------------------------
    // normalize_address_for_smtp
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_quotes_brackets_in_display_name() {
        // Real-world TUM mailing list: square brackets are not RFC 5322 atext.
        let raw = "CCBE_Researchers [TUBVCMS] <researchers.ccbe@ed.tum.de>";
        let normalized = normalize_address_for_smtp(raw);
        assert_eq!(
            normalized,
            "\"CCBE_Researchers [TUBVCMS]\" <researchers.ccbe@ed.tum.de>"
        );
        let _: lettre::message::Mailbox =
            normalized.parse().expect("lettre must parse normalized form");
    }

    #[test]
    fn normalize_leaves_already_quoted_name_untouched() {
        let raw = "\"Doe, Jane\" <jane@x.com>";
        assert_eq!(normalize_address_for_smtp(raw), raw);
    }

    #[test]
    fn normalize_leaves_atext_only_name_untouched() {
        let raw = "Alice Smith <alice@x.com>";
        assert_eq!(normalize_address_for_smtp(raw), raw);
    }

    #[test]
    fn normalize_leaves_bare_address_untouched() {
        assert_eq!(normalize_address_for_smtp("bob@x.com"), "bob@x.com");
    }

    #[test]
    fn normalize_quotes_unquoted_comma_in_display_name() {
        // "Doe, Jane <j@x>" -- one entry, but the comma inside the
        // display name was not quoted. After splitting (which will treat
        // the whole thing as one because there's no separating comma between
        // entries), normalization must quote it so lettre accepts it.
        let raw = "Doe, Jane <jane@example.com>";
        let normalized = normalize_address_for_smtp(raw);
        assert_eq!(
            normalized,
            "\"Doe, Jane\" <jane@example.com>"
        );
        let _: lettre::message::Mailbox =
            normalized.parse().expect("lettre must parse normalized form");
    }

    #[test]
    fn normalize_extracts_email_via_mailbox_for_envelope() {
        // Regression for "Partial: 1/2 succeeded": `submit`'s per-recipient
        // RCPT TO loop parses the address again to extract `mbox.email` for
        // the SMTP envelope. If we forget to normalize there, lettre rejects
        // bracketed display names and that recipient silently fails while
        // the other one goes through.
        let raw = "CCBE_Researchers [TUBVCMS] <researchers.ccbe@ed.tum.de>";
        let mbox: lettre::message::Mailbox = normalize_address_for_smtp(raw)
            .parse()
            .expect("lettre must parse normalized form for envelope");
        assert_eq!(mbox.email.to_string(), "researchers.ccbe@ed.tum.de");
    }

    #[test]
    fn normalize_escapes_inner_quotes_and_backslashes() {
        let raw = "Weird \\ \"name\" <w@x.com>";
        let normalized = normalize_address_for_smtp(raw);
        // backslash and inner double-quotes must be escaped inside the
        // resulting quoted-string.
        assert_eq!(
            normalized,
            "\"Weird \\\\ \\\"name\\\"\" <w@x.com>"
        );
        let _: lettre::message::Mailbox =
            normalized.parse().expect("lettre must parse escaped form");
    }

    // -----------------------------------------------------------------------
    // format_recipient (Contacts view seed sites, #0033)
    // -----------------------------------------------------------------------

    #[test]
    fn format_recipient_plain_name_passthrough() {
        assert_eq!(
            format_recipient("Alice Smith", "alice@x.com"),
            "Alice Smith <alice@x.com>"
        );
    }

    #[test]
    fn format_recipient_quotes_comma_name() {
        assert_eq!(
            format_recipient("Doe, John", "john@x.com"),
            "\"Doe, John\" <john@x.com>"
        );
    }

    #[test]
    fn format_recipient_escapes_quote_and_backslash() {
        assert_eq!(
            format_recipient("Weird \\ \"name\"", "w@x.com"),
            "\"Weird \\\\ \\\"name\\\"\" <w@x.com>"
        );
    }

    #[test]
    fn format_recipient_empty_name_is_bare_address() {
        assert_eq!(format_recipient("", "bob@x.com"), "bob@x.com");
        // Whitespace-only display name is treated as empty.
        assert_eq!(format_recipient("   ", "bob@x.com"), "bob@x.com");
    }

    #[test]
    fn format_recipient_survives_split_addresses_as_one() {
        // The whole point: a "Last, First" contact name must not be split into
        // two broken recipients by split_addresses (which runs before
        // normalize_address_for_smtp on the send path).
        let seeded = format_recipient("Doe, John", "john@example.com");
        let parts = split_addresses(&seeded);
        assert_eq!(parts, vec!["\"Doe, John\" <john@example.com>"]);
        // And the single recipient parses as a proper lettre Mailbox.
        let mbox: lettre::message::Mailbox = normalize_address_for_smtp(&parts[0])
            .parse()
            .expect("lettre must parse the seeded recipient");
        assert_eq!(mbox.email.to_string(), "john@example.com");
    }

    // -----------------------------------------------------------------------
    // markdown_to_html edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_markdown_to_html_signature_appended_without_placeholder() {
        // Send-time signature (invites, direct sends) is Markdown; it is
        // appended to the body Markdown and rendered through the same converter
        // so it inherits the body font.
        let sig = "-- Best, Alice";
        let html = markdown_to_html("Hello world", &default_settings(), Some(sig), None);
        // Without placeholder, signature is appended after the body
        assert!(html.contains("<p>Hello world</p>"));
        // The signature is rendered from Markdown (its own <p>), not injected raw.
        assert!(html.contains("<p>-- Best, Alice</p>"));
    }

    #[test]
    fn test_markdown_to_html_no_signature() {
        let html = markdown_to_html("Hello", &default_settings(), None, None);
        assert!(html.contains("<p>Hello</p>"));
        // Should still be valid HTML
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn test_markdown_to_html_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let html = markdown_to_html(md, &default_settings(), None, None);
        assert!(html.contains("<code"));
        assert!(html.contains("fn main()"));
    }

    #[test]
    fn test_markdown_to_html_with_quoted_html_replaces_blockquotes() {
        let md = "Reply\n\nsig\n\n{{SIGNATURE}}\n\n> original";
        let quoted = "<p>Original HTML</p>";
        let html = markdown_to_html(md, &default_settings(), None, Some(quoted));
        // When quoted_html is provided, it should be used instead of markdown blockquotes
        assert!(html.contains("Original HTML"));
        assert!(html.contains("sig"));
    }
}

/// Normalize a single address so lettre's strict RFC 5322 `Mailbox` parser
/// accepts it.
///
/// Many real-world senders ship `Display Name <user@host>` headers where the
/// display name contains characters that are not RFC 5322 `atext` (e.g.
/// `[`, `]`, `:`, `;`, `(`, `)`, `,`). The fix is to wrap such display names
/// in a quoted-string. We only touch the display name; the address part is
/// left as-is.
///
/// Examples:
/// - `CCBE_Researchers [TUBVCMS] <r@x>` → `"CCBE_Researchers [TUBVCMS]" <r@x>`
/// - `"Doe, Jane" <j@x>` → unchanged (already quoted)
/// - `Alice <a@x>` → unchanged (atext-only display name)
/// - `bob@x.com` → unchanged (no display name)
pub fn normalize_address_for_smtp(addr: &str) -> String {
    let trimmed = addr.trim();
    let (open, close) = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(o), Some(c)) if o < c => (o, c),
        _ => return trimmed.to_string(),
    };

    let name_part = trimmed[..open].trim();
    let email_part = trimmed[open + 1..close].trim();

    if name_part.is_empty() {
        return format!("<{}>", email_part);
    }

    // Already a single quoted-string spanning the whole display name -- keep.
    if name_part.len() >= 2 && name_part.starts_with('"') && name_part.ends_with('"') {
        return format!("{} <{}>", name_part, email_part);
    }

    format!("{} <{}>", quote_display_name(name_part), email_part)
}

/// Return an RFC 5322 `display-name` for `name`: the bare name if it is made
/// entirely of atext + FWS, otherwise wrapped in a quoted-string with `"` and
/// `\` escaped. Shared by [`normalize_address_for_smtp`] and
/// [`format_recipient`] so the quoting rule lives in exactly one place.
fn quote_display_name(name: &str) -> String {
    // RFC 5322 atext, plus FWS (space/tab) and `.` (allowed in dot-atom phrases).
    fn is_atext_or_fws(c: char) -> bool {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '!' | '#'
                    | '$'
                    | '%'
                    | '&'
                    | '\''
                    | '*'
                    | '+'
                    | '-'
                    | '/'
                    | '='
                    | '?'
                    | '^'
                    | '_'
                    | '`'
                    | '{'
                    | '|'
                    | '}'
                    | '~'
                    | '.'
                    | ' '
                    | '\t'
            )
    }

    if name.chars().all(is_atext_or_fws) {
        return name.to_string();
    }

    // Quote it. Escape backslashes and double quotes per RFC 5322 quoted-string.
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

/// Build a single RFC 5322 recipient string from a display name and address.
///
/// Emits `Name <addr>` with the display name wrapped in a quoted-string when it
/// contains characters outside atext + FWS (e.g. the `,` in a `"Last, First"`
/// contact name), so the result survives [`split_addresses`] as ONE recipient
/// and parses cleanly. When `display_name` is empty (after trimming) the bare
/// address is returned.
pub fn format_recipient(display_name: &str, address: &str) -> String {
    let name = display_name.trim();
    if name.is_empty() {
        return address.to_string();
    }
    format!("{} <{}>", quote_display_name(name), address)
}

/// Split a comma-separated address list respecting quoted display names.
/// e.g. `"Doe, Jane" <jane@x.com>, bob@x.com` → two entries, not three.
pub fn split_addresses(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in s.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ',' if !in_quotes => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    parts.push(trimmed);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
    parts
}

/// Build the `multipart/alternative` for an iMIP invite: `text/plain`,
/// `text/html`, and the inline `text/calendar; method=REQUEST; charset=UTF-8`
/// part carrying the `VEVENT`. This inline calendar part is the contract
/// (`docs/plans/calendar-invites.md`, §2) — Outlook and Gmail render it as an
/// actionable Accept / Tentative / Decline invite.
fn build_invite_alternative(plain: &str, html: String, ics: &str) -> MultiPart {
    let calendar_ct: ContentType = "text/calendar; method=REQUEST; charset=UTF-8"
        .parse()
        .expect("static calendar content-type");
    MultiPart::alternative()
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(plain.to_string()),
        )
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(html),
        )
        .singlepart(
            SinglePart::builder()
                .header(calendar_ct)
                .body(ics.to_string()),
        )
}

/// Build the optional `application/ics; name="invite.ics"` attachment carrying
/// the same `.ics` bytes as the inline part. Optional hardening (design §2):
/// ship it in v1, drop it if live testing shows duplicate rendering.
fn build_ics_attachment(ics: &str) -> SinglePart {
    let ct: ContentType = "application/ics; name=\"invite.ics\""
        .parse()
        .expect("static ics attachment content-type");
    Attachment::new("invite.ics".to_string()).body(ics.to_string(), ct)
}

/// Assemble the full iMIP invite body:
///
/// ```text
/// multipart/mixed
/// ├── multipart/alternative
/// │   ├── text/plain
/// │   ├── text/html
/// │   └── text/calendar; method=REQUEST; charset=UTF-8   (the contract)
/// └── application/ics; name="invite.ics"                 (optional hardening)
/// ```
///
/// Shared by the live send path and the round-trip integration test so both
/// exercise the identical MIME shape. File attachments (if any) are appended by
/// the caller after this base.
pub fn build_invite_mime_body(plain: &str, html: String, ics: &str) -> MultiPart {
    MultiPart::mixed()
        .multipart(build_invite_alternative(plain, html, ics))
        .singlepart(build_ics_attachment(ics))
}

/// Build the SMTP transport (implicit TLS on :465, STARTTLS otherwise),
/// honouring the account's OAuth2/password auth and `accept_invalid_certs`
/// opt-in. The single transport builder behind [`submit`], so every SMTP
/// submission applies identical transport policy.
fn build_smtp_transport(
    smtp_config: &SmtpConfig,
) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    if smtp_config.accept_invalid_certs {
        crate::config::ensure_invalid_certs_allowed(&smtp_config.host)?;
    }
    let creds = Credentials::new(smtp_config.username.clone(), smtp_config.password.clone());

    let mailer: AsyncSmtpTransport<Tokio1Executor> = if smtp_config.port == 465 {
        // Implicit TLS (SMTPS)
        let mut transport = AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp_config.host)?;
        if smtp_config.accept_invalid_certs {
            let tls_params = lettre::transport::smtp::client::TlsParameters::builder(smtp_config.host.clone())
                .dangerous_accept_invalid_certs(true)
                .build()?;
            transport = transport.tls(lettre::transport::smtp::client::Tls::Wrapper(tls_params));
        }
        let transport = transport.port(smtp_config.port).credentials(creds);
        if smtp_config.auth_method == AuthMethod::OAuth2 {
            transport
                .authentication(vec![lettre::transport::smtp::authentication::Mechanism::Xoauth2])
                .build()
        } else {
            transport.build()
        }
    } else {
        // STARTTLS
        let mut transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp_config.host)?;
        if smtp_config.accept_invalid_certs {
            let tls_params = lettre::transport::smtp::client::TlsParameters::builder(smtp_config.host.clone())
                .dangerous_accept_invalid_certs(true)
                .build()?;
            transport = transport.tls(lettre::transport::smtp::client::Tls::Required(tls_params));
        }
        let transport = transport.port(smtp_config.port).credentials(creds);
        if smtp_config.auth_method == AuthMethod::OAuth2 {
            transport
                .authentication(vec![lettre::transport::smtp::authentication::Mechanism::Xoauth2])
                .build()
        } else {
            transport.build()
        }
    };
    Ok(mailer)
}

/// Build the `multipart/alternative` body of an iMIP RSVP `REPLY` (#0029):
/// a minimal `text/plain` human note plus the inline
/// `text/calendar; method=REPLY; charset=UTF-8` part carrying the responding
/// `ATTENDEE`'s `PARTSTAT`. No `text/html` and no `.ics` attachment: a REPLY
/// is machine-consumed by the organizer's client, so the alternative stays
/// lean (mirrors what Gmail/Outlook emit).
pub fn build_reply_mime_body(plain: &str, ics: &str) -> MultiPart {
    let calendar_ct: ContentType = "text/calendar; method=REPLY; charset=UTF-8"
        .parse()
        .expect("static calendar content-type");
    MultiPart::alternative()
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(plain.to_string()),
        )
        .singlepart(
            SinglePart::builder()
                .header(calendar_ct)
                .body(ics.to_string()),
        )
}

/// Send an iMIP RSVP `REPLY` to a received invitation's `ORGANIZER`.
///
/// `from` is the responding account's primary address (also the REPLY
/// `ATTENDEE`); `organizer` is the single recipient. `reply_ics` is the
/// `METHOD:REPLY` payload from [`crate::invite::build_reply_ics`]. `subject`
/// follows the Outlook convention (`Accepted:`/`Tentative:`/`Declined: <summary>`).
/// The reply is built here and submitted through the durable outbox by
/// [`send_rsvp`], like any other outgoing message.
pub fn build_reply_message(
    from: &str,
    organizer: &str,
    subject: &str,
    plain_body: &str,
    reply_ics: &str,
) -> Result<BuiltMessage> {
    // Normalised once and carried on the built message, the twin of what
    // `build_draft_message` does: `submit` parses `BuiltMessage::from` to
    // derive `MAIL FROM`, so a raw `Doe, Jane <j@x.com>` stored here would
    // pass the build and then fail every submission it would ever get
    // (#0063 review).
    let normalized_from = normalize_address_for_smtp(from);
    let from_mailbox: Mailbox = normalized_from
        .parse()
        .context("Invalid 'from' address for RSVP reply")?;
    let organizer_mailbox: Mailbox = normalize_address_for_smtp(organizer)
        .parse()
        .context("Invalid ORGANIZER address for RSVP reply")?;

    info!("Building RSVP reply: subject=\"{}\", from={}, to={}", subject, from, organizer);

    let message = Message::builder()
        .from(from_mailbox.clone())
        .to(organizer_mailbox.clone())
        .subject(subject)
        .message_id(None)
        .multipart(build_reply_mime_body(plain_body, reply_ics))
        .context("Failed to build RSVP reply message")?;

    let raw_message = message.formatted();
    Ok(BuiltMessage {
        message_id: message_id_of(&raw_message),
        raw: raw_message,
        recipients: vec![(organizer.to_string(), RecipientRole::To)],
        from: normalized_from,
        // An RSVP is built from an invitation, not from a draft file, and
        // there is no second copy of it to press send on.
        draft_key: None,
    })
}

/// Outcome of an RSVP send, carried back to the caller so it can surface a
/// status line. The Sent copy is the outbox's business, not the caller's.
pub struct RsvpOutcome {
    /// Where the reply's outbox row ended up, or `None` when the store could
    /// not be opened and the reply was submitted without a durable record.
    pub outbox_state: Option<crate::outbox::OutboxState>,
    pub send_result: SendResult,
    pub raw_message: Vec<u8>,
    pub message_id: Option<String>,
    /// The `ORGANIZER` the reply was sent to.
    pub organizer: String,
    /// The Outlook-convention subject used (`Accepted: <summary>` etc.).
    pub subject: String,
}

/// End-to-end attendee RSVP to a received invite (#0029), shared by the CLI
/// and the TUI so both run identical logic (no subprocess).
///
/// Steps: build a `METHOD:REPLY` from the invitation's own iMIP payload (the
/// source of truth for `UID`/`SEQUENCE`) carrying the account's `PARTSTAT`,
/// and email it to the `ORGANIZER`. The payload itself is never rewritten.
///
/// Nothing local is flipped afterwards, and there is nothing to flip: the
/// reply goes out through the durable outbox, which appends it to the server
/// Sent mailbox and ingests that copy into the store during the send, so our
/// own `PARTSTAT` is derived from that row the next time an invite is
/// rendered (#0038 scope item 6, `crate::reconcile::own_rsvp`).
///
/// `ics` is the invitation's `invite.ics` bytes, read from the message's blob
/// by the caller; `account_address` is the responding account's primary
/// address (the REPLY `ATTENDEE`).
pub async fn send_rsvp(
    ics: &[u8],
    account_config: &crate::config::AccountConfig,
    account_address: &str,
    rsvp: crate::invite::Rsvp,
    smtp_config: &SmtpConfig,
) -> Result<RsvpOutcome> {
    let account_address = crate::parse::extract_email_address(account_address);
    if account_address.is_empty() {
        return Err(anyhow!("Account has no usable address to RSVP as"));
    }

    let ctx = crate::invite::reply_context_from_ics(ics)?;
    let reply_ics = crate::invite::build_reply_ics(&ctx, &account_address, rsvp)?;

    let summary = ctx.summary.as_deref().unwrap_or("(no subject)");
    let subject = format!("{}: {}", rsvp.subject_verb(), summary);
    let plain_body = format!(
        "{} the invitation: {}",
        rsvp.subject_verb(),
        summary
    );

    let built = build_reply_message(
        &account_address,
        &ctx.organizer,
        &subject,
        &plain_body,
        &reply_ics,
    )?;

    let report = send_durably(&built, account_config, smtp_config).await?;
    let send_result = report.send_result;
    let raw_message = built.raw;
    let message_id = Some(built.message_id);

    Ok(RsvpOutcome {
        outbox_state: report.state,
        send_result,
        raw_message,
        message_id,
        organizer: ctx.organizer,
        subject,
    })
}

// ---------------------------------------------------------------------------
// The durable send path (#0037 item 5)
// ---------------------------------------------------------------------------

/// What one durable send did, end to end.
pub struct SendReport {
    /// The SMTP (or Graph) result, per recipient.
    pub send_result: SendResult,
    /// Where the outbox row ended up. `None` when the store could not be
    /// opened at all, in which case the message was still submitted but has no
    /// durable record.
    pub state: Option<crate::outbox::OutboxState>,
    /// The outbox row id, for a status line or a later retry.
    pub row_id: Option<i64>,
}

impl SendReport {
    /// One honest line about where the message actually is, for `mp send` and
    /// the TUI status bar.
    pub fn status_line(&self) -> String {
        use crate::outbox::OutboxState;
        // A message some recipients never got is the one thing worth saying
        // before where the copy is (#0063): the outbox row keeps the detail.
        if !self.send_result.failed().is_empty() && self.send_result.any_succeeded() {
            return "partly delivered, see `mp outbox list`".to_string();
        }
        match self.state {
            Some(OutboxState::Done) => "sent + saved".to_string(),
            Some(OutboxState::SentPendingAppend) => "sent + append pending".to_string(),
            Some(OutboxState::Failed) => "failed (see the outbox)".to_string(),
            Some(OutboxState::PendingSend) => "queued, not sent".to_string(),
            None => {
                if self.send_result.any_succeeded() {
                    "sent (no local record)".to_string()
                } else {
                    "not sent".to_string()
                }
            }
        }
    }
}

/// A submission in flight: the outbox row exists, SMTP has not run yet.
///
/// Held by the caller across the submission so the Graph path and the SMTP
/// path share one state machine. See [`crate::outbox`] for the invariants.
pub struct DurableSend {
    store: crate::store::Store,
    blobs: crate::store::BlobStore,
    account: crate::config::AccountConfig,
    row_id: i64,
}

impl DurableSend {
    /// Commit the raw bytes and the `pending_send` row. Must be called before
    /// the message is submitted.
    pub fn begin(account: &crate::config::AccountConfig, built: &BuiltMessage) -> Result<Self> {
        let store = crate::store::Store::open_account(&account.name)?;
        let blobs = crate::store::BlobStore::for_account(&account.name);
        // `None` when the server files its own copy (Gmail, Graph, Proton) or
        // the user said `save_to_sent = "never"`: the row then goes straight
        // from `pending_send` to `done` on a 250, with no APPEND ever.
        let target = crate::config::appends_to_sent(account)
            .then(|| crate::config::resolve_sent_mailbox(account));
        let row_id = crate::outbox::enqueue(
            &store,
            &blobs,
            &account.name,
            target.as_deref(),
            &built.message_id,
            &built.raw,
            &crate::outbox::Envelope {
                from: built.from.clone(),
                recipients: built.recipients.clone(),
                draft_key: built.draft_key.clone(),
                ..Default::default()
            },
        )?;
        Ok(Self {
            store,
            blobs,
            account: account.clone(),
            row_id,
        })
    }

    pub fn row_id(&self) -> i64 {
        self.row_id
    }

    /// Commit "the transport is about to be entered" for this row.
    ///
    /// Called immediately before the submission, and never batched with
    /// anything else: the marker is what tells a later resume whether a
    /// `pending_send` row it finds was ever attempted (see
    /// [`crate::outbox::sweep_pending_sends`]). A failure to write it is
    /// logged, not propagated, because refusing to send over a bookkeeping
    /// error would be a worse outcome than the ambiguity it protects against.
    pub fn mark_started(&self) {
        if let Err(e) = crate::outbox::mark_submission_started(&self.store, self.row_id) {
            error!(
                "[outbox] could not mark row {} as entering submission: {e:#}",
                self.row_id
            );
        }
    }

    /// Record what the submission did, committing the transition immediately.
    pub fn record(&self, outcome: &crate::outbox::SubmitOutcome) -> Result<crate::outbox::OutboxState> {
        crate::outbox::record_submission(&self.store, &self.blobs, self.row_id, outcome)
    }

    /// Drive the outstanding APPENDs for this account, this row included, and
    /// report where this row ended up.
    ///
    /// A row that cannot be appended now stays `sent_pending_append` and is
    /// picked up by the next startup or sync tick; the message is already on
    /// the server either way.
    pub async fn settle(&self) -> Result<crate::outbox::OutboxState> {
        drain_account(&self.store, &self.blobs, &self.account).await;
        Ok(crate::outbox::load(&self.store, self.row_id)?
            .map(|row| row.state)
            .unwrap_or(crate::outbox::OutboxState::Done))
    }
}

/// Build, commit, submit over SMTP, record, append: the whole durable path for
/// an already-built message.
pub async fn send_durably(
    built: &BuiltMessage,
    account: &crate::config::AccountConfig,
    smtp_config: &SmtpConfig,
) -> Result<SendReport> {
    let durable = match DurableSend::begin(account, built) {
        Ok(d) => Some(d),
        // The admission gate is the one queue refusal that must stop the send:
        // sending anyway is precisely the double delivery it exists to prevent.
        Err(e) if crate::outbox::is_already_in_flight(&e) => return Err(e),
        // A busy store is the gate not having been consulted yet, not a gate
        // that cannot exist: another process is writing this very outbox, and
        // sending anyway would walk straight past its admission gate. Told to
        // the user as the retryable condition it is (#0063 review).
        Err(e) if crate::outbox::is_store_busy(&e) => {
            return Err(e.context("the outbox is busy with another send; try again"));
        }
        Err(e) => {
            // A store that will not open must not stop the user sending mail;
            // it only costs the durability of this one submission.
            error!("[outbox] could not queue the message durably: {e:#}");
            None
        }
    };

    if let Some(durable) = durable.as_ref() {
        durable.mark_started();
    }
    let send_result = match submit(built, smtp_config).await {
        Ok(result) => result,
        Err(e) => {
            // The marker is already committed and nothing entered the
            // transport: everything `submit` can fail on (the envelope sender,
            // the transport itself) happens before the first connection. Left
            // as it is, the next resume would read the marker as "died inside
            // the SMTP session" and park a message that was never sent, so the
            // failure is recorded as the clean one it is, which puts the
            // marker back to NULL (#0063).
            if let Some(durable) = durable.as_ref() {
                let clean = crate::outbox::SubmitOutcome::CleanPreSubmission(format!("{e:#}"));
                if let Err(e) = durable.record(&clean) {
                    error!("[outbox] could not record a pre-submission failure: {e:#}");
                }
            }
            return Err(e);
        }
    };

    let Some(durable) = durable else {
        return Ok(SendReport {
            send_result,
            state: None,
            row_id: None,
        });
    };

    let mut state = durable.record(&send_result.submit_outcome())?;
    if state == crate::outbox::OutboxState::SentPendingAppend {
        state = durable.settle().await?;
    }
    Ok(SendReport {
        send_result,
        state: Some(state),
        row_id: Some(durable.row_id()),
    })
}

/// The durable path for a submission that is not SMTP (Microsoft Graph).
///
/// `submission` is the API call, handed over as a future so the outbox row is
/// committed before it is polled. Graph files its own copy in Sent Items, so
/// with `save_to_sent = "auto"` the row never carries a target mailbox and
/// goes straight from `pending_send` to `done`; the durability being bought
/// here is of the submission itself.
pub async fn send_durably_via<F>(
    built: &BuiltMessage,
    account: &crate::config::AccountConfig,
    submission: F,
) -> Result<SendReport>
where
    F: std::future::Future<Output = Result<()>>,
{
    let durable = match DurableSend::begin(account, built) {
        Ok(d) => Some(d),
        Err(e) if crate::outbox::is_already_in_flight(&e) => return Err(e),
        // As in `send_durably`: busy is retryable, not a bypass.
        Err(e) if crate::outbox::is_store_busy(&e) => {
            return Err(e.context("the outbox is busy with another send; try again"));
        }
        Err(e) => {
            error!("[outbox] could not queue the message durably: {e:#}");
            None
        }
    };

    if let Some(durable) = durable.as_ref() {
        durable.mark_started();
    }
    let submitted = submission.await;
    let outcome = match &submitted {
        Ok(()) => crate::outbox::SubmitOutcome::Accepted,
        Err(e) => crate::outbox::classify_submission_error(e),
    };
    let send_result = SendResult {
        results: built
            .recipients
            .iter()
            .map(|(addr, role)| RecipientResult {
                address: addr.clone(),
                role: *role,
                success: submitted.is_ok(),
                error: submitted.as_ref().err().map(|e| format!("{e:#}")),
                verdict: match &outcome {
                    crate::outbox::SubmitOutcome::Accepted => RecipientVerdict::Delivered,
                    crate::outbox::SubmitOutcome::Ambiguous(_) => RecipientVerdict::Unknown,
                    _ => RecipientVerdict::Retryable,
                },
            })
            .collect(),
    };

    let Some(durable) = durable else {
        return Ok(SendReport {
            send_result,
            state: None,
            row_id: None,
        });
    };
    let mut state = durable.record(&outcome)?;
    if state == crate::outbox::OutboxState::SentPendingAppend {
        state = durable.settle().await?;
    }
    Ok(SendReport {
        send_result,
        state: Some(state),
        row_id: Some(durable.row_id()),
    })
}

// ---------------------------------------------------------------------------
// One draft, sent (#0058)
// ---------------------------------------------------------------------------

/// Everything one durable send needs that does not come out of the draft.
///
/// `graph` being `Some` is what picks the Graph submission over SMTP, the same
/// test `mp send` makes on `AuthMethod::Graph`; `smtp` is what the SMTP path
/// submits through and where its `from` fallback comes from.
pub struct SendContext {
    pub graph: Option<crate::config::GraphConfig>,
    pub smtp: Option<SmtpConfig>,
    pub account: crate::config::AccountConfig,
    pub email_settings: EmailSettings,
    pub signature: Option<String>,
}

impl SendContext {
    /// The address a draft that names no `from:` of its own is sent from.
    ///
    /// The Graph path has no SMTP config to take a fallback from, so it takes
    /// the account's; identical bytes either way (see
    /// [`build_draft_message`]).
    fn default_from(&self) -> Result<&str> {
        match (self.graph.as_ref(), self.smtp.as_ref()) {
            (Some(_), _) => Ok(&self.account.default_from),
            (None, Some(smtp)) => Ok(&smtp.default_from),
            (None, None) => Err(anyhow!("SMTP not configured")),
        }
    }
}

/// The key one draft is admitted under (#0063).
///
/// The frontmatter `id` when there is one, because it survives the rename a
/// send performs and is the same key the drafts index and every
/// `mp://<account>/drafts/<key>` selector use. A file that has none (an
/// agent-written draft the index has not seen yet) falls back to its path,
/// which is the only other thing two sends of the same draft share.
pub fn draft_key(draft: &EmailDraft) -> String {
    match draft.frontmatter.id.as_deref() {
        Some(id) if !id.trim().is_empty() => format!("id:{}", id.trim()),
        _ => format!("path:{}", draft.path.display()),
    }
}

/// The drafts a send is running for in this process.
///
/// The cheap half of the admission gate (#0063): the TUI reaches
/// [`send_draft`] on a background thread per send, and an approved draft under
/// the cursor is by definition also in the approved batch, so the same draft
/// can be sent twice with nothing durable committed yet by either. This is a
/// plain set because the whole question is "is one already running", and the
/// answer has to be given without touching the disk.
///
/// Keyed by account as well as draft, like the durable half
/// ([`crate::outbox::enqueue`] scopes its query to one account): two accounts
/// each holding a hand-written draft with the same frontmatter `id:` are two
/// messages, and refusing the second would be a false positive.
static SENDING: std::sync::LazyLock<std::sync::Mutex<HashSet<(String, String)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// One draft's slot in [`SENDING`], released when the send returns however it
/// returns.
struct SendAdmission((String, String));

impl SendAdmission {
    /// `None` when a send for this draft is already running on this account.
    fn claim(account: &str, key: &str) -> Option<Self> {
        let slot = (account.to_string(), key.to_string());
        let mut sending = SENDING.lock().unwrap_or_else(|e| e.into_inner());
        sending.insert(slot.clone()).then(|| SendAdmission(slot))
    }
}

impl Drop for SendAdmission {
    fn drop(&mut self) {
        let mut sending = SENDING.lock().unwrap_or_else(|e| e.into_inner());
        sending.remove(&self.0);
    }
}

/// What [`send_draft`] did: the durable send, and what became of the file.
pub struct SentDraft {
    /// Where the submission got to, per recipient and in the outbox.
    pub report: SendReport,
    /// The bookkeeping error a submission that did go out nevertheless hit
    /// while retiring the draft file. The message is on the server either
    /// way, so this is a line for the caller to word, never a failed send.
    pub settle_error: Option<anyhow::Error>,
}

/// Parse an address string like `"Name <addr>"` or `"addr"` into
/// `(name, address)`.
fn parse_name_address(s: &str) -> (String, String) {
    let s = s.trim();
    if let Some(lt) = s.find('<') {
        if let Some(gt) = s.find('>') {
            let name = s[..lt].trim().trim_matches('"').trim().to_string();
            let addr = s[lt + 1..gt].trim().to_string();
            return (name, addr);
        }
    }
    // Plain address
    (String::new(), s.to_string())
}

/// Parse a frontmatter address field (comma-separated) into `(name, address)`
/// pairs, which is the shape the Graph API takes its recipients in.
fn parse_graph_recipients(field: Option<&str>) -> Vec<(String, String)> {
    match field {
        Some(s) if !s.trim().is_empty() => split_addresses(s)
            .into_iter()
            .map(|a| parse_name_address(&a))
            .collect(),
        _ => Vec::new(),
    }
}

/// Expand a draft's `attachments:` frontmatter entries into concrete files.
///
/// Each entry is tilde-expanded. A directory entry contributes every regular
/// file directly inside it (non-recursive, sorted by file name, dotfiles
/// skipped); any other entry contributes itself unchanged. This lets a draft
/// name one folder instead of listing every file in it. A path that is neither
/// a readable directory nor an existing file is passed through untouched, so
/// the later `fs::read` reports the missing path the same way it always has.
pub fn resolve_attachment_paths(entries: &[String]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in entries {
        let expanded = shellexpand::tilde(entry);
        let path = PathBuf::from(expanded.as_ref());
        if path.is_dir() {
            let mut dir_files = Vec::new();
            for dent in fs::read_dir(&path)
                .with_context(|| format!("Failed to read attachment folder: {}", entry))?
            {
                let dent = dent
                    .with_context(|| format!("Failed to read attachment folder: {}", entry))?;
                let file = dent.path();
                if !file.is_file() {
                    continue;
                }
                let is_dotfile = file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'));
                if is_dotfile {
                    continue;
                }
                dir_files.push(file);
            }
            // Deterministic order so the same folder always attaches the same
            // sequence; `read_dir` yields entries in arbitrary order.
            dir_files.sort();
            paths.extend(dir_files);
        } else {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// Read one attachment file into its `(filename, bytes, content-type)` triple.
fn read_attachment(path: &Path) -> Result<(String, Vec<u8>, String)> {
    let content =
        fs::read(path).with_context(|| format!("Failed to read attachment: {}", path.display()))?;
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "attachment".to_string());
    let content_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    Ok((filename, content, content_type))
}

/// The attachments a draft names, read off the paths in its frontmatter.
///
/// Only the Graph path needs them separately: the SMTP path's bytes are built
/// by [`build_draft_message`], which reads the same list itself and fails the
/// same way on a path that is not there. Directory entries are expanded by
/// [`resolve_attachment_paths`].
fn draft_attachments(draft: &EmailDraft) -> Result<Vec<(String, Vec<u8>, String)>> {
    let Some(attachments) = draft.frontmatter.attachments.as_ref() else {
        return Ok(Vec::new());
    };
    let mut data = Vec::new();
    for path in resolve_attachment_paths(attachments)? {
        data.push(read_attachment(&path)?);
    }
    Ok(data)
}

/// Send one draft: the single orchestration behind `mp send`,
/// `mp send-approved` and the TUI's send key (#0058).
///
/// The order is the durable one (#0037 item 5): the bytes are built first
/// (which is where the approved-status requirement is enforced, by
/// [`build_draft_message`]), committed to the outbox, submitted over whichever
/// transport `ctx` names, and only then is the draft file touched. A
/// submission that reached at least one recipient rewrites the draft's
/// `status:` to `sent`; one that reached *every* recipient and got an outbox
/// row additionally takes the file out of `drafts/`, see
/// [`crate::draft::settle_sent_draft`]. The sent copy is the outbox's
/// business, not this function's.
///
/// What is left to the caller is what differs between the three: the
/// confirmation prompt, the wording of the status line, the exit code, and the
/// drafts-index refresh (a send-approved run pays for one refresh at the end
/// rather than one per draft).
pub async fn send_draft(draft: &EmailDraft, ctx: &SendContext) -> Result<SentDraft> {
    let key = draft_key(draft);
    let _admission = SendAdmission::claim(&ctx.account.name, &key).ok_or_else(|| {
        anyhow::Error::new(crate::outbox::AlreadyInFlight(
            "this draft is already being sent; it is sent once, not twice".to_string(),
        ))
    })?;
    let built = build_draft_message(
        draft,
        ctx.default_from()?,
        &ctx.email_settings,
        ctx.signature.as_deref(),
        None,
    )?;

    let report = match ctx.graph.as_ref() {
        Some(graph_config) => {
            let to = parse_graph_recipients(draft.frontmatter.to.as_deref());
            let cc = parse_graph_recipients(draft.frontmatter.cc.as_deref());
            let bcc = parse_graph_recipients(draft.frontmatter.bcc.as_deref());
            let to_refs: Vec<(&str, &str)> =
                to.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
            let cc_refs: Vec<(&str, &str)> =
                cc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
            let bcc_refs: Vec<(&str, &str)> =
                bcc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();

            // The quoted reply lives in the companion HTML the draft builder
            // wrote; the Graph API takes a rendered body rather than bytes.
            let quoted_html = draft.path.with_extension("html");
            let quoted = if quoted_html.exists() {
                fs::read_to_string(&quoted_html).ok()
            } else {
                None
            };
            let html_body = markdown_to_html(
                &draft.body_markdown,
                &ctx.email_settings,
                ctx.signature.as_deref(),
                quoted.as_deref(),
            );
            let att_data = draft_attachments(draft)?;
            let client = crate::graph::GraphClient::new_async(graph_config).await?;
            // Graph files its own copy in Sent Items, so the outbox row never
            // carries a target mailbox; what it buys here is exactly-once
            // durability of the submission.
            send_durably_via(
                &built,
                &ctx.account,
                client.send_mail(
                    &to_refs,
                    &cc_refs,
                    &bcc_refs,
                    &draft.frontmatter.subject,
                    &html_body,
                    &att_data,
                ),
            )
            .await?
        }
        None => {
            let smtp = ctx
                .smtp
                .as_ref()
                .ok_or_else(|| anyhow!("SMTP not configured"))?;
            send_durably(&built, &ctx.account, smtp).await?
        }
    };

    let mut settle_error = None;
    if report.send_result.any_succeeded() {
        if let Err(e) = crate::draft::settle_sent_draft(draft, &report, Some(&built.message_id)) {
            warn!("Sent but failed to retire {}: {e:#}", draft.path.display());
            settle_error = Some(e);
        }
        // Best-effort, no-op without a contacts cache; the message went out,
        // so the correspondent is worth remembering whether or not the file
        // could be retired.
        crate::contacts::hooks::bump_after_send(&ctx.account, draft);
        mark_source_after_send(draft, ctx).await;
    }
    Ok(SentDraft {
        report,
        settle_error,
    })
}

/// The source a sent draft has something to say about, and which bit it sets:
/// `true` for `\Answered`, `false` for `$Forwarded` (#TKT-0051).
///
/// A draft carries at most one of the two keys, because it was built as a
/// reply or as a forward and never as both; a reply wins if a hand-edited
/// draft names both, since answering is the stronger statement.
fn source_to_flag(draft: &EmailDraft) -> Option<(&str, bool)> {
    let (message_id, answered) = match (
        draft.frontmatter.in_reply_to.as_deref(),
        draft.frontmatter.forwarded_from.as_deref(),
    ) {
        (Some(id), _) => (id.trim(), true),
        (None, Some(id)) => (id.trim(), false),
        (None, None) => return None,
    };
    (!message_id.is_empty()).then_some((message_id, answered))
}

/// The distinct server mailboxes the local copies of a source live in, in a
/// stable order (#0076).
///
/// One store row per mailbox is one server folder to flag; the same folder
/// twice is one `SELECT`, so the list is deduplicated before it rides the op.
fn server_mailboxes_of(
    account: &crate::config::AccountConfig,
    rows: &[crate::store::read::MessageRow],
) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let server_mailbox = crate::config::find_server_name_for_role(account, &row.mailbox);
        if seen.insert(server_mailbox.clone()) {
            out.push(server_mailbox);
        }
    }
    out
}

/// The second status axis, written after the message actually went out
/// (#TKT-0051, moved onto the durable queue by #0076).
///
/// A reply draft carries `in_reply_to:` and a forward carries
/// `forwarded_from:`, both the source's `Message-ID`; this is the one reader
/// of either. The local flag write and the server op's queue row commit in one
/// transaction ([`crate::pending_ops::apply_post_send_flag`]), and that commit
/// is the whole cost on the send path: **no IMAP session is opened here**. The
/// server half is one multi-mailbox op that
/// [`crate::pending_ops::resume_account`] drains over a single session on the
/// next tick or at the next startup.
///
/// Best effort throughout, and that is a durability statement, not a shrug:
/// nothing here may fail, delay or retry a send that already succeeded, so
/// every error is logged and swallowed, and the function returns `()`. The
/// enqueue happens strictly after the delivery and touches no `outbox` row, so
/// the exactly-once submission marker (#0063) is untouched by this path: a
/// message can never be re-sent because its bookkeeping failed. A queue row
/// that can never be written is still self-healed by the sync, which restates
/// every flag the server holds over the whole window.
///
/// The Graph path writes locally and queues nothing: Graph exposes the answered
/// state only through extended MAPI properties, and the backend is parked
/// (#0042, #0055). Its flag merge (`ingest::apply_seen_flags`) is what keeps a
/// Graph sync from erasing what is written here.
async fn mark_source_after_send(draft: &EmailDraft, ctx: &SendContext) {
    let Some((message_id, answered)) = source_to_flag(draft) else {
        return;
    };

    let store = match crate::store::Store::open_account(&ctx.account.name) {
        Ok(store) => store,
        Err(e) => {
            warn!("[send] could not open the store to flag {message_id}: {e:#}");
            return;
        }
    };
    let rows = match crate::store::read::find_by_message_id(&store, &ctx.account.name, message_id) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("[send] could not look {message_id} up to flag it: {e:#}");
            return;
        }
    };
    if rows.is_empty() {
        debug!("[send] nothing local to flag for {message_id}");
        return;
    }
    // A Graph account, or an account with no usable IMAP config, has no server
    // half to owe: write the local bit and queue nothing.
    let server_mailboxes = if ctx.graph.is_some() {
        Vec::new()
    } else if let Err(e) = crate::config::ImapConfig::load(&ctx.account) {
        debug!("[send] no IMAP config to flag {message_id} on the server: {e:#}");
        Vec::new()
    } else {
        server_mailboxes_of(&ctx.account, &rows)
    };

    match crate::pending_ops::apply_post_send_flag(
        &store,
        &ctx.account.name,
        message_id,
        answered,
        &server_mailboxes,
    ) {
        Ok(outcome) => debug!(
            "[send] flagged {} local row(s) for {message_id}, server op {:?}",
            outcome.rows, outcome.op_id
        ),
        Err(e) => warn!("[send] could not flag {message_id}: {e:#}"),
    }
}

/// Run every outstanding APPEND for one account, best effort.
///
/// Shared by the post-send settle, the startup resume and the sync tick. Never
/// returns an error: a Sent copy that has to wait for the next tick is not a
/// reason to fail whatever the caller was doing.
pub async fn drain_account(
    store: &crate::store::Store,
    blobs: &crate::store::BlobStore,
    account: &crate::config::AccountConfig,
) -> crate::outbox::DrainResult {
    let counts = match crate::outbox::counts(store, &account.name) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[outbox] could not read the outbox for {}: {e:#}", account.name);
            return crate::outbox::DrainResult::default();
        }
    };
    if counts.open == 0 {
        return crate::outbox::DrainResult::default();
    }

    let imap_config = match crate::config::ImapConfig::load(account) {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "[outbox] {} has {} row(s) waiting for an APPEND but no usable IMAP config: {e:#}",
                account.name,
                counts.open
            );
            return crate::outbox::DrainResult::default();
        }
    };

    let mut mailbox = crate::imap_client::ImapSentMailbox::new(imap_config);
    let result = crate::outbox::drain(
        store,
        blobs,
        &account.name,
        &mut mailbox,
        crate::outbox::unix_now(),
    )
    .await;
    mailbox.close().await;
    match result {
        Ok(result) => result,
        Err(e) => {
            log::warn!("[outbox] draining {} failed: {e:#}", account.name);
            crate::outbox::DrainResult::default()
        }
    }
}

/// Resume the outbox for one account: the startup and sync-tick entry point.
///
/// Opens the account's store only when there is one, so a fresh account costs
/// nothing. Three things happen, in this order and for this reason:
///
/// 1. [`crate::outbox::sweep_pending_sends`] reads the exactly-once marker on
///    every `pending_send` row. A row that died inside its SMTP session is
///    parked in `failed` for a human and never re-sent.
/// 2. The rows that provably never reached the transport are submitted here,
///    which is the half of the crash story the driver cannot do: it needs the
///    credentials and the envelope, and both live on this side.
/// 3. [`drain_account`] finishes the outstanding APPENDs, this pass's included.
pub async fn resume_outbox(account: &crate::config::AccountConfig) -> crate::outbox::DrainResult {
    let path = crate::config::store_path(&account.name);
    if !path.exists() {
        return crate::outbox::DrainResult::default();
    }
    let store = match crate::store::Store::open(&path) {
        Ok(store) => store,
        Err(e) => {
            log::warn!("[outbox] could not open the store for {}: {e:#}", account.name);
            return crate::outbox::DrainResult::default();
        }
    };
    let blobs = crate::store::BlobStore::for_account(&account.name);
    resubmit_pending(&store, &blobs, account).await;
    drain_account(&store, &blobs, account).await
}

/// Send the `pending_send` rows that a crash left behind, exactly once each.
///
/// Never returns an error: this runs on startup and on the sync tick, where a
/// server that is still down must not fail the caller. A row that cannot be
/// submitted now stays `pending_send` and is tried again next time, under the
/// same backoff the APPEND retries use.
async fn resubmit_pending(
    store: &crate::store::Store,
    blobs: &crate::store::BlobStore,
    account: &crate::config::AccountConfig,
) {
    let sweep = match crate::outbox::sweep_pending_sends(store, &account.name) {
        Ok(sweep) => sweep,
        Err(e) => {
            log::warn!(
                "[outbox] could not classify the pending sends for {}: {e:#}",
                account.name
            );
            return;
        }
    };
    if !sweep.stranded.is_empty() {
        log::warn!(
            "[outbox] {} submission(s) for {} died mid-SMTP and are parked as failed; \
             inspect them with `mp outbox list`",
            sweep.stranded.len(),
            account.name
        );
    }
    if sweep.resubmittable.is_empty() {
        return;
    }

    if account.auth_method == crate::config::AuthMethod::Graph {
        // The Graph transport sends a structured JSON message, not the RFC822
        // bytes the row holds, so there is nothing here to resubmit from. The
        // rows stay visible in `mp outbox list` for a human to discard or to
        // re-send from the draft.
        log::warn!(
            "[outbox] {} has {} queued message(s) that were never submitted, and the Graph \
             transport cannot resend stored RFC822 bytes; see `mp outbox list`",
            account.name,
            sweep.resubmittable.len()
        );
        return;
    }

    let smtp_config = match SmtpConfig::load(account) {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "[outbox] {} has {} queued message(s) but no usable SMTP config: {e:#}",
                account.name,
                sweep.resubmittable.len()
            );
            return;
        }
    };

    let now = crate::outbox::unix_now();
    for row in sweep.resubmittable {
        if row.updated + crate::outbox::backoff_secs(row.attempts) > now {
            // A previous clean failure (no transport, a rejected address) bumped
            // the counter; wait it out rather than hammer the server.
            continue;
        }
        if let Err(e) = resubmit_row(store, blobs, &row, &smtp_config).await {
            log::warn!("[outbox] could not resubmit row {}: {e:#}", row.id);
        }
    }
}

/// Submit one never-attempted row, marker first.
async fn resubmit_row(
    store: &crate::store::Store,
    blobs: &crate::store::BlobStore,
    row: &crate::outbox::OutboxRow,
    smtp_config: &SmtpConfig,
) -> Result<crate::outbox::OutboxState> {
    let Some(envelope) = row.envelope.clone().filter(|e| e.is_submittable()) else {
        // Without an envelope there is no honest way to address the message,
        // and guessing from the headers would drop the blind recipients.
        crate::outbox::record_submission(
            store,
            blobs,
            row.id,
            &crate::outbox::SubmitOutcome::Ambiguous(
                "the queued submission has no usable envelope; it cannot be resent \
                 automatically"
                    .to_string(),
            ),
        )?;
        return Ok(crate::outbox::OutboxState::Failed);
    };
    // Only the recipients that have neither taken the message nor been refused
    // it: a recipient that answered 250 on an earlier pass must not see the
    // message a second time (#0063).
    let outstanding = envelope.outstanding();
    if outstanding.is_empty() {
        // Every recipient is settled, so there is nothing left to submit and
        // the row only needs the transition it did not get.
        return crate::outbox::record_submission(
            store,
            blobs,
            row.id,
            &crate::outbox::SubmitOutcome::PerRecipient(crate::outbox::RecipientVerdicts::default()),
        );
    }
    let raw = blobs
        .read(&row.raw_blob)
        .with_context(|| format!("reading the queued message of outbox row {}", row.id))?;
    let built = BuiltMessage {
        raw,
        message_id: row.message_id.clone(),
        recipients: outstanding,
        from: envelope.from,
        draft_key: envelope.draft_key,
    };

    info!(
        "[outbox] resubmitting row {} ({}) to {} recipient(s)",
        row.id,
        row.message_id,
        built.recipients.len()
    );
    crate::outbox::mark_submission_started(store, row.id)?;
    let result = match submit(&built, smtp_config).await {
        Ok(result) => result,
        Err(e) => {
            // Provably before the transport, as in `send_durably`: record it
            // as clean so the marker goes back to NULL and the row stays
            // submittable instead of being swept into `failed`.
            crate::outbox::record_submission(
                store,
                blobs,
                row.id,
                &crate::outbox::SubmitOutcome::CleanPreSubmission(format!("{e:#}")),
            )?;
            return Err(e);
        }
    };
    crate::outbox::record_submission(store, blobs, row.id, &result.submit_outcome())
}

/// A message that is fully built and not yet submitted.
///
/// The split between building and submitting is what the durable outbox is
/// made of: these bytes are committed to the store *before* the SMTP
/// conversation opens, so no crash window can lose a message that the server
/// might already have accepted.
#[derive(Debug, Clone)]
pub struct BuiltMessage {
    /// The RFC822 bytes, exactly as they go to SMTP and into the blob store.
    pub raw: Vec<u8>,
    /// The `Message-ID` header, synthesised when the builder produced none.
    /// The outbox's dedup search keys on it, so it is never optional.
    pub message_id: String,
    /// Every recipient with its role, deduplicated, in header order.
    pub recipients: Vec<(String, RecipientRole)>,
    /// The envelope sender.
    pub from: String,
    /// The draft these bytes were built from, when they were built from one.
    /// The outbox admits one submission per draft at a time (#0063).
    pub draft_key: Option<String>,
}

/// The `Message-ID` of a built message.
///
/// lettre generates one for every message it builds, so the fallback only
/// fires for bytes that came from somewhere else; it reuses the ingest path's
/// synthesis so the same message gets the same id on both sides.
pub fn message_id_of(raw: &[u8]) -> String {
    let header = mailparse::parse_headers(raw).ok().and_then(|(headers, _)| {
        headers
            .iter()
            .find(|h| h.get_key().eq_ignore_ascii_case("Message-ID"))
            .map(|h| h.get_value().trim().to_string())
            .filter(|v| !v.is_empty())
    });
    match header {
        Some(id) => id,
        None => match crate::parse::parse_rfc822_to_fetched_email(raw) {
            Some(email) => crate::ingest::synthesize_message_id(&email, Some(raw)),
            None => {
                use sha2::{Digest, Sha256};
                let digest = Sha256::digest(raw);
                let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
                format!("<sha256-{hex}@local.invalid>")
            }
        },
    }
}

/// Build the message a draft describes, without touching the network.
///
/// `default_from` is the account's address, used when the draft names no
/// `from:` of its own. It is passed in rather than read off the SMTP config so
/// the Graph path, which has no SMTP config, builds identical bytes.
pub fn build_draft_message(
    draft: &EmailDraft,
    default_from: &str,
    email_config: &EmailSettings,
    signature: Option<&str>,
    invite_ics: Option<&str>,
) -> Result<BuiltMessage> {
    // Check status
    if draft.frontmatter.status != EmailStatus::Approved {
        return Err(anyhow!(
            "Email not approved for sending. Current status: {}",
            draft.frontmatter.status
        ));
    }

    let from_address = draft
        .frontmatter
        .from
        .as_deref()
        .unwrap_or(default_from);

    // Normalised once and carried on the built message: what is validated
    // here is what `submit` parses to derive `MAIL FROM`, so a `from:` like
    // `Doe, Jane <j@x.com>` cannot pass the build and then fail every
    // submission forever (#0063 review).
    let normalized_from = normalize_address_for_smtp(from_address);
    let from_mailbox: Mailbox = normalized_from
        .parse()
        .context("Invalid 'from' email address")?;

    info!(
        "Sending email: subject=\"{}\", from={}",
        draft.frontmatter.subject, from_address
    );

    // Collect all recipients with roles, deduplicating by address
    let mut seen = HashSet::new();
    let mut recipients: Vec<(String, RecipientRole)> = Vec::new();

    if let Some(ref to) = draft.frontmatter.to {
        for addr in split_addresses(to) {
            if seen.insert(addr.to_lowercase()) {
                recipients.push((addr, RecipientRole::To));
            }
        }
    }
    if let Some(cc) = &draft.frontmatter.cc {
        for addr in split_addresses(cc) {
            if seen.insert(addr.to_lowercase()) {
                recipients.push((addr, RecipientRole::Cc));
            }
        }
    }
    if let Some(bcc) = &draft.frontmatter.bcc {
        for addr in split_addresses(bcc) {
            if seen.insert(addr.to_lowercase()) {
                recipients.push((addr, RecipientRole::Bcc));
            }
        }
    }

    if recipients.is_empty() {
        return Err(anyhow!("No recipients specified"));
    }

    debug!(
        "Recipients ({}): {:?}",
        recipients.len(),
        recipients
            .iter()
            .map(|(a, r)| format!("{}({})", a, r))
            .collect::<Vec<_>>()
    );

    // Build the message with visible To/Cc headers (Bcc omitted from headers by lettre)
    // message_id(None) triggers auto-generation of a unique Message-ID header
    let mut builder = Message::builder()
        .from(from_mailbox.clone())
        .subject(&draft.frontmatter.subject)
        .message_id(None);

    // Add To recipients to headers
    for (addr, role) in &recipients {
        match role {
            RecipientRole::To => {
                let mbox: Mailbox = normalize_address_for_smtp(addr)
                    .parse()
                    .context("Invalid 'to' email address")?;
                builder = builder.to(mbox);
            }
            RecipientRole::Cc => {
                let mbox: Mailbox = normalize_address_for_smtp(addr)
                    .parse()
                    .context("Invalid 'cc' email address")?;
                builder = builder.cc(mbox);
            }
            RecipientRole::Bcc => {
                let mbox: Mailbox = normalize_address_for_smtp(addr)
                    .parse()
                    .context("Invalid 'bcc' email address")?;
                builder = builder.bcc(mbox);
            }
        }
    }

    // Add Reply-To
    if let Some(reply_to) = &draft.frontmatter.reply_to {
        let reply_mailbox: Mailbox = normalize_address_for_smtp(reply_to)
            .parse()
            .context("Invalid 'reply_to' email address")?;
        builder = builder.reply_to(reply_mailbox);
    }

    // Load companion HTML for quoted section if available
    let html_companion_path = draft.path.with_extension("html");
    let quoted_html = if html_companion_path.exists() {
        fs::read_to_string(&html_companion_path).ok()
    } else {
        None
    };

    // Generate HTML with signature (and original HTML for quoted section if available)
    let body_html = markdown_to_html(
        &draft.body_markdown,
        email_config,
        signature,
        quoted_html.as_deref(),
    );

    // Build the plain/html alternative part (non-invite path).
    let body_multipart = MultiPart::alternative()
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(plain_text_body(&draft.body_markdown)),
        )
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(body_html.clone()),
        );

    // Build message with or without attachments
    let message = if let Some(ics) = invite_ics {
        // Invite path: multipart/mixed [ alternative(plain, html, calendar),
        // application/ics ]; regular file attachments (if any) follow.
        let mut mixed =
            build_invite_mime_body(&plain_text_body(&draft.body_markdown), body_html, ics);
        if let Some(attachments) = &draft.frontmatter.attachments {
            for path in resolve_attachment_paths(attachments)? {
                let (filename, file_content, content_type) = read_attachment(&path)?;
                let content_type_parsed = content_type.parse().unwrap_or_else(|_| {
                    "application/octet-stream".parse().expect("static MIME type")
                });
                mixed = mixed.singlepart(Attachment::new(filename).body(file_content, content_type_parsed));
            }
        }
        builder.multipart(mixed).context("Failed to build invite message")?
    } else if let Some(attachments) = &draft.frontmatter.attachments {
        let files = resolve_attachment_paths(attachments)?;
        if !files.is_empty() {
            let mut mixed = MultiPart::mixed().multipart(body_multipart);

            for path in files {
                let (filename, file_content, content_type) = read_attachment(&path)?;
                let content_type_parsed = content_type.parse()
                    .unwrap_or_else(|_| "application/octet-stream".parse().expect("static MIME type"));
                let attachment = Attachment::new(filename).body(file_content, content_type_parsed);
                mixed = mixed.singlepart(attachment);
            }

            builder.multipart(mixed).context("Failed to build email message")?
        } else {
            builder.multipart(body_multipart).context("Failed to build email message")?
        }
    } else {
        builder.multipart(body_multipart).context("Failed to build email message")?
    };

    // Get raw message bytes for send_raw and IMAP APPEND
    let raw_message = message.formatted();

    Ok(BuiltMessage {
        message_id: message_id_of(&raw_message),
        raw: raw_message,
        recipients,
        from: normalized_from,
        draft_key: Some(draft_key(draft)),
    })
}

/// Submit an already-built message over SMTP, one envelope per recipient.
///
/// Never called before the message is committed to the outbox: see
/// [`crate::outbox`] for why the ordering is the whole design.
pub async fn submit(built: &BuiltMessage, smtp_config: &SmtpConfig) -> Result<SendResult> {
    // Parse from address for envelope
    let from_addr: lettre::Address = built
        .from
        .parse::<Mailbox>()
        .context("Invalid 'from' address")?
        .email;

    // Create SMTP transport (branching on auth method / TLS mode).
    let mailer = build_smtp_transport(smtp_config)?;

    let mut results = Vec::with_capacity(built.recipients.len());

    for (addr, role) in &built.recipients {
        let rcpt_addr: lettre::Address = match normalize_address_for_smtp(addr).parse::<Mailbox>() {
            Ok(mbox) => mbox.email,
            Err(e) => {
                let err_msg = format!("Invalid address '{}': {}", addr, e);
                error!("{}", err_msg);
                results.push(RecipientResult {
                    address: addr.clone(),
                    role: *role,
                    success: false,
                    error: Some(err_msg),
                    // Nothing about waiting turns an unparseable address into
                    // a deliverable one.
                    verdict: RecipientVerdict::Rejected,
                });
                continue;
            }
        };

        let envelope = match Envelope::new(Some(from_addr.clone()), vec![rcpt_addr]) {
            Ok(env) => env,
            Err(e) => {
                let err_msg = format!("Failed to create envelope for '{}': {}", addr, e);
                error!("{}", err_msg);
                results.push(RecipientResult {
                    address: addr.clone(),
                    role: *role,
                    success: false,
                    error: Some(err_msg),
                    verdict: RecipientVerdict::Rejected,
                });
                continue;
            }
        };

        match mailer.send_raw(&envelope, &built.raw).await {
            Ok(_) => {
                info!("Sent to {} ({})", addr, role);
                results.push(RecipientResult {
                    address: addr.clone(),
                    role: *role,
                    success: true,
                    error: None,
                    verdict: RecipientVerdict::Delivered,
                });
            }
            Err(e) => {
                let err_msg = format!("{}", e);
                error!("Failed to send to {} ({}): {}", addr, role, err_msg);
                results.push(RecipientResult {
                    address: addr.clone(),
                    role: *role,
                    success: false,
                    verdict: smtp_failure_verdict(&e),
                    error: Some(err_msg),
                });
            }
        }
    }

    Ok(SendResult { results })
}
