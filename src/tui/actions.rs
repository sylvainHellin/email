use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result};
use ratatui::{backend::CrosstermBackend, Terminal};

use super::app::{
    mailbox_key, Action, App, BgResult, ComposeField, ComposeMode, ComposeWizard, Focus,
    HeldSend, MailboxKind, MessageRef, Overlay, StatusLevel, View,
};
use super::helpers::{
    edit_file, lib_do_multi_search_graph, lib_do_sync_graph, resume_terminal, suspend_terminal,
};
use crate::store::open_store;
use super::mutations;

use crate::draft::{
    create_draft_from_source, find_drafts, new_draft_skeleton, DraftFromSource,
    DraftRecipientEdit, SourceMessage,
};
use crate::selector::Selector;
use crate::send::SendReport;
use crate::store::BlobStore;
use crate::types::EmailStatus;

// ---------------------------------------------------------------------------
// Parking a sync behind the background work it cannot run alongside
// ---------------------------------------------------------------------------

/// Whether a fetch or a sync must wait. One gate, named once, because the
/// release condition in the event loop has to be the *same* condition: they
/// drifted apart (`bg_count` here, a mutation counter there), and the 250 ms tick
/// then released the parked action into this refusal about four times a
/// second.
pub(super) fn sync_is_blocked(app: &App) -> bool {
    app.bg_count > 0
}

/// Whether the event loop may hand a parked action back to the dispatcher.
///
/// Literally the negation of [`sync_is_blocked`], plus "nothing else is
/// already queued". Taking the same `&App` rather than the scalars it reads is
/// the point: restating the gate as `bg_count == 0` is how the two drifted
/// apart in the first place, and a call through [`sync_is_blocked`] cannot.
pub(super) fn queued_action_is_releasable(app: &App) -> bool {
    !sync_is_blocked(app) && app.pending_actions.is_empty()
}

/// Park `action` until the running sync or fetch clears, announcing it once.
///
/// Re-parking the same action is silent: the user asked for one sync and one
/// activity-log line is the honest record of that. A different action taking
/// the slot announces itself, because it is a different answer to the user.
///
/// The message no longer counts "ops pending" (#0039): a mutation used to be a
/// background job that a requested sync stacked behind, re-announcing itself on
/// every keypress. Mutations now enqueue silently into the durable queue and
/// block nothing, so the only thing a sync can wait behind is another sync or
/// fetch, and the line says just that.
fn park_until_idle(app: &mut App, action: Action, label: &str) {
    let already_parked = app
        .queued_action
        .as_ref()
        .is_some_and(|parked| std::mem::discriminant(parked) == std::mem::discriminant(&action));
    if !already_parked {
        app.set_status(format!("{label} queued (waiting for the current sync)"));
    }
    app.queued_action = Some(action);
}

// ---------------------------------------------------------------------------
// Files materialised out of the store (#0052 scope items 8, 9 and 10)
// ---------------------------------------------------------------------------

/// A file the TUI renders out of a message rather than one the message
/// carries: the browser rendition and the event source.
///
/// It lands in a `render/` subdirectory of the row's materialisation dir
/// rather than beside its attachments: an attachment filename is sanitised of
/// path separators, so it can never name a subdirectory, and a message
/// carrying its own `message.html` cannot overwrite the rendition.
fn render_temp_file(stem: &str, name: &str) -> Result<PathBuf> {
    let dir = crate::parse::materialisation_dir(stem)?.join("render");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(name))
}

