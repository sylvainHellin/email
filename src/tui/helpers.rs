use std::io::{self, stdout};
use std::panic;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::{Context, Result};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use super::app::{App, EmailEntry, MessageRef, SearchHit, SearchTarget};

use crate::config::{all_configured_mailboxes, AccountConfig, ImapConfig};
use crate::draft::parse_email_draft;
use crate::imap_client::{
    fetch_emails_on_session, open_imap_session, parse_search_query, sync_mailboxes, SyncTarget,
};
use crate::parse::FetchedEmail;
use crate::store::open_store;

// ---------------------------------------------------------------------------
// Watcher
// ---------------------------------------------------------------------------

pub(super) enum WatchEvent {
    Changed {
        account_index: usize,
    },
    Reconnected {
        account_index: usize,
    },
    Error {
        account_index: usize,
        message: String,
    },
}

pub(super) fn watcher_loop(
    tx: mpsc::Sender<WatchEvent>,
    imap_config: ImapConfig,
    account_index: usize,
) {
    use crate::imap_client::watch_mailbox as imap_watch;

    const BASE_BACKOFF_SECS: u64 = 30;
    const MAX_BACKOFF_SECS: u64 = 300;

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => {
            let _ = tx.send(WatchEvent::Error {
                account_index,
                message: "Failed to create async runtime".into(),
            });
            return;
        }
    };

    let mut consecutive_failures: u32 = 0;

    loop {
        match rt.block_on(imap_watch(&imap_config, "INBOX", Some(300))) {
            Ok(0) => {
                if consecutive_failures > 0 {
                    consecutive_failures = 0;
                    let _ = tx.send(WatchEvent::Reconnected { account_index });
                }
                if tx.send(WatchEvent::Changed { account_index }).is_err() {
                    break;
                }
            }
            Ok(2) => {
                if consecutive_failures > 0 {
                    consecutive_failures = 0;
                    let _ = tx.send(WatchEvent::Reconnected { account_index });
                }
                continue; // timeout, re-idle
            }
            Ok(_) | Err(_) => {
                // Only notify the UI on the first failure; subsequent retries are silent.
                if consecutive_failures == 0 {
                    let _ = tx.send(WatchEvent::Error {
                        account_index,
                        message: "Watch connection lost, retrying with backoff...".into(),
                    });
                }
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff = (BASE_BACKOFF_SECS * 2u64.saturating_pow(consecutive_failures - 1))
                    .min(MAX_BACKOFF_SECS);
                std::thread::sleep(std::time::Duration::from_secs(backoff));
            }
        }
    }
}

/// How long the Graph watcher waits between polls, and how that widens after
/// consecutive failures.
const GRAPH_POLL_SECS: u64 = 60;

/// Consecutive failed polls before the user is told. One failure is a hiccup
/// (a dropped connection, a throttled request); three in a row is an account
/// that needs attention, typically a refresh token the tenant revoked.
const GRAPH_FAILURES_BEFORE_ALERT: u32 = 3;

/// The delay before the next Graph poll after `failures` consecutive failures.
///
/// Shares [`crate::outbox::backoff_secs`] rather than hand-rolling a second
/// curve, floored at the normal poll interval (backing *off* must never poll
/// more often) and capped by that function at 15 minutes.
fn graph_poll_delay(failures: u32) -> std::time::Duration {
    let backoff = crate::outbox::backoff_secs(failures as i64).max(0) as u64;
    std::time::Duration::from_secs(backoff.max(GRAPH_POLL_SECS))
}

