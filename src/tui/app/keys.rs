use std::collections::HashSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    Action, App, AttachmentPicker, AttachmentPickerMode, ComposeField, ComposeMode,
    ComposeSuggestion, ComposeWizard, ConfirmAction, ConfirmDialog, DirPicker, DirPickerMode,
    EmailEntry, Focus, MailboxKind, MailboxPicker, Message, MessageRef, Overlay, RsvpChoice,
    RsvpOverlay, SearchBodies, SearchOverlayFocus, ThreadEntry, ThreadOverlay,
};

impl App {
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Message> {
        // Single overlay dispatcher (#0032): exactly one overlay is active by
        // construction, so this matches on `self.overlay` instead of the
        // former cascade of `is_some()` / bool guards. Arm order preserves the
        // historical guard precedence (it never actually mattered — overlays
        // were always mutually exclusive — but keeping it avoids any behavior
        // question).
        match &self.overlay {
            Overlay::Confirm(_) => return self.handle_confirm_key(key),
            Overlay::Error(_) => return self.handle_persistent_error_key(key),
            Overlay::Dir(_) => return self.handle_dir_picker_key(key),
            Overlay::Mailbox(_) => return self.handle_mailbox_picker_key(key),
            Overlay::Rsvp(_) => return self.handle_rsvp_overlay_key(key),
            Overlay::Thread(_) => return self.handle_thread_overlay_key(key),
            Overlay::Attachment(_) => return self.handle_attachment_picker_key(key),
            Overlay::Help => return self.handle_help_key(key),
            Overlay::Activity => return self.handle_activity_overlay_key(key),
            Overlay::Search => return self.handle_search_overlay_key(key),
            Overlay::Compose(_) => return self.handle_compose_wizard_key(key),
            Overlay::None => {}
        }

        if self.focus == Focus::Search {
            return self.handle_search_key(key);
        }

        // Jump-to-date input (#0017): armed by `g d`, it owns the keyboard
        // until Enter or Esc, the same hand-dispatched shape as the two
        // free-text inputs above.
        if self.jump_date_input.is_some() {
            return self.handle_jump_date_key(key);
        }

        // Contacts view fuzzy-search input (#0033): free-text, hand-dispatched
        // like the metadata-search input, once armed by `/`.
        if self.view == super::View::Contacts && self.contacts_view.searching {
            return self.handle_contacts_search_key(key);
        }

        // Normal-mode surface (#0032, (B)-lite): resolve the pressed key through
        // the KEYMAP table into a `KeyAction`, then run one executor. Global
        // bindings are always tried first, then the focused pane's context
        // (mirroring the old "global keys before pane dispatch" precedence).
        // Guards are evaluated against live state so context-sensitive rules
        // (`c` only in Drafts, most list keys only when non-empty, multi-account
        // account jumps) stay in the data model.
        self.dispatch_normal_mode(key)
    }

    /// Table-driven dispatch for the no-overlay surface. Returns the `Message`
    /// (only `Quit` today) or `None`.
    fn dispatch_normal_mode(&mut self, key: KeyEvent) -> Option<Message> {
        let pending = self.pending_prefix;
        let guard_ok = |g: super::Guard| self.guard_satisfied(g);

        // The focused pane's context. Mail consults its focused pane; the
        // Contacts view (#0033) and the Calendar view (#0034) each have a
        // single list context.
        let pane_ctx = match self.view {
            super::View::Mail => self.pane_ctx(),
            super::View::Contacts => Some(super::KeyCtx::Contacts),
            super::View::Calendar => Some(super::KeyCtx::Calendar),
        };

        // Global context first.
        if let Some(action) = super::resolve(super::KeyCtx::Global, key, pending, &guard_ok) {
            // In non-Mail views only the view-agnostic Global surface (view
            // switch / quit / help / activity) is live; a mail-specific Global
            // key resolves but is swallowed so it cannot fire (#0033) -- UNLESS
            // the active view's pane context rebinds that key (e.g. Contacts
            // rebinds `/` to fuzzy search), in which case the pane binding wins.
            if self.view != super::View::Mail && !action.is_view_agnostic() {
                let pane_rebinds = pane_ctx
                    .and_then(|ctx| super::resolve(ctx, key, pending, &guard_ok))
                    .is_some();
                if !pane_rebinds {
                    self.pending_prefix = None;
                    return None;
                }
            } else {
                return self.execute(action, key);
            }
        }
        // Then the pane context.
        if let Some(ctx) = pane_ctx {
            if let Some(action) = super::resolve(ctx, key, pending, &guard_ok) {
                return self.execute(action, key);
            }
        }
        // No live binding matched: clear any pending leader (an unrecognised
        // key aborts the chord), matching the old `_ => { g_pending = false }`.
        self.pending_prefix = None;
        None
    }

    /// The KEYMAP context for the currently focused pane (no overlay active).
    fn pane_ctx(&self) -> Option<super::KeyCtx> {
        match self.focus {
            Focus::Sidebar => Some(super::KeyCtx::Sidebar),
            Focus::List => Some(super::KeyCtx::List),
            Focus::Headers => Some(super::KeyCtx::Headers),
            Focus::Preview => Some(super::KeyCtx::Preview),
            Focus::Search | Focus::ComposeWizard => None,
        }
    }

    /// Evaluate a keymap `Guard` against live app state.
    fn guard_satisfied(&self, guard: super::Guard) -> bool {
        match guard {
            super::Guard::None => true,
            super::Guard::MultiAccount => self.accounts.len() > 1,
            // `c` (edit recipients) is catalogued Drafts-only, but the old code
            // still *matched* the key outside Drafts to show a status hint. We
            // resolve it in any mailbox and branch inside the executor so that
            // UX is preserved; the guard is advisory here.
            super::Guard::DraftsOnly => true,
            super::Guard::NonEmptyList => !self.visible.is_empty(),
        }
    }

