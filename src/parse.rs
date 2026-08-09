use anyhow::Result;
use colored::*;
use mailparse::{parse_mail, MailHeaderMap};
use std::fs;
use std::path::{Path, PathBuf};

/// Find the largest byte index <= `max_bytes` that lies on a UTF-8 char boundary.
///
/// Never lowercase or slice blindly for offset math: `to_lowercase()` can
/// change byte length, and a mid-character slice panics.
pub(crate) fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Filename of the iMIP calendar sidecar saved in each invite's attachments
/// directory. A fixed name (rather than the sender's, which is often just
/// `invite.ics`/`meeting.ics` anyway) keeps lookup deterministic for later
/// RSVP/reconciliation tickets and avoids collisions with real attachments.
pub const CALENDAR_SIDECAR_NAME: &str = "invite.ics";

#[derive(Debug, Clone)]
pub struct FetchedEmail {
    pub from: String,
    pub to: String,
    pub cc: Option<String>,
    pub subject: String,
    pub date: String,
    pub body_text: String,
    pub html_body: Option<String>,
    pub has_attachments: bool,
    pub message_id: Option<String>,
    pub attachments: Vec<AttachmentData>,
    /// What the server says has happened to this message: read, answered,
    /// forwarded (#TKT-0051). The parser cannot know any of it -- flags are
    /// mailbox state, not message bytes -- so it produces the empty set and
    /// the fetch fills it in from `FLAGS`.
    pub flags: crate::types::MessageFlags,
    /// Raw `text/calendar` payload (an iMIP invite), saved as a sidecar `.ics`
    /// next to the email. `None` when the email carries no calendar part.
    pub calendar_ics: Option<Vec<u8>>,
    /// Parsed `event:` frontmatter block, populated best-effort from
    /// `calendar_ics`. `None` when there is no calendar part or it was
    /// unparseable (the sidecar is still saved in the latter case).
    pub event: Option<crate::types::EventFrontmatter>,
}

#[derive(Debug, Clone)]
pub struct AttachmentData {
    pub filename: String,
    pub content: Vec<u8>,
    /// Content-ID (from the `Content-ID` header), used for inline images (`cid:` references).
    pub content_id: Option<String>,
}

pub fn html_to_plain(html: &str) -> String {
    html2text::config::plain()
        .use_doc_css()
        .no_table_borders()
        .no_link_wrapping()
        .string_from_read(html.as_bytes(), 10_000)
        .unwrap_or_else(|_| html.to_string())
}

/// Recursively collect the first text/plain and text/html parts from a parsed email.
pub fn extract_body_parts(parsed: &mailparse::ParsedMail) -> (Option<String>, Option<String>) {
    if parsed.ctype.mimetype == "text/plain" {
        let body = parsed.get_body().unwrap_or_default();
        if !body.is_empty() {
            return (Some(body), None);
        }
    }

    if parsed.ctype.mimetype == "text/html" {
        let body = parsed.get_body().unwrap_or_default();
        if !body.is_empty() {
            return (None, Some(body));
        }
    }

    let mut plain = None;
    let mut html = None;

    for sub in &parsed.subparts {
        let (sub_plain, sub_html) = extract_body_parts(sub);
        if plain.is_none() {
            plain = sub_plain;
        }
        if html.is_none() {
            html = sub_html;
        }
        if plain.is_some() && html.is_some() {
            break;
        }
    }

    (plain, html)
}

/// Extract body text from a parsed email.
/// Returns (plain_text, Option<html_body>).
pub fn extract_body_text(parsed: &mailparse::ParsedMail) -> (String, Option<String>) {
    let (plain, html) = extract_body_parts(parsed);

    if let Some(plain_text) = plain {
        (plain_text, html)
    } else if let Some(ref html_text) = html {
        (html_to_plain(html_text), html)
    } else {
        (String::new(), None)
    }
}

/// Check whether a MIME part should be treated as an attachment.
/// Matches: explicit `Content-Disposition: attachment`, inline images,
/// inline non-text parts that carry a filename (e.g. inline PDFs), and any
/// `.ics`/`text/calendar` part (so a non-invite calendar export is preserved as
/// a regular attachment; the single iMIP invite part is excluded separately by
/// the caller, which parses payloads to find it).
fn is_attachment_part(part: &mailparse::ParsedMail) -> bool {
    // A calendar part is only special when it is the actual iMIP invite lifted
    // to the sidecar; that exclusion happens in `collect_attachments` (it needs
    // the decoded payload). Every other `.ics`/`text/calendar` part is preserved
    // as a regular attachment (keeping its original filename, or a synthesized
    // `inline-N.ics` when it has none) so no calendar bytes are ever dropped.
    if is_calendar_part(part) {
        return true;
    }
    let disposition = part.get_content_disposition();
    if disposition.disposition == mailparse::DispositionType::Attachment {
        return true;
    }
    if disposition.disposition == mailparse::DispositionType::Inline {
        // Inline images (referenced via cid: in HTML).
        if part.ctype.mimetype.starts_with("image/") {
            return true;
        }
        // Inline non-text parts with a filename are effectively attachments
        // (e.g. PDFs sent with Content-Disposition: inline).
        if !part.ctype.mimetype.starts_with("text/")
            && !part.ctype.mimetype.starts_with("multipart/")
        {
            let has_filename = disposition.params.contains_key("filename")
                || part.ctype.params.contains_key("name");
            if has_filename {
                return true;
            }
        }
    }
    false
}

