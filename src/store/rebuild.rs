//! What survives a drop-and-rebuild (#0066).
//!
//! [`super::Store::open`] answers an unusable store file by deleting it and
//! creating an empty one, because `messages` and everything derived from it is
//! a cache the next sync refills. The `outbox` is the one table that is not:
//! it is the record of what has been submitted to a mail server, and
//! `mp outbox list|retry|discard` presents it as durable send state. So the
//! rebuild does three things beyond creating the file:
//!
//! 1. Before the drop, it reads the old `outbox` back defensively (by column
//!    name, tolerating a schema that is not the current one) and carries every
//!    unfinished row into the fresh file, together with a reference on the raw
//!    RFC822 blob it points at. `done` rows have nothing outstanding and are
//!    not carried.
//! 2. It then sweeps the blob tree, deleting every file the rebuilt store has
//!    no refcount row for. Without this, a rebuild leaves the whole blob
//!    directory orphaned: the files survive, the refcounts do not, and nothing
//!    ever reclaims them. The carried outbox rows are exactly what keeps their
//!    own bytes alive through the sweep.
//! 3. It writes a `store-rebuild-<timestamp>.txt` note next to the store when
//!    outbox rows were involved, so a discarded submission is never silent.
//!
//! `pending_ops` (#0039) is deliberately **not** carried, unlike the outbox it
//! otherwise mirrors. A pending mutation is a flag, move or delete of a message
//! the server still holds, so a queued-but-undrained row that a rebuild drops
//! costs at most a redone flag or a delayed move, never a lost message: the next
//! full sync restates the server's truth over the dropped optimistic write. The
//! outbox is carried because a submission to a mail server is a side effect no
//! sync can reconstruct; a pending mutation is not, so dropping it is safe and
//! the rebuild stays a cache rebuild for this table.
//!
//! The read is row by row on purpose. The file reaching this module has
//! usually failed an `integrity_check`, and a damaged page ends a SQLite scan
//! for good: `Rows::advance` resets the statement on a step error, so every
//! later `next()` says "no more rows" and a single scan would lose the whole
//! tail without noticing. So the salvage lists the rowids first and reads one
//! row per query, which costs one row per damaged page instead of all of
//! them. A listing that stopped early is itself a btree walk that stopped
//! early, and it carries no `ORDER BY`, so the positions it never named are
//! not simply the ones above the highest it reached: the salvage goes back
//! over every position between the first and `MAX(rowid)` that the listing did
//! not name, within the same budget, and whatever is still unreachable is
//! counted against `COUNT(*)` and named in the note (#0066 review follow-up).
//!
//! The sweep assumes the rebuild is the only thing touching the account
//! directory: a blob written by a concurrent ingest whose row has not
//! committed yet would be swept as an orphan. The cost is a refetch, which is
//! the same cost the rebuild itself pays. It walks `<account_dir>/blobs/` only
//! when that is a real directory: a symlinked blob root is left alone, both
//! because deleting through it would reach files the rebuild has no business
//! touching and because a user pointing `blobs/` at another disk deserves
//! their tree back intact.
//!
//! Nothing here is allowed to fail an open. A store that cannot be salvaged is
//! still a store; every error below is logged and stepped over.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{info, warn};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row};
use walkdir::WalkDir;

use super::blobs::{BlobHash, BlobStore};

/// Outbox states worth carrying: everything the send machine still owes an
/// answer for. `done` is complete by definition and stays behind.
const CARRIED_STATES: &[&str] = &["pending_send", "sent_pending_append", "failed"];

/// Where an unreadable state lands. A row whose state cannot be trusted must
/// not be re-submitted, and `failed` is the state that means "a human decides".
const UNREADABLE_STATE: &str = "failed";

/// How many outbox rows one salvage will read. A real outbox holds a handful
/// of rows at a time; a foreign or corrupt database that happens to have a
/// table named `outbox` can hold anything, and both the salvage and the note
/// file it feeds are held in memory.
const SALVAGE_LIMIT: usize = 10_000;

/// How much of one salvaged value goes into the note file. Every value here is
/// whatever the old file happened to hold, so a single oversized `message_id`
/// must not turn a note into a dump. Long enough that a real message-id, a
/// state and a blob hash all pass through untouched.
const NOTE_VALUE_LIMIT: usize = 200;

/// One `outbox` row read out of the file that is about to be deleted.
///
/// Every field is what the old file happened to hold, not what the current
/// schema promises: this is read from a database that just failed validation.
#[derive(Debug, Clone)]
pub(super) struct SalvagedRow {
    account: Option<String>,
    target_mailbox: Option<String>,
    message_id: Option<String>,
    raw_blob: Option<String>,
    state: Option<String>,
    attempts: i64,
    last_error: Option<String>,
    appended_uid: Option<i64>,
    created: Option<i64>,
    updated: Option<i64>,
    submission_started_at: Option<i64>,
    /// Set when the exactly-once marker column was there but held something
    /// that is not a timestamp. Absent and NULL are not this: they are the
    /// honest "the transport was never entered", which is what `None` above
    /// means to [`crate::outbox::sweep_pending_sends`]. Anything else is a
    /// value that was written and cannot be read, so whether the message
    /// reached SMTP is unknown and the row must be parked, never re-sent.
    marker_unreadable: bool,
    envelope: Option<String>,
}

impl SalvagedRow {
    /// How the row is named in a log line and in the note file.
    fn describe(&self) -> String {
        format!(
            "{} ({})",
            clip(self.message_id.as_deref().unwrap_or("<no message-id>")),
            clip(self.state.as_deref().unwrap_or("unreadable state"))
        )
    }
}

/// What the pre-drop read got out of the old file.
#[derive(Debug, Default)]
pub(super) struct Salvage {
    /// The unfinished rows, ready to be written into the fresh store.
    pub rows: Vec<SalvagedRow>,
    /// What the read itself could not get, in the shape
    /// [`RebuildReport::lost`] carries: a damaged page that ended the scan, a
    /// row that would not come back, a table too big to read in full. These
    /// travel into the note file alongside the rows that could not be written.
    pub lost: Vec<(String, String)>,
}

/// What a rebuild did, beyond creating an empty file.
#[derive(Debug, Default)]
pub(super) struct RebuildReport {
    /// Outbox rows carried into the fresh store, as `describe` strings.
    pub carried: Vec<String>,
    /// Outbox rows that could not be carried, each with its reason.
    pub lost: Vec<(String, String)>,
    pub swept_files: u64,
    pub swept_bytes: u64,
}

/// The blob root that belongs to a store file: `<account_dir>/blobs/`, the
/// sibling layout [`crate::config::blobs_dir`] lays down.
fn blobs_root(store_path: &Path) -> Option<PathBuf> {
    store_path.parent().map(|dir| dir.join("blobs"))
}

