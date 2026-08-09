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

use std::fmt;
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

/// Two draft files claiming the same `id:`.
///
/// The `drafts` primary key is `(account, id)`, so only one of them can be the
/// row that selector resolves to and the other becomes unaddressable. The
/// index does not try to resolve that (nothing here can say which file the
/// user meant): it picks a deterministic winner and reports the pair, so the
/// shadowed file is visible instead of silently gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdCollision {
    pub id: String,
    /// The file the index kept, i.e. what the selector now resolves to.
    pub kept: PathBuf,
    /// The file the id no longer addresses.
    pub shadowed: PathBuf,
}

impl fmt::Display for IdCollision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "two drafts share the id {}: {} is indexed, {} is not addressable until its \
             id: field is changed",
            self.id,
            self.kept.display(),
            self.shadowed.display()
        )
    }
}

/// A draft file the scan could not parse, kept so the skip is visible instead
/// of silent.
///
/// A `.md` file whose frontmatter will not deserialize (a mistyped YAML list,
/// a frontmatter block that parses to null) has no `id:` to index under and no
/// row to list, so before this it left the index and the file simply vanished
/// from `mp list` and the TUI Drafts view while sitting on disk: the user's
/// draft "disappeared" (#0080). The scan reports it instead, carrying the path
/// and a one-line parse error so the CLI and the TUI can put the broken file
/// back in front of the user, unopenable but named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedDraft {
    pub path: PathBuf,
    /// The parse failure, folded to a single line for a list or a status bar.
    pub error: String,
}

impl fmt::Display for SkippedDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
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
/// that cannot be parsed is skipped rather than failing the refresh: one
/// malformed draft must not hide the other twenty. The skip is reported
/// (see [`refresh_reporting`]) instead of only logged, so the broken file is
/// still surfaced (#0080).
///
/// Collisions are reported rather than swallowed; see [`refresh_reporting`].
pub fn refresh(store: &Store, account: &str, dir: &Path) -> Result<Vec<DraftRow>> {
    Ok(refresh_reporting(store, account, dir)?.0)
}

/// [`refresh`], additionally handing back the id collisions and the parse-
/// skipped files it found, for the callers that can put them in front of the
/// user instead of only in the log.
pub fn refresh_reporting(
    store: &Store,
    account: &str,
    dir: &Path,
) -> Result<(Vec<DraftRow>, Vec<IdCollision>, Vec<SkippedDraft>)> {
    let (parsed, skipped) = scan(dir);
    let (rows, collisions) = dedupe_by_id(parsed);
    for collision in &collisions {
        log::warn!("[drafts] {collision}");
    }
    for skip in &skipped {
        log::warn!("[drafts] skipping {skip}");
    }
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
    Ok((rows, collisions, skipped))
}

/// Keep one row per id and report the rest.
///
/// The winner is the newest file, ties broken by path, so the same directory
/// always indexes the same way regardless of readdir order. This is not a
/// resolution rule: an id is supposed to be unique, and two files carrying one
/// is a state only the user can fix.
fn dedupe_by_id(rows: Vec<DraftRow>) -> (Vec<DraftRow>, Vec<IdCollision>) {
    use std::collections::HashMap;

    let mut winner: HashMap<&str, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        match winner.get(row.id.as_str()) {
            None => {
                winner.insert(&row.id, i);
            }
            Some(&held) => {
                let current = &rows[held];
                let beats = (row.mtime, std::cmp::Reverse(&row.path))
                    > (current.mtime, std::cmp::Reverse(&current.path));
                if beats {
                    winner.insert(&row.id, i);
                }
            }
        }
    }
    let winning: HashMap<String, (usize, PathBuf)> = winner
        .iter()
        .map(|(id, &i)| ((*id).to_string(), (i, rows[i].path.clone())))
        .collect();

    let mut kept = Vec::with_capacity(winning.len());
    let mut collisions = Vec::new();
    for (i, row) in rows.into_iter().enumerate() {
        let (winner_index, winner_path) = &winning[&row.id];
        if *winner_index == i {
            kept.push(row);
        } else {
            collisions.push(IdCollision {
                id: row.id,
                kept: winner_path.clone(),
                shadowed: row.path,
            });
        }
    }
    collisions.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.shadowed.cmp(&b.shadowed)));
    (kept, collisions)
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
///
/// Returns the parsed rows and, beside them, the files that would not parse:
/// the skip is data now, not just a log line, so a caller can list the broken
/// file rather than let it vanish (#0080).
fn scan(dir: &Path) -> (Vec<DraftRow>, Vec<SkippedDraft>) {
    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for entry in WalkDir::new(dir).max_depth(1).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_draft_file(path) {
            continue;
        }
        match row_for(path) {
            Ok(row) => rows.push(row),
            Err(e) => skipped.push(SkippedDraft {
                path: path.to_path_buf(),
                error: concise_error(&e),
            }),
        }
    }
    rows.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    (rows, skipped)
}

