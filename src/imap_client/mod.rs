mod batch;
mod fetch;
mod ops;
pub mod search;
mod sent;
mod store_sync;
mod watch;

pub use batch::{batch_delete_on_server, batch_move_on_server};
pub use fetch::{
    fetch_emails, fetch_emails_on_session, fetch_new_raw_on_session, vanished_uids, FetchedRaw,
    MailboxState,
};
pub use ops::{
    add_flag_on_server, delete_email_on_server, mark_read_on_server, mark_unread_on_server,
    move_email_on_server, remove_flag_on_server,
};
pub use sent::ImapSentMailbox;
pub use search::{
    bracketed_message_id, normalize_message_id, parse_search_query, retain_exact_message_id,
    FetchCriteria,
};
pub use store_sync::{
    list_mailboxes, sync_mailboxes, FreshObservation, SyncResult, SyncTarget,
};
pub use watch::watch_mailbox;

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use anyhow::anyhow;
use futures::io::{AsyncRead, AsyncWrite};
use log::info;

use crate::config::{AuthMethod, ImapConfig};

pub type ImapSession = async_imap::Session<ImapStream>;

// ---------------------------------------------------------------------------
// Stream wrapper for STARTTLS support
// ---------------------------------------------------------------------------

/// A wrapper around a TLS stream that optionally injects a fake IMAP greeting.
pub struct ImapStream {
    prefix: Vec<u8>,
    prefix_pos: usize,
    inner: async_native_tls::TlsStream<async_std::net::TcpStream>,
}

impl ImapStream {
    fn passthrough(inner: async_native_tls::TlsStream<async_std::net::TcpStream>) -> Self {
        Self {
            prefix: Vec::new(),
            prefix_pos: 0,
            inner,
        }
    }

    fn with_greeting(inner: async_native_tls::TlsStream<async_std::net::TcpStream>) -> Self {
        Self {
            prefix: b"* OK STARTTLS ready\r\n".to_vec(),
            prefix_pos: 0,
            inner,
        }
    }
}

