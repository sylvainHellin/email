# Design: mnemonic prefix keybinding scheme

> Status: proposal, nothing implemented (2026-08-14). Owner review required before any code.
> Baseline: [.agents/research/2026-08-14-ux-workflow-audit.md] (full current matrix + friction).
> Related tickets: [#0019](../tickets/0019-configurable-keybindings.md) (configurable bindings, dropped/subsumed),
> [#0078](../tickets/0078-hint-bar-short-labels.md) (hint-bar short labels, done).
> Builds on: [tui-restructure-views.md](tui-restructure-views.md) Stage 1 (keymap-as-data, leader support).

## Why this doc

The current map is a flat single-key table whose meaning changes with pane focus.
The audit traces the cost.
The reading panes (Headers, Body) can only scroll, so acting on the message being read means moving focus back to the list first.
High-frequency global actions (`n`, `s`/`S`, `f`) are trapped in List focus.
Search has three entry points (`/`, `\`, `f`) that differ only in scope.
Send is an eight-step, approve-then-send-then-confirm tail.

The owner's direction is an nvim-like mnemonic model: one prefix key per feature family, with the highest-frequency triage keys kept flat as escape hatches.
The keymap-as-data foundation (`src/tui/app/keymap.rs`) already models leaders as first-class `prefix` chords and resolves Global-then-pane, so this redesign is a new table plus a which-key popup on top of dispatch that already exists.

## Decisions already made (from the owner, 2026-08-14) that the scheme must honour

- Search collapses to one entry point, sender-searchable and FTS-backed.
- Send works on the current draft from any focus with one confirm; approve and send merge when the draft is unapproved.
- Opening a message auto-marks it read.
- Message actions, including next/prev message, are reachable while reading (Headers/Body focus) without leaving the reading pane.

## Design principles

1. Mnemonic prefix families.
   One leader key opens a family; the continuation key names the action inside it.
   `s` opens search, `c` opens compose/create, `g` opens go/motion, `y` opens system/sync.
2. Same key, same meaning, every pane.
   A message action resolves identically in List, Headers, and Body.
   This is done by promoting the message-action set from `KeyCtx::List` to a shared message context applied to all three panes, so `a` is archive whether the cursor is in the list or the body.
3. Actions on the current message work from any focus.
   Triage, reply, send, next/prev all target the cursor message regardless of which pane holds focus.
4. Flat escape hatches for the highest-frequency actions.
   Not every triage key should cost two keystrokes.
   The flat set below stays single-key; everything rarer moves under a family prefix.

### Which keys stay flat (proposed, an owner decision)

Triage and reading are the hot path, so these keep single-key bindings in every message pane:
`j`/`k` move the cursor (list) or scroll (body), `J`/`K` next/prev message from anywhere,
`Enter`/`e` open, `r` reply, `a` archive, `d` delete, `u` toggle unread, `*` flag, `x` send current draft, `Esc` back to list.
Rarer actions (reply-all, forward, move, thread, attachments, RSVP, sync, accounts, config) move under a family prefix.

## Proposed key map

Columns: family prefix, continuation key, action, panes it is live in, and the current binding for migration visibility.
"Message panes" means List + Headers + Body (the promotion in principle 2).

### Flat / global (no prefix)

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `q` | Quit | Global | `q` (same) |
| `?` | Help overlay | Global | `?` (same) |
| `1-9` | Jump to mailbox | Global | `1-9` (same) |
| `Tab` / `Shift+Tab` | Cycle focus (reading order: Headers then Body) | Global | `Tab` cycles Body before Headers |
| `j` / `k` | Move cursor (list) or scroll (body/headers) | Message panes | `j`/`k` (list-only for cursor) |
| `J` / `K` | Next / previous message | Message panes | none (list `j`/`k` + refocus) |
| `Enter` / `e` | Open in editor | Message panes | `Enter`/`e` (List only) |
| `r` | Reply | Message panes | `r` (List only) |
| `a` | Archive | Message panes | `a` (List only) |
| `d` | Delete | Message panes | `d` (List only) |
| `u` | Toggle read/unread | Message panes | `m` (List only) |
| `*` | Toggle flag | Message panes | `*` (List only) |
| `x` | Send current draft (approve+send, one confirm) | Global | `A` then `x`/`X`, List only |
| `M` | Move to mailbox (picker) | Message panes | `M` (List only) |
| `v` | Toggle selection | List | `v` (same) |
| `Esc` | Clear selection, else return to list | Message panes | `Esc` (List clears, Body returns, Headers nothing) |
| `` ` `` / `Ctrl+1-9` | Switch / jump account | Global (multi-account) | same |

Reading auto-marks read (decision), so `u` is only the explicit override for the rare correction.

### `g` go / motion family

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `gg` / `G` | Top / bottom | Message panes | `gg`/`G` (List only) |
| `gt` | Jump to date | List | `g t` (same) |
| `gj` / `gk` | Next / previous message | Message panes | none (alias of `J`/`K`) |
| `gm` | Jump to mailbox by name (picker) | Global | none (`1-9` only) |

### `s` search family (one FTS-backed entry, sender-searchable)

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `ss` | Search all mail (FTS, sender + subject + body, all mailboxes) | Global | `f` server search, `\` content, split scope |
| `sm` | Filter the current list (metadata, incremental) | Message panes | `/` |
| `sf` | Toggle flagged-only filter | List | `F` |

`ss` subsumes `/` body inclusion, `\`, and `f`: one overlay, scope chosen inside it, sender-searchable by default.
`sm` keeps the instant in-list narrowing that `/` gave.

### `c` compose / create family

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `cn` | New draft | Global | `n` (List only) |
| `cr` | Reply (alias of flat `r`) | Message panes | `r` |
| `cR` | Reply-all | Message panes | `R` (List only) |
| `cf` | Forward | Message panes | `w` (List only) |
| `ce` | Edit recipients (Drafts only) | List | `c` (Drafts only) |

### `t` thread / attachment family

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `tt` | Show conversation (thread) | Message panes | `T` (List only) |
| `to` | Open attachment | Message panes | `o` (List/Headers/Body, hidden in help) |
| `ts` | Save attachment to disk | Message panes | `O` |
| `tb` | Open HTML in browser | Message panes | `b` |
| `tv` | RSVP to invitation | Message panes | `V` (List/Body, not Headers) |

### `y` system / sync / accounts family

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `ys` | Quick sync | Global | `s` (List only) |
| `yS` | Full sync | Global | `S` (List only) |
| `ya` | Switch account | Global (multi-account) | `` ` `` |
| `yl` | Activity log overlay | Global | `L` |
| `yc` | Open config.toml in $EDITOR | Global | `Ctrl+e` |
| `yf` | Open log file in $EDITOR | Global | `Ctrl+l` |

`y` is chosen because `s` is taken by search; the owner may prefer a different letter (see open decisions).
Zoom (`z`) and the inline activity toggle (`!`) stay flat.

### View switch (unchanged, already shipped)

`Space m` / `Space c` / `Space a` switch Mail / Contacts / Calendar; digits stay mailbox jump.
Space remains the view leader unless the owner folds views into a general command leader (open decision).

## Which-key discoverability

After a prefix key is pressed, a which-key popup lists that family's continuations (key plus short label), driven by `prefix_continuations(ctx, prefix)`, which already exists.
This is a small centered overlay, dismissed by the continuation key, `Esc`, or a timeout.
It reuses the `#0078` short labels directly, so the popup and the hint bar read the same wording.

Relationship to the existing surfaces:

- The hint bar keeps showing the flat keys plus the pending-prefix continuations, unchanged in mechanism (`render_hint_bar` already switches on pending prefix); the redesign only shrinks the flat set it must fit, which eases the `#0078` overflow.
- The `?` help overlay stays the full reference, now grouped by family rather than by pane, generated from the same `KEYMAP` (no third copy).
- An optional command palette (`:` or `Ctrl+p`) opens a fuzzy finder over the `KeyAction` catalogue for users who do not remember a chord.
  It complements the families rather than replacing them, and can land after them.

## Conflicts and edge cases

- Keys overloaded per pane today (`o`/`O`/`b` live but hidden in Headers/Body; `V` in List/Body but not Headers) collapse into the single `t` family live in all message panes, closing the asymmetry the audit flagged.
- Text-input contexts (the `ss` search prompt, the compose wizard, confirm dialogs) run as overlay `Mode`s and never consult the Normal-mode resolver, so prefix keys cannot fire mid-typing; this holds today and the redesign preserves it.
  The one rule to enforce: a family leader pressed while an input overlay is active is literal text, not a leader.
- Esc is made symmetric: from any message pane Esc first clears a selection if one exists, else returns focus to the list; Headers gains the Esc binding it lacks today.
- Tab is made symmetric: focus cycles in reading order (Sidebar, List, Headers, Body), fixing the current Body-before-Headers order.
- The `no_duplicate_live_dispatch_per_context` test already refuses two rows that match the same chord in one context; every new family row is checked against it, so `gd` versus flat `d` style ambiguities fail the build rather than ship (the reason today's jump-to-date is `gt`, not `gd`).

## Interaction with #0019 (configurable keybindings)

The prefix families are the default map, expressed as `KEYMAP` rows.
`#0019`'s config model (`[keybindings] action = "key"`) becomes an override layer: the user rebinds an action to a different key or a different chord, and unspecified actions fall back to the default family binding.
Two extensions to the `#0019` sketch:

- A binding value may be a chord (`"cf"`) or a single key (`"w"`), so a user can flatten a family action they use constantly or re-nest a flat one.
- Conflict detection runs over resolved chords per context, reusing the existing duplicate-dispatch invariant, and errors at config load exactly as `#0019` specified.

The prefix model does not block `#0019`; it gives it a coherent default to override.

## Migration plan

Keymap-as-data (Stage 1 of the restructure) is already shipped, so the leaders, the resolver, help, hint bar, and website all read one table.
Phasing:

- Phase 1, behaviour decisions independent of families: promote the message-action set to the shared message context (List + Headers + Body), add `J`/`K` next/prev, auto-mark-read on open, and merge approve+send into flat `x` with one confirm.
  This lands the audit's highest-leverage fixes without renaming any keys.
- Phase 2, families: introduce the `s`/`c`/`g`/`t`/`y` prefixes and the which-key popup, moving the rarer actions under them.
  Keep the current flat keys (`n`, `s`/`S`, `f`, `R`, `w`, `T`, `M`, `V`, `o`/`O`/`b`) live as legacy aliases behind a `legacy_keys = true` config default so no muscle memory breaks on upgrade.
- Phase 3, config overrides: ship `#0019` on top, and flip `legacy_keys` to default off one release later, leaving it as an opt-in for users who want the old flat map.

A legacy flat map therefore stays through Phase 2 and one release into Phase 3, then survives only as opt-in config.

## Open decisions for the owner

- Which triage keys stay flat: the proposed flat set is `J`/`K` `r` `a` `d` `u` `*` `x` `Enter`/`e` `M`; trim or extend it.
- Prefix letters: `s` (search), `c` (compose), `g` (go), `t` (thread/attach), `y` (system).
  `y` is the weakest mnemonic; alternatives are a `Space`-led command menu or `,` as a general leader.
- Next/prev binding: flat `J`/`K`, or `gj`/`gk`, or `n`/`p` (which frees, or collides with, `n` new draft).
- Whether Space stays the view leader or becomes a general command leader that also holds sync/accounts.
- Whether the command palette ships in Phase 2 or later, and its key (`:` versus `Ctrl+p`).
- How long the legacy flat map stays the default (one release, or longer).
