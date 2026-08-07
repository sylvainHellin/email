//! Golden-frame capture of the current TUI (#0049, unit 0a).
//!
//! These snapshots are the look-and-feel parity oracle for the data-access
//! layer rewrite: after the nuke there is no byte-identical `.md` write left
//! to diff against, so the rendered frame becomes the contract. They are
//! deliberately few. A frame is snapshotted only where it carries meaning:
//! the mail view (sidebar + list + headers + preview + status line), the same
//! view under a multi-row selection, the calendar agenda, and the help
//! overlay. There is no size sweep and no per-widget cosmetic capture.
//!
//! Two properties make the capture trustworthy:
//!
//! - The fixture is frozen. Fixed dates (never `now()`), no account, no
//!   directory walk, no network, and an empty activity log (its entries carry
//!   a `Local` timestamp that would print the wall clock into the frame).
//! - The theme is pinned. `App::new` reads `theme` from the user's global
//!   config, so an unpinned frame would encode the developer's machine.
//!   [`pin_theme`] fixes it to the default palette and asserts it took.
//!
//! ratatui's buffer `Display` prints symbols only, so a colour or modifier
//! regression would slip through a plain text dump. The snapshot therefore
//! carries a second section: per-row runs of the styles that carry meaning
//! (unread bold, cursor row fill, selection foreground, cancelled strike).
//! A full style dump was rejected on purpose; it would fail on every palette
//! tweak and teach the reader to approve diffs blindly.

use std::collections::HashSet;
use std::sync::Arc;

use insta::assert_snapshot;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use crate::tui::app::{
    App, CalendarEvent, EmailEntry, EntryKey, MailboxInfo, MailboxKind, MessageRef, Overlay, View,
};
use crate::tui::theme::{self, Theme};
use crate::types::EventFrontmatter;

/// Frame size every golden frame is captured at (#0049 fixes 120x40).
const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

// ---------------------------------------------------------------------------
// Capture helpers
// ---------------------------------------------------------------------------

/// Pin the process-wide theme to the default palette.
///
/// `theme::init` is a `OnceLock`, so whichever test wins the race sets it for
/// the whole binary; every path in the test binary resolves to the default
/// palette, and the assertion below turns a future divergence into a failure
/// here instead of an unexplained snapshot diff.
fn pin_theme() {
    let _ = theme::init(theme::DEFAULT_THEME_NAME);
    assert_eq!(
        *theme::active(),
        Theme::catppuccin_mocha(),
        "golden frames require the default theme; the process theme was pinned elsewhere"
    );
}

