---
id: 0053
title: Contacts rebuild wipes the frecency index (extractor still walks the deleted .md tree)
type: bug
priority: now
status: done
created: 2026-08-06
---

From the architecture review synthesis, Tier 0 item 1: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: S for the guard, M for the real fix.

Live data loss, found independently by three of the four review lanes.
The contacts extractor was never ported off the file layer, so it walks a directory tree the DAL cutover deleted, finds zero messages, and the caller persists that empty result over a corpus that took months of use to accumulate.

## Evidence

- `src/contacts/extractor.rs:33` `build_index_for_account` still resolves `account_dir(&account.name)` and iterates `account_mailboxes(account)`, skipping every role whose directory does not exist (`extractor.rs:44`, `extractor.rs:137-143`).
- The result is written unconditionally: `src/contacts_cmd.rs:73` and `src/contacts_cmd.rs:149` call `save_cache` with whatever `build_index_for_account` returned; `src/tui/app/mod.rs:460` does the same on the TUI refresh key.
- Three entry points reach it: `mp contacts rebuild`, the cold-cache `load_or_build` path (`src/contacts_cmd.rs:143`), and the in-TUI refresh.
- The ranking half is already independent of the file layer: `src/contacts/rank.rs` keys on a role string plus an observation field, and `src/contacts/hooks.rs` already feeds it from live sync observations.

## Scope

1. Interim guard, shippable on its own: refuse to persist an index with zero contacts over a non-empty cache, and log the refusal.
   Roughly five lines in `save_cache` or at each call site.
2. Real fix: rebuild the index from `store::read` rows instead of the filesystem. `list_account` already returns `from`, `to`, `cc` and `mailbox` in the listing shape, which is exactly what `process_header` consumes.
3. `process_header` and the whole ranking side port unchanged; the role string comes from the store `mailbox` column.
4. `InboxFrontmatter` and `gray_matter` drop out of the contacts module entirely.

## Acceptance criteria

- `mp contacts rebuild` on a synced store produces a non-empty index whose top entries match what the frecency corpus held before the rebuild.
- An extractor that returns zero contacts never overwrites a non-empty cache, and says so in the log.
- The contacts module no longer references `account_dir`, `mailbox_dir` or `gray_matter`.
- A test builds an index from a fixture store and asserts the observed roles and counts, with no filesystem mailbox tree present.

## Resolution

`build_index_for_account` opens the account store and folds `read::list_account` rows into the ranker; `process_header`, `flatten_addr` and the whole ranking side ported unchanged, and the role now comes from the row's `mailbox` column (`sent`/`inbox`/`archive` verbatim, anything else `extra`, as the per-directory walk did).
`walk_mailbox_dir`, `account_mailboxes` and the `InboxFrontmatter` + `gray_matter` usage are gone; `account_dir` survives inside `contacts/` only as the cache location, never as a mailbox tree.

The guard is `cache::save_rebuilt_cache`, which the three rebuild paths (`mp contacts rebuild`, the cold-cache `load_or_build`, the TUI refresh key) call instead of `save_cache`: an empty rebuild over a populated cache is refused, logged, and reported to the user, and the TUI keeps the loaded index too.
The incremental hooks still call `save_cache` directly, because they can only ever add.

Measured on the live `tum` account: the rebuild produced 1733 contacts against the 1735 the pre-rebuild corpus held, with an identical top ten and counts within a few percent (the store holds marginally fewer messages than the retired `.md` tree did).
The guard fired for `perso`, whose store carries no message rows, and kept its 61 hook-accumulated contacts.
