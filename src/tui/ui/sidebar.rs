use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use super::super::app::{App, Focus, StatusLevel};
use super::super::theme;
use super::util::{pane_border_style, truncate};

/// The persistent sync-failure lines for the account the sidebar is showing
/// (#0071), or `None` when its last sync worked or none has run yet.
///
/// This is the surface that does not depend on winning the status-line race:
/// it is rendered from the account's own [`crate::sync_health::SyncHealth`]
/// every frame, so it stays up while other accounts sync successfully and
/// disappears only when *this* account syncs cleanly.
pub(super) fn sync_failure_lines(app: &App) -> Option<(String, String)> {
    app.accounts
        .get(app.active_account)
        .and_then(|acct| acct.sync_health.failure_lines())
}

/// Rows the reason gets under the headline. Two, because the sidebar is about
/// forty columns wide and the errors worth acting on are longer than that:
/// `IMAP login failed: no response: ... Some("no such user")` is cut exactly
/// where it stops being generic on one line.
const REASON_ROWS: usize = 2;

/// Sidebar rows the health block needs. The layout in [`super`] sizes the
/// sidebar from the mailbox count, so the block has to be paid for there or it
/// is clipped away. Fixed rather than measured, so the layout never disagrees
/// with the wrap about how tall the block is.
pub(super) fn sync_health_rows(app: &App) -> u16 {
    if sync_failure_lines(app).is_some() {
        1 + REASON_ROWS as u16
    } else {
        0
    }
}

