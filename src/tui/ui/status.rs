use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::super::app::{hint_bindings, prefix_continuations, App, KeyCtx, View};
use super::super::theme;
use super::util::{desc_span, hint_span};

/// herdr-style mode/hint bar (#0032): a single line showing an accent-bg mode
/// badge plus the next valid keystrokes for the current context, all derived
/// from `KEYMAP`. When a leader prefix (today `g`) is pending it shows that
/// prefix's continuations instead, making the previously-invisible leader
/// discoverable (Space -> the `m/c/a` view switch; `g` -> `gg`/`G`). Overlay
/// contexts with their own inline chips (confirm,
/// pickers, compose, ...) render no hint bar (`key_context()` returns `None`).
pub(super) fn render_hint_bar(app: &App, frame: &mut Frame, area: Rect) {
    let bg = theme::active().surface;
    // Blank line (still painted with the surface bg) when there is nothing
    // contextual to show, so the row height stays stable.
    let Some(ctx) = app.key_context() else {
        let blank = Paragraph::new(Line::from("")).style(Style::default().bg(bg));
        frame.render_widget(blank, area);
        return;
    };

    let pending = app.pending_prefix();
    let (badge, hints): (String, Vec<(&'static str, &'static str)>) = if let Some(p) = pending {
        // Leader pending: show its continuations (Space -> `m/c/a`; `g` ->
        // `gg`, `G`). Global leader continuations (the Space `m/c/a` view
        // switch, #0033) resolve before the pane context, so surface them
        // alongside the pane's own continuations whenever we are not already
        // in Global.
        let mut conts: Vec<(&str, &str)> = prefix_continuations(ctx, p)
            .map(|kb| (kb.keys, kb.desc))
            .collect();
        if ctx != KeyCtx::Global {
            for kb in prefix_continuations(KeyCtx::Global, p) {
                conts.push((kb.keys, kb.desc));
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
        let hs: Vec<(&str, &str)> = hint_bindings(ctx)
            .filter(|kb| !(off_mail && ctx == KeyCtx::Global && !kb.action.is_view_agnostic()))
            .map(|kb| (kb.keys, kb.desc))
            .collect();
        (mode_label(app, ctx).to_string(), hs)
    };

    let mut spans: Vec<Span> = Vec::with_capacity(hints.len() * 2 + 2);
    // Mode badge: bold, accent background, contrasting fg.
    spans.push(Span::styled(
        format!(" {} ", badge),
        Style::default()
            .fg(theme::active().bg)
            .bg(theme::active().accent)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled("  ", Style::default().bg(bg)));
    for (i, (keys, desc)) in hints.iter().enumerate() {
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
            *desc,
            Style::default().fg(theme::active().text_muted).bg(bg),
        ));
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(bg));
    frame.render_widget(bar, area);
}

/// The badge label for a non-prefixed context. A live selection takes over the
/// badge (herdr's `N SELECTED`) since that is the most useful mode cue.
fn mode_label(app: &App, ctx: KeyCtx) -> String {
    if !app.selection.is_empty() {
        return format!("{} SELECTED", app.selection.len());
    }
    match ctx {
        KeyCtx::Global => "MAIL".to_string(),
        KeyCtx::Sidebar => "MAILBOXES".to_string(),
        KeyCtx::List => "MAIL".to_string(),
        KeyCtx::Headers => "HEADERS".to_string(),
        KeyCtx::Preview => "BODY".to_string(),
        KeyCtx::ServerSearch => "SEARCH".to_string(),
        KeyCtx::Contacts => "CONTACTS".to_string(),
        KeyCtx::Calendar => "CALENDAR".to_string(),
        KeyCtx::Activity => "ACTIVITY".to_string(),
        KeyCtx::Help => "HELP".to_string(),
    }
}

pub(super) fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let total = app.mailbox_counts[app.active_mailbox];
    let shown = app.visible.len();
    let unread_count = app.visible_emails().filter(|e| !e.read).count();
    let any_watching = app.accounts.iter().any(|a| a.watcher_active);
    let watch_prefix = if any_watching { "WATCHING " } else { "" };
    let sel_text = if app.selection.is_empty() {
        String::new()
    } else {
        format!("{} sel | ", app.selection.len())
    };
    let unread_text = if unread_count > 0 {
        format!("{} unread | ", unread_count)
    } else {
        String::new()
    };
    let mailbox_text = if shown == 0 {
        format!("{} 0 ", app.active_label())
    } else if !app.search_query.is_empty() && shown != total {
        format!(
            "{} {}/{} ({}) ",
            app.active_label(),
            app.list_index + 1,
            shown,
            total
        )
    } else {
        format!("{} {}/{} ", app.active_label(), app.list_index + 1, shown)
    };
    let right_len =
        (sel_text.len() + unread_text.len() + watch_prefix.len() + mailbox_text.len() + 1) as u16;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_len)])
        .split(area);

    let left_content = if app.bg_count > 0 {
        let frames = [
            '\u{280b}', '\u{2819}', '\u{2838}', '\u{2834}', '\u{2826}', '\u{2807}',
        ];
        let spinner = frames[app.bg_spin_tick % frames.len()];
        let label = app.status_message.as_deref().unwrap_or("Working...");
        let text = if app.bg_count > 1 {
            format!(" {} {} ({} ops)", spinner, label, app.bg_count)
        } else {
            format!(" {} {}", spinner, label)
        };
        Line::from(Span::styled(
            text,
            Style::default().fg(theme::active().success),
        ))
    } else if let Some(msg) = &app.status_message {
        Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(msg.as_str(), Style::default().fg(theme::active().success)),
        ])
    } else {
        let mut spans: Vec<Span> = vec![Span::styled(" ", Style::default())];
        if app.accounts.len() > 1 {
            for (i, acct) in app.accounts.iter().enumerate() {
                let name = &acct.account_config.name;
                let label = name.to_uppercase();
                if i == app.active_account {
                    spans.push(Span::styled(
                        format!("[{}]", label),
                        Style::default()
                            .fg(theme::active().bg)
                            .bg(theme::active().accent),
                    ));
                } else {
                    let style = if acct.has_unseen {
                        Style::default().fg(theme::active().success)
                    } else {
                        Style::default().fg(theme::active().text_faint)
                    };
                    spans.push(Span::styled(format!(" {} ", label), style));
                }
            }
            spans.push(Span::styled(
                " | ",
                Style::default().fg(theme::active().text_faint),
            ));
        }
        spans.push(hint_span("?"));
        spans.push(desc_span(" help"));
        Line::from(spans)
    };

    let left = Paragraph::new(left_content).style(
        Style::default()
            .fg(theme::active().text_muted)
            .bg(theme::active().surface),
    );
    frame.render_widget(left, chunks[0]);

    let mut right_spans = vec![Span::styled(" ", Style::default())];
    if !sel_text.is_empty() {
        right_spans.push(Span::styled(
            sel_text,
            Style::default().fg(theme::active().emphasis),
        ));
    }
    if !unread_text.is_empty() {
        right_spans.push(Span::styled(
            unread_text,
            Style::default().fg(theme::active().unread_count),
        ));
    }
    if any_watching {
        right_spans.push(Span::styled(
            watch_prefix,
            Style::default().fg(theme::active().accent_alt),
        ));
    }
    right_spans.push(Span::styled(
        mailbox_text,
        Style::default().fg(theme::active().accent),
    ));
    let right = Paragraph::new(Line::from(right_spans))
        .style(Style::default().bg(theme::active().surface))
        .alignment(Alignment::Right);
    frame.render_widget(right, chunks[1]);
}