pub fn has_attachments(parsed: &mailparse::ParsedMail) -> bool {
    let invite = extract_calendar_ics(parsed);
    let mut skip = SkipInvite::new(invite.as_deref());
    has_attachments_inner(parsed, &mut skip)
}

fn has_attachments_inner(parsed: &mailparse::ParsedMail, skip: &mut SkipInvite) -> bool {
    for sub in &parsed.subparts {
        if is_attachment_part(sub) && !skip.is_invite(sub) {
            return true;
        }
        if has_attachments_inner(sub, skip) {
            return true;
        }
    }
    false
}

/// Extract all attachments from a parsed email, recursing through MIME subparts.
/// The single iMIP invite part (lifted to the `.ics` sidecar) is excluded so it
/// is not also stored as a regular attachment; every other `.ics`/calendar part
/// is preserved.
pub fn extract_attachments(parsed: &mailparse::ParsedMail) -> Vec<AttachmentData> {
    let mut attachments = Vec::new();
    let mut counter = 0usize;
    let invite = extract_calendar_ics(parsed);
    let mut skip = SkipInvite::new(invite.as_deref());
    collect_attachments(parsed, &mut attachments, &mut counter, &mut skip);
    attachments
}

/// Tracks the single iMIP invite part to exclude from the attachment list. The
/// invite is identified by its decoded payload bytes; only the FIRST calendar
/// part matching those bytes is skipped (matching `extract_calendar_ics`, which
/// returns the first invite in the same traversal order).
struct SkipInvite<'a> {
    invite_bytes: Option<&'a [u8]>,
    skipped: bool,
}

impl<'a> SkipInvite<'a> {
    fn new(invite_bytes: Option<&'a [u8]>) -> Self {
        SkipInvite {
            invite_bytes,
            skipped: false,
        }
    }

    /// Returns true (once) for the part whose decoded body is the iMIP invite.
    fn is_invite(&mut self, part: &mailparse::ParsedMail) -> bool {
        if self.skipped {
            return false;
        }
        let Some(bytes) = self.invite_bytes else {
            return false;
        };
        if is_calendar_part(part) && part.get_body_raw().ok().as_deref() == Some(bytes) {
            self.skipped = true;
            return true;
        }
        false
    }
}

fn collect_attachments(
    parsed: &mailparse::ParsedMail,
    attachments: &mut Vec<AttachmentData>,
    counter: &mut usize,
    skip: &mut SkipInvite,
) {
    if is_attachment_part(parsed) && !skip.is_invite(parsed) {
        let disposition = parsed.get_content_disposition();
        let filename = disposition
            .params
            .get("filename")
            .or_else(|| parsed.ctype.params.get("name"))
            .cloned()
            .unwrap_or_else(|| {
                *counter += 1;
                let ext = mime_ext_for(&parsed.ctype.mimetype);
                format!("inline-{}.{}", counter, ext)
            });
        let filename = sanitize_attachment_filename(&filename);
        let content_id = parsed
            .headers
            .iter()
            .find(|h| h.get_key().eq_ignore_ascii_case("Content-ID"))
            .and_then(|h| {
                let val = h.get_value();
                // Strip angle brackets: <id@host> -> id@host
                Some(val.trim_start_matches('<').trim_end_matches('>').to_string())
            });
        if let Ok(content) = parsed.get_body_raw() {
            attachments.push(AttachmentData {
                filename,
                content,
                content_id,
            });
        }
    }
    for sub in &parsed.subparts {
        collect_attachments(sub, attachments, counter, skip);
    }
}

/// Recursively find the first calendar part that is an actual iMIP invite --
/// i.e. its decoded payload parses as a `VCALENDAR` carrying a `METHOD` property
/// (REQUEST/REPLY/CANCEL/...) -- and return its raw decoded bytes. Matches both
/// inline `text/calendar` parts and `.ics` attachments. A calendar part without
/// a `METHOD` (e.g. a plain `.ics` export) is NOT an invite and is left to the
/// regular attachment path with its original filename.
pub fn extract_calendar_ics(parsed: &mailparse::ParsedMail) -> Option<Vec<u8>> {
    if is_calendar_part(parsed) {
        if let Ok(raw) = parsed.get_body_raw() {
            if !raw.is_empty() && crate::calendar::is_imip_invite(&raw) {
                return Some(raw);
            }
        }
    }
    for sub in &parsed.subparts {
        if let Some(ics) = extract_calendar_ics(sub) {
            return Some(ics);
        }
    }
    None
}

