---
id: 0100
title: Command palette (fuzzy finder over the KeyAction catalogue)
type: feature
priority: next
status: open
created: 2026-08-14
---

A command palette is a confirmed part of the approved keybinding redesign ([docs/plans/keybinding-redesign.md](../plans/keybinding-redesign.md), "Command palette (confirmed)"), and command palettes are a baseline expectation for keyboard-first clients (feature survey §b "Command palette", §(c) shortlist item 9: "keyboard users expect Superhuman-grade discoverability").

`:` or `Ctrl+p` opens a fuzzy finder over the `KeyAction` catalogue.
The user types an action name and runs it, without remembering the chord.
It reads the same `KEYMAP` that the mnemonic families, the `?` help overlay, and the hint bar read, so it needs no second catalogue (UX audit §discoverability describes the `KEYMAP`-generated help surface it reuses).

## Relationship to the families

The palette complements the mnemonic prefix families rather than replacing them: the families are the fast path for a remembered action, the palette is the recall path for a forgotten one (keybinding plan, "Command palette (confirmed)").

## Scope

- `:` or `Ctrl+p` opens an overlay `Mode` (text-input, so family leaders typed inside it are literal, consistent with the search prompt and compose wizard).
- Fuzzy-match over the action names / short labels already in `KEYMAP`.
- Enter runs the selected action against the current context; `Esc` closes.

## Cross-references

- Depends on [#0092](0092-keybinding-scheme-redesign.md): the palette ships after the keymap-as-data foundation and the family scheme, since it consumes the same `KeyAction` catalogue and adds no behaviour the families need.
- Shares the `KEYMAP` surface with the which-key popup and `?` help overlay described in #0092 and [#0078](0078-hint-bar-short-labels.md).

## Acceptance criteria

- `:` and `Ctrl+p` open a fuzzy finder over the `KeyAction` catalogue.
- Selecting an entry runs that action in the current context.
- The palette derives from `KEYMAP`; there is no second, hand-maintained action list.
