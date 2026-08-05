use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::super::app::{
    App, AttachmentPicker, AttachmentPickerMode, ConfirmDialog, DirPicker, DirPickerMode,
    MailboxPicker, PersistentError, RsvpOverlay,
};
use super::super::theme;
use super::util::truncate;
use super::widgets::{modal_stack_areas, render_action_button, render_modal_shell};

/// Render the three-choice RSVP overlay (#0029): Accept / Tentative / Decline
/// for a received invite, opened with `V`. Esc cancels.
pub(super) fn render_rsvp_overlay(overlay: &RsvpOverlay, frame: &mut Frame, area: Rect) {
    let Some((_, inner)) = render_modal_shell(
        frame,
        area,
        46,
        8,
        theme::active().accent,
        theme::active().panel_bg,
    ) else {
        return;
    };

    let stack = modal_stack_areas(inner, 0, 1);
    let inner_width = inner.width as usize;

    let mut lines = vec![
        Line::from(Span::styled(
            truncate(&overlay.summary, inner_width),
            Style::default()
                .fg(theme::active().accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    let choices = ["Accept", "Tentative", "Decline"];
    for (i, label) in choices.iter().enumerate() {
        let selected = i == overlay.selected;
        let marker = if selected { "\u{25b6} " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(theme::active().selection)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::active().text)
        };
        lines.push(Line::from(Span::styled(format!("{marker}{label}"), style)));
    }

    frame.render_widget(Paragraph::new(lines), stack.content);
    if let Some(footer) = stack.footer {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  Enter confirm  ·  Esc cancel",
                Style::default().fg(theme::active().text_muted),
            )),
            footer,
        );
    }
}

pub(super) fn render_confirm_dialog(dialog: &ConfirmDialog, frame: &mut Frame, area: Rect) {
    let Some((_, inner)) = render_modal_shell(
        frame,
        area,
        40,
        7,
        theme::active().warning,
        theme::active().panel_bg,
    ) else {
        return;
    };

    let stack = modal_stack_areas(inner, 0, 1);

    let lines = vec![
        Line::from(Span::styled(
            &dialog.title,
            Style::default()
                .fg(theme::active().warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            truncate(&dialog.detail, inner.width as usize),
            Style::default().fg(theme::active().text),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), stack.content);

    if let Some(footer) = stack.footer {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  [y]", Style::default().fg(theme::active().success)),
                Span::styled("es  ", Style::default().fg(theme::active().text)),
                Span::styled("[n]", Style::default().fg(theme::active().error)),
                Span::styled("o", Style::default().fg(theme::active().text)),
            ])),
            footer,
        );
    }
}

pub(super) fn render_attachment_picker(picker: &AttachmentPicker, frame: &mut Frame, area: Rect) {
    let file_count = picker.files.len() as u16;
    let Some((_, inner)) = render_modal_shell(
        frame,
        area,
        50,
        file_count + 4,
        theme::active().border_focused,
        theme::active().panel_bg,
    ) else {
        return;
    };

    let is_save = picker.mode == AttachmentPickerMode::Save;

    // Header row: title + an ` esc close ` chip pushed to the right edge.
    let stack = modal_stack_areas(inner, 1, 1);
    let title = if is_save {
        "Save Attachments"
    } else {
        "Select Attachment"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default()
                .fg(theme::active().border_focused)
                .add_modifier(Modifier::BOLD),
        )),
        stack.header,
    );
    let chip_w = 10u16.min(stack.header.width);
    let chip = Rect {
        x: stack.header.x + stack.header.width.saturating_sub(chip_w),
        y: stack.header.y,
        width: chip_w,
        height: 1,
    };
    render_action_button(frame, chip, Some("esc"), "close", theme::active().border_focused);

    let inner_width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Reserve space for checkbox prefix in save mode
    let name_width = if is_save {
        inner_width.saturating_sub(4)
    } else {
        inner_width
    };

    for (i, path) in picker.files.iter().enumerate() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let display = if name.len() > name_width {
            format!("{}...", &name[..name_width.saturating_sub(3)])
        } else {
            name
        };
        let cursor_style = if i == picker.selected {
            Style::default().fg(theme::active().heading).bg(theme::active().surface)
        } else {
            Style::default().fg(theme::active().text)
        };

        if is_save {
            let check = if picker.selected_set.contains(&i) {
                "\u{f0132} " // nf-md-checkbox_marked
            } else {
                "\u{f0131} " // nf-md-checkbox_blank_outline
            };
            let check_style = if picker.selected_set.contains(&i) {
                Style::default().fg(theme::active().selection)
            } else {
                Style::default().fg(theme::active().text_faint)
            };
            lines.push(Line::from(vec![
                Span::styled(check, check_style),
                Span::styled(display, cursor_style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(display, cursor_style)));
        }
    }

    frame.render_widget(Paragraph::new(lines), stack.content);

    if let Some(footer_area) = stack.footer {
        let footer = if is_save {
            "j/k nav  Space sel  ^a all  Enter confirm  Esc cancel"
        } else {
            "j/k nav  Enter open  Esc cancel"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer,
                Style::default().fg(theme::active().text_muted),
            )),
            footer_area,
        );
    }
}

