//! The `mp://` selector contract: one grammar, one parser, one formatter.
//!
//! Paths are gone from every CLI input position (#0050). A message is named by
//!
//! ```text
//! selector := [ "mp://" account "/" ] [ mailbox "/" ] key
//! ```
//!
//! and the canonical form every command *prints* is the fully qualified
//! `mp://<account>/<mailbox>/<key>`. Elision is positional and deterministic,
//! never sniffed: without the scheme the account comes from `-A/--account` or
//! the default account, and the mailbox comes from `--mailbox` or the
//! command's declared default scope. Segment counts alone decide what was
//! elided, which works because the key is percent-encoded and therefore
//! contains no raw `/`.
//!
//! The parser never inspects the string to decide *what kind of thing* it is.
//! The [`Namespace`] is fixed by the command that called it: `mp archive` is
//! always received mail, `mp send` is always a draft. That is the whole reason
//! the two namespaces can share one grammar without a sniffing rule that would
//! silently reinterpret a key the day a Message-ID happens to look like a
//! draft id.
//!
//! Resolution ([`resolve_received`], [`resolve_draft`]) is a single indexed
//! lookup. Zero matches names the namespace searched; more than one match (the
//! cross-mailbox copy) lists every fully qualified candidate and asks for
//! `--mailbox`, and never picks one.

use std::fmt;

use anyhow::{anyhow, bail, Result};

use crate::store::drafts::{self, DraftRow};
use crate::store::read::{self, MessageRow};
use crate::store::Store;

/// The reserved mailbox segment of every draft selector. Drafts are local-only
/// files, so this is not a server folder name and can never collide with one:
/// a real IMAP `Drafts` folder syncs into the store as a `messages` row like
/// any other mailbox, in the received namespace.
pub const DRAFTS_MAILBOX: &str = "drafts";

/// The scheme prefix of a fully qualified selector.
pub const SCHEME: &str = "mp://";

/// Which key namespace a command addresses. Fixed by the command, never by the
/// shape of the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// Received mail, resolved in `messages`. The key is the RFC 5322
    /// Message-ID without angle brackets, made total by the
    /// `sha256-<hex16>@local.invalid` synthesis rule ingest applies when the
    /// header is missing.
    Received,
    /// Local drafts, resolved in the `drafts` index. The mailbox segment is
    /// the reserved [`DRAFTS_MAILBOX`] and the key is the draft id.
    Drafts,
}

impl Namespace {
    /// The name used when reporting that nothing matched.
    pub fn label(self) -> &'static str {
        match self {
            Namespace::Received => "received mail",
            Namespace::Drafts => "drafts",
        }
    }
}

/// A canonical, fully qualified selector. This is what every command prints,
/// and it round-trips through [`parse`] unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub account: String,
    pub mailbox: String,
    pub key: String,
}

impl Selector {
    pub fn new(account: impl Into<String>, mailbox: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            mailbox: mailbox.into(),
            key: key.into(),
        }
    }

    /// The canonical selector of a received message row.
    ///
    /// The key is the Message-ID *without* its angle brackets (scope item 2):
    /// the brackets are RFC 5322 delimiters, not part of the identifier, and
    /// carrying them would make every printed selector end in `%3E` and every
    /// hand-typed one need shell quoting.
    pub fn for_message(account: &str, row: &MessageRow) -> Self {
        Self::new(account, &row.mailbox, message_key(&row.message_id))
    }

    /// The canonical selector of a draft.
    pub fn for_draft(account: &str, id: &str) -> Self {
        Self::new(account, DRAFTS_MAILBOX, id)
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{SCHEME}{}/{}/{}",
            encode(&self.account),
            encode(&self.mailbox),
            encode(&self.key)
        )
    }
}

/// What one selector string said, before any default was applied. `None` means
/// the segment was elided, which is a different statement from naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorParts {
    pub account: Option<String>,
    pub mailbox: Option<String>,
    pub key: String,
}

/// A parsed selector with its defaults applied: the question the resolver
/// asks. `mailbox` stays optional for received mail, where an unqualified
/// selector deliberately searches every mailbox and reports the ambiguity
/// rather than picking one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorQuery {
    pub namespace: Namespace,
    pub account: String,
    pub mailbox: Option<String>,
    pub key: String,
}

