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
//! quote), so the query goes through the one shared parser in
//! [`crate::search`] (#0086a) and is lowered here by [`crate::search::to_fts`]
//! into a quoted `MATCH` expression plus the SQL predicates the contentless
//! index cannot carry (an attachment column test and a date range). That single
//! parser is what closes the #0043 two-grammar debt: `--local` and the server
//! path now read the same input. [`fts_expression`] survives as a thin renderer
//! for the callers and tests that only want the `MATCH` string.

use anyhow::{Context, Result};

use super::read::{row_columns, row_from_sql, MessageRow};
use super::Store;

pub use crate::search::fts_expression;

/// One ranked hit: the message row and the bm25 score it matched with.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub row: MessageRow,
    /// bm25 rank. FTS5 returns a *negative* number and a smaller (more
    /// negative) one is a better match, which is why the ordering is `ASC`.
    pub rank: f64,
}

/// A `YYYY-MM-DD` date as the unix timestamp of its UTC midnight, which is the
/// same clock `ingest::date_sort_for` stamps rows with. `after:D` keeps rows at
/// or past that instant, `before:D` keeps rows strictly before it.
fn date_to_timestamp(date: &str) -> Option<i64> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
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
    let parsed = crate::search::parse(query).map_err(|e| anyhow::anyhow!("{e}"))?;
    search_ast(store, account, &parsed, mailbox, limit)
}

/// The same search over an already-parsed [`crate::search::Query`], so the
/// `mp search --local` flags (`--from`, `--has-attachment`, ...) reach the FTS
/// index through the same AST the positional grammar builds.
pub fn search_ast(
    store: &Store,
    account: &str,
    parsed: &crate::search::Query,
    mailbox: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let render = crate::search::to_fts(parsed).map_err(|e| anyhow::anyhow!("{e}"))?;

    // A query with no clause at all (an empty string, or bare punctuation) has
    // nothing to answer, the same honest refusal the old parser gave.
    if render.match_expr.is_none()
        && !render.has_attachment
        && render.before.is_none()
        && render.after.is_none()
    {
        anyhow::bail!("nothing to search for: the query has no searchable words");
    }

    let columns = row_columns();

    // The predicates the contentless FTS index cannot carry ride as ordinary
    // SQL over the joined `messages` row: an attachment column test and a date
    // range against the same `date_sort` unix seconds ingest stamped.
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut predicates = String::new();

    // `?1` is always the account; the FTS `MATCH`, when present, is `?2`.
    binds.push(Box::new(account.to_string()));
    let (from_clause, rank_expr, base_where) = if let Some(ref expr) = render.match_expr {
        binds.push(Box::new(expr.clone()));
        (
            "messages_fts JOIN messages ON messages.id = messages_fts.rowid",
            "bm25(messages_fts, 10.0, 5.0, 1.0)",
            "messages_fts MATCH ?2 AND messages.account = ?1",
        )
    } else {
        ("messages", "0.0", "messages.account = ?1")
    };

    if let Some(mb) = mailbox {
        predicates.push_str(&format!(" AND messages.mailbox = ?{}", binds.len() + 1));
        binds.push(Box::new(mb.to_string()));
    }
    if render.has_attachment {
        predicates.push_str(" AND messages.has_attachments = 1");
    }
    if let Some(ref after) = render.after {
        let ts = date_to_timestamp(after)
            .with_context(|| format!("after: is not a valid date: {after}"))?;
        predicates.push_str(&format!(" AND messages.date_sort >= ?{}", binds.len() + 1));
        binds.push(Box::new(ts));
    }
    if let Some(ref before) = render.before {
        let ts = date_to_timestamp(before)
            .with_context(|| format!("before: is not a valid date: {before}"))?;
        predicates.push_str(&format!(" AND messages.date_sort < ?{}", binds.len() + 1));
        binds.push(Box::new(ts));
    }
    let limit_placeholder = binds.len() + 1;
    binds.push(Box::new(limit as i64));

    let sql = format!(
        "SELECT {columns}, {rank_expr} AS rank
         FROM {from_clause}
         WHERE {base_where}{predicates}
         ORDER BY rank ASC, messages.date_sort DESC, messages.id DESC
         LIMIT ?{limit_placeholder}"
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
    // expression is generated, so a failure is a bug in the translation and not
    // something to hand to the user as SQL.
    let rows = stmt
        .query_map(rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())), map)
        .context("running the full-text search")?;
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
        // Bare, every one of these is an FTS5 syntax error, so each term is
        // wrapped in quotes. Migration note (#0086a): parentheses are now
        // grammar, so `(draft)` is a one-term group rendered as `"draft"`,
        // not the literal `"(draft)"` the retired parser produced.
        assert_eq!(fts_expression("c++ (draft)").unwrap(), "\"c++\" \"draft\"");
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
