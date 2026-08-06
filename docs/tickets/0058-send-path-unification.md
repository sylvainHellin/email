---
id: 0058
title: One send implementation shared by the CLI and the TUI, plus reply/forward dedup
type: refactor
priority: next
status: open
created: 2026-08-06
---

From the architecture review synthesis, Tier 2 item 3: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: M for the send half, S for the reply/forward half.

Sending a draft is implemented three times with byte-identical helpers around each copy.
Every durability fix has to be made three times, and the review found that they have already drifted.

## Evidence

- `src/main.rs:998` `Commands::Send` builds and sends one draft inline.
- `src/main.rs:1243` `Commands::SendApproved` does the same for every approved draft, with its own copy of the surrounding helpers.
- `src/tui/actions.rs:603` defines a private `SendCtx`, `:652` `send_one_draft` sends against it, and `:976` and `:1065` are the two TUI call sites.
- The duplicated helpers are self-evident in the diff between `main.rs:998-1240` and `actions.rs:652-1000`; the review measured roughly 330 lines removable.
- Reply and forward draft creation is duplicated the same way, and the code says so in its own comments.

## Scope

1. Promote the TUI `SendCtx` into `src/send.rs` as a public `SendContext`, and expose `send::send_draft(&EmailDraft, &SendContext) -> Result<SendReport>`.
2. Point `Commands::Send`, `Commands::SendApproved` and the TUI `send_one_draft` at it.
   The outbox commit, the exactly-once marker and the Sent append stay inside `send_draft`, so all three callers inherit them.
3. Do the same for reply and forward draft creation: one function, both consumers.
4. Move the existing `send_one_draft` tests (`src/tui/actions.rs:3357-3480`) onto the shared entry point.

## Acceptance criteria

- One function performs a send; `rg 'submit\(' src/` shows a single caller.
- `mp send`, `mp send-approved` and the TUI send key produce identical outbox state transitions for the same draft, including the partial-recipient and stranded-submission cases.
- Net line count down by roughly 330 lines with no behaviour change.
- `cargo test` green, including the dead-SMTP tests moved to the shared path.