/// Poll the Graph inbox for change.
///
/// Compares the *set* of message ids, not its cardinality: one arrival plus one
/// archive inside the same interval leaves the count untouched and used to pass
/// unnoticed. One [`crate::graph::GraphClient`] serves the whole loop, its
/// token refreshed in place per pass, so a poll costs one enumeration rather
/// than a keyring read and a fresh connection pool as well.
///
/// Still an enumeration of the whole folder every minute; the delta query that
/// removes it is #0042.
pub(super) fn graph_watcher_loop(
    tx: mpsc::Sender<WatchEvent>,
    graph_config: crate::config::GraphConfig,
    account_index: usize,
) {
    use std::collections::HashSet;

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => {
            let _ = tx.send(WatchEvent::Error {
                account_index,
                message: "Failed to create async runtime".into(),
            });
            return;
        }
    };

    let mut client: Option<crate::graph::GraphClient> = None;
    let mut known: Option<HashSet<String>> = None;
    let mut consecutive_failures: u32 = 0;
    // Whether the UI has been told about the current failure run, so the
    // "reconnected" that clears it is only sent when there is something to
    // clear.
    let mut alerted = false;

    loop {
        let poll = rt.block_on(async {
            match client.as_mut() {
                Some(existing) => existing.refresh_token(&graph_config).await?,
                None => {
                    client = Some(crate::graph::GraphClient::new_async(&graph_config).await?);
                }
            }
            let client = client.as_ref().expect("client built above");
            let folder = client.enumerate_folder("inbox").await?;
            Ok::<HashSet<String>, anyhow::Error>(folder.entries.into_keys().collect())
        });

        match poll {
            Ok(ids) => {
                consecutive_failures = 0;
                if alerted {
                    alerted = false;
                    let _ = tx.send(WatchEvent::Reconnected { account_index });
                }
                let changed = known.as_ref().is_some_and(|prev| *prev != ids);
                known = Some(ids);
                if changed && tx.send(WatchEvent::Changed { account_index }).is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(GRAPH_POLL_SECS));
            }
            Err(e) => {
                // The token is the likeliest thing to have gone stale, and it
                // lives in the client, so the next pass builds a new one.
                client = None;
                consecutive_failures = consecutive_failures.saturating_add(1);
                log::warn!(
                    "Graph watcher poll failed for account {} ({} in a row): {:#}",
                    account_index,
                    consecutive_failures,
                    e
                );
                if consecutive_failures == GRAPH_FAILURES_BEFORE_ALERT {
                    alerted = true;
                    let _ = tx.send(WatchEvent::Error {
                        account_index,
                        message: format!(
                            "{consecutive_failures} failed polls, backing off: {e}"
                        ),
                    });
                }
                std::thread::sleep(graph_poll_delay(consecutive_failures));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal helpers
// ---------------------------------------------------------------------------

pub(super) fn suspend_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show,
    )?;
    disable_raw_mode()?;
    Ok(())
}

pub(super) fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

pub(super) fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub(super) fn restore_terminal() -> Result<()> {
    execute!(stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

pub(super) fn install_panic_hook() {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        original_hook(panic_info);
    }));
}

// ---------------------------------------------------------------------------
// Editor / clipboard
// ---------------------------------------------------------------------------

fn editor() -> String {
    std::env::var("EDITOR").unwrap_or_else(|_| "hx".to_string())
}

pub(super) fn edit_file(path: &Path) -> Result<()> {
    let editor = editor();
    let status = std::process::Command::new(&editor)
        .arg(path)
        .status()
        .with_context(|| format!("Failed to launch editor: {}", editor))?;
    if !status.success() {
        anyhow::bail!("Editor exited with status: {}", status);
    }
    Ok(())
}

pub(super) fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("Failed to access clipboard")?;
    clipboard
        .set_text(text)
        .context("Failed to copy to clipboard")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Library call helpers
// ---------------------------------------------------------------------------

pub(super) async fn lib_do_sync(
    account_config: &AccountConfig,
    imap_config: &ImapConfig,
    limit: usize,
) -> anyhow::Result<(String, SyncResultMeta)> {
    let span_label = if limit < usize::MAX { "lib_do_sync:quick" } else { "lib_do_sync:full" };
    let _span = crate::timing::TimingSpan::with_context(span_label, account_config.name.clone());

    let targets: Vec<SyncTarget> = all_configured_mailboxes(account_config)
        .iter()
        .map(|(role, mapping)| SyncTarget {
            role: role.clone(),
            server_name: mapping.server.clone(),
        })
        .collect();

    // The sync tick is also the outbox's retry tick: a Sent copy that could
    // not be appended when the message was sent lands here (#0037 item 5).
    crate::send::resume_outbox(account_config).await;

    // And the mutation queue's drain tick (#0039): archive, delete, move and
    // flag toggles enqueued locally are retired before this sync reads the
    // mailboxes they changed, under the engine lock. Drained before the read
    // so the server has converged by the time the reconcile looks at it.
    let ops_suffix = drain_pending_ops(account_config).await;

    // An account-level failure (a refused login above all) has to reach the
    // log: the status line it otherwise becomes loses every race against a
    // concurrent account that succeeded, which is how #0068 stayed invisible
    // for seven weeks. The per-mailbox path already warns
    // (`imap_client::store_sync`); this is its account-level equivalent.
    // A persistent per-account health surface is #0071.
    let result = sync_mailboxes(imap_config, &account_config.name, &targets, limit, false)
        .await
        .inspect_err(|e| log::error!("[sync] account '{}' failed: {e:#}", account_config.name))?;
    Ok((format!("{}{ops_suffix}", finish_sync(account_config, &result)), SyncResultMeta {
        new_inbox_mail: result.new_inbox_mail.clone(),
    }))
}

/// The fragment every failed-drain status suffix carries, so the completion
/// handler can honestly downgrade the status level of an otherwise-successful
/// sync that also rolled mutations back (#0039 review note).
pub(crate) const FAILED_OPS_MARKER: &str = "mutation(s) failed and were rolled back";

