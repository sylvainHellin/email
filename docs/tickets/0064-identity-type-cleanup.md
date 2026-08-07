---
id: 0064
title: Retire path-shaped identity (MailboxRole enum, MailboxInfo.id, narrow EmailStatus)
type: refactor
priority: later
status: done
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

## Outcome

All four criteria met.
`MailboxRole` lives in `src/types.rs` next to `EmailStatus`, with `as_str` as the canonical key, a case-insensitive `From<&str>` and `is_inbox` / `is_sent`.
The converted sites are `config::find_mailbox_mapping` and `config::all_configured_mailboxes` (which now returns `(MailboxRole, &MailboxMapping)`), `SyncTarget::role` and `FreshObservation::role`, the two `eq_ignore_ascii_case("inbox")` new-mail checks in `imap_client::store_sync` and `graph`, `contacts::extractor` (both the store rebuild and the `ObservedIn` hook), `contacts::rank::Observation`, and `contacts::hooks`.
`config::mailbox_dir` and `config::slugify_mailbox_name` are gone with their tests, which completes the deletion [#0057](0057-dead-file-era-code-deletion.md) deferred ("`config::mailbox_dir` survives because `MailboxInfo.dir` is still a `PathBuf` built from it").

Decisions taken where the ticket left a fork:

- `MailboxInfo.id` is a `String` rather than a `MailboxRole`.
  The ticket says it matches whatever the store uses as the mailbox key, and the sidebar's Drafts row has no server role at all: its key is the reserved `selector::DRAFTS_MAILBOX`, which as a `MailboxRole::Other("drafts")` would read as an ordinary extra mailbox.
  The roles still build the ids (`MailboxRole::Inbox.as_str()` and friends), so the type is the source of the spelling either way.
- The `EmailStatus` narrowing is hard, with no legacy tolerance.
  A `.md` carrying `status: inbox` or `status: archived` no longer deserializes, so `mark_as_draft` refuses it at the parser rather than at the status guard.
  Nothing writes such a file: the receive path stopped writing `.md` at the store cutover, and no draft was ever created with one (checked against every drafts directory on the dev machine, which hold only `draft`, `approved` and `sent`).
  The narrowing is frontmatter-only, so there is no schema change and no store rebuild.
- The second display derivation was deleted rather than unified.
  `kind_to_status` fed `SearchTarget::status`, which fed `SearchHit::source_status` and `SearchResultEntry::source_status`, which nothing ever read; the same held for `SearchTarget::local_dir` and its two `source_local_dir` copies, the last `PathBuf`s in the search chain.
  All six fields and `kind_to_status` are gone, leaving `status_for_mailbox` as the only derivation.

## Bug found and fixed on the way

`MailboxInfo.dir` was built by `config::mailbox_dir`, which *slugified* the server name of an `[[mailboxes.extra]]` mailbox, while `all_configured_mailboxes` handed the sync path that server name verbatim and ingest wrote it into `messages.mailbox` unchanged.
An extra mailbox named `Team/Reports` was therefore listed, counted, quick-moved into and dumped under the key `team-reports`, which no row ever carries: it showed empty, its counter read 0, and a message quick-moved into it got a row under a key sync never touches.
With `MailboxInfo.id` the sidebar key *is* the ingest key, and a test pins that (`the_sidebar_key_is_the_key_ingest_writes`).
The `dump_mailbox_integration` fixture had encoded the reader's convention (it ingested under `team-reports`) and is now written from the writer's; that changes the dump's mailbox segment and its sort position for extra mailboxes only.
No account configures an extra mailbox today, so no live store holds a slug-keyed row (checked: `inbox`, `archive`, `sent` only).

## Not in scope, left open

The website's draft-format and config pages were corrected where this ticket made a claim newly false (the `status:` vocabulary, the status-workflow diagram's `inbox -> archived` row, the per-role local directories and the slugified extra-mailbox directory).
The rest of the file-era claims there, the attachments section above all, stay with [#0070](0070-website-file-era-claims.md).
