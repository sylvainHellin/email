use html2text::render::RichAnnotation;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::StatefulImage;

use super::super::images;

use crate::types::EventFrontmatter;

use super::super::app::{App, BodyKey, Focus, MailboxKind};
use super::super::theme;
use super::util::pane_border_style;

pub(super) fn render_body(app: &mut App, frame: &mut Frame, area: Rect) {
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
    //
    // The wrapped/styled `Vec<Line>` product is memoised too (#0093): before
    // this, `wrap_and_style_body` re-parsed inline markdown and re-wrapped the
    // whole body on every frame -- every scroll keystroke and every idle tick.
    // The cache rebuilds only when the body content, the pane width, or the
    // inline-image set moved; a scroll then costs one clone of the visible
    // window instead of an O(body length) re-wrap.
    let inner = block.inner(body_area);
    let inner_width = inner.width;
    let epoch = app.preview_body.epoch();
    let images_key = app.preview_images.key().clone();
    if !app.preview_lines.holds(epoch, inner_width, &images_key) {
        // HTML-dominant mail (#0091): render the sender's own markup through
        // html2text's rich interface. The plain body for HTML mail is a lossy
        // flatten (html2text plain) re-parsed here as Markdown; rendering the
        // HTML directly keeps tables, lists, links and blockquotes structured.
        // Plain-only mail, drafts, and a render failure fall back to
        // `wrap_and_style_body` over the stored plain body, which is exactly
        // today's output -- the graceful degradation the ticket asks for.
        let mut lines = if let Some(html) = app.preview_html.rendered() {
            render_html_body(html, inner_width as usize)
        } else {
            // Drop the signature sentinel comments (#0106) so they never show
            // in the preview, then substitute the quote marker for display.
            let body = crate::draft::strip_signature_sentinels(app.preview_body.text())
                .replace("{{SIGNATURE}}", "[signature]");
            wrap_and_style_body(&body, inner_width as usize)
        };
        // Inline images (#0010) are appended to the text flow rather than
        // spliced into it: html2text gives no stable anchor for the `<img>` it
        // dropped, so the honest placement is a labelled block at the end of
        // the body, in message order. Each image contributes a `[image: name]`
        // line, and on a terminal that can draw pixels the rows it needs after
        // it, blank, which the graphics pass below paints over. The block is
        // part of the memoised product, keyed by the same image set.
        let placements = append_image_block(&app.preview_images, &mut lines, inner_width);
        app.preview_lines
            .fill(epoch, inner_width, images_key, lines, placements);
    }

    // Render just the scrolled window. The lines are pre-wrapped (one `Line`
    // per screen row), so slicing here is exactly what `.scroll()` did over the
    // full vector, at O(visible height) instead of O(body length).
    let visible = app
        .preview_lines
        .visible_slice(app.preview_scroll as usize, inner.height as usize);
    let placements = app.preview_lines.placements().to_vec();

    let content = Paragraph::new(visible).block(block);
    frame.render_widget(content, body_area);

    render_inline_images(app, frame, inner, &placements);
}

/// Where one inline image landed in the wrapped body: its index into
/// `app.preview_images` and the body line its first pixel row sits on.
#[derive(Clone)]
struct ImagePlacement {
    index: usize,
    line: usize,
    rows: u16,
}