/// Whether a MIME part is an iMIP calendar payload (inline or `.ics` attachment).
fn is_calendar_part(part: &mailparse::ParsedMail) -> bool {
    if crate::calendar::is_calendar_mimetype(&part.ctype.mimetype) {
        return true;
    }
    // Some senders attach the invite as application/octet-stream named *.ics.
    let filename = part
        .get_content_disposition()
        .params
        .get("filename")
        .or_else(|| part.ctype.params.get("name"))
        .cloned();
    filename
        .as_deref()
        .map(crate::calendar::is_ics_filename)
        .unwrap_or(false)
}

/// Map a MIME type to a reasonable file extension.
fn mime_ext_for(mime: &str) -> &str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "text/calendar" | "application/ics" => "ics",
        _ => "bin",
    }
}

pub(crate) fn sanitize_attachment_filename(name: &str) -> String {
    let name = name.replace(['/', '\\', '\0'], "_");
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim().to_string();
    if name.is_empty() {
        "attachment.bin".to_string()
    } else if name.len() > 200 {
        name[..floor_char_boundary(&name, 200)].to_string()
    } else {
        name
    }
}

/// Sanitize a Message-ID for use as a directory name.
/// Strips angle brackets, replaces path-unsafe characters with `_`, drops control
/// chars, trims, and truncates to 200 bytes (UTF-8-safe). If the result is empty,
/// returns `unknown-mid-<sha256[:16]>` of the original input.
pub fn sanitize_message_id_for_path(mid: &str) -> String {
    use sha2::{Digest, Sha256};

    let trimmed = mid.trim().trim_start_matches('<').trim_end_matches('>');
    let replaced: String = trimmed
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = replaced.trim().to_string();
    if cleaned.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(mid.as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().take(8).map(|b| format!("{:02x}", b)).collect();
        return format!("unknown-mid-{}", hex);
    }
    if cleaned.len() > 200 {
        cleaned[..floor_char_boundary(&cleaned, 200)].to_string()
    } else {
        cleaned
    }
}

/// Return the per-account stable attachments directory for a Message-ID:
/// `<account_dir>/attachments/<sanitized-message-id>/`.
pub fn stable_attachments_dir(account_dir: &Path, message_id: &str) -> PathBuf {
    account_dir
        .join("attachments")
        .join(sanitize_message_id_for_path(message_id))
}

/// The temp directory a message's files are materialised into, created
/// private to the current user.
///
/// One function for both halves of the product: `mp open` and the list's `o`
/// put the same message's files in the same place, and the bytes are
/// rewritten before every open, so a directory two accounts share (row ids
/// are per-account) never hands the opener a stale file -- only the paths
/// just written are returned. `stem` keys it: the row id for a stored
/// message, `search-<n>` for a hit that resolved to no row.
///
/// The name is predictable, so the directory is not trusted: on a shared host
/// `$TMPDIR` is world-writable, and a directory (or a symlink to one) an
/// attacker created first would otherwise receive the message bytes. It is
/// created 0o700, a pre-existing one must be a real directory owned by this
/// user, and a loose mode left by an older build is tightened rather than
/// used.
pub fn materialisation_dir(stem: &str) -> Result<PathBuf> {
    let dir = materialisation_root().join(format!("mailypoppins-{stem}"));
    create_private_dir(&dir)?;
    Ok(dir)
}

/// The directory message files are materialised under: `$TMPDIR` in a real
/// run, a per-thread directory in a test binary.
///
/// The test seam is here rather than in `$TMPDIR` (#0077): overriding the
/// variable is a process-global write that every parallel `tempfile::tempdir()`
/// reads, and the per-row directory name is otherwise the very path a live
/// `mp open` of that row uses. Keying the root on the test thread also keeps
/// two tests that materialise the same row id off each other's files.
fn materialisation_root() -> PathBuf {
    #[cfg(test)]
    {
        test_temp_root()
    }
    #[cfg(not(test))]
    {
        std::env::temp_dir()
    }
}

/// Per-process, per-thread materialisation root for the test binary.
///
/// Never removed while the process runs: another thread may still be reading
/// under it. Everything lands under one `mailypoppins-tests/` parent, so a
/// run's leftovers are `rm -rf "${TMPDIR:-/tmp}/mailypoppins-tests"`.
#[cfg(test)]
pub(crate) fn test_temp_root() -> PathBuf {
    let thread = std::thread::current();
    let key = match thread.name() {
        Some(name) => name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>(),
        None => format!("{:?}", thread.id())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>(),
    };
    let dir = std::env::temp_dir()
        .join("mailypoppins-tests")
        .join(std::process::id().to_string())
        .join(key);
    std::fs::create_dir_all(&dir).expect("test temp root");
    dir
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(anyhow::anyhow!("creating {}: {e}", dir.display()));
        }
    }
    // `symlink_metadata`, not `metadata`: a symlink pointing at a directory
    // someone else owns must be rejected, not followed.
    let meta = fs::symlink_metadata(dir)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?;
    if !meta.is_dir() {
        anyhow::bail!(
            "{} exists and is not a directory; refusing to materialise message files there",
            dir.display()
        );
    }
    // Safe: getuid() is always defined on POSIX, never fails.
    let uid = unsafe { libc_getuid() };
    if meta.uid() != uid {
        anyhow::bail!(
            "{} is owned by another user; refusing to materialise message files there",
            dir.display()
        );
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow::anyhow!("restricting {} to 0700: {e}", dir.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    // WSL is unix; native Windows is not a target, so there is no mode to set.
    fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))
}

