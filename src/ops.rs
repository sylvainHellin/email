//! The server side of a mutation, at library layer.
//!
//! A flag, a move, an archive or a delete is one local store write and one
//! remote op. The local half lives in [`crate::store::write`]; this module owns
//! the remote half: [`ServerOp`] names what the server has to do, and
//! [`run_op`] executes it against IMAP or Graph.
//!
//! It sits in the library rather than in `tui/` on purpose. The TUI's mutation
//! pairing ([`crate::tui::mutations`]) built these types first, but the durable
//! `pending_ops` queue ([`crate::pending_ops`], #0039) drains the same ops from
//! a background engine that has no terminal, and the CLI issues them too. A
//! remote op is email logic, which the TUI-implements-no-email-logic invariant
//! keeps out of `tui/`; the queue and the TUI both reference it here, and
//! `tui::mutations` re-exports it so the TUI call sites read unchanged.
//!
//! Every op names the message by `Message-ID`, the one handle both backends
//! share and the one that survives the local row moving. The store row already
//! knows the exact `(mailbox, uid)`, and #0039's amendment notes a uid fast
//! path as a later refinement; the durable queue's first cut keeps the
//! Message-ID addressing the outbox's Sent dedup already relies on, because it
//! is the proven seam and needs no widening of the `imap_client` op signatures.

use std::error::Error as StdError;
use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{AccountConfig, AuthMethod, GraphConfig, ImapConfig};

/// A backend op found no message to act on: the server's copy is already gone.
///
/// A *typed* not-found signal, so the durable queue's drain
/// ([`crate::pending_ops`], #0039 review) can tell "the target is already in
/// the desired state" from a real transport failure without matching on error
/// strings. A move or delete that succeeded on the server, then crashed before
/// its queue row retired, replays against a message the source folder no longer
/// holds; the backend reports that as this error and the drain treats it as a
/// converged op rather than parking a success as `failed`.
///
/// Direct CLI and TUI call sites keep seeing it as an ordinary error through
/// [`std::fmt::Display`], so a user acting on a message the server no longer
/// holds still gets the same message they always did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFoundOnServer {
    /// The `Message-ID` the op named.
    pub message_id: String,
    /// The source folder searched, when the backend knows it (IMAP). Graph
    /// resolves by id across the mailbox, so it has no folder to name.
    pub mailbox: Option<String>,
}

impl fmt::Display for NotFoundOnServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.mailbox {
            Some(mailbox) => write!(
                f,
                "Email with Message-ID {} not found in {} on server",
                self.message_id, mailbox
            ),
            None => write!(f, "Message {} not found on server", self.message_id),
        }
    }
}

impl StdError for NotFoundOnServer {}

impl NotFoundOnServer {
    /// True when `err` is, or wraps, a not-found signal from either backend.
    /// The drain uses this to converge a replay instead of failing it.
    pub fn is_in(err: &anyhow::Error) -> bool {
        err.downcast_ref::<NotFoundOnServer>().is_some()
    }
}

/// What the background thread (or the queue drain) will ask the server to do.
///
/// Serializable so the durable [`crate::pending_ops`] queue can persist it in a
/// row's payload and replay it after a crash (#0039).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerOp {
    /// Move between server folders. Archiving is this with the archive folder
    /// as destination, exactly as `move_email_on_server` has always framed it.
    Move {
        message_id: String,
        source_mailbox: String,
        dest_mailbox: String,
    },
    Delete {
        message_id: String,
        source_mailbox: String,
    },
    SetRead {
        message_id: String,
        mailbox: String,
        read: bool,
    },
    /// Toggle the `\Flagged` star (#0007). Like [`ServerOp::SetRead`], the
    /// server is truth on the IMAP path, so a local toggle needs a UID STORE
    /// write the next sync restates.
    SetFlagged {
        message_id: String,
        mailbox: String,
        flagged: bool,
    },
    /// The post-send bookkeeping bit on the source of a reply or a forward:
    /// `\Answered` when `answered`, `$Forwarded` otherwise (#TKT-0051, #0076).
    ///
    /// The one multi-mailbox op, because a source is one store row per mailbox
    /// (inbox, archive, sent) and the same server message in each: naming the
    /// whole list lets the drain write them over a single IMAP session instead
    /// of one login per mailbox, which is the cost #0076 exists to remove.
    ///
    /// Idempotent by construction (`+FLAGS` on a flag that may already be set)
    /// and not-found tolerant per mailbox, so a replay after a crash converges
    /// rather than failing.
    SetAnswered {
        message_id: String,
        mailboxes: Vec<String>,
        answered: bool,
    },
}

