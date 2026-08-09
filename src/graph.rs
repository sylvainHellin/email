// Microsoft Graph API REST client for Exchange Online mail operations.
//
// Provides a stateless HTTP client that wraps `reqwest::Client` + OAuth2 access token.
// Each function corresponds to a Graph API endpoint and returns domain types
// (`FetchedEmail`, folder lists, etc.). Those are what `src/ingest.rs` writes
// into the per-account store and blob store, the same landing point the IMAP
// backend uses; this module never touches the filesystem itself.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use serde::Deserialize;

use crate::config::GraphConfig;
use crate::sync::{FreshObservation, SyncResult, SyncTarget};
use crate::ingest::pass_may_prune;
use crate::types::MailboxRole;
use crate::parse::{sanitize_attachment_filename, AttachmentData, FetchedEmail};
use crate::timing::TimingSpan;

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// The `$select` a full message fetch needs, shared by every endpoint that
/// returns whole messages so they all hydrate the same [`FetchedEmail`].
const MESSAGE_SELECT: &str =
    "id,internetMessageId,subject,from,toRecipients,ccRecipients,body,receivedDateTime,hasAttachments,isRead";

/// Graph's hard ceiling on the number of requests in one `/$batch` call.
const BATCH_MAX_REQUESTS: usize = 20;

/// How many sub-request failures one `/$batch` pass tolerates before it stops
/// asking for the rest.
///
/// A failing sub-request is skipped, so the message stays new and is retried
/// next pass; without a budget a systematically failing id set (a revoked
/// permission, a mailbox mid-migration) spends the whole sync issuing requests
/// that cannot succeed and one `warn!` for each. Giving up after this many
/// leaves the pass's successful downloads in place and the rest new.
const BATCH_FAILURE_BUDGET: usize = 50;

/// How many individual sub-request failures are logged before the pass falls
/// back to a single summary line.
const BATCH_FAILURE_WARN_LIMIT: usize = 5;

/// Ceiling on a `Retry-After` a throttled sub-response asks for, so a hostile
/// or mistaken header cannot park a sync for hours.
const MAX_RETRY_AFTER_SECS: u64 = 120;

/// Pages the folder enumeration will walk before giving up.
///
/// At `$top=200` this covers a 50 000-message folder. The guard exists because
/// a `@odata.nextLink` loop has no other termination proof; hitting it marks
/// the enumeration incomplete rather than truncating silently, because a short
/// enumeration read as truth is a mass prune (see [`FolderEnumeration`]).
const MAX_ENUMERATION_PAGES: usize = 250;

/// Pages one `/messages/delta` walk will follow before giving up (#0042).
///
/// Same ceiling and same reasoning as [`MAX_ENUMERATION_PAGES`], with a
/// stricter consequence: an enumeration that hits its cap merely declines to
/// prune, while a delta walk that hits its cap has no `@odata.deltaLink` to
/// resume from and no way to know what it did not see, so it throws the token
/// away and the pass falls back to the full enumeration.
const MAX_DELTA_PAGES: usize = 250;

/// Messages per delta page, asked for with `Prefer: odata.maxpagesize`.
///
/// The delta endpoint takes its page size from the header rather than from
/// `$top`, which it rejects.
const DELTA_PAGE_SIZE: usize = 200;

/// The characters a Graph message id must not carry unescaped into a `/$batch`
/// sub-request URL.
///
/// A direct `reqwest` GET hands the id to `Url`, which escapes what a path
/// segment cannot hold; inside `/$batch` the URL is a JSON string that Graph
/// parses itself, so the escaping has to happen here. v1.0 REST ids are
/// base64url plus `=`, all legal in a path segment, so in practice this changes
/// nothing; immutable ids and the standard base64 alphabet are why it is not
/// nothing in principle (#0065).
const ID_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

// ---------------------------------------------------------------------------
// GraphClient
// ---------------------------------------------------------------------------

/// A Graph API client holding a reqwest client and a valid access token.
pub struct GraphClient {
    client: reqwest::Client,
    access_token: String,
    #[allow(dead_code)]
    username: String,
}

impl GraphClient {
    /// Create a new GraphClient, loading/refreshing the OAuth2 token synchronously.
    /// Panics if called from inside an async runtime (use `new_async` instead).
    pub fn new(config: &GraphConfig) -> Result<Self> {
        let access_token = crate::oauth2::load_or_refresh_token_blocking(
            &config.account_name,
            &config.client_id,
            &config.tenant_id,
            crate::oauth2::GRAPH_SCOPES,
        )?;
        Ok(Self {
            client: reqwest::Client::new(),
            access_token,
            username: config.username.clone(),
        })
    }

    /// Create a new GraphClient from within an async context.
    pub async fn new_async(config: &GraphConfig) -> Result<Self> {
        let access_token = crate::oauth2::load_or_refresh_token(
            &config.account_name,
            &config.client_id,
            &config.tenant_id,
            crate::oauth2::GRAPH_SCOPES,
        )
        .await?;
        Ok(Self {
            client: reqwest::Client::new(),
            access_token,
            username: config.username.clone(),
        })
    }

    fn bearer(&self) -> String {
        self.access_token.clone()
    }

    /// Refresh the access token in place, keeping the reqwest client and its
    /// connection pool.
    ///
    /// For the long-lived callers (the TUI watcher): an access token expires
    /// after about an hour, so a client built once and never touched starts
    /// returning 401 mid-session, but rebuilding the whole client per poll
    /// throws away the connection pool as well. `load_or_refresh_token` hits
    /// the network only when the cached token has actually expired.
    pub async fn refresh_token(&mut self, config: &GraphConfig) -> Result<()> {
        self.access_token = crate::oauth2::load_or_refresh_token(
            &config.account_name,
            &config.client_id,
            &config.tenant_id,
            crate::oauth2::GRAPH_SCOPES,
        )
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Graph API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphMailFolder {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub total_item_count: i64,
    #[serde(default)]
    pub unread_item_count: i64,
}

#[derive(Debug, Deserialize)]
struct GraphMailFolderList {
    value: Vec<GraphMailFolder>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessage {
    id: String,
    #[serde(default)]
    internet_message_id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    from: Option<GraphRecipient>,
    #[serde(default)]
    to_recipients: Vec<GraphRecipient>,
    #[serde(default)]
    cc_recipients: Vec<GraphRecipient>,
    #[serde(default)]
    body: Option<GraphBody>,
    #[serde(default)]
    received_date_time: Option<String>,
    #[serde(default)]
    has_attachments: bool,
    #[serde(default)]
    is_read: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRecipient {
    email_address: GraphEmailAddress,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphEmailAddress {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphBody {
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphMessageList {
    value: Vec<GraphMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphAttachment {
    #[allow(dead_code)]
    id: Option<String>,
    name: Option<String>,
    #[allow(dead_code)]
    content_type: Option<String>,
    #[serde(default)]
    content_bytes: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    is_inline: bool,
    #[serde(default)]
    content_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphAttachmentList {
    value: Vec<GraphAttachment>,
}

/// Lightweight response for enumerating a folder: everything the sync needs
/// about a message without downloading it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageIdEntry {
    id: String,
    #[serde(default)]
    internet_message_id: Option<String>,
    #[serde(default)]
    is_read: bool,
    #[serde(default)]
    received_date_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphMessageIdList {
    value: Vec<GraphMessageIdEntry>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

/// What a folder enumeration knows about one message it listed.
///
/// Keyed by `internetMessageId` (the identity the store uses, see
/// [`crate::ingest::graph_uid`]); this is the rest of the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    /// Graph's own message id: the only handle that fetches *this* message,
    /// rather than whatever currently sits in the folder's recency window.
    pub graph_id: String,
    /// The server's `\Seen` equivalent.
    pub is_read: bool,
    /// `receivedDateTime` verbatim (ISO 8601, always UTC from Graph), used to
    /// order a capped download newest-first. Sorts as a string, which is why
    /// the UTC form matters; `None` sorts oldest.
    pub received: Option<String>,
}

/// A whole folder as one enumeration saw it, and whether it saw all of it.
///
/// The flag is the prune's licence. `known − enumerated` is only a set of
/// vanished messages if the enumeration actually covered the folder; an
/// enumeration cut short by [`MAX_ENUMERATION_PAGES`] would make every message
/// past the cut look deleted, so an incomplete pass is a pass that may fetch
/// and re-flag but must not delete (#0065).
#[derive(Debug, Clone, Default)]
pub struct FolderEnumeration {
    /// Every message the folder listed, keyed by its trimmed
    /// `internetMessageId`.
    pub entries: HashMap<String, FolderEntry>,
    /// False when the page guard stopped the walk before the server ran out of
    /// pages.
    pub complete: bool,
}

/// What one target's fetch pass learned: the messages it downloaded, the ones
/// it skipped as already known, the folder as enumerated, and whether the
/// download covered everything the enumeration found.
pub struct FolderFetch {
    pub new_emails: Vec<FetchedEmail>,
    pub skipped: usize,
    pub enumeration: FolderEnumeration,
    /// True when this pass did not download every new message it found, so the
    /// leftovers are queued for a later pass. See [`sync_mailboxes_graph`] for
    /// why that suspends the prune.
    pub download_incomplete: bool,
}

/// What one `/$batch` download run returned, and whether it returned all of it.
///
/// The count matters as much as the messages: a chunk whose sub-responses
/// failed, a parse that did not yield a [`FetchedEmail`], and a run that spent
/// [`BATCH_FAILURE_BUDGET`] all hand back fewer emails than ids asked for, and
/// all three mean the same thing to the prune: this pass does not know what the
/// server holds (#0065 follow-up).
pub struct BatchFetch {
    /// How many ids the run was asked for.
    pub requested: usize,
    pub emails: Vec<FetchedEmail>,
    /// True when [`BATCH_FAILURE_BUDGET`] stopped the run before it had issued
    /// every chunk.
    pub gave_up: bool,
}

impl BatchFetch {
    /// Whether fewer messages came back than were asked for.
    fn fell_short(&self) -> bool {
        batch_fell_short(self.requested, self.emails.len(), self.gave_up)
    }
}

/// The rule behind [`BatchFetch::fell_short`], as counts: a run that handed
/// back fewer messages than ids, or gave up before issuing every chunk, did not
/// download what the enumeration found.
fn batch_fell_short(requested: usize, returned: usize, gave_up: bool) -> bool {
    gave_up || returned < requested
}

/// One entry of a `/$batch` response.
#[derive(Debug, Deserialize)]
struct GraphBatchEntry {
    id: String,
    status: u16,
    /// The sub-response's own headers. Only `Retry-After` is read, and only on
    /// a throttle: Graph throttles per sub-request, so the outer POST can be a
    /// 200 while individual messages are being told to slow down.
    ///
    /// The values are `serde_json::Value`, not `String`, because this is a
    /// whole-batch parse: a single sub-response header serialised as a number
    /// or an array (nothing in the protocol forbids it) would otherwise fail
    /// the deserialization of *every* entry in the chunk, which means zero
    /// downloads for that folder on every pass and, since #0065, a prune
    /// suspended for as long as it lasts. A header this code does not read
    /// should not be able to do that (#0065 follow-up).
    #[serde(default)]
    headers: HashMap<String, serde_json::Value>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

/// A header value as a whole number of seconds, whether Graph sent it as a
/// JSON string (the normal shape) or as a number.
fn header_secs(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

impl GraphBatchEntry {
    /// This sub-response's `Retry-After` in seconds, whatever its status.
    /// Header names are matched case-insensitively because Graph does not
    /// promise a casing.
    fn retry_after_header(&self) -> Option<u64> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, v)| header_secs(v))
            .map(|secs| secs.min(MAX_RETRY_AFTER_SECS))
    }

    /// The pause this sub-response asks for, in seconds, when it is a throttle.
    ///
    /// 429 and 503 both count: Graph's throttling guidance names the two
    /// together, and a 503 carrying a `Retry-After` is back-pressure in the
    /// same sense as a 429 (#0065 follow-up).
    fn retry_after_secs(&self) -> Option<u64> {
        if !matches!(self.status, 429 | 503) {
            return None;
        }
        self.retry_after_header()
    }

    /// Whether this sub-response is Graph asking the client to slow down rather
    /// than reporting a failure.
    ///
    /// A throttle does not spend [`BATCH_FAILURE_BUDGET`]: the budget exists to
    /// stop a pass that cannot succeed (a revoked permission, a mailbox
    /// mid-migration), and back-pressure is not that. Spending it meant a
    /// throttled first sync gave up after 50 sub-responses and reported a short
    /// download. A bare 503 with no `Retry-After` stays a failure, because
    /// nothing distinguishes it from the service being down.
    fn is_throttled(&self) -> bool {
        self.status == 429 || (self.status == 503 && self.retry_after_header().is_some())
    }
}

#[derive(Debug, Deserialize)]
struct GraphBatchResponse {
    #[serde(default)]
    responses: Vec<GraphBatchEntry>,
}

/// Lightweight response for finding a message by internet message ID.
#[derive(Debug, Deserialize)]
struct GraphMessageIdLookup {
    value: Vec<GraphMessageIdLookupEntry>,
}

#[derive(Debug, Deserialize)]
struct GraphMessageIdLookupEntry {
    id: String,
}

// ---------------------------------------------------------------------------
// Format helpers
// ---------------------------------------------------------------------------

fn format_recipient(r: &GraphRecipient) -> String {
    let addr = r.email_address.address.as_deref().unwrap_or("");
    match r.email_address.name.as_deref() {
        Some(name) if !name.is_empty() => format!("{} <{}>", name, addr),
        _ => addr.to_string(),
    }
}

fn format_recipients(recipients: &[GraphRecipient]) -> String {
    recipients
        .iter()
        .map(format_recipient)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The JSON body of a `/$batch` call fetching one message per id.
///
/// The request id is the index into `ids`, which is how the caller matches a
/// response back to the message it asked for. The message id is percent-encoded
/// ([`ID_ENCODE_SET`]) because nothing else on this path will do it.
fn batch_request_body(ids: &[&str]) -> serde_json::Value {
    let requests: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let encoded = percent_encoding::utf8_percent_encode(id, ID_ENCODE_SET);
            serde_json::json!({
                "id": i.to_string(),
                "method": "GET",
                "url": format!("/me/messages/{}?$select={}", encoded, MESSAGE_SELECT),
            })
        })
        .collect();
    serde_json::json!({ "requests": requests })
}

/// Convert an ISO 8601 datetime (e.g. "2024-01-15T10:30:00Z") to RFC 2822 format.
fn iso_to_rfc2822(iso: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        dt.to_rfc2822()
    } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%SZ") {
        let utc = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc);
        utc.to_rfc2822()
    } else {
        iso.to_string()
    }
}

/// Resolve a well-known folder name (e.g. "inbox", "sentitems") to the Graph API
/// well-known folder ID path segment. Graph accepts both well-known names and
/// folder IDs in the URL, so this is mostly for documentation clarity.
fn resolve_folder_path(folder: &str) -> String {
    let lower = folder.to_lowercase();
    match lower.as_str() {
        "inbox" | "archive" | "sentitems" | "drafts" | "deleteditems" | "junkemail" => {
            lower.clone()
        }
        _ => folder.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Phase 2: List folders
// ---------------------------------------------------------------------------

impl GraphClient {
    /// List all mail folders in the user's mailbox.
    pub async fn list_folders(&self) -> Result<Vec<GraphMailFolder>> {
        let url = format!("{}/me/mailFolders?$top=50", GRAPH_BASE);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.bearer())
            .send()
            .await
            .context("Failed to list mail folders")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("List folders failed (HTTP {}): {}", status, body));
        }

        let folders: GraphMailFolderList = resp.json().await.context("Failed to parse folder list")?;
        Ok(folders.value)
    }
}

// ---------------------------------------------------------------------------
// Phase 3: Fetch messages
// ---------------------------------------------------------------------------

impl GraphClient {
    /// Fetch messages from a folder, converting to FetchedEmail.
    pub async fn fetch_messages(
        &self,
        folder: &str,
        limit: usize,
    ) -> Result<Vec<FetchedEmail>> {
        let folder_path = resolve_folder_path(folder);
        let url = format!(
            "{}/me/mailFolders/{}/messages?\
             $top={}&\
             $orderby=receivedDateTime desc&\
             $select={}",
            GRAPH_BASE, folder_path, limit, MESSAGE_SELECT
        );

        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.bearer())
            .send()
            .await
            .context("Failed to fetch messages")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Fetch messages from '{}' failed (HTTP {}): {}",
                folder,
                status,
                body
            ));
        }