impl AsyncRead for ImapStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.prefix_pos < self.prefix.len() {
            let remaining = &self.prefix[self.prefix_pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.prefix_pos += n;
            return Poll::Ready(Ok(n));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ImapStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

impl Unpin for ImapStream {}

impl fmt::Debug for ImapStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapStream")
            .field("prefix_remaining", &(self.prefix.len() - self.prefix_pos))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// XOAUTH2 Authenticator for async_imap
// ---------------------------------------------------------------------------

/// XOAUTH2 SASL authenticator for IMAP.
/// Returns the raw XOAUTH2 string; async_imap handles base64 encoding.
struct XOAuth2Authenticator {
    user: String,
    access_token: String,
}

impl async_imap::Authenticator for &XOAuth2Authenticator {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        format!(
            "user={}\x01auth=Bearer {}\x01\x01",
            self.user, self.access_token
        )
    }
}

// ---------------------------------------------------------------------------
// Session opening (password or OAuth2)
// ---------------------------------------------------------------------------

/// Authenticate an IMAP client, branching on auth method.
async fn authenticate_client(
    client: async_imap::Client<ImapStream>,
    imap_config: &ImapConfig,
) -> anyhow::Result<ImapSession> {
    match imap_config.auth_method {
        AuthMethod::OAuth2 => {
            info!("IMAP: authenticating via XOAUTH2 for {}", imap_config.username);
            let auth = XOAuth2Authenticator {
                user: imap_config.username.clone(),
                access_token: imap_config.password.clone(), // password field holds the access token for OAuth2
            };
            let session = client
                .authenticate("XOAUTH2", &auth)
                .await
                .map_err(|e| anyhow!("IMAP XOAUTH2 authentication failed: {}", e.0))?;
            Ok(session)
        }
        AuthMethod::Password => {
            let session = client
                .login(&imap_config.username, &imap_config.password)
                .await
                .map_err(|e| anyhow!("IMAP login failed: {}", e.0))?;
            Ok(session)
        }
        AuthMethod::Graph => {
            Err(anyhow!("Graph accounts use Microsoft Graph API, not IMAP"))
        }
    }
}

/// How long the TCP connect of a new IMAP session may take before it is given
/// up on.
///
/// Nothing else here is bounded, and nothing else needs to be: a server that
/// answered the SYN and then went quiet fails its read, while a black-holed
/// host (a dropped route, a firewall that swallows the SYN) leaves the connect
/// itself hanging for the OS default, over two minutes on Linux. Every caller
/// of a session sits on some queue that has to wait for it: a sync thread, the
/// IDLE watcher, or the post-send flag write, which runs after a message has
/// already gone out (#0076).
///
/// Generous on purpose: this is a black-hole guard, not a latency budget, and
/// a slow link on a good day must not lose its connection to it.
const CONNECT_TIMEOUT_SECS: u64 = 30;

pub async fn open_imap_session(imap_config: &ImapConfig) -> anyhow::Result<ImapSession> {
    use crate::timing::TimingSpan;
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let addr = format!("{}:{}", imap_config.host, imap_config.port);
    let mut span = TimingSpan::with_context("open_imap_session", addr.clone());

    if imap_config.accept_invalid_certs {
        crate::config::ensure_invalid_certs_allowed(&imap_config.host)?;
    }

    let tls = async_native_tls::TlsConnector::new()
        .danger_accept_invalid_certs(imap_config.accept_invalid_certs);

    let mut tcp_stream = async_std::future::timeout(
        std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
        async_std::net::TcpStream::connect(&addr),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "Failed to connect to IMAP server: {} did not answer within {}s",
            addr,
            CONNECT_TIMEOUT_SECS
        )
    })?
    .map_err(|e| anyhow!("Failed to connect to IMAP server: {}", e))?;
    span.mark("tcp_connect");

    if imap_config.port == 993 {
        let tls_stream = tls
            .connect(&imap_config.host, tcp_stream)
            .await
            .map_err(|e| anyhow!("TLS handshake failed: {}", e))?;
        span.mark("tls_handshake");

        let client = async_imap::Client::new(ImapStream::passthrough(tls_stream));
        let session = authenticate_client(client, imap_config).await?;
        span.mark("login");
        Ok(session)
    } else {
        info!("IMAP STARTTLS: connecting to {} (plaintext first)", addr);

        let mut buf = [0u8; 4096];
        let n = tcp_stream
            .read(&mut buf)
            .await
            .map_err(|e| anyhow!("Failed to read IMAP greeting: {}", e))?;
        let greeting = String::from_utf8_lossy(&buf[..n]);
        if !greeting.starts_with("* ") {
            return Err(anyhow!("Unexpected IMAP greeting: {}", greeting.trim()));
        }
        span.mark("greeting");
        info!("IMAP STARTTLS: got greeting, sending STARTTLS");

        tcp_stream
            .write_all(b"a0 STARTTLS\r\n")
            .await
            .map_err(|e| anyhow!("Failed to send STARTTLS: {}", e))?;
        tcp_stream
            .flush()
            .await
            .map_err(|e| anyhow!("Failed to flush STARTTLS: {}", e))?;

        let n = tcp_stream
            .read(&mut buf)
            .await
            .map_err(|e| anyhow!("Failed to read STARTTLS response: {}", e))?;
        let response = String::from_utf8_lossy(&buf[..n]);
        if !response.contains("OK") {
            return Err(anyhow!("STARTTLS rejected by server: {}", response.trim()));
        }
        span.mark("starttls_negotiate");
        info!("IMAP STARTTLS: upgrading to TLS");

        let tls_stream = tls
            .connect(&imap_config.host, tcp_stream)
            .await
            .map_err(|e| anyhow!("STARTTLS TLS handshake failed: {}", e))?;
        span.mark("tls_handshake");

        let client = async_imap::Client::new(ImapStream::with_greeting(tls_stream));
        let session = authenticate_client(client, imap_config).await?;
        span.mark("login");
        Ok(session)
    }
}
