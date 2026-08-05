use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result};
use ratatui::{backend::CrosstermBackend, Terminal};

use super::app::{
    mailbox_key, open_store, Action, App, BgResult, ComposeField, ComposeMode, ComposeWizard,
    Focus, MailboxKind, MessageRef, Overlay, StatusLevel,
};
use super::helpers::{
    edit_file, lib_do_multi_search_graph, lib_do_sync_graph, resume_terminal, suspend_terminal,
};
use super::mutations::{self, Backend, Prepared, ServerOp};
use super::ui;

use crate::draft::{
    find_drafts, mark_draft_sent, new_draft_skeleton, DraftRecipientEdit, SourceMessage,
};
use crate::selector::Selector;
use crate::store::BlobStore;
use crate::types::EmailStatus;

// ---------------------------------------------------------------------------
// Files materialised out of the store (#0052 scope items 8, 9 and 10)
// ---------------------------------------------------------------------------

/// The directory `mp open` materialises a row's attachments into: one per
/// message row, under the system temp dir.
///
/// The CLI's own name, deliberately. Opening the same message from both halves
/// of the product then puts the same files in the same place, and the bytes
/// are rewritten before every open, so a directory two accounts share (row ids
/// are per-account) never hands the opener a stale file: only the paths just
/// written are returned.
fn attachment_temp_dir(row_id: i64) -> PathBuf {
    std::env::temp_dir().join(format!("mailypoppins-{row_id}"))
}

/// The message under the cursor for a flow that reads its blobs, or `None`
/// with the status line saying why there is not one.
///
/// Same shape as [`cursor_message`], different explanation: a draft's
/// attachments are the paths in its own `attachments:` list, not blobs of a
/// received message, so there is nothing here to materialise for it.
fn cursor_message_for_files(app: &mut App, what: &str) -> Option<MessageRef> {
    if let Some(msg) = app.selected_email_ref() {
        return Some(msg);
    }
    if app.selected_email().is_some() {
        app.set_status_level(
            format!("{what} needs a received message; a draft carries its own in `attachments:`"),
            StatusLevel::Warning,
        );
    }
    None
}

/// The attachments of the row under the cursor, materialised into files
/// (#0052 scope item 8).
///
/// This is `mp open` / `mp save`'s own read: [`materialise_attachments`]
/// writes the row's blobs into the temp directory keyed by the row and hands
/// back the paths. The picker and the save pipeline above it still address
/// files, which they always did; what changed is where the bytes come from.
///
/// An empty vector is a message with no attachments, which is the caller's
/// status line to write. `None` is a failure already on the status line.
pub(super) fn cursor_attachment_files(app: &mut App) -> Option<Vec<PathBuf>> {
    let msg = cursor_message_for_files(app, "Attachments")?;
    row_attachment_files(app, msg.row_id())
}

/// [`cursor_attachment_files`] for a row named directly, which is the
/// server-search hit that resolved to one.
pub(super) fn row_attachment_files(app: &mut App, row_id: i64) -> Option<Vec<PathBuf>> {
    let (store, blobs) = store_for_mutation(app, "Attachments")?;
    let dest = attachment_temp_dir(row_id);
    match crate::store::read::materialise_attachments(&store, &blobs, row_id, &dest) {
        Ok(files) => Some(files),
        Err(e) => {
            app.set_status_level(format!("Attachments failed: {e:#}"), StatusLevel::Error);
            None
        }
    }
}

/// The attachments of a server-search hit that resolved to no local row: the
/// bytes the overlay is already holding, written where the store-backed ones
/// go so the picker sees files either way (#0052 scope item 11).
///
/// Keyed by the hit's position in the result list rather than by a row id it
/// does not have. The filename is sanitised the way ingest sanitises it, so a
/// hostile `../` in a Content-Disposition cannot escape the temp directory.
pub(super) fn fetched_attachment_files(
    app: &mut App,
    fetched: &crate::parse::FetchedEmail,
    index: usize,
) -> Option<Vec<PathBuf>> {
    let dest = std::env::temp_dir().join(format!("mailypoppins-search-{index}"));
    match write_fetched_attachments(fetched, &dest) {
        Ok(files) => Some(files),
        Err(e) => {
            app.set_status_level(format!("Attachments failed: {e:#}"), StatusLevel::Error);
            None
        }
    }
}

fn write_fetched_attachments(
    fetched: &crate::parse::FetchedEmail,
    dest: &Path,
) -> Result<Vec<PathBuf>> {
    if fetched.attachments.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dest)?;
    let mut written = Vec::new();
    for att in &fetched.attachments {
        let out = dest.join(crate::parse::sanitize_attachment_filename(&att.filename));
        std::fs::write(&out, &att.content)?;
        written.push(out);
    }
    Ok(written)
}

/// The HTML rendition of a message, written to a file a browser can open
/// (#0052 scope item 9).
///
/// The file build wrote a `.html` beside every received `.md` and the browser
/// opened that one; after #0037 the markup is a blob, or the html part of the
/// raw message, so it is materialised on demand into the same temp area the
/// attachments use. `stem` names the file: the row id for a stored message,
/// the hit's position for one that is not stored.
fn html_temp_file(html: &str, stem: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("mailypoppins-{stem}.html"));
    std::fs::write(&path, html)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The browser rendition of a stored row, or `None` with the status line
/// saying why: a message whose sender wrote no markup has none, which is not
/// an error.
pub(super) fn html_rendition_for_row(app: &mut App, row_id: i64) -> Option<PathBuf> {
    let (store, blobs) = store_for_mutation(app, "Open in browser")?;
    let Some(html) = crate::store::read::load_html(&store, &blobs, row_id) else {
        app.set_status("No HTML version available".to_string());
        return None;
    };
    html_rendition(app, &html, &row_id.to_string())
}

/// [`html_rendition_for_row`] over markup already in hand, which is the
/// server-search hit that resolved to no row.
pub(super) fn html_rendition(app: &mut App, html: &str, stem: &str) -> Option<PathBuf> {
    match html_temp_file(html, stem) {
        Ok(path) => Some(path),
        Err(e) => {
            app.set_status_level(format!("Open failed: {e:#}"), StatusLevel::Error);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Drafts written from a source message (#0052 scope items 1, 2 and 11)
// ---------------------------------------------------------------------------

/// Which draft a [`SourceMessage`] is turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DraftFromSource {
    Reply { all: bool },
    Forward,
}

/// The message under the cursor, or `None` with the status line saying why
/// there is not one.
///
/// A Drafts row is the case worth naming: it has no `messages` row behind it,
/// so there is nothing to quote, and an empty list is not an error at all.
fn cursor_message(app: &mut App, what: &str) -> Option<MessageRef> {
    if let Some(msg) = app.selected_email_ref() {
        return Some(msg);
    }
    if app.selected_email().is_some() {
        app.set_status_level(
            format!("{what} needs a received message; a draft has none to quote"),
            StatusLevel::Warning,
        );
    }
    None
}

/// The source of a reply or a forward, read off the store row under the
/// cursor, or `None` with the status line saying why not.
///
/// This is `mp reply` / `mp forward`'s own path: the row is resolved by id,
/// the quote and the HTML companion come out of `message_blobs`, and the
/// forward's attachments are materialised by the same
/// [`crate::draft::source_from_row`] the CLI calls. Nothing here reads a
/// `.md` file, because there is not one.
fn source_for_msg(
    app: &mut App,
    msg: MessageRef,
    what: &str,
    with_attachments: bool,
) -> Option<SourceMessage> {
    let (store, blobs) = store_for_mutation(app, what)?;
    let row = match crate::store::read::find_by_id(&store, msg.row_id()) {
        Ok(Some(row)) => row,
        Ok(None) => {
            app.set_status_level(
                format!("{what} failed: that message is no longer in the store"),
                StatusLevel::Error,
            );
            return None;
        }
        Err(e) => {
            app.set_status_level(format!("{what} failed: {e:#}"), StatusLevel::Error);
            return None;
        }
    };
    match crate::draft::source_from_row(&store, &blobs, &row, with_attachments) {
        Ok(source) => Some(source),
        Err(e) => {
            app.set_status_level(format!("{what} failed: {e:#}"), StatusLevel::Error);
            None
        }
    }
}

/// Write the draft `source` produces, mint its `id:`, and refresh the drafts
/// index so the selector this hands back resolves before the status line shows
/// it (#0050's post-write refresh discipline).
///
/// `headers` is the compose wizard's recipient/subject block, applied to the
/// file before the id is minted so the index holds the final content. `None`
/// is the direct reply/forward, which takes the builder's own headers.
///
/// Shared by the list, the preview and the search overlay, and byte for byte
/// the CLI's sequence: build, `set_draft_id`, reindex, name the draft.
fn draft_from_source(
    account: &str,
    default_from: &str,
    source: &SourceMessage,
    kind: DraftFromSource,
    headers: Option<&DraftRecipientEdit>,
) -> Result<(PathBuf, Selector)> {
    let dir = crate::config::drafts_dir(account);
    let path = match kind {
        DraftFromSource::Reply { all } => {
            crate::draft::create_reply_draft_from(source, all, default_from, Some(&dir))?
        }
        DraftFromSource::Forward => {
            crate::draft::create_forward_draft_from(source, default_from, Some(&dir))?
        }
    };
    if let Some(edit) = headers {
        crate::draft::rewrite_draft_recipients(&path, edit)?;
    }
    let id = crate::store::drafts::new_id();
    crate::draft::set_draft_id(&path, &id)?;
    crate::store::drafts::refresh_account(account)?;
    Ok((path, Selector::for_draft(account, &id)))
}

/// The address a draft this account writes is sent from: the SMTP config's,
/// falling back to the account's own default.
fn default_from(app: &App) -> String {
    app.smtp_config
        .as_ref()
        .map(|s| s.default_from.clone())
        .unwrap_or_else(|| app.account_config.default_from.clone())
}

/// Build the draft and hand it straight to `$EDITOR`, which is what reply and
/// forward did before the read path moved (the draft is a starting point, not
/// a finished message).
fn write_draft_and_edit(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    source: &SourceMessage,
    kind: DraftFromSource,
    what: &str,
) -> Result<()> {
    let account = app.account_config.name.clone();
    let from = default_from(app);
    let (path, selector) = match draft_from_source(&account, &from, source, kind, None) {
        Ok(pair) => pair,
        Err(e) => {
            app.set_status_level(format!("{what} failed: {e:#}"), StatusLevel::Error);
            return Ok(());
        }
    };
    edit_new_draft(app, terminal, &path, format!("{what} draft ready: {selector}"))
}

/// Hand a freshly written draft to `$EDITOR` and put `ready` on the status
/// line, with the list, the index and the sidebar caught up afterwards.
///
/// The index is refreshed a second time on the way out because the editor
/// session is a write this application did not make: the subject and the
/// recipients the user just typed are what the Drafts list has to show.
fn edit_new_draft(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &Path,
    ready: String,
) -> Result<()> {
    if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
        app.invalidate_cache_idx(idx);
    }
    suspend_terminal(terminal)?;
    let result = edit_file(path);
    resume_terminal(terminal)?;
    match result {
        Ok(()) => app.set_status(ready),
        Err(e) => app.set_status_level(format!("Edit failed: {e}"), StatusLevel::Error),
    }
    if let Err(e) = crate::store::drafts::refresh_account(&app.account_config.name) {
        log::warn!("[drafts] refreshing after the editor session failed: {e:#}");
    }
    app.recount_all_mailboxes();
    app.reload_current_mailbox();
    Ok(())
}

/// The subject the forward wizard opens with: the row's own subject under a
/// `Fwd:` prefix, or a bare prefix when the row cannot be read.
///
/// The list entry is not the source here: it substitutes "(no subject)" for an
/// empty subject, and that placeholder must not end up in a sent header.
fn forward_subject(app: &App, msg: MessageRef) -> String {
    let subject = crate::tui::app::open_store(&app.account_config.name)
        .and_then(|store| crate::store::read::find_by_id(&store, msg.row_id()).ok().flatten())
        .and_then(|row| row.subject)
        .unwrap_or_default();
    crate::draft::fwd_subject(&subject)
}

/// The file behind an indexed draft id, without a status line: the batch
/// flows count their misses instead of narrating each one.
///
/// [`crate::store::Store::open`] rather than `open_store`, for the reason the
/// Drafts mailbox load gives: drafts are local-only files, so an account that
/// has never synced has no store file and still has drafts.
fn lookup_draft_path(account: &str, id: &str) -> Result<Option<PathBuf>> {
    let store = crate::store::Store::open(crate::config::store_path(account))?;
    Ok(crate::store::drafts::find(&store, account, id)?.map(|row| row.path))
}

/// The draft under the cursor, as its indexed id and the file that id names,
/// or `None` with the status line saying why there is not one.
///
/// `why` is what to say when the cursor is on a received message instead,
/// which every draft-only operation has to answer for itself: after #0037 a
/// received message is a store row, so "the same thing but for mail" does not
/// exist for editing, sending or approving.
fn cursor_draft(app: &mut App, why: &str) -> Option<(String, PathBuf)> {
    let id = match app.selected_email() {
        Some(email) => email.draft_id.clone(),
        None => return None,
    };
    let Some(id) = id else {
        app.set_status_level(why.to_string(), StatusLevel::Warning);
        return None;
    };
    let path = indexed_draft_path(app, &id)?;
    Some((id, path))
}

/// The file behind an indexed draft id, or `None` with the status line saying
/// the index no longer holds it.
fn indexed_draft_path(app: &mut App, id: &str) -> Option<PathBuf> {
    let account = app.account_config.name.clone();
    match lookup_draft_path(&account, id) {
        Ok(Some(path)) => Some(path),
        Ok(None) => {
            app.set_status_level(
                format!("That draft is no longer in the index ({id})"),
                StatusLevel::Error,
            );
            None
        }
        Err(e) => {
            app.set_status_level(
                format!("Reading the drafts index of {account} failed: {e:#}"),
                StatusLevel::Error,
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Approve and mark-draft (#0052 scope items 4 and 5)
// ---------------------------------------------------------------------------

/// Which way a draft's `status:` is flipped.
///
/// The legal transitions are not this module's to decide: they are
/// [`crate::draft::mark_as_approved`] and [`crate::draft::mark_as_draft`],
/// the same two functions `mp mark-approved` and `mp mark-draft` call, so an
/// illegal flip fails in the TUI with the error text the CLI prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftStatusFlip {
    Approve,
    Demote,
}

impl DraftStatusFlip {
    /// How the operation names itself in a failure line.
    fn what(self) -> &'static str {
        match self {
            DraftStatusFlip::Approve => "Approve",
            DraftStatusFlip::Demote => "Mark-draft",
        }
    }

    fn apply(self, path: &Path) -> Result<String> {
        match self {
            DraftStatusFlip::Approve => crate::draft::mark_as_approved(path),
            DraftStatusFlip::Demote => crate::draft::mark_as_draft(path),
        }
    }

    /// The line one flip shows, in the CLI's two shapes: the draft moved, or
    /// it was already where it was being moved to. Named by its selector,
    /// which is the handle the user can hand to `mp` (#0050), not by a path.
    fn line(self, already: bool, selector: &Selector) -> String {
        match (self, already) {
            (DraftStatusFlip::Approve, false) => format!("Approved {selector}"),
            (DraftStatusFlip::Approve, true) => format!("Already approved: {selector}"),
            (DraftStatusFlip::Demote, false) => format!("Demoted {selector}"),
            (DraftStatusFlip::Demote, true) => format!("Already a draft: {selector}"),
        }
    }
}

/// Re-index the drafts directory after a status flip and put the list, the
/// sidebar counts and the cached Drafts mailbox back in step with the files.
///
/// Same sequence as `mp mark-approved`'s `reindex_drafts`, plus the refresh of
/// the two things the CLI does not have: an open list and a sidebar count.
fn refresh_drafts_after_flip(app: &mut App) {
    if let Err(e) = crate::store::drafts::refresh_account(&app.account_config.name) {
        log::warn!("[drafts] refreshing after a status flip failed: {e:#}");
    }
    if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
        app.invalidate_cache_idx(idx);
    }
    app.recount_all_mailboxes();
    app.reload_current_mailbox();
}

/// Flip the `status:` of the draft under the cursor.
fn status_flip(app: &mut App, flip: DraftStatusFlip) {
    let why = format!(
        "{} needs a draft; received mail has no draft status to flip",
        flip.what()
    );
    let Some((id, path)) = cursor_draft(app, &why) else {
        return;
    };
    match flip.apply(&path) {
        Ok(msg) => {
            let selector = Selector::for_draft(&app.account_config.name, &id);
            app.set_status(flip.line(msg.starts_with("Already"), &selector));
            refresh_drafts_after_flip(app);
        }
        Err(e) => app.set_status_level(
            format!("{} failed: {e:#}", flip.what()),
            StatusLevel::Error,
        ),
    }
}

/// Flip the `status:` of every selected draft, counting what took it.
///
/// The counting and its two status-line shapes are the pre-nuke build's: a
/// batch is not all-or-nothing, and a draft the flip refuses (an already-sent
/// one, say) is one failure among N rather than an abort. The reason lands in
/// the log, because the status line has room for a count and not for N errors.
fn status_flip_batch(app: &mut App, ids: &[String], flip: DraftStatusFlip) {
    let total = ids.len();
    let account = app.account_config.name.clone();
    // One store open for the whole batch, not one per draft.
    let store = match crate::store::Store::open(crate::config::store_path(&account)) {
        Ok(store) => store,
        Err(e) => {
            app.set_status_level(
                format!("Reading the drafts index of {account} failed: {e:#}"),
                StatusLevel::Error,
            );
            return;
        }
    };
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    for id in ids {
        let outcome = match crate::store::drafts::find(&store, &account, id) {
            Ok(Some(row)) => flip.apply(&row.path).map(|_| ()),
            Ok(None) => Err(anyhow::anyhow!("no longer in the index")),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => succeeded += 1,
            Err(e) => {
                log::warn!("[drafts] {} failed for {id}: {e:#}", flip.what());
                failed += 1;
            }
        }
    }
    let line = match flip {
        DraftStatusFlip::Approve if failed == 0 => format!("Approved {succeeded} drafts"),
        DraftStatusFlip::Approve => {
            format!("Approved {succeeded}/{total} drafts ({failed} failed)")
        }
        DraftStatusFlip::Demote if failed == 0 => format!("Marked {succeeded} as draft"),
        DraftStatusFlip::Demote => {
            format!("Marked {succeeded}/{total} as draft ({failed} failed)")
        }
    };
    if failed == 0 {
        app.set_status(line);
    } else {
        app.set_status_level(line, StatusLevel::Warning);
    }
    // The refresh re-opens the store to write the index; this read handle has
    // no business being alive across it (WAL single-writer discipline).
    drop(store);
    refresh_drafts_after_flip(app);
}

// ---------------------------------------------------------------------------
// Send (#0052 scope item 3)
// ---------------------------------------------------------------------------

/// Everything one durable send needs that does not come out of the draft.
///
/// `graph` being `Some` is what picks the Graph submission over SMTP, the same
/// test `mp send` makes on `AuthMethod::Graph`; `smtp` is what the SMTP path
/// submits through and where its `from` fallback comes from.
struct SendCtx {
    graph: Option<crate::config::GraphConfig>,
    smtp: Option<crate::config::SmtpConfig>,
    account: crate::config::AccountConfig,
    email_settings: crate::config::EmailSettings,
    signature: Option<String>,
}

/// The attachments a draft names, read off the paths in its frontmatter.
///
/// Only the Graph path needs them separately: the SMTP path's bytes are built
/// by [`crate::send::build_draft_message`], which reads the same list itself.
fn draft_attachments(draft: &crate::types::EmailDraft) -> Result<Vec<(String, Vec<u8>, String)>> {
    let mut data = Vec::new();
    let Some(attachments) = draft.frontmatter.attachments.as_ref() else {
        return Ok(data);
    };
    for att_path in attachments {
        let expanded = shellexpand::tilde(att_path);
        let path = Path::new(expanded.as_ref());
        let content = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("reading the attachment {att_path}: {e}"))?;
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment".to_string());
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        data.push((filename, content, content_type));
    }
    Ok(data)
}

/// Send one draft the way `mp send <selector>` does, and leave the same trail.
///
/// The order is the CLI's, and the durability is the outbox's (#0037 item 5):
/// the bytes are built first (which is where the approved-status requirement
/// is enforced, by [`crate::send::build_draft_message`]), committed to the
/// outbox, submitted, and only a submission that reached at least one
/// recipient rewrites the draft's `status:` to `sent`. The sent copy is the
/// outbox's business, not this function's.
///
/// The drafts index is refreshed afterwards so the `sent` status is the answer
/// the next selector resolution gives, without waiting for the one-second
/// poll (#0050's post-write refresh discipline).
fn send_one_draft(
    rt: &tokio::runtime::Runtime,
    draft: &crate::types::EmailDraft,
    ctx: &SendCtx,
) -> Result<crate::send::SendReport> {
    // The Graph path has no SMTP config to take a `from` fallback from, so it
    // takes the account's; identical bytes either way (see
    // `build_draft_message`).
    let default_from = match (ctx.graph.as_ref(), ctx.smtp.as_ref()) {
        (Some(_), _) => ctx.account.default_from.clone(),
        (None, Some(smtp)) => smtp.default_from.clone(),
        (None, None) => anyhow::bail!("SMTP not configured"),
    };
    let built = crate::send::build_draft_message(
        draft,
        &default_from,
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
                std::fs::read_to_string(&quoted_html).ok()
            } else {
                None
            };
            let html_body = crate::send::markdown_to_html(
                &draft.body_markdown,
                &ctx.email_settings,
                ctx.signature.as_deref(),
                quoted.as_deref(),
            );
            let att_data = draft_attachments(draft)?;
            let client = rt.block_on(crate::graph::GraphClient::new_async(graph_config))?;
            rt.block_on(crate::send::send_durably_via(
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
            ))?
        }
        None => {
            let smtp = ctx
                .smtp
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("SMTP not configured"))?;
            rt.block_on(crate::send::send_durably(&built, &ctx.account, smtp))?
        }
    };

    if report.send_result.any_succeeded() {
        mark_draft_sent(draft, Some(&built.message_id))?;
        crate::contacts::hooks::bump_after_send(&ctx.account, draft);
        if let Err(e) = crate::store::drafts::refresh_account(&ctx.account.name) {
            log::warn!("[drafts] refreshing after the send failed: {e:#}");
        }
    }
    Ok(report)
}