        let msg_list: GraphMessageList =
            resp.json().await.context("Failed to parse message list")?;

        let mut emails = Vec::with_capacity(msg_list.value.len());
        for msg in &msg_list.value {
            emails.push(self.hydrate(msg).await);
        }

        Ok(emails)
    }

    /// Fetch messages by their Graph ids, in batches of
    /// [`BATCH_MAX_REQUESTS`].
    ///
    /// This is what makes a Graph sync converge. Asking a folder for its `$top`
    /// most recent messages can never return a message that is new to the store
    /// but old on the server (one moved into Archive, say), so detection kept
    /// reporting it and the download kept missing it; naming each message by id
    /// downloads exactly what detection found.
    ///
    /// A message that fails inside the batch is logged and skipped: the rest of
    /// the pass still lands, and a message that was not ingested is simply new
    /// again next sync. Two guards keep that from becoming a treadmill (#0065):
    /// a throttled sub-response's `Retry-After` paces the remaining chunks, and
    /// after [`BATCH_FAILURE_BUDGET`] failures the pass stops asking and
    /// reports what it got, so a systematically failing id set costs one pass
    /// rather than one request and one log line per message.
    ///
    /// Whatever the reason, a short return is *reported* rather than absorbed:
    /// the caller folds it into the prune gate, because a message that was
    /// asked for and did not arrive is exactly the copy whose absence would
    /// make another mailbox's row look deleted (#0065 follow-up).
    pub async fn fetch_messages_by_ids(&self, ids: &[&str]) -> Result<BatchFetch> {
        let url = format!("{}/$batch", GRAPH_BASE);
        let mut emails = Vec::with_capacity(ids.len());
        let mut failures = 0usize;
        let mut gave_up = false;
        // The longest pause a sub-response asked for, applied at the top of the
        // next chunk. Pacing before a chunk rather than after one is what keeps
        // a throttle in the *last* chunk from sleeping up to `MAX_RETRY_AFTER_
        // SECS` with nothing left to pace, and keeps a pass that is about to
        // give up from sleeping first (#0065 follow-up).
        let mut pending_retry_after: Option<u64> = None;

        for chunk in ids.chunks(BATCH_MAX_REQUESTS) {
            if failures >= BATCH_FAILURE_BUDGET {
                warn!(
                    "Giving up on this pass's remaining message downloads after {failures} failed \
                     sub-requests; {} message(s) landed and the rest stay new",
                    emails.len(),
                );
                gave_up = true;
                break;
            }
            if let Some(secs) = pending_retry_after.take() {
                info!("Graph asked for {secs}s before the next batch; pacing the rest of the pass");
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
            let resp = self
                .client
                .post(&url)
                .bearer_auth(self.bearer())
                .json(&batch_request_body(chunk))
                .send()
                .await
                .context("Failed to fetch messages by id")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "Batch message fetch failed (HTTP {}): {}",
                    status,
                    body
                ));
            }

            let batch: GraphBatchResponse = resp
                .json()
                .await
                .context("Failed to parse the batch response")?;

            for entry in batch.responses {
                // Graph is free to reorder the responses, so the request id
                // (the index into `chunk`) is the only correlation.
                let requested = entry
                    .id
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| chunk.get(i).copied())
                    .unwrap_or("<unknown>");
                if entry.is_throttled() {
                    pending_retry_after = pending_retry_after.max(entry.retry_after_secs());
                    debug!(
                        "Graph throttled the batch fetch of message {} (HTTP {})",
                        requested, entry.status,
                    );
                    continue;
                }
                if entry.status != 200 {
                    failures += 1;
                    if failures <= BATCH_FAILURE_WARN_LIMIT {
                        warn!(
                            "Batch fetch of message {} failed (HTTP {}): {}",
                            requested,
                            entry.status,
                            entry.body.map(|b| b.to_string()).unwrap_or_default(),
                        );
                    }
                    continue;
                }
                let Some(body) = entry.body else {
                    failures += 1;
                    if failures <= BATCH_FAILURE_WARN_LIMIT {
                        warn!("Batch fetch of message {requested} returned no body");
                    }
                    continue;
                };
                match serde_json::from_value::<GraphMessage>(body) {
                    Ok(msg) => emails.push(self.hydrate(&msg).await),
                    Err(e) => {
                        failures += 1;
                        if failures <= BATCH_FAILURE_WARN_LIMIT {
                            warn!("Batch fetch of message {requested} did not parse: {e}");
                        }
                    }
                }
            }
        }

        if failures > BATCH_FAILURE_WARN_LIMIT {
            warn!(
                "{failures} of {} batch sub-requests failed this pass (first \
                 {BATCH_FAILURE_WARN_LIMIT} logged above); those messages stay new",
                ids.len(),
            );
        }

        Ok(BatchFetch { requested: ids.len(), emails, gave_up })
    }

    /// Turn one Graph message into a [`FetchedEmail`], pulling its attachments
    /// and lifting an iMIP invite out of them. Attachment failures are logged
    /// and leave the email otherwise intact.
    async fn hydrate(&self, msg: &GraphMessage) -> FetchedEmail {
        let mut email = graph_message_to_fetched_email(msg);
        if msg.has_attachments {
            match self.fetch_attachments(&msg.id).await {
                Ok(attachments) => email.attachments = attachments,
                Err(e) => warn!("Failed to fetch attachments for {}: {}", msg.id, e),
            }
            populate_calendar_from_attachments(&mut email);
        }
        email
    }

    /// Fetch attachments for a specific message.
    async fn fetch_attachments(&self, message_id: &str) -> Result<Vec<AttachmentData>> {
        let url = format!(
            "{}/me/messages/{}/attachments?$select=id,name,contentType,contentBytes,isInline,contentId",
            GRAPH_BASE, message_id
        );

        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.bearer())
            .send()
            .await
            .context("Failed to fetch attachments")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Fetch attachments failed (HTTP {}): {}",
                status,
                body
            ));
        }

        let att_list: GraphAttachmentList =
            resp.json().await.context("Failed to parse attachment list")?;

        let mut result = Vec::new();
        for att in att_list.value {
            // Server-provided name is untrusted: strip path separators so it cannot
            // escape the attachments directory (the IMAP path does the same).
            let filename =
                sanitize_attachment_filename(&att.name.unwrap_or_else(|| "attachment".to_string()));
            let content = if let Some(b64) = att.content_bytes {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            // Skip zero-byte attachments (placeholder entries)
            if content.is_empty() {
                continue;
            }

            result.push(AttachmentData {
                filename,
                content,
                content_id: att.content_id,
            });
        }

        Ok(result)
    }
}

fn graph_message_to_fetched_email(msg: &GraphMessage) -> FetchedEmail {
    let from = msg
        .from
        .as_ref()
        .map(format_recipient)
        .unwrap_or_default();

    let to = format_recipients(&msg.to_recipients);
    let cc = if msg.cc_recipients.is_empty() {
        None
    } else {
        Some(format_recipients(&msg.cc_recipients))
    };

    let subject = msg.subject.clone().unwrap_or_default();

    let date = msg
        .received_date_time
        .as_deref()
        .map(iso_to_rfc2822)
        .unwrap_or_default();

    let (body_text, html_body) = if let Some(ref body) = msg.body {
        let content = body.content.as_deref().unwrap_or("");
        let is_html = body
            .content_type
            .as_deref()
            .map(|ct| ct.eq_ignore_ascii_case("html"))
            .unwrap_or(false);
        if is_html {
            let plain = html2text::from_read(content.as_bytes(), 80)
                .unwrap_or_else(|_| content.to_string());
            (plain, Some(content.to_string()))
        } else {
            (content.to_string(), None)
        }
    } else {
        (String::new(), None)
    };

    let message_id = msg.internet_message_id.clone();

    FetchedEmail {
        from,
        to,
        cc,
        subject,
        date,
        body_text,
        html_body,
        has_attachments: msg.has_attachments,
        message_id,
        attachments: Vec::new(), // filled by caller if has_attachments
        // Graph answers `isRead` and nothing else: the other two bits of the
        // second axis (#TKT-0051) stay unset on this path, and the flag merge
        // in `ingest::apply_seen_flags` is what keeps a Graph pass from
        // erasing one a user set here. Ingest itself writes the set verbatim,
        // so a *re-download* of a row this build flagged would drop that flag;
        // the Graph fetch only downloads Message-IDs the store does not hold
        // (`ingest::known_message_ids`), which is what keeps that out of reach.
        flags: crate::types::MessageFlags::seen(msg.is_read),
        calendar_ics: None, // filled by caller once attachments are fetched
        event: None,
    }
}

/// Detect an iMIP calendar payload among fetched Graph attachments and populate
/// `calendar_ics` + `event` on the email. The Graph API delivers `text/calendar`
/// invites as a named `.ics` attachment, so both fetch paths converge on the
/// same `parse.rs`/`calendar.rs` machinery. Best-effort: parse failures still
/// leave the raw sidecar bytes so the email save writes the `.ics`.
fn populate_calendar_from_attachments(email: &mut FetchedEmail) {
    // Lift only the FIRST `.ics` attachment that is an actual iMIP invite (its
    // payload parses as a VCALENDAR carrying a METHOD property). Every other
    // `.ics` (e.g. a plain calendar export the user shared) stays a regular
    // attachment with its original filename -- matching the IMAP path.
    let invite_idx = email
        .attachments
        .iter()
        .position(|att| {
            crate::calendar::is_ics_filename(&att.filename)
                && crate::calendar::is_imip_invite(&att.content)
        });
    if let Some(idx) = invite_idx {
        let ics = email.attachments.remove(idx).content;
        email.event = crate::calendar::parse_ics(&ics)
            .map(|ev| crate::calendar::event_frontmatter(&ev));
        email.calendar_ics = Some(ics);
    }
}

// ---------------------------------------------------------------------------
// Phase 4: Sync support
// ---------------------------------------------------------------------------

