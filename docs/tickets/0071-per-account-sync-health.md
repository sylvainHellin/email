---
id: 0071
title: Persistent per-account sync-health surface (TUI indicator, mp sync exit code)
type: bug
priority: next
status: open
created: 2026-08-06
---

From the [#0068](0068-perso-store-holds-no-messages.md) diagnosis, which is the concrete defect that ticket's second acceptance criterion allows for.
Effort: S for the `mp sync` half, M for the TUI half.

An account whose sync fails has no durable surface.
The failure becomes one transient status line, and in a multi-account sync it loses the race against the accounts that succeeded.

## Evidence

- The `perso` account failed at IMAP login on every sync tick from 2026-06-19 to 2026-08-06, roughly 2900 attempts, and the outage was invisible for seven weeks.
- The mechanism is a race, not a missing check: on a startup sync `perso` failed after 54 ms and `tum` and `assistant` finished 15 seconds later, overwriting the status line with `Fetch complete`.
  The last writer wins, and the last writer is whichever account is slowest, which is never the one that failed fast.
- The path is `tui/helpers.rs` (`lib_do_sync` / `lib_do_sync_graph`) into `tui/actions.rs` (stringified into `BgResult::Fetch` / `BgResult::Sync`) into `tui/bg.rs` (`set_status_level(.., StatusLevel::Error)`).
  Nothing between them keeps per-account state.
- Until this ticket's parent commit, the account-level failure also logged nothing, while the per-mailbox failure right below it warned (`imap_client/store_sync.rs`, `Failed to sync mailbox '{}': {}. Continuing with next.`).
  That half is already fixed: `lib_do_sync` and `lib_do_sync_graph` now log the error at `error!` before returning it, so the outage is at least in the log file.
  What is left is making it visible without reading the log.
- `mp sync` has the same gap on the CLI side: a run where one of several accounts never authenticated does not say so in a way a script or a human skimming the last line would catch.

## Scope

1. Per-account sync health on the `App`: the outcome of that account's last sync, its error and its timestamp, set when the `BgResult` lands rather than folded into the shared status line.
2. Render it in the sidebar next to the account, alongside the existing unseen badge, so a failing account is visible without a sync running.
   A failed account should keep the mark until a sync succeeds, not until the next status line.
3. `mp sync`: name every account that failed in the summary, and exit non-zero when at least one did.
   Decide deliberately whether a partial failure is exit 1 or a distinct code, and whether `--all-accounts` differs from a single named account.
4. Consider whether a repeatedly failing account should say so more loudly than once (an auth failure that has persisted for weeks is a different message from one tick that timed out), but do not build a notification system for it here.

## Acceptance criteria

- An account that cannot authenticate is visibly marked in the TUI after a sync, and stays marked while a healthy account syncs afterwards.
- `mp sync` over several accounts, one of which fails, exits non-zero and names the failing account.
- A test pins that a failing account's health survives a later successful sync of a different account, which is the exact race #0068 lost.
