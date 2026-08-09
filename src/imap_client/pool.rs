//! Persistent authenticated IMAP sessions, shared for the life of the process
//! (#0041).
//!
//! Until this module every IMAP operation paid a full TCP handshake, a TLS
//! handshake and a LOGIN: archiving three messages meant three logins, and a
//! sync of three mailboxes meant three more. The server does not care, but the
//! user waits for all of it, and on a phone-tethered link the login alone was
//! most of the `[TIMING]` line.
//!
//! The replacement is a checkout pool rather than a single session, because
//! IMAP allows exactly one SELECTed mailbox per connection: the parallel
//! per-mailbox fetch (#0005) genuinely needs N connections at once, and one
//! shared session would serialise it. What the pool changes is that those
//! connections *survive* the operation that opened them, so the next one
//! borrows an authenticated session instead of building one.
//!
//! This is the module that rewrites the "one session per operation" invariant
//! in `docs/architecture.md`, with the owner's approval, as the ticket says.
//!
//! # What makes reuse safe
//!
//! A pooled connection is a piece of server-side state that can rot while it
//! sits idle: the server may have dropped it (RFC 3501 permits an autologout
//! after 30 minutes), a NAT may have forgotten the mapping, and a previous
//! borrower may have left it SELECTed on some other mailbox.
//!
//! - Every borrower re-`SELECT`s what it needs. That is not a new rule; every
//!   call site already did it, because it could never assume a mailbox.
//! - A session idle longer than [`IDLE_MAX`] is closed rather than reused.
//! - A session idle longer than [`PROBE_AFTER`] is `NOOP`-probed on checkout;
//!   a probe that fails costs one round trip and yields a fresh login. This is
//!   also the keepalive: the probe is what tells a half-dead connection from a
//!   live one before an operation commits to it.
//! - A borrower whose operation failed [`PooledSession::poison`]s the session,
//!   so a connection abandoned mid-response can never hand its leftover bytes
//!   to the next borrower. Every conversion of a call site does this through
//!   [`PooledSession::check`].
//!
//! # What is deliberately not pooled
//!
//! The IDLE watcher (`super::watch`). IDLE blocks its connection for its whole
//! duration, so it takes a dedicated one, which is the second connection the
//! ticket describes. Logging in and out is the least of its costs.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{debug, info, warn};

use super::{open_imap_session, ImapSession};
use crate::config::ImapConfig;

/// How long a pooled session may sit idle before it is closed instead of
/// reused. RFC 3501 lets a server autologout after 30 minutes of inactivity,
/// so anything approaching that is more likely to be a corpse than a
/// connection, and proving otherwise costs the same round trip as reconnecting.
const IDLE_MAX: Duration = Duration::from_secs(10 * 60);

/// How long a pooled session may sit idle before checkout `NOOP`-probes it.
/// Below this the session was in use moments ago and the probe would be pure
/// latency; above it, one round trip buys the certainty that the next command
/// is not about to fail on a dead socket.
const PROBE_AFTER: Duration = Duration::from_secs(20);

/// How many idle sessions to keep per server. `fetch_concurrency` is clamped to
/// 8, so this is the widest the parallel fetch can ever open; more than that in
/// the pool would be connections nothing is going to borrow.
const MAX_IDLE_PER_KEY: usize = 8;

/// Connect attempts before a checkout gives up, and the pause before each
/// retry. A server that refused once often accepts a second later (Bridge
/// restarting, a laptop's link coming back), and the caller is a sync or a
/// queued op that would otherwise fail the whole pass over a blip.
const CONNECT_BACKOFF: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(1000)];

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// The post-LOGIN capabilities this client acts on, read once per connection.
///
/// Read *post*-LOGIN deliberately: several servers advertise a poorer set
/// before authentication, and Gmail's CONDSTORE only appears afterwards.
///
/// Every field is "the server said so", never "the server probably does". The
/// capability matrix research is explicit that advertise != correct, so each of
/// these is a gate on trying a path, not a promise that it works; the paths
/// themselves all fall back (see `super::fetch`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerCaps {
    /// RFC 7162 CONDSTORE: `SELECT (CONDSTORE)` reports HIGHESTMODSEQ and
    /// `FETCH ... (CHANGEDSINCE n)` returns only what changed since.
    pub condstore: bool,
    /// RFC 7162 QRESYNC. Implies CONDSTORE. Not used yet (#0084).
    pub qresync: bool,
    /// RFC 4315 UIDPLUS: APPENDUID/COPYUID on our own writes. Not used yet
    /// (#0084).
    pub uidplus: bool,
    /// RFC 2177 IDLE, which the watcher already relies on.
    pub idle: bool,
}

