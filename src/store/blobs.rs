//! Content-addressed blob store: `<account_dir>/blobs/ab/cd/<sha256>`.
//!
//! Every raw RFC822 message, decoded body and attachment is a file named by
//! the lowercase hex SHA-256 of its own bytes, fanned out over the first two
//! hex pairs so no directory grows to a hundred thousand entries. The name is
//! the content, which buys three things for free: dedup (a forwarded
//! attachment is stored once), verification ([`BlobStore::read`] re-hashes and
//! refuses to return bytes that no longer match their name) and immutability
//! (a hash never points at different bytes later).
//!
//! Refcounts live in the per-account `store.sqlite3` (`blobs` table), not on
//! disk, so a reference can be taken in the *same transaction* as the
//! `messages` / `outbox` row that holds the hash. See
//! [`BlobStore::acquire`] for that contract.
//!
//! Like the rest of `src/store/`, this module knows nothing about IMAP, MIME
//! or Markdown: it moves bytes and counts references.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use log::warn;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::timing::TimingSpan;

/// Distinguishes concurrent temp files inside one process; the pid separates
/// processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The SHA-256 of a blob's bytes, lowercase hex, 64 characters.
///
/// Parsing is validated because the hash becomes a path component: an
/// unchecked string from the database would let `../../` escape the blob root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHash(String);

impl BlobHash {
    /// Hash `bytes`. Never fails, and is the only way a blob name is minted.
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hex(&hasher.finalize()))
    }

    /// Parse a stored hash (a `body_blob` / `raw_blob` column, a CLI argument).
    /// Rejects anything that is not 64 lowercase hex characters.
    pub fn parse(s: &str) -> Result<Self> {
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(anyhow!(
                "'{s}' is not a blob hash (expected 64 lowercase hex characters)"
            ));
        }
        Ok(Self(s.to_string()))
    }

    /// The hex digest, as stored in the `blobs` table and in `*_blob` columns.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A blob store rooted at one account's `blobs/` directory.
