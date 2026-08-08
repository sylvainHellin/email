use super::app::{
    App, BgResult, MailboxKind, SearchOverlayFocus, SearchResultEntry,
    StatusLevel,
};

/// Whether a `BgResult::MailboxLoaded` may be applied or must be dropped
/// as stale (P1 step 2). A background walk is only valid if the user is
/// still looking at the same account and mailbox it was requested for,
/// AND no newer request or optimistic list mutation has bumped the
/// generation since (an older walk could resurrect an archived/deleted
/// email or clobber a newer reload's result).
fn mailbox_loaded_is_current(
    active_account: usize,
    active_mailbox: usize,
    current_generation: u64,
    account_index: usize,
    mailbox_idx: usize,
    generation: u64,
) -> bool {
    account_index == active_account
        && mailbox_idx == active_mailbox
        && generation == current_generation
}

/// Bring the list back in step with the store after a fetch or sync wrote to
/// it (#0038 follow-up).
///
/// Ingest inserts rows, the flag pass rewrites `\Seen` and the prune deletes
/// rows the server no longer lists, in every target mailbox of the account. The
/// list reads those same rows, so all of that account's per-mailbox caches are
/// dropped. The mailbox the user is looking at is then reloaded off the UI
/// thread through the same `request_mailbox_load` path a mailbox switch takes,
/// and the sidebar counts are recomputed with one grouped query.
///
/// An inactive account keeps only the cache drop: it has no list on screen and
/// no counts to redraw, and switching to it reloads from the store anyway.
fn refresh_after_server_sync(app: &mut App, account_index: usize) {
    if account_index == app.active_account {
        app.invalidate_all_caches();
        app.reload_current_mailbox();
        app.recount_all_mailboxes();
    } else {
        app.invalidate_all_caches_on(account_index);
    }
}

/// Re-read one account's outbox counts for the status-bar badge (#0037).
fn refresh_outbox(app: &mut App, account_index: usize) {
    if let Some(acct) = app.accounts.get_mut(account_index) {
        acct.outbox = crate::outbox::counts_for_account(&acct.account_config.name);
    }
}

/// ` (name)` for a known account of a multi-account setup, empty otherwise,
/// for status lines that would not otherwise say which account they are about.
fn account_label(app: &App, account_index: usize) -> String {
    match app.accounts.get(account_index) {
        Some(acct) if app.accounts.len() > 1 => format!(" ({})", acct.account_config.name),
        _ => String::new(),
    }
}

/// Record the outcome of one account's sync on that account (#0071).
///
/// Every sync completion path lands here: the startup multi-account fetch, the
/// watcher-triggered quick sync, a manual `F`, a full `S`, over IMAP or Graph
/// alike, because all of them arrive as `BgResult::Fetch` or `BgResult::Sync`
/// carrying the account they belong to. Writing the outcome *on the account*
/// rather than into the shared status line is the whole fix: an account that
/// failed keeps its mark while the accounts that succeeded overwrite the line
/// (#0068).
fn record_sync_health(app: &mut App, account_index: usize, outcome: Result<(), &str>) {
    let now = chrono::Local::now();
    if let Some(acct) = app.accounts.get_mut(account_index) {
        acct.sync_health = acct.sync_health.updated(outcome, now);
    }
}

