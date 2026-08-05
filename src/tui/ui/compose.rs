//! Compose wizard overlay: 4-field form (To/Cc/Bcc/Subject) with live
//! contact autocomplete under the focused address field.

use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState,
};
use ratatui::Frame;

use super::super::app::{App, ComposeField, ComposeMode, ComposeWizard};
use super::super::theme;

pub(super) fn render_compose_wizard(app: &mut App, frame: &mut Frame, area: Rect) {
    let Some(wizard) = app.compose_wizard() else {
        return;
    };

    let overlay_width = (area.width * 7 / 10)
        .max(50)
        .min(area.width.saturating_sub(4));
    let overlay_height = (area.height * 80 / 100)
        .max(18)
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

    // Dim and clear the background like the search overlay does.
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::active().bg)),
        area,
    );

    let overlay_area = vertical[0];

    let title = match &wizard.mode {
        ComposeMode::New => " Compose ".to_string(),
        ComposeMode::EditDraft { .. } => " Edit recipients ".to_string(),
    };

    let hint = if wizard.focus == ComposeField::Subject {
        " Enter: submit | Tab: prev field | Esc: cancel ".to_string()
    } else if !wizard.suggestions.is_empty() {
        " ↑↓/Ctrl+p/Ctrl+n: pick | Enter: accept | Tab: next field | Esc: cancel ".to_string()
    } else {
        " Tab: next field | Enter: next | Ctrl+g: submit | Esc: cancel ".to_string()
    };

    let block = Block::default()
        .title(title)
        .title_bottom(Line::from(hint).alignment(Alignment::Center))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().accent_alt))
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(overlay_area);
    frame.render_widget(block, overlay_area);

    // Layout: four field rows (2 lines each = label + input) then suggestions list.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    render_field(wizard, frame, chunks[0], ComposeField::To);
    render_field(wizard, frame, chunks[1], ComposeField::Cc);
    render_field(wizard, frame, chunks[2], ComposeField::Bcc);
    render_field(wizard, frame, chunks[3], ComposeField::Subject);

    // Separator line.
    let sep = Line::from(Span::styled(
        "─".repeat(chunks[4].width as usize),
        Style::default().fg(theme::active().text_faint),
    ));
    frame.render_widget(Paragraph::new(sep), chunks[4]);

    render_suggestions(wizard, frame, chunks[5]);
}

fn render_field(wizard: &ComposeWizard, frame: &mut Frame, area: Rect, field: ComposeField) {
    let is_focused = wizard.focus == field;
    let label_style = if is_focused {
        Style::default()
            .fg(theme::active().emphasis)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::active().text_muted)
    };

    let value = match field {
        ComposeField::To => &wizard.to,
        ComposeField::Cc => &wizard.cc,
        ComposeField::Bcc => &wizard.bcc,
        ComposeField::Subject => &wizard.subject,
    };

    let label = format!("{:>7}: ", field.label());
    // The label is ASCII, 9 display cells ("    To: " etc.). Reserve a cell for
    // the cursor block on the focused field so text never overwrites it.
    let label_width = super::util::display_width(&label);
    let cursor_reserve = if is_focused { 1 } else { 0 };
    let avail = (area.width as usize)
        .saturating_sub(label_width)
        .saturating_sub(cursor_reserve);

    // Append-only editing model: the cursor is always at the end of the field,
    // so scroll the window to keep the tail visible.
    let value_text = super::util::scrolled_input_value(value, avail);

    let mut spans = vec![
        Span::styled(label, label_style),
        Span::styled(value_text, Style::default().fg(theme::active().text)),
    ];
    if is_focused {
        spans.push(Span::styled(
            "\u{2588}",
            Style::default().fg(theme::active().accent_alt),
        ));
    }

    let label_row = Rect { height: 1, ..area };
    frame.render_widget(Paragraph::new(Line::from(spans)), label_row);
}

fn render_suggestions(wizard: &ComposeWizard, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }

    // Placeholder text when we can't show a list.
    if wizard.focus == ComposeField::Subject {
        let msg = Paragraph::new("  Press Enter to submit the draft (or Tab to go back)")
            .style(Style::default().fg(theme::active().text_muted));
        frame.render_widget(msg, area);
        return;
    }

    if wizard.contacts.is_none() {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "No contact cache for this account. Run ",
                Style::default().fg(theme::active().text_muted),
            ),
            Span::styled(
                "email contacts rebuild",
                Style::default().fg(theme::active().emphasis),
            ),
            Span::styled(
                " to enable autocomplete.",
                Style::default().fg(theme::active().text_muted),
            ),
        ]));
        frame.render_widget(msg, area);
        return;
    }

    if wizard.suggestions.is_empty() {
        let msg = Paragraph::new("  Type to filter contacts…")
            .style(Style::default().fg(theme::active().text_muted));
        frame.render_widget(msg, area);
        return;
    }

    let rows: Vec<Row> = wizard
        .suggestions
        .iter()
        .enumerate()
        .map(|(i, sug)| {
            let is_cursor = i == wizard.suggestion_idx;
            let marker = tier_marker(sug.tier);
            let marker_style = tier_marker_style(sug.tier);
            let name = if sug.display_name.is_empty() {
                Span::styled(
                    "(no name)".to_string(),
                    Style::default().fg(theme::active().text_faint),
                )
            } else {
                Span::styled(
                    sug.display_name.clone(),
                    Style::default().fg(theme::active().text),
                )
            };
            let addr = Span::styled(
                format!("<{}>", sug.address),
                Style::default().fg(theme::active().text_muted),
            );

            let cell_style = if is_cursor {
                Style::default().bg(theme::active().surface)
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(Line::from(Span::styled(marker, marker_style))),
                Cell::from(Line::from(name)),
                Cell::from(Line::from(addr)),
            ])
            .style(cell_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(45),
            Constraint::Percentage(55),
        ],
    )
    .column_spacing(1);

    let mut state = TableState::default();
    state.select(Some(wizard.suggestion_idx));
    frame.render_stateful_widget(table, area, &mut state);
}

fn tier_marker(tier: u8) -> &'static str {
    match tier {
        2 => "\u{f0012}", // nf-md-account_check
        1 => "\u{f0016}", // nf-md-account_multiple
        _ => "\u{f01f0}", // nf-md-email
    }
}

fn tier_marker_style(tier: u8) -> Style {
    match tier {
        2 => Style::default().fg(theme::active().success),
        1 => Style::default().fg(theme::active().accent_alt),
        _ => Style::default().fg(theme::active().accent),
    }
}
