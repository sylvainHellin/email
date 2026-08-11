---
id: 0086
title: Server search parity - one grammar, honest per-backend translation, an Outlook-shape TUI form
type: feature
priority: later
status: done
created: 2026-08-11
---

Bring `mp`'s server search up to what mainstream clients do. Three concrete gaps
Sylvain hit:

1. **You cannot search for mail *with attachments*.** No grammar term, no UI, no
   backend path. Plain IMAP has no attachment test at all (see Capabilities).
2. **You cannot combine conditions.** The grammar AND-s everything and has no
   `OR`, no grouping. "from a sender AND (keyword1 OR keyword2) AND has
   attachments" is inexpressible.
3. **The UI is a single text line.** Sylvain wants a form. His reference is
   Outlook's search panel: fields `Search In [Current Mailbox ▾]`, `From`, `To`,
   `Subject`, `Keywords`, `Date [Any ▾]`, `Attachment [toggle]`, `Add more
   options`, `Save Search`, and a `Search` button.

There is also a standing debt: **two query grammars** ship today, one for the
server path and one for `--local` FTS (called out in
[#0043](0043-fts5-search.md) as "three affordances kept from the server-side
grammar"). This ticket resolves that by making both paths render **one** grammar.

---

## Decisions (2026-08-11)

Sylvain's product calls, closing the three open questions and re-scoping the
ticket.
These are binding.
The design text below is kept for context, but where it conflicts, these win.

- **Q1 - plain-IMAP `has:attachment`: option (b) ONLY.** No `BODYSTRUCTURE`
  round-trip. `has:attachment` is stripped from the server query and the residue
  is answered from the store's `messages.has_attachments` column, with a printed
  warning that un-synced mail is not covered (run `mp sync` for full coverage).
  Option (a) is dropped.
- **Q2 - dates: custom `before:` / `after:` only.** No Outlook-style presets
  (Today / This Week / This Month / This Year).
  The TUI date control (in #0086b) becomes two plain date fields.
- **Q3 - scope: per-account only.** No "All Accounts" scope, in the CLI or the
  form.
  Scope is the focused mailbox or the current account's mailboxes.
- **#0086c saved searches: DROPPED, not deferred.** No persistence, no picker,
  no "Save Search" button anywhere.
  Removed from the plan entirely.

Consequences for the scopes below: **scope 3 (the TUI form, #0086b)** drops the
Save-Search button, replaces the Date dropdown with two date fields
(`After` / `Before`), and the "Search In" control loses the All-Accounts option
(Current Mailbox / Current Account only).

---

## Delivered (#0086b, 2026-08-11)

The TUI form shipped.
Choices pinned during implementation.

Entry binding: no new key.
`f` (`KeyAction::ServerSearch`, "Search (IMAP)") already opened the single-line
`Overlay::Search`; it now opens the form.
The form replaces the overlay and its `Advanced` line is the power lane, so the
old quick single-line grammar stays reachable there with the same muscle memory.
No KEYMAP row changed, so `mp dump-keys`, the website key table, and the help
overlay are untouched and no regen was needed.
(`/` and `\` were never the server-search openers; they are the in-list metadata
filter and content search, so there was no collision to resolve.)

Advanced-vs-fields rule, one direction: a non-blank `Advanced` line takes over.
While it holds anything, the structured fields are greyed and ignored (Tab skips
them; typing into them is refused), and the built query is exactly what
`search::parse(advanced)` yields.
Clear the Advanced line and the structured fields drive again.

Form to AST: the form builds a `search::Query` directly, no string
concatenation.
The `Keywords` field is parsed as the positional grammar (so `a OR b` and quoted
phrases work) and `From`, `To`, `Subject`, `After`, `Before`, attachment become
`search::Flags`; both feed `search::from_cli`, the same lowering the CLI uses.
A blank form is a no-op.
`Action::ServerSearch` now carries the parsed `Query`, not a string, so the
helpers render it without re-parsing.

Scope wiring: the `Search In` toggle picks targets, Current Mailbox to the
focused mailbox and Current Account to `all_search_targets()`.
An `in:` in the Advanced line still overrides to a named mailbox.

Focus and keys: Tab and Shift+Tab cycle fields, Space flips the scope and
attachment toggles, Enter searches, Esc closes, the compose-wizard conventions.
A `ParseError` from the Advanced line is surfaced verbatim in the overlay status.

---

## Where search lives today (integration map, cite before you touch)

**CLI grammar + IMAP translation** - `src/imap_client/search.rs`
- `FetchCriteria` struct: `src/imap_client/search.rs:2` - fields `from,to,cc,subject,body,since,before,text,message_id,in_mailbox`. No attachment, no OR, no grouping.
- `parse_search_query(input)`: `src/imap_client/search.rs:118` - hand-rolled prefix scanner (`from: to: cc: subject: body: since: before: in: message-id:`), bare words fold into `text`. Case-insensitive prefixes, quoted values via `extract_search_value` (`:230`).
- `build_imap_search_query(criteria)`: `src/imap_client/search.rs:74` - emits `FROM/TO/CC/SUBJECT/BODY/HEADER Message-ID/SINCE/BEFORE/TEXT`, **space-joined = implicit AND only**. Empty → `ALL`.
- `parse_date_to_imap`: `src/imap_client/search.rs:247` - `YYYY-MM-DD` → `D-Mon-YYYY`.

**Graph translation** - `src/graph.rs`
- `parse_search_to_graph_params(criteria)`: `src/graph.rs:2446` → `(Option<$search>, Option<$filter>)`. `from/to/cc/since/before/message_id` → `$filter`; `subject/body/text` → `$search`. Joined with ` and ` / space.
- `search_messages`: `src/graph.rs:2303`; `$search` fallback to `search_messages_filter_only`: `src/graph.rs:2382`. `$select` already pulls `hasAttachments` (`src/graph.rs:27`); `has_attachments` on the response row: `src/graph.rs:205`, `:843`.

**Local FTS grammar** - `src/store/search.rs`
- `fts_expression(query)`: `src/store/search.rs` (`pub fn fts_expression`) - its own parser: `split_terms` keeps quoted phrases, `column_for` maps `subject→subject`, `from→from_`, `body|text→body_text` (**note: no `to`/`cc` columns indexed**), trailing `*` = prefix, unknown `field:` = literal text. Terms AND-ed. **This is the second grammar.**
- `search(store, account, query, mailbox, limit)`: joins `messages_fts` to `messages`; `bm25(messages_fts, 10.0, 5.0, 1.0)`.
- `messages` already carries `has_attachments`: `src/store/read.rs:81` (struct), `:136` (SELECT), `:156` (map) - so an attachment predicate over synced mail is a plain SQL `AND messages.has_attachments = 1`, no schema change.

**CLI command** - `src/main.rs`
- `Commands::Search { query, mailbox, limit, full, local }`: `src/main.rs:308`; handler `src/main.rs:2604`. `--local` → `store::search::search`; else `parse_search_query` → IMAP or Graph. Scope: `--mailbox` > `in:` prefix > all configured mailboxes (`src/main.rs:2648`+).

**TUI overlay** - already exists (single-line)
- Open: `A::ServerSearch` at `src/tui/app/keys.rs:630`; overlay enum `Overlay::Search` routed at `src/tui/app/keys.rs:31`.
- State: `App::server_search_*` at `src/tui/app/mod.rs:172`-`180` (query, focus, results, index, scrolls, loading, status, `server_search_scope_label`).
- Key handling: `handle_search_overlay_key` `src/tui/app/keys.rs:826`; Enter builds scope from `criteria.in_mailbox` via `search_target_by_name` (`src/tui/app/mod.rs:794`) else `all_search_targets` (`src/tui/app/mod.rs:783`), then `Action::ServerSearch`.
- Dispatch: `Action::ServerSearch` `src/tui/actions.rs:1787` → `lib_do_multi_search` (`src/tui/helpers.rs:403`) or `lib_do_multi_search_graph` (`src/tui/helpers.rs:504`), capped at 50 hits.
- Render: `src/tui/ui/search.rs` - input + `from:/to:/subject:/body:/since:/before:/in:` hint row; results already show a paperclip when `result.entry.has_attachments` (`src/tui/ui/search.rs`, `subject_prefix`).
- Overlay widget kit (#0032): `src/tui/ui/widgets.rs` - `centered_modal_rect` (`:63`), `render_modal_shell` (`:82`), `modal_stack_areas` (`:107`, header/content/footer split), `render_action_button` (`:143`). Use these for the form so it matches every other overlay.

---

## Capabilities (verified - what each backend can actually do)

**Plain IMAP (RFC 3501, the xmail.mwn.de class)** - *verified against the RFC text.*
- Search keys: `FROM TO CC BCC SUBJECT BODY TEXT HEADER SINCE BEFORE ON SENTSINCE/SENTBEFORE KEYWORD LARGER SMALLER`.
- Boolean: `OR <key1> <key2>` (binary - 3+ terms nest: `OR a OR b c`), `NOT <key>`, AND is implicit (space).
- **No attachment test exists.** `grep -c attachment` over RFC 3501 = 2, both prose, zero search keys.
- `SEARCHRES` (RFC 5182, the `$` marker) and `WITHIN` (RFC 5032, `YOUNGER/OLDER` in seconds) add result-reuse and relative dates - **still no attachment test.**
- Source: `https://www.rfc-editor.org/rfc/rfc3501.txt` §6.4.4 (fetched and grepped during this design; `OR/NOT/KEYWORD/LARGER` present, no attachment key).

**Gmail IMAP (`X-GM-RAW`)** - advertised via `X-GM-EXT-1` capability.
- `SEARCH X-GM-RAW "has:attachment from:x (a OR b)"` passes Gmail's own operators server-side: `has:attachment`, `filename:pdf`, `larger:`, `OR`, parens.
- Source: Google, *IMAP Extensions* (`developers.google.com/gmail/imap/imap-extensions`). Not re-fetched live (JS-rendered); this is stable, long-documented API behaviour.

**Microsoft Graph** - `$search` (KQL) and `$filter`.
- `$filter=hasAttachments eq true`; KQL `$search="hasAttachments:true"`. `from/to` via `$filter` lambdas (already done, `src/graph.rs:2450`+); `subject/body` via `$search`. Boolean `OR`/`AND` in both.
- Source: MS Learn, *List messages* + *Search parameter*. Not re-fetched live (JS-rendered); `hasAttachments` is already in the code's `$select`.

**Local store (mp's own SQLite mirror)**
- `messages.has_attachments` is known for **every synced message** (`src/store/read.rs`). FTS covers **synced subject/from/body** only. `to`/`cc` are *not* FTS columns today.

### The honest consequence
- On **Gmail** and **Graph**, every field including `has:attachment` runs server-side.
- On **plain IMAP**, `has:attachment` **cannot** run server-side. Two honest options, decide per Q1:
  - **(a) BODYSTRUCTURE post-filter** - run the server `SEARCH` with everything it supports, then `FETCH BODYSTRUCTURE` the candidate UIDs and keep those with a non-inline / `multipart/mixed` part. Accurate for all mail on the server; costs one FETCH round-trip over the candidate set. Warn if the candidate set was capped (an attachment hit past the cap is invisible).
  - **(b) Local-only attachment filter** - serve `has:attachment` from `messages.has_attachments` (synced mail only) and **warn** "attachment filter covers synced mail; run `mp sync` for full coverage."

---

## Proposed design

### 1. One grammar, one AST (resolves the #0043 two-grammar debt)

Introduce a backend-agnostic query model - a new `src/search/query.rs` (or fold
into `store::search`) - parsed **once**:

```
Query = Vec<Clause>            // clauses AND-ed together
Clause = Or(Vec<Term>) | Single(Term)
Term = From(String) | To(String) | Cc(String) | Subject(String)
     | Body(String) | Text(String) | HasAttachment | Before(Date) | After(Date)
     | Filename(String)   // optional, Gmail/Graph/local only
```

Surface grammar (what a user types; superset of both current grammars):
- Fields: `from: to: cc: subject: body: has:attachment before:YYYY-MM-DD after:YYYY-MM-DD` (`after:` = IMAP `SINCE`; keep `since:` as an alias). Optional: `filename: larger: smaller:`.
- **OR groups:** `(a OR b)` - parenthesised, `OR`-separated terms.
- **Quoted phrases:** `from:"Ada Lovelace"`, `"quarterly report"`.
- Bare words → `Text`.
- Back-compat: today's `in:MAILBOX` stays as the scope directive (not a match term); `message-id:` stays.

`parse_search_query` (`src/imap_client/search.rs:118`) and `fts_expression`'s
term splitter (`src/store/search.rs`) both **retire** in favour of this one
parser. `FetchCriteria` becomes a *lowering target* of the AST (or is replaced by
it). `fts_expression` becomes a **renderer** of the AST, not a second parser -
that is the concrete discharge of the #0043 debt.

Three renderers, one AST:
- `to_imap(query) -> String` (replaces `build_imap_search_query`), emits nested `OR`, `SINCE/BEFORE`, and - for Gmail - an `X-GM-RAW` string.
- `to_graph(query) -> (search, filter)` (replaces `parse_search_to_graph_params`), `hasAttachments eq true` in `$filter`.
- `to_fts(query) -> (match_expr, sql_predicates)` - FTS `MATCH` for text columns + `AND messages.has_attachments=1` / date-range SQL for the predicates FTS can't express.

### 2. Per-backend translation matrix (with honest degradation)

| Field | Plain IMAP | Gmail (`X-GM-RAW`) | Graph | Local FTS |
|---|---|---|---|---|
| from/to/cc/subject/body | native `FROM/TO/…` | native | `$filter` (from/to/cc) + `$search` (subj/body) | `from_`/`subject`/`body_text` columns; **to/cc gap** → post-filter or note |
| free text | `TEXT` | native | `$search` | all columns |
| before/after | `BEFORE`/`SINCE` | `before:`/`after:` | `receivedDateTime lt/ge` | `messages.date_sort` SQL |
| **has:attachment** | **no server test** → **post-filter** (Q1: BODYSTRUCTURE or local) + **warn** | `has:attachment` server-side | `hasAttachments eq true` | `messages.has_attachments=1` (synced only) |
| **(a OR b)** | `OR a b` (nested) | `OR` in raw | `or` / KQL `OR` | FTS `OR` operator |

Rule: **server does everything it can; the residue post-filters locally and says
so.** The only residue on a modern account is nothing; on plain IMAP it is
`has:attachment` (Q1). Every degraded run prints/`set_status` one line naming what
ran locally and whether the candidate set was capped.

### 3. TUI search form overlay (Outlook shape) - #0086b, per Decisions above

Replace the single-line `Overlay::Search` input with a form built from the #0032
widget kit (`modal_stack_areas` header/content/footer; `render_action_button` for
`Search`). Fields, mapped to the AST:

| Outlook field | mp field | AST term |
|---|---|---|
| Search In [▾] | scope dropdown: **Current Mailbox** / **Current Account (all mailboxes)** | targets (see below) |
| From | text | `From` |
| To | text | `To` |
| Subject | text | `Subject` |
| Keywords | text | `Text` (subject+body) |
| After / Before | two date fields (`YYYY-MM-DD`), custom only | `After` / `Before` |
| Attachment [toggle] | bool | `HasAttachment` |
| Add more options | reveals Cc, Body-only, Filename, Size | `Cc`/`Body`/`Filename`/`Larger` |
| Search (button) | dispatch | build `Query`, `Action::ServerSearch` |

Per the Decisions above: **no Save-Search button** (#0086c dropped), the date
control is **two custom date fields** not a preset dropdown (Q2), and the scope
control has **no All-Accounts option** (Q3).

- Scope wiring maps *Current Mailbox* to a single `SearchTarget` for the focused mailbox and *Current Account* to `all_search_targets()` (`src/tui/app/mod.rs:783`, today's default), with no cross-account merge.
- Focus cycles fields with `Tab`/`BackTab`, the toggle flips on `Space`, and dropdowns open on `Enter`, reusing `SearchOverlayFocus` widened to a field enum.
- The escape hatch keeps a raw-grammar line ("Advanced") so power users type `from:x (a OR b) has:attachment` directly, parsed to the same AST via `mailypoppins::search::parse` (the #0086a engine), which preserves the current muscle memory while the form drives the same engine.
- Results list and preview panes (`src/tui/ui/search.rs`) are unchanged and already flag attachments.

### 4. CLI flags mirroring the same fields (for scripting)

Extend `Commands::Search` (`src/main.rs:308`). All flags are sugar that build the
**same AST** as the positional query; positional grammar still accepted:

```
mp search [QUERY]
  --from <s> --to <s> --cc <s> --subject <s> --body <s>
  --has-attachment
  --after <YYYY-MM-DD> --before <YYYY-MM-DD>
  --mailbox <MB>   (per-account only; no --all-accounts, per Q3)
  -n <N> --full --local
```

`mp search --from boss@corp.com --has-attachment "invoice OR receipt"` and
`mp search 'from:boss@corp.com (invoice OR receipt) has:attachment'` produce the
identical `Query`.

### Saved searches - DROPPED (was stretch #0086c)
Outlook's "Save Search" was a candidate stretch. Per the Decisions above it is
**dropped, not deferred**: no persistence, no picker, no "Save Search" button.
Not part of #0086 in any phase.

---

## Acceptance criteria

1. **Sylvain's exact query works on every backend.**
   `mp search 'from:boss@corp.com (invoice OR receipt) has:attachment'` returns
   mail from that sender that contains `invoice` OR `receipt` AND has an
   attachment. On Gmail/Graph the whole predicate runs server-side; on plain
   IMAP the `has:attachment` residue is resolved per Q1 and the run prints how.
2. **One grammar.** `parse_search_query` and `fts_expression`'s parser are gone;
   a single parser feeds `to_imap`/`to_graph`/`to_fts`. A test asserts the same
   input string produces consistent IMAP, Graph and FTS renderings (the #0043
   two-grammar debt is closed and has a test that says so).
3. **`has:attachment` exists end to end:** grammar term, CLI `--has-attachment`,
   TUI toggle, and each backend renderer, with the plain-IMAP degradation warning.
4. **`(a OR b)` grouping** parses and lowers correctly on all four renderers
   (IMAP nesting, Gmail raw, Graph, FTS).
5. **TUI form** in the Outlook shape: Search In / From / To / Subject / Keywords /
   After / Before / Attachment, a Search button, and the raw-query escape hatch.
   Scope dropdown maps to mailbox / account only (no all-accounts, per Q3).
6. **CLI flags** mirror every field and are equivalent to the positional grammar.
7. Existing behaviour (`in:`, `message-id:`, `--local`, `--mailbox`, the 50-hit
   TUI cap) is preserved; all current `search.rs` tests pass or are migrated.

## Effort & split

Bigger than one ticket. Suggested split:

- **#0086a - Grammar + backends + CLI (the engine).** The AST + parser, three
  renderers, the plain-IMAP attachment post-filter, `--has-attachment`/`--after`
  etc. flags, and migration of the existing tests. This alone fixes gaps (1) and
  (2) for scripting and discharges the #0043 debt. **~M (3-5 days).**
- **#0086b - TUI Outlook-shape form.** Field form overlay, scope dropdown
  (mailbox / account only), two custom date fields, toggle, raw escape hatch,
  wired to #0086a's engine. **~M (3-4 days).** Depends on #0086a.
- **#0086c - Saved searches: DROPPED** (see Decisions above).

Do #0086a first; it is usable on its own via CLI and the current single-line
overlay (which just gets a richer grammar for free).

## Open questions for Sylvain - RESOLVED
See the Decisions (2026-08-11) section at the top. Q1 = option (b) only, Q2 =
custom dates only, Q3 = per-account only; #0086c saved searches dropped.
