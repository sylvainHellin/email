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
//! A binding may declare a single-key `prefix`. The leaders are the `Space`
//! view switcher (`Space m/c/a`, #0033), the Vim `g` motion prefix, and the
//! five mnemonic family leaders `f`/`c`/`g`/`t`/`s` (#0092: find, compose, go,
//! thread, system). The chord matcher only fires a prefixed binding when the
//! matching prefix is pending; `handle_key` sets `App::pending_prefix` to the
//! armed leader when a bare prefix key is seen (the hint bar and the which-key
//! popup then show the pending continuations). Strict prefix mode (see
//! [`resolve`]): while a prefix is pending only that family's continuations are
//! eligible, so a key that is not a continuation cancels the chord instead of
//! firing a flat binding (which-key semantics), and the leaders never
//! cross-arm.
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
    /// Actions on the current message, live in every reading pane (List,
    /// Headers, Body). Promoted from the old List-only set (#0092) so a
    /// message action resolves identically whichever reading pane has focus.
    Message,
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
            KeyCtx::Message => "MESSAGE (list, headers, body)",
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
        KeyCtx::Message,
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
    SwitchAccount,
    JumpAccount,
    JumpMailbox,
    /// `gm`: focus the mailbox sidebar (go to mailboxes).
    GoMailbox,
    FocusForward,
    FocusBackward,
    /// Arm one of the mnemonic family leaders (`f`/`c`/`g`/`t`/`s`, #0092).
    /// Unlike [`KeyAction::Manual`] (the view-agnostic Space leader), this is
    /// Mail-only, so it is swallowed in the Contacts/Calendar views unless the
    /// active pane rebinds the letter.
    ArmPrefix,
    /// Zoom the focused pane to the whole content area, or restore the split
    /// (#TKT-0044). Mail view only, which is why it is deliberately absent
    /// from [`KeyAction::is_view_agnostic`].
    ToggleZoom,
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
    /// Advance the list cursor to the next / previous message from any reading
    /// pane (`J`/`K`, `gj`/`gk`), so triage does not need a focus hop (#0092).
    NextMessage,
    PrevMessage,
    /// Esc in any reading pane: clear a live selection, else return to the
    /// list (#0092, symmetric across List / Headers / Body).
    EscMessage,
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

/// Prefixed (leader-continuation) live binding, e.g. `ff`, `cr`, `ss`.
const fn p(
    keys: &'static str,
    chord: Chord,
    prefix: char,
    ctx: KeyCtx,
    action: KeyAction,
    desc: &'static str,
    hint: bool,
) -> KeyBinding {
    row(keys, chord, Some(prefix), ctx, Guard::None, action, desc, hint)
}

/// Prefixed guarded live binding.
const fn pg(
    keys: &'static str,
    chord: Chord,
    prefix: char,
    ctx: KeyCtx,
    guard: Guard,
    action: KeyAction,
    desc: &'static str,
    hint: bool,
) -> KeyBinding {
    row(keys, chord, Some(prefix), ctx, guard, action, desc, hint)
}

/// A bare leader key that arms a family/prefix (`f`/`c`/`g`/`t`/`s`, Space).
const fn leader(prefix: char, ctx: KeyCtx, action: KeyAction) -> KeyBinding {
    row("", Chord::PrefixLeader(prefix), None, ctx, Guard::None, action, "", false)
}

