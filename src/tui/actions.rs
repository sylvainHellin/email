use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::Result;
use ratatui::{backend::CrosstermBackend, Terminal};

use super::app::{
    Action, App, BgResult, ComposeField, ComposeMode, ComposeWizard, Focus, MailboxKind,
    MessageRef, Overlay, StatusLevel,
};
use super::helpers::{
    edit_file, ensure_search_result_saved, lib_do_multi_search_graph, lib_do_sync_graph,
    resolve_send_account, resume_terminal, suspend_terminal,
};
use super::ui;

use crate::draft::{
    create_forward_draft, create_reply_draft, find_drafts, mark_as_approved, mark_as_draft,
    new_draft_skeleton, parse_email_draft,
    mark_draft_sent, validate_draft,
};
use crate::imap_client::{
    archive_email_locally, batch_archive_emails_locally,
    batch_delete_emails_locally, delete_email_locally, get_message_id_from_file,
    mark_read_on_server, mark_unread_on_server, move_email_locally,
    update_read_status_locally,
};
use crate::types::EmailStatus;

// ---------------------------------------------------------------------------
// The `.md` bridge (#0038 unit A, temporary)
// ---------------------------------------------------------------------------

/// Resolve a [`MessageRef`] to the `.md` file the mutation paths still need,
/// which is never possible: it always returns `None`.
///
/// #0037 stopped writing the `.md` tree and #0038 moved the read path onto the
/// store, but the mutation paths (edit, reply, forward, send, approve,
/// archive, delete, move, flag, RSVP, attachments) still take a file path all
/// the way down into `draft.rs`, `imap_client` and `graph.rs`. Rewriting them
/// onto the store is #0038 scope item 7, and that item is the owner that
/// deletes this function together with every `let Some(path) = ... else` guard
/// that calls it. Nothing else may add a caller, and nothing may make it
/// return `Some`: a resurrected `.md` path would be as wrong as a store miss.
///
/// The reason it exists at all rather than the arms being deleted: the arms
/// are the specification of what item 7 has to reproduce, and deleting them
/// would lose that. Guarded by
/// `the_file_bridge_never_resolves_a_path`.
pub(super) fn message_path(_msg: MessageRef) -> Option<PathBuf> {
    None
}

/// The status line a mutation shows when [`message_path`] declined.
///
/// `what` names the operation ("Reply", "Archive"). The message tells the user
/// both halves of the truth: this build will do it soon, and there is a
/// working way to do it right now.
pub(super) fn store_backed_soon(what: &str) -> String {
    format!("{what} is store-backed soon (#0038); mp-legacy is the working fallback meanwhile")
}