impl GraphClient {
    /// Enumerate a folder: every message it holds, keyed by
    /// `internetMessageId`, with the handle and the metadata the sync needs to
    /// decide what to download, what to re-flag and what has gone.
    ///
    /// Messages without an `internetMessageId` are dropped, because the store
    /// identifies a Graph message by exactly that header (see
    /// [`crate::ingest::graph_uid`]) and an entry that cannot be matched to a
    /// row would look like a permanent new message. Since #0055 the sync also
    /// prunes from this enumeration, so such a message is not merely never
    /// downloaded but dropped locally if an older sync did download it;
    /// Exchange stamps the header on everything that goes through transport,
    /// which is what makes that acceptable.
    ///
    /// The walk is ordered by `receivedDateTime desc` (the same `$orderby`
    /// [`GraphClient::fetch_messages`] uses on this endpoint, so it is known to
    /// be supported without a `$filter`). Unordered paging lets a concurrent
    /// arrival shift the skiptoken window and drop a message from the middle of
    /// the walk, and a dropped message is indistinguishable from a deleted one:
    /// with newest-first ordering an arrival lands on a page the walk has
    /// already passed instead (#0065).
    ///
    /// If the server rejects the `$orderby` outright (a 4xx on the first page,
    /// which some tenant or folder shapes may do and which no live Graph call
    /// has yet ruled out), the walk retries once without it. Unordered paging
    /// is a degradation, not a loss: a fully-paged enumeration still lists the
    /// whole folder, so `complete` stays honest and the prune stays open; what
    /// comes back is the pre-#0065 churn of an occasional message re-downloaded
    /// after a concurrent arrival shifted the window. A folder that never
    /// enumerates at all would be the worse failure, and that is what this
    /// avoids (#0065 follow-up).
    pub async fn enumerate_folder(&self, folder: &str) -> Result<FolderEnumeration> {
        let folder_path = resolve_folder_path(folder);
        let mut result = HashMap::new();
        let mut complete = false;
        let mut pages = 0usize;
        let mut ordered = true;
        let mut url = enumeration_url(&folder_path, ordered);

        loop {
            if pages >= MAX_ENUMERATION_PAGES {
                warn!(
                    "Enumeration of '{folder}' stopped at {pages} pages ({} messages); \
                     this pass will not prune",
                    result.len(),
                );
                break;
            }
            pages += 1;
            let resp = self
                .client
                .get(&url)
                .bearer_auth(self.bearer())
                .send()
                .await
                .context("Failed to fetch message IDs")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if ordered && pages == 1 && status.is_client_error() {
                    warn!(
                        "Graph rejected the ordered enumeration of '{folder}' (HTTP {status}); \
                         retrying unordered, which pages this folder correctly but may re-download \
                         a message a concurrent arrival shifts out of the walk"
                    );
                    ordered = false;
                    pages = 0;
                    url = enumeration_url(&folder_path, ordered);
                    continue;
                }
                return Err(anyhow!(
                    "Fetch message IDs from '{}' failed (HTTP {}): {}",
                    folder,
                    status,
                    body
                ));
            }

            let page: GraphMessageIdList =
                resp.json().await.context("Failed to parse message ID list")?;

            absorb_page(&mut result, page.value);

            match page.next_link {
                Some(next) => url = next,
                None => {
                    complete = true;
                    break;
                }
            }
        }

        Ok(FolderEnumeration { entries: result, complete })
    }

    /// Two-pass fetch: enumerate the folder, then download by id the messages
    /// the store does not hold.
    ///
    /// The enumeration in the returned [`FolderFetch`] covers every message in
    /// the folder, so the caller can both apply read-status changes to rows it
    /// already holds and see what has gone.
    ///
    /// `download_incomplete` counts both ways a pass can come up short: `limit`
    /// leaving new messages unfetched, and the batch handing back fewer
    /// messages than were asked for. The second was the gap the first shipped
    /// with (#0065 follow-up): a throttled-out archive target returned an empty
    /// vector, reported a complete download, and opened the prune gate on inbox
    /// rows whose archive copies had never landed.
    pub async fn fetch_new_messages(
        &self,
        folder: &str,
        limit: usize,
        known_ids: &HashSet<String>,
    ) -> Result<FolderFetch> {
        let enumeration = self.enumerate_folder(folder).await?;
        let (new, found) = select_for_download(&enumeration.entries, known_ids, limit);
        let capped = found > new.len();
        let skipped = enumeration.entries.len() - found;

        if new.is_empty() {
            return Ok(FolderFetch {
                new_emails: Vec::new(),
                skipped,
                enumeration,
                download_incomplete: capped,
            });
        }

        let graph_ids: Vec<&str> = new.iter().map(|entry| entry.graph_id.as_str()).collect();
        let batch = self.fetch_messages_by_ids(&graph_ids).await?;
        let download_incomplete = capped || batch.fell_short();

        Ok(FolderFetch {
            new_emails: batch.emails,
            skipped,
            enumeration,
            download_incomplete,
        })
    }
}

/// The first-page URL of a folder enumeration, with or without the `$orderby`.
///
/// Split out so the ordered form and the fallback
/// ([`GraphClient::enumerate_folder`] retries once unordered on a 4xx) are one
/// definition rather than two literals that can drift apart.
fn enumeration_url(folder_path: &str, ordered: bool) -> String {
    let orderby = if ordered {
        "$orderby=receivedDateTime%20desc&"
    } else {
        ""
    };
    format!(
        "{}/me/mailFolders/{}/messages?\
         $select=id,internetMessageId,isRead,receivedDateTime&\
         {}$top=200",
        GRAPH_BASE, folder_path, orderby
    )
}

// ---------------------------------------------------------------------------
// #0042: /messages/delta
// ---------------------------------------------------------------------------

/// One entry of a `/messages/delta` page: either a message whose state
/// changed, or a removal.
///
/// A removal carries `id` and `@removed` and *nothing else*, which is the
/// whole reason the prune below does not consume it: see [`FolderDelta`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDeltaEntry {
    id: String,
    #[serde(default)]
    internet_message_id: Option<String>,
    #[serde(default)]
    is_read: bool,
    #[serde(default)]
    received_date_time: Option<String>,
    #[serde(rename = "@removed", default)]
    removed: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GraphDeltaPage {
    value: Vec<GraphDeltaEntry>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

/// What one delta walk saw: the messages whose state changed since the stored
/// token, how many removals it was told about, and the token that resumes
/// after it.
///
/// `changed` is keyed like [`FolderEnumeration::entries`], on the trimmed
/// `internetMessageId`, but it is **not** a folder listing: it is a change set,
/// so `known − changed` is meaningless and the prune must never be computed
/// from it.
#[derive(Debug, Clone, Default)]
pub struct FolderDelta {
    pub changed: HashMap<String, FolderEntry>,
    /// How many `@removed` entries the walk reported.
    pub removed: usize,
    /// The `@odata.deltaLink` the last page carried. A walk that ended without
    /// one never becomes a [`FolderDelta`]; it is a [`DeltaVerdict::Discard`].
    pub delta_link: String,
    /// Pages walked, for the log line and the timing evidence.
    pub pages: usize,
}

impl FolderDelta {
    /// Whether this delta hands the pass back to the full enumeration.
    ///
    /// **The `@removed` decision (#0042).** A removal entry names the message
    /// by Graph's own `id`, and the store does not hold that id: a Graph row's
    /// identity is [`crate::ingest::graph_uid`] of the `internetMessageId`,
    /// which a removal entry does not carry and which the server will not sell
    /// back for a message it has just deleted. So the delta cannot map a
    /// removal onto a row, and the prune keeps its existing source of truth,
    /// the full folder listing (`known − enumerated`, [`vanished_graph_uids`]).
    ///
    /// A pass whose delta reports any removal therefore escalates: it throws
    /// the change set away, enumerates the folder in full and prunes exactly as
    /// a pre-#0042 pass did. That keeps the #0072/#0074 coverage and
    /// deferred-prune gates working on unchanged inputs, and it costs the
    /// enumeration only on passes that would have had something to delete.
    ///
    /// The rejected alternative was to persist Graph's message id per row so a
    /// removal could be resolved directly. That is a schema column and a store
    /// rebuild for every account, and it buys latency on deletions rather than
    /// correctness; it is recorded as a follow-up on the ticket instead.
    fn forces_full_enumeration(&self) -> bool {
        self.removed > 0
    }
}

/// Why a pass took the full enumeration instead of the delta, or what it did
/// with a token it could not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaVerdict {
    /// Use the stored token.
    Use,
    /// No token stored: this is the bootstrap pass.
    NoToken,
    /// A full sync (`limit == usize::MAX`) always relists the folder, so the
    /// prune and the token both get a fresh, whole-folder observation.
    FullSync,
    /// The folder's Graph id is not the one the token was minted against.
    FolderChanged,
    /// Something about the token or the walk is in doubt; the token is dropped
    /// and this pass enumerates.
    Discard(DeltaDiscard),
}

/// The reasons a delta token is thrown away rather than merely unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaDiscard {
    /// Graph answered 410: the sync state behind the token is gone.
    Expired,
    /// The stored string is not a Graph delta URL.
    Malformed,
    /// The page chain hit [`MAX_DELTA_PAGES`].
    PageCap,
    /// The walk ran out of pages without an `@odata.deltaLink`, so there is no
    /// resume point and no proof the chain was complete.
    NoResumePoint,
    /// The request failed for any other reason.
    Failed,
}

/// Whether this pass may use the stored delta token, decided before any
/// request is issued.
///
/// The cardinal rule of #0042, and the same one #0041 wrote for CONDSTORE: the
/// delta may only ever be *faster*, never the only thing that looked. Every
/// branch that is not an exact match on a token this client minted for this
/// folder falls back to the full enumeration, which is the pre-#0042 pass and
/// misses nothing.
///
/// `identity_matches` is the Graph analogue of UIDVALIDITY: the token is bound
/// to a folder id, so a folder deleted and recreated under the same well-known
/// name (or a config that repoints a role at a different folder) invalidates
/// it. Graph would answer such a token with a 404 or a 410 anyway, which the
/// walk would discard on; this check makes the invalidation the client's own
/// rather than a server behaviour we depend on.
fn delta_verdict(limit: usize, stored: Option<&str>, identity_matches: bool) -> DeltaVerdict {
    let Some(token) = stored else {
        return DeltaVerdict::NoToken;
    };
    if !token.starts_with(GRAPH_BASE) || !token.contains("/delta") {
        return DeltaVerdict::Discard(DeltaDiscard::Malformed);
    }
    if !identity_matches {
        return DeltaVerdict::FolderChanged;
    }
    // A full sync is the periodic whole-folder observation everything else
    // leans on: it is what reopens the prune on removals the delta declined to
    // resolve, and what re-mints the token from a listing rather than from a
    // chain of increments.
    if limit == usize::MAX {
        return DeltaVerdict::FullSync;
    }
    DeltaVerdict::Use
}

/// What an HTTP status on the delta endpoint means for the token.
///
/// 410 is the documented expiry (`resyncRequired`); 404 is the folder having
/// gone out from under the token. Everything else that is not a success is a
/// plain failure, and all three land in the same place, because a delta that
/// did not complete cannot be told apart from one that skipped a message.
fn delta_status_discard(status: reqwest::StatusCode) -> Option<DeltaDiscard> {
    if status.is_success() {
        None
    } else if status == reqwest::StatusCode::GONE || status == reqwest::StatusCode::NOT_FOUND {
        Some(DeltaDiscard::Expired)
    } else {
        Some(DeltaDiscard::Failed)
    }
}

/// Fold one delta page into the change set, counting removals separately.
///
/// An entry with `@removed` is counted and dropped: it names a message by
/// Graph id, which the store cannot resolve (see
/// [`FolderDelta::forces_full_enumeration`]). An entry with no usable
/// `internetMessageId` is dropped for the same reason [`absorb_page`] drops
/// one.
fn absorb_delta_page(
    changed: &mut HashMap<String, FolderEntry>,
    removed: &mut usize,
    page: Vec<GraphDeltaEntry>,
) {
    for entry in page {
        if entry.removed.is_some() {
            *removed += 1;
            continue;
        }
        let Some(mid) = entry.internet_message_id else { continue };
        let mid = mid.trim();
        if mid.is_empty() {
            continue;
        }
        changed.insert(
            mid.to_string(),
            FolderEntry {
                graph_id: entry.id,
                is_read: entry.is_read,
                received: entry.received_date_time,
            },
        );
    }
}

/// The first-page URL of a delta walk, and the `$deltatoken=latest` form that
/// mints a resume point without enumerating anything.
fn delta_url(folder_path: &str, latest: bool) -> String {
    let latest = if latest { "&$deltatoken=latest" } else { "" };
    format!(
        "{}/me/mailFolders/{}/messages/delta?\
         $select=id,internetMessageId,isRead,receivedDateTime{}",
        GRAPH_BASE, folder_path, latest
    )
}

/// A `$select=id` folder response.
///
/// Not [`GraphMailFolder`]: that type requires `displayName`, which a
/// `$select=id` projection does not return, so reusing it would fail the parse
/// of every identity check.
#[derive(Debug, Deserialize)]
struct GraphFolderId {
    id: String,
}

/// The folder id as the 63-bit hash the `uidvalidity` column can hold.
///
/// Graph's folder id is the analogue of IMAP's UIDVALIDITY (it is what a
/// resume token is bound to) so it is stored in the analogous column, hashed
/// because the column is an integer. Same hash as
/// [`crate::ingest::graph_uid`], which is a stable 63-bit digest of a string
/// and carries no message-specific meaning.
fn folder_identity_hash(folder_id: &str) -> i64 {
    crate::ingest::graph_uid(folder_id)
}