/// Memoised wrapped/styled preview body (#0093).
///
/// [`wrap_and_style_body`] walks the whole body, parses inline markdown and
/// word-wraps it into one [`Line`] per screen row. Before #0093 that ran on
/// every frame, so every scroll keystroke and every idle tick re-did O(body
/// length) work. This slot holds the product (body lines plus the appended
/// inline-image block) and rebuilds it only when the body content, the pane
/// width, or the inline-image set changed. Rendering then clones just the
/// visible window, so a scroll costs O(visible height), not O(body length).
///
/// The key is `(body epoch, width, images key)`, all compared in O(1):
/// - **body content** -- `PreviewBody::epoch` bumps whenever the previewed
///   text or its identity changes, which covers a selection move, an async
///   body arrival (`prime_preview_body`), and a re-ingest under the same
///   cursor (the `mailbox_load_generation` inside the body key moves).
/// - **width** -- a terminal resize changes `inner_width` and forces a
///   re-wrap.
/// - **images** -- two neighbouring messages could share body text but differ
///   in inline images; the image memo's key discriminates them.
///
/// The theme is deliberately *not* part of the key: it is process-global and
/// fixed once at startup (`theme::init` over a `OnceLock`), so no in-session
/// change can stale the colors baked into these lines.
#[derive(Default)]
pub(crate) struct PreviewLinesCache {
    epoch: u64,
    width: u16,
    images_key: Option<BodyKey>,
    lines: Vec<Line<'static>>,
    placements: Vec<ImagePlacement>,
}

impl PreviewLinesCache {
    /// True when the cache already answers for this `(epoch, width, images)`
    /// tuple, so the render pass can skip the re-wrap.
    pub(crate) fn holds(&self, epoch: u64, width: u16, images_key: &Option<BodyKey>) -> bool {
        self.epoch == epoch && self.width == width && &self.images_key == images_key
    }

    /// Replace the cached product wholesale.
    fn fill(
        &mut self,
        epoch: u64,
        width: u16,
        images_key: Option<BodyKey>,
        lines: Vec<Line<'static>>,
        placements: Vec<ImagePlacement>,
    ) {
        self.epoch = epoch;
        self.width = width;
        self.images_key = images_key;
        self.lines = lines;
        self.placements = placements;
    }

    fn placements(&self) -> &[ImagePlacement] {
        &self.placements
    }

    /// The `height` rows starting at `scroll`, cloned. The lines are
    /// pre-wrapped, so one line is one row and this is exactly the window
    /// `Paragraph::scroll` would have shown over the full vector.
    fn visible_slice(&self, scroll: usize, height: usize) -> Vec<Line<'static>> {
        let total = self.lines.len();
        let start = scroll.min(total);
        let end = start.saturating_add(height).min(total);
        self.lines[start..end].to_vec()
    }

    #[cfg(test)]
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    #[cfg(test)]
    fn cached_epoch(&self) -> u64 {
        self.epoch
    }

    #[cfg(test)]
    fn cached_width(&self) -> u16 {
        self.width
    }
}

/// Append the `[image: name]` lines, and the blank rows a drawable image
/// needs, to `lines`.
///
/// Returns one placement per image the terminal can actually draw; on a
/// terminal without graphics the placeholder lines are still appended and the
/// returned list is empty, which is the pre-#0010 experience plus a name.
fn append_image_block<'a>(
    images: &images::PreviewImages,
    lines: &mut Vec<Line<'a>>,
    width: u16,
) -> Vec<ImagePlacement> {
    let mut placements = Vec::new();
    if images.is_empty() {
        return placements;
    }
    let muted = Style::default().fg(theme::active().text_muted);
    lines.push(Line::from(String::new()));
    for (index, image) in images.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            format!("[image: {}]", image.name),
            muted,
        )));
        if let Some(rows) = image.rows(width) {
            placements.push(ImagePlacement {
                index,
                line: lines.len(),
                rows,
            });
            for _ in 0..rows {
                lines.push(Line::from(String::new()));
            }
        }
    }
    placements
}

/// Where a reserved image block lands on screen, or `None` when it is
/// scrolled out of the pane or only half inside it.
fn placement_rect(placement: &ImagePlacement, scroll: u16, inner: Rect) -> Option<Rect> {
    let row = placement.line as i64 - i64::from(scroll);
    if row < 0 || row > i64::from(u16::MAX) {
        return None;
    }
    let area = Rect {
        x: inner.x,
        y: inner.y.saturating_add(row as u16),
        width: inner.width,
        height: placement.rows,
    };
    images::fits_within(area, inner).then_some(area)
}

