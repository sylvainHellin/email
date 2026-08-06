---
id: 0064
title: Retire path-shaped identity (MailboxRole enum, MailboxInfo.id, narrow EmailStatus)
type: refactor
priority: later
status: open
created: 2026-08-06
---

From the architecture review synthesis, Tier 2 item 4: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: M.

The types still describe a filesystem namespace that no longer exists.
A mailbox is identified by a directory path, a role is a bare string compared case-insensitively in half a dozen places, and the message status enum carries variants from the file era.

## Evidence

- `src/tui/app/types.rs:1174` `MailboxInfo.dir: PathBuf` round-trips through a directory namespace the cutover deleted; `mailbox_key` and the sidebar derive their identity from it.
- Roles are stringly-typed with three competing local conventions: `src/contacts/extractor.rs:137-143` builds `("inbox" | "archive" | "sent", dir)` pairs, `src/contacts/extractor.rs:208-210` maps `ObservedIn` back to the same strings, `src/contacts/rank.rs:57-58` matches on them, `src/contacts/hooks.rs:106` compares a lowercased copy, `src/graph.rs:645` does `target.role.eq_ignore_ascii_case("inbox")`, and `src/config.rs:836` `resolve_mailbox_dir` maps role names to directories.
- `src/types.rs:25-31` `EmailStatus` has five variants; `Inbox` and `Archived` are file-era placement states, not draft states, and the display side re-derives a status string twice (`types.rs:33-41` and the TUI list rendering).

## Scope

1. Introduce `MailboxRole` as an enum with `Inbox`, `Archive`, `Sent` and an `Other(String)` arm, and convert the six stringly-typed sites to it.
2. Replace `MailboxInfo.dir: PathBuf` with `MailboxInfo.id`, matching whatever the store uses as the mailbox key, and rewrite `mailbox_key` on top of it.
3. Narrow `EmailStatus` to the three draft states and delete the duplicate display derivation.

## Sequencing

Do the `EmailStatus` narrowing before [#TKT-0051](TKT-0051-email-status.md), which adds a second status axis (read, replied, forwarded).
Narrowing after that lands means untangling two axes instead of one.
This ticket is also the concrete half of [#0022](0022-consistent-naming.md) for mailbox identity; check that ticket for naming decisions already taken before choosing the enum spelling.

## Acceptance criteria

- No `eq_ignore_ascii_case` comparison against a role literal survives in `src/`.
- `MailboxInfo` holds no `PathBuf`.
- `EmailStatus` has three variants and one `Display` implementation, with no second derivation in the TUI.
- `cargo test` green; the sidebar, the contacts ranking and the Graph inbox check behave identically.