impl GraphClient {
    /// The folder's Graph id: the identity a delta token is bound to.
    async fn folder_identity(&self, folder: &str) -> Result<String> {
        let url = format!(
            "{}/me/mailFolders/{}?$select=id",
            GRAPH_BASE,
            resolve_folder_path(folder)
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.bearer())
            .send()
            .await
            .context("Failed to read the folder id")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Read folder id of '{}' failed (HTTP {}): {}",
                folder,
                status,
                body
            ));
        }
        let folder: GraphFolderId = resp.json().await.context("Failed to parse the folder")?;
        Ok(folder.id)
    }

    /// Mint a delta resume point for `folder` describing the folder *now*,
    /// without listing it (`$deltatoken=latest`).
    ///
    /// Called **before** the enumeration it will be stored alongside, never
    /// after: a token taken after the listing would silently cover the window
    /// between the two, and a message that arrived in it would never be
    /// reported by any pass. Taken first, that window is replayed by the next
    /// delta at worst.
    ///
    /// A tenant that rejects `$deltatoken=latest` gets `None` and keeps the
    /// pre-#0042 behaviour for good; no token is ever guessed.
    async fn mint_delta_token(&self, folder: &str) -> Option<String> {
        let url = delta_url(&resolve_folder_path(folder), true);
        let resp = match self.client.get(&url).bearer_auth(self.bearer()).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Could not mint a delta token for '{folder}': {e}");
                return None;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(
                "Graph refused a delta token for '{folder}' (HTTP {status}): {body}; \
                 this account keeps enumerating the folder in full"
            );
            return None;
        }
        match resp.json::<GraphDeltaPage>().await {
            Ok(page) => page.delta_link,
            Err(e) => {
                warn!("Could not parse the delta token for '{folder}': {e}");
                None
            }
        }
    }

    /// Walk `/messages/delta` from a stored token.
    ///
    /// `Ok(Ok(delta))` is a complete chain ending in a fresh resume point;
    /// `Ok(Err(discard))` is every doubt there is, and every one of them means
    /// the same thing to the caller: drop the token, enumerate the folder.
    async fn walk_delta(&self, link: &str) -> Result<FolderDelta, DeltaDiscard> {
        let mut changed = HashMap::new();
        let mut removed = 0usize;
        let mut pages = 0usize;
        let mut url = link.to_string();

        loop {
            if pages >= MAX_DELTA_PAGES {
                return Err(DeltaDiscard::PageCap);
            }
            pages += 1;
            let resp = match self
                .client
                .get(&url)
                .bearer_auth(self.bearer())
                .header("Prefer", format!("odata.maxpagesize={DELTA_PAGE_SIZE}"))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!("Graph delta request failed: {e}");
                    return Err(DeltaDiscard::Failed);
                }
            };
            if let Some(discard) = delta_status_discard(resp.status()) {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                warn!("Graph delta answered HTTP {status}: {body}");
                return Err(discard);
            }
            let page: GraphDeltaPage = match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    warn!("Could not parse a Graph delta page: {e}");
                    return Err(DeltaDiscard::Failed);
                }
            };
            absorb_delta_page(&mut changed, &mut removed, page.value);

            match (page.next_link, page.delta_link) {
                (Some(next), _) => url = next,
                (None, Some(delta_link)) => {
                    return Ok(FolderDelta { changed, removed, delta_link, pages })
                }
                (None, None) => return Err(DeltaDiscard::NoResumePoint),
            }
        }
    }

    /// The delta twin of [`GraphClient::fetch_new_messages`]: walk the change
    /// set, then download by id the changed messages the store does not hold.
    ///
    /// Returns `Ok(None)` when the delta reported removals, which hands the
    /// pass to the full enumeration with the token left in place
    /// ([`FolderDelta::forces_full_enumeration`]).
    async fn fetch_delta(
        &self,
        folder: &str,
        limit: usize,
        known_ids: &HashSet<String>,
        link: &str,
    ) -> Result<Option<DeltaFetch>, DeltaDiscard> {
        let delta = self.walk_delta(link).await?;
        info!(
            "Graph delta for '{folder}': {} page(s), {} changed, {} removed",
            delta.pages,
            delta.changed.len(),
            delta.removed,
        );
        if delta.forces_full_enumeration() {
            return Ok(None);
        }

        let (new, found) = select_for_download(&delta.changed, known_ids, limit);
        let capped = found > new.len();
        if new.is_empty() {
            return Ok(Some(DeltaFetch {
                new_emails: Vec::new(),
                changed: delta.changed,
                delta_link: delta.delta_link,
                download_incomplete: capped,
            }));
        }
        let graph_ids: Vec<&str> = new.iter().map(|entry| entry.graph_id.as_str()).collect();
        let batch = match self.fetch_messages_by_ids(&graph_ids).await {
            Ok(b) => b,
            Err(e) => {
                warn!("Graph delta download failed for '{folder}': {e:#}");
                return Err(DeltaDiscard::Failed);
            }
        };
        let download_incomplete = capped || batch.fell_short();
        Ok(Some(DeltaFetch {
            new_emails: batch.emails,
            changed: delta.changed,
            delta_link: delta.delta_link,
            download_incomplete,
        }))
    }
}

/// What one delta pass hands the orchestrator.
///
/// The shape deliberately differs from [`FolderFetch`]: there is no
/// `enumeration`, because a change set is not a folder listing and nothing may
/// diff against it, and no `skipped`, because a delta does not walk past the
/// messages it already holds.
struct DeltaFetch {
    new_emails: Vec<FetchedEmail>,
    /// The changed messages, for the read-flag pass.
    changed: HashMap<String, FolderEntry>,
    /// The resume point, stored only if this pass covered and wrote
    /// everything.
    delta_link: String,
    download_incomplete: bool,
}

/// Whether a pass has earned the right to store its delta resume point.
///
/// The token means "at this point the store held every message the folder
/// listed", and that is the whole safety argument for using it later: a delta
/// from a token with that property reports every subsequent change, so the
/// property survives. A pass that left a message undownloaded, or downloaded
/// one it could not write, does not have the property and must not mint the
/// claim; the older token stays and the next pass replays from it.
///
/// This is the same rule #0041 wrote for the CONDSTORE resume point, for the
/// same reason, and it is what keeps the #0074 ingest bound honest on this
/// path: `ingest_failed` is false once a poisoned message has been given up
/// on, so a message the store will never accept cannot wedge the delta chain
/// for good.
fn may_record_delta_token(covered: bool, download_incomplete: bool, ingest_failed: bool) -> bool {
    covered && !download_incomplete && !ingest_failed
}

/// Fold one page of an enumeration into the map.
///
/// The key is `internetMessageId` **trimmed**, because that is the identity
/// [`crate::ingest::resolve_message_id`] stores and [`crate::ingest::graph_uid`]
/// hashes. Keying on the header verbatim made a single padded id look both new
/// (no matching row) and vanished (no matching enumeration key) on every pass:
/// a delete-and-re-download loop, with the flags never applying because the uid
/// was computed from the untrimmed string (#0065).
///
/// A message with no usable `internetMessageId` is dropped; see
/// [`GraphClient::enumerate_folder`] for why that is acceptable.
fn absorb_page(result: &mut HashMap<String, FolderEntry>, page: Vec<GraphMessageIdEntry>) {
    for entry in page {
        let Some(mid) = entry.internet_message_id else { continue };
        let mid = mid.trim();
        if mid.is_empty() {
            continue;
        }
        result.insert(
            mid.to_string(),
            FolderEntry {
                graph_id: entry.id,
                is_read: entry.is_read,
                received: entry.received_date_time,
            },
        );
    }
}

/// The messages this pass will download, and how many new ones it found in all.
///
/// The two differ exactly when `limit` left some behind, and that difference is
/// what suspends the prune: a pass that could not fetch everything it found
/// cannot also be trusted to say what has been deleted (see [`pass_may_prune`]).
fn select_for_download<'a>(
    server: &'a HashMap<String, FolderEntry>,
    known_ids: &HashSet<String>,
    limit: usize,
) -> (Vec<&'a FolderEntry>, usize) {
    let mut new = new_ids_newest_first(server, known_ids);
    let found = new.len();
    new.truncate(limit);
    (new, found)
}

/// The folder's messages the store does not hold, newest first.
///
/// Newest first is what makes a capped sync useful (the quick sync passes
/// `limit = 100`): the pass downloads the most recent arrivals, and because it
/// downloads them *by id* the leftovers are known next pass, so a backlog
/// drains one window at a time instead of the same window repeating. Ties and
/// missing dates fall back to the message id so the order is total and a
/// truncated pass is reproducible.
fn new_ids_newest_first<'a>(
    server: &'a HashMap<String, FolderEntry>,
    known_ids: &HashSet<String>,
) -> Vec<&'a FolderEntry> {
    let mut new: Vec<(&str, &FolderEntry)> = server
        .iter()
        .filter(|(mid, _)| !known_ids.contains(mid.as_str()))
        .map(|(mid, entry)| (mid.as_str(), entry))
        .collect();
    new.sort_by(|(a_mid, a), (b_mid, b)| {
        b.received
            .cmp(&a.received)
            .then_with(|| a_mid.cmp(b_mid))
    });
    new.into_iter().map(|(_, entry)| entry).collect()
}

/// The rows the store holds for a mailbox that the folder no longer lists, as
/// the synthetic UIDs [`crate::ingest::graph_uid`] gives them.
///
/// The Graph counterpart of [`crate::imap_client::vanished_uids`], and simpler:
/// the enumeration covers the whole folder rather than a recency window, so
/// there is no range to clamp the diff to. An enumeration that came back empty
/// means an empty folder, and emptying a folder on the server does empty it
/// locally.
///
/// What replaces the IMAP clamp is the caller's two gates, because both of the
/// cases the clamp defends against exist here too (#0065): a pass that did not
/// see the whole folder or could not download its whole backlog does not prune
/// at all ([`sync_mailboxes_graph`]), and a row too freshly dated to be
/// anything but a local ingest the server has yet to file is held back
/// ([`crate::ingest::prunable_uids`]).
fn vanished_graph_uids(
    known_ids: &HashSet<String>,
    server: &HashMap<String, FolderEntry>,
) -> Vec<i64> {
    known_ids
        .iter()
        .filter(|mid| !server.contains_key(mid.as_str()))
        .map(|mid| crate::ingest::graph_uid(mid))
        .collect()
}

// ---------------------------------------------------------------------------
// Phase 4: sync_mailboxes_graph
// ---------------------------------------------------------------------------

