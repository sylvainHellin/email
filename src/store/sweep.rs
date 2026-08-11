//! The retention sweep: the one code path in mailypoppins that deletes user
//! data (#0060).
//!
//! The local store is a cache in front of the server, so evicting a blob is not
//! deletion in the durable sense: the `messages` row stays, the listing stays
//! complete, and the bytes are re-materialised by a re-ingest of the same UID
//! (see [`docs/tickets/0060-retention-enforcement.md`] and the interim note in
//! [`crate::config`] at the `RetentionPolicy` docs). Message rows are *never*
//! touched here; only blob files and their `blobs` refcount rows go.
//!
//! ## Marker mechanics (the two-strike rule)
//!
//! A sweep over an over-cap store does not evict on the first breach. It writes
//! a persisted, store-level marker (`meta[`[`RETENTION_OVER_CAP_MARKER`]`]`) and
//! warns. The *next* over-cap sweep sees the marker and evicts. Dropping back
//! under the cap clears the marker, so a store that briefly spikes and recovers
//! never loses a byte. The marker lives in `meta`, not in process memory, so the
//! "first breach warns, second evicts" contract holds across separate `mp`
//! invocations (a sync that warns, then a later `mp store gc` that evicts).
//!
//! ## Eviction order
//!
//! When a sweep does evict, victims are taken in this priority, oldest-first
//! within each group, stopping the moment the store is back under the cap:
//!
//! 1. blobs past their age horizon (attachments past `attachment_horizon_days`,
//!    then bodies past `body_horizon_days`), if the horizon is configured;
//! 2. attachment blobs, oldest-first;
//! 3. body blobs, oldest-first.
//!
//! "Age" is the freshest referencing message's `date_sort`: a blob is as recent
//! as its most recent use, so a shared attachment survives while any message
//! that still references it is inside the horizon. Raw RFC822 blobs and Graph
//! `html` blobs are not evicted (the spec's order names only attachments and
//! bodies); an all-raw store above its cap is a known, reported limitation.
//!
//! ## Transaction discipline
//!
//! The plan is computed from a read, then each victim is unlinked-then-deleted
//! in its own statement, mirroring [`BlobStore::release`]'s survivable order (a
//! file gone before its row is a hole the read path already degrades and the
//! next sweep re-reaps; a row gone before its file leaks disk). SQLite
//! serialises writers under WAL with the store's busy timeout, so a sweep and a
//! concurrent ingest transaction cannot interleave mid-statement: the sweep
//! only ever sees committed blobs, and never evicts a blob an in-flight ingest
//! has not yet committed.

use anyhow::{Context, Result};
use chrono::Utc;
use log::warn;
use rusqlite::Connection;

use crate::config::RetentionPolicy;
use crate::store::blobs::{self, BlobHash, BlobStore};
use crate::store::schema;
use crate::store::Store;

/// `meta` key holding the persisted over-cap marker. Present (value `"1"`) once
/// a sweep has warned about a breach; cleared when the store drops back under
/// the cap. See the module docs for the two-strike rule.
pub const RETENTION_OVER_CAP_MARKER: &str = "retention_over_cap";

/// A sweep that would evict more than this fraction of the store's current blob
/// bytes is refused without `--force`, a fat-finger guard against a tiny cap
/// accidentally reaping a working set that on-open re-fetch cannot yet restore
/// (#0085). Expressed as a numerator/denominator to stay in integer maths.
pub const EVICT_GUARD_NUM: u64 = 1;
pub const EVICT_GUARD_DEN: u64 = 2;

/// Which kind of blob an eviction candidate is, for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobKind {
    Attachment,
    Body,
}

impl BlobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlobKind::Attachment => "attachment",
            BlobKind::Body => "body",
        }
    }
}

/// One evicted (or would-be-evicted, under `--dry-run`) blob.
#[derive(Debug, Clone)]
pub struct EvictedBlob {
    pub hash: String,
    pub kind: BlobKind,
    pub size: u64,
    /// The freshest referencing message's `date_sort` (unix seconds; `0` when
    /// undated), the key the oldest-first order sorts on.
    pub newest_date: i64,
    /// True when the blob was chosen because it is past its age horizon.
    pub past_horizon: bool,
}