impl ServerCaps {
    /// Read the capability set off the advertised atom names.
    ///
    /// Strict, per the ticket: whole-token equality on the name, nothing
    /// inferred from anything else. In particular QRESYNC is *not* taken to
    /// imply CONDSTORE even though RFC 7162 says it does, because a server that
    /// misreports one is not evidence about the other, and CONDSTORE is cheap
    /// to advertise honestly.
    ///
    /// The comparison is ASCII-case-insensitive because IMAP atoms are, and
    /// nothing else: `XCONDSTORE` is not CONDSTORE.
    fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::default();
        for name in names {
            for (flag, want) in [
                (&mut out.condstore, "CONDSTORE"),
                (&mut out.qresync, "QRESYNC"),
                (&mut out.uidplus, "UIDPLUS"),
                (&mut out.idle, "IDLE"),
            ] {
                *flag |= name.eq_ignore_ascii_case(want);
            }
        }
        out
    }

    /// The same, off a live `CAPABILITY` response. `AUTH=` mechanisms are not
    /// extension names and are skipped.
    fn from_response(caps: &async_imap::types::Capabilities) -> Self {
        Self::from_names(caps.iter().filter_map(|cap| match cap {
            async_imap::types::Capability::Atom(name) => Some(name.as_str()),
            _ => None,
        }))
    }

    /// The advertised names, for the log line and the ticket's "probe Proton
    /// Bridge and document it" acceptance criterion.
    pub fn summary(&self) -> String {
        let mut out: Vec<&str> = Vec::new();
        for (on, name) in [
            (self.condstore, "CONDSTORE"),
            (self.qresync, "QRESYNC"),
            (self.uidplus, "UIDPLUS"),
            (self.idle, "IDLE"),
        ] {
            if on {
                out.push(name);
            }
        }
        if out.is_empty() {
            "none of CONDSTORE/QRESYNC/UIDPLUS/IDLE".to_string()
        } else {
            out.join(" ")
        }
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// A session waiting to be borrowed, with what it knows about its server.
struct Idle {
    session: ImapSession,
    caps: ServerCaps,
    since: Instant,
}

type Pool = Mutex<HashMap<String, Vec<Idle>>>;

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What makes two sessions interchangeable: the same server, the same user.
///
/// The credential is deliberately not part of the key. An OAuth2 access token
/// is refreshed behind the client's back, and an IMAP session stays
/// authenticated across that refresh; keying on the token would throw away a
/// perfectly good connection every hour.
fn key_of(config: &ImapConfig) -> String {
    format!("{}:{}/{}", config.host, config.port, config.username)
}

/// A borrowed session, returned to the pool when dropped.
///
/// Deliberately not `Deref<Target = ImapSession>`: the borrow has to go through
/// [`session`](Self::session) so that call sites read as "this is pooled" and
/// the poisoning discipline stays visible at the point where it matters.
pub struct PooledSession {
    key: String,
    caps: ServerCaps,
    /// `None` only after `into_inner`; a borrowed session always holds one.
    session: Option<ImapSession>,
    poisoned: bool,
}

impl PooledSession {
    /// What the server advertised when this connection logged in.
    pub fn caps(&self) -> ServerCaps {
        self.caps
    }

    /// The session itself. Every borrower must `SELECT` before it assumes a
    /// mailbox: the previous borrower left the connection selected on
    /// something else.
    pub fn session(&mut self) -> &mut ImapSession {
        self.session.as_mut().expect("a borrowed session is present")
    }

    /// Do not return this connection to the pool: its state is unknown.
    ///
    /// The case that matters is a command whose response was not read to the
    /// end, which leaves unread bytes in the stream that the next borrower
    /// would decode as the answer to *its* command. Cheap insurance: the cost
    /// of poisoning a healthy session is one reconnect.
    pub fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Poison on error and pass the result through, so a call site can write
    /// `let mbox = pooled.check(op.await)?;` and cannot forget.
    pub fn check<T, E>(&mut self, result: Result<T, E>) -> Result<T, E> {
        if result.is_err() {
            self.poison();
        }
        result
    }

    /// Take the session out of the pool's care for good, for a caller that owns
    /// its connection for a long time (the IDLE watcher's shape).
    pub fn into_inner(mut self) -> ImapSession {
        self.session.take().expect("a borrowed session is present")
    }
}

impl Drop for PooledSession {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else { return };
        if self.poisoned {
            debug!("IMAP pool: dropping a poisoned session for {}", self.key);
            return;
        }
        // A `std::sync::Mutex` and not an async one on purpose: `Drop` cannot
        // await, and the critical section is a `Vec` push.
        let Ok(mut pool) = pool().lock() else { return };
        let idle = pool.entry(self.key.clone()).or_default();
        if idle.len() >= MAX_IDLE_PER_KEY {
            return;
        }
        idle.push(Idle {
            session,
            caps: self.caps,
            since: Instant::now(),
        });
    }
}

