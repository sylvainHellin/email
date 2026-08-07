---
id: 0062
title: CLI read surface over the store (mp show and mp list-messages)
type: feature
priority: later
status: open
created: 2026-08-06
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
