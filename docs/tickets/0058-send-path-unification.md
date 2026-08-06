---
id: 0058
title: One send implementation shared by the CLI and the TUI, plus reply/forward dedup
type: refactor
priority: next
status: done
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

## Resolution

Shipped in `f9eba0e`.
`send::send_draft(&EmailDraft, &SendContext) -> SentDraft` is the one orchestration, `draft::create_draft_from_source` the one reply/forward builder, and the four send call sites keep only their prompt, their wording and their exit code.
There were four send orchestrations to merge, not three: the TUI's `Action::SendApproved` had its own Graph and SMTP loops and is missing from the Evidence above.

### What stayed out of scope, deliberately

Two send orchestrations remain, and neither builds from a draft, which is what this ticket unified.
`run_send_invite` in `src/main.rs` sends an iMIP invitation built from CLI arguments, and `send::send_rsvp` sends a reply built from an invitation's own ICS.
Both already go through `send_durably`, so they inherit the outbox commit, the exactly-once marker and the Sent copy; what they do not share is the draft-shaped part of `send_draft` (frontmatter, signature, quoted HTML, attachments, settling the file), because they have no draft.
Folding them in would mean widening `SendContext` to carry a message that did not come from a file, which is a different refactor.

### Acceptance-text mismatch

"Net line count down by roughly 330 lines with no behaviour change" was wrong to write, and the work recorded eight behaviour deltas instead.
Every one reduces a loss surface: an unreadable attachment fails the send rather than being dropped, a Graph transport error costs one draft rather than the batch, the contacts bump survives a draft file that could not be retired, a draft that went out is no longer reported as failed, and the drafts index is refreshed after a TUI batch that never refreshed it.
The deltas are in the CHANGELOG entry.
Unifying duplicated orchestrations cannot be behaviour-preserving when the copies have drifted: each divergence is a choice of which copy becomes the contract, and "no behaviour change" is only available if every copy already agreed.
Acceptance criteria for a merge of drifted code should ask for the divergences to be enumerated and decided, not for there to be none.

### A correction to the implementation report

The report says the `Date:` header is stamped "immediately before the outbox commit".
It is stamped in `build_draft_message`, which for the Graph arm now runs before the attachment read and the `GraphClient` construction rather than immediately before `send_durably_via`.
The gap is milliseconds to seconds and well inside the 900 s prune window that depends on it (#0065), so nothing follows from it; the claim is simply not accurate as written.
