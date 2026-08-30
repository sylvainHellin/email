use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::super::app::{App, EmailEntry, Focus};
use super::super::theme;
use super::util::{pane_border_style, truncate};

/// Nerd Font calendar glyph shown before a subject when the email carries an
/// iMIP invite (`event:` frontmatter). Distinct from the attachment paperclip
/// (`\u{f0c6}`); an invite may show both.
const INVITE_GLYPH: &str = "\u{f00ed}";

/// Nerd Font dot shown for a message the server has not marked `\Seen`.
const UNREAD_GLYPH: &str = "\u{f444}";

/// Nerd Font reply arrow: a reply to this message has gone out (#TKT-0051).
const ANSWERED_GLYPH: &str = "\u{f045a}";

/// Nerd Font forward arrow, the mirror of [`ANSWERED_GLYPH`]: this message has
/// been forwarded (#TKT-0051).
const FORWARDED_GLYPH: &str = "\u{f028d}";

/// Nerd Font filled flag shown before the subject of a starred message
/// (`\Flagged`, #0007). Rendered in the theme's `warning` colour so it reads
/// as the standout marker it is, orthogonally to the status glyph (a message
/// can be flagged and unread at once).
const FLAG_GLYPH: &str = "\u{f024}";

/// Nerd Font warning triangle shown before the filename of a draft the index
/// could not parse (#0080). The whole row is drawn in the theme's `error`
/// colour; the glyph names it as the broken file it is at a glance.
const SKIP_GLYPH: &str = "\u{f071}";

/// The one marker the status column shows for a message.
///
/// The column is two cells wide and the axis is a set (#TKT-0051), so three
/// booleans have to collapse into one glyph. The precedence is unread,
/// answered, forwarded, read:
///
/// - unread wins over everything, because new mail is the one thing the list
///   exists to surface; hiding it behind a history glyph would be a regression
///   of the read/unread axis the second one is supposed to sit beside;
/// - answered outranks forwarded, because a message you replied to is settled
///   and one you merely passed on may still be yours to answer;
/// - a read message with no history keeps the blank cell it always had.
fn status_marker(email: &EmailEntry) -> Span<'static> {
    if !email.read {
        Span::styled(UNREAD_GLYPH, Style::default().fg(theme::active().unread))
    } else if email.answered {
        Span::styled(ANSWERED_GLYPH, Style::default().fg(theme::active().success))
    } else if email.forwarded {
        Span::styled(
            FORWARDED_GLYPH,
            Style::default().fg(theme::active().accent_alt),
        )
    } else {
        Span::styled(" ", Style::default())
    }
}

/// The subject cell: the invite/attachment badges plus the truncated subject,
/// with a coloured flag glyph prepended when the message is starred (#0007).
///
/// The flag rides its own [`Span`] so it keeps the `warning` colour on a
/// cursor row, the same way the status marker keeps its colour; the badges and
/// subject stay in the row's own style. When flagged, the glyph plus its space
/// costs two columns, so the subject is truncated to what is left.
fn subject_cell(email: &EmailEntry, width: usize) -> Cell<'static> {
    let text = format!("{}{}", invite_and_attachment_prefix(email), email.subject);
    match flag_span(email) {
        Some(flag) => {
            let body = truncate(&text, width.saturating_sub(2));
            Cell::from(Line::from(vec![flag, Span::raw(" "), Span::raw(body)]))
        }
        None => Cell::from(truncate(&text, width)),
    }
}

/// The coloured flag glyph for a starred message, or `None` for an unflagged
/// one (#0007). Its own span so it keeps the `warning` colour on a cursor row.
fn flag_span(email: &EmailEntry) -> Option<Span<'static>> {
    email
        .flagged
        .then(|| Span::styled(FLAG_GLYPH, Style::default().fg(theme::active().warning)))
}

