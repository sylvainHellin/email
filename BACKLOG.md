# Backlog

Index of open tickets. One file per item lives in [docs/tickets/](docs/tickets/); see [docs/tickets/README.md](docs/tickets/README.md) for the convention. Use the `ticket` fish function to add a new entry.

When a ticket is shipped: set `status: done` in the ticket file, add an entry to [CHANGELOG.md](CHANGELOG.md), and remove its line from this index.

## Now

> Architecture review 2026-08-06, follow-ups #0053 to #0064: [synthesis](.agents/handoff/2026-08-06_architecture-review-synthesis.md). Suggested order is #0053, #0054, #0055, #0056, then #0057 and #0058. #0053, #0054, #0055, #0056, #0057, #0058 and #0064 have shipped. Their post-ship reviews all passed and left deferred notes, which are filed as #0065 to #0071; #0065, #0066, #0067, #0068 and #0071 have shipped.

_Nothing queued; the next thing to start comes from the Next tier below._

## Next

> Data-access-layer redesign (DECIDED 2026-07-14, decisions settled 2026-07-31): server-as-truth SQLite mirror + content-addressed blob store; drafts local-only, received read-only. Greenfield rebuild on a branch, no dual-write, safety net is `mp-legacy` + the `pre-dal-nuke` tag. Plan: [docs/plans/data-access-layer.md](docs/plans/data-access-layer.md). Stage 0 (#0049, the pre-nuke oracle capture and the `pre-dal-nuke` freeze) is done. Order below is the build order; the stop-gate sits after the #0038 + #0050 + #0052 triple, because the product is only half usable between them. #0038, #0050 and #0052 have all shipped, so the stop-gate is reached and the stages below it are the work after the pause.

_Nothing queued; the next thing to start comes from the Later tier below._

## Later

> TUI multi-view roadmap: [docs/plans/tui-restructure-views.md](docs/plans/tui-restructure-views.md). All three views have shipped: foundation (#0032), view switcher + Contacts (#0033), local calendar (#0034).

- [#0081 QRESYNC, UIDPLUS, and advancing the modseq on a capped pass](docs/tickets/0081-qresync-uidplus.md) -- perf _(the split-out half of #0041, which shipped the session pool and the CONDSTORE delta)_
- [#0060 Enforce the retention policy](docs/tickets/0060-retention-enforcement.md) -- feature
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
- [#0082 Verify the Graph delta against a live tenant](docs/tickets/0082-graph-delta-live-verification.md) -- perf _(the split-out half of #0042, which shipped the `/messages/delta` path and its fallbacks but had no Graph account to smoke them against)_
- [#0063 Send durability gaps, Graph half](docs/tickets/0063-send-durability-gaps.md) -- bug _(the SMTP halves shipped; scope item 3, resumable Graph `pending_send` rows, waits with the backend)_

The parity half of [#0059](docs/tickets/0059-syncbackend-trait.md) is parked with it: the trait and the engine shipped, but `graph.rs` still runs its own loop rather than being a second `SyncBackend`.
