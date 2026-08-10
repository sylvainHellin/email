---
id: 0031
title: iMIP cancellations and updates (CANCEL / SEQUENCE bump)
type: feature
priority: later
status: done
created: 2026-07-11
---

Receive half shipped; the send half is split out as [#0084](0084-imip-send-cancel-and-update.md).

## Shipped (receive side)

- **Identity is `(UID, RECURRENCE-ID)`, the version chain is `(SEQUENCE, DTSTAMP)`.** `parse_ics` now reads `RECURRENCE-ID` and normalises it exactly like `DTSTART`, so the same occurrence named in UTC and with a `TZID` is one identity. No new timezone layer.
- **`crate::reconcile::fold_status`** folds every stored CANCEL and REQUEST of the account into a `StatusIndex` and stamps three derived fields onto the event: `cancelled`, `superseded`, `cancelled_instances`. Derived on every pass, never persisted, exactly like the REPLY fold, so arrival order is irrelevant: a CANCEL ingested before its REQUEST tombstones it just the same.
- **UX decision (the ticket left it open): a CANCEL tombstones, it never deletes.** The event stays on the agenda and in the mail preview, with a red "Cancelled by the organizer." banner leading the shared event card and the `cancelled` badge on the agenda row. Nothing on disk is removed, and the sidecar ics is untouched.
- **Sequence rules.** A CANCEL applies to a copy whose `SEQUENCE` is at or below its own (`>=`, so an Outlook CANCEL that does not bump still lands); a stale CANCEL below the surviving REQUEST is ignored. A REQUEST is superseded only by a strictly greater `(SEQUENCE, DTSTAMP)`, so a replayed or re-delivered older copy can never clobber newer local state.
- **Recurrence.** A CANCEL carrying a `RECURRENCE-ID` kills that occurrence only: the series row stays live and lists the cancelled occurrences on its card. An occurrence override (`RECURRENCE-ID` on a REQUEST) is its own agenda row rather than a duplicate of the series.
- **RSVP guard.** `V` refuses a cancelled *or* superseded version in the mail view and in the Calendar view, so no reply goes out carrying a `SEQUENCE` the organizer has already moved past.
- **Degradation.** A malformed or UID-less CANCEL costs itself and nothing else: the event it names stays live, the agenda still builds, ingest is untouched.

Follow-up to the v1 iMIP work. Design: [calendar-invites](../plans/calendar-invites.md) (D8, v1/v2 boundary). Out of scope for the first cut; captured so it is not lost.

## Scope (v2)

- Handle received `METHOD:CANCEL`: mark the local invite cancelled, update the card/badge, keep the sidecar `.ics`.
- Handle updates: a re-issued invite with a bumped `SEQUENCE` supersedes the prior one; reconcile against the existing `UID`.
- ~~Send-side updates/cancellations from `mp` (re-send with bumped `SEQUENCE` / `METHOD:CANCEL`).~~ Split to [#0084](0084-imip-send-cancel-and-update.md).
- ~~Per-occurrence responses and creation of recurring invites (v1 handles whole series only, D6).~~ Split to [#0084](0084-imip-send-cancel-and-update.md); per-occurrence *cancellations* on the receive side did ship here.

## Related

- Depends on [#0027](0027-imip-receive-parse.md) (already tags `method: CANCEL` but takes no action in v1).