/// The status line one finished send shows, which is the CLI's own report:
/// how many recipients took it, and where the message actually is (#0037).
fn send_status_line(report: &crate::send::SendReport) -> Result<String> {
    let result = &report.send_result;
    if result.all_succeeded() {
        Ok(format!(
            "Sent to {} recipient(s) [{}]",
            result.results.len(),
            report.status_line()
        ))
    } else if result.any_succeeded() {
        let failed: Vec<String> = result.failed().iter().map(|r| r.address.clone()).collect();
        Ok(format!(
            "Partial: {}/{} succeeded -- failed: {} [{}]",
            result.succeeded().len(),
            result.results.len(),
            failed.join(", "),
            report.status_line()
        ))
    } else {
        anyhow::bail!("Failed to send to all {} recipient(s)", result.results.len())
    }
}

// ---------------------------------------------------------------------------
// Mutation plumbing (#0038 scope item 7)
// ---------------------------------------------------------------------------

/// The account's store and blob store, or a status line saying why not.
///
/// A mutation without a store is not a silent no-op: the row it would have
/// written is the whole local half of the operation.
fn store_for_mutation(app: &mut App, what: &str) -> Option<(crate::store::Store, BlobStore)> {
    let account = app.account_config.name.clone();
    match open_store(&account) {
        Some(store) => Some((store, BlobStore::for_account(&account))),
        None => {
            app.set_status_level(
                format!("{what} failed: no store for {account} yet (sync first)"),
                StatusLevel::Error,
            );
            None
        }
    }
}

/// The canonical `mp://` selector of the entry under the cursor (#0050 scope
/// item 7), or `None` when the entry has no name to copy.
///
/// A drafts entry answers from its indexed `id:` without touching the store,
/// because a draft has no `messages` row. A received entry answers from a
/// lookup by row id, because the selector needs the Message-ID and the mailbox
/// and the list carries neither: [`MessageRef`] is deliberately just the
/// synthetic key. That is one indexed read per keypress, which is the right
/// side of the trade against widening every list row.
///
/// `None` is the server-search hit that does not resolve locally: it has no
/// store row, so there is no selector that would name it for the CLI.
fn selected_selector(app: &App) -> Option<crate::selector::Selector> {
    let account = &app.account_config.name;
    let email = app.selected_email()?;
    if let Some(id) = email.draft_id.as_deref() {
        return Some(crate::selector::Selector::for_draft(account, id));
    }
    let msg = email.msg?;
    let store = open_store(account)?;
    let row = match crate::store::read::find_by_id(&store, msg.row_id()) {
        Ok(row) => row?,
        Err(e) => {
            log::warn!("[store] reading {msg} for its selector failed: {e:#}");
            return None;
        }
    };
    Some(crate::selector::Selector::for_message(account, &row))
}

/// The backend the server op runs against, resolved before any optimistic
/// write so a missing config leaves the store and the list untouched.
fn backend_for_mutation(app: &mut App) -> Option<Backend> {
    if app.is_graph() {
        match app.graph_config.clone() {
            Some(c) => Some(Backend::Graph(Box::new(c))),
            None => {
                app.set_status_level("Graph not configured".to_string(), StatusLevel::Error);
                None
            }
        }
    } else {
        match app.imap_config.clone() {
            Some(c) => Some(Backend::Imap(Box::new(c))),
            None => {
                app.set_status_level("IMAP not configured".to_string(), StatusLevel::Error);
                None
            }
        }
    }
}

/// Fire the server half of a batch of already-applied mutations.
///
/// One `BgResult` per op, so the counters the UI keeps stay balanced, and one
/// rollback per failure: a move that the server refused is put back where it
/// came from, which is what the pre-store build did by moving the file back.
/// A refused delete has nothing to put back and converges on the next sync
/// (see [`crate::store::write`]); a refused flag is rolled back by the
/// `BgResult::ToggleRead` handler, which owns the in-memory half too.
fn dispatch<F>(
    account: String,
    backend: Backend,
    prepared: Vec<Prepared>,
    tx: mpsc::Sender<BgResult>,
    report: F,
) where
    F: Fn(&Prepared, Result<String, String>) -> BgResult + Send + 'static,
{
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        let ops: Vec<ServerOp> = prepared.iter().map(|p| p.op.clone()).collect();
        let results = rt.block_on(mutations::run_ops(&backend, &ops));
        for (prep, result) in prepared.iter().zip(results) {
            let result = match result {
                Ok(()) => Ok(String::new()),
                Err(e) => {
                    if matches!(prep.op, ServerOp::Move { .. }) {
                        mutations::rollback_move(&account, &prep.previous);
                    }
                    Err(e.to_string())
                }
            };
            let _ = tx.send(report(prep, result));
        }
    });
}

/// What a mutation leaves stale beyond the list it just changed.
///
/// The destination mailbox's cached list no longer matches its rows, every
/// sidebar count is one query away from the truth, and an invite that moved or
/// died changes the agenda the Calendar view is holding (the same refresh
/// `bg.rs` runs after an RSVP). The store write has already happened when this
/// is called, so all three read the new state.
fn refresh_after_mutation(app: &mut App, dest_idx: Option<usize>, touched_invite: bool) {
    if let Some(idx) = dest_idx {
        app.invalidate_cache_idx(idx);
    }
    app.recount_all_mailboxes();
    if touched_invite {
        app.rebuild_calendar_if_loaded();
    }
}

/// True when any of `msgs` is an invite row in the current list, read *before*
/// the mutation removes them.
fn any_invite(app: &App, msgs: &[MessageRef]) -> bool {
    app.emails
        .iter()
        .any(|e| e.is_invite && e.msg.is_some_and(|m| msgs.contains(&m)))
}

