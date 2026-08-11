use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Frame;

use super::super::app::{App, SearchField, SearchOverlayFocus};
use super::super::theme;
use super::headers::header_line;
use super::util::truncate;

pub(super) fn render_search_overlay(app: &mut App, frame: &mut Frame, area: Rect) {
    let overlay_width = (area.width * 9 / 10)
        .max(40)
        .min(area.width.saturating_sub(4));
    let overlay_height = (area.height * 85 / 100)
        .max(15)
        .min(area.height.saturating_sub(2));

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(overlay_width)])
        .flex(Flex::Center)
        .split(area);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(overlay_height)])
        .flex(Flex::Center)
        .split(horizontal[0]);

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::active().bg)),
        area,
    );

    let overlay_area = vertical[0];

    let mailbox_label = &app.server_search_scope_label;

    let bottom_title = if let Some(ref status) = app.server_search_status {
        // A ParseError renders as three lines; the footer is one, so keep the
        // first (the message) and let the caret detail sit in the log. The
        // structured status strings are single-line already.
        let first = status.lines().next().unwrap_or(status);
        format!(" {} ", first)
    } else {
        " Tab/Shift+Tab: fields | Space: toggle | Enter: search | Esc: close ".to_string()
    };

    let border_color = theme::active().accent_alt;
    let block = Block::default()
        .title(format!(" Server Search ({}) ", mailbox_label))
        .title_bottom(Line::from(bottom_title).alignment(Alignment::Center))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(overlay_area);
    frame.render_widget(block, overlay_area);

    // The form is nine one-line rows; results (once any) sit below it.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(0)])
        .split(inner);

    let search_area = chunks[0];
    let results_area = chunks[1];

    render_search_form(app, frame, search_area);

    if app.server_search_results.is_empty() {
        let msg = if app.server_search_loading {
            "  Searching..."
        } else if app.search_form.is_blank() {
            "  Fill the form and press Enter to search"
        } else {
            "  No results"
        };
        let empty = Paragraph::new(msg).style(Style::default().fg(theme::active().text_muted));
        frame.render_widget(empty, results_area);
        return;
    }

    let show_right = results_area.width >= 60;
    if show_right {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(results_area);
        render_search_results_list(app, frame, cols[0]);

        let right_panels = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(0)])
            .split(cols[1]);
        render_search_result_headers(app, frame, right_panels[0]);
        render_search_result_body(app, frame, right_panels[1]);
    } else {
        render_search_results_list(app, frame, results_area);
    }
}

/// The Outlook-shape search form (#0086b): a scope toggle, four text fields,
/// two date fields, an attachment toggle, and the raw-grammar Advanced line.
/// A non-blank Advanced line greys the structured fields (it takes over).
fn render_search_form(app: &App, frame: &mut Frame, area: Rect) {
    let advanced_active = app.search_form.advanced_active();
    let focused = match app.server_search_focus {
        SearchOverlayFocus::Field(f) => Some(f),
        SearchOverlayFocus::List => None,
    };

    let order = [
        SearchField::Scope,
        SearchField::From,
        SearchField::To,
        SearchField::Subject,
        SearchField::Keywords,
        SearchField::After,
        SearchField::Before,
        SearchField::Attachment,
        SearchField::Advanced,
    ];

    for (i, field) in order.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let row = Rect {
            y: area.y + i as u16,
            height: 1,
            ..area
        };
        // A structured field is greyed (disabled) while Advanced is active;
        // Advanced itself is never greyed.
        let greyed = advanced_active && !matches!(field, SearchField::Advanced);
        render_search_field(app, frame, row, *field, focused == Some(*field), greyed);
    }
}

