//! Multi-view chrome (#0033): the bottom-left view switcher.
//!
//! The switcher reuses the activity-log bottom-slot pattern from
//! [`super::sidebar::render_activity_log`]: a small bordered panel pinned to
//! the bottom of the left column. Each view renders its own content elsewhere
//! (`ui::list`/`ui::preview` for Mail, `ui::contacts`, `ui::calendar`).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use super::super::app::{App, View};
use super::super::theme;

/// Height (rows) the view switcher panel needs, including its border.
pub(super) const SWITCHER_HEIGHT: u16 = 3;

/// Render the bottom-left view switcher: `mail | contacts | calendar` chips
/// with the active view highlighted. Reuses the activity-log bottom-slot
/// pattern so the left column keeps its herdr-style stacked look.
pub(super) fn render_view_switcher(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" View ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().border))
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut spans: Vec<Span> = Vec::new();
    for (i, &v) in View::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " | ",
                Style::default().fg(theme::active().text_faint),
            ));
        }
        let chip = format!(" {} ", v.label());
        let style = if v == app.view {
            Style::default()
                .fg(theme::active().bg)
                .bg(theme::active().accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::active().text_muted)
        };
        spans.push(Span::styled(chip, style));
    }

    let content = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    frame.render_widget(content, inner);
}