/// Fold an anyhow error chain to a single line for a list row or a status bar.
///
/// `{e:#}` joins the context and its source with `: `, then any embedded
/// newlines (a `serde_yaml` message can carry one) are flattened to spaces so
/// the whole reason fits on one line.
fn concise_error(e: &anyhow::Error) -> String {
    format!("{e:#}").split_whitespace().collect::<Vec<_>>().join(" ")
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

/// Mint a draft id: 16 hex characters, the first one a letter.
///
/// Random rather than derived from the filename or the subject, because the
/// whole point of the field is to survive a rename and a subject edit. Short
/// enough to type, wide enough that two agents creating drafts in the same
/// second do not collide.
///
/// The leading letter is load-bearing, not cosmetic (#0077). The id is written
/// into YAML frontmatter unquoted, and a plain hex string is not always a YAML
/// string: `8808e70039225152` is a float in scientific notation, and a
/// 16-digit id is an integer. Both shapes round-trip as something other than
/// the id that was written -- the float deserialises the `id:` field to
/// `None`, so the next refresh mints a *different* id and the draft's identity
/// silently changes; the integer fails deserialisation outright and the draft
/// is skipped from the index. About one in a thousand plain hex strings has
/// one of those shapes, which is exactly the rate at which the drafts tests
/// were failing. A first character in `a..=f` cannot start a YAML number, so
/// the id is always read back as the string that was written.
pub fn new_id() -> String {
    let n = rand::random::<u64>();
    let first = b"abcdef"[(n >> 60) as usize % 6] as char;
    format!("{first}{:015x}", n & 0x0fff_ffff_ffff_ffff)
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
        // Quoted on write (#0083): the shape is stable whatever the id holds.
        assert!(content.contains(&format!("id: \"{id}\"")), "{content}");

        // A second refresh keeps the same id: it is read back, not re-minted.
        let again = refresh(&store, "work", &dir).unwrap();
        assert_eq!(again[0].id, id);
    }

    /// #0077: the flake behind three intermittent failures.
    ///
    /// A minted id is written into YAML frontmatter unquoted, so it has to be
    /// a YAML *string* on the way back. `8808e70039225152` is a float in
    /// scientific notation (the `id:` field then reads back as `None` and the
    /// next refresh mints a different id) and `1234567890123456` is an integer
    /// (the frontmatter fails to deserialise and the draft is skipped). Both
    /// shapes are reachable from 16 random hex characters about once every
    /// thousand ids.
    #[test]
    fn a_minted_id_is_never_a_yaml_number_and_round_trips_verbatim() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("drafts");

        for i in 0..2000 {
            let id = new_id();
            assert!(
                id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit()),
                "{id} is not 16 hex characters"
            );
            assert!(
                id.as_bytes()[0].is_ascii_alphabetic(),
                "{id} starts with a digit, so YAML may read it as a number"
            );
            assert!(
                id.parse::<f64>().is_err() && id.parse::<i64>().is_err(),
                "{id} parses as a number"
            );

            // Every 200th one goes through the real writer and reader, which
            // is where the loss actually happened.
            if i % 200 == 0 {
                let path = write_draft(&dir, &format!("rt-{i}.md"), "");
                crate::draft::set_draft_id(&path, &id).unwrap();
                let back = crate::draft::parse_email_draft(&path).unwrap();
                assert_eq!(back.frontmatter.id.as_deref(), Some(id.as_str()));
            }
        }
    }

    /// The two #0077 shapes, now both loud (#0083).
    ///
    /// A hand-written `id:` that YAML reads as a number is not an id we can
    /// carry: what must never happen is the float shape's old behaviour, where
    /// it read back as `None` and the next refresh minted a *replacement* into
    /// the file, changing the draft's identity with no error anywhere. Both
    /// shapes now fail to deserialise, so both take the skipped-draft path
    /// that names the file and the reason.
    #[test]
    fn a_number_shaped_id_is_rejected_loudly_and_never_re_minted() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("drafts");

        let float_shaped = write_draft(&dir, "float.md", "id: 8808e70039225152\n");
        let err = parse_email_draft(&float_shaped).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("id"), "the error names the field: {message}");

        let int_shaped = write_draft(&dir, "int.md", "id: 1234567890123456\n");
        assert!(parse_email_draft(&int_shaped).is_err(), "an int-shaped id fails to deserialise");

        // The index skips both and names both, and neither file is rewritten.
        let before_float = fs::read_to_string(&float_shaped).unwrap();
        let before_int = fs::read_to_string(&int_shaped).unwrap();
        let (_tmp2, store) = store();
        let (rows, _collisions, skipped) = refresh_reporting(&store, "work", &dir).unwrap();
        assert!(rows.is_empty(), "neither draft is indexed under a minted id: {rows:?}");
        assert_eq!(skipped.len(), 2, "both drafts are skipped: {skipped:?}");
        let named: Vec<PathBuf> = skipped.iter().map(|s| s.path.clone()).collect();
        assert!(named.contains(&float_shaped) && named.contains(&int_shaped), "{named:?}");
        for skip in &skipped {
            let line = skip.to_string();
            assert!(line.contains(&skip.path.display().to_string()), "{line}");
            assert!(line.contains("string"), "the reason says a string was expected: {line}");
        }

        assert_eq!(fs::read_to_string(&float_shaped).unwrap(), before_float);
        assert_eq!(fs::read_to_string(&int_shaped).unwrap(), before_int);
    }

    /// A quoted number-shaped id is a string and is honoured verbatim: the
    /// rejection is of the YAML *shape*, not of digits.
    #[test]
    fn a_quoted_number_shaped_id_is_a_perfectly_good_id() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        write_draft(&dir, "quoted.md", "id: \"1234567890123456\"\n");

        let (rows, _collisions, skipped) = refresh_reporting(&store, "work", &dir).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(rows[0].id, "1234567890123456");
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

    /// Two files carrying one `id:` collapse to one row, because the index
    /// primary key says so. What must not happen is that collapse being
    /// silent: the shadowed file is still on disk and no longer addressable,
    /// so the refresh names both paths. The winner is deterministic (newest
    /// file, ties by path) so a re-index does not flip which one answers.
    #[test]
    fn two_drafts_sharing_an_id_collapse_to_one_row_and_are_reported() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        let one = write_draft(&dir, "one.md", "id: shared\n");
        let two = write_draft(&dir, "two.md", "id: shared\n");

        let (rows, collisions, _skipped) = refresh_reporting(&store, "work", &dir).unwrap();
        assert_eq!(rows.len(), 1, "one id is one row");
        assert_eq!(list(&store, "work", None).unwrap().len(), 1);
        let indexed = find(&store, "work", "shared").unwrap().unwrap().path;
        assert_eq!(indexed, rows[0].path);

        assert_eq!(collisions.len(), 1, "the shadowed file is reported, not dropped");
        assert_eq!(collisions[0].id, "shared");
        assert_eq!(collisions[0].kept, indexed);
        assert_ne!(collisions[0].shadowed, indexed);
        assert!([one.clone(), two.clone()].contains(&collisions[0].shadowed));
        let message = collisions[0].to_string();
        assert!(message.contains(&one.display().to_string()), "{message}");
        assert!(message.contains(&two.display().to_string()), "{message}");

        // Deterministic: the same directory indexes the same way every time.
        let (again, _, _) = refresh_reporting(&store, "work", &dir).unwrap();
        assert_eq!(again[0].path, indexed);
    }

    /// The winner rule itself, away from the filesystem's mtime resolution:
    /// newest file wins, and equal mtimes are broken by path so readdir order
    /// cannot decide which draft an id addresses.
    #[test]
    fn the_surviving_row_is_the_newest_file_then_the_first_path() {
        let row = |path: &str, mtime: i64| DraftRow {
            id: "shared".to_string(),
            slug: path.trim_end_matches(".md").to_string(),
            path: PathBuf::from(path),
            mtime,
            size: 1,
            status: "draft".to_string(),
            to: None,
            cc: None,
            subject: None,
            date: None,
            snippet: None,
        };

        let (kept, collisions) = dedupe_by_id(vec![row("b.md", 10), row("a.md", 20)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, PathBuf::from("a.md"));
        assert_eq!(collisions[0].shadowed, PathBuf::from("b.md"));

        let (kept, collisions) = dedupe_by_id(vec![row("b.md", 10), row("a.md", 10)]);
        assert_eq!(kept[0].path, PathBuf::from("a.md"), "equal mtime breaks by path");
        assert_eq!(collisions[0].shadowed, PathBuf::from("b.md"));

        // A third file claiming the id names the final winner, not an
        // intermediate one.
        let (kept, collisions) = dedupe_by_id(vec![row("c.md", 5), row("b.md", 10), row("a.md", 20)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(collisions.len(), 2);
        assert!(collisions.iter().all(|c| c.kept == PathBuf::from("a.md")));
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

        let (rows, _collisions, skipped) = refresh_reporting(&store, "work", &dir).unwrap();
        assert_eq!(rows.len(), 1, "only the readable draft is indexed");
        assert_eq!(rows[0].id, "aaa");
        assert!(find(&store, "work", "bbb").unwrap().is_none());

        // The broken file is not dropped: it is reported so a lister can put it
        // back in front of the user (#0080).
        assert_eq!(skipped.len(), 1, "the unparseable draft is reported");
        assert_eq!(skipped[0].path, dir.join("broken.md"));
        assert!(!skipped[0].error.is_empty(), "the skip carries a parse error");
        assert!(!skipped[0].error.contains('\n'), "the error is one line");
    }

    /// A frontmatter block that deserializes to null (`invalid type: null`)
    /// and a mistyped attachments list are both reported as skips rather than
    /// dropped, which is the "my draft disappeared" report this ticket answers
    /// (#0080). The scan never fails the whole refresh over one of them.
    #[test]
    fn parse_skipped_drafts_are_reported_with_their_paths() {
        let (tmp, store) = store();
        let dir = tmp.path().join("drafts");
        write_draft(&dir, "good.md", "id: aaa\n");
        // Whole frontmatter parses to null.
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("null.md"), "---\n---\n\nBody\n").unwrap();
        // A mistyped attachments list item (no dash-space).
        fs::write(
            dir.join("attach.md"),
            "---\nid: ccc\nstatus: draft\nattachments:\n-\"/x\"\n---\n\nBody\n",
        )
        .unwrap();

        let (rows, _collisions, skipped) = refresh_reporting(&store, "work", &dir).unwrap();
        assert_eq!(rows.len(), 1, "the readable draft still indexes");
        assert_eq!(skipped.len(), 2, "both broken files are reported");
        let paths: Vec<_> = skipped.iter().map(|s| s.path.clone()).collect();
        assert!(paths.contains(&dir.join("null.md")));
        assert!(paths.contains(&dir.join("attach.md")));
        assert!(skipped.iter().all(|s| !s.error.is_empty()));
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

