---
id: 0068
title: Investigate why the perso account's store holds no message rows
type: bug
priority: next
status: open
created: 2026-08-06
---

Observed during the live validation of [#0053](0053-contacts-rebuild-data-loss.md), out of scope there and never explained.
Source: [0053 implementation report](../../.pi-subagents/artifacts/outputs/148add6b/.agents/workflow/0053-implementation-report.md), "Live validation" and "Residual risk".

## Evidence

- `mp contacts rebuild --account perso` produced zero contacts, so the #0053 guard fired and kept the 61 the send/sync hooks had accumulated.
  That was the first live instance of the data loss the guard exists to prevent.
- `mp dump-mailbox --json --account perso` is empty too, so the emptiness is the store, not the contacts extractor: the account has a `store.sqlite3` file with no `messages` rows.
- The three other accounts (`tum`, `assistant`, and the one the 1733-contact rebuild ran against) all hold rows, so this is specific to `perso` rather than a store-wide fault.
- Cause unknown.
  Candidates, none checked: sync has never run for the account, sync runs and fails silently, the store was dropped by a version mismatch and never refilled, or the account's mailbox configuration names folders the server does not have.

## Scope

1. Run `mp sync --account perso` with logging on and read what the pull actually does: whether it authenticates, which mailboxes it selects, what the fetch window returns and whether ingest is reached.
2. Check `sync_cursors` and `mailboxes` for the account: a cursor row with a `uidvalidity` but no messages says a different thing than no rows at all.
3. Check whether the account's configured mailbox names resolve on the server.
4. Fix the cause, or file the specific defect it turns out to be and close this one as an investigation.

## Acceptance criteria

- The reason `perso` holds no message rows is written down here.
- Either the account syncs mail into its store, or a follow-up ticket names the concrete defect and this one links to it.
