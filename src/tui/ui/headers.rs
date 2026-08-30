use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::super::app::{App, EmailEntry, Focus};
use super::super::theme;
use super::util::{display_width, pane_border_style};

/// Nerd Font paperclip: the header-pane attachment affordance, the same glyph
/// the list uses so a message that has attachments reads the same in both
/// places (#0096). Pairs with the `to`/`ts` open/save-attachment actions that
/// already work from the headers pane.
const ATTACHMENT_GLYPH: &str = "\u{f0c6}";

pub(super) fn header_line<'a>(label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!(" {label}: "),
            Style::default()
                .fg(theme::active().heading)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(value, Style::default().fg(theme::active().text)),
    ])
}

/// Rows one wrapped header line occupies at `width` cells, at least one. The
/// header pane wraps (`Wrap { trim: false }`), so the clamp math has to count
/// wrapped rows, not source lines, or a long `From`/`To` would let the scroll
/// run one row past the real bottom for every wrap.
fn wrapped_rows(line: &Line, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let cells: usize = line.spans.iter().map(|s| display_width(&s.content)).sum();
    (cells.div_ceil(width)).max(1) as u16
}

pub(super) fn render_headers(app: &mut App, frame: &mut Frame, area: Rect) {
    let border_style = pane_border_style(app.focus, Focus::Headers);
    let block = Block::default()
        .title(" Headers ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(area);

    // Own the header strings up front so the metadata lines (which borrow them)
    // no longer hold a borrow of `app`, leaving `app.headers_scroll` free to
    // clamp below.
    let Some(fields) = app.selected_email().map(HeaderFields::from_entry) else {
        app.headers_scroll = 0;
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("  No email selected")
                .style(Style::default().fg(theme::active().text_muted)),
            inner,
        );
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(header_line("From", &fields.from));
    if let Some(reply_to) = present(&fields.reply_to) {
        lines.push(header_line("Reply-To", reply_to));
    }
    lines.push(header_line("To", &fields.to));
    if let Some(cc) = present(&fields.cc) {
        lines.push(header_line("Cc", cc));
    }
    if let Some(bcc) = present(&fields.bcc) {
        lines.push(header_line("Bcc", bcc));
    }
    lines.push(header_line("Subj", &fields.subject));
    lines.push(header_line("Date", &fields.date_status));
    if let Some(attach) = &fields.attach {
        lines.push(header_line("Attach", attach));
    }

    // Bound the scroll against the wrapped content height, so `j` at the
    // bottom cannot push the metadata into an empty void (#0096). The renderer
    // owns the clamp because only here are the inner width (for wrapping) and
    // height known.
    let content_rows: u16 = lines.iter().map(|l| wrapped_rows(l, inner.width)).sum();
    let max_scroll = content_rows.saturating_sub(inner.height);
    app.headers_scroll = app.headers_scroll.min(max_scroll);

    let content = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.headers_scroll, 0));
    frame.render_widget(content, area);
}

/// A header value worth a line: `Some` only when the field is present and not
/// blank, so an empty `Cc`/`Bcc`/`Reply-To` never draws an empty row.
fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// The owned header strings one message contributes, copied out of the
/// selected [`crate::tui::app::EmailEntry`] so the render can drop its borrow
/// of `App` before clamping the scroll offset.
struct HeaderFields {
    from: String,
    reply_to: Option<String>,
    to: String,
    cc: Option<String>,
    bcc: Option<String>,
    subject: String,
    date_status: String,
    attach: Option<String>,
}