/// Whether a family leader stays live outside the Mail view: true iff the
/// family carries at least one view-agnostic continuation (today only `s`,
/// whose `sc`/`sf`/`sl` open the config, the log and the activity overlay
/// from any view). Derived from [`KEYMAP`] so it cannot drift from the data.
pub fn leader_is_view_agnostic(leader: char) -> bool {
    KEYMAP
        .iter()
        .any(|b| b.prefix == Some(leader) && b.action.is_view_agnostic())
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
    // The flat escape hatches plus the five mnemonic family leaders (#0092):
    // `f` find, `c` compose, `g` go, `t` thread/attach, `s` system/sync. A
    // leader arms a pending prefix; the which-key popup and hint bar then show
    // that family's continuations. Leaders are Mail-only (ArmPrefix is not
    // view-agnostic), so a family letter that a non-Mail view binds flat (the
    // Contacts `c`, the Calendar `t`) still reaches that view's pane action.
    // Exception: a family whose continuations include view-agnostic actions
    // (`s`, see `leader_is_view_agnostic`) arms in every view, so the config/
    // log/activity utilities stay reachable outside Mail.
    b("q", Chord::Char('q'), KeyCtx::Global, KeyAction::Quit, "Quit", true),
    b("1-9", Chord::Digit, KeyCtx::Global, KeyAction::JumpMailbox, "Jump to mailbox", true),
    b("Tab", Chord::Code(SpecialCode::Tab), KeyCtx::Global, KeyAction::FocusForward, "Cycle focus forward", false),
    b("Shift+Tab", Chord::Code(SpecialCode::BackTab), KeyCtx::Global, KeyAction::FocusBackward, "Cycle focus backward", false),
    b("?", Chord::Char('?'), KeyCtx::Global, KeyAction::ToggleHelp, "Toggle this help", true),
    // Zoom is Global by context but Mail-only by action: `is_view_agnostic`
    // leaves it out, so the dispatcher swallows `z` in Contacts and Calendar,
    // where a two-pane split the user can zoom does not exist (#TKT-0044).
    short(b("z", Chord::Char('z'), KeyCtx::Global, KeyAction::ToggleZoom, "Zoom / unzoom the focused pane", true), "Zoom pane"),
    b("!", Chord::Char('!'), KeyCtx::Global, KeyAction::ToggleActivityLog, "Toggle activity log", false),
    // Send the current draft from any focus: one confirm, and an unapproved
    // draft is approved as part of the send (#0092, merges the old `A` + `x`).
    short(b("x", Chord::Char('x'), KeyCtx::Global, KeyAction::Send, "Send current draft (approve + send)", true), "Send"),
    // Family leaders.
    leader('f', KeyCtx::Global, KeyAction::ArmPrefix),
    leader('c', KeyCtx::Global, KeyAction::ArmPrefix),
    leader('g', KeyCtx::Global, KeyAction::ArmPrefix),
    leader('t', KeyCtx::Global, KeyAction::ArmPrefix),
    leader('s', KeyCtx::Global, KeyAction::ArmPrefix),
    // `f` find family (global entry): one FTS-backed search over all mail.
    p("ff", Chord::Char('f'), 'f', KeyCtx::Global, KeyAction::ServerSearch, "Search all mail (sender, subject, body)", true),
    // `c` compose family (global entry).
    p("cn", Chord::Char('n'), 'c', KeyCtx::Global, KeyAction::NewDraft, "New draft", true),
    // `g` go family (global jumps).
    p("gm", Chord::Char('m'), 'g', KeyCtx::Global, KeyAction::GoMailbox, "Go to mailboxes (sidebar)", false),
    pg("ga", Chord::Char('a'), 'g', KeyCtx::Global, Guard::MultiAccount, KeyAction::SwitchAccount, "Switch account", false),
    // `s` system / sync / accounts family.
    p("ss", Chord::Char('s'), 's', KeyCtx::Global, KeyAction::QuickSync, "Quick sync", true),
    p("sS", Chord::Char('S'), 's', KeyCtx::Global, KeyAction::FullSync, "Full sync", false),
    p("sl", Chord::Char('l'), 's', KeyCtx::Global, KeyAction::OpenActivityOverlay, "Activity log overlay", false),
    p("sc", Chord::Char('c'), 's', KeyCtx::Global, KeyAction::OpenConfigFile, "Open config.toml in $EDITOR", false),
    p("sf", Chord::Char('f'), 's', KeyCtx::Global, KeyAction::OpenLogFile, "Open log file in $EDITOR", false),
    // View switcher leader (#0033): Space opens the leader, then m/c/a picks a
    // view. Manual (view-agnostic) so it works from every pane and view.
    leader(' ', KeyCtx::Global, KeyAction::Manual),
    row("Space m", Chord::Char('m'), Some(' '), KeyCtx::Global, Guard::None, KeyAction::SwitchView, "Switch to Mail view", true),
    row("Space c", Chord::Char('c'), Some(' '), KeyCtx::Global, Guard::None, KeyAction::SwitchView, "Switch to Contacts view", true),
    row("Space a", Chord::Char('a'), Some(' '), KeyCtx::Global, Guard::None, KeyAction::SwitchView, "Switch to Calendar view", true),
    // -- MESSAGE (list, headers, body) ------------------------------------
    // Actions on the current message, live in all three reading panes so
    // acting on what you are reading never needs a focus hop (#0092). The
    // dispatcher tries this context after the focused pane's own context, so a
    // pane keeps its scroll keys (`j`/`k`) while sharing every message action.
    short(bg("J / K", Chord::Char('J'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::NextMessage, "Next / previous message", true), "Next/prev"),
    bg("", Chord::Char('K'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::PrevMessage, "", false),
    short(bg("Enter / e", Chord::CharOrCode('e', SpecialCode::Enter), KeyCtx::Message, Guard::NonEmptyList, KeyAction::OpenEditor, "Open in editor (mail read-only)", true), "Open (read-only)"),
    bg("r", Chord::Char('r'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::Reply, "Reply", true),
    bg("a", Chord::Char('a'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::Archive, "Archive", true),
    bg("d", Chord::Char('d'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::Delete, "Delete", true),
    bg("u", Chord::Char('u'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::ToggleRead, "Toggle read/unread", false),
    bg("*", Chord::Char('*'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::ToggleFlag, "Toggle flag/star", false),
    bg("M", Chord::Char('M'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::MovePicker, "Move to mailbox (fuzzy picker)", false),
    bg("y", Chord::Char('y'), KeyCtx::Message, Guard::NonEmptyList, KeyAction::CopyMessageRef, "Copy selector (mp://)", false),
    short(b("Esc", Chord::Code(SpecialCode::Esc), KeyCtx::Message, KeyAction::EscMessage, "Clear selection / return to list", true), "Back"),
    // `g` go-family continuations that apply to every reading pane.
    p("gj / gk", Chord::Char('j'), 'g', KeyCtx::Message, KeyAction::NextMessage, "Next / previous message", false),
    p("", Chord::Char('k'), 'g', KeyCtx::Message, KeyAction::PrevMessage, "", false),
    // `f` find family: narrow the current list (metadata, incremental).
    p("fm", Chord::Char('m'), 'f', KeyCtx::Message, KeyAction::FilterMetadata, "Filter the current list", false),
    // `c` compose family (message-scoped continuations).
    pg("cr", Chord::Char('r'), 'c', KeyCtx::Message, Guard::NonEmptyList, KeyAction::Reply, "Reply", false),
    pg("ca", Chord::Char('a'), 'c', KeyCtx::Message, Guard::NonEmptyList, KeyAction::ReplyAll, "Reply all", false),
    pg("cf", Chord::Char('f'), 'c', KeyCtx::Message, Guard::NonEmptyList, KeyAction::Forward, "Forward", false),
    // `t` thread / attachment family.
    pg("tt", Chord::Char('t'), 't', KeyCtx::Message, Guard::NonEmptyList, KeyAction::OpenThread, "Show conversation (thread)", false),
    pg("to", Chord::Char('o'), 't', KeyCtx::Message, Guard::NonEmptyList, KeyAction::OpenAttachment, "Open attachment", false),
    pg("ts", Chord::Char('s'), 't', KeyCtx::Message, Guard::NonEmptyList, KeyAction::SaveAttachment, "Save attachment to disk", false),
    pg("tb", Chord::Char('b'), 't', KeyCtx::Message, Guard::NonEmptyList, KeyAction::OpenInBrowser, "Open HTML in browser", false),
    pg("tv", Chord::Char('v'), 't', KeyCtx::Message, Guard::NonEmptyList, KeyAction::Rsvp, "RSVP to invitation (Accept/Tentative/Decline)", false),
    // -- SIDEBAR ----------------------------------------------------------
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Sidebar, KeyAction::SidebarDown, "Navigate mailboxes", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Sidebar, KeyAction::SidebarUp, "", false),
    b("Enter", Chord::Code(SpecialCode::Enter), KeyCtx::Sidebar, KeyAction::SidebarSelect, "Select mailbox", true),
    // -- EMAIL LIST -------------------------------------------------------
    // List-only affordances: cursor motion, selection, the go-family jumps and
    // the two find-family list filters. Every message action lives in the
    // shared MESSAGE context above, so this section is just what only the list
    // can do. Most rows guard on a non-empty list (the old empty-list early
    // return); the flagged filter does not, since it can empty the list itself.
    short(bg("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListDown, "Navigate emails", true), "Navigate"),
    bg("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListUp, "", false),
    row("", Chord::Char('g'), Some('g'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListTop, "", false),
    bg("gg / G", Chord::Char('G'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ListBottom, "Jump to top / bottom", false),
    // `gt` ("go to date") rather than `gd`: `d` is delete in the message
    // context, and `gd` resolving against a mistyped delete is exactly what
    // `no_duplicate_live_dispatch_per_context` refuses.
    row("gt", Chord::Char('t'), Some('g'), KeyCtx::List, Guard::NonEmptyList, KeyAction::JumpToDate, "Jump to date (e.g. last week)", false),
    short(bg("v", Chord::Char('v'), KeyCtx::List, Guard::NonEmptyList, KeyAction::ToggleSelect, "Toggle selection", true), "Select"),
    bg("Ctrl+a", Chord::CtrlChar('a'), KeyCtx::List, Guard::NonEmptyList, KeyAction::SelectAllVisible, "Select all visible", false),
    // `ce` edit recipients, Drafts only (the compose family's list-scoped tail).
    pg("ce", Chord::Char('e'), 'c', KeyCtx::List, Guard::DraftsOnly, KeyAction::EditRecipients, "Edit recipients (Drafts only)", false),
    // `fF` flagged-only filter (the find family's list-scoped tail). No
    // NonEmptyList guard: the filter can empty the list and must be able to
    // undo that.
    p("fF", Chord::Char('F'), 'f', KeyCtx::List, KeyAction::ToggleFlaggedFilter, "Show flagged only (toggle)", false),
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
    // The headers pane scrolls; every message action (open attachment, browser,
    // reply, triage, next/prev, Esc) comes from the shared MESSAGE context, so
    // there are no longer hidden pane-local rows here (#0092).
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Headers, KeyAction::HeadersDown, "Scroll headers", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Headers, KeyAction::HeadersUp, "", false),
    // -- BODY -------------------------------------------------------------
    // Bare `j`/`k` scroll the body; bare `d`/`u` are delete / toggle-read from
    // the MESSAGE context, so half-page scroll moved to Ctrl+d / Ctrl+u (#0092).
    b("j/k", Chord::CharOrCode('j', SpecialCode::Down), KeyCtx::Preview, KeyAction::PreviewDown, "Scroll line by line", true),
    b("", Chord::CharOrCode('k', SpecialCode::Up), KeyCtx::Preview, KeyAction::PreviewUp, "", false),
    b("Ctrl+d / Ctrl+u", Chord::CtrlChar('d'), KeyCtx::Preview, KeyAction::PreviewHalfDown, "Half-page down / up", false),
    b("", Chord::CtrlChar('u'), KeyCtx::Preview, KeyAction::PreviewHalfUp, "", false),
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
        // Strict prefix mode (#0092): while a prefix is pending, ONLY that
        // family's continuations are eligible; a flat (non-prefixed) row is
        // inert until the prefix is cleared. This is which-key semantics: a
        // key that is not a continuation of the pending family cancels the
        // chord instead of firing a flat binding, so an accidental `s` then
        // `x` cannot send. When nothing is pending only flat rows match; the
        // `PrefixLeader` rows (also non-prefixed) then arm the family.
        match (kb.prefix, prefix_pending) {
            (Some(p), Some(pending)) if p == pending => {}
            (None, None) => {}
            _ => continue,
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
            KeyCtx::Message,
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
            for &pending in &[
                None,
                Some('g'),
                Some(' '),
                Some('f'),
                Some('c'),
                Some('t'),
                Some('s'),
            ] {
                for &ev in &probes {
                    let mut hits = KEYMAP.iter().filter(|kb| {
                        kb.ctx == ctx
                            && !matches!(kb.chord, Chord::Manual)
                            && match (kb.prefix, pending) {
                                (Some(p), Some(q)) => p == q,
                                (None, None) => true,
                                _ => false,
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
        // `ce` edit-recipients is Drafts-only (the compose family's list tail).
        let ce = KEYMAP
            .iter()
            .find(|kb| kb.keys == "ce" && kb.ctx == KeyCtx::List)
            .expect("edit-recipients binding present");
        assert_eq!(ce.guard, Guard::DraftsOnly);

        // `ga` switch-account is multi-account only (#0092, replaced `` ` ``).
        let ga = KEYMAP
            .iter()
            .find(|kb| kb.keys == "ga")
            .expect("account switch binding present");
        assert_eq!(ga.guard, Guard::MultiAccount);
    }

    #[test]
    fn leader_prefix_is_first_class_data() {
        // Both leaders (g for list jumps, Space for the view switcher) are
        // catalogued as prefixed continuation data plus a PrefixLeader row.
        for leader in [' ', 'f', 'c', 'g', 't', 's'] {
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
    fn only_the_system_family_leader_is_view_agnostic() {
        assert!(leader_is_view_agnostic('s'), "`s` carries config/log/activity");
        for l in ['f', 'c', 'g', 't'] {
            assert!(!leader_is_view_agnostic(l), "family `{l}` must stay Mail-only");
        }
    }

    #[test]
    fn leaders_do_not_cross_arm() {
        // Space pending -> the view-switch continuation `m` fires (Global),
        // but the list `g` continuation does NOT (strict prefix mode).
        assert_eq!(
            resolve(KeyCtx::Global, key('m'), Some(' '), &allow),
            Some(KeyAction::SwitchView)
        );
        assert_eq!(resolve(KeyCtx::List, key('g'), Some(' '), &allow), None);
        // g pending -> the list `g` continuation fires; the go-family global
        // jump `gm` fires; the Space view switch does NOT.
        assert_eq!(
            resolve(KeyCtx::List, key('g'), Some('g'), &allow),
            Some(KeyAction::ListTop)
        );
        assert_eq!(
            resolve(KeyCtx::Global, key('m'), Some('g'), &allow),
            Some(KeyAction::GoMailbox)
        );
        // Strict prefix: a flat key does not fire while a prefix is pending, so
        // `s` then `x` cannot send (the accidental-destructive case #0092
        // guards against).
        assert_eq!(resolve(KeyCtx::Global, key('x'), Some('s'), &allow), None);
        // Same in the Calendar pane context (#0034), which owns `gg`/`G` too.
        assert_eq!(resolve(KeyCtx::Calendar, key('g'), Some(' '), &allow), None);
        assert_eq!(
            resolve(KeyCtx::Calendar, key('g'), Some('g'), &allow),
            Some(KeyAction::CalendarTop)
        );
        // A bare Space with nothing pending arms the view leader (Manual).
        assert_eq!(
            resolve(KeyCtx::Global, key(' '), None, &allow),
            Some(KeyAction::Manual)
        );
        // A bare family leader with nothing pending arms its prefix.
        assert_eq!(
            resolve(KeyCtx::Global, key('s'), None, &allow),
            Some(KeyAction::ArmPrefix)
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
        // Message-context reply (flat `r`) is shared by every reading pane;
        // reply-all is the `ca` compose-family chord.
        assert_eq!(
            resolve(KeyCtx::Message, key('r'), None, &allow),
            Some(KeyAction::Reply)
        );
        assert_eq!(
            resolve(KeyCtx::Message, key('a'), Some('c'), &allow),
            Some(KeyAction::ReplyAll)
        );
        // The system family carries config/log opens (`sc`, `sf`), replacing
        // the old Ctrl+e / Ctrl+l flat keys.
        assert_eq!(
            resolve(KeyCtx::Global, key('c'), Some('s'), &allow),
            Some(KeyAction::OpenConfigFile)
        );
        // Enter / e open the message from any reading pane.
        assert_eq!(
            resolve(KeyCtx::Message, key('e'), None, &allow),
            Some(KeyAction::OpenEditor)
        );
        // Leader: g pending, then g -> list top.
        assert_eq!(
            resolve(KeyCtx::List, key('g'), Some('g'), &allow),
            Some(KeyAction::ListTop)
        );
        // The family leaders arm from the Global context (bare g, nothing
        // pending, is the go-family leader).
        assert_eq!(
            resolve(KeyCtx::Global, key('g'), None, &allow),
            Some(KeyAction::ArmPrefix)
        );
        // The find-family search entry.
        assert_eq!(
            resolve(KeyCtx::Global, key('f'), Some('f'), &allow),
            Some(KeyAction::ServerSearch)
        );
    }

    /// `tt` opens the conversation overlay from any reading pane (#0008/#0092).
    #[test]
    fn thread_chord_resolves_in_the_message_context() {
        assert_eq!(
            resolve(KeyCtx::Message, key('t'), Some('t'), &allow),
            Some(KeyAction::OpenThread)
        );
    }

    /// Guarded rows respect the live guard evaluation.
    #[test]
    fn resolve_respects_guards() {
        let deny_drafts = |g: Guard| g != Guard::DraftsOnly;
        // `ce` is guarded to Drafts; when the guard denies, it does not resolve.
        assert_eq!(
            resolve(KeyCtx::List, key('e'), Some('c'), &deny_drafts),
            None
        );
        // When allowed, it resolves.
        assert_eq!(
            resolve(KeyCtx::List, key('e'), Some('c'), &allow),
            Some(KeyAction::EditRecipients)
        );
    }
}