pub(super) fn handle_action(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: Action,
    bg_tx: &mpsc::Sender<BgResult>,
) -> Result<()> {
    match action {
        Action::EditCurrent => {
            // `mp edit <selector>` done in-process (#0052 scope item 7): the
            // draft is resolved through the index and handed to `$EDITOR`,
            // with the index refreshed on the way back so the list shows what
            // the user just typed.
            //
            // A received row has no file to open. The pre-nuke build handed
            // `$EDITOR` the message's `.md`; after #0037 there is no such
            // file, and `mp edit` takes draft selectors only, so there is no
            // CLI behaviour to mirror and nothing honest to open. It declines
            // permanently rather than with the #0052 line, because nothing is
            // coming that would make it work.
            let Some((_id, path)) = cursor_draft(
                app,
                "Open in $EDITOR needs a draft; received mail is a store row, not a file",
            ) else {
                return Ok(());
            };
            edit_new_draft(app, terminal, &path, "Returned from editor".to_string())?;
        }
        Action::Reply(reply_all) => {
            let what = if reply_all { "Reply-all" } else { "Reply" };
            let Some(msg) = cursor_message(app, what) else {
                return Ok(());
            };
            let Some(source) = source_for_msg(app, msg, what, false) else {
                return Ok(());
            };
            write_draft_and_edit(
                app,
                terminal,
                &source,
                DraftFromSource::Reply { all: reply_all },
                what,
            )?;
        }
        Action::Forward => {
            // `w` opens the wizard (see `ComposeMode::Forward`); this arm is
            // the direct path, kept for a caller that already has the row.
            let Some(msg) = cursor_message(app, "Forward") else {
                return Ok(());
            };
            let Some(source) = source_for_msg(app, msg, "Forward", true) else {
                return Ok(());
            };
            write_draft_and_edit(app, terminal, &source, DraftFromSource::Forward, "Forward")?;
        }
        Action::Send => {
            // `mp send <selector>` in-process (#0052 scope item 3). The draft
            // is resolved through the index, validated the way the CLI
            // validates it, and submitted through the durable outbox; the
            // approved-status requirement is not checked here because it is
            // not the CLI's either -- `build_draft_message` enforces it, and
            // its refusal is the error the user sees.
            let Some((_id, path)) = cursor_draft(
                app,
                "Send needs a draft; received mail has nothing to send",
            ) else {
                return Ok(());
            };

            let draft = match crate::draft::parse_email_draft(&path) {
                Ok(draft) => draft,
                Err(e) => {
                    app.set_status_level(format!("Send failed: {e:#}"), StatusLevel::Error);
                    return Ok(());
                }
            };
            if let Err(e) = crate::draft::validate_draft(&draft) {
                app.set_status_level(format!("Send failed: {e:#}"), StatusLevel::Error);
                return Ok(());
            }

            // Which account sends it is the draft's own `from:`, not the open
            // mailbox: a draft written for another configured account is sent
            // from that account's SMTP or Graph credentials.
            //
            // The IMAP config that resolver also hands back is not used here:
            // the sent copy is an APPEND the outbox owns and drives (#0037),
            // not something this path does after the fact.
            let (acct_idx, smtp, _imap, graph, account_config, signature) =
                super::helpers::resolve_send_account(app, &path);
            let graph =
                graph.filter(|_| account_config.auth_method == crate::config::AuthMethod::Graph);
            if graph.is_none() && smtp.is_none() {
                app.set_status_level("SMTP not configured".to_string(), StatusLevel::Error);
                return Ok(());
            }
            let ctx = SendCtx {
                graph,
                smtp,
                account: account_config,
                email_settings: app.global_config.email.clone(),
                signature,
            };

            app.bg_count += 1;
            app.set_status_level("Sending...".to_string(), StatusLevel::Progress);
            let tx = bg_tx.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                let result = send_one_draft(&rt, &draft, &ctx).and_then(|r| send_status_line(&r));
                let _ = tx.send(BgResult::Send {
                    account_index: acct_idx,
                    result: result.map_err(|e| format!("{e:#}")),
                });
            });
        }
        Action::Rsvp { msg, choice } => {
            // The invitation's own iMIP payload is the source of truth for the
            // reply, and it lives in the message's blob (#0038 item 6). The
            // account is the active one by construction: the reference names a
            // row in its store.
            let Some(ics) = app.load_message_ics(msg) else {
                app.set_status_level(
                    "That message carries no invitation to reply to".to_string(),
                    StatusLevel::Error,
                );
                return Ok(());
            };
            let acct_idx = app.active_account;
            let smtp_config = app.smtp_config.clone();
            let graph_config = app.graph_config.clone();
            let account_config = app.account_config.clone();

            if graph_config.is_some()
                && account_config.auth_method == crate::config::AuthMethod::Graph
            {
                app.set_status_level(
                    "RSVP is not supported for Graph accounts yet (#0036)".to_string(),
                    StatusLevel::Error,
                );
                return Ok(());
            }
            let smtp_config = match smtp_config {
                Some(c) => c,
                None => {
                    app.set_status_level(
                        "SMTP not configured".to_string(),
                        StatusLevel::Error,
                    );
                    return Ok(());
                }
            };
            let account_address =
                crate::parse::extract_email_address(&account_config.default_from);
            let rsvp = choice.to_rsvp();

            app.bg_count += 1;
            app.set_status_level(
                format!("Sending {} reply...", choice.label().to_lowercase()),
                StatusLevel::Progress,
            );
            let tx = bg_tx.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new()
                    .expect("failed to create tokio runtime");
                let result = (|| -> anyhow::Result<String> {
                    let outcome = rt.block_on(crate::send::send_rsvp(
                        &ics,
                        &account_config,
                        &account_address,
                        rsvp,
                        &smtp_config,
                    ))?;
                    if !outcome.send_result.any_succeeded() {
                        anyhow::bail!("Failed to send RSVP to {}", outcome.organizer);
                    }
                    Ok(format!("{} — replied to {}", outcome.subject, outcome.organizer))
                })();
                let _ = tx.send(BgResult::Rsvp {
                    account_index: acct_idx,
                    result: result.map_err(|e| e.to_string()),
                });
            });
        }

        Action::SendApproved => {
            if let Some(dir) = app.active_dir().cloned() {
                if app.is_graph() {
                    let graph_config = app.graph_config.clone().unwrap();
                    let email_settings = app.global_config.email.clone();
                    let account_config = app.account_config.clone();
                    let signature = app.signature_content.clone();

                    app.bg_count += 1;
                    app.set_status_level(
                        "Sending approved via Graph...".to_string(),
                        StatusLevel::Progress,
                    );
                    let acct_idx = app.active_account;
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = (|| -> anyhow::Result<String> {
                            let drafts = find_drafts(&dir, Some(EmailStatus::Approved))?;
                            if drafts.is_empty() {
                                return Ok("No approved emails found".to_string());
                            }

                            let mut sent = 0usize;
                            let mut failed = 0usize;

                            for draft in &drafts {
                                let send_result = (|| -> anyhow::Result<String> {
                                    let to = parse_graph_recipients(
                                        draft.frontmatter.to.as_deref(),
                                    );
                                    let cc = parse_graph_recipients(
                                        draft.frontmatter.cc.as_deref(),
                                    );
                                    let bcc = parse_graph_recipients(
                                        draft.frontmatter.bcc.as_deref(),
                                    );
                                    let to_refs: Vec<(&str, &str)> = to
                                        .iter()
                                        .map(|(n, a)| (n.as_str(), a.as_str()))
                                        .collect();
                                    let cc_refs: Vec<(&str, &str)> = cc
                                        .iter()
                                        .map(|(n, a)| (n.as_str(), a.as_str()))
                                        .collect();
                                    let bcc_refs: Vec<(&str, &str)> = bcc
                                        .iter()
                                        .map(|(n, a)| (n.as_str(), a.as_str()))
                                        .collect();

                                    let quoted_html = draft.path.with_extension("html");
                                    let quoted = if quoted_html.exists() {
                                        std::fs::read_to_string(&quoted_html).ok()
                                    } else {
                                        None
                                    };
                                    let html_body = crate::send::markdown_to_html(
                                        &draft.body_markdown,
                                        &email_settings,
                                        signature.as_deref(),
                                        quoted.as_deref(),
                                    );

                                    let mut att_data: Vec<(String, Vec<u8>, String)> = Vec::new();
                                    if let Some(ref attachments) = draft.frontmatter.attachments {
                                        for att_path in attachments {
                                            let expanded = shellexpand::tilde(att_path);
                                            let p = std::path::Path::new(expanded.as_ref());
                                            let content = std::fs::read(p)?;
                                            let filename = p
                                                .file_name()
                                                .map(|n| n.to_string_lossy().to_string())
                                                .unwrap_or_else(|| "attachment".to_string());
                                            let content_type = mime_guess::from_path(p)
                                                .first_or_octet_stream()
                                                .to_string();
                                            att_data.push((filename, content, content_type));
                                        }
                                    }

                                    let client = rt.block_on(
                                        crate::graph::GraphClient::new_async(&graph_config),
                                    )?;
                                    let built = crate::send::build_draft_message(
                                        draft,
                                        &account_config.default_from,
                                        &email_settings,
                                        signature.as_deref(),
                                        None,
                                    )?;
                                    let report = rt.block_on(crate::send::send_durably_via(
                                        &built,
                                        &account_config,
                                        client.send_mail(
                                            &to_refs,
                                            &cc_refs,
                                            &bcc_refs,
                                            &draft.frontmatter.subject,
                                            &html_body,
                                            &att_data,
                                        ),
                                    ))?;
                                    if !report.send_result.any_succeeded() {
                                        anyhow::bail!("Graph send failed");
                                    }
                                    Ok(built.message_id)
                                })();

                                match send_result {
                                    Ok(message_id) => {
                                        let _ = mark_draft_sent(draft, Some(&message_id));
                                        crate::contacts::hooks::bump_after_send(
                                            &account_config,
                                            draft,
                                        );
                                        sent += 1;
                                    }
                                    Err(_) => failed += 1,
                                }
                            }

                            Ok(format!("{} sent, {} failed", sent, failed))
                        })();
                        let _ = tx.send(BgResult::SendApproved {
                            account_index: acct_idx,
                            result: result.map_err(|e| e.to_string()),
                        });
                    });
                } else {
                    let smtp_config = match app.smtp_config.clone() {
                        Some(c) => c,
                        None => {
                            app.set_status_level(
                                "SMTP not configured".to_string(),
                                StatusLevel::Error,
                            );
                            return Ok(());
                        }
                    };
                    let email_settings = app.global_config.email.clone();
                    let account_config = app.account_config.clone();
                    let signature = app.signature_content.clone();

                    app.bg_count += 1;
                    app.set_status_level(
                        "Sending approved...".to_string(),
                        StatusLevel::Progress,
                    );
                    let acct_idx = app.active_account;
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = (|| -> anyhow::Result<String> {
                            let drafts = find_drafts(&dir, Some(EmailStatus::Approved))?;
                            if drafts.is_empty() {
                                return Ok("No approved emails found".to_string());
                            }

                            let mut sent = 0usize;
                            let mut failed = 0usize;

                            for draft in &drafts {
                                let built = match crate::send::build_draft_message(
                                    draft,
                                    &smtp_config.default_from,
                                    &email_settings,
                                    signature.as_deref(),
                                    None,
                                ) {
                                    Ok(built) => built,
                                    Err(_) => {
                                        failed += 1;
                                        continue;
                                    }
                                };
                                match rt.block_on(crate::send::send_durably(
                                    &built,
                                    &account_config,
                                    &smtp_config,
                                )) {
                                    Ok(report) => {
                                        if report.send_result.any_succeeded() {
                                            let _ =
                                                mark_draft_sent(draft, Some(&built.message_id));
                                            crate::contacts::hooks::bump_after_send(
                                                &account_config,
                                                draft,
                                            );
                                            sent += 1;
                                        } else {
                                            failed += 1;
                                        }
                                    }
                                    Err(_) => failed += 1,
                                }
                            }

                            Ok(format!("{} sent, {} failed", sent, failed))
                        })();
                        let _ = tx.send(BgResult::SendApproved {
                            account_index: acct_idx,
                            result: result.map_err(|e| e.to_string()),
                        });
                    });
                }
            }
        }

        Action::NewDraft => {
            let name = chrono::Local::now()
                .format("draft-%Y%m%d-%H%M%S")
                .to_string();
            let file_name = format!("{name}.md");
            let dir = app
                .find_mailbox_by_kind(MailboxKind::Drafts)
                .map(|i| app.mailboxes[i].dir.clone())
                .or_else(|| app.drafts_dir.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let path = dir.join(&file_name);

            if path.exists() {
                app.set_status(format!("File already exists: {}", path.display()));
            } else {
                let now = chrono::Utc::now().to_rfc2822();
                let default_from = app
                    .smtp_config
                    .as_ref()
                    .map(|s| s.default_from.clone())
                    .unwrap_or_else(|| app.account_config.default_from.clone());
                let from = default_from.as_str();
                let skeleton = new_draft_skeleton(from, &now);
                match std::fs::write(&path, skeleton) {
                    Ok(()) => {
                        suspend_terminal(terminal)?;
                        let _ = edit_file(&path);
                        resume_terminal(terminal)?;
                        app.set_status(format!("Created: {}", file_name));
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                            app.invalidate_cache_idx(idx);
                        }
                        app.reload_current_mailbox();
                    }
                    Err(e) => {
                        app.set_status_level(format!("New draft failed: {e}"), StatusLevel::Error)
                    }
                }
            }
        }

        Action::Approve => {
            status_flip(app, DraftStatusFlip::Approve);
        }
        Action::BatchApprove(ids) => {
            status_flip_batch(app, &ids, DraftStatusFlip::Approve);
        }
        Action::MarkDraft => {
            status_flip(app, DraftStatusFlip::Demote);
        }
        Action::BatchMarkDraft(ids) => {
            status_flip_batch(app, &ids, DraftStatusFlip::Demote);
        }
        Action::Archive => {
            if let Some(msg) = app.selected_email_ref() {
                archive_msgs(app, terminal, bg_tx, vec![msg], false)?;
            }
        }

        Action::Delete => {
            if let Some(msg) = app.selected_email_ref() {
                delete_msgs(app, terminal, bg_tx, vec![msg], false)?;
            }
        }

        Action::BatchArchive(msgs) => {
            archive_msgs(app, terminal, bg_tx, msgs, true)?;
        }

        Action::BatchDelete(msgs) => {
            delete_msgs(app, terminal, bg_tx, msgs, true)?;
        }

        Action::MoveToMailbox { msgs, dest_idx } => {
            // Quick-move to an arbitrary mailbox (#0018): the generalized
            // archive. The store row moves optimistically, the server op
            // follows, and a refusal puts the row back (#0038 item 7).
            let (dest_mailbox, dest_label) = match app.mailboxes.get(dest_idx) {
                Some(mb) => (mailbox_key(mb), mb.label.clone()),
                None => return Ok(()),
            };
            let dest_server = match app
                .mailboxes
                .get(dest_idx)
                .and_then(|mb| mb.server_name.clone())
            {
                Some(s) => s,
                None => {
                    app.set_status_level(
                        format!("{dest_label} has no server-side folder"),
                        StatusLevel::Error,
                    );
                    return Ok(());
                }
            };
            let source_server = app.active_server_mailbox();

            // Resolve the backend and the store BEFORE any optimistic
            // mutation (same order as Archive) so a missing config leaves the
            // list and the rows untouched.
            let Some(backend) = backend_for_mutation(app) else {
                return Ok(());
            };
            let Some((store, _blobs)) = store_for_mutation(app, "Move") else {
                return Ok(());
            };

            let touched_invite = any_invite(app, &msgs);
            let prepared =
                mutations::prepare_move(&store, &msgs, &dest_mailbox, &source_server, &dest_server);
            drop(store);
            if prepared.is_empty() {
                app.set_status_level(
                    "Move failed: nothing to move".to_string(),
                    StatusLevel::Error,
                );
                return Ok(());
            }

            let moved: HashSet<MessageRef> = prepared.iter().map(|p| p.msg()).collect();
            app.remove_selected_from_list_batch(&moved);
            app.selection.clear();
            refresh_after_mutation(app, Some(dest_idx), touched_invite);

            let count = prepared.len();
            app.bg_count += count;
            app.bg_mutations += count;
            app.set_status_level(
                if count == 1 {
                    format!("Moving to {dest_label}...")
                } else {
                    format!("Moving {count} emails to {dest_label}...")
                },
                StatusLevel::Progress,
            );
            terminal.draw(|frame| ui::view(app, frame))?;

            let acct_idx = app.active_account;
            let source_idx = app.active_mailbox;
            let account = app.account_config.name.clone();
            dispatch(account, backend, prepared, bg_tx.clone(), move |_prep, result| {
                BgResult::Move {
                    account_index: acct_idx,
                    source_mailbox_idx: source_idx,
                    dest_mailbox_idx: dest_idx,
                    dest_label: dest_label.clone(),
                    result,
                }
            });
        }

        Action::ToggleRead => {
            if let Some(email) = app.selected_email() {
                let new_read = !email.read;
                let Some(msg) = email.msg else {
                    return Ok(());
                };
                let label = if new_read {
                    "Marked as read"
                } else {
                    "Marked as unread"
                };
                if set_read_flag(app, bg_tx, vec![msg], new_read) {
                    app.set_status(label.to_string());
                }
            }
        }

        Action::MarkAsRead => {
            // The auto-mark that rides on opening an email: same path, no
            // status line of its own.
            if let Some(email) = app.selected_email() {
                if email.read {
                    return Ok(());
                }
                let Some(msg) = email.msg else {
                    return Ok(());
                };
                set_read_flag(app, bg_tx, vec![msg], true);
            }
        }

        Action::BatchToggleRead(msgs) => {
            let any_unread = msgs
                .iter()
                .any(|m| app.emails.iter().any(|e| e.msg == Some(*m) && !e.read));
            let new_read = any_unread;
            let count = msgs.len();
            if set_read_flag(app, bg_tx, msgs, new_read) {
                app.selection.clear();
                app.set_status(if new_read {
                    format!("Marked {count} as read")
                } else {
                    format!("Marked {count} as unread")
                });
            }
        }

        Action::CopyMessageRef => match selected_selector(app) {
            Some(selector) => {
                let text = selector.to_string();
                match super::helpers::copy_to_clipboard(&text) {
                    Ok(()) => app.set_status(format!("{text} copied to clipboard")),
                    Err(e) => app.set_status_level(
                        format!("Copy failed: {e}"),
                        StatusLevel::Error,
                    ),
                }
            }
            None => app.set_status_level(
                "That message is not in the local store, so it has no selector yet".to_string(),
                StatusLevel::Warning,
            ),
        },
        Action::OpenLogFile => match crate::config::latest_log_file() {
            Some(path) => {
                suspend_terminal(terminal)?;
                let result = edit_file(&path);
                resume_terminal(terminal)?;
                match result {
                    Ok(()) => app.set_status("Returned from log file".to_string()),
                    Err(e) => app.set_status_level(
                        format!("Open log failed: {e}"),
                        StatusLevel::Error,
                    ),
                }
            }
            None => app.set_status_level(
                format!(
                    "No log file found in {}",
                    crate::config::logs_dir().display()
                ),
                StatusLevel::Warning,
            ),
        },

        Action::OpenConfigFile => {
            let path = crate::config::config_path();
            if path.exists() {
                suspend_terminal(terminal)?;
                let result = edit_file(&path);
                resume_terminal(terminal)?;
                match result {
                    // Theme and other settings are read once at startup
                    // (theme is an `OnceLock`), so there is no hot-reload.
                    Ok(()) => app.set_status(
                        "Config saved \u{2014} restart mailypoppins to apply changes".to_string(),
                    ),
                    Err(e) => app.set_status_level(
                        format!("Open config failed: {e}"),
                        StatusLevel::Error,
                    ),
                }
            } else {
                app.set_status_level(
                    format!(
                        "Config file not found at {}. Run `mp config init` to create it.",
                        path.display()
                    ),
                    StatusLevel::Warning,
                );
            }
        }

        Action::OpenAttachment(path) => match crate::parse::open_file_with_system(&path) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                app.set_status(format!("Opened: {name}"));
            }
            Err(e) => {
                app.set_status_level(format!("Open failed: {e}"), StatusLevel::Error);
            }
        },

        Action::SaveAttachments { sources, dest_dir } => {
            let mut saved = 0usize;
            let mut failed = 0usize;
            for source in &sources {
                match crate::parse::save_attachment(source, &dest_dir) {
                    Ok(_) => saved += 1,
                    Err(e) => {
                        log::warn!("Save attachment failed for {}: {e}", source.display());
                        failed += 1;
                    }
                }
            }
            app.last_save_dir = Some(dest_dir.clone());
            if failed == 0 {
                let dir_display = dest_dir.display();
                app.set_status_level(
                    format!("Saved {} file(s) to {dir_display}", saved),
                    StatusLevel::Success,
                );
            } else {
                app.set_status_level(
                    format!("Saved {}/{} file(s) ({} failed)", saved, saved + failed, failed),
                    StatusLevel::Warning,
                );
            }
        }

        Action::OpenHtmlInBrowser(path) => match crate::parse::open_file_with_system(&path) {
            Ok(()) => {
                app.set_status("Opened in browser".to_string());
            }
            Err(e) => {
                app.set_status_level(format!("Open failed: {e}"), StatusLevel::Error);
            }
        },

        Action::Fetch => {
            if app.bg_count > 0 {
                app.queued_action = Some(Action::Fetch);
                app.set_status(format!(
                    "Quick sync queued ({} ops pending...)",
                    app.bg_count
                ));
                return Ok(());
            }
            let account_config = app.account_config.clone();
            let acct_idx = app.active_account;
            let tx = bg_tx.clone();

            if app.is_graph() {
                let graph_config = app.graph_config.clone().unwrap();
                app.bg_count += 1;
                app.set_status_level("Quick sync (Graph)...".to_string(), StatusLevel::Progress);
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let sync_result =
                        rt.block_on(lib_do_sync_graph(&account_config, &graph_config, 100));
                    let (result, new_inbox_mail) = match sync_result {
                        Ok((msg, meta)) => (Ok(msg), meta.new_inbox_mail),
                        Err(e) => (Err(e.to_string()), Vec::new()),
                    };
                    let _ = tx.send(BgResult::Fetch {
                        account_index: acct_idx,
                        result,
                        new_inbox_mail,
                    });
                });
            } else {
                let imap_config = match app.imap_config.clone() {
                    Some(c) => c,
                    None => {
                        app.set_status_level(
                            "IMAP not configured".to_string(),
                            StatusLevel::Error,
                        );
                        return Ok(());
                    }
                };
                app.bg_count += 1;
                app.set_status_level("Quick sync...".to_string(), StatusLevel::Progress);
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let sync_result = rt.block_on(super::helpers::lib_do_sync(
                        &account_config,
                        &imap_config,
                        100,
                    ));
                    let (result, new_inbox_mail) = match sync_result {
                        Ok((msg, meta)) => (Ok(msg), meta.new_inbox_mail),
                        Err(e) => (Err(e.to_string()), Vec::new()),
                    };
                    let _ = tx.send(BgResult::Fetch {
                        account_index: acct_idx,
                        result,
                        new_inbox_mail,
                    });
                });
            }
        }

        Action::LoadMailbox { mailbox_idx, generation } => {
            // Background mailbox walk (P1 step 2). Queued by
            // `App::request_mailbox_load` on cache-miss switches/reloads so
            // `load_emails` (seconds on large mailboxes) never blocks the
            // UI thread. Follows the `BgResult::IndexReady` pattern:
            // bump `bg_count` (spinner), spawn, deliver via `bg_tx`. The
            // handler in `tui/bg.rs` drops the result if the generation
            // or account/mailbox indices went stale meanwhile.
            let mailbox = match app.mailboxes.get(mailbox_idx) {
                Some(mb) => super::app::mailbox_key(mb),
                None => return Ok(()),
            };
            let account = app.account_config.name.clone();
            let account_index = app.active_account;
            app.bg_count += 1;
            let tx = bg_tx.clone();
            std::thread::spawn(move || {
                let entries = super::app::load_emails(&account, &mailbox);
                let _ = tx.send(BgResult::MailboxLoaded {
                    account_index,
                    mailbox_idx,
                    generation,
                    entries,
                });
            });
        }

        Action::FetchAccount(acct_idx) => {
            // Per-account quick sync used by the startup auto-fetch path.
            // Triggered from `BgResult::IndexReady` once that account's
            // `message_id_index` has been populated. Unlike `Action::Fetch`,
            // does *not* gate on `bg_count > 0` -- multiple accounts'
            // fetches must be allowed to run concurrently.
            let acct = match app.accounts.get(acct_idx) {
                Some(a) => a,
                None => return Ok(()),
            };
            let account_config = acct.account_config.clone();
            let account_name = account_config.name.clone();
            let tx = bg_tx.clone();

            if acct.is_graph() {
                let graph_config = match acct.graph_config.clone() {
                    Some(c) => c,
                    None => return Ok(()),
                };
                app.bg_count += 1;
                app.set_status_level(
                    format!("Quick sync ({account_name}, Graph)..."),
                    StatusLevel::Progress,
                );
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create tokio runtime");
                    let sync_result =
                        rt.block_on(lib_do_sync_graph(&account_config, &graph_config, 100));
                    let (result, new_inbox_mail) = match sync_result {
                        Ok((msg, meta)) => (Ok(msg), meta.new_inbox_mail),
                        Err(e) => (Err(e.to_string()), Vec::new()),
                    };
                    let _ = tx.send(BgResult::Fetch {
                        account_index: acct_idx,
                        result,
                        new_inbox_mail,
                    });
                });
            } else {
                let imap_config = match acct.imap_config.clone() {
                    Some(c) => c,
                    None => return Ok(()), // local-only / no IMAP -- no-op
                };
                app.bg_count += 1;
                app.set_status_level(
                    format!("Quick sync ({account_name})..."),
                    StatusLevel::Progress,
                );
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create tokio runtime");
                    let sync_result = rt.block_on(super::helpers::lib_do_sync(
                        &account_config,
                        &imap_config,
                        100,
                    ));
                    let (result, new_inbox_mail) = match sync_result {
                        Ok((msg, meta)) => (Ok(msg), meta.new_inbox_mail),
                        Err(e) => (Err(e.to_string()), Vec::new()),
                    };
                    let _ = tx.send(BgResult::Fetch {
                        account_index: acct_idx,
                        result,
                        new_inbox_mail,
                    });
                });
            }
        }

        Action::ServerSearch { query, targets } => {
            // The account name travels with the search so each hit can be
            // resolved against that account's store (#0038).
            let account = app.account_config.name.clone();
            app.server_search_loading = true;
            app.server_search_status = Some("Searching...".to_string());
            app.bg_count += 1;
            let tx = bg_tx.clone();

            if app.is_graph() {
                let graph_config = app.graph_config.clone().unwrap();
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let result = rt.block_on(lib_do_multi_search_graph(
                        &account,
                        &graph_config,
                        &query,
                        &targets,
                    ));
                    let _ = tx.send(BgResult::ServerSearch {
                        result: result.map_err(|e| e.to_string()),
                    });
                });
            } else {
                let imap_config = match app.imap_config.clone() {
                    Some(c) => c,
                    None => {
                        app.set_status_level(
                            "IMAP not configured".to_string(),
                            StatusLevel::Error,
                        );
                        app.server_search_loading = false;
                        app.server_search_status = None;
                        app.bg_count -= 1;
                        return Ok(());
                    }
                };
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let result = rt.block_on(super::helpers::lib_do_multi_search(
                        &account,
                        &imap_config,
                        &query,
                        &targets,
                    ));
                    let _ = tx.send(BgResult::ServerSearch {
                        result: result.map_err(|e| e.to_string()),
                    });
                });
            }
        }

        Action::SearchResultOpen
        | Action::SearchResultReply(_)
        | Action::SearchResultForward
        | Action::SearchResultArchive
        | Action::SearchResultOpenInBrowser => {
            handle_search_result_action(app, terminal, action, bg_tx)?;
        }

        Action::Sync => {
            if app.bg_count > 0 {
                app.queued_action = Some(Action::Sync);
                app.set_status(format!(
                    "Full sync queued ({} ops pending...)",
                    app.bg_count
                ));
                return Ok(());
            }
            let account_config = app.account_config.clone();
            let acct_idx = app.active_account;
            let tx = bg_tx.clone();

            if app.is_graph() {
                let graph_config = app.graph_config.clone().unwrap();
                app.bg_count += 1;
                app.set_status_level(
                    "Full sync (Graph)...".to_string(),
                    StatusLevel::Progress,
                );
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let result = rt
                        .block_on(lib_do_sync_graph(&account_config, &graph_config, usize::MAX))
                        .map(|(msg, _meta)| msg)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(BgResult::Sync {
                        account_index: acct_idx,
                        result,
                    });
                });
            } else {
                let imap_config = match app.imap_config.clone() {
                    Some(c) => c,
                    None => {
                        app.set_status_level(
                            "IMAP not configured".to_string(),
                            StatusLevel::Error,
                        );
                        return Ok(());
                    }
                };
                app.bg_count += 1;
                app.set_status_level("Full sync...".to_string(), StatusLevel::Progress);
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let result = rt
                        .block_on(super::helpers::lib_do_sync(
                            &account_config,
                            &imap_config,
                            usize::MAX,
                        ))
                        .map(|(msg, _meta)| msg)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(BgResult::Sync {
                        account_index: acct_idx,
                        result,
                    });
                });
            }
        }

        Action::OpenComposeWizard(mode) => {
            open_compose_wizard(app, mode);
        }

        Action::ComposeToContact { to } => {
            open_compose_wizard_seeded(app, to);
        }

        Action::SendContactVcard { contact } => {
            send_contact_as_vcard(app, terminal, &contact)?;
        }

        Action::CopyContactEmail { address } => {
            match super::helpers::copy_to_clipboard(&address) {
                Ok(()) => app.set_status(format!("{address} copied to clipboard")),
                Err(e) => app.set_status_level(format!("Copy failed: {e}"), StatusLevel::Error),
            }
        }

        Action::OpenEventSource { msg } => {
            // The agenda row carries its own [`MessageRef`] (the invite may
            // live in any mailbox of the account), so this does not go through
            // the mail cursor like `Action::EditCurrent`.
            //
            // What `$EDITOR` gets is the invite's `.ics` blob written to a temp
            // file (#0052 scope item 10), where the file build handed it the
            // message's `.md`. Edits to that copy reach nothing, which is why
            // `Action::EditCurrent` declines on a received row rather than
            // doing the same thing: this flow is inspecting an artifact the
            // message carries, not composing, and the `.ics` is worth reading.
            let row_id = msg.row_id();
            // The store connection is scoped to the read: `$EDITOR` owns the
            // terminal for as long as the user wants it, and holding SQLite
            // open across that is pointless.
            let ics = {
                let Some((store, blobs)) = store_for_mutation(app, "Open event source") else {
                    return Ok(());
                };
                crate::store::read::load_invite_ics(&store, &blobs, row_id)
            };
            let Some(ics) = ics else {
                app.set_status_level(
                    "That event has no ics source in the store".to_string(),
                    StatusLevel::Warning,
                );
                return Ok(());
            };
            let path = std::env::temp_dir().join(format!("mailypoppins-{row_id}.ics"));
            if let Err(e) = std::fs::write(&path, &ics) {
                app.set_status_level(format!("Open failed: {e}"), StatusLevel::Error);
                return Ok(());
            }
            suspend_terminal(terminal)?;
            let result = edit_file(&path);
            resume_terminal(terminal)?;
            match result {
                Ok(()) => app.set_status(
                    "Returned from the event source (a copy of the ics; edits do not reach the message)"
                        .to_string(),
                ),
                Err(e) => {
                    app.set_status_level(format!("Open failed: {e}"), StatusLevel::Error)
                }
            }
        }
        Action::ComposeWizardCancel => {
            app.close_overlay();
            app.focus = Focus::List;
            app.set_status("Compose cancelled".to_string());
        }

        Action::ComposeWizardSubmit => {
            submit_compose_wizard(app, terminal)?;
            // Consume-and-close: `submit_compose_wizard` takes the wizard via
            // `mem::replace` and (unless validation re-opens it) leaves the
            // overlay at `None`. Promote any error queued behind it. Guarded
            // on `Overlay::None`, so the validation re-open path is a no-op.
            app.promote_pending_error();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Compose wizard handlers
// ---------------------------------------------------------------------------

fn open_compose_wizard(app: &mut App, mode: ComposeMode) {
    // Load contact cache (if any) for the active account.
    let contacts = {
        let root = crate::config::account_dir(&app.account_config.name);
        crate::contacts::load_cache(&root).ok().flatten()
    };

    let (to, cc, bcc, subject) = match &mode {
        ComposeMode::New => (String::new(), String::new(), String::new(), String::new()),
        // The forward's subject is shown before its draft exists, so it is
        // built by the same rule the draft will use.
        ComposeMode::Forward { msg } => {
            let subject = forward_subject(app, *msg);
            (String::new(), String::new(), String::new(), subject)
        }
        ComposeMode::EditDraft { id } => {
            let Some(path) = indexed_draft_path(app, id) else {
                app.focus = Focus::List;
                return;
            };
            match crate::draft::parse_email_draft(&path) {
                Ok(draft) => (
                    draft.frontmatter.to.unwrap_or_default(),
                    draft.frontmatter.cc.unwrap_or_default(),
                    draft.frontmatter.bcc.unwrap_or_default(),
                    draft.frontmatter.subject,
                ),
                Err(e) => {
                    app.set_status_level(format!("Cannot edit draft: {e}"), StatusLevel::Error);
                    app.focus = Focus::List;
                    return;
                }
            }
        }
    };

    app.overlay = Overlay::Compose(ComposeWizard {
        mode,
        to,
        cc,
        bcc,
        subject,
        focus: ComposeField::To,
        suggestions: Vec::new(),
        suggestion_idx: 0,
        contacts,
    });
    app.focus = Focus::ComposeWizard;
    // No suggestions shown until the user types (see recompute_compose_suggestions).
}

/// Open the compose wizard as a new draft pre-seeded with a recipient (#0033,
/// compose-to-contact). Reuses the same overlay + submit path as `n` in the
/// Mail list; the wizard floats above the Contacts view, so on submit/cancel
/// the user returns to Contacts (the overlay is view-agnostic). Starts focus on
/// the Subject field since the recipient is already filled.
fn open_compose_wizard_seeded(app: &mut App, to: String) {
    let contacts = {
        let root = crate::config::account_dir(&app.account_config.name);
        crate::contacts::load_cache(&root).ok().flatten()
    };
    app.overlay = Overlay::Compose(ComposeWizard {
        mode: ComposeMode::New,
        to,
        cc: String::new(),
        bcc: String::new(),
        subject: String::new(),
        focus: ComposeField::Subject,
        suggestions: Vec::new(),
        suggestion_idx: 0,
        contacts,
    });
    app.focus = Focus::ComposeWizard;
}

/// Export a contact to a `.vcf` and attach it to a brand-new draft (#0033,
/// send-contact-as-vCard). The vCard is written into the drafts mailbox's
/// `_vcards/` sidecar dir and referenced by absolute path in the draft's
/// `attachments:` frontmatter (the same plumbing `src/send.rs` already reads).
/// Then hands off to `$EDITOR` exactly like the new-draft flow.
///
/// v1 intentionally targets a *new* draft only: attaching to an existing draft
/// would need a draft picker + an in-place `attachments:` frontmatter rewrite
/// (new machinery beyond the reused new-draft path); deferred per the ticket.
fn send_contact_as_vcard(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    contact: &crate::contacts::Contact,
) -> Result<()> {
    let dir = app
        .find_mailbox_by_kind(MailboxKind::Drafts)
        .map(|i| app.mailboxes[i].dir.clone())
        .or_else(|| app.drafts_dir.clone())
        .unwrap_or_else(|| PathBuf::from("."));

    // Write the .vcf into a sidecar dir beside the drafts so it is stable and
    // out of the mailbox listing (which only reads `*.md`).
    let vcard_dir = dir.join("_vcards");
    if let Err(e) = std::fs::create_dir_all(&vcard_dir) {
        app.set_status_level(format!("vCard dir failed: {e}"), StatusLevel::Error);
        return Ok(());
    }
    let stem = crate::contacts::vcard_file_stem(contact);
    let mut vcf_path = vcard_dir.join(format!("{stem}.vcf"));
    let mut counter = 1usize;
    while vcf_path.exists() {
        vcf_path = vcard_dir.join(format!("{stem}-{counter}.vcf"));
        counter += 1;
    }
    let vcard = crate::contacts::contact_to_vcard(contact);
    if let Err(e) = std::fs::write(&vcf_path, vcard) {
        app.set_status_level(format!("vCard write failed: {e}"), StatusLevel::Error);
        return Ok(());
    }

    // Create a new draft addressed to the contact with the .vcf attached.
    let recipient = crate::send::format_recipient(&contact.display_name, &contact.address);
    let subject = format!("Contact: {}", vcard_display_name(contact));
    let path = match write_vcard_draft(app, &dir, &recipient, &subject, &vcf_path) {
        Ok(p) => p,
        Err(e) => {
            app.set_status_level(format!("Draft creation failed: {e}"), StatusLevel::Error);
            return Ok(());
        }
    };

    if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
        app.invalidate_cache_idx(idx);
    }

    // Hand off to $EDITOR (matches the new-draft flow).
    suspend_terminal(terminal)?;
    let edit_result = edit_file(&path);
    resume_terminal(terminal)?;
    match edit_result {
        Ok(()) => {
            app.set_status(format!("vCard draft: {}", recipient));
        }
        Err(e) => app.set_status_level(format!("Edit failed: {e}"), StatusLevel::Error),
    }
    app.reload_current_mailbox();
    Ok(())
}

