use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use colored::*;
use gray_matter::{engine::YAML, Matter};
use log::info;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::{EmailSettings, SmtpConfig};
use crate::parse::{
    extract_email_address, slugify_sender, slugify_subject, stable_attachments_dir,
};
use crate::types::{EmailDraft, EmailFrontmatter, EmailStatus, InboxFrontmatter};

/// Frontmatter skeleton for a brand-new empty draft (CLI `mp new` and TUI `n`).
/// `attachments:` is intentionally a bare key: it deserializes to `None` via the
/// serde default, unlike `attachments: []` which yields `Some(vec![])`.
///
/// `subject: ""` is explicit rather than bare, because a file this build writes
/// has to be readable by this build: a bare key is YAML null, and the draft
/// `mp new` had just created was skipped by the index and unreachable through
/// the selector `mp new` printed (#0050). The parser tolerates the bare form
/// too (see [`crate::types::EmailFrontmatter`]), so an agent's draft still
/// indexes; the file we write does not lean on that tolerance.
pub fn new_draft_skeleton(from: &str, date: &str) -> String {
    new_draft_skeleton_with_id(from, date, &crate::store::drafts::new_id())
}

/// [`new_draft_skeleton`] with the `id:` chosen by the caller, which is what
/// lets `mp new` print the selector of the draft it just wrote without reading
/// the file back.
///
/// `id:` is first so it survives an editor session that reorders nothing and a
/// human eye that reads the top of the file; it is the draft's identity under
/// #0050, and the whole point is that it is preserved rather than regenerated.
pub fn new_draft_skeleton_with_id(from: &str, date: &str, id: &str) -> String {
    format!("---\nid: {id}\nto:\ncc:\nbcc:\nsubject: \"\"\nstatus: draft\nfrom: {from}\ndate: {date}\nreply_to:\nattachments:\n---\n\n")
}

/// Write an `id:` into an existing draft's frontmatter, in place, preserving
/// the body and every other field byte for byte.
///
/// This is the drafts index's only write to a draft file: a file an agent
/// created without an `id:` is assigned one on the first refresh, so that the
/// selector it is listed under is the selector the file itself carries.
pub fn set_draft_id(path: &Path, id: &str) -> Result<()> {
    rewrite_frontmatter_scalars_at(path, &[("id", FieldWrite::Set(id.to_string()))])
}

pub fn select_inbox_email(inbox_dir: &Path, prompt: &str) -> Result<PathBuf> {
    let mut entries: Vec<(PathBuf, InboxFrontmatter)> = Vec::new();

    for entry in WalkDir::new(inbox_dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let matter = Matter::<YAML>::new();
            let parsed = matter.parse(&content);
            if let Some(data) = parsed.data {
                if let Ok(fm) = data.deserialize::<InboxFrontmatter>() {
                    entries.push((path.to_path_buf(), fm));
                }
            }
        }
    }

    if entries.is_empty() {
        return Err(anyhow!("No inbox emails found in {}", inbox_dir.display()));
    }

    // Sort by date descending (most recent first)
    entries.sort_by(|a, b| {
        let da = a.1.date.as_deref().unwrap_or("");
        let db = b.1.date.as_deref().unwrap_or("");
        db.cmp(da)
    });

    // Build display strings for the fuzzy selector
    let display_items: Vec<String> = entries
        .iter()
        .map(|(_, fm)| {
            let date_short = fm
                .date
                .as_deref()
                .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let from_short = extract_email_address(&fm.from);
            format!("{} | {} | {}", date_short, from_short, fm.subject)
        })
        .collect();

    let selection = dialoguer::FuzzySelect::new()
        .with_prompt(prompt)
        .items(&display_items)
        .default(0)
        .interact()
        .context("Selection cancelled")?;

    Ok(entries[selection].0.clone())
}

/// The message a reply or a forward is built from, independent of where it
/// came from.
///
/// #0050 is why this type exists: received mail is a store row now, not a
/// `.md` file, so the draft builders cannot start by reading a path. They take
/// this instead: [`source_from_row`] is the one way to build it, off a store
/// row and its blobs, for the CLI and the TUI alike (#0052).
#[derive(Debug, Clone, Default)]
pub struct SourceMessage {
    pub from: String,
    pub to: String,
    pub cc: Option<String>,
    pub subject: String,
    /// The `Date:` header verbatim, used in the attribution line.
    pub date: Option<String>,
    /// Plain-text body of the source message.
    pub body: String,
    /// Absolute paths of the attachments a forward should carry.
    pub attachments: Vec<PathBuf>,
    /// Rendered HTML of the source, when one exists, for the draft's companion
    /// `.html` (the quoted block the send path inlines).
    pub html: Option<String>,
}

/// Build the source of a reply or a forward out of a store row.
///
/// The one assembler both stacks use: `mp reply` / `mp forward` and the TUI's
/// `r` / `R` / `w` all reach a message the same way, so a draft written from
/// the list is the draft the CLI writes for the same selector (#0052).
///
/// `with_attachments` is the forward's extra cost: the attachments are blobs,
/// and a draft's `attachments:` list needs paths, so they are materialised
/// into the stable per-account mirror keyed by Message-ID (#0006) where the
/// draft keeps resolving them after the source row is archived or evicted.
pub fn source_from_row(
    store: &crate::store::Store,
    blobs: &crate::store::BlobStore,
    row: &crate::store::read::MessageRow,
    with_attachments: bool,
) -> Result<SourceMessage> {
    let body = crate::store::read::load_body(store, blobs, row.id).unwrap_or_default();
    let attachments = if with_attachments && row.has_attachments {
        let dest = stable_attachments_dir(
            &crate::config::account_dir(&account_name_of(store)),
            &row.message_id,
        );
        crate::store::read::materialise_attachments(store, blobs, row.id, &dest)?
    } else {
        Vec::new()
    };
    Ok(SourceMessage {
        from: row.from.clone().unwrap_or_default(),
        to: row.to.clone().unwrap_or_default(),
        cc: row.cc.clone(),
        subject: row.subject.clone().unwrap_or_default(),
        date: row.date_display.clone(),
        body,
        attachments,
        // The quoted HTML companion the file build wrote beside the draft:
        // without it a reply quotes plain text where the sender wrote markup.
        html: crate::store::read::load_html(store, blobs, row.id),
    })
}

/// Build the source of a reply or a forward out of a message that was fetched
/// from the server but never ingested: the server-search hit that resolved to
/// no local row (#0052).
///
/// The same shape [`source_from_row`] produces, with the bytes coming from the
/// fetch instead of a blob, so a reply to a hit quotes what the overlay just
/// showed. A hit with no Message-ID has no stable key for its attachments, so
/// it forwards its body without them rather than inventing one; the forwarded
/// header block still names the message.
pub fn source_from_fetched(
    account_dir: &Path,
    fetched: &crate::parse::FetchedEmail,
    with_attachments: bool,
) -> Result<SourceMessage> {
    let attachments = match (with_attachments, fetched.message_id.as_deref()) {
        (true, Some(message_id)) if !fetched.attachments.is_empty() => {
            let dest = stable_attachments_dir(account_dir, message_id);
            fs::create_dir_all(&dest)
                .with_context(|| format!("creating {}", dest.display()))?;
            let mut written = Vec::new();
            for att in &fetched.attachments {
                let out = dest.join(crate::parse::sanitize_attachment_filename(&att.filename));
                fs::write(&out, &att.content)
                    .with_context(|| format!("writing {}", out.display()))?;
                written.push(out);
            }
            written
        }
        _ => Vec::new(),
    };
    Ok(SourceMessage {
        from: fetched.from.clone(),
        to: fetched.to.clone(),
        cc: fetched.cc.clone(),
        subject: fetched.subject.clone(),
        date: Some(fetched.date.clone()),
        body: fetched.body_text.trim().to_string(),
        attachments,
        html: fetched.html_body.clone(),
    })
}

/// The account a store belongs to, read back from its own path
/// (`<data>/<account>/store.sqlite3`).
fn account_name_of(store: &crate::store::Store) -> String {
    store
        .path()
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The `Fwd:`-prefixed subject of a forward, idempotent on a subject that
/// already carries the prefix.
///
/// Shared with the compose wizard, which shows the subject before the draft
/// exists and must show the one the draft will carry.
pub fn fwd_subject(subject: &str) -> String {
    if subject.to_lowercase().starts_with("fwd: ") {
        subject.to_string()
    } else {
        format!("Fwd: {subject}")
    }
}

/// Reply to a message that is not a file: the #0050 path, used by
/// `mp reply <selector>` over a store row.
pub fn create_reply_draft_from(
    source: &SourceMessage,
    reply_all: bool,
    default_from: &str,
    drafts_dir: Option<&Path>,
) -> Result<PathBuf> {
    let inbox = source;
    let original_body = inbox.body.trim();

    // Build reply fields
    let reply_to = extract_email_address(&inbox.from);

    let reply_cc = if reply_all {
        let mut all_recipients: Vec<String> = Vec::new();
        for addr in crate::send::split_addresses(&inbox.to) {
            let email = extract_email_address(&addr);
            if email.to_lowercase() != default_from.to_lowercase() {
                all_recipients.push(email);
            }
        }
        if let Some(ref cc) = inbox.cc {
            for addr in crate::send::split_addresses(cc) {
                let email = extract_email_address(&addr);
                if email.to_lowercase() != default_from.to_lowercase()
                    && !all_recipients
                        .iter()
                        .any(|r| r.to_lowercase() == email.to_lowercase())
                {
                    all_recipients.push(email);
                }
            }
        }
        if all_recipients.is_empty() {
            None
        } else {
            Some(all_recipients.join(", "))
        }
    } else {
        None
    };

    // Build subject with Re: prefix (case-insensitive check)
    let reply_subject = if inbox.subject.to_lowercase().starts_with("re: ") {
        inbox.subject.clone()
    } else {
        format!("Re: {}", inbox.subject)
    };

    // The body from the .md file is already plain text (either the server's
    // text/plain or the result of html_to_plain() at fetch time). Use as-is.
    let clean_body = original_body.to_string();

    // Build quoted body with attribution
    let attribution = format!(
        "On {}, {} wrote:",
        inbox.date.as_deref().unwrap_or("(unknown date)"),
        inbox.from
    );
    let quoted_body: String = clean_body
        .trim()
        .lines()
        .map(|line| format!("> {}", line))
        .collect::<Vec<_>>()
        .join("\n");

    // Build frontmatter
    let mut fm = String::from("---\n");
    fm.push_str(&format!("from: \"{}\"\n", default_from));
    fm.push_str(&format!("to: \"{}\"\n", reply_to));
    if let Some(ref cc) = reply_cc {
        fm.push_str(&format!("cc: \"{}\"\n", cc));
    }
    fm.push_str(&format!(
        "subject: \"{}\"\n",
        reply_subject.replace('"', "\\\"")
    ));
    fm.push_str("status: draft\n");
    fm.push_str("---\n");

    // Compose full content with {{SIGNATURE}} placeholder between reply area and quoted text
    let full_content = format!(
        "{}\n\n\n{{{{SIGNATURE}}}}\n\n{}\n{}\n",
        fm.trim_end(),
        attribution,
        quoted_body
    );

    // Determine output path
    let output_dir = drafts_dir.unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_dir)?;
    let date_prefix = Utc::now().format("%Y-%m-%d-%H%M").to_string();
    let sender_slug = slugify_sender(&inbox.from);
    let subject_slug = slugify_subject(&reply_subject);
    let filename = if subject_slug.is_empty() {
        format!("{}_{}_email.md", date_prefix, sender_slug)
    } else {
        format!("{}_{}_{}.md", date_prefix, sender_slug, subject_slug)
    };
    let mut dest = output_dir.join(&filename);

    // Avoid overwriting
    if dest.exists() {
        let mut counter = 1;
        loop {
            let name = if subject_slug.is_empty() {
                format!("{}_{}_email-{}.md", date_prefix, sender_slug, counter)
            } else {
                format!(
                    "{}_{}_{}-{}.md",
                    date_prefix, sender_slug, subject_slug, counter
                )
            };
            dest = output_dir.join(&name);
            if !dest.exists() {
                break;
            }
            counter += 1;
        }
    }

    fs::write(&dest, full_content)?;

    // Copy and wrap companion HTML for the draft if the original has one
    {
        if let Some(html_content) = inbox.html.clone() {
            let wrapped = format!(
                "<p style=\"color:#666\">On {}, {} wrote:</p>\n\
                 <div style=\"margin:0;padding:0 0 0 1em;border-left:2px solid #ccc\">\n\
                 {}\n\
                 </div>",
                inbox.date.as_deref().unwrap_or("(unknown date)"),
                inbox.from,
                html_content,
            );
            let draft_html = dest.with_extension("html");
            fs::write(&draft_html, wrapped)?;
        }
    }

    Ok(dest)
}

