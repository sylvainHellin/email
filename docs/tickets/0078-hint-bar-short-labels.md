---
id: 0078
title: The hint bar truncates mid-word; KeyBinding needs a short label beside the long one
type: bug
priority: later
status: done
created: 2026-08-07
---

From the post-ship review of [#0075](0075-open-received-mail-in-editor.md) (note 5), which converges with the residual risk the implementer of #0075 recorded.

`KeyBinding` carries one `desc` and three surfaces render it: the help overlay, the website key table, and the hint bar (`src/tui/app/keymap.rs:331-351`).
The first two have a column each and can take any length.
The hint bar is a single line, it lays every `hint: true` binding of the context out end to end (`src/tui/ui/status.rs:71-87`), and ratatui clips whatever does not fit.
One long description therefore silently costs the bindings to its right.

## Evidence

At the golden 120-column width, with `Enter / e Open in editor (mail read-only)` in the row:

- `golden_mail_view` ends `... r / R Reply / Reply-all  a A`, cut mid-word.
- `golden_mail_view_with_selection` ends `... Reply / Reply-al`, so both `a Archive` and `d Delete` are gone; the `2 SELECTED` badge is wider than `MAIL`, and the difference comes straight off the end of the line.

The bar was already lossy at that width and the help overlay (`?`) remains complete, so no binding is unreachable.
What is new is that the visible end of the default-width frame reads as broken rather than as truncated, and that `d Delete` is one of the casualties.

## Scope

1. Add a short label to `KeyBinding` beside `desc`, used by the hint bar only; the help overlay and `dump-keys` (hence the website) keep the long one.
   An empty short label means "reuse `desc`", so only the rows that need one carry one.
2. Give short labels to the rows that are long today, starting with `Enter / e` (`Open (read-only)`), and keep the accurate long description in the overlay.
3. Cut the trailing element cleanly rather than mid-word when the line still overflows: drop the whole `keys` + label pair that does not fit, and mark the truncation (an ellipsis, or the `?` hint that is already there).
4. Re-accept the affected golden frames and regenerate `website/src/data/tui-keys.json`, which must not change if only the short labels move.

## Acceptance criteria

- No golden frame ends mid-word.
- `d Delete` is visible in `golden_mail_view_with_selection` at 120 columns.
- The help overlay and the website table still show the long descriptions, byte-identical to `mp dump-keys --json`.
- A test pins that a binding with no short label falls back to `desc`.

## Resolution (2026-08-11)

`KeyBinding` gained a `short: &'static str` field, `""` meaning "reuse `desc`", read only through `KeyBinding::hint_label()` and only by the hint bar.
The constructors are unchanged; a `const fn short(kb, "...")` wraps the rows that need one, so the table stays readable and eleven rows carry a short label out of ~90.

Short labels added: `Enter / e` -> `Open (read-only)` (List and Server-search), `j/k` -> `Navigate` and `v` -> `Select` (List), `s / S` -> `Sync`, Contacts' `Enter / n` -> `Compose`, `v` -> `Send vCard`, `c` -> `Copy email`, `r` -> `Refresh`, Calendar's `Enter / e` -> `Open invite email`, `V` -> `RSVP`, `t` -> `Past / upcoming`, `r` -> `Refresh`.

`render_hint_bar` now measures as it builds and drops whole `keys` + label pairs that do not fit, marking the cut with ` …`; it never hands ratatui a line longer than the pane.
Contacts and Calendar fit outright at 120 columns now; only the mail List still overflows, and it truncates cleanly.

At 120 columns:

- `golden_mail_view`: `... a Archive  d Delete  n New draft …`
- `golden_mail_view_with_selection`: `... a Archive  d Delete …` -- `d Delete` is visible, which was the criterion.
- `golden_calendar_view` and `golden_contacts_view`: complete rows, no ellipsis.

## Acceptance criteria

- No golden frame ends mid-word. **Met** -- seven frames re-accepted, every one ends on a whole label or the ellipsis.
- `d Delete` is visible in `golden_mail_view_with_selection` at 120 columns. **Met**.
- The help overlay and the website table still show the long descriptions, byte-identical to `mp dump-keys --json`. **Met** -- `golden_help_overlay` is unchanged, and regenerating `website/src/data/tui-keys.json` produces a byte-identical file.
- A test pins that a binding with no short label falls back to `desc`. **Met** -- `a_binding_without_a_short_label_falls_back_to_its_description`, plus `only_hinted_rows_carry_a_short_label` and a width budget test per context.
