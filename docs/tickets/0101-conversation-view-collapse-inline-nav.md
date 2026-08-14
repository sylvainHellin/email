---
id: 0101
title: Conversation-view collapse and inline navigation on top of the thread view
type: feature
priority: later
status: open
created: 2026-08-14
---

The thread view from [#0008](0008-threading-conversation-view.md) shipped as a flat, oldest-first inline list of a conversation, opened with `T`, read-only, with `j`/`k` to move and `Enter` to open a message (see #0008 "Shipped" and "Adaptations to the current tree").
Modern clients have moved past a flat thread list to a conversation view with collapse: Thunderbird's 2025/2026 Conversation View shows the full thread in the message pane with collapsible/expandable messages and optional inline reply, and it is now table stakes across GUI clients (feature survey §a.2 Thunderbird, §b "Threading / conversation view" and "Inline reply in thread", §(c) shortlist item 2).

## Scope

Build on the existing thread view rather than replacing it:

- Collapse and expand individual messages in the conversation, so a long thread shows one line per message and expands the one being read.
- Inline navigation within the conversation (next/prev message, jump to unread) without leaving the thread overlay.

Explicitly out of scope for this ticket (noted so the design does not quietly absorb them):

- Inline reply from within the conversation pane (feature survey rates it "adaptable": a TUI thread view plus `$EDITOR`); worth a follow-up once collapse/navigation land.
- A reply-depth indent tree; #0008 deliberately chose a flat chronological list because the store keeps a flat `thread_id` with no parent pointers, and reconstructing the parent edge at read time is a separate cost.

## Cross-references

- [#0008](0008-threading-conversation-view.md) (done) is the base: the persisted `thread_id`, `store::read::thread_messages`, and the `T` overlay this ticket extends.
- The keybinding redesign's `t` thread/attachment family ([#0092](0092-keybinding-scheme-redesign.md)) owns `tt` (show conversation); collapse/navigation keys inside the overlay should stay consistent with that family and the flat `J`/`K` next/prev.

## Acceptance criteria

- A conversation can be collapsed to one line per message and expanded per message.
- Next/prev navigation works inside the conversation overlay without returning to the list.
- The change builds on #0008's `thread_id` / `thread_messages` path; no new threading model is introduced.
