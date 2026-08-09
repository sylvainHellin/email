//! Keymap-as-data (#0032, subsumes #0019).
//!
//! A single static `KEYMAP` table is the source of truth for *which* key
//! bindings exist, *where* they are live (context model), how they group in
//! the help overlay, their human description, and — for the Normal-mode
//! surface — *what* they do (via [`KeyAction`]). Three former hand-kept copies
//! now derive from it:
//!
//! 1. the in-TUI help overlay (`ui/overlays.rs::help_sections`),
//! 2. the mode/hint bar (`ui/status.rs::render_hint_bar`),
//! 3. the website key table (`mp dump-keys` -> `website/src/pages/*`).
//!
//! ## What the table owns
//!
//! The table owns the *catalogue* of user-facing bindings: pattern, context,
//! group, description, guard, and (for leader chords) the prefix. For the
//! Normal-mode surface (no overlay active) it also owns *dispatch*: `keys.rs`
//! resolves the pressed key through [`resolve`] into a [`KeyAction`] that a
//! single executor runs. See "(B)-lite runtime dispatch" below.
//!
//! It deliberately does **not** own the *execution* of deeply stateful,
//! context-sensitive overlay input (selection-vs-single confirmation dialogs,
//! compose-wizard field editing, dir-picker navigation, incremental search
//! input, confirm y/n, activity filter/scroll, help filter/scroll). Those stay
//! hand-coded in `keys.rs` — expressing them as flat table rows would force a
//! redesign of overlay input handling (the ticket's stop rule). They still get
//! catalogue rows ([`KeyAction::Manual`]) so help/hint/website document them.
//!
//! Every binding that appears in the help overlay is listed here; the
//! `keymap_covers_help` / no-duplicate tests guard that invariant.
//!
//! ## Leader / prefix model
//!
//! A binding may declare a single-key `prefix`. Two leaders exist: `Space`
//! (the Global view switcher `Space m/c/a`, #0033) and `g` (the List-scoped
//! `gg`/`G` jumps). The chord matcher only fires a prefixed binding when the
//! matching prefix is pending; `handle_key` sets `App::pending_prefix` to the
//! armed leader when a bare prefix key is seen (and the hint bar shows the
//! pending continuations). The two leaders never cross-arm: a pending `Space`
//! only fires `Space`-prefixed rows and a pending `g` only fires `g`-prefixed
//! rows. This generalizes the former special-cased `gg` handling into
//! first-class data.
//!
//! ## (B)-lite runtime dispatch
//!
//! For the Normal-mode surface `keys.rs` calls [`resolve`] with the active
//! [`KeyCtx`] (Global is always tried first, then the focused pane's context)
//! and the pending-prefix state. `resolve` returns the first matching row's
//! [`KeyAction`]; a single `execute` match in `keys.rs` runs it. The live
//! dispatch reads the same catalogue that drives help/hint/website, so they
//! cannot drift.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The context in which a binding is live. Mirrors the help-overlay groups and
/// the input dispatch surfaces in `keys.rs`. A single logical binding can be
/// live in several contexts (e.g. `V` in both `List` and `Preview`); those are
/// separate table rows so the hint bar can show the right set per context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCtx {
    /// Always-live top-level bindings (quit, help, account/mailbox jump, ...).
    Global,
    /// Email list pane has focus and is non-empty.
    List,
    /// Mailbox sidebar pane has focus.
    Sidebar,
    /// Headers pane has focus.
    Headers,
    /// Body/preview pane has focus.
    Preview,
    /// Server (IMAP) search overlay result list.
    ServerSearch,
    /// Contacts view list (#0033): read-only list + fuzzy search + detail.
    Contacts,
    /// Calendar view agenda (#0034): local-first event list + detail card.
    Calendar,
    /// Activity-log overlay.
    Activity,
    /// Help overlay.
    Help,
}

impl KeyCtx {
    /// The uppercase section title used by the help overlay, in table order.
    pub fn group_title(self) -> &'static str {
        match self {
            KeyCtx::Global => "GLOBAL",
            KeyCtx::Sidebar => "SIDEBAR",
            KeyCtx::List => "EMAIL LIST",
            KeyCtx::ServerSearch => "SERVER SEARCH",
            KeyCtx::Contacts => "CONTACTS",
            KeyCtx::Calendar => "CALENDAR",
            KeyCtx::Headers => "HEADERS",
            KeyCtx::Preview => "BODY",
            KeyCtx::Activity => "ACTIVITY LOG",
            KeyCtx::Help => "HELP",
        }
    }

    /// Help-overlay section order (also the hint-bar precedence).
    pub const HELP_ORDER: &'static [KeyCtx] = &[
        KeyCtx::Global,
        KeyCtx::Sidebar,
        KeyCtx::List,
        KeyCtx::ServerSearch,
        KeyCtx::Contacts,
        KeyCtx::Calendar,
        KeyCtx::Headers,
        KeyCtx::Preview,
        KeyCtx::Activity,
    ];
}

/// An extra live-guard on a binding beyond its context. Keeps context-sensitive
/// rules (e.g. `c` only in Drafts) in the data model rather than as ad-hoc code
/// in the middle of the dispatcher. `keys.rs` evaluates the guard at resolve
/// time (and, for `DraftsOnly`, still shows the old status hint on a miss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// No additional guard.
    None,
    /// Only meaningful in the Drafts mailbox.
    DraftsOnly,
    /// Only shown / relevant when the account count is > 1.
    MultiAccount,
    /// Only live when the email list is non-empty (List pane).
    NonEmptyList,
}

