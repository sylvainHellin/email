---
id: 0068
title: Investigate why the perso account's store holds no message rows
type: bug
priority: next
status: done
created: 2026-08-06
---

Observed during the live validation of [#0053](0053-contacts-rebuild-data-loss.md), out of scope there and never explained.
Source: [0053 implementation report](../../.pi-subagents/artifacts/outputs/148add6b/.agents/workflow/0053-implementation-report.md), "Live validation" and "Residual risk".

## Evidence

- `mp contacts rebuild --account perso` produced zero contacts, so the #0053 guard fired and kept the 61 the cache already held.
  That was the first live instance of the data loss the guard exists to prevent.
  (Correction, from the investigation: those 61 were not accumulated by the send and sync hooks.
  `contacts-cache.json` carries `built_at: 2026-07-20T12:40:04Z` and no observation later than 2026-06-19, so it is one full extractor rebuild over the frozen `.md` corpus, which is exactly what the root cause below predicts.
  Nothing about `perso` was still working.)
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

## Resolution

Investigated read-only on 2026-08-06; full diagnosis in the #0068 diagnosis report (worker artifact, not committed).
The cause is external to mailypoppins, and the one defect that belongs to mailypoppins is why nobody noticed for seven weeks.

### Root cause

The Proton Mail Bridge that serves `perso` on `127.0.0.1:1143` is running but signed out: it holds the account in its vault and never connects it, so its embedded IMAP server answers every `LOGIN` with `no such user`.
Every sync tick therefore dies at `open_imap_session` before a single `SELECT`, which is why the store has zero `mailboxes` rows and zero `sync_cursors` rows rather than a cursor with an empty window.
Last successful login: 2026-06-19 23:15:38.
Roughly 2900 attempts since, all refused.
The bridge's own logs go `Loading users count=1` plus `Adding user to imap server` before that date and `User is not connected (skipping)` after it.

Of the four candidates above, candidate 2 (sync runs and fails silently) is the one that holds, with the refinement that sync runs on every tick and never authenticates.
Candidate 1 is wrong: `sync_mailboxes: account=perso, 3 targets` is in every log.
Candidate 3 happened and is downstream noise: the 2026-08-06 schema-v3 drop-and-rebuild replaced a file that was already empty, and the #0053 guard event earlier the same day is the same emptiness seen from the contacts side.
Candidate 4 is untestable until login works; the configured `INBOX` / `Archive` / `Sent` are the standard bridge folder names.

The `store.sqlite3` file exists because non-sync code paths open it (`Store::open_account` creates on open), not because anything was ever written into it.
The frozen `.md` tree under `accounts/perso/` stops at 2026-06-19 22:38, forty minutes before the last good login.

### The mailypoppins half

An account-level sync failure was written nowhere durable: it became a one-shot TUI status line that the two accounts which succeeded overwrote fifteen seconds later, while the per-mailbox failure path had warned all along.
`lib_do_sync` and `lib_do_sync_graph` now log the account-level error at `error!` before returning it, which would have put this outage in the log file in June.
The persistent surface it also needs, a per-account health indicator in the TUI and a `mp sync` summary that names failed accounts, is [#0071](0071-per-account-sync-health.md).

### Operator checklist

The remaining work is outside the repo and only the operator can do it.

- [ ] Open Proton Mail Bridge and sign the `perso` address back in; confirm the bridge log says `Adding user to imap server` rather than `User is not connected (skipping)`.
- [ ] `mp config set-password imap --account perso` with the bridge's freshly generated password (the bridge mints a new one on every sign-in, so the stored one is stale regardless).
- [ ] `mp config set-password smtp --account perso` with the same.
- [ ] `mp sync --account perso -n 100`, then check that `messages`, `mailboxes` and `sync_cursors` gain rows.
  Watch for `Failed to sync mailbox 'Archive'` or `'Sent'`, which is where candidate 4 would finally show.
- [ ] Back up `accounts/perso/contacts-cache.json` (it carries history from 2012 that the store will not have), then `mp contacts rebuild --account perso`.