/// The name shown in the vCard-draft subject: display name if any, else the
/// address local-part.
fn vcard_display_name(contact: &crate::contacts::Contact) -> String {
    if contact.display_name.trim().is_empty() {
        contact
            .address
            .split('@')
            .next()
            .unwrap_or(&contact.address)
            .to_string()
    } else {
        contact.display_name.trim().to_string()
    }
}

/// Write a new draft addressed to `recipient` with `vcf_path` in the
/// `attachments:` frontmatter list. Mirrors `write_new_draft_from_wizard`'s
/// frontmatter shape plus a single attachment entry.
fn write_vcard_draft(
    app: &App,
    dir: &std::path::Path,
    recipient: &str,
    subject: &str,
    vcf_path: &std::path::Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let default_from = app
        .smtp_config
        .as_ref()
        .map(|s| s.default_from.clone())
        .unwrap_or_else(|| app.account_config.default_from.clone());
    let from = default_from.as_str();
    let now = chrono::Utc::now().to_rfc2822();

    let slug = slugify_subject_for_filename(subject);
    let timestamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S");
    let base_name = if slug.is_empty() {
        format!("draft-{timestamp}.md")
    } else {
        format!("draft-{timestamp}-{slug}.md")
    };
    let mut path = dir.join(&base_name);
    let mut counter = 1usize;
    while path.exists() {
        path = dir.join(format!("draft-{timestamp}-{slug}-{counter}.md"));
        counter += 1;
    }

    let mut fm = String::from("---\n");
    fm.push_str(&format!("to: {}\n", yaml_escape(recipient)));
    fm.push_str("cc:\n");
    fm.push_str("bcc:\n");
    fm.push_str(&format!("subject: {}\n", yaml_escape(subject)));
    fm.push_str("status: draft\n");
    fm.push_str(&format!("from: \"{from}\"\n"));
    fm.push_str(&format!("date: {now}\n"));
    fm.push_str("reply_to:\n");
    fm.push_str("attachments:\n");
    fm.push_str(&format!(
        "  - \"{}\"\n",
        vcf_path.display().to_string().replace('"', "\\\"")
    ));
    fm.push_str("---\n\n");

    std::fs::write(&path, fm)?;
    Ok(path)
}

