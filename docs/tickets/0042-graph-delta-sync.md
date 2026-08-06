---
id: 0042
title: Graph /messages/delta with persisted deltaLink (replace poll / disk scan)
type: perf
priority: later
status: open
created: 2026-07-14
---

Parked 2026-08-06: the Graph backend is parked until it is picked up deliberately, first live target the EVOQS Exchange account; see [BACKLOG](../../BACKLOG.md).

Stage 5 (Graph) of the data-access-layer redesign. Plan: [data-access-layer](../plans/data-access-layer.md).

The single biggest Graph-account win: today Graph accounts disk-scan on every sync.
Replace the poll + scan with the delta query.
Depends on the store (Stage 2) for cursor persistence; complements [#0041](0041-persistent-conn-condstore.md).

## Scope

1. Use Graph `/messages/delta` per folder; persist `deltaLink` in `sync_cursors`.
2. Engine pull loop drives the delta query and writes changed envelopes/flags to the store, blobs for new bodies.
3. Remove the 60 s poll + every-sync disk scan for Graph accounts.

## Acceptance criteria

- Graph quick sync issues a delta query, not a full scan; `[TIMING]` confirms.
- A change made in Outlook web propagates into the store on the next sync via the delta.
- `deltaLink` survives restart and resumes incrementally.

## Related

- Graph sync backend for calendar / server-side RSVP is a separate track ([#0036](0036-graph-sync-backend.md), blocked on [#0035](0035-graph-admin-approval.md)).
