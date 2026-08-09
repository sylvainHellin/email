---
id: 0062
title: CLI read surface over the store (mp show and mp list-messages)
type: feature
priority: later
status: done
created: 2026-08-06
closed: 2026-08-11
---

From the architecture review synthesis, Tier 3: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: S each, the queries already exist.

After `mp sync`, mail is reachable only through the TUI or through `dump-mailbox`, which exists as a parity oracle and emits newline-delimited JSON.
The data-access-layer plan promises CLI reads; they were never built.

## Evidence

- `src/main.rs:322` `DumpMailbox` requires `--json` and is documented as the parity oracle, not as a read surface.
- Every query needed is already in `src/store/read.rs`: `list_mailbox` at `:136`, `list_account` at `:156`, `find_by_message_id` at `:200`, `find_by_id` at `:221`, `load_body` at `:428`, `load_html` at `:454`, `attachments_for` at `:247`.
- The selector contract that addresses a single message shipped with [#0050](0050-selector-contract-drafts-index.md), so `mp show <selector>` has a defined argument grammar already.

## Scope

1. `mp show <selector>`: resolve the selector, print headers, body and the attachment list for one message.
   Human-readable by default, `--json` available.
2. `mp list-messages`: list one mailbox or a whole account, honouring the existing `-A/--account` and `--mailbox` flags, with a `--limit`.
3. Reuse `store::read` unchanged; this ticket adds no query.
4. Document both in `mp --help` and on the website command pages, per the repo rule that those pages are derived from the CLI help.
5. `store::read::render_markdown` ([#0075](0075-open-received-mail-in-editor.md)) is the natural body of `mp show`, but it appends the message body verbatim after the closing `---` fence, so a body whose first line is `---` reads back ambiguously; fence or escape the body before anything downstream parses that output.

## Acceptance criteria

- `mp show` on a selector that the TUI can open prints the same body the TUI shows.
- `mp list-messages -A <account> --mailbox inbox` matches the TUI listing order and count.
- An evicted or missing body degrades to a clear message rather than an error backtrace.
- Website command pages updated in the same commit as the CLI help.

## Resolution (2026-08-11)

Both commands shipped in `src/read_cmd.rs`, wired in `src/main.rs`, with no new query: `list_mailbox`, `find_by_id` (through `selector::resolve_received`), `attachments_for` and `load_body` are used exactly as they were.

- `mp show <selector> [--mailbox M] [--json]` resolves through `selector::parse_in` + `resolve_received`, the grammar every other received command takes, and calls `account_for_selector` first so `mp://other/inbox/<id>` opens that account's store (the #0073 follow-up rule).
The human layout is the headers block `mp fetch --full` prints, plus the mailbox, the canonical selector, the flags and the attachment list, then a rule and the body.
- Scope item 5, answered by not creating the ambiguity: `mp show` does *not* render through `read::render_markdown`.
That rendition wraps the headers in a `---` YAML frontmatter and appends the body verbatim, so a body opening with `---` reads back as though the frontmatter continued; a human surface that is not meant to be parsed should not invent a document format it does not escape.
`--json` is the parseable answer, where the body is a JSON string and cannot be misread.
`render_markdown` itself is untouched: it is #0075's editor rendition and fixing its fencing is that ticket's call, not a change to make from here.
- `mp list-messages [--mailbox M] [-A account] [-n N]` lists one mailbox, or every mailbox of the account grouped in sidebar order with the drafts pseudo-mailbox skipped (`mp list` owns drafts).
The limit is per mailbox in the grouped mode, so a busy inbox cannot hide the others, and each group header reads `Inbox (2 of 128)` so what the limit cut is visible.
`--mailbox` matches a role id or a sidebar label case-insensitively, the same rule `mp dump-mailbox` applies, and an unknown name is an error naming the known ones rather than an empty listing.
- No `--json` on `mp list-messages`: `mp dump-mailbox --json` is already the machine-readable listing, and a second NDJSON surface would be a second contract to keep stable for no new information.
- An evicted or missing body prints a dimmed note under the headers and is `null` in JSON, distinct from a body that is genuinely empty.

Pinned by five unit tests in `src/read_cmd.rs` (body, the `---` body, the evicted degrade, the listing and its truncation, the empty listing) and eight binary-level tests in `tests/cli_read_surface_integration.rs` (both commands run as the real `mp` against a temp config and store: bare-key resolution, `--json`, the cross-account selector, the miss, single-mailbox order against `read::list_mailbox`, per-mailbox limits, the unknown mailbox, the empty mailbox).
`tests/snapshots/cli_help_snapshot__cli_help_surface_snapshot.snap` carries both `--help` texts, and `website/src/pages/commands.astro` gained a Local Store Commands section derived from them.
