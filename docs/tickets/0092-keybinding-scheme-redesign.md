---
id: 0092
title: Keybinding scheme redesign to an nvim-style mnemonic prefix model
type: feature
priority: now
status: blocked
created: 2026-08-14
---

The keybinding scheme has accumulated systematic inconsistencies that the UX audit traces to dispatch code (§keybinding asymmetries, §discoverability).
The Body and Headers panes are near-dead for actions: from `Focus::Preview` or `Focus::Headers` you can only scroll, so acting on the message you are reading forces a focus hop back to the List, since every action is `KeyCtx::List` only (§asymmetry 1).
Global-feeling actions are trapped in List focus: `n` (new draft), `s`/`S` (sync), `f` (server search), `F` (flagged filter) do nothing from Sidebar, Headers, or Preview (§asymmetry 3).
There is no next/prev while reading; the body pane's `j`/`k` scroll the current body, so advancing means Esc/Tab back to the List, `j`, then Tab back (§b.1).
Esc and Tab are asymmetric: Preview has Esc to return to the list but Headers has none, and Tab cycles Sidebar -> List -> Preview -> Headers, putting the body before the headers against reading order (§asymmetry 2, 4).
The `o`/`O`/`b` bindings are live in Body and Headers but carry empty `keys` and are hidden from `?` help (§discoverability).

## Owner decision (2026-08-14)

Move to an nvim-style mnemonic prefix model: one prefix key per feature family, with a which-key-style hint popup showing the continuations.
A design plan is being written in parallel at [docs/plans/keybinding-redesign.md](../plans/keybinding-redesign.md); that plan is the design of record for this ticket.

This ticket subsumes the following audit findings, which the redesign must resolve:

- Actions unreachable from Body/Headers focus (§asymmetry 1, §c.2).
- List-trapped global keys `n`, `s`/`S`, `f`, `F` (§asymmetry 3, §c.6).
- No next/prev while reading (§b.1, §c.3).
- Esc and Tab asymmetries between the reading panes and the unusual focus-cycle order (§asymmetry 2, 4, §c.10).
- Hidden `o`/`O`/`b` bindings missing from `?` help (§discoverability, §c.5).

## Status

Blocked, awaiting review of [docs/plans/keybinding-redesign.md](../plans/keybinding-redesign.md).
Do not start implementation until that design is reviewed and accepted.

## Cross-references

Configurable keybindings were tracked in [#0019](0019-configurable-keybindings.md), dropped and subsumed into the keymap-as-data foundation from [#0032](0032-tui-foundation-package.md); this redesign is the next layer on that same `Action` enum + `KEYMAP` table.
The hint bar's short-label work landed in [#0078](0078-hint-bar-short-labels.md); the which-key-style popup here is the discoverability surface that builds on the same data-driven `KEYMAP`, and the redesign must keep the help overlay and website key table in step with it.

## Acceptance criteria

- The design in `docs/plans/keybinding-redesign.md` is reviewed and accepted before implementation begins.
- Every List action is reachable while reading (Body/Headers focus).
- `n`, `s`/`S`, `f`, `F` work from any focus.
- Next/prev message advance works from the reading panes without a focus hop.
- Esc and Tab behave symmetrically across panes, and the focus-cycle order follows reading order.
- No live binding is hidden from `?` help; `o`/`O`/`b` and every other reachable key appear in the help surface.
