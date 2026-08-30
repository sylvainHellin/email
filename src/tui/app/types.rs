use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::NaiveDate;

use crate::parse::FetchedEmail;
use crate::store::read::{self, MessageRow};
use crate::store::{drafts, open_store, Store};
use crate::types::MailboxRole;

// ---------------------------------------------------------------------------
// MessageRef
// ---------------------------------------------------------------------------

/// Stable in-process handle for one stored message: `messages.id`, the
/// synthetic primary key of the row.
///
/// This is what replaced `EmailEntry.path` when the read path moved onto the
/// store (#0038). Everything that used to key on the file path keys on this:
/// the list selection set, the cursor anchor across a list rebuild, and the
/// payload of every queued `Action`.
///
/// The synthetic id rather than `(account, mailbox, uid)` because those are
/// precisely the coordinates that move. `src/store/schema.rs` puts it plainly:
/// the id exists "so a move or a UIDVALIDITY reset does not invalidate
/// references held elsewhere", and a selection the user made two seconds
/// before a sync renumbered the mailbox is exactly such a reference.
///
/// Scoped to one account, because the store is: two accounts can hold the same
/// id for different messages. Every holder (`selection`, `cursor_ref`, a queued
/// `Action`) is per-account state that is dropped or re-anchored on account
/// switch, so the scope never has to be carried alongside.
///
/// The user-facing name of a message is the `mp://<account>/<mailbox>/<key>`
/// selector, which is a different thing with a different job (it survives a
/// restart, this does not) and landed with #0050.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageRef(i64);

impl MessageRef {
    /// Wrap a `messages.id` read back from the store.
    pub fn new(row_id: i64) -> Self {
        Self(row_id)
    }

    /// The `messages.id` this refers to.
    pub fn row_id(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for MessageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "message #{}", self.0)
    }
}

/// What the multi-select set holds: one list entry, named in whichever of the
/// two namespaces it lives in (#0052).
///
/// The list has exactly two kinds of row and they are named differently: a
/// received message is a `messages` row ([`MessageRef`]), a draft is a file the
/// drafts index knows by its `id:`. Keying the selection on `MessageRef` alone
/// meant a draft could never enter it, which left batch approve and batch
/// mark-draft reachable by keystroke and dead in fact. The enum makes the two
/// namespaces explicit, the same way the `mp://` selector does at the CLI
/// boundary, so a handler that only accepts one of them says so in its types.
///
/// A selection never mixes the two in practice: a mailbox lists one kind of row
/// and [`crate::tui::app::App::switch_mailbox`] clears the set on the way out.
/// The batch handlers still filter rather than assume, because a filter is
/// honest about a set it did not build.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKey {
    /// A received message, by its store row.
    Msg(MessageRef),
    /// A draft, by its indexed `id:` (#0050 scope item 4).
    Draft(String),
}

impl EntryKey {
    /// The store row this names, or `None` for a draft.
    pub fn msg(&self) -> Option<MessageRef> {
        match self {
            EntryKey::Msg(msg) => Some(*msg),
            EntryKey::Draft(_) => None,
        }
    }

    /// The indexed draft id this names, or `None` for a received message.
    pub fn draft(&self) -> Option<&str> {
        match self {
            EntryKey::Msg(_) => None,
            EntryKey::Draft(id) => Some(id),
        }
    }
}

// ---------------------------------------------------------------------------
// EmailEntry (ported from beautifulmail's email.rs)
// ---------------------------------------------------------------------------

/// Parsed email entry for display in the list and preview.
///
/// It carries no body. A mailbox load is rows only (#0038 scope item 5), and
/// the body of the one message the preview shows is fetched from the blob
/// store on demand and memoised in [`PreviewBody`]. The list is shared behind
/// an `Arc` between the active mailbox and its cache slot, so a body parked
/// here would be paid for by every message in the mailbox to serve the one on
/// screen, and could not be refreshed without cloning the whole vector.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EmailEntry {
    /// Identity of the underlying `messages` row. Replaced the file path in
    /// #0038; see [`MessageRef`].
    ///
    /// `None` means "this entry has no store row", which happens for exactly
    /// one kind of entry: a server-search hit whose Message-ID does not
    /// resolve locally (see `tui::helpers::fetched_to_email_entry`). Such an
    /// entry can be listed and previewed from the fetched content, and every
    /// row-dependent operation on it must decline with a status message
    /// rather than act on some other message. A sentinel `MessageRef` would
    /// be a lie that could reach the selection set, so the absence is typed.
    pub msg: Option<MessageRef>,
    /// Identity of the underlying drafts-index row: the `id:` frontmatter
    /// field (#0050 scope item 4).
    ///
    /// A drafts entry has no `messages` row, so `msg` is `None` for it and
    /// this carries its name instead. The two are mutually exclusive by
    /// construction: `entry_from_row` fills `msg`, `entry_from_draft` fills
    /// `draft_id`, and nothing fills both. Anything that needs to name the
    /// selected entry (`Action::CopyMessageRef`) asks this first, because a
    /// draft's canonical selector is `mp://<account>/drafts/<id>` and never
    /// goes through the store.
    pub draft_id: Option<String>,
    /// Set when this row is a draft file the index could not parse (#0080).
    ///
    /// Such a file has no `id:` to index under, so `msg` and `draft_id` are
    /// both `None` and it would otherwise be a keyless nobody. This carries
    /// its path and the one-line parse error instead, so the Drafts list can
    /// show it as an unopenable error row where the user expects the draft to
    /// be, rather than let the file vanish. It is mutually exclusive with both
    /// `msg` and `draft_id`: a row is a message, a parsed draft, or a skip.
    pub skip: Option<crate::store::drafts::SkippedDraft>,
    pub from: String,
    pub to: String,
    pub cc: Option<String>,
    /// `Reply-To:` header, surfaced in the header pane when present (#0096).
    pub reply_to: Option<String>,
    /// `Bcc:` header, surfaced in the header pane when present (#0096).
    pub bcc: Option<String>,
    pub subject: String,
    pub status: String,
    pub date_display: String,
    pub date_sort: String,
    pub has_attachments: bool,
    pub read: bool,
    /// True when a reply to this message has gone out (#TKT-0051), either from
    /// here or from another client the server told us about.
    ///
    /// Three booleans rather than one collapsed state, because the axis is a
    /// set: a message can be read, answered and forwarded at once. Which one
    /// the marker column shows is [`crate::tui::ui::list`]'s decision.
    pub answered: bool,
    /// True when this message has been forwarded (#TKT-0051).
    pub forwarded: bool,
    /// True when the user starred this message with `\Flagged` (#0007).
    ///
    /// Orthogonal to the read/answered/forwarded axis: a message can be flagged
    /// and unread at once, so the list renders it as its own coloured marker
    /// rather than folding it into the single status glyph.
    pub flagged: bool,
    /// True when the message carries an iMIP payload, i.e. the store row has
    /// an `invite.ics` attachment blob (#0029, #0038 scope item 6).
    ///
    /// A flag rather than the parsed event: the badge is needed for every row
    /// of the mailbox and the answer rides on the listing query itself, while
    /// the event card is needed for one row at a time and is parsed from that
    /// row's ics blob on demand (see [`PreviewInvite`]). Parsing every
    /// mailbox row's invite eagerly would put back exactly the per-row blob
    /// read the lazy body work removed.
    pub is_invite: bool,
}

impl EmailEntry {
    /// The contact to display depends on the mailbox kind:
    /// Inbox/Archive/Extra show `from`, Drafts/Sent show `to`.
    pub fn display_contact(&self, kind: MailboxKind) -> &str {
        match kind {
            MailboxKind::Inbox | MailboxKind::Archive | MailboxKind::Extra => &self.from,
            MailboxKind::Drafts | MailboxKind::Sent => &self.to,
        }
    }

    /// How the selection set names this entry, or `None` for the one entry
    /// that has no name: a server-search hit that resolved to no local row.
    pub fn key(&self) -> Option<EntryKey> {
        match (self.msg, self.draft_id.as_deref()) {
            (Some(msg), _) => Some(EntryKey::Msg(msg)),
            (None, Some(id)) => Some(EntryKey::Draft(id.to_string())),
            (None, None) => None,
        }
    }
}

/// Load one mailbox of one account from the store, newest first.
///
/// `mailbox` is the role or slug ingest recorded, which is the leaf of the
/// `MailboxInfo::dir` the sidebar carries (see [`mailbox_key`]).
///
/// There is no directory walk and no fallback to one. After #0037 nothing
/// writes `.md`, so a message that is not in the store is an ingest bug, and a
/// walk that produced it anyway would hide that bug behind a slow path.
/// A store that cannot be opened or queried logs and yields an empty list,
/// which is what a mailbox that has never synced looks like anyway.
///
/// One SQL query and no blob reads at all: the bodies are loaded lazily, one
/// at a time behind the preview (see [`PreviewBody`]) and once per list
/// generation behind body search. The `[TIMING]` span for a cold start
/// therefore shows a single row-count mark and nothing else.
pub fn load_emails(account: &str, mailbox: &str) -> Vec<EmailEntry> {
    let mut span = crate::timing::TimingSpan::with_context(
        "load_emails",
        format!("{account}/{mailbox}"),
    );
    if mailbox == crate::selector::DRAFTS_MAILBOX {
        let entries = load_drafts(account);
        span.mark(&format!("{} draft(s) from the index", entries.len()));
        return entries;
    }
    let Some(store) = open_store(account) else {
        return Vec::new();
    };
    let rows = match read::list_mailbox(&store, account, mailbox) {
        Ok(rows) => rows,
        Err(e) => {
            log::warn!("[store] listing {account}/{mailbox} failed: {e:#}");
            return Vec::new();
        }
    };
    span.mark(&format!("{} row(s), no blob reads", rows.len()));

    let status = status_for_mailbox(mailbox);
    rows.into_iter()
        .map(|row| entry_from_row(row, &status))
        .collect()
}

/// The Drafts mailbox, listed from the drafts index instead of `messages`
/// (#0050 scope item 5).
///
/// Drafts are the one local-only thing in the product: they are `.md` files an
/// agent or `$EDITOR` writes behind the application's back, so there is no
/// `messages` row to list and the index is what the CLI and the TUI share.
/// The refresh is paid here rather than assumed, because a mailbox load is
/// exactly the moment the answer has to be current; the one-second fingerprint
/// poll in the event loop is what notices a change *between* loads.
///
/// Every status the index holds is listed, `sent` included: the lister filters
/// nothing, so a file someone hand-edited to `status: sent` still shows, which
/// is the escape hatch it should be. What no longer shows is a draft this
/// application sent to every recipient and recorded in the outbox, because
/// such a send retires the file (see [`crate::draft::settle_sent_draft`]); a
/// *partial* send, or one with no durable record, keeps it, marked `sent` and
/// addressable.
fn load_drafts(account: &str) -> Vec<EmailEntry> {
    let (rows, skipped) = indexed_drafts(account);
    // The unparseable files lead the list: they are the ones the user is
    // hunting for ("my draft disappeared"), and they have no date to sort by,
    // so pinning them to the top is both honest and useful (#0080).
    skipped
        .into_iter()
        .map(entry_from_skip)
        .chain(rows.into_iter().map(entry_from_draft))
        .collect()
}

/// The indexed drafts of one account: the single answer the Drafts list and
/// the sidebar count both read.
///
/// [`Store::open`] rather than [`open_store`], and the refresh is paid here
/// rather than assumed: drafts are local-only files, so an account that has
/// never synced has no store *file* and still has drafts, and a count that
/// opened differently or skipped the refresh would contradict the list it
/// labels.
fn indexed_drafts(account: &str) -> (Vec<drafts::DraftRow>, Vec<drafts::SkippedDraft>) {
    let store = match Store::open(crate::config::store_path(account)) {
        Ok(store) => store,
        Err(e) => {
            log::warn!("[drafts] could not open the store for {account}: {e:#}");
            return (Vec::new(), Vec::new());
        }
    };
    let dir = crate::config::drafts_dir(account);
    // The reporting refresh hands back the files it skipped for a parse
    // failure, so the Drafts list can show them as error rows instead of
    // silently dropping them (#0080).
    let skipped = match drafts::refresh_reporting(&store, account, &dir) {
        Ok((_, _, skipped)) => skipped,
        Err(e) => {
            log::warn!("[drafts] refreshing the index of {account} failed: {e:#}");
            Vec::new()
        }
    };
    match drafts::list(&store, account, None) {
        Ok(rows) => (rows, skipped),
        Err(e) => {
            log::warn!("[drafts] listing the index of {account} failed: {e:#}");
            (Vec::new(), skipped)
        }
    }
}