/// Open a file with the system default application (macOS `open`).
pub fn open_file_with_system(path: &Path) -> Result<()> {
    let status = std::process::Command::new("open")
        .arg(path)
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to run 'open': {e}"))?;
    if !status.success() {
        anyhow::bail!("'open' exited with status {}", status);
    }
    Ok(())
}

/// Copy an attachment file to `dest_dir`, returning the final path.
/// If a file with the same name already exists, appends `_1`, `_2`, etc.
pub fn save_attachment(source: &Path, dest_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)?;

    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Source has no file name"))?
        .to_string_lossy();
    let stem = source
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = source
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    let mut dest = dest_dir.join(file_name.as_ref());
    let mut counter = 1u32;
    while dest.exists() {
        dest = dest_dir.join(format!("{stem}_{counter}{ext}"));
        counter += 1;
    }

    fs::copy(source, &dest)?;
    Ok(dest)
}

/// Parse raw RFC822 bytes into a FetchedEmail struct.
pub fn parse_rfc822_to_fetched_email(rfc822_body: &[u8]) -> Option<FetchedEmail> {
    let parsed = parse_mail(rfc822_body).ok()?;
    let headers = &parsed.headers;
    let from = headers
        .get_first_value("From")
        .unwrap_or_else(|| "(unknown)".to_string());
    let to = headers
        .get_first_value("To")
        .unwrap_or_else(|| "(unknown)".to_string());
    let cc = headers.get_first_value("Cc");
    let subject = headers
        .get_first_value("Subject")
        .unwrap_or_else(|| "(no subject)".to_string());
    let date = headers
        .get_first_value("Date")
        .unwrap_or_else(|| "(unknown date)".to_string());
    let message_id = headers
        .get_first_value("Message-ID")
        .or_else(|| headers.get_first_value("Message-Id"));
    let (body_text, html_body) = extract_body_text(&parsed);
    let has_att = has_attachments(&parsed);
    let att_data = extract_attachments(&parsed);
    let calendar_ics = extract_calendar_ics(&parsed);
    // Best-effort: a malformed invite still saves the sidecar, just no event block.
    let event = calendar_ics
        .as_deref()
        .and_then(crate::calendar::parse_ics)
        .map(|ev| crate::calendar::event_frontmatter(&ev));

    Some(FetchedEmail {
        from,
        to,
        cc,
        subject,
        date,
        body_text,
        html_body,
        has_attachments: has_att,
        message_id,
        attachments: att_data,
        flags: crate::types::MessageFlags::default(),
        calendar_ics,
        event,
    })
}

/// Compress a sorted list of UIDs into IMAP sequence set format using ranges.
/// e.g., `[1,2,3,5,7,8,9]` -> `"1:3,5,7:9"`
pub fn compress_uid_set(uids: &[u32]) -> String {
    if uids.is_empty() {
        return String::new();
    }
    let mut sorted = uids.to_vec();
    sorted.sort();

    let mut ranges = Vec::new();
    let mut start = sorted[0];
    let mut end = sorted[0];

    for &uid in &sorted[1..] {
        if uid == end + 1 {
            end = uid;
        } else {
            if start == end {
                ranges.push(start.to_string());
            } else {
                ranges.push(format!("{}:{}", start, end));
            }
            start = uid;
            end = uid;
        }
    }
    if start == end {
        ranges.push(start.to_string());
    } else {
        ranges.push(format!("{}:{}", start, end));
    }
    ranges.join(",")
}

pub fn slugify_subject(subject: &str) -> String {
    let slug: String = subject
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else if c == ' ' { '-' } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("");
    let result = crate::types::collapse_hyphens(&slug);
    if result.len() > 40 {
        // Truncate at nearest char boundary <= 40 bytes
        let end = floor_char_boundary(&result, 40);
        let truncated = &result[..end];
        truncated.trim_end_matches('-').to_string()
    } else {
        result
    }
}

pub fn slugify_sender(from: &str) -> String {
    // Extract display name if present (e.g. "John Doe <john@example.com>" -> "John Doe")
    // Otherwise use the local part of the email address
    let name = if let Some(start) = from.find('<') {
        let display = from[..start].trim().trim_matches('"');
        if display.is_empty() {
            // No display name, use local part of email
            let email = &from[start + 1..from.find('>').unwrap_or(from.len())];
            email.split('@').next().unwrap_or("unknown").to_string()
        } else {
            display.to_string()
        }
    } else if from.contains('@') {
        from.split('@').next().unwrap_or("unknown").to_string()
    } else {
        from.to_string()
    };

    // Slugify the name
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    crate::types::collapse_hyphens(&slug)
}

