//! The server side of a mutation, at library layer.
//!
//! A flag, a move, an archive or a delete is one local store write and one
//! remote op. The local half lives in [`crate::store::write`]; this module owns
//! the remote half: [`ServerOp`] names what the server has to do, and
//! [`run_ops`] / [`run_op`] execute it against IMAP or Graph.
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

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{GraphConfig, ImapConfig};

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
        }
    }

    /// The message this op names.
    pub fn message_id(&self) -> &str {
        match self {
            ServerOp::Move { message_id, .. }
            | ServerOp::Delete { message_id, .. }
            | ServerOp::SetRead { message_id, .. }
            | ServerOp::SetFlagged { message_id, .. } => message_id,
        }
    }
}

/// The backend a batch of ops runs against, resolved before the spawn so a
/// missing config never leaves a half-applied mutation behind.
pub enum Backend {
    Imap(Box<ImapConfig>),
    Graph(Box<GraphConfig>),
}

/// Run one op, index-aligned results for a whole batch.
///
/// IMAP batches a homogeneous list over a single connection, which is what the
/// pre-store build did for archive and delete; Graph has no session to share,
/// so it runs them in order.
pub async fn run_ops(backend: &Backend, ops: &[ServerOp]) -> Vec<Result<()>> {
    match backend {
        Backend::Graph(config) => {
            let mut out = Vec::with_capacity(ops.len());
            for op in ops {
                out.push(run_graph(config, op).await);
            }
            out
        }
        Backend::Imap(config) => match homogeneous(ops) {
            Some(Homogeneous::Move { source, dest }) if ops.len() > 1 => {
                let ids: Vec<String> = ops.iter().map(message_id_of).collect();
                crate::imap_client::batch_move_on_server(config, &ids, source, dest).await
            }
            Some(Homogeneous::Delete { source }) if ops.len() > 1 => {
                let ids: Vec<String> = ops.iter().map(message_id_of).collect();
                crate::imap_client::batch_delete_on_server(config, &ids, source).await
            }
            _ => {
                let mut out = Vec::with_capacity(ops.len());
                for op in ops {
                    out.push(run_imap(config, op).await);
                }
                out
            }
        },
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
    }
}

/// A batch that can share one IMAP session: same kind, same folders.
pub(crate) enum Homogeneous<'a> {
    Move { source: &'a str, dest: &'a str },
    Delete { source: &'a str },
}

pub(crate) fn homogeneous(ops: &[ServerOp]) -> Option<Homogeneous<'_>> {
    let first = ops.first()?;
    match first {
        ServerOp::Move {
            source_mailbox,
            dest_mailbox,
            ..
        } => ops
            .iter()
            .all(|op| {
                matches!(op, ServerOp::Move { source_mailbox: s, dest_mailbox: d, .. }
                    if s == source_mailbox && d == dest_mailbox)
            })
            .then_some(Homogeneous::Move {
                source: source_mailbox,
                dest: dest_mailbox,
            }),
        ServerOp::Delete { source_mailbox, .. } => ops
            .iter()
            .all(|op| matches!(op, ServerOp::Delete { source_mailbox: s, .. } if s == source_mailbox))
            .then_some(Homogeneous::Delete {
                source: source_mailbox,
            }),
        ServerOp::SetRead { .. } | ServerOp::SetFlagged { .. } => None,
    }
}

pub(crate) fn message_id_of(op: &ServerOp) -> String {
    op.message_id().to_string()
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

    /// A uniform batch of moves is runnable over one IMAP connection: same
    /// source, same destination.
    #[test]
    fn a_uniform_move_batch_is_homogeneous() {
        let ops = vec![
            mv("<1@x>", "INBOX", "Archive"),
            mv("<2@x>", "INBOX", "Archive"),
        ];
        assert!(matches!(
            homogeneous(&ops),
            Some(Homogeneous::Move { source: "INBOX", dest: "Archive" })
        ));
    }

    /// Two moves out of different mailboxes cannot share one SELECT.
    #[test]
    fn a_mixed_move_batch_is_not_homogeneous() {
        let ops = vec![
            mv("<1@x>", "INBOX", "Archive"),
            mv("<2@x>", "Archive", "INBOX"),
        ];
        assert!(homogeneous(&ops).is_none());
    }

    /// A uniform delete batch shares a connection; a read toggle never does.
    #[test]
    fn delete_batches_but_flags_do_not() {
        let dels = vec![del("<1@x>", "INBOX"), del("<2@x>", "INBOX")];
        assert!(matches!(
            homogeneous(&dels),
            Some(Homogeneous::Delete { source: "INBOX" })
        ));
        let flags = vec![ServerOp::SetRead {
            message_id: "<1@x>".to_string(),
            mailbox: "INBOX".to_string(),
            read: true,
        }];
        assert!(homogeneous(&flags).is_none());
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
        assert_eq!(mv("<abc@x>", "INBOX", "Archive").message_id(), "<abc@x>");
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
        ] {
            let json = serde_json::to_string(&op).unwrap();
            assert_eq!(serde_json::from_str::<ServerOp>(&json).unwrap(), op);
        }
    }
}
