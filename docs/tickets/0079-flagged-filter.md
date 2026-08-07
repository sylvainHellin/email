---
id: 0079
title: Local filter/sort for flagged messages
type: feature
priority: later
status: open
created: 2026-08-09
---

The flagging feature (#0007) shipped the `\Flagged` star, its server round-trip and its list marker, but not the "support filtering for flagged" half of the ticket.
A flagged view is local-only: the bit is already in `messages.flags` and mirrored into `EmailEntry.flagged`, so no server call is needed.

## Notes

- The TUI has one local filter surface today, the `/` search over `App::visible` (`search_query == filter(emails)`).
  A flagged view needs its own filter mode (a toggle that narrows `visible` to `e.flagged`), which is why it was scoped out of #0007 rather than bolted onto search.
- Decide the interaction: a dedicated toggle key, or a search token (`is:flagged`) folded into the existing query grammar.
  The search-token route reuses the filter plumbing; the toggle route is more discoverable.
- A store-level `flagged` predicate on `read::list_mailbox` (a `WHERE flags LIKE '%\Flagged%'` equivalent) would make it a mailbox-scoped view rather than a client-side filter of the loaded page, which matters once retention (#0060) trims the loaded window.
- Sorting flagged-first is the cheaper cousin and could ship alone.