/// Render `app` through [`super::view`] on a `TestBackend` and return the
/// snapshot body: the text rows, then the meaning-carrying style runs.
fn frame_snapshot(app: &mut App, width: u16, height: u16) -> String {
    pin_theme();
    // The layout branches on the app's own idea of the terminal size (the
    // real one gets it from the resize handler), so it must match the backend.
    app.terminal_width = width;
    app.terminal_height = height;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| super::view(app, frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    format!(
        "== text {width}x{height} (trailing spaces trimmed) ==\n{}\n\
         == style runs (col ranges are half-open; rows with no meaning-carrying style are omitted) ==\n{}",
        text_rows(&buffer),
        style_runs(&buffer)
    )
}

/// The rendered glyphs, one line per row, prefixed with the row number so a
/// style-run line can be matched back to its text.
fn text_rows(buffer: &Buffer) -> String {
    let area = *buffer.area();
    (0..area.height)
        .map(|y| {
            let row: String = (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string();
            format!("{y:02} |{row}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The style tags of one cell, or an empty string when it carries none.
///
/// Only four properties are recorded, because only these four say something
/// the user could act on:
///
/// - `bold`: unread mail in the list, and emphasised chrome.
/// - `strike`: a cancelled calendar event.
/// - `bg:surface`: the mail-list cursor row fill (also the hint/status bars,
///   which share the raised surface).
/// - `bg:selection` / `fg:selection`: the calendar cursor row, and the
///   toggle-selected mail rows plus the highlighted sidebar entry.
fn cell_tags(style: ratatui::style::Style) -> String {
    let theme = theme::active();
    let mut tags: Vec<&str> = Vec::new();
    if style.bg == Some(theme.surface) {
        tags.push("bg:surface");
    }
    if style.bg == Some(theme.selection) {
        tags.push("bg:selection");
    }
    if style.fg == Some(theme.selection) {
        tags.push("fg:selection");
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        tags.push("bold");
    }
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        tags.push("strike");
    }
    tags.join("+")
}

/// Per-row runs of equal style tags, skipping untagged stretches.
fn style_runs(buffer: &Buffer) -> String {
    let area = *buffer.area();
    let mut out: Vec<String> = Vec::new();

    for y in 0..area.height {
        let mut runs: Vec<String> = Vec::new();
        let mut current: Option<(String, u16)> = None;

        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            // `Cell` exposes fg/bg/modifiers separately; rebuild the style so
            // the tagging works off one value.
            let style = ratatui::style::Style::default()
                .fg(cell.fg)
                .bg(cell.bg)
                .add_modifier(cell.modifier);
            let tags = cell_tags(style);
            match &current {
                Some((open, _)) if *open == tags => {}
                Some((open, start)) => {
                    if !open.is_empty() {
                        runs.push(format!("{open}@{start}..{x}"));
                    }
                    current = Some((tags, x));
                }
                None => current = Some((tags, x)),
            }
        }
        if let Some((open, start)) = current {
            if !open.is_empty() {
                runs.push(format!("{open}@{start}..{}", area.width));
            }
        }

        if !runs.is_empty() {
            out.push(format!("{y:02} |{}", runs.join(" ")));
        }
    }

    out.join("\n")
}

// ---------------------------------------------------------------------------
// Frozen fixture
// ---------------------------------------------------------------------------

fn mailbox(label: &str, icon: &'static str, kind: MailboxKind) -> MailboxInfo {
    MailboxInfo {
        label: label.to_string(),
        icon,
        id: label.to_lowercase(),
        kind,
        server_name: None,
    }
}

/// One frozen inbox entry. Every field is literal: no clock, no filesystem,
/// no store -- `row` is the `messages.id` the entry would have carried.
///
/// No body: since #0038 the entry does not carry one and the preview loads it
/// from the blob store when the cursor lands on the message. A fixture with no
/// store primes that memo by hand instead, so only the bodies of the rows the
/// frames actually park the cursor on exist ([`BODY_ROW_1`], [`BODY_ROW_2`]).
#[allow(clippy::too_many_arguments)]
fn email(
    row: i64,
    from: &str,
    subject: &str,
    date: &str,
    read: bool,
    has_attachments: bool,
    is_invite: bool,
) -> EmailEntry {
    EmailEntry {
        msg: Some(MessageRef::new(row)),
        draft_id: None,
        from: from.to_string(),
        to: "sylvain@example.org".to_string(),
        cc: None,
        subject: subject.to_string(),
        status: "inbox".to_string(),
        date_display: date.to_string(),
        date_sort: format!("{date}T09:00:00"),
        has_attachments,
        read,
        answered: false,
        forwarded: false,
        flagged: false,
        is_invite,
    }
}

/// The body of the invite the default frames preview (row 1).
const BODY_ROW_1: &str =
    "Hallo Sylvain,\n\nanbei der Plan f\u{fc}r die \u{dc}bergabe.\n\n> Bitte bis Freitag best\u{e4}tigen.\n\nGr\u{fc}\u{df}e\n";

/// The body of the row the selection frame parks the cursor on (row 2).
const BODY_ROW_2: &str = "Danke, sieht gut aus.\n\nEin Punkt bleibt offen: der `export`-Schritt.\n";

fn invite_frontmatter() -> EventFrontmatter {
    EventFrontmatter {
        uid: Some("fixture-uid-1".into()),
        method: Some("REQUEST".into()),
        sequence: 0,
        summary: Some("Bauprojekt Ubergabe".into()),
        start: Some("2026-08-03T09:00:00+02:00".into()),
        end: Some("2026-08-03T10:30:00+02:00".into()),
        location: Some("Raum 2.14, Arcisstrasse".into()),
        organizer: Some("planung@example.org".into()),
        rsvp: "needs-action".into(),
        recurrence: String::new(),
        attendees: Vec::new(),
    }
}

/// The frozen mail-view app: four mailboxes, five inbox entries covering a
/// read/unread mix, a unicode subject, an invite and an attachment, no
/// selection. The cursor sits on the invite so the right column shows the
/// event card and the unicode subject unclipped, while the unread entry below
/// still contributes its own bold run to the legend.
fn mail_fixture() -> App {
    let mut app = App::default_for_tests();

    app.mailboxes = vec![
        mailbox("Inbox", "\u{f0172}", MailboxKind::Inbox),
        mailbox("Drafts", "\u{f03eb}", MailboxKind::Drafts),
        mailbox("Sent", "\u{f046b}", MailboxKind::Sent),
        mailbox("Archive", "\u{f013c}", MailboxKind::Archive),
    ];
    app.mailbox_counts = vec![5, 2, 41, 128];
    app.active_mailbox = 0;
    app.sidebar_index = 0;

    let emails = vec![
        email(
            1,
            "Planung Muller <planung@example.org>",
            "Einladung: Baustellen\u{fc}bergabe \u{2014} \u{4f1a}\u{8b70} \u{2713}",
            "2026-07-28",
            false,
            true,
            true,
        ),
        email(
            2,
            "Anna Weber <anna.weber@example.com>",
            "Re: Statusbericht KW31",
            "2026-07-27",
            true,
            false,
            false,
        ),
        email(
            3,
            "scanner@example.net",
            "Scan 2026-07-26 (3 Seiten)",
            "2026-07-26",
            false,
            true,
            false,
        ),
        email(
            4,
            "TUM Newsletter <news@example.edu>",
            "Wochenr\u{fc}ckblick: sehr langer Betreff der garantiert abgeschnitten wird",
            "2026-07-24",
            true,
            false,
            false,
        ),
        email(
            5,
            "buchhaltung@example.org",
            "Rechnung 2026-0714",
            "2026-07-21",
            true,
            true,
            false,
        ),
    ];

    app.emails = Arc::new(emails);
    app.email_cache = vec![Some(Arc::clone(&app.emails)), None, None, None];
    app.rebuild_visible();
    app.list_index = 0;
    app.prime_preview_body(BODY_ROW_1);
    // The entry carries only the invite flag; the parsed event behind the
    // card is memoised for the selected row (#0038 item 6), and a fixture
    // with no store primes that memo the way it primes the body.
    app.prime_preview_invite(invite_frontmatter());
    app
}

/// The frozen calendar agenda: an accepted event, a cancelled one, and a
/// pending invitation, cursor on the first row.
fn calendar_fixture() -> App {
    let mut app = mail_fixture();
    app.view = View::Calendar;

    let event = |row: i64,
                 uid: &str,
                 summary: &str,
                 start: &str,
                 display: &str,
                 rsvp: &str,
                 cancelled: bool,
                 is_organizer: bool| CalendarEvent {
        msg: MessageRef::new(row),
        event: EventFrontmatter {
            uid: Some(uid.into()),
            method: Some("REQUEST".into()),
            sequence: 0,
            summary: Some(summary.into()),
            start: Some(format!("{start}+02:00")),
            end: None,
            location: Some("Raum 2.14".into()),
            organizer: Some("planung@example.org".into()),
            rsvp: rsvp.into(),
            recurrence: String::new(),
            attendees: Vec::new(),
        },
        subject: format!("Invitation: {summary}"),
        start_sort: start.to_string(),
        end_sort: String::new(),
        start_display: display.to_string(),
        is_organizer,
        cancelled,
    };

    app.calendar_view.loaded = true;
    app.calendar_view.events = vec![
        event(
            1,
            "cal-1",
            "Baustellen\u{fc}bergabe",
            "2026-08-03T07:00:00",
            "2026-08-03 09:00",
            "accepted",
            false,
            false,
        ),
        event(
            2,
            "cal-2",
            "Abgesagter Jour fixe",
            "2026-08-04T08:00:00",
            "2026-08-04 10:00",
            "accepted",
            true,
            false,
        ),
        event(
            3,
            "cal-3",
            "PhD Kolloquium",
            "2026-08-06T13:00:00",
            "2026-08-06 15:00",
            "needs-action",
            false,
            true,
        ),
    ];
    app.calendar_view.visible = vec![0, 1, 2];
    app.calendar_view.list_index = 0;
    app
}

// ---------------------------------------------------------------------------
// Golden frames
// ---------------------------------------------------------------------------

/// The default mail view: sidebar, list, headers, preview (with the invite
/// event card), hint and status lines.
#[test]
fn golden_mail_view() {
    let mut app = mail_fixture();
    assert_snapshot!(frame_snapshot(&mut app, WIDTH, HEIGHT));
}

/// The mail view with two rows toggle-selected: adds the checkbox column, the
/// selection foreground runs, and the `N SELECTED` hint-bar badge. Captured
/// separately because the selection changes the list layout, and the default
/// frame above is the one users see most.
#[test]
fn golden_mail_view_with_selection() {
    let mut app = mail_fixture();
    app.selection = HashSet::from([
        EntryKey::Msg(MessageRef::new(1)),
        EntryKey::Msg(MessageRef::new(3)),
    ]);
    // Cursor off the selection: the cursor fill and the selection foreground
    // are separate signals and must stay separable in the legend.
    app.list_index = 1;
    app.prime_preview_body(BODY_ROW_2);
    assert_snapshot!(frame_snapshot(&mut app, WIDTH, HEIGHT));
}

/// The mail view with the second status axis on screen (#TKT-0051): the same
/// fixture with one answered row and one forwarded row, so the marker column
/// carries three of its states at once (unread, answered, forwarded).
///
/// The fourth state, the blank marker of a plain read row, is not in this
/// frame: every already-read row here is mutated into a history glyph. Nor is
/// the precedence rule that matters most, unread outranking a history glyph,
/// which would render as an unread marker and so show nothing a frame can
/// tell apart. Both are pinned by the unit tests in `ui::list` instead; what
/// this frame is for is the glyphs' column geometry, which the default frame
/// above cannot show.
///
/// Captured separately from the default frame, which stays the read/unread
/// picture users see most, and which is what the pre-nuke parity capture was
/// recorded against.
#[test]
fn golden_mail_view_with_the_status_axis() {
    let mut app = mail_fixture();
    let mut emails = (*app.emails).clone();
    emails[1].answered = true;
    emails[3].forwarded = true;
    // Answered outranks forwarded on one row, which is the only precedence
    // rule a frame can show.
    emails[4].answered = true;
    emails[4].forwarded = true;
    app.emails = Arc::new(emails);
    app.email_cache = vec![Some(Arc::clone(&app.emails)), None, None, None];
    app.rebuild_visible();
    assert_snapshot!(frame_snapshot(&mut app, WIDTH, HEIGHT));
}

/// The calendar agenda plus the shared event card.
#[test]
fn golden_calendar_view() {
    let mut app = calendar_fixture();
    assert_snapshot!(frame_snapshot(&mut app, WIDTH, HEIGHT));
}

/// The help overlay floating over the dimmed mail view. Rendered straight
/// through `ui::view` with `Overlay::Help` set, exactly as the event loop
/// leaves the state after `?`; no event loop is needed.
#[test]
fn golden_help_overlay() {
    let mut app = mail_fixture();
    app.overlay = Overlay::Help;
    assert_snapshot!(frame_snapshot(&mut app, WIDTH, HEIGHT));
}

/// The oracle is only worth anything if it is reproducible: two renders of the
/// same fixture must be byte-identical. This catches a `now()` or a hash-order
/// dependency creeping into a renderer without waiting for a snapshot review
/// to notice.
#[test]
fn frames_are_reproducible() {
    for build in [
        mail_fixture as fn() -> App,
        calendar_fixture as fn() -> App,
    ] {
        let first = frame_snapshot(&mut build(), WIDTH, HEIGHT);
        let second = frame_snapshot(&mut build(), WIDTH, HEIGHT);
        assert_eq!(first, second, "frame render is not deterministic");
    }
}

/// Guards the legend itself. A tag table that silently stopped tagging would
/// make every future frame diff look clean, so pin the three states the
/// legend exists for.
#[test]
fn legend_tags_the_states_it_claims_to() {
    pin_theme();
    let theme = theme::active();
    assert_eq!(cell_tags(ratatui::style::Style::default()), "");
    assert_eq!(
        cell_tags(
            ratatui::style::Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD)
        ),
        "bold"
    );
    assert_eq!(
        cell_tags(
            ratatui::style::Style::default()
                .bg(theme.surface)
                .fg(theme.selection)
        ),
        "bg:surface+fg:selection"
    );
    assert_eq!(
        cell_tags(ratatui::style::Style::default().bg(theme.selection)),
        "bg:selection"
    );
    // A colour the legend has no opinion about stays untagged.
    assert_eq!(
        cell_tags(ratatui::style::Style::default().fg(Color::Rgb(1, 2, 3))),
        ""
    );
}
