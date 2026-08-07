---
id: TKT-0048
title: Contacts/Calendar views need a visual polish pass to match the overlay quality
type: feature
priority: next
status: done
created: 2026-07-30
---

User feedback after driving the new views live (2026-07-30): the Contacts and Calendar
views are "a good start, but look significantly worse than the current overlay we have
(e.g. when creating a new mail)".

The overlays got the #0032 treatment (shared modal shells, dimmed background, consistent
chrome via `ui/widgets.rs`); the views (#0033/#0034) were built function-first and do not
share that visual language. Known cosmetic debt inherited from those tickets: the sidebar
still lists mailboxes when off the Mail view, and the wide-tier left-middle slot renders
blank in Contacts/Calendar.

Scope is deliberately open until the specific gripes are collected — first step is a short
design pass comparing the overlay widgets' visual language (borders, padding, dim, emphasis,
typography) against `ui/contacts.rs` / `ui/calendar.rs` and proposing concrete changes for
approval before any implementation.

## Acceptance criteria

- Concrete gripe list / design proposal reviewed with the user before implementation.
- Contacts and Calendar share the overlays' visual language (chrome, spacing, emphasis)
  rather than each inventing their own.
- The off-Mail sidebar and blank wide-tier slot are addressed or explicitly deferred with
  a reason.

## Resolution (2026-08-07)

Shipped as a purely presentational pass; no sync/ingest/store-write path was touched.

What changed:

- Cursor-row parity. The Contacts and Calendar lists dropped the solid `selection`
  background (and the `▸` highlight symbol) they each invented and now use the Mail
  list's convention: a raised `surface` fill carrying the `selection` foreground, bold.
  Factored into a `cursor_row_style()` helper in each view.
- Wide-tier layout. Off the Mail view the widest tier used to render the mailbox sidebar
  above a blank left-middle slot, beside a cramped detail pane in the right column. The
  view now owns the whole frame the way Mail's list + preview do: the ranked list fills the
  left column, the detail pane fills the right column, and only the view switcher stays
  pinned bottom-left. New `render_contacts_split` / `render_calendar_split` entry points
  drive the two columns; `render_contacts` / `render_calendar` still handle the single-area
  medium and narrow tiers.
- Off-Mail sidebar. The mailbox sidebar is Mail-only chrome, so it is no longer rendered in
  the Contacts/Calendar views at any tier (wide, medium or narrow); the space goes to the
  view. This addresses both named cosmetic-debt items (off-Mail sidebar and blank slot)
  together.

Adaptations to a stale tree (the ticket predates the current paths):

- The renderers live at `src/tui/ui/{contacts,calendar,mod}.rs`, not `ui/contacts.rs` /
  `ui/calendar.rs` as the ticket body assumed.
- Acceptance criterion 1 (design review with the user before implementation) was satisfied
  by the approved execution direction handed down for this run (match the established
  overlay/list conventions; address the off-Mail sidebar and blank slot), not by a separate
  written proposal round.

Validation: `cargo test` green (954, up from 953 with the new `golden_contacts_view` frame);
`golden_calendar_view` re-approved for the new layout; clippy clean on touched files (24-warning
baseline preserved); `cargo install --path .` succeeds. Golden frames are the render oracle; no
interactive live-TUI smoke was run in the build sandbox. The help overlay and website were left
untouched: no key bindings or user-visible commands changed.
