use super::app::{
    Action, App, BgResult, MailboxInfo, MailboxKind, SearchOverlayFocus, SearchResultEntry,
    StatusLevel,
};
use crate::imap_client::{save_mailbox_states_cache, update_read_status_locally};

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

/// Map the local directories a sync actually touched to mailbox indices.
/// Used after a fetch to invalidate only the affected mailbox caches
/// instead of all of them.
fn mailbox_indices_for_dirs(
    mailboxes: &[MailboxInfo],
    touched: &[std::path::PathBuf],
) -> Vec<usize> {
    mailboxes
        .iter()
        .enumerate()
        .filter(|(_, mb)| touched.contains(&mb.dir))
        .map(|(i, _)| i)
        .collect()
}

/// After a failed move the on-disk rollback may leave both the SOURCE
/// mailbox (the email was optimistically removed from its list/cache)
/// and the destination cache inconsistent. Returns the cache indices to
/// invalidate (deduped) and whether the currently open mailbox is one of
/// them and therefore needs a reload. The user may have switched
/// mailboxes while the move was in flight, so the source is NOT
/// necessarily the active mailbox.
fn move_failure_invalidation(
    source_mailbox_idx: usize,
    dest_mailbox_idx: usize,
    active_mailbox: usize,
) -> (Vec<usize>, bool) {
    let mut indices = vec![source_mailbox_idx];
    if dest_mailbox_idx != source_mailbox_idx {
        indices.push(dest_mailbox_idx);
    }
    let reload_current = indices.contains(&active_mailbox);
    (indices, reload_current)
}

/// Persist the account's `mailbox_states` cache to disk. Best-effort: logs
/// and continues on failure (the cache only buys us a faster first quick
/// sync; sync correctness does not depend on it).
fn persist_mailbox_states(acct: &super::app::AccountState) {
    let root = crate::config::account_dir(&acct.account_config.name);
    if let Err(e) = save_mailbox_states_cache(&root, &acct.mailbox_states) {
        log::warn!(
            "Failed to persist mailbox states for '{}': {}",
            acct.account_config.name,
            e,
        );
    }
}

/// Decrement bg_mutations on the correct account.
fn decrement_mutations(app: &mut App, account_index: usize) {
    if account_index == app.active_account {
        app.bg_mutations = app.bg_mutations.saturating_sub(1);
    } else if let Some(acct) = app.accounts.get_mut(account_index) {
        acct.bg_mutations = acct.bg_mutations.saturating_sub(1);
    }
}

