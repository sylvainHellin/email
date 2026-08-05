//! The outbox's IMAP adapter: the Sent mailbox as [`SentMailbox`] sees it.
//!
//! Two operations, both driven by [`crate::outbox::drain`]: the Message-ID
//! dedup search that makes a retry safe, and the APPEND itself.
//!
//! One session is opened on first use and reused for the whole drain, so a
//! backlog of three pending rows costs one login rather than three.
//!
//! ## APPENDUID
//!
//! `APPENDUID` is the definitive acknowledgement, and `async-imap` 0.11 does
//! not hand it over: [`async_imap::Session::append`] returns `Result<()>` and
//! its tagged-`OK` response code (where `APPENDUID` lives) is dropped inside
//! `check_done_ok`, with the stream private so it cannot be re-implemented from
//! outside the crate. What survives is just as definitive for the state
//! machine, because `async-imap` turns any non-`OK` tagged response into an
//! error: a successful return *is* the server's acknowledgement. The UID is
//! then recovered with the same `UID SEARCH HEADER MESSAGE-ID` the dedup path
//! runs, and stored on the row. Recovering the real `APPENDUID` needs a patched
//! or vendored `async-imap`; the seam is here when that happens.

use anyhow::{anyhow, Result};
use log::{info, warn};

use super::{open_imap_session, ImapSession};
use crate::config::ImapConfig;
use crate::outbox::SentMailbox;
use crate::timing::TimingSpan;

/// A live IMAP Sent mailbox for one account.
pub struct ImapSentMailbox {
    config: ImapConfig,
    session: Option<ImapSession>,
}

impl ImapSentMailbox {
    pub fn new(config: ImapConfig) -> Self {
        Self {
            config,
            session: None,
        }
    }

    /// The shared session, opened on first use.
    async fn session(&mut self) -> Result<&mut ImapSession> {
        if self.session.is_none() {
            self.session = Some(open_imap_session(&self.config).await?);
        }
        Ok(self.session.as_mut().expect("session just opened"))
    }

    /// Log out if a session was ever opened. Errors are ignored: the server
    /// closes an abandoned connection by itself.
    pub async fn close(mut self) {
        if let Some(mut session) = self.session.take() {
            session.logout().await.ok();
        }
    }

    /// Drop the session so the next call reconnects. Used after a failure,
    /// where the connection may be half-dead.
    fn invalidate(&mut self) {
        self.session = None;
    }
}

impl SentMailbox for ImapSentMailbox {
    async fn search_message_id(&mut self, mailbox: &str, message_id: &str) -> Result<Vec<u32>> {
        let mut span = TimingSpan::with_context("outbox_sent_search", mailbox.to_string());
        let result = async {
            let session = self.session().await?;
            session
                .select(mailbox)
                .await
                .map_err(|e| anyhow!("Failed to select {mailbox}: {e}"))?;
            let query = format!("HEADER Message-ID \"{}\"", message_id);
            let uids = session
                .uid_search(&query)
                .await
                .map_err(|e| anyhow!("UID SEARCH in {mailbox} failed: {e}"))?;
            let mut uids: Vec<u32> = uids.into_iter().collect();
            uids.sort_unstable();
            Ok(uids)
        }
        .await;
        if result.is_err() {
            self.invalidate();
        }
        span.mark("searched");
        result
    }

    async fn append(&mut self, mailbox: &str, raw: &[u8]) -> Result<Option<u32>> {
        let mut span = TimingSpan::with_context("outbox_append", mailbox.to_string());
        let message_id = crate::send::message_id_of(raw);
        let appended = async {
            let session = self.session().await?;
            session
                .append(mailbox, Some("(\\Seen)"), None, raw)
                .await
                .map_err(|e| anyhow!("Failed to APPEND to '{mailbox}': {e}"))
        }
        .await;
        if let Err(e) = appended {
            self.invalidate();
            return Err(e);
        }
        span.mark("appended");
        info!("[outbox] appended {message_id} to '{mailbox}'");

        // The tagged OK above is the acknowledgement; this only recovers the
        // UID that APPENDUID would have carried (see the module docs).
        match self.search_message_id(mailbox, &message_id).await {
            Ok(uids) => Ok(uids.last().copied()),
            Err(e) => {
                warn!("[outbox] appended to '{mailbox}' but could not read back the UID: {e:#}");
                Ok(None)
            }
        }
    }
}