impl ServerOp {
    /// The `pending_ops.kind` string for this op (#0039). The five kinds the
    /// ticket names map onto these four variants: archive is a `move` with the
    /// archive folder as destination, and mark-read / mark-unread are one
    /// `SetRead` with the `read` bit.
    pub fn kind(&self) -> &'static str {
        match self {
            ServerOp::Move { .. } => "move",
            ServerOp::Delete { .. } => "delete",
            ServerOp::SetRead { .. } => "set_read",
            ServerOp::SetFlagged { .. } => "set_flagged",
            ServerOp::SetAnswered { .. } => "set_answered",
        }
    }

    /// The message this op names.
    pub fn message_id(&self) -> &str {
        match self {
            ServerOp::Move { message_id, .. }
            | ServerOp::Delete { message_id, .. }
            | ServerOp::SetRead { message_id, .. }
            | ServerOp::SetFlagged { message_id, .. }
            | ServerOp::SetAnswered { message_id, .. } => message_id,
        }
    }
}

/// The backend a batch of ops runs against, resolved before the spawn so a
/// missing config never leaves a half-applied mutation behind.
pub enum Backend {
    Imap(Box<ImapConfig>),
    Graph(Box<GraphConfig>),
}

impl Backend {
    /// The backend an account's owed ops run against, loaded from its config.
    ///
    /// The account's `auth_method` decides the transport, not whichever config
    /// happens to be present: a Graph account drains over Graph or not at all.
    /// This is the resolver the durable queue's background drain
    /// ([`crate::pending_ops::resume_account`]) uses, the config-side twin of
    /// the TUI's `backend_for_mutation`, so a drain from `mp sync` or a
    /// terminal-less engine builds the same backend the interactive path does.
    pub fn resolve(account: &AccountConfig) -> Result<Backend> {
        if account.auth_method == AuthMethod::Graph {
            Ok(Backend::Graph(Box::new(GraphConfig::load(account)?)))
        } else {
            Ok(Backend::Imap(Box::new(ImapConfig::load(account)?)))
        }
    }
}

/// Run a single op against `backend`. The durable queue drains one row at a
/// time, so it uses this rather than the batch form (#0039).
pub async fn run_op(backend: &Backend, op: &ServerOp) -> Result<()> {
    match backend {
        Backend::Graph(config) => run_graph(config, op).await,
        Backend::Imap(config) => run_imap(config, op).await,
    }
}

async fn run_imap(config: &ImapConfig, op: &ServerOp) -> Result<()> {
    match op {
        ServerOp::Move {
            message_id,
            source_mailbox,
            dest_mailbox,
        } => {
            crate::imap_client::move_email_on_server(
                config,
                message_id,
                source_mailbox,
                dest_mailbox,
            )
            .await
        }
        ServerOp::Delete {
            message_id,
            source_mailbox,
        } => crate::imap_client::delete_email_on_server(config, message_id, source_mailbox).await,
        ServerOp::SetRead {
            message_id,
            mailbox,
            read,
        } => {
            if *read {
                crate::imap_client::mark_read_on_server(config, message_id, mailbox).await
            } else {
                crate::imap_client::mark_unread_on_server(config, message_id, mailbox).await
            }
        }
        ServerOp::SetFlagged {
            message_id,
            mailbox,
            flagged,
        } => {
            let flag = crate::types::FLAG_FLAGGED;
            if *flagged {
                crate::imap_client::add_flag_on_server(config, message_id, mailbox, flag).await
            } else {
                crate::imap_client::remove_flag_on_server(config, message_id, mailbox, flag).await
            }
        }
        ServerOp::SetAnswered {
            message_id,
            mailboxes,
            answered,
        } => {
            let flag = if *answered {
                crate::types::FLAG_ANSWERED
            } else {
                crate::types::FLAG_FORWARDED
            };
            crate::imap_client::add_flag_in_mailboxes(config, message_id, mailboxes, flag).await
        }
    }
}