/// Forward a message that is not a file: the #0050 path, used by
/// `mp forward <selector>` over a store row.
pub fn create_forward_draft_from(
    source: &SourceMessage,
    default_from: &str,
    drafts_dir: Option<&Path>,
) -> Result<PathBuf> {
    let inbox = source;
    let original_body = inbox.body.trim();

    // Build forward subject
    let fwd_subject = fwd_subject(&inbox.subject);

    let attachment_paths: Vec<String> = inbox
        .attachments
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();


    // Build frontmatter
    let mut fm = String::from("---\n");
    fm.push_str(&format!("from: \"{}\"\n", default_from));
    fm.push_str("to: \"\"\n");
    fm.push_str(&format!(
        "subject: \"{}\"\n",
        fwd_subject.replace('"', "\\\"")
    ));
    fm.push_str("status: draft\n");
    if !attachment_paths.is_empty() {
        fm.push_str("attachments:\n");
        for path in &attachment_paths {
            fm.push_str(&format!("  - \"{}\"\n", path.replace('"', "\\\"")));
        }
    }
    fm.push_str("---\n");

    // Build forwarded message header block
    let fwd_header = format!(
        "---------- Forwarded message ----------\n\
         From: {}\n\
         Date: {}\n\
         Subject: {}\n\
         To: {}",
        inbox.from,
        inbox.date.as_deref().unwrap_or("(unknown date)"),
        inbox.subject,
        inbox.to,
    );

    // The body from the .md file is already plain text (either the server's
    // text/plain or the result of html_to_plain() at fetch time). Use as-is.
    let clean_body = original_body.to_string();

    let full_content = format!(
        "{}\n\n\n{{{{SIGNATURE}}}}\n\n{}\n\n{}\n",
        fm.trim_end(),
        fwd_header,
        clean_body.trim()
    );

    // Determine output path
    let output_dir = drafts_dir.unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_dir)?;
    let date_prefix = Utc::now().format("%Y-%m-%d-%H%M").to_string();
    let sender_slug = slugify_sender(&inbox.from);
    let subject_slug = slugify_subject(&fwd_subject);
    let filename = if subject_slug.is_empty() {
        format!("{}_{}_email.md", date_prefix, sender_slug)
    } else {
        format!("{}_{}_{}.md", date_prefix, sender_slug, subject_slug)
    };
    let mut dest = output_dir.join(&filename);

    // Avoid overwriting
    if dest.exists() {
        let mut counter = 1;
        loop {
            let name = if subject_slug.is_empty() {
                format!("{}_{}_email-{}.md", date_prefix, sender_slug, counter)
            } else {
                format!(
                    "{}_{}_{}-{}.md",
                    date_prefix, sender_slug, subject_slug, counter
                )
            };
            dest = output_dir.join(&name);
            if !dest.exists() {
                break;
            }
            counter += 1;
        }
    }

    fs::write(&dest, full_content)?;

    // Create companion HTML for the forward
    {
        if let Some(html_content) = inbox.html.clone() {
            let wrapped = format!(
                "<p style=\"color:#666\">---------- Forwarded message ----------<br/>\n\
                 From: {}<br/>\n\
                 Date: {}<br/>\n\
                 Subject: {}<br/>\n\
                 To: {}</p>\n\
                 <div>\n\
                 {}\n\
                 </div>",
                inbox.from,
                inbox.date.as_deref().unwrap_or("(unknown date)"),
                inbox.subject,
                inbox.to,
                html_content,
            );
            let draft_html = dest.with_extension("html");
            fs::write(&draft_html, wrapped)?;
        }
    }

    Ok(dest)
}

/// New recipient/subject values for an in-place draft frontmatter rewrite.
/// Empty strings clear the corresponding optional field (to/cc/bcc).
pub struct DraftRecipientEdit {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
}