/// Build the subject-cell prefix: invite calendar badge (if any) followed by
/// the attachment paperclip (if any). Both, one, or neither may apply.
fn invite_and_attachment_prefix(email: &EmailEntry) -> String {
    let mut prefix = String::new();
    if email.skip.is_some() {
        // A parse-skipped draft carries neither invite nor attachment badge;
        // the warning triangle stands in for both and names the error row.
        prefix.push_str(SKIP_GLYPH);
        prefix.push(' ');
        return prefix;
    }
    if email.is_invite {
        prefix.push_str(INVITE_GLYPH);
        prefix.push(' ');
    }
    if email.has_attachments {
        prefix.push_str("\u{f0c6} ");
    }
    prefix
}

/// The style one list row is drawn in, error rows included.
///
/// A parse-skipped draft (#0080) is drawn in the theme's `error` colour so it
/// reads as the broken file it is; the cursor fill still wins on the selected
/// row, applied by the table's `row_highlight_style`, so the user can see
/// which row the keys act on. Every other row defers to [`row_style`].
fn list_row_style(email: &EmailEntry, is_cursor: bool, is_in_selection: bool) -> Style {
    if email.skip.is_some() {
        return Style::default().fg(theme::active().error);
    }
    row_style(is_cursor, is_in_selection, email.read)
}

/// Style for one list row.
///
/// The background fill is the cursor's alone: it is the only way to tell
/// which email the next keystroke acts on. Toggle-selected rows carry the
/// checked checkbox in the marker column plus the selection foreground, so a
/// multi-select stays visible without a second full-row highlight competing
/// with the cursor (a selected row the cursor had left used to keep its
/// background, leaving the focused email ambiguous).
fn row_style(is_cursor: bool, is_in_selection: bool, read: bool) -> Style {
    if is_cursor {
        Style::default()
            .bg(theme::active().surface)
            .fg(theme::active().selection)
    } else if is_in_selection {
        let style = Style::default().fg(theme::active().selection);
        if read {
            style
        } else {
            style.add_modifier(Modifier::BOLD)
        }
    } else if !read {
        Style::default()
            .fg(theme::active().text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::active().text_muted)
    }
}

