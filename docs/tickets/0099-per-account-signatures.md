---
id: 0099
title: Per-account signatures appended on compose and reply
type: feature
priority: later
status: open
created: 2026-08-14
---

Per-account signatures are baseline in modern clients and a natural fit for the Markdown + per-account-config model (feature survey §b "Signatures per account", §c.8, §(c) shortlist item 8).
`mp` runs two accounts (TUM and personal Proton) with separate mailbox sets and no unified inbox (UX audit §b.7), so the correct signature is account-dependent and today there is none: the user retypes or pastes it into every draft body.

## Scope

Let each account define a signature (a Markdown snippet in per-account config, or a template file path) that is appended to the draft body on compose, reply, and forward.
Appending happens when the draft body is created (before or as `$EDITOR` opens, and before the inline body field of [#0097](0097-compose-wizard-body-field.md) if that has shipped), so the signature is visible and editable in the draft rather than injected invisibly at send time.
An account with no configured signature behaves exactly as today (no signature block).

Open sub-questions for the design, not decided here:

- Reply/forward signature placement relative to the quoted text (above or below the quote).
- Whether a per-account default can be overridden per draft.

## Cross-references

- The feature survey groups signatures with templates/snippets (§(c) item 8) as "trivial with Markdown files"; a snippets/templates feature is the larger sibling and could share the template-file plumbing, but is out of scope here.
- [#0097](0097-compose-wizard-body-field.md) (inline body field) and this ticket both write into the draft body at creation time and should agree on ordering once both exist.

## Acceptance criteria

- Each account can configure a signature; composing, replying, or forwarding from that account appends it to the draft body.
- The signature is present in the draft the user edits, not injected only at send time.
- An account with no configured signature produces no signature block.
