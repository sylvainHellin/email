# Secrets storage

mailypoppins stores SMTP/IMAP passwords and OAuth2 token caches encrypted at
rest with a key derived from the host machine ID and the current user's UID.
There is no prompt at runtime, no GPG dependency, and no Keychain ACL to
break across rebuilds.

## Threat model

What this protects against:

- **Backup leakage.** A Time Machine snapshot, an iCloud Drive sync, a
  Dropbox folder, an `rsync` to a NAS, or an accidental `git add ~/.config`
  cannot be decrypted on any machine other than the one that wrote it. The
  ciphertext is a sealed bag of bytes anywhere else.
- **Cross-machine `cp -a`.** Same property: copying the secrets file to
  another machine yields a file that no longer decrypts.
- **Casual local snooping.** `cat secrets.enc` shows ciphertext, not a
  TOML map of plaintext passwords.

What this does **not** protect against:

- An attacker running as the same user on the same machine. Such an
  attacker can attach a debugger to the running TUI process and dump the
  in-memory passwords, drive `Mail.app` via AppleScript, read
  `~/Library/Mail/`, exfiltrate browser cookies, etc. There is no defense
  against same-user execution short of biometric-gated Secure Enclave keys
  (which we deliberately reject for UX reasons).
- A malicious wholesale clone of the disk image including the TPM /
  IOPlatformUUID hardware (e.g. by an evil-maid attack with physical
  access). This is well outside the threat model for a personal CLI.

In short: the encrypted file is roughly `chmod 600 secrets.toml` against a
local attacker, and strictly stronger against any kind of off-machine
leakage. The UX and operational properties are dramatically better than
either Keychain (rebuild churn) or `pass` (GPG dependency).

## Key derivation

```
machine_id = <Linux: /etc/machine-id> | <macOS: IOPlatformUUID via ioreg>
ikm        = machine_id || u32_le(getuid()) || b"mailypoppins-v1"
key        = HKDF-SHA256(ikm, salt = b"mailypoppins-secrets", info = b"")[..32]
```

The `machine-uid` Rust crate handles the platform-specific machine-ID
read. WSL behaves like Linux (uses `/etc/machine-id`).

## File layout

```
offset  size  contents
------  ----  --------
0       5     magic    = b"MPSEC"
5       1     version  = 0x01
6       12    nonce    (CSPRNG-generated per write)
18      ..    ciphertext_with_tag (ChaCha20-Poly1305)
```

The plaintext is a TOML document of the form:

```toml
[entries]
"smtp-password-tum"      = "..."
"imap-password-tum"      = "..."
"smtp-password-personal" = "..."
```

The same crypto helpers (`secrets::encrypt_blob` / `secrets::decrypt_blob`)
are reused by `oauth2.rs` to encrypt OAuth2 token caches at
`<mailypoppins_data_dir>/tokens/<account>.enc` (raw blob format -- the plaintext is
JSON, not TOML, but the wrapping is identical).

## Recovery

If the secrets file becomes undecryptable (you restored from a backup onto
a new machine, the kernel handed out a new machine-id, or the file got
corrupted), every command that needs a password will fail with:

```
Cannot decrypt secrets store on this machine. Run `mp config reset-secrets` to wipe and re-enter passwords.
```

`mp config reset-secrets`:

1. Confirms the destructive operation interactively.
2. Removes `~/.config/mailypoppins/secrets.enc`.
3. Removes all `*.enc` files under `<mailypoppins_data_dir>/tokens/`.
4. Walks each configured `[[accounts]]` and re-prompts for SMTP / IMAP
   passwords. For OAuth2 / Graph accounts, prints a hint to re-run
   `mp config oauth2-login --account <name>`.

## Opting into the OS keyring

Power users on a release-signed build (Apple Developer ID, Linux Secret
Service, etc.) can switch to the OS keyring by adding to
`~/.config/mailypoppins/config.toml`:

```toml
secrets_backend = "keyring"
```

This swaps the storage at startup. There is no automatic migration: you
will need to re-set passwords via `mp config set-password <which>
--account <name>` after flipping the switch.

The keyring service name is `mailypoppins`.
It was `email-cli` before #0022, so a lookup that misses under `mailypoppins` retries under `email-cli` and nobody who opted in before the rename loses access to a stored password.
Writes and deletes go to `mailypoppins` only, so the next `mp config set-password` moves the credential across and leaves a stale, harmless `email-cli` entry behind.
Clear those by hand if you want them gone (Keychain Access on macOS, `secret-tool` / Seahorse on Linux).

On macOS, the keyring backend will re-prompt for every keychain item every
time the binary is rebuilt with a different code-signing identity (which
`cargo install` produces by default). It is a fine choice for end users
installing a stably-signed Homebrew artifact, and a poor choice for active
local development. The encrypted file backend is the right default until
release signing is set up.