/// Per-mailbox message counts for the sidebar, as one grouped query.
///
/// Index-aligned with `mailboxes`: a mailbox the store has no rows for counts
/// zero, so a configured-but-never-synced mailbox keeps its slot rather than
/// shifting every count after it.
///
/// Drafts are the exception, and have to be: they are not `messages` rows, so
/// the grouped query cannot see them and the sidebar would show 0 next to a
/// populated list. That count comes from [`indexed_drafts`], the same refresh
/// plus read the Drafts mailbox load itself does.
pub fn count_all_emails(account: &str, mailboxes: &[MailboxInfo]) -> Vec<usize> {
    let store = open_store(account);
    let counts = store
        .as_ref()
        .and_then(|store| match read::mailbox_counts(store, account) {
            Ok(counts) => Some(counts),
            Err(e) => {
                log::warn!("[store] counting mailboxes for {account} failed: {e:#}");
                None
            }
        })
        .unwrap_or_default();
    // The count is the length of the list, from the same [`indexed_drafts`]
    // call the mailbox load makes, so the sidebar cannot disagree with the
    // mailbox it labels.
    // The count is the length of the Drafts list, which now includes the
    // parse-skipped error rows, so the sidebar badge matches the list even
    // when some files would not parse (#0080).
    let draft_count = || {
        let (rows, skipped) = indexed_drafts(account);
        rows.len() + skipped.len()
    };

    mailboxes
        .iter()
        .map(|mb| {
            let key = mailbox_key(mb);
            if key == crate::selector::DRAFTS_MAILBOX {
                draft_count()
            } else {
                counts.get(&key).copied().unwrap_or(0)
            }
        })
        .collect()
}

/// The `messages.mailbox` value for a sidebar mailbox.
pub fn mailbox_key(mb: &MailboxInfo) -> String {
    mb.id.clone()
}

/// The `status` string a listed message shows, derived from the mailbox it was
/// listed from. Rendered in the headers pane as `[inbox]`, `[archived]` and so
/// on.
///
/// The one derivation there is. `EmailStatus` no longer carries the file-era
/// placement states this returns, and the `kind_to_status` copy that mapped a
/// `MailboxKind` to the same four strings is gone with the write-only search
/// fields that were its only reader (#0064).
fn status_for_mailbox(mailbox: &str) -> String {
    if mailbox == crate::selector::DRAFTS_MAILBOX {
        return "draft".to_string();
    }
    match MailboxRole::from(mailbox) {
        MailboxRole::Archive => "archived".to_string(),
        MailboxRole::Sent => "sent".to_string(),
        MailboxRole::Inbox | MailboxRole::Other(_) => "inbox".to_string(),
    }
}

/// Map one store row into a display entry. The body is not part of it and is
/// not read here (#0038 scope item 5).
///
/// Date display and sort keys are derived from the stored `Date:` header by
/// the same [`resolve_date`] the file build used, so both stacks apply one
/// rule; the path argument is empty because the filename fallback died with
/// the filenames.
///
/// `is_invite` comes off the listing query, so the badge costs no blob read;
/// the event card behind it is parsed lazily from the ics blob of the one row
/// the preview shows (#0038 scope item 6, [`PreviewInvite`]).
fn entry_from_row(row: MessageRow, status: &str) -> EmailEntry {
    let (date_display, date_sort) = resolve_date(&row.date_display, &None, Path::new(""));
    let flags = row.flags();
    EmailEntry {
        msg: Some(MessageRef::new(row.id)),
        draft_id: None,
        skip: None,
        from: extract_display_name(row.from.as_deref().unwrap_or_default()),
        to: extract_display_name(row.to.as_deref().unwrap_or_default()),
        cc: row.cc,
        reply_to: row.reply_to,
        bcc: row.bcc,
        subject: row
            .subject
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no subject)".to_string()),
        status: status.to_string(),
        date_display,
        date_sort,
        read: flags.seen,
        answered: flags.answered,
        forwarded: flags.forwarded,
        flagged: flags.flagged,
        has_attachments: row.has_attachments,
        is_invite: row.is_invite,
    }
}

/// Map one drafts-index row into a display entry.
///
/// `msg` stays `None`: there is no `messages` row behind a draft, and a
/// sentinel would be a lie that could reach the selection set. `read` is true
/// because a draft the user wrote is not unread mail, and the list renders
/// unread rows bold.
///
/// The date falls back to the filename through the same [`resolve_date`] the
/// file build used, so a draft whose frontmatter has no `date:` yet still
/// sorts by its `YYYY-MM-DD-...` stem rather than collapsing to the bottom.
fn entry_from_draft(row: crate::store::drafts::DraftRow) -> EmailEntry {
    let (date_display, date_sort) = resolve_date(&row.date, &None, &row.path);
    EmailEntry {
        msg: None,
        draft_id: Some(row.id),
        skip: None,
        from: String::new(),
        to: extract_display_name(row.to.as_deref().unwrap_or_default()),
        cc: row.cc,
        // Drafts index carries no reply_to/bcc column yet; the header pane
        // shows them for stored messages (#0096).
        reply_to: None,
        bcc: None,
        subject: row
            .subject
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no subject)".to_string()),
        status: row.status,
        date_display,
        date_sort,
        read: true,
        // The second axis is a property of received mail: a draft has neither
        // been answered nor forwarded, it *is* the answer.
        answered: false,
        forwarded: false,
        flagged: false,
        has_attachments: false,
        is_invite: false,
    }
}

/// Map one parse-skipped draft into an unopenable error row (#0080).
///
/// The file has no `id:` and no index row, so `msg` and `draft_id` are both
/// `None` and its identity is the `skip` field. The subject is the filename
/// (what the user sees in the directory), and the full parse error rides on
/// the `skip` for the preview pane and the row's error styling. `read` is true
/// so the list does not render it bold as if it were unread mail; the error
/// colour is what marks it, decided by [`crate::tui::ui::list`].
fn entry_from_skip(skip: crate::store::drafts::SkippedDraft) -> EmailEntry {
    let (date_display, date_sort) = resolve_date(&None, &None, &skip.path);
    let filename = skip
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    EmailEntry {
        msg: None,
        draft_id: None,
        skip: Some(skip),
        from: String::new(),
        to: String::new(),
        cc: None,
        reply_to: None,
        bcc: None,
        subject: filename,
        status: "error".to_string(),
        date_display,
        date_sort,
        read: true,
        answered: false,
        forwarded: false,
        flagged: false,
        has_attachments: false,
        is_invite: false,
    }
}

// ---------------------------------------------------------------------------
// Lazy bodies
// ---------------------------------------------------------------------------

/// What a memoised body belongs to: the account it was read for, the entry,
/// and the list generation that was current at the time.
///
/// The entry is an [`EntryKey`] and not a bare [`MessageRef`] because the
/// preview has two kinds of row to answer for: a received message, whose body
/// is a blob, and a draft, whose body is the markdown in its file. Keying on
/// the message alone meant a draft row could never build a key, which left the
/// Body pane blank for every draft.
///
/// The generation is the guard. `App::mailbox_load_generation` is bumped by
/// every mailbox load request and by every optimistic list mutation, so a body
/// read before a re-sync is discarded by the same counter that discards a
/// stale background load. The account index is in the key because a
/// [`MessageRef`] is only meaningful inside one account's store, and a drafts
/// `id:` inside one account's index.
pub(crate) type BodyKey = (usize, EntryKey, u64);

/// One-slot memo of the body behind the preview pane.
///
/// One slot rather than a map: the preview shows exactly one message, and a
/// map would grow to hold every body the user scrolled past, which is the
/// eager load this ticket removed wearing a different hat. Moving the cursor
/// costs one blob read; moving it back costs another.
///
/// It is refreshed from [`crate::tui::app::App::refresh_preview_body`] at the top of the
/// render pass, where `&mut App` is available, so the renderer itself stays a
/// pure function of state and needs no interior mutability.
#[derive(Debug, Default, Clone)]
pub struct PreviewBody {
    key: Option<BodyKey>,
    text: String,
    /// Monotonic stamp of the *content*, bumped by [`Self::fill`] only when the
    /// key or the text actually changes. The wrapped-line memo
    /// ([`crate::tui::ui::preview::PreviewLinesCache`]) keys on this so it can
    /// decide a cache hit in O(1) instead of re-comparing the whole body
    /// string every frame. The skip-draft and primed paths refill the same
    /// text every frame; a plain compare here keeps the stamp (and so the
    /// wrapped lines) stable across those frames.
    epoch: u64,
}

impl PreviewBody {
    /// The memoised body, empty when nothing is selected.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The content stamp, bumped whenever the body or its key changes. The
    /// wrapped-line memo pairs this with the pane width to detect a rebuild.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// True when the memo already answers for `key`.
    pub(crate) fn holds(&self, key: &Option<BodyKey>) -> bool {
        &self.key == key
    }

    /// Park `text` as the body for `key`, bumping the content stamp when either
    /// the key or the text moved. Called every frame on the skip-draft and
    /// primed paths, so it must not bump on an unchanged refill.
    pub(crate) fn fill(&mut self, key: Option<BodyKey>, text: String) {
        if self.key != key || self.text != text {
            self.epoch = self.epoch.wrapping_add(1);
        }
        self.key = key;
        self.text = text;
    }
}

/// One-slot memo of the parsed invite behind the preview pane (#0038 scope
/// item 6).
///
/// The sibling of [`PreviewBody`], keyed the same way and refreshed in the
/// same place, for the same reason: the event card is needed for the message
/// under the cursor and for no other, so its ics blob is read and parsed once
/// per cursor move instead of once per mailbox row. `EmailEntry.is_invite`
/// answers the list badge without any of this, so a mailbox of invites still
/// loads with zero blob reads.
///
/// The parsed event is the ics folded with the replies the store holds
/// (`crate::reconcile`), so the card shows the same attendee statuses the
/// agenda does.
#[derive(Debug, Default, Clone)]
pub struct PreviewInvite {
    key: Option<BodyKey>,
    event: Option<crate::types::EventFrontmatter>,
}

impl PreviewInvite {
    /// The memoised event, `None` when the selected message is not an invite.
    pub fn event(&self) -> Option<&crate::types::EventFrontmatter> {
        self.event.as_ref()
    }

    /// True when the memo already answers for `key`.
    pub(crate) fn holds(&self, key: &Option<BodyKey>) -> bool {
        &self.key == key
    }

    /// Park `event` as the invite for `key`.
    pub(crate) fn fill(
        &mut self,
        key: Option<BodyKey>,
        event: Option<crate::types::EventFrontmatter>,
    ) {
        self.key = key;
        self.event = event;
    }
}

/// Extract a short display name from an email address.
pub fn extract_display_name(addr: &str) -> String {
    let addr = addr.trim().trim_matches('"');
    if let Some(idx) = addr.find('<') {
        let name = addr[..idx].trim().trim_matches('"');
        if name.is_empty() {
            addr.trim_matches(|c| c == '<' || c == '>').to_string()
        } else {
            name.to_string()
        }
    } else {
        addr.to_string()
    }
}

/// Resolve date for display and sorting.
pub fn resolve_date(
    date_field: &Option<String>,
    sent_at_field: &Option<String>,
    path: &Path,
) -> (String, String) {
    // Sort key is always formatted in UTC so that emails from different
    // timezones on the same calendar day order by actual instant, not by
    // sender-local wallclock. Display stays in the sender's local time so
    // dates match what the user sees in other clients.
    if let Some(date_str) = date_field {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_str) {
            let display = dt.format("%Y-%m-%d").to_string();
            let sort = dt
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            return (display, sort);
        }
    }

    if let Some(sent_str) = sent_at_field {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(sent_str) {
            let display = dt.format("%Y-%m-%d").to_string();
            let sort = dt
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            return (display, sort);
        }
        // Trailing 'Z' means UTC, so the naive value is already in UTC.
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(sent_str, "%Y-%m-%dT%H:%M:%SZ") {
            let display = dt.format("%Y-%m-%d").to_string();
            let sort = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
            return (display, sort);
        }
    }

    let filename = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if filename.len() >= 10 {
        let date_part = &filename[..10];
        if NaiveDate::parse_from_str(date_part, "%Y-%m-%d").is_ok() {
            if filename.len() >= 15 && filename.as_bytes()[10] == b'-' {
                let time_part = &filename[11..15];
                if time_part.chars().all(|c| c.is_ascii_digit()) && time_part.len() == 4 {
                    let sort = format!("{}T{}:{}:00", date_part, &time_part[..2], &time_part[2..4]);
                    return (date_part.to_string(), sort);
                }
            }
            return (date_part.to_string(), format!("{date_part}T00:00:00"));
        }
    }

    ("".to_string(), "".to_string())
}

// ---------------------------------------------------------------------------
// App types
// ---------------------------------------------------------------------------

