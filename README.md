# mailypoppins

A CLI tool and TUI for managing emails from Markdown drafts with YAML frontmatter. Supports IMAP fetch/sync/archive and an auditable draft workflow (`draft -> approved -> sent`). Running `mp` with no arguments launches an interactive terminal UI.

## Installation

```bash
# Build and install to ~/.cargo/bin/
cargo install --path .
```

## Setup

Run the interactive setup wizard:

```bash
mp config init
```

This will:
1. Prompt for SMTP and IMAP credentials
2. Test the connections
3. Discover server mailboxes and assign roles (inbox, archive, sent)
4. Write the config to `~/.config/email/config.toml`
5. Store passwords securely in the OS keyring (macOS Keychain, Windows Credential Manager, or Linux Secret Service)

### Configuration file

All configuration lives in `~/.config/email/config.toml`:

```toml
[email]
font_family = "Helvetica, Arial, sans-serif"
font_size = "12pt"
include_signature = true

[smtp]
host = "postout.lrz.de"
port = 587
username = "your-id"
default_from = "your.name@example.com"

[imap]
host = "imap.example.com"    # Falls back to smtp.host if omitted
port = 993
username = ""                # Falls back to smtp.username if omitted

[mailboxes.inbox]
server = "INBOX"             # IMAP folder name; the local directory is
                             # always <data_dir>/accounts/<name>/inbox/

[mailboxes.archive]
server = "Archive"

[mailboxes.sent]
server = "Sent Items"

[[mailboxes.extra]]          # Additional mailboxes to sync
server = "Projects"

[signatures]
default = "work"

[signatures.work]
name = "Work Signature"
path = "~/notes/email/signatures/work.html"

[signatures.personal]
name = "Personal"
path = "~/notes/email/signatures/personal.html"
```

Passwords are **never** stored in the config file. They live in the OS keyring under keys `smtp-password` and `imap-password`. IMAP password falls back to SMTP password if not set separately.

### Where mailypoppins stores data

All mail, drafts, contacts cache, OAuth2 tokens, and logs live under a single OS-conventional app data directory:

| OS                  | Path                                                  |
|---------------------|-------------------------------------------------------|
| macOS               | `~/Library/Application Support/mailypoppins/`         |
| Linux (incl. WSL)   | `$XDG_DATA_HOME/mailypoppins/` (def. `~/.local/share/mailypoppins/`) |

Layout:

```
mailypoppins/
  accounts/<name>/{inbox,archive,sent,drafts,<extra>}/    # mail tree
  accounts/<name>/contacts-cache.json                     # contacts index
  tokens/<name>.enc                                       # OAuth2 tokens
  logs/mailypoppins-YYYY-MM-DD.log
```

The location is not user-configurable per account. To make the mail tree visible inside an Obsidian vault (or any other directory), symlink `accounts/<name>/` into the desired location. Override the root for tests / portable installs with `MAILYPOPPINS_DATA_DIR=/some/path`.

### OAuth2 / Exchange Online

For Microsoft 365 accounts using OAuth2 (IMAP/SMTP) or Graph API:

1. Register an app in Azure Entra ID with delegated permissions (`Mail.Read`, `Mail.ReadWrite`, `Mail.Send`).
2. Add the account to `config.toml` with `auth_method = "oauth2"` (IMAP/SMTP) or `auth_method = "graph"` (Graph API, for tenants that block IMAP/SMTP).
3. Run the device code flow:

```bash
mp config oauth2-login --account <name>
```

Graph accounts require an `[accounts.oauth2]` section with `client_id` and `tenant_id`, and use Graph well-known folder names (`inbox`, `archive`, `sentitems`) instead of IMAP folder names.

### Config commands

```bash
mp config init           # Interactive setup wizard
mp config show           # Display config (passwords masked)
mp config set-password   # Store SMTP or IMAP password in keyring
mp config oauth2-login   # Run OAuth2 device code flow
mp config path           # Print config file path
```

## Email Draft Format

Files use Markdown with YAML frontmatter:

```markdown
---
to: recipient@example.com
cc: optional@example.com       # Optional
bcc: hidden@example.com        # Optional
subject: "Your subject line"
status: draft                  # draft | approved | sent
from: alternate@example.com   # Optional, overrides default_from
reply_to: reply@example.com   # Optional
attachments:                   # Optional
  - /path/to/file.pdf
---

Email body in **Markdown** format.
```

### Status workflow

```
draft  --[mark-approved]-->  approved  --[send]-->  sent
```

Only approved emails can be sent. The `send` command rejects drafts that haven't been approved.

Inbox emails follow a separate flow: `inbox -> archived` (via the `archive` command).

### After sending

The file is updated in-place with `status: sent`, `sent_at` timestamp, `sent_via` version, and `message_id`.
A send that reached every recipient and was recorded in the outbox then deletes the draft file: the copy that matters lives on the server, where the outbox appends it to Sent and sync reads it back into the local store.
A partial send, or one the outbox store could not record, keeps the file with `status: sent`, because it is the only local copy left.
Sending such a draft again means editing `status:` back to `approved` by hand, since `send` refuses anything that is not approved and `mark-approved` refuses anything already sent; trim the recipient lines first, because a re-send delivers to everyone the file still lists.

