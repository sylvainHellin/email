use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::types::EventFrontmatter;

use super::super::app::{App, Focus, MailboxKind};
use super::super::theme;
use super::util::pane_border_style;

pub(super) fn render_body(app: &App, frame: &mut Frame, area: Rect) {
    let border_style = pane_border_style(app.focus, Focus::Preview);
    let block = Block::default()
        .title(" Body ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(theme::active().bg));

    if app.selected_email().is_none() {
        frame.render_widget(block, area);
        return;
    }

    // Event summary card at the top of the preview pane for invites (D3).
    // The parsed event comes from the memo the render pass just refreshed,
    // not from the list entry: the entry carries only the invite flag, and
    // the ics blob is parsed for the selected message alone (#0038 item 6).
    let body_area = if let Some(event) = app.preview_invite.event() {
        let is_sent = app.active_kind() == MailboxKind::Sent;
        let card_lines = event_card_lines(event, is_sent);
        // +2 for the card's own top/bottom border.
        let card_height = (card_lines.len() as u16 + 2).min(area.height.saturating_sub(3));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(card_height), Constraint::Min(0)])
            .split(area);
        render_event_card(card_lines, frame, chunks[0]);
        chunks[1]
    } else {
        area
    };

    // The body is not part of the entry: it is loaded from the blob store for
    // the selected message only and memoised in `App::preview_body`, refreshed
    // at the top of the render pass (#0038 scope item 5).
    let body = app.preview_body.text().replace("{{SIGNATURE}}", "[signature]");
    let inner_width = block.inner(body_area).width as usize;
    let lines: Vec<Line> = wrap_and_style_body(&body, inner_width);

    let content = Paragraph::new(lines)
        .block(block)
        .scroll((app.preview_scroll, 0));

    frame.render_widget(content, body_area);
}

/// Render the bordered event summary card into `area`.
fn render_event_card(lines: Vec<Line<'static>>, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" \u{f00ed} Invitation ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().accent))
        .style(Style::default().bg(theme::active().bg));
    let card = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(card, area);
}

/// Build the event summary card content (unit-testable): title, time range,
/// location, organizer, own RSVP state, per-attendee statuses, recurrence, and
/// the honest "not synced to Exchange" caveat. `is_sent` flips the framing for
/// our own sent invites (we are the organizer there).
pub(super) fn event_card_lines(event: &EventFrontmatter, is_sent: bool) -> Vec<Line<'static>> {
    let label_style = Style::default()
        .fg(theme::active().heading)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(theme::active().text);
    let muted = Style::default().fg(theme::active().text_muted);

    let field = |label: &str, value: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{label}: "), label_style),
            Span::styled(value, value_style),
        ])
    };

    let mut lines: Vec<Line> = Vec::new();

    if let Some(summary) = event.summary.as_deref() {
        lines.push(Line::from(Span::styled(
            summary.to_string(),
            Style::default()
                .fg(theme::active().accent)
                .add_modifier(Modifier::BOLD),
        )));
    }

    let when = match (event.start.as_deref(), event.end.as_deref()) {
        (Some(s), Some(e)) => Some(format!("{}  \u{2192}  {}", s, e)),
        (Some(s), None) => Some(s.to_string()),
        _ => None,
    };
    if let Some(when) = when {
        lines.push(field("When", when));
    }
    if !event.recurrence.is_empty() {
        lines.push(field("Repeats", event.recurrence.clone()));
    }
    if let Some(loc) = event.location.as_deref().filter(|s| !s.is_empty()) {
        lines.push(field("Where", loc.to_string()));
    }
    if let Some(org) = event.organizer.as_deref().filter(|s| !s.is_empty()) {
        lines.push(field("Organizer", org.to_string()));
    }

    // Own RSVP state (received invites only; on our sent invite we are the
    // organizer, so own-RSVP is not meaningful).
    if !is_sent {
        lines.push(Line::from(vec![
            Span::styled("Your RSVP: ", label_style),
            Span::styled(
                humanize_status(&event.rsvp),
                rsvp_style(&event.rsvp),
            ),
        ]));
    }

    if !event.attendees.is_empty() {
        lines.push(Line::from(Span::styled("Attendees:".to_string(), label_style)));
        for att in &event.attendees {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} — ", att.address), value_style),
                Span::styled(humanize_status(&att.status), rsvp_style(&att.status)),
            ]));
        }
    }

    // Honest caveat (D4): iMIP-only, no Exchange server-calendar sync in v1.
    let caveat = if is_sent {
        "Not synced to your Exchange calendar (no Graph in v1)."
    } else {
        "RSVP is emailed to the organizer; not synced to Exchange (no Graph in v1). Press V to respond."
    };
    lines.push(Line::from(Span::styled(caveat.to_string(), muted)));

    lines
}

