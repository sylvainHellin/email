mod activity;
mod calendar;
mod compose;
mod contacts;
/// Pre-rewrite golden-frame capture (#0049 unit 0a); tests only.
#[cfg(test)]
mod golden_frames;
mod headers;
mod list;
mod overlays;
mod preview;
mod search;
mod sidebar;
mod status;
mod util;
mod views;
mod widgets;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use super::app::{App, Overlay, View};

/// The stacked regions of the herdr-style left column.
struct LeftColumn {
    sidebar: ratatui::layout::Rect,
    /// Email list (Mail view) or content host (non-Mail, narrow tier).
    middle: ratatui::layout::Rect,
    /// Activity-log panel, present only when toggled on in the Mail view.
    activity: Option<ratatui::layout::Rect>,
    /// Bottom-left view switcher (#0033), always present.
    switcher: ratatui::layout::Rect,
}

/// Split the left column into sidebar / middle / optional activity-log /
/// bottom-left view switcher. The activity-log slot is preserved from the
/// pre-#0033 layout (toggled by `!`) and only shown in the Mail view; the
/// switcher is pinned to the very bottom in every view.
fn split_left_column(app: &App, area: ratatui::layout::Rect, sidebar_height: u16) -> LeftColumn {
    let show_activity = app.show_activity_log && app.view == View::Mail;
    let mut constraints = vec![Constraint::Length(sidebar_height), Constraint::Min(0)];
    if show_activity {
        constraints.push(Constraint::Length(6));
    }
    constraints.push(Constraint::Length(views::SWITCHER_HEIGHT));

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    if show_activity {
        LeftColumn {
            sidebar: rows[0],
            middle: rows[1],
            activity: Some(rows[2]),
            switcher: rows[3],
        }
    } else {
        LeftColumn {
            sidebar: rows[0],
            middle: rows[1],
            activity: None,
            switcher: rows[2],
        }
    }
}

/// Render the entire UI from the current app state.
pub fn view(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Bottom rows: a herdr-style mode/hint bar (#0032) above the status bar.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1), // hint bar
            Constraint::Length(1), // status bar
        ])
        .split(area);

    let main_area = outer[0];
    let hint_area = outer[1];
    let status_area = outer[2];

    let show_right = app.terminal_width >= 80;
    let show_sidebar = app.terminal_width >= 40;

    if show_right {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(main_area);

        let left_col = columns[0];
        let right_col = columns[1];

        let sidebar_height = (app.mailboxes.len() as u16) + 3;
        let left = split_left_column(app, left_col, sidebar_height);

        sidebar::render_sidebar(app, frame, left.sidebar);
        // Middle slot: the email list in Mail view; empty in the other views
        // (their content occupies the right column instead).
        if app.view == View::Mail {
            list::render_email_list(app, frame, left.middle);
        }
        if let Some(activity) = left.activity {
            sidebar::render_activity_log(app, frame, activity);
        }
        views::render_view_switcher(app, frame, left.switcher);

        // Right column: the mail preview panes, the Contacts view, or the
        // Calendar agenda.
        if app.view == View::Mail {
            let right_panels = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(sidebar_height), Constraint::Min(0)])
                .split(right_col);
            headers::render_headers(app, frame, right_panels[0]);
            preview::render_body(app, frame, right_panels[1]);
        } else if app.view == View::Contacts {
            contacts::render_contacts(app, frame, right_col);
        } else {
            calendar::render_calendar(app, frame, right_col);
        }
    } else if show_sidebar {
        let sidebar_height = (app.mailboxes.len() as u16) + 2;
        let left = split_left_column(app, main_area, sidebar_height);

        sidebar::render_sidebar(app, frame, left.sidebar);
        if app.view == View::Mail {
            list::render_email_list(app, frame, left.middle);
        } else if app.view == View::Contacts {
            contacts::render_contacts(app, frame, left.middle);
        } else {
            calendar::render_calendar(app, frame, left.middle);
        }
        if let Some(activity) = left.activity {
            sidebar::render_activity_log(app, frame, activity);
        }
        views::render_view_switcher(app, frame, left.switcher);
    } else if app.view == View::Mail {
        list::render_email_list(app, frame, main_area);
    } else {
        // Narrowest tier: the active view's content fills the frame, switcher
        // pinned below.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(views::SWITCHER_HEIGHT),
            ])
            .split(main_area);
        if app.view == View::Contacts {
            contacts::render_contacts(app, frame, rows[0]);
        } else {
            calendar::render_calendar(app, frame, rows[0]);
        }
        views::render_view_switcher(app, frame, rows[1]);
    }

    status::render_hint_bar(app, frame, hint_area);
    status::render_status_bar(app, frame, status_area);

    // Exactly one overlay at a time by construction (#0032): a single match
    // on `app.overlay` replaces the former if-cascade of independent overlay
    // flags. Dim the whole frame first so the modal visually floats above the
    // recessed main view.
    if app.overlay.is_active() {
        widgets::dim_background(frame, area);
    }
    // The `Help`/`Activity`/`Search`/`Compose` renderers take `&mut App`
    // (they clamp scroll offsets against the computed viewport), so they can't
    // run while holding an immutable borrow of `app.overlay`. Dispatch those
    // via a discriminant check; the payload-carrying overlays render by ref.
    match &app.overlay {
        Overlay::None | Overlay::Help | Overlay::Activity | Overlay::Search
        | Overlay::Compose(_) => {}
        Overlay::Confirm(dialog) => overlays::render_confirm_dialog(dialog, frame, area),
        Overlay::Attachment(picker) => {
            overlays::render_attachment_picker(picker, frame, area)
        }
        Overlay::Dir(picker) => overlays::render_dir_picker(picker, frame, area),
        Overlay::Mailbox(picker) => overlays::render_mailbox_picker(picker, frame, area),
        Overlay::Rsvp(overlay) => overlays::render_rsvp_overlay(overlay, frame, area),
        Overlay::Error(error) => overlays::render_persistent_error(error, frame, area),
    }
    if matches!(app.overlay, Overlay::Help) {
        overlays::render_help_overlay(app, frame, area);
    } else if matches!(app.overlay, Overlay::Activity) {
        activity::render_activity_overlay(app, frame, area);
    } else if matches!(app.overlay, Overlay::Search) {
        search::render_search_overlay(app, frame, area);
    } else if matches!(app.overlay, Overlay::Compose(_)) {
        compose::render_compose_wizard(app, frame, area);
    }
}