/// Rewrite ONLY the `to`/`cc`/`bcc`/`subject` frontmatter fields of an
/// existing draft file in place, preserving the body and every other
/// frontmatter field byte-for-byte.
///
/// The file must begin with a `---` fence and contain a closing `---`.
/// Files with missing/malformed frontmatter are rejected with an error so
/// the caller can surface a status message without any data loss (the file
/// is only written on the success path).
///
/// Existing `to:`/`cc:`/`bcc:`/`subject:` lines are replaced where present;
/// missing recipient/subject keys are appended just before the closing
/// fence. Empty recipient values are written as a bare key (e.g. `cc:`),
/// matching the new-draft skeleton convention.
pub fn rewrite_draft_recipients(path: &Path, edit: &DraftRecipientEdit) -> Result<()> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    // Detect the dominant line ending so we can re-emit the frontmatter with
    // the same style the file already uses (avoids mixed CRLF/LF endings).
    let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };

    // Split off the opening fence without touching the body bytes.
    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("No frontmatter found (file does not start with '---')"))?;

    // Collect frontmatter lines up to the closing fence (a line that is
    // exactly `---`). Everything after that newline is the untouched body.
    let mut fm_lines: Vec<String> = Vec::new();
    let mut body = String::new();
    let mut closed = false;
    let mut cursor = 0usize;
    while cursor < after_open.len() {
        let rest = &after_open[cursor..];
        let (line, advance) = match rest.find('\n') {
            Some(nl) => (&rest[..nl], nl + 1),
            None => (rest, rest.len()),
        };
        let trimmed = line.trim_end_matches('\r');
        if trimmed == "---" {
            closed = true;
            body = after_open[cursor + advance..].to_string();
            break;
        }
        fm_lines.push(trimmed.to_string());
        cursor += advance;
    }
    if !closed {
        return Err(anyhow!("Malformed frontmatter: no closing '---' fence"));
    }

    let to_line = if edit.to.trim().is_empty() {
        "to:".to_string()
    } else {
        format!("to: {}", yaml_dq_escape(&edit.to))
    };
    let cc_line = if edit.cc.trim().is_empty() {
        "cc:".to_string()
    } else {
        format!("cc: {}", yaml_dq_escape(&edit.cc))
    };
    let bcc_line = if edit.bcc.trim().is_empty() {
        "bcc:".to_string()
    } else {
        format!("bcc: {}", yaml_dq_escape(&edit.bcc))
    };
    let subject_line = format!("subject: {}", yaml_dq_escape(&edit.subject));

    // Rebuild the frontmatter line list, replacing the four managed keys in
    // place (only when they appear at top level, i.e. zero indentation) and
    // dropping any continuation lines belonging to a replaced multiline
    // scalar. Every other top-level entry — including its own continuation
    // block — is copied verbatim.
    let mut out_lines: Vec<String> = Vec::new();
    let mut wrote_to = false;
    let mut wrote_cc = false;
    let mut wrote_bcc = false;
    let mut wrote_subject = false;
    let mut i = 0usize;
    while i < fm_lines.len() {
        let line = &fm_lines[i];
        // A top-level key line has no leading whitespace. Indented lines are
        // continuation lines of the previous entry (block scalars, nested
        // mappings, flow continuations) and are never matched as managed
        // keys — they are copied verbatim below.
        let is_top_level = !line.starts_with(' ') && !line.starts_with('\t');
        let managed: Option<&str> = if is_top_level {
            if line == "to:" || line.starts_with("to: ") {
                Some("to")
            } else if line == "cc:" || line.starts_with("cc: ") {
                Some("cc")
            } else if line == "bcc:" || line.starts_with("bcc: ") {
                Some("bcc")
            } else if line == "subject:" || line.starts_with("subject: ") {
                Some("subject")
            } else {
                None
            }
        } else {
            None
        };

        match managed {
            Some(key) => {
                match key {
                    "to" => {
                        out_lines.push(to_line.clone());
                        wrote_to = true;
                    }
                    "cc" => {
                        out_lines.push(cc_line.clone());
                        wrote_cc = true;
                    }
                    "bcc" => {
                        out_lines.push(bcc_line.clone());
                        wrote_bcc = true;
                    }
                    _ => {
                        out_lines.push(subject_line.clone());
                        wrote_subject = true;
                    }
                }
                // Skip the replaced key line and every continuation line that
                // follows it (any more-indented line): folded `>`, literal
                // `|`, indent-continued plain scalars, and block
                // sequences/mappings nested under the key.
                //
                // Blank or whitespace-only lines are legal *inside* a block
                // scalar (they separate paragraphs), so they must be consumed
                // too — but only when a further indented line follows them.
                // A trailing blank line that precedes the next top-level key
                // belongs to the document, not the scalar, and must survive.
                i += 1;
                while i < fm_lines.len() {
                    let cont = &fm_lines[i];
                    if cont.starts_with(' ') || cont.starts_with('\t') {
                        i += 1;
                    } else if cont.trim().is_empty() {
                        // Blank line: only consumable if a later indented line
                        // continues the scalar block. Look ahead past any run
                        // of blank lines to find the first non-blank line.
                        let mut j = i + 1;
                        while j < fm_lines.len() && fm_lines[j].trim().is_empty() {
                            j += 1;
                        }
                        let continues = j < fm_lines.len()
                            && (fm_lines[j].starts_with(' ') || fm_lines[j].starts_with('\t'));
                        if continues {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
            None => {
                out_lines.push(line.clone());
                i += 1;
            }
        }
    }

    if !wrote_to {
        out_lines.push(to_line);
    }
    if !wrote_cc {
        out_lines.push(cc_line);
    }
    if !wrote_bcc {
        out_lines.push(bcc_line);
    }
    if !wrote_subject {
        out_lines.push(subject_line);
    }

    let mut rebuilt = String::new();
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    for line in out_lines {
        rebuilt.push_str(&line);
        rebuilt.push_str(newline);
    }
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    rebuilt.push_str(&body);

    write_atomic(path, rebuilt.as_bytes())
        .with_context(|| format!("Failed to write file: {}", path.display()))?;
    Ok(())
}

/// Rewrite ONLY the `rsvp:` line nested under the top-level `event:` block of
/// an invite email's frontmatter, in place, preserving the body and every
/// other frontmatter field byte-for-byte.
///
/// This is the local-state update after a successful RSVP send (#0029): the
/// sidecar `.ics` stays the source of truth for `UID`/`SEQUENCE`; the
/// frontmatter `event.rsvp` is a render/query cache. `new_status` is one of
/// `accepted` / `tentative` / `declined` (lowercase, no quoting needed).
///
/// Only a `rsvp:` line indented under `event:` is touched. If the `event:`
/// block has no `rsvp:` line (older invites parsed before the field existed),
/// one is inserted immediately after the `event:` key at the same indentation
/// as the block's other children. Errors (leaving the file untouched) when the
/// frontmatter is missing/malformed or has no `event:` block, so the caller can
/// surface a status message with no data loss.
pub fn set_event_rsvp(path: &Path, new_status: &str) -> Result<()> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };

    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("No frontmatter found (file does not start with '---')"))?;

    // Split frontmatter lines from the untouched body at the closing fence.
    let mut fm_lines: Vec<String> = Vec::new();
    let mut body = String::new();
    let mut closed = false;
    let mut cursor = 0usize;
    while cursor < after_open.len() {
        let rest = &after_open[cursor..];
        let (line, advance) = match rest.find('\n') {
            Some(nl) => (&rest[..nl], nl + 1),
            None => (rest, rest.len()),
        };
        let trimmed = line.trim_end_matches('\r');
        if trimmed == "---" {
            closed = true;
            body = after_open[cursor + advance..].to_string();
            break;
        }
        fm_lines.push(trimmed.to_string());
        cursor += advance;
    }
    if !closed {
        return Err(anyhow!("Malformed frontmatter: no closing '---' fence"));
    }

    // Find the top-level `event:` key (zero indentation).
    let event_idx = fm_lines
        .iter()
        .position(|l| *l == "event:" || l.starts_with("event:"))
        .filter(|&i| {
            let l = &fm_lines[i];
            !l.starts_with(' ') && !l.starts_with('\t')
        })
        .ok_or_else(|| anyhow!("Email has no event: block (not an invite)"))?;

    // Scan the block's children (indented lines) for an `rsvp:` entry, tracking
    // the child indentation so an inserted line matches the block's style.
    let mut rsvp_line_idx: Option<usize> = None;
    let mut child_indent: Option<String> = None;
    let mut i = event_idx + 1;
    while i < fm_lines.len() {
        let line = &fm_lines[i];
        let indent_len = line.len() - line.trim_start().len();
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if !is_indented && !line.trim().is_empty() {
            break; // next top-level key ends the block
        }
        if is_indented && !line.trim().is_empty() {
            if child_indent.is_none() {
                child_indent = Some(line[..indent_len].to_string());
            }
            // Match `rsvp:` only at the block's direct-child indentation, never
            // a deeper `- status:`/`address:` under `attendees:`.
            let trimmed = line.trim_start();
            let at_child_indent = child_indent
                .as_deref()
                .map(|ci| line[..indent_len] == *ci)
                .unwrap_or(false);
            if at_child_indent && (trimmed == "rsvp:" || trimmed.starts_with("rsvp:")) {
                rsvp_line_idx = Some(i);
                break;
            }
        }
        i += 1;
    }

    let indent = child_indent.unwrap_or_else(|| "  ".to_string());
    let new_line = format!("{}rsvp: {}", indent, new_status);
    match rsvp_line_idx {
        Some(idx) => fm_lines[idx] = new_line,
        None => fm_lines.insert(event_idx + 1, new_line),
    }

    let mut rebuilt = String::new();
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    for line in fm_lines {
        rebuilt.push_str(&line);
        rebuilt.push_str(newline);
    }
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    rebuilt.push_str(&body);

    write_atomic(path, rebuilt.as_bytes())
        .with_context(|| format!("Failed to write file: {}", path.display()))?;
    Ok(())
}

/// Outcome of a [`set_event_attendee_status`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttendeeUpdate {
    /// The matching attendee already carried `new_status`; the file was left
    /// byte-for-byte untouched (the idempotent fast path).
    Unchanged,
    /// The matching attendee's `status:` line was rewritten in place.
    Updated,
    /// No attendee with the target address exists in the block; the file was
    /// left untouched. Reconciliation treats this as a no-op (see
    /// `docs/plans/calendar-invites.md`, §4).
    NotFound,
}

/// Rewrite ONLY the `status:` line of the attendee entry matching `address`
/// (case-insensitive) inside the `attendees:` sequence nested under the
/// top-level `event:` block, in place, preserving the body and every other
/// frontmatter byte.
///
/// This is the organizer-side REPLY reconciliation update (#0030). The sidecar
/// `.ics` stays the source of truth for `UID`/`SEQUENCE`; `event.attendees[]`
/// is a render/query cache reconstructible from IMAP-visible messages, so this
/// update must be idempotent: re-applying the same status is a no-op that
/// leaves the file's bytes identical ([`AttendeeUpdate::Unchanged`]).
///
/// The rewrite is line-surgical and never re-serializes the block through
/// serde, which keeps unrelated frontmatter (block scalars containing fake
/// `status:`/`address:` keys, interior blank lines, CRLF endings, quoted or
/// unicode addresses) byte-identical. `new_status` is one of
/// `accepted` / `tentative` / `declined` / `needs-action` (lowercase).
///
/// Attendee entries are matched as YAML block-sequence items of the form:
///
/// ```yaml
/// event:
///   attendees:
///   - address: a@example.com
///     status: needs-action
/// ```
///
/// Both the `- address:` list-item indentation and the mapping-key indentation
/// of the target's `status:` line are preserved. Errors (leaving the file
/// untouched) when the frontmatter is missing/malformed or has no `event:`
/// block, so the caller can surface a status message with no data loss.
pub fn set_event_attendee_status(
    path: &Path,
    address: &str,
    new_status: &str,
) -> Result<AttendeeUpdate> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };

    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("No frontmatter found (file does not start with '---')"))?;

    // Split frontmatter lines from the untouched body at the closing fence.
    let mut fm_lines: Vec<String> = Vec::new();
    let mut body = String::new();
    let mut closed = false;
    let mut cursor = 0usize;
    while cursor < after_open.len() {
        let rest = &after_open[cursor..];
        let (line, advance) = match rest.find('\n') {
            Some(nl) => (&rest[..nl], nl + 1),
            None => (rest, rest.len()),
        };
        let trimmed = line.trim_end_matches('\r');
        if trimmed == "---" {
            closed = true;
            body = after_open[cursor + advance..].to_string();
            break;
        }
        fm_lines.push(trimmed.to_string());
        cursor += advance;
    }
    if !closed {
        return Err(anyhow!("Malformed frontmatter: no closing '---' fence"));
    }

    // Locate the top-level `event:` key (zero indentation).
    let event_idx = fm_lines
        .iter()
        .position(|l| *l == "event:" || l.starts_with("event:"))
        .filter(|&i| {
            let l = &fm_lines[i];
            !l.starts_with(' ') && !l.starts_with('\t')
        })
        .ok_or_else(|| anyhow!("Email has no event: block (not an invite)"))?;

    // Determine the block's end (next top-level, non-blank line) and the
    // direct-child indentation (used to disambiguate the `attendees:` key from
    // deeper `- address:`/`status:` lines).
    let mut block_end = fm_lines.len();
    let mut child_indent: Option<String> = None;
    for (i, line) in fm_lines.iter().enumerate().skip(event_idx + 1) {
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if !is_indented && !line.trim().is_empty() {
            block_end = i;
            break;
        }
        if is_indented && !line.trim().is_empty() && child_indent.is_none() {
            let indent_len = line.len() - line.trim_start().len();
            child_indent = Some(line[..indent_len].to_string());
        }
    }

    // Find the `attendees:` direct child of the block.
    let attendees_idx = (event_idx + 1..block_end).find(|&i| {
        let line = &fm_lines[i];
        let indent_len = line.len() - line.trim_start().len();
        let at_child_indent = child_indent
            .as_deref()
            .map(|ci| line[..indent_len] == *ci)
            .unwrap_or(false);
        at_child_indent && line.trim_start().starts_with("attendees:")
    });
    let Some(attendees_idx) = attendees_idx else {
        return Ok(AttendeeUpdate::NotFound);
    };

    let target = address.trim().to_lowercase();

    // Walk the sequence items following `attendees:`. Each item begins with a
    // `- ` line at a deeper indent than `attendees:`; the item's mapping keys
    // (`address:`, `status:`) are the `- address:` inline key plus any lines at
    // the item's key indentation. We scan item-by-item, recording the current
    // item's address and its `status:` line index, then rewrite on a match.
    let attendees_indent_len = {
        let l = &fm_lines[attendees_idx];
        l.len() - l.trim_start().len()
    };

    let mut i = attendees_idx + 1;
    // Per-item accumulators.
    let mut cur_address: Option<String> = None;
    let mut cur_status_idx: Option<usize> = None;
    let mut cur_status_indent: Option<String> = None;
    // Indentation of the item mapping keys of the *current* item (the column
    // where `- ` puts the first inline key, or the explicit continuation-key
    // indent). Lines shallower than this that are not new items end the block.
    let mut item_key_indent: Option<usize> = None;

    let mut result = AttendeeUpdate::NotFound;
    let mut matched_status_line: Option<(usize, String)> = None;

    while i < block_end {
        let line = &fm_lines[i];
        let indent_len = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        let is_item_start = trimmed.starts_with("- ") || trimmed == "-";
        // The sequence ends at the first non-item line whose indent is at or
        // shallower than `attendees:` (a sibling/parent key), OR a non-item
        // line shallower than the current item's mapping-key column (a key
        // that belongs to an enclosing mapping, not this item).
        if !is_item_start {
            if indent_len <= attendees_indent_len {
                break;
            }
            if let Some(ki) = item_key_indent {
                if indent_len < ki {
                    break;
                }
            }
        } else if indent_len < attendees_indent_len {
            // A sequence item shallower than `attendees:` cannot belong to it.
            break;
        }

        if is_item_start {
            // New sequence item: evaluate the item we just finished.
            if let Some(addr) = cur_address.take() {
                if addr == target {
                    if let (Some(sidx), Some(sind)) =
                        (cur_status_idx.take(), cur_status_indent.take())
                    {
                        matched_status_line = Some((sidx, sind));
                    }
                    break;
                }
            }
            cur_address = None;
            cur_status_idx = None;
            cur_status_indent = None;

            // The item's first mapping key sits inline after `- `. Its column
            // is where the continuation keys of this item align.
            let after_dash = trimmed[1..].trim_start();
            let key_col = line.len() - after_dash.len();
            item_key_indent = Some(key_col);
            let (key, val) = split_yaml_key(after_dash);
            match key {
                Some("address") => cur_address = Some(unquote_yaml(val).to_lowercase()),
                Some("status") => {
                    cur_status_idx = Some(i);
                    // Preserve the literal prefix (indent + `- `) before the
                    // key so the sequence-item dash is not lost on rewrite.
                    cur_status_indent = Some(line[..key_col].to_string());
                }
                _ => {}
            }
        } else {
            // Continuation mapping key of the current item.
            let (key, val) = split_yaml_key(trimmed);
            match key {
                Some("address") => cur_address = Some(unquote_yaml(val).to_lowercase()),
                Some("status") => {
                    cur_status_idx = Some(i);
                    cur_status_indent = Some(line[..indent_len].to_string());
                }
                _ => {}
            }
        }
        i += 1;
    }

    // Evaluate a trailing item that ran to the end of the block.
    if matched_status_line.is_none() {
        if let Some(addr) = cur_address {
            if addr == target {
                if let (Some(sidx), Some(sind)) = (cur_status_idx, cur_status_indent) {
                    matched_status_line = Some((sidx, sind));
                }
            }
        }
    }

    if let Some((sidx, sind)) = matched_status_line {
        let new_line = format!("{}status: {}", sind, new_status);
        if fm_lines[sidx] == new_line {
            return Ok(AttendeeUpdate::Unchanged);
        }
        fm_lines[sidx] = new_line;
        result = AttendeeUpdate::Updated;
    }

    if result != AttendeeUpdate::Updated {
        // NotFound or matched-without-status: leave the file untouched.
        return Ok(result);
    }

    let mut rebuilt = String::new();
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    for line in fm_lines {
        rebuilt.push_str(&line);
        rebuilt.push_str(newline);
    }
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    rebuilt.push_str(&body);

    write_atomic(path, rebuilt.as_bytes())
        .with_context(|| format!("Failed to write file: {}", path.display()))?;
    Ok(result)
}

