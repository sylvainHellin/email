use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use super::super::app::{hint_bindings, prefix_continuations, App, KeyCtx, View};
use super::super::theme;
use super::util::display_width;

/// Rows the hint bar occupies: one content row inside a top and bottom
/// border, so it reads as a pane like every other one.
pub(super) const HINT_BAR_HEIGHT: u16 = 3;

/// herdr-style mode/hint bar (#0032): a single line showing an accent-bg mode
/// badge plus the next valid keystrokes for the current context, all derived
/// from `KEYMAP`. When a leader prefix (today `g`) is pending it shows that
/// prefix's continuations instead, making the previously-invisible leader
/// discoverable (Space -> the `m/c/a` view switch; `g` -> `gg`/`G`). Overlay
/// contexts with their own inline chips (confirm,
/// pickers, compose, ...) render no hint bar (`key_context()` returns `None`).
pub(super) fn render_hint_bar(app: &App, frame: &mut Frame, area: Rect) {
    let bg = theme::active().bg;
    // The pane shell: same rounded border and background as the sidebar, list
    // and preview panes, so the bottom of the screen stops reading as a
    // foreign slab of colour.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().border))
        .style(Style::default().bg(bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Empty frame when there is nothing contextual to show, so the pane
    // height stays stable.
    let Some(ctx) = app.key_context() else {
        return;
    };

    // A reading pane (List / Headers / Body) shares the promoted MESSAGE
    // actions (#0092), so the hint bar merges the pane's own keys with the
    // message set; otherwise the bar would advertise only `j/k` and `v`.
    let msg_pane = matches!(ctx, KeyCtx::List | KeyCtx::Headers | KeyCtx::Preview);
    let pending = app.pending_prefix();
    let (badge, hints): (String, Vec<(&'static str, &'static str)>) = if let Some(p) = pending {
        // Leader pending: show its continuations (Space -> `m/c/a`; `g` ->
        // `gg`, `G`). Global leader continuations (the Space `m/c/a` view
        // switch, #0033) resolve before the pane context, so surface them
        // alongside the pane's own continuations whenever we are not already
        // in Global.
        let mut conts: Vec<(&str, &str)> = prefix_continuations(ctx, p)
            .map(|kb| (kb.keys, kb.hint_label()))
            .collect();
        if ctx != KeyCtx::Global {
            for kb in prefix_continuations(KeyCtx::Global, p) {
                conts.push((kb.keys, kb.hint_label()));
            }
        }
        if msg_pane {
            for kb in prefix_continuations(KeyCtx::Message, p) {
                if !conts.iter().any(|(k, _)| *k == kb.keys) {
                    conts.push((kb.keys, kb.hint_label()));
                }
            }
        }
        // The leader badge: a printable name for Space, otherwise the
        // uppercased key (e.g. `g` -> `G`).
        let badge = if p == ' ' { "SPACE".to_string() } else { p.to_uppercase().to_string() };
        (badge, conts)
    } else {
        // Off-Mail, only the view-agnostic Global bindings actually fire
        // (mail-specific Global keys are swallowed by the dispatcher, #0033).
        // Filter the Global hint row to match so we don't advertise swallowed
        // keys (`1-9 Jump to mailbox`, `/ Filter by metadata`) in Contacts /
        // Calendar. Pane contexts (Contacts list) are unaffected.
        let off_mail = app.view != View::Mail;
        let mut hs: Vec<(&str, &str)> = hint_bindings(ctx)
            .filter(|kb| !(off_mail && ctx == KeyCtx::Global && !kb.action.is_view_agnostic()))
            .map(|kb| (kb.keys, kb.hint_label()))
            .collect();
        if msg_pane {
            for kb in hint_bindings(KeyCtx::Message) {
                if !hs.iter().any(|(k, _)| *k == kb.keys) {
                    hs.push((kb.keys, kb.hint_label()));
                }
            }
        }
        (mode_label(app, ctx).to_string(), hs)
    };

    let badge_text = format!(" {} ", badge);
    let mut spans: Vec<Span> = Vec::with_capacity(hints.len() * 4 + 3);
    // Mode badge: bold, accent background, contrasting fg.
    let mut used = display_width(&badge_text) + 2;
    spans.push(Span::styled(
        badge_text,
        Style::default()
            .fg(theme::active().bg)
            .bg(theme::active().accent)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled("  ", Style::default().bg(bg)));

    // Drop whole `keys` + label pairs that do not fit and mark the cut with an
    // ellipsis, rather than letting ratatui clip the last one mid-word
    // (#0078). The help overlay (`?`) still lists every binding in full, so
    // nothing dropped here is unreachable.
    let total = usize::from(inner.width);
    let mut truncated = false;
    for (i, (keys, label)) in hints.iter().enumerate() {
        let sep = usize::from(i > 0) * 2;
        let width = sep + display_width(keys) + 1 + display_width(label);
        // The ellipsis needs a cell of its own unless this is the last pair.
        let reserve = if i + 1 == hints.len() { 0 } else { 2 };
        if used + width + reserve > total {
            truncated = true;
            break;
        }
        used += width;
        if i > 0 {
            spans.push(Span::styled("  ", Style::default().bg(bg)));
        }
        spans.push(Span::styled(
            *keys,
            Style::default()
                .fg(theme::active().accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default().bg(bg)));
        spans.push(Span::styled(
            *label,
            Style::default().fg(theme::active().text_muted).bg(bg),
        ));
    }
    if truncated {
        spans.push(Span::styled(" …", Style::default().fg(theme::active().text_muted).bg(bg)));
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(bg));
    frame.render_widget(bar, inner);
}

/// The badge label for a non-prefixed context. A live selection takes over the
/// badge (herdr's `N SELECTED`) since that is the most useful mode cue.
fn mode_label(app: &App, ctx: KeyCtx) -> String {
    // A zoom hides the other panes, so the bar has to say so: the badge is the
    // only chrome left that can (#TKT-0044). It suffixes whatever the badge
    // would otherwise be, selection included, because both facts matter.
    let zoom = if app.zoomed_pane().is_some() { " ZOOM" } else { "" };
    if !app.selection.is_empty() {
        return format!("{} SELECTED{zoom}", app.selection.len());
    }
    let base = match ctx {
        KeyCtx::Global => "MAIL",
        // `key_context()` never returns Message (it reports the focused pane),
        // but the match must be exhaustive; a reading pane is still "MAIL".
        KeyCtx::Message => "MAIL",
        KeyCtx::Sidebar => "MAILBOXES",
        KeyCtx::List => "MAIL",
        KeyCtx::Headers => "HEADERS",
        KeyCtx::Preview => "BODY",
        KeyCtx::ServerSearch => "SEARCH",
        KeyCtx::Contacts => "CONTACTS",
        KeyCtx::Calendar => "CALENDAR",
        KeyCtx::Activity => "ACTIVITY",
        KeyCtx::Help => "HELP",
    };
    format!("{base}{zoom}")
}