pub fn extract_email_address(raw: &str) -> String {
    if let Some(start) = raw.find('<') {
        if let Some(end) = raw.find('>') {
            return raw[start + 1..end].trim().to_string();
        }
    }
    raw.trim().to_string()
}

pub fn display_fetched_emails(emails: &[FetchedEmail], full_body: bool) {
    if emails.is_empty() {
        println!("No emails found matching the criteria.");
        return;
    }

    println!(
        "\n{} ({} result{})\n",
        "Fetched Emails".bold().cyan(),
        emails.len(),
        if emails.len() == 1 { "" } else { "s" }
    );

    for (i, email) in emails.iter().enumerate() {
        println!("{}", "─".repeat(60));
        println!("{}: {}", "From".bold().green(), email.from);
        println!("{}: {}", "To".bold().blue(), email.to);
        if let Some(ref cc) = email.cc {
            println!("{}: {}", "Cc".bold().blue(), cc);
        }
        println!("{}: {}", "Subject".bold().yellow(), email.subject);
        let date_display = if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(&email.date) {
            dt.format("%Y-%m-%d").to_string()
        } else {
            email.date.clone()
        };
        println!("{}: {}", "Date".bold().magenta(), date_display);
        if email.has_attachments {
            println!("{}", "[has attachments]".yellow());
        }

        println!();
        if full_body {
            println!("{}", email.body_text);
        } else {
            let preview: String = email.body_text.chars().take(300).collect();
            println!("{}", preview);
            if email.body_text.len() > 300 {
                println!("{}", "...".dimmed());
            }
        }

        if i < emails.len() - 1 {
            println!();
        }
    }
    println!("{}", "─".repeat(60));
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // compress_uid_set
    // -----------------------------------------------------------------------

    #[test]
    fn test_compress_uid_set_ranges() {
        assert_eq!(compress_uid_set(&[1, 2, 3, 5, 7, 8, 9]), "1:3,5,7:9");
    }

    #[test]
    fn test_compress_uid_set_empty() {
        assert_eq!(compress_uid_set(&[]), "");
    }

    #[test]
    fn test_compress_uid_set_single() {
        assert_eq!(compress_uid_set(&[42]), "42");
    }

    #[test]
    fn test_compress_uid_set_unsorted() {
        assert_eq!(compress_uid_set(&[5, 3, 1, 2, 4]), "1:5");
    }

    #[test]
    fn test_compress_uid_set_non_contiguous() {
        assert_eq!(compress_uid_set(&[1, 3, 5]), "1,3,5");
    }

    // -----------------------------------------------------------------------
    // slugify_subject
    // -----------------------------------------------------------------------

    #[test]
    fn test_slugify_subject_normal() {
        assert_eq!(slugify_subject("Hello World"), "hello-world");
    }

    #[test]
    fn test_slugify_subject_unicode() {
        let result = slugify_subject("Prufung Ergebnis");
        assert!(result.contains("prufung"));
    }

    #[test]
    fn test_slugify_subject_empty() {
        assert_eq!(slugify_subject(""), "");
    }

    #[test]
    fn test_slugify_subject_long() {
        let long = "a ".repeat(30);
        let result = slugify_subject(&long);
        assert!(result.len() <= 40);
    }

    #[test]
    fn test_slugify_subject_special_chars() {
        assert_eq!(slugify_subject("Re: Hello! @#$ World?"), "re-hello-world");
    }

    #[test]
    fn test_slugify_subject_consecutive_hyphens() {
        assert_eq!(slugify_subject("hello   world"), "hello-world");
    }

    // -----------------------------------------------------------------------
    // slugify_sender
    // -----------------------------------------------------------------------

    #[test]
    fn test_slugify_sender_display_name() {
        assert_eq!(slugify_sender("John Doe <john@example.com>"), "john-doe");
    }

    #[test]
    fn test_slugify_sender_bare_email() {
        assert_eq!(slugify_sender("john@example.com"), "john");
    }

    #[test]
    fn test_slugify_sender_no_display_name() {
        assert_eq!(slugify_sender("<john@example.com>"), "john");
    }

    #[test]
    fn test_slugify_sender_quoted_display_name() {
        assert_eq!(slugify_sender("\"John Doe\" <john@example.com>"), "john-doe");
    }

    // -----------------------------------------------------------------------
    // extract_email_address
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_email_address_angle_brackets() {
        assert_eq!(extract_email_address("John Doe <john@x.com>"), "john@x.com");
    }

    #[test]
    fn test_extract_email_address_bare() {
        assert_eq!(extract_email_address("john@x.com"), "john@x.com");
    }

    #[test]
    fn test_extract_email_address_whitespace() {
        assert_eq!(extract_email_address("  john@x.com  "), "john@x.com");
    }

    // -----------------------------------------------------------------------
    // sanitize_attachment_filename
    // -----------------------------------------------------------------------

    #[test]
    fn test_sanitize_attachment_filename_normal() {
        assert_eq!(sanitize_attachment_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn test_sanitize_attachment_filename_slashes() {
        assert_eq!(sanitize_attachment_filename("path/to/file.pdf"), "path_to_file.pdf");
    }

    #[test]
    fn test_sanitize_attachment_filename_path_traversal() {
        assert_eq!(sanitize_attachment_filename("../../evil"), ".._.._evil");
        assert_eq!(
            sanitize_attachment_filename("..\\..\\evil.exe"),
            ".._.._evil.exe"
        );
    }

    #[test]
    fn test_sanitize_attachment_filename_control_chars() {
        assert_eq!(sanitize_attachment_filename("file\x00name.pdf"), "file_name.pdf");
    }

    #[test]
    fn test_sanitize_attachment_filename_empty() {
        assert_eq!(sanitize_attachment_filename(""), "attachment.bin");
    }

    #[test]
    fn test_sanitize_attachment_filename_long() {
        let long = "a".repeat(250);
        let result = sanitize_attachment_filename(&long);
        assert!(result.len() <= 200);
    }

    // -----------------------------------------------------------------------
    // sanitize_message_id_for_path
    // -----------------------------------------------------------------------

    #[test]
    fn test_sanitize_message_id_for_path_normal() {
        assert_eq!(
            sanitize_message_id_for_path("<abc123@example.com>"),
            "abc123@example.com"
        );
    }

    #[test]
    fn test_sanitize_message_id_for_path_no_brackets() {
        assert_eq!(
            sanitize_message_id_for_path("abc123@example.com"),
            "abc123@example.com"
        );
    }

    #[test]
    fn test_sanitize_message_id_for_path_slashes_and_colons() {
        assert_eq!(
            sanitize_message_id_for_path("<a/b\\c:d@x.com>"),
            "a_b_c_d@x.com"
        );
    }

    #[test]
    fn test_sanitize_message_id_for_path_control_chars() {
        assert_eq!(
            sanitize_message_id_for_path("<a\nb\tc@x.com>"),
            "a_b_c@x.com"
        );
    }

    #[test]
    fn test_sanitize_message_id_for_path_empty_falls_back_to_hash() {
        let result = sanitize_message_id_for_path("");
        assert!(result.starts_with("unknown-mid-"));
        assert_eq!(result.len(), "unknown-mid-".len() + 16);
    }

    #[test]
    fn test_sanitize_message_id_for_path_only_brackets_falls_back() {
        let result = sanitize_message_id_for_path("<>");
        assert!(result.starts_with("unknown-mid-"));
    }

    #[test]
    fn test_sanitize_message_id_for_path_truncates() {
        let long = format!("<{}@example.com>", "a".repeat(300));
        let result = sanitize_message_id_for_path(&long);
        assert!(result.len() <= 200);
    }

    // -----------------------------------------------------------------------
    // stable_attachments_dir
    // -----------------------------------------------------------------------

    #[test]
    fn test_stable_attachments_dir_layout() {
        let acct = Path::new("/data/accounts/tum");
        assert_eq!(
            stable_attachments_dir(acct, "<m@x.com>"),
            PathBuf::from("/data/accounts/tum/attachments/m@x.com")
        );
    }

    // -----------------------------------------------------------------------
    // floor_char_boundary
    // -----------------------------------------------------------------------

    #[test]
    fn test_floor_char_boundary_ascii() {
        assert_eq!(floor_char_boundary("hello", 3), 3);
    }

    #[test]
    fn test_floor_char_boundary_multibyte() {
        // "ae" is U+00E4, 2 bytes in UTF-8
        let s = "\u{00E4}bc";
        // Byte 1 is in the middle of the 2-byte char -> should clamp to 0
        assert_eq!(floor_char_boundary(s, 1), 0);
        // Byte 2 is the start of 'b'
        assert_eq!(floor_char_boundary(s, 2), 2);
    }

    #[test]
    fn test_floor_char_boundary_exact() {
        assert_eq!(floor_char_boundary("abc", 10), 3);
    }

    // -----------------------------------------------------------------------
    // parse_rfc822_to_fetched_email
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_rfc822_minimal() {
        let raw = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: Test\r\nDate: Mon, 01 Jan 2024 12:00:00 +0000\r\nMessage-ID: <test123@example.com>\r\n\r\nHello world";
        let email = parse_rfc822_to_fetched_email(raw).expect("should parse");
        assert_eq!(email.from, "alice@example.com");
        assert_eq!(email.to, "bob@example.com");
        assert_eq!(email.subject, "Test");
        assert!(email.body_text.contains("Hello world"));
        assert_eq!(email.message_id, Some("<test123@example.com>".to_string()));
        assert!(!email.has_attachments);
    }

    #[test]
    fn test_parse_rfc822_missing_fields() {
        let raw = b"\r\nBody only";
        let email = parse_rfc822_to_fetched_email(raw).expect("should parse");
        assert_eq!(email.from, "(unknown)");
        assert_eq!(email.subject, "(no subject)");
    }

    // -----------------------------------------------------------------------
    // html_to_plain (existing tests below)
    // -----------------------------------------------------------------------

    #[test]
    fn test_html_to_plain_preserves_paragraph_breaks() {
        let html = "<p>First paragraph</p><p>Second paragraph</p>";
        let result = html_to_plain(html);
        let non_empty: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(non_empty.len(), 2, "Expected 2 non-empty lines, got: {:?}", non_empty);
        assert!(non_empty[0].contains("First paragraph"));
        assert!(non_empty[1].contains("Second paragraph"));
    }

    #[test]
    fn test_html_to_plain_no_hard_wrap() {
        // A single paragraph with a 200-char word -- must not be hard-wrapped at 80.
        let long_word = "a".repeat(200);
        let html = format!("<p>{}</p>", long_word);
        let result = html_to_plain(&html);
        let non_empty: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(non_empty.len(), 1, "Expected 1 non-empty line (no hard wrap), got: {:?}", non_empty);
        assert!(non_empty[0].contains(&long_word));
    }

    #[test]
    fn test_html_to_plain_blockquote_markers() {
        let html = "<blockquote>quoted text</blockquote>";
        let result = html_to_plain(html);
        let non_empty: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(!non_empty.is_empty(), "Expected at least one non-empty line");
        for line in &non_empty {
            assert!(line.starts_with("> "), "Expected line to start with '> ', got: {:?}", line);
        }
    }

    #[test]
    fn test_html_to_plain_nested_blockquote() {
        let html = "<blockquote><blockquote>deep quote</blockquote></blockquote>";
        let result = html_to_plain(html);
        let non_empty: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(!non_empty.is_empty(), "Expected at least one non-empty line");
        for line in &non_empty {
            assert!(
                line.starts_with("> > "),
                "Expected line to start with '> > ', got: {:?}",
                line
            );
        }
    }

    #[test]
    fn test_html_to_plain_no_table_borders() {
        let html = "<table><tr><td>cell</td></tr></table>";
        let result = html_to_plain(html);
        assert!(!result.contains('+'), "Output should not contain '+' table border chars: {:?}", result);
        assert!(!result.contains("---"), "Output should not contain '---' table border chars: {:?}", result);
        assert!(result.contains("cell"), "Output should contain the cell text");
    }

    #[test]
    fn test_html_to_plain_fallback_on_error() {
        // Empty string should not panic
        let result = html_to_plain("");
        // Just verify it returns without panicking
        let _ = result;
    }

    // -----------------------------------------------------------------------
    // compress_uid_set -- additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compress_uid_set_duplicates() {
        // compress_uid_set does not deduplicate; duplicates break ranges
        assert_eq!(compress_uid_set(&[1, 1, 2, 2, 3]), "1,1:2,2:3");
    }

    #[test]
    fn test_compress_uid_set_two_elements_contiguous() {
        assert_eq!(compress_uid_set(&[10, 11]), "10:11");
    }

    #[test]
    fn test_compress_uid_set_large_gap() {
        assert_eq!(compress_uid_set(&[1, 1000]), "1,1000");
    }

    // -----------------------------------------------------------------------
    // slugify_sender -- additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_slugify_sender_plain_name() {
        assert_eq!(slugify_sender("Alice"), "alice");
    }

    #[test]
    fn test_slugify_sender_empty() {
        assert_eq!(slugify_sender(""), "");
    }

    #[test]
    fn test_slugify_sender_email_only_angle_brackets_no_local() {
        // Edge: angle brackets with empty local part
        assert_eq!(slugify_sender("<@example.com>"), "");
    }

    // -----------------------------------------------------------------------
    // slugify_subject -- additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_slugify_subject_only_special_chars() {
        assert_eq!(slugify_subject("!@#$%^&*()"), "");
    }

    #[test]
    fn test_slugify_subject_leading_trailing_spaces() {
        assert_eq!(slugify_subject("  hello world  "), "hello-world");
    }

    // -----------------------------------------------------------------------
    // extract_email_address -- additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_email_address_malformed_angle_brackets() {
        // Only opening bracket, no closing
        assert_eq!(extract_email_address("John <john@x.com"), "John <john@x.com");
    }

    #[test]
    fn test_extract_email_address_empty() {
        assert_eq!(extract_email_address(""), "");
    }

    // -----------------------------------------------------------------------
    // floor_char_boundary -- additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_floor_char_boundary_empty_string() {
        assert_eq!(floor_char_boundary("", 5), 0);
    }

    #[test]
    fn test_floor_char_boundary_zero() {
        assert_eq!(floor_char_boundary("hello", 0), 0);
    }

    #[test]
    fn test_floor_char_boundary_emoji() {
        // Emoji is 4 bytes in UTF-8
        let s = "\u{1F600}abc"; // grinning face + "abc"
        assert_eq!(floor_char_boundary(s, 1), 0); // mid emoji
        assert_eq!(floor_char_boundary(s, 4), 4); // exactly after emoji
        assert_eq!(floor_char_boundary(s, 5), 5); // after 'a'
    }

    // -----------------------------------------------------------------------
    // parse_rfc822_to_fetched_email -- additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_rfc822_with_cc() {
        let raw = b"From: a@x.com\r\nTo: b@x.com\r\nCc: c@x.com, d@x.com\r\nSubject: Test\r\nDate: Mon, 01 Jan 2024 12:00:00 +0000\r\n\r\nBody";
        let email = parse_rfc822_to_fetched_email(raw).expect("should parse");
        assert_eq!(email.cc, Some("c@x.com, d@x.com".to_string()));
    }

    #[test]
    fn test_parse_rfc822_html_only() {
        let raw = b"From: a@x.com\r\nTo: b@x.com\r\nSubject: HTML\r\nDate: Mon, 01 Jan 2024 12:00:00 +0000\r\nContent-Type: text/html\r\n\r\n<p>Hello</p>";
        let email = parse_rfc822_to_fetched_email(raw).expect("should parse");
        assert!(email.html_body.is_some());
        assert!(email.body_text.contains("Hello"));
    }

    #[test]
    fn test_parse_rfc822_empty_body() {
        let raw = b"From: a@x.com\r\nTo: b@x.com\r\nSubject: Empty\r\nDate: Mon, 01 Jan 2024 12:00:00 +0000\r\n\r\n";
        let email = parse_rfc822_to_fetched_email(raw).expect("should parse");
        assert!(email.body_text.is_empty() || email.body_text.trim().is_empty());
    }

    // -----------------------------------------------------------------------
    // save_attachment
    // -----------------------------------------------------------------------

    #[test]
    fn test_save_attachment_basic() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let source = src_dir.path().join("report.pdf");
        std::fs::write(&source, b"pdf data").unwrap();

        let result = save_attachment(&source, dest_dir.path()).unwrap();
        assert_eq!(result, dest_dir.path().join("report.pdf"));
        assert_eq!(std::fs::read(&result).unwrap(), b"pdf data");
    }

    #[test]
    fn test_save_attachment_conflict_appends_suffix() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let source = src_dir.path().join("report.pdf");
        std::fs::write(&source, b"original").unwrap();
        // Pre-create a file with the same name
        std::fs::write(dest_dir.path().join("report.pdf"), b"existing").unwrap();

        let result = save_attachment(&source, dest_dir.path()).unwrap();
        assert_eq!(result, dest_dir.path().join("report_1.pdf"));
        assert_eq!(std::fs::read(&result).unwrap(), b"original");
        // Original file should be untouched
        assert_eq!(
            std::fs::read(dest_dir.path().join("report.pdf")).unwrap(),
            b"existing"
        );
    }

    #[test]
    fn test_save_attachment_creates_dest_dir() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let dest_subdir = dest_dir.path().join("nested").join("sub");
        let source = src_dir.path().join("file.txt");
        std::fs::write(&source, b"data").unwrap();

        let result = save_attachment(&source, &dest_subdir).unwrap();
        assert!(result.exists());
        assert_eq!(std::fs::read(&result).unwrap(), b"data");
    }

    /// The materialisation directory is created private to this user, and a
    /// path an attacker could have put there first is refused rather than
    /// written into: `$TMPDIR` is world-writable and the name is predictable.
    ///
    /// `create_private_dir` is exercised directly rather than through
    /// `materialisation_dir`, which would need `$TMPDIR` moved under the whole
    /// test process.
    #[cfg(unix)]
    #[test]
    fn a_materialisation_dir_is_private_and_never_an_attackers_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let mode_of = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // Created fresh: 0700, whatever the umask says.
        let fresh = root.path().join("mailypoppins-1");
        create_private_dir(&fresh).unwrap();
        assert_eq!(mode_of(&fresh), 0o700, "{:o}", mode_of(&fresh));

        // Ours already, but left group- and world-readable by an older build:
        // tightened, not used as found.
        fs::set_permissions(&fresh, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&fresh).unwrap();
        assert_eq!(mode_of(&fresh), 0o700, "{:o}", mode_of(&fresh));

        // A file where the directory should be: refused.
        let as_file = root.path().join("mailypoppins-2");
        fs::write(&as_file, b"not a directory").unwrap();
        let err = create_private_dir(&as_file).unwrap_err().to_string();
        assert!(err.contains("is not a directory"), "{err}");

        // A symlink pointing at a directory elsewhere: refused, not followed,
        // so the message bytes cannot be redirected out of the temp area.
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let link = root.path().join("mailypoppins-3");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let err = create_private_dir(&link).unwrap_err().to_string();
        assert!(err.contains("is not a directory"), "{err}");
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn test_save_attachment_no_extension() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let source = src_dir.path().join("Makefile");
        std::fs::write(&source, b"data").unwrap();
        // Pre-create a conflict
        std::fs::write(dest_dir.path().join("Makefile"), b"existing").unwrap();

        let result = save_attachment(&source, dest_dir.path()).unwrap();
        assert_eq!(result, dest_dir.path().join("Makefile_1"));
        assert_eq!(std::fs::read(&result).unwrap(), b"data");
    }
}