/// Split a `key: value` YAML mapping line into `(Some(key), value)`.
/// Returns `(None, "")` when there is no top-level `:` separator.
fn split_yaml_key(s: &str) -> (Option<&str>, &str) {
    match s.find(':') {
        Some(idx) => {
            let key = s[..idx].trim();
            let val = s[idx + 1..].trim();
            if key.is_empty() {
                (None, "")
            } else {
                (Some(key), val)
            }
        }
        None => (None, ""),
    }
}

/// Strip surrounding single/double quotes from a YAML scalar value, if present.
/// Best-effort: used only to normalise an attendee address for comparison.
fn unquote_yaml(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 {
        let bytes = t.as_bytes();
        let first = bytes[0];
        let last = bytes[t.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &t[1..t.len() - 1];
        }
    }
    t
}

/// Atomically overwrite `path` by writing to a `.tmp` sibling then renaming
/// over the destination. Mirrors `secrets::write_secret_file_atomic` minus the
/// fixed 0600 mode — drafts are plain files, so this preserves the permission
/// semantics of an ordinary overwrite.
///
/// A rename replaces the destination inode, so the target's permission bits
/// would otherwise be reset to whatever the umask gives the temp file: a
/// draft the user had chmod'ed to 0600 came back 0644 after every status
/// change. When the target exists, its mode is copied onto the temp file
/// *before* the payload is written, so the content is never briefly readable
/// under wider permissions. A new file keeps default umask behaviour.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;

    #[cfg(unix)]
    let existing_mode = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o7777)
    };

    // Use a PID-qualified extension so we never collide with (and clobber) a
    // real user file that happens to be named `<draft>.tmp`.
    let tmp = path.with_extension(format!("mp-tmp.{}", std::process::id()));
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow!(e))
                .with_context(|| format!("Failed to remove stale temp file: {}", tmp.display()))
        }
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("Failed to create temp file: {}", tmp.display()))?;
    #[cfg(unix)]
    if let Some(mode) = existing_mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| {
                format!(
                    "Failed to preserve permissions ({mode:o}) on temp file: {}",
                    tmp.display()
                )
            })?;
    }
    file.write_all(data)
        .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("Failed to flush temp file: {}", tmp.display()))?;
    drop(file);

    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// YAML double-quote escape for a scalar string value.
fn yaml_dq_escape(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// What to write for one top-level frontmatter key in
/// [`rewrite_frontmatter_scalars`].
#[derive(Debug, Clone)]
enum FieldWrite {
    /// Set the key to this raw YAML scalar (already quoted/escaped by the
    /// caller). Appended just before the closing fence when the key is
    /// absent.
    Set(String),
    /// Reset the key to a bare `key:` (deserializes to `None`) when it is
    /// present. Never appended: a key that was not there stays away, so we
    /// do not sprinkle `cc: null`-style noise into user files.
    ClearIfPresent,
}

/// Rewrite ONLY the listed top-level frontmatter keys of `content`,
/// preserving the body and every other frontmatter byte.
///
/// This is the shared write path behind the status transitions
/// ([`mark_as_approved`] and [`mark_as_draft`]; the `sent` transition went
/// with the local sent `.md` in #0037).
/// They used to parse into [`EmailFrontmatter`] and re-serialize, which
/// silently dropped every field that struct does not model -- including
/// `date:`, the key the TUI sorts on, so an approved draft teleported to
/// the bottom of the list. Line surgery keeps unknown and user-added
/// fields intact.
///
/// Matching is the same as [`rewrite_draft_recipients`]: only zero-indent
/// key lines are managed, and a replaced key's continuation lines (block
/// scalars, nested mappings) are dropped with it.
fn rewrite_frontmatter_scalars(
    content: &str,
    updates: &[(&str, FieldWrite)],
) -> Result<String> {
    let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };

    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("No frontmatter found (file does not start with '---')"))?;

    // Split frontmatter lines from the untouched body at the closing fence.
    let mut fm_lines: Vec<String> = Vec::new();
    let mut body = String::new();
    let mut closed = false;
    let mut cursor = 0usize;
    while cursor < after_open.len() {
        let rest = &after_open[cursor..];
        let (line, advance) = match rest.find('\n') {
            Some(nl) => (&rest[..nl], nl + 1),
            None => (rest, rest.len()),
        };
        let trimmed = line.trim_end_matches('\r');
        if trimmed == "---" {
            closed = true;
            body = after_open[cursor + advance..].to_string();
            break;
        }
        fm_lines.push(trimmed.to_string());
        cursor += advance;
    }
    if !closed {
        return Err(anyhow!("Malformed frontmatter: no closing '---' fence"));
    }

    let mut written = vec![false; updates.len()];
    let mut out_lines: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < fm_lines.len() {
        let line = &fm_lines[i];
        // Indented lines are continuation lines of the previous entry and
        // are never matched as managed keys.
        let is_top_level = !line.starts_with(' ') && !line.starts_with('\t');
        let managed = if is_top_level {
            updates.iter().position(|(key, _)| {
                line.len() > key.len()
                    && line.starts_with(key)
                    && line.as_bytes()[key.len()] == b':'
                    && line[key.len() + 1..]
                        .chars()
                        .next()
                        .is_none_or(|c| c == ' ')
            })
        } else {
            None
        };

        match managed {
            Some(pos) => {
                let (key, write) = &updates[pos];
                out_lines.push(match write {
                    FieldWrite::Set(value) => format!("{key}: {value}"),
                    FieldWrite::ClearIfPresent => format!("{key}:"),
                });
                written[pos] = true;
                // Skip the replaced key line and its continuation lines
                // (any more-indented line), including blank lines interior
                // to a block scalar but not a trailing one.
                i += 1;
                while i < fm_lines.len() {
                    let cont = &fm_lines[i];
                    if cont.starts_with(' ') || cont.starts_with('\t') {
                        i += 1;
                    } else if cont.trim().is_empty() {
                        let mut j = i + 1;
                        while j < fm_lines.len() && fm_lines[j].trim().is_empty() {
                            j += 1;
                        }
                        let continues = j < fm_lines.len()
                            && (fm_lines[j].starts_with(' ') || fm_lines[j].starts_with('\t'));
                        if continues {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
            None => {
                out_lines.push(line.clone());
                i += 1;
            }
        }
    }

    for (pos, (key, write)) in updates.iter().enumerate() {
        if written[pos] {
            continue;
        }
        if let FieldWrite::Set(value) = write {
            out_lines.push(format!("{key}: {value}"));
        }
    }

    let mut rebuilt = String::new();
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    for line in out_lines {
        rebuilt.push_str(&line);
        rebuilt.push_str(newline);
    }
    rebuilt.push_str("---");
    rebuilt.push_str(newline);
    rebuilt.push_str(&body);
    Ok(rebuilt)
}

/// Read `path`, apply [`rewrite_frontmatter_scalars`], write it back
/// atomically. The file is only touched on the success path.
fn rewrite_frontmatter_scalars_at(path: &Path, updates: &[(&str, FieldWrite)]) -> Result<()> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;
    let rebuilt = rewrite_frontmatter_scalars(&content, updates)?;
    write_atomic(path, rebuilt.as_bytes())
        .with_context(|| format!("Failed to write file: {}", path.display()))
}

pub fn parse_email_draft(path: &Path) -> Result<EmailDraft> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(&content);

    let frontmatter: EmailFrontmatter = parsed
        .data
        .ok_or_else(|| anyhow!("No frontmatter found in file"))?
        .deserialize()
        .context("Failed to parse frontmatter")?;

    let body_markdown = parsed.content.trim().to_string();

    Ok(EmailDraft {
        path: path.to_path_buf(),
        frontmatter,
        body_markdown,
    })
}

pub fn validate_draft(draft: &EmailDraft) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // Check that at least one recipient exists across to/cc/bcc
    let to_empty = draft.frontmatter.to.as_deref().map_or(true, |s| s.trim().is_empty());
    let cc_empty = draft.frontmatter.cc.as_deref().map_or(true, |s| s.trim().is_empty());
    let bcc_empty = draft.frontmatter.bcc.as_deref().map_or(true, |s| s.trim().is_empty());
    if to_empty && cc_empty && bcc_empty {
        return Err(anyhow!("No recipients (to, cc, and bcc are all empty)"));
    }

    if draft.frontmatter.subject.is_empty() {
        return Err(anyhow!("Missing 'subject' field"));
    }

    if draft.body_markdown.is_empty() {
        warnings.push("Email body is empty".to_string());
    }

    // Validate email format (basic check). Display names that are not raw
    // RFC 5322 atext are auto-quoted before parsing, mirroring what
    // the send path does, so the validator does not flag addresses that we
    // know we will be able to send.
    let validate_field = |field: Option<&String>, name: &str| -> Result<()> {
        if let Some(value) = field {
            for email in crate::send::split_addresses(value) {
                let normalized = crate::send::normalize_address_for_smtp(&email);
                if normalized.parse::<lettre::message::Mailbox>().is_err() {
                    return Err(anyhow!(
                        "Invalid email address in '{}': {}",
                        name,
                        email
                    ));
                }
            }
        }
        Ok(())
    };
    validate_field(draft.frontmatter.to.as_ref(), "to")?;
    validate_field(draft.frontmatter.cc.as_ref(), "cc")?;
    validate_field(draft.frontmatter.bcc.as_ref(), "bcc")?;

    // Check attachments exist
    if let Some(attachments) = &draft.frontmatter.attachments {
        for attachment in attachments {
            let expanded = shellexpand::tilde(attachment);
            if !Path::new(expanded.as_ref()).exists() {
                warnings.push(format!("Attachment not found: {}", attachment));
            }
        }
    }

    Ok(warnings)
}

