---
id: 0073
title: No way to delete a draft (CLI selector rejected, TUI `d` reports "nothing to delete")
type: bug
priority: next
status: done
created: 2026-08-07
---

## Close-out

Done, one commit. `mp delete` dispatches on the selector shape: a drafts
selector (`mp://<account>/drafts/<id>`, or `--mailbox drafts` beside an elided
key) deletes the draft file and its HTML companion, then the same index rescan
`mp list` runs drops the row; anything else keeps the received-mail path
unchanged. An `approved` draft is refused without `--force`, and a draft an
active outbox submission still holds (`pending_send`/`sent_pending_append`) is
refused on any flag, so a delete cannot pull a send's local anchor (#0063). The
TUI `d` key routes a Drafts row (and a Drafts selection) to the same library
check behind the existing confirmation, ending the "nothing to delete" line.
`mp delete --sent` sweeps every `status: sent` draft of the account, the upgrade
path from a build that did not retire sent drafts.

All acceptance criteria met and smoke-tested on the assistant account: create,
delete, re-delete (clean "no match" error), and the approved refuse/force pair.

A draft can be created, edited, approved and sent, but never deleted.
Neither surface offers it, and the TUI advertises a key that cannot work.

## Evidence

- `mp delete` is documented as "Delete a received email (server + local)" and its argument is `Received selector: mp://<account>/<mailbox>/<message-id>` (`src/main.rs:242`).
  A drafts selector has no mailbox and no message, so it cannot be expressed in that grammar.
- The TUI binds a single `d` -> Delete for every view (`mp dump-keys` lists `d` = Delete with no view qualifier, alongside draft-only bindings that are marked "(Drafts only)").
  `Action::Delete` resolves `app.selected_email_ref()` and hands it to `delete_msgs` (`src/tui/actions.rs:1063`), which calls `mutations::prepare_delete` against the store; a draft row produces nothing to prepare and the user sees `Delete failed: nothing to delete` (`src/tui/actions.rs:2371`).
- Observed 2026-08-07 in normal use, immediately after a successful send:

  ```
  10:32  Sent to 1 recipient(s) [sent + saved]
  10:32  Delete failed: nothing to delete
  ```

## Why it matters now

Current `mp` removes a draft file on send, so the drafts directory stays clean going forward.
Users upgrading from a version that did not remove it are left with a directory of `status: sent` files and no supported way to clear them.
On this machine that was 8 files on `tum` and 1 on `proton`, plus four pre-selector-era `.html` quote leftovers that were never indexed at all.

The workaround is undocumented and requires knowing an internal: delete the file with `rm`, then run `mp list`, which re-scans the drafts directory and prunes the orphaned row from the `drafts` table in `store.sqlite3`.
That self-healing rescan is the good news, because it means the fix does not need index bookkeeping of its own.

## Scope

1. Accept a drafts selector (`mp://<account>/drafts/<id>`) in `mp delete`, dispatching on the selector shape rather than adding a second command.
   The selector contract from [#0050](0050-selector-contract-drafts-index.md) already parses it.
2. Delete the draft file, then reconcile the drafts index the same way `mp list` does.
3. Refuse, or require `--force`, when the draft is `status: approved`, so a queued send is not silently dropped.
4. Route the TUI `d` key on a Drafts row to the same path, behind the existing confirmation prompt.
5. Consider a `--sent` sweep that clears every `status: sent` draft of an account in one call, which is the actual upgrade path for the case above.
6. Update `mp --help`, the in-TUI help overlay, and the website command pages in the same commit, per the repo rule.

## Acceptance criteria

- `mp delete mp://<account>/drafts/<id>` removes the file and the row; a following `mp list` shows neither.
- The same selector for a nonexistent id fails with a clear message, not a backtrace.
- `d` on a Drafts row deletes after confirmation and never prints "nothing to delete".
- Deleting an approved draft is refused without an explicit override.
- A received selector keeps its current behaviour, covered by the existing tests.
