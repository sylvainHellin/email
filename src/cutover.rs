//! `mp cutover`: end the transition period off the file-era `.md` mailstore
//! ([#0040](../docs/tickets/0040-drop-file-layer-cutover.md), Stage 4 of the
//! data-access-layer plan).
//!
//! # What is actually left to do
//!
//! There is no file layer in the code to delete: the greenfield build never
//! had one, and [#0037] / [#0038] removed the legacy modules as they replaced
//! them. Two things survive the rewrite, and they are both *on disk*:
//!
//! 1. **Drafts.** They are the only local-only data in the product, so they
//!    are the only thing that does not come back from the server. The file-era
//!    build kept them at `<account_dir>/drafts/`, which is byte-for-byte the
//!    directory the store's drafts index reads today (`config::drafts_dir` is
//!    unchanged since the `pre-dal-nuke` tag). The "import" is therefore not a
//!    copy — a copy would be the one operation that *could* duplicate or lose
//!    a draft — it is the id assignment: a file-era draft has no `id:`
//!    frontmatter field, and identity is the `id:` field now (decision C of
//!    the DAL plan), so a draft without one is not addressable by a selector.
//!    [`import_drafts`] runs the ordinary index refresh, which mints an `id:`
//!    into any draft that lacks one and writes it back in place. Re-running is
//!    a no-op by construction: the second pass finds the field and keeps it.
//!
//! 2. **The file-era mailstore**: `<account_dir>/inbox/`, `archive/`, `sent/`
//!    and any other slugified mailbox directory full of `.md` files, plus the
//!    `mailbox-states.json` heuristic cache. Nothing in the build reads any of
//!    it. It is dead weight, and it is also the last carrier of the TKT-0047
//!    forgery surface (a sender-controlled attachment `.md` with a forged
//!    `method: REPLY`) — the walk that could be poisoned is already gone, but
//!    the files are still there.
//!
//! # Why this command never deletes anything
//!
//! Removing the file-era tree is a one-line `rm -rf` against a directory that
//! also holds live data (`drafts/`, `blobs/`, `store.sqlite3`), on a machine
//! whose only way back is the `pre-dal-nuke` tag plus whatever is on the
//! server. A bug in a predicate here costs real mail. So the command reports:
//! it prints exactly which directories are dead, how much they weigh, and the
//! command that removes them, and the human runs that command. That keeps the
//! irreversible step attached to a human decision and keeps this module's
//! blast radius to "wrote an `id:` field into a draft".
//!
//! [#0037]: ../docs/tickets/0037-sqlite-store-engine-skeleton.md
//! [#0038]: ../docs/tickets/0038-read-path-to-db.md

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use colored::*;

use crate::config::{account_dir, drafts_dir, store_path, AccountConfig, GlobalConfig};
use crate::store::drafts::{IdCollision, SkippedDraft};
use crate::store::{drafts as drafts_index, Store};

/// Directory names directly under an account directory that the *current*
/// build owns, and which are therefore never file-era remnants.
///
/// `attachments/` is on this list on purpose even though the file-era build
/// mirrored attachment `.md` files into it: `parse::stable_attachments_dir`
/// still writes there today (it is where a reply/forward materialises the
/// attachments it carries over), so the directory is shared between the two
/// eras and cannot be classified as dead by its name.
const LIVE_DIRS: &[&str] = &["attachments", "blobs", "drafts"];

/// Loose files under an account directory that only the file-era build wrote.
///
/// `contacts-cache.json` is *not* here: `contacts::cache` still reads it.
const LEGACY_FILES: &[&str] = &["mailbox-states.json"];

/// One dead file-era path under an account directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyRemnant {
    pub path: PathBuf,
    /// `.md` files below it, at any depth. `0` for `mailbox-states.json`.
    pub md_files: usize,
    /// Total bytes of every file below it.
    pub bytes: u64,
}

/// What [`import_drafts`] did to one account's drafts directory.
#[derive(Debug, Clone, Default)]
pub struct DraftImport {
    /// Drafts that had no `id:` and were given one by this run. Empty on the
    /// second run, which is what idempotence looks like from the outside.
    pub imported: Vec<PathBuf>,
    /// Drafts that already carried an `id:`.
    pub already_indexed: usize,
    /// Files the parser refused; they are left untouched and named.
    pub skipped: Vec<SkippedDraft>,
    /// Two files claiming one id: one of them is not addressable.
    pub collisions: Vec<IdCollision>,
}