/// Title-case a lowercase status vocabulary word for display.
fn humanize_status(status: &str) -> String {
    match status {
        "accepted" => "Accepted".to_string(),
        "declined" => "Declined".to_string(),
        "tentative" => "Tentative".to_string(),
        "needs-action" | "" => "No response yet".to_string(),
        other => other.to_string(),
    }
}

fn rsvp_style(status: &str) -> Style {
    match status {
        "accepted" => Style::default().fg(theme::active().success),
        "declined" => Style::default().fg(theme::active().error),
        "tentative" => Style::default().fg(theme::active().warning),
        _ => Style::default().fg(theme::active().text_muted),
    }
}

fn parse_quote_depth(line: &str) -> (usize, &str) {
    let trimmed = line.trim_start();
    let mut depth = 0;
    let mut pos = 0;
    let bytes = trimmed.as_bytes();
    while pos < bytes.len() && bytes[pos] == b'>' {
        depth += 1;
        pos += 1;
        if pos < bytes.len() && bytes[pos] == b' ' {
            pos += 1;
        }
    }
    (depth, &trimmed[pos..])
}

fn parse_inline_markdown(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    while i < len {
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                if !plain.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut plain), base_style));
                }
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(inner, base_style.add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }

        if chars[i] == '*' && (i + 1 >= len || chars[i + 1] != '*') {
            if let Some(end) = find_closing_single(&chars, i + 1, '*') {
                if !plain.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut plain), base_style));
                }
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(
                    inner,
                    base_style.add_modifier(Modifier::ITALIC),
                ));
                i = end + 1;
                continue;
            }
        }

        if chars[i] == '`' {
            if let Some(end) = find_closing_single(&chars, i + 1, '`') {
                if !plain.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut plain), base_style));
                }
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(
                    inner,
                    Style::default()
                        .fg(theme::active().code)
                        .bg(theme::active().surface),
                ));
                i = end + 1;
                continue;
            }
        }

        if chars[i] == '[' {
            if let Some((link_text, end)) = parse_markdown_link(&chars, i) {
                if !plain.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut plain), base_style));
                }
                spans.push(Span::styled(
                    link_text,
                    Style::default()
                        .fg(theme::active().accent)
                        .add_modifier(Modifier::UNDERLINED),
                ));
                i = end;
                continue;
            }
        }

        plain.push(chars[i]);
        i += 1;
    }

    if !plain.is_empty() {
        spans.push(Span::styled(plain, base_style));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }

    spans
}

