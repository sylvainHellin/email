# Microsoft 365 / Exchange Online Setup (OAuth2)

This guide walks through setting up the email CLI with a Microsoft 365 (Exchange Online) account using OAuth2 authentication.

## Prerequisites

- A Microsoft 365 account (e.g., user@example.com)
- Access to the Azure portal (https://portal.azure.com) for app registration
- The email CLI installed

## Step 1: Register an Azure Entra ID Application

1. Go to https://portal.azure.com
2. Navigate to "Microsoft Entra ID" (formerly Azure Active Directory)
3. Select "App registrations" in the left sidebar
4. Click "New registration"
5. Fill in:
   - Name: `mailypoppins` (or any descriptive name; this is the label in your tenant, nothing reads it back)
   - Supported account types: "Accounts in this organizational directory only"
   - Redirect URI: leave blank (not needed for device code flow)
6. Click "Register"

## Step 2: Enable Public Client Flows

1. In the app registration, go to "Authentication" in the left sidebar
2. Scroll down to "Advanced settings"
3. Set "Allow public client flows" to **Yes**
4. Click "Save"

This is required for the device code flow used by the CLI.

## Step 3: Add API Permissions

1. Go to "API permissions" in the left sidebar
2. Click "Add a permission"
3. Select "APIs my organization uses"
4. Search for and select "Office 365 Exchange Online"
5. Select "Delegated permissions"
6. Add these permissions:
   - `IMAP.AccessAsUser.All` -- required for IMAP access
   - `SMTP.Send` -- required for sending via SMTP
7. Click "Add permissions"
8. Also add from "Microsoft Graph":
   - `offline_access` -- required for refresh tokens

If you see "Needs admin approval" next to any permission, you need your IT admin to grant consent. Click "Grant admin consent for [org]" if you have admin rights, or request IT to do so.

## Step 4: Note the IDs

From the app registration "Overview" page, copy:
- **Application (client) ID**: e.g., `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`
- **Directory (tenant) ID**: e.g., `yyyyyyyy-yyyy-yyyy-yyyy-yyyyyyyyyyyy`

## Step 5: Configure mailypoppins

### Option A: Interactive Setup

Run:
```
mp config init
```
Select "3. Microsoft 365 / Exchange Online (OAuth2)" and follow the prompts. You will be asked for the client_id and tenant_id, then the device code flow will start.

### Option B: Manual Configuration

Add to `~/.config/mailypoppins/config.toml`:

```toml
[[accounts]]
name = "work"
default_from = "user@example.com"
auth_method = "oauth2"

[accounts.oauth2]
client_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
tenant_id = "yyyyyyyy-yyyy-yyyy-yyyy-yyyyyyyyyyyy"

[accounts.smtp]
host = "smtp.office365.com"
port = 587
username = "user@example.com"

[accounts.imap]
host = "outlook.office365.com"
port = 993
username = "user@example.com"

[accounts.directories]
root = "~/notes/email/work"
drafts = "drafts"

[accounts.mailboxes.inbox]
server = "INBOX"
local = "inbox"

[accounts.mailboxes.archive]
server = "Archive"
local = "archive"

[accounts.mailboxes.sent]
server = "Sent Items"
local = "sent-items"
```

Then run the device code flow to acquire a token:
```
mp config oauth2-login --account work
```

## Step 6: Authenticate

When you run `mp config oauth2-login`, the CLI will:
1. Display a URL and a code
2. Open the URL in your browser (or display it for you to open manually)
3. Enter the code and sign in with your Microsoft account
4. Grant the requested permissions

The token is cached encrypted at `<mailypoppins_data_dir>/tokens/<account>.enc` (e.g. `~/Library/Application Support/mailypoppins/tokens/` on macOS) and is automatically refreshed when it expires (access tokens last ~1 hour, refresh tokens ~90 days).

## Troubleshooting

### "IMAP is disabled for your organization"
Your tenant may have IMAP disabled at the organization level. Contact IT to enable IMAP for your account, or check Exchange Admin Center > Mailboxes > [your mailbox] > Manage email apps.

### "Needs admin consent"
The `IMAP.AccessAsUser.All` permission often requires admin consent. Request your IT administrator to grant consent via the Azure portal (Entra ID > App registrations > [your app] > API permissions > Grant admin consent).

### Token expired
If you see "OAuth2 token expired" errors, run:
```
mp config oauth2-login --account work
```
to re-authenticate. Refresh tokens expire after ~90 days of inactivity.

### "AUTHENTICATE failed"
This can mean IMAP is disabled, the token has wrong scopes, or admin consent is missing. Check the app permissions and ensure IMAP is enabled for your account.

## Exchange Online Server Details

| Protocol | Host | Port |
|----------|------|------|
| IMAP (TLS) | outlook.office365.com | 993 |
| SMTP (STARTTLS) | smtp.office365.com | 587 |