pub(super) fn render_dir_picker(picker: &DirPicker, frame: &mut Frame, area: Rect) {
    let dialog_width = 60u16.min(area.width.saturating_sub(4));
    let dialog_height = 20u16.min(area.height.saturating_sub(2));

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(dialog_width)])
        .flex(Flex::Center)
        .split(area);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(dialog_height)])
        .flex(Flex::Center)
        .split(horizontal[0]);

    let dialog_area = vertical[0];
    frame.render_widget(Clear, dialog_area);

    let mode_label = match picker.mode {
        DirPickerMode::Zoxide => "zoxide",
        DirPickerMode::Browser => "browse",
    };
    let title = format!(" Save to ({mode_label}) ");

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().success))
        .style(Style::default().bg(theme::active().bg));

    let block_inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let inner_width = block_inner.width.saturating_sub(2) as usize;

    match picker.mode {
        DirPickerMode::Zoxide => {
            // Input line + results + footer
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // query input
                    Constraint::Min(0),    // results
                    Constraint::Length(1), // footer
                ])
                .split(block_inner);

            // Query input
            let avail = (chunks[0].width as usize).saturating_sub(3); // "> " + cursor
            let value = super::util::scrolled_input_value(&picker.query, avail);
            let input_spans = vec![
                Span::styled("> ", Style::default().fg(theme::active().success)),
                Span::styled(value, Style::default().fg(theme::active().text)),
                Span::styled("\u{2588}", Style::default().fg(theme::active().success)),
            ];
            frame.render_widget(Paragraph::new(Line::from(input_spans)), chunks[0]);

            // Results
            let mut result_lines: Vec<Line> = Vec::new();
            if picker.zoxide_results.is_empty() {
                if !picker.zoxide_available {
                    result_lines.push(Line::from(Span::styled(
                        "zoxide not found -- press Tab to browse",
                        Style::default().fg(theme::active().text_faint),
                    )));
                } else if picker.query.is_empty() {
                    result_lines.push(Line::from(Span::styled(
                        "Type to search directories...",
                        Style::default().fg(theme::active().text_faint),
                    )));
                } else {
                    result_lines.push(Line::from(Span::styled(
                        "No matches",
                        Style::default().fg(theme::active().text_faint),
                    )));
                }
            } else {
                for (i, path) in picker.zoxide_results.iter().enumerate() {
                    let display = path.display().to_string();
                    let display = truncate(&display, inner_width);
                    let style = if i == picker.selected {
                        Style::default().fg(theme::active().heading).bg(theme::active().surface)
                    } else {
                        Style::default().fg(theme::active().text)
                    };
                    result_lines.push(Line::from(Span::styled(display, style)));
                }
            }
            frame.render_widget(Paragraph::new(result_lines), chunks[1]);

            // Footer
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Tab browse  Enter save  Esc cancel",
                    Style::default().fg(theme::active().text_muted),
                ))),
                chunks[2],
            );
        }
        DirPickerMode::Browser => {
            // Header (current path) + entries + footer
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // current path
                    Constraint::Min(0),    // entries
                    Constraint::Length(1), // footer
                ])
                .split(block_inner);

            // Current path
            let path_str = picker.current_dir.display().to_string();
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate(&path_str, inner_width),
                    Style::default()
                        .fg(theme::active().accent)
                        .add_modifier(Modifier::BOLD),
                ))),
                chunks[0],
            );

            // Entries: index 0 = "[ Save here ]", then directories
            let mut entry_lines: Vec<Line> = Vec::new();
            let save_here_style = if picker.selected == 0 {
                Style::default()
                    .fg(theme::active().success)
                    .bg(theme::active().surface)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::active().success)
            };
            entry_lines.push(Line::from(Span::styled("[ Save here ]", save_here_style)));

            for (i, dir) in picker.dir_entries.iter().enumerate() {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| dir.display().to_string());
                let display = format!("\u{f024b} {}", truncate(&name, inner_width.saturating_sub(2)));
                let style = if i + 1 == picker.selected {
                    Style::default().fg(theme::active().heading).bg(theme::active().surface)
                } else {
                    Style::default().fg(theme::active().text)
                };
                entry_lines.push(Line::from(Span::styled(display, style)));
            }

            // Scroll the list if it's too tall
            let visible_height = chunks[1].height as usize;
            let scroll_offset = if picker.selected >= visible_height {
                picker.selected.saturating_sub(visible_height / 2)
            } else {
                0
            };

            frame.render_widget(
                Paragraph::new(entry_lines).scroll((scroll_offset as u16, 0)),
                chunks[1],
            );

            // Footer
            let footer_text = if picker.zoxide_available {
                "Tab zoxide  Enter open/save  h up  ~ home  Esc cancel"
            } else {
                "Enter open/save  h up  ~ home  Esc cancel"
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    footer_text,
                    Style::default().fg(theme::active().text_muted),
                ))),
                chunks[2],
            );
        }
    }
}