///
/// Cheap to construct: the root directory is created lazily on the first
/// write, so building one for an account that has never synced touches
/// nothing.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// A store rooted at `root` (the directory that holds the `ab/cd/` fan-out).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store for a named account: `<account_dir>/blobs/`.
    pub fn for_account(account_name: &str) -> Self {
        Self::new(crate::config::blobs_dir(account_name))
    }

    /// The blob root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `hash` lives, whether or not the file exists.
    pub fn path_for(&self, hash: &BlobHash) -> PathBuf {
        let h = hash.as_str();
        self.root.join(&h[0..2]).join(&h[2..4]).join(h)
    }

    /// True when the blob's bytes are on disk. Cheap: a `stat`, no hashing.
    ///
    /// An interrupted [`BlobStore::write`] can only leave a `.tmp` sibling
    /// behind, never a partial file under the final name, so a hit here means
    /// the whole blob is present.
    pub fn contains(&self, hash: &BlobHash) -> bool {
        self.path_for(hash).is_file()
    }

    /// Store `bytes` and return their hash.
    ///
    /// Dedup is an existence check: writing identical bytes twice returns the
    /// same hash and touches the disk once. The write itself goes to a `.tmp`
    /// sibling in the destination directory (same filesystem, so the rename is
    /// atomic) and is fsynced before the rename, so a crash mid-write can
    /// never leave a truncated blob under its final, content-addressed name.
    ///
    /// This does not take a reference; call [`BlobStore::acquire`] in the
    /// transaction that writes the row holding the hash.
    pub fn write(&self, bytes: &[u8]) -> Result<BlobHash> {
        let hash = BlobHash::of(bytes);
        let mut span = TimingSpan::with_context("blob_write", format!("{} bytes", bytes.len()));

        let final_path = self.path_for(&hash);
        if final_path.is_file() {
            span.mark("deduped");
            return Ok(hash);
        }

        let dir = final_path
            .parent()
            .ok_or_else(|| anyhow!("blob path {} has no parent", final_path.display()))?;
        fs::create_dir_all(dir)
            .with_context(|| format!("creating blob directory {}", dir.display()))?;

        let tmp = dir.join(format!(
            ".{}.tmp.{}.{}",
            hash.as_str(),
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let write_result = (|| -> Result<()> {
            let mut file = fs::File::create(&tmp)
                .with_context(|| format!("creating blob temp file {}", tmp.display()))?;
            file.write_all(bytes)
                .with_context(|| format!("writing blob temp file {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing blob temp file {}", tmp.display()))?;
            drop(file);
            fs::rename(&tmp, &final_path).with_context(|| {
                format!("renaming {} to {}", tmp.display(), final_path.display())
            })
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_result?;

        span.mark("written");
        Ok(hash)
    }

    /// Read a blob and verify it.
    ///
    /// The bytes are re-hashed and compared against `hash`: a blob that was
    /// corrupted on disk fails the read instead of returning bad bytes to the
    /// parser. Nothing is deleted on mismatch; the caller decides whether to
    /// re-fetch from the server (which is always possible, the server is
    /// truth).
    pub fn read(&self, hash: &BlobHash) -> Result<Vec<u8>> {
        let path = self.path_for(hash);
        let bytes =
            fs::read(&path).with_context(|| format!("reading blob {}", path.display()))?;
        let actual = BlobHash::of(&bytes);
        if actual != *hash {
            return Err(anyhow!(
                "blob {} is corrupt: bytes hash to {}",
                path.display(),
                actual
            ));
        }
        Ok(bytes)
    }

    /// Take one reference on `hash`, inserting the `blobs` row on first use.
    ///
    /// **Transaction contract.** This is pure SQL on the connection it is
    /// handed, so ingest passes the same `rusqlite::Transaction` it uses for
    /// the `messages` / `outbox` insert:
    ///
    /// ```ignore
    /// let tx = store.conn().unchecked_transaction()?;
    /// let hash = blobs.write(raw)?;              // outside: the file is idempotent
    /// tx.execute("INSERT INTO messages (...) VALUES (...)", params)?;
    /// blobs.acquire(&tx, &hash, raw.len() as u64)?;
    /// tx.commit()?;
    /// ```
    ///
    /// The file is written *before* the transaction on purpose: a blob with no
    /// reference is a harmless orphan that the sweep reclaims, while a row
    /// referencing a missing blob is a hole in the read path.
    ///
    /// `size` is recorded only when the row is created; on later acquires the
    /// stored size wins, because the hash already proves the bytes are the
    /// same. Returns the refcount after the increment.
    pub fn acquire(&self, conn: &Connection, hash: &BlobHash, size: u64) -> Result<i64> {
        conn.execute(
            "INSERT INTO blobs (hash, size, refcount) VALUES (?1, ?2, 1)
             ON CONFLICT (hash) DO UPDATE SET refcount = refcount + 1",
            (hash.as_str(), size as i64),
        )
        .with_context(|| format!("acquiring blob {hash}"))?;
        refcount(conn, hash)
    }

    /// Drop one reference on `hash` and unlink the file when the last one goes.
    ///
    /// Returns the refcount after the decrement; `0` means the row was deleted
    /// and the file unlinked. Releasing an unknown hash is a no-op that logs,
    /// not an error, so a double release cannot poison a caller's transaction.
    ///
    /// The unlink is *not* transactional: it happens as soon as the count hits
    /// zero, so a caller that rolls back after releasing leaves a row pointing
    /// at a missing blob. That is the survivable direction (the read path
    /// treats a missing blob as evicted and re-fetches from the server), but
    /// release belongs in a transaction the caller intends to commit.
    pub fn release(&self, conn: &Connection, hash: &BlobHash) -> Result<i64> {
        let updated = conn
            .execute(
                "UPDATE blobs SET refcount = refcount - 1 WHERE hash = ?1 AND refcount > 0",
                [hash.as_str()],
            )
            .with_context(|| format!("releasing blob {hash}"))?;
        if updated == 0 {
            warn!("[store] release of unreferenced blob {hash} ignored");
            return Ok(0);
        }

        let remaining = refcount(conn, hash)?;
        if remaining > 0 {
            return Ok(remaining);
        }

        conn.execute("DELETE FROM blobs WHERE hash = ?1", [hash.as_str()])
            .with_context(|| format!("deleting blob row {hash}"))?;
        let path = self.path_for(hash);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("unlinking blob {}", path.display()))
            }
        }
        Ok(0)
    }
}