/// Drain the account's pending-mutation queue at the sync/fetch resume point
/// (#0039), returning a status suffix that names any failures.
///
/// A drained op is silent: it only mirrored a change the store already made, so
/// there is nothing new to tell the user. A failed op has already been rolled
/// back by the drain and reappears when the sync refresh reloads the list, so
/// the suffix points at the log rather than repeating the per-op error the
/// drain has already written there. The drain builds no backend and takes no
/// lock unless a row is actually owed, so a clean account adds no traffic.
async fn drain_pending_ops(account_config: &AccountConfig) -> String {
    match crate::pending_ops::resume_account(account_config).await {
        Ok(Some(r)) if r.failed > 0 => {
            format!("; {} {FAILED_OPS_MARKER} (see the log)", r.failed)
        }
        Ok(_) => String::new(),
        Err(e) => {
            log::warn!(
                "[pending_ops] draining {} at the sync tick failed: {e:#}",
                account_config.name
            );
            String::new()
        }
    }
}

/// Post-sync hooks shared by both backends, and the one-line status message.
///
/// The `.md` era also returned the directories a sync had touched so the TUI
/// could invalidate its caches. The store made that list unnecessary rather
/// than unnecessary to act on: a sync writes rows the list reads, so the TUI
/// drops every cache of the account when the result lands (see
/// `tui::bg::refresh_after_server_sync`).
fn finish_sync(
    account_config: &AccountConfig,
    result: &crate::imap_client::SyncResult,
) -> String {
    // Incremental contacts-index update (best-effort, no-op if no cache).
    crate::contacts::hooks::bump_after_sync(account_config, &result.fresh_observations);

    // Organizer-side REPLY reconciliation (#0030) has no post-sync hook any
    // more: the fold runs where the statuses are displayed, over the rows this
    // sync just ingested (#0038 scope item 6), so there is nothing to bump.

    let mut msg = format!("Synced: {} new, {} existing", result.saved, result.skipped);
    if result.flags_updated > 0 {
        msg.push_str(&format!(", {} status updated", result.flags_updated));
    }
    if result.uid_rebound > 0 {
        msg.push_str(&format!(", {} renumbered", result.uid_rebound));
    }
    if result.pruned > 0 {
        msg.push_str(&format!(", {} no longer in this mailbox", result.pruned));
    }
    // Say so rather than reporting a clean sync: the rows are known to be gone
    // from the server and are still on screen until a pass sees every mailbox
    // in full (#0072).
    if result.prunes_deferred > 0 {
        msg.push_str(&format!(
            ", {} removal(s) held back (incomplete pass, run a full sync)",
            result.prunes_deferred
        ));
    }
    msg
}

/// Metadata returned alongside the status message from a sync.
pub(super) struct SyncResultMeta {
    /// Sender + subject of every genuinely new inbox email this sync
    /// ingested, for the desktop notification (#0009).
    pub new_inbox_mail: Vec<crate::notify::NewMailMeta>,
}

pub(super) async fn lib_do_multi_search(
    account: &str,
    imap_config: &ImapConfig,
    query: &str,
    targets: &[SearchTarget],
) -> anyhow::Result<Vec<SearchHit>> {
    let mut criteria = parse_search_query(query);
    criteria.in_mailbox = None;

    let mut session = open_imap_session(imap_config).await?;
    let total_limit = 50usize;
    let per_mb = (total_limit / targets.len().max(1)).max(5);
    let mut total = 0usize;

    let mut hits: Vec<SearchHit> = Vec::new();

    for target in targets {
        if total >= total_limit {
            break;
        }
        let budget = per_mb.min(total_limit - total);
        log::info!(
            "Server search: querying mailbox '{}' (label={})",
            target.server_name,
            target.label,
        );
        match fetch_emails_on_session(&mut session, &criteria, &target.server_name, Some(budget))
            .await
        {
            Ok(emails) => {
                log::info!(
                    "Server search: '{}' returned {} result(s)",
                    target.server_name,
                    emails.len(),
                );
                total += emails.len();
                for fetched in emails {
                    let entry = fetched_to_email_entry(account, &fetched);
                    hits.push(SearchHit {
                        entry,
                        fetched,
                        source_label: target.label.clone(),
                    });
                }
            }
            Err(e) => {
                log::warn!("Search in {} failed: {}", target.server_name, e);
            }
        }
    }

    session.logout().await.ok();

    hits.sort_by(|a, b| b.entry.date_sort.cmp(&a.entry.date_sort));

    Ok(hits)
}

