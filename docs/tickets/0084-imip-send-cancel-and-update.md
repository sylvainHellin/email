---
id: 0084
title: iMIP send-side updates and cancellations (re-send with bumped SEQUENCE, METHOD:CANCEL)
type: feature
priority: later
status: open
created: 2026-08-11
---

Split out of [#0031](0031-imip-cancel-update.md), which shipped the **receive** half: CANCEL tombstones, `SEQUENCE`-based supersession, per-occurrence cancellations.
The send half is untouched: `mp` can create an invite ([#0028](0028-imip-send-invite.md)) and RSVP to one ([#0029](0029-imip-rsvp-reply.md)), but it cannot change or cancel an invite it sent.

## Scope

- Re-send a previously sent invite with a bumped `SEQUENCE` (time / location / attendee edits), reusing the stored `UID` from the sent copy's `invite.ics`.
- Send `METHOD:CANCEL` for an invite we organized, whole series and (with `RECURRENCE-ID`) a single occurrence.
- Per-occurrence RSVP replies (`RECURRENCE-ID` on the REPLY), which #0029 deliberately omitted (D6: whole series only).
- Creation of recurring invites from `mp invite`.

## Notes inherited from #0031

- Identity is `(UID, RECURRENCE-ID)`; the version chain is `(SEQUENCE, DTSTAMP)`. The receive-side fold (`crate::reconcile::fold_status`) already reads both, so a correctly-built outgoing update lands on the sender's own agenda for free through the sent copy.
- The receive side refuses to RSVP a cancelled or superseded version; the send side must not create versions that make that refusal fire spuriously (bump `SEQUENCE`, never re-use it with a newer `DTSTAMP` alone).
- Server-side calendar state is still not touched: that is the Graph backend ([#0036](0036-graph-sync-backend.md)).
