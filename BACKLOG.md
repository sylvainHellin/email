# Backlog

Index of open tickets. One file per item lives in [docs/tickets/](docs/tickets/); see [docs/tickets/README.md](docs/tickets/README.md) for the convention. Use the `ticket` fish function to add a new entry.

When a ticket is shipped: set `status: done` in the ticket file, add an entry to [CHANGELOG.md](CHANGELOG.md), and remove its line from this index.

## Now

> Architecture review 2026-08-06, follow-ups #0053 to #0064: [synthesis](.agents/handoff/2026-08-06_architecture-review-synthesis.md). Suggested order is #0053, #0054, #0055, #0056, then #0057 and #0058. #0053, #0054, #0055, #0056, #0057, #0058 and #0064 have shipped. Their post-ship reviews all passed and left deferred notes, which are filed as #0065 to #0071; #0065, #0066, #0067, #0068 and #0071 have shipped.

> Audit 2026-08-14, owner decisions across performance, UX/workflow and feature-survey: [synthesis](.agents/research/2026-08-14-audit-synthesis.md). New tickets #0087 to #0096 spread across Now / Next / Later below.


## Next

> Data-access-layer redesign (DECIDED 2026-07-14, decisions settled 2026-07-31): server-as-truth SQLite mirror + content-addressed blob store; drafts local-only, received read-only. Greenfield rebuild on a branch, no dual-write, safety net is `mp-legacy` + the `pre-dal-nuke` tag. Plan: [docs/plans/data-access-layer.md](docs/plans/data-access-layer.md). Stage 0 (#0049, the pre-nuke oracle capture and the `pre-dal-nuke` freeze) is done. Order below is the build order; the stop-gate sits after the #0038 + #0050 + #0052 triple, because the product is only half usable between them. #0038, #0050 and #0052 have all shipped, so the stop-gate is reached and the stages below it are the work after the pause.

- [#0097 Type a short body in the compose wizard without opening $EDITOR](docs/tickets/0097-compose-wizard-body-field.md) -- feature
- [#0098 Attach a file to an existing draft from the TUI](docs/tickets/0098-attach-file-to-existing-draft.md) -- feature _(cross-ref #0016)_

## Later

> TUI multi-view roadmap: [docs/plans/tui-restructure-views.md](docs/plans/tui-restructure-views.md). All three views have shipped: foundation (#0032), view switcher + Contacts (#0033), local calendar (#0034).

- [#0081 QRESYNC, UIDPLUS, and advancing the modseq on a capped pass](docs/tickets/0081-qresync-uidplus.md) -- perf _(the split-out half of #0041, which shipped the session pool and the CONDSTORE delta)_
- [#0085 On-open re-fetch of an evicted body](docs/tickets/0085-on-open-body-refetch.md) -- feature _(the missing half of #0060, whose eviction sweep shipped; required before lowering a cap below the working set)_
- [#0090 Undo-send hold window before SMTP hand-off](docs/tickets/0090-undo-send-hold-window.md) -- feature _(configurable, default 20 s; sequence after #0089)_
- [#0091 HTML-to-text rendering through an external tool](docs/tickets/0091-html-to-text-rendering.md) -- feature _(evaluate w3m / lynx / pandoc)_
- [#0099 Per-account signatures appended on compose and reply](docs/tickets/0099-per-account-signatures.md) -- feature
- [#0101 Conversation-view collapse and inline navigation on top of the thread view](docs/tickets/0101-conversation-view-collapse-inline-nav.md) -- feature _(cross-ref #0008)_
- [#0084 iMIP send-side updates and cancellations](docs/tickets/0084-imip-send-cancel-and-update.md) -- feature _(the split-out send half of #0031, whose receive half shipped)_

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
