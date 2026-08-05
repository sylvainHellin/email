---
id: 0050
title: Unified mp:// selector contract and the drafts index
type: refactor
priority: next
status: open
created: 2026-07-31
---

Stage 2b of the data-access-layer redesign.
Plan: [data-access-layer](../plans/data-access-layer.md), decisions C and H.
Foundation plan: [2026-07-31_dal-foundation-plan-v2](../../.agents/handoff/2026-07-31_dal-foundation-plan-v2.md), section 3 and unit 6.

Paths disappear from every CLI input position, for received mail and drafts alike, and the drafts table becomes the index both the TUI and `mp list` read.
This subsumes [TKT-0045](TKT-0045-reload-drafts.md), which is marked resolved-by this ticket rather than fixed in the current build.

Depends on [#0037](0037-sqlite-store-engine-skeleton.md) and [#0038](0038-read-path-to-db.md).
The whole contract lands in one commit, so it is never half-applied.

## Scope

1. The grammar, one production, parsed the same way everywhere: `selector := [ "mp://" account "/" ] [ mailbox "/" ] key`. The canonical form is the fully qualified `mp://<account>/<mailbox>/<key>`, which is what every command that prints a selector emits. Elision is positional and deterministic: without the scheme, the account comes from `-A/--account` or the default account, and the mailbox comes from `--mailbox` or the command's declared default scope. The key is percent-encoded, so `/`, `%` and whitespace never appear raw. The parser never inspects the string to decide what kind of thing it is; the namespace is fixed by the command.
2. Two key namespaces. Received mail resolves in `messages`, where the key is the Message-ID without angle brackets, total by the `sha256-<hex16>@local.invalid` synthesis rule. Drafts resolve in the drafts index, where the mailbox segment is the reserved name `drafts` and the key is the draft id.
3. Resolution is a single indexed lookup. Zero matches is a clean error naming the namespace searched. Multiple matches, the cross-mailbox copy case, is a clean error listing the fully qualified selectors and resolvable with `--mailbox`, never a silent pick.
4. Draft identity is an `id:` frontmatter field written by `mp new`, so renaming a draft file does not change its selector. The `drafts` table is keyed by `(account, id)`; agents and automation are expected to preserve the field; a draft file without one is assigned an id on the first index refresh.
5. The drafts index: the file stays truth, the table is derived, holding id, slug, path, mtime, size, status, to, cc, subject, date and a body snippet. It is refreshed when the engine starts, after any `mp` command that writes a draft, and by a one-second mtime scan of the single `drafts/` directory, which is already a `max_depth(1)` walk of tens of files. A `notify`-style watcher is a later refinement and deliberately not a new dependency here.
6. The CLI rewrite. `mp archive`, `mp delete`, `mp open`, `mp save` and `mp invite accept|tentative|decline` take a received selector; `mp reply [--all]` and `mp forward` take a received selector and print the new draft selector; `mp send`, `mp mark-approved` and `mp mark-draft` take a draft selector; `mp validate` takes an optional draft selector and defaults to every draft on the account; `mp list` takes `[--status draft|approved|sent]` and no directory argument; `mp send-approved` takes `[--all-accounts]` and no directory argument; `mp new <name>` prints the selector; `mp path <selector>` prints the filesystem path and `mp edit <selector>` opens `$EDITOR`. No command dual-accepts a path and a selector.
7. `Action::CopyPath` becomes `CopyMessageRef` and copies the canonical `mp://` selector, which pastes directly into the new CLI.

Inherited from [#0038](0038-read-path-to-db.md), deliberately deferred to here: the clap help for `mp invite accept|tentative|decline` still reads "Path to the received invite email `.md`", and the `website/src/pages/commands.astro` rows for `mp invite`, `mp open`, `mp save`, `mp reply` and `mp forward` still advertise commands that decline today.
Both are rewritten against the real selector syntax by this ticket rather than churned twice on an unreleased binary.

`mp path` and `mp edit` are the only filesystem edge, and they are outputs of the selector, not path inputs.
Linting a draft template that does not live in `drafts/` yet would need a separately named `mp validate-file <path>`, so that `mp validate <selector>` never dual-accepts; it is not part of this cut.

## Acceptance criteria

- A draft created by an external process appears in `mp list` and in the TUI within one second and without a restart. This is the [TKT-0045](TKT-0045-reload-drafts.md) scenario and it is the acceptance test for it.
- Renaming a draft file keeps its selector working, because identity is the `id:` field and not the filename.
- Every selector printed by any command round-trips back through the parser.
- The ambiguous cross-mailbox copy errors with both fully qualified selectors and is resolvable with `--mailbox`.
- No command accepts a filesystem path where a selector is expected, and `mp path` round-trips a selector to a path that exists.
- `mp --help` snapshots are updated deliberately, alongside the affected pages under `website/src/pages/`.

## Unblocks

- [TKT-0045](TKT-0045-reload-drafts.md) closes with this ticket.
