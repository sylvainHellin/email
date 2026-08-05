pub mod app;
mod actions;
mod bg;
mod event;
mod helpers;
mod mutations;
pub mod theme;
mod ui;

use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, BgResult, MailboxKind};
use helpers::{
    graph_watcher_loop, init_terminal, install_panic_hook, restore_terminal, watcher_loop,
    WatchEvent,
};

use crate::store::drafts;

/// How often the drafts directory is stat-scanned for writes made behind the
/// application's back (#0050 scope item 5, closing [TKT-0045]).
///
/// Drafts are the one part of the model another process owns as much as we do:
/// an agent writes a `.md` into `drafts/` and `$EDITOR` rewrites one while the
/// TUI has it on screen. The IMAP watcher says nothing about either, so
/// without this the Drafts list was only as fresh as the last restart.
///
/// One second, by a `max_depth(1)` stat scan of tens of files
/// ([`drafts::fingerprint`]), rather than a `notify` watcher: the scan costs
/// one `readdir` plus one `stat` per entry against a 250 ms event tick, and the
/// watcher is a new dependency the ticket deliberately defers.
const DRAFTS_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Machine-readable dump of the TUI keymap (`mp dump-keys`), used to
/// regenerate the website key table from the single `KEYMAP` source.
pub fn dump_keys() -> String {
    app::dump_markdown()
}

/// JSON dump of the TUI keymap grouped by section (`mp dump-keys --json`), for
/// regenerating the website data file.
pub fn dump_keys_json() -> String {
    app::dump_json()
}

