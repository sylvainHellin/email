---
id: TKT-0048
title: Contacts/Calendar views need a visual polish pass to match the overlay quality
type: feature
priority: next
status: open
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