/// What a sweep decided to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepDecision {
    /// Under the cap. `cleared_marker` is true when a stale over-cap marker was
    /// cleared by this sweep.
    UnderCap { cleared_marker: bool },
    /// Over the cap for the first time: warned and persisted the marker, evicted
    /// nothing.
    WarnedFirstBreach,
    /// Over the cap with the marker already set: evicted.
    Evicted,
    /// Over the cap and due to evict, but the plan would reclaim more than the
    /// guard fraction of current bytes; refused pending `--force`.
    RefusedTooMuch { would_evict_bytes: u64 },
}

/// The outcome of one sweep, enough to render a user-facing report.
#[derive(Debug, Clone)]
pub struct SweepOutcome {
    pub cap_bytes: u64,
    /// Total blob bytes before the sweep.
    pub before_bytes: u64,
    /// Total blob bytes after the sweep (equals `before_bytes` when nothing was
    /// evicted, or the projected total under `--dry-run`).
    pub after_bytes: u64,
    pub evicted: Vec<EvictedBlob>,
    pub decision: SweepDecision,
    pub dry_run: bool,
}

impl SweepOutcome {
    /// Bytes this sweep reclaimed (or would reclaim under `--dry-run`).
    pub fn reclaimed_bytes(&self) -> u64 {
        self.evicted.iter().map(|e| e.size).sum()
    }
}

/// How a sweep should behave.
#[derive(Debug, Clone, Copy, Default)]
pub struct SweepOptions {
    /// Print and decide, but change nothing: no eviction, no marker write.
    pub dry_run: bool,
    /// Bypass the [`EVICT_GUARD_NUM`]/[`EVICT_GUARD_DEN`] "too much" guard.
    pub force: bool,
}

/// Render a byte count the way the CLI reports store sizes (mirrors
/// `read_cmd::human_size`, kept here so the sweep report is self-contained).
pub fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match bytes {
        b if b >= GB => format!("{:.2} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.1} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

/// Total logical bytes of every blob the store still holds a refcount row for.
///
/// Eviction deletes the `blobs` row with the file, so a reaped blob leaves this
/// sum immediately, which is what makes it a cheap, truthful measure of resident
/// disk without a filesystem walk.
pub fn total_blob_bytes(conn: &Connection) -> Result<u64> {
    let total: i64 = conn
        .query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |row| row.get(0))
        .context("summing blob bytes")?;
    Ok(total.max(0) as u64)
}

fn marker_set(conn: &Connection) -> Result<bool> {
    Ok(schema::get_meta(conn, RETENTION_OVER_CAP_MARKER)?.is_some())
}

/// Run the retention sweep for one store against `policy`.
///
/// Pure with respect to the network: it reads and (unless `dry_run`) deletes
/// blobs and the marker, nothing else.
pub fn sweep(
    store: &Store,
    blobs: &BlobStore,
    policy: &RetentionPolicy,
    opts: SweepOptions,
) -> Result<SweepOutcome> {
    let conn = store.conn();
    let cap = policy.max_disk_bytes;
    let before = total_blob_bytes(conn)?;

    // Under cap: clear any stale marker and stop.
    if before <= cap {
        let had_marker = marker_set(conn)?;
        if had_marker && !opts.dry_run {
            schema::clear_meta(conn, RETENTION_OVER_CAP_MARKER)?;
        }
        return Ok(SweepOutcome {
            cap_bytes: cap,
            before_bytes: before,
            after_bytes: before,
            evicted: Vec::new(),
            decision: SweepDecision::UnderCap {
                cleared_marker: had_marker,
            },
            dry_run: opts.dry_run,
        });
    }

    // Over cap. First breach warns and persists the marker; it does not evict.
    if !marker_set(conn)? {
        if !opts.dry_run {
            schema::set_meta(conn, RETENTION_OVER_CAP_MARKER, "1")?;
        }
        return Ok(SweepOutcome {
            cap_bytes: cap,
            before_bytes: before,
            after_bytes: before,
            evicted: Vec::new(),
            decision: SweepDecision::WarnedFirstBreach,
            dry_run: opts.dry_run,
        });
    }

    // Marker already set: this over-cap run evicts.
    let plan = eviction_plan(conn, policy, cap, before)?;
    let would_evict: u64 = plan.iter().map(|e| e.size).sum();

    // Fat-finger guard: refuse to reap more than the guard fraction of current
    // bytes without --force (#0060 supervisor addition, pending #0085).
    if would_evict * EVICT_GUARD_DEN > before * EVICT_GUARD_NUM && !opts.force {
        return Ok(SweepOutcome {
            cap_bytes: cap,
            before_bytes: before,
            after_bytes: before,
            evicted: Vec::new(),
            decision: SweepDecision::RefusedTooMuch {
                would_evict_bytes: would_evict,
            },
            dry_run: opts.dry_run,
        });
    }

    if opts.dry_run {
        return Ok(SweepOutcome {
            cap_bytes: cap,
            before_bytes: before,
            after_bytes: before.saturating_sub(would_evict),
            evicted: plan,
            decision: SweepDecision::Evicted,
            dry_run: true,
        });
    }

    for victim in &plan {
        let hash = BlobHash::parse(&victim.hash)
            .with_context(|| format!("evicting blob {}", victim.hash))?;
        evict_blob(conn, blobs, &hash)?;
    }

    let after = total_blob_bytes(conn)?;
    if after <= cap {
        schema::clear_meta(conn, RETENTION_OVER_CAP_MARKER)?;
    }

    Ok(SweepOutcome {
        cap_bytes: cap,
        before_bytes: before,
        after_bytes: after,
        evicted: plan,
        decision: SweepDecision::Evicted,
        dry_run: false,
    })
}

/// Delete a blob's file and its refcount row unconditionally, reclaiming its
/// bytes while leaving every `messages` / `message_blobs` reference in place.
///
/// The file is unlinked before the row is deleted, the survivable order: a
/// missing file under a live row is a hole the read path degrades and the next
/// sweep reaps again, whereas a live file under a deleted row would leak disk
/// this sum can no longer see.
fn evict_blob(conn: &Connection, blobs: &BlobStore, hash: &BlobHash) -> Result<()> {
    let path = blobs.path_for(hash);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("unlinking evicted blob {}", path.display()))
        }
    }
    conn.execute("DELETE FROM blobs WHERE hash = ?1", [hash.as_str()])
        .with_context(|| format!("deleting evicted blob row {hash}"))?;
    Ok(())
}