pub fn preview_draft(
    draft: &EmailDraft,
    smtp_config: &SmtpConfig,
    email_config: &EmailSettings,
    signature: Option<&str>,
    is_dry_run: bool,
) -> Result<()> {
    println!("\n{}", "=== Email Draft Preview ===".bold().cyan());
    println!(
        "{}: {}",
        "From".bold(),
        draft
            .frontmatter
            .from
            .as_ref()
            .unwrap_or(&smtp_config.default_from)
    );
    println!("{}: {}", "To".bold(), draft.frontmatter.to.as_deref().unwrap_or("(bcc only)"));

    if let Some(cc) = &draft.frontmatter.cc {
        println!("{}: {}", "Cc".bold(), cc);
    }

    if let Some(bcc) = &draft.frontmatter.bcc {
        println!("{}: {}", "Bcc".bold(), bcc);
    }

    println!("{}: {}", "Subject".bold(), draft.frontmatter.subject);

    println!("\n{}\n", "--- Body Preview (first 500 chars) ---".dimmed());
    let preview: String = draft.body_markdown.chars().take(500).collect();
    println!("{}", preview);
    if draft.body_markdown.len() > 500 {
        println!("{}", "...".dimmed());
    }

    println!("\n{}", "--- Settings ---".dimmed());
    println!(
        "  Font: {} ({})",
        email_config.font_family, email_config.font_size
    );
    if let Some(sig) = signature {
        let sig_preview: String = sig.chars().take(50).collect();
        println!("  Signature: {} ...", sig_preview.replace('\n', " "));
    } else {
        println!("  Signature: {}", "none".dimmed());
    }

    println!("\n{}", "--- Status ---".dimmed());

    // Validate and show status
    match validate_draft(draft) {
        Ok(warnings) => {
            println!("{} Valid YAML frontmatter", "✓".green());
            println!(
                "{} Status: {}",
                "✓".green(),
                format!("{}", draft.frontmatter.status).yellow()
            );
            println!("{} All required fields present", "✓".green());

            for warning in warnings {
                println!("{} {}", "⚠".yellow(), warning);
            }
        }
        Err(e) => {
            println!("{} Validation error: {}", "✗".red(), e);
        }
    }

    if is_dry_run {
        println!(
            "\n{}\n",
            "[DRY RUN] Would send email (use 'send' subcommand to actually send)"
                .yellow()
                .bold()
        );
    }

    Ok(())
}

/// Mark a draft as sent, in place.
///
/// The local sent `.md` this used to write into `sent/` is gone with the rest
/// of the `.md` tree (#0037): the Sent copy is now the durable outbox's job,
/// which APPENDs it to the server and ingests it into the store. What is left
/// here is the draft's own bookkeeping, rewritten surgically in the draft's
/// bytes so `date:` and any field `EmailFrontmatter` does not model survive the
/// send.
///
/// A draft that was never written to disk (the synthetic one `mp invite`
/// builds) has nothing to mark, and says so in the log rather than
/// materialising a file nobody asked for.
pub fn mark_draft_sent(draft: &EmailDraft, message_id: Option<&str>) -> Result<()> {
    info!("Updating status to sent: {}", draft.path.display());
    let sent_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let sent_via = format!("email-cli v{}", env!("CARGO_PKG_VERSION"));

    let content = match fs::read_to_string(&draft.path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!(
                "No draft file at {}; nothing to mark as sent",
                draft.path.display()
            );
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .context(format!("reading the draft {}", draft.path.display()))
        }
    };

    let new_content = rewrite_frontmatter_scalars(
        &content,
        &[
            ("status", FieldWrite::Set(EmailStatus::Sent.to_string())),
            ("sent_at", FieldWrite::Set(sent_at)),
            ("sent_via", FieldWrite::Set(yaml_dq_escape(&sent_via))),
            (
                "message_id",
                match message_id {
                    Some(id) => FieldWrite::Set(yaml_dq_escape(id)),
                    None => FieldWrite::ClearIfPresent,
                },
            ),
        ],
    )?;
    fs::write(&draft.path, new_content)?;

    // The companion HTML only exists to carry the quoted reply into the send;
    // once sent it is dead weight next to the draft.
    let html_companion = draft.path.with_extension("html");
    if html_companion.exists() {
        fs::remove_file(&html_companion).ok();
    }

    Ok(())
}

/// Resolve the drafts directory using a fallback chain:
/// 1. If the user passed an explicit (non-default) path, use it as-is.
/// 2. Else if config_drafts_dir is set and points to an existing directory, use that.
/// 3. Else fall back to "." (current directory).
pub fn resolve_drafts_dir(cli_dir: &Path, config_drafts_dir: &Option<PathBuf>) -> PathBuf {
    if cli_dir != Path::new(".") {
        return cli_dir.to_path_buf();
    }
    if let Some(ref dir) = config_drafts_dir {
        if dir.is_dir() {
            return dir.clone();
        }
    }
    cli_dir.to_path_buf()
}

