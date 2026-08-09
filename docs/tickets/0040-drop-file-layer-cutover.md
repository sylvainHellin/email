---
id: 0040
title: Decommission the legacy .md tree; one-time draft import
type: chore
priority: later
status: done
created: 2026-07-14
---

Stage 4 of the data-access-layer redesign, shrunk 2026-07-31 for the complete nuke.
The filename slug predates the rewritten title and is kept as a stable link target.
Plan: [data-access-layer](../plans/data-access-layer.md).
Foundation plan: [2026-07-31_dal-foundation-plan-v2](../../.agents/handoff/2026-07-31_dal-foundation-plan-v2.md), amendment 9.

There is no file layer to delete: the greenfield build never had one, and [#0037](0037-sqlite-store-engine-skeleton.md) and [#0038](0038-read-path-to-db.md) removed the legacy modules as they replaced them.
What is left is ending the transition period.

Depends on [#0038](0038-read-path-to-db.md) and [#0039](0039-pending-ops-queue.md).

## Scope

1. Decommission the legacy tree: after the greenfield build has run against the real accounts for long enough to trust it, remove the old `.md` mailstore and retire the `mp-legacy` binary. The `pre-dal-nuke` tag stays; it costs nothing and it is the only remaining way back.
2. One-time draft import: copy the drafts from the legacy tree into `<account_dir>/drafts/`, assigning an `id:` frontmatter field to any draft that does not have one, then let the index refresh pick them up. This is the only data that does not come back from the server, so it is the only import.
3. Close TKT-0047 by construction and record why: `reconcile::build_index` no longer walks the account root, and there is no attachment `.md` on disk for a sender to forge frontmatter into. See [TKT-0047](TKT-0047-reconcile-walks-attachment-markdown.md), which is marked resolved-by this ticket.

## What shipped (2026-08-09)

`mp cutover [--account NAME] [--dry-run]`, in `src/cutover.rs`.

**The import is an id assignment, not a copy.** The investigation found that
`config::drafts_dir` is byte-for-byte unchanged since the `pre-dal-nuke` tag:
the file-era build and the store build both read `<account_dir>/drafts/`, so
there is no second location to import *from*. Copying files between two names
for the same directory is exactly the operation that could duplicate or lose a
draft, so it is not done. What a file-era draft actually lacks is the `id:`
frontmatter field that identity moved onto (decision C), and `mp cutover`
mints one into every draft that has none, then refreshes the index, which is
what makes it resolve through a selector and appear in `mp list`. Idempotent by
construction: the second run finds the field and writes nothing. On Sylvain's
machine every live draft already carried an `id:` (the ordinary index refresh
had done it), so the import is a verified no-op there.

**The tree is reported, never deleted** (decision recorded here per the
ticket's safety review). Removing the file-era tree is an `rm -rf` inside a
directory that also holds `drafts/`, `blobs/` and `store.sqlite3`, on a machine
whose only way back is the `pre-dal-nuke` tag. A bug in a classification
predicate would cost real mail, and nothing is bought by automating a one-time
keystroke. So the command prints each dead path with its `.md` count and size
and the exact `rm` line, and the human runs it. `mp cutover` itself writes
nothing but an `id:` field, and `--dry-run` suppresses even that. It runs only
when invoked: there is no migration on startup and no upgrade hook.

A directory under an account counts as dead when it is not one the current
build owns (`attachments/`, `blobs/`, `drafts/`) and it holds at least one
`.md` file. That `.md` test is what covers the file-era slugified mailbox names
(`projekte/`, ...) that no list anywhere records. `attachments/` is spared
because `parse::stable_attachments_dir` still writes there. Symlinked
directories are neither descended nor reported, so a link inside the data
directory cannot put an outside path on an `rm -rf` line.

TKT-0047 is closed by construction and pinned by a regression test:
`reconcile::tests::a_forged_md_attachment_cannot_move_a_partstat` ingests a
message carrying a `.md` attachment whose bytes are frontmatter with a forged
`method: REPLY`, the real UID and a winning `(sequence, dtstamp)`, and asserts
the invite's attendee stays `needs-action` and the fold sees zero replies.

## Acceptance criteria

- ~~No `.md` mailstore remains on any machine, and `mp-legacy` is gone from `~/.local/bin/`.~~ Amended: `mp cutover` names them and prints the removal command; the deletion is the owner's keystroke, not an automatic migration. As of 2026-08-09 the live tree still holds 377 MB across `assistant`, `tum` and `perso`.
- Every draft that existed in the legacy tree is present in the new `drafts/` directory with an `id:` field, appears in `mp list`, and resolves through its selector.
- TKT-0047 is set to `done` with a one-line note pointing at this ticket.

## Unblocks

- [#0041](0041-persistent-conn-condstore.md), [#0042](0042-graph-delta-sync.md), [#0043](0043-fts5-search.md) (Stage 5 protocol and search on a pure store base).