/// One evictable-blob candidate as read from the store.
struct Candidate {
    hash: String,
    kind: BlobKind,
    size: u64,
    newest_date: i64,
}

/// Read every attachment/body blob with its size and freshest referencing
/// `date_sort`, joined through `message_blobs` so refcount and sharing are
/// honoured: a blob is listed once with the newest date of any message that
/// references it.
fn candidates(conn: &Connection, kind: BlobKind) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT mb.hash, b.size, MAX(COALESCE(m.date_sort, 0)) AS newest \
         FROM message_blobs mb \
         JOIN blobs b ON b.hash = mb.hash \
         JOIN messages m ON m.id = mb.message_row \
         WHERE mb.kind = ?1 \
         GROUP BY mb.hash, b.size",
    )?;
    let rows = stmt.query_map([kind.as_str()], |row| {
        Ok(Candidate {
            hash: row.get::<_, String>(0)?,
            kind,
            size: row.get::<_, i64>(1)?.max(0) as u64,
            newest_date: row.get::<_, i64>(2)?,
        })
    })?;
    let mut out = Vec::new();
    for c in rows {
        out.push(c?);
    }
    Ok(out)
}

/// Build the ordered eviction plan: age-horizon victims first, then attachments
/// oldest-first, then bodies oldest-first, taking blobs until the projected
/// total is back under `cap`.
fn eviction_plan(
    conn: &Connection,
    policy: &RetentionPolicy,
    cap: u64,
    before: u64,
) -> Result<Vec<EvictedBlob>> {
    let now = Utc::now().timestamp();
    let day = 86_400i64;

    let attachments = candidates(conn, BlobKind::Attachment)?;
    let bodies = candidates(conn, BlobKind::Body)?;

    // Horizon cutoffs: a candidate is past horizon when its freshest use is
    // strictly older than the cutoff. `0` days means keep-all (no cutoff).
    let att_cutoff = (policy.attachment_horizon_days > 0)
        .then(|| now - policy.attachment_horizon_days as i64 * day);
    let body_cutoff = (policy.body_horizon_days > 0)
        .then(|| now - policy.body_horizon_days as i64 * day);

    let past = |c: &Candidate, cutoff: Option<i64>| cutoff.is_some_and(|t| c.newest_date < t);

    // Priority key: group 0 = past horizon; group 1 = attachment; group 2 =
    // body. Within a group, oldest (smallest newest_date) first, hash for a
    // stable total order.
    let mut ordered: Vec<(u8, EvictedBlob)> = Vec::new();
    for c in attachments.iter().chain(bodies.iter()) {
        let cutoff = match c.kind {
            BlobKind::Attachment => att_cutoff,
            BlobKind::Body => body_cutoff,
        };
        let past_horizon = past(c, cutoff);
        let group = if past_horizon {
            0
        } else if c.kind == BlobKind::Attachment {
            1
        } else {
            2
        };
        ordered.push((
            group,
            EvictedBlob {
                hash: c.hash.clone(),
                kind: c.kind,
                size: c.size,
                newest_date: c.newest_date,
                past_horizon,
            },
        ));
    }
    ordered.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.newest_date.cmp(&b.1.newest_date))
            .then(a.1.hash.cmp(&b.1.hash))
    });

    let mut plan = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut remaining = before;
    for (_, blob) in ordered {
        if remaining <= cap {
            break;
        }
        if !seen.insert(blob.hash.clone()) {
            continue;
        }
        remaining = remaining.saturating_sub(blob.size);
        plan.push(blob);
    }

    if remaining > cap && !plan.is_empty() {
        warn!(
            "[retention] evicting every attachment and body blob still leaves the store \
             over its cap ({remaining} > {cap} bytes): raw message blobs are not evicted"
        );
    }
    Ok(plan)
}