impl HeaderFields {
    fn from_entry(email: &EmailEntry) -> Self {
        HeaderFields {
            from: email.from.clone(),
            reply_to: email.reply_to.clone(),
            to: email.to.clone(),
            cc: email.cc.clone(),
            bcc: email.bcc.clone(),
            subject: email.subject.clone(),
            date_status: format!("{}  [{}]", email.date_display, email.status),
            attach: email
                .has_attachments
                .then(|| format!("{ATTACHMENT_GLYPH} yes")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::MessageRef;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use std::sync::Arc;

    /// An `App` whose selected message is `entry`, so `render_headers` has a
    /// message to draw.
    fn app_showing(entry: EmailEntry) -> App {
        let mut app = App::default_for_tests();
        app.emails = Arc::new(vec![entry]);
        app.visible = vec![0];
        app.list_index = 0;
        app
    }

    fn entry(reply_to: Option<&str>, bcc: Option<&str>, has_attachments: bool) -> EmailEntry {
        EmailEntry {
            msg: Some(MessageRef::new(1)),
            draft_id: None,
            skip: None,
            from: "Alice <alice@example.com>".to_string(),
            to: "Bob <bob@example.com>".to_string(),
            cc: None,
            reply_to: reply_to.map(str::to_string),
            bcc: bcc.map(str::to_string),
            subject: "Subject line".to_string(),
            status: "inbox".to_string(),
            date_display: "2026-08-14".to_string(),
            date_sort: "2026-08-14T09:00:00".to_string(),
            has_attachments,
            read: true,
            answered: false,
            forwarded: false,
            flagged: false,
            is_invite: false,
        }
    }

    /// The header pane's rendered glyphs as one string (rows joined by `\n`).
    fn render_text(app: &mut App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_headers(app, frame, Rect::new(0, 0, w, h)))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Reply-To and Bcc each earn their own labelled row when the message
    /// carries them (#0096 acceptance 1).
    #[test]
    fn reply_to_and_bcc_show_when_present() {
        let mut app = app_showing(entry(
            Some("noreply@example.com"),
            Some("blind@example.com"),
            false,
        ));
        let text = render_text(&mut app, 60, 14);
        assert!(text.contains("Reply-To:"), "missing Reply-To label:\n{text}");
        assert!(text.contains("noreply@example.com"), "missing Reply-To value:\n{text}");
        assert!(text.contains("Bcc:"), "missing Bcc label:\n{text}");
        assert!(text.contains("blind@example.com"), "missing Bcc value:\n{text}");
    }

    /// A message without those headers draws neither row: no empty labelled
    /// lines (#0096, `present` filter).
    #[test]
    fn reply_to_and_bcc_absent_when_missing() {
        let mut app = app_showing(entry(None, Some("   "), false));
        let text = render_text(&mut app, 60, 14);
        assert!(!text.contains("Reply-To:"), "unexpected Reply-To row:\n{text}");
        // A blank Bcc is treated as absent, not drawn as an empty row.
        assert!(!text.contains("Bcc:"), "blank Bcc must not draw a row:\n{text}");
    }

    /// A message with attachments shows the attachment affordance, matching the
    /// `to`/`ts` actions that already work in the pane (#0096 acceptance 2).
    #[test]
    fn attachment_affordance_shows_when_the_message_has_attachments() {
        let mut app = app_showing(entry(None, None, true));
        let text = render_text(&mut app, 60, 14);
        assert!(text.contains("Attach:"), "missing attachment row:\n{text}");
        assert!(text.contains(ATTACHMENT_GLYPH), "missing paperclip glyph:\n{text}");

        let mut plain = app_showing(entry(None, None, false));
        let plain_text = render_text(&mut plain, 60, 14);
        assert!(!plain_text.contains("Attach:"), "attachment row on plain mail:\n{plain_text}");
    }

    /// Header scroll is clamped to the content: when everything fits, `j` at
    /// the bottom cannot push the metadata into an empty void, so the offset
    /// is pinned back to zero (#0096 acceptance 3, the `saturating_add` defect).
    #[test]
    fn scroll_is_clamped_to_the_content_height() {
        let mut app = app_showing(entry(Some("r@example.com"), None, true));
        // Pretend the user held `j` far past the bottom.
        app.headers_scroll = 50;
        // A tall pane the few header rows fit inside: max_scroll is zero.
        let _ = render_text(&mut app, 60, 20);
        assert_eq!(app.headers_scroll, 0, "scroll must not run past the content");
    }

    /// With no message selected the scroll offset is reset rather than left
    /// pointing into a pane that now shows only the placeholder.
    #[test]
    fn scroll_resets_when_no_message_is_selected() {
        let mut app = App::default_for_tests();
        app.headers_scroll = 7;
        let _ = render_text(&mut app, 60, 10);
        assert_eq!(app.headers_scroll, 0);
    }
}