/// Sync orchestrator for Graph accounts, store-only. Mirrors the IMAP
/// `sync_mailboxes`, with two documented differences forced by the API:
///
/// - Graph never returns the original RFC822, so rows get `raw_blob` NULL and
///   the body/attachment blobs are the only copies of the content;
/// - Graph has no UID, so the row's `uid` is [`crate::ingest::graph_uid`] of
///   the message's `Message-ID`, which is stable per message and keeps the
///   `UNIQUE (account, mailbox, uid)` identity meaningful.
///
/// The orchestration mirrors [`crate::imap_client::sync_mailboxes`] line for
/// line, prune second pass included: the prunes are held back until every
/// target has been ingested, so a message archived in Outlook web has its
/// archive row before its inbox row goes and never spends a window with no row
/// anywhere (#0055).
///
/// Three conditions gate that prune, standing in for the IMAP window clamp
/// (#0065): the pass must have enumerated every target in full, it must have
/// landed every new message it found (a quick sync's `limit`, a batch
/// sub-response failure, a throttle that ran the pass out of budget and an
/// ingest error all count against this), and a row dated within
/// [`crate::ingest::PRUNE_MIN_AGE_SECS`] of now is left alone. Without the
/// first two, a message moved between two folders loses its source row in the
/// pass that could not fetch its destination copy, and holds no row anywhere
/// until the backlog drains.
///
/// Since #0042 a quick sync may replace the enumeration with a
/// `/messages/delta` walk from the token in `sync_cursors.deltalink`. The
/// delta is strictly an accelerator: [`delta_verdict`] refuses it on anything
/// unusual, any doubt during the walk throws the token away
/// ([`DeltaDiscard`]), a delta that reports removals hands the pass back to the
/// enumeration ([`FolderDelta::forces_full_enumeration`]), a full sync always
/// relists, and a token is only ever minted by a pass that saw the whole
/// folder and wrote every message in it ([`may_record_delta_token`]).
pub async fn sync_mailboxes_graph(
    config: &GraphConfig,
    account_name: &str,
    targets: &[SyncTarget],
    limit: usize,
    dry_run: bool,
) -> Result<SyncResult> {
    info!(
        "sync_mailboxes_graph: account={account_name}, {} targets, limit={limit}, dry_run={dry_run}",
        targets.len(),
    );
    let span_label = if limit < usize::MAX {
        "sync_mailboxes_graph:quick"
    } else {
        "sync_mailboxes_graph:full"
    };
    let mut span =
        TimingSpan::with_context(span_label, format!("{} targets", targets.len()));

    let client = GraphClient::new_async(config).await?;
    let store = crate::store::Store::open_account(account_name)?;
    let blobs = crate::store::BlobStore::for_account(account_name);
    let mut result = SyncResult::default();
    span.mark("client_open");

    // Every prune this run will apply, collected here and applied after the
    // loop: see the second pass below for why it cannot run per target.
    let mut prunes: Vec<(MailboxRole, Vec<i64>)> = Vec::new();
    // `(enumeration complete, download truncated)` per target, which decides
    // whether the prunes above may be applied at all: see `pass_may_prune`.
    let mut coverage: Vec<(bool, bool)> = Vec::with_capacity(targets.len());

    for target in targets {
        let known = crate::ingest::known_message_ids(&store, account_name, target.role.as_str())?;
        let mailbox = target.role.as_str();
        let stored = crate::ingest::load_mailbox_cursor(&store, account_name, mailbox)?;
        let stored_token = stored.as_ref().and_then(|c| c.deltalink.clone());
        let stored_identity = stored.as_ref().and_then(|c| c.uidvalidity);

        // The folder id, read fresh. An id we could not read is not evidence
        // that the folder is the same one, so it counts as a mismatch and the
        // pass enumerates; the *stored* identity is kept, because a failed
        // lookup is no evidence of a change either.
        let observed_identity = if dry_run {
            None
        } else {
            match client.folder_identity(&target.server_name).await {
                Ok(id) => Some(folder_identity_hash(&id)),
                Err(e) => {
                    warn!("Could not read the folder id of {}: {e:#}", target.role);
                    None
                }
            }
        };
        let identity_matches =
            observed_identity.is_some() && observed_identity == stored_identity;
        let identity = observed_identity.or(stored_identity);

        // A dry run writes nothing, and every delta branch below is a write:
        // it either drops a token or mints one. So a dry run takes the
        // pre-#0042 pass, unchanged.
        let verdict = if dry_run {
            DeltaVerdict::FullSync
        } else {
            delta_verdict(limit, stored_token.as_deref(), identity_matches)
        };
        if let DeltaVerdict::Discard(reason) = verdict {
            info!("Graph delta token for {} dropped ({reason:?})", target.role);
            crate::ingest::clear_mailbox_deltalink(&store, account_name, mailbox);
        }
        if verdict == DeltaVerdict::FolderChanged {
            info!(
                "Graph folder id for {} is not the one its delta token was minted against; \
                 dropping the token and enumerating",
                target.role
            );
            crate::ingest::clear_mailbox_deltalink(&store, account_name, mailbox);
        }

        // The delta half. `None` here means this pass enumerates, for any of
        // the reasons above or because the walk itself was in doubt.
        let mut delta_fetch: Option<DeltaFetch> = None;
        if verdict == DeltaVerdict::Use {
            let link = stored_token.clone().unwrap_or_default();
            match client
                .fetch_delta(&target.server_name, limit, &known, &link)
                .await
            {
                Ok(Some(d)) => delta_fetch = Some(d),
                Ok(None) => info!(
                    "Graph delta for {} reported removals; enumerating the folder so the \
                     prune can resolve them",
                    target.role
                ),
                Err(reason) => {
                    info!(
                        "Graph delta for {} abandoned ({reason:?}); dropping the token and \
                         enumerating",
                        target.role
                    );
                    crate::ingest::clear_mailbox_deltalink(&store, account_name, mailbox);
                }
            }
        }

        // What the pass saw, in the two shapes it can come in. `server` is a
        // whole folder listing and may be diffed against the store; `changed`
        // is a change set and may not.
        let (new_emails, server, complete, download_incomplete, token, used_delta) = match delta_fetch
        {
            Some(d) => (
                d.new_emails,
                d.changed,
                // A delta pass covers the folder in the sense the prune gate
                // asks about: its token asserts the store held everything the
                // folder listed at token time, and this walk brought in every
                // change since, so nothing another target's prune might need
                // is missing. It contributes no prunes of its own.
                true,
                d.download_incomplete,
                Some(d.delta_link),
                true,
            ),
            None => {
                // Minted before the enumeration, never after: a token taken
                // afterwards would silently swallow the window between the two.
                let minted = if dry_run {
                    None
                } else {
                    client.mint_delta_token(&target.server_name).await
                };
                let fetch = match client
                    .fetch_new_messages(&target.server_name, limit, &known)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Graph sync failed for {}: {}", target.role, e);
                        // A target that did not sync at all is the strongest
                        // form of partial pass: the copy that would justify
                        // another target's deletion may be exactly what this
                        // fetch failed to bring in.
                        coverage.push((false, false));
                        continue;
                    }
                };
                let FolderFetch { new_emails, skipped, enumeration, download_incomplete } = fetch;
                result.skipped += skipped;
                (
                    new_emails,
                    enumeration.entries,
                    enumeration.complete,
                    download_incomplete,
                    minted,
                    false,
                )
            }
        };
        span.mark(&format!("fetch:{}", target.role));

        if dry_run {
            coverage.push((complete, download_incomplete));
            result.saved += new_emails.len();
            continue;
        }

        // A message that was downloaded but not written is as absent from the
        // store as one that was never fetched, so it counts against this
        // target's coverage too (#0065 follow-up).
        //
        // Bounded the same way the IMAP path is (#0074 review): without the
        // bound, one message this store rejects every time would set
        // `truncated` on every pass and suspend the account's prune for good.
        // `ingest::note_ingest_failure` counts the attempts per
        // `(account, mailbox, uid)` and gives up loudly after
        // `MAX_INGEST_ATTEMPTS`; a success clears the count, so transient
        // failures never accumulate towards it. The arrival mark has no Graph
        // half -- the pull is by id, with no positional window (see the cursor
        // below) -- so the give-up only releases the prune gate.
        let mut ingest_failed = false;
        for email in &new_emails {
            let message_id = crate::ingest::resolve_message_id(email, None);
            let uid = crate::ingest::graph_uid(&message_id);
            match crate::ingest::ingest_message(
                &store,
                &blobs,
                &crate::ingest::IngestInput {
                    account: account_name,
                    mailbox: target.role.as_str(),
                    uid,
                    email,
                    raw: None,
                },
            ) {
                Ok(outcome) => {
                    crate::ingest::clear_ingest_failure(
                        &store,
                        account_name,
                        target.role.as_str(),
                        uid,
                    );
                    if outcome.inserted {
                        result.saved += 1;
                        if target.role.is_inbox() {
                            result
                                .new_inbox_mail
                                .push(crate::notify::NewMailMeta::new(&email.from, &email.subject));
                        }
                    }
                    if outcome.uid_rebound {
                        result.uid_rebound += 1;
                    }
                    result.fresh_observations.push(FreshObservation {
                        role: target.role.clone(),
                        from: email.from.clone(),
                        to: email.to.clone(),
                        cc: email.cc.clone(),
                        date: email.date.clone(),
                    });
                }
                Err(e) => {
                    warn!("Failed to ingest {} from {}: {:#}", message_id, target.role, e);
                    ingest_failed |= crate::ingest::note_ingest_failure(
                        &store,
                        account_name,
                        target.role.as_str(),
                        &target.server_name,
                        uid,
                        &format!("{e:#} (message {message_id})"),
                    );
                }
            }
        }
        coverage.push((complete, download_incomplete || ingest_failed));
        span.mark(&format!("ingest:{}", target.role));

        // Read status for messages the store already holds, one transaction
        // for the whole folder.
        result.flags_updated += crate::ingest::apply_seen_flags(
            &store,
            account_name,
            target.role.as_str(),
            server
                .iter()
                .map(|(mid, entry)| (crate::ingest::graph_uid(mid), entry.is_read)),
        );
        span.mark(&format!("flags:{}", target.role));

        // The other half of the same diff: what the store holds here and the
        // server does not. Held back until every target has been ingested (see
        // the second pass below).
        //
        // Only a full enumeration may compute it. `server` on a delta pass is
        // a change set, so `known − server` would name every message that did
        // not happen to change since the token: the entire mailbox (#0042).
        if !used_delta {
            let vanished = vanished_graph_uids(&known, &server);
            if !vanished.is_empty() {
                prunes.push((target.role.clone(), vanished));
            }
        }

        // The resume point is only minted by a pass that saw the whole folder
        // and wrote every message it found, and only alongside the folder id
        // it is bound to: a token with no identity could never be validated
        // again (#0042).
        let earned = may_record_delta_token(complete, download_incomplete, ingest_failed);
        let minted_a_token = token.is_some();
        let token_to_store = if earned { token } else { None };

        if used_delta {
            // A delta pass observed no folder listing, so it writes the token
            // and nothing else: `exists` and the rest are observations it did
            // not make, and `record_mailbox_cursor` would null them.
            if let (Some(identity), Some(link)) = (identity, token_to_store.as_deref()) {
                crate::ingest::record_delta_token(&store, account_name, mailbox, identity, link);
            }
        } else if complete {
            // Only a complete enumeration knows how many messages the folder
            // holds; a short one would record a count the UI shows as truth,
            // so the last known-good stays instead.
            crate::ingest::record_mailbox_cursor(
                &store,
                account_name,
                mailbox,
                &crate::ingest::MailboxCursor {
                    // The Graph analogue of UIDVALIDITY: the folder id its
                    // delta token is bound to, hashed into the column.
                    uidvalidity: identity,
                    last_uid: None,
                    uidnext: None,
                    exists: Some(server.len() as i64),
                    highest_modseq: None,
                    deltalink: if identity.is_some() { token_to_store } else { None },
                    // IMAP-only: the Graph pull downloads by id, so it has no
                    // positional window that can leave an arrival behind.
                    arrival_mark: None,
                },
            )?;
            if earned && stored_token.is_some() && !minted_a_token {
                // This pass covered the folder but could not mint a token, so
                // the stored one is older than a listing that has just been
                // fully consumed. Keeping it would replay the same changes
                // (and, if they included removals, escalate to this same
                // enumeration) on every pass for good.
                crate::ingest::clear_mailbox_deltalink(&store, account_name, mailbox);
            }
        }
    }

    // Second pass: every prune, after every target has been ingested. The
    // ordering argument is [`crate::imap_client::sync_mailboxes`]'s, and holds
    // here for the same reason: targets are synced inbox, archive, sent, so
    // pruning inside the loop would delete the inbox row of a message archived
    // in Outlook web before the archive pass ingests it.
    if pass_may_prune(&coverage) {
        let now = crate::outbox::unix_now();
        for (role, vanished) in &prunes {
            // The age guard is the other half: a row the outbox has just
            // ingested locally is not something the server ever listed under
            // that identity, so it is in every vanished set until the server's
            // own copy shows up.
            let prunable =
                crate::ingest::prunable_uids(&store, account_name, role.as_str(), vanished, now);
            result.pruned +=
                crate::ingest::prune_vanished(&store, &blobs, account_name, role.as_str(), &prunable);
        }
    } else {
        result.prunes_deferred = prunes.iter().map(|(_, v)| v.len()).sum();
        if result.prunes_deferred > 0 {
            info!(
                "Graph sync: {} pending prune(s) deferred; this pass did not see every message",
                result.prunes_deferred,
            );
        }
    }
    span.mark("prune");

    Ok(result)
}

// ---------------------------------------------------------------------------
// Phase 5: Send via Graph
// ---------------------------------------------------------------------------

impl GraphClient {
    /// Send an email via Graph API's /me/sendMail endpoint.
    /// Graph automatically places the message in Sent Items.
    pub async fn send_mail(
        &self,
        to: &[(&str, &str)],       // (name, address) pairs
        cc: &[(&str, &str)],
        bcc: &[(&str, &str)],
        subject: &str,
        html_body: &str,
        attachments: &[(String, Vec<u8>, String)], // (filename, content, content_type)
    ) -> Result<()> {
        let url = format!("{}/me/sendMail", GRAPH_BASE);

        let to_json: Vec<serde_json::Value> = to
            .iter()
            .map(|(name, addr)| {
                serde_json::json!({
                    "emailAddress": {
                        "name": name,
                        "address": addr
                    }
                })
            })
            .collect();

        let cc_json: Vec<serde_json::Value> = cc
            .iter()
            .map(|(name, addr)| {
                serde_json::json!({
                    "emailAddress": {
                        "name": name,
                        "address": addr
                    }
                })
            })
            .collect();

        let bcc_json: Vec<serde_json::Value> = bcc
            .iter()
            .map(|(name, addr)| {
                serde_json::json!({
                    "emailAddress": {
                        "name": name,
                        "address": addr
                    }
                })
            })
            .collect();

        let att_json: Vec<serde_json::Value> = attachments
            .iter()
            .map(|(filename, content, content_type)| {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(content);
                serde_json::json!({
                    "@odata.type": "#microsoft.graph.fileAttachment",
                    "name": filename,
                    "contentType": content_type,
                    "contentBytes": b64
                })
            })
            .collect();

        let mut message = serde_json::json!({
            "subject": subject,
            "body": {
                "contentType": "HTML",
                "content": html_body
            },
            "toRecipients": to_json,
        });

        if !cc_json.is_empty() {
            message["ccRecipients"] = serde_json::json!(cc_json);
        }
        if !bcc_json.is_empty() {
            message["bccRecipients"] = serde_json::json!(bcc_json);
        }
        if !att_json.is_empty() {
            message["attachments"] = serde_json::json!(att_json);
        }

        let payload = serde_json::json!({
            "message": message,
            "saveToSentItems": "true"
        });

        // A connect or builder failure never reached the API, so nothing was
        // sent; anything else (a timeout, a dropped response) leaves it
        // unknown, and the outbox must not re-send on its own. See
        // [`crate::outbox::AmbiguousSubmission`].
        let resp = match self
            .client
            .post(&url)
            .bearer_auth(self.bearer())
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) if e.is_connect() || e.is_builder() => {
                return Err(anyhow!(e).context("Failed to send mail via Graph API"))
            }
            Err(e) => {
                return Err(anyhow!(crate::outbox::AmbiguousSubmission(e.to_string()))
                    .context("Failed to send mail via Graph API"))
            }
        };

        if resp.status().is_success() || resp.status().as_u16() == 202 {
            info!("Graph sendMail succeeded");
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(anyhow!(
                "Graph sendMail failed (HTTP {}): {}",
                status,
                body
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 6: Archive, delete, mark read via Graph
// ---------------------------------------------------------------------------

impl GraphClient {
    /// Find a message by its RFC822 Internet Message-ID.
    /// Searches across all folders.
    pub async fn find_message_by_internet_id(
        &self,
        internet_message_id: &str,
    ) -> Result<Option<String>> {
        // The $filter on internetMessageId needs the angle brackets escaped
        let clean_id = internet_message_id
            .trim_matches(|c| c == '<' || c == '>');
        let url = format!(
            "{}/me/messages?$filter=internetMessageId eq '<{}>'&$select=id",
            GRAPH_BASE, clean_id
        );

        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.bearer())
            .send()
            .await
            .context("Failed to search for message by ID")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Find message by ID failed (HTTP {}): {}",
                status,
                body
            ));
        }

        let result: GraphMessageIdLookup =
            resp.json().await.context("Failed to parse message lookup")?;

        Ok(result.value.first().map(|e| e.id.clone()))
    }

    /// Move a message to a different folder.
    pub async fn move_message(
        &self,
        message_id: &str,
        destination_folder: &str,
    ) -> Result<()> {
        let dest = resolve_folder_path(destination_folder);
        let url = format!("{}/me/messages/{}/move", GRAPH_BASE, message_id);

        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.bearer())
            .json(&serde_json::json!({
                "destinationId": dest
            }))
            .send()
            .await
            .context("Failed to move message")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Move message failed (HTTP {}): {}",
                status,
                body
            ));
        }

        Ok(())
    }

    /// Delete a message permanently.
    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        let url = format!("{}/me/messages/{}", GRAPH_BASE, message_id);

        let resp = self
            .client
            .delete(&url)
            .bearer_auth(self.bearer())
            .send()
            .await
            .context("Failed to delete message")?;

        if !resp.status().is_success() && resp.status().as_u16() != 204 {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Delete message failed (HTTP {}): {}",
                status,
                body
            ));
        }

        Ok(())
    }

    /// Update the read status of a message.
    pub async fn update_read_status(
        &self,
        message_id: &str,
        is_read: bool,
    ) -> Result<()> {
        let url = format!("{}/me/messages/{}", GRAPH_BASE, message_id);

        let resp = self
            .client
            .patch(&url)
            .bearer_auth(self.bearer())
            .json(&serde_json::json!({
                "isRead": is_read
            }))
            .send()
            .await
            .context("Failed to update read status")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Update read status failed (HTTP {}): {}",
                status,
                body
            ));
        }

        Ok(())
    }
}

