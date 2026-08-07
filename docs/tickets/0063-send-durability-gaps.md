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
Partly parked 2026-08-06: the Graph backend is parked, so scope item 3 (resumable Graph `pending_send` rows) waits with it; the partial-recipient and double-submit halves are SMTP-side and stay active.
Shipped 2026-08-07: scope items 1, 2 and 4 and the double-submit admission gate, on the SMTP path.
What is left of this ticket is scope item 3 alone, parked with the backend, which is why the ticket stays open and is listed under Parked (Graph).
See [BACKLOG](../../BACKLOG.md).

Two holes the durable outbox of [#0037](0037-sqlite-store-engine-skeleton.md) left open.
Both lose a delivery quietly, which is the failure mode the outbox exists to prevent.

## Evidence

- Partial recipient success is recorded as full success: `src/send.rs:79-83` `submit_outcome` returns `SubmitOutcome::Accepted` as soon as `any_succeeded()` is true.
  The reasoning at `send.rs:74-78` is correct for the Sent-copy question it was written to answer, but the failed recipients recorded at `send.rs:68` `failed()` are never retried by anything and never surface after the status line is gone.
- Graph submissions stuck in `pending_send` can never be resumed: `src/send.rs:1262-1274` returns early for `AuthMethod::Graph` because the row holds RFC822 bytes while the Graph transport sends structured JSON.
  The row stays visible in `mp outbox list` and is only recoverable by a human re-sending from the draft.
- The stranded-submission path (`src/outbox.rs:672-702` `sweep_pending_sends`) parks ambiguous rows as failed, which is correct, but it is the only automated handling either case gets.
- Nothing stops one draft being submitted twice concurrently, so the TUI can send it twice: `Action::Send` on the cursor draft and an `Action::SendApproved` batch already in flight both reach `send::send_draft` on their own background thread, and an approved draft under the cursor is by definition also in the batch.
  There is no dedup on the way in.
  `build_draft_message` mints a fresh `Message-ID` per build (`message_id(None)`, `src/send.rs`), and `DurableSend::begin` enqueues on that id with no key on the draft itself, so the second run looks like an unrelated message to the outbox and to the Sent-copy search that a retry uses to avoid duplicating.
  Whichever settle runs second retires a file the other one already moved.
  Pre-existing and unchanged by [#0058](0058-send-path-unification.md), which unified the four copies of this orchestration without adding an admission gate; noted here because the fix belongs with the outbox, not with the callers.
  The cheap half is an in-process guard on the set of drafts a send is running for; the durable half is keying the outbox row on the draft so a resume cannot double-submit either.

## Scope

1. Record the per-recipient verdict durably, not just in the status line: extend the `outbox` envelope column or add a per-recipient table, so a partially delivered message names its undelivered recipients after a restart.
2. Retry the failed recipients only, with the existing backoff, and reach a terminal state that the user can see.
3. Give the Graph path a resumable representation: either store the structured payload alongside the RFC822 bytes, or reconstruct the Graph message from the stored bytes at resume time.
   Decide which and record the choice here.
   **Parked** with the Graph backend (2026-08-07): the decision is deliberately not taken here, because it is a decision about a transport nothing runs today and the answer depends on what the woken backend looks like.
4. Keep the exactly-once marker semantics intact; a retry must not deliver twice to a recipient that already got a 250.

## Acceptance criteria

- A send where one of two recipients is rejected leaves an outbox row that names the rejected recipient and reaches a terminal state without manual intervention. **Met.**
- No recipient receives the message twice across a retry. **Met**, by `Envelope::outstanding()` being the only thing a resubmission attempts.
- A Graph account with a `pending_send` row left by a crash resumes automatically on the next run, or fails with an explicit, actionable reason. **Parked** with the Graph backend; the row still stays visible with the explicit log line `resubmit_pending` writes.
- Tests cover the partial-failure and the Graph-resume path offline. **Half met**: the partial-failure, retry, recovery and admission-gate paths are covered in `tests/outbox_integration.rs`; the Graph-resume path waits with scope item 3.

## What shipped

The verdicts are per recipient, and they are durable.
`Envelope` (the row's `envelope` column, no schema change) now carries `delivered:` and `rejected:` lines beside the addresses, `SubmitOutcome::PerRecipient` carries one verdict per recipient from the SMTP loop, and `record_submission` folds the set into the row: a recipient with no verdict parks it in `failed`, a recipient that can still be tried keeps it in `pending_send` under the existing backoff, and once nothing is outstanding it goes on to `done` if anybody took it and to `failed` if the server refused them all.
A resubmission attempts `Envelope::outstanding()` rather than the whole address list, so a recipient that answered 250 is never spoken to twice, including across an operator `mp outbox retry`.
A 5xx is now a rejection of that recipient rather than an ambiguous whole-row failure, which is what makes the partial case reach a terminal state on its own.

A message that reached some recipients and not others keeps a note in `last_error` for good.
That note is what keeps the row listed by `mp outbox list` after it is `done` (shown as `partial`, with the refused addresses named) and counted in `OutboxCounts::partial`, which the TUI badge renders as `OUTBOX n (1 partial)`.
Discarding the row is how a human closes it.

The admission gate is two halves, as the evidence above describes.
The envelope carries the draft key (frontmatter `id`, or the path for a draft that has none), `outbox::enqueue` refuses a draft that already has a `pending_send` or `sent_pending_append` row with `AlreadyInFlight`, and `send_draft` holds a process-wide slot per draft for the length of the send, so the TUI's cursor send and the approved batch it is also in cannot both submit it.
A `failed` or `done` row does not hold the gate: a deliberate re-send after a human has looked is the user's business.

One related hole closed on the way: `send_durably` marked the row as entering submission and then let a pre-transport error out of `submit` (an unparseable envelope sender, a transport that would not build) propagate with the marker still set, so the next resume read that marker as "died inside the SMTP session" and parked a message that had never been sent.
Both send paths now record such a failure as the clean pre-submission one it is, which puts the marker back to NULL.

What is *not* closed: `lettre` does not let a caller tell a TCP connect failure (clean, nothing was sent) from an i/o error on an established connection (ambiguous), so both still park the row in `failed` for a human, as they did before this ticket.
Sending while the SMTP server is unreachable therefore still needs `mp outbox retry` rather than resolving itself.

## Review follow-up (2026-08-07)

The review of the shipped commit passed and left four findings, all fixed in one follow-up:

- `build_draft_message` validated the normalised `from:` and then stored the raw one, so a `from: Doe, Jane <j@x.com>` enqueued cleanly and failed every submission it would ever get at the pre-submission step; the admission gate then refused each re-send of that draft and `retry` refuses a `pending_send` row, leaving `mp outbox discard` as the only exit.
  The built message now carries the normalised address, which is the one `submit` parses.
- The enqueue transaction began DEFERRED while reading to decide whether it may write, so under WAL the loser of a two-process race hit `SQLITE_BUSY_SNAPSHOT` at the INSERT.
  That error landed in the non-durable fallback and the message was sent with no outbox row and no gate.
  The transaction now begins IMMEDIATE (`Store::immediate_transaction`), and the fallback is narrowed: a busy store is reported to the user as retryable, and only a store that will not open at all still buys a non-durable send.
- The durable gate compared the decoded draft key, flattened by the envelope encoding, against the raw in-memory one, so a path-fallback key holding a tab or a trailing space never matched and the gate silently missed.
  Both sides are flattened now.
- The in-process gate was keyed by draft alone, so two accounts each holding a hand-written draft with the same frontmatter `id:` refused each other's send.
  It is keyed by account and draft, like the durable half.

Four further properties were judged worth writing down rather than changing:

- Downgrading to a binary older than this ticket skips the `delivered:` lines it does not know about and resubmits the whole recipient list, so a downgrade between a partial send and its resume can deliver twice.
  Upgrading back restores the recorded set; the lines survive the older binary untouched.
- `synchronous = NORMAL` (a #0037 property, unchanged here) means the marker and the verdicts survive a process crash, not an OS or power crash.
  A power cut inside the SMTP session can therefore still lose the marker that would have parked the row.
- The retryable bucket has no attempt cap: a `Kind::Client` condition that is permanent in practice keeps the row retrying at the 900s backoff ceiling for as long as the user leaves it there.
  It stays visible in `mp outbox list` throughout, and discarding it is the human's call.
- `mp outbox retry` still refuses a `pending_send` row by design, which is what makes the `from:` bug above a dead end rather than an inconvenience; with the normalisation fixed, no known path enqueues a row that cannot be submitted.