fn submit_compose_wizard(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let Overlay::Compose(mut wizard) =
        std::mem::replace(&mut app.overlay, Overlay::None)
    else {
        return Ok(());
    };
    app.focus = Focus::List;

    // Normalize the address fields: strip the trailing `, ` that
    // `accept_suggestion` leaves behind for further typing, plus any other
    // trailing separators. Otherwise mailparse::addrparse sees an empty
    // entry at the end and the send fails.
    wizard.to = normalize_recipient_field(&wizard.to);
    wizard.cc = normalize_recipient_field(&wizard.cc);
    wizard.bcc = normalize_recipient_field(&wizard.bcc);
    wizard.subject = wizard.subject.trim().to_string();

    // Basic validation: must have at least one recipient across to/cc/bcc.
    if wizard.to.is_empty() && wizard.cc.is_empty() && wizard.bcc.is_empty() {
        app.set_status_level(
            "Cannot submit: no recipients (to/cc/bcc all empty)".to_string(),
            StatusLevel::Error,
        );
        // Re-open the wizard so the user can fix the field.
        app.overlay = Overlay::Compose(wizard);
        app.focus = Focus::ComposeWizard;
        return Ok(());
    }

    let edit = DraftRecipientEdit {
        to: wizard.to.clone(),
        cc: wizard.cc.clone(),
        bcc: wizard.bcc.clone(),
        subject: wizard.subject.clone(),
    };

    // Editing an existing draft's recipients/subject rewrites the file in
    // place and does NOT open $EDITOR -- the whole point is a quick,
    // fuzzy-finder edit of the header fields.
    if let ComposeMode::EditDraft { id } = &wizard.mode {
        let id = id.clone();
        let Some(path) = indexed_draft_path(app, &id) else {
            return Ok(());
        };
        match crate::draft::rewrite_draft_recipients(&path, &edit) {
            Ok(()) => {
                let account = app.account_config.name.clone();
                // The file changed, so the row the index holds for it is
                // stale; the selector is the draft's own id either way.
                if let Err(e) = crate::store::drafts::refresh_account(&account) {
                    log::warn!("[drafts] refreshing after a recipient edit failed: {e:#}");
                }
                if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                    app.invalidate_cache_idx(idx);
                }
                app.reload_current_mailbox();
                app.set_status(format!(
                    "Recipients updated: {}",
                    Selector::for_draft(&account, &id)
                ));
            }
            Err(e) => {
                app.set_status_level(format!("Recipient update failed: {e}"), StatusLevel::Error);
            }
        }
        return Ok(());
    }

    // A forward keeps the wizard's recipients and subject over the ones the
    // builder derived, which is the whole reason it asks for them first.
    if let ComposeMode::Forward { msg } = wizard.mode {
        let Some(source) = source_for_msg(app, msg, "Forward", true) else {
            return Ok(());
        };
        let account = app.account_config.name.clone();
        let from = default_from(app);
        let (path, selector) = match draft_from_source(
            &account,
            &from,
            &source,
            DraftFromSource::Forward,
            Some(&edit),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                app.set_status_level(format!("Forward failed: {e:#}"), StatusLevel::Error);
                return Ok(());
            }
        };
        return edit_new_draft(app, terminal, &path, format!("Forward draft ready: {selector}"));
    }

    let draft_result = match &wizard.mode {
        ComposeMode::New => write_new_draft_from_wizard(app, &wizard),
        ComposeMode::Forward { .. } | ComposeMode::EditDraft { .. } => {
            unreachable!("handled above")
        }
    };

    let path = match draft_result {
        Ok(p) => p,
        Err(e) => {
            app.set_status_level(format!("Draft creation failed: {e}"), StatusLevel::Error);
            return Ok(());
        }
    };

    // Invalidate drafts-mailbox cache so the list reloads on next render.
    if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
        app.invalidate_cache_idx(idx);
    }

    // Hand off to $EDITOR.
    suspend_terminal(terminal)?;
    let edit_result = edit_file(&path);
    resume_terminal(terminal)?;
    match edit_result {
        Ok(()) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            app.set_status(format!("Created: {}", name));
        }
        Err(e) => app.set_status_level(format!("Edit failed: {e}"), StatusLevel::Error),
    }
    app.reload_current_mailbox();
    Ok(())
}