pub(super) fn render_email_list(app: &App, frame: &mut Frame, area: Rect) {
    let border_style = pane_border_style(app.focus, Focus::List);
    // Two independent narrowings, so the title names whichever are on (#0079).
    let mut narrowings: Vec<&str> = Vec::new();
    if !app.search_query.is_empty() && app.focus != Focus::Search {
        narrowings.push("filtered");
    }
    if app.flagged_only {
        narrowings.push("flagged");
    }
    let title = if narrowings.is_empty() {
        format!(" {} ", app.active_label())
    } else {
        format!(" {} ({}) ", app.active_label(), narrowings.join(", "))
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The jump-to-date prompt (#0017) borrows the same one-line slot as the
    // search input: it is a transient input over the same list, only one of
    // the two can be armed (each owns the keyboard while it is), and giving it
    // its own row would move the list under the user for the two seconds a
    // date is typed.
    let search_visible =
        app.focus == Focus::Search || !app.search_query.is_empty() || app.jump_date_input.is_some();
    let (search_area, list_area) = if search_visible {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, inner)
    };

    if let Some(search_rect) = search_area {
        let jumping = app.jump_date_input.as_deref();
        let prefix = if jumping.is_some() { "date: " } else { "/" };
        let typed = jumping.unwrap_or(app.search_query.as_str());
        let cursor_reserve = if app.focus == Focus::Search || jumping.is_some() { 1 } else { 0 };
        let avail = (search_rect.width as usize)
            .saturating_sub(super::util::display_width(prefix))
            .saturating_sub(cursor_reserve);
        let value = super::util::scrolled_input_value(typed, avail);
        let mut spans = vec![
            Span::styled(prefix, Style::default().fg(theme::active().accent)),
            Span::styled(value, Style::default().fg(theme::active().text)),
        ];
        if app.focus == Focus::Search || jumping.is_some() {
            spans.push(Span::styled(
                "\u{2588}",
                Style::default().fg(theme::active().accent),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), search_rect);
    }

    if app.visible.is_empty() {
        let msg = if !app.search_query.is_empty() {
            "  No matching emails".to_string()
        } else if app.flagged_only {
            "  No flagged emails (press F to show all)".to_string()
        } else {
            format!(
                "\n  No emails in {}\n\n  Press f to fetch new emails",
                app.active_label()
            )
        };
        let empty = Paragraph::new(msg).style(Style::default().fg(theme::active().text_muted));
        frame.render_widget(empty, list_area);
        return;
    }

    let available_width = list_area.width as usize;
    let date_width = 10;
    let spacing = 3;

    let has_selection = !app.selection.is_empty();

    // Status marker column is always present (2 chars): unread, answered or
    // forwarded, one glyph at a time (#TKT-0051).
    let unread_col_width: usize = 2;

    if available_width > 45 {
        let checkbox_extra: usize = if has_selection { 3 } else { 0 };
        let effective_width = available_width.saturating_sub(checkbox_extra + unread_col_width + 1);
        let contact_width = 15.min(effective_width.saturating_sub(date_width + spacing + 10));
        let subject_width = effective_width.saturating_sub(date_width + contact_width + spacing);

        let mut header_cells = Vec::new();
        if has_selection {
            header_cells
                .push(Cell::from("").style(Style::default().fg(theme::active().text_muted)));
        }
        header_cells.push(Cell::from("").style(Style::default().fg(theme::active().text_muted)));
        header_cells
            .push(Cell::from("DATE").style(Style::default().fg(theme::active().text_muted)));
        header_cells
            .push(Cell::from("CONTACT").style(Style::default().fg(theme::active().text_muted)));
        header_cells
            .push(Cell::from("SUBJECT").style(Style::default().fg(theme::active().text_muted)));
        let header = Row::new(header_cells).height(1);

        let rows: Vec<Row> = app
            .visible_emails()
            .enumerate()
            .map(|(i, email)| {
                let is_cursor = i == app.list_index;
                let is_in_selection = has_selection && app.is_selected(email);
                let contact = truncate(email.display_contact(app.active_kind()), contact_width);

                let style = list_row_style(email, is_cursor, is_in_selection);

                let mut cells = Vec::new();
                if has_selection {
                    let icon = if is_in_selection {
                        Span::styled("\u{f0134}", Style::default().fg(theme::active().selection))
                    } else {
                        Span::styled("\u{f0131}", Style::default().fg(theme::active().text_faint))
                    };
                    cells.push(Cell::from(icon));
                }
                // Read/answered/forwarded indicator (#TKT-0051).
                cells.push(Cell::from(status_marker(email)));
                cells.push(Cell::from(email.date_display.clone()));
                cells.push(Cell::from(contact));
                // Invite/attachment badges, subject, and the flag star (#0007).
                cells.push(subject_cell(email, subject_width));

                Row::new(cells).style(style)
            })
            .collect();

        let mut constraints = Vec::new();
        if has_selection {
            constraints.push(Constraint::Length(2));
        }
        constraints.push(Constraint::Length(unread_col_width as u16));
        constraints.push(Constraint::Length(date_width as u16));
        constraints.push(Constraint::Length(contact_width as u16));
        constraints.push(Constraint::Min(subject_width as u16));

        let table = Table::new(rows, constraints)
            .header(header)
            .column_spacing(1)
            .row_highlight_style(
                Style::default()
                    .bg(theme::active().surface)
                    .fg(theme::active().selection)
                    .add_modifier(Modifier::BOLD),
            );

        let mut state = TableState::default();
        state.select(Some(app.list_index));
        frame.render_stateful_widget(table, list_area, &mut state);
    } else {
        let checkbox_extra: usize = if has_selection { 3 } else { 0 };
        let subject_width =
            available_width.saturating_sub(date_width + 2 + checkbox_extra + unread_col_width + 1);

        let mut header_cells = Vec::new();
        if has_selection {
            header_cells
                .push(Cell::from("").style(Style::default().fg(theme::active().text_muted)));
        }
        header_cells.push(Cell::from("").style(Style::default().fg(theme::active().text_muted)));
        header_cells
            .push(Cell::from("DATE").style(Style::default().fg(theme::active().text_muted)));
        header_cells
            .push(Cell::from("SUBJECT").style(Style::default().fg(theme::active().text_muted)));
        let header = Row::new(header_cells).height(1);

        let rows: Vec<Row> = app
            .visible_emails()
            .enumerate()
            .map(|(i, email)| {
                let is_cursor = i == app.list_index;
                let is_in_selection = has_selection && app.is_selected(email);

                let style = list_row_style(email, is_cursor, is_in_selection);

                let mut cells = Vec::new();
                if has_selection {
                    let icon = if is_in_selection {
                        Span::styled("\u{f0134}", Style::default().fg(theme::active().selection))
                    } else {
                        Span::styled("\u{f0131}", Style::default().fg(theme::active().text_faint))
                    };
                    cells.push(Cell::from(icon));
                }
                // Read/answered/forwarded indicator (#TKT-0051).
                cells.push(Cell::from(status_marker(email)));
                cells.push(Cell::from(email.date_display.clone()));
                // Subject with badges and the flag star (#0007).
                cells.push(subject_cell(email, subject_width));

                Row::new(cells).style(style)
            })
            .collect();

        let mut constraints = Vec::new();
        if has_selection {
            constraints.push(Constraint::Length(2));
        }
        constraints.push(Constraint::Length(unread_col_width as u16));
        constraints.push(Constraint::Length(date_width as u16));
        constraints.push(Constraint::Min(subject_width as u16));

        let table = Table::new(rows, constraints)
            .header(header)
            .column_spacing(1)
            .row_highlight_style(
                Style::default()
                    .bg(theme::active().surface)
                    .fg(theme::active().selection)
                    .add_modifier(Modifier::BOLD),
            );

        let mut state = TableState::default();
        state.select(Some(app.list_index));
        frame.render_stateful_widget(table, list_area, &mut state);
    }
}

#[cfg(test)]
mod badge_tests {
    use super::*;

    fn entry(is_invite: bool, has_att: bool) -> EmailEntry {
        EmailEntry {
            msg: Some(crate::tui::app::MessageRef::new(1)),
            draft_id: None,
            skip: None,
            from: "a".into(), to: "b".into(), cc: None,
            subject: "S".into(), status: "inbox".into(),
            date_display: "2026-07-01".into(), date_sort: "2026-07-01T00:00:00".into(),
            has_attachments: has_att, read: false, answered: false, forwarded: false,
            flagged: false, is_invite,
        }
    }

    #[test]
    fn invite_badge_precedes_attachment_paperclip() {
        let p = invite_and_attachment_prefix(&entry(true, true));
        assert!(p.starts_with(INVITE_GLYPH), "prefix={p:?}");
        assert!(p.contains('\u{f0c6}'), "paperclip missing: {p:?}");
    }

    #[test]
    fn no_badge_for_plain_email() {
        assert_eq!(invite_and_attachment_prefix(&entry(false, false)), "");
    }

    #[test]
    fn invite_only_has_calendar_no_paperclip() {
        let p = invite_and_attachment_prefix(&entry(true, false));
        assert!(p.starts_with(INVITE_GLYPH));
        assert!(!p.contains('\u{f0c6}'));
    }

    /// A parse-skipped draft (#0080) is drawn as an error row: the warning
    /// glyph stands in for the invite/attachment badges, and the row takes the
    /// theme's `error` colour whenever it is not the cursor row (the cursor
    /// fill wins by design). The badges never show beside the warning glyph.
    #[test]
    fn a_parse_skipped_draft_renders_as_an_error_row() {
        crate::tui::theme::init(crate::tui::theme::DEFAULT_THEME_NAME);
        let mut e = entry(true, true);
        e.msg = None;
        e.skip = Some(crate::store::drafts::SkippedDraft {
            path: std::path::PathBuf::from("/d/broken.md"),
            error: "boom".into(),
        });

        let prefix = invite_and_attachment_prefix(&e);
        assert!(prefix.starts_with(SKIP_GLYPH), "prefix={prefix:?}");
        assert!(!prefix.contains(INVITE_GLYPH), "no invite badge on an error row");
        assert!(!prefix.contains('\u{f0c6}'), "no paperclip on an error row");

        let off_cursor = list_row_style(&e, false, false);
        assert_eq!(off_cursor.fg, Some(theme::active().error));
    }

    /// The status column shows one glyph for a set of three (#TKT-0051), and
    /// the order is unread, answered, forwarded, read. Unread wins over the
    /// history glyphs because new mail is what the list exists to surface.
    #[test]
    fn the_status_marker_follows_the_precedence_unread_answered_forwarded_read() {
        let marker = |read, answered, forwarded| {
            let mut e = entry(false, false);
            e.read = read;
            e.answered = answered;
            e.forwarded = forwarded;
            status_marker(&e).content.to_string()
        };

        assert_eq!(marker(false, true, true), UNREAD_GLYPH);
        assert_eq!(marker(true, true, true), ANSWERED_GLYPH);
        assert_eq!(marker(true, false, true), FORWARDED_GLYPH);
        assert_eq!(marker(true, false, false), " ");
    }

    /// The flag star is orthogonal to the status axis (#0007): it shows for a
    /// starred message whether it is read or unread, in the theme's warning
    /// colour, and is absent otherwise.
    #[test]
    fn the_flag_star_shows_for_a_starred_message_in_its_own_colour() {
        let mut e = entry(false, false);
        assert!(flag_span(&e).is_none(), "an unflagged row has no star");
        e.flagged = true;
        let star = flag_span(&e).expect("a flagged row shows the star");
        assert_eq!(star.content, FLAG_GLYPH);
        assert_eq!(star.style.fg, Some(theme::active().warning));
        // Orthogonal to the read axis: still starred once read.
        e.read = true;
        assert!(flag_span(&e).is_some());
    }

    /// Each glyph carries its own colour slot, so a theme can tell the three
    /// states apart without reading the subject line.
    #[test]
    fn each_status_marker_carries_its_own_colour() {
        let mut e = entry(false, false);
        assert_eq!(status_marker(&e).style.fg, Some(theme::active().unread));
        e.read = true;
        e.forwarded = true;
        assert_eq!(status_marker(&e).style.fg, Some(theme::active().accent_alt));
        e.answered = true;
        assert_eq!(status_marker(&e).style.fg, Some(theme::active().success));
    }
}

#[cfg(test)]
mod row_style_tests {
    use super::*;

    /// The background fill marks the focused row and nothing else: a
    /// toggle-selected row the cursor has left used to keep it, so two rows
    /// looked equally focused and the target of the next keystroke was
    /// ambiguous.
    #[test]
    fn only_the_cursor_row_gets_a_background() {
        assert_eq!(
            row_style(true, false, true).bg,
            Some(theme::active().surface)
        );
        assert_eq!(
            row_style(true, true, true).bg,
            Some(theme::active().surface)
        );
        assert_eq!(row_style(false, true, true).bg, None);
        assert_eq!(row_style(false, true, false).bg, None);
        assert_eq!(row_style(false, false, true).bg, None);
        assert_eq!(row_style(false, false, false).bg, None);
    }

    /// Without a background, the selection foreground is what still sets a
    /// selected row apart from its unselected neighbours (on top of the
    /// checkbox glyph in the marker column).
    #[test]
    fn selected_rows_keep_the_selection_foreground_and_unread_bold() {
        assert_eq!(
            row_style(false, true, true).fg,
            Some(theme::active().selection)
        );
        assert!(!row_style(false, true, true)
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(
            row_style(false, true, false).fg,
            Some(theme::active().selection)
        );
        assert!(row_style(false, true, false)
            .add_modifier
            .contains(Modifier::BOLD));
    }

    /// Unselected rows are untouched by the fix: unread bold, read muted.
    #[test]
    fn unselected_rows_keep_their_read_state_styling() {
        let unread = row_style(false, false, false);
        assert_eq!(unread.fg, Some(theme::active().text));
        assert!(unread.add_modifier.contains(Modifier::BOLD));

        let read = row_style(false, false, true);
        assert_eq!(read.fg, Some(theme::active().text_muted));
        assert!(!read.add_modifier.contains(Modifier::BOLD));
    }
}
