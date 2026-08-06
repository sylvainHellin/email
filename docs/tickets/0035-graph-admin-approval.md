---
id: 0035
title: Microsoft Graph API admin approval + Azure app verification
type: chore
priority: later
status: blocked
created: 2026-07-11
---

Parked 2026-08-06: the Graph backend is parked until it is picked up deliberately, first live target the EVOQS Exchange account; see [BACKLOG](../../BACKLOG.md).

Gating task for any Microsoft Graph-based sync backend. Design context: [calendar-invites](../plans/calendar-invites.md) (D1), [tui-restructure-views](../plans/tui-restructure-views.md) (D12).

The TUM M365 tenant requires **IT-admin approval** for Graph API access, which is currently unobtainable. Until that is resolved, all calendar work is iMIP-only (no Graph). This ticket tracks the operational/administrative path so the code work ([#0036](0036-graph-sync-backend.md)) is unblocked when — and only when — approval lands.

## Scope (operational, not code)

- Azure app **publisher verification** / app registration hardening as required for admin consent.
- Add the calendar/contacts scopes to the Azure app "API permissions" (`Calendars.ReadWrite`, `Contacts.Read`) — see `docs/exchange-setup.md` Step 3 pattern.
- Obtain **tenant admin consent** for the TUM M365 tenant.
- Document the approved scope set and consent state in `docs/auth.md` / `docs/exchange-setup.md`.

## Status

`blocked` — admin approval currently unobtainable. Revisit if the tenant policy changes.

## Blocks

- [#0036](0036-graph-sync-backend.md) Graph sync backend.
