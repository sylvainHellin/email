# Backlog

Index of open tickets. One file per item lives in [docs/tickets/](docs/tickets/); see [docs/tickets/README.md](docs/tickets/README.md) for the convention. Use the `ticket` fish function to add a new entry.

When a ticket is shipped: set `status: done` in the ticket file, add an entry to [CHANGELOG.md](CHANGELOG.md), and remove its line from this index.

## Now

- [#0022 consistent naming](docs/tickets/0022-consistent-naming.md) -- refactor
- [#TKT-0051 email status](docs/tickets/TKT-0051-email-status.md) -- feature

## Next

> Data-access-layer redesign (DECIDED 2026-07-14, decisions settled 2026-07-31): server-as-truth SQLite mirror + content-addressed blob store; drafts local-only, received read-only. Greenfield rebuild on a branch, no dual-write, safety net is `mp-legacy` + the `pre-dal-nuke` tag. Plan: [docs/plans/data-access-layer.md](docs/plans/data-access-layer.md). Stage 0 (#0049, the pre-nuke oracle capture and the `pre-dal-nuke` freeze) is done. Order below is the build order; the stop-gate sits after the #0038 + #0050 pair, because the CLI is unusable between them. Both have shipped; the stop-gate review is the next step.

- [#0038 Read path, calendar and reconcile on the store; cold start stops walking files](docs/tickets/0038-read-path-to-db.md) -- perf _(Stage 2, first half of the stop-gate pair)_
- [#0005 Parallel IMAP fetch per mailbox](docs/tickets/0005-parallel-imap-fetch-per-mailbox.md) -- perf
- [#0007 Flagging / starring](docs/tickets/0007-flagging-starring.md) -- feature
- [#0008 Threading / conversation view](docs/tickets/0008-threading-conversation-view.md) -- feature
- [#TKT-0048 Contacts/Calendar visual polish to match overlay quality](docs/tickets/TKT-0048-views-visual-polish.md) -- feature

## Later

> TUI multi-view roadmap: [docs/plans/tui-restructure-views.md](docs/plans/tui-restructure-views.md). All three views have shipped: foundation (#0032), view switcher + Contacts (#0033), local calendar (#0034).

- [#0039 Durable pending_ops queue for flag/move/delete ops](docs/tickets/0039-pending-ops-queue.md) -- refactor _(data layer, Stage 3; send durability moved to #0037)_
- [#0040 Decommission the legacy .md tree; one-time draft import](docs/tickets/0040-drop-file-layer-cutover.md) -- chore _(data layer, Stage 4; closes TKT-0047)_
- [#TKT-0047 Reconcile walks attachment .md files (forged REPLY can poison PARTSTATs)](docs/tickets/TKT-0047-reconcile-walks-attachment-markdown.md) -- bug _(parked, accepted risk, resolved by #0040)_
- [#0041 Persistent IMAP connection + CONDSTORE/QRESYNC](docs/tickets/0041-persistent-conn-condstore.md) -- perf _(data layer, Stage 5)_
- [#0042 Graph /messages/delta + deltaLink](docs/tickets/0042-graph-delta-sync.md) -- perf _(data layer, Stage 5)_
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