pub(super) fn handle_action(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: Action,
    bg_tx: &mpsc::Sender<BgResult>,
) -> Result<()> {
    match action {
        Action::EditCurrent => {
            if let Some(email) = app.selected_email() {
                let msg = email.msg;
                let was_unread = !email.read;
                let Some(path) = msg.and_then(message_path) else {
                    app.set_status_level(store_backed_soon("Open"), StatusLevel::Warning);
                    return Ok(());
                };
                suspend_terminal(terminal)?;
                let result = edit_file(&path);
                resume_terminal(terminal)?;
                match result {
                    Ok(()) => app.set_status("Returned from editor".to_string()),
                    Err(e) => app.set_status_level(format!("Edit failed: {e}"), StatusLevel::Error),
                }
                // Auto-mark as read after opening in editor. Queued
                // BEFORE the reload so the read-flag file write happens
                // before the background mailbox walk spawns -- otherwise
                // the walk could read the file pre-write and the fresh
                // list would briefly show the email as unread again.
                if was_unread {
                    app.push_action(Action::MarkAsRead);
                }
                app.reload_current_mailbox();
            }
        }

        Action::Reply(reply_all) => {
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Reply"), StatusLevel::Warning);
                    return Ok(());
                };
                let default_from = app
                    .smtp_config
                    .as_ref()
                    .map(|s| s.default_from.clone())
                    .unwrap_or_else(|| app.account_config.default_from.clone());
                let drafts_dir = app.drafts_dir.clone();
                match create_reply_draft(&path, reply_all, &default_from, drafts_dir.as_deref()) {
                    Ok(draft_path) => {
                        suspend_terminal(terminal)?;
                        let _ = edit_file(&draft_path);
                        resume_terminal(terminal)?;
                        app.set_status("Reply draft ready".to_string());
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                            app.invalidate_cache_idx(idx);
                        }
                    }
                    Err(e) => {
                        app.set_status_level(format!("Reply failed: {e}"), StatusLevel::Error)
                    }
                }
                app.reload_current_mailbox();
            }
        }

        Action::Forward => {
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Forward"), StatusLevel::Warning);
                    return Ok(());
                };
                let default_from = app
                    .smtp_config
                    .as_ref()
                    .map(|s| s.default_from.clone())
                    .unwrap_or_else(|| app.account_config.default_from.clone());
                let drafts_dir = app.drafts_dir.clone();
                match create_forward_draft(&path, &default_from, drafts_dir.as_deref()) {
                    Ok(draft_path) => {
                        suspend_terminal(terminal)?;
                        let _ = edit_file(&draft_path);
                        resume_terminal(terminal)?;
                        app.set_status("Forward draft ready".to_string());
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                            app.invalidate_cache_idx(idx);
                        }
                    }
                    Err(e) => {
                        app.set_status_level(format!("Forward failed: {e}"), StatusLevel::Error)
                    }
                }
                app.reload_current_mailbox();
            }
        }

        Action::Send => {
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Send"), StatusLevel::Warning);
                    return Ok(());
                };
                let (acct_idx, smtp_config, _imap_config, graph_config, account_config, signature) =
                    resolve_send_account(app, &path);

                if graph_config.is_some()
                    && account_config.auth_method == crate::config::AuthMethod::Graph
                {
                    let graph_config = graph_config.unwrap();
                    let email_settings = app.global_config.email.clone();

                    app.bg_count += 1;
                    app.set_status_level(
                        "Sending via Graph...".to_string(),
                        StatusLevel::Progress,
                    );
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = (|| -> anyhow::Result<String> {
                            let draft = parse_email_draft(&path)?;
                            validate_draft(&draft)?;

                            let to = parse_graph_recipients(draft.frontmatter.to.as_deref());
                            let cc = parse_graph_recipients(draft.frontmatter.cc.as_deref());
                            let bcc = parse_graph_recipients(draft.frontmatter.bcc.as_deref());
                            let to_refs: Vec<(&str, &str)> =
                                to.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
                            let cc_refs: Vec<(&str, &str)> =
                                cc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
                            let bcc_refs: Vec<(&str, &str)> =
                                bcc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();

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
                                    let content_type =
                                        mime_guess::from_path(p).first_or_octet_stream().to_string();
                                    att_data.push((filename, content, content_type));
                                }
                            }

                            let client = rt
                                .block_on(crate::graph::GraphClient::new_async(&graph_config))?;
                            let built = crate::send::build_draft_message(
                                &draft,
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
                                anyhow::bail!(
                                    "{}",
                                    report
                                        .send_result
                                        .failed()
                                        .first()
                                        .and_then(|r| r.error.clone())
                                        .unwrap_or_else(|| "Graph send failed".to_string())
                                );
                            }

                            mark_draft_sent(&draft, Some(&built.message_id))?;
                            crate::contacts::hooks::bump_after_send(&account_config, &draft);

                            Ok(format!(
                                "Sent via Graph to {} recipient(s) [{}]",
                                to.len() + cc.len() + bcc.len(),
                                report.status_line()
                            ))
                        })();
                        let _ = tx.send(BgResult::Send {
                            account_index: acct_idx,
                            result: result.map_err(|e| e.to_string()),
                        });
                    });
                } else {
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
                    let email_settings = app.global_config.email.clone();

                    app.bg_count += 1;
                    app.set_status_level("Sending...".to_string(), StatusLevel::Progress);
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = (|| -> anyhow::Result<String> {
                            let draft = parse_email_draft(&path)?;
                            validate_draft(&draft)?;

                            let built = crate::send::build_draft_message(
                                &draft,
                                &smtp_config.default_from,
                                &email_settings,
                                signature.as_deref(),
                                None,
                            )?;
                            let report = rt.block_on(crate::send::send_durably(
                                &built,
                                &account_config,
                                &smtp_config,
                            ))?;
                            let send_result = &report.send_result;

                            if send_result.any_succeeded() {
                                mark_draft_sent(&draft, Some(&built.message_id))?;
                                crate::contacts::hooks::bump_after_send(&account_config, &draft);
                                if send_result.all_succeeded() {
                                    Ok(format!(
                                        "Sent to {} recipient(s) [{}]",
                                        send_result.results.len(),
                                        report.status_line()
                                    ))
                                } else {
                                    let failed: Vec<String> = send_result
                                        .failed()
                                        .iter()
                                        .map(|r| r.address.clone())
                                        .collect();
                                    Ok(format!(
                                        "Partial: {}/{} succeeded [{}] -- failed: {}",
                                        send_result.succeeded().len(),
                                        send_result.results.len(),
                                        report.status_line(),
                                        failed.join(", ")
                                    ))
                                }
                            } else {
                                anyhow::bail!(
                                    "Failed to send to all {} recipient(s) [{}]",
                                    send_result.results.len(),
                                    report.status_line()
                                )
                            }
                        })();
                        let _ = tx.send(BgResult::Send {
                            account_index: acct_idx,
                            result: result.map_err(|e| e.to_string()),
                        });
                    });
                }
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
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Approve"), StatusLevel::Warning);
                    return Ok(());
                };
                match mark_as_approved(&path) {
                    Ok(msg) => {
                        app.set_status(msg);
                        app.reload_current_mailbox();
                    }
                    Err(e) => {
                        app.set_status_level(format!("Approve failed: {e}"), StatusLevel::Error)
                    }
                }
            }
        }

        Action::BatchApprove(msgs) => {
            let total = msgs.len();
            let mut succeeded = 0usize;
            let mut failed = 0usize;
            for msg in &msgs {
                let Some(path) = message_path(*msg) else {
                    app.set_status_level(store_backed_soon("Approve"), StatusLevel::Warning);
                    return Ok(());
                };
                match mark_as_approved(&path) {
                    Ok(_) => succeeded += 1,
                    Err(e) => {
                        log::warn!("Approve failed for {msg}: {e}");
                        failed += 1;
                    }
                }
            }
            if failed == 0 {
                app.set_status(format!("Approved {} drafts", succeeded));
            } else {
                app.set_status_level(
                    format!("Approved {}/{} drafts ({} failed)", succeeded, total, failed),
                    StatusLevel::Warning,
                );
            }
            app.selection.clear();
            app.reload_current_mailbox();
        }

        Action::MarkDraft => {
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Mark-draft"), StatusLevel::Warning);
                    return Ok(());
                };
                match mark_as_draft(&path) {
                    Ok(msg) => {
                        app.set_status(msg);
                        app.reload_current_mailbox();
                    }
                    Err(e) => app
                        .set_status_level(format!("Mark-draft failed: {e}"), StatusLevel::Error),
                }
            }
        }

        Action::BatchMarkDraft(msgs) => {
            let total = msgs.len();
            let mut succeeded = 0usize;
            let mut failed = 0usize;
            for msg in &msgs {
                let Some(path) = message_path(*msg) else {
                    app.set_status_level(store_backed_soon("Mark-draft"), StatusLevel::Warning);
                    return Ok(());
                };
                match mark_as_draft(&path) {
                    Ok(_) => succeeded += 1,
                    Err(e) => {
                        log::warn!("Mark-draft failed for {msg}: {e}");
                        failed += 1;
                    }
                }
            }
            if failed == 0 {
                app.set_status(format!("Marked {} as draft", succeeded));
            } else {
                app.set_status_level(
                    format!("Marked {}/{} as draft ({} failed)", succeeded, total, failed),
                    StatusLevel::Warning,
                );
            }
            app.selection.clear();
            app.reload_current_mailbox();
        }

        Action::Archive => {
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Archive"), StatusLevel::Warning);
                    return Ok(());
                };
                let archive_dir = match app.archive_dir.clone() {
                    Some(d) => d,
                    None => {
                        app.set_status_level(
                            "Archive directory not configured".to_string(),
                            StatusLevel::Error,
                        );
                        return Ok(());
                    }
                };
                let archive_server_name = app.archive_server_name.clone();

                if app.is_graph() {
                    let graph_config = app.graph_config.clone().unwrap();

                    app.remove_selected_from_list();
                    app.bg_count += 1;
                    app.bg_mutations += 1;
                    app.set_status_level("Archiving...".to_string(), StatusLevel::Progress);
                    terminal.draw(|frame| ui::view(app, frame))?;
                    let acct_idx = app.active_account;
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = rt
                            .block_on(crate::graph::archive_email_graph(
                                &graph_config,
                                &archive_dir,
                                &path,
                                &archive_server_name,
                            ))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Archive {
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

                    // The in-memory Message-ID index this used to update is
                    // gone (#0038): the cross-mailbox lookup is an indexed
                    // query now, so a move needs no index maintenance. The
                    // store row itself still moves on the next sync until
                    // #0038 scope item 7 writes it optimistically.
                    app.remove_selected_from_list();
                    app.bg_count += 1;
                    app.bg_mutations += 1;
                    app.set_status_level("Archiving...".to_string(), StatusLevel::Progress);
                    terminal.draw(|frame| ui::view(app, frame))?;
                    let acct_idx = app.active_account;
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = rt
                            .block_on(archive_email_locally(
                                &imap_config,
                                &archive_dir,
                                &path,
                                &archive_server_name,
                            ))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Archive {
                            account_index: acct_idx,
                            result,
                        });
                    });
                }
            }
        }

        Action::Delete => {
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Delete"), StatusLevel::Warning);
                    return Ok(());
                };
                if app.is_graph() {
                    let graph_config = app.graph_config.clone().unwrap();

                    app.remove_selected_from_list();
                    app.bg_count += 1;
                    app.bg_mutations += 1;
                    app.set_status_level("Deleting...".to_string(), StatusLevel::Progress);
                    terminal.draw(|frame| ui::view(app, frame))?;
                    let acct_idx = app.active_account;
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = rt
                            .block_on(crate::graph::delete_email_graph(&graph_config, &path))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Delete {
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

                    app.remove_selected_from_list();
                    app.bg_count += 1;
                    app.bg_mutations += 1;
                    app.set_status_level("Deleting...".to_string(), StatusLevel::Progress);
                    terminal.draw(|frame| ui::view(app, frame))?;
                    let acct_idx = app.active_account;
                    let tx = bg_tx.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = rt
                            .block_on(delete_email_locally(&imap_config, &path))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Delete {
                            account_index: acct_idx,
                            result,
                        });
                    });
                }
            }
        }

        Action::BatchArchive(msgs) => {
            let Some(paths) = msgs.iter().map(|m| message_path(*m)).collect::<Option<Vec<_>>>()
            else {
                app.set_status_level(store_backed_soon("Archive"), StatusLevel::Warning);
                return Ok(());
            };
            let archive_dir = match app.archive_dir.clone() {
                Some(d) => d,
                None => {
                    app.set_status_level(
                        "Archive directory not configured".to_string(),
                        StatusLevel::Error,
                    );
                    return Ok(());
                }
            };
            let archive_server_name = app.archive_server_name.clone();

            let msg_set: HashSet<MessageRef> = msgs.iter().copied().collect();
            app.remove_selected_from_list_batch(&msg_set);

            let count = paths.len();
            app.bg_count += count;
            app.bg_mutations += count;
            app.set_status_level(
                format!("Archiving {} emails...", count),
                StatusLevel::Progress,
            );
            terminal.draw(|frame| ui::view(app, frame))?;

            let acct_idx = app.active_account;
            let tx = bg_tx.clone();

            if app.is_graph() {
                let graph_config = app.graph_config.clone().unwrap();
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    for path in &paths {
                        let result = rt
                            .block_on(crate::graph::archive_email_graph(
                                &graph_config,
                                &archive_dir,
                                path,
                                &archive_server_name,
                            ))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Archive {
                            account_index: acct_idx,
                            result,
                        });
                    }
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
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let results = rt.block_on(batch_archive_emails_locally(
                        &imap_config,
                        &archive_dir,
                        &paths,
                        &archive_server_name,
                    ));
                    for (_path, result) in results {
                        let _ = tx.send(BgResult::Archive {
                            account_index: acct_idx,
                            result: result.map(|()| String::new()).map_err(|e| e.to_string()),
                        });
                    }
                });
            }
        }

        Action::BatchDelete(msgs) => {
            let Some(paths) = msgs.iter().map(|m| message_path(*m)).collect::<Option<Vec<_>>>()
            else {
                app.set_status_level(store_backed_soon("Delete"), StatusLevel::Warning);
                return Ok(());
            };
            let msg_set: HashSet<MessageRef> = msgs.iter().copied().collect();
            app.remove_selected_from_list_batch(&msg_set);

            let count = paths.len();
            app.bg_count += count;
            app.bg_mutations += count;
            app.set_status_level(
                format!("Deleting {} emails...", count),
                StatusLevel::Progress,
            );
            terminal.draw(|frame| ui::view(app, frame))?;

            let acct_idx = app.active_account;
            let tx = bg_tx.clone();

            if app.is_graph() {
                let graph_config = app.graph_config.clone().unwrap();
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    for path in &paths {
                        let result = rt
                            .block_on(crate::graph::delete_email_graph(&graph_config, path))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Delete {
                            account_index: acct_idx,
                            result,
                        });
                    }
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
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    let results =
                        rt.block_on(batch_delete_emails_locally(&imap_config, &paths));
                    for (_path, result) in results {
                        let _ = tx.send(BgResult::Delete {
                            account_index: acct_idx,
                            result: result.map(|()| String::new()).map_err(|e| e.to_string()),
                        });
                    }
                });
            }
        }

        Action::MoveToMailbox { msgs, dest_idx } => {
            // Quick-move to an arbitrary mailbox (#0018): generalized
            // archive. Optimistic list removal + async server/local move,
            // rollback handled by move_email_locally (IMAP) and reported
            // via BgResult::Move.
            let (dest_dir, dest_label, dest_kind) = match app.mailboxes.get(dest_idx) {
                Some(mb) => (mb.dir.clone(), mb.label.clone(), mb.kind),
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
            let old_status = super::app::kind_to_status(app.active_kind());
            let new_status = super::app::kind_to_status(dest_kind);

            // Resolve the backend config BEFORE any optimistic mutation
            // (same order as Archive) so a missing config leaves the
            // list and index untouched.
            let imap_config = if app.is_graph() {
                None
            } else {
                match app.imap_config.clone() {
                    Some(c) => Some(c),
                    None => {
                        app.set_status_level(
                            "IMAP not configured".to_string(),
                            StatusLevel::Error,
                        );
                        return Ok(());
                    }
                }
            };

            // The in-memory Message-ID index a move used to re-point is gone
            // (#0038); the lookup is an indexed query over `messages` now.
            let Some(paths) = msgs.iter().map(|m| message_path(*m)).collect::<Option<Vec<_>>>()
            else {
                app.set_status_level(store_backed_soon("Move"), StatusLevel::Warning);
                return Ok(());
            };

            let msg_set: HashSet<MessageRef> = msgs.iter().copied().collect();
            app.remove_selected_from_list_batch(&msg_set);

            let count = paths.len();
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
            let tx = bg_tx.clone();

            if app.is_graph() {
                let graph_config = app.graph_config.clone().unwrap();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create tokio runtime");
                    for path in &paths {
                        let result = rt
                            .block_on(crate::graph::move_email_graph(
                                &graph_config,
                                &dest_dir,
                                path,
                                &dest_server,
                                &old_status,
                                &new_status,
                            ))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Move {
                            account_index: acct_idx,
                            source_mailbox_idx: source_idx,
                            dest_mailbox_idx: dest_idx,
                            dest_label: dest_label.clone(),
                            result,
                        });
                    }
                });
            } else {
                let imap_config = imap_config.expect("checked before optimistic mutation");
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create tokio runtime");
                    for path in &paths {
                        let result = rt
                            .block_on(move_email_locally(
                                &imap_config,
                                &dest_dir,
                                path,
                                &source_server,
                                &dest_server,
                                &old_status,
                                &new_status,
                            ))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Move {
                            account_index: acct_idx,
                            source_mailbox_idx: source_idx,
                            dest_mailbox_idx: dest_idx,
                            dest_label: dest_label.clone(),
                            result,
                        });
                    }
                });
            }
        }

        Action::ToggleRead => {
            if let Some(email) = app.selected_email() {
                let new_read = !email.read;
                let Some(msg) = email.msg else {
                    app.set_status_level(store_backed_soon("Read flag"), StatusLevel::Warning);
                    return Ok(());
                };
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Read flag"), StatusLevel::Warning);
                    return Ok(());
                };
                let message_id = get_message_id_from_file(&path);

                // Optimistic local update (list + shared cache slot).
                update_read_status_locally(&path, new_read).ok();
                app.set_email_read(msg, new_read);

                let label = if new_read {
                    "Marked as read"
                } else {
                    "Marked as unread"
                };
                app.set_status(label.to_string());

                // Async server update
                if let Some(mid) = message_id {
                    if app.is_graph() {
                        let graph_cfg = app.graph_config.clone().unwrap();
                        let acct_idx = app.active_account;
                        app.bg_count += 1;
                        let tx = bg_tx.clone();
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Runtime::new()
                                .expect("failed to create tokio runtime");
                            let result =
                                rt.block_on(crate::graph::mark_read_graph(&graph_cfg, &mid, new_read));
                            let _ = tx.send(BgResult::ToggleRead {
                                account_index: acct_idx,
                                msg,
                                new_read_state: new_read,
                                result: result
                                    .map(|()| String::new())
                                    .map_err(|e| e.to_string()),
                            });
                        });
                    } else if let Some(imap_config) = app.imap_config.clone() {
                        let mailbox = app.active_server_mailbox();
                        let acct_idx = app.active_account;
                        app.bg_count += 1;
                        let tx = bg_tx.clone();
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Runtime::new()
                                .expect("failed to create tokio runtime");
                            let result = if new_read {
                                rt.block_on(mark_read_on_server(&imap_config, &mid, &mailbox))
                            } else {
                                rt.block_on(mark_unread_on_server(&imap_config, &mid, &mailbox))
                            };
                            let _ = tx.send(BgResult::ToggleRead {
                                account_index: acct_idx,
                                msg,
                                new_read_state: new_read,
                                result: result
                                    .map(|()| String::new())
                                    .map_err(|e| e.to_string()),
                            });
                        });
                    }
                }
            }
        }

        Action::MarkAsRead => {
            if let Some(email) = app.selected_email() {
                if email.read {
                    return Ok(());
                }
                let Some(msg) = email.msg else {
                    return Ok(());
                };
                // Silent decline: this is the auto-mark that rides on opening
                // an email, and the open itself already said its piece.
                let Some(path) = message_path(msg) else {
                    return Ok(());
                };
                let message_id = get_message_id_from_file(&path);

                // Optimistic local update (silent; list + shared cache slot).
                update_read_status_locally(&path, true).ok();
                app.set_email_read(msg, true);

                // Async server update (no status message for auto-mark)
                if let Some(mid) = message_id {
                    if app.is_graph() {
                        let graph_cfg = app.graph_config.clone().unwrap();
                        let acct_idx = app.active_account;
                        app.bg_count += 1;
                        let tx = bg_tx.clone();
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Runtime::new()
                                .expect("failed to create tokio runtime");
                            let result = rt.block_on(crate::graph::mark_read_graph(
                                &graph_cfg, &mid, true,
                            ));
                            let _ = tx.send(BgResult::ToggleRead {
                                account_index: acct_idx,
                                msg,
                                new_read_state: true,
                                result: result
                                    .map(|()| String::new())
                                    .map_err(|e| e.to_string()),
                            });
                        });
                    } else if let Some(imap_config) = app.imap_config.clone() {
                        let mailbox = app.active_server_mailbox();
                        let acct_idx = app.active_account;
                        app.bg_count += 1;
                        let tx = bg_tx.clone();
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Runtime::new()
                                .expect("failed to create tokio runtime");
                            let result =
                                rt.block_on(mark_read_on_server(&imap_config, &mid, &mailbox));
                            let _ = tx.send(BgResult::ToggleRead {
                                account_index: acct_idx,
                                msg,
                                new_read_state: true,
                                result: result
                                    .map(|()| String::new())
                                    .map_err(|e| e.to_string()),
                            });
                        });
                    }
                }
            }
        }

        Action::BatchToggleRead(msgs) => {
            let Some(paths) = msgs.iter().map(|m| message_path(*m)).collect::<Option<Vec<_>>>()
            else {
                app.set_status_level(store_backed_soon("Read flag"), StatusLevel::Warning);
                return Ok(());
            };
            let any_unread = msgs
                .iter()
                .any(|m| app.emails.iter().any(|e| e.msg == Some(*m) && !e.read));
            let new_read = any_unread;

            // Optimistic local update (list + shared cache slot).
            for (msg, path) in msgs.iter().zip(&paths) {
                update_read_status_locally(path, new_read).ok();
                app.set_email_read(*msg, new_read);
            }
            app.selection.clear();

            let label = if new_read {
                format!("Marked {} as read", paths.len())
            } else {
                format!("Marked {} as unread", paths.len())
            };
            app.set_status(label);

            // Async server update. The delivered `msg` is the first of the
            // batch: the handler only uses it to roll one row back, which is
            // the pre-existing shape of this arm (it reported one path too).
            let first = msgs.first().copied();
            if app.is_graph() {
                let graph_cfg = app.graph_config.clone().unwrap();
                let acct_idx = app.active_account;
                app.bg_count += 1;
                let tx = bg_tx.clone();
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    for path in &paths {
                        if let Some(mid) = get_message_id_from_file(path) {
                            let result = rt.block_on(crate::graph::mark_read_graph(
                                &graph_cfg, &mid, new_read,
                            ));
                            if let Err(e) = result {
                                log::warn!("Failed to toggle read for {}: {}", mid, e);
                            }
                        }
                    }
                    if let Some(msg) = first {
                        let _ = tx.send(BgResult::ToggleRead {
                            account_index: acct_idx,
                            msg,
                            new_read_state: new_read,
                            result: Ok(String::new()),
                        });
                    }
                });
            } else if let Some(imap_config) = app.imap_config.clone() {
                let mailbox = app.active_server_mailbox();
                let acct_idx = app.active_account;
                app.bg_count += 1;
                let tx = bg_tx.clone();
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
                    for path in &paths {
                        if let Some(mid) = get_message_id_from_file(path) {
                            let result = if new_read {
                                rt.block_on(mark_read_on_server(&imap_config, &mid, &mailbox))
                            } else {
                                rt.block_on(mark_unread_on_server(&imap_config, &mid, &mailbox))
                            };
                            if let Err(e) = result {
                                log::warn!("Failed to toggle read for {}: {}", mid, e);
                            }
                        }
                    }
                    if let Some(msg) = first {
                        let _ = tx.send(BgResult::ToggleRead {
                            account_index: acct_idx,
                            msg,
                            new_read_state: new_read,
                            result: Ok(String::new()),
                        });
                    }
                });
            }
        }

        Action::CopyPath => {
            if let Some(msg) = app.selected_email_ref() {
                let Some(path) = message_path(msg) else {
                    app.set_status_level(store_backed_soon("Copy path"), StatusLevel::Warning);
                    return Ok(());
                };
                match super::helpers::copy_to_clipboard(&path.display().to_string()) {
                    Ok(()) => app.set_status("Path copied to clipboard".to_string()),
                    Err(e) => app.set_status_level(format!("Copy failed: {e}"), StatusLevel::Error),
                }
            }
        }

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
            // The agenda row carries its own message reference (the invite may
            // live in any mailbox of the account), so this does not go through
            // the mail cursor like `Action::EditCurrent`; it shares the same
            // bridge to a file, which #0038 scope item 7 owns.
            let Some(path) = message_path(msg) else {
                app.set_status_level(store_backed_soon("Open"), StatusLevel::Warning);
                return Ok(());
            };
            suspend_terminal(terminal)?;
            let result = edit_file(&path);
            resume_terminal(terminal)?;
            match result {
                Ok(()) => {
                    // The invite may have changed under us, so rebuild the
                    // agenda. `refresh_calendar` sets its own status;
                    // overwriting it here would hide the reloaded count behind
                    // a bare "Returned from editor", so let that one stand.
                    app.refresh_calendar();
                }
                Err(e) => {
                    app.set_status_level(format!("Edit failed: {e}"), StatusLevel::Error)
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
        ComposeMode::Forward { source_path } => {
            let subject = crate::draft::fwd_subject_from_source(source_path)
                .unwrap_or_else(|_| String::from("Fwd: "));
            (String::new(), String::new(), String::new(), subject)
        }
        ComposeMode::EditDraft { source_path } => {
            match crate::draft::parse_email_draft(source_path) {
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

    // Editing an existing draft's recipients/subject rewrites the file in
    // place and does NOT open $EDITOR -- the whole point is a quick,
    // fuzzy-finder edit of the header fields.
    if let ComposeMode::EditDraft { source_path } = &wizard.mode {
        let edit = crate::draft::DraftRecipientEdit {
            to: wizard.to.clone(),
            cc: wizard.cc.clone(),
            bcc: wizard.bcc.clone(),
            subject: wizard.subject.clone(),
        };
        match crate::draft::rewrite_draft_recipients(source_path, &edit) {
            Ok(()) => {
                if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                    app.invalidate_cache_idx(idx);
                }
                app.reload_current_mailbox();
                app.set_status("Recipients updated".to_string());
            }
            Err(e) => {
                app.set_status_level(format!("Recipient update failed: {e}"), StatusLevel::Error);
            }
        }
        return Ok(());
    }

    let draft_result = match &wizard.mode {
        ComposeMode::New => write_new_draft_from_wizard(app, &wizard),
        ComposeMode::Forward { source_path } => {
            write_forward_draft_from_wizard(app, source_path, &wizard)
        }
        ComposeMode::EditDraft { .. } => unreachable!("handled above"),
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

fn write_forward_draft_from_wizard(
    app: &App,
    source_path: &std::path::Path,
    wizard: &ComposeWizard,
) -> Result<PathBuf> {
    let default_from_owned = app
        .smtp_config
        .as_ref()
        .map(|s| s.default_from.clone())
        .unwrap_or_else(|| app.account_config.default_from.clone());
    let default_from = default_from_owned.as_str();
    let drafts_dir = app
        .find_mailbox_by_kind(MailboxKind::Drafts)
        .map(|i| app.mailboxes[i].dir.clone())
        .or_else(|| app.drafts_dir.clone());
    let path = create_forward_draft(source_path, default_from, drafts_dir.as_deref())?;

    // Patch the frontmatter fields in place.
    patch_draft_frontmatter(&path, wizard)?;
    Ok(path)
}

fn patch_draft_frontmatter(path: &std::path::Path, wizard: &ComposeWizard) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut lines = content.lines();

    // Expect the file to start with `---`.
    let Some(first) = lines.next() else {
        return Ok(());
    };
    if first.trim() != "---" {
        return Ok(());
    }

    // Collect frontmatter lines until the closing `---`.
    let mut fm_lines: Vec<String> = Vec::new();
    let mut body_lines: Vec<String> = Vec::new();
    let mut in_body = false;
    for line in lines {
        if !in_body {
            if line.trim() == "---" {
                in_body = true;
                continue;
            }
            fm_lines.push(line.to_string());
        } else {
            body_lines.push(line.to_string());
        }
    }

    // Rewrite to/cc/bcc/subject; leave everything else alone.
    // Simple single-line field rewriter: replace `key:` lines if found,
    // otherwise append them before the closing `---`.
    let mut rewrote_to = false;
    let mut rewrote_cc = false;
    let mut rewrote_bcc = false;
    let mut rewrote_subject = false;
    for line in fm_lines.iter_mut() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("to:") {
            *line = format!("to: {}", yaml_escape(&wizard.to));
            rewrote_to = true;
        } else if trimmed.starts_with("cc:") {
            if wizard.cc.trim().is_empty() {
                *line = "cc:".to_string();
            } else {
                *line = format!("cc: {}", yaml_escape(&wizard.cc));
            }
            rewrote_cc = true;
        } else if trimmed.starts_with("bcc:") {
            if wizard.bcc.trim().is_empty() {
                *line = "bcc:".to_string();
            } else {
                *line = format!("bcc: {}", yaml_escape(&wizard.bcc));
            }
            rewrote_bcc = true;
        } else if trimmed.starts_with("subject:") {
            *line = format!("subject: {}", yaml_escape(&wizard.subject));
            rewrote_subject = true;
        }
    }
    if !rewrote_to {
        fm_lines.push(format!("to: {}", yaml_escape(&wizard.to)));
    }
    if !rewrote_cc && !wizard.cc.trim().is_empty() {
        fm_lines.push(format!("cc: {}", yaml_escape(&wizard.cc)));
    }
    if !rewrote_bcc && !wizard.bcc.trim().is_empty() {
        fm_lines.push(format!("bcc: {}", yaml_escape(&wizard.bcc)));
    }
    if !rewrote_subject {
        fm_lines.push(format!("subject: {}", yaml_escape(&wizard.subject)));
    }

    let mut rebuilt = String::from("---\n");
    for line in fm_lines {
        rebuilt.push_str(&line);
        rebuilt.push('\n');
    }
    rebuilt.push_str("---\n");
    for line in body_lines {
        rebuilt.push_str(&line);
        rebuilt.push('\n');
    }
    std::fs::write(path, rebuilt)?;
    Ok(())
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

fn handle_search_result_action(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: Action,
    bg_tx: &mpsc::Sender<BgResult>,
) -> Result<()> {
    match action {
        Action::SearchResultOpen => {
            if let Some(path) = ensure_search_result_saved(app) {
                suspend_terminal(terminal)?;
                let result = edit_file(&path);
                resume_terminal(terminal)?;
                match result {
                    Ok(()) => app.set_status("Returned from editor".to_string()),
                    Err(e) => app.set_status_level(format!("Edit failed: {e}"), StatusLevel::Error),
                }
            } else {
                app.set_status_level(
                    "Failed to save email locally".to_string(),
                    StatusLevel::Error,
                );
            }
        }

        Action::SearchResultOpenInBrowser => {
            if let Some(path) = ensure_search_result_saved(app) {
                let html_path = path.with_extension("html");
                if html_path.exists() {
                    match crate::parse::open_file_with_system(&html_path) {
                        Ok(()) => app.set_status("Opened in browser".to_string()),
                        Err(e) => {
                            app.set_status_level(format!("Open failed: {e}"), StatusLevel::Error)
                        }
                    }
                } else {
                    app.set_status("No HTML version available".to_string());
                }
            } else {
                app.set_status_level(
                    "Failed to save email locally".to_string(),
                    StatusLevel::Error,
                );
            }
        }

        Action::SearchResultReply(reply_all) => {
            if let Some(path) = ensure_search_result_saved(app) {
                let default_from = app
                    .smtp_config
                    .as_ref()
                    .map(|s| s.default_from.clone())
                    .unwrap_or_else(|| app.account_config.default_from.clone());
                let drafts_dir = app.drafts_dir.clone();
                match create_reply_draft(&path, reply_all, &default_from, drafts_dir.as_deref()) {
                    Ok(draft_path) => {
                        suspend_terminal(terminal)?;
                        let _ = edit_file(&draft_path);
                        resume_terminal(terminal)?;
                        app.set_status("Reply draft ready".to_string());
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                            app.invalidate_cache_idx(idx);
                        }
                    }
                    Err(e) => {
                        app.set_status_level(format!("Reply failed: {e}"), StatusLevel::Error)
                    }
                }
            } else {
                app.set_status_level(
                    "Failed to save email locally".to_string(),
                    StatusLevel::Error,
                );
            }
        }

        Action::SearchResultForward => {
            if let Some(path) = ensure_search_result_saved(app) {
                let default_from = app
                    .smtp_config
                    .as_ref()
                    .map(|s| s.default_from.clone())
                    .unwrap_or_else(|| app.account_config.default_from.clone());
                let drafts_dir = app.drafts_dir.clone();
                match create_forward_draft(&path, &default_from, drafts_dir.as_deref()) {
                    Ok(draft_path) => {
                        suspend_terminal(terminal)?;
                        let _ = edit_file(&draft_path);
                        resume_terminal(terminal)?;
                        app.set_status("Forward draft ready".to_string());
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                            app.invalidate_cache_idx(idx);
                        }
                    }
                    Err(e) => {
                        app.set_status_level(format!("Forward failed: {e}"), StatusLevel::Error)
                    }
                }
            } else {
                app.set_status_level(
                    "Failed to save email locally".to_string(),
                    StatusLevel::Error,
                );
            }
        }

        Action::SearchResultArchive => {
            if let Some(path) = ensure_search_result_saved(app) {
                let archive_dir = match app.archive_dir.clone() {
                    Some(d) => d,
                    None => {
                        app.set_status_level(
                            "Archive dir not configured".to_string(),
                            StatusLevel::Error,
                        );
                        return Ok(());
                    }
                };
                let archive_server_name = app.archive_server_name.clone();

                app.server_search_results.remove(app.server_search_index);
                if app.server_search_index >= app.server_search_results.len()
                    && !app.server_search_results.is_empty()
                {
                    app.server_search_index = app.server_search_results.len() - 1;
                }

                app.bg_count += 1;
                app.bg_mutations += 1;
                app.set_status_level("Archiving...".to_string(), StatusLevel::Progress);
                let acct_idx = app.active_account;
                let tx = bg_tx.clone();

                if app.is_graph() {
                    let graph_config = app.graph_config.clone().unwrap();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = rt
                            .block_on(crate::graph::archive_email_graph(
                                &graph_config,
                                &archive_dir,
                                &path,
                                &archive_server_name,
                            ))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Archive {
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
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .expect("failed to create tokio runtime");
                        let result = rt
                            .block_on(archive_email_locally(
                                &imap_config,
                                &archive_dir,
                                &path,
                                &archive_server_name,
                            ))
                            .map(|()| String::new())
                            .map_err(|e| e.to_string());
                        let _ = tx.send(BgResult::Archive {
                            account_index: acct_idx,
                            result,
                        });
                    });
                }
            } else {
                app.set_status_level(
                    "Failed to save email locally".to_string(),
                    StatusLevel::Error,
                );
            }
        }

        _ => {}
    }
    Ok(())
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

    /// The `.md` bridge must never hand a mutation a file path back.
    ///
    /// This is the guard on the stop-gate state: while #0038 scope item 7 is
    /// open, every file-taking mutation arm is fronted by [`message_path`],
    /// and the only correct answer is `None`. If someone makes it resolve a
    /// path again, the mutation would write into a tree that ingest no longer
    /// maintains and the store would silently diverge, so this test fails
    /// loudly rather than letting that ship.
    #[test]
    fn the_file_bridge_never_resolves_a_path() {
        for id in [1, 2, 42, i64::MAX] {
            assert_eq!(
                message_path(MessageRef::new(id)),
                None,
                "message_path resolved a .md path for row {id}; #0038 item 7 owns \
                 removing this bridge, nothing may make it return Some"
            );
        }
    }

    /// The decline message names both the future (store-backed) and the
    /// present (mp-legacy), so a user who hits it knows what to do now.
    #[test]
    fn the_decline_message_points_at_the_working_fallback() {
        let msg = store_backed_soon("Archive");
        assert!(msg.starts_with("Archive "), "{msg}");
        assert!(msg.contains("store-backed"), "{msg}");
        assert!(msg.contains("mp-legacy"), "{msg}");
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
