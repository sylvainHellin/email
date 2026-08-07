# Contributing

This is a small personal project, but the build flow is documented here so
anyone (including future-me on a fresh machine) can reproduce it.

## Building from source

```
cargo install --path .
```

## Secrets

By default, mailypoppins stores SMTP/IMAP passwords and OAuth2 tokens in a
machine-bound encrypted file at `~/.config/mailypoppins/secrets.enc`. There is no
Keychain prompt during `cargo install` and no codesign setup required. See
[docs/secrets.md](docs/secrets.md) for the threat model, key derivation, and
recovery procedure (`mp config reset-secrets`).

The OS keyring backend (macOS Keychain, Linux Secret Service) is supported
as an opt-in via `secrets_backend = "keyring"` in `~/.config/mailypoppins/config.toml`.
On macOS, the keyring backend will re-prompt every time the binary is
rebuilt with a different code-signing identity (which `cargo install`
produces by default). For a smoother experience, stick with the default
`encrypted-file` backend until signed releases are available.

## Tests

```
cargo test
```

All tests run offline in under a second. See `AGENTS.md` for the testing
conventions.

## Troubleshooting

- **`Cannot decrypt secrets store on this machine`** -- the encrypted
  secrets file was created on a different host (machine ID changed) or by
  a different user. Run `mp config reset-secrets` to wipe the file and
  re-enter passwords.
- **`Secrets store not initialized`** -- you have not configured any
  passwords yet. Run `mp config init` (fresh install) or
  `mp config set-password <smtp|imap> --account <name>` to populate.
