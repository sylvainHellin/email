---
id: 0105
title: Local-first search with background server merge
type: feature
priority: now
status: open
created: 2026-08-20
---

Reported by Sylvain 2026-08-20: `ff` is server-only, so every search waits on IMAP.
Wanted UX (Apple Mail / Outlook): local hits render immediately, an indicator shows the server search still running, and the list updates in place when server hits arrive.

## Current state

- The overlay lowers the form to a `search::Query` and dispatches only the server renderer (`lib_do_multi_search` / `lib_do_multi_search_graph`).
- A local FTS renderer already exists and is wired to the CLI as `mp search --local` (`search.rs` "Renderer: local FTS", `store::search`).
- The two grammars were unified in #0043, so the same `Query` AST lowers to both sides already; #0086/#0088 unified the entry points.

## Proposed scope

1. On submit, run the local FTS lowering synchronously (milliseconds) and render its hits immediately, flagged with a source label (`local`).
2. Keep the existing background server search running; footer/status shows a spinner or `Searching server...` while it is in flight.
3. When server hits arrive, merge into the visible list instead of replacing it: dedupe by Message-ID (fall back to mailbox+UID), keep cursor position stable on the currently selected hit.
4. Local-only limitations stay visible: `to:`/`cc:`/`filename:` terms that the FTS cannot answer just yield no local hits (no error), the server pass covers them.
5. If the local lowering errors (unsupported term combination), skip the local pass silently and behave as today.

## Acceptance

- Submitting a subject/keyword search shows local hits before any network round-trip completes.
- Status line distinguishes "local results, server search running" from "search complete (N results)".
- No duplicate rows after the merge; selection does not jump when the merge lands.

Cross-ref: #0104 (result actions), #0086 (search form), #0088 (unified entry points).