fn write_new_draft_from_wizard(app: &App, wizard: &ComposeWizard) -> Result<PathBuf> {
    let dir = app
        .find_mailbox_by_kind(MailboxKind::Drafts)
        .map(|i| app.mailboxes[i].dir.clone())
        .or_else(|| app.drafts_dir.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;

    let default_from = app
        .smtp_config
        .as_ref()
        .map(|s| s.default_from.clone())
        .unwrap_or_else(|| app.account_config.default_from.clone());
    let from = default_from.as_str();
    let now = chrono::Utc::now().to_rfc2822();

    // Build a unique filename from the subject slug (fall back to timestamp).
    let slug = slugify_subject_for_filename(&wizard.subject);
    let timestamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S");
    let base_name = if slug.is_empty() {
        format!("draft-{timestamp}.md")
    } else {
        format!("draft-{timestamp}-{slug}.md")
    };
    let mut path = dir.join(&base_name);
    let mut counter = 1usize;
    while path.exists() {
        let name = if slug.is_empty() {
            format!("draft-{timestamp}-{counter}.md")
        } else {
            format!("draft-{timestamp}-{slug}-{counter}.md")
        };
        path = dir.join(name);
        counter += 1;
    }

    let mut fm = String::from("---\n");
    fm.push_str(&format!("to: {}\n", yaml_escape(&wizard.to)));
    if !wizard.cc.trim().is_empty() {
        fm.push_str(&format!("cc: {}\n", yaml_escape(&wizard.cc)));
    } else {
        fm.push_str("cc:\n");
    }
    if !wizard.bcc.trim().is_empty() {
        fm.push_str(&format!("bcc: {}\n", yaml_escape(&wizard.bcc)));
    } else {
        fm.push_str("bcc:\n");
    }
    fm.push_str(&format!("subject: {}\n", yaml_escape(&wizard.subject)));
    fm.push_str("status: draft\n");
    fm.push_str(&format!("from: \"{from}\"\n"));
    fm.push_str(&format!("date: {now}\n"));
    fm.push_str("reply_to:\n");
    fm.push_str("attachments:\n");
    fm.push_str("---\n\n");

    std::fs::write(&path, fm)?;
    Ok(path)
}

/// Normalize a recipient-list field on wizard submit:
/// - trim leading/trailing whitespace,
/// - strip trailing commas and whitespace left behind by the
///   "accept suggestion + continue typing" flow,
/// - collapse whitespace inside the separators between recipients.
///
/// Leaves the interior of each recipient alone (including display-name
/// quoting), so `"Doe, Jane" <addr>, bob@x.com, ` becomes
/// `"Doe, Jane" <addr>, bob@x.com`.
fn normalize_recipient_field(s: &str) -> String {
    let trimmed = s.trim();
    // Repeatedly strip any trailing `,` or whitespace chars.
    let cleaned = trimmed.trim_end_matches(|c: char| c == ',' || c.is_whitespace());
    cleaned.to_string()
}

/// YAML double-quote escape for a scalar string value.
fn yaml_escape(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn slugify_subject_for_filename(subject: &str) -> String {
    let slug: String = subject
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = crate::types::collapse_hyphens(&slug);
    // Trim to a reasonable length so filenames don't blow up.
    slug.chars().take(40).collect()
}

/// The overlay's own action set: a server-search hit is not a list row, so it
/// has its own Open / Save / Reply / Forward / Archive.
///
/// All of them run off the store for a hit that resolved to a row, and off the
/// fetch the overlay is already rendering for one that did not, with two
/// exceptions. Archive needs a local row to move and says so when there is
/// none. Open needs a file that no longer exists at all, and declines the way
/// `Action::EditCurrent` declines on a received row.
fn handle_search_result_action(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: Action,
    bg_tx: &mpsc::Sender<BgResult>,
) -> Result<()> {
    match action {
        Action::SearchResultOpen => {
            // The file build saved the hit as a `.md` and opened that in
            // `$EDITOR`; after #0037 nothing saves a hit to a file, and
            // `mp edit` takes draft selectors only, so there is no CLI
            // behaviour to port and nothing honest to open. It declines
            // permanently, for the same reason `Action::EditCurrent` declines
            // on a received row, and the overlay is already showing the
            // headers and the body that editor window used to hold.
            app.set_status_level(
                "Open in $EDITOR needs a draft; a search hit is a message on the server, not a file"
                    .to_string(),
                StatusLevel::Warning,
            );
        }

        Action::SearchResultOpenInBrowser => {
            let Some(hit) = app.server_search_results.get(app.server_search_index) else {
                return Ok(());
            };
            // A hit that resolved reads its markup out of `message_blobs`, the
            // same as the list flow; one that did not has the html part of the
            // fetch the overlay is rendering, and that is what the browser
            // gets rather than a decline.
            let index = app.server_search_index;
            let path = match hit.entry.msg {
                Some(msg) => html_rendition_for_row(app, msg.row_id()),
                None => match hit.fetched.html_body.clone() {
                    Some(html) => html_rendition(app, &html, &format!("search-{index}")),
                    None => {
                        app.set_status("No HTML version available".to_string());
                        None
                    }
                },
            };
            let Some(path) = path else {
                return Ok(());
            };
            match crate::parse::open_file_with_system(&path) {
                Ok(()) => app.set_status("Opened in browser".to_string()),
                Err(e) => app.set_status_level(format!("Open failed: {e}"), StatusLevel::Error),
            }
        }

        Action::SearchResultReply(all) => {
            let what = if all { "Reply-all" } else { "Reply" };
            search_result_draft(app, terminal, DraftFromSource::Reply { all }, what)?;
        }

        Action::SearchResultForward => {
            search_result_draft(app, terminal, DraftFromSource::Forward, "Forward")?;
        }

        Action::SearchResultArchive => {
            let hit = app
                .server_search_results
                .get(app.server_search_index)
                .and_then(|r| r.entry.msg);
            let Some(msg) = hit else {
                app.set_status_level(
                    "Not in the local store yet: sync (F) before archiving this hit".to_string(),
                    StatusLevel::Error,
                );
                return Ok(());
            };

            let index = app.server_search_index;
            app.server_search_results.remove(index);
            if app.server_search_index >= app.server_search_results.len()
                && !app.server_search_results.is_empty()
            {
                app.server_search_index = app.server_search_results.len() - 1;
            }

            archive_msgs(app, terminal, bg_tx, vec![msg], false)?;
        }

        _ => {}
    }
    Ok(())
}


/// Reply to or forward the selected server-search hit.
///
/// A hit that resolved to a row is the list flow exactly: same store read,
/// same builder, same draft. A hit that did not resolve has no row, and its
/// content is the fetch the overlay is already rendering, so the draft is
/// built from that rather than declined: the message is in front of the user,
/// and refusing to quote it would be a limitation of the plumbing, not of what
/// is known.
fn search_result_draft(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    kind: DraftFromSource,
    what: &str,
) -> Result<()> {
    let with_attachments = matches!(kind, DraftFromSource::Forward);
    let Some(hit) = app.server_search_results.get(app.server_search_index) else {
        return Ok(());
    };
    let msg = hit.entry.msg;
    // Cloned only for the unresolved hit, which is the one case that needs the
    // fetched payload while `app` is borrowed mutably for the status line.
    let fetched = msg.is_none().then(|| hit.fetched.clone());

    let source = match (msg, fetched) {
        (Some(msg), _) => source_for_msg(app, msg, what, with_attachments),
        (None, Some(fetched)) => {
            let account_dir = crate::config::account_dir(&app.account_config.name);
            match crate::draft::source_from_fetched(&account_dir, &fetched, with_attachments) {
                Ok(source) => Some(source),
                Err(e) => {
                    app.set_status_level(format!("{what} failed: {e:#}"), StatusLevel::Error);
                    None
                }
            }
        }
        (None, None) => None,
    };
    let Some(source) = source else {
        return Ok(());
    };
    write_draft_and_edit(app, terminal, &source, kind, what)
}

/// Archive one or many messages: the store rows move into the archive mailbox,
/// then the server op follows (#0038 scope item 7).
///
/// `batch` says whether the selection should be cleared afterwards, which is
/// the only difference between the single and the batch arm.
fn archive_msgs(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    bg_tx: &mpsc::Sender<BgResult>,
    msgs: Vec<MessageRef>,
    batch: bool,
) -> Result<()> {
    let Some(dest_idx) = app.find_mailbox_by_kind(MailboxKind::Archive) else {
        app.set_status_level(
            "Archive mailbox not configured".to_string(),
            StatusLevel::Error,
        );
        return Ok(());
    };
    let dest_mailbox = match app.mailboxes.get(dest_idx) {
        Some(mb) => mailbox_key(mb),
        None => return Ok(()),
    };
    let dest_server = app.archive_server_name.clone();
    let source_server = app.active_server_mailbox();

    let Some(backend) = backend_for_mutation(app) else {
        return Ok(());
    };
    let Some((store, _blobs)) = store_for_mutation(app, "Archive") else {
        return Ok(());
    };

    let touched_invite = any_invite(app, &msgs);
    let prepared =
        mutations::prepare_move(&store, &msgs, &dest_mailbox, &source_server, &dest_server);
    drop(store);
    if prepared.is_empty() {
        app.set_status_level(
            "Archive failed: nothing to archive".to_string(),
            StatusLevel::Error,
        );
        return Ok(());
    }

    let archived: HashSet<MessageRef> = prepared.iter().map(|p| p.msg()).collect();
    app.remove_selected_from_list_batch(&archived);
    if batch {
        app.selection.clear();
    }
    refresh_after_mutation(app, Some(dest_idx), touched_invite);

    let count = prepared.len();
    app.bg_count += count;
    app.bg_mutations += count;
    app.set_status_level(
        if count == 1 {
            "Archiving...".to_string()
        } else {
            format!("Archiving {count} emails...")
        },
        StatusLevel::Progress,
    );
    terminal.draw(|frame| ui::view(app, frame))?;

    let acct_idx = app.active_account;
    let account = app.account_config.name.clone();
    dispatch(account, backend, prepared, bg_tx.clone(), move |_prep, result| {
        BgResult::Archive {
            account_index: acct_idx,
            result,
        }
    });
    Ok(())
}

/// Delete one or many messages: the store rows go, then the server op follows.
///
/// The rows are removed rather than tombstoned, so a refused server delete is
/// answered by the next sync refetching the message; see
/// [`crate::store::write`] for why that is the right shape here.
fn delete_msgs(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    bg_tx: &mpsc::Sender<BgResult>,
    msgs: Vec<MessageRef>,
    batch: bool,
) -> Result<()> {
    let source_server = app.active_server_mailbox();
    let Some(backend) = backend_for_mutation(app) else {
        return Ok(());
    };
    let Some((store, blobs)) = store_for_mutation(app, "Delete") else {
        return Ok(());
    };

    let touched_invite = any_invite(app, &msgs);
    let prepared = mutations::prepare_delete(&store, &blobs, &msgs, &source_server);
    drop(store);
    if prepared.is_empty() {
        app.set_status_level(
            "Delete failed: nothing to delete".to_string(),
            StatusLevel::Error,
        );
        return Ok(());
    }

    // Every deleted row's id is dead the moment the row is: the list, the
    // selection set and the cursor anchor must not carry one across this
    // boundary, because a re-ingest of the same message mints a new id.
    let deleted: HashSet<MessageRef> = prepared.iter().map(|p| p.msg()).collect();
    app.remove_selected_from_list_batch(&deleted);
    if batch {
        app.selection.clear();
    }
    refresh_after_mutation(app, None, touched_invite);

    let count = prepared.len();
    app.bg_count += count;
    app.bg_mutations += count;
    app.set_status_level(
        if count == 1 {
            "Deleting...".to_string()
        } else {
            format!("Deleting {count} emails...")
        },
        StatusLevel::Progress,
    );
    terminal.draw(|frame| ui::view(app, frame))?;

    let acct_idx = app.active_account;
    let account = app.account_config.name.clone();
    dispatch(account, backend, prepared, bg_tx.clone(), move |_prep, result| {
        BgResult::Delete {
            account_index: acct_idx,
            result,
        }
    });
    Ok(())
}

/// Set the read flag on one or many messages: store row first, then the server.
///
/// Returns false when nothing was applied, so the caller can skip its status
/// line. The in-memory list is updated beside the row because the list is what
/// the user is looking at; both halves are rolled back together by the
/// `BgResult::ToggleRead` handler when the server refuses.
fn set_read_flag(
    app: &mut App,
    bg_tx: &mpsc::Sender<BgResult>,
    msgs: Vec<MessageRef>,
    read: bool,
) -> bool {
    let server_mailbox = app.active_server_mailbox();
    let Some(backend) = backend_for_mutation(app) else {
        return false;
    };
    let Some((store, _blobs)) = store_for_mutation(app, "Read flag") else {
        return false;
    };
    let prepared = mutations::prepare_read_flag(&store, &msgs, read, &server_mailbox);
    drop(store);
    if prepared.is_empty() {
        return false;
    }

    for prep in &prepared {
        app.set_email_read(prep.msg(), read);
    }

    let acct_idx = app.active_account;
    let account = app.account_config.name.clone();
    // ToggleRead deliberately does not touch `bg_mutations`: a flag does not
    // block a fetch the way a move does.
    app.bg_count += prepared.len();
    dispatch(account, backend, prepared, bg_tx.clone(), move |prep, result| {
        BgResult::ToggleRead {
            account_index: acct_idx,
            msg: prep.msg(),
            new_read_state: read,
            result,
        }
    });
    true
}

fn parse_name_address(s: &str) -> (String, String) {
    let s = s.trim();
    if let Some(lt) = s.find('<') {
        if let Some(gt) = s.find('>') {
            let name = s[..lt].trim().trim_matches('"').trim().to_string();
            let addr = s[lt + 1..gt].trim().to_string();
            return (name, addr);
        }
    }
    (String::new(), s.to_string())
}

fn parse_graph_recipients(field: Option<&str>) -> Vec<(String, String)> {
    match field {
        Some(s) if !s.trim().is_empty() => crate::send::split_addresses(s)
            .into_iter()
            .map(|a| parse_name_address(&a))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::EmailEntry;

    fn entry(subject: &str, id: i64, is_invite: bool) -> EmailEntry {
        EmailEntry {
            msg: Some(MessageRef::new(id)),
            draft_id: None,
            from: "Sender <s@example.com>".to_string(),
            to: "me@example.com".to_string(),
            cc: None,
            subject: subject.to_string(),
            status: "inbox".to_string(),
            date_display: "2026-07-01".to_string(),
            date_sort: "2026-07-01T00:00:00".to_string(),
            has_attachments: false,
            read: false,
            is_invite,
        }
    }

    /// The agenda is only rebuilt when a mutation actually touched an invite,
    /// which is read off the list rows *before* they are removed.
    #[test]
    fn only_a_mutation_that_touches_an_invite_asks_for_an_agenda_rebuild() {
        let mut app = App::default_for_tests();
        app.emails = std::sync::Arc::new(vec![
            entry("Standup", 1, true),
            entry("Receipt", 2, false),
        ]);

        assert!(any_invite(&app, &[MessageRef::new(1)]));
        assert!(any_invite(&app, &[MessageRef::new(2), MessageRef::new(1)]));
        assert!(!any_invite(&app, &[MessageRef::new(2)]));
        assert!(!any_invite(&app, &[MessageRef::new(404)]));
    }

    #[test]
    fn normalize_strips_trailing_comma_and_space() {
        // The exact case the user reported: wizard leaves `, ` dangling after
        // an accepted suggestion.
        let input = "\"Doe, Jane\" <jane@example.com>, ";
        let out = normalize_recipient_field(input);
        assert_eq!(out, "\"Doe, Jane\" <jane@example.com>");
    }

    #[test]
    fn normalize_strips_multiple_trailing_separators() {
        assert_eq!(normalize_recipient_field("a@x.com,,  "), "a@x.com");
        assert_eq!(normalize_recipient_field("a@x.com  , "), "a@x.com");
        assert_eq!(normalize_recipient_field("a@x.com"), "a@x.com");
    }

    #[test]
    fn normalize_preserves_multi_recipient_list() {
        assert_eq!(
            normalize_recipient_field("alice@x.com, bob@x.com, "),
            "alice@x.com, bob@x.com"
        );
    }

    #[test]
    fn normalize_preserves_interior_commas_in_quoted_names() {
        let input = "\"Doe, Jane\" <jane@x.com>, \"Roe, John\" <john@x.com>, ";
        let out = normalize_recipient_field(input);
        assert_eq!(
            out,
            "\"Doe, Jane\" <jane@x.com>, \"Roe, John\" <john@x.com>"
        );
    }

    #[test]
    fn normalize_empty_and_whitespace_only() {
        assert_eq!(normalize_recipient_field(""), "");
        assert_eq!(normalize_recipient_field("   "), "");
        assert_eq!(normalize_recipient_field(", ,  "), "");
    }

    /// End-to-end guarantee: the normalized output must round-trip
    /// through `mailparse::addrparse` without leaving an empty entry,
    /// since that's what actually triggered the send failure in the
    /// bug report.
    #[test]
    fn normalized_field_parses_cleanly_with_mailparse() {
        let cases = [
            "\"Doe, Jane\" <jane@example.com>, ",
            "alice@x.com, bob@x.com, ",
            "Alice <alice@x.com>,",
        ];
        for raw in cases {
            let cleaned = normalize_recipient_field(raw);
            let parsed = mailparse::addrparse(&cleaned)
                .unwrap_or_else(|e| panic!("addrparse failed for {cleaned:?}: {e}"));
            assert!(
                parsed.iter().next().is_some(),
                "no recipients parsed from {cleaned:?}"
            );
            for info in parsed.iter() {
                match info {
                    mailparse::MailAddr::Single(s) => {
                        assert!(
                            !s.addr.trim().is_empty(),
                            "empty address from {cleaned:?}"
                        );
                    }
                    mailparse::MailAddr::Group(g) => {
                        for s in &g.addrs {
                            assert!(
                                !s.addr.trim().is_empty(),
                                "empty group address from {cleaned:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The store-backed drafting flows (#0052 unit A), over a real ingested store.
///
/// Each test writes its message through the ingest API, builds the source the
/// way the list flow does ([`crate::draft::source_from_row`]) and then asserts
/// the three things a user sees: the draft file `mp reply` / `mp forward`
/// would have written for the same source, the drafts index holding it right
/// away, and a status-line selector that resolves back to it.
#[cfg(test)]
mod store_backed_drafts {
    use super::*;
    use crate::parse::{AttachmentData, FetchedEmail};
    use crate::selector::Namespace;
    use crate::store::read::MessageRow;
    use crate::store::Store;

    /// Point the data directory at a tempdir so every `config::` path resolves
    /// inside the fixture, serialised against the other data-dir tests.
    pub(super) struct Fixture {
        _dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let guard = crate::config::data_dir_lock();
            let previous = std::env::var("MAILYPOPPINS_DATA_DIR").ok();
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("MAILYPOPPINS_DATA_DIR", dir.path());
            Self {
                _dir: dir,
                _guard: guard,
                previous,
            }
        }

        pub(super) fn store(&self) -> Store {
            Store::open(crate::config::store_path("alice")).unwrap()
        }

        /// Ingest one message and hand back the row the list would show.
        pub(super) fn ingest(&self, email: &FetchedEmail) -> MessageRow {
            let store = self.store();
            let blobs = BlobStore::for_account("alice");
            let outcome = crate::ingest::ingest_message(
                &store,
                &blobs,
                &crate::ingest::IngestInput {
                    account: "alice",
                    mailbox: "inbox",
                    uid: 1,
                    email,
                    raw: None,
                },
            )
            .unwrap();
            crate::store::read::find_by_id(&store, outcome.row_id)
                .unwrap()
                .unwrap()
        }

        /// The source of a reply or a forward off that row, which is what both
        /// the list flow and `mp reply` build.
        pub(super) fn source(&self, row: &MessageRow, with_attachments: bool) -> SourceMessage {
            let store = self.store();
            let blobs = BlobStore::for_account("alice");
            crate::draft::source_from_row(&store, &blobs, row, with_attachments).unwrap()
        }

        /// Resolve a selector through the drafts index, exactly as
        /// `mp send <selector>` would after reading it off the status line.
        pub(super) fn resolve(&self, selector: &Selector) -> crate::store::drafts::DraftRow {
            let store = self.store();
            let query =
                crate::selector::parse_in(&selector.to_string(), Namespace::Drafts, "alice", None)
                    .unwrap();
            crate::selector::resolve_draft(&store, &query).unwrap().0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("MAILYPOPPINS_DATA_DIR", v),
                None => std::env::remove_var("MAILYPOPPINS_DATA_DIR"),
            }
        }
    }

    pub(super) fn fixture_email(subject: &str) -> FetchedEmail {
        FetchedEmail {
            from: "Alice <alice@example.com>".into(),
            to: "me@example.com, bob@example.com".into(),
            cc: Some("carol@example.com".into()),
            subject: subject.into(),
            date: "Mon, 01 Jan 2024 12:00:00 +0000".into(),
            body_text: "Original body".into(),
            html_body: Some("<p>Rich body</p>".into()),
            has_attachments: false,
            message_id: Some(format!("<{subject}@example.com>")),
            attachments: Vec::new(),
            is_read: true,
            calendar_ics: None,
            event: None,
        }
    }

    /// Reply: the quote comes out of the body blob, the companion HTML out of
    /// the html blob, and the draft is the one `mp reply` writes for the same
    /// row. It is in the index before the status line names it.
    #[test]
    fn reply_writes_the_cli_draft_and_indexes_it_immediately() {
        let fx = Fixture::new();
        let row = fx.ingest(&fixture_email("Hello"));
        let source = fx.source(&row, false);

        let (path, selector) = draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: false },
            None,
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("from: \"me@example.com\""), "{content}");
        assert!(content.contains("to: \"alice@example.com\""), "{content}");
        assert!(content.contains("subject: \"Re: Hello\""), "{content}");
        assert!(content.contains("status: draft"), "{content}");
        // A plain reply addresses the sender only.
        assert!(!content.contains("cc: \""), "{content}");
        assert!(content.contains("{{SIGNATURE}}"), "{content}");
        assert!(
            content.contains("On Mon, 01 Jan 2024 12:00:00 +0000, Alice <alice@example.com> wrote:"),
            "{content}"
        );
        assert!(content.contains("> Original body"), "{content}");

        // The sender wrote markup, so the reply quotes markup (#0050 review).
        let companion = std::fs::read_to_string(path.with_extension("html")).unwrap();
        assert!(companion.contains("<p>Rich body</p>"), "{companion}");

        // The index holds it under the selector the status line shows.
        let indexed = fx.resolve(&selector);
        assert_eq!(indexed.path, path);
        assert_eq!(indexed.status, "draft");
        assert!(
            content.contains(&format!("id: {}", indexed.id)),
            "the file carries the id it is indexed under: {content}"
        );
    }

    /// Reply-all: every other recipient of the source, To and Cc alike, minus
    /// this account's own address.
    #[test]
    fn reply_all_carries_the_other_recipients_and_not_this_account() {
        let fx = Fixture::new();
        let row = fx.ingest(&fixture_email("Meeting"));
        let source = fx.source(&row, false);

        let (path, _) = draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: true },
            None,
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("to: \"alice@example.com\""), "{content}");
        assert!(
            content.contains("cc: \"bob@example.com, carol@example.com\""),
            "the other recipients, deduplicated and in order: {content}"
        );
        // This account is not copied on its own reply; the only line naming it
        // is the `from:`.
        assert_eq!(
            content
                .lines()
                .filter(|l| l.contains("me@example.com"))
                .collect::<Vec<_>>(),
            vec!["from: \"me@example.com\""],
            "{content}"
        );
    }

    /// Forward: the forwarded header block plus the body, and the row's
    /// attachment blobs materialised into the stable per-account mirror that
    /// outlives the source row (#0006).
    #[test]
    fn forward_carries_the_header_block_and_the_materialised_attachments() {
        let fx = Fixture::new();
        let mut email = fixture_email("Report");
        email.has_attachments = true;
        email.attachments = vec![AttachmentData {
            filename: "report.pdf".into(),
            content: b"fake pdf".to_vec(),
            content_id: None,
        }];
        let row = fx.ingest(&email);
        let source = fx.source(&row, true);

        let (path, selector) = draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Forward,
            None,
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("subject: \"Fwd: Report\""), "{content}");
        assert!(content.contains("to: \"\""), "{content}");
        assert!(
            content.contains("---------- Forwarded message ----------"),
            "{content}"
        );
        assert!(content.contains("From: Alice <alice@example.com>"), "{content}");
        assert!(content.contains("Original body"), "{content}");

        let expected = crate::parse::stable_attachments_dir(
            &crate::config::account_dir("alice"),
            "<Report@example.com>",
        )
        .join("report.pdf");
        assert!(
            content.contains(expected.to_string_lossy().as_ref()),
            "the draft references the stable mirror: {content}"
        );
        assert_eq!(std::fs::read(&expected).unwrap(), b"fake pdf");

        assert_eq!(fx.resolve(&selector).path, path);
    }

    /// The forward wizard's recipients and subject win over the ones the
    /// builder derived, and the body it wrote is left alone.
    #[test]
    fn the_forward_wizard_headers_replace_the_builders() {
        let fx = Fixture::new();
        let row = fx.ingest(&fixture_email("Report"));
        let source = fx.source(&row, true);

        let (path, selector) = draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Forward,
            Some(&DraftRecipientEdit {
                to: "dave@example.com".to_string(),
                cc: String::new(),
                bcc: String::new(),
                subject: "Fwd: Report (for review)".to_string(),
            }),
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("to: \"dave@example.com\""), "{content}");
        assert!(
            content.contains("subject: \"Fwd: Report (for review)\""),
            "{content}"
        );
        assert!(
            content.contains("---------- Forwarded message ----------"),
            "the body survives the header rewrite: {content}"
        );

        // The index holds the edited subject, not the derived one.
        let indexed = fx.resolve(&selector);
        assert_eq!(indexed.subject.as_deref(), Some("Fwd: Report (for review)"));
    }

    /// Edit recipients resolves the draft through the index by its `id:`, not
    /// through a path the list happened to be holding, and the index is
    /// refreshed so the row matches the file the wizard just rewrote.
    #[test]
    fn edit_recipients_finds_the_draft_through_the_index() {
        let fx = Fixture::new();
        let row = fx.ingest(&fixture_email("Hello"));
        let source = fx.source(&row, false);
        let (path, selector) = draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: false },
            None,
        )
        .unwrap();

        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        let id = fx.resolve(&selector).id;
        assert_eq!(indexed_draft_path(&mut app, &id), Some(path.clone()));

        crate::draft::rewrite_draft_recipients(
            &path,
            &DraftRecipientEdit {
                to: "erin@example.com".to_string(),
                cc: String::new(),
                bcc: String::new(),
                subject: "Re: Hello, again".to_string(),
            },
        )
        .unwrap();
        crate::store::drafts::refresh_account("alice").unwrap();

        let indexed = fx.resolve(&selector);
        assert_eq!(indexed.to.as_deref(), Some("erin@example.com"));
        assert_eq!(indexed.subject.as_deref(), Some("Re: Hello, again"));

        // A draft the index no longer holds declines instead of guessing.
        assert_eq!(indexed_draft_path(&mut app, "does-not-exist"), None);
    }

    /// A server-search hit that resolved to no row still replies: the fetched
    /// content the overlay is rendering is the source, and the draft it
    /// produces is the same shape a resolved hit's would be.
    #[test]
    fn an_unresolved_search_hit_replies_from_its_fetched_content() {
        let fx = Fixture::new();
        let mut email = fixture_email("Never synced");
        email.attachments = vec![AttachmentData {
            filename: "notes.txt".into(),
            content: b"notes".to_vec(),
            content_id: None,
        }];

        let source = crate::draft::source_from_fetched(
            &crate::config::account_dir("alice"),
            &email,
            true,
        )
        .unwrap();

        let (path, selector) = draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Forward,
            None,
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("subject: \"Fwd: Never synced\""),
            "{content}"
        );
        assert!(content.contains("Original body"), "{content}");
        let expected = crate::parse::stable_attachments_dir(
            &crate::config::account_dir("alice"),
            "<Never synced@example.com>",
        )
        .join("notes.txt");
        assert_eq!(std::fs::read(&expected).unwrap(), b"notes");
        assert!(
            content.contains(expected.to_string_lossy().as_ref()),
            "{content}"
        );
        assert_eq!(fx.resolve(&selector).path, path);
    }
}

