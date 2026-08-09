//! Full-text search over the store's FTS5 index (#0043).
//!
//! Search is a `SELECT`, not a file stream. The index (`messages_fts`) has
//! existed since #0038 and is written inside the same transaction as the
//! `messages` row it describes ([`crate::ingest`]), and removed by every
//! delete path (`store::write::delete_row`, `pending_ops::apply_delete`, and
//! the prune through `delete_by_uid`), so it never needs a reconcile pass.
//! What was missing was a way to ask it a question; this module is that.
//!
//! Two constraints come from the index being *contentless*
//! (`content=''`, `contentless_delete=1`, see [`super::schema`]):
//!
//! - Only `rowid`-returning `MATCH` queries work. `snippet()` and
//!   `highlight()` fail, so a hit is rendered from the `messages` row it joins
//!   to (which is where the stored snippet lives) and never from the index.
//! - There is nothing to rebuild the index *from*: the body text lives in a
//!   blob and the index keeps no copy. That is not a gap, because the store is
//!   a cache with a drop-and-rebuild contract: a file whose index is suspect is
//!   deleted and refilled by the next sync. [`index_drift`] is the check that
//!   says whether it is suspect, so the question can be asked without guessing.
//!
//! The user's query is *not* passed to FTS5 verbatim. A bare MATCH expression
//! makes half of ordinary typing a syntax error (`c++`, `foo:bar`, a stray
//! quote), so [`fts_expression`] translates the input into a quoted expression
//! whose terms are AND-ed, with three affordances kept from the server-side
//! `mp search` grammar: `"a phrase"`, a trailing `*` for a prefix, and the
//! `subject:` / `from:` / `body:` column filters.

use anyhow::{bail, Context, Result};

use super::read::{row_columns, row_from_sql, MessageRow};
use super::Store;

/// One ranked hit: the message row and the bm25 score it matched with.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub row: MessageRow,
    /// bm25 rank. FTS5 returns a *negative* number and a smaller (more
    /// negative) one is a better match, which is why the ordering is `ASC`.
    pub rank: f64,
}

/// Which FTS column a `field:` prefix names, if any.
fn column_for(field: &str) -> Option<&'static str> {
    match field {
        "subject" => Some("subject"),
        "from" => Some("from_"),
        "body" | "text" => Some("body_text"),
        _ => None,
    }
}