    /// Run a resolved Normal-mode [`KeyAction`]. This is the single executor the
    /// table dispatches into; it replaces the former per-pane match arms.
    fn execute(&mut self, action: super::KeyAction, key: KeyEvent) -> Option<Message> {
        use super::KeyAction as A;

        // Every executed action clears the pending leader except the leader key
        // itself (handled explicitly below) and the `gg` continuation.
        match action {
            // -- Global -------------------------------------------------------
            A::Quit => return Some(Message::Quit),
            A::ToggleHelp => {
                self.pending_prefix = None;
                self.help_scroll = 0;
                self.help_filter.clear();
                self.help_filter_active = false;
                self.overlay = Overlay::Help;
            }
            A::ToggleZoom => {
                self.pending_prefix = None;
                self.toggle_zoom();
            }
            A::ToggleActivityLog => {
                self.pending_prefix = None;
                self.show_activity_log = !self.show_activity_log;
            }
            A::OpenActivityOverlay => {
                self.pending_prefix = None;
                self.activity_filter.clear();
                self.activity_filter_active = false;
                self.activity_scroll = 0;
                self.overlay = Overlay::Activity;
            }
            A::OpenLogFile => {
                self.pending_prefix = None;
                self.push_action(Action::OpenLogFile);
            }
            A::OpenConfigFile => {
                self.pending_prefix = None;
                self.push_action(Action::OpenConfigFile);
            }
            A::FilterMetadata => {
                self.pending_prefix = None;
                self.focus = Focus::Search;
                self.search_query.clear();
                self.search_includes_body = false;
                self.reload_from_cache();
            }
            A::SearchContent => {
                self.pending_prefix = None;
                self.focus = Focus::Search;
                self.search_query.clear();
                self.search_includes_body = true;
                self.reload_from_cache();
            }
            A::SwitchAccount => {
                self.pending_prefix = None;
                let next = (self.active_account + 1) % self.accounts.len();
                self.switch_account(next);
            }
            A::JumpAccount => {
                // Ctrl+1..9 -> direct account jump (guarded to multi-account).
                self.pending_prefix = None;
                if let KeyCode::Char(c @ '1'..='9') = key.code {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < self.accounts.len() {
                        self.switch_account(idx);
                    }
                }
            }
            A::JumpMailbox => {
                self.pending_prefix = None;
                if let KeyCode::Char(c @ '1'..='9') = key.code {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < self.mailboxes.len() {
                        self.sidebar_index = idx;
                        self.switch_mailbox(idx);
                        self.focus = Focus::List;
                    }
                }
            }
            A::FocusForward => {
                self.pending_prefix = None;
                if self.focus == Focus::Sidebar {
                    self.switch_mailbox(self.sidebar_index);
                }
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::List,
                    Focus::List => Focus::Preview,
                    Focus::Preview => Focus::Headers,
                    Focus::Headers => Focus::Sidebar,
                    Focus::Search => Focus::List,
                    Focus::ComposeWizard => Focus::ComposeWizard,
                };
            }
            A::FocusBackward => {
                self.pending_prefix = None;
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Headers,
                    Focus::Headers => Focus::Preview,
                    Focus::Preview => Focus::List,
                    Focus::List => Focus::Sidebar,
                    Focus::Search => Focus::List,
                    Focus::ComposeWizard => Focus::ComposeWizard,
                };
            }
            A::SwitchView => {
                // `Space m/c/a`: the continuation key selects the target view.
                // `switch_view` clears the pending leader itself.
                if let KeyCode::Char(c) = key.code {
                    if let Some(&target) =
                        super::View::ALL.iter().find(|v| v.switch_key() == c)
                    {
                        self.switch_view(target);
                    }
                }
                self.pending_prefix = None;
            }
            // -- Sidebar ------------------------------------------------------
            A::SidebarDown => {
                self.pending_prefix = None;
                if self.sidebar_index < self.mailboxes.len().saturating_sub(1) {
                    self.sidebar_index += 1;
                }
            }
            A::SidebarUp => {
                self.pending_prefix = None;
                self.sidebar_index = self.sidebar_index.saturating_sub(1);
            }
            A::SidebarSelect => {
                self.pending_prefix = None;
                self.switch_mailbox(self.sidebar_index);
                self.focus = Focus::List;
            }
            // -- Headers ------------------------------------------------------
            A::HeadersDown => {
                self.pending_prefix = None;
                self.headers_scroll = self.headers_scroll.saturating_add(1);
            }
            A::HeadersUp => {
                self.pending_prefix = None;
                self.headers_scroll = self.headers_scroll.saturating_sub(1);
            }
            // -- Preview / body ----------------------------------------------
            A::PreviewDown => {
                self.pending_prefix = None;
                self.preview_scroll = self.preview_scroll.saturating_add(1);
            }
            A::PreviewUp => {
                self.pending_prefix = None;
                self.preview_scroll = self.preview_scroll.saturating_sub(1);
            }
            A::PreviewHalfDown => {
                self.pending_prefix = None;
                self.preview_scroll = self.preview_scroll.saturating_add(10);
            }
            A::PreviewHalfUp => {
                self.pending_prefix = None;
                self.preview_scroll = self.preview_scroll.saturating_sub(10);
            }
            A::PreviewToList => {
                self.pending_prefix = None;
                self.focus = Focus::List;
            }
            // -- Contacts view (#0033) ---------------------------------------
            A::ContactsDown => {
                self.pending_prefix = None;
                let len = self.contacts_view.matches.len();
                if len > 0 && self.contacts_view.list_index < len - 1 {
                    self.contacts_view.list_index += 1;
                }
            }
            A::ContactsUp => {
                self.pending_prefix = None;
                self.contacts_view.list_index =
                    self.contacts_view.list_index.saturating_sub(1);
            }
            A::ContactsTop => {
                // Reached only with `g` pending (the leader continuation).
                self.contacts_view.list_index = 0;
                self.pending_prefix = None;
            }
            A::ContactsBottom => {
                self.pending_prefix = None;
                self.contacts_view.list_index =
                    self.contacts_view.matches.len().saturating_sub(1);
            }
            A::ContactsSearch => {
                self.pending_prefix = None;
                self.contacts_view.searching = true;
            }
            A::ContactsCompose => {
                self.pending_prefix = None;
                if let Some(contact) = self.selected_contact() {
                    let to =
                        crate::send::format_recipient(&contact.display_name, &contact.address);
                    self.push_action(Action::ComposeToContact { to });
                }
            }
            A::ContactsVcard => {
                self.pending_prefix = None;
                if let Some(contact) = self.selected_contact() {
                    self.push_action(Action::SendContactVcard {
                        contact: contact.clone(),
                    });
                }
            }
            A::ContactsCopyEmail => {
                self.pending_prefix = None;
                match self.selected_contact() {
                    Some(contact) => {
                        let address = contact.address.clone();
                        self.push_action(Action::CopyContactEmail { address });
                    }
                    None => self.set_status("No contact selected".to_string()),
                }
            }
            A::ContactsRefresh => {
                self.pending_prefix = None;
                self.refresh_contacts();
            }
            // -- Calendar view (#0034) ---------------------------------------
            A::CalendarDown => {
                self.pending_prefix = None;
                let len = self.calendar_view.visible.len();
                if len > 0 && self.calendar_view.list_index < len - 1 {
                    self.calendar_view.list_index += 1;
                }
            }
            A::CalendarUp => {
                self.pending_prefix = None;
                self.calendar_view.list_index =
                    self.calendar_view.list_index.saturating_sub(1);
            }
            A::CalendarTop => {
                // Reached only with `g` pending (the leader continuation).
                self.calendar_view.list_index = 0;
                self.pending_prefix = None;
            }
            A::CalendarBottom => {
                self.pending_prefix = None;
                self.calendar_view.list_index =
                    self.calendar_view.visible.len().saturating_sub(1);
            }
            A::CalendarOpenSource => {
                self.pending_prefix = None;
                if let Some(event) = self.selected_event() {
                    let msg = event.msg;
                    self.push_action(Action::OpenEventSource { msg });
                }
            }
            A::CalendarRsvp => {
                self.pending_prefix = None;
                self.open_rsvp_overlay_for_event();
            }
            A::CalendarToggleScope => {
                self.pending_prefix = None;
                self.calendar_view.show_past = !self.calendar_view.show_past;
                self.calendar_view.list_index = 0;
                self.recompute_calendar_visible();
                let scope = if self.calendar_view.show_past {
                    "all events"
                } else {
                    "upcoming events"
                };
                self.set_status(format!("Calendar: showing {scope}"));
            }
            A::CalendarRefresh => {
                self.pending_prefix = None;
                self.refresh_calendar();
            }
            // -- List / shared -----------------------------------------------
            _ => return self.execute_list(action, key),
        }

        None
    }

    /// List-context actions (and the shared attachment/browser/RSVP actions the
    /// list, headers, and preview panes all use). Split out to keep `execute`
    /// readable; it also owns the list-cursor scroll-reset bookkeeping.
    fn execute_list(&mut self, action: super::KeyAction, key: KeyEvent) -> Option<Message> {
        use super::KeyAction as A;
        let old_index = self.list_index;

        match action {
            A::ListDown => {
                self.pending_prefix = None;
                if self.list_index < self.visible.len() - 1 {
                    self.list_index += 1;
                }
            }
            A::ListUp => {
                self.pending_prefix = None;
                self.list_index = self.list_index.saturating_sub(1);
            }
            A::ListTop => {
                // Reached only with `g` pending (the leader continuation).
                self.list_index = 0;
                self.pending_prefix = None;
            }
            A::ListBottom => {
                self.pending_prefix = None;
                self.list_index = self.visible.len().saturating_sub(1);
            }
            A::JumpToDate => {
                // Reached only with `g` pending (the leader continuation).
                self.pending_prefix = None;
                self.jump_date_input = Some(String::new());
            }
            A::OpenEditor => {
                self.pending_prefix = None;
                self.push_action(Action::EditCurrent);
            }
            A::Reply => {
                self.pending_prefix = None;
                self.push_action(Action::Reply(false));
            }
            A::ReplyAll => {
                self.pending_prefix = None;
                self.push_action(Action::Reply(true));
            }
            A::Forward => {
                self.pending_prefix = None;
                if let Some(msg) = self.selected_email_ref() {
                    self.push_action(Action::OpenComposeWizard(ComposeMode::Forward { msg }));
                } else if self.selected_email().is_some() {
                    // A Drafts row has no message behind it to forward.
                    self.set_status(
                        "Forward needs a received message; a draft has none to quote".to_string(),
                    );
                }
            }
            A::EditRecipients => {
                // Only meaningful in Drafts; outside Drafts keep the old status
                // hint (the guard is advisory, resolved everywhere).
                self.pending_prefix = None;
                if self.active_kind() == MailboxKind::Drafts {
                    // The draft is named by its `id:`, not by the path it
                    // happens to sit at: the wizard resolves it through the
                    // drafts index when it opens and again when it submits.
                    if let Some(id) = self.selected_email().and_then(|e| e.draft_id.clone()) {
                        self.push_action(Action::OpenComposeWizard(ComposeMode::EditDraft {
                            id,
                        }));
                    }
                } else {
                    self.set_status(
                        "Edit recipients (c) is only available in Drafts".to_string(),
                    );
                }
            }
            A::SelectAllVisible => {
                self.pending_prefix = None;
                self.selection = self.visible_emails().filter_map(|e| e.key()).collect();
            }
            A::Archive => {
                self.pending_prefix = None;
                if !self.selection.is_empty() {
                    let count = self.selection.len();
                    self.overlay = Overlay::Confirm(ConfirmDialog {
                        title: format!("Archive {} emails?", count),
                        detail: format!("{} selected emails", count),
                        action: ConfirmAction::Archive,
                    });
                } else if let Some(email) = self.selected_email() {
                    self.overlay = Overlay::Confirm(ConfirmDialog {
                        title: "Archive this email?".to_string(),
                        detail: format!("{} - {}", email.from, email.subject),
                        action: ConfirmAction::Archive,
                    });
                }
            }
            A::Delete => {
                self.pending_prefix = None;
                if !self.selection.is_empty() {
                    let count = self.selection.len();
                    self.overlay = Overlay::Confirm(ConfirmDialog {
                        title: format!("Delete {} emails?", count),
                        detail: format!("{} selected emails", count),
                        action: ConfirmAction::Delete,
                    });
                } else if let Some(email) = self.selected_email() {
                    self.overlay = Overlay::Confirm(ConfirmDialog {
                        title: "Delete this email?".to_string(),
                        detail: format!("{} - {}", email.from, email.subject),
                        action: ConfirmAction::Delete,
                    });
                }
            }
            A::Approve => {
                self.pending_prefix = None;
                if !self.selection.is_empty() {
                    self.confirm_draft_batch(
                        "Approve",
                        |count| format!("Approve {count} drafts?"),
                        ConfirmAction::Approve,
                    );
                } else {
                    self.push_action(Action::Approve);
                }
            }
            A::MarkDraft => {
                self.pending_prefix = None;
                if !self.selection.is_empty() {
                    self.confirm_draft_batch(
                        "Mark as draft",
                        |count| format!("Mark {count} drafts as draft?"),
                        ConfirmAction::MarkDraft,
                    );
                } else {
                    self.push_action(Action::MarkDraft);
                }
            }
            A::Send => {
                self.pending_prefix = None;
                if let Some(email) = self.selected_email() {
                    self.overlay = Overlay::Confirm(ConfirmDialog {
                        title: "Send this email?".to_string(),
                        detail: format!("To: {} - {}", email.to, email.subject),
                        action: ConfirmAction::Send,
                    });
                }
            }
            A::SendAll => {
                self.pending_prefix = None;
                self.overlay = Overlay::Confirm(ConfirmDialog {
                    title: "Send all approved emails?".to_string(),
                    detail: format!("In {}", self.active_label()),
                    action: ConfirmAction::SendApproved,
                });
            }
            A::CopyMessageRef => {
                self.pending_prefix = None;
                self.push_action(Action::CopyMessageRef);
            }
            A::ToggleRead => {
                self.pending_prefix = None;
                if !self.selection.is_empty() {
                    let msgs: Vec<MessageRef> =
                        self.selection.iter().filter_map(|k| k.msg()).collect();
                    self.push_action(Action::BatchToggleRead(msgs));
                } else {
                    self.push_action(Action::ToggleRead);
                }
            }
            A::ToggleFlag => {
                self.pending_prefix = None;
                if !self.selection.is_empty() {
                    let msgs: Vec<MessageRef> =
                        self.selection.iter().filter_map(|k| k.msg()).collect();
                    self.push_action(Action::BatchToggleFlag(msgs));
                } else {
                    self.push_action(Action::ToggleFlag);
                }
            }
            A::ToggleFlaggedFilter => {
                self.pending_prefix = None;
                self.flagged_only = !self.flagged_only;
                let anchor = self.cursor_anchor();
                let fallback = self.list_index;
                self.selection.clear();
                self.rebuild_visible();
                self.restore_cursor(anchor, fallback);
                self.headers_scroll = 0;
                self.preview_scroll = 0;
                let shown = self.visible.len();
                if self.flagged_only {
                    self.set_status(format!("Flagged only ({shown})"));
                } else {
                    self.set_status("Showing all messages".to_string());
                }
            }
            A::MovePicker => {
                self.pending_prefix = None;
                self.open_mailbox_picker();
            }
            A::Rsvp => {
                self.pending_prefix = None;
                self.open_rsvp_overlay();
            }
            A::OpenThread => {
                self.pending_prefix = None;
                self.open_thread_overlay();
            }
            A::NewDraft => {
                self.pending_prefix = None;
                self.push_action(Action::OpenComposeWizard(ComposeMode::New));
            }
            A::QuickSync => {
                self.pending_prefix = None;
                self.push_action(Action::Fetch);
            }
            A::FullSync => {
                self.pending_prefix = None;
                self.push_action(Action::Sync);
            }
            A::ServerSearch => {
                self.pending_prefix = None;
                self.server_search_query.clear();
                self.server_search_results.clear();
                self.server_search_index = 0;
                self.server_search_scroll = 0;
                self.server_search_headers_scroll = 0;
                self.server_search_focus = SearchOverlayFocus::Input;
                self.server_search_loading = false;
                self.server_search_status = None;
                self.overlay = Overlay::Search;
            }
            A::OpenAttachment => {
                self.pending_prefix = None;
                self.open_attachment_picker(AttachmentPickerMode::Open);
            }
            A::SaveAttachment => {
                self.pending_prefix = None;
                self.open_attachment_picker(AttachmentPickerMode::Save);
            }
            A::OpenInBrowser => {
                self.pending_prefix = None;
                // The markup is a blob (or the html part of the raw message),
                // not a `.html` beside a `.md`, so it is written to a temp
                // file on demand and that is what the browser opens (#0052
                // scope item 9).
                if let Some(msg) = self.selected_email_ref() {
                    if let Some(path) =
                        crate::tui::actions::html_rendition_for_row(self, msg.row_id())
                    {
                        self.push_action(Action::OpenHtmlInBrowser(path));
                    }
                }
            }
            A::ToggleSelect => {
                self.pending_prefix = None;
                // Drafts are selectable too (#0052): the set is keyed on
                // `EntryKey`, so the Drafts mailbox has a batch again.
                if let Some(key) = self.selected_email().and_then(|e| e.key()) {
                    if self.selection.contains(&key) {
                        self.selection.remove(&key);
                    } else {
                        self.selection.insert(key);
                    }
                    if self.list_index < self.visible.len() - 1 {
                        self.list_index += 1;
                    }
                }
            }
            A::ClearSelection => {
                self.pending_prefix = None;
                if !self.selection.is_empty() {
                    self.selection.clear();
                }
            }
            // The leader key itself: begin/toggle the pending leader chord.
            // This is the only action that intentionally leaves a leader armed.
            // Two leaders exist (#0033 follow-up): Space (view switcher) and
            // `g` (list `gg`/`G`); the pressed key identifies which one.
            A::Manual => {
                if let KeyCode::Char(leader) = key.code {
                    self.pending_prefix = if self.pending_prefix == Some(leader) {
                        None
                    } else {
                        Some(leader)
                    };
                }
            }
            // Any pane-only action mistakenly routed here is a no-op.
            _ => {
                self.pending_prefix = None;
            }
        }

        if self.list_index != old_index {
            self.headers_scroll = 0;
            self.preview_scroll = 0;
        }

        None
    }

    /// Open the confirmation for a batch that writes to draft files, or say
    /// why there is nothing to confirm.
    ///
    /// The batch takes the drafts half of the selection (see
    /// [`Self::handle_confirm_key`]), so a selection holding no draft key --
    /// `A` over a received-mail selection, which the keymap allows -- has
    /// nothing to approve. Without this guard the dialog opened on the full
    /// count and the batch then reported "Approved 0 drafts", asking a
    /// question whose only honest answer was already known.
    fn confirm_draft_batch(
        &mut self,
        what: &str,
        title: impl Fn(usize) -> String,
        action: ConfirmAction,
    ) {
        let count = self
            .selection
            .iter()
            .filter(|key| key.draft().is_some())
            .count();
        if count == 0 {
            self.set_status(format!(
                "{what} needs drafts; the selection has no draft in it"
            ));
            return;
        }
        self.overlay = Overlay::Confirm(ConfirmDialog {
            title: title(count),
            detail: format!("{count} selected drafts"),
            action,
        });
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> Option<Message> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Overlay::Confirm(dialog) =
                    std::mem::replace(&mut self.overlay, Overlay::None)
                {
                    match dialog.action {
                        // Approve and mark-draft write to draft files, so they
                        // take the drafts half of the selection; archive and
                        // delete are store mutations and take the messages
                        // half (#0052). A selection never holds both -- one
                        // mailbox lists one kind of row and switching clears
                        // the set -- but each side filters rather than assumes.
                        ConfirmAction::Approve if !self.selection.is_empty() => {
                            let ids: Vec<String> = self
                                .selection
                                .drain()
                                .filter_map(|k| k.draft().map(str::to_string))
                                .collect();
                            self.push_action(Action::BatchApprove(ids));
                        }
                        ConfirmAction::MarkDraft if !self.selection.is_empty() => {
                            let ids: Vec<String> = self
                                .selection
                                .drain()
                                .filter_map(|k| k.draft().map(str::to_string))
                                .collect();
                            self.push_action(Action::BatchMarkDraft(ids));
                        }
                        ConfirmAction::Archive if !self.selection.is_empty() => {
                            let msgs: Vec<MessageRef> =
                                self.selection.drain().filter_map(|k| k.msg()).collect();
                            self.push_action(Action::BatchArchive(msgs));
                        }
                        ConfirmAction::Delete if !self.selection.is_empty() => {
                            // Delete takes the messages half, as archive does:
                            // a received message is a store mutation. A Drafts
                            // selection holds draft ids and no `messages` row,
                            // so once the messages half is empty the delete is
                            // the local file removal instead (#0073), not
                            // `prepare_delete` reporting nothing to delete. A
                            // selection never mixes the two in practice; each
                            // half still filters rather than assumes.
                            let msgs: Vec<MessageRef> =
                                self.selection.iter().filter_map(|k| k.msg()).collect();
                            if msgs.is_empty() {
                                let drafts: Vec<String> = self
                                    .selection
                                    .drain()
                                    .filter_map(|k| k.draft().map(str::to_string))
                                    .collect();
                                self.push_action(Action::BatchDeleteDrafts(drafts));
                            } else {
                                self.selection.clear();
                                self.push_action(Action::BatchDelete(msgs));
                            }
                        }
                        _ => {
                            self.push_action(match dialog.action {
                                ConfirmAction::Approve => Action::Approve,
                                ConfirmAction::MarkDraft => Action::MarkDraft,
                                ConfirmAction::Archive => Action::Archive,
                                ConfirmAction::Delete => Action::Delete,
                                ConfirmAction::Send => Action::Send,
                                ConfirmAction::SendApproved => Action::SendApproved,
                            });
                        }
                    }
                    // Consume-and-close: the confirm dialog was taken above.
                    // Promote any error that had been queued behind it.
                    self.promote_pending_error();
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.close_overlay();
            }
            _ => {}
        }
        None
    }

    fn handle_search_overlay_key(&mut self, key: KeyEvent) -> Option<Message> {
        match self.server_search_focus {
            SearchOverlayFocus::Input => self.handle_search_overlay_input_key(key),
            SearchOverlayFocus::List => self.handle_search_overlay_list_key(key),
        }
    }

    fn handle_search_overlay_input_key(&mut self, key: KeyEvent) -> Option<Message> {
        match key.code {
            KeyCode::Char(c) => {
                self.server_search_query.push(c);
            }
            KeyCode::Backspace => {
                self.server_search_query.pop();
            }
            KeyCode::Enter => {
                if !self.server_search_query.is_empty() {
                    let criteria =
                        crate::imap_client::parse_search_query(&self.server_search_query);
                    let (targets, scope_label) = if let Some(ref name) = criteria.in_mailbox {
                        if let Some(target) = self.search_target_by_name(name) {
                            let label = target.label.clone();
                            (vec![target], label)
                        } else {
                            self.server_search_status = Some(format!("Unknown mailbox: {}", name));
                            return None;
                        }
                    } else {
                        (self.all_search_targets(), "All".to_string())
                    };
                    self.server_search_scope_label = scope_label;
                    self.push_action(Action::ServerSearch {
                        query: self.server_search_query.clone(),
                        targets,
                    });
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                if !self.server_search_results.is_empty() {
                    self.server_search_focus = SearchOverlayFocus::List;
                }
            }
            KeyCode::Esc => {
                self.close_overlay();
            }
            _ => {}
        }
        None
    }

    fn handle_search_overlay_list_key(&mut self, key: KeyEvent) -> Option<Message> {
        let len = self.server_search_results.len();
        if len == 0 {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => {
                    self.server_search_focus = SearchOverlayFocus::Input;
                }
                KeyCode::Esc => {
                    self.close_overlay();
                }
                _ => {}
            }
            return None;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.server_search_index < len - 1 {
                    self.server_search_index += 1;
                    self.server_search_scroll = 0;
                    self.server_search_headers_scroll = 0;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.server_search_index > 0 {
                    self.server_search_index -= 1;
                    self.server_search_scroll = 0;
                    self.server_search_headers_scroll = 0;
                }
            }
            KeyCode::Char('g') => {
                if self.pending_prefix == Some('g') {
                    self.server_search_index = 0;
                    self.server_search_scroll = 0;
                    self.server_search_headers_scroll = 0;
                    self.pending_prefix = None;
                } else {
                    self.pending_prefix = Some('g');
                }
                return None;
            }
            KeyCode::Char('G') => {
                self.pending_prefix = None;
                self.server_search_index = len.saturating_sub(1);
                self.server_search_scroll = 0;
                self.server_search_headers_scroll = 0;
            }
            KeyCode::Char('d') => {
                self.server_search_scroll = self.server_search_scroll.saturating_add(10);
            }
            KeyCode::Char('u') => {
                self.server_search_scroll = self.server_search_scroll.saturating_sub(10);
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                self.push_action(Action::SearchResultOpen);
            }
            KeyCode::Char('r') => {
                self.push_action(Action::SearchResultReply(false));
            }
            KeyCode::Char('R') => {
                self.push_action(Action::SearchResultReply(true));
            }
            KeyCode::Char('w') => {
                self.push_action(Action::SearchResultForward);
            }
            KeyCode::Char('a') => {
                self.push_action(Action::SearchResultArchive);
            }
            KeyCode::Char('b') => {
                self.push_action(Action::SearchResultOpenInBrowser);
            }
            KeyCode::Char('o') => {
                self.open_search_result_attachment_picker(AttachmentPickerMode::Open);
            }
            KeyCode::Char('O') => {
                self.open_search_result_attachment_picker(AttachmentPickerMode::Save);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.server_search_focus = SearchOverlayFocus::Input;
            }
            KeyCode::Esc => {
                self.close_overlay();
            }
            _ => {}
        }
        self.pending_prefix = None;
        None
    }

    // -----------------------------------------------------------------
    // Activity log overlay
    // -----------------------------------------------------------------

    fn handle_activity_overlay_key(&mut self, key: KeyEvent) -> Option<Message> {
        if self.activity_filter_active {
            match key.code {
                KeyCode::Char(c) => {
                    self.activity_filter.push(c);
                    self.activity_scroll = 0;
                }
                KeyCode::Backspace => {
                    self.activity_filter.pop();
                    self.activity_scroll = 0;
                }
                KeyCode::Enter => {
                    self.activity_filter_active = false;
                }
                KeyCode::Esc => {
                    if !self.activity_filter.is_empty() {
                        self.activity_filter.clear();
                        self.activity_filter_active = false;
                        self.activity_scroll = 0;
                    } else {
                        self.close_overlay();
                    }
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.pending_prefix = None;
                    self.activity_scroll = self.activity_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.pending_prefix = None;
                    self.activity_scroll = self.activity_scroll.saturating_sub(1);
                }
                KeyCode::Char('g') => {
                    if self.pending_prefix == Some('g') {
                        self.activity_scroll = 0;
                        self.pending_prefix = None;
                    } else {
                        self.pending_prefix = Some('g');
                    }
                    return None;
                }
                KeyCode::Char('G') => {
                    self.pending_prefix = None;
                    self.activity_scroll = u16::MAX;
                }
                KeyCode::Char('d') => {
                    self.pending_prefix = None;
                    self.activity_scroll = self.activity_scroll.saturating_add(10);
                }
                KeyCode::Char('u') => {
                    self.pending_prefix = None;
                    self.activity_scroll = self.activity_scroll.saturating_sub(10);
                }
                KeyCode::Char('/') => {
                    self.pending_prefix = None;
                    self.activity_filter_active = true;
                    self.activity_filter.clear();
                    self.activity_scroll = 0;
                }
                KeyCode::Esc | KeyCode::Char('L') | KeyCode::Char('q') => {
                    self.pending_prefix = None;
                    self.close_overlay();
                    self.activity_scroll = 0;
                    self.activity_filter.clear();
                    self.activity_filter_active = false;
                }
                _ => {
                    self.pending_prefix = None;
                }
            }
        }
        None
    }

    // -----------------------------------------------------------------
    // Compose wizard
    // -----------------------------------------------------------------

    fn handle_compose_wizard_key(&mut self, key: KeyEvent) -> Option<Message> {
        let wizard = self.compose_wizard_mut()?;

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Esc => {
                self.push_action(Action::ComposeWizardCancel);
                return None;
            }
            KeyCode::Tab => {
                wizard.focus = wizard.focus.next();
                wizard.suggestion_idx = 0;
                self.recompute_compose_suggestions();
                return None;
            }
            KeyCode::BackTab => {
                wizard.focus = wizard.focus.prev();
                wizard.suggestion_idx = 0;
                self.recompute_compose_suggestions();
                return None;
            }
            KeyCode::Up => {
                if wizard.focus.is_address()
                    && !wizard.suggestions.is_empty()
                    && wizard.suggestion_idx > 0
                {
                    wizard.suggestion_idx -= 1;
                }
                return None;
            }
            KeyCode::Down => {
                if wizard.focus.is_address()
                    && !wizard.suggestions.is_empty()
                    && wizard.suggestion_idx + 1 < wizard.suggestions.len()
                {
                    wizard.suggestion_idx += 1;
                }
                return None;
            }
            KeyCode::Char('g') if ctrl => {
                // Force-submit from any field.
                self.push_action(Action::ComposeWizardSubmit);
                return None;
            }
            KeyCode::Char('n') if ctrl => {
                if wizard.focus.is_address()
                    && !wizard.suggestions.is_empty()
                    && wizard.suggestion_idx + 1 < wizard.suggestions.len()
                {
                    wizard.suggestion_idx += 1;
                }
                return None;
            }
            KeyCode::Char('p') if ctrl => {
                if wizard.focus.is_address()
                    && !wizard.suggestions.is_empty()
                    && wizard.suggestion_idx > 0
                {
                    wizard.suggestion_idx -= 1;
                }
                return None;
            }
            KeyCode::Char('u') if ctrl => {
                // Clear the current field.
                current_field_mut(wizard).clear();
                self.recompute_compose_suggestions();
                return None;
            }
            KeyCode::Enter => {
                // On an address field with a highlighted suggestion, accept it.
                if wizard.focus.is_address() && !wizard.suggestions.is_empty() {
                    let sug = wizard.suggestions[wizard.suggestion_idx].clone();
                    accept_suggestion(current_field_mut(wizard), &sug);
                    // After accepting, clear the suggestion list so another
                    // Enter moves on rather than re-appending the same contact.
                    wizard.suggestions.clear();
                    wizard.suggestion_idx = 0;
                    return None;
                }
                // On subject (or an empty-suggestion address field), submit.
                if wizard.focus == ComposeField::Subject || !wizard.subject.trim().is_empty() {
                    self.push_action(Action::ComposeWizardSubmit);
                    return None;
                }
                // Otherwise cycle to the next field.
                wizard.focus = wizard.focus.next();
                wizard.suggestion_idx = 0;
                self.recompute_compose_suggestions();
                return None;
            }
            KeyCode::Backspace => {
                current_field_mut(wizard).pop();
                if wizard.focus.is_address() {
                    wizard.suggestion_idx = 0;
                    self.recompute_compose_suggestions();
                }
                return None;
            }
            KeyCode::Char(c) => {
                // Ctrl-prefixed chars not handled above are ignored.
                if ctrl {
                    return None;
                }
                let _ = shift; // Shift+letter is just the uppercase char.
                current_field_mut(wizard).push(c);
                if wizard.focus.is_address() {
                    wizard.suggestion_idx = 0;
                    self.recompute_compose_suggestions();
                }
                return None;
            }
            _ => {}
        }

        None
    }

    pub(crate) fn recompute_compose_suggestions(&mut self) {
        let Some(wizard) = self.compose_wizard_mut() else {
            return;
        };
        if !wizard.focus.is_address() {
            wizard.suggestions.clear();
            wizard.suggestion_idx = 0;
            return;
        }
        let field_value = match wizard.focus {
            ComposeField::To => &wizard.to,
            ComposeField::Cc => &wizard.cc,
            ComposeField::Bcc => &wizard.bcc,
            ComposeField::Subject => {
                wizard.suggestions.clear();
                return;
            }
        };
        let query = field_value
            .rsplit(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        let Some(index) = wizard.contacts.as_ref() else {
            wizard.suggestions.clear();
            return;
        };

        // Don't flood with N untyped entries — only show suggestions
        // once the user has typed at least 1 char of the partial.
        if query.is_empty() {
            wizard.suggestions.clear();
            wizard.suggestion_idx = 0;
            return;
        }

        let results = crate::contacts::search(index, &query, 12);
        wizard.suggestions = results
            .into_iter()
            .map(|r| ComposeSuggestion {
                address: r.contact.address.clone(),
                display_name: r.contact.display_name.clone(),
                tier: if r.contact.sent_to > 0 {
                    2
                } else if r.contact.sent_cc > 0 {
                    1
                } else {
                    0
                },
            })
            .collect();
        wizard.suggestion_idx = 0;
    }

    /// Open the RSVP overlay for the cursor email, guarding against
    /// non-invites and self-authored (organizer-side) invites. RSVP is only
    /// for received REQUEST invites (D3): our own Sent invites make us the
    /// organizer, so we hint instead of opening.
    fn open_rsvp_overlay(&mut self) {
        let Some(email) = self.selected_email() else {
            return;
        };
        if !email.is_invite {
            self.set_status("Not a calendar invite".to_string());
            return;
        }
        if self.active_kind() == MailboxKind::Sent {
            self.set_status(
                "You are the organizer of this invite — nothing to RSVP".to_string(),
            );
            return;
        }
        let subject = email.subject.clone();
        let Some(msg) = email.msg else {
            self.set_status("This search hit has no local copy to RSVP from".to_string());
            return;
        };
        // The method and the summary live in the ics blob, which is parsed for
        // the selected message only (#0038 item 6). The cursor is on it, so
        // this is the memo the render pass already filled in the common case.
        let event = self.load_message_invite(msg);
        if let Some(refusal) = rsvp_refusal(event.as_ref()) {
            self.set_status(refusal);
            return;
        }
        let summary = event
            .and_then(|e| e.summary)
            .unwrap_or(subject);
        self.overlay = Overlay::Rsvp(RsvpOverlay {
            msg,
            summary,
            selected: 0,
        });
    }

    /// Calendar-view sibling of [`Self::open_rsvp_overlay`] (#0034): the same
    /// guards (not our own / must be a REQUEST), plus one the mail path cannot
    /// need, since cancelled rows exist only here: RSVP'ing to a meeting the
    /// organizer already cancelled would mail a reply about a dead event. Read
    /// from the selected agenda row instead of the mail cursor; organizer-ness
    /// comes from the row (the winning copy's mailbox), not the active mailbox.
    fn open_rsvp_overlay_for_event(&mut self) {
        let Some(event) = self.selected_event() else {
            return;
        };
        if event.cancelled {
            self.set_status(
                "This event was cancelled by the organizer — nothing to RSVP".to_string(),
            );
            return;
        }
        if event.is_organizer {
            self.set_status(
                "You are the organizer of this invite — nothing to RSVP".to_string(),
            );
            return;
        }
        let is_request = event
            .event
            .method
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("REQUEST"));
        if !is_request {
            self.set_status(
                "Only received invitations (REQUEST) can be RSVP'd".to_string(),
            );
            return;
        }
        let summary = event
            .event
            .summary
            .clone()
            .unwrap_or_else(|| event.subject.clone());
        let msg = event.msg;
        self.overlay = Overlay::Rsvp(RsvpOverlay {
            msg,
            summary,
            selected: 0,
        });
    }

    /// Build and open the conversation overlay for the cursor message (#0008).
    ///
    /// The thread is read straight out of the store: the selected row's
    /// `thread_id`, then every message ingest gave that same id, oldest first
    /// (see [`crate::store::read::thread_messages`]). Nothing re-parses headers
    /// here; the grouping was decided at ingest. A message with no related mail
    /// in the store (a lone message, or a reply whose parents are not
    /// downloaded) says so rather than opening a one-line overlay.
    fn open_thread_overlay(&mut self) {
        let Some(msg) = self.selected_email_ref() else {
            // A draft or an unresolved server-search hit has no store row.
            if self.selected_email().is_some() {
                self.set_status("A draft has no conversation to show".to_string());
            }
            return;
        };
        let account = self.account_config.name.clone();
        let Some(store) = crate::store::open_store(&account) else {
            self.set_status("This account has no store yet".to_string());
            return;
        };
        let row = match crate::store::read::find_by_id(&store, msg.row_id()) {
            Ok(Some(row)) => row,
            _ => {
                self.set_status("That message is no longer in the store".to_string());
                return;
            }
        };
        let thread_id = row
            .thread_id
            .clone()
            .unwrap_or_else(|| row.message_id.clone());
        let rows = match crate::store::read::thread_messages(&store, &account, &thread_id) {
            Ok(rows) => rows,
            Err(e) => {
                self.set_status(format!("Could not load the conversation: {e:#}"));
                return;
            }
        };
        if rows.len() <= 1 {
            self.set_status("No related emails for this message in the store".to_string());
            return;
        }
        let current_mid = row.message_id.clone();
        let subject = row
            .subject
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no subject)".to_string());
        let mut selected = 0;
        let messages: Vec<ThreadEntry> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let flags = r.flags();
                if r.message_id == current_mid {
                    selected = i;
                }
                ThreadEntry {
                    msg: MessageRef::new(r.id),
                    mailbox: r.mailbox.clone(),
                    from: super::extract_display_name(
                        r.from.as_deref().unwrap_or_default(),
                    ),
                    date_display: super::resolve_date(
                        &r.date_display,
                        &None,
                        std::path::Path::new(""),
                    )
                    .0,
                    read: flags.seen,
                    answered: flags.answered,
                    forwarded: flags.forwarded,
                    flagged: flags.flagged,
                    current: r.message_id == current_mid,
                }
            })
            .collect();
        self.overlay = Overlay::Thread(ThreadOverlay {
            subject,
            messages,
            selected,
        });
    }

    /// Input for the conversation overlay (#0008): `j`/`k` move, `Enter`/`e`
    /// opens the highlighted message (switching mailbox when it lives in
    /// another), `Esc`/`q`/`T` closes.
    fn handle_thread_overlay_key(&mut self, key: KeyEvent) -> Option<Message> {
        let Overlay::Thread(overlay) = &mut self.overlay else {
            return None;
        };
        let len = overlay.messages.len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if overlay.selected + 1 < len {
                    overlay.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                overlay.selected = overlay.selected.saturating_sub(1);
            }
            KeyCode::Char('g') => {
                overlay.selected = 0;
            }
            KeyCode::Char('G') => {
                overlay.selected = len.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let target = overlay.messages.get(overlay.selected).cloned();
                self.close_overlay();
                if let Some(entry) = target {
                    self.open_message(entry.msg, &entry.mailbox);
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('T') => {
                self.close_overlay();
            }
            _ => {}
        }
        None
    }

    fn handle_rsvp_overlay_key(&mut self, key: KeyEvent) -> Option<Message> {
        let overlay = self.rsvp_overlay_mut()?;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                if overlay.selected < 2 {
                    overlay.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                overlay.selected = overlay.selected.saturating_sub(1);
            }
            KeyCode::Char('a') => overlay.selected = 0,
            KeyCode::Char('t') => overlay.selected = 1,
            KeyCode::Char('d') => overlay.selected = 2,
            KeyCode::Enter => {
                let choice = match overlay.selected {
                    0 => RsvpChoice::Accept,
                    1 => RsvpChoice::Tentative,
                    _ => RsvpChoice::Decline,
                };
                let Overlay::Rsvp(overlay) =
                    std::mem::replace(&mut self.overlay, Overlay::None)
                else {
                    return None;
                };
                self.push_action(Action::Rsvp {
                    msg: overlay.msg,
                    choice,
                });
                // Consume-and-close: promote any error queued behind it.
                self.promote_pending_error();
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close_overlay();
            }
            _ => {}
        }
        None
    }

    fn handle_attachment_picker_key(&mut self, key: KeyEvent) -> Option<Message> {
        let picker = self.attachment_picker_mut().unwrap();
        match picker.mode {
            AttachmentPickerMode::Open => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if picker.selected < picker.files.len().saturating_sub(1) {
                        picker.selected += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                KeyCode::Enter => {
                    let Overlay::Attachment(picker) =
                        std::mem::replace(&mut self.overlay, Overlay::None)
                    else {
                        return None;
                    };
                    let path = picker.files[picker.selected].clone();
                    self.push_action(Action::OpenAttachment(path));
                    // Consume-and-close: promote any error queued behind it.
                    self.promote_pending_error();
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.close_overlay();
                }
                _ => {}
            },
            AttachmentPickerMode::Save => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if picker.selected < picker.files.len().saturating_sub(1) {
                        picker.selected += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                KeyCode::Char(' ') => {
                    let idx = picker.selected;
                    if picker.selected_set.contains(&idx) {
                        picker.selected_set.remove(&idx);
                    } else {
                        picker.selected_set.insert(idx);
                    }
                    // Advance cursor
                    if picker.selected < picker.files.len().saturating_sub(1) {
                        picker.selected += 1;
                    }
                }
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if picker.selected_set.len() == picker.files.len() {
                        picker.selected_set.clear();
                    } else {
                        picker.selected_set = (0..picker.files.len()).collect();
                    }
                }
                KeyCode::Enter => {
                    let Overlay::Attachment(picker) =
                        std::mem::replace(&mut self.overlay, Overlay::None)
                    else {
                        return None;
                    };
                    // Collect selected files, or cursor item if none selected
                    let sources: Vec<PathBuf> = if picker.selected_set.is_empty() {
                        vec![picker.files[picker.selected].clone()]
                    } else {
                        let mut indices: Vec<usize> =
                            picker.selected_set.iter().copied().collect();
                        indices.sort();
                        indices
                            .iter()
                            .filter_map(|&i| picker.files.get(i).cloned())
                            .collect()
                    };
                    // Overlay->overlay handoff: `open_dir_picker` sets
                    // `Overlay::Dir`, so `promote_pending_error` (guarded on
                    // `Overlay::None`) correctly does NOT fire between taking
                    // the attachment picker and opening the dir picker.
                    self.open_dir_picker(sources);
                    self.promote_pending_error();
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.close_overlay();
                }
                _ => {}
            },
        }
        None
    }

    fn handle_dir_picker_key(&mut self, key: KeyEvent) -> Option<Message> {
        let picker = match self.dir_picker_mut() {
            Some(p) => p,
            None => return None,
        };

        match picker.mode {
            DirPickerMode::Zoxide => match key.code {
                KeyCode::Down => {
                    if !picker.zoxide_results.is_empty()
                        && picker.selected < picker.zoxide_results.len().saturating_sub(1)
                    {
                        picker.selected += 1;
                    }
                }
                KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                KeyCode::Backspace => {
                    picker.query.pop();
                    picker.selected = 0;
                    refresh_zoxide_results(picker);
                }
                KeyCode::Tab => {
                    // Switch to browser mode. Use highlighted result or default dir.
                    let start_dir = picker
                        .zoxide_results
                        .get(picker.selected)
                        .cloned()
                        .unwrap_or_else(|| picker.current_dir.clone());
                    picker.mode = DirPickerMode::Browser;
                    picker.current_dir = start_dir;
                    picker.selected = 0;
                    refresh_browser_entries(picker);
                }
                KeyCode::Enter => {
                    if let Some(dir) = picker.zoxide_results.get(picker.selected).cloned() {
                        let sources = picker.sources.clone();
                        self.close_overlay();
                        self.push_action(Action::SaveAttachments {
                            sources,
                            dest_dir: dir,
                        });
                    }
                }
                KeyCode::Esc => {
                    self.close_overlay();
                }
                KeyCode::Char(c) => {
                    picker.query.push(c);
                    picker.selected = 0;
                    refresh_zoxide_results(picker);
                }
                _ => {}
            },
            DirPickerMode::Browser => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    let max = picker.dir_entries.len(); // entry 0 = [ Save here ]
                    if picker.selected < max {
                        picker.selected += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                KeyCode::Char('g') => {
                    picker.selected = 0;
                }
                KeyCode::Char('G') => {
                    picker.selected = picker.dir_entries.len(); // last entry (save-here is 0)
                }
                KeyCode::Char('~') => {
                    let home =
                        PathBuf::from(shellexpand::tilde("~").into_owned());
                    picker.current_dir = home;
                    picker.selected = 0;
                    refresh_browser_entries(picker);
                }
                KeyCode::Char('h') | KeyCode::Backspace => {
                    if let Some(parent) = picker.current_dir.parent() {
                        picker.current_dir = parent.to_path_buf();
                        picker.selected = 0;
                        refresh_browser_entries(picker);
                    }
                }
                KeyCode::Char('l') => {
                    // Descend into selected directory (no-op on "[ Save here ]")
                    if picker.selected > 0 {
                        let idx = picker.selected - 1;
                        if let Some(dir) = picker.dir_entries.get(idx).cloned() {
                            picker.current_dir = dir;
                            picker.selected = 0;
                            refresh_browser_entries(picker);
                        }
                    }
                }
                KeyCode::Enter => {
                    if picker.selected == 0 {
                        // "[ Save here ]" -- confirm
                        let sources = picker.sources.clone();
                        let dest_dir = picker.current_dir.clone();
                        self.close_overlay();
                        self.push_action(Action::SaveAttachments {
                            sources,
                            dest_dir,
                        });
                    } else {
                        // Descend into selected directory
                        let idx = picker.selected - 1;
                        if let Some(dir) = picker.dir_entries.get(idx).cloned() {
                            picker.current_dir = dir;
                            picker.selected = 0;
                            refresh_browser_entries(picker);
                        }
                    }
                }
                KeyCode::Tab => {
                    picker.mode = DirPickerMode::Zoxide;
                    picker.selected = 0;
                    refresh_zoxide_results(picker);
                }
                KeyCode::Esc => {
                    self.close_overlay();
                }
                _ => {}
            },
        }
        None
    }

    // -----------------------------------------------------------------
    // Quick-move mailbox picker (#0018)
    // -----------------------------------------------------------------

    /// Open the quick-move picker for the current selection (or the
    /// cursor email). Candidates are all server-backed mailboxes other
    /// than the active one, so "move to the same mailbox" cannot happen.
    fn open_mailbox_picker(&mut self) {
        // The source mailbox must have a server-side folder; Drafts
        // (local-only) can't be quick-moved -- drafts leave via send.
        if self
            .mailboxes
            .get(self.active_mailbox)
            .and_then(|m| m.server_name.as_ref())
            .is_none()
        {
            self.set_status("Quick-move is not available in this mailbox".to_string());
            return;
        }

        let msgs: Vec<MessageRef> = if !self.selection.is_empty() {
            self.selection.iter().filter_map(|k| k.msg()).collect()
        } else if let Some(m) = self.selected_email_ref() {
            vec![m]
        } else {
            return;
        };

        let candidates: Vec<(usize, String)> = self
            .mailboxes
            .iter()
            .enumerate()
            .filter(|(i, m)| *i != self.active_mailbox && m.server_name.is_some())
            .map(|(i, m)| (i, m.label.clone()))
            .collect();
        if candidates.is_empty() {
            self.set_status("No other mailboxes to move to".to_string());
            return;
        }

        let filtered = (0..candidates.len()).collect();
        self.overlay = Overlay::Mailbox(MailboxPicker {
            query: String::new(),
            candidates,
            filtered,
            selected: 0,
            msgs,
        });
    }

    fn handle_mailbox_picker_key(&mut self, key: KeyEvent) -> Option<Message> {
        let picker = self.mailbox_picker_mut()?;

        match key.code {
            KeyCode::Down | KeyCode::Tab => {
                if !picker.filtered.is_empty()
                    && picker.selected < picker.filtered.len() - 1
                {
                    picker.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(&cand_idx) = picker.filtered.get(picker.selected) {
                    let dest_idx = picker.candidates[cand_idx].0;
                    let Overlay::Mailbox(picker) =
                        std::mem::replace(&mut self.overlay, Overlay::None)
                    else {
                        return None;
                    };
                    self.selection.clear();
                    self.push_action(Action::MoveToMailbox {
                        msgs: picker.msgs,
                        dest_idx,
                    });
                    // Consume-and-close: promote any error queued behind it.
                    self.promote_pending_error();
                }
            }
            KeyCode::Esc => {
                self.close_overlay();
            }
            KeyCode::Backspace => {
                picker.query.pop();
                refresh_mailbox_picker_filter(picker);
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                refresh_mailbox_picker_filter(picker);
            }
            _ => {}
        }
        None
    }

    /// Helper to open the attachment picker in the given mode.
    ///
    /// The files come out of `message_blobs` (#0052 scope item 8): the row
    /// under the cursor is materialised into a temp directory the way
    /// `mp open` and `mp save` do it, and the picker and the save pipeline
    /// below it address those files, as they always did.
    fn open_attachment_picker(&mut self, mode: AttachmentPickerMode) {
        let Some(files) = crate::tui::actions::cursor_attachment_files(self) else {
            return;
        };
        self.present_attachments(files, mode);
    }

    /// Helper to open the attachment picker for a search result.
    ///
    /// A hit that resolved to a local row is [`Self::open_attachment_picker`]
    /// exactly; one that did not has the attachment bytes of the fetch the
    /// overlay is rendering, which are written out to the same temp area, so
    /// neither half declines (#0052 scope item 11).
    fn open_search_result_attachment_picker(&mut self, mode: AttachmentPickerMode) {
        let Some(hit) = self.server_search_results.get(self.server_search_index) else {
            return;
        };
        let msg = hit.entry.msg;
        let index = self.server_search_index;
        // Cloned only for the unresolved hit, which is the one case that needs
        // the fetched payload while `self` is borrowed mutably below.
        let fetched = msg.is_none().then(|| hit.fetched.clone());
        let files = match (msg, fetched) {
            (Some(msg), _) => crate::tui::actions::row_attachment_files(self, msg.row_id()),
            (None, Some(fetched)) => {
                crate::tui::actions::fetched_attachment_files(self, &fetched, index)
            }
            (None, None) => None,
        };
        let Some(files) = files else {
            return;
        };
        self.present_attachments(files, mode);
    }

    /// Put a materialised attachment list in front of the user: nothing to
    /// show says so, one file skips the picker (`o` opens it, `O` goes
    /// straight to the directory picker), several open the picker.
    ///
    /// The pre-store build's own branching, kept: only the origin of the files
    /// changed. `mp open`'s CLI shortcut of opening every attachment at once
    /// stays CLI-only, because a TUI that opened six windows on one keypress
    /// would be the surprising half of the two.
    fn present_attachments(&mut self, files: Vec<PathBuf>, mode: AttachmentPickerMode) {
        match files.len() {
            0 => self.set_status("No attachments".to_string()),
            1 if mode == AttachmentPickerMode::Open => {
                self.push_action(Action::OpenAttachment(files.into_iter().next().unwrap()));
            }
            1 => self.open_dir_picker(files),
            _ => {
                self.overlay = Overlay::Attachment(AttachmentPicker {
                    files,
                    selected: 0,
                    mode,
                    selected_set: HashSet::new(),
                });
            }
        }
    }

    /// Open the directory picker overlay with the given source files.
    fn open_dir_picker(&mut self, sources: Vec<PathBuf>) {
        let default_dir = self
            .last_save_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(shellexpand::tilde("~/Downloads").into_owned()));

        let zoxide_available = which_zoxide();
        let start_mode = if zoxide_available {
            DirPickerMode::Zoxide
        } else {
            DirPickerMode::Browser
        };

        let mut picker = DirPicker {
            mode: start_mode,
            query: String::new(),
            zoxide_results: Vec::new(),
            zoxide_available,
            current_dir: default_dir,
            dir_entries: Vec::new(),
            selected: 0,
            sources,
        };

        if zoxide_available {
            refresh_zoxide_results(&mut picker);
        } else {
            refresh_browser_entries(&mut picker);
        }

        self.overlay = Overlay::Dir(picker);
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Option<Message> {
        if self.help_filter_active {
            match key.code {
                KeyCode::Char(c) => {
                    self.help_filter.push(c);
                    self.help_scroll = 0;
                }
                KeyCode::Backspace => {
                    self.help_filter.pop();
                    self.help_scroll = 0;
                }
                KeyCode::Enter => {
                    self.help_filter_active = false;
                }
                KeyCode::Esc => {
                    if !self.help_filter.is_empty() {
                        self.help_filter.clear();
                        self.help_filter_active = false;
                        self.help_scroll = 0;
                    } else {
                        self.close_overlay();
                    }
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.pending_prefix = None;
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.pending_prefix = None;
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::Char('g') => {
                    if self.pending_prefix == Some('g') {
                        self.help_scroll = 0;
                        self.pending_prefix = None;
                    } else {
                        self.pending_prefix = Some('g');
                    }
                }
                KeyCode::Char('G') => {
                    self.pending_prefix = None;
                    self.help_scroll = u16::MAX;
                }
                KeyCode::Char('d') => {
                    self.pending_prefix = None;
                    self.help_scroll = self.help_scroll.saturating_add(10);
                }
                KeyCode::Char('u') => {
                    self.pending_prefix = None;
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                }
                KeyCode::Char('/') => {
                    self.pending_prefix = None;
                    self.help_filter_active = true;
                    self.help_filter.clear();
                    self.help_scroll = 0;
                }
                KeyCode::Char('?') | KeyCode::Esc => {
                    self.pending_prefix = None;
                    self.close_overlay();
                    self.help_scroll = 0;
                    self.help_filter.clear();
                    self.help_filter_active = false;
                }
                _ => {
                    self.pending_prefix = None;
                }
            }
        }
        None
    }

    fn handle_persistent_error_key(&mut self, key: KeyEvent) -> Option<Message> {
        match key.code {
            KeyCode::Char('s') => {
                self.close_overlay();
                self.push_action(Action::Sync);
            }
            KeyCode::Char('d') | KeyCode::Esc => {
                self.close_overlay();
            }
            _ => {}
        }
        None
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Option<Message> {
        match key.code {
            KeyCode::Enter => {
                self.focus = Focus::List;
            }
            KeyCode::Esc => {
                self.search_query.clear();
                self.search_includes_body = false;
                self.reload_from_cache();
                self.focus = Focus::List;
            }
            KeyCode::Char(c) => {
                let old_lower = self.search_query.to_lowercase();
                self.search_query.push(c);
                // Appending a character normally only shrinks the match
                // set (substring containment is monotone), so narrow the
                // current visible set instead of rescanning everything.
                // But lowercasing is not always append-monotone: Greek
                // capital sigma is context-sensitive ("ΘΕΟΣ" -> "θεος"
                // with final ς, yet "ΘΕΟΣΦ" -> "θεοσφ" with medial σ),
                // so a haystack can match the extended query without
                // matching the shorter one. Narrow only when the old
                // lowercased query is a prefix of the new one; otherwise
                // recompute from the full list.
                let narrow = self.search_query.to_lowercase().starts_with(&old_lower);
                self.apply_search_filter(narrow);
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.apply_search_filter(false);
            }
            _ => {}
        }
        None
    }

    /// Jump-to-date input (#0017).
    ///
    /// `Enter` resolves the typed date and moves the cursor, `Esc` abandons
    /// it, and an unreadable date leaves the prompt armed with the reason on
    /// the status line -- a typo costs a correction, not a re-arm.
    ///
    /// `now()` is read here, at the moment the user commits, and passed into
    /// the pure grammar: `last week` means a week before the keypress, and
    /// nothing below this line knows what day it is.
    fn handle_jump_date_key(&mut self, key: KeyEvent) -> Option<Message> {
        match key.code {
            KeyCode::Enter => {
                let input = self.jump_date_input.clone().unwrap_or_default();
                let today = chrono::Local::now().date_naive();
                match super::jump_date::parse_jump_date(&input, today) {
                    Ok(target) => {
                        self.jump_date_input = None;
                        self.jump_to_date(target);
                    }
                    Err(reason) => {
                        self.set_status_level(reason, crate::tui::app::StatusLevel::Warning);
                    }
                }
            }
            KeyCode::Esc => {
                self.jump_date_input = None;
            }
            KeyCode::Char(c) => {
                if let Some(buf) = self.jump_date_input.as_mut() {
                    buf.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.jump_date_input.as_mut() {
                    buf.pop();
                }
            }
            _ => {}
        }
        None
    }

    /// Contacts view fuzzy-search input (#0033). Free-text like the metadata
    /// search: each edit recomputes the matched contact list. `Enter` leaves
    /// the input but keeps the filter; `Esc` clears it.
    fn handle_contacts_search_key(&mut self, key: KeyEvent) -> Option<Message> {
        match key.code {
            KeyCode::Enter => {
                self.contacts_view.searching = false;
            }
            KeyCode::Esc => {
                self.contacts_view.searching = false;
                self.contacts_view.query.clear();
                self.recompute_contact_matches();
            }
            KeyCode::Char(c) => {
                self.contacts_view.query.push(c);
                self.recompute_contact_matches();
            }
            KeyCode::Backspace => {
                self.contacts_view.query.pop();
                self.recompute_contact_matches();
            }
            _ => {}
        }
        None
    }

    /// Recompute the visible view for the current `search_query` (P3).
    ///
    /// `narrow` may be true only when the *lowercased* query changed by
    /// appending characters to the lowercased query the current view was
    /// built from (i.e. a keystroke in search mode that keeps the old
    /// lowered query as a prefix): substring matching is monotone under
    /// query extension, so the new match set is a subset of the current
    /// one and we can retain-filter `visible` instead of rescanning the
    /// full list. Backspace/edits/resets — and appends where lowercasing
    /// rewrites earlier characters (Greek final sigma) — must pass
    /// `narrow = false`.
    ///
    /// The needle is lowercased once per call, not once per email.
    pub(crate) fn apply_search_filter(&mut self, narrow: bool) {
        self.selection.clear();

        if self.search_query.is_empty() {
            self.visible = (0..self.emails.len()).collect();
        } else {
            self.sync_search_bodies();
            let kind = self.active_kind();
            let bodies = self.search_includes_body.then_some(&self.search_bodies);
            if narrow {
                let needle = self.search_query.to_lowercase();
                narrow_visible(&self.emails, &mut self.visible, &needle, kind, bodies);
            } else {
                // `filter_visible` lowercases the needle once internally.
                self.visible = filter_visible(&self.emails, &self.search_query, kind, bodies);
            }
        }
        self.apply_flagged_filter();

        self.list_index = 0;
        self.headers_scroll = 0;
        self.preview_scroll = 0;
    }

    /// Reset the view after clearing/entering search: reapply the (now
    /// usually empty) query to the full list and reset the cursor. The
    /// full list is always at hand in `self.emails` (it is never
    /// filtered in place anymore), so no cache round-trip is needed.
    pub(crate) fn reload_from_cache(&mut self) {
        self.rebuild_visible();
        self.list_index = 0;
        self.headers_scroll = 0;
        self.preview_scroll = 0;
    }
}

// ---------------------------------------------------------------------------
// Search filter helpers (P2/P3: visible-indices model)
// ---------------------------------------------------------------------------

/// Does `email` match the (already lowercased) search needle?
///
/// `bodies` is `Some` only in body-search mode (`\`), and holds the mailbox's
/// bodies already lowercased, so the body test is the same substring test the
/// entry used to answer from its own `body` field before it became lazy
/// (#0038 scope item 5). An entry the index has no body for (no store row, or
/// a blob the retention sweep evicted) simply does not match on body.
/// Why the invite under the mail cursor cannot be RSVP'd, or `None` when it
/// can (#0029 for the REQUEST guard, #0031 for the version guards).
///
/// Pure so the version rules are pinned without a store: the payload the card
/// shows is one *version* of the event, and a reply to a cancelled or
/// superseded version would carry a `SEQUENCE` the organizer has already moved
/// past. Refuse and say why rather than mailing an answer about a dead
/// version; the invite itself stays readable either way.
fn rsvp_refusal(event: Option<&crate::types::EventFrontmatter>) -> Option<String> {
    let is_request = event
        .and_then(|e| e.method.as_deref())
        .is_some_and(|m| m.eq_ignore_ascii_case("REQUEST"));
    if !is_request {
        return Some("Only received invitations (REQUEST) can be RSVP'd".to_string());
    }
    if event.is_some_and(|e| e.cancelled) {
        return Some("This event was cancelled by the organizer \u{2014} nothing to RSVP".to_string());
    }
    if event.is_some_and(|e| e.superseded) {
        return Some(
            "A newer version of this invitation has arrived \u{2014} RSVP from that one".to_string(),
        );
    }
    None
}

fn email_matches(
    email: &EmailEntry,
    needle_lower: &str,
    kind: MailboxKind,
    bodies: Option<&SearchBodies>,
) -> bool {
    email.subject.to_lowercase().contains(needle_lower)
        || email
            .display_contact(kind)
            .to_lowercase()
            .contains(needle_lower)
        || email.date_display.to_lowercase().contains(needle_lower)
        || email.from.to_lowercase().contains(needle_lower)
        || email.to.to_lowercase().contains(needle_lower)
        || bodies
            .zip(email.msg)
            .and_then(|(bodies, msg)| bodies.get(msg))
            .is_some_and(|body| body.contains(needle_lower))
}

/// Build the visible-index view of `emails` for `query` from scratch.
/// Empty query -> all indices (in order). The needle is lowercased once.
pub(super) fn filter_visible(
    emails: &[EmailEntry],
    query: &str,
    kind: MailboxKind,
    bodies: Option<&SearchBodies>,
) -> Vec<usize> {
    if query.is_empty() {
        return (0..emails.len()).collect();
    }
    let needle = query.to_lowercase();
    emails
        .iter()
        .enumerate()
        .filter(|(_, e)| email_matches(e, &needle, kind, bodies))
        .map(|(i, _)| i)
        .collect()
}

/// Narrow an existing visible set in place: keep only the indices whose
/// email still matches the (extended) needle. Valid only when the new
/// query is an extension of the one `visible` was built from.
fn narrow_visible(
    emails: &[EmailEntry],
    visible: &mut Vec<usize>,
    needle_lower: &str,
    kind: MailboxKind,
    bodies: Option<&SearchBodies>,
) {
    visible.retain(|&i| {
        emails
            .get(i)
            .is_some_and(|e| email_matches(e, needle_lower, kind, bodies))
    });
}

// ---------------------------------------------------------------------------
// Compose wizard free helpers
// ---------------------------------------------------------------------------

fn current_field_mut(wizard: &mut ComposeWizard) -> &mut String {
    match wizard.focus {
        ComposeField::To => &mut wizard.to,
        ComposeField::Cc => &mut wizard.cc,
        ComposeField::Bcc => &mut wizard.bcc,
        ComposeField::Subject => &mut wizard.subject,
    }
}

/// Aerc-style suggestion acceptance: replace the trailing partial
/// (everything after the last comma) with the suggestion's address,
/// then append `, ` so the user can keep typing more recipients.
fn accept_suggestion(field: &mut String, suggestion: &ComposeSuggestion) {
    let prefix_end = field.rfind(',').map(|i| i + 1).unwrap_or(0);
    field.truncate(prefix_end);
    if prefix_end > 0 && !field.ends_with(' ') {
        field.push(' ');
    }
    // If we have a display name, render "Name <addr>, ". Otherwise just "addr, ".
    if suggestion.display_name.is_empty() {
        field.push_str(&suggestion.address);
    } else {
        // Quote the display name if it contains commas.
        if suggestion.display_name.contains(',') {
            field.push('"');
            field.push_str(&suggestion.display_name);
            field.push('"');
        } else {
            field.push_str(&suggestion.display_name);
        }
        field.push_str(" <");
        field.push_str(&suggestion.address);
        field.push('>');
    }
    field.push_str(", ");
}

// ---------------------------------------------------------------------------
// Quick-move mailbox picker helpers (#0018)
// ---------------------------------------------------------------------------

/// Case-insensitive subsequence ("fuzzy") match: every char of `needle`
/// must appear in `haystack` in order. Empty needle matches everything.
pub(super) fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    let hay = haystack.to_lowercase();
    let mut hay_chars = hay.chars();
    needle
        .to_lowercase()
        .chars()
        .all(|nc| hay_chars.any(|hc| hc == nc))
}

/// Recompute `picker.filtered` from `picker.query` and reset the cursor.
pub(super) fn refresh_mailbox_picker_filter(picker: &mut MailboxPicker) {
    picker.filtered = picker
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, (_, label))| fuzzy_match(&picker.query, label))
        .map(|(i, _)| i)
        .collect();
    picker.selected = 0;
}

