---
id: 0039
title: Durable pending_ops queue for flag, move and delete mutations
type: refactor
priority: later
status: done
created: 2026-07-14
---

Stage 3 of the data-access-layer redesign, amended 2026-07-31 for the complete nuke.
Plan: [data-access-layer](../plans/data-access-layer.md).
Foundation plan: [2026-07-31_dal-foundation-plan-v2](../../.agents/handoff/2026-07-31_dal-foundation-plan-v2.md), amendment 8.

The sent-durability half of this ticket has moved into [#0037](0037-sqlite-store-engine-skeleton.md) as the durable outbox, because the nuke removes the local sent `.md` on day one and leaving send best-effort until this stage would be a regression the owner has to live with in between.
What remains here is the generic drain for the other mutations: archive, delete, move, and mark-read or mark-unread.

The old mitigation ("lands behind the still-present file layer so a queue bug cannot lose mail") is void, since there is no file layer.
The replacement mitigation is narrower and honest: these ops change server-side state that the server itself still holds, so a queue bug loses a flag change or delays a move, not a message.

## Scope

1. `pending_ops(id, kind, target_message_id, payload, state, attempts, last_error, created)` drained by the engine; kinds: archive, delete, move, mark-read, mark-unread. Send is not a kind here; it lives in `outbox`.
2. TUI `Action` handlers enqueue an op and apply the local store change, instead of spawning a per-op IMAP task. `BgResult` carries op state transitions back to `App::update`.
3. Retry with exponential backoff; pending and failed ops surfaced in the UI.
4. Crash-safety: ops replay on startup, with a guard against duplicate apply and against stuck rows.
5. Kill the "Quick sync queued (N ops pending...)" pattern, owner directive 2026-08-05 from first live use of the branch build: a quick sync requested while a background job runs is queued and each request logs its own Activity line, stacking duplicates.
   With ops in a durable queue drained by the engine there is nothing for the user-visible sync to wait behind: mutations enqueue silently, the drain reports per-op state transitions, and the Activity log gets one consolidated line per state change instead of one per keypress.

## Amendment 2026-08-06 (architecture review)

From [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md), Tier 2 item 2 and Tier 3.

- Mutation-path unification belongs in this scope: the TUI writes locally first and rolls back (`src/tui/mutations.rs`, the right seam in the wrong module), the CLI writes server-first with an inline store write and no rollback (`src/main.rs:1946+`).
  Move `mutations.rs` to `src/ops.rs`, make the ordering a parameter, and have both consumers call it; this ticket has to unify them anyway to enqueue ops.
- `pending_ops` schema decisions to take here: keep `target_message_id` as the `messages` row id for correlation and carry the full addressing in the JSON payload, rather than storing the String Message-ID in an INTEGER column.
  The `updated` column and the account-column convention are handled by [#0054](0054-schema-bump-bundle.md).
- Ops are addressed by Message-ID through a full-mailbox `UID SEARCH HEADER` although the store row already knows the exact `(mailbox, uid)`.
  Carry the uid on `ServerOp` and keep the search as a fallback for sentinel UIDs only.
- The engine advisory lock ([#0061](0061-engine-advisory-lock.md)) is a fold-in candidate: two drains racing on this queue is the point where the missing lock turns destructive.

## Acceptance criteria

- A mutation reflects instantly in the UI and is confirmed or retried in the background with visible feedback.
- No "Quick sync queued" stacking: requesting a sync during a background job either coalesces into the running drain or replaces the queued request; duplicate Activity lines for the same pending state are gone.
- Kill the process mid-op; on restart the op replays exactly once and converges.
- A delete that the server rejects surfaces as a failed op with its error, and the local row returns to the server's state rather than staying optimistically changed.
- Test harness decision made (IMAP mock versus accepted live-server validation) and documented.

## Unblocks

- [#0040](0040-drop-file-layer-cutover.md) (the legacy tree can be retired once every mutation is durable).

## Review fix (blocker 2, crash-replay convergence)

The first landing claimed the server op was "idempotent by Message-ID, a no-op on both backends", which was false on the crash-replay window (server op succeeded, crash before the row retired).
Three not-found paths returned a plain `Err`: `imap_client::move_email_on_server` / `delete_email_on_server` on an empty UID search, and `graph::mark_read_graph` on a not-found id.
A replayed IMAP move therefore retried to `MAX_ATTEMPTS`, parked a succeeded op as `failed`, and rolled the local row home, diverging from the server and surfacing a false failure.
The fix is a typed not-found signal (`ops::NotFoundOnServer`), not string matching: those three backends return it, and `pending_ops::drain` treats it as a converged replay (retire the row, no rollback), while every other error stays a genuine failure that rolls back once the budget is spent.
The IMAP flag ops and Graph move/delete already returned `Ok` on not-found, so all four op kinds converge on replay.
Direct CLI and TUI callers are untouched: the typed error's `Display` is byte-identical to the old messages, so a user deleting a message the server no longer holds still sees the not-found error.
Also this pass: the crash-replay test now scripts the real not-found and asserts convergence (paired with a genuine-error rollback regression), `pending_ops::drain` is `pub(crate)` so no caller drains outside `drain_account`'s engine lock, and `store::rebuild` documents that `pending_ops` is deliberately dropped on a rebuild (a pending mutation is server-recoverable, unlike an outbox submission).

## Implementation status (2026-08-11)

Landed this pass, the durability core.
The approved increment boundary puts the review-gated half in a tight, fully offline-tested diff and gives the product-visible TUI rewiring its own pass.

- `src/pending_ops.rs`: the durable queue, the mutation twin of `src/outbox.rs`, following its patterns rather than inventing parallel ones.
  The local write and the queue row commit in one transaction (`apply_move`, `apply_delete`, `apply_set_read`, `apply_set_flagged`).
  A background `drain` retires successful ops and backs off transient failures on the outbox's `backoff_secs` curve, and past a retry budget it rolls the local state back to the server's and parks the row `failed`.
  Replay is exactly-once for the local half by construction: the write happens once inside the commit and the drain never re-applies it, only the server op, and the drain converges that op on replay (see the review-fix note below).
  States are `queued` and `failed` only, and a successful op deletes its row, because a done mutation (unlike a partial-delivery send) has nothing to retain.
- `src/ops.rs`: the amendment's "move `mutations.rs` to `src/ops.rs`" for the parts the queue needs.
  `ServerOp`, `Backend` and `run_ops` now live at library layer, re-exported from `tui/mutations.rs` so the TUI call sites read unchanged.
  A remote op is email logic, which the TUI-implements-no-email-logic invariant keeps out of `tui/`.
  The prepare/rollback pairing stays in `tui/mutations.rs` for now because it is keyed on `MessageRef`, a TUI type, and relocating it belongs with the consumer rewiring below.
- `src/engine_lock.rs`: the engine advisory lock ([#0061](0061-engine-advisory-lock.md) folded in), a non-blocking `flock` on `<account_dir>/store.lock`.
  Two drains racing on the queue is the destructive case it excludes.
  A process that cannot take it degrades to read-only and lets the holder drain, which `drain_account` gates on.

Decisions taken where the ticket left a choice, smallest design consistent with the outbox precedent:

- **Addressing.**
  `target_message_id` is the `messages` row id (amendment), and the full server addressing rides in the JSON payload as a `ServerOp`, which names the message by `Message-ID`.
  The uid fast-path the amendment notes (carry the uid on `ServerOp`, keep the full-mailbox `UID SEARCH HEADER` as a fallback for sentinel UIDs) is deferred: it is a latency refinement, not a durability guarantee, and it would widen every `imap_client::ops` signature.
  The first cut keeps the Message-ID addressing the outbox's Sent dedup already relies on.
- **Failure classification.**
  The `imap_client` and Graph op functions return a plain error with no permanent-vs-transient verdict, unlike the outbox's SMTP classification, so the drain retries a bounded `MAX_ATTEMPTS` times under backoff and then surfaces the failure.
  A standing refusal (a delete the server rejects) surfaces after the budget rather than on the first attempt.
  This satisfies the acceptance criterion, a failed op with its error and the local row returned to the server's state, without inventing a classifier the backends cannot feed.
- **Test-harness decision (acceptance criterion).**
  An in-memory `OpExecutor` fake, exactly the seam `outbox::SentMailbox` uses, not a live-server validation and not an IMAP mock server (there is none, and building one is [#0059](0059-syncbackend-trait.md)).
  The crash-safety, replay, idempotency, backoff and per-kind rollback paths are all covered offline and deterministically in `src/pending_ops.rs`.

### Absorption verdicts

- **[#0061](0061-engine-advisory-lock.md): absorbed.**
  The lock lands here as `src/engine_lock.rs`; closed as a duplicate.
- **[#0076](0076-post-send-flag-write-opens-a-session-per-mailbox.md): not subsumed.**
  #0076 is about the send path (`mark_source_after_send` opening one IMAP session per mailbox), which lives in the library and is shared with `mp send-approved`.
  The durable queue drains user mutations (archive, delete, move, mark-read), not the post-send `\Answered` / `$Forwarded` bookkeeping, and routing that write through this queue would need the queue wired into `send_draft` and a new op kind for a best-effort flag that must never fail a delivered send.
  Its direction 2 ("the flag write becomes a queued op") could ride this queue once the consumer wiring below exists, but nothing in this pass subsumes it, so it stays open.
- **[#0079](0079-flagged-filter.md): not subsumed.**
  #0079 is a local read-side view (filter and sort `messages.flags` for `\Flagged`), with no server op and no queue.
  Untouched by this ticket, and left open.

### Remaining scope (its own pass and review)

The product-visible half, deferred deliberately:

1. Wire the TUI `Action` handlers to enqueue through `apply_*` and apply the local store change, instead of spawning a per-op IMAP task; `BgResult` carries op state transitions back to `App::update`; pending and failed ops surfaced in the UI (`pending_ops::counts` / `failed_ops` are ready for a badge).
2. Wire the CLI mutation commands (`mp archive`, `mp delete`) to the same seam, and relocate the `MessageRef`-keyed prepare/rollback pairing into `src/ops.rs` as part of that unification.
3. Run the drain from the existing resume points (startup and the sync tick, beside `resume_outbox`), under the engine lock, taking care not to add sync traffic against live accounts.
4. Kill the "Quick sync queued (N ops pending...)" stacking (owner directive 2026-08-05): with mutations in the durable queue there is nothing for the user-visible sync to wait behind.

## Implementation status (piece 4, the consumer wiring)

Landed the product-visible half, closing the ticket.

- **TUI.** `src/tui/mutations.rs` is now the TUI's entry into the queue: `queue_move` / `queue_delete` / `queue_read_flag` / `queue_flag` call `pending_ops::apply_*` (local write plus enqueue in one transaction) and return the rows they touched for the list update. The action handlers (`archive_msgs`, `delete_msgs`, `set_read_flag`, `set_flag`, the `MoveToMailbox` arm and the search-result archive) no longer spawn a per-op server thread, keep no rollback, and no longer touch `bg_count` / `bg_mutations`: the change is instant and the server op is retired in the background. The per-op `BgResult::{Archive,Move,Delete,ToggleRead,ToggleFlag}` variants and their `bg.rs` handlers are gone with the threads that fed them.
- **CLI.** `mp archive` and `mp delete` enqueue through the same `apply_*` and then run the op synchronously with `pending_ops::run_and_settle`, so the CLI keeps its blocking UX and its crash durability. `run_and_settle` retires the row on success and, on failure, rolls the local half back and returns the error verbatim; unlike the background drain it does not converge a not-found, because a synchronous caller is never a crash replay, so `mp delete` for a message the server no longer holds still prints the byte-identical not-found error.
- **Resume points.** The drain runs beside `resume_outbox` at the sync/fetch tick (`pending_ops::resume_account` in `lib_do_sync` / `lib_do_sync_graph`, and in the `mp sync` CLI path). Startup reaches it through the auto-fetch. `resume_account` builds no backend and takes no engine lock unless a row is owed, so a clean account adds no server traffic. A drain that rolls an op back names the failure in the sync line; the rolled-back row reappears when the sync refresh reloads.
- **Killed the stacking.** With mutations off the background-job counters, a sync requested during mutations is no longer parked, and `park_until_idle` no longer prints "(N ops pending)": the only thing a sync can wait behind is another sync or fetch.

Deferred, out of scope for the durability contract this ticket owns: a persistent status-bar badge for pending/failed ops (`counts` / `failed_ops` remain ready for one), and the uid fast-path on `ServerOp`. The vestigial always-zero `bg_mutations` field, and the unused `ops::run_ops` / `homogeneous` batch seam the reviewer flagged, were both removed in the #0076 pass.
