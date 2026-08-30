---
id: 0089
title: Send the current draft in place, approve and send in one confirmed step
type: feature
priority: now
status: done
created: 2026-08-14
---

Compose to send is roughly eight steps, three of them after the editor (UX audit §b.2, §c.1).
After the editor exits you are returned to Drafts at status `draft`, then must navigate to the Drafts mailbox, press `A` to approve, then `x` to send with a confirm dialog.
Approve (`A`) and Send (`x`) are two separate manual key presses, and there is no send-now that approves and sends together.

There is a dead end.
Send on an unapproved draft still shows the confirm dialog, then fails at build time: the TUI `Send` does not pre-check approval (comment at `src/tui/actions.rs:1067`), and the refusal is proven by `send_refuses_an_unapproved_draft_before_it_reaches_the_outbox`.
So a user who skips `A` gets a confirm-then-error dead end.

## Owner decision (2026-08-14)

Sending must work on the current draft without switching to the Drafts mailbox and without a separate approve step.
Pressing send on an unapproved draft shows one warning dialog that confirms approve and send together.
The normal send confirm dialog stays for an already-approved draft.
Fix the current dead end where send on an unapproved draft confirms then fails at build time.
This closes open question 4 of the audit synthesis (does approve-and-send bypass the confirm dialog): it keeps a confirm, but a single dialog that approves and sends.

## Scope

1. Send acts on the current draft in context, without a mailbox switch to Drafts.
2. Send on an already-approved draft keeps today's confirm dialog and send.
3. Send on an unapproved draft shows one warning dialog that, on confirm, approves and sends in the same step rather than confirming and then failing at build time.
4. Remove the confirm-then-error dead end at `src/tui/actions.rs:1067`; the approval pre-check happens before, not after, the confirm.

## Resolution (2026-08-30)

Mostly delivered by #0092, closed out here.
The keybinding redesign merged approve+send onto the Global `x` (`src/tui/app/keymap.rs`), acting on the current draft from any focus with one confirm dialog, which removed the separate `A` approve step.
This ticket added the rest: the confirm dialog warns "Draft is not approved. Approve and send?" on an unapproved draft and keeps the plain "Send this email?" on an approved one (`src/tui/app/keys.rs`, `A::Send`); the send preamble was reordered so parse and validate run before `mark_as_approved` persists anything (`src/tui/actions.rs::validate_then_approve`), closing the dead end where a refused send left an approved flag behind.
Regression test: `a_draft_that_fails_validation_is_not_marked_approved`.
The send-refusal invariant is unchanged (`send_refuses_an_unapproved_draft_before_it_reaches_the_outbox`).

## Cross-references

Send itself is the shared implementation from [#0058](0058-send-path-unification.md) (one send path for CLI and TUI); this ticket changes the TUI approval-and-confirm flow around it, not the send mechanism.

## Acceptance criteria

- Pressing send on the currently selected draft sends it without navigating to the Drafts mailbox first.
- Send on an unapproved draft shows a single warning dialog whose confirmation approves and sends; there is no confirm-then-build-time-error path left.
- Send on an approved draft still shows the normal send confirm dialog.
- The existing send-refusal invariant (an unapproved draft never silently reaches the outbox) is preserved.