// ---------------------------------------------------------------------------
// Directory picker helpers
// ---------------------------------------------------------------------------

/// Check whether `zoxide` is available on PATH.
fn which_zoxide() -> bool {
    std::process::Command::new("zoxide")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run `zoxide query --list <terms>` and populate `picker.zoxide_results`.
fn refresh_zoxide_results(picker: &mut DirPicker) {
    if !picker.zoxide_available {
        picker.zoxide_results.clear();
        return;
    }

    let mut cmd = std::process::Command::new("zoxide");
    cmd.arg("query").arg("--list");
    if !picker.query.is_empty() {
        // Split query on whitespace so "no do" becomes two positional args
        for term in picker.query.split_whitespace() {
            cmd.arg(term);
        }
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    picker.zoxide_results = match cmd.output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .lines()
                .filter(|l| !l.is_empty())
                .take(20)
                .map(|l| PathBuf::from(l.trim()))
                .collect()
        }
        _ => Vec::new(),
    };
}

/// Scan `picker.current_dir` for sub-directories and populate `picker.dir_entries`.
fn refresh_browser_entries(picker: &mut DirPicker) {
    picker.dir_entries = match std::fs::read_dir(&picker.current_dir) {
        Ok(rd) => {
            let mut dirs: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
                .filter(|e| {
                    // Hide dotfiles
                    e.file_name()
                        .to_str()
                        .map(|s| !s.starts_with('.'))
                        .unwrap_or(false)
                })
                .map(|e| e.path())
                .collect();
            dirs.sort_by(|a, b| {
                a.file_name()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .cmp(&b.file_name().unwrap_or_default().to_ascii_lowercase())
            });
            dirs
        }
        Err(_) => Vec::new(),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{AttachmentPicker, EntryKey};
    use super::super::PersistentError;
    use std::path::PathBuf;

    /// A `MessageRef` derived from the subject, so the same fixture email
    /// built twice (a reload delivering a re-sorted `sample()`) carries the
    /// same identity, exactly as two loads of one store row would. The
    /// subject is the fixtures' de-facto primary key.
    fn ref_for(subject: &str) -> MessageRef {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        subject.hash(&mut h);
        MessageRef::new((h.finish() >> 1) as i64)
    }

    fn entry(subject: &str, from: &str) -> EmailEntry {
        EmailEntry {
            msg: Some(ref_for(subject)),
            draft_id: None,
            skip: None,
            from: from.to_string(),
            to: "me@example.com".to_string(),
            cc: None,
            subject: subject.to_string(),
            status: "inbox".to_string(),
            date_display: "2026-07-01".to_string(),
            date_sort: "2026-07-01T00:00:00".to_string(),
            has_attachments: false,
            read: false,
            answered: false,
            forwarded: false,
            flagged: false,
            is_invite: false,
        }
    }

    fn sample() -> Vec<EmailEntry> {
        vec![
            entry("Invoice March", "Alice"),
            entry("Invoice April", "Bob"),
            entry("Weekly report", "Alice"),
            entry("Holiday plans", "Carol"),
        ]
    }

    /// The bodies of `sample()`, in the shape body search now takes them:
    /// an index the store filled, keyed by the same `MessageRef` the entry
    /// carries (#0038 scope item 5). These are the exact strings the entries
    /// used to hold in an `EmailEntry.body` field.
    fn sample_bodies() -> Vec<(MessageRef, String)> {
        [
            ("Invoice March", "please pay"),
            ("Invoice April", "reminder"),
            ("Weekly report", "invoice attached"),
            ("Holiday plans", "beach"),
        ]
        .into_iter()
        .map(|(subject, body)| (ref_for(subject), body.to_string()))
        .collect()
    }

    fn sample_index() -> SearchBodies {
        SearchBodies::for_tests(sample_bodies())
    }

    // -----------------------------------------------------------------------
    // filter_visible (P2: visible-index mapping)
    // -----------------------------------------------------------------------

    #[test]
    fn empty_query_yields_all_indices_in_order() {
        let emails = sample();
        assert_eq!(
            filter_visible(&emails, "", MailboxKind::Inbox, None),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn filter_matches_subject_case_insensitively() {
        let emails = sample();
        assert_eq!(
            filter_visible(&emails, "INVOICE", MailboxKind::Inbox, None),
            vec![0, 1]
        );
    }

    #[test]
    fn filter_matches_contact_field() {
        let emails = sample();
        // Inbox displays `from`; Alice appears in entries 0 and 2.
        assert_eq!(
            filter_visible(&emails, "alice", MailboxKind::Inbox, None),
            vec![0, 2]
        );
    }

    #[test]
    fn body_matches_only_when_included() {
        let emails = sample();
        assert_eq!(
            filter_visible(&emails, "beach", MailboxKind::Inbox, None),
            Vec::<usize>::new()
        );
        assert_eq!(
            filter_visible(&emails, "beach", MailboxKind::Inbox, Some(&sample_index())),
            vec![3]
        );
    }

    #[test]
    fn filter_indices_map_back_to_underlying_entries() {
        let emails = sample();
        let visible = filter_visible(&emails, "invoice", MailboxKind::Inbox, Some(&sample_index()));
        // "invoice" hits subjects 0/1 and the body of 2 (content search).
        assert_eq!(visible, vec![0, 1, 2]);
        // Selecting position 2 of the view must resolve to "Weekly report":
        // the actions layer operates on the underlying entry via this map.
        assert_eq!(emails[visible[2]].subject, "Weekly report");
    }

    // -----------------------------------------------------------------------
    // narrow_visible (P3: incremental search narrowing)
    // -----------------------------------------------------------------------

    #[test]
    fn narrowing_equals_full_recompute_on_append() {
        let emails = sample();
        // Simulate typing "inv" then "invo": narrow from the "inv" view.
        let bodies = sample_index();
        let mut visible = filter_visible(&emails, "inv", MailboxKind::Inbox, Some(&bodies));
        narrow_visible(&emails, &mut visible, "invo", MailboxKind::Inbox, Some(&bodies));
        assert_eq!(
            visible,
            filter_visible(&emails, "invo", MailboxKind::Inbox, Some(&bodies))
        );
    }

    #[test]
    fn narrowing_removes_entries_that_stop_matching() {
        let emails = sample();
        let mut visible = filter_visible(&emails, "invoice", MailboxKind::Inbox, None);
        assert_eq!(visible, vec![0, 1]);
        narrow_visible(&emails, &mut visible, "invoice m", MailboxKind::Inbox, None);
        assert_eq!(visible, vec![0]);
        narrow_visible(&emails, &mut visible, "invoice mx", MailboxKind::Inbox, None);
        assert!(visible.is_empty());
    }

    #[test]
    fn narrowing_ignores_out_of_range_indices() {
        let emails = sample();
        // A stale index beyond the list must be dropped, not panic.
        let mut visible = vec![0, 99];
        narrow_visible(&emails, &mut visible, "invoice", MailboxKind::Inbox, None);
        assert_eq!(visible, vec![0]);
    }

    #[test]
    fn greek_final_sigma_append_falls_back_to_full_recompute() {
        // Lowercasing is context-sensitive for Greek capital sigma:
        // "ΘΕΟΣ".to_lowercase() == "θεος" (final ς) does NOT match the
        // haystack "θεοσφανια", but "ΘΕΟΣΦ".to_lowercase() == "θεοσφ"
        // (medial σ) does. Naive narrowing over the previous visible set
        // would drop the entry forever; the fifth keystroke must fall
        // back to a full recompute and bring it back.
        let mut emails = sample();
        emails.push(entry("θεοσφανια", "Dora"));
        let theos_idx = emails.len() - 1;
        let mut app = app_with_emails(emails);
        app.focus = Focus::Search;
        for c in "ΘΕΟΣ".chars() {
            app.handle_search_key(KeyEvent::from(KeyCode::Char(c)));
        }
        // "θεος" matches nothing.
        assert!(app.visible.is_empty());
        app.handle_search_key(KeyEvent::from(KeyCode::Char('Φ')));
        // "θεοσφ" matches the haystack again.
        assert_eq!(app.visible, vec![theos_idx]);
    }

    // -----------------------------------------------------------------------
    // App-level: selection mapping through the visible view
    // -----------------------------------------------------------------------

    fn app_with_emails(emails: Vec<EmailEntry>) -> App {
        let mut app = App::default_for_tests();
        app.emails = std::sync::Arc::new(emails);
        app.email_cache = vec![Some(std::sync::Arc::clone(&app.emails))];
        app.mailbox_counts = vec![app.emails.len()];
        app.rebuild_visible();
        // The fixture has no store behind it, so the index body search would
        // have read from the blob store is primed instead.
        app.prime_search_bodies(sample_bodies());
        app
    }

    /// A parked conversation-jump target lands on its row once the list holds
    /// it, and clears itself so a later switch is not hijacked (#0008).
    #[test]
    fn a_pending_select_lands_on_its_row_and_clears() {
        let mut app = app_with_emails(sample());
        let target = app.emails[2].msg.unwrap();
        app.list_index = 0;
        app.pending_select = Some(target);
        app.consume_pending_select();
        assert_eq!(app.list_index, 2, "the cursor moved onto the parked target");
        assert!(app.pending_select.is_none(), "the target cleared once it landed");
    }

    /// A target the current list does not hold is left parked (the async load
    /// for a cross-mailbox jump has not arrived yet), not silently dropped.
    #[test]
    fn a_pending_select_absent_from_the_list_stays_parked() {
        let mut app = app_with_emails(sample());
        let absent = MessageRef::new(-999);
        app.list_index = 1;
        app.pending_select = Some(absent);
        app.consume_pending_select();
        assert_eq!(app.list_index, 1, "the cursor did not move");
        assert_eq!(app.pending_select, Some(absent), "the target is still parked");
    }

    /// A Drafts row: no `messages` row behind it, its indexed `id:` instead
    /// (what `entry_from_draft` builds).
    fn draft_entry(id: &str, subject: &str) -> EmailEntry {
        EmailEntry {
            msg: None,
            draft_id: Some(id.to_string()),
            skip: None,
            subject: subject.to_string(),
            status: "draft".to_string(),
            read: true,
            answered: false,
            forwarded: false,
            ..entry(subject, "me")
        }
    }

    /// A draft can enter the selection (#0052).
    ///
    /// It could not while the set was keyed on `MessageRef`: a draft has no
    /// store row, so `Ctrl+a` in Drafts selected nothing and `A` fell through
    /// to the single-draft path. The batch it guarded was reachable by
    /// keystroke and dead in fact.
    #[test]
    fn select_all_in_drafts_takes_the_draft_keys() {
        let mut app = app_with_emails(vec![draft_entry("aaa", "One"), draft_entry("bbb", "Two")]);

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));

        assert_eq!(app.selection.len(), 2);
        let mut ids: Vec<&str> = app.selection.iter().filter_map(|k| k.draft()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["aaa", "bbb"]);
    }

    /// `v` toggles a Drafts row in and out of the selection like any other.
    #[test]
    fn toggling_selects_the_draft_under_the_cursor() {
        let mut app = app_with_emails(vec![draft_entry("aaa", "One"), draft_entry("bbb", "Two")]);

        app.handle_key(KeyEvent::from(KeyCode::Char('v')));
        assert_eq!(
            app.selection.iter().next().and_then(|k| k.draft()),
            Some("aaa")
        );
        // The cursor moved on; toggling the second row leaves both selected.
        app.handle_key(KeyEvent::from(KeyCode::Char('v')));
        assert_eq!(app.selection.len(), 2);
    }

    /// `A` over a Drafts selection confirms, then queues the batch by draft
    /// id: approving is a write to a file, and a draft has no `MessageRef`
    /// to name it by.
    #[test]
    fn confirming_a_drafts_selection_queues_the_batch_by_id() {
        let mut app = app_with_emails(vec![draft_entry("aaa", "One"), draft_entry("bbb", "Two")]);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));

        app.handle_key(KeyEvent::from(KeyCode::Char('A')));
        assert!(matches!(app.overlay, Overlay::Confirm(_)));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));

        match app.pending_actions.pop_front() {
            Some(Action::BatchApprove(mut ids)) => {
                ids.sort();
                assert_eq!(ids, vec!["aaa".to_string(), "bbb".to_string()]);
            }
            other => panic!("expected BatchApprove, got {other:?}"),
        }
        assert!(app.selection.is_empty(), "the confirm drained the selection");
    }

    /// `A` and `D` over a received-mail selection ask nothing: the batch takes
    /// the drafts half of the set, so a selection with no draft in it would
    /// have confirmed "Approve 4 drafts?" and then reported "Approved 0
    /// drafts".
    #[test]
    fn a_selection_without_drafts_never_opens_the_draft_confirmation() {
        let mut app = app_with_emails(sample());
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.selection.len(), 4, "the received rows are selected");

        app.handle_key(KeyEvent::from(KeyCode::Char('A')));
        assert!(matches!(app.overlay, Overlay::None), "no dialog opens");
        assert_eq!(
            app.status_message.as_deref(),
            Some("Approve needs drafts; the selection has no draft in it")
        );
        assert!(app.pending_actions.is_empty(), "and nothing is queued");

        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert!(matches!(app.overlay, Overlay::None), "no dialog opens");
        assert_eq!(
            app.status_message.as_deref(),
            Some("Mark as draft needs drafts; the selection has no draft in it")
        );
        assert!(app.pending_actions.is_empty(), "and nothing is queued");
        assert_eq!(app.selection.len(), 4, "the selection is left alone");
    }

    /// A selection cannot mix the two namespaces in practice -- a mailbox
    /// lists one kind of row and switching mailboxes clears the set -- but
    /// each batch filters rather than assumes, so neither can act on the
    /// other's rows.
    #[test]
    fn each_batch_takes_only_its_own_half_of_a_mixed_selection() {
        let mut app = app_with_emails(sample());
        let msg = app.emails[0].msg.unwrap();
        app.selection = std::collections::HashSet::from([
            EntryKey::Msg(msg),
            EntryKey::Draft("aaa".to_string()),
        ]);

        app.handle_key(KeyEvent::from(KeyCode::Char('A')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        match app.pending_actions.pop_front() {
            Some(Action::BatchApprove(ids)) => assert_eq!(ids, vec!["aaa".to_string()]),
            other => panic!("expected BatchApprove, got {other:?}"),
        }

        app.selection = std::collections::HashSet::from([
            EntryKey::Msg(msg),
            EntryKey::Draft("aaa".to_string()),
        ]);
        app.handle_key(KeyEvent::from(KeyCode::Char('d')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        match app.pending_actions.pop_front() {
            Some(Action::BatchDelete(msgs)) => assert_eq!(msgs, vec![msg]),
            other => panic!("expected BatchDelete, got {other:?}"),
        }
    }

    /// `d` over a Drafts selection deletes the draft files, not store rows
    /// (#0073): a draft has no `messages` row, so the old `BatchDelete` half
    /// found nothing and reported "nothing to delete".
    #[test]
    fn delete_over_a_drafts_selection_takes_the_draft_ids() {
        let mut app = app_with_emails(vec![draft_entry("aaa", "One"), draft_entry("bbb", "Two")]);
        app.selection = std::collections::HashSet::from([
            EntryKey::Draft("aaa".to_string()),
            EntryKey::Draft("bbb".to_string()),
        ]);
        app.handle_key(KeyEvent::from(KeyCode::Char('d')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        match app.pending_actions.pop_front() {
            Some(Action::BatchDeleteDrafts(mut ids)) => {
                ids.sort();
                assert_eq!(ids, vec!["aaa".to_string(), "bbb".to_string()]);
            }
            other => panic!("expected BatchDeleteDrafts, got {other:?}"),
        }
        assert!(app.selection.is_empty(), "the selection is consumed");
    }

    /// `y` queues the selector copy, not the dead path copy (#0050 scope item
    /// 7). Dispatch-level only, like the contacts copy test: the `arboard`
    /// call lives in `actions.rs` and would touch the real system clipboard.
    #[test]
    fn copy_key_queues_the_selector_copy() {
        let mut app = app_with_emails(sample());
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        assert!(
            matches!(app.pending_actions.pop_front(), Some(Action::CopyMessageRef)),
            "y must queue CopyMessageRef"
        );
    }

    #[test]
    fn selected_email_resolves_through_visible_indices() {
        let mut app = app_with_emails(sample());
        app.search_query = "invoice".to_string();
        app.search_includes_body = true;
        app.apply_search_filter(false);
        // View: [Invoice March, Invoice April, Weekly report]
        app.list_index = 2;
        assert_eq!(app.selected_email().unwrap().subject, "Weekly report");
    }

    #[test]
    fn remove_selected_maps_back_to_underlying_entry_under_filter() {
        let mut app = app_with_emails(sample());
        app.search_query = "invoice".to_string();
        app.search_includes_body = true;
        app.apply_search_filter(false);
        app.list_index = 2; // "Weekly report" via body match

        let removed = app.remove_selected_from_list().unwrap();
        assert_eq!(removed, ref_for("Weekly report"));
        // The underlying list lost exactly that entry...
        assert_eq!(app.emails.len(), 3);
        assert!(app.emails.iter().all(|e| e.subject != "Weekly report"));
        // ...the cache slot shares the same (updated) allocation...
        let cached = app.email_cache[0].as_ref().unwrap();
        assert!(std::sync::Arc::ptr_eq(cached, &app.emails));
        // ...and the filtered view is rebuilt with the cursor clamped.
        assert_eq!(app.visible, vec![0, 1]);
        assert_eq!(app.list_index, 1);
    }

    #[test]
    fn set_email_read_updates_shared_cache_without_deep_clone() {
        let mut app = app_with_emails(sample());
        let msg = app.emails[1].msg.unwrap();
        app.set_email_read(msg, true);
        assert!(app.emails[1].read);
        let cached = app.email_cache[0].as_ref().unwrap();
        assert!(std::sync::Arc::ptr_eq(cached, &app.emails));
        assert!(cached[1].read);
    }

    #[test]
    fn with_emails_mut_leaves_invalidated_cache_slot_none() {
        let mut app = app_with_emails(sample());
        app.email_cache[0] = None; // e.g. load in flight
        let msg = app.emails[0].msg.unwrap();
        app.set_email_read(msg, true);
        assert!(app.emails[0].read);
        assert!(app.email_cache[0].is_none());
    }

    // -----------------------------------------------------------------------
    // Cursor stability across list rebuilds
    //
    // `list_index` is a bare position into `visible`, so any reload that
    // re-sorts or grows the entry list used to move the cursor to a
    // different email (approving a draft re-sorted the list; new inbox mail
    // shifted every row down under queued keystrokes). The cursor is
    // anchored on the entry's `MessageRef` and restored after the rebuild.
    // -----------------------------------------------------------------------

    /// Deliver a fresh entry list through the real async funnel
    /// (`BgResult::MailboxLoaded`), the single path every reload takes.
    fn deliver_mailbox_load(app: &mut App, entries: Vec<EmailEntry>) {
        crate::tui::bg::handle_bg_result(
            app,
            super::super::BgResult::MailboxLoaded {
                account_index: app.active_account,
                mailbox_idx: app.active_mailbox,
                generation: app.mailbox_load_generation,
                entries,
            },
        );
    }

    #[test]
    fn cursor_follows_its_email_when_the_reload_resorts_the_list() {
        let mut app = app_with_emails(sample());
        app.list_index = 1;
        let anchored = app.selected_email().unwrap().msg;

        // The approved draft's status/date changed, so the reload sorts it
        // last instead of second.
        let mut resorted = sample();
        let moved = resorted.remove(1);
        resorted.push(moved);
        deliver_mailbox_load(&mut app, resorted);

        assert_eq!(app.list_index, 3);
        assert_eq!(app.selected_email().unwrap().msg, anchored);
    }

    #[test]
    fn cursor_stays_put_when_new_mail_is_prepended() {
        let mut app = app_with_emails(sample());
        app.list_index = 2;
        let anchored = app.selected_email().unwrap().msg;

        // New inbox mail sorts above everything and shifts every row down.
        let mut grown = vec![entry("Fresh arrival", "Dave")];
        grown.extend(sample());
        deliver_mailbox_load(&mut app, grown);

        assert_eq!(app.list_index, 3);
        assert_eq!(app.selected_email().unwrap().msg, anchored);
    }

    #[test]
    fn cursor_falls_back_to_the_clamped_index_when_its_email_is_gone() {
        let mut app = app_with_emails(sample());
        app.list_index = 3; // "Holiday plans"

        // The reload lost the anchored email (archived from another client)
        // and is shorter than the old cursor position.
        let shorter = vec![sample()[0].clone(), sample()[1].clone()];
        deliver_mailbox_load(&mut app, shorter);

        assert_eq!(app.list_index, 1);
        assert_eq!(app.selected_email().unwrap().subject, "Invoice April");
    }

    #[test]
    fn empty_reload_leaves_the_cursor_at_zero() {
        let mut app = app_with_emails(sample());
        app.list_index = 3;
        deliver_mailbox_load(&mut app, Vec::new());
        assert_eq!(app.list_index, 0);
        assert!(app.selected_email().is_none());
    }

    #[test]
    fn batch_removal_above_the_cursor_does_not_drag_it() {
        // Six rows, cursor in the middle: with only four survivors the old
        // clamp (`min(list_index, len - 1)`) would have left the cursor at
        // row 3, two emails below the one the user was looking at.
        let mut emails = sample();
        emails.push(entry("Team sync", "Dave"));
        emails.push(entry("Renewal notice", "Eve"));
        let mut app = app_with_emails(emails);
        app.list_index = 3; // "Holiday plans"
        let anchored = app.selected_email().unwrap().msg;

        // Archive the two rows above the cursor: the cursor's own email
        // survives, so it must stay under the cursor (at its new position).
        let mut batch = std::collections::HashSet::new();
        batch.insert(app.emails[0].msg.unwrap());
        batch.insert(app.emails[1].msg.unwrap());
        let removed = app.remove_selected_from_list_batch(&batch);

        assert_eq!(removed.len(), 2);
        assert_eq!(app.list_index, 1);
        assert_eq!(app.selected_email().unwrap().msg, anchored);
    }

    #[test]
    fn batch_removal_including_the_cursor_lands_on_the_next_survivor() {
        let mut emails = sample();
        emails.push(entry("Team sync", "Dave"));
        emails.push(entry("Renewal notice", "Eve"));
        let mut app = app_with_emails(emails);
        app.list_index = 3; // "Holiday plans"

        // The cursor's own row is part of the batch, together with two rows
        // above it: the cursor falls back to the count of survivors above
        // it, i.e. the row that took its place.
        let mut batch = std::collections::HashSet::new();
        batch.insert(app.emails[0].msg.unwrap());
        batch.insert(app.emails[1].msg.unwrap());
        batch.insert(app.emails[3].msg.unwrap());
        app.remove_selected_from_list_batch(&batch);

        assert_eq!(app.list_index, 1);
        assert_eq!(app.selected_email().unwrap().subject, "Team sync");
    }

    /// A deleted row's `MessageRef` must not survive in the selection.
    ///
    /// The id of a deleted row is not reserved: the next ingest is handed the
    /// same number (pinned by `store::write`'s
    /// `a_deleted_row_id_can_be_handed_to_the_next_message`), so a reference
    /// held across the boundary can name a *different* message. The list drops
    /// it, and so must the selection set (#0038 scope item 7).
    #[test]
    fn removing_a_row_drops_its_reference_from_the_selection() {
        let mut app = app_with_emails(sample());
        app.list_index = 2;
        let doomed = app.selected_email().unwrap().msg.unwrap();
        let survivor = app.emails[0].msg.unwrap();
        app.selection.insert(EntryKey::Msg(doomed));
        app.selection.insert(EntryKey::Msg(survivor));

        app.remove_selected_from_list();

        assert!(
            !app.selection.contains(&EntryKey::Msg(doomed)),
            "the selection kept a reference to a row that no longer exists"
        );
        assert!(app.selection.contains(&EntryKey::Msg(survivor)));
        assert!(app.cursor_anchor().is_none_or(|m| m != doomed));
    }

    /// Same guarantee for a batch: every removed reference leaves the
    /// selection, so a follow-up mutation cannot act on a freed row id.
    #[test]
    fn batch_removal_drops_every_removed_reference_from_the_selection() {
        let mut app = app_with_emails(sample());
        let batch: std::collections::HashSet<MessageRef> = app.emails[..2]
            .iter()
            .filter_map(|e| e.msg)
            .collect();
        app.selection = app.emails.iter().filter_map(|e| e.key()).collect();

        app.remove_selected_from_list_batch(&batch);

        assert_eq!(app.selection.len(), 2);
        assert!(app
            .selection
            .iter()
            .all(|k| !k.msg().is_some_and(|m| batch.contains(&m))));
        assert!(app.emails.iter().all(|e| !e.msg.is_some_and(|m| batch.contains(&m))));
    }

    // -----------------------------------------------------------------------
    // Quick-move mailbox picker (#0018)
    // -----------------------------------------------------------------------

    #[test]
    fn fuzzy_match_empty_needle_matches_everything() {
        assert!(fuzzy_match("", "Inbox"));
        assert!(fuzzy_match("", ""));
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_subsequence() {
        assert!(fuzzy_match("arc", "Archive"));
        assert!(fuzzy_match("ACV", "archive")); // a..c..v in order
        assert!(fuzzy_match("inbx", "Inbox"));
        assert!(!fuzzy_match("xz", "Inbox"));
        assert!(!fuzzy_match("xobni", "Inbox")); // order matters
    }

    fn picker_with_labels(labels: &[&str]) -> super::super::MailboxPicker {
        super::super::MailboxPicker {
            query: String::new(),
            candidates: labels
                .iter()
                .enumerate()
                .map(|(i, l)| (i, l.to_string()))
                .collect(),
            filtered: (0..labels.len()).collect(),
            selected: 0,
            msgs: vec![MessageRef::new(1)],
        }
    }

    #[test]
    fn picker_filter_narrows_and_resets_cursor() {
        let mut picker = picker_with_labels(&["Inbox", "Sent", "Archive", "Newsletters"]);
        picker.selected = 3;
        picker.query = "ne".to_string();
        refresh_mailbox_picker_filter(&mut picker);
        // "ne" (subsequence): Se~n~t? no 'e' after 'n'... Sent = s,e,n,t:
        // n then e fails (e precedes n). Archive has no 'n'. Newsletters
        // and Inbox? i-n-b-o-x: n then no e. Only Newsletters matches.
        assert_eq!(picker.filtered, vec![3]);
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn picker_filter_empty_query_restores_all() {
        let mut picker = picker_with_labels(&["Inbox", "Archive"]);
        picker.query = "arch".to_string();
        refresh_mailbox_picker_filter(&mut picker);
        assert_eq!(picker.filtered, vec![1]);
        picker.query.clear();
        refresh_mailbox_picker_filter(&mut picker);
        assert_eq!(picker.filtered, vec![0, 1]);
    }

    #[test]
    fn picker_filter_no_match_yields_empty() {
        let mut picker = picker_with_labels(&["Inbox", "Archive"]);
        picker.query = "zzz".to_string();
        refresh_mailbox_picker_filter(&mut picker);
        assert!(picker.filtered.is_empty());
    }

    fn mb_info(label: &str, id: &str, kind: MailboxKind, server: Option<&str>) -> super::super::MailboxInfo {
        super::super::MailboxInfo {
            label: label.to_string(),
            icon: "",
            id: id.to_string(),
            kind,
            server_name: server.map(|s| s.to_string()),
        }
    }

    fn app_with_mailboxes() -> App {
        let mut app = app_with_emails(sample());
        app.mailboxes = vec![
            mb_info("Inbox", "inbox", MailboxKind::Inbox, Some("INBOX")),
            mb_info("Drafts", "drafts", MailboxKind::Drafts, None),
            mb_info("Sent", "sent", MailboxKind::Sent, Some("Sent")),
            mb_info("Archive", "archive", MailboxKind::Archive, Some("Archive")),
        ];
        app.active_mailbox = 0;
        app
    }

    #[test]
    fn open_picker_excludes_active_mailbox_and_local_only() {
        let mut app = app_with_mailboxes();
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        let Overlay::Mailbox(picker) = &app.overlay else {
            panic!("picker should open");
        };
        // Active mailbox (Inbox) and local-only Drafts are excluded.
        let labels: Vec<&str> = picker
            .candidates
            .iter()
            .map(|(_, l)| l.as_str())
            .collect();
        assert_eq!(labels, vec!["Sent", "Archive"]);
        // Cursor email is carried as the move target.
        assert_eq!(picker.msgs.len(), 1);
    }

    #[test]
    fn open_picker_uses_selection_when_present() {
        let mut app = app_with_mailboxes();
        app.selection.insert(EntryKey::Msg(app.emails[0].msg.unwrap()));
        app.selection.insert(EntryKey::Msg(app.emails[2].msg.unwrap()));
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        let Overlay::Mailbox(picker) = &app.overlay else {
            panic!("picker should open");
        };
        assert_eq!(picker.msgs.len(), 2);
    }

    #[test]
    fn open_picker_refused_in_local_only_mailbox() {
        let mut app = app_with_mailboxes();
        app.active_mailbox = 1; // Drafts: no server folder
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        assert!(!matches!(app.overlay, Overlay::Mailbox(_)));
    }

    #[test]
    fn picker_enter_pushes_move_action_with_dest() {
        let mut app = app_with_mailboxes();
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        // Filter to "Archive", then confirm.
        for c in "arch".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(!matches!(app.overlay, Overlay::Mailbox(_)));
        match app.pending_actions.pop_front() {
            Some(Action::MoveToMailbox { msgs, dest_idx }) => {
                assert_eq!(dest_idx, 3); // Archive
                assert_eq!(msgs.len(), 1);
            }
            other => panic!("expected MoveToMailbox, got {:?}", other),
        }
    }

    #[test]
    fn picker_esc_closes_without_action() {
        let mut app = app_with_mailboxes();
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        assert!(matches!(app.overlay, Overlay::Mailbox(_)));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!matches!(app.overlay, Overlay::Mailbox(_)));
        assert!(app.pending_actions.is_empty());
    }

    #[test]
    fn picker_enter_on_no_match_is_noop() {
        let mut app = app_with_mailboxes();
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        for c in "zzz".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        // No match highlighted: picker stays open, nothing queued.
        assert!(matches!(app.overlay, Overlay::Mailbox(_)));
        assert!(app.pending_actions.is_empty());
    }

    // -----------------------------------------------------------------------
    // Flagged-only view (#0079)
    // -----------------------------------------------------------------------

    /// `F` narrows the list to flagged rows and widens it back, keeping the
    /// cursor on the row it was on when that row survives the narrowing.
    #[test]
    fn f_toggles_the_flagged_only_view() {
        let mut app = app_with_emails(sample());
        let mut emails = (*app.emails).clone();
        emails[1].flagged = true;
        app.emails = std::sync::Arc::new(emails);
        app.email_cache = vec![Some(std::sync::Arc::clone(&app.emails))];
        app.rebuild_visible();
        app.list_index = 1;

        app.handle_key(KeyEvent::from(KeyCode::Char('F')));

        assert!(app.flagged_only);
        assert_eq!(app.visible, vec![1]);
        assert_eq!(app.list_index, 0, "cursor followed its row");

        app.handle_key(KeyEvent::from(KeyCode::Char('F')));

        assert!(!app.flagged_only);
        assert_eq!(app.visible, vec![0, 1, 2, 3]);
        assert_eq!(app.list_index, 1, "cursor still on the same row");
    }

    /// The flagged view and the `/` search are independent narrowings: what is
    /// visible is the intersection, whichever order they were armed in.
    #[test]
    fn the_flagged_view_intersects_with_the_search_filter() {
        let mut app = app_with_emails(sample());
        let mut emails = (*app.emails).clone();
        for e in emails.iter_mut() {
            e.flagged = true;
        }
        emails[3].flagged = false;
        app.emails = std::sync::Arc::new(emails);
        app.email_cache = vec![Some(std::sync::Arc::clone(&app.emails))];
        app.rebuild_visible();

        app.handle_key(KeyEvent::from(KeyCode::Char('F')));
        assert_eq!(app.visible, vec![0, 1, 2]);

        // The query that alone would match only the one unflagged row.
        app.search_query = "holiday".to_string();
        app.apply_search_filter(false);
        assert!(app.visible.is_empty(), "flagged view still applies");

        app.handle_key(KeyEvent::from(KeyCode::Char('F')));
        assert_eq!(app.visible, vec![3], "search alone again");
    }

    /// The toggle is not guarded by a non-empty list: a filter that emptied
    /// the list must still be undoable with the key that armed it.
    #[test]
    fn the_flagged_view_can_be_left_when_it_shows_nothing() {
        let mut app = app_with_emails(sample());

        app.handle_key(KeyEvent::from(KeyCode::Char('F')));
        assert!(app.visible.is_empty(), "no flagged rows in the fixture");

        app.handle_key(KeyEvent::from(KeyCode::Char('F')));
        assert!(!app.flagged_only);
        assert_eq!(app.visible, vec![0, 1, 2, 3]);
    }

    #[test]
    fn clearing_search_restores_full_view() {
        let mut app = app_with_emails(sample());
        app.search_query = "holiday".to_string();
        app.apply_search_filter(false);
        assert_eq!(app.visible, vec![3]);
        app.search_query.clear();
        app.reload_from_cache();
        assert_eq!(app.visible, vec![0, 1, 2, 3]);
        assert_eq!(app.list_index, 0);
    }

    // -----------------------------------------------------------------------
    // Background error over an open overlay (#0032 regression)
    //
    // `set_persistent_error` is called from bg.rs regardless of overlay
    // state. It must NOT clobber an active overlay (that both destroys the
    // overlay's unsaved state and, for the compose wizard, leaves
    // `focus == Focus::ComposeWizard` while `overlay == None` -- a state
    // that hits `keys.rs`'s `unreachable!()` on the next keystroke). The
    // error is queued and promoted to `Overlay::Error` when the overlay
    // actually closes.
    // -----------------------------------------------------------------------

    fn open_compose_wizard_for_test(app: &mut App) {
        app.overlay = Overlay::Compose(ComposeWizard {
            mode: ComposeMode::New,
            to: "alice@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "draft in progress".to_string(),
            focus: ComposeField::To,
            suggestions: Vec::new(),
            suggestion_idx: 0,
            contacts: None,
        });
        app.focus = Focus::ComposeWizard;
    }

    /// A background failure while the compose wizard is open must not
    /// clobber the wizard (which would strand `focus == ComposeWizard` and
    /// panic on the next key). The error is queued, surfaced on the status
    /// line, and promoted only after the wizard closes -- and the wizard
    /// close resets focus, so the panicking `None`+`ComposeWizard` combo
    /// never occurs.
    #[test]
    fn bg_error_over_compose_wizard_does_not_clobber_or_panic() {
        let mut app = app_with_emails(sample());
        open_compose_wizard_for_test(&mut app);

        app.set_persistent_error(
            "Archive failed: boom\nEmail restored to inbox. Sync (F) to fix?".to_string(),
        );

        // The wizard is preserved (draft intact), focus stays consistent
        // with the overlay, and the error is queued + surfaced immediately.
        assert!(
            matches!(app.overlay, Overlay::Compose(_)),
            "wizard must not be clobbered by a background error"
        );
        assert_eq!(app.focus, Focus::ComposeWizard);
        assert!(app.pending_error.is_some());
        assert_eq!(app.status_message.as_deref(), Some("Archive failed: boom"));

        // Close the wizard the way `Action::ComposeWizardCancel` does.
        app.close_overlay();
        app.focus = Focus::List;

        // The queued error is now the active overlay, and focus is a valid
        // pane -- the `overlay == None && focus == ComposeWizard` state that
        // would hit `unreachable!()` never exists.
        assert!(matches!(app.overlay, Overlay::Error(_)));
        assert_eq!(app.focus, Focus::List);
        assert!(app.pending_error.is_none());

        // Dismissing the promoted error returns to the normal view and does
        // not re-open anything.
        app.handle_key(KeyEvent::from(KeyCode::Char('d')));
        assert!(matches!(app.overlay, Overlay::None));
    }

    /// A background failure while the mailbox picker is open preserves the
    /// picker (and its selection/paths) and shows the error only after the
    /// picker closes.
    #[test]
    fn bg_error_over_mailbox_picker_preserves_picker_until_close() {
        let mut app = app_with_mailboxes();
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        assert!(matches!(app.overlay, Overlay::Mailbox(_)));

        app.set_persistent_error("Move failed: boom\nEmail restored.".to_string());

        // Picker (and its carried messages) survive; the error is queued.
        let Overlay::Mailbox(picker) = &app.overlay else {
            panic!("picker must not be clobbered by a background error");
        };
        assert_eq!(picker.msgs.len(), 1);
        assert!(app.pending_error.is_some());
        assert_eq!(app.status_message.as_deref(), Some("Move failed: boom"));

        // Closing the picker promotes the queued error.
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::Error(_)));
        assert!(app.pending_error.is_none());
    }

    /// With no overlay open, `set_persistent_error` behaves exactly as
    /// before: it opens the error overlay directly and queues nothing.
    #[test]
    fn persistent_error_with_no_overlay_opens_directly() {
        let mut app = app_with_emails(sample());
        app.set_persistent_error("Delete failed: boom".to_string());
        assert!(matches!(app.overlay, Overlay::Error(_)));
        assert!(app.pending_error.is_none());
    }

    /// The attachment picker -> dir picker handoff (`O` save) must not let a
    /// queued error fire mid-transition: promotion is guarded on
    /// `Overlay::None`, so after the handoff the dir picker is active and
    /// the error stays queued until the dir picker itself closes.
    #[test]
    fn pending_error_does_not_fire_during_attachment_to_dir_handoff() {
        let mut app = app_with_emails(sample());
        // Simulate the attachment save picker with a queued error behind it.
        app.overlay = Overlay::Attachment(AttachmentPicker {
            files: vec![PathBuf::from("/tmp/a.pdf")],
            selected: 0,
            mode: AttachmentPickerMode::Save,
            selected_set: std::collections::HashSet::new(),
        });
        app.pending_error = Some(PersistentError { message: "bg boom".to_string() });

        // Enter triggers the attachment -> dir picker handoff.
        app.handle_key(KeyEvent::from(KeyCode::Enter));

        // The dir picker is now active; the error did NOT fire in between.
        assert!(matches!(app.overlay, Overlay::Dir(_)));
        assert!(app.pending_error.is_some());
    }

    // -----------------------------------------------------------------------
    // Multi-view: leader switching, per-view key gating (#0033)
    // -----------------------------------------------------------------------

    use super::super::View;

    /// `Space c` / `Space a` / `Space m` switch the active top-level view; the
    /// leader is consumed and `App::view` updates. Space alone only arms the
    /// leader (#0033 follow-up: Space is the view leader).
    #[test]
    fn leader_switches_top_level_view() {
        let mut app = app_with_mailboxes();
        assert_eq!(app.view, View::Mail);

        // Bare Space arms the leader without switching.
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(app.pending_prefix, Some(' '), "Space must arm the leader");
        assert_eq!(app.view, View::Mail);

        // `Space c` -> Contacts.
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert_eq!(app.view, View::Contacts);
        assert_eq!(app.pending_prefix, None, "leader consumed by the switch");

        // `Space a` -> Calendar (proving Space arms from a non-Mail view).
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        app.handle_key(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(app.view, View::Calendar);

        // `Space m` -> back to Mail.
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        app.handle_key(KeyEvent::from(KeyCode::Char('m')));
        assert_eq!(app.view, View::Mail);
    }

    /// `v` toggles list selection (the binding that moved off Space, #0033
    /// follow-up). Space no longer touches selection.
    #[test]
    fn v_toggles_list_selection_and_space_does_not() {
        let mut app = app_with_mailboxes();
        app.focus = Focus::List;
        assert!(!app.visible.is_empty(), "fixture must have selectable emails");
        let before = app.selection.len();
        app.handle_key(KeyEvent::from(KeyCode::Char('v')));
        assert_eq!(app.selection.len(), before + 1, "v must add to the selection");

        // Space with a non-empty list arms the view leader instead of toggling.
        let sel_after_v = app.selection.len();
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(app.pending_prefix, Some(' '), "Space arms the view leader");
        assert_eq!(
            app.selection.len(),
            sel_after_v,
            "Space must not change the selection"
        );
    }

    /// `g` arms only where continuations exist. Both content views own
    /// `gg`/`G` jumps, so `g` arms in each (Calendar gained them in #0034 --
    /// this test previously asserted the opposite, when Calendar was a
    /// placeholder with no pane context).
    #[test]
    fn g_leader_arms_in_both_content_views() {
        let mut app = app_with_mailboxes();
        app.switch_view(View::Calendar);
        app.handle_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.pending_prefix, Some('g'), "g arms in Calendar (#0034)");

        app.switch_view(View::Contacts);
        app.handle_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.pending_prefix, Some('g'), "g arms in Contacts");
    }

    /// Space switches views from the Contacts view too (Global leader).
    #[test]
    fn space_leader_switches_view_from_contacts() {
        let mut app = app_with_mailboxes();
        app.switch_view(View::Contacts);
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(app.pending_prefix, Some(' '));
        app.handle_key(KeyEvent::from(KeyCode::Char('m')));
        assert_eq!(app.view, View::Mail);
    }

    /// ...and from the Calendar view, now that it owns a pane context (#0034):
    /// the Global Space leader still resolves before the pane.
    #[test]
    fn space_leader_switches_view_from_calendar() {
        let mut app = app_in_calendar();
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(app.pending_prefix, Some(' '));
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert_eq!(app.view, View::Contacts);
    }

    /// Switching to Mail restores the parked focus (the `MailView` proxy),
    /// mirroring the `AccountState` save/load pattern.
    #[test]
    fn switching_back_to_mail_restores_parked_focus() {
        let mut app = app_with_mailboxes();
        app.focus = Focus::Preview;
        app.switch_view(View::Contacts);
        // Focus is irrelevant while in a non-Mail view; on return it is
        // restored from the parked mail-view snapshot.
        app.switch_view(View::Mail);
        assert_eq!(app.focus, Focus::Preview);
    }

    /// Mail-specific keys must not fire while a non-Mail view is active: the
    /// Global mail surface (mailbox jump) is gated off, and the Mail pane
    /// contexts are not consulted at all (`a` = archive is not rebound by
    /// either content view).
    #[test]
    fn mail_keys_do_not_fire_in_contacts_or_calendar() {
        for view in [View::Contacts, View::Calendar] {
            let mut app = app_with_mailboxes();
            app.switch_view(view);
            let before = app.active_mailbox;

            // Digit jump (a Global mail action) is swallowed.
            app.handle_key(KeyEvent::from(KeyCode::Char('2')));
            assert_eq!(app.active_mailbox, before, "digit jump must not fire in {view:?}");

            // A List-context key (archive) does nothing: the Mail pane contexts
            // are not consulted outside Mail, and it is not rebound there.
            app.handle_key(KeyEvent::from(KeyCode::Char('a')));
            assert!(matches!(app.overlay, Overlay::None));
            assert!(app.pending_actions.is_empty());
            assert_eq!(app.view, view, "a bare mail key must not change the view");
        }
    }

    /// View-agnostic Global keys (help, quit) still work in the non-Mail views.
    #[test]
    fn view_agnostic_keys_work_in_non_mail_views() {
        for view in [View::Contacts, View::Calendar] {
            let mut app = app_with_mailboxes();
            app.switch_view(view);

            // Help overlay toggles.
            app.handle_key(KeyEvent::from(KeyCode::Char('?')));
            assert!(matches!(app.overlay, Overlay::Help), "? in {view:?}");
            app.handle_key(KeyEvent::from(KeyCode::Char('?')));
            assert!(matches!(app.overlay, Overlay::None));

            // Quit is honoured.
            let msg = app.handle_key(KeyEvent::from(KeyCode::Char('q')));
            assert!(matches!(msg, Some(Message::Quit)), "q in {view:?}");
        }
    }

    /// Digits 1-9 still jump mailboxes in the Mail view (no leader collision).
    #[test]
    fn digits_still_jump_mailboxes_in_mail_view() {
        let mut app = app_with_mailboxes();
        assert_eq!(app.view, View::Mail);
        assert_eq!(app.active_mailbox, 0);

        // `2` -> mailbox index 1 (Drafts).
        app.handle_key(KeyEvent::from(KeyCode::Char('2')));
        assert_eq!(app.active_mailbox, 1);
        assert_eq!(app.focus, Focus::List);
    }

    // -- Contacts view (#0033) -------------------------------------------

    fn contact(addr: &str, name: &str, sent_to: u32) -> crate::contacts::Contact {
        crate::contacts::Contact {
            address: addr.into(),
            display_name: name.into(),
            sent_to,
            sent_cc: 0,
            received: 0,
            first_seen: "2026-01-01T00:00:00Z".into(),
            last_seen: "2026-04-08T00:00:00Z".into(),
            source: crate::contacts::ContactSource::Local,
        }
    }

    /// Build an app already in the Contacts view with a seeded index.
    fn app_in_contacts() -> App {
        let mut app = app_with_mailboxes();
        let mut contacts = std::collections::HashMap::new();
        for c in [
            contact("alice@foo.com", "Alice Smith", 3),
            contact("bob@bar.com", "Bob Jones", 2),
            contact("carol@baz.com", "Carol", 1),
        ] {
            contacts.insert(c.address.clone(), c);
        }
        app.contacts_view.index = Some(crate::contacts::ContactIndex {
            account: "test".into(),
            contacts,
            built_at: "2026-04-08T00:00:00Z".into(),
        });
        app.contacts_view.loaded = true;
        app.view = View::Contacts;
        app.recompute_contact_matches();
        app
    }

    /// The fuzzy-search input filters the contact list at the App level, and
    /// `Esc` clears the query and restores the full ranked list.
    #[test]
    fn contacts_fuzzy_search_filters_list() {
        let mut app = app_in_contacts();
        // Empty query: all three, rank-ordered (sent_to desc): alice, bob, carol.
        assert_eq!(app.contacts_view.matches.len(), 3);
        assert_eq!(app.contacts_view.matches[0], "alice@foo.com");

        // `/` arms the search input; typing narrows the list.
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        assert!(app.contacts_view.searching);
        for c in "bob".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(app.contacts_view.matches, vec!["bob@bar.com".to_string()]);

        // `Esc` clears the query and restores the full list.
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.contacts_view.searching);
        assert!(app.contacts_view.query.is_empty());
        assert_eq!(app.contacts_view.matches.len(), 3);
    }

    /// Contacts navigation keys move the list cursor and drive the selection.
    #[test]
    fn contacts_navigation_moves_cursor() {
        let mut app = app_in_contacts();
        assert_eq!(app.contacts_view.list_index, 0);
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.contacts_view.list_index, 1);
        assert_eq!(app.selected_contact().unwrap().address, "bob@bar.com");
        app.handle_key(KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(app.contacts_view.list_index, 0);
    }

    /// `Enter`/`n` in Contacts opens the compose wizard seeded with the
    /// selected contact's address.
    #[test]
    fn compose_to_contact_seeds_wizard() {
        let mut app = app_in_contacts();
        app.handle_key(KeyEvent::from(KeyCode::Char('j'))); // select bob
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        match app.pending_actions.pop_front() {
            Some(Action::ComposeToContact { to }) => {
                assert_eq!(to, "Bob Jones <bob@bar.com>");
            }
            other => panic!("expected ComposeToContact, got {other:?}"),
        }
    }

    /// `v` in Contacts queues a vCard export for the selected contact.
    #[test]
    fn vcard_key_queues_export_for_selection() {
        let mut app = app_in_contacts();
        app.handle_key(KeyEvent::from(KeyCode::Char('v')));
        match app.pending_actions.pop_front() {
            Some(Action::SendContactVcard { contact }) => {
                assert_eq!(contact.address, "alice@foo.com");
            }
            other => panic!("expected SendContactVcard, got {other:?}"),
        }
    }

    /// `c` in Contacts queues the clipboard copy for the selected contact.
    /// Dispatch-level only: the actual `arboard` call lives in `actions.rs` and
    /// would touch the real system clipboard (unavailable headless), so tests
    /// stop at the queued `Action`, like the vCard test above.
    #[test]
    fn copy_email_key_queues_clipboard_copy_for_selection() {
        let mut app = app_in_contacts();
        app.handle_key(KeyEvent::from(KeyCode::Char('j'))); // select bob
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        match app.pending_actions.pop_front() {
            Some(Action::CopyContactEmail { address }) => {
                assert_eq!(address, "bob@bar.com");
            }
            other => panic!("expected CopyContactEmail, got {other:?}"),
        }
    }

    /// With no contact selected (empty index) `c` is a no-op with a status
    /// hint, not a queued copy.
    #[test]
    fn copy_email_key_is_noop_without_selection() {
        let mut app = app_with_mailboxes();
        app.view = View::Contacts;
        app.contacts_view.loaded = true;
        app.recompute_contact_matches();
        assert!(app.selected_contact().is_none());
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert!(
            app.pending_actions
                .iter()
                .all(|a| !matches!(a, Action::CopyContactEmail { .. })),
            "c must not queue a copy without a selection"
        );
        assert_eq!(app.status_message.as_deref(), Some("No contact selected"));
    }

    /// Contacts keys must not fire in the Mail view: pressing `v` (vCard) in
    /// Mail does not queue a contact action (it is not a Mail binding).
    #[test]
    fn contacts_keys_do_not_fire_in_mail() {
        let mut app = app_with_mailboxes();
        assert_eq!(app.view, View::Mail);
        app.handle_key(KeyEvent::from(KeyCode::Char('v')));
        assert!(
            !app
                .pending_actions
                .iter()
                .any(|a| matches!(a, Action::SendContactVcard { .. })),
            "v must not queue a vCard export in Mail"
        );
        // And a bare `n` in Mail is the New-draft path, not compose-to-contact.
        app.pending_actions.clear();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(
            app
                .pending_actions
                .iter()
                .all(|a| !matches!(a, Action::ComposeToContact { .. })),
            "n in Mail must be NewDraft, not ComposeToContact"
        );
        // `c` in Mail is edit-recipients (Drafts-only), never a contact copy.
        app.pending_actions.clear();
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert!(
            app.pending_actions
                .iter()
                .all(|a| !matches!(a, Action::CopyContactEmail { .. })),
            "c in Mail must not queue a contact copy"
        );
    }

    /// `c` must not queue a contact copy from the Calendar view either (it has
    /// no `c` binding, and the Global `c` is only a `Space` continuation).
    #[test]
    fn copy_email_key_does_not_fire_in_calendar() {
        let mut app = app_in_calendar();
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert!(
            app.pending_actions
                .iter()
                .all(|a| !matches!(a, Action::CopyContactEmail { .. })),
            "c must not queue a contact copy in Calendar"
        );
    }

    // -- Calendar view (#0034) -------------------------------------------

    fn cal_event(
        summary: &str,
        start: &str,
        is_organizer: bool,
        method: &str,
    ) -> super::super::CalendarEvent {
        super::super::CalendarEvent {
            msg: ref_for(summary),
            event: crate::types::EventFrontmatter {
                uid: Some(format!("uid-{summary}")),
                method: Some(method.to_string()),
                sequence: 0,
                summary: Some(summary.to_string()),
                start: Some(start.to_string()),
                end: None,
                location: None,
                organizer: Some("org@example.com".into()),
                rsvp: "needs-action".into(),
                recurrence: String::new(),
                attendees: Vec::new(),
                ..Default::default()
            },
            subject: format!("Invitation: {summary}"),
            start_sort: start.to_string(),
            end_sort: String::new(),
            start_display: start.to_string(),
            is_organizer,
            cancelled: false,
        }
    }

    /// Build an app already in the Calendar view with a seeded agenda. Events
    /// are far-future so the default upcoming-only scope keeps them all.
    fn app_in_calendar() -> App {
        let mut app = app_with_mailboxes();
        app.calendar_view.events = vec![
            cal_event("Standup", "2099-08-01T09:00:00", false, "REQUEST"),
            cal_event("Retro", "2099-08-02T09:00:00", true, "REQUEST"),
            cal_event("Review", "2099-08-03T09:00:00", false, "REQUEST"),
        ];
        app.calendar_view.loaded = true;
        app.view = View::Calendar;
        app.recompute_calendar_visible();
        app
    }

    /// Calendar navigation keys move the cursor and drive the selection.
    #[test]
    fn calendar_navigation_moves_cursor() {
        let mut app = app_in_calendar();
        assert_eq!(app.calendar_view.visible.len(), 3);
        assert_eq!(app.calendar_view.list_index, 0);
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.calendar_view.list_index, 1);
        assert_eq!(
            app.selected_event().unwrap().event.summary.as_deref(),
            Some("Retro")
        );
        app.handle_key(KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(app.calendar_view.list_index, 0);
    }

    /// `gg` / `G` jump to the ends of the agenda.
    #[test]
    fn calendar_top_bottom_jumps() {
        let mut app = app_in_calendar();
        app.handle_key(KeyEvent::from(KeyCode::Char('G')));
        assert_eq!(app.calendar_view.list_index, 2);
        app.handle_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.pending_prefix, Some('g'));
        app.handle_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.calendar_view.list_index, 0);
        assert_eq!(app.pending_prefix, None);
    }

    /// `V` on a received agenda invite opens the RSVP overlay against that
    /// row's own message, not the mail cursor's: the agenda carries a
    /// `MessageRef` since the calendar moved onto the store (#0038 item 6).
    #[test]
    fn calendar_rsvp_opens_against_the_agenda_rows_message() {
        let mut app = app_in_calendar();
        app.handle_key(KeyEvent::from(KeyCode::Char('V')));
        let Overlay::Rsvp(overlay) = &app.overlay else {
            panic!("V on a received agenda invite must open the RSVP overlay");
        };
        assert_eq!(overlay.msg, ref_for("Standup"), "the agenda row's own message");
        assert_eq!(overlay.summary, "Standup");
        assert_eq!(overlay.selected, 0);
    }

    /// `V` on an invite we sent refuses with a hint (we are the organizer).
    #[test]
    fn calendar_rsvp_refused_for_organizer_event() {
        let mut app = app_in_calendar();
        app.handle_key(KeyEvent::from(KeyCode::Char('j'))); // Retro (organizer)
        app.handle_key(KeyEvent::from(KeyCode::Char('V')));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("organizer")));
    }

    /// #0031: the mail-view RSVP refuses a cancelled or superseded version of
    /// an event, so no reply goes out carrying a `SEQUENCE` the organizer has
    /// already moved past. A current REQUEST is still accepted.
    #[test]
    fn rsvp_refusal_covers_cancelled_and_superseded_versions() {
        let request = |f: fn(&mut crate::types::EventFrontmatter)| {
            let mut ev = crate::types::EventFrontmatter {
                method: Some("REQUEST".into()),
                ..Default::default()
            };
            f(&mut ev);
            ev
        };
        assert!(super::rsvp_refusal(Some(&request(|_| {}))).is_none());
        assert!(super::rsvp_refusal(None)
            .is_some_and(|m| m.contains("Only received invitations")));
        assert!(super::rsvp_refusal(Some(&crate::types::EventFrontmatter {
            method: Some("CANCEL".into()),
            ..Default::default()
        }))
        .is_some_and(|m| m.contains("Only received invitations")));
        assert!(
            super::rsvp_refusal(Some(&request(|e| e.cancelled = true)))
                .is_some_and(|m| m.contains("cancelled")),
        );
        assert!(
            super::rsvp_refusal(Some(&request(|e| e.superseded = true)))
                .is_some_and(|m| m.contains("newer version")),
        );
    }

    /// `V` on a cancelled row refuses: the organizer already called the
    /// meeting off, so an RSVP would be mailed about a dead event.
    #[test]
    fn calendar_rsvp_refused_for_cancelled_event() {
        let mut app = app_in_calendar();
        app.calendar_view.events[0].cancelled = true;
        app.handle_key(KeyEvent::from(KeyCode::Char('V')));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("cancelled")));
    }

    /// `V` on a non-REQUEST row refuses: only a received invitation can be
    /// RSVP'd, the same guard the mail path applies.
    #[test]
    fn calendar_rsvp_refused_for_non_request_event() {
        let mut app = app_in_calendar();
        app.calendar_view.events[0].event.method = Some("PUBLISH".to_string());
        app.handle_key(KeyEvent::from(KeyCode::Char('V')));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("REQUEST")));
    }

    /// With the RSVP overlay open, `t` selects Tentative rather than toggling
    /// the Calendar scope: overlays intercept before normal-mode dispatch.
    #[test]
    fn calendar_rsvp_overlay_t_selects_tentative_not_scope_toggle() {
        let mut app = app_in_calendar();
        // Opened directly rather than with `V`: what this test is about is
        // key precedence with the overlay already up, not how it got there.
        app.overlay = Overlay::Rsvp(RsvpOverlay {
            msg: MessageRef::new(1),
            summary: "Standup".to_string(),
            selected: 0,
        });
        let scope_before = app.calendar_view.show_past;
        app.handle_key(KeyEvent::from(KeyCode::Char('t')));
        let Overlay::Rsvp(overlay) = &app.overlay else {
            panic!("t must not close the RSVP overlay");
        };
        assert_eq!(overlay.selected, 1, "t selects Tentative");
        assert_eq!(app.calendar_view.show_past, scope_before);
    }

    /// An in-progress meeting (started, not yet ended) stays in the upcoming
    /// scope until its `end_sort` passes, rather than vanishing at its start.
    #[test]
    fn in_progress_meeting_stays_upcoming_until_it_ends() {
        let mut app = app_with_mailboxes();
        let now = chrono::Utc::now();
        let fmt = |dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut running = cal_event(
            "Running",
            &fmt(now - chrono::Duration::minutes(30)),
            false,
            "REQUEST",
        );
        running.end_sort = fmt(now + chrono::Duration::minutes(30));
        let mut finished = cal_event(
            "Finished",
            &fmt(now - chrono::Duration::hours(3)),
            false,
            "REQUEST",
        );
        finished.end_sort = fmt(now - chrono::Duration::hours(2));
        app.calendar_view.events = vec![running, finished];
        app.calendar_view.loaded = true;
        app.recompute_calendar_visible();
        let visible: Vec<&str> = app
            .calendar_view
            .visible
            .iter()
            .map(|&i| app.calendar_view.events[i].subject.as_str())
            .collect();
        assert_eq!(visible, vec!["Invitation: Running"]);
    }

    /// `Enter` queues opening the source invite email, carrying the agenda
    /// row's own message reference (the invite may live in any mailbox).
    #[test]
    fn calendar_enter_opens_the_source_invite() {
        let mut app = app_in_calendar();
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        match app.pending_actions.pop_front() {
            Some(Action::OpenEventSource { msg }) => {
                assert_eq!(msg, ref_for("Standup"));
            }
            other => panic!("expected OpenEventSource, got {other:?}"),
        }
    }

    /// `t` toggles the upcoming-only scope; past events appear only when on.
    #[test]
    fn calendar_scope_toggle_reveals_past_events() {
        let mut app = app_in_calendar();
        app.calendar_view
            .events
            .push(cal_event("Old", "2000-01-01T09:00:00", false, "REQUEST"));
        app.recompute_calendar_visible();
        assert_eq!(app.calendar_view.visible.len(), 3, "past event hidden");

        app.handle_key(KeyEvent::from(KeyCode::Char('t')));
        assert!(app.calendar_view.show_past);
        assert_eq!(app.calendar_view.visible.len(), 4);

        app.handle_key(KeyEvent::from(KeyCode::Char('t')));
        assert_eq!(app.calendar_view.visible.len(), 3);
    }

    /// Calendar keys must not fire in Mail: `V` there is the mail-list RSVP
    /// path (guarded on the cursor email being an invite), never the calendar
    /// one, and `t` is not a Mail binding at all.
    #[test]
    fn calendar_keys_do_not_fire_in_mail() {
        let mut app = app_with_mailboxes();
        app.calendar_view.events =
            vec![cal_event("Standup", "2099-08-01T09:00:00", false, "REQUEST")];
        app.calendar_view.loaded = true;
        app.recompute_calendar_visible();
        assert_eq!(app.view, View::Mail);

        // The cursor email is not an invite, so `V` hints instead of opening.
        app.handle_key(KeyEvent::from(KeyCode::Char('V')));
        assert!(matches!(app.overlay, Overlay::None));

        // `t` (calendar scope toggle) does nothing in Mail.
        let before = app.calendar_view.show_past;
        app.handle_key(KeyEvent::from(KeyCode::Char('t')));
        assert_eq!(app.calendar_view.show_past, before);
        assert!(app.pending_actions.is_empty());
    }

    /// Mail-specific Global keys stay swallowed in Calendar even though it now
    /// owns a pane context: digits do not jump mailboxes, `/` does not open the
    /// metadata filter (Calendar rebinds neither).
    #[test]
    fn mail_global_keys_stay_swallowed_in_calendar() {
        let mut app = app_in_calendar();
        let before = app.active_mailbox;
        app.handle_key(KeyEvent::from(KeyCode::Char('2')));
        assert_eq!(app.active_mailbox, before, "digit jump must not fire");
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        assert_ne!(app.focus, Focus::Search, "/ must not arm the mail filter");
        assert_eq!(app.view, View::Calendar);
    }

    // -----------------------------------------------------------------------
    // Jump to date (#0017)
    // -----------------------------------------------------------------------

    /// A dated list, newest first, the way `store::read` orders one.
    fn dated_emails(days: &[&str]) -> Vec<EmailEntry> {
        days.iter()
            .map(|day| {
                let mut e = entry(day, "Alice");
                e.date_display = (*day).to_string();
                e.date_sort = format!("{day}T09:00:00");
                e
            })
            .collect()
    }

    fn app_on_dates() -> App {
        let mut app = App::default_for_tests();
        app.emails = std::sync::Arc::new(dated_emails(&[
            "2026-08-10", "2026-08-01", "2026-07-15", "2026-06-30", "2025-12-24",
        ]));
        app.email_cache = vec![Some(std::sync::Arc::clone(&app.emails))];
        app.mailbox_counts = vec![app.emails.len()];
        app.rebuild_visible();
        app
    }

    fn day(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// The cursor lands on the newest row that is not after the target, and
    /// the list itself does not move: this is navigation, not a filter.
    #[test]
    fn a_jump_lands_on_the_newest_row_on_or_before_the_date() {
        let mut app = app_on_dates();
        let before = app.visible.clone();

        app.jump_to_date(day("2026-07-20"));
        assert_eq!(app.list_index, 2, "2026-07-15 is the newest row on or before it");
        assert_eq!(app.visible, before, "nothing is filtered out");

        // An exact hit lands on that row, not past it.
        app.jump_to_date(day("2026-08-01"));
        assert_eq!(app.list_index, 1);

        // A date newer than everything lands on the newest row.
        app.jump_to_date(day("2026-12-31"));
        assert_eq!(app.list_index, 0);
    }

    /// A date older than the whole mailbox parks on the oldest row and says
    /// so, rather than silently pretending it found it.
    #[test]
    fn a_jump_past_the_oldest_row_says_where_it_stopped() {
        let mut app = app_on_dates();
        app.jump_to_date(day("2020-01-01"));
        assert_eq!(app.list_index, 4);
        let status = app.status_message.clone().unwrap();
        assert!(status.contains("Nothing on or before 2020-01-01"), "{status}");
    }

    /// `g t` arms the prompt, typing edits it, Enter jumps and disarms, and a
    /// date the grammar cannot read keeps the prompt up with the reason shown.
    #[test]
    fn the_prompt_is_armed_typed_and_committed_by_the_keyboard() {
        let mut app = app_on_dates();
        app.handle_key(KeyEvent::from(KeyCode::Char('g')));
        app.handle_key(KeyEvent::from(KeyCode::Char('t')));
        assert_eq!(app.jump_date_input.as_deref(), Some(""), "the prompt is armed");

        for c in "2026-07-20".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(app.jump_date_input.as_deref(), Some("2026-07-20"));
        app.handle_key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(app.jump_date_input.as_deref(), Some("2026-07-2"));
        app.handle_key(KeyEvent::from(KeyCode::Char('0')));

        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.jump_date_input.is_none(), "committing disarms the prompt");
        assert_eq!(app.list_index, 2);
    }

    /// While the prompt is armed it owns the keyboard: `d` types a character,
    /// it does not delete the message under the cursor.
    #[test]
    fn the_armed_prompt_swallows_the_keys_that_would_otherwise_act() {
        let mut app = app_on_dates();
        app.jump_date_input = Some(String::new());
        app.handle_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(app.jump_date_input.as_deref(), Some("d"));
        assert!(app.pending_actions.is_empty(), "{:?}", app.pending_actions);

        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.jump_date_input.is_none(), "Esc abandons the prompt");
        assert_eq!(app.list_index, 0, "an abandoned prompt moves nothing");
    }

    /// An unreadable date is a correction, not a re-arm: the prompt stays up
    /// and the status line names the forms that would have worked.
    #[test]
    fn an_unreadable_date_keeps_the_prompt_and_explains() {
        let mut app = app_on_dates();
        app.jump_date_input = Some("tomorrow".to_string());
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.jump_date_input.as_deref(), Some("tomorrow"));
        assert_eq!(app.list_index, 0, "nothing moved");
        let status = app.status_message.clone().unwrap();
        assert!(status.contains("YYYY-MM-DD"), "{status}");
    }

    // -----------------------------------------------------------------------
    // Pane zoom (#TKT-0044)
    // -----------------------------------------------------------------------

    #[test]
    fn z_zooms_the_focused_pane_and_z_again_restores_the_split() {
        let mut app = app_with_emails(sample());
        app.focus = Focus::Preview;
        assert_eq!(app.zoomed_pane(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('z')));
        assert_eq!(app.zoomed_pane(), Some(Focus::Preview));
        app.handle_key(KeyEvent::from(KeyCode::Char('z')));
        assert_eq!(app.zoomed_pane(), None);
    }

    #[test]
    fn the_zoom_follows_the_focus() {
        // herdr zooms *the active pane*, so cycling focus under a zoom moves
        // the zoom rather than leaving a zoomed pane the keyboard no longer
        // drives.
        let mut app = app_with_emails(sample());
        app.focus = Focus::List;
        app.handle_key(KeyEvent::from(KeyCode::Char('z')));
        assert_eq!(app.zoomed_pane(), Some(Focus::List));
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Preview);
        assert_eq!(app.zoomed_pane(), Some(Focus::Preview));
    }

    #[test]
    fn the_search_prompt_zooms_the_list_it_filters() {
        let mut app = app_with_emails(sample());
        app.focus = Focus::Search;
        app.zoomed = true;
        assert_eq!(app.zoomed_pane(), Some(Focus::List));
    }

    #[test]
    fn the_compose_wizard_has_nothing_to_zoom_and_says_so() {
        let mut app = app_with_emails(sample());
        app.focus = Focus::ComposeWizard;
        app.toggle_zoom();
        assert!(!app.zoomed, "the flag must not arm for a later pane");
        assert_eq!(
            app.status_message.as_deref(),
            Some("Nothing to zoom here.")
        );
    }

    #[test]
    fn zoom_is_a_mail_view_layout_and_is_ignored_elsewhere() {
        // `z` never fires off Mail (`is_view_agnostic` leaves `ToggleZoom`
        // out), and even a flag set before the switch changes nothing there.
        let mut app = app_with_emails(sample());
        app.zoomed = true;
        app.view = View::Contacts;
        assert_eq!(app.zoomed_pane(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('z')));
        assert!(app.zoomed, "the flag is untouched off Mail");
        // Back on Mail the zoom the user armed is still there.
        app.view = View::Mail;
        assert_eq!(app.zoomed_pane(), Some(Focus::List));
    }

    #[test]
    fn a_zoom_survives_an_account_switch() {
        // Session state, not per-account state: switching accounts must not
        // silently un-zoom the pane the user is reading.
        let mut app = app_with_emails(sample());
        app.focus = Focus::Preview;
        app.zoomed = true;
        app.save_to_account();
        app.load_from_account(0);
        assert_eq!(app.zoomed_pane(), Some(Focus::Preview));
    }
}