/// Parse a selector into its segments, applying no defaults.
///
/// The grammar is positional. With the scheme, two segments are
/// `account/key` and three are `account/mailbox/key`; without it, one segment
/// is `key` and two are `mailbox/key`. Anything longer is rejected rather than
/// re-split, because a key that needed a `/` should have been encoded.
pub fn parse(input: &str) -> Result<SelectorParts> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty selector: expected {SCHEME}<account>/<mailbox>/<key>");
    }
    if looks_like_path(trimmed) {
        bail!(
            "{trimmed} looks like a filesystem path; commands take a selector \
             ({SCHEME}<account>/<mailbox>/<key>), and `mp path <selector>` is the only \
             way back to a path"
        );
    }

    let (qualified, rest) = match trimmed.strip_prefix(SCHEME) {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.iter().any(|s| s.is_empty()) {
        bail!("selector {trimmed} has an empty segment");
    }

    let parts = match (qualified, segments.len()) {
        (true, 2) => SelectorParts {
            account: Some(decode(segments[0])?),
            mailbox: None,
            key: decode(segments[1])?,
        },
        (true, 3) => SelectorParts {
            account: Some(decode(segments[0])?),
            mailbox: Some(decode(segments[1])?),
            key: decode(segments[2])?,
        },
        (false, 1) => SelectorParts {
            account: None,
            mailbox: None,
            key: decode(segments[0])?,
        },
        (false, 2) => SelectorParts {
            account: None,
            mailbox: Some(decode(segments[0])?),
            key: decode(segments[1])?,
        },
        _ => bail!(
            "selector {trimmed} has {} segments: expected {SCHEME}<account>/<mailbox>/<key> \
             or one of its elided forms (<mailbox>/<key>, <key>). A `/` inside a key must be \
             percent-encoded as %2F",
            segments.len()
        ),
    };
    Ok(parts)
}

/// Parse `input` in a fixed namespace and apply the command's defaults.
///
/// `account_default` is `-A/--account` or the first configured account;
/// `mailbox_flag` is the command's `--mailbox`, which the selector's own
/// mailbox segment overrides (naming it in the selector is more specific than
/// naming it beside it). In the drafts namespace the mailbox is always the
/// reserved [`DRAFTS_MAILBOX`], and any other value is rejected instead of
/// being quietly ignored.
pub fn parse_in(
    input: &str,
    namespace: Namespace,
    account_default: &str,
    mailbox_flag: Option<&str>,
) -> Result<SelectorQuery> {
    let parts = parse(input)?;
    let account = parts
        .account
        .or_else(|| non_empty(account_default))
        .ok_or_else(|| {
            anyhow!("no account for {input}: name one with -A/--account or configure a default")
        })?;

    let named_mailbox = parts.mailbox.or_else(|| mailbox_flag.map(str::to_string));
    let mailbox = match namespace {
        Namespace::Drafts => {
            if let Some(mb) = named_mailbox.as_deref() {
                if mb != DRAFTS_MAILBOX {
                    bail!(
                        "{input} names the mailbox {mb}, but a draft selector's mailbox segment \
                         is the reserved name `{DRAFTS_MAILBOX}`"
                    );
                }
            }
            Some(DRAFTS_MAILBOX.to_string())
        }
        Namespace::Received => {
            if named_mailbox.as_deref() == Some(DRAFTS_MAILBOX) {
                bail!(
                    "{input} names the reserved mailbox `{DRAFTS_MAILBOX}`, which holds local \
                     drafts; this command addresses received mail"
                );
            }
            named_mailbox
        }
    };

    Ok(SelectorQuery {
        namespace,
        account,
        mailbox,
        key: parts.key,
    })
}

/// Resolve a received-mail query to exactly one row, plus its canonical
/// selector.
///
/// One indexed lookup on `messages_message_id`. Several rows is the normal
/// cross-mailbox copy case, so it is reported with every candidate spelled out
/// in full rather than resolved by a rule the user cannot see.
pub fn resolve_received(store: &Store, query: &SelectorQuery) -> Result<(MessageRow, Selector)> {
    debug_assert_eq!(query.namespace, Namespace::Received);
    // Ingest stores the header verbatim, brackets and all, while the selector
    // key is the bare identifier; so the bracketed form is asked first and the
    // bare one second, which also answers for a row whose stored id has no
    // brackets. Both are the same indexed lookup on `messages_message_id`.
    let key = message_key(&query.key);
    let mut rows = read::find_by_message_id(store, &query.account, &format!("<{key}>"))?;
    if rows.is_empty() {
        rows = read::find_by_message_id(store, &query.account, key)?;
    }
    if let Some(mailbox) = query.mailbox.as_deref() {
        rows.retain(|row| row.mailbox == mailbox);
    }
    match rows.len() {
        0 => Err(not_found(query)),
        1 => {
            let row = rows.remove(0);
            let selector = Selector::for_message(&query.account, &row);
            Ok((row, selector))
        }
        _ => Err(ambiguous(
            query,
            rows.iter()
                .map(|row| Selector::for_message(&query.account, row))
                .collect(),
        )),
    }
}

