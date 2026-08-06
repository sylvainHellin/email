---
id: 0061
title: Engine advisory lock on store.lock (mp sync plus an open TUI is a live double writer)
type: refactor
priority: later
status: open
created: 2026-08-06
---

From the architecture review synthesis, Tier 3: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: M.

The data-access-layer plan specifies a single-writer engine guarded by an advisory lock.
The lock was never built, and the second writer it was meant to exclude exists today.

## Evidence

- `docs/plans/data-access-layer.md:207` specifies a non-blocking exclusive advisory lock, `flock`-style, on `<account_dir>/store.lock`, taken by whichever process runs the engine and released when that process exits or dies.
- `rg 'store.lock|flock' src/` returns nothing, so the mechanism does not exist in the crate.
- The double writer is reachable now: `mp sync` in one terminal and an open TUI both ingest, both compute prune sets from windows the other is mutating, and both drive `resume_outbox` (`src/send.rs:1212`).
- It becomes destructive rather than merely wasteful once [#0039](0039-pending-ops-queue.md) lands, because two drains would race on the same queue rows.

## Scope

1. Take a non-blocking exclusive `flock` on `<account_dir>/store.lock` at engine start.
2. A process that cannot take the lock degrades to read-only against the store rather than failing, and says so.
3. Release on exit and on crash, which `flock` gives for free; document the takeover behaviour.
4. Cover the `mp sync` plus open TUI case explicitly in the tests or in the acceptance run.

## Acceptance criteria

- `mp sync` while the TUI is open does not double-ingest and does not double-drain the outbox.
- Killing the lock holder lets the next process acquire the lock without manual cleanup.
- The read-only degradation is visible to the user, not silent.

## Relationship to #0039

This may fold into [#0039](0039-pending-ops-queue.md) rather than ship alone: #0039 needs the lock to make its drain safe, and the review recommends pulling it in there.
Kept as its own ticket so the pre-#0039 exposure stays visible; close it as a duplicate if #0039 absorbs it.