pub(super) fn render_mailbox_picker(picker: &MailboxPicker, frame: &mut Frame, area: Rect) {
    let dialog_width = 40u16.min(area.width.saturating_sub(4));
    let list_len = picker.filtered.len().max(1) as u16;
    let dialog_height = (list_len + 5).min(area.height.saturating_sub(2));

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(dialog_width)])
        .flex(Flex::Center)
        .split(area);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(dialog_height)])
        .flex(Flex::Center)
        .split(horizontal[0]);

    let dialog_area = vertical[0];
    frame.render_widget(Clear, dialog_area);

    let count = picker.msgs.len();
    let title = if count == 1 {
        " Move to ".to_string()
    } else {
        format!(" Move {count} emails to ")
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().border_focused))
        .style(Style::default().bg(theme::active().bg));

    let block_inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // query input
            Constraint::Min(0),    // mailbox list
            Constraint::Length(1), // footer
        ])
        .split(block_inner);

    // Query input
    let avail = (chunks[0].width as usize).saturating_sub(3); // "> " + cursor
    let value = super::util::scrolled_input_value(&picker.query, avail);
    let input_spans = vec![
        Span::styled("> ", Style::default().fg(theme::active().border_focused)),
        Span::styled(value, Style::default().fg(theme::active().text)),
        Span::styled("\u{2588}", Style::default().fg(theme::active().border_focused)),
    ];
    frame.render_widget(Paragraph::new(Line::from(input_spans)), chunks[0]);

    // Mailbox list
    let inner_width = block_inner.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    if picker.filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching mailbox",
            Style::default().fg(theme::active().text_faint),
        )));
    } else {
        for (pos, &cand_idx) in picker.filtered.iter().enumerate() {
            let label = &picker.candidates[cand_idx].1;
            let style = if pos == picker.selected {
                Style::default().fg(theme::active().heading).bg(theme::active().surface)
            } else {
                Style::default().fg(theme::active().text)
            };
            lines.push(Line::from(Span::styled(truncate(label, inner_width), style)));
        }
    }
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    // Footer
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "type filter  \u{2191}/\u{2193} nav  Enter move  Esc cancel",
            Style::default().fg(theme::active().text_muted),
        ))),
        chunks[2],
    );
}