/// Per-account TUI state (config, caches, cursor positions).
pub struct AccountState {
    pub account_config: crate::config::AccountConfig,
    pub imap_config: Option<crate::config::ImapConfig>,
    pub smtp_config: Option<crate::config::SmtpConfig>,
    pub graph_config: Option<crate::config::GraphConfig>,
    pub signature_content: Option<String>,
    pub archive_server_name: String,
    pub drafts_dir: Option<PathBuf>,
    pub mailboxes: Vec<MailboxInfo>,
    pub mailbox_counts: Vec<usize>,
    /// Per-mailbox cache of parsed entries. `Arc` so switching mailboxes
    /// or accounts shares the allocation with `App::emails` instead of
    /// deep-cloning thousands of entries (P2); see
    /// `App::with_emails_mut` for the mutation strategy.
    pub email_cache: Vec<Option<Arc<Vec<EmailEntry>>>>,
    pub sidebar_index: usize,
    pub active_mailbox: usize,
    pub list_index: usize,
    /// Identity of the email `list_index` pointed at when this account was
    /// parked. `list_index` alone is a bare position, so a list that grew
    /// or re-sorted while the account was in the background would put the
    /// cursor on a different email on switch-back; `App::restore_cursor`
    /// re-anchors on this reference and falls back to `list_index`.
    pub cursor_ref: Option<MessageRef>,
    pub headers_scroll: u16,
    pub preview_scroll: u16,
    pub selection: std::collections::HashSet<EntryKey>,
    pub search_query: String,
    pub watcher_active: bool,
    pub has_unseen: bool,
    /// Whether this account's store has not yet been opened in the background
    /// (#0003 two-phase startup). `AccountState::new` no longer opens the
    /// store -- the per-account `PRAGMA integrity_check` (~240 ms on a 44 MB
    /// store) used to sum serially before the first paint. It starts `true`,
    /// with `mailbox_counts` all-zero and `outbox` empty, and is cleared by
    /// `BgResult::AccountOpened` once the background open has read the real
    /// counts. The sidebar shows a loading marker while it is set.
    pub opening: bool,
    /// Non-`done` outbox rows for this account (#0037 item 5). Refreshed at
    /// startup and after every send or sync; rendered as a status-bar badge so
    /// a message stuck between SMTP and its Sent copy is visible rather than
    /// silent.
    pub outbox: crate::outbox::OutboxCounts,
    /// Outcome of this account's last completed sync (#0071). Written when
    /// that account's own `BgResult::Fetch`/`BgResult::Sync` lands, so it
    /// survives every later success of a *different* account: the exact race
    /// #0068 lost. Session-scoped, never read from disk.
    pub sync_health: crate::sync_health::SyncHealth,
}

impl AccountState {
    pub fn new(
        account_config: crate::config::AccountConfig,
        email_settings: &crate::config::EmailSettings,
    ) -> Self {
        let imap_config = crate::config::ImapConfig::load(&account_config).ok();
        let smtp_config = crate::config::SmtpConfig::load(&account_config).ok();
        let graph_config = crate::config::GraphConfig::load(&account_config).ok();

        // The account's Markdown signature, appended to the draft body at
        // compose/reply/forward time (#0099). `None` when the account has none
        // or the user turned signatures off.
        let signature_content = if email_settings.include_signature {
            crate::config::resolve_signature_markdown(&account_config, None)
        } else {
            None
        };

        let archive_server_name = account_config
            .mailboxes
            .archive
            .as_ref()
            .map(|m| m.server.as_str())
            .unwrap_or("Archive")
            .to_string();
        let drafts_dir = Some(crate::config::drafts_dir(&account_config.name));

        let mut span = crate::timing::TimingSpan::with_context(
            "AccountState::new",
            account_config.name.clone(),
        );
        // No store is opened here (#0003 two-phase startup). The grouped count
        // query and the outbox read both open `store.sqlite3`, and the first
        // open of each file runs the full `PRAGMA integrity_check` (~240 ms on
        // a 44 MB store); summed serially across accounts that was ~1.2 s of
        // blank terminal before the first paint. `build_mailboxes` is
        // config-only and cheap, so the sidebar shape is known immediately;
        // the counts start at zero, the outbox empty, and `opening` marks the
        // account as not-yet-read. `run_loop` spawns one background open per
        // account after the first `terminal.draw`, and `BgResult::AccountOpened`
        // fills the real counts, refreshes the outbox and clears `opening`.
        let mailboxes = build_mailboxes(&account_config);
        let n = mailboxes.len();
        span.mark(&format!("built {} mailbox(es), store open deferred", n));

        Self {
            account_config,
            imap_config,
            smtp_config,
            graph_config,
            signature_content,
            archive_server_name,
            drafts_dir,
            mailboxes,
            mailbox_counts: vec![0; n],
            email_cache: vec![None; n],
            sidebar_index: 0,
            active_mailbox: 0,
            list_index: 0,
            cursor_ref: None,
            headers_scroll: 0,
            preview_scroll: 0,
            selection: std::collections::HashSet::new(),
            search_query: String::new(),
            watcher_active: false,
            has_unseen: false,
            opening: true,
            outbox: crate::outbox::OutboxCounts::default(),
            sync_health: crate::sync_health::SyncHealth::default(),
        }
    }

    pub fn is_graph(&self) -> bool {
        self.account_config.auth_method == crate::config::AuthMethod::Graph
    }
}

/// Result from a background CLI operation.
#[derive(Debug)]
pub enum BgResult {
    Fetch {
        account_index: usize,
        result: Result<String, String>,
        /// Sender + subject of every genuinely new inbox email the sync
        /// ingested. Drives the opt-in desktop notification (#0009); empty
        /// on failure or when nothing new arrived.
        new_inbox_mail: Vec<crate::notify::NewMailMeta>,
    },
    Sync {
        account_index: usize,
        result: Result<String, String>,
    },
    Send {
        account_index: usize,
        result: Result<String, String>,
    },
    /// An attendee RSVP (#0029) finished sending its METHOD:REPLY.
    Rsvp {
        account_index: usize,
        result: Result<String, String>,
    },
    SendApproved {
        account_index: usize,
        result: Result<String, String>,
    },
    // Archive / Move / Delete / ToggleRead / ToggleFlag are gone (#0039):
    // a mutation no longer fires a per-op server thread that reports back
    // here. It enqueues into the durable `pending_ops` queue and the drain
    // retires it at the sync/fetch resume point, surfacing failures through
    // the sync result rather than a dedicated BgResult.
    ServerSearch {
        result: Result<Vec<SearchHit>, String>,
    },
    /// A background `load_emails` query for one mailbox has
    /// finished (P1 step 2: the load blocks for seconds on large
    /// mailboxes, so it runs off the UI thread). Applied only while
    /// `generation` still matches `App::mailbox_load_generation` --
    /// stale/out-of-order results are dropped (see
    /// `mailbox_loaded_disposition` in `tui/bg.rs`).
    MailboxLoaded {
        account_index: usize,
        mailbox_idx: usize,
        generation: u64,
        entries: Vec<EmailEntry>,
    },
    /// One account's store has been opened in the background and its real
    /// counts read (#0003 two-phase startup). `AccountState::new` defers the
    /// per-account store open (and its `PRAGMA integrity_check`) off the
    /// first-paint path; `run_loop` spawns one open per account after the
    /// first `terminal.draw`, and this carries the grouped mailbox counts and
    /// the outbox badge back so the sidebar fills in. The handler clears
    /// `AccountState::opening`, loads the active account's open mailbox, and
    /// kicks the startup auto-fetch for that account (once the store is known
    /// good, not before). The store open runs the integrity check and, on
    /// failure, the drop-and-rebuild path (#0066) exactly as a foreground open
    /// would -- only the thread it runs on changed.
    AccountOpened {
        account_index: usize,
        counts: Vec<usize>,
        outbox: crate::outbox::OutboxCounts,
    },
}

/// A mailbox target for server search.
#[derive(Debug, Clone)]
pub struct SearchTarget {
    pub server_name: String,
    pub label: String,
}

/// A single search result with source metadata (returned from background task).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub entry: EmailEntry,
    pub fetched: FetchedEmail,
    pub source_label: String,
}

/// A single server search result held in memory.
///
/// It used to carry the path it had been saved to; nothing saves a hit to a
/// file any more (#0038), and naming one without a file is #0050's selector.
#[derive(Debug, Clone)]
pub struct SearchResultEntry {
    pub entry: EmailEntry,
    pub fetched: FetchedEmail,
    pub source_label: String,
}

/// Which control of the Outlook-shape server-search form (#0086b) is focused,
/// or the results list once a search has returned hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOverlayFocus {
    /// One of the form's structured / advanced controls.
    Field(SearchField),
    /// The results list (only reachable once there are hits).
    List,
}

/// The fields of the server-search form (#0086b), in Tab order. The Outlook
/// shape trimmed to the #0086 decisions: a scope toggle (no All-Accounts), the
/// four text fields, two custom date fields, an attachment toggle, and the
/// raw-grammar Advanced power lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchField {
    /// Search In: Current Mailbox / Current Account (toggle).
    Scope,
    From,
    To,
    Subject,
    /// Free text (incl. OR groups / phrases); lowers to `Text` clauses.
    Keywords,
    /// `after:` custom date (YYYY-MM-DD).
    After,
    /// `before:` custom date (YYYY-MM-DD).
    Before,
    /// Attachment toggle -> `Term::HasAttachment`.
    Attachment,
    /// Raw #0086a grammar; when non-empty it takes over and the structured
    /// fields are greyed / ignored.
    Advanced,
}

impl SearchField {
    pub fn label(&self) -> &'static str {
        match self {
            SearchField::Scope => "Search In",
            SearchField::From => "From",
            SearchField::To => "To",
            SearchField::Subject => "Subject",
            SearchField::Keywords => "Keywords",
            SearchField::After => "After",
            SearchField::Before => "Before",
            SearchField::Attachment => "Attachment",
            SearchField::Advanced => "Advanced",
        }
    }

    /// True for the free-text fields that accept typed characters. The scope
    /// and attachment toggles are flipped, not typed into.
    pub fn is_text(&self) -> bool {
        !matches!(self, SearchField::Scope | SearchField::Attachment)
    }

    pub fn next(&self) -> Self {
        match self {
            SearchField::Scope => SearchField::From,
            SearchField::From => SearchField::To,
            SearchField::To => SearchField::Subject,
            SearchField::Subject => SearchField::Keywords,
            SearchField::Keywords => SearchField::After,
            SearchField::After => SearchField::Before,
            SearchField::Before => SearchField::Attachment,
            SearchField::Attachment => SearchField::Advanced,
            SearchField::Advanced => SearchField::Scope,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            SearchField::Scope => SearchField::Advanced,
            SearchField::From => SearchField::Scope,
            SearchField::To => SearchField::From,
            SearchField::Subject => SearchField::To,
            SearchField::Keywords => SearchField::Subject,
            SearchField::After => SearchField::Keywords,
            SearchField::Before => SearchField::After,
            SearchField::Attachment => SearchField::Before,
            SearchField::Advanced => SearchField::Attachment,
        }
    }
}

/// Scope of a server search (#0086b, Q3: per-account only; no All-Accounts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// Just the focused mailbox.
    CurrentMailbox,
    /// Every mailbox of the current account.
    CurrentAccount,
}

impl SearchScope {
    pub fn label(&self) -> &'static str {
        match self {
            SearchScope::CurrentMailbox => "Current Mailbox",
            SearchScope::CurrentAccount => "Current Account",
        }
    }

    pub fn toggled(&self) -> Self {
        match self {
            SearchScope::CurrentMailbox => SearchScope::CurrentAccount,
            SearchScope::CurrentAccount => SearchScope::CurrentMailbox,
        }
    }
}

/// Scratch state for the Outlook-shape server-search form (#0086b).
///
/// The form builds a [`crate::search::Query`] AST directly (no string
/// concatenation) and routes it through the same lowering path the CLI uses:
/// the free-text `keywords` field is parsed by [`crate::search::parse`] and the
/// structured fields become [`crate::search::Flags`], so both feed
/// [`crate::search::from_cli`]. The `advanced` line, when non-empty, takes over
/// entirely and is parsed on its own.
#[derive(Debug, Clone, Default)]
pub struct SearchForm {
    pub scope: SearchScopeState,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub keywords: String,
    pub after: String,
    pub before: String,
    pub attachment: bool,
    pub advanced: String,
}

/// Wrapper so [`SearchForm`] can `derive(Default)` with a sensible scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchScopeState(pub SearchScope);

impl Default for SearchScopeState {
    fn default() -> Self {
        SearchScopeState(SearchScope::CurrentAccount)
    }
}

impl SearchForm {
    /// True when the Advanced line is the active input (it takes precedence
    /// over every structured field once it holds anything non-blank).
    pub fn advanced_active(&self) -> bool {
        !self.advanced.trim().is_empty()
    }

    /// True when nothing has been entered anywhere: neither the Advanced line
    /// nor any structured field nor the attachment toggle. An empty form is a
    /// no-op (Enter does not dispatch a search).
    pub fn is_blank(&self) -> bool {
        !self.advanced_active()
            && self.from.trim().is_empty()
            && self.to.trim().is_empty()
            && self.subject.trim().is_empty()
            && self.keywords.trim().is_empty()
            && self.after.trim().is_empty()
            && self.before.trim().is_empty()
            && !self.attachment
    }

    /// A mutable handle to the text of the focused field, or `None` for the
    /// toggles (scope / attachment) which are flipped rather than typed into.
    pub fn text_field_mut(&mut self, field: SearchField) -> Option<&mut String> {
        match field {
            SearchField::From => Some(&mut self.from),
            SearchField::To => Some(&mut self.to),
            SearchField::Subject => Some(&mut self.subject),
            SearchField::Keywords => Some(&mut self.keywords),
            SearchField::After => Some(&mut self.after),
            SearchField::Before => Some(&mut self.before),
            SearchField::Advanced => Some(&mut self.advanced),
            SearchField::Scope | SearchField::Attachment => None,
        }
    }

