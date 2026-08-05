mod calendar_view;
mod keymap;
mod keys;
mod types;

pub use calendar_view::load_events_for_account;
pub use keymap::{
    dump_json, dump_markdown, help_sections, hint_bindings, prefix_continuations, resolve, Guard,
    KeyAction, KeyBinding, KeyCtx, KEYMAP,
};
pub use types::*;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Top-level application state.
pub struct App {
    pub focus: Focus,
    /// The active top-level view (#0033). `Mail` is the full email client;
    /// `Contacts` (#0033) and `Calendar` (#0034) are the two content panes.
    /// Key dispatch and rendering branch on this; the mail-specific proxy
    /// fields below are the active `MailView` projected flat (parked in
    /// `mail_view` on switch).
    pub view: View,
    /// Parked mail-view state, restored when the user switches back to `Mail`
    /// (mirrors the `AccountState` proxy pattern; see `MailView`).
    pub mail_view: MailView,
    /// Contacts view state (#0033): read-only list + fuzzy search + detail
    /// pane over the local contacts index. Loaded lazily on first switch.
    pub contacts_view: ContactsView,
    /// Calendar view state (#0034): a local-first agenda over the events the
    /// iMIP traffic already produced. Loaded lazily on first switch.
    pub calendar_view: CalendarView,
    pub running: bool,
    pub terminal_width: u16,
    pub terminal_height: u16,

    // Multi-account
    pub accounts: Vec<AccountState>,
    pub active_account: usize,

    // --- Fields proxied from active account (kept in sync) ---
    pub mailboxes: Vec<MailboxInfo>,
    pub sidebar_index: usize,
    pub active_mailbox: usize,
    pub mailbox_counts: Vec<usize>,
    /// Full entry list of the active mailbox, shared with the cache slot
    /// (P2). Never filtered: the search view is expressed by `visible`.
    /// Mutate only through `with_emails_mut`.
    pub emails: Arc<Vec<EmailEntry>>,
    /// Indices into `emails` forming the current view (search-filtered,
    /// or all entries when no search is active). `list_index` indexes
    /// into THIS vec; rendering, navigation and selection all go through
    /// it. Invariant: `visible == filter(search_query, emails)` -- every
    /// reassignment of `emails` must call `rebuild_visible`.
    pub visible: Vec<usize>,
    pub list_index: usize,
    /// The armed leader prefix, if any. Two leaders exist (#0033 follow-up):
    /// `' '` (Space) arms the view switcher (`Space m/c/a`), `'g'` arms the
    /// list-scoped `gg`/`G` jumps. `None` when no leader is pending. A pressed
    /// continuation only fires the row whose `prefix` matches, so the two
    /// leaders never cross-arm.
    pub pending_prefix: Option<char>,
    pub headers_scroll: u16,
    pub preview_scroll: u16,
    pub selection: HashSet<MessageRef>,
    pub email_cache: Vec<Option<Arc<Vec<EmailEntry>>>>,
    pub search_query: String,
    pub search_includes_body: bool,
    pub watcher_active: bool,
    pub bg_mutations: usize,
    pub imap_config: Option<crate::config::ImapConfig>,
    pub smtp_config: Option<crate::config::SmtpConfig>,
    pub graph_config: Option<crate::config::GraphConfig>,
    pub signature_content: Option<String>,
    pub sent_dir: Option<PathBuf>,
    pub archive_dir: Option<PathBuf>,
    pub archive_server_name: String,
    pub drafts_dir: Option<PathBuf>,
    pub inbox_dir: Option<PathBuf>,
    pub account_config: crate::config::AccountConfig,

    // --- Global state ---
    pub pending_actions: VecDeque<Action>,
    /// The single active modal overlay (#0032). Exactly one overlay is
    /// renderable at a time by construction; `Overlay::None` is the normal
    /// mail view.
    pub overlay: Overlay,
    /// A persistent error that arrived (from a background result) while
    /// another overlay was already open (#0032). We do not clobber the
    /// active overlay; instead we surface a transient status-line notice
    /// and hold the error here, promoting it to `Overlay::Error` the
    /// moment the active overlay closes (see `promote_pending_error`).
    pub pending_error: Option<PersistentError>,
    pub status_message: Option<String>,
    pub status_ticks: u8,
    pub help_scroll: u16,
    pub help_filter: String,
    pub help_filter_active: bool,

    pub bg_count: usize,
    pub bg_spin_tick: usize,
    /// Monotonic counter guarding background mailbox loads (P1 step 2).
    /// Every `request_mailbox_load` bumps it and stamps the spawned walk;
    /// a `BgResult::MailboxLoaded` whose stamp no longer matches (the user
    /// switched account/mailbox, requested a newer reload, or mutated the
    /// list optimistically) is dropped in `tui/bg.rs`.
    pub mailbox_load_generation: u64,
    pub queued_action: Option<Action>,
    pub last_save_dir: Option<PathBuf>,

    pub status_log: VecDeque<StatusEntry>,
    pub show_activity_log: bool,

    // Activity log overlay scratch state (overlay presence is Overlay::Activity)
    pub activity_filter: String,
    pub activity_filter_active: bool,
    pub activity_scroll: u16,

    // Server search overlay scratch state (overlay presence is Overlay::Search)
    pub server_search_query: String,
    pub server_search_focus: SearchOverlayFocus,
    pub server_search_results: Vec<SearchResultEntry>,
    pub server_search_index: usize,
    pub server_search_headers_scroll: u16,
    pub server_search_scroll: u16,
    pub server_search_loading: bool,
    pub server_search_status: Option<String>,
    pub server_search_scope_label: String,

