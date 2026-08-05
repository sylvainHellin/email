//! Calendar view rendering (#0034): a local-first agenda list + the shared
//! event card, over the events the iMIP traffic already produced on disk.
//!
//! Layout mirrors [`super::contacts`]: given a single content `area`, split it
//! into an agenda column and a detail pane, dropping the detail pane when the
//! area is too narrow for two columns.
//!
//! The list pane carries a permanent caveat line: this agenda is built purely
//! from invitation emails, so an event created directly in Outlook (never
//! emailed to us) is invisible until the Graph sync backend lands (#0036).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::super::app::{App, CalendarEvent};
use super::super::theme;
use super::util::display_width;

/// Below this content width we drop the detail pane and show the list only.
const DETAIL_MIN_WIDTH: u16 = 56;

/// Width of the date/time column (`YYYY-MM-DD HH:MM`).
const WHEN_WIDTH: usize = 16;

/// Width of the status/RSVP badge column.
const STATUS_WIDTH: usize = 12;

/// The honest limitation of a purely local, iMIP-derived calendar (#0034,
/// acceptance criterion 3). Shown in the list pane so it is visible without a
/// selection.
const OUTLOOK_CAVEAT: &str =
    "Only events that arrived by email are shown; Outlook-created events need Graph sync (#0036).";

/// Render the whole Calendar view into `area`. Splits into agenda + detail when
/// wide enough; agenda-only otherwise.
pub(super) fn render_calendar(app: &App, frame: &mut Frame, area: Rect) {
    if area.width >= DETAIL_MIN_WIDTH {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        render_list(app, frame, cols[0]);
        render_detail(app, frame, cols[1]);
    } else {
        render_list(app, frame, area);
    }
}

/// The agenda column: the event rows with the cursor highlighted, plus the
/// pinned Outlook-blindness caveat at the bottom.
fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let cv = &app.calendar_view;
    let scope = if cv.show_past { "all" } else { "upcoming" };
    let title = format!(" Calendar ({} {}) ", cv.visible.len(), scope);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().border))
        .style(Style::default().bg(theme::active().bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // Reserve exactly the rows the caveat wraps into, so it is never clipped
    // on a narrow pane (and never steals rows from the agenda on a wide one).
    let caveat_height = caveat_rows(inner.width, inner.height);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(caveat_height)])
        .split(inner);
    let list_area = rows[0];
    let caveat_area = rows[1];

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            OUTLOOK_CAVEAT,
            Style::default().fg(theme::active().text_muted),
        )))
        .wrap(Wrap { trim: true })
        .style(Style::default().bg(theme::active().bg)),
        caveat_area,
    );

    if cv.visible.is_empty() {
        let msg = if cv.events.is_empty() {
            "No invitations found on disk \u{2014} press r to refresh."
        } else {
            "No upcoming events \u{2014} press t to include past ones."
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme::active().text_muted),
            )))
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(theme::active().bg)),
            list_area,
        );
        return;
    }

    let title_w = (list_area.width as usize)
        .saturating_sub(WHEN_WIDTH + STATUS_WIDTH + 4)
        .max(4);
    let table_rows: Vec<Row> = cv
        .visible
        .iter()
        .filter_map(|&i| cv.events.get(i))
        .map(|ev| {
            let when = if ev.start_display.is_empty() {
                "(undated)".to_string()
            } else {
                ev.start_display.clone()
            };
            let title = ev
                .event
                .summary
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| ev.subject.clone());
            let style = if ev.cancelled {
                Style::default()
                    .fg(theme::active().text_muted)
                    .add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default().fg(theme::active().text)
            };
            Row::new(vec![
                Span::styled(truncate_cells(&when, WHEN_WIDTH), style),
                Span::styled(truncate_cells(&title, title_w), style),
                Span::styled(
                    truncate_cells(&status_badge(ev), STATUS_WIDTH),
                    badge_style(ev),
                ),
            ])
        })
        .collect();

    let mut state = TableState::default();
    state.select(Some(cv.list_index));
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(WHEN_WIDTH as u16),
            Constraint::Min(4),
            Constraint::Length(STATUS_WIDTH as u16),
        ],
    )
    .row_highlight_style(
        Style::default()
            .bg(theme::active().selection)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("\u{25b8} ")
    .style(Style::default().bg(theme::active().bg));
    frame.render_stateful_widget(table, list_area, &mut state);
}

