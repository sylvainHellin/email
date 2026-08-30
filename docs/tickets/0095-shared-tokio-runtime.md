---
id: 0095
title: One shared tokio runtime instead of a fresh runtime per network action
type: chore
priority: later
status: done
created: 2026-08-14
---

## Resolution (2026-08-14)

Added `src/tui/runtime.rs`: a `LazyLock<tokio::runtime::Runtime>` (default
multi-thread) exposed as `runtime::shared() -> &'static Runtime`. Replaced the
11 per-action `Runtime::new().expect(...)` sites in `src/tui/actions.rs` and the
2 watcher sites in `src/tui/helpers.rs` with `super::runtime::shared()`. Call
sites keep `rt.block_on(...)` unchanged (`block_on` takes `&self`), so blocking
semantics are identical.

Multi-thread (not `new_current_thread`) because several OS threads (background
actions plus the two watcher threads) call `block_on` on the shared runtime
concurrently, which a current-thread scheduler does not serve well; a
multi-thread runtime supports concurrent `block_on` from many threads and shares
one worker pool.

Nesting safety: every site runs inside an OS thread from `std::thread::spawn` or
a watcher thread, never on a tokio worker thread and never already inside a
`block_on`, so no call can nest a runtime and trip tokio's "Cannot start a
runtime from within the context of another runtime" panic. `oauth2.rs` and
`config_cmd/helpers.rs` (out of scope) already guard nested cases with
`Handle::try_current()` and offload to a fresh thread, so token refresh under a
`block_on` future stays safe unchanged.

Drop semantics: a `static` runtime is never dropped; nothing relied on the old
per-op runtimes' teardown for cleanup (IMAP sockets live on the async-std global
reactor and the pooled session cache; SMTP/Graph connections close when their
futures complete). Threads are reclaimed by the OS at process exit.

Out of scope, left as-is: the 3 `Runtime::new().unwrap()` sites in `actions.rs`
tests (one runtime per test, not a per-action hot path), and the `oauth2.rs` /
`config_cmd/helpers.rs` CLI runtimes (not TUI network actions).

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
