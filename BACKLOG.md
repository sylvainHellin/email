# Backlog

Index of open tickets. One file per item lives in [docs/tickets/](docs/tickets/); see [docs/tickets/README.md](docs/tickets/README.md) for the convention. Use the `ticket` fish function to add a new entry.

When a ticket is shipped: set `status: done` in the ticket file, add an entry to [CHANGELOG.md](CHANGELOG.md), and remove its line from this index.

## Now

> Architecture review 2026-08-06, follow-ups #0053 to #0064: [synthesis](.agents/handoff/2026-08-06_architecture-review-synthesis.md). Suggested order is #0053, #0054, #0055, #0056, then #0057 and #0058. #0053, #0054, #0055, #0056, #0057, #0058 and #0064 have shipped. Their post-ship reviews all passed and left deferred notes, which are filed as #0065 to #0071; #0065, #0066, #0068 and #0071 have shipped.

_Nothing queued; the next thing to start comes from the Next tier below._

## Next

> Data-access-layer redesign (DECIDED 2026-07-14, decisions settled 2026-07-31): server-as-truth SQLite mirror + content-addressed blob store; drafts local-only, received read-only. Greenfield rebuild on a branch, no dual-write, safety net is `mp-legacy` + the `pre-dal-nuke` tag. Plan: [docs/plans/data-access-layer.md](docs/plans/data-access-layer.md). Stage 0 (#0049, the pre-nuke oracle capture and the `pre-dal-nuke` freeze) is done. Order below is the build order; the stop-gate sits after the #0038 + #0050 + #0052 triple, because the product is only half usable between them. #0038, #0050 and #0052 have all shipped, so the stop-gate is reached and the stages below it are the work after the pause.

- [#0007 Flagging / starring](docs/tickets/0007-flagging-starring.md) -- feature
- [#0008 Threading / conversation view](docs/tickets/0008-threading-conversation-view.md) -- feature _(also owns the "list the related emails" half of #TKT-0051)_
- [#TKT-0048 Contacts/Calendar visual polish to match overlay quality](docs/tickets/TKT-0048-views-visual-polish.md) -- feature

## Later

> TUI multi-view roadmap: [docs/plans/tui-restructure-views.md](docs/plans/tui-restructure-views.md). All three views have shipped: foundation (#0032), view switcher + Contacts (#0033), local calendar (#0034).

- [#0061 Engine advisory lock on store.lock](docs/tickets/0061-engine-advisory-lock.md) -- refactor _(fold-into-#0039 candidate)_
- [#0039 Durable pending_ops queue for flag/move/delete ops](docs/tickets/0039-pending-ops-queue.md) -- refactor _(data layer, Stage 3; send durability moved to #0037; absorbs mutation unification and the engine lock)_
- [#0040 Decommission the legacy .md tree; one-time draft import](docs/tickets/0040-drop-file-layer-cutover.md) -- chore _(data layer, Stage 4; closes TKT-0047)_
- [#TKT-0047 Reconcile walks attachment .md files (forged REPLY can poison PARTSTATs)](docs/tickets/TKT-0047-reconcile-walks-attachment-markdown.md) -- bug _(parked, accepted risk, resolved by #0040)_
- [#0059 Extract a SyncBackend trait](docs/tickets/0059-syncbackend-trait.md) -- refactor _(the IMAP/Graph parity half is parked; what stands is the testable sync-engine seam #0041 assumes)_
- [#0041 Persistent IMAP connection + CONDSTORE/QRESYNC](docs/tickets/0041-persistent-conn-condstore.md) -- perf _(data layer, Stage 5; #0054 has landed, sequenced after #0059)_
- [#0043 FTS5 full-text search](docs/tickets/0043-fts5-search.md) -- feature _(data layer, Stage 5)_
- [#0060 Enforce the retention policy](docs/tickets/0060-retention-enforcement.md) -- feature
- [#0062 CLI read surface over the store (mp show, mp list-messages)](docs/tickets/0062-cli-store-read-surface.md) -- feature
- [#0067 Contacts guard refinements (observed_at, corrupt cache, partial erosion)](docs/tickets/0067-contacts-guard-refinements.md) -- bug
- [#0069 Delete the file-era invite rewriters (set_event_rsvp, InboxFrontmatter)](docs/tickets/0069-drop-file-era-invite-rewriters.md) -- chore _(from the #0057 review)_
- [#0070 Audit the website for file-era claims](docs/tickets/0070-website-file-era-claims.md) -- chore _(website; from the #0057 review)_
- [#0074 An ingest failure is not carried by the arrival mark](docs/tickets/0074-arrival-mark-misses-ingest-failures.md) -- bug _(from the #0072 sweep review)_
- [#0076 The post-send flag write opens one IMAP session per mailbox](docs/tickets/0076-post-send-flag-write-opens-a-session-per-mailbox.md) -- perf _(from the #TKT-0051 review; subsumed by #0039 if that lands first)_
- [#0077 Three intermittent test failures (temp-dir and env-var races)](docs/tickets/0077-flaky-tests.md) -- bug
- [#0078 The hint bar truncates mid-word (short label beside the long one in KeyBinding)](docs/tickets/0078-hint-bar-short-labels.md) -- bug _(from the #0075 review)_
- [#TKT-0044 Pane zoom/focus (herdr-style), after the data-layer rework](docs/tickets/TKT-0044-after-the-data-layer-rework-it-would-be-good-to-ha.md) -- feature
- [#0031 iMIP cancellations/updates (CANCEL / SEQUENCE)](docs/tickets/0031-imip-cancel-update.md) -- feature
- [#0010 Inline image rendering](docs/tickets/0010-inline-image-rendering.md) -- feature
- [#0016 Open attachments for drafts (`o`)](docs/tickets/0016-attachment-open-for-drafts.md) -- feature
- [#0017 Jump-to-date in mailbox list](docs/tickets/0017-jump-to-date.md) -- feature

### Distribution / cross-platform (adoption track)

> Windows is targeted via WSL only. Native Windows (msvc, Credential Manager, Scoop, winget, EV signing) is out of scope.

- [#0012 Apple Developer ID signing for macOS releases](docs/tickets/0012-apple-developer-id-signing.md) -- chore
- [#0014 Linux packaging (.deb, .rpm, AUR, musl)](docs/tickets/0014-linux-packaging.md) -- chore
- [#0015 Cross-platform smoke tests](docs/tickets/0015-cross-platform-smoke-tests.md) -- chore

## Parked (Graph)

> Graph backend parked (decision 2026-08-06): nothing depends on it today, and the priority is features and stability on the IMAP/SMTP path.
> It wakes deliberately, not opportunistically: the first live target is the EVOQS Exchange account, the items below are picked up together, and the pre-live-contact checklist (end of [0065-followup-report](.agents/workflow/0065-followup-report.md), also the Verification section of [#0065](docs/tickets/0065-graph-prune-batch-hardening.md)) is run before that first contact.

- [#0035 Graph API admin approval + Azure app verification](docs/tickets/0035-graph-admin-approval.md) -- chore _(blocked; written against the TUM tenant, re-scope for EVOQS on wake)_
- [#0036 Graph sync backend (calendar + server-side RSVP)](docs/tickets/0036-graph-sync-backend.md) -- feature _(blocked by #0035)_
- [#0042 Graph /messages/delta + deltaLink](docs/tickets/0042-graph-delta-sync.md) -- perf _(data layer, Stage 5; sequenced after #0059)_
- [#0063 Send durability gaps, Graph half](docs/tickets/0063-send-durability-gaps.md) -- bug _(the SMTP halves shipped; scope item 3, resumable Graph `pending_send` rows, waits with the backend)_

The Graph half of one more active ticket is parked with it: the parity/dedup motivation of [#0059](docs/tickets/0059-syncbackend-trait.md).