/// Borrow an authenticated session for `config`, reusing a pooled one when the
/// pool holds a live one and logging in when it does not.
///
/// The returned session's SELECT state is *not* specified: borrow, SELECT, use,
/// drop.
pub async fn checkout(config: &ImapConfig) -> Result<PooledSession> {
    let key = key_of(config);

    // Take candidates out under the lock and probe them outside it: the probe
    // is a network round trip and the lock is held by `Drop` on other tasks.
    loop {
        let Some(mut idle) = take_idle(&key) else { break };
        if idle.since.elapsed() > IDLE_MAX {
            debug!("IMAP pool: {key} session idle too long, reconnecting");
            continue;
        }
        if idle.since.elapsed() <= PROBE_AFTER {
            return Ok(borrowed(key, idle));
        }
        match idle.session.noop().await {
            Ok(()) => return Ok(borrowed(key, idle)),
            Err(e) => {
                debug!("IMAP pool: {key} session failed its NOOP probe ({e}), reconnecting");
                continue;
            }
        }
    }

    let (session, caps) = connect_with_backoff(config).await?;
    Ok(PooledSession {
        key,
        caps,
        session: Some(session),
        poisoned: false,
    })
}

fn take_idle(key: &str) -> Option<Idle> {
    let mut pool = pool().lock().ok()?;
    pool.get_mut(key)?.pop()
}

fn borrowed(key: String, idle: Idle) -> PooledSession {
    PooledSession {
        key,
        caps: idle.caps,
        session: Some(idle.session),
        poisoned: false,
    }
}

