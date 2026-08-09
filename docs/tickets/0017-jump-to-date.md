---
id: 0017
title: Jump-to-date in mailbox list
type: feature
priority: later
status: done
created: 2026-05-01
---

Quickly jump to a specific date range in large mailboxes. Useful for archives with thousands of emails where `j`/`k` and `gg`/`G` are not enough.

## Notes

- Trigger via a key (e.g. `D`).
- Date input UI: small inline prompt; accept `YYYY-MM-DD`, `last week`, `2 months ago`, etc.
- Implementation: binary-search the list (already date-sorted) and move the cursor; do not filter.

## Reconciliation (2026-08-11)

The suggested trigger `D` is taken (mark approved as draft, #0058), and the obvious `g d` resolves ambiguously against the unprefixed `d` (delete): `no_duplicate_live_dispatch_per_context` refuses that pairing, rightly, because a reordered KEYMAP would turn a mistyped jump into a delete. The binding is `g t` ("go to"), which sits with `gg` / `G` in the navigation block and collides with nothing.

## Resolution (2026-08-11)

`g t` arms an inline prompt that borrows the list pane's one-line input slot (the same row `/` and `\` use, since only one of the three can be armed at a time) and renders `date: <typed>` with the block cursor. The prompt owns the keyboard while it is up -- `d` types a `d`, it does not delete -- Esc abandons it, Enter commits.

The grammar is `src/tui/app/jump_date.rs`: a pure `parse_jump_date(input, today)` accepting `YYYY-MM-DD`, `YYYY-MM`, `YYYY` (each meaning its own first day), `today`, `yesterday`, `last week/month/year` and `N days/weeks/months/years ago`, case- and whitespace-insensitive. `now()` is read once, in the key handler, and passed in, so the whole grammar is tested without a clock. It is closed on purpose: a natural-language date parser is a dependency and an ambiguity budget for a key that moves a cursor, and a wrong guess lands the user silently in the wrong year. An input it cannot read leaves the prompt armed with the accepted forms on the status line, so a typo costs a correction rather than a re-arm.

`App::jump_to_date` is the ticket's binary search: the list is `date_sort DESC`, so the target is the first visible row on or before the date, found by `partition_point` over the visible indices (rows with no usable date sort last and count as older, which keeps the predicate monotone). Nothing is filtered -- the rows above and below stay where they were, which is the whole difference from `/` -- and the status line says where it landed. A date older than the mailbox parks on the oldest row and says so rather than pretending it found it.

Golden frame `golden_mail_view_jump_date_prompt` captures the armed prompt; `golden_help_overlay` was re-accepted for the new row (one line, 97 -> 98 bindings); `website/src/data/tui-keys.json` was regenerated with `scripts/regen-website-keys.sh` and the site rebuilt.

## Acceptance

- Trigger via a key. **Met** -- `g t`, in the List context, guarded on a non-empty list.
- Inline prompt accepting `YYYY-MM-DD` and relative forms. **Met** -- 6 grammar tests in `jump_date.rs`, plus `the_prompt_is_armed_typed_and_committed_by_the_keyboard`, `the_armed_prompt_swallows_the_keys_that_would_otherwise_act` and `an_unreadable_date_keeps_the_prompt_and_explains`.
- Binary-search the list and move the cursor; do not filter. **Met** -- `a_jump_lands_on_the_newest_row_on_or_before_the_date` asserts both the landing row and that `visible` is unchanged; `a_jump_past_the_oldest_row_says_where_it_stopped` covers the far end.