pub(super) fn handle_bg_result(app: &mut App, result: BgResult) {
    app.bg_count = app.bg_count.saturating_sub(1);
    match &result {
        BgResult::Send { account_index, .. }
        | BgResult::SendApproved { account_index, .. }
        | BgResult::Rsvp { account_index, .. }
        | BgResult::Sync { account_index, .. }
        | BgResult::Fetch { account_index, .. } => refresh_outbox(app, *account_index),
        _ => {}
    }

    match result {
        BgResult::Send { account_index, result } => {
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Email sent".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    if account_index == app.active_account {
                        // Send moves a draft to Sent -- only those two need invalidation
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                            app.invalidate_cache_idx(idx);
                        }
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Sent) {
                            app.invalidate_cache_idx(idx);
                        }
                        app.reload_current_mailbox();
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                }
                Err(e) => app.set_status_level(format!("Send failed: {e}"), StatusLevel::Error),
            }
        }

        BgResult::Rsvp { account_index, result } => {
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "RSVP sent".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    // Our own reply is now a row in `sent` (the outbox
                    // ingests the appended copy during the send), so the
                    // derived own-RSVP has changed: refresh the open mailbox
                    // and rebuild the agenda from it (#0038 item 6).
                    if account_index == app.active_account {
                        app.invalidate_cache_idx(app.active_mailbox);
                        app.reload_current_mailbox();
                        if app.calendar_view.loaded {
                            app.refresh_calendar();
                        }
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                }
                Err(e) => app.set_status_level(format!("RSVP failed: {e}"), StatusLevel::Error),
            }
        }

        BgResult::SendApproved { account_index, result } => {
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Approved emails sent".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    if account_index == app.active_account {
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Drafts) {
                            app.invalidate_cache_idx(idx);
                        }
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Sent) {
                            app.invalidate_cache_idx(idx);
                        }
                        app.reload_current_mailbox();
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                }
                Err(e) => app.set_status_level(format!("Send-approved failed: {e}"), StatusLevel::Error),
            }
        }

        BgResult::Fetch {
            account_index,
            result,
            new_inbox_mail,
        } => {
            record_sync_health(
                app,
                account_index,
                result.as_ref().map(|_| ()).map_err(|e| e.as_str()),
            );
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Fetch complete".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    // Desktop notification for genuinely new inbox mail
                    // (#0009). Opt-in via `notifications = true` in
                    // config.toml; no-op when the list is empty (read-flag
                    // updates never populate it).
                    if app.global_config.notifications && !new_inbox_mail.is_empty() {
                        let account_name = app
                            .accounts
                            .get(account_index)
                            .map(|a| a.account_config.name.as_str())
                            .unwrap_or("");
                        crate::notify::notify_new_mail(account_name, &new_inbox_mail);
                    }
                    refresh_after_server_sync(app, account_index);
                }
                Err(e) => {
                    // Named, because a multi-account run turns an anonymous
                    // "Fetch failed" into a line nobody can act on (#0068).
                    let name = account_label(app, account_index);
                    app.set_status_level(format!("Fetch failed{name}: {e}"), StatusLevel::Error)
                }
            }
        }

        BgResult::Sync { account_index, result } => {
            record_sync_health(
                app,
                account_index,
                result.as_ref().map(|_| ()).map_err(|e| e.as_str()),
            );
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Sync complete".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    refresh_after_server_sync(app, account_index);
                }
                Err(e) => {
                    let name = account_label(app, account_index);
                    app.set_status_level(format!("Sync failed{name}: {e}"), StatusLevel::Error)
                }
            }
        }

        BgResult::MailboxLoaded { account_index, mailbox_idx, generation, entries } => {
            if !mailbox_loaded_is_current(
                app.active_account,
                app.active_mailbox,
                app.mailbox_load_generation,
                account_index,
                mailbox_idx,
                generation,
            ) {
                // Stale: the user switched account/mailbox, a newer reload
                // superseded this one, or an optimistic mutation
                // (archive/delete) made this walk's snapshot unsafe to
                // apply. Do NOT populate the cache either -- the walk may
                // predate an in-flight file move. The cache slot stays
                // `None`, so the next visit triggers a fresh load.
                return;
            }
            // The cursor's identity must be read from the OLD list: a
            // reload re-sorts (an approved draft changes status/date) and
            // grows (new inbox mail shifts every row down), so the bare
            // `list_index` would land on a different email. Anchor on the
            // message ref, fall back to the clamped index when that email
            // is gone from the fresh list.
            let anchor = app.cursor_anchor();
            let fallback = app.list_index;
            let entries = std::sync::Arc::new(entries);
            if let Some(slot) = app.email_cache.get_mut(mailbox_idx) {
                // Cache slot and `app.emails` share the allocation (P2):
                // no deep clone on delivery.
                *slot = Some(std::sync::Arc::clone(&entries));
            }
            app.emails = entries;
            // A key the fresh list no longer holds must leave the selection
            // with it (#0052): an externally deleted draft is gone from the
            // index, not just from the screen.
            app.scrub_selection();
            // Reapply the active search filter (if any) to the fresh
            // entries, then put the cursor back on the anchored email.
            app.rebuild_visible();
            app.restore_cursor(anchor, fallback);
            // A conversation-overlay jump into this mailbox parked a target
            // (#0008); now that the fresh list is here, put the cursor on it,
            // overriding the anchor restore above.
            app.consume_pending_select();
            if let Some(count) = app.mailbox_counts.get_mut(mailbox_idx) {
                *count = app.emails.len();
            }
            // Clear the "Loading <mailbox>..." indication set by a
            // cache-miss mailbox switch (harmless no-op otherwise).
            if matches!(&app.status_message, Some(m) if m.starts_with("Loading ")) {
                app.status_message = None;
                app.status_ticks = 0;
            }
        }

        BgResult::ServerSearch { result } => {
            app.server_search_loading = false;
            match result {
                Ok(hits) => {
                    let count = hits.len();
                    app.server_search_results = hits
                        .into_iter()
                        .map(|hit| SearchResultEntry {
                            entry: hit.entry,
                            fetched: hit.fetched,
                            source_label: hit.source_label,
                        })
                        .collect();
                    app.server_search_index = 0;
                    app.server_search_scroll = 0;
                    app.server_search_headers_scroll = 0;
                    app.server_search_status = Some(format!(
                        "{} result{}",
                        count,
                        if count == 1 { "" } else { "s" }
                    ));
                    if count > 0 {
                        app.server_search_focus = SearchOverlayFocus::List;
                    }
                }
                Err(e) => {
                    app.server_search_status = Some(format!("Error: {}", e));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // mailbox_loaded_is_current (P1 step 2: background mailbox loads)
    // -----------------------------------------------------------------------

    #[test]
    fn mailbox_loaded_applies_when_everything_matches() {
        assert!(mailbox_loaded_is_current(0, 2, 7, 0, 2, 7));
    }

    /// User switched account while the walk was in flight.
    #[test]
    fn mailbox_loaded_dropped_on_account_switch() {
        assert!(!mailbox_loaded_is_current(1, 2, 7, 0, 2, 7));
    }

    /// User switched to another mailbox (e.g. a cached one, which does not
    /// bump the generation) while the walk was in flight.
    #[test]
    fn mailbox_loaded_dropped_on_mailbox_switch() {
        assert!(!mailbox_loaded_is_current(0, 3, 7, 0, 2, 7));
    }

    /// A newer reload (or an optimistic archive/delete mutation) bumped
    /// the generation; the older walk must not clobber it / resurrect
    /// removed entries.
    #[test]
    fn mailbox_loaded_dropped_on_stale_generation() {
        assert!(!mailbox_loaded_is_current(0, 2, 8, 0, 2, 7));
    }

    /// Out-of-order delivery: a walk stamped with a *newer* generation
    /// than the app's counter can only happen through wraparound or a
    /// bug -- treated as stale either way.
    #[test]
    fn mailbox_loaded_dropped_on_future_generation() {
        assert!(!mailbox_loaded_is_current(0, 2, 7, 0, 2, 8));
    }

    // -----------------------------------------------------------------------
    // refresh_after_server_sync (#0038 follow-up: a refresh that refreshes)
    // -----------------------------------------------------------------------

    /// Point the data directory at a temp dir so `recount_all_mailboxes`
    /// resolves its store inside the fixture. Serialised against the other
    /// data-dir tests by `config::data_dir_lock`.
    struct DataDir {
        _dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl DataDir {
        fn new() -> Self {
            let guard = crate::config::data_dir_lock();
            let previous = std::env::var("MAILYPOPPINS_DATA_DIR").ok();
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("MAILYPOPPINS_DATA_DIR", dir.path());
            Self { _dir: dir, _guard: guard, previous }
        }
    }

    impl Drop for DataDir {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => std::env::set_var("MAILYPOPPINS_DATA_DIR", v),
                None => std::env::remove_var("MAILYPOPPINS_DATA_DIR"),
            }
        }
    }

    /// An app parked on mailbox 1 of a two-mailbox account, both caches warm.
    fn app_with_warm_caches() -> App {
        let mut app = App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.mailboxes = vec![
            crate::tui::app::MailboxInfo {
                label: "Inbox".into(),
                icon: "",
                id: "inbox".into(),
                kind: MailboxKind::Inbox,
                server_name: None,
            },
            crate::tui::app::MailboxInfo {
                label: "Archive".into(),
                icon: "",
                id: "archive".into(),
                kind: MailboxKind::Archive,
                server_name: None,
            },
        ];
        app.mailbox_counts = vec![3, 4];
        app.email_cache = vec![
            Some(std::sync::Arc::new(Vec::new())),
            Some(std::sync::Arc::new(Vec::new())),
        ];
        app.active_mailbox = 1;
        app
    }

    /// A minimal `AccountState` with one warm mailbox cache, for the account
    /// that is not on screen. Built as a struct literal rather than through
    /// `AccountState::new`, which reads the user's config and keyring.
    fn background_account(name: &str) -> crate::tui::app::AccountState {
        crate::tui::app::AccountState {
            account_config: crate::config::AccountConfig {
                name: name.to_string(),
                ..Default::default()
            },
            imap_config: None,
            smtp_config: None,
            graph_config: None,
            signature_content: None,
            archive_server_name: "Archive".to_string(),
            drafts_dir: None,
            mailboxes: Vec::new(),
            mailbox_counts: vec![7],
            email_cache: vec![Some(std::sync::Arc::new(Vec::new()))],
            sidebar_index: 0,
            active_mailbox: 0,
            list_index: 0,
            cursor_ref: None,
            headers_scroll: 0,
            preview_scroll: 0,
            selection: std::collections::HashSet::new(),
            search_query: String::new(),
            search_includes_body: false,
            bg_mutations: 0,
            watcher_active: false,
            outbox: crate::outbox::OutboxCounts::default(),
            has_unseen: false,
            sync_health: crate::sync_health::SyncHealth::default(),
        }
    }

    /// A completed sync rewrote rows in every target mailbox, so every cache
    /// of that account goes, the open mailbox is queued for a background
    /// reload, and the sidebar counts are recomputed. Before this the handler
    /// only set a status line, so a message archived in another client stayed
    /// in the local inbox until a mailbox switch.
    #[test]
    fn a_finished_sync_drops_every_cache_and_reloads_the_open_mailbox() {
        let _data = DataDir::new();
        let mut app = app_with_warm_caches();

        refresh_after_server_sync(&mut app, 0);

        assert!(
            app.email_cache.iter().all(|slot| slot.is_none()),
            "a sync writes to every target mailbox, so every cache is stale"
        );
        let queued: Vec<usize> = app
            .pending_actions
            .iter()
            .filter_map(|a| match a {
                crate::tui::app::Action::LoadMailbox { mailbox_idx, .. } => Some(*mailbox_idx),
                _ => None,
            })
            .collect();
        assert_eq!(
            queued,
            vec![1],
            "the open mailbox reloads off the UI thread, like a mailbox switch"
        );
        assert_eq!(
            app.mailbox_counts,
            vec![0, 0],
            "the sidebar counts come back from the store, not from the stale cache"
        );
    }

    /// A sync on a background account has no list on screen and no counts to
    /// redraw: it drops that account's caches and leaves the active account
    /// alone. Account 1 carries a warm cache of its own, so the drop is
    /// observable rather than a no-op on an empty `accounts` vector.
    #[test]
    fn a_finished_sync_on_another_account_leaves_the_open_list_alone() {
        let _data = DataDir::new();
        let mut app = app_with_warm_caches();
        app.accounts = vec![background_account("alice"), background_account("bob")];

        refresh_after_server_sync(&mut app, 1);

        assert!(
            app.accounts[1].email_cache.iter().all(|slot| slot.is_none()),
            "the synced account's rows changed under its cache"
        );
        assert_eq!(
            app.accounts[1].mailbox_counts,
            vec![7],
            "an off-screen account keeps its counts until it is switched to"
        );
        assert!(
            app.accounts[0].email_cache.iter().all(|slot| slot.is_some()),
            "the other background account is untouched"
        );
        assert!(
            app.email_cache.iter().all(|slot| slot.is_some()),
            "the active account's caches are not stale"
        );
        assert!(app.pending_actions.is_empty(), "nothing to reload");
        assert_eq!(app.mailbox_counts, vec![3, 4]);
    }

    // -----------------------------------------------------------------------
    // Per-account sync health (#0071, the race #0068 lost)
    // -----------------------------------------------------------------------

    /// Two accounts, one broken. `perso` fails at login in milliseconds,
    /// `tum` finishes fifteen seconds later and overwrites the status line
    /// with a success. That is the exact sequence that hid a seven-week
    /// outage: the assertion is that `perso` is still marked failed
    /// afterwards, with its reason intact, while `tum` reads healthy.
    #[test]
    fn a_failed_account_stays_failed_while_another_account_syncs_cleanly() {
        let _data = DataDir::new();
        let mut app = app_with_warm_caches();
        app.accounts = vec![background_account("perso"), background_account("tum")];
        app.active_account = 0;

        handle_bg_result(
            &mut app,
            BgResult::Fetch {
                account_index: 0,
                result: Err("IMAP login failed: no such user".to_string()),
                new_inbox_mail: Vec::new(),
            },
        );
        handle_bg_result(
            &mut app,
            BgResult::Fetch {
                account_index: 1,
                result: Ok("Synced: 8 new, 0 existing".to_string()),
                new_inbox_mail: Vec::new(),
            },
        );

        assert!(
            app.accounts[0].sync_health.is_failed(),
            "the broken account keeps its mark after another account succeeds"
        );
        assert_eq!(
            app.accounts[0].sync_health.failure_lines().unwrap().1,
            "IMAP login failed: no such user"
        );
        assert!(!app.accounts[1].sync_health.is_failed());
        assert_eq!(
            app.status_message.as_deref(),
            Some("Synced: 8 new, 0 existing"),
            "the status line still shows the last writer, which is why the \
             health lives on the account instead"
        );
    }

    /// The mark is cleared by that same account syncing cleanly, not by the
    /// next status line, and a full sync (`BgResult::Sync`) clears a quick
    /// sync's failure: both paths write the same per-account state.
    #[test]
    fn an_accounts_own_success_clears_its_mark() {
        let _data = DataDir::new();
        let mut app = app_with_warm_caches();
        app.accounts = vec![background_account("perso")];
        app.active_account = 0;

        handle_bg_result(
            &mut app,
            BgResult::Fetch {
                account_index: 0,
                result: Err("IMAP login failed".to_string()),
                new_inbox_mail: Vec::new(),
            },
        );
        assert!(app.accounts[0].sync_health.is_failed());

        handle_bg_result(
            &mut app,
            BgResult::Sync {
                account_index: 0,
                result: Ok("Synced: 3 new, 0 existing".to_string()),
            },
        );
        assert!(!app.accounts[0].sync_health.is_failed());
        assert_eq!(app.accounts[0].sync_health.failure_lines(), None);
    }

    /// Repeated failures of the same account count up rather than resetting,
    /// so an outage reads differently from a hiccup.
    #[test]
    fn repeated_failures_of_one_account_accumulate_across_results() {
        let _data = DataDir::new();
        let mut app = app_with_warm_caches();
        app.accounts = vec![background_account("perso")];
        app.active_account = 0;

        for _ in 0..3 {
            handle_bg_result(
                &mut app,
                BgResult::Fetch {
                    account_index: 0,
                    result: Err("IMAP login failed".to_string()),
                    new_inbox_mail: Vec::new(),
                },
            );
        }

        assert!(app.accounts[0]
            .sync_health
            .failure_lines()
            .unwrap()
            .0
            .contains("x3"));
    }
}