fn find_closing(chars: &[char], start: usize, delim: &[char; 2]) -> Option<usize> {
    let len = chars.len();
    let mut i = start;
    while i + 1 < len {
        if chars[i] == delim[0] && chars[i + 1] == delim[1] && i > start {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_closing_single(chars: &[char], start: usize, delim: char) -> Option<usize> {
    (start..chars.len()).find(|&i| chars[i] == delim && i > start)
}

fn parse_markdown_link(chars: &[char], start: usize) -> Option<(String, usize)> {
    let len = chars.len();
    let mut i = start + 1;
    while i < len && chars[i] != ']' {
        i += 1;
    }
    if i >= len {
        return None;
    }
    let text: String = chars[start + 1..i].iter().collect();
    i += 1;
    if i >= len || chars[i] != '(' {
        return None;
    }
    let _paren_start = i + 1;
    while i < len && chars[i] != ')' {
        i += 1;
    }
    if i >= len {
        return None;
    }
    if text.is_empty() {
        return None;
    }
    Some((text, i + 1))
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let mut level = 0;
    for ch in trimmed.chars() {
        if ch == '#' {
            level += 1;
        } else {
            break;
        }
    }
    if level > 0 && level <= 6 {
        let rest = trimmed[level..].trim_start();
        if !rest.is_empty() || trimmed.len() > level {
            return Some((level, rest));
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < 3 {
        return false;
    }
    let ch = trimmed.chars().next().unwrap();
    if ch == '-' || ch == '*' || ch == '_' {
        return trimmed.chars().all(|c| c == ch || c == ' ');
    }
    false
}

fn parse_list_item(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let pad: String = " ".repeat(indent);

    if trimmed.len() >= 2 {
        let first = trimmed.as_bytes()[0];
        if (first == b'-' || first == b'+') && trimmed.as_bytes()[1] == b' ' {
            return Some((format!("{}\u{2022} ", pad), &trimmed[2..]));
        }
    }

    let mut num_end = 0;
    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            num_end += 1;
        } else {
            break;
        }
    }
    if num_end > 0 && trimmed.len() > num_end + 1 {
        let rest = &trimmed[num_end..];
        if let Some(stripped) = rest.strip_prefix(". ") {
            let number = &trimmed[..num_end];
            return Some((format!("{}{}. ", pad, number), stripped));
        }
    }

    None
}

fn wrap_and_style_body<'a>(body: &'a str, width: usize) -> Vec<Line<'a>> {
    let mut result: Vec<Line> = Vec::new();
    let mut in_code_block = false;

    for line in body.lines() {
        if line.trim() == "[signature]" {
            result.push(Line::from(Span::styled(
                "  -- signature --".to_string(),
                Style::default().fg(theme::active().text_faint),
            )));
            continue;
        }

        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            result.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(theme::active().text_faint),
            )));
            continue;
        }

        if in_code_block {
            result.push(Line::from(Span::styled(
                line.to_string(),
                Style::default()
                    .fg(theme::active().code)
                    .bg(theme::active().surface),
            )));
            continue;
        }

        let (depth, content) = parse_quote_depth(line);

        if depth == 0 {
            if is_horizontal_rule(content) {
                let rule: String = "\u{2500}".repeat(width.min(40));
                result.push(Line::from(Span::styled(
                    rule,
                    Style::default().fg(theme::active().text_faint),
                )));
                continue;
            }

            if let Some((level, heading_text)) = parse_heading(content) {
                let style = match level {
                    1 => Style::default()
                        .fg(theme::active().heading)
                        .add_modifier(Modifier::BOLD),
                    2 => Style::default()
                        .fg(theme::active().accent)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default()
                        .fg(theme::active().accent_alt)
                        .add_modifier(Modifier::BOLD),
                };
                for wrapped in word_wrap(heading_text, width) {
                    result.push(Line::from(parse_inline_markdown(&wrapped, style)));
                }
                continue;
            }

            if let Some((prefix, item_content)) = parse_list_item(content) {
                let prefix_width = prefix.chars().count();
                let text_width = width.saturating_sub(prefix_width);
                let base_style = Style::default().fg(theme::active().text);
                if text_width < 5 {
                    let mut spans = vec![Span::styled(
                        prefix,
                        Style::default().fg(theme::active().accent),
                    )];
                    spans.extend(parse_inline_markdown(item_content, base_style));
                    result.push(Line::from(spans));
                } else {
                    let wrapped_lines = word_wrap(item_content, text_width);
                    for (i, wrapped) in wrapped_lines.iter().enumerate() {
                        let line_prefix = if i == 0 {
                            prefix.clone()
                        } else {
                            " ".repeat(prefix_width)
                        };
                        let mut spans = vec![Span::styled(
                            line_prefix,
                            Style::default().fg(theme::active().accent),
                        )];
                        spans.extend(parse_inline_markdown(wrapped, base_style));
                        result.push(Line::from(spans));
                    }
                }
                continue;
            }

            if is_attribution(content.trim()) {
                let style = Style::default()
                    .fg(theme::active().text_muted)
                    .add_modifier(Modifier::ITALIC);
                for wrapped in word_wrap(content, width) {
                    result.push(Line::from(Span::styled(wrapped, style)));
                }
            } else {
                let base_style = Style::default().fg(theme::active().text);
                for wrapped in word_wrap(content, width) {
                    result.push(Line::from(parse_inline_markdown(&wrapped, base_style)));
                }
            }
        } else {
            let prefix = "\u{2502} ".repeat(depth);
            let prefix_width = depth * 2;
            let text_width = width.saturating_sub(prefix_width);

            let is_attr = is_attribution(content.trim());
            let text_style = if is_attr {
                Style::default()
                    .fg(theme::active().text_muted)
                    .add_modifier(Modifier::ITALIC)
            } else {
                match depth {
                    1 => Style::default().fg(theme::active().text_faint),
                    _ => Style::default().fg(theme::active().surface),
                }
            };

            if text_width < 5 {
                let mut spans = vec![Span::styled(
                    prefix,
                    Style::default().fg(theme::active().accent),
                )];
                if is_attr {
                    spans.push(Span::styled(content.to_string(), text_style));
                } else {
                    spans.extend(parse_inline_markdown(content, text_style));
                }
                result.push(Line::from(spans));
            } else {
                for wrapped in word_wrap(content, text_width) {
                    let mut spans = vec![Span::styled(
                        prefix.clone(),
                        Style::default().fg(theme::active().accent),
                    )];
                    if is_attr {
                        spans.push(Span::styled(wrapped, text_style));
                    } else {
                        spans.extend(parse_inline_markdown(&wrapped, text_style));
                    }
                    result.push(Line::from(spans));
                }
            }
        }
    }

    result
}

fn is_attribution(line: &str) -> bool {
    line.starts_with("On ") && line.ends_with("wrote:")
}

