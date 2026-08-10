---
id: TKT-0044
title: after the data layer rework. It would be good to have a zoom/focus feature for any of the pane. similarly to what is possible with `herdr`, where I can have multiple panes for a window, but zoom on any of them, a similar functionality would be very useful
type: feature
priority: next
status: done
created: 2026-07-15
---


## Shipped (2026-08-13)

`z` zooms the focused pane to the whole content area and `z` again restores the
split. What is zoomed is always *the focused pane*, herdr-style, rather than a
pane remembered separately: `Tab` under a zoom moves the zoom with the focus,
so the keyboard always drives what is on screen. The state is one bool
(`App::zoomed`) plus `App::zoomed_pane()`, which resolves it against the focus
and the view.

Scope, deliberately: Mail view only. `ToggleZoom` is left out of
`KeyAction::is_view_agnostic`, so the dispatcher swallows `z` in Contacts and
Calendar, where the layout is a list and a detail card that already resize
themselves and there is no split to collapse. The flag survives the trip: zoom
the preview, look at the calendar, come back, still zoomed.

Details worth knowing:

- The hint and status bars are *never* hidden by a zoom. Losing the row that
  says how to leave a mode is how a mode traps a user.
- The badge reads `BODY ZOOM` / `MAIL ZOOM` / `MAILBOXES ZOOM`, suffixed onto
  whatever the badge would otherwise say, selection count included.
- The panes keep their own renderers, so a zoomed list is the same table with
  more columns visible and a zoomed preview is the same body with more width.
  Nothing about the content is special-cased for the zoom.
- `Focus::Search` zooms the list it filters (the `/` prompt is drawn inside the
  list pane); the compose wizard, which is already a full-frame overlay,
  refuses with "Nothing to zoom here." rather than arming a zoom that would
  surface later somewhere else.
- Zoom is session state, not per-account state: an account switch keeps it.

Not shipped: `Esc` does not unzoom (it is already the clear-selection and
close-prompt key in several panes, and overloading it there would make one of
those actions unreachable under a zoom), and there is no zoom for the activity
log panel, which `!` already toggles.
