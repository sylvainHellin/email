---
id: 0036
title: Microsoft Graph sync backend (calendar + server-side RSVP)
type: feature
priority: later
status: blocked
created: 2026-07-11
---

Parked 2026-08-06: the Graph backend is parked until it is picked up deliberately, first live target the EVOQS Exchange account; see [BACKLOG](../../BACKLOG.md).

Future Graph-based sync backend that closes the gaps v1 leaves open. Design: [calendar-invites](../plans/calendar-invites.md) (D1, D4, risks), [tui-restructure-views](../plans/tui-restructure-views.md) (D12). **Blocked by [#0035](0035-graph-admin-approval.md)** (admin approval unobtainable today).

v1 is iMIP-only and never touches the server-side Exchange calendar. This ticket adds Graph as an additional sync backend for Graph accounts.

## Scope (when unblocked)

- Add `Calendars.ReadWrite` (and `Contacts.Read`) scopes to `GRAPH_SCOPES` (`src/oauth2.rs`); re-consent/re-login (scope-bound refresh token).
- Read: `GET /me/calendarView` → merge server events into the local calendar view (closes the "blind to Outlook-created events" gap from [#0034](0034-local-calendar-view.md)).
- Write: native `POST /me/events/{id}/accept|decline|tentativelyAccept` so accepting via `mp` flips the event in Outlook / Mac Calendar; create real events for sent invites so they appear in the user's own Exchange calendar.
- **Exchange organizer-matching** correctness (moved here from ticket 0026 risks): validated organizer identity and reliable `UID`→event RSVP mapping are Graph-native concerns.
- Contacts: optional `/me/contacts` tier merged into the local contacts index (additive `ContactSource`).

## Acceptance criteria

- Graph accounts see Outlook-created events in the calendar view.
- Accepting/declining via `mp` updates the server-side calendar.
- Personal IMAP/SMTP accounts remain iMIP-only (unchanged).

## Blocked by

- [#0035](0035-graph-admin-approval.md) admin approval + Azure app verification.