/// The message under the cursor for a flow that reads its blobs, or `None`
/// with the status line saying why there is not one.
///
/// Same shape as [`cursor_message`], different explanation: what reaches here
/// with no store row is no longer a draft (a draft answers from its own
/// `attachments:` list since #0016) but a row with no identity at all -- a
/// parse-skipped draft file (#0080) or a server-search hit that resolved to
/// nothing -- and neither has bytes to materialise.
fn cursor_message_for_files(app: &mut App, what: &str) -> Option<MessageRef> {
    if let Some(msg) = app.selected_email_ref() {
        return Some(msg);
    }
    if app.selected_email().is_some() {
        app.set_status_level(
            format!("{what} needs a message or a readable draft; this row has neither"),
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
    // A draft names its attachments itself, so it answers from its own
    // frontmatter rather than from blobs it has none of (#0016).
    if app.selected_email().is_some_and(|e| e.draft_id.is_some()) {
        return draft_attachment_files(app);
    }
    let msg = cursor_message_for_files(app, "Attachments")?;
    row_attachment_files(app, msg.row_id())
}

/// The attachments of the *draft* under the cursor: the paths in its
/// `attachments:` frontmatter, resolved on disk (#0016).
///
/// Nothing is materialised, because nothing has to be: a draft's attachments
/// are already files, written there by the forward builder (into the stable
/// per-account mirror, #0006) or named by the user in `$EDITOR`. So this is
/// the one attachment source that hands the picker the real paths instead of
/// a private temp copy, and `o` opens the very file that will be sent, which
/// is the point of the key on a draft.
///
/// `~` is expanded the way the send path expands it ([`crate::send`]'s
/// `draft_attachments`), so a draft that sends is a draft that opens.
/// A path that is not there is named rather than skipped silently: a stale
/// entry is precisely what `o` is being pressed to find out about, and it is
/// the failure `mp send` would hit later (see the forwarded-attachment note in
/// `docs/lessons-learned.md`).
fn draft_attachment_files(app: &mut App) -> Option<Vec<PathBuf>> {
    let (_id, path) = cursor_draft(app, "Attachments needs a draft or a received message")?;
    let draft = match crate::draft::parse_email_draft(&path) {
        Ok(draft) => draft,
        Err(e) => {
            app.set_status_level(format!("Attachments failed: {e:#}"), StatusLevel::Error);
            return None;
        }
    };

    let listed = draft.frontmatter.attachments.unwrap_or_default();
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for entry in &listed {
        let expanded = shellexpand::tilde(entry).into_owned();
        let candidate = PathBuf::from(expanded);
        if candidate.is_file() {
            files.push(candidate);
        } else {
            missing.push(entry.clone());
        }
    }

    if !missing.is_empty() {
        let level = if files.is_empty() { StatusLevel::Error } else { StatusLevel::Warning };
        let n = missing.len();
        let noun = if n == 1 { "attachment" } else { "attachments" };
        app.set_status_level(
            format!("{n} {noun} missing: {}", missing.join(", ")),
            level,
        );
        if files.is_empty() {
            return None;
        }
    }
    Some(files)
}

/// Append a file path to the cursor draft's `attachments:` frontmatter (#0098).
///
/// The write counterpart of [`draft_attachment_files`] (#0016): where that
/// resolves and opens the paths a draft already lists, this adds one. It
/// reuses the same [`cursor_draft`] resolution, so the file it writes is the
/// draft under the cursor, and it stores the path verbatim -- `~` and all --
/// so the entry is the very one [`crate::send`]'s `draft_attachments` expands
/// and sends. The path was checked for existence at the prompt (see
/// `App::handle_attach_file_key`), which is where a stale path is surfaced;
/// a write failure here is named on the status line.
fn attach_file_to_draft(app: &mut App, path: &str) {
    let Some((id, draft_path)) = cursor_draft(
        app,
        "Attach needs a draft; received mail has no attachments to grow",
    ) else {
        return;
    };
    match crate::draft::append_draft_attachment(&draft_path, path) {
        Ok(()) => {
            let selector = Selector::for_draft(&app.account_config.name, &id);
            app.set_status(format!("Attached {path} to {selector}"));
            refresh_drafts_after_flip(app);
        }
        Err(e) => {
            app.set_status_level(format!("Attach failed: {e:#}"), StatusLevel::Error);
        }
    }
}

/// [`cursor_attachment_files`] for a row named directly, which is the
/// server-search hit that resolved to one.
pub(super) fn row_attachment_files(app: &mut App, row_id: i64) -> Option<Vec<PathBuf>> {
    let (store, blobs) = store_for_mutation(app, "Attachments")?;
    // The CLI's own directory, through the CLI's own helper: `mp open` and
    // `o` put the same row's files in the same private place.
    let dest = match crate::parse::materialisation_dir(&row_id.to_string()) {
        Ok(dir) => dir,
        Err(e) => {
            app.set_status_level(format!("Attachments failed: {e:#}"), StatusLevel::Error);
            return None;
        }
    };
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
    let dest = match crate::parse::materialisation_dir(&format!("search-{index}")) {
        Ok(dir) => dir,
        Err(e) => {
            app.set_status_level(format!("Attachments failed: {e:#}"), StatusLevel::Error);
            return None;
        }
    };
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
/// attachments use. `stem` keys the directory: the row id for a stored
/// message, the hit's position for one that is not stored.
fn html_temp_file(html: &str, stem: &str) -> Result<PathBuf> {
    let path = render_temp_file(stem, "message.html")?;
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

/// The name the read-only view carries, which is the title the editor puts on
/// the buffer (#0075).
///
/// The subject, so the window says which message is on screen; the row id when
/// there is no subject to slug. Uniqueness is not this name's job -- the
/// directory it lands in is already keyed by the row -- so two messages
/// sharing a subject cannot collide.
fn readonly_view_name(subject: &str, row_id: i64) -> String {
    let slug = slugify_subject_for_filename(subject);
    if slug.is_empty() {
        format!("message-{row_id}.md")
    } else {
        format!("{slug}.md")
    }
}

/// Write `contents` where the user cannot save over it (#0075).
///
/// 0444, so `$EDITOR` opens the buffer read-only and says so, rather than
/// letting someone believe an edit reaches the message. The mode is a signal,
/// not the guarantee: the guarantee is that nothing reads the file back, and
/// the file is gone when the editor exits.
///
/// A previous view of the same row left a file that mode also makes
/// unwritable, so it is removed rather than truncated -- the rendition is
/// rebuilt from the store on every open, and a stale one must not survive.
fn write_readonly(path: &Path, contents: &str) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("clearing {}: {e}", path.display()),
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
            .with_context(|| format!("making {} read-only", path.display()))?;
    }
    Ok(())
}

/// The read-only Markdown view of a stored row, or `None` with the status line
/// saying why there is none (#0075).
///
/// It lands beside the browser rendition and the invite source, under the
/// private per-row directory `parse::materialisation_dir` validates, and it is
/// deliberately nowhere the drafts index or the reconciler walks: the store is
/// the source of truth and this file is scratch.
pub(super) fn readonly_view_for_row(app: &mut App, row_id: i64) -> Option<PathBuf> {
    // The store connection is scoped to the read, the way the event source
    // scopes its own: `$EDITOR` owns the terminal for as long as the user
    // wants it, and holding SQLite open across that is pointless.
    let rendered = {
        let (store, blobs) = store_for_mutation(app, "Open")?;
        match crate::store::read::find_by_id(&store, row_id) {
            Ok(Some(row)) => {
                let name = readonly_view_name(row.subject.as_deref().unwrap_or_default(), row.id);
                Some((crate::store::read::render_markdown(&store, &blobs, &row), name))
            }
            Ok(None) => None,
            Err(e) => {
                app.set_status_level(format!("Open failed: {e:#}"), StatusLevel::Error);
                return None;
            }
        }
    };
    let Some((markdown, name)) = rendered else {
        app.set_status_level(
            "Open failed: that message is no longer in the store".to_string(),
            StatusLevel::Error,
        );
        return None;
    };
    let written = render_temp_file(&row_id.to_string(), &name)
        .and_then(|path| write_readonly(&path, &markdown).map(|()| path));
    match written {
        Ok(path) => Some(path),
        Err(e) => {
            app.set_status_level(format!("Open failed: {e:#}"), StatusLevel::Error);
            None
        }
    }
}

/// Parse and validate a draft, and only then persist its approved status.
///
/// `x` merges approve + send (#0092): the redesign dropped the separate
/// approve key, so an unapproved draft is approved as part of the send.
/// Approval runs strictly after the draft parses and validates (#0089): a
/// draft that fails validation keeps its `draft` status instead of carrying an
/// approved flag from a send that never happened. `mark_as_approved` is
/// idempotent, so a draft approved out of band passes through unchanged.
///
/// The draft is re-parsed after the approval write, and that is load-bearing:
/// `mark_as_approved` rewrites `status:` in the file, not in the struct this
/// function already parsed. Returning the pre-approval value handed
/// [`Action::Send`] a copy still reading `status: draft`, which
/// [`crate::send::build_draft_message`] then refused with "Email not approved
/// for sending" even though the file on disk was approved -- the `x`
/// approve-and-send key could never send an unapproved draft.
fn validate_then_approve(path: &Path) -> Result<crate::types::EmailDraft> {
    let draft = crate::draft::parse_email_draft(path)?;
    crate::draft::validate_draft(&draft)?;
    crate::draft::mark_as_approved(path)?;
    crate::draft::parse_email_draft(path)
}

/// Hand the read-only view of a stored row to `$EDITOR` and discard it on the
/// way back (#0075).
///
/// Same suspend / launch / resume dance as the draft flow and the event
/// source; what differs is the end, where the file is removed. Nothing read it
/// back, so an edit forced past the read-only buffer reaches nothing, and the
/// status line says so.
fn open_readonly_view(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    row_id: i64,
) -> Result<()> {
    let Some(path) = readonly_view_for_row(app, row_id) else {
        return Ok(());
    };
    // Neither half of the terminal dance may return before the rendition is
    // discarded: a terminal that cannot be suspended or restored must not
    // also leave the 0444 file behind for the next open to trip over.
    if let Err(e) = suspend_terminal(terminal) {
        discard_readonly_view(&path);
        return Err(e);
    }
    let result = edit_file(&path);
    let resumed = resume_terminal(terminal);
    finish_readonly_view(app, &path, result);
    resumed
}

/// Remove the read-only view, saying so in the log when it survives (#0075).
///
/// The removal is unconditional: a clean exit, an editor that never launched
/// and one that exited non-zero all land here, because the file is scratch in
/// every case. A removal that fails is logged rather than swallowed, and no
/// further: the next open rebuilds over whatever is left (`write_readonly`
/// removes first), so it is a trace for the log, not an error the user can
/// act on.
fn discard_readonly_view(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        log::warn!(
            "[open] the read-only view left {} behind: {e}",
            path.display()
        );
    }
}

/// Discard the read-only view and say how the editor session ended (#0075).
fn finish_readonly_view(app: &mut App, path: &Path, result: Result<()>) {
    discard_readonly_view(path);
    match result {
        Ok(()) => app.set_status(
            "Returned from the read-only copy (edits do not reach the message)".to_string(),
        ),
        Err(e) => app.set_status_level(format!("Open failed: {e}"), StatusLevel::Error),
    }
}

// ---------------------------------------------------------------------------
// Drafts written from a source message (#0052 scope items 1, 2 and 11)
// ---------------------------------------------------------------------------

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
    let signature = app.signature_content.clone();
    let (path, selector) =
        match create_draft_from_source(&account, &from, source, kind, None, signature.as_deref()) {
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
    let subject = open_store(&app.account_config.name)
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

/// Open the store and look a draft up by id for a delete: the shared prelude
/// of the single-row and batch draft deletes. `Store::open` rather than
/// `open_store`, for the reason the Drafts load gives: a never-synced account
/// has no store file and still has drafts.
fn draft_store_and_row(
    app: &mut App,
    id: &str,
) -> Option<(crate::store::Store, crate::store::drafts::DraftRow)> {
    let account = app.account_config.name.clone();
    let store = match crate::store::Store::open(crate::config::store_path(&account)) {
        Ok(store) => store,
        Err(e) => {
            app.set_status_level(
                format!("Reading the drafts index of {account} failed: {e:#}"),
                StatusLevel::Error,
            );
            return None;
        }
    };
    match crate::store::drafts::find(&store, &account, id) {
        Ok(Some(row)) => Some((store, row)),
        Ok(None) => {
            app.set_status_level(
                format!("That draft is no longer in the index ({id})"),
                StatusLevel::Error,
            );
            None
        }
        Err(e) => {
            app.set_status_level(
                format!("Reading the draft {id} failed: {e:#}"),
                StatusLevel::Error,
            );
            None
        }
    }
}

/// Delete the draft under the cursor: file and index row, behind the same `d`
/// confirmation received mail uses (#0073). Local-only, so no background op.
///
/// The TUI never force-deletes: an approved draft keeps its guard, and the user
/// demotes it with the mark-draft key first, exactly as the CLI asks. An
/// in-flight draft (#0063) is refused by the same library check the CLI runs.
fn delete_draft(app: &mut App, id: &str) {
    let account = app.account_config.name.clone();
    let Some((store, row)) = draft_store_and_row(app, id) else {
        return;
    };
    match crate::draft::delete_indexed_draft(&store, &account, &row, false) {
        Ok(()) => {
            drop(store);
            let selector = Selector::for_draft(&account, id);
            app.set_status(format!("Deleted {selector}"));
            refresh_drafts_after_flip(app);
        }
        Err(e) => app.set_status_level(
            format!("Delete failed: {e:#}"),
            StatusLevel::Error,
        ),
    }
}

/// Delete a parse-skipped draft by its path (#0080).
///
/// A skipped file has no index row and no `id:`, so the guard [`delete_draft`]
/// runs (approved, mid-send) cannot apply and there is nothing to resolve: the
/// file the error row names is removed straight from disk, and the drafts
/// refresh drops the row it stood for.
fn delete_skip_file(app: &mut App, path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {
            app.set_status(format!("Deleted {}", path.display()));
            refresh_drafts_after_flip(app);
        }
        Err(e) => {
            app.set_status_level(format!("Delete failed: {e:#}"), StatusLevel::Error);
        }
    }
}

/// Delete every selected draft, counting what went and logging what was
/// refused, the same shape as the batch status flip (#0073). A draft the guard
/// keeps (approved, or mid-send) is one miss among N, not an abort.
fn delete_drafts_batch(app: &mut App, ids: &[String]) {
    let account = app.account_config.name.clone();
    let total = ids.len();
    let mut deleted = 0usize;
    for id in ids {
        let Some((store, row)) = draft_store_and_row(app, id) else {
            continue;
        };
        match crate::draft::delete_indexed_draft(&store, &account, &row, false) {
            Ok(()) => deleted += 1,
            Err(e) => log::warn!("[drafts] not deleting {id}: {e:#}"),
        }
    }
    if deleted == total {
        app.set_status(format!("Deleted {deleted} drafts"));
    } else {
        app.set_status_level(
            format!("Deleted {deleted} of {total} drafts; the rest were kept (see the log)"),
            StatusLevel::Warning,
        );
    }
    refresh_drafts_after_flip(app);
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

/// Send one draft the way `mp send <selector>` does, and leave the same trail.
///
/// The orchestration itself is [`crate::send::send_draft`] (#0058): the outbox
/// commit, the transport choice, the sent copy and the draft file's fate are
/// all shared with the CLI. What is added here is the TUI's own half: the
/// blocking bridge into the async path, and the drafts-index refresh so the
/// retirement (or the `sent` status a partial send leaves) is the answer the
/// next selector resolution gives, without waiting for the one-second poll
/// (#0050's post-write refresh discipline).
fn send_one_draft(
    rt: &tokio::runtime::Runtime,
    draft: &crate::types::EmailDraft,
    ctx: &crate::send::SendContext,
) -> Result<SendReport> {
    let sent = rt.block_on(crate::send::send_draft(draft, ctx))?;
    if sent.report.send_result.any_succeeded() {
        if let Some(e) = sent.settle_error.as_ref() {
            log::warn!("[drafts] the send left the draft file behind: {e:#}");
        }
        if let Err(e) = crate::store::drafts::refresh_account(&ctx.account.name) {
            log::warn!("[drafts] refreshing after the send failed: {e:#}");
        }
    }
    Ok(sent.report)
}

/// Hand a parked send to the background send thread (#0090).
///
/// The tail of the send key: whatever the undo window was, this is where the
/// draft finally leaves for SMTP. Shared by the zero-window opt-out (fired
/// straight from `Action::Send`) and the event-loop tick that fires a held
/// send once its window elapses, so both take the identical path the send key
/// always took.
pub(super) fn fire_held_send(
    app: &mut App,
    held: HeldSend,
    bg_tx: &mpsc::Sender<BgResult>,
) {
    let HeldSend { draft, ctx, account_index, .. } = held;
    app.bg_count += 1;
    app.set_status_level("Sending...".to_string(), StatusLevel::Progress);
    let tx = bg_tx.clone();
    std::thread::spawn(move || {
        let rt = super::runtime::shared();
        let result = send_one_draft(rt, &draft, &ctx).and_then(|r| send_status_line(&r));
        let _ = tx.send(BgResult::Send {
            account_index,
            result: result.map_err(|e| format!("{e:#}")),
        });
    });
}

/// The status line one finished send shows, which is the CLI's own report:
/// how many recipients took it, and where the message actually is (#0037).
fn send_status_line(report: &SendReport) -> Result<String> {
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
///
/// Despite the name it is also the plain open helper, and read-only flows use
/// it as such (attachments, the browser rendition, the read-only view #0075):
/// it opens the two stores and nothing else, and `what` only names the
/// operation in the failure status line.
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

// The server half of a mutation is no longer fired from here (#0039): a
// mutation enqueues its op through `mutations::queue_*` and the durable
// `pending_ops` drain retires it at the next sync/fetch resume point, rolling
// back a refusal itself. So this module keeps no backend resolver, no per-op
// dispatch thread and no rollback of its own: the queue owns all three.

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
            // One key, two things to open, because the row under the cursor is
            // one of two things.
            //
            // A received row is materialised out of the store as Markdown and
            // opened read-only (#0075): the pre-nuke build handed `$EDITOR`
            // the message's own `.md`, and #0037 deleted the files without
            // replacing what they were good for -- searching, folding and
            // yanking a long message in a real editor rather than in a pane.
            // The store stays truth, so the copy is 0444 and discarded on the
            // way back.
            //
            // A drafts row is `mp edit <selector>` done in-process (#0052
            // scope item 7): resolved through the index and handed to
            // `$EDITOR` writable, with the index refreshed afterwards so the
            // list shows what the user just typed.
            if let Some(msg) = app.selected_email_ref() {
                return open_readonly_view(app, terminal, msg.row_id());
            }
            // A parse-skipped draft (#0080) has no index row to resolve, so it
            // opens its raw file writable: the whole point of the error row is
            // to let the user reach the broken YAML and fix it. The index
            // refresh on the way out then lists it as a normal draft.
            if let Some(skip) = app.selected_email().and_then(|e| e.skip.clone()) {
                edit_new_draft(app, terminal, &skip.path, "Returned from editor".to_string())?;
                return Ok(());
            }
            let Some((_id, path)) = cursor_draft(app, "Open in $EDITOR needs a message or a draft")
            else {
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

            let draft = match validate_then_approve(&path) {
                Ok(draft) => draft,
                Err(e) => {
                    app.set_status_level(format!("Send failed: {e:#}"), StatusLevel::Error);
                    return Ok(());
                }
            };

            // Which account sends it is the draft's own `from:`, not the open
            // mailbox: a draft written for another configured account is sent
            // from that account's SMTP or Graph credentials.
            //
            // The IMAP config that resolver also hands back is not used here:
            // the sent copy is an APPEND the outbox owns and drives (#0037),
            // not something this path does after the fact.
            // The signature now lives in the draft body (#0099), appended at
            // creation, so the send-time injection is off here: passing it
            // again would double it.
            let (acct_idx, smtp, _imap, graph, account_config, _signature) =
                super::helpers::resolve_send_account(app, &path);
            // The account's `auth_method` decides the transport, not which
            // config happened to load: a Graph account sends over Graph or not
            // at all (see `resolve_send_transport`).
            let (graph, smtp) =
                match super::helpers::resolve_send_transport(&account_config, graph, smtp) {
                    Ok(pair) => pair,
                    Err(missing) => {
                        app.set_status_level(missing.to_string(), StatusLevel::Error);
                        return Ok(());
                    }
                };
            let ctx = crate::send::SendContext {
                graph,
                smtp,
                account: account_config,
                email_settings: app.global_config.email.clone(),
                signature: None,
            };

            // Undo-send hold (#0090): park the send behind the configured
            // window instead of handing it to SMTP at once. A zero window is
            // the opt-out and fires immediately; a non-zero one waits, so `u`
            // can cancel it (see `dispatch_normal_mode` and the event loop).
            let hold = std::time::Duration::from_secs(app.global_config.email.send_hold_secs);
            let held = HeldSend {
                draft,
                ctx,
                account_index: acct_idx,
                fire_at: std::time::Instant::now() + hold,
            };
            if hold.is_zero() {
                fire_held_send(app, held, bg_tx);
            } else {
                // Only one send waits at a time: a second arm flushes the first
                // rather than dropping it, so pressing send twice never loses a
                // message.
                if let Some(prev) = app.held_send.take() {
                    fire_held_send(app, prev, bg_tx);
                }
                app.set_status_level(
                    format!(
                        "Sending in {}s (press u to undo)",
                        app.global_config.email.send_hold_secs
                    ),
                    StatusLevel::Progress,
                );
                app.held_send = Some(held);
            }
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
                let rt = super::runtime::shared();
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
            // `mp send-approved` in-process, over the same one send
            // implementation the single-draft key and the CLI use (#0058):
            // every approved draft in the open Drafts directory goes through
            // [`crate::send::send_draft`], which owns the outbox commit, the
            // transport choice and the draft file's fate. What is counted
            // here is only how many made it.
            // Only the Drafts mailbox has a directory to scan; from anywhere
            // else the answer is the one the old directory walk gave, without
            // walking a tree that has not existed since the store cutover.
            let Some(dir) = app.active_drafts_dir() else {
                app.set_status_level(
                    "No approved emails found".to_string(),
                    StatusLevel::Success,
                );
                return Ok(());
            };
            // A Graph account sends over Graph or not at all: an SMTP config
            // that happens to be loaded is not a fallback for a Graph config
            // that is not (see `resolve_send_transport`).
            let (graph, smtp) = match super::helpers::resolve_send_transport(
                &app.account_config,
                app.graph_config.clone(),
                app.smtp_config.clone(),
            ) {
                Ok(pair) => pair,
                Err(missing) => {
                    app.set_status_level(missing.to_string(), StatusLevel::Error);
                    return Ok(());
                }
            };
            let is_graph = graph.is_some();
            let ctx = crate::send::SendContext {
                graph,
                smtp,
                account: app.account_config.clone(),
                email_settings: app.global_config.email.clone(),
                // Signature is in the draft body (#0099); no send-time inject.
                signature: None,
            };

            app.bg_count += 1;
            app.set_status_level(
                if is_graph {
                    "Sending approved via Graph...".to_string()
                } else {
                    "Sending approved...".to_string()
                },
                StatusLevel::Progress,
            );
            let acct_idx = app.active_account;
            let tx = bg_tx.clone();
            std::thread::spawn(move || {
                let rt = super::runtime::shared();
                let result = (|| -> anyhow::Result<String> {
                    let drafts = find_drafts(&dir, Some(EmailStatus::Approved))?;
                    if drafts.is_empty() {
                        return Ok("No approved emails found".to_string());
                    }

                    let mut sent = 0usize;
                    let mut failed = 0usize;
                    for draft in &drafts {
                        match rt.block_on(crate::send::send_draft(draft, &ctx)) {
                            Ok(outcome) if outcome.report.send_result.any_succeeded() => sent += 1,
                            Ok(_) => failed += 1,
                            Err(e) => {
                                log::warn!(
                                    "[send] {} was not sent: {e:#}",
                                    draft.path.display()
                                );
                                failed += 1;
                            }
                        }
                    }
                    // One refresh for the batch, not one per draft: the index
                    // is read again the moment the status line lands.
                    if sent > 0 {
                        if let Err(e) = crate::store::drafts::refresh_account(&ctx.account.name) {
                            log::warn!("[drafts] refreshing after send-approved failed: {e:#}");
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

        Action::NewDraft => {
            let name = chrono::Local::now()
                .format("draft-%Y%m%d-%H%M%S")
                .to_string();
            let file_name = format!("{name}.md");
            let dir = app.drafts_dir();
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
                let skeleton = new_draft_skeleton(from, &now, app.signature_content.as_deref());
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
                archive_msgs(app, vec![msg], false);
            }
        }

        Action::Delete => {
            // A Drafts row has no `messages` row to prepare, so `d` on it went
            // to `delete_msgs` and reported "nothing to delete" (#0073). Route
            // it to the local-only draft delete instead; received mail keeps
            // the store-mutation path.
            let skip_path = app
                .selected_email()
                .and_then(|e| e.skip.as_ref().map(|s| s.path.clone()));
            let draft_id = app.selected_email().and_then(|e| e.draft_id.clone());
            if let Some(path) = skip_path {
                // A parse-skipped draft has no index row, so it is deleted by
                // its path: the file the user can see is the file that goes
                // (#0080).
                delete_skip_file(app, &path);
            } else if let Some(id) = draft_id {
                delete_draft(app, &id);
            } else if let Some(msg) = app.selected_email_ref() {
                delete_msgs(app, vec![msg], false);
            }
        }

        Action::BatchArchive(msgs) => {
            archive_msgs(app, msgs, true);
        }

        Action::BatchDelete(msgs) => {
            delete_msgs(app, msgs, true);
        }

        Action::BatchDeleteDrafts(ids) => {
            delete_drafts_batch(app, &ids);
        }

        Action::MoveToMailbox { msgs, dest_idx } => {
            // Quick-move to an arbitrary mailbox (#0018): the generalized
            // archive. The store row moves and the owed server move enqueue in
            // one transaction; the drain carries it to the server at the next
            // resume point and rolls a refusal back (#0039).
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

            let Some((store, _blobs)) = store_for_mutation(app, "Move") else {
                return Ok(());
            };
            let account = app.account_config.name.clone();
            let touched_invite = any_invite(app, &msgs);
            let moved =
                mutations::queue_move(&store, &account, &msgs, &dest_mailbox, &source_server, &dest_server);
            drop(store);
            if moved.is_empty() {
                app.set_status_level(
                    "Move failed: nothing to move".to_string(),
                    StatusLevel::Error,
                );
                return Ok(());
            }

            let removed: HashSet<MessageRef> = moved.iter().copied().collect();
            app.remove_selected_from_list_batch(&removed);
            app.selection.clear();
            refresh_after_mutation(app, Some(dest_idx), touched_invite);

            let count = moved.len();
            app.set_status_level(
                if count == 1 {
                    format!("Moved to {dest_label}")
                } else {
                    format!("Moved {count} emails to {dest_label}")
                },
                StatusLevel::Success,
            );
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
                if set_read_flag(app, vec![msg], new_read) {
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
                set_read_flag(app, vec![msg], true);
            }
        }

        Action::BatchToggleRead(msgs) => {
            let any_unread = msgs
                .iter()
                .any(|m| app.emails.iter().any(|e| e.msg == Some(*m) && !e.read));
            let new_read = any_unread;
            let count = msgs.len();
            if set_read_flag(app, msgs, new_read) {
                app.selection.clear();
                app.set_status(if new_read {
                    format!("Marked {count} as read")
                } else {
                    format!("Marked {count} as unread")
                });
            }
        }

        Action::ToggleFlag => {
            if let Some(email) = app.selected_email() {
                let new_flag = !email.flagged;
                let Some(msg) = email.msg else {
                    return Ok(());
                };
                let label = if new_flag { "Flagged" } else { "Unflagged" };
                if set_flag(app, vec![msg], new_flag) {
                    app.set_status(label.to_string());
                }
            }
        }

        Action::BatchToggleFlag(msgs) => {
            let any_unflagged = msgs
                .iter()
                .any(|m| app.emails.iter().any(|e| e.msg == Some(*m) && !e.flagged));
            let new_flag = any_unflagged;
            let count = msgs.len();
            if set_flag(app, msgs, new_flag) {
                app.selection.clear();
                app.set_status(if new_flag {
                    format!("Flagged {count}")
                } else {
                    format!("Unflagged {count}")
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
            if sync_is_blocked(app) {
                park_until_idle(app, Action::Fetch, "Quick sync");
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
                        super::runtime::shared();
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
                        super::runtime::shared();
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
                    let rt = super::runtime::shared();
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
                    let rt = super::runtime::shared();
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
                        super::runtime::shared();
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
                        super::runtime::shared();
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
            handle_search_result_action(app, terminal, action)?;
        }

        Action::Sync => {
            if sync_is_blocked(app) {
                park_until_idle(app, Action::Sync, "Full sync");
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
                        super::runtime::shared();
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
                        super::runtime::shared();
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
            let path = match render_temp_file(&row_id.to_string(), "invite.ics") {
                Ok(path) => path,
                Err(e) => {
                    app.set_status_level(format!("Open failed: {e:#}"), StatusLevel::Error);
                    return Ok(());
                }
            };
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
        Action::AttachFileToDraft { path } => {
            attach_file_to_draft(app, &path);
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
        body: String::new(),
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
        body: String::new(),
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
    let dir = app.drafts_dir();

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
        let signature = app.signature_content.clone();
        let (path, selector) = match create_draft_from_source(
            &account,
            &from,
            &source,
            DraftFromSource::Forward,
            Some(&edit),
            signature.as_deref(),
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

    // A non-empty inline body (#0097) means the draft is complete: it was
    // written into the file by `write_new_draft_from_wizard`, so skip the
    // `$EDITOR` round-trip and land in Drafts directly. An empty body keeps
    // the original behaviour and hands off to `$EDITOR` below.
    if !wizard.body.trim().is_empty() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        app.set_status(format!("Created: {}", name));
        app.reload_current_mailbox();
        return Ok(());
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
    let dir = app.drafts_dir();
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

    // An inline body (#0097) is written straight into the draft so a short
    // message never needs `$EDITOR`. Empty leaves the body blank, which is the
    // signal `submit_compose_wizard` uses to open `$EDITOR` instead.
    let body = wizard.body.trim();
    if !body.is_empty() {
        fm.push_str(body);
        fm.push('\n');
    }

    // Per-account signature (#0099): appended to the body at creation so it is
    // visible and editable, below the inline body when there is one. An empty
    // body still opens `$EDITOR` (that decision reads `wizard.body`, not the
    // file), so the user edits a draft that already carries the signature.
    if let Some(sig) = app.signature_content.as_deref() {
        let sig = sig.trim_end();
        if !sig.is_empty() {
            if !body.is_empty() {
                fm.push('\n');
            }
            fm.push_str(sig);
            fm.push('\n');
        }
    }

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
/// none. Open needs a stored row to render, because the read-only view is a
/// materialisation of the store (#0075) and a hit that never synced has no row
/// to materialise.
fn handle_search_result_action(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: Action,
) -> Result<()> {
    match action {
        Action::SearchResultOpen => {
            // A hit that resolved is opened exactly as the list opens it: the
            // read-only Markdown rendition of its row. One that did not is a
            // message on the server this account has never ingested, so there
            // is nothing to render, and the overlay is already showing the
            // headers and the body that editor window would hold.
            let Some(hit) = app.server_search_results.get(app.server_search_index) else {
                return Ok(());
            };
            let Some(msg) = hit.entry.msg else {
                app.set_status_level(
                    "Open in $EDITOR needs a stored message; this hit is not in the local store"
                        .to_string(),
                    StatusLevel::Warning,
                );
                return Ok(());
            };
            open_readonly_view(app, terminal, msg.row_id())?;
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

            archive_msgs(app, vec![msg], false);
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

/// Archive one or many messages: the store rows move into the archive mailbox
/// and the owed server moves enqueue in the same transaction (#0039).
///
/// The drain carries them to the server at the next sync/fetch resume point and
/// rolls a refusal back, so there is no per-op thread and no status to wait on:
/// the local move is instant and confirmed. `batch` says whether the selection
/// should be cleared afterwards, the only difference between the single and the
/// batch arm.
fn archive_msgs(app: &mut App, msgs: Vec<MessageRef>, batch: bool) {
    let Some(dest_idx) = app.find_mailbox_by_kind(MailboxKind::Archive) else {
        app.set_status_level(
            "Archive mailbox not configured".to_string(),
            StatusLevel::Error,
        );
        return;
    };
    let dest_mailbox = match app.mailboxes.get(dest_idx) {
        Some(mb) => mailbox_key(mb),
        None => return,
    };
    let dest_server = app.archive_server_name.clone();
    let source_server = app.active_server_mailbox();

    let Some((store, _blobs)) = store_for_mutation(app, "Archive") else {
        return;
    };
    let account = app.account_config.name.clone();
    let touched_invite = any_invite(app, &msgs);
    let archived_refs =
        mutations::queue_move(&store, &account, &msgs, &dest_mailbox, &source_server, &dest_server);
    drop(store);
    if archived_refs.is_empty() {
        app.set_status_level(
            "Archive failed: nothing to archive".to_string(),
            StatusLevel::Error,
        );
        return;
    }

    let archived: HashSet<MessageRef> = archived_refs.iter().copied().collect();
    app.remove_selected_from_list_batch(&archived);
    if batch {
        app.selection.clear();
    }
    refresh_after_mutation(app, Some(dest_idx), touched_invite);

    let count = archived_refs.len();
    app.set_status_level(
        if count == 1 {
            "Email archived".to_string()
        } else {
            format!("Archived {count} emails")
        },
        StatusLevel::Success,
    );
}

/// Delete one or many messages: the store rows go and the owed server deletes
/// enqueue in the same transaction (#0039).
///
/// The rows are removed rather than tombstoned, so a refused server delete is
/// answered by the next sync refetching the message; the drain surfaces the
/// refusal (see [`crate::pending_ops`]).
fn delete_msgs(app: &mut App, msgs: Vec<MessageRef>, batch: bool) {
    let source_server = app.active_server_mailbox();
    let Some((store, blobs)) = store_for_mutation(app, "Delete") else {
        return;
    };
    let account = app.account_config.name.clone();
    let touched_invite = any_invite(app, &msgs);
    let deleted_refs = mutations::queue_delete(&store, &blobs, &account, &msgs, &source_server);
    drop(store);
    if deleted_refs.is_empty() {
        app.set_status_level(
            "Delete failed: nothing to delete".to_string(),
            StatusLevel::Error,
        );
        return;
    }

    // Every deleted row's id is dead the moment the row is: the list, the
    // selection set and the cursor anchor must not carry one across this
    // boundary, because a re-ingest of the same message mints a new id.
    let deleted: HashSet<MessageRef> = deleted_refs.iter().copied().collect();
    app.remove_selected_from_list_batch(&deleted);
    if batch {
        app.selection.clear();
    }
    refresh_after_mutation(app, None, touched_invite);

    let count = deleted_refs.len();
    app.set_status_level(
        if count == 1 {
            "Email deleted".to_string()
        } else {
            format!("Deleted {count} emails")
        },
        StatusLevel::Success,
    );
}

/// Set the read flag on one or many messages: the store row and the owed server
/// op commit together (#0039).
///
/// Returns false when nothing was applied, so the caller can skip its status
/// line. The in-memory list is updated beside the row because the list is what
/// the user is looking at; the drain rolls both the store row and (on the next
/// refresh) the list back if the server refuses.
fn set_read_flag(app: &mut App, msgs: Vec<MessageRef>, read: bool) -> bool {
    let server_mailbox = app.active_server_mailbox();
    let Some((store, _blobs)) = store_for_mutation(app, "Read flag") else {
        return false;
    };
    let account = app.account_config.name.clone();
    let flagged = mutations::queue_read_flag(&store, &account, &msgs, read, &server_mailbox);
    drop(store);
    if flagged.is_empty() {
        return false;
    }

    for msg in &flagged {
        app.set_email_read(*msg, read);
    }
    true
}

/// Auto-mark the message shown in the preview pane as read (#0087).
///
/// The trigger is opening a message into the preview, with no dwell timer
/// (owner decision, 2026-08-14). [`App::take_message_to_auto_mark_read`] yields
/// the message once per open and skips drafts and already-read rows, and this
/// reuses the manual [`set_read_flag`] path, so the local write and the owed
/// `\Seen` op commit together (#0039) and converge on the next sync (#0004)
/// rather than opening a second write path. Returns whether a row was marked,
/// so the caller can force a repaint.
///
/// Only the plain mail view shows a message in the preview pane: a calendar or
/// contacts cursor, or any modal overlay, is not an open, so this is a no-op
/// there and does not disturb the once-per-open tracker. An input-owning focus
/// is not an open either: while `/` filters the list, every keystroke narrows
/// `visible` and resets the cursor to the new top row, and marking each
/// transient top result read would commit `\Seen` ops for messages the user
/// only filtered past. The row the cursor lands on when the input is left
/// counts as the open instead.
pub(crate) fn auto_mark_open_read(app: &mut App) -> bool {
    if app.view != View::Mail
        || app.overlay.is_active()
        || matches!(
            app.focus,
            crate::tui::app::Focus::Search | crate::tui::app::Focus::ComposeWizard
        )
    {
        return false;
    }
    let Some(msg) = app.take_message_to_auto_mark_read() else {
        return false;
    };
    set_read_flag(app, vec![msg], true)
}

/// Set the `\Flagged` star on one or many messages (#0007): the store row and
/// the owed server op commit together (#0039). Modelled on [`set_read_flag`].
fn set_flag(app: &mut App, msgs: Vec<MessageRef>, flagged: bool) -> bool {
    let server_mailbox = app.active_server_mailbox();
    let Some((store, _blobs)) = store_for_mutation(app, "Flag") else {
        return false;
    };
    let account = app.account_config.name.clone();
    let starred = mutations::queue_flag(&store, &account, &msgs, flagged, &server_mailbox);
    drop(store);
    if starred.is_empty() {
        return false;
    }

    for msg in &starred {
        app.set_email_flagged(*msg, flagged);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::EmailEntry;

    fn entry(subject: &str, id: i64, is_invite: bool) -> EmailEntry {
        EmailEntry {
            msg: Some(MessageRef::new(id)),
            draft_id: None,
            skip: None,
            from: "Sender <s@example.com>".to_string(),
            to: "me@example.com".to_string(),
            cc: None,
            reply_to: None,
            bcc: None,
            subject: subject.to_string(),
            status: "inbox".to_string(),
            date_display: "2026-07-01".to_string(),
            date_sort: "2026-07-01T00:00:00".to_string(),
            has_attachments: false,
            read: false,
            answered: false,
            forwarded: false,
            flagged: false,
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

    /// Auto-mark-read (#0087) only fires in the plain mail view: a Calendar or
    /// Contacts cursor does not show a message in the preview pane, so opening
    /// nothing there neither marks a row nor arms the once-per-open tracker.
    #[test]
    fn auto_mark_open_read_is_a_no_op_outside_the_mail_view() {
        let mut app = App::default_for_tests();
        app.emails = std::sync::Arc::new(vec![entry("Unread", 1, false)]);
        app.visible = vec![0];
        app.list_index = 0;
        app.view = View::Calendar;

        assert!(!auto_mark_open_read(&mut app));
        assert!(app.auto_read_opened.is_none(), "the tracker was not armed");
    }

    /// Auto-mark-read (#0087) does not fire while a modal overlay is up: an
    /// open thread, search or help overlay is what the user is reading, not the
    /// preview beneath it.
    #[test]
    fn auto_mark_open_read_is_a_no_op_under_an_overlay() {
        let mut app = App::default_for_tests();
        app.emails = std::sync::Arc::new(vec![entry("Unread", 1, false)]);
        app.visible = vec![0];
        app.list_index = 0;
        app.overlay = Overlay::Help;

        assert!(!auto_mark_open_read(&mut app));
        assert!(app.auto_read_opened.is_none(), "the tracker was not armed");
    }

    /// Auto-mark-read (#0087) does not fire while the inline filter owns the
    /// input: each `/` keystroke narrows `visible` and resets the cursor to
    /// the new top row, and marking those transient top results read would
    /// commit `\Seen` ops for messages the user only filtered past.
    #[test]
    fn auto_mark_open_read_is_a_no_op_while_the_filter_owns_the_input() {
        let mut app = App::default_for_tests();
        app.emails = std::sync::Arc::new(vec![entry("Unread", 1, false)]);
        app.visible = vec![0];
        app.list_index = 0;
        app.focus = crate::tui::app::Focus::Search;

        assert!(!auto_mark_open_read(&mut app));
        assert!(app.auto_read_opened.is_none(), "the tracker was not armed");
    }

    // -----------------------------------------------------------------------
    // Parking a sync: one announcement, and a release that matches the gate
    // -----------------------------------------------------------------------

    /// A parked action is announced when it is parked, and never again. The
    /// event loop re-offers it on every tick until the background work clears,
    /// and each re-offer used to push another activity line: ~4 per second for
    /// as long as the sync ran.
    #[test]
    fn re_parking_the_same_action_does_not_announce_it_again() {
        let mut app = App::default_for_tests();
        app.bg_count = 2;

        park_until_idle(&mut app, Action::Fetch, "Quick sync");
        for _ in 0..40 {
            park_until_idle(&mut app, Action::Fetch, "Quick sync");
        }

        assert_eq!(app.status_log.len(), 1, "one keypress, one activity line");
        assert_eq!(
            app.status_log[0].message,
            "Quick sync queued (waiting for the current sync)"
        );
        assert!(matches!(app.queued_action, Some(Action::Fetch)));
    }

    /// A different action taking the slot is a different answer to the user,
    /// so it says so.
    #[test]
    fn parking_a_different_action_announces_it() {
        let mut app = App::default_for_tests();
        app.bg_count = 1;

        park_until_idle(&mut app, Action::Fetch, "Quick sync");
        park_until_idle(&mut app, Action::Sync, "Full sync");
        park_until_idle(&mut app, Action::Sync, "Full sync");

        assert_eq!(app.status_log.len(), 2);
        assert_eq!(
            app.status_log[1].message,
            "Full sync queued (waiting for the current sync)"
        );
        assert!(matches!(app.queued_action, Some(Action::Sync)));
    }

    /// The release condition and the gate the released action re-enters are
    /// the same condition. A background sync raises `bg_count` without
    /// raising any mutation counter, which is exactly the case the old
    /// mutations-are-idle release got wrong.
    #[test]
    fn a_parked_action_is_released_only_once_the_gate_it_re_enters_has_cleared() {
        let mut app = App::default_for_tests();
        app.bg_count = 1;
        assert!(sync_is_blocked(&app));
        assert!(
            !queued_action_is_releasable(&app),
            "releasing here hands the action straight back into its own refusal"
        );

        app.bg_count = 0;
        assert!(!sync_is_blocked(&app));
        assert!(queued_action_is_releasable(&app));

        app.pending_actions.push_back(Action::Fetch);
        assert!(
            !queued_action_is_releasable(&app),
            "a queue that already holds work waits its turn"
        );
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

    // -----------------------------------------------------------------------
    // Inline compose body (#0097)
    // -----------------------------------------------------------------------

    /// A `New` wizard with the given inline body, pointed at a tempdir so the
    /// draft lands inside the fixture instead of the real Drafts folder.
    fn wizard_with_body(body: &str) -> ComposeWizard {
        ComposeWizard {
            mode: ComposeMode::New,
            to: "alice@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Quick note".to_string(),
            body: body.to_string(),
            focus: ComposeField::Body,
            suggestions: Vec::new(),
            suggestion_idx: 0,
            contacts: None,
        }
    }

    /// A non-empty inline body is written straight into the draft `.md`, after
    /// the frontmatter fence, so a short message never needs `$EDITOR`.
    #[test]
    fn wizard_body_is_written_into_the_draft_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::default_for_tests();
        app.drafts_dir = Some(dir.path().to_path_buf());

        let wizard = wizard_with_body("Thanks, see you Tuesday.");
        let path = write_new_draft_from_wizard(&app, &wizard).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("subject: \"Quick note\""), "{content}");
        // Body follows the closing frontmatter fence, not the opening one.
        let after_fence = content
            .rsplit_once("---\n\n")
            .map(|(_, tail)| tail)
            .unwrap_or_default();
        assert_eq!(after_fence, "Thanks, see you Tuesday.\n", "{content}");
    }

    /// An empty inline body leaves the draft body blank: the file ends at the
    /// closing fence, which is the signal `submit_compose_wizard` uses to open
    /// `$EDITOR` instead.
    #[test]
    fn wizard_empty_body_leaves_the_draft_body_blank() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::default_for_tests();
        app.drafts_dir = Some(dir.path().to_path_buf());

        let wizard = wizard_with_body("   ");
        let path = write_new_draft_from_wizard(&app, &wizard).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.ends_with("---\n\n"), "{content:?}");
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
    /// inside the fixture.
    ///
    /// Thread-local (#0077): no process environment is mutated, so no other
    /// test can observe this fixture's data dir and no lock is needed.
    /// Materialised message files land under `parse::test_temp_root()` for the
    /// same reason -- see the note there.
    pub(super) struct Fixture {
        _dir: crate::config::test_env::TestDataDir,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            Self {
                _dir: crate::config::test_env::TestDataDir::new(),
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

    pub(super) fn fixture_email(subject: &str) -> FetchedEmail {
        FetchedEmail {
            from: "Alice <alice@example.com>".into(),
            to: "me@example.com, bob@example.com".into(),
            cc: Some("carol@example.com".into()),
            reply_to: None,
            bcc: None,
            subject: subject.into(),
            date: "Mon, 01 Jan 2024 12:00:00 +0000".into(),
            body_text: "Original body".into(),
            html_body: Some("<p>Rich body</p>".into()),
            has_attachments: false,
            message_id: Some(format!("<{subject}@example.com>")),
            attachments: Vec::new(),
            flags: crate::types::MessageFlags::seen(true),
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

        let (path, selector) = create_draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: false },
            None,
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
            content.contains(&format!("id: \"{}\"", indexed.id)),
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

        let (path, _) = create_draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: true },
            None,
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

        let (path, selector) = create_draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Forward,
            None,
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

        let (path, selector) = create_draft_from_source(
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
            None,
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
        let (path, selector) = create_draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: false },
            None,
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

        let (path, selector) = create_draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Forward,
            None,
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
/// The send tests run against [`crate::send::send_draft`], which is the one
/// implementation `mp send`, `mp send-approved` and this key all reach
/// (#0058), so what they pin holds for the CLI too. They are offline by
/// construction. Three halves of the contract need no server and are exactly
/// the ones worth pinning: a draft that is not approved is refused before
/// anything is enqueued, a submission that reaches nobody still leaves the
/// durable record the outbox exists for with the draft file untouched, and a
/// context naming no transport at all refuses before the draft is read.
#[cfg(test)]
mod store_backed_mutations {
    use super::store_backed_drafts::{fixture_email, Fixture};
    use super::*;
    use crate::draft::mark_draft_sent;
    use crate::tui::app::EmailEntry;

    /// An account that submits to a closed port: the SMTP conversation fails
    /// on connect, deterministically and without a network.
    fn dead_smtp_ctx() -> crate::send::SendContext {
        crate::send::SendContext {
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
            skip: None,
            from: String::new(),
            to: "alice@example.com".to_string(),
            cc: None,
            reply_to: None,
            bcc: None,
            subject: "Re: Hello".to_string(),
            status: "draft".to_string(),
            date_display: "2026-07-01".to_string(),
            date_sort: "2026-07-01T00:00:00".to_string(),
            has_attachments: false,
            read: true,
            answered: false,
            forwarded: false,
            flagged: false,
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
        let (path, selector) = create_draft_from_source(
            "alice",
            "me@example.com",
            &source,
            DraftFromSource::Reply { all: false },
            None,
            None,
        )
        .unwrap();
        let id = fx.resolve(&selector).id;
        (path, selector, id)
    }

    fn outbox_counts(fx: &Fixture) -> crate::outbox::OutboxCounts {
        crate::outbox::counts(&fx.store(), "alice").unwrap()
    }

    /// The TUI send preamble (#0089): a draft that fails validation keeps its
    /// `draft` status. Approval must never persist off a send that was
    /// refused, or a later send would skip the approve-and-send warning the
    /// confirm dialog shows for an unapproved draft.
    #[test]
    fn a_draft_that_fails_validation_is_not_marked_approved() {
        let fx = Fixture::new();
        let (path, _selector, _id) = a_draft(&fx);
        // Blank the recipients: validate_draft refuses a draft whose to, cc
        // and bcc are all empty.
        let text = std::fs::read_to_string(&path).unwrap();
        let unaddressed = text.replacen("\nto:", "\nto: \"\"\nx-to:", 1);
        std::fs::write(&path, &unaddressed).unwrap();

        let err = validate_then_approve(&path).unwrap_err();
        assert!(format!("{err:#}").contains("No recipients"), "{err:#}");
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("status: draft"),
            "a refused send must not persist an approved flag"
        );

        // With the recipients restored the same preamble approves the draft.
        std::fs::write(&path, &text).unwrap();
        validate_then_approve(&path).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("status: approved"));
    }

    /// Regression: `validate_then_approve` must hand back the *approved*
    /// draft, not the copy it parsed before the approval write.
    ///
    /// `mark_as_approved` rewrites `status:` in the file only, so returning
    /// the pre-approval struct gave [`Action::Send`] a value still reading
    /// `status: draft`; `build_draft_message` reads that in-memory status and
    /// refused every `x` approve-and-send with "Email not approved for
    /// sending. Current status: draft", even though the file on disk was
    /// approved. The two assertions are the two halves of that bug: the
    /// returned status, and the send actually reaching the outbox.
    #[test]
    fn approve_and_send_returns_the_approved_draft_and_reaches_the_outbox() {
        let fx = Fixture::new();
        let (path, _selector, _id) = a_draft(&fx);

        let draft = validate_then_approve(&path).unwrap();
        assert_eq!(
            draft.frontmatter.status,
            crate::types::EmailStatus::Approved,
            "the returned draft must carry the approval that was just written"
        );

        // The transport is dead, so the send fails at submission -- but it
        // fails *after* the outbox commit, which is only reachable once
        // `build_draft_message` accepts the status. A refusal would leave the
        // outbox empty (see the unapproved-draft test above).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let sent = rt
            .block_on(crate::send::send_draft(&draft, &dead_smtp_ctx()))
            .unwrap();
        assert!(
            sent.report.row_id.is_some(),
            "an approved draft must reach the outbox instead of being refused"
        );
        assert_eq!(outbox_counts(&fx).total(), 1);
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
        let err = match rt.block_on(crate::send::send_draft(&draft, &dead_smtp_ctx())) {
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
        let sent = rt
            .block_on(crate::send::send_draft(&draft, &dead_smtp_ctx()))
            .unwrap();
        let report = sent.report;

        assert!(!report.send_result.any_succeeded());
        assert!(
            sent.settle_error.is_none(),
            "nothing was sent, so nothing was retired"
        );
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

    /// A context that names neither transport is a configuration error, and it
    /// is caught before the outbox hears about the draft: the same refusal the
    /// TUI shows when `resolve_send_account` finds no SMTP and no Graph.
    #[test]
    fn a_context_with_no_transport_refuses_before_anything_is_enqueued() {
        let fx = Fixture::new();
        let (path, _selector, _id) = a_draft(&fx);
        crate::draft::mark_as_approved(&path).unwrap();
        let draft = crate::draft::parse_email_draft(&path).unwrap();
        let ctx = crate::send::SendContext {
            smtp: None,
            ..dead_smtp_ctx()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(crate::send::send_draft(&draft, &ctx)) {
            Ok(_) => panic!("a draft with no transport must not be sent"),
            Err(e) => e,
        };

        assert_eq!(err.to_string(), "SMTP not configured");
        assert_eq!(outbox_counts(&fx).total(), 0, "nothing was enqueued");
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

    /// `$EDITOR` opens the file the index holds for the row under the cursor.
    ///
    /// A received row never reaches this resolver: it is materialised out of
    /// the store instead (#0075, covered in `store_backed_files`), and the
    /// decline below is what is left for an entry that is neither.
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

        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.emails = std::sync::Arc::new(vec![received_entry()]);
        app.rebuild_visible();
        assert_eq!(
            cursor_draft(&mut app, "Open in $EDITOR needs a message or a draft"),
            None
        );
        let status = app.status_message.clone().unwrap();
        assert_eq!(status, "Open in $EDITOR needs a message or a draft");
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
            skip: None,
            from: row.from.clone().unwrap_or_default(),
            to: row.to.clone().unwrap_or_default(),
            cc: None,
            reply_to: None,
            bcc: None,
            subject: row.subject.clone().unwrap_or_default(),
            status: "inbox".to_string(),
            date_display: row.date_display.clone().unwrap_or_default(),
            date_sort: String::new(),
            has_attachments: true,
            read: true,
            answered: false,
            forwarded: false,
            flagged: false,
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
        // The name `mp open` uses, spelled out rather than read back off the
        // helper: it is the CLI/TUI parity this test exists to pin.
        let expected = crate::parse::test_temp_root().join(format!("mailypoppins-{}", row.id));
        assert_eq!(files[0].parent().unwrap(), expected);
        // And it is private to this user (0700), because `$TMPDIR` is not.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&expected).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        }
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

    /// A row that is neither a message nor an indexed draft (a parse-skipped
    /// draft file, a server-search hit that resolved to nothing) has no
    /// attachments to reach, and the status line says so.
    #[test]
    fn a_row_with_no_identity_declines_the_attachment_key() {
        let _fx = Fixture::new();
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.emails = std::sync::Arc::new(vec![draft_entry(None)]);
        app.rebuild_visible();

        assert_eq!(cursor_attachment_files(&mut app), None);
        assert_eq!(
            app.status_message.clone().unwrap(),
            "Attachments needs a message or a readable draft; this row has neither"
        );
    }

    /// A list entry for a draft, or (with `None`) for a row that carries no
    /// identity at all.
    fn draft_entry(draft_id: Option<&str>) -> EmailEntry {
        EmailEntry {
            msg: None,
            draft_id: draft_id.map(str::to_string),
            skip: None,
            from: String::new(),
            to: "alice@example.com".to_string(),
            cc: None,
            reply_to: None,
            bcc: None,
            subject: "Re: Hello".to_string(),
            status: "draft".to_string(),
            date_display: "2026-07-01".to_string(),
            date_sort: "2026-07-01T00:00:00".to_string(),
            has_attachments: false,
            read: true,
            answered: false,
            forwarded: false,
            flagged: false,
            is_invite: false,
        }
    }

    /// Write a draft naming `attachments` and index it, returning the app with
    /// the cursor on it.
    fn app_on_draft(fx: &Fixture, attachments: &[String]) -> App {
        let dir = crate::config::drafts_dir("alice");
        std::fs::create_dir_all(&dir).unwrap();
        let listed: String = attachments.iter().map(|a| format!("  - \"{a}\"\n")).collect();
        let body = if listed.is_empty() {
            "attachments:\n".to_string()
        } else {
            format!("attachments:\n{listed}")
        };
        std::fs::write(
            dir.join("note.md"),
            format!("---\nto: bob@example.com\nsubject: Files\nstatus: draft\n{body}---\n\nBody\n"),
        )
        .unwrap();
        let store = fx.store();
        let rows = crate::store::drafts::refresh(&store, "alice", &dir).unwrap();

        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.emails = std::sync::Arc::new(vec![draft_entry(Some(&rows[0].id))]);
        app.rebuild_visible();
        app
    }

    /// `o` on a draft opens the files the draft names (#0016): the real paths,
    /// not a temp copy, because those are the bytes that will be sent.
    #[test]
    fn a_draft_answers_the_attachment_key_from_its_own_frontmatter() {
        let fx = Fixture::new();
        let files_dir = crate::config::account_dir("alice").join("files");
        std::fs::create_dir_all(&files_dir).unwrap();
        let one = files_dir.join("report.pdf");
        let two = files_dir.join("notes.txt");
        std::fs::write(&one, b"%PDF-1.4").unwrap();
        std::fs::write(&two, b"notes").unwrap();

        let listed = vec![one.display().to_string(), two.display().to_string()];
        let mut app = app_on_draft(&fx, &listed);

        assert_eq!(cursor_attachment_files(&mut app), Some(vec![one, two]));
        assert!(app.status_message.is_none(), "{:?}", app.status_message);
    }

    /// A draft with no `attachments:` is not an error: the empty list is what
    /// the picker turns into "No attachments", the same as a bare message.
    #[test]
    fn a_draft_without_attachments_resolves_to_an_empty_list() {
        let fx = Fixture::new();
        let mut app = app_on_draft(&fx, &[]);

        assert_eq!(cursor_attachment_files(&mut app), Some(Vec::new()));
        assert!(app.status_message.is_none(), "{:?}", app.status_message);
    }

    /// A path that is no longer there is named, not skipped: a stale entry is
    /// exactly what `o` is pressed to find out about before `mp send` hits it.
    #[test]
    fn a_missing_draft_attachment_is_named_on_the_status_line() {
        let fx = Fixture::new();
        let files_dir = crate::config::account_dir("alice").join("files");
        std::fs::create_dir_all(&files_dir).unwrap();
        let present = files_dir.join("here.txt");
        std::fs::write(&present, b"here").unwrap();
        let gone = files_dir.join("gone.txt").display().to_string();

        let mut app = app_on_draft(&fx, &[present.display().to_string(), gone.clone()]);
        assert_eq!(cursor_attachment_files(&mut app), Some(vec![present]));
        let status = app.status_message.clone().unwrap();
        assert!(status.starts_with("1 attachment missing: "), "{status}");
        assert!(status.contains(&gone), "{status}");

        // Every path gone is a failure, not an empty picker saying "none".
        let mut app = app_on_draft(&fx, std::slice::from_ref(&gone));
        assert_eq!(cursor_attachment_files(&mut app), None);
        assert!(app.status_message.clone().unwrap().contains(&gone));
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

    /// `e` on a received row writes the store's own rendition of the message
    /// where the browser rendition and the invite source go, names it after
    /// the subject so the editor's buffer title is recognisable, and makes it
    /// unwritable (#0075).
    #[test]
    fn the_read_only_view_lands_beside_the_other_renditions() {
        let fx = Fixture::new();
        let mut email = fixture_email("Quarterly Report: Q3");
        email.has_attachments = true;
        email.attachments = vec![attachment("report.pdf", b"%PDF-1.4")];
        let row = fx.ingest(&email);
        let mut app = app_on_row(&row);

        let path = readonly_view_for_row(&mut app, row.id).unwrap();

        assert_eq!(
            path,
            crate::parse::test_temp_root()
                .join(format!("mailypoppins-{}", row.id))
                .join("render")
                .join("quarterly-report-q3.md"),
            "the subject names the buffer, under the per-row render directory"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("---\n"), "{content}");
        assert!(content.contains("subject: 'Quarterly Report: Q3'\n"), "{content}");
        assert!(content.contains("mailbox: inbox\n"), "{content}");
        assert!(content.contains("read: true\n"), "{content}");
        assert!(content.contains("answered: false\n"), "{content}");
        assert!(content.contains("forwarded: false\n"), "{content}");
        assert!(content.contains("- report.pdf\n"), "{content}");
        assert!(content.ends_with("Original body\n"), "{content}");

        // 0444: the editor opens the buffer read-only and says so, instead of
        // letting anyone believe a save reaches the message.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o444, "{mode:o}");
        }

        // A second open of the same row rebuilds the file rather than failing
        // on the mode the first one left, and the copy is scratch: removing it
        // is what the action does when the editor exits.
        let again = readonly_view_for_row(&mut app, row.id).unwrap();
        assert_eq!(again, path);
        std::fs::remove_file(&path).unwrap();
        assert!(!path.exists());
    }

    /// The rendition is scratch whichever way the editor session ends: a
    /// clean exit and an editor that failed both come back with no 0444 file
    /// left on disk (#0075).
    #[test]
    fn the_read_only_view_is_discarded_however_the_editor_exits() {
        let fx = Fixture::new();
        let row = fx.ingest(&fixture_email("Quarterly Report"));
        let mut app = app_on_row(&row);

        let path = readonly_view_for_row(&mut app, row.id).unwrap();
        assert!(path.exists());
        finish_readonly_view(&mut app, &path, Ok(()));
        assert!(!path.exists(), "a clean exit takes the rendition with it");
        assert_eq!(
            app.status_message.as_deref(),
            Some("Returned from the read-only copy (edits do not reach the message)")
        );

        // An editor that never launched, or exited non-zero, leaves nothing
        // behind either -- the status line is the only difference.
        let path = readonly_view_for_row(&mut app, row.id).unwrap();
        assert!(path.exists());
        finish_readonly_view(&mut app, &path, Err(anyhow::anyhow!("editor exited with 1")));
        assert!(
            !path.exists(),
            "a failed editor takes the rendition with it too"
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("Open failed: editor exited with 1")
        );
    }

    /// A message with no subject still gets a name a human can read, and one
    /// with no store row is a status line rather than an empty buffer.
    #[test]
    fn the_read_only_view_names_a_subjectless_message_by_its_row() {
        let fx = Fixture::new();
        let mut email = fixture_email("placeholder");
        email.subject = String::new();
        let row = fx.ingest(&email);
        let mut app = app_on_row(&row);

        let path = readonly_view_for_row(&mut app, row.id).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("message-{}.md", row.id)
        );

        assert_eq!(readonly_view_for_row(&mut app, row.id + 999), None);
        assert_eq!(
            app.status_message.as_deref(),
            Some("Open failed: that message is no longer in the store")
        );
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
