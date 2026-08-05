use anyhow::{anyhow, Context, Result};
use lettre::{
    address::Envelope,
    message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use log::{debug, error, info};
use pulldown_cmark::{html, Options, Parser as MdParser};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

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

#[derive(Debug)]
pub struct RecipientResult {
    pub address: String,
    pub role: RecipientRole,
    pub success: bool,
    pub error: Option<String>,
    /// True when the failure leaves it unknown whether the server accepted the
    /// message (a dropped connection, a timeout). Drives the outbox's
    /// never-auto-re-send rule; see [`SendResult::submit_outcome`].
    pub ambiguous: bool,
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

    /// How the durable outbox must read this result (#0037 item 5).
    ///
    /// One 250 is enough to call the message submitted: the per-recipient loop
    /// sends the same bytes in separate envelopes, so a partial result still
    /// means the server holds the message and the Sent copy is owed. With no
    /// acceptance at all the question becomes whether a copy might exist
    /// anyway, which is exactly [`RecipientResult::ambiguous`].
    pub fn submit_outcome(&self) -> crate::outbox::SubmitOutcome {
        use crate::outbox::SubmitOutcome;
        if self.any_succeeded() {
            return SubmitOutcome::Accepted;
        }
        let detail = self
            .failed()
            .iter()
            .map(|r| {
                format!(
                    "{}: {}",
                    r.address,
                    r.error.as_deref().unwrap_or("unknown error")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let detail = if detail.is_empty() {
            "no recipients were attempted".to_string()
        } else {
            detail
        };
        if self.results.iter().any(|r| r.ambiguous) {
            SubmitOutcome::Ambiguous(detail)
        } else {
            SubmitOutcome::CleanPreSubmission(detail)
        }
    }
}

/// Whether an SMTP failure leaves it unknown that the message was not
/// accepted.
///
/// A response error is the server saying no in words, and a client-side error
/// (bad address, TLS setup, no connection) happens before any bytes could be
/// accepted: both are clean. A timeout or a connection that dies mid-
/// conversation is not, because the 250 may simply have been lost on the way
/// back.
fn smtp_failure_is_ambiguous(err: &lettre::transport::smtp::Error) -> bool {
    if err.is_timeout() {
        return true;
    }
    !(err.is_response() || err.is_client() || err.is_tls())
}

pub fn markdown_to_html(
    markdown: &str,
    config: &EmailSettings,
    signature: Option<&str>,
    quoted_html: Option<&str>,
) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = MdParser::new_ext(markdown, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);

    // Handle signature placement:
    // If {{SIGNATURE}} placeholder is present (reply drafts), split the HTML at the placeholder,
    // inject the signature, and wrap the quoted content in a styled div.
    // Replace <blockquote> with styled <div> in the quoted section to prevent email clients
    // (Apple Mail, Gmail) from collapsing the signature and conversation behind "see more".
    // Otherwise (regular drafts), append signature at the end.
    let signature_html = signature.unwrap_or_default();
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
                "{}\n{}\n<div style=\"padding-top:1em\">\n{}\n</div>",
                reply_part.trim_end(),
                signature_html,
                original_html,
            )
        } else {
            // Fallback: convert Markdown blockquotes to styled divs
            let quoted_part = if parts.len() > 1 { parts[1] } else { "" };
            let quoted_styled = quoted_part
                .replace("<blockquote>", "<div style=\"margin:0;padding:0 0 0 1em;border-left:2px solid #ccc\">")
                .replace("</blockquote>", "</div>");
            format!(
                "{}\n{}\n<div style=\"padding-top:1em\">\n{}\n</div>",
                reply_part.trim_end(),
                signature_html,
                quoted_styled.trim_start()
            )
        }
    } else {
        format!("{}\n{}", html_output, signature_html)
    };

    // Wrap in basic HTML structure with styling from config
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
{body}
</body>
</html>"#,
        font_family = config.font_family,
        font_size = config.font_size,
        body = body,
    )
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
    fn test_markdown_to_html_with_signature_placeholder() {
        let md = "My reply\n\n{{SIGNATURE}}\n\n> Original message";
        let sig = "<p>-- Best, Alice</p>";
        let html = markdown_to_html(md, &default_settings(), Some(sig), None);
        assert_snapshot!(html);
    }

    #[test]
    fn test_markdown_to_html_signature_with_quoted_html() {
        let md = "My reply\n\n{{SIGNATURE}}\n\n> Quoted text";
        let sig = "<p>-- Best, Alice</p>";
        let quoted = "<p>Original HTML content</p>";
        let html = markdown_to_html(md, &default_settings(), Some(sig), Some(quoted));
        assert_snapshot!(html);
    }

    #[test]
    fn test_markdown_to_html_signature_without_quoted_html() {
        let md = "My reply\n\n{{SIGNATURE}}\n\n> Quoted text";
        let sig = "<p>-- Best, Alice</p>";
        let html = markdown_to_html(md, &default_settings(), Some(sig), None);
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
        };
        let html = markdown_to_html("Hello", &settings, None, None);
        assert!(html.contains("Georgia, serif"));
        assert!(html.contains("14px"));
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
                ambiguous: false,
            });
        }
        for addr in failures {
            results.push(RecipientResult {
                address: addr.to_string(),
                role: RecipientRole::To,
                success: false,
                error: Some("SMTP error".to_string()),
                ambiguous: false,
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
        let sig = "<p>-- Best, Alice</p>";
        let html = markdown_to_html("Hello world", &default_settings(), Some(sig), None);
        // Without placeholder, signature is appended after the body
        assert!(html.contains("<p>Hello world</p>"));
        assert!(html.contains("-- Best, Alice"));
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
        let md = "Reply\n\n{{SIGNATURE}}\n\n> original";
        let sig = "<p>sig</p>";
        let quoted = "<p>Original HTML</p>";
        let html = markdown_to_html(md, &default_settings(), Some(sig), Some(quoted));
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
    let from_mailbox: Mailbox = normalize_address_for_smtp(from)
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
        from: from.to_string(),
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
/// Steps: read the invite email's sidecar `invite.ics` (source of truth for
/// `UID`/`SEQUENCE`), build a `METHOD:REPLY` with the account's `PARTSTAT`,
/// email it to the `ORGANIZER`, and — only on a successful send — flip the
/// local `event.rsvp` frontmatter. The sidecar is never rewritten.
///
/// `email_path` is the received invite `.md`; `account_address` is the
/// responding account's primary address (the REPLY `ATTENDEE`).
pub async fn send_rsvp(
    email_path: &Path,
    account_config: &crate::config::AccountConfig,
    account_address: &str,
    rsvp: crate::invite::Rsvp,
    smtp_config: &SmtpConfig,
) -> Result<RsvpOutcome> {
    let account_address = crate::parse::extract_email_address(account_address);
    if account_address.is_empty() {
        return Err(anyhow!("Account has no usable address to RSVP as"));
    }

    // Locate and read the sidecar .ics colocated with the email's attachments.
    let sidecar = crate::parse::attachments_dir_for(email_path)
        .join(crate::parse::CALENDAR_SIDECAR_NAME);
    let ics_bytes = fs::read(&sidecar).with_context(|| {
        format!(
            "No calendar sidecar found at {} (is this a received invite?)",
            sidecar.display()
        )
    })?;

    let ctx = crate::invite::reply_context_from_ics(&ics_bytes)?;
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

    // Update local state only after the reply actually left the machine.
    if send_result.any_succeeded() {
        if let Err(e) = crate::draft::set_event_rsvp(email_path, rsvp.frontmatter_status()) {
            log::warn!(
                "RSVP sent but failed to update event.rsvp in {}: {}",
                email_path.display(),
                e
            );
        }
    }

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
    let send_result = submit(built, smtp_config).await?;

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
                ambiguous: matches!(outcome, crate::outbox::SubmitOutcome::Ambiguous(_)),
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
    let raw = blobs
        .read(&row.raw_blob)
        .with_context(|| format!("reading the queued message of outbox row {}", row.id))?;
    let built = BuiltMessage {
        raw,
        message_id: row.message_id.clone(),
        recipients: envelope.recipients,
        from: envelope.from,
    };

    info!("[outbox] resubmitting row {} ({})", row.id, row.message_id);
    crate::outbox::mark_submission_started(store, row.id)?;
    let result = submit(&built, smtp_config).await?;
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

    let from_mailbox: Mailbox = normalize_address_for_smtp(from_address)
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
                .body(draft.body_markdown.clone()),
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
        let mut mixed = build_invite_mime_body(&draft.body_markdown, body_html, ics);
        if let Some(attachments) = &draft.frontmatter.attachments {
            for attachment_path in attachments {
                let expanded = shellexpand::tilde(attachment_path);
                let path = Path::new(expanded.as_ref());
                let file_content = fs::read(path)
                    .with_context(|| format!("Failed to read attachment: {}", attachment_path))?;
                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "attachment".to_string());
                let content_type = mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .to_string();
                let content_type_parsed = content_type.parse().unwrap_or_else(|_| {
                    "application/octet-stream".parse().expect("static MIME type")
                });
                mixed = mixed.singlepart(Attachment::new(filename).body(file_content, content_type_parsed));
            }
        }
        builder.multipart(mixed).context("Failed to build invite message")?
    } else if let Some(attachments) = &draft.frontmatter.attachments {
        if !attachments.is_empty() {
            let mut mixed = MultiPart::mixed().multipart(body_multipart);

            for attachment_path in attachments {
                let expanded = shellexpand::tilde(attachment_path);
                let path = Path::new(expanded.as_ref());

                let file_content = fs::read(path)
                    .with_context(|| format!("Failed to read attachment: {}", attachment_path))?;

                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "attachment".to_string());

                let content_type = mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .to_string();

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
        from: from_address.to_string(),
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
                    ambiguous: false,
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
                    ambiguous: false,
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
                    ambiguous: false,
                });
            }
            Err(e) => {
                let err_msg = format!("{}", e);
                error!("Failed to send to {} ({}): {}", addr, role, err_msg);
                results.push(RecipientResult {
                    address: addr.clone(),
                    role: *role,
                    success: false,
                    ambiguous: smtp_failure_is_ambiguous(&e),
                    error: Some(err_msg),
                });
            }
        }
    }

    Ok(SendResult { results })
}