pub(super) fn render_persistent_error(error: &PersistentError, frame: &mut Frame, area: Rect) {
    let msg_lines = error.message.lines().count() as u16;
    let Some((_, inner)) = render_modal_shell(
        frame,
        area,
        50,
        msg_lines + 6,
        theme::active().error,
        theme::active().panel_bg,
    ) else {
        return;
    };
    let inner_width = inner.width as usize;

    let stack = modal_stack_areas(inner, 0, 1);

    let mut lines = vec![
        Line::from(Span::styled(
            "Error",
            Style::default().fg(theme::active().error).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for msg_line in error.message.lines() {
        lines.push(Line::from(Span::styled(
            truncate(msg_line, inner_width),
            Style::default().fg(theme::active().text),
        )));
    }
    frame.render_widget(Paragraph::new(lines), stack.content);

    if let Some(footer) = stack.footer {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  [s]", Style::default().fg(theme::active().success)),
                Span::styled("ync now  ", Style::default().fg(theme::active().text)),
                Span::styled("[d]", Style::default().fg(theme::active().text_muted)),
                Span::styled("ismiss", Style::default().fg(theme::active().text)),
            ])),
            footer,
        );
    }
}

/// Help-overlay sections, derived from the single `KEYMAP` source of truth
/// (#0032). The former hand-kept copy lived here; it now delegates so the help
/// overlay, the hint bar, and the website table cannot drift apart.
pub(super) fn help_sections() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    crate::tui::app::help_sections()
}

pub(super) fn render_help_overlay(app: &mut App, frame: &mut Frame, area: Rect) {
    let help_width = 50u16.min(area.width.saturating_sub(4));
    let help_height = 42u16.min(area.height.saturating_sub(2));

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(help_width)])
        .flex(Flex::Center)
        .split(area);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(help_height)])
        .flex(Flex::Center)
        .split(horizontal[0]);

    let help_area = vertical[0];
    frame.render_widget(Clear, help_area);

    let section_line = |title: &str| -> Line {
        Line::from(Span::styled(
            format!("  {title}"),
            Style::default()
                .fg(theme::active().heading)
                .add_modifier(Modifier::BOLD),
        ))
    };

    let entry_line = |key: &str, desc: &str| -> Line {
        Line::from(vec![
            Span::styled(format!("  {key:<12}"), Style::default().fg(theme::active().accent)),
            Span::styled(desc.to_string(), Style::default().fg(theme::active().text)),
        ])
    };

    let filter = app.help_filter.to_lowercase();
    let sections = help_sections();
    let mut lines: Vec<Line> = Vec::new();

    for (title, entries) in &sections {
        let matched: Vec<_> = if filter.is_empty() {
            entries.iter().collect()
        } else {
            entries
                .iter()
                .filter(|(key, desc)| {
                    key.to_lowercase().contains(&filter) || desc.to_lowercase().contains(&filter)
                })
                .collect()
        };

        if matched.is_empty() {
            continue;
        }

        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(section_line(title));
        for (key, desc) in matched {
            lines.push(entry_line(key, desc));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matching bindings",
            Style::default().fg(theme::active().text_faint),
        )));
    }

    let content_height = lines.len() as u16;

    let show_filter_bar = app.help_filter_active || !app.help_filter.is_empty();
    let inner_height = help_area.height.saturating_sub(2);
    let filter_bar_height: u16 = if show_filter_bar && inner_height >= 3 {
        1
    } else {
        0
    };
    let visible_height = inner_height.saturating_sub(filter_bar_height);

    let max_scroll = content_height.saturating_sub(visible_height);
    app.help_scroll = app.help_scroll.min(max_scroll);

    let bottom_title = if content_height > visible_height {
        let top = app.help_scroll + 1;
        let bottom = (app.help_scroll + visible_height).min(content_height);
        format!(" {top}-{bottom}/{content_height} ")
    } else {
        " /filter  j/k scroll ".to_string()
    };

    let block = Block::default()
        .title(" Help ")
        .title_bottom(Line::from(bottom_title).alignment(Alignment::Center))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::active().accent))
        .style(Style::default().bg(theme::active().bg));

    let block_inner = block.inner(help_area);
    frame.render_widget(block, help_area);

    let content_area = if filter_bar_height > 0 && block_inner.height >= 2 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(block_inner);

        let mut spans = vec![
            Span::styled("/", Style::default().fg(theme::active().accent)),
            Span::styled(app.help_filter.as_str(), Style::default().fg(theme::active().text)),
        ];
        if app.help_filter_active {
            spans.push(Span::styled("\u{2588}", Style::default().fg(theme::active().accent)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);

        chunks[1]
    } else {
        block_inner
    };

    let help = Paragraph::new(lines).scroll((app.help_scroll, 0));
    frame.render_widget(help, content_area);
}