/// Read the unfinished `outbox` rows out of a store that failed validation.
///
/// Best effort by construction: the file may be corrupt, may be some other
/// database, may hold an `outbox` of an older shape. Every column is read by
/// name and a column that is absent or holds the wrong type yields `None`
/// rather than failing the row, because a partially readable submission is
/// still worth showing to a human.
pub(super) fn salvage_outbox(path: &Path) -> Salvage {
    let mut salvage = Salvage::default();
    let conn = match Connection::open(path) {
        Ok(conn) => conn,
        Err(e) => {
            warn!("[store] no outbox salvage from {}: {e}", path.display());
            return salvage;
        }
    };

    if let Err(e) = conn.prepare("SELECT * FROM outbox") {
        // The common case by far: the file is not one of ours, or is too
        // damaged to read a table list from.
        info!("[store] no outbox to salvage from {}: {e}", path.display());
        return salvage;
    }

    // What the old file says it holds, so a read that stops early can say how
    // much it never reached. Both can fail on a damaged file, which is why
    // both are optional and why neither gates the salvage.
    let claimed: Option<i64> = conn
        .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
        .ok();
    let last_rowid: Option<i64> = conn
        .query_row("SELECT MAX(rowid) FROM outbox", [], |r| r.get(0))
        .ok()
        .flatten();
    // Where the probe starts. A table whose live rowids all sit above the
    // salvage budget (an outbox that has drained and refilled for years) would
    // otherwise be probed from position 1 and recover nothing at all, because
    // the budget runs out long before the first row (#0066 review 2).
    let first_rowid: Option<i64> = conn
        .query_row("SELECT MIN(rowid) FROM outbox", [], |r| r.get(0))
        .ok()
        .flatten();

    match list_rowids(&conn) {
        Ok((ids, stopped)) => {
            let count = ids.len();
            let listed: HashSet<i64> = ids.iter().copied().collect();
            let reads = read_rows_by_id(&conn, ids, true, &mut salvage);
            if reads.missing > 0 {
                // The file named the position and then held no row there: a
                // page that still parses but has lost the cell, or a row
                // deleted between the two queries. Either way a row the file
                // itself claimed is gone, and a clean listing pushes no gap
                // entry, so this is the only place it can be named.
                salvage.lost.push((
                    format!(
                        "<{} row(s) the old file listed and then would not produce>",
                        reads.missing
                    ),
                    "the old file named the position and held no row there when it was read"
                        .to_string(),
                ));
            }
            if let Some(e) = stopped {
                // The listing walks a btree like anything else, so it stops at
                // the damaged page too. Every row it did not reach is still
                // addressable by position: each read seeks from the root, so
                // it reaches whatever the damage does not actually sit on.
                // Positions that hold no row answer "no rows" and cost
                // nothing. The listing is deliberately unordered, so what it
                // missed is not necessarily above the highest position it
                // reached: the probe covers the whole table minus the
                // positions already listed.
                let budget = SALVAGE_LIMIT.saturating_sub(count);
                let probed = probe_unlisted(
                    &conn,
                    &listed,
                    first_rowid,
                    last_rowid,
                    budget,
                    &mut salvage,
                );
                salvage.lost.push((
                    "<the outbox could not be read to the end>".to_string(),
                    gap_reason(&e, count, reads.read, &probed, claimed, last_rowid),
                ));
            } else if count >= SALVAGE_LIMIT {
                if let Some(reason) = truncation_reason(count, claimed) {
                    salvage.lost.push((
                        "<the outbox was too large to read in full>".to_string(),
                        reason,
                    ));
                }
            }
        }
        Err(e) => {
            // No usable rowids: an `outbox` that is a view, or WITHOUT ROWID.
            // Not a shape this store ever had, so fall back to the plain scan
            // and accept that a damaged page costs the tail rather than a row.
            info!(
                "[store] outbox rowids unavailable in {}: {e}",
                path.display()
            );
            scan_rows(&conn, claimed, &mut salvage);
        }
    }
    salvage
}

/// The rowids of the old `outbox`, with the error that ended the listing early
/// if one did.
///
/// The listing reads a btree like any other query, so it can hit the damaged
/// page too; a partial list is a normal outcome here, not a failure. `Err` is
/// only for a table that has no rowids to list at all.
fn list_rowids(conn: &Connection) -> Result<(Vec<i64>, Option<String>), rusqlite::Error> {
    // Deliberately unordered: `ORDER BY rowid` forces the table btree, while
    // the planner is free to satisfy this from the smaller `outbox_state`
    // index, which a page damaged in the table itself leaves intact.
    let mut stmt = conn.prepare(&format!("SELECT rowid FROM outbox LIMIT {SALVAGE_LIMIT}"))?;
    let mut rows = stmt.query([])?;
    let mut ids = Vec::new();
    let stopped = loop {
        match rows.next() {
            Ok(Some(row)) => match row.get::<_, i64>(0) {
                Ok(id) => ids.push(id),
                Err(e) => break Some(e.to_string()),
            },
            Ok(None) => break None,
            Err(e) => break Some(e.to_string()),
        }
    };
    ids.sort_unstable();
    Ok((ids, stopped))
}

/// What one pass of row-by-row reads got back.
#[derive(Debug, Default)]
struct Reads {
    /// Positions that answered with a row.
    read: usize,
    /// Positions that refused to be read.
    refused: usize,
    /// Positions that answered "no row". Only counted for a pass over
    /// positions the file itself listed, where an empty position is a row the
    /// file claimed and would not produce; past a damaged page an empty
    /// position is the ordinary answer and means nothing.
    missing: usize,
}

/// Read one row per query, so a damaged page costs that row and no other.
///
/// `name_each` says whether a refusal is named in the note one by one: true
/// for a position the file itself listed (a row was there and is gone), false
/// for a position only guessed at past a damaged page, where a refusal says
/// nothing about whether a row was ever there and is counted instead.
fn read_rows_by_id(
    conn: &Connection,
    ids: impl IntoIterator<Item = i64>,
    name_each: bool,
    salvage: &mut Salvage,
) -> Reads {
    let mut stmt = match conn.prepare("SELECT * FROM outbox WHERE rowid = ?1") {
        Ok(stmt) => stmt,
        Err(e) => {
            salvage.lost.push((
                "<the outbox could not be read row by row>".to_string(),
                format!("the old file refused the read: {e}"),
            ));
            return Reads::default();
        }
    };
    let mut reads = Reads::default();
    for id in ids {
        match stmt.query_row([id], |row| Ok(read_salvaged_row(row))) {
            // A finished row owes nothing and is not worth carrying.
            Ok(row) => {
                reads.read += 1;
                if row.state.as_deref() != Some("done") {
                    salvage.rows.push(row);
                }
            }
            // Gone between the listing and the read, or never there at all.
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                if name_each {
                    reads.missing += 1;
                }
            }
            Err(e) => {
                reads.refused += 1;
                if name_each {
                    warn!("[store] outbox row {id} could not be read out of the old file: {e}");
                    salvage.lost.push((
                        format!("<the row at position {id}>"),
                        format!("it could not be read out of the old file: {e}"),
                    ));
                }
            }
        }
    }
    reads
}

/// What reading position by position around a damaged page got back.
#[derive(Debug, Default)]
struct Probed {
    /// Positions that answered with a row.
    recovered: usize,
    /// Positions that refused to be read. A row may or may not have been there.
    refused: usize,
    /// The highest position tried, when any was.
    upto: Option<i64>,
    /// Set when the listing had already named every position the table holds,
    /// so there was nothing left to re-read and no gap to warn about.
    covered_everything: bool,
}

