//! One process-wide tokio runtime shared by every TUI network action.
//!
//! Each background action (sync, send, search) and each watcher used to build
//! its own [`tokio::runtime::Runtime`] with `Runtime::new()` and tear it down
//! when the op finished, spinning up a worker thread per core plus a blocking
//! pool every time (#0095, performance audit §b.5). This module hands out a
//! single lazily-built multi-thread runtime instead, so those threads pay
//! `block_on` but not runtime construction and teardown.
//!
//! ## Why this is safe to share
//!
//! [`tokio::runtime::Runtime::block_on`] takes `&self`, so several OS threads
//! calling it on the same runtime concurrently is supported: the future runs
//! on the calling thread while spawned tasks and I/O use the shared worker
//! pool and reactor. Every call site here runs inside an OS thread spawned by
//! `std::thread::spawn` (or a watcher thread), never on a tokio worker thread
//! and never already inside a `block_on`, so no call can nest a runtime inside
//! another and trip tokio's "Cannot start a runtime from within the context of
//! another runtime" panic.
//!
//! ## Drop semantics
//!
//! A `static` runtime is never dropped, so its worker/blocking threads live for
//! the process lifetime and are reclaimed by the OS at exit. Nothing here
//! relies on runtime teardown for cleanup: IMAP sockets are owned by the
//! process-global async-std reactor and a pooled session cache (`imap_client`),
//! SMTP/Graph connections are closed by their own futures completing, and the
//! previous per-op runtimes were dropped only incidentally when an op ended,
//! never to run shutdown work.

use std::sync::LazyLock;

use tokio::runtime::Runtime;

/// The shared multi-thread runtime. Built on first use and kept for the
/// process lifetime.
static SHARED: LazyLock<Runtime> =
    LazyLock::new(|| Runtime::new().expect("failed to create the shared tokio runtime"));

/// Borrow the process-wide tokio runtime. Callers `block_on` it exactly as
/// they did the throwaway per-action runtime; the only change is that the
/// runtime outlives the call.
pub(super) fn shared() -> &'static Runtime {
    &SHARED
}
