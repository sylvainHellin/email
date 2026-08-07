---
id: 0008
title: Threading / conversation view
type: feature
priority: next
status: done
created: 2026-05-01
---

Group emails by `In-Reply-To` / `References` headers.
Show a conversation as an expandable tree or inline thread.

It also absorbs the "list the related emails" half of [#TKT-0051](TKT-0051-email-status.md), which was scoped out of the second status axis: that axis is about one message's history, while grouping a conversation is this ticket's job and rides on the `thread_id` ingest already fills.

## Shipped (2026)

Threading rides the persisted `thread_id` rather than a read-side derivation.
The tree already carried a filled `messages.thread_id` column, assigned at ingest in `ingest::resolve_thread_id` from `In-Reply-To` and the last `References` entry, with the subject fallback deliberately absent.
The design guidance allowed a persisted key when it is clearly cheaper, and here it already existed: recomputing the chain on every overlay open would re-parse headers ingest already resolved, so the read side reads the column.

Ingest and sync are untouched: no schema change and no write on any sync path.
The #0072 arrival-mark and #0063 invariants are unweakened by construction, because nothing in this change reaches the pull, the prune gate or the cursor logic.
The only new store code is read-only, `store::read::thread_messages`.

`MessageRow` now carries `thread_id`, and `store::read::thread_messages(account, thread_id)` returns the conversation oldest-first across every mailbox.
A message that sits in several mailboxes (an inbox copy and its archived original) collapses to one entry per `Message-ID`.

The TUI conversation overlay is opened with `T` on a list row.
It is a read-only list of the conversation with the mail list's own status glyphs and a caret on the message it was opened from.
`j`/`k` move, `Enter` opens the highlighted message and switches mailbox when it lives in another (via a one-shot `App::pending_select` the async mailbox load consumes), and `Esc`/`q`/`T` close.
A message with no related mail in the store says so rather than opening a one-line overlay.
The binding is one `KEYMAP` row, so the help overlay, the hint bar and the website table all derive it.

The CLI half of #TKT-0051 is `mp dump-mailbox --json`, which now emits a `thread` field beside `invite`: the conversation key, so a script can group related mail without re-parsing headers.
It is documented in [docs/dump-allow-list.md](../dump-allow-list.md) as a new field with no pre-nuke counterpart.
This closes the deferred half of #TKT-0051; nothing of that ticket remains open.

## Adaptations to the current tree

"Show a conversation as an expandable tree" landed as a flat, oldest-first inline list rather than an indent tree.
The store keeps a flat `thread_id`, not parent pointers, and a flat chronological read places every message in the thread unambiguously.
A visual reply-depth indent would need the parent edge reconstructed at read time and is not worth the second pass; it is noted here rather than filed, since the flat list already answers "show the conversation".

No golden frame was added for the overlay itself, matching the RSVP and mailbox-picker overlays, which have none either.
The overlay state machine is unit-tested (`consume_pending_select`, the `T` binding) and the help-overlay golden was re-accepted for the new binding.

## References

- The `docs/plans/threading.md` brief the header referenced never existed; the model is documented inline above and at `ingest::resolve_thread_id`.
