---
id: 0090
title: Undo-send hold window before SMTP hand-off
type: feature
priority: later
status: open
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