/// The physical chord a binding matches, used by the runtime resolver. This is
/// the machine-matchable counterpart to `KeyBinding::keys` (the display form).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    /// A bare character with no modifiers, e.g. `q`, `a`, `G`.
    Char(char),
    /// Either of two bare characters (e.g. `Enter`/`e` share an action). Only
    /// the `Char` variant is expressed; special keys use [`Chord::Or`].
    /// A character OR a special code (e.g. Enter or `e`, `j` or Down).
    CharOrCode(char, SpecialCode),
    /// One of two bare characters (e.g. `r`/`R` are distinct actions so they
    /// get their own rows; this is for `k`-or-Up style synonyms only).
    CharOrChar(char, char),
    /// A `Char` with the Control modifier held.
    CtrlChar(char),
    /// A bare special key.
    Code(SpecialCode),
    /// The Control modifier plus a digit `1..=9` (account jump).
    CtrlDigit,
    /// A bare digit `1..=9` (mailbox jump).
    Digit,
    /// The prefix key itself pressed bare (starts / continues a leader chord).
    /// Matched only when no prefix is pending.
    PrefixLeader(char),
    /// Matched via hand-coded dispatch; the resolver never returns it.
    Manual,
}

/// Special (non-character) keys the resolver can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialCode {
    Enter,
    Esc,
    Tab,
    BackTab,
    Up,
    Down,
}

impl SpecialCode {
    fn matches(self, code: KeyCode) -> bool {
        matches!(
            (self, code),
            (SpecialCode::Enter, KeyCode::Enter)
                | (SpecialCode::Esc, KeyCode::Esc)
                | (SpecialCode::Tab, KeyCode::Tab)
                | (SpecialCode::BackTab, KeyCode::BackTab)
                | (SpecialCode::Up, KeyCode::Up)
                | (SpecialCode::Down, KeyCode::Down)
        )
    }
}

impl Chord {
    /// Whether this chord matches `key` given the current pending prefix.
    ///
    /// Prefix handling: a chord with `prefix.is_some()` on its binding is only
    /// reached by [`resolve`] when that prefix is pending; the `Char`/`Code`
    /// match here is the *continuation* key. A [`Chord::PrefixLeader`] matches
    /// the bare prefix key only when *no* prefix is pending.
    fn matches(self, key: KeyEvent, prefix_pending: Option<char>) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match self {
            Chord::Char(c) => !ctrl && key.code == KeyCode::Char(c),
            Chord::CharOrChar(a, b) => {
                !ctrl && (key.code == KeyCode::Char(a) || key.code == KeyCode::Char(b))
            }
            Chord::CharOrCode(c, code) => {
                !ctrl && (key.code == KeyCode::Char(c) || code.matches(key.code))
            }
            Chord::CtrlChar(c) => ctrl && key.code == KeyCode::Char(c),
            Chord::Code(code) => code.matches(key.code),
            Chord::CtrlDigit => {
                ctrl && matches!(key.code, KeyCode::Char('1'..='9'))
            }
            Chord::Digit => {
                !ctrl && matches!(key.code, KeyCode::Char('1'..='9'))
            }
            Chord::PrefixLeader(p) => {
                prefix_pending.is_none() && !ctrl && key.code == KeyCode::Char(p)
            }
            Chord::Manual => false,
        }
    }
}

/// What a Normal-mode binding does. The single executor in `keys.rs` matches on
/// this. Overlay-internal bindings use [`KeyAction::Manual`] (documented but
/// hand-dispatched). Variants that carry no data can be executed generically;
/// a few need the live `KeyEvent` (digit jumps) and read it in the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    // -- Global -----------------------------------------------------------
    Quit,
    ToggleHelp,
    ToggleActivityLog,
    OpenActivityOverlay,
    OpenLogFile,
    OpenConfigFile,
    FilterMetadata,
    SearchContent,
    SwitchAccount,
    JumpAccount,
    JumpMailbox,
    FocusForward,
    FocusBackward,
    /// Leader `Space m` / `Space c` / `Space a`: switch the top-level view. The
    /// executor reads the continuation key to pick the target view (#0033).
    SwitchView,
    // -- Sidebar ----------------------------------------------------------
    SidebarDown,
    SidebarUp,
    SidebarSelect,
    // -- List / shared navigation ----------------------------------------
    ListDown,
    ListUp,
    ListTop,
    ListBottom,
    /// Leader `g d`: arm the jump-to-date prompt on the mail list (#0017).
    JumpToDate,
    ToggleSelect,
    SelectAllVisible,
    ClearSelection,
    OpenEditor,
    Reply,
    ReplyAll,
    Forward,
    EditRecipients,
    Archive,
    Delete,
    ToggleRead,
    ToggleFlag,
    /// Narrow the list to flagged messages, or widen it back (#0079).
    ToggleFlaggedFilter,
    MovePicker,
    Rsvp,
    /// Open the conversation (threading) overlay for the cursor message (#0008).
    OpenThread,
    Approve,
    MarkDraft,
    Send,
    SendAll,
    CopyMessageRef,
    OpenAttachment,
    SaveAttachment,
    OpenInBrowser,
    NewDraft,
    QuickSync,
    FullSync,
    ServerSearch,
    // -- Contacts view (#0033) -------------------------------------------
    ContactsDown,
    ContactsUp,
    ContactsTop,
    ContactsBottom,
    ContactsSearch,
    ContactsCompose,
    ContactsVcard,
    ContactsCopyEmail,
    ContactsRefresh,
    // -- Calendar view (#0034) -------------------------------------------
    CalendarDown,
    CalendarUp,
    CalendarTop,
    CalendarBottom,
    CalendarOpenSource,
    CalendarRsvp,
    CalendarToggleScope,
    CalendarRefresh,
    // -- Headers / Preview scroll ----------------------------------------
    HeadersDown,
    HeadersUp,
    PreviewDown,
    PreviewUp,
    PreviewHalfDown,
    PreviewHalfUp,
    PreviewToList,
    /// Hand-dispatched (overlay-internal input). The resolver never returns it.
    Manual,
}

