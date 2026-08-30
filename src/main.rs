use mailypoppins::types::*;
use mailypoppins::config::*;
use mailypoppins::parse::*;
use mailypoppins::imap_client::{self, *};
use mailypoppins::draft::*;
use mailypoppins::send::*;
use mailypoppins::config_cmd::*;
use mailypoppins::graph;
use mailypoppins::ops::{Backend, ServerOp};
use mailypoppins::pending_ops;
use mailypoppins::selector::{Namespace, Selector};
use mailypoppins::store::read::materialise_attachments;
use mailypoppins::store::Store;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use colored::*;
use log::{error, info, warn};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "mailypoppins")]
#[command(about = "A terminal email client: Markdown drafts on disk, received mail in a local store")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Draft selector to preview (dry-run mode)
    #[arg(value_name = "SELECTOR")]
    selector: Option<String>,

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

/// The long help for `mp search`: one grammar, every backend, with examples.
const SEARCH_LONG_ABOUT: &str = "\
Search emails with one grammar that every backend speaks.

Fields (combine freely, implicit AND between them):
  from:  to:  cc:  subject:  body:  filename:   match a field
  has:attachment                                 only mail with an attachment
  before:YYYY-MM-DD  after:YYYY-MM-DD             a date range (since: aliases after:)
  in:MAILBOX                                      scope to one mailbox
  message-id:<id>                                 exact Message-ID lookup

Operators:
  \"a phrase\"        a quoted phrase is one term
  a OR b            either term
  (a OR b)          a parenthesised OR group, AND-ed with the rest

Examples:
  mp search 'from:boss@corp.com (invoice OR receipt) has:attachment'
  mp search --from boss@corp.com --has-attachment 'invoice OR receipt'
  mp search 'subject:\"quarterly report\" after:2026-01-01 before:2026-07-01'
  mp search --local 'from:ada ledger'

Backend honesty: on Gmail and Exchange every term runs server-side. On plain
IMAP has:attachment has no server key, so it is answered from the local store
(synced mail only) and the run prints a warning. filename: is Gmail/Exchange/
--local only.";