/// Read every position the listing did not name, one at a time, bounded by
/// what the table says its last position is and by whatever is left of the
/// salvage budget.
///
/// Not just the positions above the highest one listed: the listing carries no
/// `ORDER BY` on purpose, so the planner may serve it from the `outbox_state`
/// index, whose order is `(state, rowid)`. A listing that stopped mid-index
/// therefore leaves un-listed positions scattered below the highest one it did
/// reach, and those are rows too.
fn probe_unlisted(
    conn: &Connection,
    listed: &HashSet<i64>,
    first_rowid: Option<i64>,
    last_rowid: Option<i64>,
    budget: usize,
    salvage: &mut Salvage,
) -> Probed {
    let Some(last_rowid) = last_rowid else {
        // Without a last position there is no bounded range to walk.
        return Probed::default();
    };
    if budget == 0 {
        return Probed::default();
    }
    // The walk starts where the table says its rows start, so a budget of a
    // few thousand positions is spent on positions that can hold a row rather
    // than on the empty range below the first one. A `MIN(rowid)` the damaged
    // file would not answer falls back to 1, which is where this used to start
    // unconditionally, and a listing that named something lower still (nothing
    // stops a file from holding negative rowids) pulls the start down to it.
    let from = match (first_rowid, listed.iter().copied().min()) {
        (Some(first), Some(lowest_listed)) => first.min(lowest_listed),
        (Some(first), None) => first,
        (None, Some(lowest_listed)) => lowest_listed.min(1),
        (None, None) => 1,
    };
    if from > last_rowid {
        return Probed::default();
    }
    // `take` bounds the walk: the filter skips at most `listed.len()`
    // positions before yielding, so this costs at most one salvage budget plus
    // one listing however wide the range is.
    let ids: Vec<i64> = (from..=last_rowid)
        .filter(|id| !listed.contains(id))
        .take(budget)
        .collect();
    let Some(&upto) = ids.last() else {
        return Probed {
            covered_everything: true,
            ..Probed::default()
        };
    };
    let reads = read_rows_by_id(conn, ids, false, salvage);
    Probed {
        recovered: reads.read,
        refused: reads.refused,
        upto: Some(upto),
        covered_everything: false,
    }
}

/// The fallback read for a table with no rowids to address rows by.
///
/// One scan, so the first damaged page ends it; what that costs is counted
/// against `COUNT(*)` and named rather than dropped quietly. The
/// termination-on-error branch below is covered by
/// `a_damaged_rowidless_outbox_names_what_the_scan_could_not_reach`.
fn scan_rows(conn: &Connection, claimed: Option<i64>, salvage: &mut Salvage) {
    let mut stmt = match conn.prepare(&format!("SELECT * FROM outbox LIMIT {SALVAGE_LIMIT}")) {
        Ok(stmt) => stmt,
        Err(e) => {
            warn!("[store] outbox salvage failed: {e}");
            return;
        }
    };
    let mut rows = match stmt.query([]) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("[store] outbox salvage failed: {e}");
            return;
        }
    };
    let mut read = 0usize;
    let stopped = loop {
        match rows.next() {
            Ok(Some(row)) => {
                read += 1;
                let row = read_salvaged_row(row);
                if row.state.as_deref() != Some("done") {
                    salvage.rows.push(row);
                }
            }
            Ok(None) => break None,
            Err(e) => break Some(e.to_string()),
        }
    };
    if let Some(e) = stopped {
        salvage.lost.push((
            "<the outbox could not be read to the end>".to_string(),
            gap_reason(&e, read, read, &Probed::default(), claimed, None),
        ));
    } else if read >= SALVAGE_LIMIT {
        if let Some(reason) = truncation_reason(read, claimed) {
            salvage.lost.push((
                "<the outbox was too large to read in full>".to_string(),
                reason,
            ));
        }
    }
}

/// Say what a read that stopped early left behind, as precisely as the old
/// file still allows.
///
/// `listed` is how many positions the read named, `read` how many of them
/// actually answered with a row: a refusal is named on its own line in the
/// same note and must not also be counted as read here, or the two halves of
/// the note disagree.
fn gap_reason(
    error: &str,
    listed: usize,
    read: usize,
    probed: &Probed,
    claimed: Option<i64>,
    last_rowid: Option<i64>,
) -> String {
    let mut reason = format!("the old file stopped listing rows after {listed} of them ({error})");
    match probed.upto {
        Some(upto) => reason.push_str(&format!(
            "; reading position by position up to {upto} recovered {} more row(s) and left {} \
             position(s) unreadable",
            probed.recovered, probed.refused
        )),
        None if probed.covered_everything => reason.push_str(
            "; the listing had already named every position the table holds, so there was nothing \
             past it to re-read",
        ),
        None => reason.push_str("; everything past that point is gone"),
    }
    let read = read + probed.recovered;
    if let Some(claimed) = claimed {
        let missing = (claimed - read as i64).max(0);
        reason.push_str(&format!(
            "; the table said it held {claimed} row(s), so about {missing} were never read"
        ));
    } else if let Some(last) = last_rowid {
        reason.push_str(&format!(
            "; the table runs to position {last}, so how many rows that leaves unread could not \
             be established exactly"
        ));
    } else {
        reason.push_str("; how many rows that leaves unread could not be established");
    }
    reason
}

/// Say that the salvage stopped at its own bound rather than at the end, when
/// it actually did.
///
/// `None` when the table held no more rows than the bound: a table of exactly
/// [`SALVAGE_LIMIT`] rows is read whole, and the bound coinciding with the
/// last row is not a loss to warn about.
fn truncation_reason(read: usize, claimed: Option<i64>) -> Option<String> {
    match claimed {
        Some(claimed) if claimed <= read as i64 => None,
        Some(claimed) => Some(format!(
            "the table held {claimed} row(s) and a salvage reads at most {SALVAGE_LIMIT}, so \
             {} of them were left in the deleted file",
            claimed - read as i64
        )),
        // The count itself was unreadable, so the bound may or may not have
        // cost anything and the note says only what is certain.
        None => Some(format!(
            "a salvage reads at most {SALVAGE_LIMIT} row(s), and the table had at least that many, \
             so any beyond them were left in the deleted file"
        )),
    }
}

fn read_salvaged_row(row: &Row<'_>) -> SalvagedRow {
    let (marker, marker_unreadable) = read_marker(row);
    SalvagedRow {
        account: opt_string(row, "account"),
        target_mailbox: opt_string(row, "target_mailbox"),
        message_id: opt_string(row, "message_id"),
        raw_blob: opt_string(row, "raw_blob"),
        state: opt_string(row, "state"),
        attempts: opt_i64(row, "attempts").unwrap_or(0),
        last_error: opt_string(row, "last_error"),
        appended_uid: opt_i64(row, "appended_uid"),
        created: opt_i64(row, "created"),
        updated: opt_i64(row, "updated"),
        submission_started_at: marker,
        marker_unreadable,
        envelope: opt_string(row, "envelope"),
    }
}

/// Seconds since the epoch, for a salvaged row that carries no timestamps.
/// Local to this module so the store layer keeps depending on nothing above
/// it.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Cut a salvaged value down to what a note file can carry, on a character
/// boundary. Applied where a value reaches the note, never where it reaches
/// the rebuilt store: a carried row keeps every byte it had.
fn clip(value: &str) -> String {
    let mut cut = String::new();
    for (taken, c) in value.chars().enumerate() {
        if taken == NOTE_VALUE_LIMIT {
            cut.push_str("... (clipped)");
            return cut;
        }
        cut.push(c);
    }
    cut
}

