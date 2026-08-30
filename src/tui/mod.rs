pub mod app;
mod actions;
mod bg;
mod event;
mod helpers;
pub(crate) mod images;
mod mutations;
mod runtime;
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
    // Ask the terminal what graphics protocol it speaks (#0010), once, after
    // the alternate screen is up and before any key is read: the query talks
    // to stdio and a concurrent reader would eat the reply.
    images::init();

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

    // Two-phase startup (#0003). `App::new` built every `AccountState` cheaply
    // -- config only, no store opened -- so the shell above painted with
    // zeroed counts and an empty list. Now, after the first frame is on its
    // way, open each account's store off the UI thread. The first open of a
    // store file runs `PRAGMA integrity_check` (~240 ms on a 44 MB store) and,
    // on failure, the drop-and-rebuild path (#0066); summed serially across
    // accounts inside `App::new` that was ~1.2 s of blank terminal. Here the
    // opens overlap and none of them gate the paint.
    //
    // Each thread reads the grouped mailbox counts and the outbox badge and
    // reports `BgResult::AccountOpened`. Its handler (in `tui/bg.rs`) fills
    // those in, clears `AccountState::opening`, loads the active account's
    // open mailbox against the now-validated store, and kicks the startup
    // auto-fetch (#0001) for that account. Deferring the fetch until the store
    // is known good is deliberate: it avoids a sync racing the very first open
    // of the same file and a redundant second integrity check.
    //
    // `bg_count` is bumped per account so the existing spinner shows "working"
    // until every store is open; the message-ID index scan that used to run
    // here is gone outright (#0038).
    for (i, acct) in app.accounts.iter().enumerate() {
        let account_name = acct.account_config.name.clone();
        let mailboxes = acct.mailboxes.clone();
        let tx = bg_tx.clone();
        app.bg_count += 1;
        std::thread::spawn(move || {
            // `count_all_emails` opens the store (running the integrity check /
            // rebuild on the first open) and returns the grouped per-mailbox
            // counts; the outbox read reuses the now-open, validated file.
            let counts = app::count_all_emails(&account_name, &mailboxes);
            let outbox = crate::outbox::counts_for_account(&account_name);
            let _ = tx.send(BgResult::AccountOpened {
                account_index: i,
                counts,
                outbox,
            });
        });
    }

    // Redraw only when something changed (#0093 §b.7). The loop used to call
    // `terminal.draw` unconditionally every iteration -- ~4x/s at the 250 ms
    // idle poll -- rebuilding every widget's content each tick. `dirty` starts
    // true so the first frame always paints; every branch below that mutates
    // visible state sets it again. Background updates (watcher events, bg
    // results, drafts changes) set it too, so an async body arrival or a sync
    // completion is never swallowed. The spinner keeps a slow tick of its own
    // while `bg_count > 0`.
    let mut dirty = true;
    while app.running {
        if dirty {
            terminal.draw(|frame| ui::view(&mut app, frame))?;
            dirty = false;
        }

        if let Some(msg) = event::poll_event()? {
            let mut current_msg = Some(msg);
            while let Some(m) = current_msg {
                current_msg = app.update(m);
            }
            // Any input -- keypress or resize -- can change what is shown.
            dirty = true;
        } else {
            let status_ticks_before = app.status_ticks;
            app.tick_status();
            if app.status_ticks != status_ticks_before {
                // A status message expired and cleared itself.
                dirty = true;
            }
            if app.bg_count > 0 {
                app.bg_spin_tick = app.bg_spin_tick.wrapping_add(1);
                dirty = true;
            }
        }

        // Check background watcher
        match watch_rx.try_recv() {
            Ok(WatchEvent::Changed { account_index }) => {
                let mut current_msg = Some(app::Message::MailboxChanged { account_index });
                while let Some(m) = current_msg {
                    current_msg = app.update(m);
                }
                dirty = true;
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
                dirty = true;
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
                dirty = true;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                // The watch_tx original is held by this function, so this arm
                // is effectively unreachable during the run; guard the redraw
                // on a real state change anyway so it cannot spin.
                let was_active =
                    app.watcher_active || app.accounts.iter().any(|a| a.watcher_active);
                for acct in &mut app.accounts {
                    acct.watcher_active = false;
                }
                app.watcher_active = false;
                if was_active {
                    dirty = true;
                }
            }
        }

        // Check background task results (drain all available). These carry the
        // async work the UI parked -- sync completion, a body/invite/image
        // arrival -- so every drained result forces a redraw (#0093).
        let mut got_bg_result = false;
        while let Ok(result) = bg_rx.try_recv() {
            bg::handle_bg_result(&mut app, result);
            got_bg_result = true;
        }
        if got_bg_result {
            dirty = true;
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
                // A drafts change reindexed and possibly reloaded the list.
                dirty = true;
            }
        }

        // Auto-execute the parked action once the condition it parked on has
        // cleared. It must be the same condition the action re-enters (see
        // `actions::sync_is_blocked`): releasing on a mutation counter while the
        // gate read `bg_count` meant a running sync released the parked fetch
        // straight back into its own refusal, four times a second (the counter
        // itself is gone with the background mutation jobs, #0039/#0076).
        if actions::queued_action_is_releasable(&app) {
            if let Some(action) = app.queued_action.take() {
                app.pending_actions.push_back(action);
            }
        }

        // Process pending actions (drain queue so user actions are never lost).
        // Handling one mutates state or spawns background work, both of which
        // need a repaint.
        let had_actions = !app.pending_actions.is_empty();
        while let Some(action) = app.pending_actions.pop_front() {
            actions::handle_action(&mut app, terminal, action, &bg_tx)?;
        }
        if had_actions {
            dirty = true;
        }

        // Undo-send hold window (#0090): once a parked send's window has
        // elapsed, hand it to the background send thread. The 250 ms idle poll
        // is the coarsest this can lag, which is imperceptible against a
        // multi-second window. `u` (dispatch_normal_mode) clears the slot
        // first, which is the undo.
        if app.held_send.as_ref().is_some_and(|h| h.is_ready()) {
            if let Some(held) = app.held_send.take() {
                actions::fire_held_send(&mut app, held, &bg_tx);
                dirty = true;
            }
        }

        // Auto-mark the message shown in the preview as read (#0087). This runs
        // after events, background results and actions have settled the
        // selection, and fires once per open: a scroll or an idle tick over the
        // same message is a no-op.
        if actions::auto_mark_open_read(&mut app) {
            dirty = true;
        }
    }

    Ok(())
}