/// The current refcount of `hash`; `0` when the store has no row for it.
pub fn refcount(conn: &Connection, hash: &BlobHash) -> Result<i64> {
    let mut stmt = conn.prepare("SELECT refcount FROM blobs WHERE hash = ?1")?;
    let mut rows = stmt.query([hash.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(row.get(0)?),
        None => Ok(0),
    }
}

/// The recorded size of `hash` in bytes, or `None` when the store has no row
/// for it. Reads the `blobs` table, never the filesystem.
pub fn size(conn: &Connection, hash: &BlobHash) -> Result<Option<u64>> {
    let mut stmt = conn.prepare("SELECT size FROM blobs WHERE hash = ?1")?;
    let mut rows = stmt.query([hash.as_str()])?;
    match rows.next()? {
        Some(row) => {
            let size: i64 = row.get(0)?;
            Ok(Some(size.max(0) as u64))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::{tempdir, TempDir};

    /// A blob store and a store database in one temp directory, laid out the
    /// way an account directory is.
    fn fixture() -> (TempDir, BlobStore, Store) {
        let dir = tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        (dir, blobs, store)
    }

    fn count_blob_files(root: &Path) -> usize {
        if !root.exists() {
            return 0;
        }
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count()
    }

    #[test]
    fn hash_is_sha256_hex_and_round_trips() {
        let hash = BlobHash::of(b"hello");
        assert_eq!(
            hash.as_str(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(BlobHash::parse(hash.as_str()).unwrap(), hash);
    }

    #[test]
    fn hash_parsing_rejects_non_hashes() {
        for bad in ["", "../../etc/passwd", "ZZ", &"a".repeat(63), &"A".repeat(64)] {
            assert!(
                BlobHash::parse(bad).is_err(),
                "{bad:?} should not parse as a blob hash"
            );
        }
    }

    #[test]
    fn fan_out_path_uses_the_first_two_hex_pairs() {
        let (_dir, blobs, _store) = fixture();
        let hash = BlobHash::of(b"hello");
        let path = blobs.path_for(&hash);
        assert_eq!(
            path,
            blobs.root().join("2c").join("f2").join(hash.as_str()),
            "expected <root>/ab/cd/<sha256>"
        );
    }

    #[test]
    fn identical_bytes_dedup_to_one_file() {
        let (_dir, blobs, _store) = fixture();

        let first = blobs.write(b"the same bytes").unwrap();
        let path = blobs.path_for(&first);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();

        let second = blobs.write(b"the same bytes").unwrap();
        assert_eq!(first, second, "identical bytes must hash the same");
        assert_eq!(count_blob_files(blobs.root()), 1, "dedup wrote a second file");
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "the second write should not have touched the file"
        );

        // Different bytes are a different blob.
        blobs.write(b"other bytes").unwrap();
        assert_eq!(count_blob_files(blobs.root()), 2);
    }

    #[test]
    fn read_returns_the_written_bytes() {
        let (_dir, blobs, _store) = fixture();
        let hash = blobs.write(b"body text").unwrap();
        assert!(blobs.contains(&hash));
        assert_eq!(blobs.read(&hash).unwrap(), b"body text");
    }

    #[test]
    fn corrupted_blob_fails_the_read() {
        let (_dir, blobs, _store) = fixture();
        let hash = blobs.write(b"trustworthy bytes").unwrap();

        fs::write(blobs.path_for(&hash), b"tampered bytes").unwrap();

        let err = blobs.read(&hash).unwrap_err();
        assert!(
            err.to_string().contains("corrupt"),
            "expected a corruption error, got {err:#}"
        );
    }

    #[test]
    fn missing_blob_fails_the_read() {
        let (_dir, blobs, _store) = fixture();
        let hash = BlobHash::of(b"never written");
        assert!(!blobs.contains(&hash));
        assert!(blobs.read(&hash).is_err());
    }

    #[test]
    fn interrupted_write_leaves_nothing_visible() {
        let (_dir, blobs, _store) = fixture();
        let bytes = b"a large attachment";
        let hash = BlobHash::of(bytes);

        // Simulate a crash between "temp file created" and "renamed into
        // place": half the payload sits in a temp sibling of the final name.
        let dir = blobs.path_for(&hash).parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!(".{hash}.tmp.4242.0")), &bytes[..4]).unwrap();

        assert!(!blobs.contains(&hash), "a partial write must not be visible");
        assert!(blobs.read(&hash).is_err());

        // The retry completes normally and the blob reads back whole.
        assert_eq!(blobs.write(bytes).unwrap(), hash);
        assert_eq!(blobs.read(&hash).unwrap(), bytes);
    }

    #[test]
    fn refcount_lifecycle_unlinks_only_at_zero() {
        let (_dir, blobs, store) = fixture();
        let conn = store.conn();
        let bytes = b"shared attachment";
        let hash = blobs.write(bytes).unwrap();

        assert_eq!(refcount(conn, &hash).unwrap(), 0, "write takes no reference");

        assert_eq!(blobs.acquire(conn, &hash, bytes.len() as u64).unwrap(), 1);
        assert_eq!(blobs.acquire(conn, &hash, bytes.len() as u64).unwrap(), 2);
        assert_eq!(size(conn, &hash).unwrap(), Some(bytes.len() as u64));

        assert_eq!(blobs.release(conn, &hash).unwrap(), 1);
        assert!(
            blobs.contains(&hash),
            "a blob with references left must survive"
        );

        assert_eq!(blobs.release(conn, &hash).unwrap(), 0);
        assert!(!blobs.contains(&hash), "the last release must unlink");
        assert_eq!(refcount(conn, &hash).unwrap(), 0);
        assert_eq!(size(conn, &hash).unwrap(), None, "the row goes with the file");
    }

    #[test]
    fn releasing_an_unreferenced_blob_is_a_no_op() {
        let (_dir, blobs, store) = fixture();
        let hash = blobs.write(b"orphan").unwrap();

        assert_eq!(blobs.release(store.conn(), &hash).unwrap(), 0);
        assert!(
            blobs.contains(&hash),
            "releasing a blob nobody acquired must not unlink it"
        );
    }

    #[test]
    fn acquire_rolls_back_with_the_row_that_referenced_it() {
        let (_dir, blobs, store) = fixture();
        let bytes = b"raw rfc822";
        let hash = blobs.write(bytes).unwrap();

        let tx = store.conn().unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO messages (account, mailbox, uid, message_id, raw_blob)
             VALUES ('alice', 'inbox', 1, '<m@example.com>', ?1)",
            [hash.as_str()],
        )
        .unwrap();
        blobs.acquire(&tx, &hash, bytes.len() as u64).unwrap();
        tx.rollback().unwrap();

        let rows: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(
            refcount(store.conn(), &hash).unwrap(),
            0,
            "the reference must roll back with its row"
        );
        assert!(
            blobs.contains(&hash),
            "the file survives a rollback as an orphan, never a hole"
        );
    }

    #[test]
    fn refcount_never_goes_negative() {
        let (_dir, blobs, store) = fixture();
        let hash = blobs.write(b"bytes").unwrap();
        blobs.acquire(store.conn(), &hash, 5).unwrap();

        assert_eq!(blobs.release(store.conn(), &hash).unwrap(), 0);
        assert_eq!(blobs.release(store.conn(), &hash).unwrap(), 0);
        assert_eq!(refcount(store.conn(), &hash).unwrap(), 0);
    }
}