/// Paint the images over the rows [`append_image_block`] reserved for them.
///
/// An image is drawn only when its whole block is inside the pane: a kitty or
/// sixel image is painted by the terminal over the cell grid, so a partially
/// scrolled one would spill past the border instead of clipping like text.
/// Scrolling it back into view redraws it; the widget re-encodes only when the
/// target rect changed, so a still frame costs nothing.
fn render_inline_images(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    placements: &[ImagePlacement],
) {
    if placements.is_empty() {
        return;
    }
    let mut rects: Vec<(usize, Rect)> = Vec::new();
    for placement in placements {
        if let Some(area) = placement_rect(placement, app.preview_scroll, inner) {
            rects.push((placement.index, area));
        }
    }
    if rects.is_empty() {
        return;
    }
    for (index, image) in app.preview_images.iter_mut().enumerate() {
        let Some((_, area)) = rects.iter().find(|(i, _)| *i == index) else {
            continue;
        };
        let Some(protocol) = image.protocol.as_mut() else {
            continue;
        };
        frame.render_stateful_widget(StatefulImage::default(), *area, protocol);
        if let Some(Err(e)) = protocol.last_encoding_result() {
            log::debug!("[images] encoding {} failed: {e}", image.name);
        }
    }
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

/// The "some occurrences were cancelled" line of a recurring event's card
/// (#0031): a count plus the first few `RECURRENCE-ID`s, so a long series does
/// not push the rest of the card off the pane.
fn cancelled_instances_line(instances: &[String]) -> String {
    const SHOWN: usize = 3;
    let head = instances
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let rest = instances.len().saturating_sub(SHOWN);
    let suffix = if rest > 0 {
        format!(", +{rest} more")
    } else {
        String::new()
    };
    format!(
        "{} occurrence(s) cancelled: {head}{suffix}",
        instances.len()
    )
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

    // Cancellation and supersession first: they change what every line below
    // them means (#0031). A cancelled invite is kept and shown, tombstoned --
    // never deleted out from under the user.
    if event.cancelled {
        lines.push(Line::from(Span::styled(
            "Cancelled by the organizer.".to_string(),
            Style::default()
                .fg(theme::active().error)
                .add_modifier(Modifier::BOLD),
        )));
    } else if event.superseded {
        lines.push(Line::from(Span::styled(
            "Superseded: a newer version of this invitation has arrived.".to_string(),
            Style::default()
                .fg(theme::active().warning)
                .add_modifier(Modifier::BOLD),
        )));
    }
    if !event.cancelled && !event.cancelled_instances.is_empty() {
        lines.push(Line::from(Span::styled(
            cancelled_instances_line(&event.cancelled_instances),
            Style::default().fg(theme::active().warning),
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
    let caveat = if event.cancelled {
        // No RSVP prompt on a dead version: `V` refuses it (#0031).
        "Cancelled invitations take no response; nothing was sent to Exchange (no Graph in v1)."
    } else if is_sent {
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

/// Render a message's HTML part into styled preview lines through html2text's
/// rich interface (#0091, Option B: no new dependency, html2text is already in
/// the tree).
///
/// This replaces the round-trip the plain path takes for HTML mail -- HTML
/// flattened to plain text at ingest, then re-parsed here as Markdown -- with a
/// single structured pass: html2text wraps the HTML to `width` and returns
/// per-span [`RichAnnotation`]s (emphasis, strong, links, code, CSS colours),
/// which [`style_for_annotations`] turns straight into ratatui styles. Tables,
/// nested lists, blockquotes and links come out as the sender laid them, not
/// reconstructed from guesswork.
///
/// Link footnotes are left off (the rich default), so links render as their
/// visible text, underlined; the full URL stays one keypress away behind the
/// `b`/`tb` open-in-browser escape hatch, unchanged by this ticket.
///
/// A render error (HTML html2text refuses) falls back to the plain pipeline
/// over [`crate::parse::html_to_plain`], so a bad message degrades to today's
/// output rather than blanking the pane.
fn render_html_body(html: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let rich = match html2text::config::rich()
        .use_doc_css()
        .no_table_borders()
        .no_link_wrapping()
        .lines_from_read(html.as_bytes(), width)
    {
        Ok(lines) => lines,
        Err(_) => return wrap_and_style_body(&crate::parse::html_to_plain(html), width),
    };

    let mut result: Vec<Line<'static>> = Vec::new();
    for tagged in &rich {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for ts in tagged.tagged_strings() {
            if ts.s.is_empty() {
                continue;
            }
            spans.push(Span::styled(ts.s.clone(), style_for_annotations(&ts.tag)));
        }
        // An empty span vector is a blank line html2text emitted between
        // blocks; keep it so paragraphs stay separated.
        result.push(Line::from(spans));
    }
    if result.is_empty() {
        result.push(Line::from(String::new()));
    }
    result
}

/// Fold html2text's rich annotations for one span into a ratatui [`Style`]
/// (#0091).
///
/// The "outer" annotation comes first in the slice, so inner ones layer on
/// top: a `<strong>` inside a link keeps the link's underline and adds bold.
/// Sender CSS colours (`Colour`/`BgColour`) are deliberately ignored (review
/// finding): they are authored against the sender's background, not the
/// terminal theme's, so a dark-mode email on a light terminal (or one that
/// sets only a background) can drop below readable contrast. Structure,
/// emphasis and links carry the meaning; the `b`/`tb` browser hatch shows the
/// sender's full styling. The enum is `#[non_exhaustive]`, hence the
/// catch-all arm.
fn style_for_annotations(tags: &[RichAnnotation]) -> Style {
    let mut style = Style::default().fg(theme::active().text);
    for tag in tags {
        style = match tag {
            RichAnnotation::Emphasis => style.add_modifier(Modifier::ITALIC),
            RichAnnotation::Strong => style.add_modifier(Modifier::BOLD),
            RichAnnotation::Strikeout => style.add_modifier(Modifier::CROSSED_OUT),
            RichAnnotation::Link(_) => style
                .fg(theme::active().accent)
                .add_modifier(Modifier::UNDERLINED),
            RichAnnotation::Code | RichAnnotation::Preformat(_) => {
                style.fg(theme::active().code).bg(theme::active().surface)
            }
            RichAnnotation::Image(_) => style.fg(theme::active().text_muted),
            RichAnnotation::Colour(_) | RichAnnotation::BgColour(_) => style,
            _ => style,
        };
    }
    style
}

fn wrap_and_style_body(body: &str, width: usize) -> Vec<Line<'static>> {
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

    pub(super) fn line_text(line: &Line) -> String {
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
            ..Default::default()
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

    /// #0031: the cancellation banner is part of the shared card, so the mail
    /// preview and the Calendar detail say the same thing, and the rest of the
    /// invite is still rendered below it (tombstone, not deletion).
    #[test]
    fn cancelled_card_leads_with_the_banner_and_keeps_the_event() {
        let mut ev = sample_event();
        ev.cancelled = true;
        let text: String = event_card_lines(&ev, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Cancelled by the organizer."), "text=\n{text}");
        assert!(text.contains("LOC Day planning"), "the event is still shown");
        assert!(text.contains("Room 4.12"));
        assert!(
            !text.contains("Press V to respond"),
            "a cancelled invite takes no RSVP:\n{text}"
        );
    }

    /// A superseded copy says so instead of pretending to be current, and the
    /// cancellation banner wins when both apply.
    #[test]
    fn superseded_card_says_a_newer_version_arrived() {
        let mut ev = sample_event();
        ev.superseded = true;
        let text: String = event_card_lines(&ev, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Superseded"), "text=\n{text}");

        ev.cancelled = true;
        let text: String = event_card_lines(&ev, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Cancelled by the organizer."));
        assert!(!text.contains("Superseded"), "one banner, not two:\n{text}");
    }

    /// A recurring series with individually cancelled occurrences lists them
    /// (truncated) instead of tombstoning the whole series.
    #[test]
    fn cancelled_occurrences_are_listed_on_the_series_card() {
        let mut ev = sample_event();
        ev.cancelled_instances = vec![
            "2026-07-27T12:00:00Z".into(),
            "2026-08-03T12:00:00Z".into(),
            "2026-08-10T12:00:00Z".into(),
            "2026-08-17T12:00:00Z".into(),
        ];
        let text: String = event_card_lines(&ev, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("4 occurrence(s) cancelled"), "text=\n{text}");
        assert!(text.contains("2026-07-27T12:00:00Z"));
        assert!(text.contains("+1 more"), "the list is truncated:\n{text}");
        assert!(!text.contains("Cancelled by the organizer."));
    }

    #[test]
    fn card_handles_minimal_event() {
        let ev = EventFrontmatter {
            uid: None, method: Some("REQUEST".to_string()), sequence: 0,
            summary: None, start: None, end: None, location: None,
            organizer: None, rsvp: "needs-action".to_string(),
            recurrence: String::new(), attendees: vec![],
            ..Default::default()
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

/// The inline-image half of the pane (#0010): what the text flow reserves and
/// where the graphics land. Rendering itself is the terminal's business; what
/// is testable offline is the geometry and the degradation, and both are here.
#[cfg(test)]
mod inline_image_tests {
    use super::event_card_tests::line_text;
    use super::*;

    // -----------------------------------------------------------------
    // Inline images (#0010)
    // -----------------------------------------------------------------

    /// A 2x1 red/blue PNG, decoded into a protocol below.
    fn tiny_image() -> image::DynamicImage {
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            40,
            20,
            image::Rgba([255, 0, 0, 255]),
        ))
    }

    /// A drawable image, as a terminal with a graphics protocol would produce
    /// it. The picker is built from a fixed cell size rather than a terminal
    /// query, so this test needs no tty and no `images::init`.
    fn drawable(name: &str) -> images::PreviewImage {
        // `halfblocks()` is the one picker constructor that needs no terminal;
        // it fixes the cell size at 10x20, which is what `cell_size()` falls
        // back to here, so the row arithmetic below is the production one.
        let picker = ratatui_image::picker::Picker::halfblocks();
        images::PreviewImage {
            name: name.to_string(),
            dimensions: Some((40, 20)),
            protocol: Some(picker.new_resize_protocol(tiny_image())),
        }
    }

    /// The name-only shape every terminal without graphics produces.
    fn placeholder_only(name: &str) -> images::PreviewImage {
        images::PreviewImage {
            name: name.to_string(),
            dimensions: Some((40, 20)),
            protocol: None,
        }
    }

    #[test]
    fn a_terminal_without_graphics_gets_one_placeholder_line_and_no_pixels() {
        let mut memo = images::PreviewImages::default();
        memo.fill(None, vec![placeholder_only("logo.png")]);
        let mut lines: Vec<Line> = vec![Line::from("body".to_string())];
        let placements = append_image_block(&memo, &mut lines, 60);
        assert!(placements.is_empty(), "nothing is drawn without a protocol");
        // Body, blank separator, placeholder. No reserved rows: the pane must
        // look exactly as it did before the image existed, plus the name.
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["body", "", "[image: logo.png]"]);
    }

    #[test]
    fn a_drawable_image_reserves_exactly_the_rows_it_needs() {
        let mut memo = images::PreviewImages::default();
        memo.fill(None, vec![drawable("logo.png")]);
        let mut lines: Vec<Line> = vec![Line::from("body".to_string())];
        let placements = append_image_block(&memo, &mut lines, 60);
        assert_eq!(placements.len(), 1);
        let rows = images::rows_for((40, 20), 60, images::cell_size());
        assert_eq!(placements[0].rows, rows);
        assert_eq!(placements[0].index, 0);
        // The reserved rows follow the placeholder line and are blank, so the
        // text under them is pushed down instead of being overpainted.
        assert_eq!(placements[0].line, 3);
        assert_eq!(lines.len(), 3 + rows as usize);
        assert!(lines[3..].iter().all(|l| line_text(l).is_empty()));
    }

    #[test]
    fn no_images_means_not_one_extra_line() {
        let memo = images::PreviewImages::default();
        let mut lines: Vec<Line> = vec![Line::from("body".to_string())];
        assert!(append_image_block(&memo, &mut lines, 60).is_empty());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn a_scrolled_image_is_drawn_only_while_it_fits_whole() {
        let inner = Rect::new(1, 1, 40, 10);
        let placement = ImagePlacement {
            index: 0,
            line: 6,
            rows: 4,
        };
        // Unscrolled: rows 7..11 of a pane that ends at 11. Fits.
        assert_eq!(
            placement_rect(&placement, 0, inner),
            Some(Rect::new(1, 7, 40, 4))
        );
        // One row up: it would start below the pane's last row.
        assert_eq!(placement_rect(&placement, 0, Rect::new(1, 1, 40, 9)), None);
        // Scrolled so the block sits in the middle of the pane.
        assert_eq!(
            placement_rect(&placement, 3, inner),
            Some(Rect::new(1, 4, 40, 4))
        );
        // Scrolled past the top: half of it would be above the pane.
        assert_eq!(placement_rect(&placement, 8, inner), None);
    }
}
/// The wrapped-body memo (#0093): the styled lines are built once per
/// `(body epoch, width, image set)` and rendered as a scrolled window, so a
/// scroll or an unrelated keypress no longer re-parses the whole body. What is
/// testable offline is the wrap product, the cache-key discipline, and the
/// windowing; all three are here.
#[cfg(test)]
mod preview_cache_tests {
    use super::event_card_tests::line_text;
    use super::*;

    #[test]
    fn wrapped_lines_are_owned_and_respect_the_width() {
        let body = "the quick brown fox jumps over the lazy dog";
        let lines = wrap_and_style_body(body, 10);
        assert!(lines.len() > 1, "a long line wraps to several rows");
        for line in &lines {
            let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(width <= 10, "row wider than the pane: {width}");
        }
    }

    #[test]
    fn cache_hits_only_on_matching_epoch_width_and_images() {
        let mut cache = PreviewLinesCache::default();
        let lines = wrap_and_style_body("hello world", 40);
        let n = lines.len();
        cache.fill(7, 40, None, lines, Vec::new());

        assert!(cache.holds(7, 40, &None), "same key hits");
        assert!(!cache.holds(8, 40, &None), "a new body epoch misses");
        assert!(!cache.holds(7, 41, &None), "a resize misses");
        assert_eq!(cache.line_count(), n);
        assert_eq!(cache.cached_epoch(), 7);
        assert_eq!(cache.cached_width(), 40);
    }

    #[test]
    fn visible_slice_is_the_scrolled_window_and_clamps() {
        let mut cache = PreviewLinesCache::default();
        let body = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = wrap_and_style_body(&body, 40);
        assert_eq!(lines.len(), 10, "ten short lines, one row each");
        cache.fill(1, 40, None, lines, Vec::new());

        let window = cache.visible_slice(2, 3);
        assert_eq!(window.len(), 3);
        assert_eq!(line_text(&window[0]), "line 2");
        assert_eq!(line_text(&window[2]), "line 4");

        // Scrolled past the end: an empty window, not a panic.
        assert!(cache.visible_slice(50, 5).is_empty());
        // A window taller than what remains is clamped to the remainder.
        assert_eq!(cache.visible_slice(8, 10).len(), 2);
    }
}

/// HTML-to-text rendering (#0091): the preview renders a message's own HTML
/// through html2text's rich interface instead of the lossy plain flatten, and
/// falls back to the plain pipeline when the HTML is absent or unparseable.
/// What is testable offline is the styled-line product and the annotation
/// mapping; both are here.
#[cfg(test)]
mod html_body_tests {
    use super::event_card_tests::line_text;
    use super::*;

    /// A borrow of every span across every rendered line, for style probing.
    fn all_spans<'a>(lines: &'a [Line<'static>]) -> Vec<&'a Span<'static>> {
        lines.iter().flat_map(|l| l.spans.iter()).collect()
    }

    #[test]
    fn html_structure_survives_where_the_markdown_wrap_would_flatten_it() {
        let html = "<h1>Title</h1><p>Hello <strong>bold</strong> and \
            <em>italic</em>.</p><ul><li>first</li><li>second</li></ul>";
        let text: String = render_html_body(html, 40)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Title"), "text=\n{text}");
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
        // The list items keep their own lines rather than running together.
        assert!(text.contains("first"), "text=\n{text}");
        assert!(text.contains("second"));
        assert!(text.contains('*'), "bullets are drawn, not stripped:\n{text}");
    }

    #[test]
    fn strong_and_emphasis_carry_their_modifiers() {
        let lines = render_html_body("<p><strong>b</strong> <em>i</em> plain</p>", 40);
        let spans = all_spans(&lines);
        let bold = spans
            .iter()
            .find(|s| s.content.as_ref() == "b")
            .expect("a span for the <strong> text");
        assert!(
            bold.style.add_modifier.contains(Modifier::BOLD),
            "strong is bold"
        );
        let ital = spans
            .iter()
            .find(|s| s.content.as_ref() == "i")
            .expect("a span for the <em> text");
        assert!(
            ital.style.add_modifier.contains(Modifier::ITALIC),
            "em is italic"
        );
    }

    #[test]
    fn a_link_is_underlined_accent() {
        let html = r#"<p>see <a href="https://example.com">the site</a></p>"#;
        let lines = render_html_body(html, 40);
        let link = all_spans(&lines)
            .into_iter()
            .find(|s| s.content.as_ref().contains("the site"))
            .expect("a span for the link text");
        assert!(
            link.style.add_modifier.contains(Modifier::UNDERLINED),
            "a link is underlined"
        );
        assert_eq!(link.style.fg, Some(theme::active().accent), "in the accent colour");
    }

    #[test]
    fn every_rendered_row_fits_the_pane_width() {
        let html = "<p>the quick brown fox jumps over the lazy dog again and \
            again and again</p>";
        let lines = render_html_body(html, 20);
        assert!(lines.len() > 1, "a long paragraph wraps to several rows");
        for line in &lines {
            let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(width <= 20, "row wider than the pane: {width}");
        }
    }

    #[test]
    fn a_blank_line_between_paragraphs_is_kept() {
        let lines = render_html_body("<p>one</p><p>two</p>", 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t == "one"));
        assert!(texts.iter().any(|t| t == "two"));
        assert!(
            texts.iter().any(|t| t.is_empty()),
            "paragraphs stay separated: {texts:?}"
        );
    }

    #[test]
    fn pathological_input_returns_lines_and_never_panics() {
        // html2text is lenient, and the error arm falls back to the plain
        // pipeline; either way the pane gets lines rather than a panic.
        assert!(!render_html_body("<<<not really html<<<", 30).is_empty());
        assert!(!render_html_body("", 30).is_empty());
    }

    #[test]
    fn annotation_folding_layers_inner_over_outer() {
        // A strong link keeps the link's underline and adds the strong bold.
        let style = style_for_annotations(&[
            RichAnnotation::Link("https://x".to_string()),
            RichAnnotation::Strong,
        ]);
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }
}