pub(super) async fn lib_do_sync_graph(
    account_config: &AccountConfig,
    graph_config: &crate::config::GraphConfig,
    limit: usize,
) -> anyhow::Result<(String, SyncResultMeta)> {
    let span_label = if limit < usize::MAX {
        "lib_do_sync_graph:quick"
    } else {
        "lib_do_sync_graph:full"
    };
    let _span = crate::timing::TimingSpan::with_context(span_label, account_config.name.clone());

    let targets: Vec<SyncTarget> = all_configured_mailboxes(account_config)
        .iter()
        .map(|(role, mapping)| SyncTarget {
            role: role.clone(),
            server_name: mapping.server.clone(),
        })
        .collect();

    // Same reason as the IMAP path above: an account-level failure has to be
    // in the log, not only in a status line another account will overwrite
    // (#0068, #0071).
    // The mutation queue drains here too (#0039), before the Graph read. Graph
    // has no outbox resume (its resubmit is a no-op), but move / delete /
    // mark-read ops are real work the queue owes the server.
    let ops_suffix = drain_pending_ops(account_config).await;

    let result = crate::graph::sync_mailboxes_graph(
        graph_config,
        &account_config.name,
        &targets,
        limit,
        false,
    )
    .await
    .inspect_err(|e| log::error!("[sync] account '{}' failed: {e:#}", account_config.name))?;

    Ok((format!("{}{ops_suffix}", finish_sync(account_config, &result)), SyncResultMeta {
        new_inbox_mail: result.new_inbox_mail.clone(),
    }))
}

pub(super) async fn lib_do_multi_search_graph(
    account: &str,
    graph_config: &crate::config::GraphConfig,
    query: &str,
    targets: &[SearchTarget],
) -> anyhow::Result<Vec<SearchHit>> {
    let mut criteria = parse_search_query(query);
    criteria.in_mailbox = None;

    let client = crate::graph::GraphClient::new_async(graph_config).await?;
    let total_limit = 50usize;
    let per_mb = (total_limit / targets.len().max(1)).max(5);
    let mut total = 0usize;
    let mut hits: Vec<SearchHit> = Vec::new();

    for target in targets {
        if total >= total_limit {
            break;
        }
        let budget = per_mb.min(total_limit - total);
        match client
            .search_messages(&criteria, Some(&target.server_name), budget)
            .await
        {
            Ok(emails) => {
                total += emails.len();
                for fetched in emails {
                    let entry = fetched_to_email_entry(account, &fetched);
                    hits.push(SearchHit {
                        entry,
                        fetched,
                        source_label: target.label.clone(),
                    });
                }
            }
            Err(e) => {
                log::warn!("Graph search in {} failed: {}", target.server_name, e);
            }
        }
    }

    hits.sort_by(|a, b| b.entry.date_sort.cmp(&a.entry.date_sort));
    Ok(hits)
}

/// Turn a server-search hit into a list entry.
///
/// The hit came straight off the server and was never ingested, so it may or
/// may not correspond to a row this account already holds. The entry is
/// resolved against the store by Message-ID (`store::read::find_by_message_id`,
/// the same indexed lookup that replaced the startup Message-ID walk in
/// #0038): a hit that resolves carries `Some(MessageRef)` and behaves like any
/// other row, a hit that does not carries `None` and every row-dependent
/// operation on it declines with a status message. There is deliberately no
/// sentinel `MessageRef`: an entry that pretends to be row 0 could reach the
/// selection set and a batch action would then act on the wrong message.
///
/// This is the honest continuation of the gap #0049 recorded as 4a
/// ("server-search open hit only resolves messages already local"): before the
/// nuke the resolution was a Message-ID scan of the mailbox directory, now it
/// is the same question asked of the store. A server-only hit is still listed
/// and previewable from the fetched content and still not openable as a local
/// message; closing that needs the hit to be ingested on demand, which is not
/// this unit.
///
/// A message that lives in several mailboxes resolves to the first row in
/// `(mailbox, uid)` order, which is what the directory scan did too (it looked
/// only in the mailbox the hit came from and took the single match there).
fn fetched_to_email_entry(account: &str, fetched: &FetchedEmail) -> EmailEntry {
    let (date_display, date_sort) =
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(&fetched.date) {
            (
                dt.format("%Y-%m-%d").to_string(),
                dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
            )
        } else {
            (
                fetched.date.chars().take(10).collect(),
                fetched.date.clone(),
            )
        };

    let msg = resolve_fetched_hit(account, fetched);

    EmailEntry {
        msg,
        draft_id: None,
        skip: None,
        from: fetched.from.clone(),
        to: fetched.to.clone(),
        cc: fetched.cc.clone(),
        subject: fetched.subject.clone(),
        status: String::new(),
        date_display,
        date_sort,
        has_attachments: fetched.has_attachments,
        read: fetched.flags.seen,
        answered: fetched.flags.answered,
        forwarded: fetched.flags.forwarded,
        flagged: fetched.flags.flagged,
        is_invite: fetched.event.is_some(),
    }
}

