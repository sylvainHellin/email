---
id: 0056
title: Rewrite docs/architecture.md for the store era, fix the wizard and dump-mailbox help
type: chore
priority: now
status: done
created: 2026-08-06
---

From the architecture review synthesis, Tier 0 item 3 and Tier 2 item 6: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: M for the rewrite, S for the two output fixes.

All four review lanes flagged the same thing.
`docs/architecture.md` still teaches the retired file model, and every session is told to read it first, so an agent following it would conclude that the contacts walk of [#0053](0053-contacts-rebuild-data-loss.md) is still correct behaviour.
Two user-facing strings advertise the same retired model.

## Evidence

- `docs/architecture.md:22` states the core invariant as "Emails are files", describing `.md` plus companion `.html` plus `_attachments/` and the per-account attachment mirror, none of which survives the cutover.
- `docs/architecture.md:41` lists `src/sync.rs` in the module map although the file is deleted, and there is no row for `store`, `ingest`, `outbox`, `selector`, `dump` or `reconcile`.
- `docs/architecture.md:121` claims 252 tests (210 unit, 42 integration); the suite is at 794.
- `src/main.rs:311` documents `dump-mailbox` as "reads the local `.md` files only", while `src/dump.rs:54` correctly says it reads the local store.
- `config init` prints an Inbox, Sent and Archive directory path per account (`src/config_cmd/init.rs:374-377`, `:722-725`, `:915-918`) that nothing creates, and never mentions `store.sqlite3` or `blobs/`, which are the only things the account directory actually holds.

## Scope

1. Rewrite `docs/architecture.md` against the current tree: server-as-truth SQLite mirror plus content-addressed blob store, drafts local-only, received read-only, the selector contract, and the real module map.
2. Refresh the test-count and layout section from the current suite.
3. Fix the `dump-mailbox` help text at `src/main.rs:311`.
4. Fix the three wizard completion blocks to print the store and blob paths that exist, and drop the mailbox directory lines.

## Acceptance criteria

- No sentence in `docs/architecture.md` asserts that an email is a file on disk, and no module row names a deleted file.
- Every module under `src/` that a newcomer needs appears in the module map.
- `mp dump-mailbox --help` and `mp config init` describe paths that exist after the command runs.
- A fresh `mp config init` run against a temp data dir prints only paths that are present on disk afterwards.
