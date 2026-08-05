use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::Result;
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
// The TUI mutation half (#0052)
// ---------------------------------------------------------------------------

/// The status line an action shows while its TUI flow has not been ported.
///
/// The mutations moved onto the store with #0038 scope item 7 and the naming
/// half landed with #0050, which is what these used to wait on. What is left
/// is the TUI flows that still address a message as a file: a draft to edit,
/// a file to hand `$EDITOR`, an attachment to write somewhere, a quote to
/// build. Their CLI counterparts already run on the store and the selector,
/// and [#0052] is the ticket that ports the TUI onto the same substrate.
///
/// `what` names the operation ("Reply", "Attachments"). The message tells the
/// user both halves of the truth: this build will do it soon, and there is a
/// working way to do it right now.
pub(super) fn needs_tui_mutation_half(what: &str) -> String {
    format!("{what} lands with the TUI mutation half (#0052); mp-legacy is the working fallback meanwhile")
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

/// The file behind an indexed draft id, or `None` with the status line saying
/// the index no longer holds it.
///
/// [`crate::store::Store::open`] rather than `open_store`, for the reason the
/// Drafts mailbox load gives: drafts are local-only files, so an account that
/// has never synced has no store file and still has drafts.
fn indexed_draft_path(app: &mut App, id: &str) -> Option<PathBuf> {
    let account = app.account_config.name.clone();
    let store = match crate::store::Store::open(crate::config::store_path(&account)) {
        Ok(store) => store,
        Err(e) => {
            app.set_status_level(
                format!("No drafts index for {account}: {e:#}"),
                StatusLevel::Error,
            );
            return None;
        }
    };
    match crate::store::drafts::find(&store, &account, id) {
        Ok(Some(row)) => Some(row.path),
        Ok(None) => {
            app.set_status_level(
                format!("That draft is no longer in the index ({id})"),
                StatusLevel::Error,
            );
            None
        }
        Err(e) => {
            app.set_status_level(
                format!("Reading the drafts index failed: {e:#}"),
                StatusLevel::Error,
            );
            None
        }
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
            // Opening a message in `$EDITOR` means handing it a file, which
            // `mp edit <selector>` already does; the TUI flow lands with #0052.
            app.set_status_level(needs_tui_mutation_half("Open"), StatusLevel::Warning);
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
            app.set_status_level(needs_tui_mutation_half("Send"), StatusLevel::Warning);
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
            app.set_status_level(needs_tui_mutation_half("Approve"), StatusLevel::Warning);
        }
        Action::BatchApprove(_msgs) => {
            app.set_status_level(needs_tui_mutation_half("Approve"), StatusLevel::Warning);
        }
        Action::MarkDraft => {
            app.set_status_level(needs_tui_mutation_half("Mark-draft"), StatusLevel::Warning);
        }
        Action::BatchMarkDraft(_msgs) => {
            app.set_status_level(needs_tui_mutation_half("Mark-draft"), StatusLevel::Warning);
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

        Action::OpenEventSource { msg: _ } => {
            app.set_status_level(needs_tui_mutation_half("Open"), StatusLevel::Warning);
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
/// Four of the five address the hit as a file (an editor, a browser rendition,
/// a draft), which is #0052's ground. The fifth is a mutation, and it runs on
/// the store like every other one, for the hits that resolved to a row: a hit
/// the account has never synced has nothing local to archive, and says so.
fn handle_search_result_action(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: Action,
    bg_tx: &mpsc::Sender<BgResult>,
) -> Result<()> {
    match action {
        Action::SearchResultOpen => {
            app.set_status_level(needs_tui_mutation_half("Open"), StatusLevel::Warning);
        }

        Action::SearchResultOpenInBrowser => {
            app.set_status_level(
                needs_tui_mutation_half("Open in browser"),
                StatusLevel::Warning,
            );
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

    /// The decline message names both the future (the TUI mutation half) and
    /// the present (mp-legacy), so a user who hits it knows what to do now.
    ///
    /// The ticket it names must be an open one: #0038 and #0050 have landed,
    /// and a message pointing at a done ticket tells the user to wait for
    /// something that already happened. The operation is one that still
    /// declines, because the ones #0052 has landed no longer show this line
    /// at all.
    #[test]
    fn the_decline_message_points_at_the_working_fallback() {
        let msg = needs_tui_mutation_half("Send");
        assert!(msg.starts_with("Send "), "{msg}");
        assert!(msg.contains("#0052"), "{msg}");
        assert!(!msg.contains("#0050"), "{msg}");
        assert!(msg.contains("mp-legacy"), "{msg}");
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
    struct Fixture {
        _dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl Fixture {
        fn new() -> Self {
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

        fn store(&self) -> Store {
            Store::open(crate::config::store_path("alice")).unwrap()
        }

        /// Ingest one message and hand back the row the list would show.
        fn ingest(&self, email: &FetchedEmail) -> MessageRow {
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
        fn source(&self, row: &MessageRow, with_attachments: bool) -> SourceMessage {
            let store = self.store();
            let blobs = BlobStore::for_account("alice");
            crate::draft::source_from_row(&store, &blobs, row, with_attachments).unwrap()
        }

        /// Resolve a selector through the drafts index, exactly as
        /// `mp send <selector>` would after reading it off the status line.
        fn resolve(&self, selector: &Selector) -> crate::store::drafts::DraftRow {
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

    fn fixture_email(subject: &str) -> FetchedEmail {
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