/// Log in, retrying on the transient failures a laptop produces constantly:
/// a link that just came back, a Bridge mid-restart, a server shedding load.
///
/// The last error is what surfaces, because it is the one that describes the
/// state the client is actually in.
async fn connect_with_backoff(config: &ImapConfig) -> Result<(ImapSession, ServerCaps)> {
    let mut attempt = 0usize;
    loop {
        match open_imap_session(config).await {
            Ok(mut session) => {
                // Post-LOGIN CAPABILITY: several servers, Gmail among them,
                // advertise less before authentication.
                let caps = match session.capabilities().await {
                    Ok(caps) => ServerCaps::from_response(&caps),
                    Err(e) => {
                        // Not fatal, and not a reason to poison the session
                        // either: an unknown capability set is the same as an
                        // empty one, which is the fully defensive path.
                        warn!("IMAP: CAPABILITY failed for {}: {e}", config.host);
                        ServerCaps::default()
                    }
                };
                info!(
                    "IMAP: new session to {}:{} advertises {}",
                    config.host,
                    config.port,
                    caps.summary()
                );
                return Ok((session, caps));
            }
            Err(e) => {
                let Some(pause) = CONNECT_BACKOFF.get(attempt).copied() else {
                    return Err(e);
                };
                warn!(
                    "IMAP: connection to {}:{} failed ({e}); retrying in {}ms",
                    config.host,
                    config.port,
                    pause.as_millis()
                );
                async_std::task::sleep(pause).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(port: u16) -> ImapConfig {
        ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "nobody@example.com".into(),
            password: "unused".into(),
            auth_method: crate::config::AuthMethod::Password,
            accept_invalid_certs: false,
            fetch_concurrency: 4,
        }
    }

    /// The reconnect half of the pool, over the one failure that is instant and
    /// deterministic offline: a refused connection.
    ///
    /// What is pinned is that a checkout retries rather than failing on the
    /// first refusal, and pauses between attempts. A laptop's link coming back,
    /// a Bridge mid-restart and a server shedding load all look like this, and
    /// before #0041 each of them failed the whole sync pass.
    #[test]
    fn a_refused_connection_is_retried_with_backoff_before_it_fails() {
        // Port 1 is reserved and nothing listens on it, so `connect` returns
        // ECONNREFUSED immediately and the elapsed time is the backoff itself.
        let started = Instant::now();
        let err = match futures::executor::block_on(checkout(&config(1))) {
            Ok(_) => panic!("nothing is listening on port 1"),
            Err(e) => e,
        };
        let waited = started.elapsed();

        let total: Duration = CONNECT_BACKOFF.iter().sum();
        assert!(
            waited >= total,
            "every backoff pause must have been taken: waited {waited:?}, expected >= {total:?}"
        );
        assert!(
            waited < total + Duration::from_secs(10),
            "and it must give up rather than retry forever: {waited:?}"
        );
        assert!(
            err.to_string().contains("Failed to connect"),
            "the surfaced error is the last real one, not a retry wrapper: {err}"
        );
    }

    /// Two sessions are interchangeable when they are the same user on the same
    /// server, and the credential is not part of that: an OAuth2 token refresh
    /// must not orphan a live connection.
    #[test]
    fn the_pool_key_is_the_server_and_the_user_and_not_the_credential() {
        let base = |user: &str, port: u16, token: &str| ImapConfig {
            host: "imap.example.com".into(),
            port,
            username: user.into(),
            password: token.into(),
            auth_method: crate::config::AuthMethod::Password,
            accept_invalid_certs: false,
            fetch_concurrency: 4,
        };
        assert_eq!(
            key_of(&base("a@x", 993, "first-token")),
            key_of(&base("a@x", 993, "refreshed-token")),
            "an OAuth2 refresh must not orphan a live session"
        );
        assert_ne!(key_of(&base("a@x", 993, "t")), key_of(&base("b@x", 993, "t")));
        assert_ne!(key_of(&base("a@x", 993, "t")), key_of(&base("a@x", 143, "t")));
    }

    /// The gate is an exact token match and nothing is inferred, including the
    /// QRESYNC-implies-CONDSTORE rule RFC 7162 actually states: a server that
    /// misreports one is not evidence for the other.
    #[test]
    fn capabilities_are_read_strictly_off_the_advertised_names() {
        let parse = |line: &str| ServerCaps::from_names(line.split(' '));

        // Dovecot's full ladder.
        let dovecot = parse("IMAP4rev1 CONDSTORE QRESYNC UIDPLUS IDLE MOVE");
        assert_eq!(
            dovecot,
            ServerCaps { condstore: true, qresync: true, uidplus: true, idle: true }
        );
        assert_eq!(dovecot.summary(), "CONDSTORE QRESYNC UIDPLUS IDLE");

        // Gmail: CONDSTORE but never QRESYNC.
        let gmail = parse("IMAP4rev1 UIDPLUS IDLE CONDSTORE ENABLE X-GM-EXT-1");
        assert!(gmail.condstore && gmail.uidplus && gmail.idle);
        assert!(!gmail.qresync, "Gmail has never implemented QRESYNC");

        // Proton Bridge (Gluon): neither, so the heuristic path and nothing
        // else. This is the daily driver, and the assertion that keeps the
        // delta paths from being reachable there by accident.
        let proton = parse("IMAP4rev1 IDLE UNSELECT UIDPLUS MOVE ID");
        assert!(!proton.condstore && !proton.qresync);
        assert!(proton.uidplus && proton.idle);

        // Nothing advertised is the fully defensive answer, and so is a
        // CAPABILITY that could not be read at all.
        let bare = parse("IMAP4rev1");
        assert_eq!(bare, ServerCaps::default());
        assert_eq!(bare.summary(), "none of CONDSTORE/QRESYNC/UIDPLUS/IDLE");

        // No inference: QRESYNC alone does not switch CONDSTORE on, even though
        // RFC 7162 says it implies it.
        assert!(!parse("IMAP4rev1 QRESYNC").condstore);
        // ...and a substring is not a token, while case is not significant,
        // because IMAP atoms are case-insensitive.
        assert!(!parse("IMAP4rev1 XCONDSTORE CONDSTORE-LIKE").condstore);
        assert!(parse("IMAP4rev1 CondStore").condstore);
    }
}
