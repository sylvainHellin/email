use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Frame;

use super::super::app::{App, SearchOverlayFocus};
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
        format!(" {} ", status)
    } else {
        " Enter: search | Tab: switch focus | Esc: close ".to_string()
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    let search_area = chunks[0];
    let results_area = chunks[1];

    render_search_input(app, frame, search_area);

    if app.server_search_results.is_empty() {
        let msg = if app.server_search_loading {
            "  Searching..."
        } else if app.server_search_query.is_empty() {
            "  Type a query and press Enter to search"
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

fn render_search_input(app: &App, frame: &mut Frame, area: Rect) {
    let input_focus = app.server_search_focus == SearchOverlayFocus::Input;

    let input_area = Rect { height: 1, ..area };
    let cursor_reserve = if input_focus { 1 } else { 0 };
    let avail = (input_area.width as usize)
        .saturating_sub(2) // "> " prompt
        .saturating_sub(cursor_reserve);
    let value = super::util::scrolled_input_value(&app.server_search_query, avail);
    let mut spans = vec![
        Span::styled("> ", Style::default().fg(theme::active().accent_alt)),
        Span::styled(value, Style::default().fg(theme::active().text)),
    ];
    if input_focus {
        spans.push(Span::styled(
            "\u{2588}",
            Style::default().fg(theme::active().accent_alt),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), input_area);

    if area.height >= 2 {
        let hints_area = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        let hints = Line::from(vec![
            Span::styled("from:", Style::default().fg(theme::active().accent)),
            Span::styled(" to:", Style::default().fg(theme::active().accent)),
            Span::styled(" subject:", Style::default().fg(theme::active().accent)),
            Span::styled(" body:", Style::default().fg(theme::active().accent)),
            Span::styled(" since:", Style::default().fg(theme::active().accent)),
            Span::styled(" before:", Style::default().fg(theme::active().accent)),
            Span::styled(" in:", Style::default().fg(theme::active().accent)),
            Span::styled(
                "  or free text",
                Style::default().fg(theme::active().text_faint),
            ),
        ]);
        frame.render_widget(Paragraph::new(hints), hints_area);
    }
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