/// Move a message to another folder via Graph (#0018), naming it by its
/// `Message-ID` and touching nothing locally: the store row was already moved
/// optimistically by the caller (`crate::store::write`). Archiving is this with
/// the archive folder as destination.
///
/// A message the server does not have is not an error. Graph's copy may already
/// be gone (moved from another client), and the local row is what the user is
/// looking at.
pub async fn move_message_graph(
    config: &GraphConfig,
    internet_message_id: &str,
    dest_folder: &str,
) -> Result<()> {
    let client = GraphClient::new_async(config).await?;

    match client.find_message_by_internet_id(internet_message_id).await? {
        Some(graph_id) => {
            client.move_message(&graph_id, dest_folder).await?;
            info!("Graph: moved message {} to {}", internet_message_id, dest_folder);
        }
        None => warn!(
            "Graph: message {} not found on server, nothing to move",
            internet_message_id
        ),
    }
    Ok(())
}

/// Delete a message via Graph, naming it by its `Message-ID`. Same contract as
/// [`move_message_graph`]: server only, and a missing message is not an error.
pub async fn delete_message_graph(
    config: &GraphConfig,
    internet_message_id: &str,
) -> Result<()> {
    let client = GraphClient::new_async(config).await?;

    match client.find_message_by_internet_id(internet_message_id).await? {
        Some(graph_id) => {
            client.delete_message(&graph_id).await?;
            info!("Graph: deleted message {} from server", internet_message_id);
        }
        None => warn!(
            "Graph: message {} not found on server, nothing to delete",
            internet_message_id
        ),
    }
    Ok(())
}

/// Mark read/unread via Graph API.
pub async fn mark_read_graph(
    config: &GraphConfig,
    internet_message_id: &str,
    is_read: bool,
) -> Result<()> {
    let client = GraphClient::new_async(config).await?;

    if let Some(graph_id) = client
        .find_message_by_internet_id(internet_message_id)
        .await?
    {
        client.update_read_status(&graph_id, is_read).await?;
        Ok(())
    } else {
        // Typed not-found so the durable queue's drain converges a replay whose
        // read toggle already landed (#0039 review); a direct caller still sees
        // it as an error through `Display`.
        Err(crate::ops::NotFoundOnServer {
            message_id: internet_message_id.to_string(),
            mailbox: None,
        }
        .into())
    }
}

// ---------------------------------------------------------------------------
// Phase 7: Search via Graph
// ---------------------------------------------------------------------------

impl GraphClient {
    /// Search messages using Graph $search and $filter.
    pub async fn search_messages(
        &self,
        criteria: &crate::imap_client::FetchCriteria,
        folder: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FetchedEmail>> {
        let (search_param, filter_param) = parse_search_to_graph_params(criteria);

        let base = if let Some(f) = folder {
            let fp = resolve_folder_path(f);
            format!("{}/me/mailFolders/{}/messages", GRAPH_BASE, fp)
        } else {
            format!("{}/me/messages", GRAPH_BASE)
        };

        let mut url = format!(
            "{}?$top={}&$orderby=receivedDateTime desc&$select=id,internetMessageId,subject,from,toRecipients,ccRecipients,body,receivedDateTime,hasAttachments,isRead",
            base, limit
        );

        if let Some(ref search) = search_param {
            url.push_str(&format!("&$search=\"{}\"", search));
        }
        if let Some(ref filter) = filter_param {
            url.push_str(&format!("&$filter={}", filter));
        }

        debug!("Graph search URL: {}", url);

        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.bearer())
            .header("ConsistencyLevel", "eventual") // Required for $search
            .send()
            .await
            .context("Failed to search messages")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // If $search failed, fall back to $filter only
            if search_param.is_some() {
                warn!(
                    "Graph $search failed (HTTP {}), falling back to $filter only",
                    status
                );
                return self
                    .search_messages_filter_only(criteria, folder, limit)
                    .await;
            }
            return Err(anyhow!(
                "Graph search failed (HTTP {}): {}",
                status,
                body
            ));
        }

        let msg_list: GraphMessageList =
            resp.json().await.context("Failed to parse search results")?;

        let mut emails = Vec::with_capacity(msg_list.value.len());
        for msg in &msg_list.value {
            let mut email = graph_message_to_fetched_email(msg);
            if msg.has_attachments {
                if let Ok(att) = self.fetch_attachments(&msg.id).await {
                    email.attachments = att;
                }
            }
            populate_calendar_from_attachments(&mut email);
            emails.push(email);
        }

        crate::imap_client::retain_exact_message_id(&mut emails, criteria);

        Ok(emails)
    }

    /// Fallback search using only $filter (when $search is not available).
    async fn search_messages_filter_only(
        &self,
        criteria: &crate::imap_client::FetchCriteria,
        folder: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FetchedEmail>> {
        let (_, filter_param) = parse_search_to_graph_params(criteria);

        let base = if let Some(f) = folder {
            let fp = resolve_folder_path(f);
            format!("{}/me/mailFolders/{}/messages", GRAPH_BASE, fp)
        } else {
            format!("{}/me/messages", GRAPH_BASE)
        };

        let mut url = format!(
            "{}?$top={}&$orderby=receivedDateTime desc&$select=id,internetMessageId,subject,from,toRecipients,ccRecipients,body,receivedDateTime,hasAttachments,isRead",
            base, limit
        );

        if let Some(ref filter) = filter_param {
            url.push_str(&format!("&$filter={}", filter));
        }

        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.bearer())
            .send()
            .await
            .context("Failed to search messages (filter only)")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Graph search (filter) failed (HTTP {}): {}",
                status,
                body
            ));
        }

        let msg_list: GraphMessageList =
            resp.json().await.context("Failed to parse search results")?;

        let mut emails = Vec::with_capacity(msg_list.value.len());
        for msg in &msg_list.value {
            let mut email = graph_message_to_fetched_email(msg);
            if msg.has_attachments {
                if let Ok(att) = self.fetch_attachments(&msg.id).await {
                    email.attachments = att;
                }
            }
            populate_calendar_from_attachments(&mut email);
            emails.push(email);
        }

        crate::imap_client::retain_exact_message_id(&mut emails, criteria);

        Ok(emails)
    }
}