fn word_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        let char_count = remaining.chars().count();
        if char_count <= width {
            lines.push(remaining.to_string());
            break;
        }

        let byte_at_width: usize = remaining
            .char_indices()
            .nth(width)
            .map_or(remaining.len(), |(i, _)| i);

        let break_at = remaining[..byte_at_width]
            .rfind(' ')
            .map(|i| i + 1)
            .unwrap_or(byte_at_width);

        let (chunk, rest) = remaining.split_at(break_at);
        lines.push(chunk.trim_end().to_string());
        remaining = rest.trim_start();
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod event_card_tests {
    use super::*;
    use crate::types::{EventAttendee, EventFrontmatter};

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn sample_event() -> EventFrontmatter {
        EventFrontmatter {
            uid: Some("u@x".to_string()),
            method: Some("REQUEST".to_string()),
            sequence: 0,
            summary: Some("LOC Day planning".to_string()),
            start: Some("2026-07-20T14:00:00+02:00".to_string()),
            end: Some("2026-07-20T15:00:00+02:00".to_string()),
            location: Some("Room 4.12".to_string()),
            organizer: Some("chair@tum.de".to_string()),
            rsvp: "accepted".to_string(),
            recurrence: "Weekly on Monday".to_string(),
            attendees: vec![
                EventAttendee { address: "a@example.com".to_string(), status: "accepted".to_string() },
                EventAttendee { address: "b@example.com".to_string(), status: "needs-action".to_string() },
            ],
        }
    }

    #[test]
    fn received_card_shows_all_fields_and_rsvp_and_caveat() {
        let text: String = event_card_lines(&sample_event(), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("LOC Day planning"));
        assert!(text.contains("2026-07-20T14:00:00+02:00"));
        assert!(text.contains("Room 4.12"));
        assert!(text.contains("chair@tum.de"));
        assert!(text.contains("Repeats: Weekly on Monday"));
        assert!(text.contains("Your RSVP: Accepted"), "text=\n{text}");
        assert!(text.contains("a@example.com"));
        assert!(text.contains("No response yet")); // b@ needs-action
        assert!(text.to_lowercase().contains("not synced to exchange"), "caveat missing:\n{text}");
        assert!(text.contains("Press V to respond"));
    }

    #[test]
    fn sent_card_omits_own_rsvp_and_uses_organizer_framing() {
        let text: String = event_card_lines(&sample_event(), true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        // On our own sent invite, own-RSVP is not shown (we are organizer).
        assert!(!text.contains("Your RSVP"), "text=\n{text}");
        assert!(text.contains("Not synced to your Exchange calendar"));
        assert!(!text.contains("Press V to respond"));
    }

    #[test]
    fn card_handles_minimal_event() {
        let ev = EventFrontmatter {
            uid: None, method: Some("REQUEST".to_string()), sequence: 0,
            summary: None, start: None, end: None, location: None,
            organizer: None, rsvp: "needs-action".to_string(),
            recurrence: String::new(), attendees: vec![],
        };
        let lines = event_card_lines(&ev, false);
        // Own RSVP + caveat still render even with no other fields.
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Your RSVP: No response yet"));
        assert!(!lines.is_empty());
    }

    /// The card renders the *derived* answer, not a stored one: with the sent
    /// REPLY in the store, the same fold the agenda runs turns the invite's
    /// `NEEDS-ACTION` into `Declined` on the card (#0038 scope item 6).
    #[test]
    fn the_card_shows_the_rsvp_derived_from_the_sent_reply() {
        use crate::reconcile;
        use crate::reconcile::tests::{fixture, reply_ics};

        let fx = fixture();
        let me = "me@example.com";
        fx.ingest_invite(
            "inbox",
            1,
            "Plan",
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
             UID:uid-card\r\nSEQUENCE:0\r\nSUMMARY:Plan\r\nDTSTART:20260801T090000Z\r\n\
             ORGANIZER:mailto:org@example.com\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        fx.ingest_invite(
            "sent",
            2,
            "Declined: Plan",
            &reply_ics("uid-card", 0, me, "DECLINED", "20260710T120000Z"),
        );

        // The same derivation `App::load_message_invite` performs.
        let invites = reconcile::load_invites(&fx.store, &fx.blobs, "alice");
        let replies = reconcile::fold_replies(&invites);
        let request = invites
            .iter()
            .find(|i| i.method() == "REQUEST")
            .expect("the REQUEST is in the store");
        let mut event = crate::calendar::event_frontmatter(&request.parsed);
        let by_addr = replies.get("uid-card");
        reconcile::apply_replies(&mut event, request.parsed.sequence, by_addr);
        event.rsvp = reconcile::own_rsvp(&event, me, by_addr);

        let text: String = event_card_lines(&event, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Your RSVP: Declined"), "text=\n{text}");
        assert!(text.contains("me@example.com \u{2014} Declined"), "text=\n{text}");
    }
}