/// A text column that may not exist and may not be text.
fn opt_string(row: &Row<'_>, name: &str) -> Option<String> {
    row.get::<_, Option<String>>(name)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// An integer column that may not exist and may not be an integer.
fn opt_i64(row: &Row<'_>, name: &str) -> Option<i64> {
    row.get::<_, Option<i64>>(name).ok().flatten()
}

/// The exactly-once marker, and whether it was there but unreadable.
///
/// `submission_started_at` is the one column where "could not read it" and
/// "it was empty" must not be the same answer: an empty marker tells
/// [`crate::outbox::sweep_pending_sends`] the transport was never entered, so
/// it hands the row back to SMTP. Hence the three-way read:
///
/// - no such column: the old file predates the marker (it was added mid-v2),
///   so it never recorded one for any row and there is nothing to distinguish
///   a mid-submission row from a queued one. Carried as empty, which is what
///   the code that wrote the file already assumed.
/// - NULL: the marker is genuinely empty. Carried as empty.
/// - anything else: something was written here and cannot be read as a
///   timestamp. The row is parked as `failed` by [`carried_state`], because a
///   message that may already have been submitted is never re-sent.
fn read_marker(row: &Row<'_>) -> (Option<i64>, bool) {
    match row.get_ref("submission_started_at") {
        Err(_) => (None, false),
        Ok(ValueRef::Null) => (None, false),
        Ok(ValueRef::Integer(v)) => (Some(v), false),
        Ok(_) => (None, true),
    }
}

/// Restore the salvaged rows into the fresh store and sweep the blob tree.
///
/// Returns what happened so the caller can log it and write the note file.
pub(super) fn finish(conn: &Connection, path: &Path, salvaged: Salvage) -> RebuildReport {
    // Whatever the read itself could not get is already a loss with a reason.
    let mut report = RebuildReport {
        lost: salvaged.lost,
        ..Default::default()
    };
    let Some(root) = blobs_root(path) else {
        // Unreachable for any real store path, and still not a place where a
        // submission may vanish without being named.
        for row in &salvaged.rows {
            report.lost.push((
                row.describe(),
                "the store path has no directory to hold a blob store".to_string(),
            ));
        }
        return report;
    };
    let blobs = BlobStore::new(root);

    for row in salvaged.rows {
        match restore_row(conn, &blobs, &row) {
            Ok(()) => report.carried.push(row.describe()),
            Err(reason) => report.lost.push((row.describe(), reason)),
        }
    }

    match sweep_orphan_blobs(conn, &blobs) {
        Ok((files, bytes)) => {
            report.swept_files = files;
            report.swept_bytes = bytes;
        }
        Err(e) => warn!(
            "[store] sweeping orphaned blobs under {} failed: {e:#}",
            blobs.root().display()
        ),
    }

    report
}

/// Write one salvaged row into the fresh `outbox`, with its blob reference.
///
/// The `Err` string is the reason the row could not be carried, phrased for
/// the note file a human reads.
fn restore_row(conn: &Connection, blobs: &BlobStore, row: &SalvagedRow) -> Result<(), String> {
    let account = row
        .account
        .clone()
        .ok_or_else(|| "the row named no account".to_string())?;
    let message_id = row
        .message_id
        .clone()
        .ok_or_else(|| "the row named no message-id".to_string())?;
    let raw_blob = row
        .raw_blob
        .as_deref()
        .ok_or_else(|| "the row named no raw message".to_string())?;
    let hash = BlobHash::parse(raw_blob).map_err(|_| {
        format!(
            "'{}' is not a blob hash, so the bytes are unreachable",
            clip(raw_blob)
        )
    })?;

    let size = fs::metadata(blobs.path_for(&hash))
        .map_err(|_| "the raw bytes are no longer in the blob store".to_string())?
        .len();

    let (state, last_error) = carried_state(row);

    let now = now_unix();
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("the rebuilt store refused a transaction: {e}"))?;
    tx.execute(
        "INSERT INTO outbox (account, target_mailbox, message_id, raw_blob, state, attempts,
                             last_error, appended_uid, created, updated, submission_started_at,
                             envelope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            account,
            row.target_mailbox,
            message_id,
            hash.as_str(),
            state,
            row.attempts,
            last_error,
            row.appended_uid,
            row.created.unwrap_or(now),
            row.updated.unwrap_or(now),
            row.submission_started_at,
            row.envelope,
        ],
    )
    .map_err(|e| format!("the rebuilt store refused the row: {e}"))?;
    blobs
        .acquire(&tx, &hash, size)
        .map_err(|e| format!("the rebuilt store refused the blob reference: {e:#}"))?;
    tx.commit()
        .map_err(|e| format!("the rebuilt store refused the commit: {e}"))?;
    Ok(())
}

/// The state a salvaged row lands in, and the note left on it.
///
/// Two things make a row unsafe to hand back to the send path: a state that is
/// not one of the schema's, and an exactly-once marker that was written and
/// cannot be read. Either parks the row as `failed`, the state that means "a
/// human decides", rather than as something a driver would re-submit.
fn carried_state(row: &SalvagedRow) -> (String, Option<String>) {
    let mut reasons: Vec<String> = Vec::new();
    let state = match row.state.as_deref() {
        Some(state) if CARRIED_STATES.contains(&state) => state.to_string(),
        other => {
            reasons.push(format!(
                "its state was unreadable ({})",
                clip(other.unwrap_or("none"))
            ));
            UNREADABLE_STATE.to_string()
        }
    };
    if row.marker_unreadable {
        reasons.push(
            "its submission marker was there but is not a timestamp, so whether the message \
             reached the mail server is unknown"
                .to_string(),
        );
    }
    if reasons.is_empty() {
        return (state, row.last_error.clone());
    }
    (
        UNREADABLE_STATE.to_string(),
        Some(format!(
            "carried across a store rebuild and parked for a human: {}",
            reasons.join("; ")
        )),
    )
}