/// Resolve a draft query to exactly one indexed draft, plus its canonical
/// selector. The drafts table is keyed `(account, id)`, so there is no
/// ambiguous case here: a duplicate id is impossible by the primary key.
pub fn resolve_draft(store: &Store, query: &SelectorQuery) -> Result<(DraftRow, Selector)> {
    debug_assert_eq!(query.namespace, Namespace::Drafts);
    match drafts::find(store, &query.account, &query.key)? {
        Some(row) => {
            let selector = Selector::for_draft(&query.account, &row.id);
            Ok((row, selector))
        }
        None => Err(not_found(query)),
    }
}

/// The zero-match error: it names the namespace searched, so the reader can
/// tell "no such message" from "you asked the wrong index".
fn not_found(query: &SelectorQuery) -> anyhow::Error {
    let scope = match query.mailbox.as_deref() {
        Some(mailbox) => format!("{}/{mailbox}", query.account),
        None => query.account.clone(),
    };
    anyhow!(
        "no match for {} in the {} index of {scope}",
        query.key,
        query.namespace.label()
    )
}

/// The multiple-match error: every candidate fully qualified, and the flag
/// that picks one.
fn ambiguous(query: &SelectorQuery, candidates: Vec<Selector>) -> anyhow::Error {
    let list = candidates
        .iter()
        .map(|s| format!("\n  {s}"))
        .collect::<String>();
    anyhow!(
        "{} matches {} messages in {}; name one with --mailbox:{list}",
        query.key,
        candidates.len(),
        query.account
    )
}

/// The selector key of a Message-ID: the identifier without the RFC 5322
/// angle brackets, whichever form the caller happens to hold.
fn message_key(message_id: &str) -> &str {
    message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// True for strings that are obviously filesystem paths, so the decline names
/// the real mistake instead of "no match for ./drafts/foo.md".
fn looks_like_path(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('~')
        || s.ends_with(".md")
}

// ---------------------------------------------------------------------------
// Percent-encoding
// ---------------------------------------------------------------------------

/// Bytes that survive [`encode`] unescaped: RFC 3986 unreserved plus `@`,
/// which every Message-ID contains and which is unambiguous inside a segment.
/// Everything else, `/` and `%` and whitespace above all, is escaped, which is
/// what makes splitting a selector on `/` total.
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'@')
}

