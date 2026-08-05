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

/// Build the subject-cell prefix: invite calendar badge (if any) followed by
/// the attachment paperclip (if any). Both, one, or neither may apply.
fn invite_and_attachment_prefix(email: &EmailEntry) -> String {
    let mut prefix = String::new();
    if email.is_invite {
        prefix.push_str(INVITE_GLYPH);
        prefix.push(' ');
    }
    if email.has_attachments {
        prefix.push_str("\u{f0c6} ");
    }
    prefix
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
    let title = if !app.search_query.is_empty() && app.focus != Focus::Search {
        if app.search_includes_body {
            format!(" {} (content search) ", app.active_label())
        } else {
            format!(" {} (filtered) ", app.active_label())
        }
    } else {
        format!(" {} ", app.active_label())
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let search_visible = app.focus == Focus::Search || !app.search_query.is_empty();
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
        let prefix = if app.search_includes_body { "\\" } else { "/" };
        let cursor_reserve = if app.focus == Focus::Search { 1 } else { 0 };
        let avail = (search_rect.width as usize)
            .saturating_sub(super::util::display_width(prefix))
            .saturating_sub(cursor_reserve);
        let value = super::util::scrolled_input_value(&app.search_query, avail);
        let mut spans = vec![
            Span::styled(prefix, Style::default().fg(theme::active().accent)),
            Span::styled(value, Style::default().fg(theme::active().text)),
        ];
        if app.focus == Focus::Search {
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

    // Unread indicator column is always present (2 chars)
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
                let is_in_selection = has_selection && email.msg.is_some_and(|m| app.selection.contains(&m));
                let contact = truncate(email.display_contact(app.active_kind()), contact_width);
                // Invite badge (calendar glyph) is distinct from the
                // attachment paperclip; an invite may also have attachments.
                let subject_prefix = invite_and_attachment_prefix(email);
                let subject = truncate(
                    &format!("{}{}", subject_prefix, email.subject),
                    subject_width,
                );

                let style = row_style(is_cursor, is_in_selection, email.read);

                let mut cells = Vec::new();
                if has_selection {
                    let icon = if is_in_selection {
                        Span::styled("\u{f0134}", Style::default().fg(theme::active().selection))
                    } else {
                        Span::styled("\u{f0131}", Style::default().fg(theme::active().text_faint))
                    };
                    cells.push(Cell::from(icon));
                }
                // Unread indicator
                let marker = if email.read {
                    Span::styled(" ", Style::default())
                } else {
                    Span::styled("\u{f444}", Style::default().fg(theme::active().unread))
                };
                cells.push(Cell::from(marker));
                cells.push(Cell::from(email.date_display.clone()));
                cells.push(Cell::from(contact));
                cells.push(Cell::from(subject));

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
                let is_in_selection = has_selection && email.msg.is_some_and(|m| app.selection.contains(&m));
                let subject_prefix = invite_and_attachment_prefix(email);
                let subject = truncate(
                    &format!("{}{}", subject_prefix, email.subject),
                    subject_width,
                );

                let style = row_style(is_cursor, is_in_selection, email.read);

                let mut cells = Vec::new();
                if has_selection {
                    let icon = if is_in_selection {
                        Span::styled("\u{f0134}", Style::default().fg(theme::active().selection))
                    } else {
                        Span::styled("\u{f0131}", Style::default().fg(theme::active().text_faint))
                    };
                    cells.push(Cell::from(icon));
                }
                // Unread indicator
                let marker = if email.read {
                    Span::styled(" ", Style::default())
                } else {
                    Span::styled("\u{f444}", Style::default().fg(theme::active().unread))
                };
                cells.push(Cell::from(marker));
                cells.push(Cell::from(email.date_display.clone()));
                cells.push(Cell::from(subject));

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
            from: "a".into(), to: "b".into(), cc: None,
            subject: "S".into(), status: "inbox".into(),
            date_display: "2026-07-01".into(), date_sort: "2026-07-01T00:00:00".into(),
            has_attachments: has_att, read: false,
            is_invite,
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
