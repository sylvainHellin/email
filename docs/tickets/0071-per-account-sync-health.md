---
id: 0071
title: Persistent per-account sync-health surface (TUI indicator, mp sync exit code)
type: bug
priority: next
status: done
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

## Resolution

Shipped 2026-08-06.

### State

`AccountState::sync_health: SyncHealth` (`src/sync_health.rs`), one value per account, session-scoped and never read from disk.
`SyncHealth` is `Unknown` / `Ok { at }` / `Failed { reason, at, consecutive }`, and `SyncHealth::updated(outcome, at)` is the only transition: a success clears the mark outright, a failure keeps counting from the previous one so an outage reads differently from a hiccup.
`reason` is the error's first non-empty line capped at 80 characters, sized to the two sidebar rows that render it.

Every sync completion path writes it, because every one of them already arrived as a `BgResult::Fetch` or a `BgResult::Sync` carrying its account index: the startup multi-account fetch, the watcher-triggered quick sync, `F`, `S`, over IMAP or Graph alike.
`tui::bg::record_sync_health` is the single write, called before the existing status-line branch.
The `BgResult` channel is unchanged.

### TUI

Two surfaces, neither of which can be raced away, because both read the account's own state every frame rather than the shared status line.

- Sidebar, a three-row block under the mailbox list of the account on screen: a bold headline `⚠ sync failed [xN] HH:MM` and the reason word-wrapped over two rows.
  The layout pays for those rows (`sidebar::sync_health_rows`), so the block is not clipped in the narrow tier.
- Status-bar account strip, next to the existing unseen badge: a failing account's label is prefixed `⚠` and drawn in the error colour, active or not.

The account-level failure status line now also names its account (`Fetch failed (perso): ...`), which is what the activity log keeps.

### CLI

`mp sync --all-accounts` syncs every configured account, one failure not stopping the others.
The per-account body is the same function the single-account form calls, so the two cannot drift.
Every failure is named on its own `✗ <account>: <error>` line and again in the closing summary, `✗ 1 of 3 account(s) failed to sync: perso`.

Exit code, the deliberate decision the scope asked for: 1 for any failure, partial or total, and the same code whether one account was named or `--all-accounts` was passed.
A caller writes `mp sync --all-accounts || alert`, and a partial failure exiting 0 is exactly the silence this ticket exists to remove.
A distinct code for "some but not all" was rejected: it is only readable by a caller that already knows how many accounts are configured.
The single-account form already exited non-zero (the error propagated out of `main`) and still does, now with the account named.

### Scope item 4

Answered by `consecutive` rather than by a notification system: the sidebar headline carries `xN` from the second failure on, so seven weeks of refused logins is visibly different from one tick that timed out.

### Verification

`cargo test` 835 passing, +21.
The pure seams are covered in `src/sync_health.rs` (the reason cap, the transitions, the summary and the exit code) and `src/tui/ui/sidebar.rs` (the wrap).
The third acceptance criterion is `tui::bg::tests::a_failed_account_stays_failed_while_another_account_syncs_cleanly`, which drives `handle_bg_result` with `perso` failing and then `tum` succeeding and asserts both that `perso` keeps its mark and that the status line does show `tum`'s success, i.e. that the race is still lost and no longer matters.

Live, against the still-broken `perso` bridge account: `mp sync --all-accounts -n 5` printed `✓` for `assistant` and `tum`, `✗ perso: IMAP login failed: ...`, the summary line, and exited 1.
In the TUI the status bar read `[ASSISTANT] TUM  ⚠PERSO` after all three startup syncs had finished, and switching to `perso` showed the sidebar block with the timestamp and the wrapped reason while `assistant` and `tum` showed nothing.
