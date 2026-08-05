use email::types::*;
use email::config::*;
use email::parse::*;
use email::imap_client::{self, *};
use email::draft::*;
use email::send::*;
use email::config_cmd::*;
use email::graph;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use colored::*;
use log::{error, info, warn};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Parser)]
#[command(name = "mailypoppins")]
#[command(about = "A CLI tool for sending emails from Markdown drafts with YAML frontmatter")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// File to preview (dry-run mode)
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Signature to use (overrides config default)
    #[arg(short, long, global = true)]
    signature: Option<String>,

    /// Skip signature entirely
    #[arg(long, global = true)]
    no_signature: bool,

    /// Account to use (default: first in config)
    #[arg(short = 'A', long, global = true)]
    account: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Send a single approved email, or (with --invite) a calendar invitation
    Send {
        /// Path to the email draft file (omit when using --invite)
        file: Option<PathBuf>,
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Send an iMIP calendar invitation (METHOD:REQUEST) instead of a draft.
        /// Attendees come from --to/--cc; the subject is used as the event
        /// summary. Requires --to, --start, --subject, and one of --end/--duration.
        #[arg(long)]
        invite: bool,
        /// Invite recipient(s), comma-separated (invite mode; ATTENDEE + To).
        #[arg(long)]
        to: Option<String>,
        /// Invite CC recipient(s), comma-separated (invite mode; ATTENDEE + Cc).
        #[arg(long)]
        cc: Option<String>,
        /// Event subject / summary (invite mode).
        #[arg(long)]
        subject: Option<String>,
        /// Event start. Local time (2026-07-20T14:00 or "2026-07-20 14:00") or
        /// RFC3339 with offset (2026-07-20T14:00:00+02:00, ...Z). Invite mode.
        #[arg(long)]
        start: Option<String>,
        /// Event end (same formats as --start). Provide this or --duration.
        #[arg(long)]
        end: Option<String>,
        /// Event duration instead of --end: ISO8601 (PT1H30M) or short (1h30m).
        #[arg(long)]
        duration: Option<String>,
        /// Optional event location (invite mode).
        #[arg(long)]
        location: Option<String>,
        /// Optional event description / body (invite mode).
        #[arg(long)]
        description: Option<String>,
    },
    /// Send all approved emails in a directory
    SendApproved {
        /// Directory containing email drafts
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// List emails by status
    List {
        /// Directory to scan
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Validate YAML frontmatter in files
    Validate {
        /// File or directory to validate
        path: PathBuf,
    },
    /// Mark an email as approved
    MarkApproved {
        /// File to mark as approved
        file: PathBuf,
    },
    /// Demote an approved draft back to `draft` status (reverse of `mark-approved`)
    MarkDraft {
        /// File to mark as draft
        file: PathBuf,
    },
    /// Create a new email draft from template
    New {
        /// Name for the new draft file
        name: String,
    },
    /// Create a reply draft from a received email
    Reply {
        /// Path to the email to reply to (interactive selection if omitted)
        file: Option<PathBuf>,
        /// Reply to all recipients
        #[arg(long)]
        all: bool,
    },
    /// Forward an email to new recipients
    Forward {
        /// Path to the email to forward (interactive selection if omitted)
        file: Option<PathBuf>,
    },
    /// RSVP to a received calendar invitation (iMIP REPLY over SMTP)
    Invite {
        #[command(subcommand)]
        action: InviteAction,
    },
    /// List available IMAP mailboxes/folders
    ListMailboxes,

    /// Fetch emails from IMAP server
    Fetch {
        /// Filter by sender address
        #[arg(long)]
        from: Option<String>,
        /// Filter by recipient address
        #[arg(long)]
        to: Option<String>,
        /// Filter by CC address
        #[arg(long)]
        cc: Option<String>,
        /// Subject contains
        #[arg(long)]
        subject: Option<String>,
        /// Body contains
        #[arg(long)]
        body: Option<String>,
        /// Emails since date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Emails before date (YYYY-MM-DD)
        #[arg(long)]
        before: Option<String>,
        /// Max results (default: 10)
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
        /// Show full body instead of preview
        #[arg(long)]
        full: bool,
        /// Mailbox name (default: INBOX)
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
    },
    /// Sync mailboxes from the server into the local store
    Sync {
        /// Max messages per mailbox (default: 50)
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// Mailboxes to sync (default: INBOX, Archive, Sent)
        #[arg(long)]
        mailbox: Option<Vec<String>>,
        /// Show what would be ingested without writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Watch a mailbox for changes using IMAP IDLE
    Watch {
        /// Mailbox to watch (default: INBOX)
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        /// Timeout in seconds (exits with code 2 on timeout)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Archive an inbox email (server + local)
    Archive {
        /// Path to the inbox email file
        file: PathBuf,
    },
    /// Delete an inbox email (server + local)
    Delete {
        /// Path to the inbox email file
        file: PathBuf,
    },
    /// Open an email's attachment in the default application
    Open {
        /// Path to the email file
        file: PathBuf,
    },
    /// Save an email's attachment(s) to a directory
    Save {
        /// Path to the email file
        file: PathBuf,
        /// Output directory (default: current directory)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Search emails on the IMAP server
    Search {
        /// Search query (supports from:, to:, subject:, body:, since:, before:,
        /// message-id:, in: prefixes). message-id: matches the RFC 5322
        /// Message-ID exactly, with or without angle brackets.
        query: String,
        /// Mailbox to search (default: all configured mailboxes)
        #[arg(long)]
        mailbox: Option<String>,
        /// Max results (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Show full body instead of preview
        #[arg(long)]
        full: bool,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Contact index operations
    Contacts {
        #[command(subcommand)]
        action: ContactsAction,
    },
    /// Calendar / iMIP invite operations
    Calendar {
        #[command(subcommand)]
        action: CalendarAction,
    },
    /// Inspect and unblock the durable send queue
    Outbox {
        #[command(subcommand)]
        action: OutboxAction,
    },
    /// Dump the TUI key bindings from the single KEYMAP source of truth.
    ///
    /// Markdown by default; `--json` emits the section-grouped shape the
    /// website consumes. Regenerate the site data with:
    /// `mp dump-keys --json > website/src/data/tui-keys.json`
    /// (see scripts/regen-website-keys.sh).
    DumpKeys {
        /// Emit JSON grouped by section instead of Markdown.
        #[arg(long)]
        json: bool,
    },
    /// Dump message envelopes from the local mail tree as NDJSON.
    ///
    /// Offline: reads the local `.md` files only, never the network. One
    /// compact JSON object per line, with the fields account, mailbox,
    /// message_id, from, to, cc, subject, date_sort, flags, attachments
    /// (name + size) and invite. No filesystem paths appear in the output.
    ///
    /// Records are sorted by account, mailbox, date_sort, message_id and
    /// subject (the local filename breaks remaining ties without being
    /// emitted), so two runs over an unchanged tree are byte-identical.
    ///
    /// Dumps every configured account by default; `-A/--account` restricts it
    /// to one, `--mailbox` to the named mailboxes.
    DumpMailbox {
        /// Emit newline-delimited JSON. Currently the only output format, and
        /// required, so a later default cannot silently change this one.
        #[arg(long, required = true)]
        json: bool,
        /// Mailbox to dump (role, slug or sidebar label; repeatable).
        /// Default: every mailbox of every selected account.
        #[arg(long)]
        mailbox: Option<Vec<String>>,
    },
}

/// Operator commands for the durable outbox (#0037).
///
/// The outbox drives itself: queued messages are submitted and their Sent copy
/// appended on the next startup or sync. These are for the one case it cannot
/// decide alone, a submission that died without a verdict and may or may not
/// have been delivered.
#[derive(Subcommand)]
enum OutboxAction {
    /// List every queued, retrying or failed submission for the account
    List,
    /// Send a failed submission again (only after checking it did not arrive)
    Retry {
        /// Outbox row id, as shown by `mp outbox list`
        id: i64,
    },
    /// Drop a submission and release the message bytes it holds
    Discard {
        /// Outbox row id, as shown by `mp outbox list`
        id: i64,
    },
}

/// Organizer-side calendar operations (#0030).
#[derive(Subcommand)]
enum CalendarAction {
    /// Report what the stored attendee REPLY emails resolve on the stored
    /// invitations. Writes nothing: attendee statuses are derived from the
    /// `invite.ics` payloads wherever they are displayed, so there is no
    /// cached copy to rebuild.
    Rebuild {
        /// Account name (default: all configured accounts)
        #[arg(long)]
        account: Option<String>,
    },
}

/// RSVP actions for `mp invite <accept|tentative|decline> <email-path>`.
/// Whole-series only (v1); the target is a received invite `.md` with an
/// `event:` block and a sidecar `invite.ics`.
#[derive(Subcommand)]
enum InviteAction {
    /// Accept the invitation (PARTSTAT=ACCEPTED)
    Accept {
        /// Path to the received invite email `.md`
        file: PathBuf,
    },
    /// Tentatively accept the invitation (PARTSTAT=TENTATIVE)
    Tentative {
        /// Path to the received invite email `.md`
        file: PathBuf,
    },
    /// Decline the invitation (PARTSTAT=DECLINED)
    Decline {
        /// Path to the received invite email `.md`
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum ContactsAction {
    /// Search the contact index
    Search {
        /// Query string (fuzzy-matched against name and email)
        query: Option<String>,
        /// Emit tab-delimited `email\tname` lines (for mutt/aerc/vim integration)
        #[arg(long)]
        parsable: bool,
        /// Max number of results
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Account name (default: first configured account)
        #[arg(long)]
        account: Option<String>,
    },
    /// Rebuild the contact index from local mailbox files
    Rebuild {
        /// Account name (default: all configured accounts)
        #[arg(long)]
        account: Option<String>,
    },
    /// Show index statistics
    Stats {
        /// Account name (default: first configured account)
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Interactive setup wizard
    Init,
    /// Show current configuration
    Show,
    /// Store a password in the active secrets backend
    SetPassword {
        /// Which password to set: "smtp" or "imap"
        which: String,
        /// Account name (required if multiple accounts)
        #[arg(long)]
        account: Option<String>,
    },
    /// Wipe the encrypted secrets file (and OAuth2 token caches) and re-prompt
    /// for credentials. Use this after a Time Machine restore to a new
    /// machine, or whenever the secrets file can no longer be decrypted.
    ResetSecrets,
    /// Add a new account to the existing config
    AddAccount,
    /// Run OAuth2 device code flow to acquire and cache a token
    Oauth2Login {
        /// Account name (default: first OAuth2 account)
        #[arg(long)]
        account: Option<String>,
    },
    /// Print config file path
    Path,
}

/// Sort fetched emails by date descending (newest first).
fn sort_fetched_by_date(emails: &mut [FetchedEmail]) {
    emails.sort_by(|a, b| {
        let da = chrono::DateTime::parse_from_rfc2822(&a.date).ok();
        let db = chrono::DateTime::parse_from_rfc2822(&b.date).ok();
        db.cmp(&da)
    });
}

fn prompt_confirmation(message: &str) -> bool {
    print!("{} [y/N] ", message);
    let _ = io::stdout().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Parse an address string like `"Name <addr>"` or `"addr"` into `(name, address)`.
fn parse_name_address(s: &str) -> (String, String) {
    let s = s.trim();
    if let Some(lt) = s.find('<') {
        if let Some(gt) = s.find('>') {
            let name = s[..lt].trim().trim_matches('"').trim().to_string();
            let addr = s[lt + 1..gt].trim().to_string();
            return (name, addr);
        }
    }
    // Plain address
    (String::new(), s.to_string())
}

/// Parse a frontmatter address field (comma-separated) into `(name, address)` pairs.
fn parse_recipients(field: Option<&str>) -> Vec<(String, String)> {
    match field {
        Some(s) if !s.trim().is_empty() => {
            split_addresses(s)
                .into_iter()
                .map(|a| parse_name_address(&a))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// CLI arguments for `mp send --invite`.
struct InviteArgs {
    to: Option<String>,
    cc: Option<String>,
    subject: Option<String>,
    start: Option<String>,
    end: Option<String>,
    duration: Option<String>,
    location: Option<String>,
    description: Option<String>,
    yes: bool,
}

/// `mp outbox <list|retry|discard>`: the operator surface of the durable send
/// queue (#0037 item 5).
///
/// Everything here is deliberately manual. A submission that died without a
/// verdict may or may not have been delivered, and no automatic rule can tell
/// the difference, so the row waits in `failed` until a human has looked in the
/// recipient's mailbox and decided between `retry` and `discard`.
async fn cmd_outbox(
    account_config: &email::config::AccountConfig,
    action: OutboxAction,
) -> Result<()> {
    use email::outbox::{self, OutboxState};

    let path = email::config::store_path(&account_config.name);
    if !path.exists() {
        println!("  {} nothing has been queued for {} yet", "·".dimmed(), account_config.name);
        return Ok(());
    }
    let store = email::store::Store::open(&path)?;
    let blobs = email::store::BlobStore::for_account(&account_config.name);

    match action {
        OutboxAction::List => {
            let rows = outbox::unfinished_rows(&store, &account_config.name)?;
            if rows.is_empty() {
                println!(
                    "  {} the outbox for {} is clear",
                    "\u{2713}".green(),
                    account_config.name
                );
                return Ok(());
            }
            for row in &rows {
                let state = match row.state {
                    OutboxState::Failed => row.state.to_string().red(),
                    OutboxState::PendingSend => row.state.to_string().yellow(),
                    _ => row.state.to_string().normal(),
                };
                println!(
                    "  {:>4}  {:<20} {}  {}",
                    row.id,
                    state,
                    format_unix_time(row.updated).dimmed(),
                    row.message_id
                );
                if let Some(target) = row.target_mailbox.as_deref() {
                    println!("        {} {target}", "sent copy ->".dimmed());
                }
                if row.state == OutboxState::PendingSend && row.submission_started_at.is_none() {
                    println!("        {}", "never submitted; the next sync sends it".dimmed());
                }
                if let Some(err) = row.last_error.as_deref() {
                    println!("        {} {err}", "last error:".dimmed());
                }
            }
            let counts = outbox::counts(&store, &account_config.name)?;
            println!(
                "  {} {} working, {} failed",
                "\u{21bb}".dimmed(),
                counts.open,
                counts.failed
            );
        }
        OutboxAction::Retry { id } => {
            outbox::retry(&store, id)?;
            println!(
                "  {} row {id} is queued again; sending it now",
                "\u{21bb}".dimmed()
            );
            // Drop the handle first: the resume path opens the store itself.
            drop(store);
            let result = email::send::resume_outbox(account_config).await;
            let store = email::store::Store::open(&path)?;
            match outbox::load(&store, id)? {
                Some(row) => println!("  {} row {id} is now {}", "\u{2713}".green(), row.state),
                None => println!("  {} row {id} is gone", "\u{2713}".green()),
            }
            if result.completed > 0 {
                println!("  {} {} sent copy/copies filed", "\u{2713}".green(), result.completed);
            }
        }
        OutboxAction::Discard { id } => {
            let Some(row) = outbox::load(&store, id)? else {
                return Err(anyhow!("no outbox row {id}"));
            };
            outbox::discard(&store, &blobs, id)?;
            println!(
                "  {} discarded row {id} ({}); its bytes are released",
                "\u{2713}".green(),
                row.message_id
            );
        }
    }
    Ok(())
}

/// A unix timestamp as local `YYYY-MM-DD HH:MM`, or `-` when it is unset.
fn format_unix_time(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

/// Send an iMIP calendar invitation (`METHOD:REQUEST`) over SMTP.
///
/// Builds the `VEVENT` (via `email::invite`), assembles the iMIP MIME tree
/// (via `build_draft_message(..., Some(ics))`) and submits it through the
/// durable outbox, which also files the local Sent copy so the #0027 receive
/// path (and #0030 reconciliation) can pick it up.
async fn run_send_invite(
    account_config: &email::config::AccountConfig,
    smtp_config: &SmtpConfig,
    global_config: &GlobalConfig,
    signature: Option<&str>,
    args: InviteArgs,
) -> Result<()> {
    // Graph accounts cannot send iMIP invites yet (Graph send is #0036, blocked
    // on tenant admin approval #0035). Fail clearly rather than silently.
    if account_config.auth_method == AuthMethod::Graph {
        return Err(anyhow!(
            "`mp send --invite` is not supported for Graph accounts yet (Graph calendar send \
             is tracked by #0036, blocked on #0035). Use an SMTP-configured account."
        ));
    }

    let subject = args
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("--invite requires --subject (used as the event summary)"))?;
    let start = args
        .start
        .as_deref()
        .ok_or_else(|| anyhow!("--invite requires --start"))?;

    // Resolve times with validation (end after start, exactly one of end/duration).
    let (start_dt, end_dt) =
        email::invite::resolve_times(start, args.end.as_deref(), args.duration.as_deref())?;

    // Attendees from --to/--cc (dedup, bare addresses). To/Cc headers keep the
    // full form; ATTENDEE lines use the extracted address.
    let to_field = args.to.as_deref().filter(|s| !s.trim().is_empty());
    let cc_field = args.cc.as_deref().filter(|s| !s.trim().is_empty());

    let mut attendees: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for field in [to_field, cc_field].into_iter().flatten() {
        for raw in split_addresses(field) {
            let addr = extract_email_address(&raw);
            if !addr.is_empty() && seen.insert(addr.to_lowercase()) {
                attendees.push(addr);
            }
        }
    }
    if attendees.is_empty() {
        return Err(anyhow!("--invite requires at least one recipient via --to/--cc"));
    }

    // ORGANIZER = the sending account's primary address (must match, or Exchange
    // silently drops the invite). Use the bare email part of default_from.
    let organizer = extract_email_address(&account_config.default_from);
    if organizer.is_empty() {
        return Err(anyhow!(
            "Account has no usable primary address (default_from); cannot set ORGANIZER"
        ));
    }

    let uid = email::invite::generate_uid(&organizer);
    let spec = email::invite::InviteSpec {
        uid: uid.clone(),
        organizer: organizer.clone(),
        attendees: attendees.clone(),
        summary: subject.to_string(),
        start: start_dt,
        end: end_dt,
        location: args.location.clone().filter(|s| !s.trim().is_empty()),
        description: args.description.clone().filter(|s| !s.trim().is_empty()),
    };
    let ics = email::invite::build_invite_ics(&spec)?;

    // Human-readable body: reuse the description, else a short summary line.
    let body = args
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("You are invited to: {}", subject));

    // Build the event: frontmatter block from the same ICS (round-trips via #0027).
    let event = email::calendar::parse_ics(ics.as_bytes())
        .map(|p| email::calendar::event_frontmatter(&p));

    // Synthetic draft, used only to build the message: nothing is written to
    // disk on this path any more (the Sent copy is the outbox's, #0037).
    let sent_at = chrono::Utc::now();
    let draft_path = PathBuf::from(format!(
        "{}-invite-{}.md",
        sent_at.format("%Y%m%d-%H%M%S"),
        email::parse::slugify_subject(subject)
    ));

    let draft = EmailDraft {
        path: draft_path.clone(),
        frontmatter: EmailFrontmatter {
            to: to_field.map(str::to_string),
            cc: cc_field.map(str::to_string),
            bcc: None,
            subject: subject.to_string(),
            status: EmailStatus::Approved,
            from: Some(account_config.default_from.clone()),
            reply_to: None,
            attachments: None,
            sent_at: None,
            sent_via: None,
            message_id: None,
            event,
        },
        body_markdown: body,
    };

    // Preview.
    println!("{}", "--- Invite Preview ---".bold());
    println!("  {} {}", "Summary:".yellow(), subject);
    println!("  {} {}", "Organizer:".green(), organizer);
    println!("  {} {}", "Attendees:".green(), attendees.join(", "));
    println!(
        "  {} {}  →  {}",
        "When:".blue(),
        start_dt.to_rfc3339(),
        end_dt.to_rfc3339()
    );
    if let Some(loc) = spec.location.as_deref() {
        println!("  {} {}", "Location:".blue(), loc);
    }
    println!("  {} {}", "UID:".dimmed(), uid);
    println!("{}", "---".dimmed());

    if !args.yes && !prompt_confirmation("Send this invitation?") {
        println!("Cancelled.");
        return Ok(());
    }

    println!("Sending invitation...");
    let built = email::send::build_draft_message(
        &draft,
        &smtp_config.default_from,
        &global_config.email,
        signature,
        Some(&ics),
    )?;
    let report = email::send::send_durably(&built, account_config, smtp_config).await?;
    let send_result = &report.send_result;

    for r in send_result.succeeded() {
        println!("  {} {} ({})", "✓".green(), r.address, r.role);
    }
    for r in send_result.failed() {
        println!(
            "  {} {} ({}): {}",
            "✗".red(),
            r.address,
            r.role,
            r.error.as_deref().unwrap_or("unknown error")
        );
    }

    if !send_result.any_succeeded() {
        return Err(anyhow!(
            "Failed to send invitation to all {} recipient(s)",
            send_result.results.len()
        ));
    }

    email::contacts::hooks::bump_after_send(account_config, &draft);

    if send_result.all_succeeded() {
        println!(
            "{} Invitation sent to all {} recipient(s) [{}]",
            "✓".green().bold(),
            send_result.results.len(),
            report.status_line()
        );
    } else {
        println!(
            "{} Partial send: {} succeeded, {} failed",
            "⚠".yellow().bold(),
            send_result.succeeded().len(),
            send_result.failed().len()
        );
    }

    Ok(())
}

/// RSVP to a received calendar invitation (`mp invite accept|tentative|decline`).
///
/// Reads the invitation's `invite.ics` blob out of the account's store, builds
/// a `METHOD:REPLY` with the account's `PARTSTAT` and emails it to the
/// `ORGANIZER`. Nothing local is rewritten: our own `PARTSTAT` is derived from
/// the reply the outbox files in Sent (#0038 scope item 6). Graph accounts are
/// rejected (Graph RSVP is #0036).
///
/// `file` still names a `.md` path, which no longer exists: naming a message
/// on the command line is the selector contract [#0050] owns, so this command
/// declines until it lands rather than guessing which row was meant.
async fn run_invite_rsvp(
    account_config: &email::config::AccountConfig,
    smtp_config: &SmtpConfig,
    file: &Path,
    rsvp: email::invite::Rsvp,
) -> Result<()> {
    if account_config.auth_method == AuthMethod::Graph {
        return Err(anyhow!(
            "`mp invite` RSVP is not supported for Graph accounts yet (Graph calendar RSVP \
             is tracked by #0036, blocked on #0035). Use an SMTP-configured account."
        ));
    }

    let ics = std::fs::read(file).map_err(|e| {
        anyhow!(
            "cannot read {}: {e}. Mail is no longer stored as files; addressing a message on \
             the command line lands with the selector contract (#0050). RSVP from the TUI \
             (`V`) meanwhile.",
            file.display()
        )
    })?;

    let account_address = email::parse::extract_email_address(&account_config.default_from);

    println!("Sending {} reply...", rsvp.subject_verb().to_lowercase());
    let outcome =
        email::send::send_rsvp(&ics, account_config, &account_address, rsvp, smtp_config).await?;

    if !outcome.send_result.any_succeeded() {
        return Err(anyhow!(
            "Failed to send RSVP to organizer {}",
            outcome.organizer
        ));
    }

    println!(
        "{} {} — replied to {}",
        "✓".green().bold(),
        outcome.subject,
        outcome.organizer
    );
    println!(
        "  {} Note: not synced to the Exchange server calendar (no Graph in v1).",
        "ℹ".blue()
    );

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();

    info!("mailypoppins started: {:?}", std::env::args().collect::<Vec<_>>());

    // Load global config from ~/.config/email/config.toml
    let global_config = load_global_config().unwrap_or_else(|e| {
        eprintln!("{} {}", "⚠".yellow(), e);
        eprintln!("  Some commands may not work without proper configuration.");
        GlobalConfig::default()
    });

    // Initialize the secrets backend (encrypted file by default, or OS keyring
    // if the user opted in via `secrets_backend = "keyring"` in config.toml).
    if let Err(e) = email::config::init_secrets_backend(&global_config) {
        match &e {
            email::secrets::SecretsError::NotInitialized(_) => {
                // Empty store at startup is fine -- `mp config init` or
                // `set-password` will populate it. The actual missing-key
                // error surfaces later when `SmtpConfig::load` is called.
            }
            email::secrets::SecretsError::Undecryptable(_, _) => {
                eprintln!("{} {}", "\u{2717}".red(), e);
                std::process::exit(1);
            }
            email::secrets::SecretsError::Other(err) => {
                eprintln!("{} Could not initialize secrets backend: {}", "\u{26a0}".yellow(), err);
            }
        }
    }

    // Resolve which account to use
    let account_config: email::config::AccountConfig = if let Some(ref name) = cli.account {
        global_config.accounts.iter()
            .find(|a| a.name == *name)
            .cloned()
            .unwrap_or_else(|| {
                eprintln!("{} Account '{}' not found in config", "⚠".yellow(), name);
                email::config::AccountConfig::default()
            })
    } else {
        global_config.accounts.first().cloned().unwrap_or_default()
    };

    // Load SMTP config from account config + keyring
    let smtp_config = SmtpConfig::load(&account_config).unwrap_or_else(|e| {
        eprintln!("{} Could not load SMTP config: {}", "⚠".yellow(), e);
        eprintln!("  Some commands may not work without proper configuration.");
        SmtpConfig {
            host: "localhost".to_string(),
            port: 465,
            username: String::new(),
            password: String::new(),
            default_from: "user@example.com".to_string(),
            accept_invalid_certs: false,
            auth_method: email::config::AuthMethod::Password,
        }
    });

    // Determine signature to use
    let signature_content: Option<String> = if cli.no_signature {
        None
    } else if global_config.email.include_signature {
        load_signature(&account_config, cli.signature.as_deref())
    } else {
        // include_signature is false, but user can override with --signature
        cli.signature
            .as_deref()
            .and_then(|s| load_signature(&account_config, Some(s)))
    };

    // Resolve directories from config (mailbox-based -> derived from data dir)
    let drafts_dir: Option<PathBuf> = Some(email::config::drafts_dir(&account_config.name));
    let inbox_dir: Option<PathBuf> = account_config
        .mailboxes
        .inbox
        .as_ref()
        .map(|_| email::config::mailbox_dir(&account_config.name, "inbox"));
    let archive_dir: Option<PathBuf> = account_config
        .mailboxes
        .archive
        .as_ref()
        .map(|_| email::config::mailbox_dir(&account_config.name, "archive"));

    match cli.command {
        Some(Commands::Send {
            file,
            yes,
            invite,
            to,
            cc,
            subject,
            start,
            end,
            duration,
            location,
            description,
        }) => {
            if invite {
                run_send_invite(
                    &account_config,
                    &smtp_config,
                    &global_config,
                    signature_content.as_deref(),
                    InviteArgs {
                        to,
                        cc,
                        subject,
                        start,
                        end,
                        duration,
                        location,
                        description,
                        yes,
                    },
                )
                .await?;
                return Ok(());
            }

            let file = file.ok_or_else(|| {
                anyhow!("`mp send` needs a draft file, or use `--invite` to send a calendar invitation")
            })?;
            let draft = parse_email_draft(&file)?;
            validate_draft(&draft)?;

            if account_config.auth_method == AuthMethod::Graph {
                // Graph API send path
                let graph_config = GraphConfig::load(&account_config)?;

                // Preview (simplified -- no SMTP config needed)
                println!("{}", "--- Email Preview ---".bold());
                println!("  {} {}", "To:".green(), draft.frontmatter.to.as_deref().unwrap_or("(none)"));
                if let Some(ref cc) = draft.frontmatter.cc {
                    println!("  {} {}", "Cc:".blue(), cc);
                }
                if let Some(ref bcc) = draft.frontmatter.bcc {
                    println!("  {} {}", "Bcc:".blue(), bcc);
                }
                println!("  {} {}", "Subject:".yellow(), draft.frontmatter.subject);
                println!("{}", "---".dimmed());

                if !yes && !prompt_confirmation("Send this email?") {
                    println!("Cancelled.");
                    return Ok(());
                }

                println!("Sending via Graph API...");
                let to = parse_recipients(draft.frontmatter.to.as_deref());
                let cc = parse_recipients(draft.frontmatter.cc.as_deref());
                let bcc = parse_recipients(draft.frontmatter.bcc.as_deref());

                let to_refs: Vec<(&str, &str)> = to.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
                let cc_refs: Vec<(&str, &str)> = cc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
                let bcc_refs: Vec<(&str, &str)> = bcc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();

                // Render HTML
                let quoted_html = draft.path.with_extension("html");
                let quoted = if quoted_html.exists() { fs::read_to_string(&quoted_html).ok() } else { None };
                let html_body = markdown_to_html(
                    &draft.body_markdown,
                    &global_config.email,
                    signature_content.as_deref(),
                    quoted.as_deref(),
                );

                // Read attachments
                let mut att_data: Vec<(String, Vec<u8>, String)> = Vec::new();
                if let Some(ref attachments) = draft.frontmatter.attachments {
                    for att_path in attachments {
                        let expanded = shellexpand::tilde(att_path);
                        let path = Path::new(expanded.as_ref());
                        let content = fs::read(path)
                            .with_context(|| format!("Failed to read attachment: {}", att_path))?;
                        let filename = path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "attachment".to_string());
                        let content_type = mime_guess::from_path(path)
                            .first_or_octet_stream()
                            .to_string();
                        att_data.push((filename, content, content_type));
                    }
                }

                let client = graph::GraphClient::new_async(&graph_config).await?;
                let built = email::send::build_draft_message(
                    &draft,
                    &account_config.default_from,
                    &global_config.email,
                    signature_content.as_deref(),
                    None,
                )?;
                // Graph files its own copy in Sent Items, so the outbox row
                // never carries a target mailbox; what it buys here is
                // exactly-once durability of the submission.
                let report = email::send::send_durably_via(
                    &built,
                    &account_config,
                    client.send_mail(&to_refs, &cc_refs, &bcc_refs, &draft.frontmatter.subject, &html_body, &att_data),
                )
                .await?;
                if !report.send_result.any_succeeded() {
                    return Err(anyhow!(
                        "{}",
                        report
                            .send_result
                            .failed()
                            .first()
                            .and_then(|r| r.error.clone())
                            .unwrap_or_else(|| "Graph send failed".to_string())
                    ));
                }

                mark_draft_sent(&draft, Some(&built.message_id))?;
                info!("Email sent via Graph and marked as sent: {}", draft.path.display());
                email::contacts::hooks::bump_after_send(&account_config, &draft);
                println!(
                    "{} Email sent successfully via Graph API [{}]",
                    "✓".green().bold(),
                    report.status_line()
                );
            } else {
                // SMTP send path (existing)
                preview_draft(
                    &draft,
                    &smtp_config,
                    &global_config.email,
                    signature_content.as_deref(),
                    false,
                )?;

                if !yes && !prompt_confirmation("Send this email?") {
                    println!("Cancelled.");
                    return Ok(());
                }

                println!("Sending email...");
                let built = email::send::build_draft_message(
                    &draft,
                    &smtp_config.default_from,
                    &global_config.email,
                    signature_content.as_deref(),
                    None,
                )?;
                let report =
                    email::send::send_durably(&built, &account_config, &smtp_config).await?;
                let send_result = &report.send_result;

                // Display per-recipient results
                for r in &send_result.succeeded() {
                    println!(
                        "  {} {} ({})",
                        "✓".green(),
                        r.address,
                        r.role
                    );
                }
                for r in &send_result.failed() {
                    println!(
                        "  {} {} ({}): {}",
                        "✗".red(),
                        r.address,
                        r.role,
                        r.error.as_deref().unwrap_or("unknown error")
                    );
                }

                if send_result.all_succeeded() {
                    mark_draft_sent(&draft, Some(&built.message_id))?;
                    info!("Email marked as sent: {}", draft.path.display());

                    // Incremental contacts-index update (best-effort, no-op if no cache)
                    email::contacts::hooks::bump_after_send(&account_config, &draft);

                    println!(
                        "{} Email sent successfully to all {} recipient(s) [{}]",
                        "✓".green().bold(),
                        send_result.results.len(),
                        report.status_line()
                    );
                } else if send_result.any_succeeded() {
                    mark_draft_sent(&draft, Some(&built.message_id))?;
                    warn!(
                        "Partial send: {} succeeded, {} failed for {}",
                        send_result.succeeded().len(),
                        send_result.failed().len(),
                        draft.path.display()
                    );

                    // Incremental contacts-index update (best-effort)
                    email::contacts::hooks::bump_after_send(&account_config, &draft);

                    println!(
                        "{} Partial send: {} succeeded, {} failed [{}] (marked as sent -- see logs for details)",
                        "⚠".yellow().bold(),
                        send_result.succeeded().len().to_string().green(),
                        send_result.failed().len().to_string().red(),
                        report.status_line()
                    );
                } else {
                    error!("All recipients failed for {}", draft.path.display());
                    return Err(anyhow!(
                        "Failed to send to all {} recipient(s)",
                        send_result.results.len()
                    ));
                }
            }
        }

        Some(Commands::SendApproved { dir, yes }) => {
            let dir = resolve_drafts_dir(&dir, &drafts_dir);
            let drafts = find_drafts(&dir, Some(EmailStatus::Approved))?;

            if drafts.is_empty() {
                println!("No approved emails found in {}", dir.display());
                return Ok(());
            }

            println!(
                "\n{} approved email(s) found:\n",
                drafts.len().to_string().bold()
            );

            for draft in &drafts {
                println!(
                    "  {} -> {}",
                    draft.path.file_name().unwrap_or_default().to_string_lossy(),
                    draft.frontmatter.to.as_deref().unwrap_or("(bcc only)")
                );
            }

            if !yes && !prompt_confirmation(&format!("\nSend all {} emails?", drafts.len())) {
                println!("Cancelled.");
                return Ok(());
            }

            let mut sent_count = 0;
            let mut failed_count = 0;

            if account_config.auth_method == AuthMethod::Graph {
                let graph_config = GraphConfig::load(&account_config)?;
                let client = graph::GraphClient::new_async(&graph_config).await?;

                for draft in drafts {
                    print!("Sending to {}... ", draft.frontmatter.to.as_deref().unwrap_or("(bcc only)"));
                    io::stdout().flush()?;

                    let to = parse_recipients(draft.frontmatter.to.as_deref());
                    let cc = parse_recipients(draft.frontmatter.cc.as_deref());
                    let bcc = parse_recipients(draft.frontmatter.bcc.as_deref());
                    let to_refs: Vec<(&str, &str)> = to.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
                    let cc_refs: Vec<(&str, &str)> = cc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
                    let bcc_refs: Vec<(&str, &str)> = bcc.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();

                    let quoted_html = draft.path.with_extension("html");
                    let quoted = if quoted_html.exists() { fs::read_to_string(&quoted_html).ok() } else { None };
                    let html_body = markdown_to_html(
                        &draft.body_markdown,
                        &global_config.email,
                        signature_content.as_deref(),
                        quoted.as_deref(),
                    );

                    let mut att_data: Vec<(String, Vec<u8>, String)> = Vec::new();
                    if let Some(ref attachments) = draft.frontmatter.attachments {
                        for att_path in attachments {
                            let expanded = shellexpand::tilde(att_path);
                            let path = Path::new(expanded.as_ref());
                            if let Ok(content) = fs::read(path) {
                                let filename = path.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "attachment".to_string());
                                let ct = mime_guess::from_path(path).first_or_octet_stream().to_string();
                                att_data.push((filename, content, ct));
                            }
                        }
                    }

                    let built = match email::send::build_draft_message(
                        &draft,
                        &account_config.default_from,
                        &global_config.email,
                        signature_content.as_deref(),
                        None,
                    ) {
                        Ok(built) => built,
                        Err(e) => {
                            println!("{} {}", "✗".red(), e);
                            error!("Failed to build {}: {}", draft.path.display(), e);
                            failed_count += 1;
                            continue;
                        }
                    };
                    let report = email::send::send_durably_via(
                        &built,
                        &account_config,
                        client.send_mail(&to_refs, &cc_refs, &bcc_refs, &draft.frontmatter.subject, &html_body, &att_data),
                    )
                    .await?;
                    if report.send_result.any_succeeded() {
                        if let Err(e) = mark_draft_sent(&draft, Some(&built.message_id)) {
                            println!("{} (sent but failed to update status: {})", "⚠".yellow(), e);
                        } else {
                            email::contacts::hooks::bump_after_send(&account_config, &draft);
                            println!("{} [{}]", "✓".green(), report.status_line());
                        }
                        sent_count += 1;
                    } else {
                        let err = report
                            .send_result
                            .failed()
                            .first()
                            .and_then(|r| r.error.clone())
                            .unwrap_or_else(|| "Graph send failed".to_string());
                        println!("{} {}", "✗".red(), err);
                        error!("Graph send error for {}: {}", draft.path.display(), err);
                        failed_count += 1;
                    }
                }
            } else {
                for draft in drafts {
                    print!("Sending to {}... ", draft.frontmatter.to.as_deref().unwrap_or("(bcc only)"));
                    io::stdout().flush()?;

                    let built = match email::send::build_draft_message(
                        &draft,
                        &smtp_config.default_from,
                        &global_config.email,
                        signature_content.as_deref(),
                        None,
                    ) {
                        Ok(built) => built,
                        Err(e) => {
                            println!("{} {}", "✗".red(), e);
                            error!("Failed to build {}: {}", draft.path.display(), e);
                            failed_count += 1;
                            continue;
                        }
                    };
                    match email::send::send_durably(&built, &account_config, &smtp_config).await {
                        Ok(report) => {
                            let send_result = &report.send_result;
                            if send_result.any_succeeded() {
                                if let Err(e) = mark_draft_sent(&draft, Some(&built.message_id)) {
                                    println!("{} (sent but failed to update status: {})", "⚠".yellow(), e);
                                } else {
                                    // Incremental contacts-index update (best-effort)
                                    email::contacts::hooks::bump_after_send(&account_config, &draft);
                                    if send_result.all_succeeded() {
                                        println!("{} [{}]", "✓".green(), report.status_line());
                                    } else {
                                        println!(
                                            "{} (partial: {}/{} recipients) [{}]",
                                            "⚠".yellow(),
                                            send_result.succeeded().len(),
                                            send_result.results.len(),
                                            report.status_line()
                                        );
                                    }
                                }
                                for r in &send_result.failed() {
                                    warn!(
                                        "Failed recipient {} ({}) for {}: {}",
                                        r.address,
                                        r.role,
                                        draft.path.display(),
                                        r.error.as_deref().unwrap_or("unknown")
                                    );
                                }
                                sent_count += 1;
                            } else {
                                println!("{} all recipients failed [{}]", "✗".red(), report.status_line());
                                for r in &send_result.failed() {
                                    error!(
                                        "Failed recipient {} ({}) for {}: {}",
                                        r.address,
                                        r.role,
                                        draft.path.display(),
                                        r.error.as_deref().unwrap_or("unknown")
                                    );
                                }
                                failed_count += 1;
                            }
                        }
                        Err(e) => {
                            println!("{} {}", "✗".red(), e);
                            error!("Fatal send error for {}: {}", draft.path.display(), e);
                            failed_count += 1;
                        }
                    }
                }
            }

            println!(
                "\n{}: {} sent, {} failed",
                "Summary".bold(),
                sent_count.to_string().green(),
                failed_count.to_string().red()
            );
        }

        Some(Commands::List { dir }) => {
            let dir = resolve_drafts_dir(&dir, &drafts_dir);
            let drafts = find_drafts(&dir, None)?;

            if drafts.is_empty() {
                println!("No email drafts found in {}", dir.display());
                return Ok(());
            }

            let mut draft_count = 0;
            let mut approved_count = 0;
            let mut sent_count = 0;
            let mut inbox_count = 0;
            let mut archived_count = 0;

            println!("\n{}", "Email Drafts:".bold());
            println!("{}", "─".repeat(60));

            for draft in &drafts {
                let status_colored = match draft.frontmatter.status {
                    EmailStatus::Draft => "draft".yellow(),
                    EmailStatus::Approved => "approved".green(),
                    EmailStatus::Sent => "sent".dimmed(),
                    EmailStatus::Inbox => "inbox".cyan(),
                    EmailStatus::Archived => "archived".blue(),
                };

                match draft.frontmatter.status {
                    EmailStatus::Draft => draft_count += 1,
                    EmailStatus::Approved => approved_count += 1,
                    EmailStatus::Sent => sent_count += 1,
                    EmailStatus::Inbox => inbox_count += 1,
                    EmailStatus::Archived => archived_count += 1,
                }

                println!(
                    "[{}] {} → {}",
                    status_colored,
                    draft.path.file_name().unwrap_or_default().to_string_lossy(),
                    draft.frontmatter.to.as_deref().unwrap_or("(bcc only)")
                );
            }

            println!("{}", "─".repeat(60));
            println!(
                "Total: {} | Draft: {} | Approved: {} | Sent: {} | Inbox: {} | Archived: {}",
                drafts.len(),
                draft_count.to_string().yellow(),
                approved_count.to_string().green(),
                sent_count.to_string().dimmed(),
                inbox_count.to_string().cyan(),
                archived_count.to_string().blue()
            );
        }

        Some(Commands::Validate { path }) => {
            let files: Vec<PathBuf> = if path.is_dir() {
                WalkDir::new(&path)
                    .max_depth(1)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path().is_file() && e.path().extension().is_some_and(|ext| ext == "md")
                    })
                    .map(|e| e.path().to_path_buf())
                    .collect()
            } else {
                vec![path.clone()]
            };

            let mut valid_count = 0;
            let mut invalid_count = 0;

            for file in &files {
                match parse_email_draft(file) {
                    Ok(draft) => match validate_draft(&draft) {
                        Ok(warnings) => {
                            print!("{} {}", "✓".green(), file.display());
                            if !warnings.is_empty() {
                                print!(" ({})", warnings.join(", ").yellow());
                            }
                            println!();
                            valid_count += 1;
                        }
                        Err(e) => {
                            println!("{} {} - {}", "✗".red(), file.display(), e);
                            invalid_count += 1;
                        }
                    },
                    Err(e) => {
                        println!("{} {} - {}", "✗".red(), file.display(), e);
                        invalid_count += 1;
                    }
                }
            }

            println!(
                "\nValidation complete: {} valid, {} invalid",
                valid_count.to_string().green(),
                invalid_count.to_string().red()
            );

            if invalid_count > 0 {
                std::process::exit(1);
            }
        }

        Some(Commands::MarkApproved { file }) => {
            let msg = mark_as_approved(&file)?;
            if msg.starts_with("Already") {
                println!("{} {}", "ℹ".blue(), msg);
            } else {
                println!("{} {}", "✓".green(), msg);
            }
        }

        Some(Commands::MarkDraft { file }) => {
            let msg = mark_as_draft(&file)?;
            if msg.starts_with("Already") {
                println!("{} {}", "\u{2139}".blue(), msg);
            } else {
                println!("{} {}", "\u{2713}".green(), msg);
            }
        }

        Some(Commands::New { name }) => {
            // Add .md extension if not present
            let file_name = if Path::new(&name).extension().is_some() {
                name.clone()
            } else {
                format!("{}.md", name)
            };
            let dir = resolve_drafts_dir(Path::new("."), &drafts_dir);
            let path = dir.join(&file_name);

            // Check if file already exists
            if path.exists() {
                return Err(anyhow!("File already exists: {}", path.display()));
            }

            // Create skeleton content with all frontmatter fields
            let now = chrono::Utc::now().to_rfc2822();
            let from = &smtp_config.default_from;
            let skeleton = new_draft_skeleton(from, &now);

            fs::write(&path, skeleton)?;
            println!("{} Created new draft: {}", "✓".green(), path.display());
        }

        Some(Commands::Reply { file, all }) => {
            let resolved = resolve_drafts_dir(Path::new("."), &drafts_dir);
            let reply_drafts_dir: Option<PathBuf> = if resolved != Path::new(".").to_path_buf() {
                Some(resolved)
            } else {
                None
            };

            let source_file = match file {
                Some(f) => f,
                None => {
                    let dir = inbox_dir.as_ref().ok_or_else(|| {
                        anyhow!("Inbox mailbox not configured. Check [mailboxes.inbox] in {}", config_path().display())
                    })?;
                    select_inbox_email(dir, "Select an email to reply to")?
                }
            };

            let draft_path = create_reply_draft(
                &source_file,
                all,
                &smtp_config.default_from,
                reply_drafts_dir.as_deref(),
            )?;
            println!(
                "{} Reply draft created: {}",
                "✓".green(),
                draft_path.display()
            );
        }

        Some(Commands::Forward { file }) => {
            let resolved = resolve_drafts_dir(Path::new("."), &drafts_dir);
            let fwd_drafts_dir: Option<PathBuf> = if resolved != Path::new(".").to_path_buf() {
                Some(resolved)
            } else {
                None
            };

            let source_file = match file {
                Some(f) => f,
                None => {
                    let dir = inbox_dir.as_ref().ok_or_else(|| {
                        anyhow!("Inbox mailbox not configured. Check [mailboxes.inbox] in {}", config_path().display())
                    })?;
                    select_inbox_email(dir, "Select an email to forward")?
                }
            };

            let draft_path = create_forward_draft(
                &source_file,
                &smtp_config.default_from,
                fwd_drafts_dir.as_deref(),
            )?;
            println!(
                "{} Forward draft created: {}",
                "✓".green(),
                draft_path.display()
            );
        }

        Some(Commands::Invite { action }) => {
            let (file, rsvp) = match action {
                InviteAction::Accept { file } => (file, email::invite::Rsvp::Accepted),
                InviteAction::Tentative { file } => (file, email::invite::Rsvp::Tentative),
                InviteAction::Decline { file } => (file, email::invite::Rsvp::Declined),
            };
            run_invite_rsvp(&account_config, &smtp_config, &file, rsvp).await?;
        }

        Some(Commands::ListMailboxes) => {
            if account_config.auth_method == AuthMethod::Graph {
                let graph_config = email::config::GraphConfig::load(&account_config)?;
                let client = email::graph::GraphClient::new_async(&graph_config).await?;
                let folders = client.list_folders().await?;

                println!("{} Available folders:", "ℹ".blue());
                for folder in &folders {
                    let unread = if folder.unread_item_count > 0 {
                        format!(" ({})", format!("{} unread", folder.unread_item_count).yellow())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {} {} total{}",
                        folder.display_name.green(),
                        folder.total_item_count,
                        unread,
                    );
                }
            } else {
                let imap_config = ImapConfig::load(&account_config)?;
                let mailboxes = list_mailboxes(&imap_config).await?;

                println!("{} Available mailboxes:", "ℹ".blue());
                for name in &mailboxes {
                    println!("  {}", name);
                }
            }
        }

        Some(Commands::Fetch {
            from,
            to,
            cc,
            subject,
            body,
            since,
            before,
            limit,
            full,
            mailbox,
        }) => {

            let emails = if account_config.auth_method == AuthMethod::Graph {
                let graph_config = GraphConfig::load(&account_config)?;
                let client = graph::GraphClient::new_async(&graph_config).await?;
                client.fetch_messages(&mailbox, limit).await?
            } else {
                let imap_config = ImapConfig::load(&account_config)?;
                let criteria = FetchCriteria {
                    from,
                    to,
                    cc,
                    subject,
                    body,
                    since,
                    before,
                    text: None,
                    message_id: None,
                    in_mailbox: None,
                };
                fetch_emails(&imap_config, &criteria, &mailbox, Some(limit)).await?
            };

            display_fetched_emails(&emails, full);

            // `mp fetch` is a lookup, not an ingest: it prints what the server
            // has and writes nothing. Messages enter the store through
            // `mp sync` (#0037), which is the only path that fetches by UID
            // and can key a row.
        }

        Some(Commands::Sync { limit, mailbox, dry_run }) => {
            let targets: Vec<imap_client::SyncTarget> = if let Some(ref user_mailboxes) = mailbox {
                user_mailboxes
                    .iter()
                    .map(|mb| imap_client::SyncTarget {
                        role: mb.clone(),
                        server_name: find_server_name_for_role(&account_config, mb),
                    })
                    .collect()
            } else {
                all_configured_mailboxes(&account_config)
                    .iter()
                    .map(|(role, mapping)| imap_client::SyncTarget {
                        role: role.clone(),
                        server_name: mapping.server.clone(),
                    })
                    .collect()
            };

            if !dry_run {
                // Resume the outbox first: a message that reached the server
                // before the last crash gets its Sent copy before this sync
                // reads the mailbox it belongs in (#0037 item 5).
                let drained = email::send::resume_outbox(&account_config).await;
                if drained.completed > 0 || drained.still_open > 0 {
                    println!(
                        "  {} outbox: {} completed, {} still pending",
                        "↻".dimmed(),
                        drained.completed,
                        drained.still_open + drained.awaiting_submission
                    );
                }
            }

            let result = if account_config.auth_method == AuthMethod::Graph {
                let graph_config = GraphConfig::load(&account_config)?;
                graph::sync_mailboxes_graph(
                    &graph_config,
                    &account_config.name,
                    &targets,
                    limit,
                    dry_run,
                )
                .await?
            } else {
                let imap_config = ImapConfig::load(&account_config)?;
                sync_mailboxes(
                    &imap_config,
                    &account_config.name,
                    &targets,
                    limit,
                    dry_run,
                )
                .await?
            };

            if !dry_run {
                // Incremental contacts-index update (best-effort).
                email::contacts::hooks::bump_after_sync(
                    &account_config,
                    &result.fresh_observations,
                );
            }

            let prefix = if dry_run { "[dry-run] " } else { "" };

            if result.skipped > 0 {
                println!(
                    "{} {}Synced: {} new, {} already present",
                    "✓".green(),
                    prefix,
                    result.saved,
                    result.skipped,
                );
            } else {
                println!(
                    "{} {}Synced: {} email(s) {}",
                    "✓".green(),
                    prefix,
                    result.saved,
                    if dry_run { "to download" } else { "ingested" },
                );
            }

            if result.read_updated > 0 {
                println!(
                    "{} {}Read status updated on {} message(s)",
                    "ℹ".blue(),
                    prefix,
                    result.read_updated,
                );
            }
            if result.uid_rebound > 0 {
                println!(
                    "{} {}Rebound {} message(s) to new UIDs after a UIDVALIDITY reset",
                    "ℹ".blue(),
                    prefix,
                    result.uid_rebound,
                );
            }
        }

        Some(Commands::Watch { mailbox, timeout }) => {
            if account_config.auth_method == AuthMethod::Graph {
                return Err(anyhow!(
                    "IMAP IDLE watch is not supported for Graph accounts. Use 'mp sync' instead."
                ));
            }
            let imap_config = ImapConfig::load(&account_config)?;
            println!("Watching {} for changes...", mailbox);
            let exit_code = watch_mailbox(&imap_config, &mailbox, timeout).await?;

            match exit_code {
                0 => println!("{} Mailbox changed.", "✓".green()),
                2 => println!("{} Timed out.", "ℹ".blue()),
                _ => {}
            }

            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }

        Some(Commands::Archive { file }) => {
            let dir = archive_dir.as_ref().ok_or_else(|| {
                anyhow!("Archive mailbox not configured. Check [mailboxes.archive] in {}", config_path().display())
            })?;
            let archive_server_name = account_config.mailboxes.archive.as_ref()
                .map(|m| m.server.as_str())
                .unwrap_or("Archive");

            let result = if account_config.auth_method == AuthMethod::Graph {
                let graph_config = GraphConfig::load(&account_config)?;
                graph::archive_email_graph(&graph_config, dir, &file, archive_server_name).await
            } else {
                let imap_config = ImapConfig::load(&account_config)?;
                archive_email_locally(&imap_config, dir, &file, archive_server_name).await
            };

            match result {
                Ok(()) => {
                    println!(
                        "{} Archived: {}",
                        "✓".green(),
                        file.display()
                    );
                }
                Err(e) => {
                    eprintln!(
                        "{} Failed to archive {}: {}",
                        "✗".red(),
                        file.display(),
                        e
                    );
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Delete { file }) => {
            let result = if account_config.auth_method == AuthMethod::Graph {
                let graph_config = GraphConfig::load(&account_config)?;
                graph::delete_email_graph(&graph_config, &file).await
            } else {
                let imap_config = ImapConfig::load(&account_config)?;
                delete_email_locally(&imap_config, &file).await
            };

            match result {
                Ok(()) => {
                    println!("{} Deleted: {}", "✓".green(), file.display());
                }
                Err(e) => {
                    eprintln!(
                        "{} Failed to delete {}: {}",
                        "✗".red(),
                        file.display(),
                        e
                    );
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Open { file }) => {
            let attachments = list_attachments(&file)?;
            if attachments.is_empty() {
                println!("No attachments found for {}", file.display());
                return Ok(());
            }
            let path = if attachments.len() == 1 {
                attachments.into_iter().next().expect("non-empty verified above")
            } else {
                let display_items: Vec<String> = attachments
                    .iter()
                    .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
                    .collect();
                let selection = dialoguer::FuzzySelect::new()
                    .with_prompt("Select attachment to open")
                    .items(&display_items)
                    .default(0)
                    .interact()
                    .map_err(|e| anyhow!("Selection cancelled: {e}"))?;
                attachments[selection].clone()
            };
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            println!("Opening: {name}");
            open_file_with_system(&path)?;
        }

        Some(Commands::Save { file, output }) => {
            let attachments = list_attachments(&file)?;
            if attachments.is_empty() {
                println!("No attachments found for {}", file.display());
                return Ok(());
            }
            let dest_dir = output.unwrap_or_else(|| PathBuf::from("."));

            // Multi-select attachments
            let display_items: Vec<String> = attachments
                .iter()
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
                .collect();

            let selections = if attachments.len() == 1 {
                vec![0]
            } else {
                let sel = dialoguer::MultiSelect::new()
                    .with_prompt("Select attachment(s) to save")
                    .items(&display_items)
                    .interact()
                    .map_err(|e| anyhow!("Selection cancelled: {e}"))?;
                if sel.is_empty() {
                    println!("No attachments selected.");
                    return Ok(());
                }
                sel
            };

            for idx in &selections {
                let source = &attachments[*idx];
                match email::parse::save_attachment(source, &dest_dir) {
                    Ok(dest) => {
                        println!(
                            "  {} Saved: {} -> {}",
                            "\u{2713}".green(),
                            source.file_name().unwrap_or_default().to_string_lossy(),
                            dest.display()
                        );
                    }
                    Err(e) => {
                        let name = source.file_name().unwrap_or_default().to_string_lossy();
                        println!("  {} Failed to save {}: {}", "\u{2717}".red(), name, e);
                    }
                }
            }
        }

        Some(Commands::Search {
            query,
            mailbox,
            limit,
            full,
        }) => {
            let mut criteria = parse_search_query(&query);

            // Resolve mailbox scope: --mailbox flag > in: prefix > all
            let mailbox_name = mailbox.or_else(|| criteria.in_mailbox.take());
            criteria.in_mailbox = None;

            if account_config.auth_method == AuthMethod::Graph {
                let graph_config = GraphConfig::load(&account_config)?;
                let client = graph::GraphClient::new_async(&graph_config).await?;
                let mut emails = client
                    .search_messages(&criteria, mailbox_name.as_deref(), limit)
                    .await?;
                sort_fetched_by_date(&mut emails);
                if emails.is_empty() {
                    println!("{}", "No results found".yellow());
                } else {
                    display_fetched_emails(&emails, full);
                }
            } else {
                let imap_config = ImapConfig::load(&account_config)?;

                if let Some(ref mb) = mailbox_name {
                    let mut emails =
                        fetch_emails(&imap_config, &criteria, mb, Some(limit)).await?;
                    sort_fetched_by_date(&mut emails);
                    if emails.is_empty() {
                        println!("{}", "No results found".yellow());
                    } else {
                        println!("{} result(s) in {}:\n", emails.len(), mb);
                        display_fetched_emails(&emails, full);
                    }
                } else {
                    // Search all configured mailboxes
                    let configured = all_configured_mailboxes(&account_config);
                    let mut session = imap_client::open_imap_session(&imap_config).await?;
                    let per_mb = (limit / configured.len().max(1)).max(5);
                    let mut total = 0usize;
                    let mut all_emails: Vec<FetchedEmail> = Vec::new();

                    for (role, mapping) in &configured {
                        if total >= limit {
                            break;
                        }
                        let budget = per_mb.min(limit - total);
                        match imap_client::fetch_emails_on_session(
                            &mut session,
                            &criteria,
                            &mapping.server,
                            Some(budget),
                        )
                        .await
                        {
                            Ok(emails) if !emails.is_empty() => {
                                total += emails.len();
                                all_emails.extend(emails);
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!(
                                    "{} Search in {} failed: {}",
                                    "\u{26a0}".yellow(),
                                    role,
                                    e
                                );
                            }
                        }
                    }
                    session.logout().await.ok();

                    sort_fetched_by_date(&mut all_emails);
                    if all_emails.is_empty() {
                        println!("{}", "No results found".yellow());
                    } else {
                        display_fetched_emails(&all_emails, full);
                    }
                }
            }
        }

        Some(Commands::Contacts { action }) => {
            match action {
                ContactsAction::Search { query, parsable, limit, account } => {
                    let acct = account.or_else(|| cli.account.clone());
                    email::contacts_cmd::handle_search(
                        &global_config,
                        query,
                        parsable,
                        limit,
                        acct,
                    )?;
                }
                ContactsAction::Rebuild { account } => {
                    let acct = account.or_else(|| cli.account.clone());
                    email::contacts_cmd::handle_rebuild(&global_config, acct)?;
                }
                ContactsAction::Stats { account } => {
                    let acct = account.or_else(|| cli.account.clone());
                    email::contacts_cmd::handle_stats(&global_config, acct)?;
                }
            }
        }

        Some(Commands::Calendar { action }) => match action {
            CalendarAction::Rebuild { account } => {
                let acct = account.or_else(|| cli.account.clone());
                email::calendar_cmd::handle_rebuild(&global_config, acct)?;
            }
        },

        Some(Commands::Outbox { action }) => {
            cmd_outbox(&account_config, action).await?;
        }

        Some(Commands::DumpKeys { json }) => {
            if json {
                print!("{}", email::tui::dump_keys_json());
            } else {
                print!("{}", email::tui::dump_keys());
            }
        }

        Some(Commands::DumpMailbox { json: _, mailbox }) => {
            // `--json` is `required = true`, so the format is already pinned.
            let accounts: Vec<email::config::AccountConfig> = match cli.account {
                Some(ref name) => global_config
                    .accounts
                    .iter()
                    .filter(|a| a.name == *name)
                    .cloned()
                    .collect(),
                None => global_config.accounts.clone(),
            };
            if accounts.is_empty() {
                return Err(anyhow!("No account to dump (check `mp config show`)"));
            }
            let filter = mailbox.unwrap_or_default();
            let records = email::dump::collect_records(&accounts, &filter);
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(email::dump::to_ndjson(&records).as_bytes())?;
            out.flush()?;
        }

        Some(Commands::Config { action }) => {
            match action {
                ConfigAction::Init => cmd_config_init()?,
                ConfigAction::Show => cmd_config_show()?,
                ConfigAction::SetPassword { which, account } => {
                    let acct_name = account
                        .or_else(|| cli.account.clone())
                        .or_else(|| global_config.accounts.first().map(|a| a.name.clone()))
                        .unwrap_or_else(|| "main".to_string());
                    cmd_set_password(&which, &acct_name)?;
                }
                ConfigAction::AddAccount => cmd_config_add_account()?,
                ConfigAction::Oauth2Login { account } => {
                    let acct_name = account
                        .or_else(|| cli.account.clone());
                    cmd_oauth2_login(acct_name.as_deref()).await?;
                }

                ConfigAction::ResetSecrets => cmd_reset_secrets()?,
                ConfigAction::Path => cmd_config_path(),
            }
        }

        None => {
            if let Some(file) = cli.file {
                // Preview mode (dry run)
                let draft = parse_email_draft(&file)?;
                preview_draft(
                    &draft,
                    &smtp_config,
                    &global_config.email,
                    signature_content.as_deref(),
                    true,
                )?;
            } else {
                // No file, no subcommand -> launch TUI
                email::tui::run()?;
            }
        }
    }

    Ok(())
}
