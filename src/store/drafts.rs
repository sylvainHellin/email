//! The drafts index: a derived table over `<account_dir>/drafts/`.
//!
//! The file stays truth (#0050 scope item 5). Drafts are the only local-only
//! thing in the product: agents write `.md` files into the drafts directory
//! and `$EDITOR` rewrites them behind the application's back, so a table that
//! claimed to own them would be wrong within a second. What the table buys is
//! the *lookup*: `(account, id)` is the primary key, so a selector resolves in
//! one indexed read instead of a parse loop over the directory, and the TUI
//! and `mp list` read the same rows.
//!
//! Identity is the `id:` frontmatter field (decision C of the DAL plan), not
//! the filename, so renaming a draft keeps its selector working. A file
//! without one is assigned an id on the first refresh and the field is written
//! back, which is the only write this module ever makes to a draft.
//!
//! Freshness is a one-second [`fingerprint`] scan of that single `max_depth(1)`
//! directory of tens of files, plus an explicit [`refresh`] at engine start and
//! after any command that writes a draft. A `notify`-style watcher is a later
//! refinement and deliberately not a new dependency here.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use walkdir::WalkDir;

use crate::draft::{parse_email_draft, set_draft_id};
use crate::store::Store;

/// How much of the body the index keeps for a list line.
const SNIPPET_CHARS: usize = 200;

/// One row of the `drafts` table, i.e. one `.md` file as the index sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRow {
    /// The `id:` frontmatter field: the draft's identity and selector key.
    pub id: String,
    /// The filename stem, kept for display only. It changes on a rename; the
    /// id does not.
    pub slug: String,
    pub path: PathBuf,
    pub mtime: i64,
    pub size: i64,
    /// `draft`, `approved` or `sent`, as the frontmatter spells it.
    pub status: String,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    pub snippet: Option<String>,
}

/// Rebuild the whole index for one account from `dir`.
///
/// Deliberately a full rebuild rather than a diff: the directory holds tens of
/// files, a full pass is one `stat` plus one parse each, and a diff would need
/// its own correctness argument for the case the fingerprint cannot see (an
/// edit that preserves mtime and size). The caller decides *when* to pay it;
/// [`fingerprint`] is what makes that decision cheap.
///
/// A file with no `id:` gets one assigned and written back before it is
/// indexed, so the selector it is listed under is the one in the file. A file
/// that cannot be parsed is skipped with a log line rather than failing the
/// refresh: one malformed draft must not hide the other twenty.
pub fn refresh(store: &Store, account: &str, dir: &Path) -> Result<Vec<DraftRow>> {
    let rows = scan(dir);
    let conn = store.conn();
    let tx_guard = conn.unchecked_transaction()?;
    conn.execute("DELETE FROM drafts WHERE account = ?1", [account])?;
    {
        let mut stmt = conn.prepare(
            "INSERT INTO drafts
             (account, id, slug, path, mtime, size, status, to_, cc, subject, date, snippet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (account, id) DO UPDATE SET
               slug = excluded.slug, path = excluded.path, mtime = excluded.mtime,
               size = excluded.size, status = excluded.status, to_ = excluded.to_,
               cc = excluded.cc, subject = excluded.subject, date = excluded.date,
               snippet = excluded.snippet",
        )?;
        for row in &rows {
            stmt.execute(rusqlite::params![
                account,
                row.id,
                row.slug,
                row.path.to_string_lossy(),
                row.mtime,
                row.size,
                row.status,
                row.to,
                row.cc,
                row.subject,
                row.date,
                row.snippet,
            ])?;
        }
    }
    tx_guard.commit()?;
    Ok(rows)
}

/// Refresh the index of an account from its configured drafts directory,
/// opening the store itself. Best-effort: an account with no store yet (never
/// synced) has nowhere to index into, which is not an error.
pub fn refresh_account(account: &str) -> Result<Vec<DraftRow>> {
    let dir = crate::config::drafts_dir(account);
    let store = Store::open(crate::config::store_path(account))
        .with_context(|| format!("opening the store of {account}"))?;
    refresh(&store, account, &dir)
}

/// Every indexed draft of one account, newest file first, optionally filtered
/// by status.
pub fn list(store: &Store, account: &str, status: Option<&str>) -> Result<Vec<DraftRow>> {
    let sql = format!(
        "SELECT id, slug, path, mtime, size, status, to_, cc, subject, date, snippet
         FROM drafts WHERE account = ?1 {}
         ORDER BY mtime DESC, id ASC",
        if status.is_some() { "AND status = ?2" } else { "" }
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let mapped = |row: &rusqlite::Row<'_>| -> rusqlite::Result<DraftRow> {
        Ok(DraftRow {
            id: row.get(0)?,
            slug: row.get(1)?,
            path: PathBuf::from(row.get::<_, String>(2)?),
            mtime: row.get(3)?,
            size: row.get(4)?,
            status: row.get(5)?,
            to: row.get(6)?,
            cc: row.get(7)?,
            subject: row.get(8)?,
            date: row.get(9)?,
            snippet: row.get(10)?,
        })
    };
    let rows = match status {
        Some(status) => stmt
            .query_map(rusqlite::params![account, status], mapped)?
            .collect::<rusqlite::Result<Vec<_>>>(),
        None => stmt
            .query_map(rusqlite::params![account], mapped)?
            .collect::<rusqlite::Result<Vec<_>>>(),
    };
    rows.context("reading the drafts index")
}

/// One indexed draft by id: the single lookup a draft selector resolves to.
pub fn find(store: &Store, account: &str, id: &str) -> Result<Option<DraftRow>> {
    let mut stmt = store.conn().prepare(
        "SELECT id, slug, path, mtime, size, status, to_, cc, subject, date, snippet
         FROM drafts WHERE account = ?1 AND id = ?2",
    )?;
    let row = stmt
        .query_row(rusqlite::params![account, id], |row| {
            Ok(DraftRow {
                id: row.get(0)?,
                slug: row.get(1)?,
                path: PathBuf::from(row.get::<_, String>(2)?),
                mtime: row.get(3)?,
                size: row.get(4)?,
                status: row.get(5)?,
                to: row.get(6)?,
                cc: row.get(7)?,
                subject: row.get(8)?,
                date: row.get(9)?,
                snippet: row.get(10)?,
            })
        })
        .optional()
        .context("looking a draft up by id")?;
    Ok(row)
}

/// A cheap stat-only summary of the drafts directory: file count, names and
/// `(mtime, size)` folded into one number.
///
/// This is the one-second poll. It reads no file contents, so it costs one
/// `readdir` plus one `stat` per entry, and it changes whenever a draft is
/// created, deleted, renamed or written. Two different directory states can in
/// principle fold to the same number; the cost of that collision is a listing
/// that refreshes one tick late, which is why the explicit [`refresh`] calls
/// after our own writes exist.
pub fn fingerprint(dir: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut entries: Vec<(String, i64, u64)> = Vec::new();
    for entry in WalkDir::new(dir).max_depth(1).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_draft_file(path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        entries.push((
            path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            mtime_secs(&meta),
            meta.len(),
        ));
    }
    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entries.hash(&mut hasher);
    hasher.finish()
}

/// Read every draft in `dir`, assigning an id to any file that lacks one.
fn scan(dir: &Path) -> Vec<DraftRow> {
    let mut rows = Vec::new();
    for entry in WalkDir::new(dir).max_depth(1).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_draft_file(path) {
            continue;
        }
        match row_for(path) {
            Ok(row) => rows.push(row),
            Err(e) => log::warn!("[drafts] skipping {}: {e:#}", path.display()),
        }
    }
    rows.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
    rows
}

