# Backlog

Index of open tickets. One file per item lives in [docs/tickets/](docs/tickets/); see [docs/tickets/README.md](docs/tickets/README.md) for the convention. Use the `ticket` fish function to add a new entry.

When a ticket is shipped: set `status: done` in the ticket file, add an entry to [CHANGELOG.md](CHANGELOG.md), and remove its line from this index.

## Now

> Architecture review 2026-08-06, follow-ups #0053 to #0064: [synthesis](.agents/handoff/2026-08-06_architecture-review-synthesis.md). Suggested order is #0053, #0054, #0055, #0056, then #0057 and #0058. #0053 has shipped.

- [#0054 Schema bump bundle (modseq/UID split, pending_ops.updated, dead columns)](docs/tickets/0054-schema-bump-bundle.md) -- refactor _(prerequisite for #0041)_
- [#0055 Graph backend sync parity (prune, converge, watcher, timing)](docs/tickets/0055-graph-sync-parity.md) -- bug
- [#0056 Rewrite docs/architecture.md, fix the wizard and dump-mailbox help](docs/tickets/0056-architecture-docs-rewrite.md) -- chore
- [#0022 consistent naming](docs/tickets/0022-consistent-naming.md) -- refactor
- [#TKT-0051 email status](docs/tickets/TKT-0051-email-status.md) -- feature

## Next

> Data-access-layer redesign (DECIDED 2026-07-14, decisions settled 2026-07-31): server-as-truth SQLite mirror + content-addressed blob store; drafts local-only, received read-only. Greenfield rebuild on a branch, no dual-write, safety net is `mp-legacy` + the `pre-dal-nuke` tag. Plan: [docs/plans/data-access-layer.md](docs/plans/data-access-layer.md). Stage 0 (#0049, the pre-nuke oracle capture and the `pre-dal-nuke` freeze) is done. Order below is the build order; the stop-gate sits after the #0038 + #0050 + #0052 triple, because the product is only half usable between them. #0038, #0050 and #0052 have all shipped, so the stop-gate is reached and the stages below it are the work after the pause.

- [#0057 Delete the dead file-era code](docs/tickets/0057-dead-file-era-code-deletion.md) -- chore
- [#0058 One send implementation for the CLI and the TUI](docs/tickets/0058-send-path-unification.md) -- refactor
- [#0005 Parallel IMAP fetch per mailbox](docs/tickets/0005-parallel-imap-fetch-per-mailbox.md) -- perf
- [#0007 Flagging / starring](docs/tickets/0007-flagging-starring.md) -- feature
- [#0008 Threading / conversation view](docs/tickets/0008-threading-conversation-view.md) -- feature
- [#TKT-0048 Contacts/Calendar visual polish to match overlay quality](docs/tickets/TKT-0048-views-visual-polish.md) -- feature

## Later

> TUI multi-view roadmap: [docs/plans/tui-restructure-views.md](docs/plans/tui-restructure-views.md). All three views have shipped: foundation (#0032), view switcher + Contacts (#0033), local calendar (#0034).

- [#0059 Extract a SyncBackend trait](docs/tickets/0059-syncbackend-trait.md) -- refactor _(sequence before #0041 and #0042; the seam both assume)_
- [#0061 Engine advisory lock on store.lock](docs/tickets/0061-engine-advisory-lock.md) -- refactor _(fold-into-#0039 candidate)_
- [#0060 Enforce the retention policy](docs/tickets/0060-retention-enforcement.md) -- feature
- [#0062 CLI read surface over the store (mp show, mp list-messages)](docs/tickets/0062-cli-store-read-surface.md) -- feature
- [#0063 Send durability gaps (partial recipients, Graph resume)](docs/tickets/0063-send-durability-gaps.md) -- bug
- [#0064 Retire path-shaped identity (MailboxRole, MailboxInfo.id, EmailStatus)](docs/tickets/0064-identity-type-cleanup.md) -- refactor _(before #TKT-0051; concrete half of #0022)_
- [#0039 Durable pending_ops queue for flag/move/delete ops](docs/tickets/0039-pending-ops-queue.md) -- refactor _(data layer, Stage 3; send durability moved to #0037; absorbs mutation unification and the engine lock)_
- [#0040 Decommission the legacy .md tree; one-time draft import](docs/tickets/0040-drop-file-layer-cutover.md) -- chore _(data layer, Stage 4; closes TKT-0047)_
- [#TKT-0047 Reconcile walks attachment .md files (forged REPLY can poison PARTSTATs)](docs/tickets/TKT-0047-reconcile-walks-attachment-markdown.md) -- bug _(parked, accepted risk, resolved by #0040)_
- [#0041 Persistent IMAP connection + CONDSTORE/QRESYNC](docs/tickets/0041-persistent-conn-condstore.md) -- perf _(data layer, Stage 5; blocked on #0054, sequenced after #0059)_
- [#0042 Graph /messages/delta + deltaLink](docs/tickets/0042-graph-delta-sync.md) -- perf _(data layer, Stage 5; sequenced after #0059)_
- [#0043 FTS5 full-text search](docs/tickets/0043-fts5-search.md) -- feature _(data layer, Stage 5)_
- [#TKT-0044 Pane zoom/focus (herdr-style), after the data-layer rework](docs/tickets/TKT-0044-after-the-data-layer-rework-it-would-be-good-to-ha.md) -- feature
- [#0031 iMIP cancellations/updates (CANCEL / SEQUENCE)](docs/tickets/0031-imip-cancel-update.md) -- feature
- [#0035 Graph API admin approval + Azure app verification](docs/tickets/0035-graph-admin-approval.md) -- chore _(blocked)_
- [#0036 Graph sync backend (calendar + server-side RSVP)](docs/tickets/0036-graph-sync-backend.md) -- feature _(blocked by #0035)_
- [#0010 Inline image rendering](docs/tickets/0010-inline-image-rendering.md) -- feature
- [#0016 Open attachments for drafts (`o`)](docs/tickets/0016-attachment-open-for-drafts.md) -- feature
- [#0017 Jump-to-date in mailbox list](docs/tickets/0017-jump-to-date.md) -- feature

### Distribution / cross-platform (adoption track)

> Windows is targeted via WSL only. Native Windows (msvc, Credential Manager, Scoop, winget, EV signing) is out of scope.

- [#0012 Apple Developer ID signing for macOS releases](docs/tickets/0012-apple-developer-id-signing.md) -- chore
- [#0014 Linux packaging (.deb, .rpm, AUR, musl)](docs/tickets/0014-linux-packaging.md) -- chore
- [#0015 Cross-platform smoke tests](docs/tickets/0015-cross-platform-smoke-tests.md) -- chore