    /// Build the [`crate::search::Query`] this form describes.
    ///
    /// When the Advanced line is active it is parsed directly. Otherwise the
    /// structured fields lower through [`crate::search::from_cli`]: `keywords`
    /// is the positional grammar (so `a OR b` and quoted phrases work) and the
    /// rest are flags. Returns the `ParseError` / validation message verbatim
    /// for the overlay to surface. Returns `Ok(None)` for a blank form.
    pub fn build_query(&self) -> Result<Option<crate::search::Query>, String> {
        if self.advanced_active() {
            return crate::search::parse(self.advanced.trim())
                .map(Some)
                .map_err(|e| e.to_string());
        }
        if self.is_blank() {
            return Ok(None);
        }
        let opt = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        let flags = crate::search::Flags {
            from: opt(&self.from),
            to: opt(&self.to),
            cc: None,
            subject: opt(&self.subject),
            body: None,
            filename: None,
            has_attachment: self.attachment,
            after: opt(&self.after),
            before: opt(&self.before),
        };
        crate::search::from_cli(self.keywords.trim(), &flags).map(Some)
    }
}

// ---------------------------------------------------------------------------
// Compose wizard
// ---------------------------------------------------------------------------

/// Which field of the compose wizard is currently focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
    /// The optional inline body (#0097). Multi-line and the last field before
    /// submit: a non-empty body is written straight into the draft, an empty
    /// one falls back to opening `$EDITOR` on the draft.
    Body,
}

impl ComposeField {
    pub fn label(&self) -> &'static str {
        match self {
            ComposeField::To => "To",
            ComposeField::Cc => "Cc",
            ComposeField::Bcc => "Bcc",
            ComposeField::Subject => "Subject",
            ComposeField::Body => "Body",
        }
    }

    pub fn is_address(&self) -> bool {
        matches!(
            self,
            ComposeField::To | ComposeField::Cc | ComposeField::Bcc
        )
    }

    pub fn next(&self) -> Self {
        match self {
            ComposeField::To => ComposeField::Cc,
            ComposeField::Cc => ComposeField::Bcc,
            ComposeField::Bcc => ComposeField::Subject,
            ComposeField::Subject => ComposeField::Body,
            ComposeField::Body => ComposeField::To,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            ComposeField::To => ComposeField::Body,
            ComposeField::Cc => ComposeField::To,
            ComposeField::Bcc => ComposeField::Cc,
            ComposeField::Subject => ComposeField::Bcc,
            ComposeField::Body => ComposeField::Subject,
        }
    }
}

/// Wizard mode: blank new draft, forward of a stored message, or edit of the
/// recipient/subject fields of an existing draft in place.
///
/// Both non-blank modes name their subject on the substrate that owns it
/// (#0052): a forward carries the [`MessageRef`] of the row it quotes, the way
/// `mp forward <selector>` names a row, and an edit carries the draft's `id:`,
/// which the drafts index turns into a path at the moment it is needed. A
/// cached path would be a second copy of an answer the index already owns, and
/// `$EDITOR` can rename the file between opening the wizard and submitting it.
#[derive(Debug, Clone)]
pub enum ComposeMode {
    New,
    Forward { msg: MessageRef },
    EditDraft { id: String },
}

/// A fuzzy-matched contact suggestion under the focused address field.
#[derive(Debug, Clone)]
pub struct ComposeSuggestion {
    pub address: String,
    pub display_name: String,
    /// Tier marker for visual: 0=received, 1=sent-cc, 2=sent-to.
    pub tier: u8,
}

/// State for the compose wizard overlay.
pub struct ComposeWizard {
    pub mode: ComposeMode,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    /// The inline body typed in the wizard (#0097). Multi-line: `Enter` on the
    /// Body field inserts a newline rather than submitting. Only a `New`
    /// compose exposes it (see [`ComposeWizard::has_body_field`]); a non-empty
    /// value is written into the draft body and skips the `$EDITOR` round-trip.
    pub body: String,
    pub focus: ComposeField,
    /// Fuzzy-matched suggestions for the currently-focused field
    /// (empty for Subject/Body or when no cache exists).
    pub suggestions: Vec<ComposeSuggestion>,
    pub suggestion_idx: usize,
    /// The contact index for the active account, loaded once when the
    /// wizard opens. `None` means "no cache yet — run rebuild first".
    pub contacts: Option<crate::contacts::ContactIndex>,
}

impl ComposeWizard {
    /// Whether the inline body field participates in this wizard (#0097).
    ///
    /// Only a brand-new draft can take an inline body: `Forward` always opens
    /// `$EDITOR` on the quoted draft, and `EditDraft` only rewrites the header
    /// fields in place. Field navigation skips `Body` when this is false, so
    /// the field never receives focus outside a `New` compose.
    pub fn has_body_field(&self) -> bool {
        matches!(self.mode, ComposeMode::New)
    }

    /// The field after `focus` in Tab order, skipping `Body` when this wizard
    /// has no body field.
    pub fn next_field(&self) -> ComposeField {
        let next = self.focus.next();
        if next == ComposeField::Body && !self.has_body_field() {
            next.next()
        } else {
            next
        }
    }

    /// The field before `focus` in Tab order, skipping `Body` when this wizard
    /// has no body field.
    pub fn prev_field(&self) -> ComposeField {
        let prev = self.focus.prev();
        if prev == ComposeField::Body && !self.has_body_field() {
            prev.prev()
        } else {
            prev
        }
    }
}

/// Which pane currently has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    Sidebar,
    #[default]
    List,
    Headers,
    Preview,
    Search,
    ComposeWizard,
}

/// The active top-level view (#0033). `Mail` is the full email client (the
/// original TUI); `Contacts` (#0033) and `Calendar` (#0034) are the two
/// content panes. Each owns its state (`MailView` / `ContactsView` /
/// `CalendarView`) on `App`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Mail,
    Contacts,
    Calendar,
}

impl View {
    /// The three views in switcher / cycle order.
    pub const ALL: &'static [View] = &[View::Mail, View::Contacts, View::Calendar];

    /// Short chip label for the bottom-left view switcher.
    pub fn label(self) -> &'static str {
        match self {
            View::Mail => "mail",
            View::Contacts => "contacts",
            View::Calendar => "calendar",
        }
    }

    /// The leader continuation key that switches to this view (`g <key>`).
    pub fn switch_key(self) -> char {
        match self {
            View::Mail => 'm',
            View::Contacts => 'c',
            View::Calendar => 'a',
        }
    }
}

/// Snapshot of the mail view's proxy state (#0033).
///
/// Mirrors the `AccountState` proxy pattern: the mail view's working fields
/// live *flat* on `App` (so the ~550 existing call sites are untouched), and
/// this struct is where they are parked when the user switches to another
/// view and restored when they switch back. `App::save_to_mail_view` /
/// `App::load_from_mail_view` are the sync points, exactly like
/// `save_to_account` / `load_from_account`.
///
/// Only the *transient* mail-view state that is not already account-proxied
/// needs parking (focus + the pending leader). The account-proxied fields
/// (`emails`, `mailboxes`, cursor, selection, ...) are restored from the
/// active `AccountState` on demand and never diverge across a view switch, so
/// they do not need duplicating here.
#[derive(Debug, Clone, Default)]
pub struct MailView {
    pub focus: Focus,
}

/// State for the Contacts view (#0033).
///
/// A read-only list + fuzzy search + detail pane over the local contacts index
/// (`crate::contacts`). Sibling of [`MailView`]: it owns the Contacts view's
/// transient state and lives on `App` beside `mail_view`. The index is loaded
/// lazily from the on-disk cache the first time the user switches to the view
/// (`App::ensure_contacts_loaded`); a manual refresh key rebuilds it.
///
/// `matches` is the ordered list of contact addresses currently shown (the
/// result of fuzzy-matching `query` against `index`), recomputed whenever the
/// query or index changes via `App::recompute_contact_matches`. `list_index`
/// indexes into `matches`; the detail pane shows `index.contacts[matches[list_index]]`.
#[derive(Debug, Clone, Default)]
pub struct ContactsView {
    /// True once the on-disk cache load has been attempted (whether or not a
    /// cache existed). Gates the lazy first-switch load.
    pub loaded: bool,
    /// The loaded contact index for the active account. `None` means no cache
    /// yet (the pane shows a "run rebuild" hint).
    pub index: Option<crate::contacts::ContactIndex>,
    /// Incremental fuzzy-filter query (edited while `searching`).
    pub query: String,
    /// Whether the search input is focused (type-to-filter mode).
    pub searching: bool,
    /// Addresses of the current matches, in rank/score order.
    pub matches: Vec<String>,
    /// Cursor into `matches`.
    pub list_index: usize,
}

/// One agenda row of the Calendar view (#0034): a single logical event
/// derived from the local iMIP files on disk.
///
/// Plain data (no borrows, `Send`) so the loader is unit-testable and could be
/// moved off the UI thread later. `event` is the frontmatter block the shared
/// event card (`ui::preview::event_card_lines`) renders; the derived fields
/// carry what the agenda list itself needs.
#[derive(Debug, Clone)]
pub struct CalendarEvent {
    /// The store row the winning copy of this event came from, so an RSVP
    /// from the agenda addresses the same message the mail list would
    /// (#0038 scope item 6 replaced the `.md` path with it).
    pub msg: MessageRef,
    /// The `event:` frontmatter block, rendered by the shared event card.
    pub event: crate::types::EventFrontmatter,
    /// Email subject, used as the row title when the event has no `summary`.
    pub subject: String,
    /// UTC-normalised `YYYY-MM-DDTHH:MM:SS` sort key, empty when the start is
    /// missing or unparseable (those rows sort last). Display stays local, the
    /// sort key is always UTC -- same rule as `resolve_date` (#0024).
    pub start_sort: String,
    /// UTC-normalised end key (same format), empty when unknown. Used to keep
    /// an in-progress event in the "upcoming" view until it actually ends; an
    /// all-day event with no explicit end gets the start of the next local day.
    pub end_sort: String,
    /// Local, human-readable start (`YYYY-MM-DD HH:MM`, or the date alone for
    /// all-day events). Empty when the start is unknown.
    pub start_display: String,
    /// True when the winning copy came from the Sent mailbox, i.e. we are the
    /// organizer (no own-RSVP, and RSVP is refused).
    pub is_organizer: bool,
    /// True when a `METHOD:CANCEL` for this event's `(UID, RECURRENCE-ID)`
    /// exists in the store with a sequence at least as high as this event's
    /// (#0031). The row stays listed and readable: a cancellation tombstones
    /// the event, it never deletes it. Mirrors `event.cancelled`, which the
    /// shared card renders.
    pub cancelled: bool,
}

/// State for the Calendar view (#0034).
///
/// Sibling of [`ContactsView`]: a read-only agenda over the events the local
/// iMIP traffic already produced (received invites, our own sent invites),
/// loaded lazily on the first switch to the view (`App::ensure_calendar_loaded`)
/// and rebuilt by the manual refresh key. Scoped to the active account and
/// reset on `switch_account`.
///
/// `events` holds every event found on disk, sorted by start instant with
/// undated ones last; `visible` is the subset currently shown (upcoming only
/// unless `show_past`), recomputed by `App::recompute_calendar_visible`.
/// `list_index` indexes into `visible`, exactly like the mail list.
#[derive(Debug, Clone, Default)]
pub struct CalendarView {
    /// True once the on-disk walk has been attempted. Gates the lazy load.
    pub loaded: bool,
    /// Every event found for the active account, sorted (undated last).
    pub events: Vec<CalendarEvent>,
    /// Indices into `events` forming the current agenda view.
    pub visible: Vec<usize>,
    /// Cursor into `visible`.
    pub list_index: usize,
    /// When true, past events are listed too (toggled with `t`).
    pub show_past: bool,
}

/// Messages that drive state transitions (TEA pattern).
#[derive(Debug)]
pub enum Message {
    Key(crossterm::event::KeyEvent),
    Resize(u16, u16),
    Quit,
    /// Background watcher detected new mail for a given account.
    MailboxChanged {
        account_index: usize,
    },
}

/// Behavioral kind of a mailbox (used for action differentiation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxKind {
    Inbox,
    Drafts,
    Sent,
    Archive,
    Extra,
}

/// A sidebar mailbox: how it is named on screen, on the server, and in the
/// store.
///
/// `id` is the store key -- the `messages.mailbox` value ingest wrote and the
/// mailbox segment of an `mp://` selector. It replaced a `dir: PathBuf` whose
/// leaf everything actually wanted, and whose directory had not been read
/// since the store cutover; for an unmapped mailbox the leaf was a *slug* of
/// the server name while ingest filed the rows under the server name itself,
/// so such a mailbox listed empty (#0064).
#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub label: String,
    pub icon: &'static str,
    pub id: String,
    pub kind: MailboxKind,
    pub server_name: Option<String>,
}