fn is_draft_file(path: &Path) -> bool {
    path.is_file() && path.extension().is_some_and(|ext| ext == "md")
}

/// Index one file, writing an `id:` back into it when it has none.
fn row_for(path: &Path) -> Result<DraftRow> {
    let draft = parse_email_draft(path)?;
    let id = match draft.frontmatter.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => {
            let id = new_id();
            set_draft_id(path, &id)?;
            id
        }
    };
    // Stat *after* a possible write-back, so the indexed mtime matches the file.
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    Ok(DraftRow {
        id,
        slug: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        path: path.to_path_buf(),
        mtime: mtime_secs(&meta),
        size: meta.len() as i64,
        status: draft.frontmatter.status.to_string(),
        to: draft.frontmatter.to.clone(),
        cc: draft.frontmatter.cc.clone(),
        subject: non_empty(&draft.frontmatter.subject),
        date: draft.frontmatter.date.clone(),
        snippet: snippet_of(&draft.body_markdown),
    })
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// The first [`SNIPPET_CHARS`] characters of the body, on one line.
fn snippet_of(body: &str) -> Option<String> {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    Some(flat.chars().take(SNIPPET_CHARS).collect())
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Mint a draft id: 16 random hex characters.
///
/// Random rather than derived from the filename or the subject, because the
/// whole point of the field is to survive a rename and a subject edit. Short
/// enough to type, wide enough that two agents creating drafts in the same
/// second do not collide.
pub fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::fs;
    use tempfile::TempDir;

    fn store() -> (TempDir, Store) {
        let tmp = TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path().join("store.sqlite3")).expect("store");
        (tmp, store)
    }

    fn write_draft(dir: &Path, name: &str, extra: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(
            &path,
            format!("---\nto: a@example.com\nsubject: Hello\nstatus: draft\n{extra}---\n\nBody here\n"),
        )
        .unwrap();
        path
    }

    #[test]
    fn a_draft_without_an_id_is_assigned_one_and_it_is_written_back() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        let path = write_draft(&dir, "note.md", "");

        let rows = refresh(&store, "work", &dir).unwrap();
        assert_eq!(rows.len(), 1);
        let id = rows[0].id.clone();
        assert_eq!(id.len(), 16);

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(&format!("id: {id}")), "{content}");

        // A second refresh keeps the same id: it is read back, not re-minted.
        let again = refresh(&store, "work", &dir).unwrap();
        assert_eq!(again[0].id, id);
    }

    #[test]
    fn renaming_a_file_keeps_the_draft_id() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        let path = write_draft(&dir, "before.md", "");
        let id = refresh(&store, "work", &dir).unwrap()[0].id.clone();

        let renamed = dir.join("after.md");
        fs::rename(&path, &renamed).unwrap();
        refresh(&store, "work", &dir).unwrap();

        let row = find(&store, "work", &id).unwrap().expect("id survives the rename");
        assert_eq!(row.path, renamed);
        assert_eq!(row.slug, "after");
    }

    #[test]
    fn a_deleted_file_leaves_the_index() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        let path = write_draft(&dir, "note.md", "");
        let id = refresh(&store, "work", &dir).unwrap()[0].id.clone();
        fs::remove_file(&path).unwrap();
        refresh(&store, "work", &dir).unwrap();
        assert!(find(&store, "work", &id).unwrap().is_none());
    }

    #[test]
    fn the_index_carries_the_list_columns_and_filters_by_status() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        write_draft(&dir, "one.md", "id: aaa\ncc: c@example.com\ndate: Mon, 1 Jan 2026\n");
        write_draft(&dir, "two.md", "id: bbb\n");
        // `two` is approved.
        let two = dir.join("two.md");
        let content = fs::read_to_string(&two).unwrap().replace("status: draft", "status: approved");
        fs::write(&two, content).unwrap();

        refresh(&store, "work", &dir).unwrap();
        let all = list(&store, "work", None).unwrap();
        assert_eq!(all.len(), 2);
        let one = find(&store, "work", "aaa").unwrap().unwrap();
        assert_eq!(one.to.as_deref(), Some("a@example.com"));
        assert_eq!(one.cc.as_deref(), Some("c@example.com"));
        assert_eq!(one.subject.as_deref(), Some("Hello"));
        assert_eq!(one.date.as_deref(), Some("Mon, 1 Jan 2026"));
        assert_eq!(one.snippet.as_deref(), Some("Body here"));
        assert_eq!(one.status, "draft");

        let approved = list(&store, "work", Some("approved")).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, "bbb");
    }

    /// A draft written with a bare `subject:` line, which is what the old
    /// skeleton produced and what an agent writing YAML by hand produces, is
    /// indexed with an empty subject rather than skipped (#0050). The index
    /// making it invisible is precisely the failure this ticket exists to end;
    /// `mp validate` is where the missing subject is reported.
    #[test]
    fn a_draft_with_a_bare_subject_key_is_indexed_not_skipped() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bare.md");
        fs::write(
            &path,
            "---\nid: aaa\nto:\nsubject:\nstatus: draft\n---\n\nBody\n",
        )
        .unwrap();

        refresh(&store, "work", &dir).unwrap();
        let row = find(&store, "work", "aaa").unwrap().expect("indexed");
        assert_eq!(row.subject, None, "an empty subject is listed as empty");
        assert_eq!(row.to, None);

        // And validation, not the index, is what refuses it.
        let draft = parse_email_draft(&path).unwrap();
        let err = crate::draft::validate_draft(&draft).unwrap_err().to_string();
        assert!(err.contains("No recipients"), "{err}");
    }

    /// Tolerance is scoped: a file whose YAML is genuinely broken is still
    /// skipped, and the rest of the directory still indexes. One malformed
    /// draft must not hide the other twenty, and it must not be indexed under
    /// a guessed identity either.
    #[test]
    fn a_genuinely_malformed_draft_is_still_skipped() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        write_draft(&dir, "good.md", "id: aaa\n");
        fs::write(
            dir.join("broken.md"),
            "---\nid: bbb\nstatus: not-a-status\nsubject: [unclosed\n---\n\nBody\n",
        )
        .unwrap();

        let rows = refresh(&store, "work", &dir).unwrap();
        assert_eq!(rows.len(), 1, "only the readable draft is indexed");
        assert_eq!(rows[0].id, "aaa");
        assert!(find(&store, "work", "bbb").unwrap().is_none());
    }

    #[test]
    fn the_fingerprint_changes_when_the_directory_does() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("drafts");
        fs::create_dir_all(&dir).unwrap();
        let empty = fingerprint(&dir);

        let path = write_draft(&dir, "note.md", "id: aaa\n");
        let one = fingerprint(&dir);
        assert_ne!(empty, one);
        assert_eq!(one, fingerprint(&dir), "a stable directory is stable");

        fs::write(&path, "---\nsubject: Hello\nstatus: draft\nid: aaa\n---\n\nLonger body\n").unwrap();
        assert_ne!(one, fingerprint(&dir), "a rewrite changes the size");

        fs::remove_file(&path).unwrap();
        assert_eq!(empty, fingerprint(&dir));
    }
}
