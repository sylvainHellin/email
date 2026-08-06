---
id: 0063
title: Send durability gaps (partial-recipient success, unresumable Graph pending_send rows)
type: bug
priority: later
status: open
created: 2026-08-06
---

From the architecture review synthesis, Tier 3: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: M each.

Two holes the durable outbox of [#0037](0037-sqlite-store-engine-skeleton.md) left open.
Both lose a delivery quietly, which is the failure mode the outbox exists to prevent.

## Evidence

- Partial recipient success is recorded as full success: `src/send.rs:79-83` `submit_outcome` returns `SubmitOutcome::Accepted` as soon as `any_succeeded()` is true.
  The reasoning at `send.rs:74-78` is correct for the Sent-copy question it was written to answer, but the failed recipients recorded at `send.rs:68` `failed()` are never retried by anything and never surface after the status line is gone.
- Graph submissions stuck in `pending_send` can never be resumed: `src/send.rs:1262-1274` returns early for `AuthMethod::Graph` because the row holds RFC822 bytes while the Graph transport sends structured JSON.
  The row stays visible in `mp outbox list` and is only recoverable by a human re-sending from the draft.
- The stranded-submission path (`src/outbox.rs:672-702` `sweep_pending_sends`) parks ambiguous rows as failed, which is correct, but it is the only automated handling either case gets.

## Scope

1. Record the per-recipient verdict durably, not just in the status line: extend the `outbox` envelope column or add a per-recipient table, so a partially delivered message names its undelivered recipients after a restart.
2. Retry the failed recipients only, with the existing backoff, and reach a terminal state that the user can see.
3. Give the Graph path a resumable representation: either store the structured payload alongside the RFC822 bytes, or reconstruct the Graph message from the stored bytes at resume time.
   Decide which and record the choice here.
4. Keep the exactly-once marker semantics intact; a retry must not deliver twice to a recipient that already got a 250.

## Acceptance criteria

- A send where one of two recipients is rejected leaves an outbox row that names the rejected recipient and reaches a terminal state without manual intervention.
- No recipient receives the message twice across a retry.
- A Graph account with a `pending_send` row left by a crash resumes automatically on the next run, or fails with an explicit, actionable reason.
- Tests cover the partial-failure and the Graph-resume path offline.