    // Config (loaded once at startup)
    pub global_config: crate::config::GlobalConfig,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let global_config = crate::config::load_global_config().unwrap_or_default();

        // Select the TUI theme once, before the first frame. Unknown names
        // warn (surfaced in the activity log below) and fall back to the
        // default theme instead of failing.
        let theme_warning = super::theme::init(&global_config.theme);

        let accounts: Vec<AccountState> = global_config
            .accounts
            .iter()
            .map(|ac| AccountState::new(ac.clone(), &global_config.email))
            .collect();

        let mut app = Self {
            focus: Focus::List,
            view: View::Mail,
            mail_view: MailView::default(),
            contacts_view: ContactsView::default(),
            calendar_view: CalendarView::default(),
            running: true,
            terminal_width: 0,
            terminal_height: 0,
            accounts,
            active_account: 0,
            mailboxes: Vec::new(),
            sidebar_index: 0,
            active_mailbox: 0,
            mailbox_counts: Vec::new(),
            emails: Arc::new(Vec::new()),
            visible: Vec::new(),
            list_index: 0,
            pending_prefix: None,
            headers_scroll: 0,
            preview_scroll: 0,
            selection: HashSet::new(),
            email_cache: Vec::new(),
            search_query: String::new(),
            search_includes_body: false,
            watcher_active: false,
            bg_mutations: 0,
            imap_config: None,
            smtp_config: None,
            graph_config: None,
            signature_content: None,
            sent_dir: None,
            archive_dir: None,
            archive_server_name: "Archive".to_string(),
            drafts_dir: None,
            inbox_dir: None,
            account_config: crate::config::AccountConfig::default(),
            pending_actions: VecDeque::new(),
            overlay: Overlay::None,
            pending_error: None,
            status_message: None,
            status_ticks: 0,
            help_scroll: 0,
            help_filter: String::new(),
            help_filter_active: false,
            bg_count: 0,
            bg_spin_tick: 0,
            mailbox_load_generation: 0,
            queued_action: None,
            last_save_dir: None,
            status_log: VecDeque::new(),
            show_activity_log: true,
            activity_filter: String::new(),
            activity_filter_active: false,
            activity_scroll: 0,
            server_search_query: String::new(),
            server_search_focus: SearchOverlayFocus::Input,
            server_search_results: Vec::new(),
            server_search_index: 0,
            server_search_headers_scroll: 0,
            server_search_scroll: 0,
            server_search_loading: false,
            server_search_status: None,
            server_search_scope_label: "All".to_string(),
            global_config,
        };

        app.load_from_account(0);
        if !app.mailboxes.is_empty() {
            let account_name = app.account_config.name.clone();
            let loaded = Arc::new(load_emails(
                &account_name,
                &mailbox_key(&app.mailboxes[0]),
            ));
            app.email_cache[0] = Some(Arc::clone(&loaded));
            app.emails = loaded;
            app.rebuild_visible();
        }

        if let Some(warning) = theme_warning {
            app.push_status(warning, StatusLevel::Warning);
        }

