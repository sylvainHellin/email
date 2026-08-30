---
id: 0087
title: Opening a message in the preview marks it read
type: feature
priority: next
status: done
created: 2026-08-14
---

Reading a message never changes its read state.
The read bit moves only through the explicit `m` key (`ToggleRead` -> `apply_set_read`, `src/tui/app/mutations.rs:123`); opening the read-only editor copy or scrolling the preview does not touch it (UX audit §b.1).
Triaging a full inbox therefore means pressing `m` on every message, which no mainstream client asks of the user.

## Owner decision (2026-08-14)

Opening a message in the preview marks it read.
This closes open question 1 of the audit synthesis (auto-mark-read, from which pane, after what dwell): the trigger is opening the message into the preview, with no dwell timer.

## Scope

1. When a message is shown in the preview pane, set its read bit through the existing `apply_set_read` path so the local state and the server `\Seen` flag both converge on the next sync.
2. Fire once per message on open, not on every scroll keypress or idle tick, so the mutation and its sync write are not repeated.
3. Draft rows and already-read rows are no-ops.
4. Manual `m` still toggles either way, so a user can mark a read message unread again.

## Cross-references

Read-status sync itself is already correct after [#0004](0004-fix-read-unread-sync.md), which fixed the bidirectional `\Seen` propagation and the snapshot-clobber window.
This ticket only adds a new trigger for the same mutation; it must reuse #0004's `sync_local_read_flags` staleness guard rather than introduce a second write path.

## Acceptance criteria

- Opening an unread message in the preview marks it read locally, and the change survives a sync round-trip.
- Scrolling within an already-open message does not re-issue the mutation.
- `m` on a read message still marks it unread.

## Resolution

The existing `MarkAsRead` action arm (`src/tui/actions.rs`) already routed to
`set_read_flag` -> `queue_read_flag` -> `apply_set_read`, but nothing dispatched
it. The trigger is now wired in.

- `App::take_message_to_auto_mark_read` (`src/tui/app/mod.rs`) yields the
  message the preview should mark read, tracked by a new `App::auto_read_opened`
  field so it fires once per open. Draft rows (no `MessageRef`) and already-read
  rows are no-ops; a preview scroll leaves the list cursor put, so it never
  re-issues.
- `actions::auto_mark_open_read` (`src/tui/actions.rs`) reuses the manual
  `set_read_flag` path (#0039 durable local write + owed `\Seen` op, converging
  on the next sync per #0004), guarded to the plain mail view with no overlay
  and no input-owning focus (post-review fix: while `/` filters the list, each
  keystroke resets the cursor to the narrowed set's top row, and marking those
  transient top results read would commit `\Seen` ops for messages the user
  only filtered past; the row the cursor lands on when the filter is left
  counts as the open instead).
- `run_loop` (`src/tui/mod.rs`) calls it once per iteration after events,
  background results and actions have settled the selection.

Manual `m`/`u` still toggles either way: after an open marks a message read, the
once-per-open tracker holds that message, so a follow-up mark-unread is not
undone until the cursor leaves and returns.

Note: the message shown in the preview on startup (the top row of the active
mailbox) is treated as an open and marked read, matching scope item 1's "when a
message is shown in the preview pane".
