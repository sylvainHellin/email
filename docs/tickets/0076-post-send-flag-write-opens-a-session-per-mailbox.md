---
id: 0076
title: The post-send flag write opens one IMAP session per mailbox
type: perf
priority: later
status: done
created: 2026-08-08
closed: 2026-08-11
---

Deferred note (N4) from the fresh-context review of [#TKT-0051](TKT-0051-email-status.md), commit `04d311c`, not a regression of it.
Effort: M.

`send::mark_source_after_send` (`src/send.rs:1782-1826`) flags the source of a reply or a forward on the server after the message has gone out.
It loops over the distinct server mailboxes the source has a copy in and calls `imap_client::add_flag_on_server` once per mailbox, and that function opens its own session: connect, TLS, login, SELECT, UID SEARCH, UID STORE, logout (`src/imap_client/ops.rs:192-227`).
A source filed in inbox, archive and sent, which the live validation of #TKT-0051 observed, is three sequential logins on the tail of a send.
`mp send-approved` multiplies that per reply draft in the batch (`src/main.rs:1443-1447`).

This is the first IMAP round trip ever performed inside `send_draft`, so it is a new cost on a path that had none.

## What it is not

The review that raised this stated that the TUI runs `send_draft` under `rt.block_on` on the event loop, and therefore that a black-holed IMAP host freezes the TUI after a successful send.
That premise does not hold: both TUI send paths already run on a spawned thread with their own runtime (`Action::Send` at `src/tui/actions.rs:957-967`, `Action::SendApproved` at `src/tui/actions.rs:1089-1126`), and only that thread waits.
What a hung host costs the TUI is a background thread parked for the duration and a `Sending...` progress line that does not resolve; the event loop keeps drawing and keeps taking keys.

That is why this is filed rather than fixed in the #TKT-0051 sweep: the latency is real, the freeze is not.

## Stopgap already shipped

`open_imap_session` had no connect timeout at all, so a black-holed host (a dropped route, a firewall swallowing the SYN) held any IMAP caller for the OS default, over two minutes on Linux.
The sweep bounded the TCP connect at `CONNECT_TIMEOUT_SECS = 30` (`src/imap_client/mod.rs`), which caps the worst case for every IMAP path, this one included.
Nothing else in the session setup is bounded, which is deliberate: a server that answered the SYN and then went quiet fails its read.

## Scope

Take the flag write off the per-mailbox session, keeping best-effort semantics: it runs after a message has already been delivered and must never fail, retry or delay a send.

Two directions, in increasing order of cost:

1. One session for all of a source's mailboxes.
   `add_flag_on_server` grows a multi-mailbox form the way `batch_move_on_server` already batches over one connection (`src/imap_client/batch.rs`), and `mark_source_after_send` hands it the whole list.
   Small, local to `imap_client` plus one loop in `send.rs`, and it fixes the N-logins part for both the CLI and the TUI.
2. Off the send path entirely.
   `tui::mutations::run_imap` and its `Homogeneous` batching are the TUI's home for server ops, but `mark_source_after_send` lives in the library and is shared with the CLI, so routing it there would invert the layering and leave `mp send-approved` untouched.
   The direction that serves both is the post-send settle: the flag write becomes a queued op that `drain_account` / `resume_outbox` picks up alongside the pending APPENDs (`src/send.rs:1838-1911`), which is where a "the message is out, this is bookkeeping" write belongs.
   Check the #0063 durability invariants before moving anything into that path: the outbox state machine and the exactly-once submission marker must not gain a new way to be touched after `record_submission`.
   This is the shape #0039 (durable `pending_ops` queue) would subsume, so it may be cheaper to wait for #0039 than to build a private queue here.

## Known gap, same area (review note N5)

Replying to a server-search hit that did not resolve to a store row flags nothing, anywhere.
`draft::source_from_fetched` (`src/draft.rs:156`) carries the hit's `Message-ID` into the draft, but `mark_source_after_send` looks the source up locally first and returns early when `find_by_message_id` finds nothing (`src/send.rs:1786-1789`), which skips the server half too, even though the hit knows the server mailbox it came from.
The early return is right for the case its test names, a source deleted since the draft was written (`src/send.rs:465-471`); it just also swallows this one.

Closing it needs the source's mailbox to reach `send_draft`, and the draft file is the only channel between writing a reply and sending it (`mp send-approved` may run in another process, days later).
That means a new frontmatter key, which is a draft-format change and a website-documentation change, so it is a decision rather than a sweep fix.
Left as a known gap: the local half of the axis is correct for every hit that resolved, which is every hit after the next sync.

## #0039 verdict (2026-08-11): not subsumed

The durable `pending_ops` queue landed its core in [#0039](0039-pending-ops-queue.md) (queue engine, engine lock, op-seam extraction).
It drains user mutations (archive, delete, move, mark-read), not the post-send `\Answered` / `$Forwarded` bookkeeping this ticket is about.
Direction 2 here ("the flag write becomes a queued op") could ride that queue, but only once #0039's deferred consumer wiring exists and a best-effort op kind that must never fail a delivered send is added, so nothing in the #0039 core pass subsumes this.
Still open; revisit alongside the #0039 TUI/CLI wiring.

## Acceptance criteria

- One IMAP session for a source held in N mailboxes, pinned by whatever the chosen direction makes testable offline.
- A send still cannot be failed, delayed past delivery, or retried by the flag write.
- `cargo test` green; the #0063 outbox invariants unchanged.

## Implementation (2026-08-11): direction 2, on the #0039 queue

Both directions landed, because direction 2 needs a server op and the cheapest correct one is direction 1's multi-mailbox form.

- **The op.** `ServerOp::SetAnswered { message_id, mailboxes, answered }` (`src/ops.rs`), kind `set_answered`, the one multi-mailbox op: a source is one store row per mailbox and the same server message in each, so naming the list lets the drain write them all over a single session.
  `imap_client::add_flag_in_mailboxes` (`src/imap_client/batch.rs`) is that session: open once, SELECT/SEARCH/`UID STORE +FLAGS` per folder, logout.
  Idempotent (`+FLAGS` on a flag that may already be set is a no-op) and not-found tolerant per folder, so a crash replay converges; the drain's typed `NotFoundOnServer` convergence covers the rest.
  Graph enqueues nothing at all: the send path passes an empty mailbox list, and the Graph arm of `run_op` is a logged `Ok` for safety.
- **The rollback is `Rollback::None`, and that is the considered answer.** The answered bit records something that happened: the reply left the building. A server that refuses the `UID STORE` does not make that untrue, so rolling the local bit back would replace a true statement the next sync can correct with a false one. This is also the one op kind whose local half is written on the tail of a delivered send, so an automatic undo there is the last thing wanted. Convergence is the sync, which restates every flag the server holds over the whole window.
- **The send path.** `send::mark_source_after_send` now reads the source's rows, deduplicates their server folders (`server_mailboxes_of`) and calls `pending_ops::apply_post_send_flag`, which commits the local flag on every copy and the single op in one transaction. No IMAP session is opened on the send path at all; the cost is one `COMMIT`. Every error is still logged and swallowed, the function still returns `()`, and the enqueue happens strictly after delivery and touches no `outbox` row, so the #0063 exactly-once submission marker is untouched by this path.

### Acceptance

- One IMAP session for a source held in N mailboxes: `pending_ops::a_post_send_flag_writes_every_copy_and_queues_one_op` pins two folders on one op; `send::the_server_mailbox_list_is_deduplicated` pins the dedup.
- A send cannot be failed, delayed or retried by the flag write: `pending_ops::a_refused_post_send_flag_never_re_sends_the_message` drives the flag op past its retry budget after a recorded submission and asserts the outbox row keeps its terminal state, `sweep_pending_sends` finds nothing resubmittable, the answered bit stands and the refusal is visible as a `failed` queue row.
- `a_post_send_flag_replay_converges` pins the crash replay.

### Known gap N5 unchanged

Replying to a server-search hit with no local row still flags nothing: `apply_post_send_flag` finds no rows and queues nothing, exactly as the old path returned early. Closing it still needs a draft-frontmatter key, still a decision rather than a sweep fix.