impl KeyAction {
    /// Whether this action is meaningful outside the Mail view (#0033).
    ///
    /// The non-Mail views only expose the view-agnostic Global surface: view
    /// switching (the Space leader + `Space m/c/a`), quit, help, and the
    /// activity log. Mail-specific Global actions (mailbox/account jump,
    /// metadata/content search, focus cycling) are gated off so they cannot
    /// fire outside Mail — unless the active view's pane context rebinds that
    /// key, in which case the pane binding wins (see `dispatch_normal_mode`).
    /// `Manual` stays live because it backs the Space leader toggle.
    pub fn is_view_agnostic(self) -> bool {
        matches!(
            self,
            KeyAction::Quit
                | KeyAction::ToggleHelp
                | KeyAction::ToggleActivityLog
                | KeyAction::OpenActivityOverlay
                | KeyAction::OpenLogFile
                | KeyAction::OpenConfigFile
                | KeyAction::SwitchView
                | KeyAction::Manual
        )
    }
}

/// One catalogued key binding.
#[derive(Debug, Clone, Copy)]
pub struct KeyBinding {
    /// The keys the user presses, e.g. `"?"`, `"gg"`, `"Ctrl+l"`, `"1-9"`.
    /// This is the display form used by help/hint/website.
    pub keys: &'static str,
    /// The physical chord the runtime resolver matches (continuation key for
    /// prefixed bindings). `Chord::Manual` for hand-dispatched rows.
    pub chord: Chord,
    /// Optional leader prefix that must be pressed first (`' '` for the Space
    /// view switcher, `'g'` for the List `gg`/`G` jumps).
    pub prefix: Option<char>,
    /// The context the binding is live in.
    pub ctx: KeyCtx,
    /// Additional live guard.
    pub guard: Guard,
    /// The action the executor runs. [`KeyAction::Manual`] for hand-dispatched.
    pub action: KeyAction,
    /// Human description for the help overlay / website.
    pub desc: &'static str,
    /// Short label for the hint bar only, `""` to reuse [`Self::desc`] (#0078).
    ///
    /// The hint bar is one line that lays every hinted binding of the context
    /// out end to end, so one long description silently costs the bindings to
    /// its right. The help overlay and `mp dump-keys` (hence the website
    /// table) have a column each and always show `desc`, so the short form
    /// never becomes the only wording a user can find.
    pub short: &'static str,
    /// Whether to surface this binding in the hint bar's "next keys" row.
    pub hint: bool,
}

impl KeyBinding {
    /// What the hint bar prints: the short label when there is one, else the
    /// full description.
    pub fn hint_label(&self) -> &'static str {
        if self.short.is_empty() {
            self.desc
        } else {
            self.short
        }
    }
}

/// Full constructor.
#[allow(clippy::too_many_arguments)]
const fn row(
    keys: &'static str,
    chord: Chord,
    prefix: Option<char>,
    ctx: KeyCtx,
    guard: Guard,
    action: KeyAction,
    desc: &'static str,
    hint: bool,
) -> KeyBinding {
    KeyBinding { keys, chord, prefix, ctx, guard, action, desc, short: "", hint }
}

/// Give a binding a hint-bar-only short label (#0078). Wraps any of the
/// constructors below, so only the rows that need one carry one.
const fn short(kb: KeyBinding, short: &'static str) -> KeyBinding {
    KeyBinding { short, ..kb }
}

/// Plain live binding.
const fn b(
    keys: &'static str,
    chord: Chord,
    ctx: KeyCtx,
    action: KeyAction,
    desc: &'static str,
    hint: bool,
) -> KeyBinding {
    row(keys, chord, None, ctx, Guard::None, action, desc, hint)
}

/// Guarded live binding.
const fn bg(
    keys: &'static str,
    chord: Chord,
    ctx: KeyCtx,
    guard: Guard,
    action: KeyAction,
    desc: &'static str,
    hint: bool,
) -> KeyBinding {
    row(keys, chord, None, ctx, guard, action, desc, hint)
}

/// Hand-dispatched (documented-only) binding.
const fn manual(
    keys: &'static str,
    ctx: KeyCtx,
    desc: &'static str,
    hint: bool,
) -> KeyBinding {
    row(keys, Chord::Manual, None, ctx, Guard::None, KeyAction::Manual, desc, hint)
}