pub fn find_drafts(dir: &Path, status_filter: Option<EmailStatus>) -> Result<Vec<EmailDraft>> {
    let mut drafts = Vec::new();

    for entry in WalkDir::new(dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            match parse_email_draft(path) {
                Ok(draft) => {
                    if let Some(ref filter) = status_filter {
                        if &draft.frontmatter.status == filter {
                            drafts.push(draft);
                        }
                    } else {
                        drafts.push(draft);
                    }
                }
                Err(e) => {
                    eprintln!("{} Skipping {}: {}", "⚠".yellow(), path.display(), e);
                }
            }
        }
    }

    Ok(drafts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EmailDraft, EmailFrontmatter, EmailStatus};
    use std::path::PathBuf;

    fn make_draft(to: &str, subject: &str, body: &str, status: EmailStatus) -> EmailDraft {
        let to_opt = if to.is_empty() { None } else { Some(to.to_string()) };
        EmailDraft {
            path: PathBuf::from("test.md"),
            frontmatter: EmailFrontmatter {
                id: None,
                date: None,
                to: to_opt,
                cc: None,
                bcc: None,
                subject: subject.to_string(),
                status,
                from: Some("me@example.com".to_string()),
                reply_to: None,
                attachments: None,
                sent_at: None,
                sent_via: None,
                message_id: None,
                event: None,
            },
            body_markdown: body.to_string(),
        }
    }

    #[test]
    fn test_rewrite_draft_recipients_preserves_body_and_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("draft.md");
        let original = concat!(
            "---\n",
            "to: old@example.com\n",
            "cc:\n",
            "bcc:\n",
            "subject: Old subject\n",
            "status: draft\n",
            "from: \"me@example.com\"\n",
            "date: Thu, 10 Jul 2026 08:00:00 +0000\n",
            "reply_to:\n",
            "attachments:\n",
            "custom_field: keep-me\n",
            "---\n",
            "\n",
            "Hello body line 1.\n",
            "Line 2 with --- dashes inside.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "alice@example.com, bob@example.com".to_string(),
            cc: "carol@example.com".to_string(),
            bcc: String::new(),
            subject: "New subject".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();

        // Recipient/subject fields rewritten.
        assert!(result.contains("to: \"alice@example.com, bob@example.com\""));
        assert!(result.contains("cc: \"carol@example.com\""));
        assert!(
            result.contains("\nbcc:\n"),
            "empty bcc should be a bare key: {result}"
        );
        assert!(result.contains("subject: \"New subject\""));
        assert!(!result.contains("Old subject"));
        assert!(!result.contains("old@example.com"));

        // Unknown / unrelated frontmatter fields preserved verbatim.
        assert!(result.contains("status: draft"));
        assert!(result.contains("from: \"me@example.com\""));
        assert!(result.contains("date: Thu, 10 Jul 2026 08:00:00 +0000"));
        assert!(result.contains("reply_to:"));
        assert!(result.contains("custom_field: keep-me"));

        // Body preserved byte-for-byte (including the interior `---`).
        assert!(
            result.ends_with("\nHello body line 1.\nLine 2 with --- dashes inside.\n"),
            "body not preserved: {result:?}"
        );

        // The rewritten file still parses and reflects the new values.
        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(
            draft.frontmatter.to.as_deref(),
            Some("alice@example.com, bob@example.com")
        );
        assert_eq!(draft.frontmatter.cc.as_deref(), Some("carol@example.com"));
        assert_eq!(draft.frontmatter.bcc, None);
        assert_eq!(draft.frontmatter.subject, "New subject");
    }

    #[test]
    fn test_set_event_rsvp_updates_in_place_and_preserves_body() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite.md");
        let original = concat!(
            "---\n",
            "from: \"chair@tum.de\"\n",
            "to: \"me@example.com\"\n",
            "subject: LOC Day planning\n",
            "status: inbox\n",
            "event:\n",
            "  uid: abc123@outlook.com\n",
            "  method: REQUEST\n",
            "  sequence: 3\n",
            "  summary: LOC Day planning\n",
            "  rsvp: needs-action\n",
            "  attendees:\n",
            "  - address: me@example.com\n",
            "    status: needs-action\n",
            "---\n",
            "\n",
            "You are invited.\nLine with --- dashes.\n",
        );
        fs::write(&path, original).unwrap();

        set_event_rsvp(&path, "accepted").unwrap();
        let result = fs::read_to_string(&path).unwrap();

        // Only the block's own rsvp: flipped; attendee status untouched.
        assert!(result.contains("  rsvp: accepted"), "{result}");
        assert!(!result.contains("rsvp: needs-action"), "{result}");
        assert!(
            result.contains("    status: needs-action"),
            "attendee status must not change: {result}"
        );
        // Body preserved byte-for-byte, including interior dashes.
        assert!(
            result.ends_with("\nYou are invited.\nLine with --- dashes.\n"),
            "body not preserved: {result:?}"
        );

        // Round-trips: parses back with the new own-RSVP status.
        let matter = Matter::<YAML>::new();
        let parsed = matter.parse(&fs::read_to_string(&path).unwrap());
        let fm: crate::types::InboxFrontmatter =
            parsed.data.unwrap().deserialize().unwrap();
        let event = fm.event.unwrap();
        assert_eq!(event.rsvp, "accepted");
        assert_eq!(event.attendees.len(), 1);
        assert_eq!(event.attendees[0].status, "needs-action");
    }

    #[test]
    fn test_set_event_rsvp_inserts_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite2.md");
        // An older invite without an rsvp: line under event:.
        let original = concat!(
            "---\n",
            "from: chair@tum.de\n",
            "to: me@example.com\n",
            "subject: S\n",
            "status: inbox\n",
            "event:\n",
            "  uid: u@x\n",
            "  method: REQUEST\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        fs::write(&path, original).unwrap();

        set_event_rsvp(&path, "declined").unwrap();
        let result = fs::read_to_string(&path).unwrap();
        assert!(result.contains("  rsvp: declined"), "{result}");
        // Inserted at child indentation, right after event:.
        assert!(result.contains("event:\n  rsvp: declined\n  uid: u@x"), "{result}");

        let matter = Matter::<YAML>::new();
        let parsed = matter.parse(&result);
        let fm: crate::types::InboxFrontmatter =
            parsed.data.unwrap().deserialize().unwrap();
        assert_eq!(fm.event.unwrap().rsvp, "declined");
    }

    #[test]
    fn test_set_event_rsvp_errors_without_event_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.md");
        let original = "---\nsubject: S\nstatus: inbox\n---\n\nBody.\n";
        fs::write(&path, original).unwrap();
        assert!(set_event_rsvp(&path, "accepted").is_err());
        // File untouched on error.
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn test_rewrite_draft_recipients_appends_missing_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("draft.md");
        // No cc/bcc keys present at all.
        let original = "---\nto: old@example.com\nsubject: S\nstatus: draft\n---\n\nBody.\n";
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: "cc@example.com".to_string(),
            bcc: "bcc@example.com".to_string(),
            subject: "S2".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.to.as_deref(), Some("new@example.com"));
        assert_eq!(draft.frontmatter.cc.as_deref(), Some("cc@example.com"));
        assert_eq!(draft.frontmatter.bcc.as_deref(), Some("bcc@example.com"));
        assert_eq!(draft.frontmatter.subject, "S2");
        assert_eq!(draft.body_markdown, "Body.");
    }

    #[test]
    fn test_rewrite_draft_recipients_folded_subject_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("folded.md");
        // A folded `subject: >` block spanning two indented continuation lines.
        let original = concat!(
            "---\n",
            "to: old@example.com\n",
            "subject: >\n",
            "  This is a long folded\n",
            "  subject line.\n",
            "status: draft\n",
            "custom_field: keep-me\n",
            "---\n",
            "\n",
            "Body stays.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "New subject".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        // Orphaned continuation lines of the old folded scalar must be gone.
        assert!(
            !result.contains("This is a long folded"),
            "folded continuation not consumed: {result}"
        );
        assert!(!result.contains("subject line."));
        assert!(result.contains("custom_field: keep-me"));

        // Must still parse and yield the new values with body preserved.
        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.to.as_deref(), Some("new@example.com"));
        assert_eq!(draft.frontmatter.subject, "New subject");
        assert_eq!(draft.body_markdown, "Body stays.");
    }

    #[test]
    fn test_rewrite_draft_recipients_literal_block_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("literal.md");
        // A literal `subject: |` block.
        let original = concat!(
            "---\n",
            "to: old@example.com\n",
            "subject: |\n",
            "  line one\n",
            "  line two\n",
            "status: draft\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "S2".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        assert!(!result.contains("line one"), "literal block not consumed: {result}");
        assert!(!result.contains("line two"));

        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.subject, "S2");
        assert_eq!(draft.body_markdown, "Body.");
    }

    #[test]
    fn test_rewrite_draft_recipients_folded_subject_interior_blank_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("folded_blank.md");
        // A folded `subject: >` block with a blank line separating two
        // paragraphs. The blank line is legal YAML and is NOT indented, so a
        // naive "stop at first non-indented line" consumer would orphan the
        // second paragraph and make the frontmatter unparseable.
        let original = concat!(
            "---\n",
            "to: old@example.com\n",
            "subject: >\n",
            "  first paragraph\n",
            "\n",
            "  second paragraph\n",
            "status: draft\n",
            "---\n",
            "\n",
            "Body stays.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "New subject".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        // Neither paragraph of the old folded scalar may survive.
        assert!(
            !result.contains("first paragraph"),
            "first paragraph not consumed: {result}"
        );
        assert!(
            !result.contains("second paragraph"),
            "orphaned second paragraph after interior blank line: {result}"
        );
        assert!(result.contains("status: draft"));

        // Must still parse cleanly with body preserved.
        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.to.as_deref(), Some("new@example.com"));
        assert_eq!(draft.frontmatter.subject, "New subject");
        assert_eq!(draft.body_markdown, "Body stays.");
    }

    #[test]
    fn test_rewrite_draft_recipients_literal_to_interior_blank_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("literal_blank.md");
        // A literal `to: |` block with an interior blank line between entries.
        let original = concat!(
            "---\n",
            "to: |\n",
            "  first@example.com\n",
            "\n",
            "  second@example.com\n",
            "subject: Old\n",
            "status: draft\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "New".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        assert!(
            !result.contains("first@example.com"),
            "first literal line not consumed: {result}"
        );
        assert!(
            !result.contains("second@example.com"),
            "orphaned literal line after interior blank line: {result}"
        );

        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.to.as_deref(), Some("new@example.com"));
        assert_eq!(draft.frontmatter.subject, "New");
        assert_eq!(draft.body_markdown, "Body.");
    }

    #[test]
    fn test_rewrite_draft_recipients_block_scalar_trailing_blank_before_next_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trailing_blank.md");
        // A managed block scalar followed by a blank line and then a NEXT
        // top-level key. The trailing blank belongs to the document, not the
        // scalar, and the next key must survive verbatim.
        let original = concat!(
            "---\n",
            "subject: >\n",
            "  folded line one\n",
            "  folded line two\n",
            "\n",
            "custom_field: keep-me\n",
            "status: draft\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "New".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        assert!(!result.contains("folded line one"), "scalar not consumed: {result}");
        assert!(!result.contains("folded line two"));
        // The next top-level key survives verbatim.
        assert!(
            result.contains("custom_field: keep-me"),
            "next top-level key was swallowed: {result}"
        );

        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.subject, "New");
        assert_eq!(draft.body_markdown, "Body.");
    }

    #[test]
    fn test_rewrite_draft_recipients_nested_key_not_matched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested.md");
        // A nested `to:` under `meta:` must NOT be treated as the top-level
        // recipient key, and must not produce duplicate top-level keys.
        let original = concat!(
            "---\n",
            "to: old@example.com\n",
            "subject: Old\n",
            "meta:\n",
            "  to: nested@example.com\n",
            "status: draft\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "New".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        // The nested key is preserved verbatim under `meta:`.
        assert!(
            result.contains("meta:\n  to: nested@example.com"),
            "nested key mangled: {result}"
        );
        // Exactly one top-level `to:` line (the rewritten one).
        let top_level_to = result
            .lines()
            .filter(|l| l.starts_with("to: ") || *l == "to:")
            .count();
        assert_eq!(top_level_to, 1, "duplicate top-level to keys: {result}");

        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.to.as_deref(), Some("new@example.com"));
        assert_eq!(draft.frontmatter.subject, "New");
    }

    #[test]
    fn test_rewrite_draft_recipients_preserves_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crlf.md");
        let original = concat!(
            "---\r\n",
            "to: old@example.com\r\n",
            "subject: Old\r\n",
            "status: draft\r\n",
            "---\r\n",
            "\r\n",
            "Body with CRLF.\r\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "New".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        // Frontmatter lines must use CRLF, not bare LF (no mixed endings).
        assert!(result.contains("to: \"new@example.com\"\r\n"), "frontmatter not CRLF: {result:?}");
        assert!(
            !result.lines().any(|l| l.starts_with("to:") && !result.contains(&format!("{l}\r"))),
            "mixed endings: {result:?}"
        );
        // No lone LF that isn't part of a CRLF pair.
        assert!(!result.replace("\r\n", "").contains('\n'), "stray LF found: {result:?}");

        let draft = parse_email_draft(&path).unwrap();
        assert_eq!(draft.frontmatter.to.as_deref(), Some("new@example.com"));
        assert_eq!(draft.frontmatter.subject, "New");
    }

    #[test]
    fn test_rewrite_draft_recipients_duplicate_key_does_not_multiply() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dup.md");
        // Two top-level `to:` lines (malformed input). Both are managed keys
        // and get replaced; the result must not multiply the count.
        let original = concat!(
            "---\n",
            "to: a@example.com\n",
            "to: b@example.com\n",
            "subject: Old\n",
            "status: draft\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "new@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "New".to_string(),
        };
        rewrite_draft_recipients(&path, &edit).unwrap();

        let result = fs::read_to_string(&path).unwrap();
        let to_count = result
            .lines()
            .filter(|l| l.starts_with("to: ") || *l == "to:")
            .count();
        // Both duplicate keys are replaced in place; the count is unchanged
        // (2), never multiplied. (Duplicate top-level keys are malformed YAML
        // to begin with, so this only guards against the rewriter making the
        // situation worse by appending extra copies.)
        assert_eq!(to_count, 2, "to keys multiplied: {result}");
        // Both were rewritten to the new value.
        assert!(!result.contains("a@example.com"), "stale value: {result}");
        assert!(!result.contains("b@example.com"), "stale value: {result}");
        assert_eq!(
            result.matches("to: \"new@example.com\"").count(),
            2,
            "both duplicates should hold the new value: {result}"
        );
    }

    #[test]
    fn test_rewrite_draft_recipients_errors_on_missing_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nofm.md");
        let original = "Just a body, no frontmatter fence.\n";
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "x@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "S".to_string(),
        };
        let result = rewrite_draft_recipients(&path, &edit);
        assert!(result.is_err());
        // File must be left untouched on error (no data loss).
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn test_rewrite_draft_recipients_errors_on_unclosed_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("unclosed.md");
        let original = "---\nto: a@x.com\nsubject: S\nno closing fence here\n";
        fs::write(&path, original).unwrap();

        let edit = DraftRecipientEdit {
            to: "x@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "S".to_string(),
        };
        let result = rewrite_draft_recipients(&path, &edit);
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn test_new_draft_skeleton_attachments_none() {
        let skeleton = new_draft_skeleton("me@example.com", "Thu, 10 Jul 2026 08:00:00 +0000");
        let matter = Matter::<YAML>::new();
        let parsed = matter.parse(&skeleton);
        let fm: EmailFrontmatter = parsed.data.unwrap().deserialize().unwrap();
        assert_eq!(fm.attachments, None);
        assert_eq!(fm.status, EmailStatus::Draft);
        assert_eq!(fm.from.as_deref(), Some("me@example.com"));
        assert_eq!(fm.subject, "");
    }

    /// The skeleton parses as written, with no edit first (#0050). It used to
    /// need `subject:` filled in before it could be read at all, which meant
    /// the drafts index skipped the draft `mp new` had just created and the
    /// selector `mp new` printed resolved to nothing.
    #[test]
    fn the_new_draft_skeleton_parses_without_being_edited_first() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fresh.md");
        fs::write(
            &path,
            new_draft_skeleton("me@example.com", "Thu, 10 Jul 2026 08:00:00 +0000"),
        )
        .unwrap();

        let draft = parse_email_draft(&path).expect("a draft we just wrote must parse");
        assert_eq!(draft.frontmatter.subject, "");
        // Validation is still the place that says it is not sendable.
        let err = validate_draft(&draft).unwrap_err().to_string();
        assert!(err.contains("No recipients"), "{err}");
    }

    #[test]
    fn test_validate_draft_valid() {
        let draft = make_draft(
            "alice@example.com",
            "Hello",
            "Body text",
            EmailStatus::Draft,
        );
        let result = validate_draft(&draft);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_validate_draft_missing_to() {
        let draft = make_draft("", "Hello", "Body text", EmailStatus::Draft);
        let result = validate_draft(&draft);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No recipients"));
    }

    #[test]
    fn test_validate_draft_bcc_only() {
        let mut draft = make_draft("", "Hello", "Body text", EmailStatus::Draft);
        draft.frontmatter.bcc = Some("secret@example.com".to_string());
        let result = validate_draft(&draft);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_draft_missing_subject() {
        let draft = make_draft("alice@example.com", "", "Body text", EmailStatus::Draft);
        let result = validate_draft(&draft);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("subject"));
    }

    #[test]
    fn test_validate_draft_empty_body_warning() {
        let draft = make_draft("alice@example.com", "Hello", "", EmailStatus::Draft);
        let result = validate_draft(&draft);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("empty"));
    }

    #[test]
    fn test_validate_draft_invalid_email() {
        let draft = make_draft("not-an-email", "Hello", "Body", EmailStatus::Draft);
        let result = validate_draft(&draft);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid email"));
    }

    #[test]
    fn test_validate_draft_multiple_recipients_one_invalid() {
        let draft = make_draft(
            "alice@example.com, badaddr",
            "Hello",
            "Body",
            EmailStatus::Draft,
        );
        let result = validate_draft(&draft);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_drafts_dir_explicit_path() {
        let result = resolve_drafts_dir(Path::new("/explicit/path"), &None);
        assert_eq!(result, PathBuf::from("/explicit/path"));
    }

    #[test]
    fn test_resolve_drafts_dir_config_fallback() {
        // When cli_dir is ".", use config dir if it exists
        // We can't easily test a real dir here, so test the default fallback
        let result = resolve_drafts_dir(Path::new("."), &Some(PathBuf::from("/nonexistent")));
        // /nonexistent doesn't exist as a dir, so should fall back to "."
        assert_eq!(result, PathBuf::from("."));
    }

    #[test]
    fn test_resolve_drafts_dir_default_fallback() {
        let result = resolve_drafts_dir(Path::new("."), &None);
        assert_eq!(result, PathBuf::from("."));
    }

    // ------------------------------------------------------------------
    // set_event_attendee_status (#0030): surgical attendee-status rewrite.
    // ------------------------------------------------------------------

    fn attendee_status(path: &Path, address: &str) -> Option<String> {
        let matter = Matter::<YAML>::new();
        let parsed = matter.parse(&fs::read_to_string(path).unwrap());
        let fm: crate::types::InboxFrontmatter = parsed.data.unwrap().deserialize().unwrap();
        fm.event?
            .attendees
            .into_iter()
            .find(|a| a.address.eq_ignore_ascii_case(address))
            .map(|a| a.status)
    }

    #[test]
    fn test_set_event_attendee_status_updates_matching_and_preserves_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite.md");
        let original = concat!(
            "---\n",
            "from: \"me@example.com\"\n",
            "to: \"a@example.com, b@example.com\"\n",
            "subject: LOC Day planning\n",
            "status: sent\n",
            "event:\n",
            "  uid: abc123@mailypoppins\n",
            "  method: REQUEST\n",
            "  sequence: 0\n",
            "  rsvp: needs-action\n",
            "  attendees:\n",
            "  - address: a@example.com\n",
            "    status: needs-action\n",
            "  - address: b@example.com\n",
            "    status: needs-action\n",
            "---\n",
            "\n",
            "You are invited.\nLine with --- dashes.\n",
        );
        fs::write(&path, original).unwrap();

        let r = set_event_attendee_status(&path, "A@Example.com", "accepted").unwrap();
        assert_eq!(r, AttendeeUpdate::Updated);
        assert_eq!(attendee_status(&path, "a@example.com").as_deref(), Some("accepted"));
        // Sibling attendee untouched.
        assert_eq!(
            attendee_status(&path, "b@example.com").as_deref(),
            Some("needs-action")
        );
        let result = fs::read_to_string(&path).unwrap();
        // Own rsvp untouched, body preserved byte-for-byte.
        assert!(result.contains("  rsvp: needs-action"));
        assert!(result.ends_with("\nYou are invited.\nLine with --- dashes.\n"));
    }

    #[test]
    fn test_set_event_attendee_status_idempotent_no_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite.md");
        let original = concat!(
            "---\n",
            "subject: S\nstatus: sent\n",
            "event:\n",
            "  uid: u@x\n  method: REQUEST\n  attendees:\n",
            "  - address: a@example.com\n    status: accepted\n",
            "---\n\nBody.\n",
        );
        fs::write(&path, original).unwrap();

        // Applying the same status is a no-op that leaves bytes identical.
        let r = set_event_attendee_status(&path, "a@example.com", "accepted").unwrap();
        assert_eq!(r, AttendeeUpdate::Unchanged);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn test_set_event_attendee_status_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite.md");
        let original = concat!(
            "---\nsubject: S\nstatus: sent\n",
            "event:\n  uid: u@x\n  method: REQUEST\n  attendees:\n",
            "  - address: a@example.com\n    status: needs-action\n",
            "---\n\nBody.\n",
        );
        fs::write(&path, original).unwrap();

        let r = set_event_attendee_status(&path, "stranger@example.com", "declined").unwrap();
        assert_eq!(r, AttendeeUpdate::NotFound);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn test_set_event_attendee_status_errors_without_event_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.md");
        let original = "---\nsubject: S\nstatus: sent\n---\n\nBody.\n";
        fs::write(&path, original).unwrap();
        assert!(set_event_attendee_status(&path, "a@x.com", "accepted").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn test_set_event_attendee_status_ignores_fake_keys_in_block_scalar() {
        // A `description:` literal block scalar whose text contains lines that
        // look exactly like `- address:` / `status:` mapping entries. The
        // line-surgical rewriter must NOT touch them: only the real attendee
        // under `attendees:` may change.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite.md");
        let original = concat!(
            "---\n",
            "from: me@example.com\nto: real@example.com\nsubject: S\nstatus: sent\n",
            "event:\n",
            "  uid: u@x\n",
            "  method: REQUEST\n",
            "  description: |\n",
            "    Notes from the planning thread:\n",
            "    - address: decoy@evil.com\n",
            "      status: accepted\n",
            "\n",
            "    (interior blank line above is part of the block)\n",
            "  attendees:\n",
            "  - address: real@example.com\n",
            "    status: needs-action\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        fs::write(&path, original).unwrap();

        let r = set_event_attendee_status(&path, "real@example.com", "declined").unwrap();
        assert_eq!(r, AttendeeUpdate::Updated);
        let result = fs::read_to_string(&path).unwrap();
        // Decoy lines inside the block scalar survive verbatim.
        assert!(result.contains("    - address: decoy@evil.com"), "{result}");
        assert!(
            result.contains("      status: accepted"),
            "decoy status must not change: {result}"
        );
        // The real attendee flipped.
        assert_eq!(
            attendee_status(&path, "real@example.com").as_deref(),
            Some("declined")
        );
        // Interior blank line + trailing description prose preserved.
        assert!(result.contains("    (interior blank line above is part of the block)"));
    }

    #[test]
    fn test_set_event_attendee_status_crlf_and_quoted_unicode_address() {
        // CRLF line endings + a double-quoted attendee address containing a
        // unicode label. The rewriter must match on the unquoted address and
        // re-emit CRLF endings.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite.md");
        let original = concat!(
            "---\r\n",
            "from: me@example.com\r\nto: b@example.com\r\nsubject: S\r\nstatus: sent\r\n",
            "event:\r\n",
            "  uid: u@x\r\n",
            "  method: REQUEST\r\n",
            "  attendees:\r\n",
            "  - address: \"rené@exämple.com\"\r\n",
            "    status: needs-action\r\n",
            "  - address: b@example.com\r\n",
            "    status: needs-action\r\n",
            "---\r\n",
            "\r\n",
            "Body.\r\n",
        );
        fs::write(&path, original).unwrap();

        let r = set_event_attendee_status(&path, "rené@exämple.com", "tentative").unwrap();
        assert_eq!(r, AttendeeUpdate::Updated);
        let result = fs::read_to_string(&path).unwrap();
        // CRLF preserved throughout.
        assert!(result.contains("\r\n"), "CRLF must be preserved");
        assert!(!result.contains("\n\n\n"), "no stray bare-LF blocks: {result:?}");
        // The quoted-address attendee flipped; the other stayed.
        assert!(result.contains("    status: tentative\r\n"), "{result:?}");
        assert_eq!(
            attendee_status(&path, "b@example.com").as_deref(),
            Some("needs-action")
        );
    }

    #[test]
    fn test_set_event_attendee_status_inline_status_key() {
        // A compact YAML style where `status:` is the item's *first* key,
        // inline on the `- ` line, and `address:` is the continuation.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invite.md");
        let original = concat!(
            "---\nfrom: me@example.com\nto: a@example.com\nsubject: S\nstatus: sent\n",
            "event:\n  uid: u@x\n  method: REQUEST\n  attendees:\n",
            "  - status: needs-action\n    address: a@example.com\n",
            "---\n\nBody.\n",
        );
        fs::write(&path, original).unwrap();
        let r = set_event_attendee_status(&path, "a@example.com", "accepted").unwrap();
        assert_eq!(r, AttendeeUpdate::Updated);
        // Assert on raw bytes: this compact `- status:`-first item ordering is
        // valid YAML but never emitted by serde in practice, so we verify the
        // surgical rewrite directly rather than round-tripping through serde.
        let result = fs::read_to_string(&path).unwrap();
        assert!(result.contains("  - status: accepted\n"), "{result}");
        assert!(result.contains("    address: a@example.com\n"), "{result}");
    }

    // -----------------------------------------------------------------------
    // Status transitions are line surgery, not a frontmatter round-trip
    //
    // These used to parse into `EmailFrontmatter` and re-serialize, which
    // silently dropped every key that struct does not model. Losing `date:`
    // made the TUI fall back to the filename (unparseable for
    // `draft-%Y%m%d-%H%M%S.md`), so `date_sort` went empty and the approved
    // row teleported to the bottom of the list.
    // -----------------------------------------------------------------------

    /// A draft carrying `date:` plus a field no struct models.
    fn draft_with_unknown_fields(dir: &Path, status: &str) -> PathBuf {
        let path = dir.join("draft-20260701-120000-hello.md");
        fs::write(
            &path,
            format!(
                concat!(
                    "---\n",
                    "to: alice@example.com\n",
                    "subject: Hello\n",
                    "status: {}\n",
                    "from: me@example.com\n",
                    "date: 2026-07-01T12:00:00+02:00\n",
                    "x-ticket: PROJ-42\n",
                    "tags:\n",
                    "  - urgent\n",
                    "  - billing\n",
                    "---\n",
                    "\n",
                    "Body stays put.\n",
                ),
                status,
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn mark_as_approved_only_rewrites_the_status_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = draft_with_unknown_fields(tmp.path(), "draft");
        let before = fs::read_to_string(&path).unwrap();

        mark_as_approved(&path).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, before.replace("status: draft", "status: approved"));
    }

    #[test]
    fn mark_as_draft_only_rewrites_the_status_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = draft_with_unknown_fields(tmp.path(), "approved");
        let before = fs::read_to_string(&path).unwrap();

        mark_as_draft(&path).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, before.replace("status: approved", "status: draft"));
    }

    /// The approve/demote round trip must be a byte-level identity, and in
    /// particular must not add the `cc: null` / `bcc: null` noise a serde
    /// round-trip produced.
    #[test]
    fn approve_then_demote_round_trips_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let path = draft_with_unknown_fields(tmp.path(), "draft");
        let before = fs::read_to_string(&path).unwrap();

        mark_as_approved(&path).unwrap();
        mark_as_draft(&path).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    /// Existing validation stays: only the write path changed.
    #[test]
    fn mark_as_draft_still_refuses_a_sent_email() {
        let tmp = tempfile::tempdir().unwrap();
        let path = draft_with_unknown_fields(tmp.path(), "sent");
        let before = fs::read_to_string(&path).unwrap();

        assert!(mark_as_draft(&path).is_err());
        assert!(mark_as_approved(&path).is_err());
        // The rejected file is left untouched.
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn mark_draft_sent_preserves_date_and_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = draft_with_unknown_fields(tmp.path(), "approved");
        let draft = parse_email_draft(&path).unwrap();

        mark_draft_sent(&draft, Some("<abc@example.com>")).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("status: sent\n"), "{after}");
        assert!(after.contains("date: 2026-07-01T12:00:00+02:00\n"), "{after}");
        assert!(after.contains("x-ticket: PROJ-42\n"), "{after}");
        assert!(after.contains("  - urgent\n"), "{after}");
        assert!(after.contains("message_id: \"<abc@example.com>\"\n"), "{after}");
        assert!(after.contains("sent_at: 20"), "{after}");
        assert!(after.contains("sent_via: \"email-cli v"), "{after}");
        assert!(after.contains("Body stays put.\n"), "{after}");
        // The re-parsed file still round-trips through the struct.
        let reparsed = parse_email_draft(&path).unwrap();
        assert_eq!(reparsed.frontmatter.status, EmailStatus::Sent);
        assert_eq!(
            reparsed.frontmatter.message_id.as_deref(),
            Some("<abc@example.com>")
        );
    }

    /// `mp invite` builds a synthetic draft that was never written to disk.
    /// There is nothing to mark and nothing to write: the sent copy is the
    /// outbox's, not a file this function invents (#0037).
    #[test]
    fn mark_draft_sent_is_a_no_op_for_a_draft_with_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut draft = make_draft("a@example.com", "Invite", "Body", EmailStatus::Approved);
        draft.path = tmp.path().join("20260701-120000-invite.md");

        mark_draft_sent(&draft, None).unwrap();

        assert!(
            !draft.path.exists(),
            "a draft with no file must not be materialised by marking it sent"
        );
    }

    /// `write_atomic` renames a fresh temp file over the destination, which
    /// replaces the inode: without an explicit copy, a draft the user had
    /// restricted to 0600 came back with the umask default after every
    /// status change.
    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_the_targets_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("private.md");

        fs::write(&path, b"first").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&path, b"second").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "write_atomic reset the permission bits");
    }

    /// The same guarantee through a real caller: approving a 0600 draft must
    /// not widen it to 0644.
    #[cfg(unix)]
    #[test]
    fn mark_as_approved_preserves_a_0600_drafts_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = draft_with_unknown_fields(tmp.path(), "draft");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        mark_as_approved(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "approving a draft widened its permissions");
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("status: approved\n"));
    }

    /// A file that did not exist keeps default umask behaviour: no mode is
    /// forced, so the result matches an ordinary `fs::write`.
    #[cfg(unix)]
    #[test]
    fn write_atomic_leaves_a_new_file_to_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let reference = tmp.path().join("reference.md");
        fs::write(&reference, b"x").unwrap();
        let expected = fs::metadata(&reference).unwrap().permissions().mode() & 0o777;

        let path = tmp.path().join("fresh.md");
        write_atomic(&path, b"x").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, expected);
    }
}

