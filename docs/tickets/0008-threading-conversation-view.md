---
id: 0008
title: Threading / conversation view
type: feature
priority: next
status: open
created: 2026-05-01
---

Group emails by `In-Reply-To` / `References` headers. Show a conversation as an expandable tree or inline thread.

It also absorbs the "list the related emails" half of [#TKT-0051](TKT-0051-email-status.md), which was scoped out of the second status axis: that axis is about one message's history, while grouping a conversation is this ticket's job and rides on the `thread_id` ingest already fills.

## References

- Design brief: [docs/plans/threading.md](../plans/threading.md)
