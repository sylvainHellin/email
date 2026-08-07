//! Contacts view rendering (#0033): read-only list + fuzzy search + detail
//! pane over the local contacts index (`crate::contacts`).
//!
//! Layout follows the mail spirit and the three width tiers. Given a single
//! content `area`, this splits it into a list column (with an incremental
//! fuzzy-search input line on top) and a detail pane for the selected contact.
//! When the area is too narrow for two columns, only the list is shown.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::super::app::App;
use super::super::theme;
use super::util::truncate;

/// Below this content width we drop the detail pane and show the list only.
const DETAIL_MIN_WIDTH: u16 = 56;

/// Render the whole Contacts view into `area`. Splits into list + detail when
/// wide enough; list-only otherwise. Used by the single-area tiers (medium and
/// narrow); the wide tier drives the two columns directly via
/// [`render_contacts_split`].
pub(super) fn render_contacts(app: &App, frame: &mut Frame, area: Rect) {
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

/// Wide-tier entry point: the ranked list owns the left column and the detail
/// pane the right one, so the view fills the frame the way the Mail list +
/// preview do, instead of leaving the old blank left-middle slot beside a
/// cramped detail pane (#TKT-0048).
pub(super) fn render_contacts_split(
    app: &App,
    frame: &mut Frame,
    list_area: Rect,
    detail_area: Rect,
) {
    render_list(app, frame, list_area);
    render_detail(app, frame, detail_area);
}

/// The list column: a title, a fuzzy-search input line, then the ranked
/// contact list with the cursor highlighted.
fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let cv = &app.contacts_view;
    let border = if cv.searching {
        theme::active().border_focused
    } else {
        theme::active().border
    };
    let title = format!(" Contacts ({}) ", cv.matches.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(theme::active().bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    let search_area = rows[0];
    let list_area = rows[1];

    // Search input line.
    let cursor = if cv.searching { "_" } else { "" };
    let (prompt_style, query_style) = if cv.searching {
        (
            Style::default().fg(theme::active().accent).add_modifier(Modifier::BOLD),
            Style::default().fg(theme::active().text),
        )
    } else {
        (
            Style::default().fg(theme::active().text_faint),
            Style::default().fg(theme::active().text_muted),
        )
    };
    let cursor_reserve = super::util::display_width(cursor);
    let avail = (search_area.width as usize)
        .saturating_sub(2) // "/ " prompt
        .saturating_sub(cursor_reserve);
    let query_text = if cv.query.is_empty() && !cv.searching {
        "type / to search".to_string()
    } else {
        format!("{}{}", super::util::scrolled_input_value(&cv.query, avail), cursor)
    };
    let search_line = Line::from(vec![
        Span::styled("/ ", prompt_style),
        Span::styled(query_text, query_style),
    ]);
    frame.render_widget(
        Paragraph::new(search_line).style(Style::default().bg(theme::active().bg)),
        search_area,
    );

    // The list body.
    if cv.index.is_none() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "No contact cache yet — press r to build the index.",
            Style::default().fg(theme::active().text_muted),
        )))
        .style(Style::default().bg(theme::active().bg));
        frame.render_widget(msg, list_area);
        return;
    }
    if cv.matches.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "No matching contacts.",
            Style::default().fg(theme::active().text_muted),
        )))
        .style(Style::default().bg(theme::active().bg));
        frame.render_widget(msg, list_area);
        return;
    }

    let index = cv.index.as_ref().unwrap();
    let name_w = list_area.width.saturating_sub(2) as usize;
    let table_rows: Vec<Row> = cv
        .matches
        .iter()
        .map(|addr| {
            let contact = index.contacts.get(addr);
            let label = match contact {
                Some(c) if !c.display_name.trim().is_empty() => {
                    format!("{} <{}>", c.display_name, c.address)
                }
                _ => addr.clone(),
            };
            Row::new(vec![truncate(&label, name_w)])
        })
        .collect();

    let mut state = TableState::default();
    state.select(Some(cv.list_index));
    let table = Table::new(table_rows, [Constraint::Percentage(100)])
        .row_highlight_style(cursor_row_style())
        .style(Style::default().bg(theme::active().bg));
    frame.render_stateful_widget(table, list_area, &mut state);
}

/// The detail pane for the selected contact: available fields.
fn render_detail(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Details ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().border))
        .style(Style::default().bg(theme::active().bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(contact) = app.selected_contact() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Select a contact to see details.",
                Style::default().fg(theme::active().text_muted),
            )))
            .style(Style::default().bg(theme::active().bg)),
            inner,
        );
        return;
    };

    let label_style = Style::default().fg(theme::active().text_faint);
    let value_style = Style::default().fg(theme::active().text);
    let name = if contact.display_name.trim().is_empty() {
        "(no name)".to_string()
    } else {
        contact.display_name.clone()
    };
    let field = |label: &str, value: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{label:<11}"), label_style),
            Span::styled(value, value_style),
        ])
    };

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            name.clone(),
            Style::default()
                .fg(theme::active().heading)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        field("Email", contact.address.clone()),
        field(
            "Interactions",
            format!(
                "sent-to {}, cc {}, received {}",
                contact.sent_to, contact.sent_cc, contact.received
            ),
        ),
        field("First seen", short_date(&contact.first_seen)),
        field("Last seen", short_date(&contact.last_seen)),
        Line::from(""),
        Line::from(Span::styled(
            "Enter compose · v vCard · r refresh",
            Style::default().fg(theme::active().text_muted),
        )),
    ];
    // Clamp to the pane height.
    lines.truncate(inner.height as usize);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::active().bg)),
        inner,
    );
}

/// The cursor-row highlight, matching the Mail list convention (#TKT-0048): a
/// raised `surface` fill carrying the `selection` foreground, rather than the
/// former solid `selection` background the views each invented on their own.
fn cursor_row_style() -> Style {
    Style::default()
        .bg(theme::active().surface)
        .fg(theme::active().selection)
        .add_modifier(Modifier::BOLD)
}

/// Trim an RFC-3339 timestamp down to its `YYYY-MM-DD` date, or pass through
/// if it does not look like one.
fn short_date(ts: &str) -> String {
    ts.split('T').next().unwrap_or(ts).to_string()
}