/// The whole picture for one account.
#[derive(Debug, Clone)]
pub struct AccountCutover {
    pub account: String,
    pub drafts: DraftImport,
    pub remnants: Vec<LegacyRemnant>,
}

impl AccountCutover {
    /// Bytes the human would reclaim by removing every remnant.
    pub fn reclaimable_bytes(&self) -> u64 {
        self.remnants.iter().map(|r| r.bytes).sum()
    }
}

/// Drafts in `dir` that carry no usable `id:` frontmatter field, sorted.
///
/// This is the pre-image of the import: it is what [`import_drafts`] will
/// write to, and it is also the whole of `--dry-run`'s answer, so the dry run
/// and the real run cannot disagree about what would change.
pub fn drafts_missing_id(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        // A file that will not parse is not "missing an id": it is broken, and
        // the refresh reports it as skipped rather than writing to it.
        let Ok(draft) = crate::draft::parse_email_draft(&path) else {
            continue;
        };
        if draft
            .frontmatter
            .id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Give every file-era draft in `dir` an `id:` and put it in the index.
///
/// `dry_run` reports what would be minted and writes nothing at all — neither
/// the frontmatter field nor the index rows.
///
/// Idempotent and lossless by construction: no file is created, moved, renamed
/// or removed, and the only mutation is adding one `id:` line to a draft that
/// has none. Running it twice mints ids once.
pub fn import_drafts(
    store: &Store,
    account: &str,
    dir: &Path,
    dry_run: bool,
) -> Result<DraftImport> {
    let missing = drafts_missing_id(dir);
    if dry_run {
        let total = count_drafts(dir);
        return Ok(DraftImport {
            already_indexed: total.saturating_sub(missing.len()),
            imported: missing,
            skipped: Vec::new(),
            collisions: Vec::new(),
        });
    }
    let (rows, collisions, skipped) = drafts_index::refresh_reporting(store, account, dir)?;
    Ok(DraftImport {
        already_indexed: rows.len().saturating_sub(missing.len()),
        imported: missing,
        skipped,
        collisions,
    })
}

/// `.md` files directly in `dir`, whether or not they parse.
fn count_drafts(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let p = e.path();
            p.is_file() && p.extension().is_some_and(|x| x == "md")
        })
        .count()
}

/// Every dead file-era path under `account_dir`, sorted by path.
///
/// A directory counts as a remnant when it is not one the current build owns
/// ([`LIVE_DIRS`]) and it holds at least one `.md` file. The `.md` test is
/// what keeps this honest across the file era's slugified mailbox names (a
/// server folder called "Projekte" became `<account_dir>/projekte/`, and there
/// is no list of those anywhere): the file-era mailstore is *made of* `.md`
/// files, and nothing the current build writes outside [`LIVE_DIRS`] is.
///
/// Symlinked directories are not descended into and not reported: following
/// one would let a link inside the data directory put a path outside it on a
/// `rm -rf` line.
pub fn scan_legacy(account_dir: &Path) -> Vec<LegacyRemnant> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(account_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if LIVE_DIRS.contains(&name.as_str()) {
                continue;
            }
            let (md_files, bytes) = tree_size(&path);
            if md_files > 0 {
                out.push(LegacyRemnant { path, md_files, bytes });
            }
        } else if LEGACY_FILES.contains(&name.as_str()) {
            out.push(LegacyRemnant { path, md_files: 0, bytes: meta.len() });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// `(.md count, total bytes)` below `root`, not following symlinks.
fn tree_size(root: &Path) -> (usize, u64) {
    let mut md = 0usize;
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        bytes += meta.len();
        if entry.path().extension().is_some_and(|e| e == "md") {
            md += 1;
        }
    }
    (md, bytes)
}

