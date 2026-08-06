//! JSON cache for the per-account contact index.
//!
//! Each account's index lives at `<account_dir>/contacts-cache.json` (where
//! `account_dir` resolves under `mailypoppins_data_dir()`).

use crate::contacts::types::ContactIndex;
use anyhow::{Context, Result};
use log::warn;
use std::fs;
use std::path::{Path, PathBuf};

/// What `save_rebuilt_cache` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSave {
    /// The rebuilt index replaced whatever was on disk.
    Written,
    /// The rebuild produced no contacts and the cache on disk holds `kept` of
    /// them, so the write was refused.
    RefusedEmpty { kept: usize },
}

/// Cache file for a given account lives inside that account's data directory.
pub fn cache_path(account_root: &Path) -> PathBuf {
    account_root.join("contacts-cache.json")
}

pub fn load_cache(account_root: &Path) -> Result<Option<ContactIndex>> {
    let path = cache_path(account_root);
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("reading contacts cache at {}", path.display()))?;
    let index: ContactIndex = serde_json::from_str(&data)
        .with_context(|| format!("parsing contacts cache at {}", path.display()))?;
    Ok(Some(index))
}

/// Persist a freshly rebuilt index, refusing to replace a populated cache with
/// an empty one.
///
/// The frecency corpus is accumulated by the send/sync hooks over months and a
/// rebuild is the only thing that can throw it away wholesale, so a rebuild
/// that comes back with nothing is treated as a failure to read rather than as
/// an account with no correspondents (#0053). Incremental writes go through
/// `save_cache` directly: they can only ever add.
pub fn save_rebuilt_cache(account_root: &Path, index: &ContactIndex) -> Result<CacheSave> {
    if index.contacts.is_empty() {
        let kept = load_cache(account_root)?.map_or(0, |c| c.contacts.len());
        if kept > 0 {
            warn!(
                "[contacts] refusing to overwrite {} cached contacts for '{}' with an empty rebuild ({})",
                kept,
                index.account,
                cache_path(account_root).display()
            );
            return Ok(CacheSave::RefusedEmpty { kept });
        }
    }
    save_cache(account_root, index)?;
    Ok(CacheSave::Written)
}

pub fn save_cache(account_root: &Path, index: &ContactIndex) -> Result<()> {
    let path = cache_path(account_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating cache directory at {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(index)?;
    fs::write(&path, data)
        .with_context(|| format!("writing contacts cache at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contacts::types::{Contact, ContactSource};
    use std::collections::HashMap;

    fn index(addresses: &[&str]) -> ContactIndex {
        let mut contacts = HashMap::new();
        for addr in addresses {
            contacts.insert(
                (*addr).to_string(),
                Contact {
                    address: (*addr).to_string(),
                    display_name: String::new(),
                    sent_to: 1,
                    sent_cc: 0,
                    received: 0,
                    first_seen: "2026-01-01T00:00:00+00:00".into(),
                    last_seen: "2026-01-01T00:00:00+00:00".into(),
                    source: ContactSource::Local,
                },
            );
        }
        ContactIndex {
            account: "alice".into(),
            contacts,
            built_at: "2026-01-01T00:00:00+00:00".into(),
        }
    }

    /// #0053: an empty rebuild never replaces a populated cache.
    #[test]
    fn an_empty_rebuild_does_not_overwrite_a_populated_cache() {
        let dir = tempfile::tempdir().unwrap();
        save_cache(dir.path(), &index(&["alice@example.com", "bob@example.com"])).unwrap();

        let outcome = save_rebuilt_cache(dir.path(), &index(&[])).unwrap();

        assert_eq!(outcome, CacheSave::RefusedEmpty { kept: 2 });
        let kept = load_cache(dir.path()).unwrap().unwrap();
        assert_eq!(kept.contacts.len(), 2);
    }

    /// The guard only fires against a populated cache: a first build of an
    /// account with no correspondents still writes its empty index.
    #[test]
    fn an_empty_rebuild_writes_when_there_is_nothing_to_lose() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            save_rebuilt_cache(dir.path(), &index(&[])).unwrap(),
            CacheSave::Written
        );
        assert!(load_cache(dir.path()).unwrap().unwrap().contacts.is_empty());

        // And again over the empty cache it just wrote.
        assert_eq!(
            save_rebuilt_cache(dir.path(), &index(&[])).unwrap(),
            CacheSave::Written
        );
    }

    /// A non-empty rebuild replaces whatever was there.
    #[test]
    fn a_populated_rebuild_replaces_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        save_cache(dir.path(), &index(&["alice@example.com"])).unwrap();

        let outcome = save_rebuilt_cache(dir.path(), &index(&["bob@example.com"])).unwrap();

        assert_eq!(outcome, CacheSave::Written);
        let loaded = load_cache(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.contacts.len(), 1);
        assert!(loaded.contacts.contains_key("bob@example.com"));
    }
}