/// Look one server-search hit up in the account's store by Message-ID.
///
/// `None` covers all three misses that mean the same thing to the caller: the
/// account has no store yet, the hit carries no Message-ID, or no row holds
/// it. The store is opened per hit, which is a few microseconds against a
/// search that just did a network round trip.
fn resolve_fetched_hit(account: &str, fetched: &FetchedEmail) -> Option<MessageRef> {
    let message_id = fetched.message_id.as_deref()?;
    let store = open_store(account)?;
    match crate::store::read::find_by_message_id(&store, account, message_id) {
        Ok(rows) => rows.first().map(|row| MessageRef::new(row.id)),
        Err(e) => {
            log::warn!("[store] resolving search hit {message_id}: {e:#}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Account resolution for Send
// ---------------------------------------------------------------------------

/// Which transport a send actually uses, keyed off the account's
/// `auth_method` alone.
///
/// A Graph account sends over Graph or not at all: an SMTP config that happens
/// to be loaded is not a fallback for a `GraphConfig` that is not.
/// `AccountState::new` loads the Graph config with `GraphConfig::load(..).ok()`,
/// so a Graph account whose config fails to load carries `graph_config: None`;
/// a guard that asks "is there a Graph config?" therefore sends such an account
/// over SMTP behind the user's back, under an identity Graph would have
/// stamped. Asking the account what it is instead makes that case an error the
/// user sees.
///
/// `Err` carries the status-line wording for the missing transport.
pub(super) fn resolve_send_transport(
    account_config: &AccountConfig,
    graph: Option<crate::config::GraphConfig>,
    smtp: Option<crate::config::SmtpConfig>,
) -> Result<(Option<crate::config::GraphConfig>, Option<crate::config::SmtpConfig>), &'static str> {
    if account_config.auth_method == crate::config::AuthMethod::Graph {
        match graph {
            Some(g) => Ok((Some(g), None)),
            None => Err("Graph not configured"),
        }
    } else {
        match smtp {
            Some(s) => Ok((None, Some(s))),
            None => Err("SMTP not configured"),
        }
    }
}

/// Which account sends this draft: the one whose address its `from:` names,
/// falling back to the active one.
///
/// The draft file rather than the open mailbox is the source of truth, because
/// a draft written for another configured account (a reply to mail that
/// arrived there) has to leave through that account's credentials.
pub(super) fn resolve_send_account(
    app: &App,
    draft_path: &Path,
) -> (
    usize,
    Option<crate::config::SmtpConfig>,
    Option<crate::config::ImapConfig>,
    Option<crate::config::GraphConfig>,
    AccountConfig,
    Option<String>,
) {
    if let Ok(draft) = parse_email_draft(draft_path) {
        let from = draft.frontmatter.from.unwrap_or_default().to_lowercase();
        for (i, acct) in app.accounts.iter().enumerate() {
            if from.contains(&acct.account_config.default_from.to_lowercase()) {
                return (
                    i,
                    acct.smtp_config.clone(),
                    acct.imap_config.clone(),
                    acct.graph_config.clone(),
                    acct.account_config.clone(),
                    acct.signature_content.clone(),
                );
            }
        }
    }
    let acct = &app.accounts[app.active_account];
    (
        app.active_account,
        acct.smtp_config.clone(),
        acct.imap_config.clone(),
        acct.graph_config.clone(),
        acct.account_config.clone(),
        acct.signature_content.clone(),
    )
}

// ---------------------------------------------------------------------------
// Tests (#0049 unit 0b)
//
// `src/tui/helpers.rs` had no tests at all. These capture the two pieces the
// audit called out as user-visible: which account a draft is sent from, and
// how a server-search hit is turned into a list row.
//
// Tagging convention, per #0049: `parity` means the new build must reproduce
// the recorded behaviour, `known-bug` means the recorded behaviour is wrong
// and the comment names the target. Nothing here is fixed in this unit.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountConfig;
    use crate::parse::FetchedEmail;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// A minimal `AccountState`. Built as a struct literal rather than through
    /// `AccountState::new`, which reads the user's config, keyring and
    /// signature files and would make these tests machine-dependent.
    fn account(name: &str, default_from: &str) -> super::super::app::AccountState {
        super::super::app::AccountState {
            account_config: AccountConfig {
                name: name.to_string(),
                default_from: default_from.to_string(),
                ..Default::default()
            },
            imap_config: None,
            smtp_config: None,
            graph_config: None,
            signature_content: Some(format!("-- \n{name}")),
            archive_server_name: "Archive".to_string(),
            drafts_dir: None,
            mailboxes: Vec::new(),
            mailbox_counts: Vec::new(),
            email_cache: Vec::new(),
            sidebar_index: 0,
            active_mailbox: 0,
            list_index: 0,
            cursor_ref: None,
            headers_scroll: 0,
            preview_scroll: 0,
            selection: std::collections::HashSet::new(),
            search_query: String::new(),
            search_includes_body: false,
            watcher_active: false,
            outbox: crate::outbox::OutboxCounts::default(),
            has_unseen: false,
            sync_health: crate::sync_health::SyncHealth::default(),
        }
    }

    fn app_with(accounts: Vec<super::super::app::AccountState>, active: usize) -> App {
        let mut app = App::default_for_tests();
        app.accounts = accounts;
        app.active_account = active;
        app
    }

    /// Write a draft whose `from:` is exactly `from`.
    fn draft(dir: &Path, name: &str, from: Option<&str>) -> PathBuf {
        let from_line = match from {
            Some(f) => format!("from: \"{f}\"\n"),
            None => String::new(),
        };
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("---\nto: someone@example.com\nsubject: hi\nstatus: draft\n{from_line}---\n\nbody\n"),
        )
        .unwrap();
        path
    }

    fn smtp() -> crate::config::SmtpConfig {
        crate::config::SmtpConfig {
            host: "smtp.example.com".to_string(),
            port: 587,
            username: "me@example.com".to_string(),
            password: "secret".to_string(),
            default_from: "me@example.com".to_string(),
            accept_invalid_certs: false,
            auth_method: crate::config::AuthMethod::Password,
        }
    }

    fn graph() -> crate::config::GraphConfig {
        crate::config::GraphConfig {
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            username: "me@example.com".to_string(),
            account_name: "work".to_string(),
        }
    }

    fn account_config(auth_method: crate::config::AuthMethod) -> AccountConfig {
        AccountConfig {
            name: "work".to_string(),
            default_from: "me@example.com".to_string(),
            auth_method,
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // resolve_send_transport (#0058 review)
    // -----------------------------------------------------------------------

    /// A Graph account sends over Graph, and the SMTP config it also carries
    /// is dropped rather than kept as a fallback.
    #[test]
    fn resolve_send_transport_sends_a_graph_account_over_graph_only() {
        let cfg = account_config(crate::config::AuthMethod::Graph);
        // `SmtpConfig` holds a password and deliberately has no `Debug`, so
        // these unwrap by hand rather than through `Result::unwrap`.
        let Ok((g, s)) = resolve_send_transport(&cfg, Some(graph()), Some(smtp())) else {
            panic!("a Graph account with a Graph config has a transport");
        };
        assert!(g.is_some());
        assert!(s.is_none(), "the loaded SMTP config is not a Graph fallback");
    }

    /// The regression this guard exists for: `AccountState::new` loads the
    /// Graph config with `.ok()`, so a Graph account whose config fails to
    /// load reaches the send path with `graph: None`. It must be refused, not
    /// quietly sent over the SMTP config that did load.
    #[test]
    fn resolve_send_transport_refuses_a_graph_account_with_no_graph_config() {
        let cfg = account_config(crate::config::AuthMethod::Graph);
        let Err(missing) = resolve_send_transport(&cfg, None, Some(smtp())) else {
            panic!("a Graph account with no Graph config fell back to SMTP");
        };
        assert_eq!(missing, "Graph not configured");
    }

    /// A password account sends over SMTP, and a Graph config that somehow
    /// loaded is not consulted.
    #[test]
    fn resolve_send_transport_sends_a_password_account_over_smtp_only() {
        let cfg = account_config(crate::config::AuthMethod::Password);
        let Ok((g, s)) = resolve_send_transport(&cfg, Some(graph()), Some(smtp())) else {
            panic!("a password account with an SMTP config has a transport");
        };
        assert!(g.is_none());
        assert!(s.is_some());

        let Err(missing) = resolve_send_transport(&cfg, Some(graph()), None) else {
            panic!("a password account with no SMTP config has no transport");
        };
        assert_eq!(missing, "SMTP not configured");
    }

    fn fetched(date: &str) -> FetchedEmail {
        FetchedEmail {
            from: "Jürgen Müller <juergen@example.de>".to_string(),
            to: "me@example.com".to_string(),
            cc: Some("cc@example.com".to_string()),
            subject: "Grüße".to_string(),
            date: date.to_string(),
            body_text: "body text".to_string(),
            html_body: Some("<p>body text</p>".to_string()),
            has_attachments: true,
            message_id: Some("<m1@example.de>".to_string()),
            attachments: Vec::new(),
            flags: crate::types::MessageFlags::seen(true),
            calendar_ics: None,
            event: None,
        }
    }

    // -----------------------------------------------------------------------
    // Graph watcher
    // -----------------------------------------------------------------------

    /// The Graph watcher widens its poll interval as failures pile up instead
    /// of retrying a revoked token once a minute forever, and never polls
    /// *faster* than the healthy interval.
    #[test]
    fn the_graph_poll_delay_widens_and_never_dips_below_the_poll_interval() {
        assert_eq!(graph_poll_delay(0).as_secs(), GRAPH_POLL_SECS);
        assert_eq!(graph_poll_delay(1).as_secs(), GRAPH_POLL_SECS);
        let widening: Vec<u64> = (1..=8).map(|n| graph_poll_delay(n).as_secs()).collect();
        assert!(
            widening.windows(2).all(|w| w[1] >= w[0]),
            "delays must be monotonic: {widening:?}"
        );
        assert_eq!(
            *widening.last().unwrap(),
            crate::outbox::BACKOFF_MAX_SECS as u64,
            "and settle at the shared ceiling"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_send_account
    // -----------------------------------------------------------------------

    /// parity. The draft's `from:` selects the account, case-insensitively,
    /// and the whole per-account bundle (index, configs, signature, sent dir)
    /// comes from that account rather than from the active one.
    #[test]
    fn resolve_send_account_matches_the_draft_from_address_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app_with(
            vec![
                account("work", "sylvain@work.example"),
                account("perso", "sylvain@perso.example"),
            ],
            0,
        );
        let path = draft(
            tmp.path(),
            "d.md",
            Some("Sylvain Hellin <SYLVAIN@Perso.Example>"),
        );

        let (idx, _smtp, _imap, _graph, cfg, signature) =
            resolve_send_account(&app, &path);
        assert_eq!(idx, 1);
        assert_eq!(cfg.name, "perso");
        assert_eq!(signature.as_deref(), Some("-- \nperso"));
    }

    /// parity. With no `from:` in the draft, or with a `from:` that matches no
    /// account, or with an unreadable path, the active account sends. The
    /// fallback is silent: nothing tells the user the address they typed was
    /// ignored.
    #[test]
    fn resolve_send_account_falls_back_to_the_active_account() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app_with(
            vec![
                account("work", "sylvain@work.example"),
                account("perso", "sylvain@perso.example"),
            ],
            1,
        );

        let no_from = draft(tmp.path(), "no-from.md", None);
        assert_eq!(resolve_send_account(&app, &no_from).0, 1);

        let foreign = draft(tmp.path(), "foreign.md", Some("someone@elsewhere.example"));
        assert_eq!(resolve_send_account(&app, &foreign).0, 1);

        let missing = tmp.path().join("does-not-exist.md");
        assert_eq!(resolve_send_account(&app, &missing).0, 1);
    }

    /// parity. When two accounts share the same address the first one in
    /// config order wins.
    #[test]
    fn resolve_send_account_takes_the_first_of_two_accounts_sharing_an_address() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app_with(
            vec![
                account("first", "shared@example.com"),
                account("second", "shared@example.com"),
            ],
            1,
        );
        let path = draft(tmp.path(), "d.md", Some("shared@example.com"));
        let (idx, _, _, _, cfg, _) = resolve_send_account(&app, &path);
        assert_eq!(idx, 0);
        assert_eq!(cfg.name, "first");
    }

    /// known-bug. The match is a substring test (`from.contains(default_from)`),
    /// not an address comparison, so a draft from `not-sylvain@work.example`
    /// resolves to the `sylvain@work.example` account: the mail goes out over
    /// the wrong account's SMTP server and is filed in the wrong Sent folder.
    /// Target: compare the parsed address of the draft's `from:` with the
    /// account address for equality (case-insensitive), and fall back to the
    /// active account when nothing matches.
    #[test]
    fn resolve_send_account_substring_match_picks_a_foreign_address() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app_with(
            vec![
                account("decoy", "no-such@example.org"),
                account("work", "sylvain@work.example"),
            ],
            0,
        );
        let path = draft(tmp.path(), "d.md", Some("not-sylvain@work.example"));
        let (idx, _, _, _, cfg, _) = resolve_send_account(&app, &path);
        assert_eq!(idx, 1, "a different mailbox matched by substring");
        assert_eq!(cfg.name, "work");
    }

    /// known-bug. An account with an empty `default_from` (a half-configured
    /// account, which the config wizard allows) swallows every draft, because
    /// every string contains the empty string. It wins even over the account
    /// whose address the draft actually names, since it comes first.
    /// Target: an empty account address must never match.
    #[test]
    fn resolve_send_account_empty_default_from_matches_every_draft() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app_with(
            vec![account("half-configured", ""), account("work", "sylvain@work.example")],
            1,
        );
        let path = draft(tmp.path(), "d.md", Some("sylvain@work.example"));
        let (idx, _, _, _, cfg, _) = resolve_send_account(&app, &path);
        assert_eq!(idx, 0);
        assert_eq!(cfg.name, "half-configured");
    }

    // -----------------------------------------------------------------------
    // fetched_to_email_entry
    // -----------------------------------------------------------------------

    /// parity. A server-search hit becomes a list row with no store row (the
    /// account below has no store at all) and an empty status, and the
    /// remaining fields are copied through.
    #[test]
    fn fetched_to_email_entry_copies_fields_and_leaves_msg_and_status_empty() {
        let entry = fetched_to_email_entry("nobody", &fetched("Mon, 01 Jan 2024 12:00:00 +0000"));

        assert_eq!(entry.msg, None);
        assert_eq!(entry.status, "");
        assert_eq!(entry.subject, "Grüße");
        assert_eq!(entry.to, "me@example.com");
        assert_eq!(entry.cc.as_deref(), Some("cc@example.com"));
        assert!(entry.has_attachments);
        assert!(entry.read);
        assert!(!entry.is_invite);
        assert_eq!(entry.date_display, "2024-01-01");
    }

    /// known-bug. The sort key keeps the sender's local wallclock instead of
    /// being normalised to UTC, so two hits from different timezones on the
    /// same day sort by wallclock rather than by instant. `resolve_date` in
    /// `src/tui/app/types.rs` normalises (that was the fix for #0024); this
    /// path never got it, so a search result and the same email loaded from
    /// disk carry different sort keys.
    /// Target: `date_sort` in UTC, i.e. `2024-01-01T08:00:00` below.
    #[test]
    fn fetched_to_email_entry_sort_key_keeps_the_sender_local_wallclock() {
        let entry = fetched_to_email_entry("nobody", &fetched("Mon, 01 Jan 2024 10:00:00 +0200"));
        assert_eq!(entry.date_display, "2024-01-01");
        assert_eq!(entry.date_sort, "2024-01-01T10:00:00");

        // 10:00+0200 is 08:00 UTC, so this later message (09:00 UTC) must sort
        // after it. On the recorded wallclock keys it sorts before.
        let later = fetched_to_email_entry("nobody", &fetched("Mon, 01 Jan 2024 09:00:00 +0000"));
        assert_eq!(later.date_sort, "2024-01-01T09:00:00");
        assert!(
            later.date_sort < entry.date_sort,
            "the later message sorts first"
        );
    }

    /// parity. A `Date:` header that is not RFC 2822 (or the `(unknown date)`
    /// placeholder from `parse_rfc822_to_fetched_email`) degrades to its first
    /// ten characters for display, counted in characters so a multi-byte date
    /// string cannot panic, with the raw string as the sort key.
    #[test]
    fn fetched_to_email_entry_falls_back_to_the_first_ten_chars() {
        let entry = fetched_to_email_entry("nobody", &fetched("2024-01-01 12:00 (approx)"));
        assert_eq!(entry.date_display, "2024-01-01");
        assert_eq!(entry.date_sort, "2024-01-01 12:00 (approx)");

        let placeholder = fetched_to_email_entry("nobody", &fetched("(unknown date)"));
        assert_eq!(placeholder.date_display, "(unknown d");
        assert_eq!(placeholder.date_sort, "(unknown date)");

        let unicode = fetched_to_email_entry("nobody", &fetched("日本語の日付です、これは長い"));
        assert_eq!(unicode.date_display, "日本語の日付です、こ");
    }

    /// The decided behaviour for a server-search hit (#0038): the entry is
    /// resolved against the account's store by Message-ID, so a hit a previous
    /// sync already ingested carries that row's `MessageRef` and a hit that
    /// exists only on the server carries `None`. No sentinel ref is minted for
    /// the second case, because a fake ref could reach the selection set and a
    /// batch action would then act on a different message.
    #[test]
    fn a_search_hit_carries_a_ref_only_when_the_store_holds_it() {
        let _dir = crate::config::test_env::TestDataDir::new();

        let mut local = fetched("Mon, 01 Jan 2024 12:00:00 +0000");
        local.message_id = Some("<local@example.de>".to_string());
        let store = crate::store::Store::open(crate::config::store_path("alice")).unwrap();
        let blobs = crate::store::BlobStore::for_account("alice");
        let row_id = crate::ingest::ingest_message(
            &store,
            &blobs,
            &crate::ingest::IngestInput {
                account: "alice",
                mailbox: "inbox",
                uid: 1,
                email: &local,
                raw: None,
            },
        )
        .unwrap()
        .row_id;
        drop(store);

        let resolved = fetched_to_email_entry("alice", &local);
        assert_eq!(
            resolved.msg,
            Some(crate::tui::app::MessageRef::new(row_id)),
            "a hit the store already holds resolves to its row"
        );

        let mut server_only = fetched("Mon, 01 Jan 2024 12:00:00 +0000");
        server_only.message_id = Some("<never-synced@example.de>".to_string());
        assert_eq!(
            fetched_to_email_entry("alice", &server_only).msg,
            None,
            "a server-only hit carries no ref"
        );

        let mut anonymous = fetched("Mon, 01 Jan 2024 12:00:00 +0000");
        anonymous.message_id = None;
        assert_eq!(
            fetched_to_email_entry("alice", &anonymous).msg,
            None,
            "a hit without a Message-ID cannot be resolved"
        );
    }

    /// known-bug. The row keeps the raw `From:` header, address included,
    /// while `parse_email` (the on-disk path) stores only the display name.
    /// The same email therefore reads "Jürgen Müller <juergen@example.de>" in
    /// the search overlay and "Jürgen Müller" in the list.
    /// Target: one projection for both paths.
    #[test]
    fn fetched_to_email_entry_keeps_the_raw_from_header() {
        let entry = fetched_to_email_entry("nobody", &fetched("Mon, 01 Jan 2024 12:00:00 +0000"));
        assert_eq!(entry.from, "Jürgen Müller <juergen@example.de>");
        assert_eq!(
            crate::tui::app::extract_display_name(&entry.from),
            "Jürgen Müller",
            "what the list would have shown for the same email"
        );
    }
}
