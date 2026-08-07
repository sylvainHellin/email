---
id: 0007
title: Flagging / starring
type: feature
priority: next
status: done
created: 2026-05-01
---

Flag important emails on the server, display a flag icon in the list, support filtering for flagged.

## Notes

- IMAP: use the `\Flagged` system flag.
- Graph: use the `flag.flagStatus` property on the message resource.
- TUI: add a key binding (e.g. `*`), add a flag column / icon to the email list.
- Persist flag state in frontmatter (`flagged: bool`) so local-only filtering works without a server roundtrip.

## Resolution (2026-08-09)

The `\Flagged` star rides the existing `messages.flags` column and the #TKT-0051 sync semantics rather than a new schema field or frontmatter. `types::MessageFlags` gained a fourth bit (`flagged`), parsed from `\Flagged`, written in canonical order (`\Seen \Answered \Flagged $Forwarded`), and orthogonal to the read/answered/forwarded axis (a message can be flagged and unread at once).

Server round-trip: `*` toggles the star.
The local store write lands first (`store::write::set_flagged`, a read-modify-write of the flag column), then a background `UID STORE +FLAGS (\Flagged)` / `-FLAGS (\Flagged)` mirrors it, exactly the way the read toggle mirrors `\Seen`.
The new IMAP op `remove_flag_on_server` is the clear half of the existing `add_flag_on_server`.
The op runs through the same `tui::mutations` prepare/dispatch/rollback machinery (`ServerOp::SetFlagged`, `prepare_flag`, `rollback_flag`, `BgResult::ToggleFlag`), including the batch path over the selection.
The next sync restates the whole flag set (IMAP pass 1 fetches `FLAGS`), so a star cleared in another client heals here and vice versa, and `flags_of` in the fetch path reads `Flag::Flagged` as the server-to-local channel.

Keybinding: `*` in the list context (`Guard::NonEmptyList`), consistent with `m` (toggle read).
It is batch-aware: with a multi-select active it flags the whole set, and flagging wins when any is unflagged, mirroring the read toggle's rule.

Marker: a filled flag glyph (`\u{f024}`) prepended to the subject cell in the theme's `warning` colour, rendered as its own span so it keeps its colour on a cursor row.
It is a subject prefix rather than a slot in the two-cell status column, because that column collapses one axis to a single glyph and the flag is orthogonal to it (an unread flagged message must show both).

`mp dump-mailbox` reports `flagged` beside `seen`, `answered` and `forwarded` in the flags array.

### Adaptations to the current tree

The frontmatter note (`flagged: bool`) is stale: the store cutover (#0037) removed frontmatter for received mail, and flags live in SQLite.
Flag state persists in `messages.flags`, which is where a local-only filter would read it.

Graph stays seen-only (parked, per the 2026-08-06 Graph decision), so `flag.flagStatus` is not written.
`ServerOp::SetFlagged` on the Graph backend parks the star locally (a no-op `Ok`, so the optimistic local write stands) and logs.
When the Graph backend wakes, wire `flag.flagStatus` there.

The `\Flagged is #0007` deferral comment in `MessageFlags::parse` and the `unknown_flags_are_ignored` test are resolved: `\Flagged` is now honoured, not read past.

Local flagged filter/sort is deferred to #0079: the TUI has one local filter surface (search), and a flagged view needs its own filter mode, which is more than this commit's scope.
The server round-trip, the marker and the keybinding, which are the core of the ticket, ship here.