/// Import one account's drafts and scan its directory for file-era remnants.
pub fn cutover_account(
    store: &Store,
    account: &str,
    account_dir: &Path,
    drafts_dir: &Path,
    dry_run: bool,
) -> Result<AccountCutover> {
    Ok(AccountCutover {
        account: account.to_string(),
        drafts: import_drafts(store, account, drafts_dir, dry_run)?,
        remnants: scan_legacy(account_dir),
    })
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `mp cutover [--account NAME] [--dry-run]`.
///
/// Reports; the only write is the `id:` field the drafts import adds, and
/// `--dry-run` suppresses that too.
pub fn handle_cutover(
    config: &GlobalConfig,
    account_name: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let accounts: Vec<&AccountConfig> = match account_name {
        Some(name) => vec![config
            .accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| anyhow!("no account named '{}'", name))?],
        None => {
            if config.accounts.is_empty() {
                return Err(anyhow!("no accounts configured"));
            }
            config.accounts.iter().collect()
        }
    };

    if dry_run {
        println!("{} dry run: nothing will be written", "ℹ".blue());
    }

    let mut reports = Vec::new();
    for account in accounts {
        let path = store_path(&account.name);
        let dir = account_dir(&account.name);
        println!("{} {}", "ℹ".blue(), account.name.yellow().bold());
        if !path.exists() {
            println!("  {} no store yet; run `mp sync` first", "•".blue());
            continue;
        }
        let store = Store::open(&path)?;
        let report = cutover_account(
            &store,
            &account.name,
            &dir,
            &drafts_dir(&account.name),
            dry_run,
        )?;

        let verb = if dry_run { "would get" } else { "got" };
        if report.drafts.imported.is_empty() {
            println!(
                "  {} drafts: nothing to import ({} already carry an id)",
                "✓".green(),
                report.drafts.already_indexed
            );
        } else {
            println!(
                "  {} drafts: {} {} an id: field ({} already carried one)",
                "✓".green(),
                report.drafts.imported.len().to_string().bold(),
                verb,
                report.drafts.already_indexed
            );
            for p in &report.drafts.imported {
                println!("      {}", p.display());
            }
        }
        for skip in &report.drafts.skipped {
            println!("  {} unreadable draft left untouched: {skip}", "!".yellow());
        }
        for collision in &report.drafts.collisions {
            println!("  {} {collision}", "!".yellow());
        }

        if report.remnants.is_empty() {
            println!("  {} no file-era mailstore left in {}", "✓".green(), dir.display());
        } else {
            println!(
                "  {} file-era mailstore still on disk ({}), unused by this build:",
                "•".blue(),
                human_bytes(report.reclaimable_bytes())
            );
            for remnant in &report.remnants {
                println!(
                    "      {}  ({} .md, {})",
                    remnant.path.display(),
                    remnant.md_files,
                    human_bytes(remnant.bytes)
                );
            }
        }
        reports.push(report);
    }

    let remnants: Vec<&LegacyRemnant> = reports.iter().flat_map(|r| &r.remnants).collect();
    if !remnants.is_empty() {
        println!();
        println!(
            "{} Nothing above is read by mailypoppins any more. It is safe to delete by hand,",
            "→".blue()
        );
        println!("  once you are happy the store has everything you want (mail comes back from");
        println!("  the server; drafts do not, and they are not in these directories):");
        println!();
        for remnant in &remnants {
            let flags = if remnant.path.is_dir() { "-rf" } else { "-f" };
            println!("    rm {flags} {}", shell_quote(&remnant.path));
        }
        println!("    rm -f ~/.local/bin/mp-legacy");
        println!();
        println!(
            "  {} deletes nothing itself, by design; re-run it afterwards to confirm.",
            "mp cutover".bold()
        );
    }
    Ok(())
}

