---
id: 0079
title: Local filter/sort for flagged messages
type: feature
priority: later
status: done
created: 2026-08-09
closed: 2026-08-11
---

The flagging feature (#0007) shipped the `\Flagged` star, its server round-trip and its list marker, but not the "support filtering for flagged" half of the ticket.
A flagged view is local-only: the bit is already in `messages.flags` and mirrored into `EmailEntry.flagged`, so no server call is needed.

## #0039 verdict (2026-08-11): not subsumed

Checked while landing the [#0039](0039-pending-ops-queue.md) durable-queue core.
This is a local read-side view (filter and sort `messages.flags` for `\Flagged`), with no server op and no queue, so the mutation queue does not touch it.
Still open, unchanged.

## Notes

- The TUI has one local filter surface today, the `/` search over `App::visible` (`search_query == filter(emails)`).
  A flagged view needs its own filter mode (a toggle that narrows `visible` to `e.flagged`), which is why it was scoped out of #0007 rather than bolted onto search.
- Decide the interaction: a dedicated toggle key, or a search token (`is:flagged`) folded into the existing query grammar.
  The search-token route reuses the filter plumbing; the toggle route is more discoverable.
- A store-level `flagged` predicate on `read::list_mailbox` (a `WHERE flags LIKE '%\Flagged%'` equivalent) would make it a mailbox-scoped view rather than a client-side filter of the loaded page, which matters once retention (#0060) trims the loaded window.
- Sorting flagged-first is the cheaper cousin and could ship alone.

## Resolution (2026-08-11)

Shipped as the toggle, not as a search token: `F` in the mail list narrows `visible` to `EmailEntry::flagged` and widens it back. The toggle route was chosen for the discoverability the notes credit it with, and because the two narrowings are genuinely independent — the flagged view intersects with the `/` search instead of replacing it, which an `is:flagged` token folded into the query grammar could not express as cleanly.

- `App::flagged_only` is session state (it survives a mailbox or account switch, like a filter armed deliberately), and `App::apply_flagged_filter` runs at the end of every path that rebuilds `visible`, so the invariant is `visible == flagged(search(emails))`.
- The binding has no `NonEmptyList` guard, unlike its `*` sibling: the filter can empty the list, and the key that emptied it has to undo that.
- Surfaced three ways: the list title reads `Inbox (flagged)` (or `Inbox (filtered, flagged)` with a search on), the status line shows `shown/total`, and the empty list says which key restores it.
- Scoped out, as the notes anticipated: the store-level `flagged` predicate on `read::list_mailbox`. This filters the loaded page, which is the whole mailbox today; it becomes a real limitation only once retention (#0060) trims the loaded window, and that ticket is the place to add the predicate. Flagged-first sorting is likewise not implemented.

Golden frame `golden_mail_view_flagged_filter` captures the narrowed view; `website/src/data/tui-keys.json` was regenerated from `KEYMAP` via `scripts/regen-website-keys.sh`, so the site's key table and the `?` overlay carry the binding.