/// Side-effects that the main loop must execute (keeps update pure).
#[derive(Debug)]
pub enum Action {
    EditCurrent,
    Reply(bool),
    Send,
    SendApproved,
    NewDraft,
    Approve,
    /// Batch variant for `Action::Approve`: the indexed `id:` of every draft
    /// in the selection (#0052). Draft ids rather than [`MessageRef`]s
    /// because approving is a write to a draft file, and a draft has no
    /// `messages` row to name it by.
    BatchApprove(Vec<String>),
    /// Demote a single approved draft back to `draft` status (#0021).
    MarkDraft,
    /// Batch variant for `Action::MarkDraft` -- run mark_as_draft over
    /// the current selection, by indexed draft id (see [`Action::BatchApprove`]).
    BatchMarkDraft(Vec<String>),
    Archive,
    Delete,
    BatchArchive(Vec<MessageRef>),
    BatchDelete(Vec<MessageRef>),
    /// Delete every draft in the selection by indexed `id:` (#0073). Draft ids
    /// rather than [`MessageRef`]s because a draft has no `messages` row, the
    /// same reason [`Action::BatchApprove`] takes ids: deleting a draft is a
    /// local file removal, not a store mutation.
    BatchDeleteDrafts(Vec<String>),
    /// Quick-move emails to another mailbox (#0018). `msgs` is the
    /// selection (or the cursor email); `dest_idx` indexes
    /// `App::mailboxes`.
    MoveToMailbox {
        msgs: Vec<MessageRef>,
        dest_idx: usize,
    },
    ToggleRead,
    MarkAsRead,
    BatchToggleRead(Vec<MessageRef>),
    /// Toggle the `\Flagged` star on the cursor message (#0007).
    ToggleFlag,
    /// Toggle the `\Flagged` star on the whole selection (#0007). Flagging
    /// wins when any is unflagged, mirroring the read toggle's rule.
    BatchToggleFlag(Vec<MessageRef>),
    /// Copy the canonical `mp://` selector of the selected entry to the system
    /// clipboard (#0050 scope item 7).
    ///
    /// It replaced `CopyPath`, which copied the `.md` file the store stopped
    /// writing with #0038. A selector is the better thing to hold anyway: it
    /// survives a restart, a re-sync and a draft rename, and it pastes
    /// straight into `mp archive`, `mp reply` or `mp send`.
    CopyMessageRef,
    /// Open the newest log file from `logs_dir()` in `$EDITOR` (#0025).
    OpenLogFile,
    /// Open the global config file (`config_path()`) in `$EDITOR`. Changes
    /// apply on restart (theme is `OnceLock`; no hot-reload).
    OpenConfigFile,
    OpenAttachment(PathBuf),
    SaveAttachments {
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    Fetch,
    /// Load one mailbox of the active account off the UI thread (P1
    /// step 2). Queued by `App::request_mailbox_load` on
    /// every cache-miss switch/reload; the result arrives as
    /// `BgResult::MailboxLoaded` carrying the same `generation`.
    LoadMailbox {
        mailbox_idx: usize,
        generation: u64,
    },
    /// Quick-sync a specific account by index. Used for the per-account
    /// startup auto-fetch (#0001).
    FetchAccount(usize),
    Sync,
    ServerSearch {
        /// The already-parsed AST the form (or the overlay's raw line) built,
        /// so the lowering path renders it without re-parsing (#0086b).
        query: crate::search::Query,
        targets: Vec<SearchTarget>,
    },
    SearchResultOpen,
    SearchResultReply(bool),
    SearchResultForward,
    SearchResultArchive,
    SearchResultOpenInBrowser,
    OpenHtmlInBrowser(PathBuf),
    /// Open the compose wizard overlay (new or forward).
    OpenComposeWizard(ComposeMode),
    /// Commit the wizard: write the draft file and launch $EDITOR.
    ComposeWizardSubmit,
    /// Close the wizard without writing anything.
    ComposeWizardCancel,
    /// RSVP to a received invite (#0029): send a METHOD:REPLY to the
    /// organizer on a background thread and flip local `event.rsvp`.
    Rsvp {
        msg: MessageRef,
        choice: RsvpChoice,
    },
    /// Compose a new draft to a contact (#0033). Opens the compose wizard
    /// (Overlay) seeded with `to`; the overlay floats above whatever view is
    /// active, so the user stays in Contacts and returns to it on cancel.
    ComposeToContact {
        to: String,
    },
    /// Export a contact to a `.vcf` and attach it to a new draft (#0033).
    SendContactVcard {
        contact: crate::contacts::Contact,
    },
    /// Copy a contact's bare email address to the system clipboard. Carries the
    /// address so the clipboard side effect stays in `actions.rs` while the
    /// selection is resolved in the key executor (like `SendContactVcard`).
    CopyContactEmail {
        address: String,
    },
    /// Open the invite email an agenda row was derived from in `$EDITOR`
    /// (#0034). Carries its own message reference: the event may live in any
    /// mailbox of the active account, not just the one the mail list shows.
    OpenEventSource {
        msg: MessageRef,
    },
    /// Append a file path to the cursor draft's `attachments:` frontmatter
    /// (#0098). Carries the raw path the user typed at the attach prompt; the
    /// handler resolves the cursor draft, appends the entry, and refreshes the
    /// drafts index. The path's existence was already checked at the prompt,
    /// so what reaches here is a path that was on disk when Enter was pressed.
    AttachFileToDraft {
        path: String,
    },
}

/// Which destructive action a confirmation dialog is guarding.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    Approve,
    MarkDraft,
    Archive,
    Delete,
    Send,
    SendApproved,
}

/// Background RSVP send result (#0029).
#[derive(Debug)]
pub struct RsvpDone {
    pub account_index: usize,
    pub result: Result<String, String>,
}

/// A choice in the three-option RSVP overlay (#0029).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsvpChoice {
    Accept,
    Tentative,
    Decline,
}

impl RsvpChoice {
    pub fn label(self) -> &'static str {
        match self {
            RsvpChoice::Accept => "Accept",
            RsvpChoice::Tentative => "Tentative",
            RsvpChoice::Decline => "Decline",
        }
    }

    /// Map to the library RSVP status enum.
    pub fn to_rsvp(self) -> crate::invite::Rsvp {
        match self {
            RsvpChoice::Accept => crate::invite::Rsvp::Accepted,
            RsvpChoice::Tentative => crate::invite::Rsvp::Tentative,
            RsvpChoice::Decline => crate::invite::Rsvp::Declined,
        }
    }
}

/// State for the small RSVP overlay opened with `V` on a received invite.
/// Three choices (Accept / Tentative / Decline) plus Esc to cancel.
pub struct RsvpOverlay {
    /// The invite email being answered.
    pub msg: MessageRef,
    /// Event summary, shown in the overlay title.
    pub summary: String,
    /// Cursor over the three choices.
    pub selected: usize,
}

/// One message of the conversation shown in the thread overlay (#0008).
///
/// A flat row of display data plus the [`MessageRef`] the Enter key opens.
/// `mailbox` is the store key (`messages.mailbox`), carried so the jump can
/// resolve the sidebar index without a second store read; the overlay renders
/// its sidebar label. `current` marks the message the overlay was opened from,
/// so the reader keeps their place in the conversation.
#[derive(Debug, Clone)]
pub struct ThreadEntry {
    pub msg: MessageRef,
    pub mailbox: String,
    pub from: String,
    pub date_display: String,
    pub read: bool,
    pub answered: bool,
    pub forwarded: bool,
    pub flagged: bool,
    /// True for the message the overlay was opened from.
    pub current: bool,
}

/// State for the conversation (threading) overlay opened with `T` (#0008).
///
/// Read-only list of every message ingest put in the same `thread_id`, oldest
/// first, across every mailbox of the active account. It is the "list the
/// related emails" half of [#TKT-0051]: grouping a conversation, derived from
/// the `In-Reply-To` / `References` chain ingest already resolved. Enter opens
/// the highlighted message (switching mailbox when it lives in another), Esc
/// closes.
pub struct ThreadOverlay {
    /// Subject of the message the overlay was opened from, shown in the title.
    pub subject: String,
    /// The conversation, oldest first.
    pub messages: Vec<ThreadEntry>,
    /// Cursor into `messages`.
    pub selected: usize,
}

/// Data for rendering the confirmation dialog overlay.
#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pub title: String,
    pub detail: String,
    pub action: ConfirmAction,
}

/// One selectable row of the command palette (#0100): a runnable action and
/// the label the fuzzy filter matches against and the list renders.
#[derive(Debug, Clone, Copy)]
pub struct PaletteEntry {
    pub action: super::KeyAction,
    pub label: &'static str,
}

/// Scratch state for the command palette overlay (`:` / `Ctrl+p`, #0100).
///
/// A text-input fuzzy finder over the runnable `KeyAction` catalogue
/// ([`super::palette_actions`]): the user types part of an action name and runs
/// it with Enter, without recalling its chord. It is the recall path the
/// mnemonic families' fast path complements, not a second action list — the
/// entries derive from the same `KEYMAP` the help/hint surfaces read.
///
/// Modelled on [`MailboxPicker`]: `entries` is the full catalogue captured when
/// the overlay opens, `filtered` holds the indices that match `query` (in
/// catalogue order), and `selected` is the cursor into `filtered`.
pub struct CommandPalette {
    /// The full runnable catalogue, snapshotted on open.
    pub entries: Vec<PaletteEntry>,
    /// Type-to-filter query (family leaders typed here are literal).
    pub query: String,
    /// Indices into `entries` whose label fuzzy-matches `query`.
    pub filtered: Vec<usize>,
    /// Cursor into `filtered`.
    pub selected: usize,
}

impl CommandPalette {
    /// Build the palette from the current [`super::palette_actions`] catalogue,
    /// with an empty query (so every entry is visible) and the cursor at the
    /// top.
    pub fn new() -> Self {
        let entries = super::palette_actions()
            .into_iter()
            .map(|(action, label)| PaletteEntry { action, label })
            .collect::<Vec<_>>();
        let filtered = (0..entries.len()).collect();
        Self {
            entries,
            query: String::new(),
            filtered,
            selected: 0,
        }
    }

    /// The action under the cursor, if any.
    pub fn selected_action(&self) -> Option<super::KeyAction> {
        self.filtered
            .get(self.selected)
            .map(|&i| self.entries[i].action)
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

/// The single active modal overlay, if any (#0032).
///
/// Replaces the former set of independent `Option`/`bool` overlay fields on
/// `App` (`confirm_dialog`, `show_help`, `show_activity_overlay`,
/// `show_search_overlay`, `compose_wizard`, `attachment_picker`, `dir_picker`,
/// `mailbox_picker`, `rsvp_overlay`, `persistent_error`). Exactly one overlay
/// is renderable at a time by construction: `view` matches on this and
/// `handle_key` dispatches on it.
///
/// Variants that carry per-overlay data own their struct payload. The three
/// former bare-`bool` overlays (`Help`, `Activity`, `Search`) are unit
/// variants — their scratch state (scroll offsets, filter strings, server
/// search buffers) still lives in sibling `App` fields, since those buffers
/// outlive a single open/close only as reset-on-open scratch space.
#[derive(Default)]
pub enum Overlay {
    /// No overlay: the normal three-pane mail view has full focus.
    #[default]
    None,
    /// Destructive-action confirmation (`y`/`n`).
    Confirm(ConfirmDialog),
    /// Help overlay (`?`). Scroll/filter state in `App::help_*`.
    Help,
    /// Activity-log overlay (`L`). Scroll/filter state in `App::activity_*`.
    Activity,
    /// Server (IMAP) search overlay (`f`). State in `App::server_search_*`.
    Search,
    /// Compose wizard (new / forward / edit-recipients).
    Compose(ComposeWizard),
    /// Attachment picker (`o` open / `O` save).
    Attachment(AttachmentPicker),
    /// Directory picker for saving attachments.
    Dir(DirPicker),
    /// Fuzzy mailbox picker for quick-move (`M`).
    Mailbox(MailboxPicker),
    /// RSVP overlay for a received invite (`V`, #0029).
    Rsvp(RsvpOverlay),
    /// Conversation / threading overlay (`T`, #0008).
    Thread(ThreadOverlay),
    /// Command palette (`:` / `Ctrl+p`, #0100): a fuzzy finder over the
    /// runnable `KeyAction` catalogue.
    Palette(CommandPalette),
    /// Persistent error requiring explicit dismissal.
    Error(PersistentError),
}

impl Overlay {
    /// Whether any overlay is currently active.
    pub fn is_active(&self) -> bool {
        !matches!(self, Overlay::None)
    }
}

/// Persistent error notification (requires user action to dismiss).
pub struct PersistentError {
    pub message: String,
}

/// Whether the attachment picker is opening or saving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentPickerMode {
    Open,
    Save,
}

/// Overlay state for choosing among multiple attachments.
pub struct AttachmentPicker {
    pub files: Vec<PathBuf>,
    pub selected: usize,
    pub mode: AttachmentPickerMode,
    /// Set of selected indices (used in Save mode for multi-select).
    pub selected_set: HashSet<usize>,
}

/// Overlay state for the quick-move mailbox picker (#0018): a fuzzy
/// type-to-filter list of destination mailboxes, opened with `M` from
/// the email list.
pub struct MailboxPicker {
    /// Type-to-filter query.
    pub query: String,
    /// Candidate destinations: `(mailbox index into App::mailboxes,
    /// label)`. Excludes the active mailbox and any mailbox without a
    /// server-side folder.
    pub candidates: Vec<(usize, String)>,
    /// Indices into `candidates` matching the current query.
    pub filtered: Vec<usize>,
    /// Cursor position in `filtered`.
    pub selected: usize,
    /// Emails to move (current selection, or the cursor email).
    pub msgs: Vec<MessageRef>,
}

/// Whether the directory picker is in zoxide or browser mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirPickerMode {
    Zoxide,
    Browser,
}

