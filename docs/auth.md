# Authentication methods

Each account in `~/.config/mailypoppins/config.toml` declares an `auth_method`. Three are supported.

| `auth_method` | Transport | Use case |
|--------------|-----------|----------|
| `password` (default) | IMAP + SMTP with username/password | Most providers (Proton Bridge, Fastmail, generic IMAP) |
| `oauth2` | IMAP + SMTP using XOAUTH2 SASL | Microsoft 365 tenants that allow IMAP/SMTP |
| `graph` | Microsoft Graph REST API | Microsoft 365 tenants that block IMAP/SMTP at the org level |

Auth method is set per account; one config can mix all three.

## Password

- Passwords stored in the secrets backend under `smtp-password-<account>` and `imap-password-<account>`.
- IMAP falls back to the SMTP password if `imap-password-<account>` is missing.
- Default backend: machine-bound encrypted file at `~/.config/mailypoppins/secrets.enc`. Opt into the OS keyring with `secrets_backend = "keyring"` in `config.toml`. See [secrets.md](secrets.md).
- Set or rotate via `mp config set-password {smtp|imap} --account <name>`.

## OAuth2 (IMAP/SMTP, XOAUTH2)

- Used for Microsoft 365 / Exchange Online when the tenant still permits IMAP and SMTP. Requires an Azure Entra ID app registration.
- Config block:

  ```toml
  [[accounts]]
  name = "work"
  auth_method = "oauth2"
  default_from = "you@tenant.onmicrosoft.com"

  [accounts.oauth2]
  client_id = "..."
  tenant_id = "..."   # or "common", "consumers", "organizations"
  ```

- Token cache: `<mailypoppins_data_dir>/tokens/<account>.enc` (encrypted with the same `encrypt_blob` / `decrypt_blob` as `secrets.enc`).
- Scopes: `IMAP_SMTP_SCOPES` constant in `oauth2.rs` (`https://outlook.office365.com/IMAP.AccessAsUser.All`, `SMTP.Send`, `offline_access`).
- IMAP authenticates via `async_imap::Authenticator` (XOAUTH2 SASL string built in `oauth2.rs`).
- SMTP authenticates via `lettre::Mechanism::Xoauth2`.
- Token refresh is automatic on every IMAP/SMTP call. The refresh token expires after roughly 90 days of inactivity; when that happens, re-run `mp config oauth2-login --account <name>`.

## Graph (Microsoft Graph REST)

- Used when IMAP/SMTP are blocked organisation-wide. Requires the same Entra ID app registration as OAuth2 but with Graph scopes.
- Config block:

  ```toml
  [[accounts]]
  name = "tenant-only"
  auth_method = "graph"
  default_from = "you@tenant.onmicrosoft.com"

  [accounts.oauth2]
  client_id = "..."
  tenant_id = "..."
  ```

- Scopes: `GRAPH_SCOPES` constant in `oauth2.rs` (`Mail.ReadWrite`, `Mail.Send`, `User.Read`, `offline_access`).
- `SmtpConfig::load()` and `ImapConfig::load()` return errors for Graph accounts. Use `GraphConfig::load()` instead.
- All mail operations route through `src/graph.rs`: `list_folders`, `fetch_messages`, `fetch_new_messages`, `sync_mailboxes_graph`, `send_mail`, `archive_email_graph`, `delete_email_graph`, `mark_read_graph`, `search_messages`.
- TUI actions branch on `app.is_graph()` in `tui/actions.rs::handle_action()` and dispatch to Graph helpers (`graph::archive_email_graph`, etc.) instead of IMAP/SMTP.
- No IMAP IDLE -- the watcher is a 60-second timer that polls inbox message count and emits `WatchEvent::Changed` on delta.
- `mp config oauth2-login --account <name>` tests Graph accounts via the `/me` endpoint. `mp config show` displays "graph (Microsoft Graph API)" and hides SMTP/IMAP details.

## Setup wizard

`mp config init` (and `mp config add-account`) presents four provider options:

1. Generic IMAP/SMTP (password)
2. Proton Mail (Bridge, password, self-signed cert)
3. Microsoft 365 / Exchange Online (OAuth2, IMAP+SMTP)
4. Microsoft 365 / Exchange Online (Graph API, no IMAP/SMTP)

For OAuth2 / Graph, the wizard runs the device-code flow, tests the credential, discovers folders, and lets you assign mailbox roles. See [exchange-setup.md](exchange-setup.md) for the Entra ID app-registration walkthrough.

## Recovery

- Forgotten password: `mp config set-password {smtp|imap} --account <name>`.
- Expired OAuth2 / Graph refresh token: `mp config oauth2-login --account <name>`.
- Lost / corrupted secrets file (e.g. Time Machine restore on a new Mac): `mp config reset-secrets`. This wipes `secrets.enc` and `tokens/`, then walks each account prompting for re-entry.
