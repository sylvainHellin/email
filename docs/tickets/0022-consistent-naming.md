---
id: 0022
title: consistent naming
type: refactor
priority: now
status: done
created: 2026-05-02
---

Ensure the naming is consistent throughout: `mailypoppins`
at the moment, the config is still under `email` and the cli is also called like this. for the CLI, check if `mp` is available

## Progress

The CLI binary was renamed `email` -> `mp` (`[[bin]]` in `Cargo.toml`; `cargo install --path .` now installs `mp`).
All user-facing command references (`--help`, README, `docs/*.md`, website) were updated.
The displayed name/version string stays `mailypoppins X.Y.Z` (clap `#[command(name = "mailypoppins")]`).

Still under `email` at that point: the Cargo package + library name and the config directory `~/.config/email/`, both data-location or internal-identity and invisible in the binary name.
Renaming the config dir was judged to need a migration path, against the "no migrations until v1.0" policy, and was deferred.
The library rename was judged a large `use email::` churn with no user-visible payoff and left as-is.

## The un-defer (2026-08-07)

The deferral above is reversed.
The ticket sat in the Now tier with nothing left in it but the two items it had declared out of scope, so finishing it meant deciding to un-defer, not executing a plan.
Decision taken with the supervisor before any code was written; the four items below, and the reasoning under each, are the record of it.

Adapting the original plan to the tree it landed in: the ticket predates the whole data-layer rewrite, so its "config directory" framing is now only half the story.
App-managed data (stores, blobs, drafts, tokens, logs) already moved to `mailypoppins_data_dir()` and was already spelled `mailypoppins`; what was left under `email` was the user-owned config directory alone.
Two further `email` spellings the ticket never named were found in the sweep and are folded in: the keyring service constant and the `sent_via` value written into a draft on send.

### 1. Cargo package + library: `email` -> `mailypoppins`

Shipped. `[package] name`, `[lib] name`, and 263 `email::` path references across `src/main.rs` and eight files in `tests/`.
Compiler-verified and invisible to users.
One consequence worth knowing: `insta` keys snapshot filenames on the crate name, so all 11 `.snap` files were renamed `email__*` -> `mailypoppins__*`.
They were `git mv`d and their contents verified byte-identical rather than re-accepted, because a golden-frame diff is a decision and this rename produced none.
The other consequence, since `cargo install` tracks installs by package name: the first `cargo install --path .` after this needs `--force` once.

Two user-facing banners were found in the same sweep and renamed with it: `=== Email CLI Setup ===` and `=== Email CLI Configuration ===` are now `=== mailypoppins setup ===` and `=== mailypoppins configuration ===`.

### 2. Config directory: `~/.config/email/` -> `~/.config/mailypoppins/`

Shipped, with a one-time automatic move at startup (`config::migrate_legacy_config_dir`, called from `main()` before anything reads config or secrets).

On the invariant the original deferral cited: "no migration paths until v1.0" is scoped to data formats, secret storage and wire protocols.
This is a location change and not one byte inside the directory is read or rewritten, so the invariant does not govern it.
A hard cut would have cost the user every stored SMTP/IMAP password and OAuth2 client id, which is a real price for a cosmetic rename.

The move's guarantees:

- `fs::rename` and nothing else, no copy fallback. Both paths are under `~/.config`, so they are on one filesystem in practice.
- Idempotent and safe under two concurrent `mp` invocations. The second process either sees the new directory present and does nothing, or loses the rename race and gets `ENOENT`; old-absent plus new-present is success in both cases.
- An existing `~/.config/mailypoppins/` is never overwritten or merged into.
- No read fallback anywhere. Nothing outside the migration function ever looks at `~/.config/email`. A rename that fails names both paths and the exact `mv` to run and exits 1, because a client that quietly kept reading the old location would never finish the move.
- Skipped entirely when `MAILYPOPPINS_CONFIG_DIR` is set: an explicit override must not carry a migration side effect.

The new `MAILYPOPPINS_CONFIG_DIR` env var mirrors `MAILYPOPPINS_DATA_DIR` and replaces the two test hardcodes of `$HOME/.config/email` (`tests/dump_mailbox_integration.rs`, `tests/cli_selector_contract.rs`).

The gap the live config exposed, decided mid-implementation: `fs::rename` moves the file and not the strings inside it, and a `config.toml` may reference its own directory.
The real one did, at `[accounts.signatures.robin] path = "~/.config/email/signatures/robin.html"`.
Afterwards that resolves to nothing, and `load_signature` answers a missing signature file with one stderr line and an unsigned message, which from the TUI is silent.
The answer is `warn_about_self_references`: right after a successful move it scans the config for values whose *directory prefix* is the old dir, in both the tilde and expanded-home spellings, and names each key, its old value and the exact replacement.
It warns and rewrites nothing, warn-only and never a non-zero exit.
Rewriting `config.toml` would turn a location change into a content migration of a user-edited file, which is the one thing this ticket must not smuggle in; and failing startup would block all of `mp` over a degradation confined to signatures.
Warned once, at move time; the steady-state signal stays `load_signature`'s own message.

### 3. Keyring service: `email-cli` -> `mailypoppins`

Shipped with a read fallback. `get` prefers `mailypoppins` and retries under `email-cli`; `set` and `delete` touch the new service only, so the next `mp config set-password` migrates the credential and leaves a harmless stale entry behind.
Without the fallback this would orphan credentials for anyone on the opt-in keyring backend, which was the only genuinely dangerous direction in the ticket.
The lookup order is parameterised over the lookup closure so both orders are testable without a keyring daemon.
Documented in [secrets.md](../secrets.md).

### 4. `sent_via: "email-cli vX.Y.Z"` -> `"mailypoppins vX.Y.Z"`

Shipped. Written into a draft's frontmatter on send.
Checked first that nothing parses it back: `EmailFrontmatter::sent_via` is an `Option<String>` that is only ever written, and there is no `X-Mailer` or `User-Agent` header anywhere in the crate, so no reader had to learn both spellings.

### Live validation

The move ran on this machine's real `~/.config/email` (assistant + tum), with the directory backed up to `/var/tmp/mp-0022-config-backup` first.
The self-reference warning fired on the robin signature path as designed, and that one line in `config.toml` was then edited by hand to `~/.config/mailypoppins/signatures/robin.html`.

### What is deliberately not renamed

Nothing. After this the string `email` survives in the tree only as the English word (email addresses, `EmailDraft`, `default_from`) and in the keyring fallback constant, which exists to be read and not written.