/// Overlay for picking a target directory (for saving attachments).
pub struct DirPicker {
    pub mode: DirPickerMode,
    /// Text input for zoxide query.
    pub query: String,
    /// Results from `zoxide query --list`.
    pub zoxide_results: Vec<PathBuf>,
    /// Whether zoxide is available on this system.
    pub zoxide_available: bool,
    /// Current directory in browser mode.
    pub current_dir: PathBuf,
    /// Subdirectories of `current_dir` (browser mode listing).
    pub dir_entries: Vec<PathBuf>,
    /// Cursor position in the result/entry list.
    pub selected: usize,
    /// The attachment files to save (carried from the attachment picker).
    pub sources: Vec<PathBuf>,
}

pub const STATUS_LOG_CAPACITY: usize = 100;

#[derive(Debug, Clone)]
pub enum StatusLevel {
    Info,
    Success,
    Warning,
    Error,
    Progress,
}

#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub timestamp: chrono::DateTime<chrono::Local>,
    pub message: String,
    pub level: StatusLevel,
}

// ---------------------------------------------------------------------------
// Mailbox helpers (free functions)
// ---------------------------------------------------------------------------

pub fn build_mailboxes(config: &crate::config::AccountConfig) -> Vec<MailboxInfo> {
    let mut result = Vec::new();

    result.push(MailboxInfo {
        label: "Inbox".to_string(),
        icon: "\u{f0172}",
        id: MailboxRole::Inbox.as_str().to_string(),
        kind: MailboxKind::Inbox,
        server_name: config.mailboxes.inbox.as_ref().map(|m| m.server.clone()),
    });

    result.push(MailboxInfo {
        label: "Drafts".to_string(),
        icon: "\u{f03eb}",
        id: crate::selector::DRAFTS_MAILBOX.to_string(),
        kind: MailboxKind::Drafts,
        server_name: None,
    });

    result.push(MailboxInfo {
        label: "Sent".to_string(),
        icon: "\u{f046b}",
        id: MailboxRole::Sent.as_str().to_string(),
        kind: MailboxKind::Sent,
        server_name: config.mailboxes.sent.as_ref().map(|m| m.server.clone()),
    });

    result.push(MailboxInfo {
        label: "Archive".to_string(),
        icon: "\u{f013c}",
        id: MailboxRole::Archive.as_str().to_string(),
        kind: MailboxKind::Archive,
        server_name: config.mailboxes.archive.as_ref().map(|m| m.server.clone()),
    });

    if let Some(ref extras) = config.mailboxes.extra {
        for m in extras {
            result.push(MailboxInfo {
                label: m.server.clone(),
                icon: "\u{f0247}",
                id: MailboxRole::Other(m.server.clone()).as_str().to_string(),
                kind: MailboxKind::Extra,
                server_name: Some(m.server.clone()),
            });
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // -----------------------------------------------------------------------
    // PreviewBody content stamp (#0093)
    // -----------------------------------------------------------------------

    /// The wrapped-line memo keys on this stamp, so it must move exactly when
    /// the shown content moves and hold still on an unchanged refill (the
    /// skip-draft and primed paths refill the same text every frame).
    #[test]
    fn preview_body_epoch_bumps_on_change_and_holds_on_refill() {
        let mut pb = PreviewBody::default();
        let start = pb.epoch();

        // A new body bumps the stamp.
        pb.fill(None, "hello".to_string());
        let after_first = pb.epoch();
        assert_ne!(after_first, start, "first fill must bump");

        // Refilling the identical (key, text) must not bump: this is the
        // every-frame skip-draft/primed case.
        pb.fill(None, "hello".to_string());
        assert_eq!(pb.epoch(), after_first, "unchanged refill holds the stamp");

        // Different text bumps.
        pb.fill(None, "world".to_string());
        assert_ne!(pb.epoch(), after_first, "changed text bumps");
    }

    // -----------------------------------------------------------------------
    // Search form -> AST (#0086b)
    // -----------------------------------------------------------------------

    use crate::search::{Clause, Term};

    #[test]
    fn search_form_empty_is_noop() {
        // A blank form builds nothing: Enter must not dispatch a search.
        let form = SearchForm::default();
        assert!(form.is_blank());
        assert_eq!(form.build_query().unwrap(), None);
    }

    #[test]
    fn search_form_default_scope_is_current_account() {
        assert_eq!(
            SearchForm::default().scope.0,
            SearchScope::CurrentAccount
        );
    }

    #[test]
    fn search_form_builds_every_structured_field() {
        let form = SearchForm {
            from: "boss@corp.com".to_string(),
            to: "me@corp.com".to_string(),
            subject: "budget".to_string(),
            keywords: "invoice OR receipt".to_string(),
            after: "2026-01-01".to_string(),
            before: "2026-07-01".to_string(),
            attachment: true,
            ..Default::default()
        };
        let q = form.build_query().unwrap().expect("non-blank form");
        // No string concatenation: each field is its own AST clause. `keywords`
        // parses as the positional grammar, so `a OR b` is an Or clause.
        assert!(q
            .clauses
            .contains(&Clause::Single(Term::From("boss@corp.com".into()))));
        assert!(q
            .clauses
            .contains(&Clause::Single(Term::To("me@corp.com".into()))));
        assert!(q
            .clauses
            .contains(&Clause::Single(Term::Subject("budget".into()))));
        assert!(q.clauses.contains(&Clause::Or(vec![
            Term::Text("invoice".into()),
            Term::Text("receipt".into()),
        ])));
        assert!(q
            .clauses
            .contains(&Clause::Single(Term::After("2026-01-01".into()))));
        assert!(q
            .clauses
            .contains(&Clause::Single(Term::Before("2026-07-01".into()))));
        assert!(q.clauses.contains(&Clause::Single(Term::HasAttachment)));
    }

    #[test]
    fn search_form_attachment_toggle_only() {
        // The attachment toggle alone is a real (non-blank) query.
        let form = SearchForm {
            attachment: true,
            ..Default::default()
        };
        assert!(!form.is_blank());
        let q = form.build_query().unwrap().unwrap();
        assert_eq!(q.clauses, vec![Clause::Single(Term::HasAttachment)]);
    }

    #[test]
    fn search_form_rejects_bad_date() {
        let form = SearchForm {
            after: "not-a-date".to_string(),
            ..Default::default()
        };
        let err = form.build_query().unwrap_err();
        assert!(err.contains("after"), "date error surfaced: {err}");
    }

    #[test]
    fn search_form_advanced_overrides_structured_fields() {
        // Rule: a non-blank Advanced line takes over; the structured fields are
        // ignored (greyed in the UI). Here `from`/`subject` are set but the
        // built query is exactly what Advanced parses to.
        let form = SearchForm {
            from: "ignored@corp.com".to_string(),
            subject: "ignored".to_string(),
            advanced: "has:attachment".to_string(),
            ..Default::default()
        };
        assert!(form.advanced_active());
        let q = form.build_query().unwrap().unwrap();
        assert_eq!(q.clauses, vec![Clause::Single(Term::HasAttachment)]);
    }

    #[test]
    fn search_form_advanced_parse_error_surfaces() {
        // A malformed Advanced grammar returns the ParseError Display verbatim
        // (its three-line caret message), for the overlay to show.
        let form = SearchForm {
            advanced: "from:x (invoice OR".to_string(),
            ..Default::default()
        };
        let err = form.build_query().unwrap_err();
        assert!(
            err.contains("invalid search query"),
            "ParseError Display surfaced: {err}"
        );
        assert!(err.contains('^'), "caret present: {err}");
    }

    #[test]
    fn search_field_focus_cycles_through_all_nine() {
        // Tab forward from the first field visits every field once and wraps.
        let mut f = SearchField::Scope;
        let mut seen = vec![f];
        for _ in 0..8 {
            f = f.next();
            seen.push(f);
        }
        assert_eq!(seen.len(), 9);
        assert_eq!(f.next(), SearchField::Scope, "wraps to the start");
        // Shift-Tab is the exact inverse.
        assert_eq!(SearchField::Scope.prev(), SearchField::Advanced);
        assert_eq!(SearchField::From.prev(), SearchField::Scope);
        // Every field is distinct in the cycle.
        let mut uniq = seen.clone();
        uniq.sort_by_key(|f| f.label());
        uniq.dedup_by_key(|f| f.label());
        assert_eq!(uniq.len(), 9);
    }

    #[test]
    fn search_field_toggles_are_not_text() {
        assert!(!SearchField::Scope.is_text());
        assert!(!SearchField::Attachment.is_text());
        assert!(SearchField::From.is_text());
        assert!(SearchField::Advanced.is_text());
    }

    // -----------------------------------------------------------------------
    // extract_display_name
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_display_name_full_address() {
        assert_eq!(extract_display_name("John Doe <john@x.com>"), "John Doe");
    }

    #[test]
    fn test_extract_display_name_bare_email() {
        assert_eq!(extract_display_name("john@x.com"), "john@x.com");
    }

    #[test]
    fn test_extract_display_name_no_name() {
        assert_eq!(extract_display_name("<john@x.com>"), "john@x.com");
    }

    #[test]
    fn test_extract_display_name_quoted() {
        assert_eq!(
            extract_display_name("\"John Doe\" <john@x.com>"),
            "John Doe"
        );
    }

    #[test]
    fn test_extract_display_name_empty() {
        assert_eq!(extract_display_name(""), "");
    }

    // -----------------------------------------------------------------------
    // resolve_date
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_date_rfc2822() {
        let date = Some("Mon, 01 Jan 2024 12:00:00 +0000".to_string());
        let (display, sort) = resolve_date(&date, &None, Path::new("test.md"));
        assert_eq!(display, "2024-01-01");
        assert_eq!(sort, "2024-01-01T12:00:00");
    }

    #[test]
    fn test_resolve_date_rfc3339_sent_at() {
        let sent = Some("2024-06-15T14:30:00+02:00".to_string());
        let (display, sort) = resolve_date(&None, &sent, Path::new("test.md"));
        // Display is in sender-local time, sort key is normalised to UTC.
        assert_eq!(display, "2024-06-15");
        assert_eq!(sort, "2024-06-15T12:30:00");
    }

    /// Two emails on the same day from different timezones must sort by the
    /// actual instant, not by sender-local wallclock. Regression test for
    /// ticket #0024.
    #[test]
    fn test_resolve_date_sort_normalises_timezone() {
        // 10:00 +0200 == 08:00 UTC (earlier instant)
        let early = Some("Mon, 06 May 2024 10:00:00 +0200".to_string());
        // 09:30 +0000 == 09:30 UTC (later instant)
        let late = Some("Mon, 06 May 2024 09:30:00 +0000".to_string());
        let (_, sort_early) = resolve_date(&early, &None, Path::new("a.md"));
        let (_, sort_late) = resolve_date(&late, &None, Path::new("b.md"));
        assert!(
            sort_late > sort_early,
            "expected later UTC instant to sort higher: late={sort_late} early={sort_early}"
        );

        // Same check for the RFC3339 sent_at branch.
        let early_rfc3339 = Some("2024-05-06T10:00:00+02:00".to_string());
        let late_rfc3339 = Some("2024-05-06T09:30:00+00:00".to_string());
        let (_, sort_early) = resolve_date(&None, &early_rfc3339, Path::new("a.md"));
        let (_, sort_late) = resolve_date(&None, &late_rfc3339, Path::new("b.md"));
        assert!(
            sort_late > sort_early,
            "expected later UTC instant to sort higher: late={sort_late} early={sort_early}"
        );
    }

    #[test]
    fn test_resolve_date_naive_sent_at() {
        let sent = Some("2024-06-15T14:30:00Z".to_string());
        let (display, sort) = resolve_date(&None, &sent, Path::new("test.md"));
        assert_eq!(display, "2024-06-15");
        assert_eq!(sort, "2024-06-15T14:30:00");
    }

    #[test]
    fn test_resolve_date_filename_fallback() {
        let path = Path::new("2024-03-15-1430_sender_subject.md");
        let (display, sort) = resolve_date(&None, &None, path);
        assert_eq!(display, "2024-03-15");
        assert_eq!(sort, "2024-03-15T14:30:00");
    }

    #[test]
    fn test_resolve_date_filename_date_only() {
        let path = Path::new("2024-03-15_sender_subject.md");
        let (display, sort) = resolve_date(&None, &None, path);
        assert_eq!(display, "2024-03-15");
        assert_eq!(sort, "2024-03-15T00:00:00");
    }

    #[test]
    fn test_resolve_date_all_missing() {
        let path = Path::new("random-name.md");
        let (display, sort) = resolve_date(&None, &None, path);
        assert_eq!(display, "");
        assert_eq!(sort, "");
    }

    // -----------------------------------------------------------------------
    // Store-backed list and counts
    //
    // Ported from the file-tree fixtures of #0049 unit 0b. The tagging
    // convention carries over: `parity` means the recorded behaviour must
    // reproduce, and where a `known-bug` case became unreachable the comment
    // says so instead of quietly disappearing.
    // -----------------------------------------------------------------------

    use crate::ingest::{ingest_message, IngestInput};
    use crate::parse::FetchedEmail;
    use crate::store::BlobStore;

    fn mb(label: &str, id: &str, kind: MailboxKind) -> MailboxInfo {
        MailboxInfo {
            label: label.to_string(),
            icon: "",
            id: id.to_string(),
            kind,
            server_name: None,
        }
    }

    /// Point the data directory at a temp dir so `config::store_path` and
    /// `BlobStore::for_account` resolve inside the fixture. Thread-local, so
    /// no other test can observe it (#0077).
    struct DataDir {
        _dir: crate::config::test_env::TestDataDir,
    }

    impl DataDir {
        fn new() -> Self {
            Self {
                _dir: crate::config::test_env::TestDataDir::new(),
            }
        }

        /// Mailbox info whose store key is `name`. No directory is involved
        /// at all: the read path is one query against `messages.mailbox`.
        fn mailbox(&self, label: &str, name: &str, kind: MailboxKind) -> MailboxInfo {
            mb(label, name, kind)
        }
    }

    fn fixture_email(subject: &str, date: &str, read: bool) -> FetchedEmail {
        FetchedEmail {
            from: format!("Sender {subject} <s@example.com>"),
            to: "me@example.com".into(),
            cc: None,
            reply_to: None,
            bcc: None,
            subject: subject.into(),
            date: date.into(),
            body_text: format!("body of {subject}"),
            html_body: None,
            has_attachments: false,
            message_id: Some(format!("<{subject}@example.com>")),
            attachments: Vec::new(),
            flags: crate::types::MessageFlags::seen(read),
            calendar_ics: None,
            event: None,
        }
    }

    /// Write a fixture message through the real ingest API, so the rows under
    /// test are the rows the sync path actually produces.
    fn ingest_fixture(mailbox: &str, uid: i64, email: &FetchedEmail) {
        let store = crate::store::Store::open(crate::config::store_path("alice")).unwrap();
        let blobs = BlobStore::for_account("alice");
        ingest_message(
            &store,
            &blobs,
            &IngestInput {
                account: "alice",
                mailbox,
                uid,
                email,
                raw: None,
            },
        )
        .unwrap();
    }

    /// parity. The sidebar number is the number of messages the mailbox holds,
    /// and it equals the number of rows `load_emails` produces for the same
    /// mailbox. In the file build this was a count of top-level `.md` files
    /// and the two could disagree; both sides now read the same rows.
    #[test]
    fn counts_match_the_number_of_listable_messages() {
        let data = DataDir::new();
        ingest_fixture("inbox", 1, &fixture_email("a", "Mon, 01 Jan 2024 09:00:00 +0000", false));
        ingest_fixture("inbox", 2, &fixture_email("b", "Mon, 01 Jan 2024 10:00:00 +0000", true));
        ingest_fixture("inbox", 3, &fixture_email("c", "Mon, 01 Jan 2024 11:00:00 +0000", true));
        ingest_fixture("archive", 1, &fixture_email("old", "Mon, 01 Jan 2023 09:00:00 +0000", true));

        let mailboxes = vec![
            data.mailbox("Inbox", "inbox", MailboxKind::Inbox),
            data.mailbox("Archive", "archive", MailboxKind::Archive),
        ];

        assert_eq!(count_all_emails("alice", &mailboxes), vec![3, 1]);
        assert_eq!(load_emails("alice", "inbox").len(), 3);
        assert_eq!(load_emails("alice", "archive").len(), 1);
    }

    /// Write `body` as a draft `.md` into the account's drafts directory, the
    /// way an agent or `$EDITOR` does: no id, no index entry, nothing told to
    /// the application.
    fn external_draft(name: &str, to: &str, subject: &str, status: &str) -> std::path::PathBuf {
        let dir = crate::config::drafts_dir("alice");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("---\nto: {to}\nsubject: {subject}\nstatus: {status}\n---\n\nBody of {subject}\n"),
        )
        .unwrap();
        path
    }

    /// The Drafts mailbox lists from the drafts index, not from `messages`
    /// (#0050 scope item 5). This is the end of the stop-gate state in which
    /// the mailbox rendered empty because ingest writes no row for a local
    /// draft.
    ///
    /// The entries carry `draft_id` and no `MessageRef`, which is what
    /// `Action::CopyMessageRef` and every row-dependent action branch on.
    #[test]
    fn the_drafts_mailbox_lists_from_the_drafts_index() {
        let _data = DataDir::new();
        external_draft("2026-07-01-note.md", "a@example.com", "Hello", "draft");
        external_draft("2026-07-02-later.md", "b@example.com", "Later", "approved");

        let entries = load_emails("alice", "drafts");
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert!(entry.msg.is_none(), "a draft has no messages row");
            assert_eq!(entry.draft_id.as_ref().map(String::len), Some(16));
            assert!(entry.read, "a draft the user wrote is not unread mail");
        }
        let subjects: HashSet<&str> = entries.iter().map(|e| e.subject.as_str()).collect();
        assert_eq!(subjects, HashSet::from(["Hello", "Later"]));

        // The lister filters nothing: a hand-written `status: sent` file is
        // listed like any other. A draft a *send* retired is gone from the
        // directory, so it is not a status the list has to hide.
        let statuses: HashSet<&str> = entries.iter().map(|e| e.status.as_str()).collect();
        assert_eq!(statuses, HashSet::from(["draft", "approved"]));
        external_draft("2026-07-03-done.md", "c@example.com", "Done", "sent");
        let statuses: HashSet<String> = load_emails("alice", "drafts")
            .iter()
            .map(|e| e.status.clone())
            .collect();
        assert!(statuses.contains("sent"), "{statuses:?}");
        std::fs::remove_file(crate::config::drafts_dir("alice").join("2026-07-03-done.md"))
            .unwrap();

        // And the sidebar agrees with the list it is counting.
        let mailboxes = vec![mb(
            "Drafts",
            crate::selector::DRAFTS_MAILBOX,
            MailboxKind::Drafts,
        )];
        assert_eq!(count_all_emails("alice", &mailboxes), vec![2]);
    }

    /// A send that reached every recipient retires the draft, so the row
    /// leaves the Drafts list, the sidebar count and `mp list`'s query with
    /// the file. A partial send keeps all three, marked `sent`, so the retry
    /// still has something to name.
    #[test]
    fn a_fully_sent_draft_leaves_the_drafts_list_and_a_partial_one_stays() {
        let _data = DataDir::new();
        let done = external_draft("2026-07-01-note.md", "a@example.com", "Hello", "approved");
        let partial = external_draft("2026-07-02-later.md", "b@example.com", "Later", "approved");
        assert_eq!(load_emails("alice", "drafts").len(), 2);

        let outcome = |ok: bool| crate::send::SendReport {
            send_result: crate::send::SendResult {
                results: vec![crate::send::RecipientResult {
                    address: "a@example.com".to_string(),
                    role: crate::send::RecipientRole::To,
                    success: ok,
                    error: None,
                    verdict: if ok {
                        crate::send::RecipientVerdict::Delivered
                    } else {
                        crate::send::RecipientVerdict::Rejected
                    },
                }],
            },
            state: Some(crate::outbox::OutboxState::Done),
            row_id: Some(1),
        };
        let settle = |path: &std::path::Path, ok: bool| {
            let draft = crate::draft::parse_email_draft(path).unwrap();
            crate::draft::settle_sent_draft(&draft, &outcome(ok), None).unwrap();
        };
        settle(&done, true);
        settle(&partial, false);

        assert!(!done.exists(), "the fully sent draft left drafts/");
        let entries = load_emails("alice", "drafts");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].subject, "Later");
        assert_eq!(entries[0].status, "sent", "a partial send stays addressable");
        assert_eq!(
            count_all_emails(
                "alice",
                &[mb("Drafts", crate::selector::DRAFTS_MAILBOX, MailboxKind::Drafts)]
            ),
            vec![1]
        );

        // And `mp list` reads the same table the list just refreshed.
        let store = Store::open(crate::config::store_path("alice")).unwrap();
        let rows = crate::store::drafts::list(&store, "alice", None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject.as_deref(), Some("Later"));
    }

    /// The sidebar count and the Drafts list read the same index through the
    /// same open, so an account that has never synced (no store file yet, its
    /// drafts written straight into the directory) cannot list drafts and
    /// count 0 beside them. Nothing has loaded the mailbox here: the count is
    /// the first read.
    #[test]
    fn the_drafts_count_agrees_with_the_list_on_a_never_synced_account() {
        let _data = DataDir::new();
        external_draft("2026-08-01-one.md", "a@example.com", "One", "draft");
        external_draft("2026-08-02-two.md", "b@example.com", "Two", "draft");
        assert!(
            !crate::config::store_path("alice").exists(),
            "the account must be un-synced for this to be the case under test"
        );

        let mailboxes = vec![mb(
            "Drafts",
            crate::selector::DRAFTS_MAILBOX,
            MailboxKind::Drafts,
        )];
        assert_eq!(count_all_emails("alice", &mailboxes), vec![2]);
        assert_eq!(load_emails("alice", "drafts").len(), 2);
    }

    /// The [TKT-0045] scenario from the TUI's side: a draft written by another
    /// process is listed by the next load, with no restart and no `mp`
    /// command in between, because the load refreshes the index itself. The
    /// one-second [`crate::store::drafts::fingerprint`] poll in the event loop
    /// is what asks for that load.
    #[test]
    fn a_draft_written_externally_appears_on_the_next_load() {
        let _data = DataDir::new();
        external_draft("first.md", "a@example.com", "First", "draft");
        let before = load_emails("alice", "drafts");
        assert_eq!(before.len(), 1);
        let fingerprint_before =
            crate::store::drafts::fingerprint(&crate::config::drafts_dir("alice"));

        external_draft("second.md", "b@example.com", "Second", "draft");

        assert_ne!(
            fingerprint_before,
            crate::store::drafts::fingerprint(&crate::config::drafts_dir("alice")),
            "the poll must see the new file"
        );
        let after = load_emails("alice", "drafts");
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|e| e.subject == "Second"));
    }

    /// parity. A mailbox that was configured but never synced counts 0 rather
    /// than being skipped, so the returned vector stays index-aligned with
    /// `mailboxes`. An account with no store at all is the same case.
    #[test]
    fn counts_are_zero_for_unsynced_mailboxes_and_stay_index_aligned() {
        let data = DataDir::new();
        ingest_fixture("inbox", 1, &fixture_email("a", "Mon, 01 Jan 2024 09:00:00 +0000", false));

        let mailboxes = vec![
            data.mailbox("Never synced", "some-folder", MailboxKind::Extra),
            data.mailbox("Inbox", "inbox", MailboxKind::Inbox),
            data.mailbox("Sent", "sent", MailboxKind::Sent),
        ];
        assert_eq!(count_all_emails("alice", &mailboxes), vec![0, 1, 0]);
        assert_eq!(count_all_emails("alice", &[]), Vec::<usize>::new());

        // No store file yet: every count is zero and nothing panics.
        assert_eq!(count_all_emails("nobody", &mailboxes), vec![0, 0, 0]);
        assert!(load_emails("nobody", "inbox").is_empty());
    }

    /// parity. The count is a total, not an unread count: it is identical for
    /// an all-read and an all-unread mailbox. There was no unread-count
    /// function in the file build and there is none here, so the sidebar still
    /// cannot show one (#0049 recorded this as a gap, not a contract).
    #[test]
    fn counts_ignore_the_read_flag() {
        let data = DataDir::new();
        for i in 0..3 {
            ingest_fixture(
                "inbox",
                i + 1,
                &fixture_email(&format!("r{i}"), "Mon, 01 Jan 2024 09:00:00 +0000", true),
            );
            ingest_fixture(
                "archive",
                i + 1,
                &fixture_email(&format!("u{i}"), "Mon, 01 Jan 2024 09:00:00 +0000", false),
            );
        }

        let mailboxes = vec![
            data.mailbox("Read", "inbox", MailboxKind::Inbox),
            data.mailbox("Unread", "archive", MailboxKind::Archive),
        ];
        assert_eq!(count_all_emails("alice", &mailboxes), vec![3, 3]);
    }

    /// Resolution of the `known-bug` case recorded in #0049 unit 0b
    /// (`count_all_emails_counts_files_the_list_cannot_show`): the file build
    /// counted a non-UTF-8 `.md` that `load_emails` then dropped, so the
    /// sidebar said 2 while the list showed 1.
    ///
    /// The bug is gone by construction, not by fix: there is no file to be
    /// unreadable, and both numbers come from the same rows. The honest
    /// store-side statement of the same property is that an unreadable *body
    /// blob* (the nearest surviving analogue, and a case retention can
    /// genuinely produce) still lists and still counts, because the envelope
    /// lives in the row. It degrades to an empty body, never to a missing row.
    #[test]
    fn a_message_whose_body_blob_is_gone_still_lists_and_still_counts() {
        let data = DataDir::new();
        ingest_fixture("inbox", 1, &fixture_email("good", "Mon, 01 Jan 2024 09:00:00 +0000", false));
        ingest_fixture("inbox", 2, &fixture_email("broken", "Mon, 01 Jan 2024 10:00:00 +0000", false));

        // Evict one body the way a retention sweep would: unlink the blob file
        // and leave the row pointing at it.
        let store = crate::store::Store::open(crate::config::store_path("alice")).unwrap();
        let hash: String = store
            .conn()
            .query_row(
                "SELECT body_blob FROM messages WHERE subject = 'broken'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let blobs = BlobStore::for_account("alice");
        std::fs::remove_file(blobs.path_for(&crate::store::BlobHash::parse(&hash).unwrap()))
            .unwrap();
        drop(store);

        let mailboxes = vec![data.mailbox("Inbox", "inbox", MailboxKind::Inbox)];
        let entries = load_emails("alice", "inbox");
        assert_eq!(entries.len(), 2, "the row survives its blob");
        assert_eq!(count_all_emails("alice", &mailboxes), vec![2]);
        let broken = entries.iter().find(|e| e.subject == "broken").unwrap();
        assert_eq!(broken.from, "Sender broken", "the envelope is intact");

        // The body is no longer part of the entry, so the eviction is only
        // visible where the body is actually wanted: the preview's on-demand
        // read degrades to an empty body, one message wide.
        let store = open_store("alice").unwrap();
        let blobs = BlobStore::for_account("alice");
        assert_eq!(
            read::load_body(&store, &blobs, broken.msg.unwrap().row_id()).unwrap(),
            "",
            "an evicted body degrades to empty"
        );
        let good = entries.iter().find(|e| e.subject == "good").unwrap();
        assert_eq!(
            read::load_body(&store, &blobs, good.msg.unwrap().row_id()).unwrap(),
            "body of good",
            "the neighbouring body is untouched"
        );
    }

    /// parity. The list is newest first, exactly as the file build sorted it,
    /// and the sort now happens in SQL over `date_sort`.
    #[test]
    fn the_list_is_newest_first_and_deterministic() {
        let _data = DataDir::new();
        ingest_fixture("inbox", 1, &fixture_email("older", "Mon, 01 Jan 2024 09:00:00 +0000", false));
        ingest_fixture("inbox", 2, &fixture_email("newest", "Mon, 01 Jan 2024 18:00:00 +0000", false));
        ingest_fixture("inbox", 3, &fixture_email("middle", "Mon, 01 Jan 2024 12:00:00 +0000", false));

        let subjects: Vec<String> = load_emails("alice", "inbox")
            .into_iter()
            .map(|e| e.subject)
            .collect();
        assert_eq!(subjects, vec!["newest", "middle", "older"]);
        let again: Vec<String> = load_emails("alice", "inbox")
            .into_iter()
            .map(|e| e.subject)
            .collect();
        assert_eq!(subjects, again, "two loads must agree");
    }

    /// parity. Display fields keep the file build's rules: the display name is
    /// extracted from the address, the date is the sender-local day with a UTC
    /// sort key, and an empty subject becomes the `(no subject)` placeholder.
    #[test]
    fn display_fields_follow_the_file_builds_rules() {
        let _data = DataDir::new();
        let mut e = fixture_email("x", "Mon, 06 May 2024 10:00:00 +0200", true);
        e.subject = String::new();
        e.from = "Ada Lovelace <ada@example.com>".into();
        ingest_fixture("inbox", 1, &e);

        let entries = load_emails("alice", "inbox");
        let entry = &entries[0];
        assert_eq!(entry.subject, "(no subject)");
        assert_eq!(entry.from, "Ada Lovelace");
        assert_eq!(entry.date_display, "2024-05-06", "display stays sender-local");
        assert_eq!(entry.date_sort, "2024-05-06T08:00:00", "the sort key is UTC");
        assert!(entry.read);
        assert_eq!(entry.status, "inbox", "status comes from the mailbox now");
    }

    // -----------------------------------------------------------------------
    // Lazy bodies (#0038 scope item 5)
    // -----------------------------------------------------------------------

    /// An app parked on one store-backed mailbox, as `App::new` would leave it.
    fn app_on_inbox() -> crate::tui::app::App {
        let mut app = crate::tui::app::App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.mailboxes = vec![mb(
            "Inbox",
            "inbox",
            MailboxKind::Inbox,
        )];
        app.mailbox_counts = vec![0];
        app.email_cache = vec![None];
        app.emails = std::sync::Arc::new(load_emails("alice", "inbox"));
        app.email_cache[0] = Some(std::sync::Arc::clone(&app.emails));
        app.rebuild_visible();
        app
    }

    /// The list load itself reads no blob at all, which is the cold-start
    /// criterion: every body blob can be missing and the mailbox still lists,
    /// counts and displays. Only a body actually asked for degrades, one
    /// message wide.
    #[test]
    fn the_list_loads_with_every_body_blob_missing() {
        let data = DataDir::new();
        for i in 1..=3 {
            ingest_fixture(
                "inbox",
                i,
                &fixture_email(&format!("m{i}"), "Mon, 01 Jan 2024 09:00:00 +0000", false),
            );
        }
        std::fs::remove_dir_all(BlobStore::for_account("alice").root()).unwrap();

        let entries = load_emails("alice", "inbox");
        assert_eq!(entries.len(), 3, "the rows are all the list needs");
        assert!(entries.iter().all(|e| e.subject.starts_with('m')));
        assert_eq!(
            count_all_emails("alice", &[data.mailbox("Inbox", "inbox", MailboxKind::Inbox)]),
            vec![3]
        );

        let mut app = app_on_inbox();
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "", "only the preview degrades");
    }

    /// The preview loads the body of the message the cursor is on, and follows
    /// the cursor. Moving to another message reads that message's blob.
    #[test]
    fn the_preview_loads_the_body_of_the_selected_message() {
        let _data = DataDir::new();
        ingest_fixture("inbox", 1, &fixture_email("newest", "Mon, 01 Jan 2024 12:00:00 +0000", false));
        ingest_fixture("inbox", 2, &fixture_email("older", "Mon, 01 Jan 2024 09:00:00 +0000", false));

        let mut app = app_on_inbox();
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "body of newest");

        app.list_index = 1;
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "body of older");

        // A frame that changes nothing re-reads nothing, and shows the same
        // body: the memo answers from its key.
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "body of older");
    }

    /// A re-ingest that rewrites the body reaches the preview, because the
    /// reload that publishes the new rows bumps the generation the memo is
    /// keyed by. This is the case a body parked in `EmailEntry` could only
    /// answer by cloning the whole shared list.
    #[test]
    fn the_preview_body_follows_a_reingest() {
        let _data = DataDir::new();
        ingest_fixture("inbox", 1, &fixture_email("subject", "Mon, 01 Jan 2024 12:00:00 +0000", false));

        let mut app = app_on_inbox();
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "body of subject");

        let mut rewritten = fixture_email("subject", "Mon, 01 Jan 2024 12:00:00 +0000", false);
        rewritten.body_text = "a corrected body".to_string();
        ingest_fixture("inbox", 1, &rewritten);

        // The reload path the TUI takes after a sync: invalidate, request
        // (which bumps the generation), then deliver off the background thread.
        app.reload_current_mailbox();
        let loaded = BgResult::MailboxLoaded {
            account_index: app.active_account,
            mailbox_idx: app.active_mailbox,
            generation: app.mailbox_load_generation,
            entries: load_emails("alice", "inbox"),
        };
        crate::tui::bg::handle_bg_result(&mut app, loaded);

        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "a corrected body");
    }

    /// An app parked on the Drafts mailbox, which lists from the drafts index
    /// rather than from `messages`.
    fn app_on_drafts() -> crate::tui::app::App {
        let mut app = crate::tui::app::App::default_for_tests();
        app.account_config.name = "alice".to_string();
        app.mailboxes = vec![mb(
            "Drafts",
            crate::selector::DRAFTS_MAILBOX,
            MailboxKind::Drafts,
        )];
        app.mailbox_counts = vec![0];
        app.email_cache = vec![None];
        app.emails = std::sync::Arc::new(load_emails("alice", "drafts"));
        app.email_cache[0] = Some(std::sync::Arc::clone(&app.emails));
        app.rebuild_visible();
        app
    }

    /// Park the cursor on the draft whose subject is `subject`, whatever order
    /// the index listed the directory in.
    fn cursor_on(app: &mut crate::tui::app::App, subject: &str) {
        app.list_index = app
            .visible
            .iter()
            .position(|&i| app.emails[i].subject == subject)
            .unwrap_or_else(|| panic!("no draft row for {subject}"));
    }

    /// The Body pane of a draft row shows the draft's own markdown. It was
    /// blank for every draft, because the memo was keyed on a `MessageRef` and
    /// a draft has none: the key never built, so the pane never filled.
    #[test]
    fn the_preview_shows_the_body_of_the_selected_draft() {
        let _data = DataDir::new();
        external_draft("2026-07-01-note.md", "a@example.com", "Hello", "draft");
        external_draft("2026-07-02-later.md", "b@example.com", "Later", "approved");

        let mut app = app_on_drafts();
        cursor_on(&mut app, "Hello");
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "Body of Hello");

        // The memo follows the cursor across drafts, one file read per move.
        cursor_on(&mut app, "Later");
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "Body of Later");

        // A frame that changes nothing re-reads nothing and answers the same.
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "Body of Later");

        // A draft carries no ics blob, so the invite card stays empty rather
        // than reaching for a store row that does not exist.
        app.refresh_preview_invite();
        assert!(app.preview_invite.event().is_none());
    }

    /// The index can name a file that is no longer there: another process moved
    /// it, or a send retired it between the poll and the frame. The preview
    /// degrades to an empty pane, one draft wide, and does not panic.
    #[test]
    fn a_draft_whose_file_vanished_previews_empty() {
        let _data = DataDir::new();
        let path = external_draft("2026-07-01-note.md", "a@example.com", "Hello", "draft");

        let mut app = app_on_drafts();
        cursor_on(&mut app, "Hello");
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "Body of Hello");

        // The row survives in the list (it was loaded before), the file does
        // not, and the generation bump is what asks the memo again.
        std::fs::remove_file(&path).unwrap();
        app.mailbox_load_generation += 1;
        app.refresh_preview_body();
        assert_eq!(app.preview_body.text(), "");
    }

    /// #0088: the in-list metadata filter (the old `/`, now `fm`) is served
    /// from the loaded rows only. Body search is retired from this path (the
    /// old `\`, whose synchronous bulk blob read froze the UI); a body-only
    /// token no longer narrows the list, while the sender does, which the old
    /// `/` could not answer (UX audit §b.5). The unified `ff` entry is where
    /// body search now lives, served off the UI thread.
    #[test]
    fn the_in_list_filter_matches_the_sender_but_no_longer_the_body() {
        let _data = DataDir::new();
        // A token that lives only in the body, in no header field.
        let mut buried = fixture_email("Trip", "Mon, 01 Jan 2024 09:00:00 +0000", false);
        buried.body_text = "let us go kayaking".to_string();
        buried.from = "trips@example.com".to_string();
        ingest_fixture("inbox", 1, &buried);
        // A distinctive sender.
        let mut outbound = fixture_email("Numbers", "Mon, 01 Jan 2024 10:00:00 +0000", false);
        outbound.from = "cfo@acme.example".to_string();
        ingest_fixture("inbox", 2, &outbound);

        let mut app = app_on_inbox();
        let subjects = |app: &crate::tui::app::App| -> Vec<String> {
            app.visible_emails().map(|e| e.subject.clone()).collect()
        };

        // The retired `\` body search is gone: a body-only token does not
        // narrow the list.
        app.search_query = "kayaking".to_string();
        app.apply_search_filter(false);
        assert!(
            subjects(&app).is_empty(),
            "body substrings no longer match in the in-list filter"
        );

        // Searching by sender lands.
        app.search_query = "cfo".to_string();
        app.apply_search_filter(false);
        assert_eq!(subjects(&app), vec!["Numbers"]);
    }

    /// The key the sidebar queries the store with is the key the sync path
    /// hands to ingest, for every mailbox including the unmapped ones.
    ///
    /// It used to be the leaf of a directory path, and for an unmapped mailbox
    /// that leaf was a *slug*: an extra mailbox called `INBOX.Archive` listed
    /// under `inbox-archive` while ingest filed its rows under
    /// `INBOX.Archive`, so it showed empty and counted zero (#0064).
    #[test]
    fn the_sidebar_key_is_the_key_ingest_writes() {
        let mapping = |server: &str| crate::config::MailboxMapping {
            server: server.to_string(),
        };
        let config = crate::config::AccountConfig {
            name: "tum".to_string(),
            mailboxes: crate::config::MailboxesConfig {
                inbox: Some(mapping("INBOX")),
                archive: Some(mapping("Archive")),
                sent: Some(mapping("Sent")),
                extra: Some(vec![mapping("INBOX.Archive")]),
            },
            ..Default::default()
        };

        let keys: Vec<String> = build_mailboxes(&config).iter().map(mailbox_key).collect();
        assert_eq!(keys, ["inbox", "drafts", "sent", "archive", "INBOX.Archive"]);

        for (role, _) in crate::config::all_configured_mailboxes(&config) {
            assert!(
                keys.contains(&role.as_str().to_string()),
                "the sidebar has no slot for the rows ingest files under '{role}'"
            );
        }
    }

    /// The `status` string the headers pane shows is derived from the mailbox,
    /// and that derivation lives in exactly one place now that `EmailStatus`
    /// no longer carries the file-era placement states (#0064).
    #[test]
    fn status_is_derived_from_the_mailbox() {
        assert_eq!(status_for_mailbox("inbox"), "inbox");
        assert_eq!(status_for_mailbox("archive"), "archived");
        assert_eq!(status_for_mailbox("sent"), "sent");
        assert_eq!(status_for_mailbox("drafts"), "draft");
        assert_eq!(status_for_mailbox("some-folder"), "inbox");
        // The role reading is case-insensitive, so a mailbox synced as
        // `--mailbox INBOX` shows the inbox status, not the extra one.
        assert_eq!(status_for_mailbox("INBOX"), "inbox");
        assert_eq!(status_for_mailbox("Archive"), "archived");
    }
}
