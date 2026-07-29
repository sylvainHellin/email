---
id: 0034
title: Local calendar view (iMIP-derived, agenda/list)
type: feature
priority: later
status: done
created: 2026-07-11
---

Shipped. Agenda list + shared event card over the iMIP files on disk, `KeyCtx::Calendar` keymap rows (`j/k`, `gg`/`G`, `Enter`/`e`, `V`, `t`, `r`), and a permanent in-pane caveat that Outlook-created events are invisible until #0036. Deduped by iCal UID (highest `(sequence, dtstamp)` wins, sidecar `.ics` authoritative); REPLY messages are not agenda rows; a matching `CANCEL` tags the row as cancelled (display only — the mutation semantics stay with #0031). Loaded synchronously on first switch: the walk measures ~100 ms on the largest local account.

Local-first Calendar view. Design: [tui-restructure-views](../plans/tui-restructure-views.md) (Stage 3, D12). Depends on [#0033](0033-view-switcher-contacts-view.md) (View abstraction) and [#0027](0027-imip-receive-parse.md) (on-disk events from iMIP).

Approved option c: the calendar is **blind to Outlook-created events** (events that never came through email) until the Graph sync backend lands ([#0036](0036-graph-sync-backend.md), gated on [#0035](0035-graph-admin-approval.md)).

## Scope

- **Local-first data source:** events are the files iMIP traffic already produced — received invites, accepted/sent invites — read from `event:` frontmatter + sidecar `.ics`. No new fetch backend.
- **Agenda/list rendering** of upcoming events, reusing the event-card detail from the mail preview (title, time, location, organizer, RSVP, attendee statuses).
- Wire the Calendar `View` arm (placeholder from #0033 → real content).

## Acceptance criteria

- Calendar view lists upcoming events derived from local iMIP files, newest-relevant first.
- Selecting an event shows the shared event card.
- Explicitly documented that Outlook-only events are not shown until the Graph backend.

## Non-goals

- Any calendar fetch/sync backend (that is [#0036](0036-graph-sync-backend.md)).
- Event creation/editing beyond what the iMIP send/RSVP tickets already do.