/// Delete every file under the blob root that the store holds no refcount row
/// for, then prune the fan-out directories that emptied.
///
/// Matching is on the file's own name *and* its fan-out position, so a
/// leftover `.tmp` from an interrupted write and a blob sitting in the wrong
/// directory both go the way of the orphans.
fn sweep_orphan_blobs(conn: &Connection, blobs: &BlobStore) -> Result<(u64, u64)> {
    let root = blobs.root();
    // Deliberately `symlink_metadata`: `is_dir` and `WalkDir` both resolve a
    // symlinked root even with `follow_links(false)`, so a `blobs/` pointing
    // at the account directory would have the sweep delete the store file it
    // was just rebuilt into. A blob root on another disk is a layout this
    // never created and has no business emptying either.
    let Ok(meta) = fs::symlink_metadata(root) else {
        return Ok((0, 0));
    };
    if meta.file_type().is_symlink() {
        warn!(
            "[store] {} is a symlink, so the blob sweep was skipped; nothing under it was touched",
            root.display()
        );
        return Ok((0, 0));
    }
    if !meta.is_dir() {
        return Ok((0, 0));
    }

    let mut retained: HashSet<String> = HashSet::new();
    let mut stmt = conn
        .prepare("SELECT hash FROM blobs")
        .context("listing retained blobs")?;
    let hashes = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for hash in hashes {
        retained.insert(hash?);
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in WalkDir::new(root).contents_first(true).into_iter().flatten() {
        let path = entry.path();
        if entry.file_type().is_dir() {
            if path != root {
                // Only succeeds when the directory emptied out above.
                let _ = fs::remove_dir(path);
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(hash) = BlobHash::parse(&name) {
            if retained.contains(hash.as_str()) && blobs.path_for(&hash) == path {
                continue;
            }
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(path) {
            Ok(()) => {
                files += 1;
                bytes += size;
            }
            Err(e) => warn!("[store] could not remove orphaned blob {}: {e}", path.display()),
        }
    }
    Ok((files, bytes))
}

/// Write the human-readable note that says what the rebuild kept and what it
/// could not, next to the store file. Returns the path it wrote.
///
/// Only written when outbox rows were involved: a rebuild that touched nothing
/// a user submitted is a cache refill and needs no paperwork.
pub(super) fn write_notice(path: &Path, reason: &str, report: &RebuildReport) -> Option<PathBuf> {
    if report.carried.is_empty() && report.lost.is_empty() {
        return None;
    }
    let dir = path.parent()?;
    let now = chrono::Utc::now();
    // Millisecond granularity so two rebuilds of the same account in one
    // second leave two notes rather than one overwriting the other.
    let notice = dir.join(format!(
        "store-rebuild-{}.txt",
        now.format("%Y%m%dT%H%M%S%.3fZ")
    ));

    let mut text = format!(
        "mailypoppins rebuilt {} on {}.\n\n\
         Why: {reason}.\n\n\
         The store is a cache in front of the mail server, so the messages it held come back on \
         the next sync.\n\
         The outbox is not a cache, so its unfinished rows were carried across the rebuild.\n",
        path.display(),
        now.format("%Y-%m-%d %H:%M:%S UTC"),
    );
    if !report.carried.is_empty() {
        text.push_str("\nCarried into the rebuilt store, and listed by `mp outbox list`:\n");
        for row in &report.carried {
            text.push_str(&format!("  {row}\n"));
        }
    }
    if !report.lost.is_empty() {
        text.push_str("\nDiscarded, because they could not be carried:\n");
        for (row, why) in &report.lost {
            text.push_str(&format!("  {row}: {why}\n"));
        }
    }
    if report.swept_files > 0 {
        text.push_str(&format!(
            "\nOrphaned blob files removed: {} ({} bytes).\n",
            report.swept_files, report.swept_bytes
        ));
    }

    match fs::write(&notice, text) {
        Ok(()) => Some(notice),
        Err(e) => {
            warn!("[store] could not write {}: {e}", notice.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{schema, Store};
    use tempfile::{tempdir, TempDir};

    /// An account directory laid out the way the real one is: the store file
    /// and its `blobs/` sibling.
    fn account_dir() -> (TempDir, PathBuf, BlobStore) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let blobs = BlobStore::new(dir.path().join("blobs"));
        (dir, path, blobs)
    }

    fn blob_files(root: &Path) -> Vec<String> {
        if !root.exists() {
            return Vec::new();
        }
        let mut names: Vec<String> = WalkDir::new(root)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn notice_file(dir: &Path) -> Option<PathBuf> {
        fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("store-rebuild-"))
            })
    }

    /// Enqueue a submission the way `crate::outbox` does: blob first, then the
    /// row and its reference in one transaction.
    fn enqueue(store: &Store, blobs: &BlobStore, message_id: &str, state: &str, raw: &[u8]) {
        let hash = blobs.write(raw).unwrap();
        let tx = store.conn().unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO outbox (account, target_mailbox, message_id, raw_blob, state, attempts,
                                 created, updated, submission_started_at, envelope)
             VALUES ('alice', 'sent', ?1, ?2, ?3, 2, 100, 200, 300, 'from:alice@example.com')",
            rusqlite::params![message_id, hash.as_str(), state],
        )
        .unwrap();
        blobs.acquire(&tx, &hash, raw.len() as u64).unwrap();
        tx.commit().unwrap();
    }

    /// The columns of a carried row, so one assertion covers all of them.
    #[derive(Debug, PartialEq)]
    struct Carried {
        account: String,
        target_mailbox: Option<String>,
        state: String,
        attempts: i64,
        created: i64,
        updated: i64,
        submission_started_at: Option<i64>,
        envelope: Option<String>,
    }

    impl Carried {
        fn load(store: &Store, message_id: &str) -> Self {
            store
                .conn()
                .query_row(
                    "SELECT account, target_mailbox, state, attempts, created, updated,
                            submission_started_at, envelope
                     FROM outbox WHERE message_id = ?1",
                    [message_id],
                    |r| {
                        Ok(Self {
                            account: r.get(0)?,
                            target_mailbox: r.get(1)?,
                            state: r.get(2)?,
                            attempts: r.get(3)?,
                            created: r.get(4)?,
                            updated: r.get(5)?,
                            submission_started_at: r.get(6)?,
                            envelope: r.get(7)?,
                        })
                    },
                )
                .unwrap()
        }
    }

    /// Make the file unusable the way a schema bump does, which is the case
    /// #0066 was filed for.
    fn stamp_a_wrong_version(path: &Path) {
        let store = Store::open(path).unwrap();
        schema::set_meta(store.conn(), schema::META_SCHEMA_VERSION, "99").unwrap();
    }

    #[test]
    fn a_rebuild_carries_unfinished_outbox_rows_and_their_bytes() {
        let (dir, path, blobs) = account_dir();
        {
            let store = Store::open(&path).unwrap();
            enqueue(&store, &blobs, "<pending@example.com>", "pending_send", b"raw pending");
            enqueue(&store, &blobs, "<appending@example.com>", "sent_pending_append", b"raw appending");
            enqueue(&store, &blobs, "<parked@example.com>", "failed", b"raw parked");
            enqueue(&store, &blobs, "<finished@example.com>", "done", b"raw finished");
        }
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));

        let mut stmt = store
            .conn()
            .prepare("SELECT message_id FROM outbox ORDER BY message_id")
            .unwrap();
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![
                "<appending@example.com>",
                "<parked@example.com>",
                "<pending@example.com>"
            ],
            "the three unfinished rows survive and the done row does not"
        );

        assert_eq!(
            Carried::load(&store, "<pending@example.com>"),
            Carried {
                account: "alice".to_string(),
                target_mailbox: Some("sent".to_string()),
                state: "pending_send".to_string(),
                attempts: 2,
                created: 100,
                updated: 200,
                submission_started_at: Some(300),
                envelope: Some("from:alice@example.com".to_string()),
            },
            "every column of a carried row comes across, the exactly-once marker included"
        );

        // Their bytes survive with a fresh reference; the done row's do not.
        assert_eq!(
            blob_files(blobs.root()).len(),
            3,
            "only the carried rows' blobs are kept"
        );
        for raw in [&b"raw pending"[..], b"raw appending", b"raw parked"] {
            let hash = BlobHash::of(raw);
            assert!(blobs.contains(&hash), "carried bytes must survive the sweep");
            assert_eq!(super::super::blobs::refcount(store.conn(), &hash).unwrap(), 1);
        }
        assert!(
            !blobs.contains(&BlobHash::of(b"raw finished")),
            "a done row's bytes are swept with the rest"
        );

        let notice = notice_file(dir.path()).expect("a rebuild touching the outbox writes a note");
        let text = fs::read_to_string(notice).unwrap();
        assert!(text.contains("<pending@example.com> (pending_send)"), "{text}");
        assert!(text.contains("Carried into the rebuilt store"), "{text}");
    }

    #[test]
    fn a_rebuild_leaves_no_orphaned_blob_files() {
        let (dir, path, blobs) = account_dir();
        {
            let store = Store::open(&path).unwrap();
            let raw = b"a message body";
            let hash = blobs.write(raw).unwrap();
            store
                .conn()
                .execute(
                    "INSERT INTO messages (account, mailbox, uid, message_id, raw_blob)
                     VALUES ('alice', 'inbox', 1, '<m@example.com>', ?1)",
                    [hash.as_str()],
                )
                .unwrap();
            blobs.acquire(store.conn(), &hash, raw.len() as u64).unwrap();
            // An interrupted write leaves a temp sibling; it is orphaned too.
            let dir = blobs.path_for(&hash).parent().unwrap().to_path_buf();
            fs::write(dir.join(format!(".{hash}.tmp.4242.0")), b"half").unwrap();
        }
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        assert!(
            blob_files(blobs.root()).is_empty(),
            "a rebuilt store must not leave blob files nothing references"
        );
        let blob_rows: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blob_rows, 0);
        assert!(
            notice_file(dir.path()).is_none(),
            "a rebuild that touched no outbox row needs no note"
        );
    }

    #[test]
    fn a_row_whose_bytes_are_gone_is_named_rather_than_dropped_silently() {
        let (dir, path, blobs) = account_dir();
        {
            let store = Store::open(&path).unwrap();
            enqueue(&store, &blobs, "<orphan@example.com>", "pending_send", b"raw orphan");
        }
        // Retention, a manual cleanup, a half-restored backup: the row points
        // at bytes that are not there any more.
        fs::remove_file(blobs.path_for(&BlobHash::of(b"raw orphan"))).unwrap();
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        let rows: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "a row with no bytes cannot be carried");

        let notice = notice_file(dir.path()).expect("a discarded row must leave a note");
        let text = fs::read_to_string(notice).unwrap();
        assert!(text.contains("Discarded, because they could not be carried"), "{text}");
        assert!(text.contains("<orphan@example.com> (pending_send)"), "{text}");
        assert!(text.contains("no longer in the blob store"), "{text}");
    }

    #[test]
    fn salvage_reads_an_outbox_of_an_older_shape() {
        let (_dir, path, blobs) = account_dir();
        let raw = b"raw from an older schema";
        let hash = blobs.write(raw).unwrap();

        // A v3-era file: fewer columns, and none of the ones added since.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL,
                     attempts   INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '3')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO outbox (account, message_id, raw_blob, state, attempts)
                 VALUES ('alice', '<old@example.com>', ?1, 'pending_send', 1)",
                [hash.as_str()],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let (message_id, state, attempts, envelope, marker): (
            String,
            String,
            i64,
            Option<String>,
            Option<i64>,
        ) = store
            .conn()
            .query_row(
                "SELECT message_id, state, attempts, envelope, submission_started_at FROM outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(message_id, "<old@example.com>");
        assert_eq!(state, "pending_send");
        assert_eq!(attempts, 1);
        assert_eq!(envelope, None, "a column the old file never had is empty");
        // An outbox that predates the marker column recorded one for no row,
        // so nothing in it distinguishes a queued submission from one that was
        // inside an SMTP session. Carried as empty, which is the assumption
        // the code that wrote that file already made; the alternative parks
        // every queued mail of every pre-marker store on a path every such
        // store takes exactly once. A marker that is *there* and unreadable is
        // the other case, and parks: see the test below.
        assert_eq!(marker, None, "and so is the exactly-once marker");
        assert!(blobs.contains(&hash), "its bytes survive the sweep");
    }

    /// A marker column holding something that is not a timestamp: the row may
    /// have been inside an SMTP session, and nothing can tell. TEXT and REAL
    /// are the two shapes a damaged page or an older writer can leave there.
    #[test]
    fn a_marker_that_is_not_a_timestamp_parks_the_row_instead_of_re_sending_it() {
        let (_dir, path, blobs) = account_dir();
        let text_hash = blobs.write(b"raw with a text marker").unwrap();
        let real_hash = blobs.write(b"raw with a real marker").unwrap();
        let null_hash = blobs.write(b"raw with no marker at all").unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL,
                     submission_started_at
                 );",
            )
            .unwrap();
            let insert = |id: &str, hash: &BlobHash, marker: &dyn rusqlite::ToSql| {
                conn.execute(
                    "INSERT INTO outbox (account, message_id, raw_blob, state,
                                         submission_started_at)
                     VALUES ('alice', ?1, ?2, 'pending_send', ?3)",
                    rusqlite::params![id, hash.as_str(), marker],
                )
                .unwrap();
            };
            insert("<text@example.com>", &text_hash, &"300");
            insert("<real@example.com>", &real_hash, &300.5f64);
            insert("<null@example.com>", &null_hash, &rusqlite::types::Null);
        }

        let store = Store::open(&path).unwrap();
        let row = |id: &str| -> (String, Option<String>, Option<i64>) {
            store
                .conn()
                .query_row(
                    "SELECT state, last_error, submission_started_at FROM outbox
                     WHERE message_id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        };

        for id in ["<text@example.com>", "<real@example.com>"] {
            let (state, last_error, marker) = row(id);
            assert_eq!(
                state, "failed",
                "{id}: a marker that cannot be read may mean the message was already submitted"
            );
            assert!(
                last_error.unwrap().contains("submission marker"),
                "{id}: the row says why a human has to decide"
            );
            assert_eq!(marker, None, "{id}: nothing readable to carry");
        }

        let (state, last_error, marker) = row("<null@example.com>");
        assert_eq!(
            state, "pending_send",
            "a genuinely empty marker still means the transport was never entered"
        );
        assert_eq!(last_error, None);
        assert_eq!(marker, None);
    }

    /// `blobs/` pointing somewhere else: the sweep must not delete through it.
    /// Probed on the review of #0066 with `blobs -> .`, which deleted the
    /// freshly rebuilt store file from under its own open handle.
    #[test]
    fn a_symlinked_blob_root_is_left_alone_rather_than_swept_through() {
        let dir = tempdir().unwrap();
        let account = dir.path().join("account");
        fs::create_dir_all(&account).unwrap();
        let path = account.join("store.sqlite3");

        // Somewhere else entirely, holding a file no rebuild may touch.
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("precious.txt"), b"not the store's to delete").unwrap();

        {
            let store = Store::open(&path).unwrap();
            drop(store);
        }
        std::os::unix::fs::symlink(&elsewhere, account.join("blobs")).unwrap();
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));
        assert!(
            elsewhere.join("precious.txt").exists(),
            "a symlinked blob root is not the rebuild's to empty"
        );
        assert!(path.exists(), "and the rebuilt store file survives its own sweep");
    }

    /// A file that is not ours but happens to hold a table named `outbox`, in
    /// numbers no real outbox reaches: the salvage stops at its own bound and
    /// says so rather than reading gigabytes into a note file.
    #[test]
    fn an_outbox_too_large_to_read_in_full_says_so() {
        let (dir, path, _blobs) = account_dir();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL
                 );",
            )
            .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO outbox (account, message_id, raw_blob, state)
                         VALUES ('alice', ?1, 'nothing', 'done')",
                    )
                    .unwrap();
                for i in 0..(SALVAGE_LIMIT + 1) {
                    stmt.execute([format!("<{i}@example.com>")]).unwrap();
                }
            }
            tx.commit().unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));
        let notice = notice_file(dir.path()).expect("a truncated salvage is not a silent one");
        let text = fs::read_to_string(notice).unwrap();
        assert!(text.contains("too large to read in full"), "{text}");
        assert!(
            text.contains(&(SALVAGE_LIMIT + 1).to_string()),
            "the note counts what the table held: {text}"
        );
        assert!(
            text.len() < 4096,
            "the note stays a note, not a dump: {} bytes",
            text.len()
        );
    }

    #[test]
    fn an_unreadable_state_is_parked_for_a_human_rather_than_re_sent() {
        let (_dir, path, blobs) = account_dir();
        let raw = b"raw with a bad state";
        let hash = blobs.write(raw).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO outbox (account, message_id, raw_blob, state)
                 VALUES ('alice', '<weird@example.com>', ?1, 'almost_sent')",
                [hash.as_str()],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let (state, last_error): (String, Option<String>) = store
            .conn()
            .query_row("SELECT state, last_error FROM outbox", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(state, "failed", "an unknown state must never be re-submitted");
        assert!(
            last_error.unwrap().contains("almost_sent"),
            "the note on the row says where it came from"
        );
    }

    /// A table with no positions to address rows by. Not a shape this store
    /// ever had, so the scan the salvage falls back to is allowed to be the
    /// weaker read; it still has to carry what it can reach.
    #[test]
    fn an_outbox_with_no_rowids_falls_back_to_a_plain_scan() {
        let (_dir, path, blobs) = account_dir();
        let hash = blobs.write(b"raw from a rowidless table").unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL PRIMARY KEY,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO outbox (account, message_id, raw_blob, state)
                 VALUES ('alice', '<rowidless@example.com>', ?1, 'pending_send')",
                [hash.as_str()],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let (message_id, state): (String, String) = store
            .conn()
            .query_row("SELECT message_id, state FROM outbox", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(message_id, "<rowidless@example.com>");
        assert_eq!(state, "pending_send");
        assert!(blobs.contains(&hash), "its bytes survive the sweep");
    }

    /// The case the row-by-row read exists for: a page in the middle of the
    /// file is gone. A single `SELECT * FROM outbox` ends for good at the
    /// damaged page (`Rows::advance` resets the statement on a step error), so
    /// before the #0066 review follow-up everything past it vanished with the
    /// note reporting nothing discarded at all.
    #[test]
    fn a_damaged_page_costs_rows_but_is_never_silent_about_them() {
        const ROWS: usize = 400;
        let (dir, path, blobs) = account_dir();
        {
            // Small pages, so 4 KB of damage lands squarely in the middle of
            // the table rather than taking the whole file with it. Built by
            // hand because `page_size` can only be set before the first table
            // and the store opens WAL.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA page_size = 512;
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id             INTEGER PRIMARY KEY,
                     account        TEXT NOT NULL,
                     target_mailbox TEXT,
                     message_id     TEXT NOT NULL,
                     raw_blob       TEXT NOT NULL,
                     state          TEXT NOT NULL,
                     attempts       INTEGER NOT NULL DEFAULT 0,
                     last_error     TEXT,
                     appended_uid   INTEGER,
                     created        INTEGER,
                     updated        INTEGER,
                     submission_started_at INTEGER,
                     envelope       TEXT
                 );
                 CREATE INDEX outbox_state ON outbox (state);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [schema::SCHEMA_VERSION.to_string()],
            )
            .unwrap();
            for i in 0..ROWS {
                let hash = blobs.write(format!("raw message {i}").as_bytes()).unwrap();
                conn.execute(
                    "INSERT INTO outbox (account, message_id, raw_blob, state, created, updated)
                     VALUES ('alice', ?1, ?2, 'pending_send', 100, 200)",
                    rusqlite::params![format!("<{i}@example.com>"), hash.as_str()],
                )
                .unwrap();
            }
        }

        // Zero 4 KB in the middle of the file, page-aligned, well clear of the
        // header and the schema on page 1.
        {
            use std::io::{Seek, SeekFrom, Write};
            let len = fs::metadata(&path).unwrap().len();
            assert!(len > 16 * 1024, "the probe needs a file with a middle");
            let at = (len / 2) / 512 * 512;
            let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(at)).unwrap();
            file.write_all(&[0u8; 4096]).unwrap();
            file.sync_all().unwrap();
        }

        let store = Store::open(&path).unwrap();
        let carried: usize = store
            .conn()
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap() as usize;
        let notice = notice_file(dir.path()).expect("a rebuild that lost submissions writes a note");
        let text = fs::read_to_string(notice).unwrap();

        assert!(
            carried < ROWS,
            "the probe is only meaningful if the damage cost something"
        );
        assert!(
            text.contains("Discarded, because they could not be carried"),
            "the loss must be named, not counted as zero: {text}"
        );
        assert!(
            text.contains("stopped listing rows after"),
            "the note has to say the read did not reach the end: {text}"
        );
        assert!(
            text.contains("unreadable") || text.contains("never read"),
            "and has to quantify what that cost: {text}"
        );
        // The whole point of reading by position: one damaged page costs the
        // rows it holds, not every row behind it. A single scan stopped dead
        // at the damage and carried 196 of these 400 when this was measured.
        assert!(
            carried > ROWS / 2,
            "a damaged page must not cost the tail of the table: carried {carried} of {ROWS}"
        );
    }

    /// A file that lists positions and then produces no row at them: a page
    /// that still parses but has lost the cell, a row deleted between the two
    /// queries, or (as here) a table shape that is not a table at all. The
    /// listing completes cleanly, so nothing else in the salvage would ever
    /// mention the rows it named.
    #[test]
    fn a_position_the_old_file_lists_and_will_not_produce_is_named_in_the_note() {
        let (dir, path, _blobs) = account_dir();
        {
            let conn = Connection::open(&path).unwrap();
            // `random()` is re-evaluated per query, so every position the
            // listing hands out is one the row-by-row read cannot find again.
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE base (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL
                 );
                 INSERT INTO base (account, message_id, raw_blob, state)
                 VALUES ('alice', '<one@example.com>', 'nothing', 'pending_send'),
                        ('alice', '<two@example.com>', 'nothing', 'pending_send');
                 CREATE VIEW outbox AS
                     SELECT random() AS rowid, account, message_id, raw_blob, state FROM base;",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));
        let notice =
            notice_file(dir.path()).expect("a row the file listed and would not produce is a loss");
        let text = fs::read_to_string(notice).unwrap();
        assert!(
            text.contains("<2 row(s) the old file listed and then would not produce>"),
            "the note has to count them rather than drop them: {text}"
        );
    }

    /// The top of the rowid range. Nothing in the salvage may fail an open,
    /// and arithmetic on a position is where that is easiest to lose: `id + 1`
    /// on `i64::MAX` panicked `Store::open` in any build with overflow checks.
    #[test]
    fn a_row_at_the_top_of_the_position_range_does_not_fail_the_open() {
        let (_dir, path, blobs) = account_dir();
        let hash = blobs.write(b"raw at the top of the range").unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO outbox (id, account, message_id, raw_blob, state)
                 VALUES (?1, 'alice', '<topmost@example.com>', ?2, 'pending_send')",
                rusqlite::params![i64::MAX, hash.as_str()],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let message_id: String = store
            .conn()
            .query_row("SELECT message_id FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(message_id, "<topmost@example.com>");
    }

    /// A table of exactly the salvage bound is read whole. The bound landing
    /// on the last row is not a truncation, and warning about rows "left in
    /// the deleted file" when there were none is a false alarm.
    #[test]
    fn an_outbox_of_exactly_the_salvage_bound_is_not_reported_as_truncated() {
        let (dir, path, _blobs) = account_dir();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     id         INTEGER PRIMARY KEY,
                     account    TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL
                 );",
            )
            .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO outbox (account, message_id, raw_blob, state)
                         VALUES ('alice', ?1, 'nothing', 'done')",
                    )
                    .unwrap();
                for i in 0..SALVAGE_LIMIT {
                    stmt.execute([format!("<{i}@example.com>")]).unwrap();
                }
            }
            tx.commit().unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));
        assert!(
            notice_file(dir.path()).is_none(),
            "every row was read, so there is nothing to warn about"
        );
    }

    /// The note is a note. A salvaged value is whatever the old file held, so
    /// what reaches the note is clipped; what reaches the rebuilt store is not.
    #[test]
    fn an_oversized_value_is_clipped_in_the_note_and_kept_in_the_store() {
        let (dir, path, blobs) = account_dir();
        let message_id = format!("<{}@example.com>", "x".repeat(8192));
        {
            let store = Store::open(&path).unwrap();
            enqueue(&store, &blobs, &message_id, "pending_send", b"raw oversized");
        }
        stamp_a_wrong_version(&path);

        let store = Store::open(&path).unwrap();
        let carried: String = store
            .conn()
            .query_row("SELECT message_id FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(carried, message_id, "the row itself keeps every byte it had");

        let notice = notice_file(dir.path()).expect("a carried row leaves a note");
        let text = fs::read_to_string(notice).unwrap();
        assert!(text.contains("... (clipped)"), "{text}");
        assert!(
            text.len() < 1024,
            "the note stays a note, not a dump: {} bytes",
            text.len()
        );
    }

    /// The listing carries no `ORDER BY` on purpose, so one that stopped early
    /// may have skipped positions *below* the highest it reached. Those hold
    /// rows like any other and the probe has to go back for them.
    #[test]
    fn the_probe_goes_back_for_positions_the_listing_skipped() {
        let (_dir, path, blobs) = account_dir();
        let store = Store::open(&path).unwrap();
        for i in 1..=5 {
            enqueue(
                &store,
                &blobs,
                &format!("<{i}@example.com>"),
                "pending_send",
                format!("raw message {i}").as_bytes(),
            );
        }

        // A listing that named positions 1, 4 and 5 and stopped without ever
        // naming 2 and 3: what an index-ordered listing leaves behind.
        let listed: HashSet<i64> = [1, 4, 5].into_iter().collect();
        let mut salvage = Salvage::default();
        let probed = probe_unlisted(
            store.conn(),
            &listed,
            Some(1),
            Some(5),
            SALVAGE_LIMIT,
            &mut salvage,
        );
        assert_eq!(probed.recovered, 2, "the skipped positions hold rows");
        assert_eq!(probed.refused, 0);
        assert_eq!(probed.upto, Some(3));
        let mut recovered: Vec<String> = salvage
            .rows
            .iter()
            .filter_map(|r| r.message_id.clone())
            .collect();
        recovered.sort();
        assert_eq!(recovered, vec!["<2@example.com>", "<3@example.com>"]);

        // And a listing that did name every position leaves nothing to probe.
        let all: HashSet<i64> = (1..=5).collect();
        let probed = probe_unlisted(
            store.conn(),
            &all,
            Some(1),
            Some(5),
            SALVAGE_LIMIT,
            &mut Salvage::default(),
        );
        assert!(probed.covered_everything);
        assert_eq!(probed.upto, None);
    }

    /// #0066 review 2: a long-lived outbox has drained and refilled many
    /// times, so every live row sits at a rowid far above the salvage budget.
    /// Probing from position 1 spends the whole budget on the empty range
    /// below the first row and recovers nothing.
    #[test]
    fn the_probe_starts_where_the_rows_are_not_at_position_one() {
        let (_dir, path, blobs) = account_dir();
        let store = Store::open(&path).unwrap();
        for i in 1..=3 {
            enqueue(
                &store,
                &blobs,
                &format!("<{i}@example.com>"),
                "pending_send",
                format!("raw message {i}").as_bytes(),
            );
        }
        // Where an outbox that has been draining for years keeps its rows.
        store
            .conn()
            .execute("UPDATE outbox SET id = id + 10000", [])
            .unwrap();

        let budget = 3;
        let listed = HashSet::new();
        let mut salvage = Salvage::default();
        let probed = probe_unlisted(
            store.conn(),
            &listed,
            Some(10001),
            Some(10003),
            budget,
            &mut salvage,
        );
        assert_eq!(probed.recovered, 3, "the budget lands on the live rows");
        assert_eq!(salvage.rows.len(), 3);

        // What it used to do, and what a `MIN(rowid)` the damaged file will
        // not answer still falls back to.
        let probed = probe_unlisted(
            store.conn(),
            &listed,
            None,
            Some(10003),
            budget,
            &mut Salvage::default(),
        );
        assert_eq!(probed.recovered, 0);
    }

    /// The two halves of a gap note have to agree: a row named as unreadable a
    /// few lines above is not also a row that was read, and a read that
    /// reached the end of the table does not leave a gap past it.
    #[test]
    fn the_gap_note_counts_refused_rows_as_unread_and_claims_no_gap_it_has_not_found() {
        let refused_two = gap_reason(
            "database disk image is malformed",
            5,
            3,
            &Probed::default(),
            Some(5),
            None,
        );
        assert!(
            refused_two.contains("stopped listing rows after 5 of them"),
            "{refused_two}"
        );
        assert!(
            refused_two.contains("about 2 were never read"),
            "a position that refused to be read is not a row that was read: {refused_two}"
        );

        let nothing_past = gap_reason(
            "database disk image is malformed",
            5,
            5,
            &Probed {
                covered_everything: true,
                ..Probed::default()
            },
            Some(5),
            Some(5),
        );
        assert!(
            !nothing_past.contains("everything past that point is gone"),
            "there is no gap past the last position: {nothing_past}"
        );
        assert!(
            nothing_past.contains("about 0 were never read"),
            "{nothing_past}"
        );
    }

    /// The fallback scan's own termination-on-error branch: a `WITHOUT ROWID`
    /// table has no positions to read one at a time, so the salvage falls back
    /// to a single scan, and the first damaged page ends it for good. What
    /// that costs still has to be named rather than counted as zero.
    #[test]
    fn a_damaged_rowidless_outbox_names_what_the_scan_could_not_reach() {
        const ROWS: usize = 400;
        let (dir, path, blobs) = account_dir();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA page_size = 512;
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE outbox (
                     message_id TEXT NOT NULL PRIMARY KEY,
                     account    TEXT NOT NULL,
                     raw_blob   TEXT NOT NULL,
                     state      TEXT NOT NULL
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [schema::SCHEMA_VERSION.to_string()],
            )
            .unwrap();
            for i in 0..ROWS {
                let hash = blobs.write(format!("raw message {i}").as_bytes()).unwrap();
                conn.execute(
                    "INSERT INTO outbox (account, message_id, raw_blob, state)
                     VALUES ('alice', ?1, ?2, 'pending_send')",
                    rusqlite::params![format!("<{i:04}@example.com>"), hash.as_str()],
                )
                .unwrap();
            }
        }

        // Same recipe as the rowid case: zero 4 KB in the middle of the file,
        // page-aligned, well clear of the header and the schema on page 1.
        {
            use std::io::{Seek, SeekFrom, Write};
            let len = fs::metadata(&path).unwrap().len();
            assert!(len > 16 * 1024, "the probe needs a file with a middle");
            let at = (len / 2) / 512 * 512;
            let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(at)).unwrap();
            file.write_all(&[0u8; 4096]).unwrap();
            file.sync_all().unwrap();
        }

        let store = Store::open(&path).unwrap();
        let carried: usize = store
            .conn()
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap() as usize;
        assert!(carried < ROWS, "the probe is only meaningful if the damage cost something");
        let notice = notice_file(dir.path()).expect("a rebuild that lost submissions writes a note");
        let text = fs::read_to_string(notice).unwrap();
        assert!(
            text.contains("Discarded, because they could not be carried"),
            "the loss must be named, not counted as zero: {text}"
        );
        assert!(
            text.contains("stopped listing rows after"),
            "the note has to say the scan did not reach the end: {text}"
        );
        assert!(
            text.contains("never read") || text.contains("could not be established"),
            "and has to say what that left unread: {text}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_database_salvages_nothing_and_still_rebuilds() {
        let (dir, path, blobs) = account_dir();
        blobs.write(b"an orphan from the dead file").unwrap();
        fs::write(&path, b"not a database, just bytes").unwrap();

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(schema::SCHEMA_VERSION));
        assert!(blob_files(blobs.root()).is_empty(), "the orphan is swept");
        assert!(notice_file(dir.path()).is_none());
    }
}