/// The store-backed mutation flows (#0052 unit B), over the same fixture:
/// send, approve, mark-draft and the batch forms of the last two.
///
/// The send tests are offline by construction. The two halves of `mp send`'s
/// contract that do not need a server are exactly the two worth pinning: a
/// draft that is not approved is refused before anything is enqueued, and a
/// submission that reaches nobody still leaves the durable record the outbox
/// exists for, with the draft file untouched.
#[cfg(test)]
mod store_backed_mutations {
    use super::store_backed_drafts::{fixture_email, Fixture};
    use super::*;
    use crate::tui::app::EmailEntry;

    /// An account that submits to a closed port: the SMTP conversation fails
    /// on connect, deterministically and without a network.
    fn dead_smtp_ctx() -> SendCtx {
        SendCtx {
            graph: None,
            smtp: Some(crate::config::SmtpConfig {
                host: "127.0.0.1".to_string(),
                port: 1,
                username: String::new(),
                password: String::new(),
                default_from: "me@example.com".to_string(),
                accept_invalid_certs: false,
                auth_method: crate::config::AuthMethod::Password,
            }),
            account: crate::config::AccountConfig {
                name: "alice".to_string(),
                default_from: "me@example.com".to_string(),
                ..Default::default()
            },
            email_settings: crate::config::EmailSettings::default(),
            signature: None,
        }
    }

    /// A received list row: a `messages` row and no draft id, which is what
    /// `entry_from_row` builds.
    fn received_entry() -> EmailEntry {
        EmailEntry {
            msg: Some(MessageRef::new(1)),
            draft_id: None,
            ..draft_entry("unused")
        }
    }

    /// A Drafts list row: the indexed id and no `messages` row, which is what
    /// `entry_from_draft` builds.
    fn draft_entry(id: &str) -> EmailEntry {
        EmailEntry {
            msg: None,
            draft_id: Some(id.to_string()),
            from: String::new(),
            to: "alice@example.com".to_string(),
            cc: None,
            subject: "Re: Hello".to_string(),
            status: "draft".to_string(),
            date_display: "2026-07-01".to_string(),
            date_sort: "2026-07-01T00:00:00".to_string(),
            has_attachments: false,
            read: true,
            is_invite: false,
        }
    }

    /// An app whose cursor sits on `id`'s Drafts row.
    fn app_on_draft(id: &str) -> App {
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.emails = std::sync::Arc::new(vec![draft_entry(id)]);
        app.rebuild_visible();
        app
    }

