---
id: 0055
title: Graph backend sync parity (prune, converge, watcher, transactions, timing, backoff)
type: bug
priority: now
status: done
created: 2026-08-06
---

From the architecture review synthesis, Tier 1: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: S each except the converge fix, which is M.

The recent IMAP sync fixes never reached the Graph path.
Six defects, all standalone, none previously tracked, batched here because they live in the same two functions.

## Evidence

- No prune: `sync_mailboxes_graph` (`src/graph.rs:587-687`) ingests, applies read flags and records a cursor, and never deletes rows for messages the server no longer has, so mail archived or deleted in Outlook web stays locally forever.
  The data is already in hand: `server_flags` covers the whole folder, so the prune-after-all-ingests pattern used on the IMAP side applies directly.
- No-converge loop: detection uses `fetch_new_messages(&target.server_name, limit, &known)` (`src/graph.rs:604`), which is folder-wide, while the download window is recency-ordered and capped at `limit`. A new-but-not-recent message, for example one moved into Archive, is reported new on every sync and never downloaded.
- Watcher compares folder cardinality: `graph_watcher_loop` (`src/tui/helpers.rs:99-142`) polls `fetch_message_ids("inbox").await?.len()` every 60 s and fires only when the integer changes, so one arrival plus one archive in the same window is invisible.
  It also constructs a fresh `GraphClient` on every pass (`helpers.rs:120`), paying a token decrypt and a new connection pool each minute, and `fetch_message_ids` pages a 5000-message inbox to produce one number.
- O(folder) flag updates: `src/graph.rs:661-669` calls `apply_seen_flag` once per message in autocommit mode, and the IMAP side has the same shape.
- No `TimingSpan` marks anywhere on the Graph path, so none of the above is visible in `[TIMING]` logs. [#0042](0042-graph-delta-sync.md) has an acceptance criterion that needs them.
- `graph_watcher_loop` swallows every error into a `log::warn!` (`helpers.rs:134`) with no backoff and no `WatchEvent::Error`. A revoked token becomes one silent failed request per minute, forever.

## Scope

1. Prune vanished messages after all ingests for a target complete, from the `server_flags` id set.
2. Fix new-message detection so it converges: either page the detection window in the same order as the download window, or download exactly the ids detection reported.
   Superseded by [#0042](0042-graph-delta-sync.md); do this standalone only while #0042 is far off.
3. Watcher: compare id sets rather than counts, and build one `GraphClient` outside the loop.
4. Wrap the per-message flag updates in one transaction, on both the Graph and the IMAP path.
5. Add `TimingSpan` marks to the Graph sync phases, matching the IMAP naming.
6. Watcher backoff on consecutive failures, sharing `outbox::backoff_secs`, and a `WatchEvent::Error` after the threshold so a revoked token reaches the user.

## Acceptance criteria

- A message archived in Outlook web disappears locally on the next `mp sync`.
- A message moved into Archive on the server is downloaded once and not re-detected as new on the following sync.
- One arrival plus one archive inside a single watcher interval still raises a change event.
- `[TIMING]` output for a Graph sync has the same phase breakdown as an IMAP sync.
- A revoked Graph token produces a visible error in the TUI and a widening poll interval, not a silent per-minute failure.