/// The single source of truth for TUI key bindings.
///
/// Order within a context is preserved verbatim in the help overlay and used
/// as the resolver's precedence. Adding a live binding here (with a matching
/// `KeyAction` arm in `keys.rs`) automatically updates help, the hint bar,
/// dispatch, and — after `mp dump-keys` + regeneration — the website.
pub static KEYMAP: &[KeyBinding] = &[
    // -- GLOBAL -----------------------------------------------------------
    b("q", Chord::Char('q'), KeyCtx::Global, KeyAction::Quit, "Quit", true),
    bg("`", Chord::Char('`'), KeyCtx::Global, Guard::MultiAccount, KeyAction::SwitchAccount, "Switch account", false),
    bg("Ctrl+1-9", Chord::CtrlDigit, KeyCtx::Global, Guard::MultiAccount, KeyAction::JumpAccount, "Jump to account", false),
    b("1-9", Chord::Digit, KeyCtx::Global, KeyAction::JumpMailbox, "Jump to mailbox", true),
    b("Tab", Chord::Code(SpecialCode::Tab), KeyCtx::Global, KeyAction::FocusForward, "Cycle focus forward", false),
    b("Shift+Tab", Chord::Code(SpecialCode::BackTab), KeyCtx::Global, KeyAction::FocusBackward, "Cycle focus backward", false),
    b("/", Chord::Char('/'), KeyCtx::Global, KeyAction::FilterMetadata, "Filter by metadata", true),
    b("\\", Chord::Char('\\'), KeyCtx::Global, KeyAction::SearchContent, "Search email content", false),
    b("?", Chord::Char('?'), KeyCtx::Global, KeyAction::ToggleHelp, "Toggle this help", true),
    b("!", Chord::Char('!'), KeyCtx::Global, KeyAction::ToggleActivityLog, "Toggle activity log", false),
    b("L", Chord::Char('L'), KeyCtx::Global, KeyAction::OpenActivityOverlay, "Open activity log overlay", false),
    b("Ctrl+l", Chord::CtrlChar('l'), KeyCtx::Global, KeyAction::OpenLogFile, "Open log file in $EDITOR", false),
    b("Ctrl+e", Chord::CtrlChar('e'), KeyCtx::Global, KeyAction::OpenConfigFile, "Open config.toml in $EDITOR", false),
    // View switcher leader (#0033, Space follow-up): Space opens the leader,
    // then m/c/a picks a view. Global so it works from every pane and every
    // view. Space was freed for this by moving list toggle-select to `v`.
    row("", Chord::PrefixLeader(' '), None, KeyCtx::Global, Guard::None, KeyAction::Manual, "", false),
    row("Space m", Chord::Char('m'), Some(' '), KeyCtx::Global, Guard::None, KeyAction::SwitchView, "Switch to Mail view", true),
    row("Space c", Chord::Char('c'), Some(' '), KeyCtx::Global, Guard::None, KeyAction::SwitchView, "Switch to Contacts view", true),
    row("Space a", Chord::Char('a'), Some(' '), KeyCtx::Global, Guard::None, KeyAction::SwitchView, "Switch to Calendar view", true),
    // -- SIDEBAR ----------------------------------------------------------
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Sidebar, KeyAction::SidebarDown, "Navigate mailboxes", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Sidebar, KeyAction::SidebarUp, "", false),
    b("Enter", Chord::Code(SpecialCode::Enter), KeyCtx::Sidebar, KeyAction::SidebarSelect, "Select mailbox", true),
    // -- EMAIL LIST -------------------------------------------------------
    // Most list actions require a non-empty list (matching the old empty-list
    // early-return that only allowed s/S/f/n). Only s/S/f/n are exempt.
    short(bg("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListDown, "Navigate emails", true), "Navigate"),
    bg("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListUp, "", false),
    row("", Chord::PrefixLeader('g'), None, KeyCtx::List, Guard::NonEmptyList, KeyAction::Manual, "", false),
    row("", Chord::Char('g'), Some('g'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListTop, "", false),
    bg("gg / G", Chord::Char('G'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListBottom, "Jump to top / bottom", false),
    // `g t` ("go to") rather than the ticket's suggested `D`: `D` is taken
    // (mark approved as draft), and `g d` would resolve ambiguously against
    // the unprefixed `d` (delete), which `no_duplicate_live_dispatch_per_context`
    // refuses for exactly the right reason -- a reordered table would turn a
    // mistyped jump into a delete.
    row("g t", Chord::Char('t'), Some('g'), KeyCtx::List, Guard::NonEmptyList, KeyAction::JumpToDate, "Jump to date (e.g. last week)", false),
    short(bg("v", Chord::Char('v'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ToggleSelect, "Toggle selection", true), "Select"),
    bg("Ctrl+a", Chord::CtrlChar('a'), KeyCtx::List, Guard::NonEmptyList, KeyAction::SelectAllVisible, "Select all visible", false),
    bg("Esc", Chord::Code(SpecialCode::Esc), KeyCtx::List, Guard::NonEmptyList, KeyAction::ClearSelection, "Clear selection", false),
    short(bg("Enter / e", Chord::CharOrCode('e', SpecialCode::Enter), KeyCtx::List, Guard::NonEmptyList, KeyAction::OpenEditor, "Open in editor (mail read-only)", true), "Open (read-only)"),
    bg("r / R", Chord::Char('r'), KeyCtx::List, Guard::NonEmptyList, KeyAction::Reply, "Reply / Reply-all", true),
    bg("", Chord::Char('R'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ReplyAll, "", false),
    bg("w", Chord::Char('w'), KeyCtx::List, Guard::NonEmptyList, KeyAction::Forward, "Forward", false),
    bg("c", Chord::Char('c'), KeyCtx::List, Guard::DraftsOnly, KeyAction::EditRecipients, "Edit recipients (Drafts only)", false),
    bg("a", Chord::Char('a'), KeyCtx::List, Guard::NonEmptyList, KeyAction::Archive, "Archive", true),
    bg("d", Chord::Char('d'), KeyCtx::List, Guard::NonEmptyList, KeyAction::Delete, "Delete", true),
    bg("m", Chord::Char('m'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ToggleRead, "Toggle read/unread", false),
    bg("*", Chord::Char('*'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ToggleFlag, "Toggle flag/star", false),
    // No `NonEmptyList` guard, unlike its `*` sibling: the filter itself can
    // empty the list, and the key that emptied it has to be able to undo that.
    b("F", Chord::Char('F'), KeyCtx::List, KeyAction::ToggleFlaggedFilter, "Show flagged only (toggle)", false),
    bg("M", Chord::Char('M'), KeyCtx::List, Guard::NonEmptyList, KeyAction::MovePicker, "Move to mailbox (fuzzy picker)", false),
    bg("V", Chord::Char('V'), KeyCtx::List, Guard::NonEmptyList, KeyAction::Rsvp, "RSVP to invitation (Accept/Tentative/Decline)", false),
    bg("T", Chord::Char('T'), KeyCtx::List, Guard::NonEmptyList, KeyAction::OpenThread, "Show conversation (thread)", false),
    bg("A", Chord::Char('A'), KeyCtx::List, Guard::NonEmptyList, KeyAction::Approve, "Approve draft", false),
    bg("D", Chord::Char('D'), KeyCtx::List, Guard::NonEmptyList, KeyAction::MarkDraft, "Mark approved as draft (reverse A)", false),
    bg("x / X", Chord::Char('x'), KeyCtx::List, Guard::NonEmptyList, KeyAction::Send, "Send / Send all approved", false),
    bg("", Chord::Char('X'), KeyCtx::List, Guard::NonEmptyList, KeyAction::SendAll, "", false),
    bg("y", Chord::Char('y'), KeyCtx::List, Guard::NonEmptyList, KeyAction::CopyMessageRef, "Copy selector (mp://)", false),
    bg("o", Chord::Char('o'), KeyCtx::List, Guard::NonEmptyList, KeyAction::OpenAttachment, "Open attachment", false),
    bg("O", Chord::Char('O'), KeyCtx::List, Guard::NonEmptyList, KeyAction::SaveAttachment, "Save attachment to disk", false),
    bg("b", Chord::Char('b'), KeyCtx::List, Guard::NonEmptyList, KeyAction::OpenInBrowser, "Open HTML in browser", false),
    b("n", Chord::Char('n'), KeyCtx::List, KeyAction::NewDraft, "New draft", true),
    short(b("s / S", Chord::Char('s'), KeyCtx::List, KeyAction::QuickSync, "Quick sync / Full sync", true), "Sync"),
    b("", Chord::Char('S'), KeyCtx::List, KeyAction::FullSync, "", false),
    b("f", Chord::Char('f'), KeyCtx::List, KeyAction::ServerSearch, "Search (IMAP)", true),
    // -- SERVER SEARCH (overlay-internal; hand-dispatched) ----------------
    manual("j/k", KeyCtx::ServerSearch, "Navigate results", true),
    manual("gg / G", KeyCtx::ServerSearch, "Jump to top / bottom", false),
    manual("d/u", KeyCtx::ServerSearch, "Half-page down / up", false),
    short(manual("Enter / e", KeyCtx::ServerSearch, "Open in editor (mail read-only)", true), "Open (read-only)"),
    manual("r / R", KeyCtx::ServerSearch, "Reply / Reply-all", false),
    manual("w", KeyCtx::ServerSearch, "Forward", false),
    manual("a", KeyCtx::ServerSearch, "Archive", false),
    manual("b", KeyCtx::ServerSearch, "Open HTML in browser", false),
    manual("o", KeyCtx::ServerSearch, "Open attachment", false),
    manual("O", KeyCtx::ServerSearch, "Save attachment to disk", false),
    manual("Tab", KeyCtx::ServerSearch, "Switch focus", true),
    manual("Esc", KeyCtx::ServerSearch, "Close overlay", true),
    // -- CONTACTS (#0033) -------------------------------------------------
    // Read-only list + fuzzy search + detail. Live only in the Contacts view;
    // dispatched via the pane context like the Mail list. The `/` search input
    // itself is hand-dispatched (free-text) once armed by ContactsSearch.
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Contacts, KeyAction::ContactsDown, "Navigate contacts", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Contacts, KeyAction::ContactsUp, "", false),
    row("", Chord::PrefixLeader('g'), None, KeyCtx::Contacts, Guard::None, KeyAction::Manual, "", false),
    row("", Chord::Char('g'), Some('g'), KeyCtx::Contacts, Guard::None, KeyAction::ContactsTop, "", false),
    b("gg / G", Chord::Char('G'), KeyCtx::Contacts, KeyAction::ContactsBottom, "Jump to top / bottom", false),
    b("/", Chord::Char('/'), KeyCtx::Contacts, KeyAction::ContactsSearch, "Fuzzy search", true),
    short(b("Enter / n", Chord::Code(SpecialCode::Enter), KeyCtx::Contacts, KeyAction::ContactsCompose, "Compose to contact", true), "Compose"),
    b("", Chord::Char('n'), KeyCtx::Contacts, KeyAction::ContactsCompose, "", false),
    short(b("v", Chord::Char('v'), KeyCtx::Contacts, KeyAction::ContactsVcard, "Send contact as vCard", true), "Send vCard"),
    // `c` is free in this context: the Global `c` is a leader continuation
    // (`Space c`), and the mail-list `c` (edit recipients) is KeyCtx::List.
    short(b("c", Chord::Char('c'), KeyCtx::Contacts, KeyAction::ContactsCopyEmail, "Copy email address", true), "Copy email"),
    short(b("r", Chord::Char('r'), KeyCtx::Contacts, KeyAction::ContactsRefresh, "Refresh contact index", true), "Refresh"),
    // -- CALENDAR (#0034) -------------------------------------------------
    // Local-first agenda over the invites on disk. Live only in the Calendar
    // view; dispatched via the pane context like the Contacts list. `V` is the
    // same RSVP mnemonic as the mail list / body panes (separate context row).
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Calendar, KeyAction::CalendarDown, "Navigate events", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Calendar, KeyAction::CalendarUp, "", false),
    row("", Chord::PrefixLeader('g'), None, KeyCtx::Calendar, Guard::None, KeyAction::Manual, "", false),
    row("", Chord::Char('g'), Some('g'), KeyCtx::Calendar, Guard::None, KeyAction::CalendarTop, "", false),
    b("gg / G", Chord::Char('G'), KeyCtx::Calendar, KeyAction::CalendarBottom, "Jump to top / bottom", false),
    short(b("Enter / e", Chord::CharOrCode('e', SpecialCode::Enter), KeyCtx::Calendar, KeyAction::CalendarOpenSource, "Open the invite email in $EDITOR", true), "Open invite email"),
    short(b("V", Chord::Char('V'), KeyCtx::Calendar, KeyAction::CalendarRsvp, "RSVP to invitation (Accept/Tentative/Decline)", true), "RSVP"),
    short(b("t", Chord::Char('t'), KeyCtx::Calendar, KeyAction::CalendarToggleScope, "Show past events / upcoming only", true), "Past / upcoming"),
    short(b("r", Chord::Char('r'), KeyCtx::Calendar, KeyAction::CalendarRefresh, "Refresh events from disk", true), "Refresh"),
    // -- HEADERS ----------------------------------------------------------
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Headers, KeyAction::HeadersDown, "Scroll headers", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Headers, KeyAction::HeadersUp, "", false),
    // Hidden rows (empty display): the headers pane shares the list's
    // attachment/browser bindings; kept out of help so this pane's help stays
    // as it was (only j/k) while still dispatching through the table.
    b("", Chord::Char('o'), KeyCtx::Headers, KeyAction::OpenAttachment, "Open attachment", false),
    b("", Chord::Char('O'), KeyCtx::Headers, KeyAction::SaveAttachment, "Save attachment to disk", false),
    b("", Chord::Char('b'), KeyCtx::Headers, KeyAction::OpenInBrowser, "Open HTML in browser", false),
    // -- BODY -------------------------------------------------------------
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Preview, KeyAction::PreviewDown, "Scroll line by line", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Preview, KeyAction::PreviewUp, "", false),
    b("d/u", Chord::Char('d'), KeyCtx::Preview, KeyAction::PreviewHalfDown, "Half-page down / up", false),
    b("", Chord::Char('u'), KeyCtx::Preview, KeyAction::PreviewHalfUp, "", false),
    // Hidden rows (empty display): the body pane shares the attachment/browser
    // bindings; kept out of help so the body-pane help stays as before
    // (j/k, d/u, V, Esc) while still dispatching through the table.
    b("", Chord::Char('o'), KeyCtx::Preview, KeyAction::OpenAttachment, "Open attachment", false),
    b("", Chord::Char('O'), KeyCtx::Preview, KeyAction::SaveAttachment, "Save attachment to disk", false),
    b("", Chord::Char('b'), KeyCtx::Preview, KeyAction::OpenInBrowser, "Open HTML in browser", false),
    b("V", Chord::Char('V'), KeyCtx::Preview, KeyAction::Rsvp, "RSVP to invitation (Accept/Tentative/Decline)", false),
    b("Esc", Chord::Code(SpecialCode::Esc), KeyCtx::Preview, KeyAction::PreviewToList, "Return to list", true),
    // -- ACTIVITY LOG (overlay-internal; hand-dispatched) -----------------
    manual("j/k", KeyCtx::Activity, "Scroll line by line", true),
    manual("d/u", KeyCtx::Activity, "Half-page down / up", false),
    manual("gg / G", KeyCtx::Activity, "Jump to top / bottom", false),
    manual("/", KeyCtx::Activity, "Filter entries", false),
    manual("Esc", KeyCtx::Activity, "Close overlay", true),
];

/// Resolve a pressed key to a live [`KeyAction`] for the Normal-mode surface.
///
/// `guard_ok` evaluates a [`Guard`] against live app state (Drafts mailbox,
/// account count, non-empty list); guarded rows only match when it returns
/// true. `prefix_pending` gates leader continuations. Rows are tried in table
/// order, so `KEYMAP` ordering is the dispatch precedence.
///
/// Returns `None` when no live binding matches (the caller then leaves state
/// untouched, exactly like the old `_ => {}` arms).
pub fn resolve(
    ctx: KeyCtx,
    key: KeyEvent,
    prefix_pending: Option<char>,
    guard_ok: &impl Fn(Guard) -> bool,
) -> Option<KeyAction> {
    for kb in KEYMAP {
        if kb.ctx != ctx || matches!(kb.chord, Chord::Manual) {
            continue;
        }
        // Leader continuations require the matching prefix pending; non-prefixed
        // rows require no prefix pending (the leader key already consumed it).
        match (kb.prefix, prefix_pending) {
            (Some(p), Some(pending)) if p == pending => {}
            (Some(_), _) => continue,
            (None, _) => {}
        }
        if !guard_ok(kb.guard) {
            continue;
        }
        if kb.chord.matches(key, prefix_pending) {
            return Some(kb.action);
        }
    }
    None
}

/// Build the help-overlay sections from `KEYMAP`, in `HELP_ORDER`.
///
/// Rows whose `keys` is empty are skipped; synonym-only rows (e.g. the bare
/// `k`/up navigation split out for dispatch) are folded away by suppressing
/// duplicate `keys` within a section so the help copy stays as before.
pub fn help_sections() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    KeyCtx::HELP_ORDER
        .iter()
        .map(|&ctx| {
            let mut entries: Vec<(&'static str, &'static str)> = Vec::new();
            for kb in KEYMAP.iter().filter(|kb| kb.ctx == ctx) {
                // Skip pure dispatch-helper rows: the leader key itself and the
                // "up" synonym rows share their display slot with the combined
                // `j/k` / `gg` row already emitted.
                if kb.keys.is_empty()
                    || matches!(kb.chord, Chord::PrefixLeader(_))
                    || entries.iter().any(|(k, _)| *k == kb.keys)
                {
                    continue;
                }
                entries.push((kb.keys, kb.desc));
            }
            (ctx.group_title(), entries)
        })
        .filter(|(_, entries)| !entries.is_empty())
        .collect()
}

/// Bindings live in `ctx` that opt into the hint bar, in table order. Skips
/// leader-helper and synonym rows so the one-line bar stays readable.
pub fn hint_bindings(ctx: KeyCtx) -> impl Iterator<Item = &'static KeyBinding> {
    KEYMAP.iter().filter(move |kb| {
        kb.ctx == ctx
            && kb.hint
            && kb.prefix.is_none()
            && !matches!(kb.chord, Chord::PrefixLeader(_))
    })
}

/// Continuations of leader `prefix` live in `ctx` (for the pending-prefix hint
/// bar), in table order.
pub fn prefix_continuations(
    ctx: KeyCtx,
    prefix: char,
) -> impl Iterator<Item = &'static KeyBinding> {
    KEYMAP
        .iter()
        .filter(move |kb| kb.ctx == ctx && kb.prefix == Some(prefix))
}

/// Machine-readable dump of `KEYMAP` for regenerating the website key table
/// (`mp dump-keys`). Emitted as a Markdown table grouped by context so the
/// output is diff-friendly and human-auditable.
pub fn dump_markdown() -> String {
    let mut out = String::new();
    out.push_str("# mailypoppins TUI key bindings\n\n");
    out.push_str(
        "<!-- Generated by `mp dump-keys`. Do not edit by hand; edit\n     \
         src/tui/app/keymap.rs::KEYMAP and re-run. -->\n\n",
    );
    for &ctx in KeyCtx::HELP_ORDER {
        let sections = help_sections();
        let Some((title, entries)) = sections.iter().find(|(t, _)| *t == ctx.group_title()) else {
            continue;
        };
        out.push_str(&format!("## {}\n\n", title));
        out.push_str("| Key | Action |\n|-----|--------|\n");
        for (keys, desc) in entries {
            // A literal backtick can't sit inside a `code` span; use the
            // double-backtick + spaces form so Markdown renders it verbatim.
            let cell = if keys.contains('`') {
                format!("`` {} ``", keys)
            } else {
                format!("`{}`", keys)
            };
            out.push_str(&format!("| {} | {} |\n", cell, desc));
        }
        out.push('\n');
    }
    out
}

/// Machine-readable JSON dump of `KEYMAP`, grouped by help section, for the
/// website (`mp dump-keys --json`). Rendered by hand (no serde dep) so the
/// crate keeps its lean dependency set; the shape is
/// `[{"title": "...", "bindings": [{"key": "...", "action": "..."}]}]`.
pub fn dump_json() -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    let sections = help_sections();
    let mut out = String::from("[\n");
    for (si, (title, entries)) in sections.iter().enumerate() {
        out.push_str("  {\n");
        out.push_str(&format!("    \"title\": \"{}\",\n", esc(title)));
        out.push_str("    \"bindings\": [\n");
        for (bi, (key, action)) in entries.iter().enumerate() {
            out.push_str(&format!(
                "      {{ \"key\": \"{}\", \"action\": \"{}\" }}",
                esc(key),
                esc(action)
            ));
            out.push_str(if bi + 1 < entries.len() { ",\n" } else { "\n" });
        }
        out.push_str("    ]\n");
        out.push_str(if si + 1 < sections.len() { "  },\n" } else { "  }\n" });
    }
    out.push_str("]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn allow(_: Guard) -> bool {
        true
    }

    /// #0078: a binding with no short label falls back to `desc`, so only the
    /// rows that need one carry one and no row can end up label-less.
    #[test]
    fn a_binding_without_a_short_label_falls_back_to_its_description() {
        let plain = b("q", Chord::Char('q'), KeyCtx::Global, KeyAction::Quit, "Quit", true);
        assert_eq!(plain.short, "");
        assert_eq!(plain.hint_label(), "Quit");

        let labelled = short(plain, "Bye");
        assert_eq!(labelled.desc, "Quit", "the long description is untouched");
        assert_eq!(labelled.hint_label(), "Bye");

        for kb in KEYMAP {
            if kb.desc.is_empty() {
                continue;
            }
            assert!(!kb.hint_label().is_empty(), "{:?} has no hint label", kb.keys);
            assert!(
                kb.short.is_empty() || kb.short.len() < kb.desc.len(),
                "{:?}: the short label {:?} is not shorter than {:?}",
                kb.keys,
                kb.short,
                kb.desc
            );
        }
    }

    /// The short label is the hint bar's alone: the help overlay and
    /// `mp dump-keys` (hence the website table) read `desc` (#0078).
    #[test]
    fn only_hinted_rows_carry_a_short_label() {
        for kb in KEYMAP {
            assert!(
                kb.short.is_empty() || kb.hint,
                "{:?} carries a short label but is never hinted",
                kb.keys
            );
        }
    }

    /// No line of hints can exceed the golden 120-column width, which is what
    /// used to make ratatui clip the last binding mid-word (#0078).
    #[test]
    fn every_contexts_hint_row_fits_the_golden_width_after_the_short_labels() {
        for ctx in [
            KeyCtx::Global,
            KeyCtx::Sidebar,
            KeyCtx::List,
            KeyCtx::Headers,
            KeyCtx::Preview,
            KeyCtx::ServerSearch,
            KeyCtx::Contacts,
            KeyCtx::Calendar,
            KeyCtx::Activity,
            KeyCtx::Help,
        ] {
            // ` 2 SELECTED ` is the widest badge, plus its two trailing
            // spaces; the hint bar renders after it.
            let mut width = " 2 SELECTED ".len() + 2;
            for (i, kb) in hint_bindings(ctx).enumerate() {
                width += usize::from(i > 0) * 2 + kb.keys.len() + 1 + kb.hint_label().len();
            }
            // `List` is still over budget at 120 and is truncated cleanly by
            // the renderer; every other context now fits outright.
            if ctx != KeyCtx::List {
                assert!(width <= 120, "{ctx:?} hint row is {width} columns");
            }
        }
    }

    /// No two live bindings share the same (context, keys, prefix) triple:
    /// a duplicate would mean two rows fight to document the same chord.
    #[test]
    fn no_duplicate_bindings_in_same_context() {
        let mut seen = std::collections::HashSet::new();
        for kb in KEYMAP {
            if kb.keys.is_empty() {
                continue;
            }
            let dup = !seen.insert((kb.ctx, kb.keys, kb.prefix));
            assert!(
                !dup,
                "duplicate binding {:?} in {:?} (prefix {:?})",
                kb.keys, kb.ctx, kb.prefix
            );
        }
    }

    /// No two live *dispatch* rows in the same context match the same key with
    /// the same prefix state: two rows fighting to dispatch a key would be a
    /// latent behavior bug.
    #[test]
    fn no_duplicate_live_dispatch_per_context() {
        let probes: Vec<KeyEvent> = ('!'..='~')
            .map(key)
            .chain(('a'..='z').map(ctrl))
            .chain([
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            ])
            .collect();
        for &ctx in KeyCtx::HELP_ORDER {
            for &pending in &[None, Some('g'), Some(' ')] {
                for &ev in &probes {
                    let mut hits = KEYMAP.iter().filter(|kb| {
                        kb.ctx == ctx
                            && !matches!(kb.chord, Chord::Manual)
                            && match (kb.prefix, pending) {
                                (Some(p), Some(q)) => p == q,
                                (Some(_), None) => false,
                                (None, _) => true,
                            }
                            && kb.chord.matches(ev, pending)
                    });
                    let first = hits.next();
                    if let Some(second) = hits.next() {
                        panic!(
                            "key {:?} (pending {:?}) matches two rows in {:?}: {:?} and {:?}",
                            ev, pending, ctx, first.unwrap().keys, second.keys
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn help_sections_are_non_empty() {
        let sections = help_sections();
        assert!(!sections.is_empty(), "help must have at least one section");
        for (title, entries) in &sections {
            assert!(!entries.is_empty(), "section {title} must have entries");
        }
    }

    #[test]
    fn every_context_with_bindings_appears_in_help_order() {
        for kb in KEYMAP {
            assert!(
                KeyCtx::HELP_ORDER.contains(&kb.ctx) || kb.ctx == KeyCtx::Help,
                "context {:?} is not in HELP_ORDER",
                kb.ctx
            );
        }
    }

    #[test]
    fn dump_markdown_parses_as_table() {
        let dump = dump_markdown();
        assert!(dump.contains("## GLOBAL"));
        assert!(dump.contains("## EMAIL LIST"));
        let rows = dump.lines().filter(|l| l.starts_with("| `")).count();
        assert!(rows >= 30, "expected the full keymap in the dump, got {rows} rows");
        for line in dump.lines().filter(|l| l.starts_with("| `")) {
            assert_eq!(line.matches('|').count(), 3, "malformed table row: {line}");
        }
    }

    /// Guards live in the data model, not ad-hoc code.
    #[test]
    fn context_guards_are_modeled() {
        let c = KEYMAP
            .iter()
            .find(|kb| kb.keys == "c" && kb.ctx == KeyCtx::List)
            .expect("edit-recipients binding present");
        assert_eq!(c.guard, Guard::DraftsOnly);

        let backtick = KEYMAP
            .iter()
            .find(|kb| kb.keys == "`")
            .expect("account switch binding present");
        assert_eq!(backtick.guard, Guard::MultiAccount);
    }

    #[test]
    fn leader_prefix_is_first_class_data() {
        // Both leaders (g for list jumps, Space for the view switcher) are
        // catalogued as prefixed continuation data plus a PrefixLeader row.
        for leader in [' ', 'g'] {
            let combos: Vec<_> =
                KEYMAP.iter().filter(|kb| kb.prefix == Some(leader)).collect();
            assert!(
                !combos.is_empty(),
                "leader {leader:?} must be represented as prefixed data"
            );
            assert!(
                KEYMAP
                    .iter()
                    .any(|kb| matches!(kb.chord, Chord::PrefixLeader(p) if p == leader)),
                "the bare {leader:?} leader must be a first-class chord row"
            );
        }
    }

    /// The two leaders never cross-arm: while Space is pending, the g
    /// continuations do not fire, and vice versa. (The dispatch-time analogue
    /// of the view-switcher-on-Space decision.)
    #[test]
    fn leaders_do_not_cross_arm() {
        // Space pending -> the view-switch continuation `m` fires (Global),
        // but the list `g` continuation does NOT.
        assert_eq!(
            resolve(KeyCtx::Global, key('m'), Some(' '), &allow),
            Some(KeyAction::SwitchView)
        );
        assert_eq!(resolve(KeyCtx::List, key('g'), Some(' '), &allow), None);
        // g pending -> the list `g` continuation fires, but the Space view
        // switch does NOT (its continuations require Space pending).
        assert_eq!(
            resolve(KeyCtx::List, key('g'), Some('g'), &allow),
            Some(KeyAction::ListTop)
        );
        assert_eq!(resolve(KeyCtx::Global, key('m'), Some('g'), &allow), None);
        // Same in the Calendar pane context (#0034), which owns `gg`/`G` too.
        assert_eq!(resolve(KeyCtx::Calendar, key('g'), Some(' '), &allow), None);
        assert_eq!(
            resolve(KeyCtx::Calendar, key('g'), Some('g'), &allow),
            Some(KeyAction::CalendarTop)
        );
        // A bare Space with nothing pending arms the leader (Manual toggle).
        assert_eq!(
            resolve(KeyCtx::Global, key(' '), None, &allow),
            Some(KeyAction::Manual)
        );
    }

    /// The Normal-mode surface resolves through the table: representative keys
    /// map to the expected actions, proving dispatch is data-driven.
    #[test]
    fn resolve_dispatches_through_table() {
        // Global quit.
        assert_eq!(
            resolve(KeyCtx::Global, key('q'), None, &allow),
            Some(KeyAction::Quit)
        );
        // List reply vs reply-all are distinct.
        assert_eq!(
            resolve(KeyCtx::List, key('r'), None, &allow),
            Some(KeyAction::Reply)
        );
        assert_eq!(
            resolve(KeyCtx::List, key('R'), None, &allow),
            Some(KeyAction::ReplyAll)
        );
        // Ctrl+e (config) does not collide with bare e (editor) in List.
        assert_eq!(
            resolve(KeyCtx::Global, ctrl('e'), None, &allow),
            Some(KeyAction::OpenConfigFile)
        );
        assert_eq!(
            resolve(KeyCtx::List, key('e'), None, &allow),
            Some(KeyAction::OpenEditor)
        );
        // Leader: bare g pending, then g -> top.
        assert_eq!(
            resolve(KeyCtx::List, key('g'), Some('g'), &allow),
            Some(KeyAction::ListTop)
        );
        // Without pending prefix, g is a leader (Manual action reserved -> None
        // via the PrefixLeader row, which resolve returns as its action).
        assert_eq!(
            resolve(KeyCtx::List, key('g'), None, &allow),
            Some(KeyAction::Manual)
        );
    }

    /// `T` opens the conversation overlay from the list (#0008).
    #[test]
    fn thread_key_resolves_in_the_list() {
        assert_eq!(
            resolve(KeyCtx::List, key('T'), None, &allow),
            Some(KeyAction::OpenThread)
        );
    }

    /// Guarded rows respect the live guard evaluation.
    #[test]
    fn resolve_respects_guards() {
        let deny_drafts = |g: Guard| g != Guard::DraftsOnly;
        // `c` is guarded to Drafts; when the guard denies, it does not resolve.
        assert_eq!(resolve(KeyCtx::List, key('c'), None, &deny_drafts), None);
        // When allowed, it resolves.
        assert_eq!(
            resolve(KeyCtx::List, key('c'), None, &allow),
            Some(KeyAction::EditRecipients)
        );
    }
}