/// Convert FetchCriteria to Graph ($search, $filter) parameters.
fn parse_search_to_graph_params(
    criteria: &crate::imap_client::FetchCriteria,
) -> (Option<String>, Option<String>) {
    let mut search_parts: Vec<String> = Vec::new();
    let mut filter_parts: Vec<String> = Vec::new();

    // Free-text search (maps to $search)
    if let Some(ref text) = criteria.text {
        search_parts.push(text.clone());
    }

    // Structured fields
    if let Some(ref from) = criteria.from {
        filter_parts.push(format!(
            "from/emailAddress/address eq '{}'",
            from.replace('\'', "''")
        ));
    }
    if let Some(ref to) = criteria.to {
        // $filter on toRecipients requires /any() lambda
        filter_parts.push(format!(
            "toRecipients/any(r: r/emailAddress/address eq '{}')",
            to.replace('\'', "''")
        ));
    }
    if let Some(ref cc) = criteria.cc {
        filter_parts.push(format!(
            "ccRecipients/any(r: r/emailAddress/address eq '{}')",
            cc.replace('\'', "''")
        ));
    }
    if let Some(ref subject) = criteria.subject {
        // subject contains is better done via $search
        search_parts.push(format!("subject:{}", subject));
    }
    if let Some(ref body) = criteria.body {
        search_parts.push(format!("body:{}", body));
    }
    if let Some(ref since) = criteria.since {
        filter_parts.push(format!("receivedDateTime ge {}", since));
    }
    if let Some(ref before) = criteria.before {
        filter_parts.push(format!("receivedDateTime lt {}", before));
    }
    if let Some(ref message_id) = criteria.message_id {
        // Graph stores the header verbatim, angle brackets included.
        filter_parts.push(format!(
            "internetMessageId eq '{}'",
            crate::imap_client::bracketed_message_id(message_id).replace('\'', "''")
        ));
    }

    let search = if search_parts.is_empty() {
        None
    } else {
        Some(search_parts.join(" "))
    };

    let filter = if filter_parts.is_empty() {
        None
    } else {
        Some(filter_parts.join(" and "))
    };

    (search, filter)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn att(filename: &str, content: &str) -> crate::parse::AttachmentData {
        crate::parse::AttachmentData {
            filename: filename.to_string(),
            content: content.as_bytes().to_vec(),
            content_id: None,
        }
    }

    fn email_with(attachments: Vec<crate::parse::AttachmentData>) -> FetchedEmail {
        FetchedEmail {
            from: "a@b.com".into(),
            to: "me@x.com".into(),
            cc: None,
            subject: "s".into(),
            date: "d".into(),
            body_text: String::new(),
            html_body: None,
            has_attachments: true,
            message_id: None,
            attachments,
            flags: Default::default(),
            calendar_ics: None,
            event: None,
        }
    }

    const INVITE_ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:graph-invite-1\r\nSUMMARY:Sync\r\nDTSTART:20260720T120000Z\r\nDTEND:20260720T130000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const EXPORT_ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:graph-export-1\r\nSUMMARY:Dentist\r\nDTSTART:20260801T090000Z\r\nDTEND:20260801T093000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn graph_lifts_invite_and_keeps_shared_ics() {
        let mut email = email_with(vec![
            att("invite.ics", INVITE_ICS),
            att("my-calendar.ics", EXPORT_ICS),
        ]);
        populate_calendar_from_attachments(&mut email);
        // Invite lifted to sidecar + event block.
        assert_eq!(email.calendar_ics.as_deref(), Some(INVITE_ICS.as_bytes()));
        assert!(email.event.is_some());
        // The non-invite export survives as a regular attachment, filename kept.
        assert_eq!(email.attachments.len(), 1);
        assert_eq!(email.attachments[0].filename, "my-calendar.ics");
        assert_eq!(email.attachments[0].content, EXPORT_ICS.as_bytes());
    }

    #[test]
    fn graph_non_imip_ics_export_stays_a_plain_attachment() {
        let mut email = email_with(vec![att("schedule.ics", EXPORT_ICS)]);
        populate_calendar_from_attachments(&mut email);
        assert!(email.calendar_ics.is_none(), "no sidecar for a plain export");
        assert!(email.event.is_none());
        assert_eq!(email.attachments.len(), 1);
        assert_eq!(email.attachments[0].filename, "schedule.ics");
    }

    // -----------------------------------------------------------------------
    // Sync diff: what a folder enumeration says to download and to prune
    // -----------------------------------------------------------------------

    fn entry(graph_id: &str, received: Option<&str>) -> FolderEntry {
        FolderEntry {
            graph_id: graph_id.to_string(),
            is_read: false,
            received: received.map(|s| s.to_string()),
        }
    }

    /// A folder as the server enumerates it: `(internetMessageId, graph id,
    /// receivedDateTime)`.
    fn folder(rows: &[(&str, &str, Option<&str>)]) -> HashMap<String, FolderEntry> {
        rows.iter()
            .map(|(mid, gid, received)| (mid.to_string(), entry(gid, *received)))
            .collect()
    }

    fn known(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// The defect that made a Graph sync never converge: a message that is new
    /// to the store but old on the server. Detection always found it; the
    /// download asked the folder for its most recent messages and never got it
    /// back. Selection now hands the caller that message's own Graph id.
    #[test]
    fn an_old_message_new_to_the_store_is_selected_for_download() {
        let server = folder(&[
            ("<recent@x>", "AAA", Some("2026-08-06T10:00:00Z")),
            ("<moved-in@x>", "BBB", Some("2024-01-01T09:00:00Z")),
        ]);
        let selected = new_ids_newest_first(&server, &known(&["<recent@x>"]));
        assert_eq!(
            selected.iter().map(|e| e.graph_id.as_str()).collect::<Vec<_>>(),
            vec!["BBB"],
        );
    }

    /// Newest first, so a capped pass takes the arrivals a user is waiting for;
    /// no `received` sorts last rather than dropping out.
    #[test]
    fn new_ids_come_back_newest_first() {
        let server = folder(&[
            ("<old@x>", "OLD", Some("2026-01-01T00:00:00Z")),
            ("<new@x>", "NEW", Some("2026-08-06T00:00:00Z")),
            ("<undated@x>", "UND", None),
        ]);
        let selected = new_ids_newest_first(&server, &HashSet::new());
        assert_eq!(
            selected.iter().map(|e| e.graph_id.as_str()).collect::<Vec<_>>(),
            vec!["NEW", "OLD", "UND"],
        );
    }

    /// Same timestamp on two messages must still give one order, or a truncated
    /// pass would pick a different pair each time and neither would land.
    #[test]
    fn ties_break_on_the_message_id_so_a_capped_pass_is_reproducible() {
        let server = folder(&[
            ("<b@x>", "B", Some("2026-08-06T00:00:00Z")),
            ("<a@x>", "A", Some("2026-08-06T00:00:00Z")),
        ]);
        let first = new_ids_newest_first(&server, &HashSet::new());
        let second = new_ids_newest_first(&server, &HashSet::new());
        assert_eq!(
            first.iter().map(|e| e.graph_id.as_str()).collect::<Vec<_>>(),
            vec!["A", "B"],
        );
        assert_eq!(first, second);
    }

    /// The prune set: what the store holds for this mailbox and the server did
    /// not list, as the UIDs the rows carry.
    #[test]
    fn a_message_gone_from_the_folder_is_a_vanished_uid() {
        let server = folder(&[("<stays@x>", "AAA", None)]);
        let vanished = vanished_graph_uids(&known(&["<stays@x>", "<archived@x>"]), &server);
        assert_eq!(vanished, vec![crate::ingest::graph_uid("<archived@x>")]);
    }

    #[test]
    fn a_folder_the_store_matches_prunes_nothing() {
        let server = folder(&[("<a@x>", "AAA", None), ("<b@x>", "BBB", None)]);
        assert!(vanished_graph_uids(&known(&["<a@x>", "<b@x>"]), &server).is_empty());
    }

    /// #0065 item 2. The enumeration keys on the trimmed `internetMessageId`,
    /// which is what ingest stores. Keyed verbatim, a padded header made the
    /// same message look new (its trimmed row id is not the map key) *and*
    /// vanished (its row id is not in the map): a delete-and-re-download loop
    /// once the prune landed.
    #[test]
    fn a_padded_internet_message_id_neither_loops_nor_prunes() {
        let mut map = HashMap::new();
        absorb_page(
            &mut map,
            vec![GraphMessageIdEntry {
                id: "AAA".into(),
                internet_message_id: Some("  <padded@x>\r\n".into()),
                is_read: false,
                received_date_time: Some("2026-08-06T10:00:00Z".into()),
            }],
        );
        assert_eq!(map.keys().collect::<Vec<_>>(), vec!["<padded@x>"]);

        // The store holds it under the id `resolve_message_id` produced, which
        // is the same trim.
        let stored = known(&["<padded@x>"]);
        assert!(
            select_for_download(&map, &stored, usize::MAX).0.is_empty(),
            "a message the store holds must not be downloaded again",
        );
        assert!(
            vanished_graph_uids(&stored, &map).is_empty(),
            "a message the folder still lists must not be pruned",
        );
    }

    /// An entry with no usable id is dropped rather than keyed on an empty
    /// string, which would collide every such message onto one map entry.
    #[test]
    fn an_entry_without_a_message_id_is_dropped() {
        let mut map = HashMap::new();
        absorb_page(
            &mut map,
            vec![
                GraphMessageIdEntry {
                    id: "AAA".into(),
                    internet_message_id: None,
                    is_read: false,
                    received_date_time: None,
                },
                GraphMessageIdEntry {
                    id: "BBB".into(),
                    internet_message_id: Some("   ".into()),
                    is_read: false,
                    received_date_time: None,
                },
            ],
        );
        assert!(map.is_empty());
    }

    // -----------------------------------------------------------------------
    // #0065 item 4: a capped pass does not prune
    // -----------------------------------------------------------------------

    /// `limit` cuts the download, and the cut is reported: `found` is what the
    /// pass would have taken, the vector is what it took.
    #[test]
    fn a_capped_pass_reports_what_it_left_behind() {
        let server = folder(&[
            ("<a@x>", "A", Some("2026-08-06T03:00:00Z")),
            ("<b@x>", "B", Some("2026-08-06T02:00:00Z")),
            ("<c@x>", "C", Some("2026-08-06T01:00:00Z")),
        ]);
        let (selected, found) = select_for_download(&server, &HashSet::new(), 2);
        assert_eq!(found, 3);
        assert_eq!(
            selected.iter().map(|e| e.graph_id.as_str()).collect::<Vec<_>>(),
            vec!["A", "B"],
        );
        let (all, found) = select_for_download(&server, &HashSet::new(), usize::MAX);
        assert_eq!((all.len(), found), (3, 3), "a full sync never truncates");
    }

    /// The gate itself. A quick sync that could not download its whole backlog
    /// must not delete anything, and it must not delete anything *anywhere*:
    /// the inbox row of a message archived elsewhere is only safe to drop
    /// because the archive pass ingested its copy, and the archive pass is the
    /// one that was capped.
    #[test]
    fn one_capped_target_suspends_the_prune_for_every_target() {
        // (enumeration complete, download truncated) per target.
        assert!(pass_may_prune(&[(true, false), (true, false), (true, false)]));
        assert!(!pass_may_prune(&[(true, false), (true, true), (true, false)]));
        assert!(!pass_may_prune(&[(false, false), (true, false)]));
        assert!(pass_may_prune(&[]), "a pass with no targets prunes nothing anyway");
    }

    // -----------------------------------------------------------------------
    // #0065 items 5 and 6: the /$batch sub-requests
    // -----------------------------------------------------------------------

    /// Graph parses the sub-request URL out of the JSON itself, so nothing else
    /// on this path escapes the id.
    #[test]
    fn batch_sub_request_ids_are_percent_encoded() {
        let body = batch_request_body(&["AA/BB+CC=", "plain-_id="]);
        let url = body["requests"][0]["url"].as_str().unwrap();
        assert!(url.starts_with("/me/messages/AA%2FBB%2BCC=?"), "got {url}");
        // The base64url alphabet and `=` stay legible: the common case is
        // unchanged.
        let url = body["requests"][1]["url"].as_str().unwrap();
        assert!(url.starts_with("/me/messages/plain-_id=?"), "got {url}");
    }

    fn batch_entry(status: u16, headers: &[(&str, &str)]) -> GraphBatchEntry {
        GraphBatchEntry {
            id: "0".into(),
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
                .collect(),
            body: None,
        }
    }

    /// A 429 *sub*-response inside a 200 batch is the throttle a big first sync
    /// actually meets; its `Retry-After` paces the remaining chunks.
    #[test]
    fn a_throttled_sub_response_asks_for_a_pause() {
        assert_eq!(
            batch_entry(429, &[("Retry-After", "7")]).retry_after_secs(),
            Some(7),
        );
        assert_eq!(
            batch_entry(429, &[("retry-after", " 7 ")]).retry_after_secs(),
            Some(7),
            "Graph promises no casing",
        );
        assert_eq!(
            batch_entry(429, &[("Retry-After", "999999")]).retry_after_secs(),
            Some(MAX_RETRY_AFTER_SECS),
            "a hostile header cannot park the sync",
        );
        assert_eq!(batch_entry(429, &[]).retry_after_secs(), None);
        assert_eq!(
            batch_entry(404, &[("Retry-After", "7")]).retry_after_secs(),
            None,
            "only a throttle is a reason to wait",
        );
    }

    // -----------------------------------------------------------------------
    // #0065 follow-up: the batch's own failure modes
    // -----------------------------------------------------------------------

    /// A header value that is not a JSON string must not fail the parse of the
    /// whole chunk. It used to: `HashMap<String, String>` made one numeric or
    /// array-valued header on one sub-response into zero downloads for that
    /// folder, on every pass, with the prune suspended throughout.
    #[test]
    fn a_non_string_sub_response_header_does_not_fail_the_batch() {
        let raw = serde_json::json!({
            "responses": [
                {
                    "id": "0",
                    "status": 429,
                    "headers": {
                        "Retry-After": 9,
                        "Content-Length": 512,
                        "X-Weird": ["a", "b"],
                    },
                },
                { "id": "1", "status": 200, "headers": { "Content-Type": "application/json" } },
            ]
        });
        let parsed: GraphBatchResponse =
            serde_json::from_value(raw).expect("a header shape this code does not read cannot \
                                                be allowed to fail the whole chunk");
        assert_eq!(parsed.responses.len(), 2);
        assert_eq!(
            parsed.responses[0].retry_after_secs(),
            Some(9),
            "a numeric Retry-After is still a Retry-After",
        );
        assert_eq!(parsed.responses[1].retry_after_secs(), None);
    }

    /// Throttling is back-pressure, not failure: it paces the pass but does not
    /// spend the budget that exists to stop a pass that cannot succeed. A 503
    /// counts as a throttle only when it carries the header, because a bare one
    /// is indistinguishable from the service being down.
    #[test]
    fn a_throttle_is_not_a_failure_but_a_bare_503_is() {
        assert!(batch_entry(429, &[("Retry-After", "7")]).is_throttled());
        assert!(batch_entry(429, &[]).is_throttled(), "429 throttles with or without the header");
        assert!(batch_entry(503, &[("Retry-After", "7")]).is_throttled());
        assert_eq!(
            batch_entry(503, &[("Retry-After", "7")]).retry_after_secs(),
            Some(7),
            "Graph's throttling guidance names 503 alongside 429",
        );
        assert!(!batch_entry(503, &[]).is_throttled());
        assert!(!batch_entry(404, &[("Retry-After", "7")]).is_throttled());
        assert!(!batch_entry(200, &[]).is_throttled());
    }

    /// The gap #0065 shipped with: `download_incomplete` was derived from
    /// `limit` alone, so a target whose batch was throttled out returned no
    /// messages and still reported a complete download. The prune gate then
    /// opened on inbox rows whose archive copies had never landed.
    #[test]
    fn a_short_batch_return_marks_the_pass_incomplete() {
        // (ids asked for, messages returned, the budget ran out).
        assert!(!batch_fell_short(20, 20, false));
        assert!(
            batch_fell_short(20, 19, false),
            "one failed sub-response is one message the store does not hold",
        );
        assert!(
            batch_fell_short(20, 0, false),
            "a wholly throttled-out target is the case that opened the gate",
        );
        assert!(
            batch_fell_short(0, 0, true),
            "a pass that spent its failure budget did not see the folder",
        );
        // And the gate is closed by it, whichever target came up short.
        assert!(!pass_may_prune(&[(true, false), (true, true)]));
    }

    /// The `$orderby` fallback: a tenant that rejects the ordered enumeration
    /// gets an unordered walk rather than a folder that never syncs.
    #[test]
    fn the_enumeration_url_can_drop_the_orderby() {
        let ordered = enumeration_url("inbox", true);
        assert!(ordered.contains("$orderby=receivedDateTime%20desc"), "got {ordered}");
        let unordered = enumeration_url("inbox", false);
        assert!(!unordered.contains("$orderby"), "got {unordered}");
        for url in [&ordered, &unordered] {
            assert!(url.contains("/me/mailFolders/inbox/messages?"), "got {url}");
            assert!(url.contains("$select=id,internetMessageId,isRead,receivedDateTime"));
            assert!(url.contains("$top=200"));
        }
    }

    #[test]
    fn batch_body_names_one_get_per_id() {
        let body = batch_request_body(&["AAA", "BBB"]);
        let requests = body["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["id"], "0");
        assert_eq!(requests[1]["id"], "1");
        assert_eq!(requests[0]["method"], "GET");
        let url = requests[1]["url"].as_str().unwrap();
        assert!(url.starts_with("/me/messages/BBB?$select="), "got {url}");
        assert!(url.contains("internetMessageId"));
    }

    #[test]
    fn test_iso_to_rfc2822() {
        let rfc = iso_to_rfc2822("2024-01-15T10:30:00Z");
        assert!(rfc.contains("2024"));
        assert!(rfc.contains("10:30"));
    }

    #[test]
    fn test_iso_to_rfc2822_with_timezone() {
        let rfc = iso_to_rfc2822("2024-06-15T14:30:00+02:00");
        assert!(rfc.contains("2024"));
    }

    #[test]
    fn test_iso_to_rfc2822_invalid() {
        let result = iso_to_rfc2822("not-a-date");
        assert_eq!(result, "not-a-date");
    }

    #[test]
    fn test_format_recipient_with_name() {
        let r = GraphRecipient {
            email_address: GraphEmailAddress {
                name: Some("John Doe".to_string()),
                address: Some("john@example.com".to_string()),
            },
        };
        assert_eq!(format_recipient(&r), "John Doe <john@example.com>");
    }

    #[test]
    fn test_format_recipient_no_name() {
        let r = GraphRecipient {
            email_address: GraphEmailAddress {
                name: None,
                address: Some("john@example.com".to_string()),
            },
        };
        assert_eq!(format_recipient(&r), "john@example.com");
    }

    #[test]
    fn test_resolve_folder_path_wellknown() {
        assert_eq!(resolve_folder_path("Inbox"), "inbox");
        assert_eq!(resolve_folder_path("SentItems"), "sentitems");
        assert_eq!(resolve_folder_path("Archive"), "archive");
    }

    #[test]
    fn test_resolve_folder_path_custom() {
        assert_eq!(resolve_folder_path("MyFolder"), "MyFolder");
    }

    #[test]
    fn test_parse_search_to_graph_params_text_only() {
        let criteria = crate::imap_client::FetchCriteria {
            text: Some("hello world".to_string()),
            ..Default::default()
        };
        let (search, filter) = parse_search_to_graph_params(&criteria);
        assert_eq!(search.unwrap(), "hello world");
        assert!(filter.is_none());
    }

    #[test]
    fn test_parse_search_to_graph_params_from_filter() {
        let criteria = crate::imap_client::FetchCriteria {
            from: Some("alice@example.com".to_string()),
            ..Default::default()
        };
        let (search, filter) = parse_search_to_graph_params(&criteria);
        assert!(search.is_none());
        assert!(filter.unwrap().contains("from/emailAddress/address"));
    }

    #[test]
    fn test_parse_search_to_graph_params_date_range() {
        let criteria = crate::imap_client::FetchCriteria {
            since: Some("2024-01-01".to_string()),
            before: Some("2024-02-01".to_string()),
            ..Default::default()
        };
        let (_, filter) = parse_search_to_graph_params(&criteria);
        let f = filter.unwrap();
        assert!(f.contains("receivedDateTime ge"));
        assert!(f.contains("receivedDateTime lt"));
    }

    // -----------------------------------------------------------------------
    // #0074 review: the Graph ingest-failure bound
    // -----------------------------------------------------------------------

    /// The Graph sync loop folds an ingest failure into `truncated`, which
    /// suspends the account's prune. Without a bound, a message this store
    /// rejects every time would do that on every pass, for good: the deadlock
    /// #0074 closed on the IMAP side.
    ///
    /// This walks the two calls the Graph loop makes per failed message, in
    /// production order (`note_ingest_failure` folded into `ingest_failed`,
    /// then `pass_may_prune` over the coverage tuple), over the real store.
    #[test]
    fn a_poisoned_graph_message_stops_holding_the_prune_after_three_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("store.sqlite3")).unwrap();
        let uid = crate::ingest::graph_uid("<poison@example.com>");

        for pass in 1..=2 {
            let ingest_failed = crate::ingest::note_ingest_failure(
                &store, "acct", "inbox", "Inbox", uid, "the store will not take it",
            );
            assert!(ingest_failed, "pass {pass} must still retry");
            assert!(
                !crate::ingest::pass_may_prune(&[(true, ingest_failed)]),
                "pass {pass} still reports itself short, so the prune stays deferred"
            );
        }

        let ingest_failed = crate::ingest::note_ingest_failure(
            &store, "acct", "inbox", "Inbox", uid, "the store will not take it",
        );
        assert!(!ingest_failed, "the third failure gives up on the message");
        assert!(
            crate::ingest::pass_may_prune(&[(true, ingest_failed)]),
            "and the Graph account's prune runs again instead of being suspended for good"
        );
        assert_eq!(crate::ingest::ingest_failure_attempts(&store, "acct", "inbox", uid), 3);
    }

    /// The other half of the bound: a success clears the count, so a message
    /// that fails transiently gets a full three attempts every time, not three
    /// for its lifetime.
    #[test]
    fn a_successful_graph_ingest_clears_the_failure_count() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("store.sqlite3")).unwrap();
        let uid = crate::ingest::graph_uid("<flaky@example.com>");

        assert!(crate::ingest::note_ingest_failure(&store, "acct", "inbox", "Inbox", uid, "locked"));
        assert!(crate::ingest::note_ingest_failure(&store, "acct", "inbox", "Inbox", uid, "locked"));
        assert_eq!(crate::ingest::ingest_failure_attempts(&store, "acct", "inbox", uid), 2);

        // The success path of the loop's `match`.
        crate::ingest::clear_ingest_failure(&store, "acct", "inbox", uid);
        assert_eq!(crate::ingest::ingest_failure_attempts(&store, "acct", "inbox", uid), 0);

        assert!(crate::ingest::note_ingest_failure(&store, "acct", "inbox", "Inbox", uid, "locked"));
        assert_eq!(
            crate::ingest::ingest_failure_attempts(&store, "acct", "inbox", uid),
            1,
            "the next failure starts the count over"
        );
    }

    // -----------------------------------------------------------------
    // #0042: /messages/delta
    // -----------------------------------------------------------------

    fn delta_page(json: serde_json::Value) -> GraphDeltaPage {
        serde_json::from_value(json).expect("a delta page must parse")
    }

    #[test]
    fn the_delta_decision_matrix() {
        let token = format!("{GRAPH_BASE}/me/mailFolders('AAA')/messages/delta?$deltatoken=t1");

        // No token: the bootstrap pass enumerates and mints one.
        assert_eq!(delta_verdict(100, None, false), DeltaVerdict::NoToken);
        assert_eq!(delta_verdict(usize::MAX, None, true), DeltaVerdict::NoToken);

        // A full sync always relists: it is the periodic whole-folder
        // observation the prune and the token both lean on.
        assert_eq!(
            delta_verdict(usize::MAX, Some(&token), true),
            DeltaVerdict::FullSync
        );

        // A quick sync with a token minted against this folder is the only
        // branch that takes the delta.
        assert_eq!(delta_verdict(100, Some(&token), true), DeltaVerdict::Use);

        // The Graph analogue of a UIDVALIDITY change: a folder that is not the
        // one the token was minted against invalidates it, whatever the sync.
        assert_eq!(
            delta_verdict(100, Some(&token), false),
            DeltaVerdict::FolderChanged
        );
        assert_eq!(
            delta_verdict(usize::MAX, Some(&token), false),
            DeltaVerdict::FolderChanged
        );

        // Anything that is not a delta URL this client would have stored is
        // thrown away rather than sent to the server.
        for junk in ["", "not a url", "https://evil.example/me/messages/delta"] {
            assert_eq!(
                delta_verdict(100, Some(junk), true),
                DeltaVerdict::Discard(DeltaDiscard::Malformed),
                "{junk:?} is not a token"
            );
        }
        let no_delta = format!("{GRAPH_BASE}/me/mailFolders('AAA')/messages?$top=200");
        assert_eq!(
            delta_verdict(100, Some(&no_delta), true),
            DeltaVerdict::Discard(DeltaDiscard::Malformed)
        );
    }

    #[test]
    fn an_expired_delta_link_is_discarded_and_anything_else_unusual_with_it() {
        use reqwest::StatusCode;
        assert_eq!(delta_status_discard(StatusCode::OK), None);
        // The documented expiry: 410 Gone / resyncRequired.
        assert_eq!(
            delta_status_discard(StatusCode::GONE),
            Some(DeltaDiscard::Expired)
        );
        // The folder went out from under the token.
        assert_eq!(
            delta_status_discard(StatusCode::NOT_FOUND),
            Some(DeltaDiscard::Expired)
        );
        // Everything else is a failure, and a failed walk is indistinguishable
        // from one that skipped a message, so it drops the token too.
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::UNAUTHORIZED,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert_eq!(
                delta_status_discard(status),
                Some(DeltaDiscard::Failed),
                "{status} must not be trusted"
            );
        }
    }

    #[test]
    fn a_removed_entry_is_counted_and_never_mistaken_for_a_message() {
        let page = delta_page(serde_json::json!({
            "value": [
                {
                    "id": "AAA",
                    "internetMessageId": " <kept@example.com> ",
                    "isRead": true,
                    "receivedDateTime": "2026-01-01T00:00:00Z"
                },
                // A removal carries the Graph id and nothing else: no
                // internetMessageId, which is the identity the store keys on.
                { "id": "BBB", "@removed": { "reason": "deleted" } },
                { "id": "CCC", "@removed": { "reason": "changed" } },
                // No usable identity: dropped, as in a full enumeration.
                { "id": "DDD", "isRead": false }
            ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/x/delta?$deltatoken=t2"
        }));
        let mut changed = HashMap::new();
        let mut removed = 0usize;
        absorb_delta_page(&mut changed, &mut removed, page.value);

        assert_eq!(removed, 2, "both removal reasons count");
        assert_eq!(changed.len(), 1);
        let entry = changed.get("<kept@example.com>").expect("keyed on the trimmed id");
        assert_eq!(entry.graph_id, "AAA");
        assert!(entry.is_read);

        let delta = FolderDelta {
            changed,
            removed,
            delta_link: page.delta_link.unwrap(),
            pages: 1,
        };
        assert!(
            delta.forces_full_enumeration(),
            "a delta that reports removals hands the pass to the enumeration, because the \
             prune resolves a deletion by listing the folder and not from @removed"
        );
        assert!(!FolderDelta::default().forces_full_enumeration());
    }

    #[test]
    fn a_change_set_is_never_a_folder_listing() {
        // Why the orchestrator only computes the prune on a full enumeration:
        // fed a delta's change set, the same diff calls every message that
        // simply did not change since the token a deletion.
        let known = known(&["<a@x>", "<b@x>", "<c@x>"]);
        let change_set = folder(&[("<b@x>", "BBB", Some("2026-01-01T00:00:00Z"))]);
        let vanished = vanished_graph_uids(&known, &change_set);
        assert_eq!(
            vanished.len(),
            2,
            "which would be a mass prune, so `used_delta` gates the call"
        );
    }

    #[test]
    fn a_resume_point_is_only_minted_by_a_pass_that_covered_the_folder() {
        // covered, nothing left behind, everything written.
        assert!(may_record_delta_token(true, false, false));
        // An enumeration that did not see the whole folder.
        assert!(!may_record_delta_token(false, false, false));
        // A pass that left new messages undownloaded.
        assert!(!may_record_delta_token(true, true, false));
        // A message downloaded and not written is as absent as one never
        // fetched, so the token would over-claim.
        assert!(!may_record_delta_token(true, false, true));
    }

    #[test]
    fn a_poisoned_message_cannot_wedge_the_delta_chain_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("store.sqlite3")).unwrap();
        let uid = crate::ingest::graph_uid("<poison@example.com>");

        // The #0074 bound applies identically on the delta path: while the
        // message is still owed the token does not advance, so the next pass
        // replays the same changes...
        for pass in 1..=2 {
            let ingest_failed = crate::ingest::note_ingest_failure(
                &store, "acct", "inbox", "Inbox", uid, "the store will not take it",
            );
            assert!(
                !may_record_delta_token(true, false, ingest_failed),
                "pass {pass} has not written everything, so it mints no token"
            );
        }
        // ...and once the message is given up on, the chain moves again.
        let ingest_failed = crate::ingest::note_ingest_failure(
            &store, "acct", "inbox", "Inbox", uid, "the store will not take it",
        );
        assert!(!ingest_failed);
        assert!(
            may_record_delta_token(true, false, ingest_failed),
            "a message the store will never accept must not freeze the delta for ever"
        );
    }

    #[test]
    fn the_delta_token_survives_a_full_window_pass_and_only_a_clear_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("store.sqlite3")).unwrap();
        let link = format!("{GRAPH_BASE}/me/mailFolders('AAA')/messages/delta?$deltatoken=t1");
        let identity = folder_identity_hash("AAA");

        crate::ingest::record_delta_token(&store, "acct", "inbox", identity, &link);
        let stored = crate::ingest::load_mailbox_cursor(&store, "acct", "inbox")
            .unwrap()
            .unwrap();
        assert_eq!(stored.deltalink.as_deref(), Some(link.as_str()));
        assert_eq!(stored.uidvalidity, Some(identity));

        // The #0054 carry-forward hazard, pinned for `deltalink` the way #0041
        // pinned it for `highest_modseq`: a full-window pass says nothing about
        // the token and must not wipe it.
        crate::ingest::record_mailbox_cursor(
            &store,
            "acct",
            "inbox",
            &crate::ingest::MailboxCursor {
                uidvalidity: Some(identity),
                exists: Some(42),
                deltalink: None,
                ..Default::default()
            },
        )
        .unwrap();
        let stored = crate::ingest::load_mailbox_cursor(&store, "acct", "inbox")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.deltalink.as_deref(),
            Some(link.as_str()),
            "a pass with nothing to say about the token leaves it alone"
        );

        // A folder identity change is what makes the token meaningless, and
        // the only way out is the explicit clear.
        let observed = folder_identity_hash("BBB");
        assert_ne!(observed, identity);
        assert_eq!(
            delta_verdict(100, stored.deltalink.as_deref(), observed == identity),
            DeltaVerdict::FolderChanged
        );
        crate::ingest::clear_mailbox_deltalink(&store, "acct", "inbox");
        let stored = crate::ingest::load_mailbox_cursor(&store, "acct", "inbox")
            .unwrap()
            .unwrap();
        assert_eq!(stored.deltalink, None, "the token cannot survive the reset");
        assert_eq!(
            delta_verdict(100, stored.deltalink.as_deref(), true),
            DeltaVerdict::NoToken,
            "and the next pass enumerates and mints a fresh one"
        );
    }

    #[test]
    fn the_delta_url_has_a_listing_form_and_a_mint_form() {
        let listing = delta_url("inbox", false);
        assert!(listing.starts_with(&format!("{GRAPH_BASE}/me/mailFolders/inbox/messages/delta?")));
        assert!(listing.contains("$select=id,internetMessageId,isRead,receivedDateTime"));
        assert!(
            !listing.contains("$deltatoken"),
            "the listing form asks for the folder's state, not for a bare token"
        );
        // No `$top`: the delta endpoint takes its page size from the `Prefer`
        // header, which `walk_delta` sends.
        assert!(!listing.contains("$top"));
        assert!(delta_url("inbox", true).ends_with("&$deltatoken=latest"));
    }

    #[test]
    fn a_delta_page_chain_ends_in_a_resume_point() {
        // The two shapes `walk_delta` branches on: a page that continues, and
        // a page that closes the chain.
        let middle = delta_page(serde_json::json!({
            "value": [],
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/next"
        }));
        assert_eq!(middle.next_link.as_deref(), Some("https://graph.microsoft.com/v1.0/next"));
        assert_eq!(middle.delta_link, None);

        let last = delta_page(serde_json::json!({
            "value": [],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/d?$deltatoken=t2"
        }));
        assert_eq!(last.next_link, None);
        assert!(last.delta_link.is_some());

        // A chain that simply stops has no resume point and no proof it was
        // complete; `walk_delta` answers `NoResumePoint` and the token goes.
        let orphan = delta_page(serde_json::json!({ "value": [] }));
        assert!(orphan.next_link.is_none() && orphan.delta_link.is_none());
    }
}