pub fn mark_as_approved(path: &Path) -> Result<String> {
    let draft = parse_email_draft(path)?;

    if draft.frontmatter.status == EmailStatus::Approved {
        return Ok(format!("Already approved: {}", path.display()));
    }

    if draft.frontmatter.status == EmailStatus::Sent {
        return Err(anyhow!("Cannot approve an already sent email"));
    }

    // Surgical `status:` rewrite: re-serializing `EmailFrontmatter` would
    // drop every field it does not model, including the `date:` line the
    // TUI sorts on (the approved draft then jumped to the bottom of the
    // list) and any user-added key.
    rewrite_frontmatter_scalars_at(
        path,
        &[("status", FieldWrite::Set(EmailStatus::Approved.to_string()))],
    )?;

    Ok(format!("Marked as approved: {}", path.display()))
}

/// Reverse of [`mark_as_approved`] -- demote a draft back to `draft` status.
///
/// Useful when the user pressed `A` by mistake and wants to keep editing.
/// Only `approved` drafts can be demoted: `draft` is a no-op (returns an
/// `Already a draft` message), and `sent` / `inbox` / `archived` files are
/// rejected with an error -- those have left the draft pipeline and must
/// not be silently rewritten.
pub fn mark_as_draft(path: &Path) -> Result<String> {
    let draft = parse_email_draft(path)?;

    match draft.frontmatter.status {
        EmailStatus::Draft => return Ok(format!("Already a draft: {}", path.display())),
        EmailStatus::Approved => {}
        EmailStatus::Sent => {
            return Err(anyhow!("Cannot revert a sent email back to draft"));
        }
        EmailStatus::Inbox | EmailStatus::Archived => {
            return Err(anyhow!(
                "Only approved drafts can be marked as draft (status was {})",
                draft.frontmatter.status
            ));
        }
    }

    // Surgical `status:` rewrite, see `mark_as_approved`.
    rewrite_frontmatter_scalars_at(
        path,
        &[("status", FieldWrite::Set(EmailStatus::Draft.to_string()))],
    )?;

    Ok(format!("Marked as draft: {}", path.display()))
}
