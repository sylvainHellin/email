---
id: 0095
title: One shared tokio runtime instead of a fresh runtime per network action
type: chore
priority: later
status: open
created: 2026-08-14
---

A fresh multi-threaded tokio runtime is created for every network action (performance audit §b.5, confidence 0.9 existence, low-to-moderate impact).
`handle_action` spawns a thread and calls `tokio::runtime::Runtime::new()` per op, across 21 sites in `src/tui/actions.rs` (for example `:1125`, `:1671`, `:1742`, `:1892`) plus `src/tui/helpers.rs:50`, `:134`.
`Runtime::new()` builds a full multi-thread runtime, a worker thread per core plus a blocking pool, torn down when the op finishes.

The effect is thread-pool spin-up and teardown on every sync, send, and search.
It is not a UI freeze, since it runs off-thread, and IMAP I/O runs on async-std's global reactor anyway, so correctness and pooling are unaffected.
It is repeated wasted work and memory churn on a make-it-snappy target.

## Scope

1. Build one shared runtime once and reuse it across the 21 action sites, or use `Builder::new_current_thread` given the actual I/O reactor for IMAP is async-std's global one.
2. Replace the per-op `Runtime::new()` call sites in `src/tui/actions.rs` and `src/tui/helpers.rs` with the shared handle.

## Acceptance criteria

- No `Runtime::new()` is called per action; a single runtime (or a current-thread runtime) is reused across sync, send, and search.
- No regression in IMAP, SMTP, or Graph network actions.