async fn run_graph(config: &GraphConfig, op: &ServerOp) -> Result<()> {
    match op {
        ServerOp::Move {
            message_id,
            dest_mailbox,
            ..
        } => crate::graph::move_message_graph(config, message_id, dest_mailbox).await,
        ServerOp::Delete { message_id, .. } => {
            crate::graph::delete_message_graph(config, message_id).await
        }
        ServerOp::SetRead {
            message_id, read, ..
        } => crate::graph::mark_read_graph(config, message_id, *read).await,
        // Graph stays seen-only (#0007): the star is parked locally, honoured
        // by the store and the list, but never mirrored to Graph. A no-op Ok
        // rather than an error, so the optimistic local write stands.
        ServerOp::SetFlagged { message_id, .. } => {
            log::debug!("[graph] flag parked locally for {message_id}; Graph is seen-only");
            Ok(())
        }
        // Graph exposes the answered state only through extended MAPI
        // properties and the backend is parked (#0042, #0055), so the local
        // write stands alone; `ingest::apply_seen_flags` is what keeps a Graph
        // sync from erasing it. `Ok`, not an error: this op must never fail a
        // send's bookkeeping (#0076). The queue-side guard is that the send
        // path enqueues nothing on a Graph account at all.
        ServerOp::SetAnswered { message_id, .. } => {
            log::debug!("[graph] answered/forwarded parked locally for {message_id}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(id: &str, source: &str, dest: &str) -> ServerOp {
        ServerOp::Move {
            message_id: id.to_string(),
            source_mailbox: source.to_string(),
            dest_mailbox: dest.to_string(),
        }
    }

    fn del(id: &str, source: &str) -> ServerOp {
        ServerOp::Delete {
            message_id: id.to_string(),
            source_mailbox: source.to_string(),
        }
    }

    /// The kind strings are the `pending_ops.kind` column values (#0039), and
    /// every op names its message.
    #[test]
    fn kind_and_message_id_are_stable() {
        assert_eq!(mv("<1@x>", "INBOX", "Archive").kind(), "move");
        assert_eq!(del("<1@x>", "INBOX").kind(), "delete");
        assert_eq!(
            ServerOp::SetRead {
                message_id: "<1@x>".to_string(),
                mailbox: "INBOX".to_string(),
                read: false,
            }
            .kind(),
            "set_read"
        );
        assert_eq!(
            ServerOp::SetAnswered {
                message_id: "<1@x>".to_string(),
                mailboxes: vec!["INBOX".to_string()],
                answered: true,
            }
            .kind(),
            "set_answered"
        );
        assert_eq!(mv("<abc@x>", "INBOX", "Archive").message_id(), "<abc@x>");
        assert_eq!(
            ServerOp::SetAnswered {
                message_id: "<abc@x>".to_string(),
                mailboxes: Vec::new(),
                answered: false,
            }
            .message_id(),
            "<abc@x>"
        );
    }

    /// The op round-trips through JSON, which is what the durable queue stores
    /// in the row payload and replays after a crash (#0039).
    #[test]
    fn a_server_op_round_trips_through_json() {
        for op in [
            mv("<1@x>", "INBOX", "Archive"),
            del("<2@x>", "Archive"),
            ServerOp::SetRead {
                message_id: "<3@x>".to_string(),
                mailbox: "INBOX".to_string(),
                read: true,
            },
            ServerOp::SetFlagged {
                message_id: "<4@x>".to_string(),
                mailbox: "INBOX".to_string(),
                flagged: false,
            },
            ServerOp::SetAnswered {
                message_id: "<5@x>".to_string(),
                mailboxes: vec!["INBOX".to_string(), "Archive".to_string()],
                answered: true,
            },
        ] {
            let json = serde_json::to_string(&op).unwrap();
            assert_eq!(serde_json::from_str::<ServerOp>(&json).unwrap(), op);
        }
    }
}
