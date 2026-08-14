---
id: 0088
title: Collapse the three search entry points into one, body search off the UI thread
type: feature
priority: next
status: open
created: 2026-08-14
---

Search is split across three bindings with different scope and mental models (UX audit §b.5, §c.4).
`/` is a client-side metadata filter over the current mailbox's loaded rows (`search_includes_body = false`, `handle_search_key`).
`\` is the same client-side path with `search_includes_body = true`, loading bodies via `sync_search_bodies`.
`f` opens the server (IMAP) search form, a network round-trip with its own result list and action set.
`/` and `\` differ only by a boolean yet are two bindings, both confined to the current mailbox; only `f` reaches the server or other mailboxes.

Two concrete defects fall out of the split:

- You cannot search by sender through `/`.
  It filters the loaded metadata rows but has no sender path, so the cheapest in-list search cannot answer "mail from this person" (UX §b.5).
- Toggling body-inclusive search freezes the UI.
  `\` -> `rebuild_visible` -> `sync_search_bodies` (`src/tui/app/mod.rs:1097`, `:1402`) opens the store and calls `read::load_bodies` over every message id in the mailbox, decoding and lowercasing each blob inline on the UI thread (performance audit §b.3).
  The first keystroke of a body search on a large mailbox blocks for hundreds of blob reads.

## Owner decision (2026-08-14)

Collapse the three entry points into one coherent search entry.
It must support search by sender, which `/` cannot do today.
Body search must be served by the FTS index off the UI thread, fixing the freeze in performance audit §b.3.
Retire `\`.
This closes open question 2 of the audit synthesis (collapse the three modes or keep them distinct): collapse.

## Scope

1. One search entry point that covers metadata (including sender), body, and the server reach that `f` provides today, so the user has a single gesture and mental model for "find a message".
2. Body search is served from the existing FTS5 index (`messages_fts`) and runs off the UI thread, either as an FTS query or behind an `Action`/`BgResult` with a searching state, never a synchronous bulk blob read on the UI thread.
3. Sender is a first-class field of the unified entry.
4. `\` is retired; its content-search capability is subsumed by the unified entry.

## Cross-references

The one-grammar work is already done in [#0086](0086-server-search-parity.md): a single parser feeds `to_imap` / `to_graph` / `to_fts`, and `f` already opens an Outlook-shape form over that grammar.
This ticket consolidates the TUI entry points on top of that grammar rather than adding a fourth.
[#0043](0043-fts5-search.md) shipped the FTS5 index and states, in the `SearchBodies` doc comment, why the old `\` substring filter was deliberately not served by FTS (whole-token vs substring semantics); the unified entry must decide the body-search semantics against that note, not silently regress it.

## Acceptance criteria

- One binding opens a search that can match by sender, subject, and body, and can reach the server and other mailboxes.
- A body search on a large mailbox does not freeze the UI; it is served by FTS or runs off-thread with a visible searching state.
- Searching by sender returns the expected hits, which `/` could not do.
- `\` is gone and no workflow relies on it.
