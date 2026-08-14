# Design: mnemonic prefix keybinding scheme

> Status: approved with amendments (2026-08-14). Implementation may begin.
> Baseline: [.agents/research/2026-08-14-ux-workflow-audit.md] (full current matrix + friction).
> Related tickets: [#0019](../tickets/0019-configurable-keybindings.md) (configurable bindings, dropped/subsumed),
> [#0078](../tickets/0078-hint-bar-short-labels.md) (hint-bar short labels, done).
> Builds on: [tui-restructure-views.md](tui-restructure-views.md) Stage 1 (keymap-as-data, leader support).

## Review (2026-08-14)

The owner reviewed this plan and approved it with amendments.
The search/find family is `f` and the system/sync/accounts family is `s` (the two family letters swap).
Reply-all is `ca`, not `cR`.
Account switching gains a `ga` chord in the go/motion family.
The command palette is confirmed as part of the design, not an optional complement.
There is no legacy flat map and no transition period: the new scheme replaces the old one outright as a breaking change in the next release.
The flat triage key list and `J`/`K` for next/prev stand as the accepted default.
The sections below fold these amendments in.

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
   `f` opens search/find, `c` opens compose/create, `g` opens go/motion, `s` opens system/sync.
2. Same key, same meaning, every pane.
   A message action resolves identically in List, Headers, and Body.
   This is done by promoting the message-action set from `KeyCtx::List` to a shared message context applied to all three panes, so `a` is archive whether the cursor is in the list or the body.
3. Actions on the current message work from any focus.
   Triage, reply, send, next/prev all target the cursor message regardless of which pane holds focus.
4. Flat escape hatches for the highest-frequency actions.
   Not every triage key should cost two keystrokes.
   The flat set below stays single-key; everything rarer moves under a family prefix.

### Which keys stay flat (accepted default, owner review 2026-08-14)

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

Account switching is not flat: it lives at `ga` in the go family (see below), replacing the old flat `` ` `` cycle and `Ctrl+1-9` jump.

Reading auto-marks read (decision), so `u` is only the explicit override for the rare correction.

### `g` go / motion family

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `gg` / `G` | Top / bottom | Message panes | `gg`/`G` (List only) |
| `gt` | Jump to date | List | `g t` (same) |
| `gj` / `gk` | Next / previous message | Message panes | none (alias of `J`/`K`) |
| `gm` | Jump to mailbox by name (picker) | Global | none (`1-9` only) |
| `ga` | Switch account (picker), or `g` then an account initial to jump (e.g. `ga` then `t` for tum, `p` for proton) | Global (multi-account) | `` ` `` cycle, `Ctrl+1-9` jump |

`ga` is the mnemonic home for account switching (go to account).
It replaces the flat `` ` `` cycle and `Ctrl+1-9` jump from the flat table, which are dropped in favour of the picker-or-initial motion: with two accounts a picker keyed by initial is faster than a cycle, and the go family is where every other jump already lives (mailbox, date, top/bottom).
Account switching therefore leaves the system family and lives here as a motion; the system family keeps sync, logs, and config.

### `f` search / find family (one FTS-backed entry, sender-searchable)

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `ff` | Search all mail (FTS, sender + subject + body, all mailboxes) | Global | `f` server search, `\` content, split scope |
| `fm` | Filter the current list (metadata, incremental) | Message panes | `/` |
| `fF` | Toggle flagged-only filter | List | `F` |

`ff` subsumes `/` body inclusion, `\`, and the old flat `f`: one overlay, scope chosen inside it, sender-searchable by default.
`fm` keeps the instant in-list narrowing that `/` gave.
The flagged-only filter is `fF` rather than `ff` because `ff` is the search entry; both stay inside the find family (find flagged, find anything).

### `c` compose / create family

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `cn` | New draft | Global | `n` (List only) |
| `cr` | Reply (alias of flat `r`) | Message panes | `r` |
| `ca` | Reply-all (compose, reply [a]ll) | Message panes | `R` (List only) |
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

### `s` system / sync / accounts family

| Key | Action | Live in | Current |
|-----|--------|---------|---------|
| `ss` | Quick sync | Global | `s` (List only) |
| `sS` | Full sync | Global | `S` (List only) |
| `sl` | Activity log overlay | Global | `L` |
| `sc` | Open config.toml in $EDITOR | Global | `Ctrl+e` |
| `sf` | Open log file in $EDITOR | Global | `Ctrl+l` |

Account switching itself is `ga` in the go family, so this family carries no account-switch chord; the family name keeps "accounts" because sync operates per account and the account state is what these keys act on.
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

## Command palette (confirmed)

A command palette is part of the design.
`:` or `Ctrl+p` opens a fuzzy finder over the `KeyAction` catalogue, so a user who does not remember a chord can type an action name and run it.
It reads the same `KEYMAP` that the families, help overlay, and hint bar read, so it needs no second catalogue.
The palette complements the families rather than replacing them: the families are the fast path for a remembered action, the palette is the recall path for a forgotten one.
It ships as its own ticket after the keymap-as-data foundation is in place, since it depends on the same `KeyAction` catalogue the families build on and adds no behaviour the families need.

## Conflicts and edge cases

- Keys overloaded per pane today (`o`/`O`/`b` live but hidden in Headers/Body; `V` in List/Body but not Headers) collapse into the single `t` family live in all message panes, closing the asymmetry the audit flagged.
- Text-input contexts (the `ff` search prompt, the compose wizard, confirm dialogs) run as overlay `Mode`s and never consult the Normal-mode resolver, so prefix keys cannot fire mid-typing; this holds today and the redesign preserves it.
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

The new scheme replaces the old one outright.
There is no legacy flat map and no transition period: the next release ships the family scheme as a breaking change, and the old flat map is gone.
Keymap-as-data (Stage 1 of the restructure) is already shipped, so the leaders, the resolver, help, hint bar, and website all read one table, and swapping the table swaps every surface at once.

Work order within the one release, so review can land it in reviewable pieces rather than a phased rollout of two coexisting maps:

- The behaviour fixes that do not depend on the family letters: promote the message-action set to the shared message context (List + Headers + Body), add `J`/`K` next/prev, auto-mark-read on open, and merge approve+send into flat `x` with one confirm.
- The families themselves: introduce the `f`/`c`/`g`/`t`/`s` prefixes and the which-key popup, moving the rarer actions under them and deleting the old flat bindings they replace.
- The `#0019` config override layer on top, so a user who wants a different key or a flattened family action rebinds it, with no default legacy map to fall back to.

The changelog for the release calls out the keymap change as breaking, and the `?` help overlay plus the website key table (both generated from `KEYMAP`) are the migration reference for existing users.

## Decisions (resolved 2026-08-14) and remaining open items

Resolved in the owner review:

- Prefix letters: `f` (search/find), `c` (compose), `g` (go), `t` (thread/attach), `s` (system/sync/accounts).
  The search and system letters swapped from the first draft (`s`/`y`) to `f`/`s`.
- Reply-all is `ca`.
- Account switching is a `ga` chord in the go family, replacing the flat `` ` `` cycle and `Ctrl+1-9` jump.
- The command palette is confirmed and ships as its own ticket after the keymap-as-data foundation.
- There is no legacy flat map: the new scheme replaces the old one as a breaking change in the next release.

Accepted as the default (the owner did not object during review, so these stand):

- Flat triage set: `J`/`K` `r` `a` `d` `u` `*` `x` `Enter`/`e` `M`.
- Next/prev message is flat `J`/`K` (with `gj`/`gk` as the go-family alias).

Still genuinely open:

- Whether Space stays the view leader or becomes a general command leader that also holds sync and accounts.