/// Hard-wrap `text` on word boundaries into exactly `rows` lines of at most
/// `width` characters, padding with empty lines and ellipsising the last one
/// when the text does not fit.
///
/// A word longer than `width` (a URL, a base64 blob in an error) is broken
/// rather than allowed to overflow.
fn wrap_to(text: &str, width: usize, rows: usize) -> Vec<String> {
    if width == 0 || rows == 0 {
        return vec![String::new(); rows];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let sep = if current.is_empty() { 0 } else { 1 };
            let room = width.saturating_sub(current.chars().count() + sep);
            if word.chars().count() <= room {
                if sep == 1 {
                    current.push(' ');
                }
                current.push_str(word);
                break;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            // An oversized word on an empty line: break it at the width.
            let head: String = word.chars().take(width).collect();
            let consumed = head.len();
            lines.push(head);
            word = &word[consumed..];
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    let overflowed = lines.len() > rows;
    lines.truncate(rows);
    if overflowed {
        if let Some(last) = lines.last_mut() {
            let mut chars: Vec<char> = last.chars().collect();
            if chars.len() >= width {
                chars.truncate(width - 1);
            }
            chars.push('\u{2026}');
            *last = chars.into_iter().collect();
        }
    }
    lines.resize(rows, String::new());
    lines
}

pub(super) fn render_sidebar(app: &App, frame: &mut Frame, area: Rect) {
    let border_style = pane_border_style(app.focus, Focus::Sidebar);
    let sidebar_title = if app.accounts.len() > 1 {
        format!(" {} ", app.account_config.name.to_uppercase())
    } else {
        " Mail ".to_string()
    };
    let block = Block::default()
        .title(sidebar_title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    for (i, mb) in app.mailboxes.iter().enumerate() {
        let is_selected = i == app.active_mailbox;
        let is_highlighted = app.focus == Focus::Sidebar && i == app.sidebar_index;
        let count = app.mailbox_counts.get(i).copied().unwrap_or(0);

        let marker = if is_selected { ">" } else { " " };

        let label = format!("{} {} {} {:>2}", marker, mb.icon, mb.label, count);

        let style = if is_highlighted {
            Style::default()
                .fg(theme::active().selection)
                .add_modifier(Modifier::BOLD)
        } else if is_selected {
            Style::default().fg(theme::active().accent)
        } else {
            Style::default().fg(theme::active().text)
        };

        lines.push(Line::from(Span::styled(label, style)));
    }

    if let Some((headline, reason)) = sync_failure_lines(app) {
        let width = inner.width.saturating_sub(1) as usize;
        lines.push(Line::from(Span::styled(
            format!(" {}", truncate(&headline, width)),
            Style::default()
                .fg(theme::active().error)
                .add_modifier(Modifier::BOLD),
        )));
        for row in wrap_to(&reason, width, REASON_ROWS) {
            lines.push(Line::from(Span::styled(
                format!(" {row}"),
                Style::default().fg(theme::active().error),
            )));
        }
    }

    let sidebar_content = Paragraph::new(lines);
    frame.render_widget(sidebar_content, inner);
}

pub(super) fn render_activity_log(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Activity ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().accent_alt))
        .style(Style::default().bg(theme::active().bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.status_log.is_empty() {
        let empty = Paragraph::new("  No activity yet")
            .style(Style::default().fg(theme::active().text_muted));
        frame.render_widget(empty, inner);
        return;
    }

    let visible = inner.height as usize;
    let skip = app.status_log.len().saturating_sub(visible);

    let lines: Vec<Line> = app
        .status_log
        .iter()
        .skip(skip)
        .take(visible)
        .map(|entry| {
            let time = entry.timestamp.format("%H:%M").to_string();
            let color = match entry.level {
                StatusLevel::Success => theme::active().success,
                StatusLevel::Error => theme::active().error,
                StatusLevel::Warning => theme::active().warning,
                StatusLevel::Info => theme::active().info,
                StatusLevel::Progress => theme::active().accent_alt,
            };
            Line::from(vec![
                Span::styled(
                    format!(" {time} "),
                    Style::default().fg(theme::active().text_faint),
                ),
                Span::styled(
                    truncate(&entry.message, inner.width.saturating_sub(7) as usize),
                    Style::default().fg(color),
                ),
            ])
        })
        .collect();

    let content = Paragraph::new(lines);
    frame.render_widget(content, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_health::SyncHealth;
    use ratatui::backend::TestBackend;
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
            bg_mutations: 0,
            watcher_active: false,
            outbox: crate::outbox::OutboxCounts::default(),
            has_unseen: false,
            sync_health,
        }
    }

    fn app_with(accounts: Vec<crate::tui::app::AccountState>, active: usize) -> App {
        let mut app = App::default_for_tests();
        app.accounts = accounts;
        app.active_account = active;
        app
    }

    // -----------------------------------------------------------------------
    // wrap_to
    // -----------------------------------------------------------------------

    #[test]
    fn wrap_to_breaks_on_words_and_pads_to_the_row_count() {
        assert_eq!(
            wrap_to("IMAP login failed: no such user", 20, 2),
            vec!["IMAP login failed:".to_string(), "no such user".to_string()]
        );
        assert_eq!(
            wrap_to("short", 20, 2),
            vec!["short".to_string(), String::new()],
            "the block is a fixed height, so a short reason pads"
        );
    }

    #[test]
    fn wrap_to_marks_a_reason_that_does_not_fit() {
        let wrapped = wrap_to("one two three four five six seven eight", 10, 2);
        assert_eq!(wrapped.len(), 2);
        assert!(
            wrapped[1].ends_with('\u{2026}'),
            "the cut must be visible: {wrapped:?}"
        );
        assert!(wrapped.iter().all(|l| l.chars().count() <= 10));
    }

    /// A word longer than the sidebar (a URL, a base64 blob inside an error)
    /// is broken instead of overflowing, and broken on a character boundary so
    /// a multi-byte error cannot panic.
    #[test]
    fn wrap_to_breaks_an_oversized_word_on_a_character_boundary() {
        assert_eq!(
            wrap_to("üüüüüüü", 4, 2),
            vec!["üüüü".to_string(), "üüü".to_string()]
        );
        assert_eq!(wrap_to("anything", 0, 2), vec![String::new(), String::new()]);
    }

    fn failed_at(hour: u32, minute: u32) -> SyncHealth {
        use chrono::TimeZone;
        let at = chrono::Local
            .with_ymd_and_hms(2026, 8, 6, hour, minute, 0)
            .single()
            .expect("unambiguous local time");
        SyncHealth::default().updated(Err("IMAP login failed: no such user"), at)
    }

    /// The rendered sidebar text, rows joined by newlines.
    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_sidebar(app, frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The persistent surface (#0071): the failing account's reason is drawn
    /// into the sidebar from its own state, with no sync running and no status
    /// line involved, so it cannot be raced away by another account.
    #[test]
    fn the_sidebar_draws_the_active_accounts_sync_failure() {
        let app = app_with(vec![account("perso", failed_at(15, 42))], 0);
        let frame = render(&app, 40, 6);
        assert!(
            frame.contains("sync failed 15:42"),
            "the sidebar must carry the failure; got:\n{frame}"
        );
        assert!(
            frame.contains("IMAP login failed: no such"),
            "and its reason, wrapped rather than cut at the preamble:\n{frame}"
        );
        assert!(frame.contains("user"), "including its tail:\n{frame}");
        assert_eq!(sync_health_rows(&app), 3, "the block is paid for in the layout");
    }

    /// A healthy account, and an account that has not synced yet, add nothing.
    #[test]
    fn a_healthy_sidebar_says_nothing_about_sync_health() {
        for health in [
            SyncHealth::Unknown,
            SyncHealth::Ok { at: chrono::Local::now() },
        ] {
            let app = app_with(vec![account("tum", health)], 0);
            assert_eq!(sync_failure_lines(&app), None);
            assert_eq!(sync_health_rows(&app), 0);
            assert!(!render(&app, 40, 6).contains("sync failed"));
        }
    }

    /// The line follows the account the sidebar is showing, not whichever
    /// account failed last: switching to a healthy account clears it, and the
    /// broken one still carries its own mark.
    #[test]
    fn the_line_follows_the_active_account() {
        let accounts = vec![
            account("perso", failed_at(15, 42)),
            account("tum", SyncHealth::Ok { at: chrono::Local::now() }),
        ];
        let mut app = app_with(accounts, 0);
        assert!(sync_failure_lines(&app).is_some());
        app.active_account = 1;
        assert_eq!(sync_failure_lines(&app), None);
        assert!(app.accounts[0].sync_health.is_failed());
    }
}
