---
id: 0074
title: An ingest failure is not carried by the arrival mark
type: bug
priority: later
status: done
created: 2026-08-07
closed: 2026-08-11
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

## Resolution (2026-08-11)

The mark is now derived from what the pass *wrote*, not from what it downloaded, and the retry that follows is bounded rather than open-ended.

- `fetch::mark_below_unmet(pending, unmet)` (`src/imap_client/fetch.rs`) lowers the mark the download reported to `min(pending, lowest_unwritten - 1)`.
`arrival_coverage` is unchanged: it still answers "what did this pass download", which is all it can know, and the ingest loop that learns the rest applies the correction where that knowledge lives (`store_sync.rs`, right before `record_mailbox_cursor`).
Both failure kinds feed it, a message that does not parse and a store write that errored, because they are indistinguishable from the store's side: the message is not there.
- The bound is `ingest_failures (account, mailbox, uid, attempts, last_error, updated)`, schema v6, written by `ingest::record_ingest_failure` and deleted by `ingest::clear_ingest_failure` on every successful ingest, so transient failures never accumulate towards it.
After `ingest::MAX_INGEST_ATTEMPTS` (3) failed passes the UID is given up on with a loud `warn!`, drops out of the pass's `unmet` set, and stops both lowering the mark and reporting the pass short.
The escape the ticket asked for is therefore *both* available: the mark still clears when the server stops listing the message, and three passes is the ceiling when it does not.
Clearing on success is what keeps the counter honest across a UIDVALIDITY reset, where the same UID can name a different message.
- The poisoned-message decision: one message that will not ingest costs the batch nothing but the prune.
The ingest loop already `continue`s past every failure, so the rest of the window is written normally; the failure is carried in the mark rather than by abandoning the pass.
Skipping the message entirely (never retrying) was rejected because a locked store or a lost blob write is transient and a message dropped for it would be invisible until a full sync.
- Schema v6 drops and refills every existing store on the next open, which is the standing contract for a version bump and costs nothing but a resync.

Pinned by `a_failed_ingest_holds_the_mark_down_and_the_retry_writes_the_message_once` (mark below the failed UID, `pass_may_prune` false, retry writes it, third ingest of the same UID inserts nothing: exactly one row), `a_permanently_unwritable_message_stops_holding_the_prune_after_three_passes` (the give-up, and the gate reopening with it), `a_poisoned_message_does_not_wedge_the_rest_of_the_batch` (the UIDs either side are written and `known` to the next pass) in `src/imap_client/store_sync.rs`, and `an_unwritten_uid_pulls_the_mark_below_itself` in `src/imap_client/fetch.rs`.