#[derive(Subcommand)]
enum Commands {
    /// Send a single approved email, or (with --invite) a calendar invitation
    Send {
        /// Draft selector: mp://<account>/drafts/<id>, drafts/<id> or <id>
        /// (omit when using --invite)
        #[arg(value_name = "SELECTOR")]
        selector: Option<String>,
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
    /// Send every approved draft of the account
    SendApproved {
        /// Send the approved drafts of every configured account
        #[arg(long)]
        all_accounts: bool,
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// List the account's drafts from the drafts index
    List {
        /// Only list drafts with this status
        #[arg(long, value_name = "STATUS")]
        status: Option<DraftStatusFilter>,
    },
    /// Validate a draft's frontmatter (default: every draft of the account)
    Validate {
        /// Draft selector: mp://<account>/drafts/<id>, drafts/<id> or <id>
        #[arg(value_name = "SELECTOR")]
        selector: Option<String>,
    },
    /// Mark a draft as approved
    MarkApproved {
        /// Draft selector: mp://<account>/drafts/<id>, drafts/<id> or <id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
    },
    /// Demote an approved draft back to `draft` status (reverse of `mark-approved`)
    MarkDraft {
        /// Draft selector: mp://<account>/drafts/<id>, drafts/<id> or <id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
    },
    /// Create a new email draft from template and print its selector
    New {
        /// Name for the new draft file
        name: String,
    },
    /// Print the filesystem path of a draft (the only selector-to-path edge)
    Path {
        /// Draft selector: mp://<account>/drafts/<id>, drafts/<id> or <id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
    },
    /// Open a draft in $EDITOR
    Edit {
        /// Draft selector: mp://<account>/drafts/<id>, drafts/<id> or <id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
    },
    /// Create a reply draft from a received email
    Reply {
        /// Received selector: mp://<account>/<mailbox>/<message-id>,
        /// <mailbox>/<message-id> or <message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Reply to all recipients
        #[arg(long)]
        all: bool,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Forward an email to new recipients
    Forward {
        /// Received selector: mp://<account>/<mailbox>/<message-id>,
        /// <mailbox>/<message-id> or <message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
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
        /// Sync every configured account (failures are named at the end;
        /// exit code 1 if any account failed)
        //
        // Conflicts with `-A/--account`: the two answer the same question, and
        // silently ignoring the selector is how a cron line ends up syncing
        // accounts it never named. A second doc-comment paragraph would turn
        // this into clap's long help and reformat the whole subcommand's
        // `--help`, so the rationale stays a plain comment.
        #[arg(long, conflicts_with = "account")]
        all_accounts: bool,
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
    /// Archive a received email (server + local)
    Archive {
        /// Received selector: mp://<account>/<mailbox>/<message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Delete a received email (server + local) or a local draft
    //
    // Kept to one doc line on purpose: a second paragraph flips clap into its
    // long-help layout and reformats the whole subcommand's `--help`, the same
    // footgun `Sync` guards against. The drafts vs received split and the
    // --force/--sent rules are carried by the argument help below.
    Delete {
        /// Received (mp://<acct>/<mbox>/<id>) or drafts (mp://<acct>/drafts/<id>) selector; omit with --sent
        #[arg(value_name = "SELECTOR", required_unless_present = "sent")]
        selector: Option<String>,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
        /// Delete an approved draft (a queued send) anyway
        #[arg(long)]
        force: bool,
        /// Clear every sent draft of the account (takes no selector)
        #[arg(long, conflicts_with_all = ["selector", "mailbox", "force"])]
        sent: bool,
    },
    /// Open a received email's attachment in the default application
    Open {
        /// Received selector: mp://<account>/<mailbox>/<message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Save a received email's attachment(s) to a directory
    Save {
        /// Received selector: mp://<account>/<mailbox>/<message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Output directory (default: current directory)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Print one received message from the local store (offline)
    Show {
        /// Received selector: mp://<account>/<mailbox>/<message-id>,
        /// <mailbox>/<message-id> or <message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
        /// Emit one JSON object (headers, attachments and body) instead
        #[arg(long)]
        json: bool,
    },
    /// List received messages from the local store (offline)
    ListMessages {
        /// Mailbox to list (role, slug or sidebar label).
        /// Default: every mailbox of the account, grouped.
        #[arg(long)]
        mailbox: Option<String>,
        /// Max messages per mailbox listed (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
    },
    /// Search emails on the IMAP server, or locally with --local
    #[command(long_about = SEARCH_LONG_ABOUT)]
    Search {
        /// Search query. One grammar for every backend: fields from: to: cc:
        /// subject: body: filename:, the flag has:attachment, dates
        /// before:YYYY-MM-DD / after:YYYY-MM-DD (since: aliases after:),
        /// quoted "phrases", OR and (a OR b) groups, plus in: and message-id:.
        /// Combine with the flags below; they build the same query.
        #[arg(default_value = "")]
        query: String,
        /// Mailbox to search (default: all the account's mailboxes)
        #[arg(long)]
        mailbox: Option<String>,
        /// Match the sender (from:)
        #[arg(long)]
        from: Option<String>,
        /// Match a recipient (to:)
        #[arg(long)]
        to: Option<String>,
        /// Match a Cc recipient (cc:)
        #[arg(long)]
        cc: Option<String>,
        /// Match the subject (subject:)
        #[arg(long)]
        subject: Option<String>,
        /// Match the body (body:)
        #[arg(long)]
        body: Option<String>,
        /// Match an attachment filename (Gmail/Exchange/--local only)
        #[arg(long)]
        filename: Option<String>,
        /// Only mail carrying an attachment (has:attachment)
        #[arg(long)]
        has_attachment: bool,
        /// On or after this date, YYYY-MM-DD (after:)
        #[arg(long)]
        after: Option<String>,
        /// Strictly before this date, YYYY-MM-DD (before:)
        #[arg(long)]
        before: Option<String>,
        /// Max results (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Show full body instead of preview
        #[arg(long)]
        full: bool,
        /// Search the local store's full-text index instead of the server
        /// (offline, ranked, covers every synced mailbox at once)
        #[arg(long)]
        local: bool,
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
    /// Local store maintenance (retention garbage collection)
    Store {
        #[command(subcommand)]
        action: StoreAction,
    },
    /// Report what is left of the file-era `.md` tree, and import its drafts.
    ///
    /// Assigns an `id:` frontmatter field to any draft that has none, so a
    /// file-era draft becomes addressable by selector, then names the
    /// file-era mailbox directories nothing reads any more and prints the
    /// command that removes them. It deletes nothing itself. `--dry-run`
    /// writes not even the `id:` field.
    Cutover {
        /// Account name (default: all configured accounts)
        #[arg(long)]
        account: Option<String>,
        /// Report only; write nothing at all
        #[arg(long)]
        dry_run: bool,
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
    /// Dump message envelopes from the local message store as NDJSON.
    ///
    /// Offline: reads the local store only, never the network. One compact
    /// JSON object per line, with the fields account, mailbox, message_id,
    /// from, to, cc, subject, date_sort, flags, attachments (name + size) and
    /// invite. No filesystem paths appear in the output.
    ///
    /// Records are sorted by account, mailbox, date_sort, message_id and
    /// subject (the message's uid breaks remaining ties without being
    /// emitted), so two runs over an unchanged store are byte-identical.
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
/// appended on the next startup or sync. These are for the two cases it cannot
/// decide alone, a submission that died without a verdict and may or may not
/// have been delivered, and one that a recipient was refused (#0063), which
/// nothing but a human can close.
#[derive(Subcommand)]
enum OutboxAction {
    /// List every queued, retrying, failed or partly delivered submission
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

/// Local store maintenance (#0060). Today just the retention garbage
/// collector; the sweep that also runs automatically after every sync.
#[derive(Subcommand)]
enum StoreAction {
    /// Run the retention sweep now: evict cached blobs over the disk cap.
    ///
    /// The first over-cap run only warns and records a marker; run it again to
    /// evict. `--dry-run` prints what would go without touching anything. A run
    /// that would reclaim more than half the store's blob bytes is refused
    /// without `--force` (a fat-finger guard while on-demand re-fetch, #0085,
    /// does not yet exist).
    Gc {
        /// Print what would be evicted and stop; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Evict even when the plan would reclaim more than half the store.
        #[arg(long)]
        force: bool,
        /// Sweep every configured account rather than just the default / `-A`.
        #[arg(long)]
        all_accounts: bool,
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

/// RSVP actions for `mp invite <accept|tentative|decline> <selector>`.
/// Whole-series only (v1); the target is a received message whose store row
/// carries an `invite.ics` blob.
#[derive(Subcommand)]
enum InviteAction {
    /// Accept the invitation (PARTSTAT=ACCEPTED)
    Accept {
        /// Received selector of the invitation: mp://<account>/<mailbox>/<message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Tentatively accept the invitation (PARTSTAT=TENTATIVE)
    Tentative {
        /// Received selector of the invitation: mp://<account>/<mailbox>/<message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Decline the invitation (PARTSTAT=DECLINED)
    Decline {
        /// Received selector of the invitation: mp://<account>/<mailbox>/<message-id>
        #[arg(value_name = "SELECTOR")]
        selector: String,
        /// Mailbox to resolve the selector in
        #[arg(long)]
        mailbox: Option<String>,
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
    /// Rebuild the contact index from the local message store
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
    account_config: &mailypoppins::config::AccountConfig,
    action: OutboxAction,
) -> Result<()> {
    use mailypoppins::outbox::{self, OutboxState};

    let path = mailypoppins::config::store_path(&account_config.name);
    if !path.exists() {
        println!("  {} nothing has been queued for {} yet", "·".dimmed(), account_config.name);
        return Ok(());
    }
    let store = mailypoppins::store::Store::open(&path)?;
    let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);

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
                // A `done` row is only listed when it kept a note, which is
                // what a partial delivery leaves behind (#0063); calling that
                // `done` would bury the recipient who never got it.
                let partial = row.state == OutboxState::Done;
                let state = if partial {
                    "partial".to_string().yellow()
                } else {
                    match row.state {
                        OutboxState::Failed => row.state.to_string().red(),
                        OutboxState::PendingSend => row.state.to_string().yellow(),
                        _ => row.state.to_string().normal(),
                    }
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
                if let Some(envelope) = row.envelope.as_ref() {
                    for (addr, reason) in &envelope.rejected {
                        println!("        {} {addr} ({reason})", "never delivered to:".red());
                    }
                    let waiting = envelope.outstanding();
                    if !waiting.is_empty() && row.state != OutboxState::Done {
                        println!(
                            "        {} {}",
                            "still to deliver to:".dimmed(),
                            waiting
                                .iter()
                                .map(|(addr, _)| addr.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
                if let Some(err) = row.last_error.as_deref() {
                    let label = if partial { "outcome:" } else { "last error:" };
                    println!("        {} {err}", label.dimmed());
                }
            }
            let counts = outbox::counts(&store, &account_config.name)?;
            println!(
                "  {} {} working, {} failed, {} partly delivered",
                "\u{21bb}".dimmed(),
                counts.open,
                counts.failed,
                counts.partial
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
            let result = mailypoppins::send::resume_outbox(account_config).await;
            let store = mailypoppins::store::Store::open(&path)?;
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
/// Builds the `VEVENT` (via `mailypoppins::invite`), assembles the iMIP MIME tree
/// (via `build_draft_message(..., Some(ics))`) and submits it through the
/// durable outbox, which also files the local Sent copy so the #0027 receive
/// path (and #0030 reconciliation) can pick it up.
async fn run_send_invite(
    account_config: &mailypoppins::config::AccountConfig,
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
        mailypoppins::invite::resolve_times(start, args.end.as_deref(), args.duration.as_deref())?;

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

    let uid = mailypoppins::invite::generate_uid(&organizer);
    let spec = mailypoppins::invite::InviteSpec {
        uid: uid.clone(),
        organizer: organizer.clone(),
        attendees: attendees.clone(),
        summary: subject.to_string(),
        start: start_dt,
        end: end_dt,
        location: args.location.clone().filter(|s| !s.trim().is_empty()),
        description: args.description.clone().filter(|s| !s.trim().is_empty()),
    };
    let ics = mailypoppins::invite::build_invite_ics(&spec)?;

    // Human-readable body: reuse the description, else a short summary line.
    let body = args
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("You are invited to: {}", subject));

    // Build the event: frontmatter block from the same ICS (round-trips via #0027).
    let event = mailypoppins::calendar::parse_ics(ics.as_bytes())
        .map(|p| mailypoppins::calendar::event_frontmatter(&p));

    // Synthetic draft, used only to build the message: nothing is written to
    // disk on this path any more (the Sent copy is the outbox's, #0037).
    let sent_at = chrono::Utc::now();
    let draft_path = PathBuf::from(format!(
        "{}-invite-{}.md",
        sent_at.format("%Y%m%d-%H%M%S"),
        mailypoppins::parse::slugify_subject(subject)
    ));

    let draft = EmailDraft {
        path: draft_path.clone(),
        frontmatter: EmailFrontmatter {
            id: None,
            date: None,
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
            in_reply_to: None,
            forwarded_from: None,
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
    let built = mailypoppins::send::build_draft_message(
        &draft,
        &smtp_config.default_from,
        &global_config.email,
        signature,
        Some(&ics),
    )?;
    let report = mailypoppins::send::send_durably(&built, account_config, smtp_config).await?;
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

    mailypoppins::contacts::hooks::bump_after_send(account_config, &draft);

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

// ---------------------------------------------------------------------------
// The selector edge (#0050)
// ---------------------------------------------------------------------------

/// The store's mailbox key for the archive folder. `mp archive` is a move with
/// a fixed destination, exactly as the TUI frames it.
const ARCHIVE_MAILBOX: &str = "archive";

/// `--status` values for `mp list`. A closed set rather than a free string, so
/// a typo is a clap error instead of an empty listing.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum DraftStatusFilter {
    Draft,
    Approved,
    Sent,
}

impl DraftStatusFilter {
    fn as_str(self) -> &'static str {
        match self {
            DraftStatusFilter::Draft => "draft",
            DraftStatusFilter::Approved => "approved",
            DraftStatusFilter::Sent => "sent",
        }
    }
}

/// Open the account's store for a *received* lookup.
///
/// A missing file means the account has never synced, which is a different
/// answer from "no such message" and is worth saying: resolving a selector
/// against an empty index would otherwise report the message as unknown.
fn received_store(account: &str) -> Result<Store> {
    let path = mailypoppins::config::store_path(account);
    if !path.exists() {
        return Err(anyhow!(
            "{account} has no local store yet, so no received mail can be addressed; \
             run `mp sync` first"
        ));
    }
    Store::open(&path).with_context(|| format!("opening the store of {account}"))
}

/// Open the account's store and refresh its drafts index.
///
/// This is the engine-start refresh of #0050 scope item 5, paid by every
/// draft-facing command before it reads the table: a draft an agent wrote a
/// second ago is in the index by the time the command lists or resolves it.
fn drafts_store(account: &str) -> Result<Store> {
    Ok(drafts_store_reporting(account)?.0)
}

/// [`drafts_store`], additionally handing back the files the refresh skipped
/// for a parse failure, so `mp list` can name them after its listing instead
/// of letting a broken draft vanish from the output (#0080).
fn drafts_store_reporting(
    account: &str,
) -> Result<(Store, Vec<mailypoppins::store::drafts::SkippedDraft>)> {
    let store = Store::open(mailypoppins::config::store_path(account))
        .with_context(|| format!("opening the store of {account}"))?;
    let dir = mailypoppins::config::drafts_dir(account);
    let (_, collisions, skipped) =
        mailypoppins::store::drafts::refresh_reporting(&store, account, &dir)
            .with_context(|| format!("refreshing the drafts index of {account}"))?;
    // Two files claiming one id means one of them is unaddressable. The index
    // cannot decide which the user meant, so it says so rather than dropping
    // the loser in silence.
    for collision in &collisions {
        eprintln!("{} {collision}", "⚠".yellow());
    }
    Ok((store, skipped))
}

/// Print the warning block `mp list` shows after its listing when the refresh
/// skipped one or more drafts for a parse failure (#0080).
///
/// A skipped file is a draft the index cannot see: no `id:`, no row, absent
/// from the listing above. Naming it here, with its one-line parse error, is
/// what turns "my draft disappeared" into a fixable line. The exit code stays
/// 0: the listing itself succeeded, and the broken file is a warning about the
/// directory, not a failure of the command.
fn print_skipped_drafts(skipped: &[mailypoppins::store::drafts::SkippedDraft]) {
    if skipped.is_empty() {
        return;
    }
    let n = skipped.len();
    let noun = if n == 1 { "draft" } else { "drafts" };
    eprintln!(
        "\n{} {n} {noun} skipped (frontmatter would not parse; fix the YAML to list them):",
        "⚠".yellow()
    );
    for skip in skipped {
        eprintln!("  {} - {}", skip.path.display().to_string().yellow(), skip.error);
    }
}

/// Re-index the drafts directory after a command wrote a draft, so the next
/// reader (the TUI, `mp list`, the next command) sees it without waiting for
/// the one-second scan. Best-effort: the write already happened, and the scan
/// would pick it up anyway.
fn reindex_drafts(account: &str) {
    if let Err(e) = drafts_store(account) {
        warn!("could not refresh the drafts index of {account}: {e:#}");
    }
}

/// Resolve a draft selector to its indexed row plus the canonical selector.
fn resolve_draft_arg(
    store: &Store,
    selector: &str,
    account: &str,
) -> Result<(mailypoppins::store::drafts::DraftRow, Selector)> {
    let query = mailypoppins::selector::parse_in(selector, Namespace::Drafts, account, None)?;
    mailypoppins::selector::resolve_draft(store, &query)
}

/// Whether a `mp delete` argument names a draft rather than received mail.
///
/// Dispatch is on the selector shape (#0073 scope item 1), not a second
/// command: a fully qualified drafts selector carries the reserved `drafts`
/// mailbox segment, and `--mailbox drafts` names it beside an elided selector.
/// Anything else is received mail, whose namespace `resolve_received_arg`
/// enforces.
fn is_drafts_selector(selector: &str, mailbox: Option<&str>) -> Result<bool> {
    if mailbox == Some(mailypoppins::selector::DRAFTS_MAILBOX) {
        return Ok(true);
    }
    let parts = mailypoppins::selector::parse(selector)?;
    Ok(parts.mailbox.as_deref() == Some(mailypoppins::selector::DRAFTS_MAILBOX))
}

/// Resolve a received selector to its message row plus the canonical selector.
/// The mailbox key (`MailboxRole` id) a `--mailbox` argument names.
///
/// Matches a role id or a sidebar label case-insensitively, the same rule
/// `mp dump-mailbox` and `mp list-messages` apply, and an unknown name is an
/// error naming what it could have been rather than an empty result.
fn resolve_mailbox_key(account: &AccountConfig, want: &str) -> Result<String> {
    let mailboxes: Vec<_> = mailypoppins::tui::app::build_mailboxes(account)
        .into_iter()
        .filter(|m| m.id != mailypoppins::selector::DRAFTS_MAILBOX)
        .collect();
    if let Some(hit) = mailboxes
        .iter()
        .find(|m| want.eq_ignore_ascii_case(&m.id) || want.eq_ignore_ascii_case(&m.label))
    {
        return Ok(hit.id.clone());
    }
    let known = mailboxes
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!(
        "'{want}' is not a mailbox of {} (known: {known})",
        account.name
    ))
}

/// The `(label, total, first `limit` rows)` groups `mp list-messages` prints.
///
/// One group per configured mailbox, in sidebar order, so a whole-account
/// listing reads like the sidebar rather than like a merged stream; the drafts
/// pseudo-mailbox is skipped, because it is local truth and `mp list` owns it.
/// The limit is per mailbox for the same reason: a shared budget would let a
/// busy inbox hide every other mailbox entirely.
///
/// `--mailbox` matches a role id or a sidebar label case-insensitively, the
/// same rule `mp dump-mailbox` applies, and an unknown name is an error naming
/// what it could have been rather than an empty listing.
fn list_message_groups(
    store: &Store,
    account: &AccountConfig,
    mailbox: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, usize, Vec<mailypoppins::store::read::MessageRow>)>> {
    let mailboxes: Vec<_> = mailypoppins::tui::app::build_mailboxes(account)
        .into_iter()
        .filter(|m| m.id != mailypoppins::selector::DRAFTS_MAILBOX)
        .collect();
    let selected: Vec<_> = match mailbox {
        Some(want) => {
            let hit: Vec<_> = mailboxes
                .iter()
                .filter(|m| want.eq_ignore_ascii_case(&m.id) || want.eq_ignore_ascii_case(&m.label))
                .cloned()
                .collect();
            if hit.is_empty() {
                let known = mailboxes
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(anyhow!(
                    "'{want}' is not a mailbox of {} (known: {known})",
                    account.name
                ));
            }
            hit
        }
        None => mailboxes,
    };

    let mut groups = Vec::new();
    for info in selected {
        let mut rows = mailypoppins::store::read::list_mailbox(store, &account.name, &info.id)?;
        let total = rows.len();
        rows.truncate(limit);
        groups.push((info.label.clone(), total, rows));
    }
    Ok(groups)
}

fn resolve_received_arg(
    store: &Store,
    selector: &str,
    account: &str,
    mailbox: Option<&str>,
) -> Result<(mailypoppins::store::read::MessageRow, Selector)> {
    let query = mailypoppins::selector::parse_in(selector, Namespace::Received, account, mailbox)?;
    mailypoppins::selector::resolve_received(store, &query)
}

/// The account a selector operates on: its own `mp://<account>/…` segment when
/// present, otherwise the `-A`/default account already resolved. The selector's
/// account overrides the flag because naming it in the selector is the more
/// specific statement, exactly as `parse_in` lets it override `--mailbox`.
///
/// Every selector command must call this *before* opening a store or loading a
/// transport, so a cross-account selector opens the right account's store and
/// server credentials instead of resolving against the default and reporting a
/// wrong-store miss (the #0073 follow-up bug). A selector naming an
/// unconfigured account fails here, loudly, rather than as a phantom miss.
fn account_for_selector(
    selector: &str,
    default: &AccountConfig,
    global: &GlobalConfig,
) -> Result<AccountConfig> {
    let parts = mailypoppins::selector::parse(selector)?;
    match parts.account {
        Some(name) if name != default.name => global
            .accounts
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| {
                let known = global
                    .accounts
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow!(
                    "selector names account '{name}', which is not configured (known: {})",
                    if known.is_empty() { "none" } else { known.as_str() }
                )
            }),
        _ => Ok(default.clone()),
    }
}

/// Guard for a command whose transport is loaded before the selector is parsed
/// (`mp send`, `mp invite`): the SMTP/Graph credentials and the signature
/// already belong to `bound`, so a cross-account selector cannot be honoured
/// without reloading them. Rather than send from the wrong account silently, or
/// widen the send path to rebind mid-command, fail loudly and point at `-A`.
fn ensure_selector_account_matches(selector: &str, bound: &AccountConfig) -> Result<()> {
    let parts = mailypoppins::selector::parse(selector)?;
    if let Some(name) = parts.account {
        if name != bound.name {
            bail!(
                "selector names account '{name}', but this command is bound to '{}' \
                 (its transport is already configured); re-run with `-A {name}`",
                bound.name
            );
        }
    }
    Ok(())
}

/// The account's Markdown signature for a draft body (#0099), honouring the
/// global `--no-signature` / `--signature <name>` flags and the
/// `include_signature` config. Returns `None` (no signature block) when the
/// user opted out or nothing is configured.
fn resolve_body_signature(
    account: &AccountConfig,
    no_signature: bool,
    signature_name: Option<&str>,
    email: &EmailSettings,
) -> Option<String> {
    if no_signature {
        None
    } else if email.include_signature {
        resolve_signature_markdown(account, signature_name)
    } else {
        // include_signature is off, but an explicit --signature still selects one.
        signature_name.and_then(|s| resolve_signature_markdown(account, Some(s)))
    }
}

/// The mailboxes an account is configured for, as a human-readable list for
/// the error a `--mailbox` typo produces.
fn configured_mailbox_names(account: &AccountConfig) -> String {
    let names: Vec<String> = all_configured_mailboxes(account)
        .iter()
        .map(|(role, mapping)| {
            if role.as_str().eq_ignore_ascii_case(&mapping.server) {
                mapping.server.clone()
            } else {
                format!("{} ({})", role.as_str(), mapping.server)
            }
        })
        .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// One account's `mp sync`: the outbox drain, the sync itself, the contacts
/// hook, and the per-account summary lines.
///
/// Factored out of the `Sync` arm so `--all-accounts` is a loop over exactly
/// the single-account body (#0071). `Err` is an account-level failure, a
/// refused login above all; the caller names it and keeps going.
async fn sync_one_account(
    account_config: &AccountConfig,
    limit: usize,
    mailbox: Option<&[String]>,
    dry_run: bool,
) -> Result<()> {
    let targets: Vec<imap_client::SyncTarget> = if let Some(user_mailboxes) = mailbox {
        // Both halves of a target come from one configured mapping: building
        // the role from the typed string files an extra mailbox's rows under
        // `projects` while the rest of the product reads `Projects` (#0064).
        user_mailboxes
            .iter()
            .map(|mb| {
                let (role, server_name) = find_sync_target(account_config, mb)
                    .ok_or_else(|| {
                        anyhow!(
                            "account '{}' has no mailbox '{}' configured; it knows {}",
                            account_config.name,
                            mb,
                            configured_mailbox_names(account_config)
                        )
                    })?;
                Ok(imap_client::SyncTarget { role, server_name })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        all_configured_mailboxes(account_config)
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
        let drained = mailypoppins::send::resume_outbox(account_config).await;
        if drained.completed > 0 || drained.still_open > 0 {
            println!(
                "  {} outbox: {} completed, {} still pending",
                "↻".dimmed(),
                drained.completed,
                drained.still_open + drained.awaiting_submission
            );
        }

        // The sync tick is also the mutation queue's drain tick (#0039): a
        // move, delete or flag toggle enqueued locally (by the TUI, or by a
        // CLI invocation that crashed before its op ran) is retired here,
        // before this sync reads the mailboxes those ops changed. Nothing is
        // drained when nothing is owed, so a clean account adds no traffic.
        if let Some(ops) = pending_ops::resume_account(account_config).await? {
            if ops.completed > 0 || ops.failed > 0 {
                println!(
                    "  {} mutations: {} completed, {} failed",
                    "↻".dimmed(),
                    ops.completed,
                    ops.failed
                );
            }
        }
    }

    let result = if account_config.auth_method == AuthMethod::Graph {
        let graph_config = GraphConfig::load(account_config)?;
        graph::sync_mailboxes_graph(
            &graph_config,
            &account_config.name,
            &targets,
            limit,
            dry_run,
        )
        .await?
    } else {
        let imap_config = ImapConfig::load(account_config)?;
        sync_mailboxes(&imap_config, &account_config.name, &targets, limit, dry_run).await?
    };

    if !dry_run {
        // Incremental contacts-index update (best-effort).
        mailypoppins::contacts::hooks::bump_after_sync(account_config, &result.fresh_observations);
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

    if result.flags_updated > 0 {
        println!(
            "{} {}Status updated on {} message(s)",
            "ℹ".blue(),
            prefix,
            result.flags_updated,
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
    if result.pruned > 0 {
        println!(
            "{} {}{} message(s) left their mailbox on the server",
            "ℹ".blue(),
            prefix,
            result.pruned,
        );
    }
    if result.prunes_deferred > 0 {
        println!(
            "{} {}{} removal(s) held back: this pass did not see every message, \
             run a full sync to apply them",
            "⚠".yellow(),
            prefix,
            result.prunes_deferred,
        );
    }

    Ok(())
}

/// Run the retention sweep for one account and report it, the body of
/// `mp store gc`.
///
/// A store file that does not exist yet has nothing to sweep, which is the
/// common case for a freshly configured or drafts-only account.
fn run_store_gc(
    global_config: &GlobalConfig,
    account: &AccountConfig,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let path = mailypoppins::config::store_path(&account.name);
    if !path.exists() {
        println!(
            "  {} {} has no store yet; nothing to sweep",
            "\u{b7}".dimmed(),
            account.name
        );
        return Ok(());
    }
    let policy = mailypoppins::config::retention_for(global_config, account)?;
    let store = mailypoppins::store::Store::open(&path)?;
    let blobs = mailypoppins::store::BlobStore::for_account(&account.name);
    let outcome = mailypoppins::store::sweep::sweep(
        &store,
        &blobs,
        &policy,
        mailypoppins::store::sweep::SweepOptions { dry_run, force },
    )?;
    report_sweep_outcome(&account.name, &outcome, true);
    Ok(())
}

/// Run the automatic post-sync retention sweep for one account, best-effort.
///
/// A sweep failure never fails the sync it rides on: the store is a cache and
/// an unswept cache is merely too big, not broken, so the error is logged and
/// the sync still reports success.
fn retention_sweep_after_sync(global_config: &GlobalConfig, account: &AccountConfig) {
    let policy = match mailypoppins::config::retention_for(global_config, account) {
        Ok(p) => p,
        Err(e) => {
            warn!("[retention] skipping sweep for '{}': {e:#}", account.name);
            return;
        }
    };
    let path = mailypoppins::config::store_path(&account.name);
    if !path.exists() {
        return;
    }
    let store = match mailypoppins::store::Store::open(&path) {
        Ok(s) => s,
        Err(e) => {
            warn!("[retention] sweep could not open the store of '{}': {e:#}", account.name);
            return;
        }
    };
    let blobs = mailypoppins::store::BlobStore::for_account(&account.name);
    match mailypoppins::store::sweep::sweep(
        &store,
        &blobs,
        &policy,
        mailypoppins::store::sweep::SweepOptions::default(),
    ) {
        Ok(outcome) => report_sweep_outcome(&account.name, &outcome, false),
        Err(e) => warn!("[retention] sweep of '{}' failed: {e:#}", account.name),
    }
}

/// Print a retention sweep outcome. `manual` distinguishes `mp store gc` (which
/// speaks even when there is nothing to do) from the post-sync sweep (which
/// stays quiet unless it warned, evicted, or cleared a stale marker).
fn report_sweep_outcome(
    account: &str,
    outcome: &mailypoppins::store::sweep::SweepOutcome,
    manual: bool,
) {
    use mailypoppins::store::sweep::{human_bytes, SweepDecision};
    let at = human_bytes(outcome.before_bytes);
    let cap = human_bytes(outcome.cap_bytes);
    match &outcome.decision {
        SweepDecision::UnderCap { cleared_marker } => {
            if *cleared_marker {
                println!(
                    "{} {}: store back under budget ({at} / {cap}); over-cap marker cleared",
                    "\u{2713}".green(),
                    account
                );
            } else if manual {
                println!(
                    "{} {}: store at {at} / cap {cap}, under budget (nothing to evict)",
                    "\u{2713}".green(),
                    account
                );
            }
        }
        SweepDecision::WarnedFirstBreach => {
            println!(
                "{} {}: store at {at} / cap {cap}, will prune on next run",
                "\u{26a0}".yellow(),
                account
            );
        }
        SweepDecision::RefusedTooMuch { would_evict_bytes } => {
            println!(
                "{} {}: pruning would reclaim {} ({} > half of {at}); refusing. \
                 Re-run `mp store gc --force` to proceed \
                 (on-demand re-fetch of an evicted body is not built yet, #0085).",
                "\u{26a0}".yellow(),
                account,
                human_bytes(*would_evict_bytes),
                human_bytes(*would_evict_bytes),
            );
        }
        SweepDecision::Evicted => {
            let prefix = if outcome.dry_run { "[dry-run] " } else { "" };
            let now = human_bytes(outcome.after_bytes);
            println!(
                "{} {}{}: {} {} blob(s) ({}), store {} at {now} / cap {cap}",
                "\u{2713}".green(),
                prefix,
                account,
                if outcome.dry_run { "would evict" } else { "evicted" },
                outcome.evicted.len(),
                human_bytes(outcome.reclaimed_bytes()),
                if outcome.dry_run { "would be" } else { "now" },
            );
            if outcome.dry_run {
                for e in &outcome.evicted {
                    let horizon = if e.past_horizon { " [past horizon]" } else { "" };
                    println!(
                        "    {} {} {} ({}){}",
                        "\u{2192}".dimmed(),
                        &e.hash[..12],
                        e.kind.as_str(),
                        human_bytes(e.size),
                        horizon,
                    );
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();

    info!("mailypoppins started: {:?}", std::env::args().collect::<Vec<_>>());

    // Move a pre-#0022 ~/.config/email directory before anything reads config
    // or secrets out of it. A failure here is fatal: the alternative is running
    // against an empty config and silently re-prompting for every password.
    if let Err(e) = mailypoppins::config::migrate_legacy_config_dir() {
        eprintln!("{} {}", "\u{2717}".red(), e);
        std::process::exit(1);
    }

    // Load global config from ~/.config/mailypoppins/config.toml
    let global_config = load_global_config().unwrap_or_else(|e| {
        eprintln!("{} {}", "⚠".yellow(), e);
        eprintln!("  Some commands may not work without proper configuration.");
        GlobalConfig::default()
    });

    // Initialize the secrets backend (encrypted file by default, or OS keyring
    // if the user opted in via `secrets_backend = "keyring"` in config.toml).
    if let Err(e) = mailypoppins::config::init_secrets_backend(&global_config) {
        match &e {
            mailypoppins::secrets::SecretsError::NotInitialized(_) => {
                // Empty store at startup is fine -- `mp config init` or
                // `set-password` will populate it. The actual missing-key
                // error surfaces later when `SmtpConfig::load` is called.
            }
            mailypoppins::secrets::SecretsError::Undecryptable(_, _) => {
                eprintln!("{} {}", "\u{2717}".red(), e);
                std::process::exit(1);
            }
            mailypoppins::secrets::SecretsError::Other(err) => {
                eprintln!("{} Could not initialize secrets backend: {}", "\u{26a0}".yellow(), err);
            }
        }
    }

    // Resolve which account to use
    let account_config: mailypoppins::config::AccountConfig = if let Some(ref name) = cli.account {
        global_config.accounts.iter()
            .find(|a| a.name == *name)
            .cloned()
            .unwrap_or_else(|| {
                eprintln!("{} Account '{}' not found in config", "⚠".yellow(), name);
                mailypoppins::config::AccountConfig::default()
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
            auth_method: mailypoppins::config::AuthMethod::Password,
        }
    });

    // Signature for direct sends and invites, which have no editable draft to
    // carry it in the body (#0099). Draft sends (`mp send`) take None: their
    // signature was appended to the body at `mp reply`/`mp forward`/`mp new`
    // time.
    let signature_content: Option<String> = resolve_body_signature(
        &account_config,
        cli.no_signature,
        cli.signature.as_deref(),
        &global_config.email,
    );

    match cli.command {
        Some(Commands::Send {
            selector,
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

            let selector = selector.ok_or_else(|| {
                anyhow!(
                    "`mp send` needs a draft selector, or use `--invite` to send a calendar \
                     invitation"
                )
            })?;
            // Transport and signature are already bound to this account; a
            // cross-account selector fails loudly rather than sending from the
            // wrong account (see `ensure_selector_account_matches`).
            ensure_selector_account_matches(&selector, &account_config)?;
            let store = drafts_store(&account_config.name)?;
            let (row, canonical) = resolve_draft_arg(&store, &selector, &account_config.name)?;
            drop(store);
            println!("{} {}", "\u{2192}".dimmed(), canonical);
            let draft = parse_email_draft(&row.path)?;
            validate_draft(&draft)?;

            // Which transport carries it is the account's business, and the
            // send itself is `send::send_draft`'s (#0058). What stays here is
            // the CLI's own half: the preview, the confirmation and the
            // wording of what happened.
            let is_graph = account_config.auth_method == AuthMethod::Graph;
            let graph = if is_graph {
                Some(GraphConfig::load(&account_config)?)
            } else {
                None
            };

            if is_graph {
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
            } else {
                // The signature lives in the draft body since #0099; a
                // send-time signature line would advertise an injection that
                // no longer happens.
                preview_draft(&draft, &smtp_config, &global_config.email, None, false)?;
            }

            if !yes && !prompt_confirmation("Send this email?") {
                println!("Cancelled.");
                return Ok(());
            }

            if is_graph {
                println!("Sending via Graph API...");
            } else {
                println!("Sending email...");
            }

            let ctx = mailypoppins::send::SendContext {
                graph,
                smtp: (!is_graph).then(|| smtp_config.clone()),
                account: account_config.clone(),
                email_settings: global_config.email.clone(),
                // The signature is already in the draft body (#0099).
                signature: None,
            };
            let sent = mailypoppins::send::send_draft(&draft, &ctx).await?;
            let report = &sent.report;
            let send_result = &report.send_result;

            // The message is out; a bookkeeping failure here is a warning,
            // not a failed send (the log line is `send_draft`'s).
            let retire_warning = || {
                if let Some(e) = sent.settle_error.as_ref() {
                    println!("{} (sent but failed to retire draft: {})", "\u{26a0}".yellow(), e);
                }
            };

            if is_graph {
                if !send_result.any_succeeded() {
                    return Err(anyhow!(
                        "{}",
                        send_result
                            .failed()
                            .first()
                            .and_then(|r| r.error.clone())
                            .unwrap_or_else(|| "Graph send failed".to_string())
                    ));
                }
                retire_warning();
                info!("Email sent via Graph and marked as sent: {}", draft.path.display());
                reindex_drafts(&account_config.name);
                println!(
                    "{} Email sent successfully via Graph API [{}]",
                    "\u{2713}".green().bold(),
                    report.status_line()
                );
            } else {
                // Display per-recipient results
                for r in &send_result.succeeded() {
                    println!(
                        "  {} {} ({})",
                        "\u{2713}".green(),
                        r.address,
                        r.role
                    );
                }
                for r in &send_result.failed() {
                    println!(
                        "  {} {} ({}): {}",
                        "\u{2717}".red(),
                        r.address,
                        r.role,
                        r.error.as_deref().unwrap_or("unknown error")
                    );
                }

                if send_result.all_succeeded() {
                    retire_warning();
                    info!("Email marked as sent: {}", draft.path.display());
                    reindex_drafts(&account_config.name);

                    println!(
                        "{} Email sent successfully to all {} recipient(s) [{}]",
                        "\u{2713}".green().bold(),
                        send_result.results.len(),
                        report.status_line()
                    );
                } else if send_result.any_succeeded() {
                    retire_warning();
                    warn!(
                        "Partial send: {} succeeded, {} failed for {}",
                        send_result.succeeded().len(),
                        send_result.failed().len(),
                        draft.path.display()
                    );
                    reindex_drafts(&account_config.name);

                    println!(
                        "{} Partial send: {} succeeded, {} failed [{}] (marked as sent -- see logs for details)",
                        "\u{26a0}".yellow().bold(),
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

        Some(Commands::SendApproved { all_accounts, yes }) => {
            // `--all-accounts` is a loop over the same body rather than a
            // second code path: each account resolves its own SMTP config,
            // signature and drafts index, so the per-account send is exactly
            // what the single-account form does.
            let accounts: Vec<mailypoppins::config::AccountConfig> = if all_accounts {
                global_config.accounts.clone()
            } else {
                vec![account_config.clone()]
            };
            if accounts.is_empty() {
                return Err(anyhow!("No account configured (check `mp config show`)"));
            }

            for account_config in accounts {
            let smtp_config = SmtpConfig::load(&account_config).unwrap_or_else(|e| {
                eprintln!("{} Could not load SMTP config: {}", "\u{26a0}".yellow(), e);
                smtp_config.clone()
            });
            let store = drafts_store(&account_config.name)?;
            let rows =
                mailypoppins::store::drafts::list(&store, &account_config.name, Some("approved"))?;
            drop(store);
            let drafts: Vec<EmailDraft> = rows
                .iter()
                .filter_map(|row| match parse_email_draft(&row.path) {
                    Ok(draft) => Some(draft),
                    Err(e) => {
                        eprintln!("{} Skipping {}: {}", "\u{26a0}".yellow(), row.id, e);
                        None
                    }
                })
                .collect();

            if drafts.is_empty() {
                println!("No approved drafts for {}", account_config.name);
                continue;
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

            if !yes
                && !prompt_confirmation(&format!(
                    "\nSend all {} emails for {}?",
                    drafts.len(),
                    account_config.name
                ))
            {
                println!("Cancelled.");
                continue;
            }

            let mut sent_count = 0;
            let mut failed_count = 0;

            // One transport for the whole batch, one send implementation for
            // every draft in it (#0058): what this loop owns is the running
            // tally and the per-draft line.
            let is_graph = account_config.auth_method == AuthMethod::Graph;
            let ctx = mailypoppins::send::SendContext {
                graph: if is_graph {
                    Some(GraphConfig::load(&account_config)?)
                } else {
                    None
                },
                smtp: (!is_graph).then(|| smtp_config.clone()),
                account: account_config.clone(),
                email_settings: global_config.email.clone(),
                // Signatures live in the draft body now (#0099).
                signature: None,
            };

            for draft in drafts {
                print!("Sending to {}... ", draft.frontmatter.to.as_deref().unwrap_or("(bcc only)"));
                io::stdout().flush()?;

                let sent = match mailypoppins::send::send_draft(&draft, &ctx).await {
                    Ok(sent) => sent,
                    Err(e) => {
                        println!("{} {}", "\u{2717}".red(), e);
                        error!("Send failed for {}: {e:#}", draft.path.display());
                        failed_count += 1;
                        continue;
                    }
                };
                let send_result = &sent.report.send_result;

                if send_result.any_succeeded() {
                    if let Some(e) = sent.settle_error.as_ref() {
                        println!("{} (sent but failed to update status: {})", "\u{26a0}".yellow(), e);
                    } else if send_result.all_succeeded() {
                        println!("{} [{}]", "\u{2713}".green(), sent.report.status_line());
                    } else {
                        println!(
                            "{} (partial: {}/{} recipients) [{}]",
                            "\u{26a0}".yellow(),
                            send_result.succeeded().len(),
                            send_result.results.len(),
                            sent.report.status_line()
                        );
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
                    match send_result.failed().first().and_then(|r| r.error.clone()) {
                        Some(reason) => println!(
                            "{} all recipients failed: {} [{}]",
                            "\u{2717}".red(),
                            reason,
                            sent.report.status_line()
                        ),
                        None => println!(
                            "{} all recipients failed [{}]",
                            "\u{2717}".red(),
                            sent.report.status_line()
                        ),
                    }
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

            reindex_drafts(&account_config.name);
            println!(
                "\n{} {}: {} sent, {} failed",
                "Summary".bold(),
                account_config.name,
                sent_count.to_string().green(),
                failed_count.to_string().red()
            );
            }
        }

        Some(Commands::List { status }) => {
            let (store, skipped) = drafts_store_reporting(&account_config.name)?;
            let rows = mailypoppins::store::drafts::list(
                &store,
                &account_config.name,
                status.map(DraftStatusFilter::as_str),
            )?;
            if rows.is_empty() && skipped.is_empty() {
                println!("No drafts for {}", account_config.name);
                return Ok(());
            }
            if rows.is_empty() {
                println!("No listable drafts for {}", account_config.name);
                print_skipped_drafts(&skipped);
                return Ok(());
            }

            println!("\n{}", "Drafts:".bold());
            println!("{}", "\u{2500}".repeat(72));
            let mut counts: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for row in &rows {
                *counts.entry(row.status.clone()).or_default() += 1;
                let status_colored = match row.status.as_str() {
                    "draft" => "draft".yellow(),
                    "approved" => "approved".green(),
                    "sent" => "sent".dimmed(),
                    other => other.normal(),
                };
                println!(
                    "[{}] {} \u{2192} {}",
                    status_colored,
                    Selector::for_draft(&account_config.name, &row.id),
                    row.to.as_deref().unwrap_or("(bcc only)")
                );
                if let Some(subject) = row.subject.as_deref() {
                    println!("      {}", subject.dimmed());
                }
            }
            println!("{}", "\u{2500}".repeat(72));
            let summary = counts
                .iter()
                .map(|(status, n)| format!("{status}: {n}"))
                .collect::<Vec<_>>()
                .join(" | ");
            println!("Total: {} | {}", rows.len(), summary);
            print_skipped_drafts(&skipped);
        }

        Some(Commands::Validate { selector }) => {
            let account_config = match &selector {
                Some(sel) => account_for_selector(sel, &account_config, &global_config)?,
                None => account_config.clone(),
            };
            let store = drafts_store(&account_config.name)?;
            let targets: Vec<(Selector, PathBuf)> = match selector {
                Some(ref sel) => {
                    let (row, canonical) = resolve_draft_arg(&store, sel, &account_config.name)?;
                    vec![(canonical, row.path)]
                }
                None => mailypoppins::store::drafts::list(&store, &account_config.name, None)?
                    .into_iter()
                    .map(|row| (Selector::for_draft(&account_config.name, &row.id), row.path))
                    .collect(),
            };
            if targets.is_empty() {
                println!("No drafts to validate for {}", account_config.name);
                return Ok(());
            }

            let mut valid_count = 0;
            let mut invalid_count = 0;
            for (canonical, path) in &targets {
                match parse_email_draft(path).and_then(|draft| validate_draft(&draft)) {
                    Ok(warnings) => {
                        print!("{} {}", "\u{2713}".green(), canonical);
                        if !warnings.is_empty() {
                            print!(" ({})", warnings.join(", ").yellow());
                        }
                        println!();
                        valid_count += 1;
                    }
                    Err(e) => {
                        println!("{} {} - {}", "\u{2717}".red(), canonical, e);
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

        Some(Commands::MarkApproved { selector }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = drafts_store(&account_config.name)?;
            let (row, canonical) = resolve_draft_arg(&store, &selector, &account_config.name)?;
            drop(store);
            let msg = mark_as_approved(&row.path)?;
            reindex_drafts(&account_config.name);
            if msg.starts_with("Already") {
                println!("{} {} is already approved", "\u{2139}".blue(), canonical);
            } else {
                println!("{} approved {}", "\u{2713}".green(), canonical);
            }
        }

        Some(Commands::MarkDraft { selector }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = drafts_store(&account_config.name)?;
            let (row, canonical) = resolve_draft_arg(&store, &selector, &account_config.name)?;
            drop(store);
            let msg = mark_as_draft(&row.path)?;
            reindex_drafts(&account_config.name);
            if msg.starts_with("Already") {
                println!("{} {} is already a draft", "\u{2139}".blue(), canonical);
            } else {
                println!("{} demoted {}", "\u{2713}".green(), canonical);
            }
        }

        Some(Commands::New { name }) => {
            let file_name = if Path::new(&name).extension().is_some() {
                name.clone()
            } else {
                format!("{}.md", name)
            };
            let dir = mailypoppins::config::drafts_dir(&account_config.name);
            fs::create_dir_all(&dir)?;
            let path = dir.join(&file_name);
            if path.exists() {
                return Err(anyhow!("A draft already exists at {}", path.display()));
            }

            // The id is minted here rather than by the index, so the selector
            // printed below is the one in the file from the first byte.
            let id = mailypoppins::store::drafts::new_id();
            let now = chrono::Utc::now().to_rfc2822();
            let skeleton = new_draft_skeleton_with_id(
                &smtp_config.default_from,
                &now,
                &id,
                signature_content.as_deref(),
            );
            fs::write(&path, skeleton)?;
            reindex_drafts(&account_config.name);
            println!(
                "{} {}",
                "\u{2713}".green(),
                Selector::for_draft(&account_config.name, &id)
            );
        }

        Some(Commands::Path { selector }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = drafts_store(&account_config.name)?;
            let (row, _canonical) = resolve_draft_arg(&store, &selector, &account_config.name)?;
            println!("{}", row.path.display());
        }

        Some(Commands::Edit { selector }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = drafts_store(&account_config.name)?;
            let (row, canonical) = resolve_draft_arg(&store, &selector, &account_config.name)?;
            drop(store);
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "hx".to_string());
            let status = std::process::Command::new(&editor)
                .arg(&row.path)
                .status()
                .with_context(|| format!("running {editor}"))?;
            reindex_drafts(&account_config.name);
            if !status.success() {
                return Err(anyhow!("{editor} exited with {status}"));
            }
            println!("{} {}", "\u{2713}".green(), canonical);
        }

        Some(Commands::Reply { selector, all, mailbox }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = received_store(&account_config.name)?;
            let (row, canonical) =
                resolve_received_arg(&store, &selector, &account_config.name, mailbox.as_deref())?;
            let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
            let source = source_from_row(&store, &blobs, &row, false)?;
            drop(store);

            let (_path, draft) = mailypoppins::draft::create_draft_from_source(
                &account_config.name,
                &account_config.default_from,
                &source,
                mailypoppins::draft::DraftFromSource::Reply { all },
                None,
                resolve_body_signature(
                    &account_config,
                    cli.no_signature,
                    cli.signature.as_deref(),
                    &global_config.email,
                )
                .as_deref(),
            )?;
            println!("{} reply to {}", "\u{2713}".green(), canonical);
            println!("{}", draft);
        }

        Some(Commands::Forward { selector, mailbox }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = received_store(&account_config.name)?;
            let (row, canonical) =
                resolve_received_arg(&store, &selector, &account_config.name, mailbox.as_deref())?;
            let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
            let source = source_from_row(&store, &blobs, &row, true)?;
            drop(store);

            let (_path, draft) = mailypoppins::draft::create_draft_from_source(
                &account_config.name,
                &account_config.default_from,
                &source,
                mailypoppins::draft::DraftFromSource::Forward,
                None,
                resolve_body_signature(
                    &account_config,
                    cli.no_signature,
                    cli.signature.as_deref(),
                    &global_config.email,
                )
                .as_deref(),
            )?;
            println!("{} forward of {}", "\u{2713}".green(), canonical);
            println!("{}", draft);
        }

        Some(Commands::Invite { action }) => {
            let (selector, mailbox, rsvp) = match action {
                InviteAction::Accept { selector, mailbox } => {
                    (selector, mailbox, mailypoppins::invite::Rsvp::Accepted)
                }
                InviteAction::Tentative { selector, mailbox } => {
                    (selector, mailbox, mailypoppins::invite::Rsvp::Tentative)
                }
                InviteAction::Decline { selector, mailbox } => {
                    (selector, mailbox, mailypoppins::invite::Rsvp::Declined)
                }
            };
            if account_config.auth_method == AuthMethod::Graph {
                return Err(anyhow!(
                    "RSVP is not supported for Graph accounts yet (#0036, blocked on #0035)"
                ));
            }
            // The RSVP goes out over this account's SMTP transport, already
            // bound; a cross-account selector fails loudly rather than replying
            // from the wrong account.
            ensure_selector_account_matches(&selector, &account_config)?;

            let store = received_store(&account_config.name)?;
            let (row, canonical) =
                resolve_received_arg(&store, &selector, &account_config.name, mailbox.as_deref())?;
            let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
            // The invitation's own iMIP payload is the source of truth for the
            // reply, and it is a blob on the row (#0038 item 6).
            let ics = mailypoppins::store::read::load_invite_ics(&store, &blobs, row.id)
                .ok_or_else(|| anyhow!("{canonical} carries no invitation to reply to"))?;
            drop(store);

            let outcome = mailypoppins::send::send_rsvp(
                &ics,
                &account_config,
                &account_config.default_from,
                rsvp,
                &smtp_config,
            )
            .await?;
            if !outcome.send_result.any_succeeded() {
                return Err(anyhow!("Failed to send the RSVP to {}", outcome.organizer));
            }
            println!(
                "{} {} \u{2014} replied to {}",
                "\u{2713}".green(),
                outcome.subject,
                outcome.organizer
            );
        }

        Some(Commands::ListMailboxes) => {
            if account_config.auth_method == AuthMethod::Graph {
                let graph_config = mailypoppins::config::GraphConfig::load(&account_config)?;
                let client = mailypoppins::graph::GraphClient::new_async(&graph_config).await?;
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

        Some(Commands::Sync { limit, mailbox, dry_run, all_accounts }) => {
            // `--all-accounts` is a loop over the same body rather than a
            // second code path, like `send-approved --all-accounts`: each
            // account resolves its own transport and targets, so the
            // per-account sync is exactly what the single-account form does.
            let accounts: Vec<AccountConfig> = if all_accounts {
                global_config.accounts.clone()
            } else {
                vec![account_config.clone()]
            };
            // An empty config (or a `-A` that named nothing) resolves to
            // `AccountConfig::default()`, whose name is empty: without this the
            // run reports `✗ : <error>` for an account that does not exist
            // (#0071 review).
            if accounts.is_empty() || accounts.iter().all(|a| a.name.is_empty()) {
                return Err(anyhow!("No account to sync (check `mp config show`)"));
            }

            // One account's failure does not abort the others: the run
            // continues and every failure is named at the end (#0071). The
            // seven-week outage in #0068 was a failure nothing named.
            let mut attempted = 0usize;
            let mut failed: Vec<String> = Vec::new();
            for account_config in &accounts {
                if accounts.len() > 1 {
                    println!("\n{}", format!("── {} ──", account_config.name).bold());
                }
                // A drafts-only account has nothing to sync and is not a
                // failure; counting it as one exits 1 on every run of a config
                // that legitimately holds one (#0071 review).
                if account_config.is_local_only() {
                    println!("{} {}: local-only, skipped", "-".dimmed(), account_config.name);
                    continue;
                }
                attempted += 1;
                match sync_one_account(account_config, limit, mailbox.as_deref(), dry_run).await {
                    Ok(()) => {
                        // The retention sweep rides on every real sync (#0060):
                        // a dry-run touches nothing, and a failed sync is not a
                        // moment to start deleting cached blobs.
                        if !dry_run {
                            retention_sweep_after_sync(&global_config, account_config);
                        }
                    }
                    Err(e) => {
                        error!("[sync] account '{}' failed: {e:#}", account_config.name);
                        eprintln!("{} {}: {:#}", "✗".red(), account_config.name, e);
                        failed.push(account_config.name.clone());
                    }
                }
            }

            // Skipped accounts are out of the denominator too: "1 of 2" when
            // the second was never synced would be a claim about an account
            // this run said nothing about.
            if let Some(summary) = mailypoppins::sync_health::failure_summary(attempted, &failed) {
                eprintln!("{} {}", "✗".red(), summary);
            }
            let code = mailypoppins::sync_health::exit_code(&failed);
            if code != 0 {
                std::process::exit(code);
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

        Some(Commands::Archive { selector, mailbox }) => {
            // The server move and the row rewrite both belong to the selector's
            // account: resolve it before opening the store or loading creds.
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = received_store(&account_config.name)?;
            let (row, canonical) =
                resolve_received_arg(&store, &selector, &account_config.name, mailbox.as_deref())?;
            let source_server = find_server_name_for_role(&account_config, &row.mailbox);
            let dest_server = find_server_name_for_role(&account_config, ARCHIVE_MAILBOX);

            // Through the durable queue, the same seam the TUI drains (#0039):
            // the row moves and the owed server op commit in one transaction,
            // then the op runs synchronously so the CLI keeps its blocking UX.
            // A crash between the two halves leaves the op queued for the next
            // drain rather than losing it, which server-first-then-row could
            // not promise. On a server refusal `run_and_settle` rolls the row
            // home and propagates the error verbatim, so a not-found stays
            // byte-identical to the pre-queue message.
            let op = ServerOp::Move {
                message_id: row.message_id.clone(),
                source_mailbox: source_server,
                dest_mailbox: dest_server,
            };
            let backend = Backend::resolve(&account_config)?;
            let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
            let Some((_previous, op_id)) =
                pending_ops::apply_move(&store, &account_config.name, row.id, ARCHIVE_MAILBOX, op)?
            else {
                return Err(anyhow!("{canonical} is no longer in the store"));
            };
            pending_ops::run_and_settle(&store, &blobs, op_id, &backend).await?;
            println!("{} archived {}", "\u{2713}".green(), canonical);
            println!(
                "  {} {}",
                "now".dimmed(),
                Selector::new(&account_config.name, ARCHIVE_MAILBOX, &row.message_id)
            );
        }

        Some(Commands::Delete { selector, mailbox, force, sent }) => {
            if sent {
                // The upgrade path (#0073): a version that did not retire a
                // sent draft on send leaves a directory of `status: sent`
                // files with nothing left to do to them. Clear them in one
                // call, file and row alike.
                let store = drafts_store(&account_config.name)?;
                let rows = mailypoppins::store::drafts::list(
                    &store,
                    &account_config.name,
                    Some("sent"),
                )?;
                if rows.is_empty() {
                    println!("No sent drafts to clear on {}", account_config.name);
                } else {
                    let mut cleared = 0usize;
                    for row in &rows {
                        match mailypoppins::draft::delete_indexed_draft(
                            &store,
                            &account_config.name,
                            row,
                            false,
                        ) {
                            Ok(()) => cleared += 1,
                            Err(e) => eprintln!(
                                "{} keeping {}: {e:#}",
                                "\u{26a0}".yellow(),
                                Selector::for_draft(&account_config.name, &row.id)
                            ),
                        }
                    }
                    reindex_drafts(&account_config.name);
                    println!(
                        "{} cleared {cleared} sent draft{} on {}",
                        "\u{2713}".green(),
                        if cleared == 1 { "" } else { "s" },
                        account_config.name
                    );
                }
            } else {
                // `required_unless_present = "sent"` guarantees the selector.
                let selector = selector.expect("clap requires a selector without --sent");
                // A cross-account selector deletes from its own account, so the
                // store and (for received mail) the server credentials must be
                // the selector's, not `-A`'s (the #0073 follow-up bug).
                let account_config =
                    account_for_selector(&selector, &account_config, &global_config)?;
                if is_drafts_selector(&selector, mailbox.as_deref())? {
                    // Drafts are local-only: no server op, just the file and
                    // the index row the rescan drops (#0073).
                    let store = drafts_store(&account_config.name)?;
                    let (row, canonical) =
                        resolve_draft_arg(&store, &selector, &account_config.name)?;
                    mailypoppins::draft::delete_indexed_draft(
                        &store,
                        &account_config.name,
                        &row,
                        force,
                    )?;
                    reindex_drafts(&account_config.name);
                    println!("{} deleted {}", "\u{2713}".green(), canonical);
                } else {
                    let store = received_store(&account_config.name)?;
                    let (row, canonical) = resolve_received_arg(
                        &store,
                        &selector,
                        &account_config.name,
                        mailbox.as_deref(),
                    )?;
                    let source_server =
                        find_server_name_for_role(&account_config, &row.mailbox);

                    // The durable queue again (#0039): the row delete and the
                    // owed server delete commit together, then the op runs
                    // synchronously. A delete has nothing to roll back (the row
                    // is gone and the server still holds the message), so a
                    // refusal propagates verbatim and the next sync refetches
                    // the UID; the not-found message stays byte-identical.
                    let op = ServerOp::Delete {
                        message_id: row.message_id.clone(),
                        source_mailbox: source_server,
                    };
                    let backend = Backend::resolve(&account_config)?;
                    let blobs =
                        mailypoppins::store::BlobStore::for_account(&account_config.name);
                    let Some((_previous, op_id)) = pending_ops::apply_delete(
                        &store,
                        &blobs,
                        &account_config.name,
                        row.id,
                        op,
                    )?
                    else {
                        return Err(anyhow!("{canonical} is no longer in the store"));
                    };
                    pending_ops::run_and_settle(&store, &blobs, op_id, &backend).await?;
                    println!("{} deleted {}", "\u{2713}".green(), canonical);
                }
            }
        }

        Some(Commands::Open { selector, mailbox }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = received_store(&account_config.name)?;
            let (row, canonical) =
                resolve_received_arg(&store, &selector, &account_config.name, mailbox.as_deref())?;
            let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
            // Attachments are blobs; the system opener needs files, so they are
            // materialised into a temp directory keyed by the row. The TUI's
            // `o` comes through the same helper, so both put them in the same
            // private place.
            let dir = mailypoppins::parse::materialisation_dir(&row.id.to_string())?;
            let files = materialise_attachments(&store, &blobs, row.id, &dir)?;
            if files.is_empty() {
                return Err(anyhow!("{canonical} has no attachments"));
            }
            for file in &files {
                mailypoppins::parse::open_file_with_system(file)?;
                println!("{} opened {}", "\u{2713}".green(), file.display());
            }
        }

        Some(Commands::Save { selector, output, mailbox }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = received_store(&account_config.name)?;
            let (row, canonical) =
                resolve_received_arg(&store, &selector, &account_config.name, mailbox.as_deref())?;
            let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
            let dest = output.unwrap_or_else(|| PathBuf::from("."));
            let files = materialise_attachments(&store, &blobs, row.id, &dest)?;
            if files.is_empty() {
                return Err(anyhow!("{canonical} has no attachments"));
            }
            for file in &files {
                println!("{} {}", "\u{2713}".green(), file.display());
            }
        }

        // The read surface over the store (#0062): both offline, both reusing
        // the queries `store::read` already had. `mp show` is the single-message
        // half, addressed by the same selector grammar every other received
        // command takes, and resolved through `account_for_selector` first so a
        // cross-account selector opens the right store (the #0073 follow-up).
        Some(Commands::Show { selector, mailbox, json }) => {
            let account_config = account_for_selector(&selector, &account_config, &global_config)?;
            let store = received_store(&account_config.name)?;
            let (row, _) =
                resolve_received_arg(&store, &selector, &account_config.name, mailbox.as_deref())?;
            let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
            let message =
                mailypoppins::read_cmd::shown_message(&store, &blobs, &account_config.name, &row);
            if json {
                println!("{}", mailypoppins::read_cmd::to_json(&message)?);
            } else {
                print!("{}", mailypoppins::read_cmd::render_show(&message));
            }
        }

        Some(Commands::ListMessages { mailbox, limit }) => {
            let store = received_store(&account_config.name)?;
            let groups = list_message_groups(
                &store,
                &account_config,
                mailbox.as_deref(),
                limit,
            )?;
            print!(
                "{}",
                mailypoppins::read_cmd::render_list(&account_config.name, &groups)
            );
        }

        Some(Commands::Search {
            query,
            mailbox,
            from,
            to,
            cc,
            subject,
            body,
            filename,
            has_attachment,
            after,
            before,
            limit,
            full,
            local,
        }) => {
            // One parser, one AST (#0086a): the positional grammar and the
            // flags build the identical query. A malformed query is an error
            // with a caret, never a silent search for less.
            let flags = mailypoppins::search::Flags {
                from,
                to,
                cc,
                subject,
                body,
                filename,
                has_attachment,
                after,
                before,
            };
            let query_ast = mailypoppins::search::from_cli(&query, &flags)
                .map_err(|e| anyhow!("{e}"))?;

            // Resolve mailbox scope: --mailbox flag > in: directive > all.
            let mailbox_name = mailbox.or_else(|| query_ast.in_mailbox.clone());

            if local {
                let store = received_store(&account_config.name)?;
                let mailbox_key = match mailbox_name.as_deref() {
                    Some(want) => Some(resolve_mailbox_key(&account_config, want)?),
                    None => None,
                };
                let span = mailypoppins::timing::TimingSpan::with_context(
                    "search-local",
                    account_config.name.clone(),
                );
                let hits = mailypoppins::store::search::search_ast(
                    &store,
                    &account_config.name,
                    &query_ast,
                    mailbox_key.as_deref(),
                    limit,
                )?;
                drop(span);
                let blobs = mailypoppins::store::BlobStore::for_account(&account_config.name);
                let rows: Vec<_> = hits
                    .into_iter()
                    .map(|hit| {
                        let body = full
                            .then(|| {
                                mailypoppins::store::read::load_body(&store, &blobs, hit.row.id)
                            })
                            .flatten();
                        (hit.row, body)
                    })
                    .collect();
                print!(
                    "{}",
                    mailypoppins::read_cmd::render_search(&account_config.name, &query, &rows)
                );
                return Ok(());
            }

            if account_config.auth_method == AuthMethod::Graph {
                let graph_config = GraphConfig::load(&account_config)?;
                let client = graph::GraphClient::new_async(&graph_config).await?;
                let mut emails = client
                    .search_messages(&query_ast, mailbox_name.as_deref(), limit)
                    .await?;
                sort_fetched_by_date(&mut emails);
                if emails.is_empty() {
                    println!("{}", "No results found".yellow());
                } else {
                    display_fetched_emails(&emails, full);
                }
            } else {
                let imap_config = ImapConfig::load(&account_config)?;
                // Gmail is the only IMAP server with an attachment key
                // (`X-GM-RAW has:attachment`); a plain server has none, so its
                // has:attachment residue is post-filtered from the store below.
                let host_lc = imap_config.host.to_ascii_lowercase();
                let gmail = host_lc == "imap.gmail.com"
                    || host_lc.ends_with(".gmail.com")
                    || host_lc.ends_with("googlemail.com");
                let (imap_search, attachment_postfilter) = if gmail {
                    (mailypoppins::search::to_gmail_search_command(&query_ast), false)
                } else {
                    let r = mailypoppins::search::to_imap(&query_ast)
                        .map_err(|e| anyhow!("{e}"))?;
                    (r.search, r.attachment_postfilter)
                };
                let msg_id = query_ast.message_id.clone();

                // The target mailboxes: the scoped one, or all the account's.
                let targets: Vec<(String, String)> = match mailbox_name {
                    Some(ref mb) => vec![(mb.clone(), mb.clone())],
                    None => all_configured_mailboxes(&account_config)
                        .iter()
                        .map(|(role, mapping)| (role.to_string(), mapping.server.clone()))
                        .collect(),
                };

                let mut session = imap_client::open_imap_session(&imap_config).await?;
                let per_mb = (limit / targets.len().max(1)).max(5);
                let mut total = 0usize;
                let mut all_emails: Vec<FetchedEmail> = Vec::new();
                for (label, server) in &targets {
                    if total >= limit {
                        break;
                    }
                    let budget = if targets.len() == 1 {
                        limit
                    } else {
                        per_mb.min(limit - total)
                    };
                    match imap_client::search_on_session(
                        &mut session,
                        &imap_search,
                        msg_id.as_deref(),
                        server,
                        Some(budget),
                    )
                    .await
                    {
                        Ok(emails) => {
                            total += emails.len();
                            all_emails.extend(emails);
                        }
                        Err(e) => {
                            eprintln!("{} Search in {} failed: {}", "\u{26a0}".yellow(), label, e);
                        }
                    }
                }
                session.logout().await.ok();

                // Plain-IMAP has:attachment (#0086a, option b): keep only hits
                // the local store marks as carrying an attachment, and say that
                // un-synced mail is not covered.
                if attachment_postfilter {
                    let store = received_store(&account_config.name)?;
                    let with_att = mailypoppins::store::read::message_ids_with_attachments(
                        &store,
                        &account_config.name,
                    )?;
                    all_emails.retain(|e| {
                        e.message_id.as_deref().is_some_and(|m| {
                            with_att.contains(&mailypoppins::store::read::normalize_message_id_key(m))
                        })
                    });
                    eprintln!(
                        "{} has:attachment on plain IMAP is answered from the local store, so \
                         un-synced mail is not covered. Run `mp sync` for full coverage.",
                        "\u{26a0}".yellow()
                    );
                }

                sort_fetched_by_date(&mut all_emails);
                if all_emails.is_empty() {
                    println!("{}", "No results found".yellow());
                } else {
                    display_fetched_emails(&all_emails, full);
                }
            }
        }

        Some(Commands::Contacts { action }) => {
            match action {
                ContactsAction::Search { query, parsable, limit, account } => {
                    let acct = account.or_else(|| cli.account.clone());
                    mailypoppins::contacts_cmd::handle_search(
                        &global_config,
                        query,
                        parsable,
                        limit,
                        acct,
                    )?;
                }
                ContactsAction::Rebuild { account } => {
                    let acct = account.or_else(|| cli.account.clone());
                    mailypoppins::contacts_cmd::handle_rebuild(&global_config, acct)?;
                }
                ContactsAction::Stats { account } => {
                    let acct = account.or_else(|| cli.account.clone());
                    mailypoppins::contacts_cmd::handle_stats(&global_config, acct)?;
                }
            }
        }

        Some(Commands::Calendar { action }) => match action {
            CalendarAction::Rebuild { account } => {
                let acct = account.or_else(|| cli.account.clone());
                mailypoppins::calendar_cmd::handle_rebuild(&global_config, acct)?;
            }
        },

        Some(Commands::Outbox { action }) => {
            cmd_outbox(&account_config, action).await?;
        }

        Some(Commands::Store { action }) => {
            let StoreAction::Gc {
                dry_run,
                force,
                all_accounts,
            } = action;
            let accounts: Vec<AccountConfig> = if all_accounts {
                global_config.accounts.clone()
            } else {
                vec![account_config.clone()]
            };
            if accounts.is_empty() || accounts.iter().all(|a| a.name.is_empty()) {
                return Err(anyhow!("No account to sweep (check `mp config show`)"));
            }
            for account in &accounts {
                if accounts.len() > 1 {
                    println!("\n{}", format!("\u{2500}\u{2500} {} \u{2500}\u{2500}", account.name).bold());
                }
                run_store_gc(&global_config, account, dry_run, force)?;
            }
        }

        Some(Commands::Cutover { account, dry_run }) => {
            let acct = account.or_else(|| cli.account.clone());
            mailypoppins::cutover::handle_cutover(&global_config, acct, dry_run)?;
        }

        Some(Commands::DumpKeys { json }) => {
            if json {
                print!("{}", mailypoppins::tui::dump_keys_json());
            } else {
                print!("{}", mailypoppins::tui::dump_keys());
            }
        }

        Some(Commands::DumpMailbox { json: _, mailbox }) => {
            // `--json` is `required = true`, so the format is already pinned.
            let accounts: Vec<mailypoppins::config::AccountConfig> = match cli.account {
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
            let records = mailypoppins::dump::collect_records(&accounts, &filter);
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(mailypoppins::dump::to_ndjson(&records).as_bytes())?;
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
            if let Some(ref selector) = cli.selector {
                // Preview mode (dry run): a draft selector, never a path.
                let account_config =
                    account_for_selector(selector, &account_config, &global_config)?;
                let store = drafts_store(&account_config.name)?;
                let (row, _canonical) =
                    resolve_draft_arg(&store, selector, &account_config.name)?;
                drop(store);
                let draft = parse_email_draft(&row.path)?;
                // Same as the single-send preview: the body already carries
                // the signature (#0099).
                preview_draft(&draft, &smtp_config, &global_config.email, None, true)?;
            } else {
                // No file, no subcommand -> launch TUI
                mailypoppins::tui::run()?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_drafts_selector;

    /// `mp delete` dispatches on the selector shape, not a second command
    /// (#0073): the reserved `drafts` mailbox segment, or `--mailbox drafts`
    /// beside an elided selector, names a draft; anything else is received.
    #[test]
    fn a_drafts_selector_is_recognised_by_its_mailbox_segment() {
        assert!(is_drafts_selector("mp://tum/drafts/abc123", None).unwrap());
        assert!(is_drafts_selector("drafts/abc123", None).unwrap());
        // The flag names the mailbox beside an elided key.
        assert!(is_drafts_selector("abc123", Some("drafts")).unwrap());
    }

    #[test]
    fn a_received_selector_is_not_a_draft() {
        assert!(!is_drafts_selector("mp://tum/INBOX/msg@example.com", None).unwrap());
        assert!(!is_drafts_selector("mp://tum/Archive/msg@example.com", None).unwrap());
        // A bare key with no drafts flag is a received key by default scope.
        assert!(!is_drafts_selector("msg@example.com", None).unwrap());
    }
}
