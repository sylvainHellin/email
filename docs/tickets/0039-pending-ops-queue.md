---
id: 0039
title: Durable pending_ops queue for flag, move and delete mutations
type: refactor
priority: later
status: open
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
