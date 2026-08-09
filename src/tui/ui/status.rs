use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::super::app::{hint_bindings, prefix_continuations, App, KeyCtx, View};
use super::super::theme;
use super::util::{desc_span, display_width, hint_span};

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
    // Outbox badge (#0037 item 5): only rendered when something is actually
    // outstanding, so the quiet case costs no width and no attention.
    let outbox = app
        .accounts
        .iter()
        .fold(crate::outbox::OutboxCounts::default(), |mut acc, a| {
            acc.open += a.outbox.open;
            acc.failed += a.outbox.failed;
            acc.partial += a.outbox.partial;
            acc
        });
    // `partial` is a message that went out to some of its recipients and never
    // to the rest (#0063): nothing will move it on, so it is named here until
    // the row is discarded.
    let mut stuck = Vec::new();
    if outbox.failed > 0 {
        stuck.push(format!("{} failed", outbox.failed));
    }
    if outbox.partial > 0 {
        stuck.push(format!("{} partial", outbox.partial));
    }
    let outbox_text = if !stuck.is_empty() {
        format!("OUTBOX {} ({}) | ", outbox.total(), stuck.join(", "))
    } else if outbox.open > 0 {
        format!("OUTBOX {} | ", outbox.open)
    } else {
        String::new()
    };
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
    } else if (!app.search_query.is_empty() || app.flagged_only) && shown != total {
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
    // Display cells, not bytes: a mailbox label with an umlaut or a wide glyph
    // would otherwise reserve more columns than it draws and eat into the
    // account strip on the left.
    let right_len = (display_width(&sel_text)
        + display_width(&unread_text)
        + display_width(&outbox_text)
        + display_width(watch_prefix)
        + display_width(&mailbox_text)
        + 1) as u16;

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
                // The account's own last sync outcome, not the shared status
                // line, so a failure survives every later success of another
                // account (#0071, the race #0068 lost).
                let failed = acct.sync_health.is_failed();
                let label = if failed {
                    format!("\u{26a0}{}", name.to_uppercase())
                } else {
                    name.to_uppercase()
                };
                if i == app.active_account {
                    spans.push(Span::styled(
                        format!("[{}]", label),
                        Style::default()
                            .fg(theme::active().bg)
                            .bg(theme::active().accent),
                    ));
                } else {
                    let style = if failed {
                        Style::default().fg(theme::active().error)
                    } else if acct.has_unseen {
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
    if !outbox_text.is_empty() {
        // A failed row needs a human, so it gets the error colour; a row that
        // is merely waiting for its APPEND gets the softer one.
        let style = if outbox.failed > 0 {
            Style::default().fg(theme::active().error)
        } else {
            Style::default().fg(theme::active().emphasis)
        };
        right_spans.push(Span::styled(outbox_text, style));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_health::SyncHealth;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    /// A minimal `AccountState`, built as a struct literal rather than through
    /// `AccountState::new`, which reads the user's config and keyring.
    fn account(name: &str, sync_health: SyncHealth) -> crate::tui::app::AccountState {
        crate::tui::app::AccountState {
            account_config: crate::config::AccountConfig {
                name: name.to_string(),
                ..Default::default()
            },
            imap_config: None,
            smtp_config: None,
            graph_config: None,
            signature_content: None,
            archive_server_name: "Archive".to_string(),
            drafts_dir: None,
            mailboxes: Vec::new(),
            mailbox_counts: Vec::new(),
            email_cache: Vec::new(),
            sidebar_index: 0,
            active_mailbox: 0,
            list_index: 0,
            cursor_ref: None,
            headers_scroll: 0,
            preview_scroll: 0,
            selection: std::collections::HashSet::new(),
            search_query: String::new(),
            search_includes_body: false,
            watcher_active: false,
            outbox: crate::outbox::OutboxCounts::default(),
            has_unseen: false,
            sync_health,
        }
    }

    fn failed_at(hour: u32, minute: u32) -> SyncHealth {
        SyncHealth::default().updated(Err("IMAP login failed: no such user"), local(hour, minute))
    }

    fn local(hour: u32, minute: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 8, 6, hour, minute, 0)
            .single()
            .expect("unambiguous local time")
    }

    /// An app with `accounts`, the first one active, and just enough mailbox
    /// state for the status bar's counters to resolve.
    fn app_with(accounts: Vec<crate::tui::app::AccountState>) -> App {
        let mut app = App::default_for_tests();
        app.accounts = accounts;
        app.active_account = 0;
        app.mailbox_counts = vec![0];
        app
    }

    fn render(app: &App, width: u16) -> Buffer {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_status_bar(app, frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &Buffer) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect()
    }

    /// The only surface that shows a *non-active* account's failure (#0071):
    /// the sidebar block follows the account being viewed, so without this
    /// strip a broken account nobody has switched to says nothing at all.
    #[test]
    fn the_strip_marks_a_non_active_failing_account() {
        let app = app_with(vec![
            account("tum", SyncHealth::Ok { at: local(15, 43) }),
            account("perso", failed_at(15, 42)),
        ]);
        let buffer = render(&app, 60);
        let text = row_text(&buffer);
        assert!(
            text.contains("\u{26a0}PERSO"),
            "the failing account carries the marker; got:\n{text}"
        );
        assert!(
            text.contains("[TUM]"),
            "and the active account stays bracketed; got:\n{text}"
        );

        let marker = text.find('\u{26a0}').expect("marker rendered") as u16;
        assert_eq!(
            buffer[(marker, 0)].style().fg,
            Some(theme::active().error),
            "a failure needs a human, so it gets the error colour"
        );
    }

    /// A healthy strip carries no marker and no error colour: the mark means
    /// something only because the quiet case is silent.
    #[test]
    fn a_healthy_strip_carries_no_marker() {
        let app = app_with(vec![
            account("tum", SyncHealth::Ok { at: local(15, 43) }),
            account("perso", SyncHealth::Unknown),
        ]);
        let text = row_text(&render(&app, 60));
        assert!(!text.contains('\u{26a0}'), "got:\n{text}");
        assert!(text.contains("PERSO"), "got:\n{text}");
    }
}
