// Microsoft Graph API REST client for Exchange Online mail operations.
//
// Provides a stateless HTTP client that wraps `reqwest::Client` + OAuth2 access token.
// Each function corresponds to a Graph API endpoint and returns domain types
// (`FetchedEmail`, folder lists, etc.) that integrate with the existing local
// storage layer (`.md` + `.html` + `_attachments/`).

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use serde::Deserialize;

use crate::config::GraphConfig;
use crate::imap_client::{FreshObservation, SyncResult, SyncTarget};
use crate::parse::{sanitize_attachment_filename, AttachmentData, FetchedEmail};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

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

/// Lightweight response for fetching just Message-IDs and read status.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageIdEntry {
    #[serde(default)]
    internet_message_id: Option<String>,
    #[serde(default)]
    is_read: bool,
}

#[derive(Debug, Deserialize)]
struct GraphMessageIdList {
    value: Vec<GraphMessageIdEntry>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
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
             $select=id,internetMessageId,subject,from,toRecipients,ccRecipients,body,receivedDateTime,hasAttachments,isRead",
            GRAPH_BASE, folder_path, limit
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
            let mut email = graph_message_to_fetched_email(msg);

            // Fetch attachments if the message has them
            if msg.has_attachments {
                match self.fetch_attachments(&msg.id).await {
                    Ok(attachments) => email.attachments = attachments,
                    Err(e) => warn!("Failed to fetch attachments for {}: {}", msg.id, e),
                }
                populate_calendar_from_attachments(&mut email);
            }

            emails.push(email);
        }

        Ok(emails)
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
        is_read: msg.is_read,
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
    /// Fetch all internet message IDs from a folder (for dedup).
    /// Also returns the read status for each message.
    pub async fn fetch_message_ids(
        &self,
        folder: &str,
    ) -> Result<HashMap<String, bool>> {
        let folder_path = resolve_folder_path(folder);
        let mut result = HashMap::new();
        let mut url = format!(
            "{}/me/mailFolders/{}/messages?$select=internetMessageId,isRead&$top=200",
            GRAPH_BASE, folder_path
        );

        loop {
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
                return Err(anyhow!(
                    "Fetch message IDs from '{}' failed (HTTP {}): {}",
                    folder,
                    status,
                    body
                ));
            }

            let page: GraphMessageIdList =
                resp.json().await.context("Failed to parse message ID list")?;

            for entry in page.value {
                if let Some(mid) = entry.internet_message_id {
                    if !mid.is_empty() {
                        result.insert(mid, entry.is_read);
                    }
                }
            }

            match page.next_link {
                Some(next) => url = next,
                None => break,
            }
        }

        Ok(result)
    }

    /// Two-pass fetch: first get IDs, then full messages for new ones only.
    /// Returns (new_emails, skipped_count, server_read_flags), where the flag
    /// map covers *every* message in the folder so the ingest path can apply
    /// read-status changes to rows it already holds.
    pub async fn fetch_new_messages(
        &self,
        folder: &str,
        limit: usize,
        known_ids: &HashSet<String>,
    ) -> Result<(Vec<FetchedEmail>, usize, HashMap<String, bool>)> {
        let server_ids = self.fetch_message_ids(folder).await?;
        let new_ids: Vec<&String> = server_ids
            .keys()
            .filter(|id| !known_ids.contains(id.as_str()))
            .collect();

        let skipped = server_ids.len() - new_ids.len();
        let to_fetch = new_ids.len().min(limit);

        if to_fetch == 0 {
            return Ok((Vec::new(), skipped, server_ids));
        }

        // Fetch the full messages. We use the folder endpoint with a limit,
        // then filter to only the new ones.
        let fetch_limit = to_fetch + skipped.min(20); // over-fetch slightly for robustness
        let all = self.fetch_messages(folder, fetch_limit).await?;

        let new_set: HashSet<&str> = new_ids.iter().map(|s| s.as_str()).collect();
        let filtered: Vec<FetchedEmail> = all
            .into_iter()
            .filter(|e| {
                e.message_id
                    .as_deref()
                    .map(|mid| new_set.contains(mid))
                    .unwrap_or(true) // keep messages without message-id (unlikely)
            })
            .take(to_fetch)
            .collect();

        Ok((filtered, skipped, server_ids))
    }
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
/// TODO(#0037-4b-or-0038): this still enumerates `internetMessageId` for the
/// whole folder on every pass. The `sync_cursors.deltalink` column is written
/// as NULL until the `/messages/delta` fetch replaces that enumeration.
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

    let client = GraphClient::new_async(config).await?;
    let store = crate::store::Store::open_account(account_name)?;
    let blobs = crate::store::BlobStore::for_account(account_name);
    let mut result = SyncResult::default();

    for target in targets {
        let known = crate::ingest::known_message_ids(&store, account_name, &target.role)?;

        let (new_emails, skipped, server_flags) = match client
            .fetch_new_messages(&target.server_name, limit, &known)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("Graph sync failed for {}: {}", target.role, e);
                continue;
            }
        };
        result.skipped += skipped;

        if dry_run {
            result.saved += new_emails.len();
            continue;
        }

        for email in &new_emails {
            let message_id = crate::ingest::resolve_message_id(email, None);
            let uid = crate::ingest::graph_uid(&message_id);
            match crate::ingest::ingest_message(
                &store,
                &blobs,
                &crate::ingest::IngestInput {
                    account: account_name,
                    mailbox: &target.role,
                    uid,
                    email,
                    raw: None,
                },
            ) {
                Ok(outcome) => {
                    if outcome.inserted {
                        result.saved += 1;
                        if target.role.eq_ignore_ascii_case("inbox") {
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
                Err(e) => warn!("Failed to ingest {} from {}: {:#}", message_id, target.role, e),
            }
        }

        // Read status for messages the store already holds.
        for (mid, is_read) in &server_flags {
            let uid = crate::ingest::graph_uid(mid);
            match crate::ingest::apply_seen_flag(&store, account_name, &target.role, uid, *is_read)
            {
                Ok(true) => result.read_updated += 1,
                Ok(false) => {}
                Err(e) => warn!("Failed to apply the read flag for {mid}: {e:#}"),
            }
        }

        crate::ingest::record_mailbox_cursor(
            &store,
            account_name,
            &target.role,
            &crate::ingest::MailboxCursor {
                uidvalidity: None,
                last_uid: None,
                uidnext: None,
                exists: Some(server_flags.len() as i64),
                highest_modseq: None,
                deltalink: None,
            },
        )?;
    }

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
        Err(anyhow!(
            "Message {} not found on server",
            internet_message_id
        ))
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
            is_read: false,
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
}