/// Split a query into terms, keeping `"a quoted phrase"` in one piece.
///
/// An unterminated quote closes at the end of the input rather than failing:
/// the user is mid-typing, and a search that errors on a half-typed phrase is
/// worse than one that searches the half.
fn split_terms(query: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    for ch in query.chars() {
        match ch {
            '"' => {
                if in_quotes {
                    in_quotes = false;
                    out.push((std::mem::take(&mut current), true));
                    quoted = false;
                } else {
                    if !current.is_empty() {
                        out.push((std::mem::take(&mut current), false));
                    }
                    in_quotes = true;
                    quoted = true;
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    out.push((std::mem::take(&mut current), false));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push((current, quoted));
    }
    out
}

/// True when the term carries nothing an FTS5 tokenizer would index, so
/// searching for it would either be a syntax error or match everything.
fn is_indexable(term: &str) -> bool {
    term.chars().any(char::is_alphanumeric)
}

/// Translate a user's query into an FTS5 MATCH expression.
///
/// Every term becomes a double-quoted string literal, which is what makes
/// arbitrary punctuation safe: inside quotes FTS5 treats the content as text
/// and only `"` needs escaping (by doubling). Terms are space-separated, and
/// FTS5's implicit operator between them is `AND`.
///
/// Recognised, in this order, per term:
///
/// - `field:term` where field is `subject`, `from`, `body` or `text` -> a
///   column filter. An unknown field is *not* a filter: `foo:bar` is searched
///   as the text it is, because a colon in a search box is far more often part
///   of a subject line than a field the user meant.
/// - a trailing `*` -> prefix match (`"term"*`). Inside a quoted phrase the
///   `*` is literal, as it is in every other search box.
///
/// `Err` when nothing indexable survives, because an empty MATCH expression is
/// an FTS5 syntax error and "you searched for nothing" is the honest answer.
pub fn fts_expression(query: &str) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for (term, was_quoted) in split_terms(query) {
        let mut term = term;
        let mut column = None;
        if !was_quoted {
            if let Some((field, rest)) = term.split_once(':') {
                if let Some(col) = column_for(&field.to_lowercase()) {
                    if is_indexable(rest) {
                        column = Some(col);
                        term = rest.to_string();
                    }
                }
            }
        }
        let prefix = !was_quoted && term.ends_with('*');
        if prefix {
            term.pop();
        }
        if !is_indexable(&term) {
            continue;
        }
        let escaped = term.replace('"', "\"\"");
        let star = if prefix { "*" } else { "" };
        parts.push(match column {
            Some(col) => format!("{col}:\"{escaped}\"{star}"),
            None => format!("\"{escaped}\"{star}"),
        });
    }
    if parts.is_empty() {
        bail!("nothing to search for: the query has no searchable words");
    }
    Ok(parts.join(" "))
}

/// Search one account's messages, best match first.
///
/// `mailbox` scopes the search to one mailbox key (the `MailboxRole` spelling
/// ingest recorded); `None` searches every mailbox of the account, which is
/// the point of the command -- the file era could only ever grep one directory
/// at a time.
///
/// The bm25 weights are `subject`, `from_`, `body_text` in that order: a word
/// in the subject outranks the same word buried in a quoted reply chain.
pub fn search(
    store: &Store,
    account: &str,
    query: &str,
    mailbox: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let expression = fts_expression(query)?;
    let columns = row_columns();
    let mailbox_clause = if mailbox.is_some() {
        "AND messages.mailbox = ?4"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {columns}, bm25(messages_fts, 10.0, 5.0, 1.0) AS rank
         FROM messages_fts
         JOIN messages ON messages.id = messages_fts.rowid
         WHERE messages_fts MATCH ?1 AND messages.account = ?2 {mailbox_clause}
         ORDER BY rank ASC, messages.date_sort DESC, messages.id DESC
         LIMIT ?3"
    );
    let mut stmt = store
        .conn()
        .prepare(&sql)
        .context("preparing the full-text search")?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok(SearchHit {
            row: row_from_sql(row)?,
            rank: row.get("rank")?,
        })
    };
    // An FTS5 syntax error surfaces here rather than at prepare time, and the
    // expression is generated, so a failure is a bug in the translation above
    // and not something to hand to the user as SQL.
    let rows = match mailbox {
        Some(mb) => stmt.query_map(
            rusqlite::params![&expression, account, limit as i64, mb],
            map,
        ),
        None => stmt.query_map(rusqlite::params![&expression, account, limit as i64], map),
    }
    .with_context(|| format!("running the full-text search for {expression}"))?;
    let mut out = Vec::new();
    for hit in rows {
        out.push(hit.context("reading a search hit")?);
    }
    Ok(out)
}

/// How far the index has drifted from the rows: `(rows not indexed, index
/// entries with no row)`.
///
/// Both are zero in a store every writer went through, because every write
/// path indexes inside the transaction that writes the row and every delete
/// path removes the entry in the transaction that removes it. It is here so
/// the invariant is *checkable* rather than merely claimed, which is what the
/// tests assert after ingest, re-ingest, delete and prune. A non-zero answer
/// is not repairable in place (a contentless index cannot be rebuilt from the
/// store), so the remedy is the drop-and-rebuild contract the store already
/// has: delete the file and sync.
pub fn index_drift(store: &Store) -> Result<(i64, i64)> {
    let unindexed: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM messages
             WHERE id NOT IN (SELECT rowid FROM messages_fts)",
            [],
            |r| r.get(0),
        )
        .context("counting unindexed messages")?;
    let orphaned: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM messages_fts
             WHERE rowid NOT IN (SELECT id FROM messages)",
            [],
            |r| r.get(0),
        )
        .context("counting orphaned index entries")?;
    Ok((unindexed, orphaned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_query_becomes_and_ed_quoted_terms() {
        assert_eq!(fts_expression("hello world").unwrap(), "\"hello\" \"world\"");
    }

    #[test]
    fn punctuation_is_quoted_rather_than_parsed() {
        // Bare, every one of these is an FTS5 syntax error.
        assert_eq!(fts_expression("c++ (draft)").unwrap(), "\"c++\" \"(draft)\"");
        assert_eq!(fts_expression("say \"\"hi").unwrap(), "\"say\" \"hi\"");
    }

    #[test]
    fn a_quoted_phrase_stays_one_term() {
        assert_eq!(
            fts_expression("\"quarterly report\" urgent").unwrap(),
            "\"quarterly report\" \"urgent\""
        );
    }

    #[test]
    fn an_unterminated_quote_searches_what_was_typed() {
        assert_eq!(fts_expression("\"half typed").unwrap(), "\"half typed\"");
    }

    #[test]
    fn a_trailing_star_is_a_prefix_query() {
        assert_eq!(fts_expression("invoi*").unwrap(), "\"invoi\"*");
    }

    #[test]
    fn a_star_inside_a_phrase_is_literal() {
        assert_eq!(fts_expression("\"a*b\"").unwrap(), "\"a*b\"");
    }

    #[test]
    fn known_fields_become_column_filters() {
        assert_eq!(fts_expression("subject:invoice").unwrap(), "subject:\"invoice\"");
        assert_eq!(fts_expression("from:ada").unwrap(), "from_:\"ada\"");
        assert_eq!(fts_expression("body:ledger").unwrap(), "body_text:\"ledger\"");
    }

    #[test]
    fn an_unknown_field_is_searched_as_text() {
        assert_eq!(fts_expression("re:budget").unwrap(), "\"re:budget\"");
    }

    #[test]
    fn a_query_with_no_words_is_refused() {
        assert!(fts_expression("").is_err());
        assert!(fts_expression("   ").is_err());
        assert!(fts_expression("-- ??").is_err());
    }
}