fn render_search_field(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    field: SearchField,
    is_focused: bool,
    greyed: bool,
) {
    let theme = theme::active();
    let label_style = if greyed {
        Style::default().fg(theme.text_faint)
    } else if is_focused {
        Style::default().fg(theme.emphasis).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_muted)
    };
    let value_style = if greyed {
        Style::default().fg(theme.text_faint)
    } else {
        Style::default().fg(theme.text)
    };

    let label = format!("{:>10}: ", field.label());
    let label_width = super::util::display_width(&label);

    // The two toggles show their state instead of a text cursor.
    let value_owned: String = match field {
        SearchField::Scope => app.search_form.scope.0.label().to_string(),
        SearchField::Attachment => {
            if app.search_form.attachment {
                "[x] has attachment".to_string()
            } else {
                "[ ] has attachment".to_string()
            }
        }
        SearchField::From => app.search_form.from.clone(),
        SearchField::To => app.search_form.to.clone(),
        SearchField::Subject => app.search_form.subject.clone(),
        SearchField::Keywords => app.search_form.keywords.clone(),
        SearchField::After => app.search_form.after.clone(),
        SearchField::Before => app.search_form.before.clone(),
        SearchField::Advanced => app.search_form.advanced.clone(),
    };

    let is_toggle = matches!(field, SearchField::Scope | SearchField::Attachment);
    let cursor_reserve = if is_focused && !is_toggle { 1 } else { 0 };
    let avail = (area.width as usize)
        .saturating_sub(label_width)
        .saturating_sub(cursor_reserve);
    let value_text = super::util::scrolled_input_value(&value_owned, avail);

    let mut spans = vec![
        Span::styled(label, label_style),
        Span::styled(value_text, value_style),
    ];
    // A focused text field shows a block cursor; a focused toggle shows a
    // caret marker so focus is still visible without a fake cursor.
    if is_focused && !is_toggle {
        spans.push(Span::styled(
            "\u{2588}",
            Style::default().fg(theme.accent_alt),
        ));
    } else if is_focused && is_toggle {
        spans.push(Span::styled(
            "  \u{25c0} space",
            Style::default().fg(theme.accent),
        ));
    }
    // A placeholder hint on an empty, unfocused Advanced line.
    if field == SearchField::Advanced && value_owned.is_empty() && !is_focused {
        spans.push(Span::styled(
            "from:x (a OR b) has:attachment …",
            Style::default().fg(theme.text_faint),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_search_results_list(app: &App, frame: &mut Frame, area: Rect) {
    let list_focus = app.server_search_focus == SearchOverlayFocus::List;
    let show_mailbox_col = app.server_search_scope_label == "All";

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(if list_focus {
            theme::active().accent_alt
        } else {
            theme::active().text_faint
        }));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let available_width = inner.width as usize;
    let date_width = 10;
    let mb_width = if show_mailbox_col { 8 } else { 0 };
    let contact_width = 15.min(available_width.saturating_sub(date_width + mb_width + 10));
    let subject_width = available_width.saturating_sub(
        date_width + mb_width + contact_width + 2 + if show_mailbox_col { 1 } else { 0 },
    );

    let mut header_cells =
        vec![Cell::from("DATE").style(Style::default().fg(theme::active().text_muted))];
    if show_mailbox_col {
        header_cells
            .push(Cell::from("MAILBOX").style(Style::default().fg(theme::active().text_muted)));
    }
    header_cells.push(Cell::from("CONTACT").style(Style::default().fg(theme::active().text_muted)));
    header_cells.push(Cell::from("SUBJECT").style(Style::default().fg(theme::active().text_muted)));
    let header = Row::new(header_cells);

    let rows: Vec<Row> = app
        .server_search_results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            let is_cursor = list_focus && i == app.server_search_index;
            let contact = truncate(&result.entry.from, contact_width);
            let subject_prefix = if result.entry.has_attachments {
                "\u{f0c6} "
            } else {
                ""
            };
            let subject = truncate(
                &format!("{}{}", subject_prefix, result.entry.subject),
                subject_width,
            );

            let style = if is_cursor {
                Style::default()
                    .bg(theme::active().surface)
                    .fg(theme::active().selection)
            } else {
                Style::default().fg(theme::active().text)
            };

            let mut cells = vec![Cell::from(result.entry.date_display.clone())];
            if show_mailbox_col {
                cells.push(
                    Cell::from(truncate(&result.source_label, mb_width))
                        .style(Style::default().fg(theme::active().text_faint)),
                );
            }
            cells.push(Cell::from(contact));
            cells.push(Cell::from(subject));

            Row::new(cells).style(style)
        })
        .collect();

    let mut constraints: Vec<Constraint> = vec![Constraint::Length(date_width as u16)];
    if show_mailbox_col {
        constraints.push(Constraint::Length(mb_width as u16));
    }
    constraints.push(Constraint::Length(contact_width as u16));
    constraints.push(Constraint::Min(subject_width as u16));

    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(1);

    let mut state = TableState::default();
    if list_focus {
        state.select(Some(app.server_search_index));
    }
    frame.render_stateful_widget(table, inner, &mut state);
}

fn render_search_result_headers(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Headers ")
        .borders(Borders::TOP | Borders::LEFT)
        .border_style(Style::default().fg(theme::active().text_faint))
        .style(Style::default().bg(theme::active().bg));

    let result = app.server_search_results.get(app.server_search_index);
    if result.is_none() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("  No email selected")
                .style(Style::default().fg(theme::active().text_muted)),
            inner,
        );
        return;
    }

    let entry = &result.unwrap().entry;
    let mut lines: Vec<Line> = vec![
        header_line("From", &entry.from),
        header_line("To", &entry.to),
    ];
    if let Some(ref cc) = entry.cc {
        if !cc.is_empty() {
            lines.push(header_line("Cc", cc));
        }
    }
    lines.push(header_line("Subject", &entry.subject));
    lines.push(header_line("Date", &entry.date_display));

    let content = Paragraph::new(lines)
        .block(block)
        .scroll((app.server_search_headers_scroll, 0));

    frame.render_widget(content, area);
}

fn render_search_result_body(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Body ")
        .borders(Borders::TOP | Borders::LEFT)
        .border_style(Style::default().fg(theme::active().text_faint))
        .style(Style::default().bg(theme::active().bg));

    let result = app.server_search_results.get(app.server_search_index);
    if result.is_none() {
        frame.render_widget(block, area);
        return;
    }

    // A server-search hit renders the body it was fetched with, not one from
    // the store: the hit may not be a local message at all (see
    // `helpers::fetched_to_email_entry`).
    let content = Paragraph::new(result.unwrap().fetched.body_text.as_str())
        .style(Style::default().fg(theme::active().text))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.server_search_scroll, 0));

    frame.render_widget(content, area);
}