        app
    }

    /// Bare App for unit tests: no config load, no directory walks, no
    /// accounts. Tests populate `emails` / `email_cache` / `mailboxes`
    /// directly.
    #[cfg(test)]
    pub(crate) fn default_for_tests() -> Self {
        Self {
            focus: Focus::List,
            view: View::Mail,
            mail_view: MailView::default(),
            contacts_view: ContactsView::default(),
            calendar_view: CalendarView::default(),
            running: true,
            terminal_width: 0,
            terminal_height: 0,
            accounts: Vec::new(),
            active_account: 0,
            mailboxes: Vec::new(),
            sidebar_index: 0,
            active_mailbox: 0,
            mailbox_counts: Vec::new(),
            emails: Arc::new(Vec::new()),
            visible: Vec::new(),
            list_index: 0,
            pending_prefix: None,
            headers_scroll: 0,
            preview_scroll: 0,
            selection: HashSet::new(),
            email_cache: Vec::new(),
            search_query: String::new(),
            search_includes_body: false,
            watcher_active: false,
            bg_mutations: 0,
            imap_config: None,
            smtp_config: None,
            graph_config: None,
            signature_content: None,
            sent_dir: None,
            archive_dir: None,
            archive_server_name: "Archive".to_string(),
            drafts_dir: None,
            inbox_dir: None,
            account_config: crate::config::AccountConfig::default(),
            pending_actions: VecDeque::new(),
            overlay: Overlay::None,
            pending_error: None,
            status_message: None,
            status_ticks: 0,
            help_scroll: 0,
            help_filter: String::new(),
            help_filter_active: false,
            bg_count: 0,
            bg_spin_tick: 0,
            mailbox_load_generation: 0,
            queued_action: None,
            last_save_dir: None,
            status_log: VecDeque::new(),
            show_activity_log: true,
            activity_filter: String::new(),
            activity_filter_active: false,
            activity_scroll: 0,
            server_search_query: String::new(),
            server_search_focus: SearchOverlayFocus::Input,
            server_search_results: Vec::new(),
            server_search_index: 0,
            server_search_headers_scroll: 0,
            server_search_scroll: 0,
            server_search_loading: false,
            server_search_status: None,
            server_search_scope_label: "All".to_string(),
            global_config: crate::config::GlobalConfig::default(),
        }
    }

    // ---------------------------------------------------------------
    // Account state sync
    // ---------------------------------------------------------------

    pub(crate) fn save_to_account(&mut self) {
        let cursor_ref = self.cursor_anchor();
        if let Some(acct) = self.accounts.get_mut(self.active_account) {
            acct.sidebar_index = self.sidebar_index;
            acct.active_mailbox = self.active_mailbox;
            acct.mailbox_counts = self.mailbox_counts.clone();
            acct.list_index = self.list_index;
            acct.cursor_ref = cursor_ref;
            acct.headers_scroll = self.headers_scroll;
            acct.preview_scroll = self.preview_scroll;
            acct.selection = self.selection.clone();
            acct.email_cache = self.email_cache.clone();
            acct.search_query = self.search_query.clone();
            acct.search_includes_body = self.search_includes_body;
            acct.bg_mutations = self.bg_mutations;
            acct.watcher_active = self.watcher_active;
        }
    }

    pub(crate) fn load_from_account(&mut self, idx: usize) {
        if let Some(acct) = self.accounts.get(idx) {
            self.mailboxes = acct.mailboxes.clone();
            self.sidebar_index = acct.sidebar_index;
            self.active_mailbox = acct.active_mailbox;
            self.mailbox_counts = acct.mailbox_counts.clone();
            self.list_index = acct.list_index;
            self.headers_scroll = acct.headers_scroll;
            self.preview_scroll = acct.preview_scroll;
            self.selection = acct.selection.clone();
            self.email_cache = acct.email_cache.clone();
            self.search_query = acct.search_query.clone();
            self.search_includes_body = acct.search_includes_body;
            self.watcher_active = acct.watcher_active;
            self.bg_mutations = acct.bg_mutations;
            self.imap_config = acct.imap_config.clone();
            self.smtp_config = acct.smtp_config.clone();
            self.graph_config = acct.graph_config.clone();
            self.signature_content = acct.signature_content.clone();
            self.sent_dir = acct.sent_dir.clone();
            self.archive_dir = acct.archive_dir.clone();
            self.archive_server_name = acct.archive_server_name.clone();
            self.drafts_dir = acct.drafts_dir.clone();
            self.inbox_dir = acct.inbox_dir.clone();
            self.account_config = acct.account_config.clone();
        }
    }

    // ---------------------------------------------------------------
    // View switching (#0033)
    // ---------------------------------------------------------------

    /// Switch the active top-level view. Parks the current mail-view proxy
    /// state on the way out and restores it on the way back in (mirroring the
    /// account save/load pattern). No-op when already on `target`.
    ///
    /// `Mail` and the two content views (`Contacts`, `Calendar`) each own their
    /// state; switching into one lazily loads it on the first visit.
    pub fn switch_view(&mut self, target: View) {
        if target == self.view {
            return;
        }
        // Any pending leader chord is consumed by the switch.
        self.pending_prefix = None;
        if self.view == View::Mail {
            self.save_to_mail_view();
        }
        self.view = target;
        if target == View::Mail {
            self.load_from_mail_view();
        }
        if target == View::Contacts {
            self.ensure_contacts_loaded();
        }
        if target == View::Calendar {
            self.ensure_calendar_loaded();
        }
    }

    /// Park the mail view's transient proxy state (mirrors `save_to_account`).
    fn save_to_mail_view(&mut self) {
        self.mail_view.focus = self.focus;
    }

    /// Restore the mail view's transient proxy state (mirrors
    /// `load_from_account`).
    fn load_from_mail_view(&mut self) {
        self.focus = self.mail_view.focus;
    }

    // -- Contacts view (#0033) -------------------------------------------

    /// Lazily load the active account's contact index from the on-disk cache
    /// the first time the Contacts view is shown. The cache load is a single
    /// JSON read (instant); a full rebuild is only triggered by the manual
    /// refresh key. Mirrors the compose wizard's `load_cache` precedent, so the
    /// UI thread is never blocked by a mailbox walk here.
    pub fn ensure_contacts_loaded(&mut self) {
        if self.contacts_view.loaded {
            return;
        }
        let root = crate::config::account_dir(&self.account_config.name);
        self.contacts_view.index = crate::contacts::load_cache(&root).ok().flatten();
        self.contacts_view.loaded = true;
        self.contacts_view.list_index = 0;
        self.recompute_contact_matches();
    }

    /// Force the contact index to reload from the active account's cache
    /// (invoked when the account changes while Contacts state is already
    /// loaded, so the pane never shows a stale account's contacts).
    pub fn reset_contacts_view(&mut self) {
        self.contacts_view = ContactsView::default();
    }

    /// Rebuild the active account's contact index from its mailboxes and
    /// persist the cache (manual refresh key). The index build is a fast
    /// bounded walk (~100 ms even on the largest local account), so it runs
    /// synchronously; failures surface as a status message and leave the
    /// previously-loaded index intact.
    pub fn refresh_contacts(&mut self) {
        match crate::contacts::build_index_for_account(&self.account_config) {
            Ok(index) => {
                let root = crate::config::account_dir(&self.account_config.name);
                if let Err(e) = crate::contacts::save_cache(&root, &index) {
                    self.set_status_level(
                        format!("Contacts cache save failed: {e}"),
                        StatusLevel::Error,
                    );
                }
                let count = index.contacts.len();
                self.contacts_view.index = Some(index);
                self.contacts_view.loaded = true;
                self.recompute_contact_matches();
                self.set_status(format!("Contacts refreshed ({count})"));
            }
            Err(e) => {
                self.set_status_level(
                    format!("Contacts refresh failed: {e}"),
                    StatusLevel::Error,
                );
            }
        }
    }

    /// Recompute the fuzzy-matched address list for the current query, clamping
    /// the cursor. Called after any query edit, index (re)load, or refresh.
    pub fn recompute_contact_matches(&mut self) {
        let query = self.contacts_view.query.clone();
        let matches: Vec<String> = match &self.contacts_view.index {
            Some(index) => crate::contacts::search(index, &query, usize::MAX)
                .into_iter()
                .map(|m| m.contact.address.clone())
                .collect(),
            None => Vec::new(),
        };
        self.contacts_view.matches = matches;
        let len = self.contacts_view.matches.len();
        if len == 0 {
            self.contacts_view.list_index = 0;
        } else if self.contacts_view.list_index >= len {
            self.contacts_view.list_index = len - 1;
        }
    }

    /// The `Contact` currently selected in the Contacts list, if any.
    pub fn selected_contact(&self) -> Option<&crate::contacts::Contact> {
        let index = self.contacts_view.index.as_ref()?;
        let addr = self.contacts_view.matches.get(self.contacts_view.list_index)?;
        index.contacts.get(addr)
    }

    // -- Calendar view (#0034) -------------------------------------------

    /// Lazily build the active account's agenda the first time the Calendar
    /// view is shown. The walk is the same one `mp calendar rebuild` performs
    /// (measured at ~100 ms on the largest local account), so it runs
    /// synchronously on the UI thread like the Contacts cache load.
    pub fn ensure_calendar_loaded(&mut self) {
        if self.calendar_view.loaded {
            return;
        }
        self.calendar_view.events = match self.calendar_account_root() {
            Some(root) => calendar_view::load_events_for_account(&root),
            None => Vec::new(),
        };
        self.calendar_view.loaded = true;
        self.calendar_view.list_index = 0;
        self.recompute_calendar_visible();
    }

    /// The account root to walk for events, or `None` when no account is
    /// configured. Guarded on a non-empty name because `account_dir("")` is the
    /// shared `accounts/` parent -- walking it would mix every account's events
    /// into one agenda.
    fn calendar_account_root(&self) -> Option<PathBuf> {
        let name = self.account_config.name.trim();
        if name.is_empty() {
            return None;
        }
        Some(crate::config::account_dir(name))
    }

    /// Drop the loaded agenda (events are per-account, so the view reloads
    /// lazily for the newly-active account).
    pub fn reset_calendar_view(&mut self) {
        self.calendar_view = CalendarView::default();
    }

    /// Re-walk the account for events (manual refresh key).
    pub fn refresh_calendar(&mut self) {
        self.calendar_view.events = match self.calendar_account_root() {
            Some(root) => calendar_view::load_events_for_account(&root),
            None => Vec::new(),
        };
        self.calendar_view.loaded = true;
        self.recompute_calendar_visible();
        let count = self.calendar_view.visible.len();
        self.set_status(format!("Calendar refreshed ({count} events)"));
    }

    /// Recompute the visible agenda rows for the current scope, clamping the
    /// cursor. Upcoming-only by default: an event stays visible until its end
    /// (or its start, when the end is unknown) is in the past. Undated events
    /// are always listed — they cannot be placed on the timeline, so hiding
    /// them would silently lose data.
    pub fn recompute_calendar_visible(&mut self) {
        let now = calendar_view::now_sort_key();
        let show_past = self.calendar_view.show_past;
        self.calendar_view.visible = self
            .calendar_view
            .events
            .iter()
            .enumerate()
            .filter(|(_, ev)| {
                if show_past || ev.start_sort.is_empty() {
                    return true;
                }
                let horizon = if ev.end_sort.is_empty() {
                    &ev.start_sort
                } else {
                    &ev.end_sort
                };
                horizon.as_str() >= now.as_str()
            })
            .map(|(i, _)| i)
            .collect();
        let len = self.calendar_view.visible.len();
        if len == 0 {
            self.calendar_view.list_index = 0;
        } else if self.calendar_view.list_index >= len {
            self.calendar_view.list_index = len - 1;
        }
    }

    /// The agenda row currently under the Calendar cursor, if any.
    pub fn selected_event(&self) -> Option<&CalendarEvent> {
        let idx = *self
            .calendar_view
            .visible
            .get(self.calendar_view.list_index)?;
        self.calendar_view.events.get(idx)
    }

    pub fn active_kind(&self) -> MailboxKind {
        self.mailboxes
            .get(self.active_mailbox)
            .map(|m| m.kind)
            .unwrap_or(MailboxKind::Inbox)
    }

    pub fn active_label(&self) -> &str {
        self.mailboxes
            .get(self.active_mailbox)
            .map(|m| m.label.as_str())
            .unwrap_or("Mail")
    }

    /// The keymap context the next keystroke will be dispatched in, used by the
    /// mode/hint bar to show the live continuations from `KEYMAP`. Overlays
    /// take precedence over pane focus (mirroring `handle_key`'s dispatcher).
    pub fn key_context(&self) -> Option<KeyCtx> {
        match &self.overlay {
            Overlay::Search => Some(KeyCtx::ServerSearch),
            Overlay::Activity => Some(KeyCtx::Activity),
            Overlay::Help => Some(KeyCtx::Help),
            // Modal overlays (confirm / pickers / rsvp / error / compose) have
            // their own inline chips; the hint bar defers to them.
            Overlay::Confirm(_)
            | Overlay::Compose(_)
            | Overlay::Attachment(_)
            | Overlay::Dir(_)
            | Overlay::Mailbox(_)
            | Overlay::Rsvp(_)
            | Overlay::Error(_) => None,
            // Contacts view (#0033): the list pane owns the hint bar (unless the
            // fuzzy-search input is armed, which is free-text — no hint row).
            Overlay::None if self.view == View::Contacts => {
                if self.contacts_view.searching {
                    None
                } else {
                    Some(KeyCtx::Contacts)
                }
            }
            // Calendar view (#0034): the agenda list owns the hint bar.
            Overlay::None if self.view == View::Calendar => Some(KeyCtx::Calendar),
            // Any other non-Mail view has no pane; only the view-agnostic Global
            // surface is live, so the hint bar shows Global (filtered to
            // view-agnostic bindings in the renderer).
            Overlay::None if self.view != View::Mail => Some(KeyCtx::Global),
            Overlay::None => match self.focus {
                Focus::Sidebar => Some(KeyCtx::Sidebar),
                Focus::List => Some(KeyCtx::List),
                Focus::Headers => Some(KeyCtx::Headers),
                Focus::Preview => Some(KeyCtx::Preview),
                // Metadata search input / compose field editing: no hint row.
                Focus::Search | Focus::ComposeWizard => None,
            },
        }
    }

    /// The pending leader prefix, if any (`' '` for the Space view switcher,
    /// `'g'` for the list `gg`/`G` jumps). Drives the hint bar's continuation
    /// view.
    pub fn pending_prefix(&self) -> Option<char> {
        self.pending_prefix
    }

    pub fn active_dir(&self) -> Option<&PathBuf> {
        self.mailboxes.get(self.active_mailbox).map(|m| &m.dir)
    }

    pub fn active_server_mailbox(&self) -> String {
        self.mailboxes
            .get(self.active_mailbox)
            .and_then(|m| m.server_name.clone())
            .unwrap_or_else(|| "INBOX".to_string())
    }

    pub fn account_name(&self) -> &str {
        &self.account_config.name
    }

    pub fn is_graph(&self) -> bool {
        self.graph_config.is_some()
            && self.account_config.auth_method == crate::config::AuthMethod::Graph
    }

    pub fn account_index_by_from(&self, from: &str) -> usize {
        let lower = from.to_lowercase();
        self.accounts
            .iter()
            .position(|acct| lower.contains(&acct.account_config.default_from.to_lowercase()))
            .unwrap_or(self.active_account)
    }

    pub fn all_search_targets(&self) -> Vec<SearchTarget> {
        self.mailboxes
            .iter()
            .filter(|m| m.server_name.is_some())
            .map(|m| SearchTarget {
                server_name: m.server_name.clone().expect("filtered for is_some"),
                local_dir: m.dir.clone(),
                status: kind_to_status(m.kind),
                label: m.label.clone(),
            })
            .collect()
    }

    pub fn search_target_by_name(&self, name: &str) -> Option<SearchTarget> {
        let lower = name.to_lowercase();
        self.mailboxes
            .iter()
            .find(|m| {
                m.server_name
                    .as_ref()
                    .is_some_and(|s| s.to_lowercase() == lower)
                    || m.label.to_lowercase() == lower
            })
            .and_then(|m| {
                Some(SearchTarget {
                    server_name: m.server_name.clone()?,
                    local_dir: m.dir.clone(),
                    status: kind_to_status(m.kind),
                    label: m.label.clone(),
                })
            })
    }

    pub fn mailbox_index_for_dir(&self, dir: &Path) -> Option<usize> {
        self.mailboxes.iter().position(|m| m.dir == dir)
    }

    pub fn find_mailbox_by_kind(&self, kind: MailboxKind) -> Option<usize> {
        self.mailboxes.iter().position(|m| m.kind == kind)
    }

    pub fn switch_account(&mut self, idx: usize) {
        if idx >= self.accounts.len() || idx == self.active_account {
            return;
        }
        self.save_to_account();
        self.active_account = idx;
        self.accounts[idx].has_unseen = false;
        self.load_from_account(idx);
        // Contacts are per-account; drop the previous account's index so the
        // Contacts view reloads lazily for the newly-active account (#0033).
        self.reset_contacts_view();
        if self.view == View::Contacts {
            self.ensure_contacts_loaded();
        }
        // Events are per-account too (#0034): drop them so the agenda reloads
        // for the newly-active account.
        self.reset_calendar_view();
        if self.view == View::Calendar {
            self.ensure_calendar_loaded();
        }
        // The cursor identity to restore is the INCOMING account's own,
        // saved when it was last parked -- the outgoing account's path
        // can never appear in this account's list.
        let anchor = self
            .accounts
            .get(idx)
            .and_then(|acct| acct.cursor_ref);
        let am = self.active_mailbox;
        if let Some(cached) = self.email_cache.get(am).and_then(|c| c.as_ref()) {
            self.emails = Arc::clone(cached);
        } else if let Some(mb) = self.mailboxes.get(am) {
            // Cache miss: same off-thread load as `switch_mailbox` (P1
            // step 2) -- show an empty list + loading status until the
            // new account's entries arrive via `BgResult::MailboxLoaded`.
            let label = mb.label.clone();
            self.emails = Arc::new(Vec::new());
            self.set_status_level(format!("Loading {label}..."), StatusLevel::Progress);
            self.request_mailbox_load(am);
        } else {
            self.emails = Arc::new(Vec::new());
        }
        // Reapply the restored account's search query to the fresh list.
        // (Pre-P2, the raw cache was restored even with a saved query, so
        // the filter was silently lost on switch-back; now it survives.)
        self.rebuild_visible();
        // Put the cursor back on the email it sat on when this account was
        // parked; fall back to the saved index, clamped to the filtered
        // view. On a cache miss the list is empty here and the async
        // `BgResult::MailboxLoaded` arm has nothing to anchor on, so the
        // cursor lands at row 0 once the entries arrive.
        self.restore_cursor(anchor, self.list_index);
        self.focus = Focus::List;
    }

    pub fn update(&mut self, msg: Message) -> Option<Message> {
        match msg {
            Message::Key(key) => self.handle_key(key),
            Message::Resize(w, h) => {
                self.terminal_width = w;
                self.terminal_height = h;
                None
            }
            Message::MailboxChanged { account_index } => {
                if account_index == self.active_account {
                    self.push_action_dedup(Action::Fetch);
                } else if let Some(acct) = self.accounts.get_mut(account_index) {
                    acct.has_unseen = true;
                }
                None
            }
            Message::Quit => {
                self.running = false;
                None
            }
        }
    }

    pub fn push_status(&mut self, message: String, level: StatusLevel) {
        if self.status_log.len() >= STATUS_LOG_CAPACITY {
            self.status_log.pop_front();
        }
        self.status_log.push_back(StatusEntry {
            timestamp: chrono::Local::now(),
            message,
            level,
        });
    }

    pub fn set_status(&mut self, msg: String) {
        self.push_status(msg.clone(), StatusLevel::Info);
        self.status_message = Some(msg);
        self.status_ticks = 12;
    }

    pub fn set_status_level(&mut self, msg: String, level: StatusLevel) {
        self.push_status(msg.clone(), level);
        self.status_message = Some(msg);
        self.status_ticks = 12;
    }

    pub fn tick_status(&mut self) {
        if self.bg_count > 0 {
            return;
        }
        if self.status_ticks > 0 {
            self.status_ticks -= 1;
            if self.status_ticks == 0 {
                self.status_message = None;
            }
        }
    }

    pub fn selected_email(&self) -> Option<&EmailEntry> {
        self.visible
            .get(self.list_index)
            .and_then(|&i| self.emails.get(i))
    }

    /// Iterate the entries of the current (filtered) view in display
    /// order. Position `i` in this iterator corresponds to `list_index
    /// == i`.
    pub fn visible_emails(&self) -> impl Iterator<Item = &EmailEntry> {
        self.visible.iter().filter_map(|&i| self.emails.get(i))
    }

    /// Capture the cursor's stable identity before a list rebuild.
    ///
    /// `list_index` is a bare position into `visible`, so any rebuild that
    /// re-sorts or grows the entry list silently moves the cursor to a
    /// different email (a draft approved at the top of Drafts, new inbox
    /// mail shifting everything down). The `MessageRef` is the stable key
    /// of an entry (`selection`, `set_email_read` key on it too), so we
    /// anchor on it and restore with `restore_cursor`. An entry with no
    /// store row cannot be anchored and falls back to the index.
    pub(crate) fn cursor_anchor(&self) -> Option<MessageRef> {
        self.selected_email().and_then(|e| e.msg)
    }

    /// Restore the cursor to `anchor` after `visible` was rebuilt. Falls
    /// back to the clamped `fallback` index when the anchored email is
    /// gone (archived, deleted, filtered out, moved to another mailbox).
    pub(crate) fn restore_cursor(&mut self, anchor: Option<MessageRef>, fallback: usize) {
        if self.visible.is_empty() {
            self.list_index = 0;
            return;
        }
        if let Some(m) = anchor {
            if let Some(pos) = self
                .visible
                .iter()
                .position(|&i| self.emails.get(i).is_some_and(|e| e.msg == Some(m)))
            {
                self.list_index = pos;
                return;
            }
        }
        self.list_index = fallback.min(self.visible.len() - 1);
    }

    /// Recompute `visible` from scratch: apply the active search query
    /// to the full entry list. Must be called after every reassignment
    /// or structural mutation of `self.emails` so the view never holds
    /// dangling indices.
    pub(crate) fn rebuild_visible(&mut self) {
        self.visible = keys::filter_visible(
            &self.emails,
            &self.search_query,
            self.active_kind(),
            self.search_includes_body,
        );
    }

    /// Run a mutation against the full entry list of the active mailbox,
    /// keeping the cache slot in sync (P2 mutation strategy).
    ///
    /// The cache slot's strong reference is released first, so when the
    /// slot and `self.emails` share the allocation (the normal case)
    /// `Arc::make_mut` mutates in place instead of deep-cloning the
    /// whole Vec. A deep clone only happens when another strong ref
    /// still shares the allocation (e.g. the mirrored
    /// `AccountState::email_cache` from the last account switch) -- at
    /// most once per such sharing. If the slot was `None` (invalidated /
    /// load in flight) it stays `None`, matching the old semantics of
    /// skipping the cache update.
    pub(crate) fn with_emails_mut<R>(
        &mut self,
        f: impl FnOnce(&mut Vec<EmailEntry>) -> R,
    ) -> R {
        let slot_populated = self
            .email_cache
            .get(self.active_mailbox)
            .is_some_and(|c| c.is_some());
        if slot_populated {
            if let Some(slot) = self.email_cache.get_mut(self.active_mailbox) {
                *slot = None;
            }
        }
        let r = f(Arc::make_mut(&mut self.emails));
        if slot_populated {
            if let Some(slot) = self.email_cache.get_mut(self.active_mailbox) {
                *slot = Some(Arc::clone(&self.emails));
            }
        }
        r
    }

    /// Optimistically set the read flag of one entry (by message ref) in
    /// both the in-memory list and the cache slot. No-op if the message is
    /// not in the active mailbox's list.
    pub(crate) fn set_email_read(&mut self, msg: MessageRef, read: bool) {
        self.with_emails_mut(|entries| {
            if let Some(e) = entries.iter_mut().find(|e| e.msg == Some(msg)) {
                e.read = read;
            }
        });
    }

    /// The `MessageRef` of the cursor email, when it has a store row.
    pub fn selected_email_ref(&self) -> Option<MessageRef> {
        self.selected_email().and_then(|e| e.msg)
    }

    pub fn remove_selected_from_list(&mut self) -> Option<MessageRef> {
        let msg = self.selected_email()?.msg?;
        let fallback = self.list_index;
        self.with_emails_mut(|entries| entries.retain(|e| e.msg != Some(msg)));
        self.invalidate_pending_mailbox_loads();

        // Underlying indices shifted -- recompute the view, then park the
        // cursor on the row that took the removed one's place (the "next
        // row" behaviour), clamped to the shortened view. The removed
        // email is gone by definition, so there is nothing to anchor on.
        self.rebuild_visible();
        self.restore_cursor(None, fallback);

        if let Some(count) = self.mailbox_counts.get_mut(self.active_mailbox) {
            *count = self.emails.len();
        }

        self.headers_scroll = 0;
        self.preview_scroll = 0;

        Some(msg)
    }

    pub fn remove_selected_from_list_batch(
        &mut self,
        msgs: &HashSet<MessageRef>,
    ) -> Vec<MessageRef> {
        let removed: Vec<MessageRef> = self
            .emails
            .iter()
            .filter_map(|e| e.msg)
            .filter(|m| msgs.contains(m))
            .collect();

        // The cursor's own row may or may not be part of the batch. Anchor
        // on it when it survives; otherwise fall back to the number of
        // surviving rows ABOVE the old cursor, so removing rows above it
        // does not drag the cursor down the list.
        let anchor = self.cursor_anchor().filter(|m| !msgs.contains(m));
        let fallback = self
            .visible_emails()
            .take(self.list_index)
            .filter(|e| !e.msg.is_some_and(|m| msgs.contains(&m)))
            .count();

        self.with_emails_mut(|entries| {
            entries.retain(|e| !e.msg.is_some_and(|m| msgs.contains(&m)))
        });
        self.invalidate_pending_mailbox_loads();

        self.rebuild_visible();
        self.restore_cursor(anchor, fallback);

        if let Some(count) = self.mailbox_counts.get_mut(self.active_mailbox) {
            *count = self.emails.len();
        }

        self.headers_scroll = 0;
        self.preview_scroll = 0;

        removed
    }

    /// Surface a persistent error that requires explicit dismissal.
    ///
    /// When no overlay is open this sets `Overlay::Error` directly, as
    /// before. When another overlay is already active (this is reachable
    /// from background results in `bg.rs`, which run regardless of overlay
    /// state), clobbering it would destroy the overlay's unsaved state and,
    /// for the compose wizard, leave `focus == Focus::ComposeWizard` while
    /// `overlay == None` -- a state that panics on the next keystroke
    /// (`keys.rs` `unreachable!()`). Instead we keep the overlay, show an
    /// immediate status-line notice with the error's first line, and stash
    /// the full error in `pending_error`; it is promoted to `Overlay::Error`
    /// the moment the active overlay closes (`promote_pending_error`).
    pub fn set_persistent_error(&mut self, msg: String) {
        if self.overlay.is_active() {
            // Short, single-line notice so the failure is not invisible
            // while the overlay stays open. The full multi-line message is
            // preserved for the promoted error overlay.
            let notice = msg.lines().next().unwrap_or(&msg).to_string();
            self.set_status_level(notice, StatusLevel::Error);
            self.pending_error = Some(PersistentError { message: msg });
        } else {
            self.overlay = Overlay::Error(PersistentError { message: msg });
        }
    }

    /// Promote a `pending_error` (queued by `set_persistent_error` while an
    /// overlay was open) to a real `Overlay::Error`, but only once the
    /// overlay has actually closed to `Overlay::None`. Callers invoke this
    /// at every overlay-close site (via `close_overlay`, or directly after a
    /// consume-and-close `mem::replace`). Guarding on `Overlay::None` keeps
    /// the error from firing during an overlay->overlay handoff (e.g. the
    /// attachment picker -> dir picker transition), which momentarily takes
    /// the source overlay before opening the next one.
    pub fn promote_pending_error(&mut self) {
        if matches!(self.overlay, Overlay::None) {
            if let Some(err) = self.pending_error.take() {
                self.overlay = Overlay::Error(err);
            }
        }
    }

    // ---------------------------------------------------------------
    // Overlay accessors (#0032)
    //
    // Typed borrows into the single `overlay` field, so handlers and
    // renderers keep their original shape (`if let Some(picker) = ...`).
    // Returns `None` when a different overlay (or none) is active.
    // ---------------------------------------------------------------

    pub fn compose_wizard(&self) -> Option<&ComposeWizard> {
        match &self.overlay {
            Overlay::Compose(w) => Some(w),
            _ => None,
        }
    }

    pub fn compose_wizard_mut(&mut self) -> Option<&mut ComposeWizard> {
        match &mut self.overlay {
            Overlay::Compose(w) => Some(w),
            _ => None,
        }
    }

    pub fn attachment_picker_mut(&mut self) -> Option<&mut AttachmentPicker> {
        match &mut self.overlay {
            Overlay::Attachment(p) => Some(p),
            _ => None,
        }
    }

    pub fn dir_picker_mut(&mut self) -> Option<&mut DirPicker> {
        match &mut self.overlay {
            Overlay::Dir(p) => Some(p),
            _ => None,
        }
    }

    pub fn mailbox_picker_mut(&mut self) -> Option<&mut MailboxPicker> {
        match &mut self.overlay {
            Overlay::Mailbox(p) => Some(p),
            _ => None,
        }
    }

    pub fn rsvp_overlay_mut(&mut self) -> Option<&mut RsvpOverlay> {
        match &mut self.overlay {
            Overlay::Rsvp(o) => Some(o),
            _ => None,
        }
    }

    /// Dismiss the active overlay, returning to the normal mail view.
    ///
    /// Single-sources overlay-close promotion: after closing, any error
    /// that arrived while the overlay was open (`pending_error`) is promoted
    /// to `Overlay::Error` so background failures are never silently lost.
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
        self.promote_pending_error();
    }

    pub fn invalidate_cache_idx(&mut self, idx: usize) {
        if let Some(slot) = self.email_cache.get_mut(idx) {
            *slot = None;
        }
    }

    pub fn invalidate_cache_idx_on(&mut self, account_index: usize, mailbox_idx: usize) {
        if let Some(acct) = self.accounts.get_mut(account_index) {
            if let Some(slot) = acct.email_cache.get_mut(mailbox_idx) {
                *slot = None;
            }
        }
    }

    pub fn invalidate_all_caches(&mut self) {
        for slot in &mut self.email_cache {
            *slot = None;
        }
    }

    pub fn invalidate_all_caches_on(&mut self, account_index: usize) {
        if let Some(acct) = self.accounts.get_mut(account_index) {
            for slot in &mut acct.email_cache {
                *slot = None;
            }
        }
    }

    /// Push an action to the queue.
    pub fn push_action(&mut self, action: Action) {
        self.pending_actions.push_back(action);
    }

    /// Push an action only if no equivalent variant is already queued.
    /// Used for watcher-triggered fetches to avoid duplicates.
    pub fn push_action_dedup(&mut self, action: Action) {
        let dominated = self.pending_actions.iter().any(|a| {
            std::mem::discriminant(a) == std::mem::discriminant(&action)
        });
        if !dominated {
            self.pending_actions.push_back(action);
        }
    }

    pub fn reload_current_mailbox(&mut self) {
        let am = self.active_mailbox;
        let anchor = self.cursor_anchor();
        let fallback = self.list_index;
        self.invalidate_cache_idx(am);
        self.switch_mailbox(am);

        // The fresh entries arrive asynchronously via
        // `BgResult::MailboxLoaded`; the stale list stays visible
        // meanwhile (same-mailbox reload keeps it, see `switch_mailbox`).
        // Restore the cursor against that stale view so the UI stays
        // valid; the arrival handler re-anchors against the fresh list
        // (its own `cursor_anchor` call reads the cursor we set here) and
        // updates the mailbox count.
        self.restore_cursor(anchor, fallback);
    }

    /// Recount all mailbox sizes with one grouped query (#0038).
    /// Only needed after full sync/reconciliation that moves emails between mailboxes.
    pub fn recount_all_mailboxes(&mut self) {
        self.mailbox_counts = count_all_emails(&self.account_config.name, &self.mailboxes);
    }

    pub(crate) fn switch_mailbox(&mut self, idx: usize) {
        let changing = self.active_mailbox != idx;
        self.active_mailbox = idx;
        if changing {
            self.selection.clear();
            self.search_query.clear();
            self.search_includes_body = false;
        }

        if let Some(cached) = self.email_cache.get(idx).and_then(|c| c.as_ref()) {
            self.emails = Arc::clone(cached);
            self.rebuild_visible();
            if let Some(count) = self.mailbox_counts.get_mut(idx) {
                *count = self.emails.len();
            }
        } else if let Some(mb) = self.mailboxes.get(idx) {
            // Cache miss: walk the directory off the UI thread (P1 step 2).
            // The entries arrive via `BgResult::MailboxLoaded`; the mailbox
            // count is updated then.
            if changing {
                let label = mb.label.clone();
                // The user expects the NEW mailbox's content -- show an
                // empty list with the existing bg spinner (same loading
                // indication as `BgResult::IndexReady`) rather than the
                // previous mailbox's entries.
                self.emails = Arc::new(Vec::new());
                self.rebuild_visible();
                self.set_status_level(
                    format!("Loading {label}..."),
                    StatusLevel::Progress,
                );
            }
            // else: same-mailbox reload -- keep the stale list (and its
            // view) visible until the fresh entries arrive (no flicker,
            // no empty state).
            self.request_mailbox_load(idx);
        } else {
            self.emails = Arc::new(Vec::new());
            self.rebuild_visible();
        }

        if changing {
            self.list_index = 0;
        }
    }

    /// Queue a background `load_emails` walk for mailbox `idx` of the
    /// active account (P1 step 2). Bumping the generation first makes any
    /// still-in-flight older walk stale, so out-of-order arrivals cannot
    /// clobber a newer result.
    fn request_mailbox_load(&mut self, idx: usize) {
        self.mailbox_load_generation = self.mailbox_load_generation.wrapping_add(1);
        self.push_action(Action::LoadMailbox {
            mailbox_idx: idx,
            generation: self.mailbox_load_generation,
        });
    }

    /// Drop any in-flight background mailbox load. Called after optimistic
    /// list mutations (archive/delete remove entries before the server
    /// confirms): a walk that started before the mutation could otherwise
    /// resurrect the removed email when it lands.
    fn invalidate_pending_mailbox_loads(&mut self) {
        self.mailbox_load_generation = self.mailbox_load_generation.wrapping_add(1);
    }
}