    /// Write one reply draft off a fresh row and hand back its file and id.
    fn a_draft(fx: &Fixture) -> (PathBuf, Selector, String) {
        let row = fx.ingest(&fixture_email("Hello"));
        let source = fx.source(&row, false);
        let (path, selector) = draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: false },
            None,
        )
        .unwrap();
        let id = fx.resolve(&selector).id;
        (path, selector, id)
    }

    fn outbox_counts(fx: &Fixture) -> crate::outbox::OutboxCounts {
        crate::outbox::counts(&fx.store(), "alice").unwrap()
    }

    /// The approved-status requirement is `mp send`'s, and it lives in
    /// [`crate::send::build_draft_message`], which runs before the outbox row
    /// is written: a draft that is not approved is refused with the CLI's own
    /// message and leaves nothing behind.
    #[test]
    fn send_refuses_an_unapproved_draft_before_it_reaches_the_outbox() {
        let fx = Fixture::new();
        let (path, _selector, _id) = a_draft(&fx);
        let draft = crate::draft::parse_email_draft(&path).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match send_one_draft(&rt, &draft, &dead_smtp_ctx()) {
            Ok(_) => panic!("an unapproved draft must not be sent"),
            Err(e) => e,
        };

        let text = format!("{err:#}");
        assert!(text.contains("Email not approved for sending"), "{text}");
        assert!(text.contains("Current status: draft"), "{text}");
        assert_eq!(outbox_counts(&fx).total(), 0, "nothing was enqueued");
        assert!(std::fs::read_to_string(&path).unwrap().contains("status: draft"));
    }

    /// An approved draft is committed to the outbox before the submission is
    /// attempted, so a send that reaches nobody leaves a durable `failed` row
    /// and a draft that is still approved rather than a message lost between
    /// the two.
    #[test]
    fn a_send_that_reaches_nobody_leaves_a_failed_outbox_row_and_an_unsent_draft() {
        let fx = Fixture::new();
        let (path, selector, _id) = a_draft(&fx);
        crate::draft::mark_as_approved(&path).unwrap();
        crate::store::drafts::refresh_account("alice").unwrap();
        let draft = crate::draft::parse_email_draft(&path).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let report = send_one_draft(&rt, &draft, &dead_smtp_ctx()).unwrap();

        assert!(!report.send_result.any_succeeded());
        assert!(report.row_id.is_some(), "the message reached the outbox");
        assert!(matches!(
            report.state,
            Some(crate::outbox::OutboxState::Failed)
        ));
        assert_eq!(outbox_counts(&fx).failed, 1);

        // The status line is the CLI's refusal, and the draft is untouched:
        // nothing was marked sent.
        let err = send_status_line(&report).unwrap_err();
        assert!(
            err.to_string().starts_with("Failed to send to all"),
            "{err}"
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("status: approved"));
        assert_eq!(fx.resolve(&selector).status, "approved");
    }

    /// Approve and mark-draft flip the file `mp mark-approved` /
    /// `mp mark-draft` flip, name the draft by its selector, and leave the
    /// index holding the new status.
    #[test]
    fn approve_and_mark_draft_flip_the_indexed_status() {
        let fx = Fixture::new();
        let (_path, selector, id) = a_draft(&fx);
        let mut app = app_on_draft(&id);

        status_flip(&mut app, DraftStatusFlip::Approve);
        assert_eq!(app.status_message.as_deref(), Some(&*format!("Approved {selector}")));
        assert_eq!(fx.resolve(&selector).status, "approved");

        // The reload the flip triggers emptied the list (the fixture app has
        // no mailboxes to load from), so the cursor is put back by hand.
        app.emails = std::sync::Arc::new(vec![draft_entry(&id)]);
        app.rebuild_visible();
        status_flip(&mut app, DraftStatusFlip::Approve);
        assert_eq!(
            app.status_message.as_deref(),
            Some(&*format!("Already approved: {selector}"))
        );

        app.emails = std::sync::Arc::new(vec![draft_entry(&id)]);
        app.rebuild_visible();
        status_flip(&mut app, DraftStatusFlip::Demote);
        assert_eq!(app.status_message.as_deref(), Some(&*format!("Demoted {selector}")));
        assert_eq!(fx.resolve(&selector).status, "draft");
    }

    /// An illegal transition fails with the library's own error text, which is
    /// the one `mp mark-draft` prints: a sent email has left the draft
    /// pipeline and is not rewritten back into it.
    #[test]
    fn marking_a_sent_draft_back_to_draft_fails_like_the_cli() {
        let fx = Fixture::new();
        let (path, _selector, id) = a_draft(&fx);
        let draft = crate::draft::parse_email_draft(&path).unwrap();
        mark_draft_sent(&draft, None).unwrap();
        crate::store::drafts::refresh_account("alice").unwrap();

        let mut app = app_on_draft(&id);
        status_flip(&mut app, DraftStatusFlip::Demote);

        let status = app.status_message.clone().unwrap();
        assert!(
            status.starts_with("Mark-draft failed: Cannot revert a sent email back to draft"),
            "{status}"
        );
    }

    /// The batch flips every selected draft and counts what it could not do,
    /// which is the pre-nuke build's contract: one refusal is one failure, not
    /// an abort.
    #[test]
    fn the_batch_flips_every_selected_draft_and_counts_the_refusals() {
        let fx = Fixture::new();
        let (_p1, sel_one, one) = a_draft(&fx);
        let (_p2, sel_two, two) = a_draft(&fx);
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();

        status_flip_batch(&mut app, &[one.clone(), two.clone()], DraftStatusFlip::Approve);
        assert_eq!(app.status_message.as_deref(), Some("Approved 2 drafts"));
        assert_eq!(fx.resolve(&sel_one).status, "approved");
        assert_eq!(fx.resolve(&sel_two).status, "approved");

        status_flip_batch(
            &mut app,
            &[one.clone(), "not-in-the-index".to_string()],
            DraftStatusFlip::Demote,
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("Marked 1/2 as draft (1 failed)")
        );
        assert_eq!(fx.resolve(&sel_one).status, "draft");
        assert_eq!(fx.resolve(&sel_two).status, "approved");
    }

    /// `$EDITOR` opens the file the index holds for the row under the cursor,
    /// and a received row has none to open.
    #[test]
    fn edit_current_resolves_the_cursor_draft_through_the_index() {
        let fx = Fixture::new();
        let (path, _selector, id) = a_draft(&fx);
        let mut app = app_on_draft(&id);

        assert_eq!(
            cursor_draft(&mut app, "never shown"),
            Some((id, path)),
            "the cursor's draft resolves to the file the index holds"
        );

        // On a received row there is no file to open, and no CLI behaviour to
        // mirror: `mp edit` takes draft selectors only. The decline is
        // permanent, so it does not carry the #0052 line.
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.emails = std::sync::Arc::new(vec![received_entry()]);
        app.rebuild_visible();
        assert_eq!(
            cursor_draft(&mut app, "Open in $EDITOR needs a draft"),
            None
        );
        let status = app.status_message.clone().unwrap();
        assert_eq!(status, "Open in $EDITOR needs a draft");
        assert!(!status.contains("#0052"), "{status}");
    }
}

/// The store-backed file flows (#0052 unit C), over the same fixture:
/// attachments, the browser rendition and the invite source.
///
/// Every one of them used to read a file the ingest wrote beside a `.md`.
/// What is pinned here is that the bytes now come out of `message_blobs` and
/// land where the CLI puts them, that the naming a save collision produces is
/// the pre-nuke one, and that a server-search hit with no local row is served
/// from the fetch rather than declined.
#[cfg(test)]
mod store_backed_files {
    use super::store_backed_drafts::{fixture_email, Fixture};
    use super::*;
    use crate::parse::{AttachmentData, FetchedEmail};
    use crate::store::read::MessageRow;
    use crate::tui::app::EmailEntry;

    fn attachment(name: &str, bytes: &[u8]) -> AttachmentData {
        AttachmentData {
            filename: name.to_string(),
            content: bytes.to_vec(),
            content_id: None,
        }
    }

    /// An app whose cursor sits on the list row for `row`.
    fn app_on_row(row: &MessageRow) -> App {
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.emails = std::sync::Arc::new(vec![EmailEntry {
            msg: Some(MessageRef::new(row.id)),
            draft_id: None,
            from: row.from.clone().unwrap_or_default(),
            to: row.to.clone().unwrap_or_default(),
            cc: None,
            subject: row.subject.clone().unwrap_or_default(),
            status: "inbox".to_string(),
            date_display: row.date_display.clone().unwrap_or_default(),
            date_sort: String::new(),
            has_attachments: true,
            read: true,
            is_invite: false,
        }]);
        app.rebuild_visible();
        app
    }

    /// `o` and `O` on a received row resolve the row's blobs into the same
    /// files `mp open` materialises, under the same temp directory name.
    #[test]
    fn the_cursor_row_materialises_its_blobs_where_mp_open_puts_them() {
        let fx = Fixture::new();
        let mut email = fixture_email("With files");
        email.has_attachments = true;
        email.attachments = vec![
            attachment("notes.txt", b"notes"),
            attachment("report.pdf", b"%PDF-1.4"),
        ];
        let row = fx.ingest(&email);
        let mut app = app_on_row(&row);

        let files = cursor_attachment_files(&mut app).unwrap();

        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!(files[0].parent().unwrap(), attachment_temp_dir(row.id));
        let by_name: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(by_name, vec!["notes.txt", "report.pdf"]);
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"notes");
        assert_eq!(std::fs::read(&files[1]).unwrap(), b"%PDF-1.4");
    }

    /// A message with no attachments is not an error: the empty list is what
    /// the picker turns into "No attachments".
    #[test]
    fn a_row_without_attachments_resolves_to_an_empty_list() {
        let fx = Fixture::new();
        let row = fx.ingest(&fixture_email("Bare"));
        let mut app = app_on_row(&row);

        assert_eq!(cursor_attachment_files(&mut app), Some(Vec::new()));
        assert!(app.status_message.is_none(), "{:?}", app.status_message);
    }

    /// A Drafts row has no `messages` row behind it, so there are no blobs to
    /// materialise, and the status line says what it does have instead.
    #[test]
    fn a_draft_row_says_where_its_own_attachments_live() {
        let _fx = Fixture::new();
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.emails = std::sync::Arc::new(vec![EmailEntry {
            msg: None,
            draft_id: Some("some-draft".to_string()),
            from: String::new(),
            to: "alice@example.com".to_string(),
            cc: None,
            subject: "Re: Hello".to_string(),
            status: "draft".to_string(),
            date_display: "2026-07-01".to_string(),
            date_sort: "2026-07-01T00:00:00".to_string(),
            has_attachments: false,
            read: true,
            is_invite: false,
        }]);
        app.rebuild_visible();

        assert_eq!(cursor_attachment_files(&mut app), None);
        let status = app.status_message.clone().unwrap();
        assert_eq!(
            status,
            "Attachments needs a received message; a draft carries its own in `attachments:`"
        );
        assert!(!status.contains("#0052"), "{status}");
    }

    /// Save writes the materialised file into the chosen directory, and a
    /// second save of the same name does not overwrite the first: the
    /// `_1` suffix is the pre-nuke collision rule, unchanged because the save
    /// half still copies files.
    #[test]
    fn saving_the_same_attachment_twice_keeps_both_copies() {
        let fx = Fixture::new();
        let mut email = fixture_email("Twice");
        email.has_attachments = true;
        email.attachments = vec![attachment("notes.txt", b"notes")];
        let row = fx.ingest(&email);
        let mut app = app_on_row(&row);
        let files = cursor_attachment_files(&mut app).unwrap();

        let dest = tempfile::tempdir().unwrap();
        let first = crate::parse::save_attachment(&files[0], dest.path()).unwrap();
        let second = crate::parse::save_attachment(&files[0], dest.path()).unwrap();

        assert_eq!(first, dest.path().join("notes.txt"));
        assert_eq!(second, dest.path().join("notes_1.txt"));
        assert_eq!(std::fs::read(&first).unwrap(), b"notes");
        assert_eq!(std::fs::read(&second).unwrap(), b"notes");
    }

    /// `b` writes the html blob to a file and hands the browser that: the
    /// markup is the sender's own, not a re-render of the plain text.
    #[test]
    fn the_browser_gets_the_html_blob_written_to_a_file() {
        let fx = Fixture::new();
        let row = fx.ingest(&fixture_email("Rich"));
        let mut app = app_on_row(&row);

        let path = html_rendition_for_row(&mut app, row.id).unwrap();

        assert_eq!(path.extension().unwrap(), "html");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "<p>Rich body</p>");
    }

    /// A sender who wrote no markup has no rendition, which is a status line
    /// rather than an error or an empty page.
    #[test]
    fn a_message_without_html_says_so_instead_of_opening_an_empty_page() {
        let fx = Fixture::new();
        let mut email = fixture_email("Plain");
        email.html_body = None;
        let row = fx.ingest(&email);
        let mut app = app_on_row(&row);

        assert_eq!(html_rendition_for_row(&mut app, row.id), None);
        assert_eq!(
            app.status_message.as_deref(),
            Some("No HTML version available")
        );
    }

    /// The server-search hit that resolved to no local row: its attachments
    /// and its markup are the bytes the overlay is already holding, written
    /// out so the picker and the browser see files either way.
    #[test]
    fn an_unresolved_search_hit_is_served_from_the_fetch() {
        let _fx = Fixture::new();
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        let fetched = FetchedEmail {
            attachments: vec![attachment("../escape.txt", b"payload")],
            has_attachments: true,
            ..fixture_email("Never synced")
        };

        let files = fetched_attachment_files(&mut app, &fetched, 7).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].file_name().unwrap().to_string_lossy(),
            ".._escape.txt",
            "the filename is sanitised, so a hostile one cannot escape the temp dir"
        );
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"payload");

        let html = fetched.html_body.clone().unwrap();
        let page = html_rendition(&mut app, &html, "search-7").unwrap();
        assert_eq!(std::fs::read_to_string(&page).unwrap(), "<p>Rich body</p>");
    }

    /// The agenda's Open-source reads the invite's own ics blob off the row
    /// the `CalendarEvent` carries, which is what the action writes to the
    /// file `$EDITOR` is handed.
    #[test]
    fn the_event_source_resolves_the_invites_ics_blob() {
        let fx = Fixture::new();
        let ics = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";
        let mut email = fixture_email("Standup");
        email.calendar_ics = Some(ics.to_vec());
        let row = fx.ingest(&email);

        let store = fx.store();
        let blobs = BlobStore::for_account("alice");
        let source = crate::store::read::load_invite_ics(&store, &blobs, row.id).unwrap();

        assert_eq!(source, ics.to_vec());
        // A message that carries no invite has no source to open.
        let plain = fx.ingest(&fixture_email("Receipt"));
        assert_eq!(
            crate::store::read::load_invite_ics(&store, &blobs, plain.id),
            None
        );
    }
}
