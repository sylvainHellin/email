---
id: 0059
title: Extract a SyncBackend trait so sync orchestration is written once
type: refactor
priority: later
status: open
created: 2026-08-06
---

From the architecture review synthesis, Tier 2 item 1: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: L, the largest structural item in the review.

Six paired IMAP and Graph implementations differ only in transport.
The abstraction already exists implicitly, which is why [#0055](0055-graph-sync-parity.md) exists at all: every fix applied to one path has to be re-applied by hand to the other, and the review found six places where that did not happen.

## Evidence

- Paired implementations: sync orchestration (`src/imap_client/store_sync.rs` versus `src/graph.rs:587` `sync_mailboxes_graph`), the TUI wrappers, search (`src/imap_client` versus `src/graph.rs:1030` `search_messages`), ops (`src/graph.rs:961-1028` `move_message_graph`, `delete_message_graph`, `mark_read_graph`), the watcher (`src/tui/helpers.rs:41` versus `:99`), and the CLI sync command.
- `src/graph.rs` already imports `FreshObservation`, `SyncResult` and `SyncTarget` from `imap_client`, which is the abstraction admitting it exists in the wrong module.
- The orchestration that would be written once is ingest, fresh observations, notify, cursor recording, prune and the contacts hook; `sync_mailboxes_graph` (`src/graph.rs:598-686`) is that sequence minus prune.
- `src/imap_client/store_sync.rs`, `ops.rs` and `batch.rs` have zero tests despite three recent prune fixes in the same 264 lines; there is no mockable boundary to test against.
- The Graph half is worse, and the fresh-context review of [#0055](0055-graph-sync-parity.md) (deferred note 7) is the evidence: dev-dependencies are `tempfile` and `insta` only, with no HTTP mock, so there is no test of `fetch_new_messages` or `sync_mailboxes_graph` at all.
  The six unit tests #0055 added exercise only the helpers it introduced, so none of them could have failed against the old code; the second-pass prune ordering itself is verified by reading `graph.rs` against `store_sync.rs` and calling it parity.
  A fake backend behind this trait is what makes that orchestration testable, which is the concrete reason to sequence it before the next Graph change rather than after.

## Scope

1. Define `SyncBackend` with the per-transport operations only: list folders, fetch a window, fetch the flag or read-state set for a folder, move, delete, set read.
2. Write the orchestration once against a `Pull` struct covering ingest, observations, notify, cursor, prune and the contacts hook, and have both transports call it.
3. Move `FreshObservation`, `SyncResult` and `SyncTarget` out of `imap_client` into the shared module.
4. Add a fake backend and the first sync-engine tests, following the `SentMailbox` fake-trait pattern already in the suite.

## Acceptance criteria

- Adding a prune fix or a timing mark touches one file, not two.
- The sync engine has tests that run offline against a fake backend, covering at least prune, uid rebinding and cursor advance.
- No behaviour change for either transport, verified against the `dump-mailbox` parity oracle.

## Sequencing

Extract this before [#0041](0041-persistent-conn-condstore.md) and [#0042](0042-graph-delta-sync.md), not as a competing refactor: both tickets assume this seam, and both would otherwise widen the duplication they are meant to remove.
