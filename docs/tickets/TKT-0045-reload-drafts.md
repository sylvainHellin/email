---
id: TKT-0045
title: reload drafts
type: bug
priority: next
workspace: When the app is already running, and I create a draft from another place, the draft don't appear in the list, and I have to first close the app and reopen to find it
status: done
created: 2026-07-22
---

Resolved by [#0050](0050-selector-contract-drafts-index.md), not by a fix in the current build.
The drafts index that ticket introduces is refreshed on engine start, after any `mp` command that writes a draft, and by a one-second mtime scan of the `drafts/` directory, so an externally created draft appears in the TUI and in `mp list` without a restart.
That scenario is #0050's first acceptance criterion.
Decided 2026-07-31, see [data-access-layer](../plans/data-access-layer.md), decision H.

Resolved 2026-08-05 by [#0050](0050-selector-contract-drafts-index.md).
The TUI polls `store::drafts::fingerprint` once a second and re-indexes when the directory changes, `mp` refreshes the index at engine start and after any command that writes a draft, and both readers list the same table.
Pinned by `an_externally_written_draft_shows_up_within_one_second` in [tests/cli_selector_contract.rs](../../tests/cli_selector_contract.rs) and by `a_draft_written_externally_appears_on_the_next_load` in `src/tui/app/types.rs`.