## Commands

### TUI

```bash
mp                              # Launch the interactive TUI
```

### Global options

```bash
-s, --signature <name>   Use a specific signature
    --no-signature       Skip signature entirely
```

### Drafts

```bash
mp <file>                       # Preview a draft (dry-run)
mp new <name>                   # Create a new draft from template
mp list [dir]                   # List drafts grouped by status
mp validate <file|dir>          # Validate frontmatter
mp mark-approved <file>         # Mark draft as approved
mp send <file> [-y]             # Send a single approved email
mp send-approved [dir] [-y]     # Send all approved emails in directory
mp reply [file] [--all]         # Create a reply draft from a received email
mp forward [file]               # Create a forward draft from a received email
```

### IMAP

```bash
mp fetch [filters]              # Fetch emails from server
mp sync [options]               # Sync local folders with server
mp watch [options]              # Watch mailbox for changes (IMAP IDLE)
mp list-mailboxes               # List available server mailboxes
mp archive <file>               # Archive an inbox email (server + local)
mp delete <file>                # Delete an inbox email (server + local)
mp search <query>               # Search emails on the server
mp open <file>                  # Open an attachment in the default app
mp save <file> [--output]       # Save attachment(s) to a directory
```

### Fetch options

| Option | Description |
|--------|-------------|
| `--from <addr>` | Filter by sender |
| `--to <addr>` | Filter by recipient |
| `--cc <addr>` | Filter by CC |
| `--subject <text>` | Subject contains |
| `--body <text>` | Body contains |
| `--since <YYYY-MM-DD>` | Emails since date |
| `--before <YYYY-MM-DD>` | Emails before date |
| `-n, --limit <N>` | Max results (default: 10) |
| `--full` | Show full body instead of preview |
| `--mailbox <name>` | Mailbox name (default: INBOX) |

### Sync options

| Option | Description |
|--------|-------------|
| `-n, --limit <N>` | Max messages per mailbox (default: 50) |
| `--mailbox <name>...` | Mailboxes to sync (default: all configured) |
| `--reconcile` | Also detect server-side moves and deletes |

Sync is additive by default (new emails only, deduped by Message-ID). With `--reconcile`, it also detects emails moved between INBOX and Archive on the server, and removes locally deleted emails.

### Watch options

| Option | Description |
|--------|-------------|
| `--mailbox <name>` | Mailbox to watch (default: INBOX) |
| `--timeout <seconds>` | Timeout in seconds |

Exit codes: `0` = mailbox changed, `1` = error, `2` = timed out.

## Signatures

Signature files are HTML snippets appended after the email body:

```html
<div style="font-family: Helvetica, Arial, sans-serif; font-size: 12pt;">
<p style="margin: 0;">
<b>Your Name</b><br>
Title | Organization
</p>
</div>
```

Reply drafts include a `{{SIGNATURE}}` placeholder between the reply area and quoted text. When sent, the placeholder is replaced with the signature HTML.

## TUI

Running `mp` with no arguments opens a full-screen terminal interface (built on `ratatui`).

**Layout:** sidebar (mailbox list) | email list | headers + body preview. Adapts to terminal width.

**Key bindings (list view):**

| Key | Action |
|-----|--------|
| `j`/`k`, arrows | Navigate |
| `Enter`/`e` | Open in `$EDITOR` |
| `r`/`R` | Reply / Reply all |
| `a`/`d` | Archive / Delete (with confirmation) |
| `A` | Mark as approved |
| `x`/`X` | Send / Send all approved |
| `n` | New draft |
| `f`/`F`/`S` | Fetch / Sync / Reconcile |
| `/` or `\` | Search (header-only or full-body) |
| `1`-`9` | Jump to mailbox |
| `?` | Help |
| `q` | Quit |

The TUI calls library functions directly (no subprocess spawning). Background IMAP IDLE watches for new mail and triggers automatic fetches.

## Example workflow

```bash
# 1. Set up (once)
mp config init

# 2. Launch the TUI for interactive management
mp

# Or use CLI commands directly:

# 3. Sync inbox
mp sync

# 4. Create and review drafts
mp new meeting-followup
mp list ~/notes/email/drafts/
mp ~/notes/email/drafts/2026-03-01_meeting-followup.md   # preview

# 5. Approve and send
mp mark-approved ~/notes/email/drafts/2026-03-01_meeting-followup.md
mp send ~/notes/email/drafts/2026-03-01_meeting-followup.md

# 6. Or batch send all approved
mp send-approved ~/notes/email/drafts/
```

## Troubleshooting

### "Config file not found"
Run `mp config init` to create the config file.

### "Password not found in keyring"
Run `mp config set-password` to store your password.

### "Email not approved for sending"
Run `mp mark-approved <file>` first.

### "SMTP authentication failed"
Check your credentials with `mp config show` and re-run `mp config set-password smtp` if needed.

### "Signature file not found"
Check the path in your config. Paths support `~` expansion.

## Building

```bash
cargo build --release           # Build
cargo install --path .          # Install to ~/.cargo/bin/
```