pub(super) fn handle_bg_result(app: &mut App, result: BgResult) {
    app.bg_count = app.bg_count.saturating_sub(1);

    match result {
        BgResult::Archive { account_index, result } => {
            decrement_mutations(app, account_index);
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Email archived".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    if account_index == app.active_account {
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Archive) {
                            app.invalidate_cache_idx(idx);
                        }
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                }
                Err(e) => {
                    app.push_status(format!("Archive failed: {e}"), StatusLevel::Error);
                    if account_index == app.active_account {
                        // Only invalidate Inbox + Archive (the two involved mailboxes)
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Inbox) {
                            app.invalidate_cache_idx(idx);
                        }
                        if let Some(idx) = app.find_mailbox_by_kind(MailboxKind::Archive) {
                            app.invalidate_cache_idx(idx);
                        }
                        app.reload_current_mailbox();
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                    app.set_persistent_error(format!(
                        "Archive failed: {e}\nEmail restored to inbox. Sync (F) to fix?"
                    ));
                }
            }
        }

        BgResult::Move {
            account_index,
            source_mailbox_idx,
            dest_mailbox_idx,
            dest_label,
            result,
        } => {
            decrement_mutations(app, account_index);
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() {
                        format!("Moved to {dest_label}")
                    } else {
                        msg
                    };
                    app.set_status_level(text, StatusLevel::Success);
                    if account_index == app.active_account {
                        // The source list was already updated optimistically;
                        // only the destination cache went stale.
                        app.invalidate_cache_idx(dest_mailbox_idx);
                        if let Some(count) = app.mailbox_counts.get_mut(dest_mailbox_idx) {
                            *count += 1;
                        }
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                }
                Err(e) => {
                    app.push_status(format!("Move failed: {e}"), StatusLevel::Error);
                    if account_index == app.active_account {
                        // Source and destination may both be inconsistent
                        // after a rollback -- invalidate both by index
                        // (the user may have switched mailboxes while the
                        // move was in flight) and reload the open mailbox
                        // only if it is one of them.
                        let (indices, reload_current) = move_failure_invalidation(
                            source_mailbox_idx,
                            dest_mailbox_idx,
                            app.active_mailbox,
                        );
                        for idx in indices {
                            app.invalidate_cache_idx(idx);
                        }
                        if reload_current {
                            app.reload_current_mailbox();
                        }
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                    app.set_persistent_error(format!(
                        "Move failed: {e}\nEmail restored. Sync (F) to fix?"
                    ));
                }
            }
        }

        BgResult::Delete { account_index, result } => {
            decrement_mutations(app, account_index);
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Email deleted".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                }
                Err(e) => {
                    app.push_status(format!("Delete failed: {e}"), StatusLevel::Error);
                    if account_index == app.active_account {
                        app.reload_current_mailbox();
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                    app.set_persistent_error(format!(
                        "Delete failed: {e}\nEmail restored. Sync (F) to fix?"
                    ));
                }
            }
        }

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
                    // The invite's local event.rsvp frontmatter changed on
                    // disk; refresh the open mailbox so the preview card and
                    // list badge reflect the new own-RSVP state.
                    if account_index == app.active_account {
                        app.invalidate_cache_idx(app.active_mailbox);
                        app.reload_current_mailbox();
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
            new_index,
            new_mailbox_states,
            touched_dirs,
            new_inbox_mail,
        } => {
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Fetch complete".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    // Desktop notification for genuinely new inbox mail
                    // (#0009). Opt-in via `notifications = true` in
                    // config.toml; no-op when the list is empty (read-flag
                    // updates and reconciliation never populate it).
                    if app.global_config.notifications && !new_inbox_mail.is_empty() {
                        let account_name = app
                            .accounts
                            .get(account_index)
                            .map(|a| a.account_config.name.as_str())
                            .unwrap_or("");
                        crate::notify::notify_new_mail(account_name, &new_inbox_mail);
                    }
                    // Merge updated index and mailbox states back into AccountState
                    if let Some(acct) = app.accounts.get_mut(account_index) {
                        if let Some(index) = new_index {
                            acct.message_id_index = index;
                        }
                        if let Some(states) = new_mailbox_states {
                            acct.mailbox_states = states;
                            persist_mailbox_states(acct);
                        }
                    }
                    match touched_dirs {
                        // No-op sync: nothing changed on disk (common for
                        // IDLE-triggered fetches). Skip cache invalidation
                        // and the mailbox reload entirely.
                        Some(dirs) if dirs.is_empty() => {}
                        // Sync reported exactly which local dirs it modified
                        // (saves, read-flag updates, dedup, reconciliation
                        // moves/removals -- moves list both source and
                        // destination). Invalidate only those caches.
                        Some(dirs) => {
                            if account_index == app.active_account {
                                let indices =
                                    mailbox_indices_for_dirs(&app.mailboxes, &dirs);
                                let reload_current = indices.contains(&app.active_mailbox);
                                for idx in indices {
                                    app.invalidate_cache_idx(idx);
                                }
                                if reload_current {
                                    app.reload_current_mailbox();
                                }
                            } else {
                                let indices = app
                                    .accounts
                                    .get(account_index)
                                    .map(|acct| {
                                        mailbox_indices_for_dirs(&acct.mailboxes, &dirs)
                                    })
                                    .unwrap_or_default();
                                for idx in indices {
                                    app.invalidate_cache_idx_on(account_index, idx);
                                }
                            }
                        }
                        // Unknown which dirs changed -- fall back to the
                        // conservative full invalidation.
                        None => {
                            if account_index == app.active_account {
                                app.invalidate_all_caches();
                                app.reload_current_mailbox();
                            } else {
                                app.invalidate_all_caches_on(account_index);
                            }
                        }
                    }
                }
                Err(e) => app.set_status_level(format!("Fetch failed: {e}"), StatusLevel::Error),
            }
        }

        BgResult::Sync { account_index, result, new_index, new_mailbox_states } => {
            match result {
                Ok(msg) => {
                    let text = if msg.is_empty() { "Sync complete".into() } else { msg };
                    app.set_status_level(text, StatusLevel::Success);
                    // Merge updated index and mailbox states back into AccountState
                    if let Some(acct) = app.accounts.get_mut(account_index) {
                        if let Some(index) = new_index {
                            acct.message_id_index = index;
                        }
                        if let Some(states) = new_mailbox_states {
                            acct.mailbox_states = states;
                            persist_mailbox_states(acct);
                        }
                    }
                    if account_index == app.active_account {
                        // Full sync may move emails between mailboxes via reconciliation
                        app.invalidate_all_caches();
                        app.reload_current_mailbox();
                        app.recount_all_mailboxes();
                    } else {
                        app.invalidate_all_caches_on(account_index);
                    }
                }
                Err(e) => app.set_status_level(format!("Sync failed: {e}"), StatusLevel::Error),
            }
        }

        BgResult::ToggleRead { account_index, path, new_read_state, result } => {
            // ToggleRead does NOT use bg_mutations -- it doesn't block fetch/sync
            match result {
                Ok(_) => { /* Server confirmed, local already updated optimistically */ }
                Err(e) => {
                    // Rollback local state
                    let reverted = !new_read_state;
                    update_read_status_locally(&path, reverted).ok();
                    if account_index == app.active_account {
                        // Updates both the in-memory list and the shared
                        // cache slot (they are the same Arc).
                        app.set_email_read(&path, reverted);
                    }
                    app.push_status(format!("Read status sync failed: {e}"), StatusLevel::Warning);
                }
            }
        }

        BgResult::IndexReady { account_index, index } => {
            let mut should_auto_fetch = false;
            if let Some(acct) = app.accounts.get_mut(account_index) {
                acct.message_id_index = index;
                acct.indexing = false;
                // Trigger the per-account startup auto-fetch (#0001) iff
                // the account has a remote source configured. Local-only
                // accounts (no IMAP, no Graph) get nothing to fetch.
                should_auto_fetch =
                    acct.imap_config.is_some() || acct.graph_config.is_some();
            }
            // Clear status only when the LAST account finishes. Each
            // account spawns its own indexing thread; we want the
            // "Indexing..." spinner to remain visible until they all
            // arrive. The per-thread `bg_count` decrement above keeps
            // the spinner spinning; setting a Success message here on
            // the final arrival mirrors the other bg-result UX.
            if !app.accounts.iter().any(|a| a.indexing) {
                app.set_status_level("Index ready".to_string(), StatusLevel::Success);
            }
            // Push *after* the status update so the per-account
            // "Quick sync (...)" message wins over "Index ready". Use
            // `push_action` rather than `push_action_dedup` -- different
            // `FetchAccount(idx)` values share a discriminant and would
            // otherwise be falsely deduplicated.
            if should_auto_fetch {
                app.push_action(Action::FetchAccount(account_index));
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
            // path, fall back to the clamped index when that email is
            // gone from the fresh list.
            let anchor = app.cursor_anchor();
            let fallback = app.list_index;
            let entries = std::sync::Arc::new(entries);
            if let Some(slot) = app.email_cache.get_mut(mailbox_idx) {
                // Cache slot and `app.emails` share the allocation (P2):
                // no deep clone on delivery.
                *slot = Some(std::sync::Arc::clone(&entries));
            }
            app.emails = entries;
            // Reapply the active search filter (if any) to the fresh
            // entries, then put the cursor back on the anchored email.
            app.rebuild_visible();
            app.restore_cursor(anchor, fallback);
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
                            saved_path: None,
                            source_label: hit.source_label,
                            source_local_dir: hit.source_local_dir,
                            source_status: hit.source_status,
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
    use std::path::PathBuf;

    fn mb(label: &str, dir: &str, kind: MailboxKind) -> MailboxInfo {
        MailboxInfo {
            label: label.to_string(),
            icon: "",
            dir: PathBuf::from(dir),
            kind,
            server_name: None,
        }
    }

    fn sample_mailboxes() -> Vec<MailboxInfo> {
        vec![
            mb("Inbox", "/mail/acct/inbox", MailboxKind::Inbox),
            mb("Drafts", "/mail/acct/drafts", MailboxKind::Drafts),
            mb("Sent", "/mail/acct/sent", MailboxKind::Sent),
            mb("Archive", "/mail/acct/archive", MailboxKind::Archive),
        ]
    }

    #[test]
    fn no_touched_dirs_maps_to_no_indices() {
        let mailboxes = sample_mailboxes();
        assert!(mailbox_indices_for_dirs(&mailboxes, &[]).is_empty());
    }

    #[test]
    fn single_touched_dir_maps_to_its_mailbox_only() {
        let mailboxes = sample_mailboxes();
        let touched = vec![PathBuf::from("/mail/acct/inbox")];
        assert_eq!(mailbox_indices_for_dirs(&mailboxes, &touched), vec![0]);
    }

    /// A reconciliation move touches both source and destination; both
    /// mailbox caches must be invalidated.
    #[test]
    fn move_touches_source_and_destination() {
        let mailboxes = sample_mailboxes();
        let touched = vec![
            PathBuf::from("/mail/acct/inbox"),
            PathBuf::from("/mail/acct/archive"),
        ];
        assert_eq!(mailbox_indices_for_dirs(&mailboxes, &touched), vec![0, 3]);
    }

    /// A touched dir not shown in the sidebar (e.g. a mailbox removed from
    /// config between syncs) is simply ignored -- no panic, no bogus index.
    #[test]
    fn unknown_touched_dir_is_ignored() {
        let mailboxes = sample_mailboxes();
        let touched = vec![PathBuf::from("/mail/acct/junk")];
        assert!(mailbox_indices_for_dirs(&mailboxes, &touched).is_empty());
    }

    #[test]
    fn order_follows_mailbox_list_not_touched_list() {
        let mailboxes = sample_mailboxes();
        let touched = vec![
            PathBuf::from("/mail/acct/archive"),
            PathBuf::from("/mail/acct/inbox"),
        ];
        assert_eq!(mailbox_indices_for_dirs(&mailboxes, &touched), vec![0, 3]);
    }

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
    // move_failure_invalidation (#0018 follow-up: invalidate the actual
    // SOURCE mailbox, not whatever mailbox happens to be open)
    // -----------------------------------------------------------------------

    /// User stayed on the source mailbox: both caches invalidated,
    /// open mailbox reloaded.
    #[test]
    fn move_failure_source_active_invalidates_both_and_reloads() {
        assert_eq!(move_failure_invalidation(0, 3, 0), (vec![0, 3], true));
    }

    /// User switched to the destination while the move was in flight:
    /// both caches invalidated, open mailbox (dest) reloaded.
    #[test]
    fn move_failure_dest_active_invalidates_both_and_reloads() {
        assert_eq!(move_failure_invalidation(0, 3, 3), (vec![0, 3], true));
    }

    /// User switched to an unrelated mailbox: the SOURCE cache (where the
    /// email was optimistically removed) must still be invalidated so the
    /// rolled-back email reappears on the next visit -- but the open
    /// mailbox is untouched by the rollback, so no reload.
    #[test]
    fn move_failure_unrelated_active_invalidates_source_without_reload() {
        assert_eq!(move_failure_invalidation(0, 3, 2), (vec![0, 3], false));
    }

    /// Degenerate same-mailbox move: no duplicate invalidation.
    #[test]
    fn move_failure_same_source_and_dest_dedupes() {
        assert_eq!(move_failure_invalidation(3, 3, 3), (vec![3], true));
    }
}