/// Percent-encode one selector segment.
pub fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &b in segment.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decode one selector segment. A truncated or non-hex escape is an
/// error rather than a literal `%`, so a mistyped selector cannot resolve to
/// something the user did not write.
pub fn decode(segment: &str) -> Result<String> {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| anyhow!("selector segment {segment} ends in a truncated % escape"))?;
            let hi = (hex[0] as char)
                .to_digit(16)
                .ok_or_else(|| anyhow!("selector segment {segment} has a non-hex % escape"))?;
            let lo = (hex[1] as char)
                .to_digit(16)
                .ok_or_else(|| anyhow!("selector segment {segment} has a non-hex % escape"))?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| anyhow!("selector segment {segment} decodes to invalid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys chosen to break a naive split-on-slash or a naive encoder: the
    /// characters the grammar promises never appear raw, plus unicode and the
    /// scheme itself as a payload.
    fn hostile_keys() -> Vec<String> {
        vec![
            "plain@example.com".to_string(),
            "has/slash@example.com".to_string(),
            "has%25percent@example.com".to_string(),
            "has percent % and space".to_string(),
            "has\ttab\nnewline".to_string(),
            "üñíçø∂é@例え.jp".to_string(),
            "mp://looks/like/a/selector".to_string(),
            "../../etc/passwd".to_string(),
            "sha256-0123456789abcdef@local.invalid".to_string(),
            "<already-bracketed@example.com>".to_string(),
            "trailing-dot.".to_string(),
            "%%%".to_string(),
            "a".repeat(300),
            "emoji \u{1F4E7} key".to_string(),
        ]
    }

    #[test]
    fn encode_decode_round_trips_over_hostile_keys() {
        for key in hostile_keys() {
            let encoded = encode(&key);
            assert!(!encoded.contains('/'), "{encoded} must not contain a raw /");
            assert!(
                !encoded.chars().any(char::is_whitespace),
                "{encoded} must not contain raw whitespace"
            );
            assert_eq!(decode(&encoded).unwrap(), key, "round trip of {key:?}");
        }
    }

    #[test]
    fn canonical_selectors_round_trip_through_the_parser() {
        for key in hostile_keys() {
            for mailbox in ["inbox", "archive", DRAFTS_MAILBOX, "Some Folder/Sub"] {
                let selector = Selector::new("work account", mailbox, key.clone());
                let printed = selector.to_string();
                let parts = parse(&printed).expect("canonical form parses");
                assert_eq!(parts.account.as_deref(), Some("work account"));
                assert_eq!(parts.mailbox.as_deref(), Some(mailbox));
                assert_eq!(parts.key, key);
            }
        }
    }

    #[test]
    fn elided_forms_take_the_defaults_positionally() {
        let q = parse_in("key@example.com", Namespace::Received, "work", None).unwrap();
        assert_eq!(q.account, "work");
        assert_eq!(q.mailbox, None);
        assert_eq!(q.key, "key@example.com");

        let q = parse_in("archive/key@example.com", Namespace::Received, "work", None).unwrap();
        assert_eq!(q.account, "work");
        assert_eq!(q.mailbox.as_deref(), Some("archive"));

        // The selector's own mailbox wins over the flag beside it.
        let q = parse_in(
            "archive/key@example.com",
            Namespace::Received,
            "work",
            Some("inbox"),
        )
        .unwrap();
        assert_eq!(q.mailbox.as_deref(), Some("archive"));

        let q = parse_in("key@example.com", Namespace::Received, "work", Some("inbox")).unwrap();
        assert_eq!(q.mailbox.as_deref(), Some("inbox"));

        // Two segments after the scheme is account + key, not mailbox + key.
        let q = parse_in("mp://home/key@example.com", Namespace::Received, "work", None).unwrap();
        assert_eq!(q.account, "home");
        assert_eq!(q.mailbox, None);
        assert_eq!(q.key, "key@example.com");
    }

    #[test]
    fn the_namespace_is_fixed_by_the_caller_not_the_string() {
        // The same string resolves in either namespace; nothing about it is
        // sniffed. Only the mailbox segment is namespace-checked.
        let received = parse_in("2026-07-31-note", Namespace::Received, "work", None).unwrap();
        let draft = parse_in("2026-07-31-note", Namespace::Drafts, "work", None).unwrap();
        assert_eq!(received.key, draft.key);
        assert_eq!(received.namespace, Namespace::Received);
        assert_eq!(draft.mailbox.as_deref(), Some(DRAFTS_MAILBOX));
    }

    #[test]
    fn a_draft_selector_rejects_a_non_drafts_mailbox() {
        let err = parse_in("inbox/some-id", Namespace::Drafts, "work", None).unwrap_err();
        assert!(err.to_string().contains("reserved name"), "{err}");
    }

    #[test]
    fn a_received_selector_rejects_the_reserved_drafts_mailbox() {
        let err = parse_in("drafts/some-id", Namespace::Received, "work", None).unwrap_err();
        assert!(err.to_string().contains("reserved mailbox"), "{err}");
    }

    #[test]
    fn paths_are_refused_with_the_reason_rather_than_searched() {
        for path in [
            "./drafts/2026-07-31.md",
            "/home/user/mail/inbox/message.md",
            "../other.md",
            "~/mail/x.md",
        ] {
            let err = parse(path).unwrap_err();
            assert!(
                err.to_string().contains("looks like a filesystem path"),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn over_long_and_empty_selectors_are_rejected() {
        assert!(parse("").is_err());
        assert!(parse("a//b").is_err());
        assert!(parse("a/b/c").is_err(), "three unqualified segments are not a form");
        assert!(parse("mp://a/b/c/d").is_err());
        assert!(parse("mp://a").is_err(), "the scheme needs at least a key");
    }

    /// The key is the bare Message-ID, so a selector can be typed without
    /// shell-quoting and printed without a trailing `%3E`. Both forms are
    /// accepted on input, because a user pasting from a mail header holds the
    /// bracketed one.
    #[test]
    fn angle_brackets_are_not_part_of_the_key() {
        assert_eq!(message_key("<a@example.com>"), "a@example.com");
        assert_eq!(message_key("a@example.com"), "a@example.com");
        assert_eq!(
            message_key("<sha256-0123456789abcdef@local.invalid>"),
            "sha256-0123456789abcdef@local.invalid"
        );
    }

    #[test]
    fn a_truncated_escape_is_an_error_not_a_literal_percent() {
        assert!(decode("%4").is_err());
        assert!(decode("%zz").is_err());
        assert!(decode("ok%20fine").is_ok());
    }
}