/// The detail pane: the shared event card (#0029) for the selected event, so
/// the calendar and the mail preview never drift apart.
fn render_detail(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Event ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().border))
        .style(Style::default().bg(theme::active().bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(event) = app.selected_event() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Select an event to see its details.",
                Style::default().fg(theme::active().text_muted),
            )))
            .style(Style::default().bg(theme::active().bg)),
            inner,
        );
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    if event.cancelled {
        lines.push(Line::from(Span::styled(
            "Cancelled by the organizer.",
            Style::default()
                .fg(theme::active().error)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.extend(super::preview::event_card_lines(
        &event.event,
        event.is_organizer,
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(theme::active().bg)),
        inner,
    );
}

/// Rows the caveat needs at `width`, clamped so it can never take more than
/// half the pane (on a very short pane the agenda keeps priority).
fn caveat_rows(width: u16, height: u16) -> u16 {
    if width == 0 || height == 0 {
        return 0;
    }
    let needed = wrapped_rows(OUTLOOK_CAVEAT, width).max(1);
    needed.min(height / 2).max(if height >= 2 { 1 } else { 0 })
}

/// Rows `text` occupies when a `Paragraph` with `Wrap { trim: true }` renders
/// it at `width` columns.
///
/// This mirrors ratatui's `WordWrapper`: greedy word packing on whitespace, one
/// separating space between words, and a word wider than the pane broken across
/// rows. A cell-count `div_ceil` is *not* equivalent -- it is the character
/// packing lower bound, so it under-reserves by a row whenever a word straddles
/// a boundary, and the caveat was then silently clipped mid-sentence (the
/// `(#0036)` reference vanished at 31 / 46 / 23 inner columns among others).
fn wrapped_rows(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let width = width as usize;
    let mut rows: u16 = 1;
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let mut w = display_width(word);
        if used > 0 {
            if used + 1 + w <= width {
                used += 1 + w;
                continue;
            }
            rows = rows.saturating_add(1);
        }
        // At a line start: a word wider than the pane is broken across rows.
        while w > width {
            w -= width;
            rows = rows.saturating_add(1);
        }
        used = w;
    }
    rows
}

/// The right-hand badge for an agenda row: cancellation first, then our own
/// RSVP state (or "organizer" for the invites we sent).
fn status_badge(event: &CalendarEvent) -> String {
    if event.cancelled {
        return "cancelled".to_string();
    }
    if event.is_organizer {
        return "organizer".to_string();
    }
    match event.event.rsvp.as_str() {
        "accepted" => "accepted".to_string(),
        "declined" => "declined".to_string(),
        "tentative" => "tentative".to_string(),
        _ => "no reply".to_string(),
    }
}

fn badge_style(event: &CalendarEvent) -> Style {
    if event.cancelled {
        return Style::default().fg(theme::active().error);
    }
    if event.is_organizer {
        return Style::default().fg(theme::active().text_faint);
    }
    match event.event.rsvp.as_str() {
        "accepted" => Style::default().fg(theme::active().success),
        "declined" => Style::default().fg(theme::active().error),
        "tentative" => Style::default().fg(theme::active().warning),
        _ => Style::default().fg(theme::active().text_muted),
    }
}

/// Truncate to a *display-cell* budget (not a char count): event summaries
/// routinely carry accented, CJK or emoji characters whose cells are wider
/// than one column, and a char-based budget overflows the column for those.
fn truncate_cells(s: &str, max_cells: usize) -> String {
    if display_width(s) <= max_cells {
        return s.to_string();
    }
    if max_cells == 0 {
        return String::new();
    }
    // Reserve one cell for the ellipsis.
    let budget = max_cells - 1;
    let mut out = String::new();
    let mut width = 0usize;
    for c in s.chars() {
        let w = display_width(&c.to_string());
        if width + w > budget {
            break;
        }
        out.push(c);
        width += w;
    }
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(rsvp: &str, is_organizer: bool, cancelled: bool) -> CalendarEvent {
        CalendarEvent {
            msg: crate::tui::app::MessageRef::new(1),
            event: crate::types::EventFrontmatter {
                uid: Some("uid-1".into()),
                method: Some("REQUEST".into()),
                sequence: 0,
                summary: Some("Planning".into()),
                start: Some("2026-08-01T09:00:00+00:00".into()),
                end: None,
                location: None,
                organizer: Some("org@example.com".into()),
                rsvp: rsvp.to_string(),
                recurrence: String::new(),
                attendees: Vec::new(),
            },
            subject: "Invitation: Planning".into(),
            start_sort: "2026-08-01T09:00:00".into(),
            end_sort: String::new(),
            start_display: "2026-08-01 09:00".into(),
            is_organizer,
            cancelled,
        }
    }

    /// Render the agenda pane at `width` x `height` and read back the text
    /// *inside* the block, as one whitespace-collapsed line.
    ///
    /// The border cells are excluded on purpose: including them splices the
    /// box-drawing glyphs into the flattened text and makes every `contains`
    /// assertion below fail for the wrong reason.
    fn rendered_inner_text(width: u16, height: u16) -> String {
        let mut app = crate::tui::app::App::default_for_tests();
        app.calendar_view.loaded = true;
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_list(&app, f, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        // The block is `Borders::ALL`, so the inner area is inset by one cell.
        (1..height.saturating_sub(1))
            .flat_map(|y| {
                (1..width.saturating_sub(1))
                    .map(move |x| (x, y))
                    .map(|(x, y)| buf[(x, y)].symbol().to_string())
                    .chain(std::iter::once(" ".to_string()))
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The caveat must render *whole*, ticket reference included, at every
    /// realistic pane width. `caveat_rows` used to reserve a cell-count
    /// `div_ceil` instead of a word-wrap count, which clipped the operative
    /// clause at inner widths 31, 46 and 23 (terminals of 113, 164 and 86
    /// columns): the pane read "Outlook-created events need" with the
    /// `Graph sync (#0036)` payload gone.
    #[test]
    fn caveat_renders_in_full_at_every_width() {
        // 33 / 48 / 25 are the panes whose inner width is 31 / 46 / 23.
        for pane_width in 20u16..=120 {
            let flat = rendered_inner_text(pane_width, 24);
            assert!(
                flat.contains(OUTLOOK_CAVEAT),
                "caveat clipped at pane width {pane_width}: {flat}"
            );
        }
    }

    /// The reserved height matches what ratatui's word wrapper actually needs,
    /// never takes more than half the pane, and degrades gracefully on tiny
    /// panes (where the agenda keeps priority over a complete caveat).
    #[test]
    fn caveat_rows_match_the_word_wrapper() {
        let cells = display_width(OUTLOOK_CAVEAT) as u16;
        // Wide pane: one row is enough, and the agenda keeps the rest.
        assert_eq!(caveat_rows(cells + 10, 20), 1);
        // Word wrapping needs strictly more rows than character packing at the
        // widths that were clipped -- that gap *is* the bug.
        for inner in [31u16, 46, 23] {
            assert!(
                wrapped_rows(OUTLOOK_CAVEAT, inner) > cells.div_ceil(inner),
                "inner width {inner} must need more than the div_ceil bound"
            );
        }
        // Greedy word packing, and a word longer than the pane is broken
        // across rows rather than dropped.
        assert_eq!(wrapped_rows("ab cd", 5), 1);
        assert_eq!(wrapped_rows("ab cd", 4), 2);
        assert_eq!(wrapped_rows("abcdefghij", 4), 3);
        // Never more than half the pane.
        assert!(caveat_rows(20, 6) <= 3);
        // Degenerate panes do not underflow.
        assert_eq!(caveat_rows(0, 10), 0);
        assert_eq!(caveat_rows(40, 0), 0);
        assert_eq!(caveat_rows(40, 1), 0);
    }

    #[test]
    fn badge_prefers_cancellation_then_organizer_then_rsvp() {
        assert_eq!(status_badge(&event("accepted", false, true)), "cancelled");
        // Cancelled *and* organizer: cancellation still wins. Without this row
        // the two branches can be swapped with the suite still green.
        assert_eq!(status_badge(&event("accepted", true, true)), "cancelled");
        assert_eq!(status_badge(&event("accepted", true, false)), "organizer");
        assert_eq!(status_badge(&event("accepted", false, false)), "accepted");
        assert_eq!(status_badge(&event("", false, false)), "no reply");
    }

    /// The detail pane renders the same card as the mail preview, including
    /// the honest no-Graph caveat.
    #[test]
    fn detail_reuses_the_shared_event_card() {
        let ev = event("accepted", false, false);
        let lines = super::super::preview::event_card_lines(&ev.event, ev.is_organizer);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("Planning"));
        assert!(text.contains("Organizer"));
        assert!(text.contains("no Graph in v1"));
    }

    /// Column budgets are in display cells: a CJK title (2 cells per char)
    /// must not overflow its column.
    #[test]
    fn truncate_cells_respects_wide_characters() {
        let wide = "\u{4f1a}\u{8b70}\u{4f1a}\u{8b70}\u{4f1a}\u{8b70}"; // 6 chars, 12 cells
        let out = truncate_cells(wide, 7);
        assert!(display_width(&out) <= 7, "got {} cells", display_width(&out));
        assert!(out.ends_with('\u{2026}'));
        // Short strings pass through untouched.
        assert_eq!(truncate_cells("ok", 8), "ok");
        // ASCII at an odd budget pins the one-cell ellipsis reserve, which the
        // 2-cell CJK step above cannot see (dropping the reserve renders one
        // cell past the column).
        assert_eq!(truncate_cells("abcdefgh", 4), "abc\u{2026}");
        assert_eq!(display_width(&truncate_cells("Sprint review", 8)), 8);
    }
}