/// Entry point for the TUI. Call this when `mp` is invoked with no arguments.
pub fn run() -> Result<()> {
    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = run_loop(&mut terminal);
    restore_terminal()?;
    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new();

    let size = terminal.size()?;
    app.terminal_width = size.width;
    app.terminal_height = size.height;

    // Spawn one watcher thread per account that has IMAP config
    let (watch_tx, watch_rx) = mpsc::channel::<WatchEvent>();
    for (i, acct) in app.accounts.iter_mut().enumerate() {
        if let Some(ref imap_cfg) = acct.imap_config {
            acct.watcher_active = true;
            let tx = watch_tx.clone();
            let imap_clone = imap_cfg.clone();
            let acct_idx = i;
            std::thread::spawn(move || {
                watcher_loop(tx, imap_clone, acct_idx);
            });
        } else if let Some(ref graph_cfg) = acct.graph_config {
            acct.watcher_active = true;
            let tx = watch_tx.clone();
            let graph_clone = graph_cfg.clone();
            let acct_idx = i;
            std::thread::spawn(move || {
                graph_watcher_loop(tx, graph_clone, acct_idx);
            });
        }
    }
    // Sync watcher_active for active account
    if let Some(acct) = app.accounts.first() {
        app.watcher_active = acct.watcher_active;
    }

    // Background task results channel
    let (bg_tx, bg_rx) = mpsc::channel::<BgResult>();

    // Baseline for the drafts poll: whatever the directory looks like now is
    // what the first listing will show, so the first change to react to is the
    // next one.
    let mut drafts_account = app.account_config.name.clone();
    let mut drafts_fingerprint = drafts::fingerprint(&crate::config::drafts_dir(&drafts_account));
    let mut last_drafts_poll = Instant::now();

    // The per-account message-ID index scan that used to run here is gone
    // (#0038): identity is the `messages` row and a cross-mailbox lookup is
    // an indexed query at the moment it is asked, so there is nothing to
    // build at launch and no "Indexing..." phase to wait through.
    //
    // The startup auto-fetch (#0001) used to ride on that scan finishing
    // (`BgResult::IndexReady` pushed one `Action::FetchAccount` per
    // account). With no scan to wait for it is queued directly, one action
    // per account with a remote source; local-only accounts have nothing to
    // fetch. The actions still run one at a time through the normal queue,
    // so the staggering the old path got for free is preserved.
    let auto_fetch: Vec<usize> = app
        .accounts
        .iter()
        .enumerate()
        .filter(|(_, acct)| acct.imap_config.is_some() || acct.graph_config.is_some())
        .map(|(i, _)| i)
        .collect();
    for i in auto_fetch {
        app.push_action(app::Action::FetchAccount(i));
    }

    while app.running {
        terminal.draw(|frame| ui::view(&mut app, frame))?;

        if let Some(msg) = event::poll_event()? {
            let mut current_msg = Some(msg);
            while let Some(m) = current_msg {
                current_msg = app.update(m);
            }
        } else {
            app.tick_status();
            if app.bg_count > 0 {
                app.bg_spin_tick = app.bg_spin_tick.wrapping_add(1);
            }
        }

        // Check background watcher
        match watch_rx.try_recv() {
            Ok(WatchEvent::Changed { account_index }) => {
                let mut current_msg = Some(app::Message::MailboxChanged { account_index });
                while let Some(m) = current_msg {
                    current_msg = app.update(m);
                }
            }
            Ok(WatchEvent::Reconnected { account_index }) => {
                let acct_name = app.accounts.get(account_index)
                    .map(|a| a.account_config.name.clone())
                    .unwrap_or_default();
                app.set_status(format!("Watch ({}): reconnected", acct_name));
                if let Some(acct) = app.accounts.get_mut(account_index) {
                    acct.watcher_active = true;
                }
                if account_index == app.active_account {
                    app.watcher_active = true;
                }
            }
            Ok(WatchEvent::Error { account_index, message }) => {
                let acct_name = app.accounts.get(account_index)
                    .map(|a| a.account_config.name.clone())
                    .unwrap_or_default();
                app.set_status(format!("Watch ({}): {}", acct_name, message));
                if let Some(acct) = app.accounts.get_mut(account_index) {
                    acct.watcher_active = false;
                }
                if account_index == app.active_account {
                    app.watcher_active = false;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                for acct in &mut app.accounts {
                    acct.watcher_active = false;
                }
                app.watcher_active = false;
            }
        }

        // Check background task results (drain all available)
        while let Ok(result) = bg_rx.try_recv() {
            bg::handle_bg_result(&mut app, result);
        }

        // The drafts directory, scanned once a second (#0050). The fingerprint
        // is stat-only, so an unchanged directory costs nothing beyond the
        // walk; a change re-indexes and reloads the list the user is looking
        // at, without a restart.
        if last_drafts_poll.elapsed() >= DRAFTS_POLL_INTERVAL {
            last_drafts_poll = Instant::now();
            let account = app.account_config.name.clone();
            let fingerprint = drafts::fingerprint(&crate::config::drafts_dir(&account));
            if account != drafts_account {
                // Account switch: adopt the new directory's state silently.
                // The switch reloaded the mailboxes itself, so there is
                // nothing here to react to.
                drafts_account = account;
                drafts_fingerprint = fingerprint;
            } else if fingerprint != drafts_fingerprint {
                drafts_fingerprint = fingerprint;
                if let Err(e) = drafts::refresh_account(&drafts_account) {
                    log::warn!("[drafts] refreshing the index of {drafts_account} failed: {e:#}");
                }
                if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                    app.invalidate_cache_idx(idx);
                    if app.active_mailbox == idx {
                        app.reload_current_mailbox();
                    }
                }
                app.recount_all_mailboxes();
            }
        }

        // Auto-execute queued action when all mutations complete
        if app.bg_mutations == 0 && app.pending_actions.is_empty() {
            if let Some(action) = app.queued_action.take() {
                app.pending_actions.push_back(action);
            }
        }

        // Process pending actions (drain queue so user actions are never lost)
        while let Some(action) = app.pending_actions.pop_front() {
            actions::handle_action(&mut app, terminal, action, &bg_tx)?;
        }
    }

    Ok(())
}