/// The current refcount of `hash`, re-exported for tests and callers that want
/// to assert survival without reaching into [`blobs`].
pub fn refcount(conn: &Connection, hash: &BlobHash) -> Result<i64> {
    blobs::refcount(conn, hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::blobs::BlobHash;
    use rusqlite::Connection;
    use tempfile::{tempdir, TempDir};

    struct Fixture {
        _dir: TempDir,
        blobs: BlobStore,
        store: Store,
    }

    fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        Fixture {
            _dir: dir,
            blobs,
            store,
        }
    }

    /// Insert a message row and return its id.
    fn insert_message(conn: &Connection, uid: i64, date_sort: i64) -> i64 {
        conn.execute(
            "INSERT INTO messages (account, mailbox, uid, message_id, date_sort) \
             VALUES ('a', 'inbox', ?1, ?2, ?3)",
            (uid, format!("<m{uid}@x>"), date_sort),
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Write a blob of `size` bytes, acquire one reference for `message_row`
    /// under `kind`, and record the `message_blobs` row. Returns the hash.
    fn add_blob(
        f: &Fixture,
        message_row: i64,
        kind: &str,
        size: usize,
        seed: u8,
    ) -> BlobHash {
        let bytes = vec![seed; size];
        let hash = f.blobs.write(&bytes).unwrap();
        let conn = f.store.conn();
        f.blobs.acquire(conn, &hash, size as u64).unwrap();
        conn.execute(
            "INSERT INTO message_blobs (message_row, kind, ordinal, hash, size) \
             VALUES (?1, ?2, 0, ?3, ?4)",
            (message_row, kind, hash.as_str(), size as i64),
        )
        .unwrap();
        hash
    }

    fn policy(cap: u64) -> RetentionPolicy {
        RetentionPolicy {
            metadata_horizon_days: 0,
            body_horizon_days: 0,
            attachment_horizon_days: 0,
            max_disk_bytes: cap,
        }
    }

    fn message_count(f: &Fixture) -> i64 {
        f.store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    }

    // -- under cap: a no-op that touches nothing -----------------------------

    #[test]
    fn under_cap_is_a_no_op() {
        let f = fixture();
        let m = insert_message(f.store.conn(), 1, 1000);
        let h = add_blob(&f, m, "body", 100, 1);

        let out = sweep(&f.store, &f.blobs, &policy(10_000), SweepOptions::default()).unwrap();
        assert!(matches!(
            out.decision,
            SweepDecision::UnderCap { cleared_marker: false }
        ));
        assert!(out.evicted.is_empty());
        assert!(f.blobs.contains(&h), "nothing under cap is evicted");
        assert_eq!(message_count(&f), 1);
    }

    // -- first breach warns, persists a marker, evicts nothing ---------------

    #[test]
    fn first_breach_warns_and_persists_marker_without_evicting() {
        let f = fixture();
        let m = insert_message(f.store.conn(), 1, 1000);
        let h = add_blob(&f, m, "body", 1000, 1);

        let out = sweep(&f.store, &f.blobs, &policy(100), SweepOptions::default()).unwrap();
        assert_eq!(out.decision, SweepDecision::WarnedFirstBreach);
        assert!(out.evicted.is_empty(), "the first breach evicts nothing");
        assert!(f.blobs.contains(&h));
        // The marker is persisted in meta, not in memory.
        assert!(marker_set(f.store.conn()).unwrap(), "marker must persist");
    }

    // -- second breach evicts, oldest-first, down past the cap ---------------

    #[test]
    fn second_breach_evicts_oldest_first_down_past_the_cap() {
        let f = fixture();
        // Five body blobs of 1000 bytes each at increasing dates (5000 total).
        let mut hashes = Vec::new();
        for uid in 1..=5 {
            let m = insert_message(f.store.conn(), uid, uid * 1000);
            hashes.push(add_blob(&f, m, "body", 1000, uid as u8));
        }

        // Cap 3000: evict the oldest two (2000 bytes, 40% -- under the guard).
        let pol = policy(3000);

        // First sweep: warn only.
        let first = sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap();
        assert_eq!(first.decision, SweepDecision::WarnedFirstBreach);

        // Second sweep: evict oldest-first until under 3000 -> drop uid 1 + 2.
        let out = sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap();
        assert_eq!(out.decision, SweepDecision::Evicted);
        assert_eq!(out.evicted.len(), 2, "two blobs to get under 3000");
        assert_eq!(out.evicted[0].hash, hashes[0].to_string(), "oldest first");
        assert_eq!(out.evicted[1].hash, hashes[1].to_string());
        assert!(!f.blobs.contains(&hashes[0]));
        assert!(!f.blobs.contains(&hashes[1]));
        assert!(f.blobs.contains(&hashes[2]), "the newer blobs stay");
        assert!(f.blobs.contains(&hashes[4]));
        assert!(out.after_bytes <= 3000);
        // Message rows are never touched.
        assert_eq!(message_count(&f), 5);
        // Under cap now -> marker cleared.
        assert!(!marker_set(f.store.conn()).unwrap());
    }

    // -- attachments are evicted before bodies -------------------------------

    #[test]
    fn attachments_are_evicted_before_bodies() {
        let f = fixture();
        // A newer attachment and an older body; oldest-first would take the
        // body, but the kind order takes the attachment first.
        let m_att = insert_message(f.store.conn(), 1, 5000);
        let m_body = insert_message(f.store.conn(), 2, 1000);
        let att = add_blob(&f, m_att, "attachment", 1000, 1);
        let body = add_blob(&f, m_body, "body", 1000, 2);

        let pol = policy(1200);
        sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap(); // warn
        let out = sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap();
        assert_eq!(out.evicted.len(), 1);
        assert_eq!(out.evicted[0].hash, att.to_string(), "attachment goes first");
        assert!(!f.blobs.contains(&att));
        assert!(f.blobs.contains(&body));
    }

    // -- age horizon evicts before the cap order, but keeps shared-fresh ------

    #[test]
    fn age_horizon_evicts_past_horizon_blobs_first() {
        let f = fixture();
        let now = Utc::now().timestamp();
        let old_date = now - 200 * 86_400; // 200 days old
        let fresh_date = now - 10 * 86_400; // 10 days old

        let m_old = insert_message(f.store.conn(), 1, old_date);
        let m_fresh = insert_message(f.store.conn(), 2, fresh_date);
        let old = add_blob(&f, m_old, "attachment", 1000, 1);
        let fresh = add_blob(&f, m_fresh, "attachment", 1000, 2);

        // Cap is generous (both fit), but the attachment horizon is 90 days.
        let pol = RetentionPolicy {
            metadata_horizon_days: 0,
            body_horizon_days: 0,
            attachment_horizon_days: 90,
            max_disk_bytes: 1500, // still over cap so eviction runs
        };
        sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap(); // warn
        let out = sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap();
        assert_eq!(out.evicted.len(), 1);
        assert_eq!(out.evicted[0].hash, old.to_string());
        assert!(out.evicted[0].past_horizon, "chosen for being past horizon");
        assert!(!f.blobs.contains(&old));
        assert!(f.blobs.contains(&fresh), "a fresh blob survives the horizon");
    }

    // -- a blob shared by two messages survives while one still references it -

    #[test]
    fn a_shared_blob_survives_while_a_fresh_message_references_it() {
        let f = fixture();
        let now = Utc::now().timestamp();
        let old_date = now - 200 * 86_400;
        let fresh_date = now - 86_400;

        let m_old = insert_message(f.store.conn(), 1, old_date);
        let m_fresh = insert_message(f.store.conn(), 2, fresh_date);
        // The same attachment bytes referenced by both messages (refcount 2).
        let bytes = vec![7u8; 1000];
        let hash = f.blobs.write(&bytes).unwrap();
        let conn = f.store.conn();
        f.blobs.acquire(conn, &hash, 1000).unwrap();
        f.blobs.acquire(conn, &hash, 1000).unwrap();
        conn.execute(
            "INSERT INTO message_blobs (message_row, kind, ordinal, hash, size) VALUES (?1,'attachment',0,?2,1000)",
            (m_old, hash.as_str()),
        ).unwrap();
        conn.execute(
            "INSERT INTO message_blobs (message_row, kind, ordinal, hash, size) VALUES (?1,'attachment',0,?2,1000)",
            (m_fresh, hash.as_str()),
        ).unwrap();

        // Horizon 90 days: the old message is past it, but the fresh one is not,
        // and the blob's freshest use decides -> not past horizon.
        let pol = RetentionPolicy {
            metadata_horizon_days: 0,
            body_horizon_days: 0,
            attachment_horizon_days: 90,
            max_disk_bytes: 100_000, // generous cap so only horizon could evict
        };
        // Not even over cap, so it is a plain under-cap no-op; the point is that
        // the horizon query classified the shared blob as fresh.
        let out = sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap();
        assert!(out.evicted.is_empty());
        assert!(f.blobs.contains(&hash), "a blob a fresh message references survives");
        assert_eq!(refcount(f.store.conn(), &hash).unwrap(), 2);
    }

    // -- the >50% guard refuses without --force ------------------------------

    #[test]
    fn evicting_more_than_half_is_refused_without_force() {
        let f = fixture();
        let m1 = insert_message(f.store.conn(), 1, 1000);
        let m2 = insert_message(f.store.conn(), 2, 2000);
        add_blob(&f, m1, "body", 1000, 1);
        add_blob(&f, m2, "body", 1000, 2);

        let pol = policy(500); // would need to evict both (100%)
        sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap(); // warn
        let refused = sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap();
        assert!(matches!(
            refused.decision,
            SweepDecision::RefusedTooMuch { .. }
        ));
        assert!(refused.evicted.is_empty(), "the guard evicts nothing");

        // With --force it goes through.
        let forced = sweep(
            &f.store,
            &f.blobs,
            &pol,
            SweepOptions {
                dry_run: false,
                force: true,
            },
        )
        .unwrap();
        assert_eq!(forced.decision, SweepDecision::Evicted);
        assert!(!forced.evicted.is_empty());
    }

    // -- dry-run reports a plan but changes nothing --------------------------

    #[test]
    fn dry_run_reports_but_evicts_nothing_and_writes_no_marker() {
        let f = fixture();
        let m = insert_message(f.store.conn(), 1, 1000);
        let h = add_blob(&f, m, "body", 1000, 1);

        let pol = policy(100);
        // Dry-run over an un-marked store: reports the first-breach warning, but
        // does not persist the marker.
        let out = sweep(
            &f.store,
            &f.blobs,
            &pol,
            SweepOptions {
                dry_run: true,
                force: false,
            },
        )
        .unwrap();
        assert_eq!(out.decision, SweepDecision::WarnedFirstBreach);
        assert!(f.blobs.contains(&h));
        assert!(
            !marker_set(f.store.conn()).unwrap(),
            "dry-run must not persist the marker"
        );
    }

    // -- an evicted body round-trips back through a re-ingest of the same UID -

    #[test]
    fn an_evicted_body_re_materialises_on_re_acquire() {
        // The interim re-fetch path (#0060 acceptance / #0085): the sweep
        // reclaims the file and its row, the message row and message_blobs
        // reference survive, and re-writing + re-acquiring the same bytes (what
        // a same-UID re-ingest does) restores the blob and its refcount.
        let f = fixture();
        let m = insert_message(f.store.conn(), 1, 1000);
        let body_bytes = b"the body that will be evicted and re-fetched".to_vec();
        let hash = f.blobs.write(&body_bytes).unwrap();
        let conn = f.store.conn();
        f.blobs.acquire(conn, &hash, body_bytes.len() as u64).unwrap();
        conn.execute(
            "INSERT INTO message_blobs (message_row, kind, ordinal, hash, size) VALUES (?1,'body',0,?2,?3)",
            (m, hash.as_str(), body_bytes.len() as i64),
        ).unwrap();
        conn.execute(
            "UPDATE messages SET body_blob = ?1 WHERE id = ?2",
            (hash.as_str(), m),
        ).unwrap();

        let pol = policy(10); // tiny cap
        sweep(&f.store, &f.blobs, &pol, SweepOptions::default()).unwrap(); // warn
        let out = sweep(
            &f.store,
            &f.blobs,
            &pol,
            SweepOptions {
                dry_run: false,
                force: true,
            },
        )
        .unwrap();
        assert_eq!(out.decision, SweepDecision::Evicted);
        assert!(!f.blobs.contains(&hash), "the body blob was evicted");
        // The row and its reference survive the eviction.
        assert_eq!(message_count(&f), 1);
        let refs: i64 = f
            .store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM message_blobs WHERE hash = ?1",
                [hash.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refs, 1, "the message_blobs reference survives eviction");

        // A same-UID re-ingest: write the bytes again and re-acquire.
        let again = f.blobs.write(&body_bytes).unwrap();
        assert_eq!(again, hash, "same bytes, same hash");
        f.blobs
            .acquire(f.store.conn(), &hash, body_bytes.len() as u64)
            .unwrap();
        assert!(f.blobs.contains(&hash), "the body re-materialised");
        assert_eq!(f.blobs.read(&hash).unwrap(), body_bytes, "round-trips");
    }

    // -- a sweep run from a second connection reaps only committed blobs and
    //    keeps a freshly-ingested one (the mid-sync `mp store gc` case) --------

    #[test]
    fn sweep_respects_a_committed_concurrent_ingest_across_connections() {
        // The `mp store gc` mid-sync case: the sweep runs on its own store
        // connection while a sync ingests on another. SQLite's single-writer WAL
        // (the store's `BUSY_TIMEOUT_MS`) serialises the two, so a sweep never
        // interleaves mid-statement with an ingest transaction; here we pin the
        // visible half of that contract -- a sweep on connection B evicts the
        // old blob and leaves the blob connection A just committed.
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("store.sqlite3");
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let f_a = Fixture {
            _dir: tempdir().unwrap(),
            blobs: BlobStore::new(dir.path().join("blobs")),
            store: Store::open(&store_path).unwrap(),
        };

        // Three committed old blobs (3000 bytes) at increasing dates.
        let mut old = Vec::new();
        for uid in 1..=3 {
            let m = insert_message(f_a.store.conn(), uid, uid * 1000);
            old.push(add_blob(&f_a, m, "body", 1000, uid as u8));
        }

        let store_b = Store::open(&store_path).unwrap();
        let pol = policy(2500);
        sweep(&store_b, &blobs, &pol, SweepOptions::default()).unwrap(); // warn

        // Connection A ingests a fresh, high-date blob in a committed transaction.
        let fresh = blobs.write(&vec![9u8; 1000]).unwrap();
        {
            let tx = f_a.store.conn().unchecked_transaction().unwrap();
            tx.execute(
                "INSERT INTO messages (account, mailbox, uid, message_id, date_sort) VALUES ('a','inbox',9,'<f@x>',9000)",
                [],
            ).unwrap();
            let m_fresh: i64 = tx.query_row("SELECT id FROM messages WHERE uid = 9", [], |r| r.get(0)).unwrap();
            blobs.acquire(&tx, &fresh, 1000).unwrap();
            tx.execute(
                "INSERT INTO message_blobs (message_row, kind, ordinal, hash, size) VALUES (?1,'body',0,?2,1000)",
                (m_fresh, fresh.as_str()),
            ).unwrap();
            tx.commit().unwrap();
        }

        // 4000 bytes over a 2500 cap: evict the two oldest (2000 = 50%, within
        // the guard) and stop; the just-ingested fresh blob survives.
        let out = sweep(&store_b, &blobs, &pol, SweepOptions::default()).unwrap();
        assert_eq!(out.decision, SweepDecision::Evicted);
        assert_eq!(out.evicted.len(), 2);
        assert_eq!(out.evicted[0].hash, old[0].to_string());
        assert_eq!(out.evicted[1].hash, old[1].to_string());
        assert!(!blobs.contains(&old[0]));
        assert!(blobs.contains(&old[2]), "a newer committed blob survives");
        assert!(blobs.contains(&fresh), "the committed ingest survives the sweep");
    }
}
