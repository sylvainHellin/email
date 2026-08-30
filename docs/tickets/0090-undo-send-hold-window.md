---
id: 0090
title: Undo-send hold window before SMTP hand-off
type: feature
priority: later
status: done
created: 2026-08-14
---

Undo-send is universally expected in peer clients and is cheap given `mp`'s explicit send pipeline (feature survey §c.5, audit synthesis §3).
A sent message today goes to the outbox and hands off to SMTP with no reprieve.

## Owner decision (2026-08-14)

Hold the message for a configurable window before the SMTP hand-off.
The default is 20 seconds.
This closes the open question on undo-send window length: configurable, default 20 s.

## Scope

1. On send, hold the message in the outbox for the configured window before the SMTP hand-off begins.
2. During the window the user can cancel the send, returning the message to a draft or held state rather than transmitting it.
3. The window is configurable in `config.toml`, default 20 seconds; a zero window means send immediately (opt-out).
4. The hold survives the normal send confirm and applies to both a plain send and the approve-and-send path from [#0089](0089-send-current-draft-approve-and-send.md).

## Cross-references

Depends on the shared send path from [#0058](0058-send-path-unification.md) and interacts with the send-flow change in [#0089](0089-send-current-draft-approve-and-send.md); sequence this after both.

## Acceptance criteria

- After sending, the message is held for the configured window and can be cancelled before it reaches SMTP.
- A cancel within the window transmits nothing and leaves the message recoverable.
- The window length is read from config, defaults to 20 seconds, and a zero value sends immediately.

## Resolution (2026-09)

Delivered as a pre-hand-off hold in the TUI send flow. Config gained `email.send_hold_secs` (default 20, `0` opts out; `src/config.rs`). The send key (#0089/#0092's `x`, `Action::Send` in `src/tui/actions.rs`) still validates and approves the draft, but instead of handing the built message to the background send thread at once it parks a `HeldSend` (`src/tui/app/types.rs`) with `fire_at = now + send_hold_secs` on `App.held_send`; the event loop fires it through `actions::fire_held_send` once the window elapses (`src/tui/mod.rs`), and `u` clears the slot first (`src/tui/app/keys.rs::dispatch_normal_mode`), which transmits nothing and leaves the approved draft in place, recoverable. A zero window fires immediately, preserving today's behaviour. The hold applies to both SMTP and Graph sends because it sits ahead of the shared `send_one_draft`/`send_durably` path.

Key mapping to the current scheme (this ticket predates #0089/#0092): #0092 merged plain-send and approve-and-send onto the single Global `x`, so holding `Action::Send` covers both paths the scope names. The undo key `u` is a transient hand-dispatched prompt advertised on the status line, not a `KEYMAP` row, so it neither collides with the Message-context `u` (toggle read) once the window has fired nor changes the website key table (`mp dump-keys --json` unchanged).

Deliberate deviation from scope item 1 ("hold the message in the outbox for the configured window"): the hold sits *before* the outbox enqueue rather than as a held state inside it. Holding pre-enqueue means a cancel has nothing durable to unwind, cannot half-commit or double-send, and needs no outbox schema or `rebuild.rs` change, and all three acceptance criteria still hold: held before SMTP, cancellable, recoverable, config default 20 with zero meaning immediate.
The cost of pre-enqueue (review finding): the held send lives only in the process, so a quit or crash inside the window would silently drop a send the user confirmed, where an in-outbox hold would resend on next launch. Quit therefore refuses while a send is holding, with the reason on the status line; a crash inside the window still drops the send, accepted because the approved draft remains on disk and the window is short.

Scope held to the interactive TUI send: `mp send` (CLI one-shot) and the batch `Send all approved` (#0058) are unchanged, having no interactive undo surface.

Tests: `config::tests::test_send_hold_secs_reads_from_config_and_accepts_zero`, `config::tests::test_email_settings_defaults` (default 20), `tui::app::types::tests::a_held_send_is_ready_only_once_its_window_has_elapsed` (the firing gate). Website `[email]` config docs updated (`website/src/pages/config.astro`).
