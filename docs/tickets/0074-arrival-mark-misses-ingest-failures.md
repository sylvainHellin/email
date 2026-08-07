---
id: 0074
title: An ingest failure is not carried by the arrival mark
type: bug
priority: later
status: open
created: 2026-08-07
---

Deferred note (N4) from the fresh-context review of the [#0072](0072-quick-sync-misses-server-side-removals.md) sweep fix, commit `c366308`, not a regression of it.
Effort: S.

`arrival_coverage` (`src/imap_client/fetch.rs:269-288`) measures the set the pass *downloaded*, not the set it managed to write.
A message that was fetched and then failed to ingest therefore reads as covered, the pass reports itself complete and persists no arrival mark, and the only trace of the failure is the `ingest_failed` half of that pass's coverage tuple (`src/imap_client/store_sync.rs:214-217`), which lives for the length of the pass and holds the prune back once.
The next pass stands on a floor above the message it never wrote, so the gate is open while a listed message is still missing from the store.

The download window makes this narrow in practice: the failed message is below the next pass's floor, so it is backlog, and a full sync brings it in the same way it brings in anything else the store missed.
It is still the one remaining way the gate can open over a hole, which is the property the mark exists to guarantee.

## Evidence

- `arrival_coverage(listed 1..=110, known 1..=100, ingested 101..=110)` is complete with `pending_mark = None`, whatever happened to the ten it downloaded.
- The ingest result is discarded before it reaches the coverage call: `store_sync.rs:205-211` logs the error and sets `ingest_failed`, and the UID never enters the `ingested` vector the coverage is computed from.
- Identical before and after c366308; the sweep fix changed how the mark is *derived* at first contact, not what counts as ingested.

## Scope

Carry the failure into the mark rather than only into the pass: the natural shape is for the mark to be derived from the UIDs actually written, so a failed ingest leaves an unmet arrival below the next pass's floor and the gate stays shut until a pass writes it.
Check first that this does not resurrect the stuck-mark failure mode #0072 closed: a message that fails to ingest deterministically (a parse the store rejects every time) must not hold the prune back for the whole account for good, so the answer probably needs the same "clears when the server stops listing it" escape the mark already has, or a bounded number of retries.

## Acceptance criteria

- A pass that downloads a message and fails to ingest it persists an arrival mark below that UID, pinned by a test.
- A permanently unwritable message does not suspend the prune indefinitely, pinned by a test.
- `cargo test` green.