/// Single-quote a path for the `rm -rf` line we print, so a space or a quote
/// in a slugified mailbox name cannot turn the suggestion into a different
/// command than the one shown.
fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c)) {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// An account directory with a drafts dir, plus its store.
    struct Fixture {
        _dir: TempDir,
        account_dir: PathBuf,
        drafts: PathBuf,
        store: Store,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let account_dir = dir.path().join("acct");
        let drafts = account_dir.join("drafts");
        fs::create_dir_all(&drafts).unwrap();
        let store = Store::open(account_dir.join("store.sqlite3")).unwrap();
        Fixture { _dir: dir, account_dir, drafts, store }
    }

    /// A file-era draft: no `id:` field anywhere in the frontmatter.
    fn legacy_draft(dir: &Path, name: &str, subject: &str) -> PathBuf {
        let path = dir.join(format!("{name}.md"));
        fs::write(
            &path,
            format!(
                "---\nto: a@example.com\ncc: null\nbcc: null\nsubject: {subject}\n\
                 status: draft\nfrom: me@example.com\nattachments: []\n---\n\nbody of {name}\n"
            ),
        )
        .unwrap();
        path
    }

    fn read_id(path: &Path) -> Option<String> {
        crate::draft::parse_email_draft(path).unwrap().frontmatter.id
    }

    #[test]
    fn import_mints_an_id_for_every_file_era_draft() {
        let fx = fixture();
        let a = legacy_draft(&fx.drafts, "one", "first");
        let b = legacy_draft(&fx.drafts, "two", "second");

        let report = import_drafts(&fx.store, "acct", &fx.drafts, false).unwrap();
        assert_eq!(report.imported, vec![a.clone(), b.clone()]);
        assert_eq!(report.already_indexed, 0);
        let id_a = read_id(&a).unwrap();
        let id_b = read_id(&b).unwrap();
        assert!(!id_a.is_empty() && id_a != id_b);

        // And each one resolves through the index the selector reads.
        assert!(drafts_index::find(&fx.store, "acct", &id_a).unwrap().is_some());
        assert!(drafts_index::find(&fx.store, "acct", &id_b).unwrap().is_some());
    }

    #[test]
    fn import_is_idempotent_and_never_duplicates_a_draft() {
        let fx = fixture();
        let a = legacy_draft(&fx.drafts, "one", "first");
        import_drafts(&fx.store, "acct", &fx.drafts, false).unwrap();
        let first_id = read_id(&a).unwrap();
        let bytes_after_first = fs::read_to_string(&a).unwrap();

        let second = import_drafts(&fx.store, "acct", &fx.drafts, false).unwrap();
        assert!(second.imported.is_empty(), "second run must mint nothing");
        assert_eq!(second.already_indexed, 1);
        assert_eq!(read_id(&a).unwrap(), first_id, "the id must not be reminted");
        assert_eq!(fs::read_to_string(&a).unwrap(), bytes_after_first);
        assert_eq!(count_drafts(&fx.drafts), 1, "no file was copied or duplicated");

        let rows = drafts_index::list(&fx.store, "acct", None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, first_id);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let fx = fixture();
        let a = legacy_draft(&fx.drafts, "one", "first");
        let before = fs::read_to_string(&a).unwrap();

        let report = import_drafts(&fx.store, "acct", &fx.drafts, true).unwrap();
        assert_eq!(report.imported, vec![a.clone()]);
        assert_eq!(fs::read_to_string(&a).unwrap(), before, "dry run touched the file");
        assert!(read_id(&a).is_none());
        assert!(drafts_index::list(&fx.store, "acct", None).unwrap().is_empty());

        // The real run then reports exactly what the dry run promised.
        let real = import_drafts(&fx.store, "acct", &fx.drafts, false).unwrap();
        assert_eq!(real.imported, report.imported);
    }

    #[test]
    fn a_draft_that_already_has_an_id_keeps_it() {
        let fx = fixture();
        let path = fx.drafts.join("kept.md");
        fs::write(
            &path,
            "---\nto: a@example.com\nsubject: kept\nstatus: draft\nfrom: me@example.com\n\
             id: deadbeefdeadbeef\n---\n\nbody\n",
        )
        .unwrap();
        let report = import_drafts(&fx.store, "acct", &fx.drafts, false).unwrap();
        assert!(report.imported.is_empty());
        assert_eq!(report.already_indexed, 1);
        assert_eq!(read_id(&path).as_deref(), Some("deadbeefdeadbeef"));
    }

    #[test]
    fn a_malformed_legacy_draft_is_reported_and_left_alone() {
        let fx = fixture();
        let good = legacy_draft(&fx.drafts, "good", "fine");
        let bad = fx.drafts.join("broken.md");
        fs::write(&bad, "---\nto: [unclosed\nsubject: nope\n---\nbody\n").unwrap();
        let bytes = fs::read_to_string(&bad).unwrap();

        let report = import_drafts(&fx.store, "acct", &fx.drafts, false).unwrap();
        assert_eq!(report.imported, vec![good], "the good draft still imports");
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path, bad);
        assert_eq!(fs::read_to_string(&bad).unwrap(), bytes, "the broken file was rewritten");
    }

    #[test]
    fn two_drafts_sharing_an_id_are_reported_not_swallowed() {
        let fx = fixture();
        for name in ["a", "b"] {
            fs::write(
                fx.drafts.join(format!("{name}.md")),
                "---\nto: a@example.com\nsubject: dup\nstatus: draft\nfrom: me@example.com\n\
                 id: cafecafecafecafe\n---\n\nbody\n",
            )
            .unwrap();
        }
        let report = import_drafts(&fx.store, "acct", &fx.drafts, false).unwrap();
        assert_eq!(report.collisions.len(), 1);
        assert_eq!(report.collisions[0].id, "cafecafecafecafe");
        assert_eq!(count_drafts(&fx.drafts), 2, "neither file was removed");
    }

    #[test]
    fn scan_finds_the_file_era_mailboxes_and_spares_the_live_dirs() {
        let fx = fixture();
        for mailbox in ["inbox", "archive", "sent", "projekte"] {
            let dir = fx.account_dir.join(mailbox);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("2026-01-01_x.md"), "---\nfrom: a\n---\nbody").unwrap();
        }
        // Live paths that must never be offered for deletion.
        fs::create_dir_all(fx.account_dir.join("blobs/ab")).unwrap();
        fs::write(fx.account_dir.join("blobs/ab/cd"), "blob").unwrap();
        fs::create_dir_all(fx.account_dir.join("attachments/msg")).unwrap();
        fs::write(fx.account_dir.join("attachments/msg/note.md"), "attached").unwrap();
        legacy_draft(&fx.drafts, "keep", "keep me");
        fs::write(fx.account_dir.join("contacts-cache.json"), "{}").unwrap();
        fs::write(fx.account_dir.join("mailbox-states.json"), "{}").unwrap();

        let names: Vec<String> = scan_legacy(&fx.account_dir)
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["archive", "inbox", "mailbox-states.json", "projekte", "sent"]
        );
    }

    #[test]
    fn scan_ignores_a_symlinked_directory() {
        let fx = fixture();
        let outside = fx.account_dir.parent().unwrap().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious.md"), "not ours").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, fx.account_dir.join("inbox")).unwrap();
        assert!(scan_legacy(&fx.account_dir).is_empty());
        assert!(outside.join("precious.md").exists());
    }

    #[test]
    fn scan_is_empty_on_a_clean_account() {
        let fx = fixture();
        legacy_draft(&fx.drafts, "one", "first");
        fs::create_dir_all(fx.account_dir.join("blobs")).unwrap();
        assert!(scan_legacy(&fx.account_dir).is_empty());
    }

    #[test]
    fn cutover_account_reports_both_halves() {
        let fx = fixture();
        legacy_draft(&fx.drafts, "one", "first");
        let inbox = fx.account_dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.md"), "x".repeat(100)).unwrap();

        let report =
            cutover_account(&fx.store, "acct", &fx.account_dir, &fx.drafts, false).unwrap();
        assert_eq!(report.account, "acct");
        assert_eq!(report.drafts.imported.len(), 1);
        assert_eq!(report.remnants.len(), 1);
        assert_eq!(report.reclaimable_bytes(), 100);
        // The remnant is still on disk: this command reports, it does not delete.
        assert!(inbox.join("a.md").exists());
    }

    #[test]
    fn shell_quote_protects_a_slugified_name_with_a_space() {
        assert_eq!(shell_quote(Path::new("/a/b_c-1.md")), "/a/b_c-1.md");
        assert_eq!(shell_quote(Path::new("/a/b c")), "'/a/b c'");
        assert_eq!(shell_quote(Path::new("/a/it's")), r"'/a/it'\''s'");
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
